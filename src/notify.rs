//! Native macOS notification sender.
//!
//! [`init`] establishes the source-application identity exactly once at startup
//! (a hard failure — without it notifications cannot fire). [`send`] delivers a
//! single banner; delivery errors are logged and swallowed so a transient
//! failure can never crash the poll loop.

use anyhow::Context;
use mac_notification_sys::{
    Notification, Sound, get_bundle_identifier_or_default, send_notification,
};

/// Set the source-application identity for delivered notifications.
///
/// Must be called once before any [`send`]. Borrows a system bundle id (Script
/// Editor) so notifications have a valid identity without shipping a signed
/// `.app`. Returns an error so startup can fail loudly if identity can't be set.
pub fn init() -> anyhow::Result<()> {
    let bundle = get_bundle_identifier_or_default("Script Editor");
    mac_notification_sys::set_application(&bundle)
        .with_context(|| format!("failed to set notification application identity to {bundle:?}"))
}

/// Build the notification body from the optional status line and link.
///
/// Layout:
/// - both → `"<status>\n<link>"`
/// - status only → `"<status>"`
/// - link only → `"<link>"`
/// - neither → `""`
pub fn build_body(status: Option<&str>, link: Option<&str>) -> String {
    match (status, link) {
        (Some(status), Some(link)) => format!("{status}\n{link}"),
        (Some(status), None) => status.to_string(),
        (None, Some(link)) => link.to_string(),
        (None, None) => String::new(),
    }
}

/// Deliver a single native notification.
///
/// `title` is the feed name, `subtitle` the entry title, `body` the status/link
/// block. Plays the default system sound. Delivery errors are logged and
/// swallowed so a failing send never propagates into the poll loop.
pub fn send(title: &str, subtitle: &str, body: &str) {
    let mut notification = Notification::new();
    notification.sound(Sound::Default);

    let subtitle = if subtitle.is_empty() {
        None
    } else {
        Some(subtitle)
    };

    if let Err(e) = send_notification(title, subtitle, body, Some(&notification)) {
        log::warn!("failed to send notification {title:?}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_body_both() {
        assert_eq!(
            build_body(Some("Resolved"), Some("https://example.com/x")),
            "Resolved\nhttps://example.com/x"
        );
    }

    #[test]
    fn build_body_status_only() {
        assert_eq!(build_body(Some("Investigating"), None), "Investigating");
    }

    #[test]
    fn build_body_link_only() {
        assert_eq!(
            build_body(None, Some("https://example.com/x")),
            "https://example.com/x"
        );
    }

    #[test]
    fn build_body_neither() {
        assert_eq!(build_body(None, None), "");
    }
}
