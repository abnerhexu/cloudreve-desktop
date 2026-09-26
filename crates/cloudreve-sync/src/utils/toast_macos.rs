//! UI installs a notification callback, keeping the sync crate independent of Tauri.
use std::{path::PathBuf, sync::OnceLock};
type NotificationHandler = Box<dyn Fn(&str, &str) + Send + Sync>;
static HANDLER: OnceLock<NotificationHandler> = OnceLock::new();
pub fn set_notification_handler(handler: impl Fn(&str, &str) + Send + Sync + 'static) {
    let _ = HANDLER.set(Box::new(handler));
}
pub fn send_general_text_toast(title: &str, message: &str) {
    if let Some(handler) = HANDLER.get() {
        handler(title, message);
    } else {
        tracing::info!(title, message, "Notification");
    }
}
pub fn send_warning_toast(title: &str, message: &str) {
    send_general_text_toast(title, message);
}
pub fn send_token_expiry_toast(_: &str, title: &str, message: &str) {
    if crate::ConfigManager::try_get().is_some_and(|c| !c.notify_credential_expired()) {
        return;
    }
    send_warning_toast(title, message);
}
pub fn send_conflict_toast(_: &str, path: &PathBuf, _: i64) {
    if crate::ConfigManager::try_get().is_some_and(|c| !c.notify_file_conflict()) {
        return;
    }
    send_warning_toast(
        "Sync conflict",
        &format!(
            "{} could not be uploaded. Both versions have been preserved; sync again to reconcile.",
            path.display()
        ),
    );
}
