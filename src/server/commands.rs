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

/// Build the osascript Command for requesting user approval on macOS.
/// Not cfg-gated so that tests can verify the command structure on all platforms.
fn build_approval_command(command: &str, project_name: &str) -> std::process::Command {
    let script = r#"on run argv
    set cmd to item 1 of argv
    set projName to item 2 of argv
    display dialog ("Run command:" & linefeed & cmd) buttons {"Allow Once", "Always Allow", "Deny"} default button "Deny" with title ("ai-pod: " & projName)
end run"#;
    let mut c = std::process::Command::new("osascript");
    c.arg("-e").arg(script).arg("--").arg(command).arg(project_name);
    c
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
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let output = build_approval_command(&command, &project_name).output();
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

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalOutcome {
    Approved,
    AlwaysAllow,
    Denied,
    Timeout,
    PipeRejected,
}

pub async fn run_host_command(
    state: &AppState,
    command: &str,
    workspace: &Path,
) -> ApprovalOutcome {
    if ends_with_pipe_to_head_or_tail(command) {
        return ApprovalOutcome::PipeRejected;
    }
    match check_approval(state, command, workspace).await {
        CheckResult::PreApproved => ApprovalOutcome::Approved,
        CheckResult::AlwaysAllow => {
            let hash = workspace_hash(workspace);
            let state_file = state.config_dir.join(format!("{}.json", hash));
            let mut ps = ProjectState::load(&state_file);
            ps.add_allowed(command);
            let _ = ps.save(&state_file);
            ApprovalOutcome::AlwaysAllow
        }
        CheckResult::Denied => ApprovalOutcome::Denied,
        CheckResult::PermissionTimeout => ApprovalOutcome::Timeout,
    }
}

pub fn ends_with_pipe_to_head_or_tail(cmd: &str) -> bool {
    if let Some(pipe_pos) = cmd.trim_end().rfind('|') {
        let after = cmd[pipe_pos + 1..].trim_start();
        let word = &after[..after.find(|c: char| c.is_whitespace()).unwrap_or(after.len())];
        return word == "head" || word == "tail";
    }
    false
}

pub fn get_allowed_commands(state: &AppState, workspace: &Path) -> Vec<String> {
    let hash = workspace_hash(workspace);
    let state_file = state.config_dir.join(format!("{}.json", hash));
    let project_state = ProjectState::load(&state_file);
    project_state.allowed_commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_command_does_not_interpolate_user_input() {
        let cmd = build_approval_command(
            r#"echo "injected" & do shell script "evil""#,
            "my-project",
        );
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args.len(), 5);
        assert_eq!(args[0], "-e");
        let script = args[1].to_str().unwrap();
        assert!(!script.contains("injected"));
        assert!(!script.contains("evil"));
        assert!(script.contains("on run argv"));
        assert_eq!(args[2], "--");
        assert_eq!(args[3], r#"echo "injected" & do shell script "evil""#);
        assert_eq!(args[4], "my-project");
    }

    #[test]
    fn approval_command_handles_empty_strings() {
        let cmd = build_approval_command("", "");
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args.len(), 5);
        assert_eq!(args[3], "");
        assert_eq!(args[4], "");
    }

    #[test]
    fn approval_command_handles_backslashes_and_quotes() {
        let cmd = build_approval_command(
            r#"echo "hello\" & do shell script "whoami""#,
            r#"proj\"name"#,
        );
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        let script = args[1].to_str().unwrap();
        assert!(!script.contains("whoami"));
        assert!(!script.contains("hello"));
        assert_eq!(args[3], r#"echo "hello\" & do shell script "whoami""#);
        assert_eq!(args[4], r#"proj\"name"#);
    }

    #[test]
    fn approval_command_handles_newlines_and_control_chars() {
        let cmd = build_approval_command("line1\nline2", "proj\tname");
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        let script = args[1].to_str().unwrap();
        assert!(!script.contains("line1"));
        assert_eq!(args[3], "line1\nline2");
        assert_eq!(args[4], "proj\tname");
    }

    #[test]
    fn approval_command_handles_dash_prefixed_args() {
        let cmd = build_approval_command("-e malicious_script", "--version");
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args[2], "--");
        assert_eq!(args[3], "-e malicious_script");
        assert_eq!(args[4], "--version");
    }
}
