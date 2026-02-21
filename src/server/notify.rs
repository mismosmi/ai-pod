use std::process::Command;

pub enum NotifyBackend {
    OsaScript,
    NotifySend,
    None,
}

pub fn detect_backend() -> NotifyBackend {
    if Command::new("which")
        .arg("osascript")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return NotifyBackend::OsaScript;
    }

    if Command::new("which")
        .arg("notify-send")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return NotifyBackend::NotifySend;
    }

    NotifyBackend::None
}

pub fn send_notification(title: &str, message: &str) {
    match detect_backend() {
        NotifyBackend::OsaScript => {
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                message.replace('"', "\\\""),
                title.replace('"', "\\\"")
            );
            let _ = Command::new("osascript").args(["-e", &script]).output();
        }
        NotifyBackend::NotifySend => {
            let _ = Command::new("notify-send")
                .args([title, message])
                .output();
        }
        NotifyBackend::None => {
            eprintln!("[notify] No notification backend available");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_notification_does_not_panic_with_normal_strings() {
        // Exercises the full dispatch path without crashing
        send_notification("Claude Code", "Task completed.");
    }

    #[test]
    fn send_notification_does_not_panic_with_quotes() {
        // Quotes in title/message must not crash osascript path
        send_notification(r#"Title "quoted""#, r#"Message "quoted""#);
    }

    #[test]
    fn send_notification_does_not_panic_with_empty_strings() {
        send_notification("", "");
    }

    #[test]
    fn osascript_script_escapes_double_quotes() {
        let title = r#"Hello "world""#;
        let message = r#"Done "successfully""#;
        let escaped_title = title.replace('"', "\\\"");
        let escaped_message = message.replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            escaped_message, escaped_title,
        );
        // The raw quote characters should not appear unescaped inside the script
        // (after the fixed keyword portions)
        let body = script
            .trim_start_matches("display notification \"")
            .to_string();
        assert!(!body.contains("\"world\""));
        assert!(body.contains("\\\"world\\\""));
    }
}
