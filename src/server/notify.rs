/// Ask for a one-time reset. Only an explicit Reset action grants it;
/// notification errors, dismissal and timeout all keep the cooldown intact.
/// Called from a blocking worker, never from an async runtime thread.
pub(super) fn request_rate_limit_reset(ip: std::net::IpAddr, wait: std::time::Duration) -> bool {
    let body = format!(
        "Too many API requests from {ip}. Reset this client's request counter? \
         Otherwise, wait for the cooldown (next request in about {} seconds).",
        wait.as_secs().max(1),
    );
    #[cfg(target_os = "linux")]
    {
        let result = notify_rust::Notification::new()
            .summary("ai-pod: API rate limit")
            .body(&body)
            .action("reset", "Reset counter")
            .action("deny", "Wait for cooldown")
            .timeout(notify_rust::Timeout::Milliseconds(60000))
            .show();
        let mut reset = false;
        match result {
            Ok(handle) => handle.wait_for_action(|action| reset = action == "reset"),
            Err(e) => eprintln!("[notify] Failed to request rate-limit reset: {e}"),
        }
        reset
    }
    #[cfg(target_os = "macos")]
    {
        // macOS notifications do not support action buttons through notify-rust.
        // Pair the notification with a bounded, default-deny approval dialog.
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(include_str!(
                "../../templates/rate_limit_dialog.applescript"
            ))
            .arg("--")
            .arg(body)
            .output()
            .map(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).trim() == "Reset counter"
            })
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = body;
        false
    }
}

pub fn send_notification(title: &str, message: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary(title)
        .body(message)
        .show()
    {
        eprintln!("[notify] Failed to send notification: {e}");
    }
}
