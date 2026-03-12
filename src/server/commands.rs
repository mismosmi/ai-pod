use std::path::Path;

use super::AppState;
use super::lifecycle::ProjectState;
use crate::workspace::workspace_hash;

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    AllowOnce,
    AlwaysAllow,
    Deny,
    PermissionTimeout,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckResult {
    PreApproved,
    AlwaysAllow,
    Denied,
    PermissionTimeout,
}

pub async fn request_approval(
    state: &AppState,
    command: &str,
    project_name: &str,
) -> ApprovalDecision {
    let _guard = state.approval_lock.lock().await;
    let command = command.to_string();
    let project_name = project_name.to_string();

    tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "linux")]
        {
            let mut decision = ApprovalDecision::PermissionTimeout;
            let result = notify_rust::Notification::new()
                .summary(&format!("ai-pod: {}", project_name))
                .body(&format!("Run command:\n{}", command))
                .action("allow_once", "Allow Once")
                .action("always_allow", "Always Allow")
                .action("deny", "Deny")
                .timeout(notify_rust::Timeout::Milliseconds(60000))
                .show();
            if let Ok(handle) = result {
                handle.wait_for_action(|action| {
                    decision = match action {
                        "allow_once" => ApprovalDecision::AllowOnce,
                        "always_allow" => ApprovalDecision::AlwaysAllow,
                        "deny" => ApprovalDecision::Deny,
                        "__closed" => ApprovalDecision::PermissionTimeout,
                        _ => ApprovalDecision::Deny,
                    };
                });
            }
            decision
        }
        #[cfg(target_os = "macos")]
        {
            let script = format!(
                r#"display dialog "Run command:\n{}" buttons {{"Allow Once","Always Allow","Deny"}} default button "Deny" with title "ai-pod: {}""#,
                command.replace('"', "\\\""),
                project_name.replace('"', "\\\""),
            );
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let output = std::process::Command::new("osascript")
                    .arg("-e")
                    .arg(&script)
                    .output();
                let _ = tx.send(output);
            });
            match rx.recv_timeout(std::time::Duration::from_secs(60)) {
                Ok(Ok(o)) if o.status.success() => {
                    let result = String::from_utf8_lossy(&o.stdout);
                    if result.contains("Always Allow") {
                        ApprovalDecision::AlwaysAllow
                    } else if result.contains("Allow Once") {
                        ApprovalDecision::AllowOnce
                    } else {
                        ApprovalDecision::Deny
                    }
                }
                Err(_) => ApprovalDecision::PermissionTimeout,
                _ => ApprovalDecision::Deny,
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            ApprovalDecision::Deny
        }
    })
    .await
    .unwrap_or(ApprovalDecision::PermissionTimeout)
}

pub async fn check_approval(state: &AppState, command: &str, workspace: &Path) -> CheckResult {
    let hash = workspace_hash(workspace);
    let state_file = state.config_dir.join(format!("{}.json", hash));
    let project_state = ProjectState::load(&state_file);

    if project_state.is_allowed(command) {
        return CheckResult::PreApproved;
    }

    let project_name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    match request_approval(state, command, &project_name).await {
        ApprovalDecision::AlwaysAllow => CheckResult::AlwaysAllow,
        ApprovalDecision::AllowOnce => CheckResult::PreApproved,
        ApprovalDecision::Deny => CheckResult::Denied,
        ApprovalDecision::PermissionTimeout => CheckResult::PermissionTimeout,
    }
}

pub fn get_allowed_commands(state: &AppState, workspace: &Path) -> Vec<String> {
    let hash = workspace_hash(workspace);
    let state_file = state.config_dir.join(format!("{}.json", hash));
    let project_state = ProjectState::load(&state_file);
    project_state.allowed_commands
}
