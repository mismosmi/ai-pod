use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::AppState;
use super::lifecycle::ProjectState;
use crate::workspace::workspace_hash;

/// Regex that rejects annoying command patterns:
///   - commands starting with `cd /` (working directory is already set)
///   - commands ending with `| head` or `| tail` (trim output inside the container instead)
pub static COMMAND_REJECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^\s*cd\s+/)|([|]\s*(head|tail)(\s[^|]*)?\s*$)").unwrap());

/// Why the user denied a command. Surfaced back to the agent so it knows how
/// to proceed (e.g. retry inside the container, change approach, stop and
/// wait).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialReason {
    /// Run the command inside the container instead of on the host.
    RunInContainer,
    /// The current approach is wrong; try a different one.
    WrongDirection,
    /// Stop and wait for user input.
    StopAndAsk,
    /// No reason given.
    NoReason,
}

impl DenialReason {
    /// Human-readable message returned to the agent.
    pub fn message(self) -> &'static str {
        match self {
            DenialReason::RunInContainer => {
                "Command denied — the user wants this command run inside the container instead of on the host. Re-run it in the container."
            }
            DenialReason::WrongDirection => {
                "Command denied — wrong direction. This is not the right solution; try a different approach."
            }
            DenialReason::StopAndAsk => {
                "Command denied — stop your current work and wait for the user to provide further input."
            }
            DenialReason::NoReason => "Command denied by user.",
        }
    }

    /// Short slug used in machine-readable contexts (REST `reason` field).
    pub fn slug(self) -> &'static str {
        match self {
            DenialReason::RunInContainer => "run_in_container",
            DenialReason::WrongDirection => "wrong_direction",
            DenialReason::StopAndAsk => "stop_and_ask",
            DenialReason::NoReason => "no_reason",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    AllowOnce,
    AlwaysAllow,
    Deny(DenialReason),
    PermissionTimeout,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckResult {
    PreApproved,
    AlwaysAllow,
    Denied(DenialReason),
    PermissionTimeout,
}

/// Build the osascript Command for requesting user approval on macOS.
/// cfg-gated to macOS production code and tests so the function is available
/// on all platforms for test verification without triggering dead_code warnings.
#[cfg(any(target_os = "macos", test))]
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

#[cfg(target_os = "linux")]
fn request_denial_reason_linux(project_name: &str) -> DenialReason {
    let mut reason = DenialReason::NoReason;
    let result = notify_rust::Notification::new()
        .summary(&format!("ai-pod: {}", project_name))
        .body("Why deny?")
        .action("run_in_container", "Run in container")
        .action("wrong_direction", "Wrong direction")
        .action("stop_and_ask", "Stop and ask")
        .action("no_reason", "No reason")
        .timeout(notify_rust::Timeout::Milliseconds(60000))
        .show();
    if let Ok(handle) = result {
        handle.wait_for_action(|action| {
            reason = parse_denial_reason(action);
        });
    }
    reason
}

/// Map a button/slug returned by the OS dialog to a [`DenialReason`].
pub fn parse_denial_reason(s: &str) -> DenialReason {
    match s.trim() {
        "run_in_container" | "Run in container" => DenialReason::RunInContainer,
        "wrong_direction" | "Wrong direction" => DenialReason::WrongDirection,
        "stop_and_ask" | "Stop and ask" => DenialReason::StopAndAsk,
        _ => DenialReason::NoReason,
    }
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
                        "deny" => {
                            ApprovalDecision::Deny(request_denial_reason_linux(&project_name))
                        }
                        "__closed" => ApprovalDecision::PermissionTimeout,
                        _ => ApprovalDecision::Deny(DenialReason::NoReason),
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
                    parse_macos_approval_output(&result)
                }
                Err(_) => ApprovalDecision::PermissionTimeout,
                _ => ApprovalDecision::Deny(DenialReason::NoReason),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            ApprovalDecision::Deny(DenialReason::NoReason)
        }
    })
    .await
    .unwrap_or(ApprovalDecision::PermissionTimeout)
}

