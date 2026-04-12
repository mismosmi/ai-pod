use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::AppState;
use super::lifecycle::ProjectState;
use crate::workspace::workspace_hash;

/// Regex that rejects dangerous command patterns:
///   - commands starting with `cd /` (working directory is already set)
///   - commands ending with `| head` or `| tail` (trim output inside the container instead)
pub static COMMAND_REJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^\s*cd\s+/)|([|]\s*(head|tail)(\s[^|]*)?\s*$)").unwrap());

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
    let script = include_str!("../../templates/approval_dialog.applescript");
    let mut c = std::process::Command::new("osascript");
    c.arg("-e")
        .arg(script)
        .arg("--")
        .arg(command)
        .arg(project_name);
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
    Rejected,
}

/// Check whether a command matches the rejection regex.
pub fn check_command_rejected(cmd: &str) -> bool {
    COMMAND_REJECT_RE.is_match(cmd)
}

pub async fn run_host_command(
    state: &AppState,
    command: &str,
    workspace: &Path,
) -> ApprovalOutcome {
    if check_command_rejected(command) {
        return ApprovalOutcome::Rejected;
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
        let cmd =
            build_approval_command(r#"echo "injected" & do shell script "evil""#, "my-project");
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

    #[test]
    fn cd_slash_is_rejected() {
        assert!(check_command_rejected("cd /home/user && ls"));
        assert!(check_command_rejected("cd /tmp && make"));
        assert!(check_command_rejected("  cd /some/path && echo hi"));
    }

    #[test]
    fn cd_relative_is_not_rejected() {
        assert!(!check_command_rejected("cd subdir && ls"));
    }

    #[test]
    fn pipe_to_head_or_tail_is_rejected() {
        assert!(check_command_rejected("ls | head"));
        assert!(check_command_rejected("ls | head -n 10"));
        assert!(check_command_rejected("ls | tail"));
        assert!(check_command_rejected("ls | tail -5"));
        assert!(check_command_rejected("ls |  head"));
        assert!(check_command_rejected("ls |  tail -n 5"));
        assert!(check_command_rejected("ls | head   "));
    }

    #[test]
    fn pipe_to_head_in_middle_of_pipeline_is_allowed() {
        assert!(!check_command_rejected("cat file | head | cat"));
        assert!(!check_command_rejected("ls | head | wc -l"));
    }

    #[test]
    fn normal_commands_not_rejected() {
        assert!(!check_command_rejected("ls"));
        assert!(!check_command_rejected("echo cd /foo"));
        assert!(!check_command_rejected("cat file | grep foo"));
        assert!(!check_command_rejected("echo hello"));
        assert!(!check_command_rejected("make build"));
        assert!(!check_command_rejected(""));
    }

    #[test]
    fn words_starting_with_head_or_tail_not_rejected() {
        assert!(!check_command_rejected("ls | headroom"));
        assert!(!check_command_rejected("ls | tailored"));
        assert!(!check_command_rejected("ls | heading"));
    }
}
