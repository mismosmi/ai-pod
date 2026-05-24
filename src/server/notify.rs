pub fn send_notification(title: &str, message: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary(title)
        .body(message)
        .show()
    {
        eprintln!("[notify] Failed to send notification: {e}");
    }
}