/// Parse the stdout of the macOS approval applescript.
///
/// The script prints one of: `Allow Once`, `Always Allow`, or
/// `Deny:<reason>` where `<reason>` is the chosen denial label (or empty if
/// the secondary dialog was cancelled / no reason was selected).
#[cfg(any(target_os = "macos", test))]
pub fn parse_macos_approval_output(s: &str) -> ApprovalDecision {
    let trimmed = s.trim();
    if trimmed.contains("Always Allow") {
        ApprovalDecision::AlwaysAllow
    } else if trimmed.contains("Allow Once") {
        ApprovalDecision::AllowOnce
    } else if let Some(rest) = trimmed.strip_prefix("Deny:") {
        ApprovalDecision::Deny(parse_denial_reason(rest))
    } else {
        // Anything else (bare "Deny", empty, unexpected output) is treated as
        // a denial with no reason.
        ApprovalDecision::Deny(DenialReason::NoReason)
    }
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
        ApprovalDecision::Deny(reason) => CheckResult::Denied(reason),
        ApprovalDecision::PermissionTimeout => CheckResult::PermissionTimeout,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalOutcome {
    Approved,
    AlwaysAllow,
    Denied(DenialReason),
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
        CheckResult::Denied(reason) => ApprovalOutcome::Denied(reason),
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
    fn parse_denial_reason_recognises_slugs_and_labels() {
        assert_eq!(
            parse_denial_reason("run_in_container"),
            DenialReason::RunInContainer
        );
        assert_eq!(
            parse_denial_reason("Run in container"),
            DenialReason::RunInContainer
        );
        assert_eq!(
            parse_denial_reason("wrong_direction"),
            DenialReason::WrongDirection
        );
        assert_eq!(
            parse_denial_reason("Wrong direction"),
            DenialReason::WrongDirection
        );
        assert_eq!(
            parse_denial_reason("stop_and_ask"),
            DenialReason::StopAndAsk
        );
        assert_eq!(parse_denial_reason("Stop and ask"), DenialReason::StopAndAsk);
        assert_eq!(parse_denial_reason("No reason"), DenialReason::NoReason);
        assert_eq!(parse_denial_reason(""), DenialReason::NoReason);
        assert_eq!(parse_denial_reason("garbage"), DenialReason::NoReason);
    }

    #[test]
    fn denial_reason_messages_are_distinct_and_nonempty() {
        let reasons = [
            DenialReason::RunInContainer,
            DenialReason::WrongDirection,
            DenialReason::StopAndAsk,
            DenialReason::NoReason,
        ];
        for r in reasons {
            assert!(!r.message().is_empty());
            assert!(!r.slug().is_empty());
        }
        // Distinct
        let msgs: Vec<&str> = reasons.iter().map(|r| r.message()).collect();
        let slugs: Vec<&str> = reasons.iter().map(|r| r.slug()).collect();
        for i in 0..reasons.len() {
            for j in (i + 1)..reasons.len() {
                assert_ne!(msgs[i], msgs[j]);
                assert_ne!(slugs[i], slugs[j]);
            }
        }
    }

    #[test]
    fn parse_macos_approval_output_handles_all_decisions() {
        assert_eq!(
            parse_macos_approval_output("Allow Once\n"),
            ApprovalDecision::AllowOnce
        );
        assert_eq!(
            parse_macos_approval_output("Always Allow\n"),
            ApprovalDecision::AlwaysAllow
        );
        assert_eq!(
            parse_macos_approval_output("Deny:Run in container\n"),
            ApprovalDecision::Deny(DenialReason::RunInContainer)
        );
        assert_eq!(
            parse_macos_approval_output("Deny:Wrong direction"),
            ApprovalDecision::Deny(DenialReason::WrongDirection)
        );
        assert_eq!(
            parse_macos_approval_output("Deny:Stop and ask"),
            ApprovalDecision::Deny(DenialReason::StopAndAsk)
        );
        assert_eq!(
            parse_macos_approval_output("Deny:No reason"),
            ApprovalDecision::Deny(DenialReason::NoReason)
        );
        // Bare "Deny" (e.g. unexpected) falls back to NoReason rather than panicking.
        assert_eq!(
            parse_macos_approval_output("Deny"),
            ApprovalDecision::Deny(DenialReason::NoReason)
        );
        // Unknown text is treated as denial with no reason.
        assert_eq!(
            parse_macos_approval_output(""),
            ApprovalDecision::Deny(DenialReason::NoReason)
        );
    }

    #[test]
    fn applescript_template_offers_denial_reasons() {
        let script = include_str!("../../templates/approval_dialog.applescript");
        assert!(script.contains("Run in container"));
        assert!(script.contains("Wrong direction"));
        assert!(script.contains("Stop and ask"));
        assert!(script.contains("No reason"));
        // Returns "Deny:<reason>" so Rust can parse it.
        assert!(script.contains("\"Deny:\""));
    }

    #[test]
    fn words_starting_with_head_or_tail_not_rejected() {
        assert!(!check_command_rejected("ls | headroom"));
        assert!(!check_command_rejected("ls | tailored"));
        assert!(!check_command_rejected("ls | heading"));
    }
}
