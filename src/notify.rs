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

/// System application whose bundle id is borrowed for the notification source
/// identity, so banners have a valid identity without shipping a signed `.app`.
const BUNDLE_APP_NAME: &str = "Script Editor";

/// Set the source-application identity for delivered notifications.
///
/// Must be called once before any [`send`]. Borrows a system bundle id (Script
/// Editor) so notifications have a valid identity without shipping a signed
/// `.app`. Returns an error so startup can fail loudly if identity can't be set.
pub fn init() -> anyhow::Result<()> {
    let bundle = get_bundle_identifier_or_default(BUNDLE_APP_NAME);
    mac_notification_sys::set_application(&bundle)
        .with_context(|| format!("failed to set notification application identity to {bundle:?}"))
}

/// Build the notification body from the optional status keyword, detail message,
/// and link.
///
/// The first line composes the status keyword and detail prose:
/// - status + detail → `"<status> — <detail>"` (separator is ` — `, an em-dash
///   U+2014 with surrounding spaces)
/// - detail only → `"<detail>"` (defensive/unreachable: `detail` is `Some` only
///   when a keyword block was found, which also makes `status` `Some`)
/// - status only → `"<status>"`
/// - neither → `""`
///
/// When a `link` is present, `"\n<link>"` is appended to that line.
pub fn build_body(status: Option<&str>, detail: Option<&str>, link: Option<&str>) -> String {
    let mut body = match (status, detail) {
        (Some(status), Some(detail)) => format!("{status} — {detail}"),
        (None, Some(detail)) => detail.to_string(),
        (Some(status), None) => status.to_string(),
        (None, None) => String::new(),
    };

    if let Some(link) = link {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(link);
    }

    body
}

/// Deliver a single native notification.
///
/// `title` is the feed name, `subtitle` the entry title, `body` the status/link
/// block. Plays the default system sound. Returns `true` if delivery succeeded
/// and `false` if it failed. Delivery errors are logged and swallowed (never
/// panic/propagate) so a failing send can't crash the poll loop; the returned
/// `bool` lets the caller decide whether to mark the entry as seen.
pub fn send(title: &str, subtitle: &str, body: &str) -> bool {
    let mut notification = Notification::new();
    notification.sound(Sound::Default);

    let subtitle = if subtitle.is_empty() {
        None
    } else {
        Some(subtitle)
    };

    match send_notification(title, subtitle, body, Some(&notification)) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("failed to send notification {title:?}: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_body_status_and_detail_with_link() {
        assert_eq!(
            build_body(
                Some("Monitoring"),
                Some("A fix has been implemented"),
                Some("https://example.com/x"),
            ),
            "Monitoring — A fix has been implemented\nhttps://example.com/x"
        );
    }

    #[test]
    fn build_body_status_and_detail_no_link() {
        assert_eq!(
            build_body(Some("Monitoring"), Some("A fix has been implemented"), None),
            "Monitoring — A fix has been implemented"
        );
    }

    #[test]
    fn build_body_status_only_with_link() {
        assert_eq!(
            build_body(Some("Investigating"), None, Some("https://example.com/x")),
            "Investigating\nhttps://example.com/x"
        );
    }

    #[test]
    fn build_body_status_only_no_link() {
        assert_eq!(
            build_body(Some("Investigating"), None, None),
            "Investigating"
        );
    }

    #[test]
    fn build_body_neither_with_link() {
        assert_eq!(
            build_body(None, None, Some("https://example.com/x")),
            "https://example.com/x"
        );
    }

    #[test]
    fn build_body_neither_no_link() {
        assert_eq!(build_body(None, None, None), "");
    }

    // Defensive: the detail-only branch is unreachable in practice (whenever
    // `detail` is `Some`, `status` is `Some` too). This test exists only to
    // assert the match arm is total and well-behaved, not as a live runtime mode.
    #[test]
    fn build_body_detail_only_is_total() {
        assert_eq!(
            build_body(None, Some("orphan detail"), None),
            "orphan detail"
        );
    }
}
