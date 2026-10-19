//! The Accessibility permission that posting input events needs, and the
//! guard on relaunching to pick it up.

use std::path::Path;
use std::time::{Duration, SystemTime};

use objc2_core_graphics::{CGPreflightPostEventAccess, CGRequestPostEventAccess};
use tracing::warn;

const ACCESSIBILITY_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// No relaunch for the permission within this long of the previous one.
const RELAUNCH_GUARD: Duration = Duration::from_secs(60);

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Returns a C `Boolean` (unsigned char).
    fn AXIsProcessTrusted() -> u8;
}

/// What the system says about this process's Accessibility permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessState {
    /// `AXIsProcessTrusted`: follows the switch in System Settings live.
    pub trusted: bool,
    /// `CGPreflightPostEventAccess`: may stay false after the switch is
    /// turned on until the process restarts.
    pub can_post: bool,
}

impl AccessState {
    /// The PC can control this Mac.
    pub fn ready(self) -> bool {
        self.trusted || self.can_post
    }
}

/// Asks the system now (TCC queries: milliseconds, so not on a latency path).
pub fn access_state() -> AccessState {
    // SAFETY: a plain query with no arguments.
    let trusted = unsafe { AXIsProcessTrusted() } != 0;
    AccessState {
        trusted,
        can_post: CGPreflightPostEventAccess(),
    }
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

/// Whether to relaunch for the permission at `now`, given when this app
/// last did (`last`). Refused within `RELAUNCH_GUARD` of it, so a grant
/// the system never applies cannot cause a relaunch loop.
pub fn may_relaunch(last: Option<SystemTime>, now: SystemTime) -> bool {
    match last {
        None => true,
        // A stamp in the future (the clock was set back) says nothing.
        Some(t) => now.duration_since(t).map_or(true, |d| d >= RELAUNCH_GUARD),
    }
}

/// When the app last relaunched for the permission, from the stamp file.
pub fn read_relaunch_stamp(path: &Path) -> Option<SystemTime> {
    let secs: u64 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

/// Records a relaunch at `at` (whole seconds).
pub fn write_relaunch_stamp(path: &Path, at: SystemTime) -> std::io::Result<()> {
    let secs = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    std::fs::write(path, secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn relaunch_allowed_without_a_recent_one() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(may_relaunch(None, now), "never relaunched");
        assert!(may_relaunch(Some(now - Duration::from_secs(60)), now));
        assert!(may_relaunch(Some(now - Duration::from_secs(3600)), now));
    }

    #[test]
    fn relaunch_refused_within_a_minute_of_the_last() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(!may_relaunch(Some(now), now));
        assert!(!may_relaunch(Some(now - Duration::from_secs(59)), now));
    }

    #[test]
    fn relaunch_allowed_when_the_stamp_is_in_the_future() {
        // The clock was set back: the stamp says nothing about a loop.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(may_relaunch(Some(now + Duration::from_secs(5)), now));
    }

    #[test]
    fn relaunch_stamp_roundtrips_and_tolerates_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relaunch.stamp");
        assert_eq!(read_relaunch_stamp(&path), None, "missing");
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        write_relaunch_stamp(&path, at).unwrap();
        assert_eq!(read_relaunch_stamp(&path), Some(at));
        std::fs::write(&path, "not a time").unwrap();
        assert_eq!(read_relaunch_stamp(&path), None, "unreadable");
    }

    #[test]
    fn access_state_answers_without_prompting() {
        // Read-only TCC queries; the answer depends on the test binary's grant.
        let state = access_state();
        assert_eq!(state.ready(), state.trusted || state.can_post);
    }

    #[test]
    fn ready_if_either_signal_says_so() {
        let state = |trusted, can_post| AccessState { trusted, can_post };
        assert!(!state(false, false).ready());
        assert!(state(true, false).ready());
        assert!(state(false, true).ready());
        assert!(state(true, true).ready());
    }
}
