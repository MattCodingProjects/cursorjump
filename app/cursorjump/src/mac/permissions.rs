//! The Accessibility permission that posting input events needs.

use objc2_core_graphics::{CGPreflightPostEventAccess, CGRequestPostEventAccess};
use tracing::warn;

const ACCESSIBILITY_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// True if this process may post events (Privacy & Security > Accessibility).
pub fn has_post_event_access() -> bool {
    CGPreflightPostEventAccess()
}

/// Asks for permission to post events and opens the Accessibility pane in
/// System Settings so the user can switch CursorJump on.
pub fn request_post_event_access() {
    CGRequestPostEventAccess();
    match std::process::Command::new("open")
        .arg(ACCESSIBILITY_PANE)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => warn!("opening the Accessibility settings failed: {s}"),
        Err(e) => warn!("could not open the Accessibility settings: {e}"),
    }
}
