use serde_json::Value;
use std::path::Path;
use tokio::process::Command;

use super::AppState;
use super::lifecycle::ProjectState;
use crate::workspace::workspace_hash;

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    AllowOnce,
    AlwaysAllow,
    Deny,
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
            let mut decision = ApprovalDecision::Deny;
            let result = notify_rust::Notification::new()
                .summary(&format!("ai-pod: {}", project_name))
                .body(&format!("Run command:\n{}", command))
                .action("allow_once", "Allow Once")
                .action("always_allow", "Always Allow")
                .action("deny", "Deny")
                .timeout(notify_rust::Timeout::Never)
                .show();
            if let Ok(handle) = result {
                handle.wait_for_action(|action| {
                    decision = match action {
                        "allow_once" => ApprovalDecision::AllowOnce,
                        "always_allow" => ApprovalDecision::AlwaysAllow,
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
            let output = std::process::Command::new("osascript")
                .arg("-e")
                .arg(&script)
                .output();
            match output {
                Ok(o) if o.status.success() => {
                    let result = String::from_utf8_lossy(&o.stdout);
                    if result.contains("Always Allow") {
                        ApprovalDecision::AlwaysAllow
                    } else if result.contains("Allow Once") {
                        ApprovalDecision::AllowOnce
                    } else {
                        ApprovalDecision::Deny
                    }
                }
                _ => ApprovalDecision::Deny,
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            ApprovalDecision::Deny
        }
    })
    .await
    .unwrap_or(ApprovalDecision::Deny)
}

pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub async fn execute_command(command: &str, workspace: &Path) -> CommandOutput {
    let result = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workspace)
        .output()
        .await;

    match result {
        Ok(output) => CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(-1),
        },
        Err(e) => CommandOutput {
            stdout: String::new(),
            stderr: format!("Failed to execute command: {}", e),
            exit_code: -1,
        },
    }
}

/// Returns a JSON tool result value.
pub async fn run_host_command(state: &AppState, command: &str, workspace: &Path) -> Value {
    let project_name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let hash = workspace_hash(workspace);
    let state_file = state.config_dir.join(format!("{}.json", hash));
    let project_state = ProjectState::load(&state_file);
    let pre_approved = project_state.is_allowed(command);

    if !pre_approved {
        let decision = request_approval(state, command, &project_name).await;
        match decision {
            ApprovalDecision::Deny => {
                return serde_json::json!({
                    "content": [{ "type": "text", "text": "Command denied by user." }],
                    "isError": true
                });
            }
            ApprovalDecision::AlwaysAllow => {
                let mut ps = ProjectState::load(&state_file);
                ps.add_allowed(command);
                let _ = ps.save(&state_file);
            }
            ApprovalDecision::AllowOnce => {}
        }
    }

    let output = execute_command(command, workspace).await;

    let mut text = output.stdout.clone();
    if !output.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("stderr: ");
        text.push_str(&output.stderr);
    }
    text.push_str(&format!("\nexit code: {}", output.exit_code));

    let is_error = output.exit_code != 0;
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    })
}

pub async fn list_allowed_commands(state: &AppState, workspace: &Path) -> Value {
    let hash = workspace_hash(workspace);
    let state_file = state.config_dir.join(format!("{}.json", hash));
    let project_state = ProjectState::load(&state_file);
    let list = if project_state.allowed_commands.is_empty() {
        "(none)".to_string()
    } else {
        project_state.allowed_commands.join("\n")
    };
    serde_json::json!({
        "content": [{ "type": "text", "text": list }]
    })
}
