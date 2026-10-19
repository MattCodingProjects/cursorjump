//! The Accessibility permission that posting input events needs, and
//! relaunching to pick it up (how, and the guard against a loop).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use objc2_core_graphics::{CGPreflightPostEventAccess, CGRequestPostEventAccess};
use tracing::warn;

const ACCESSIBILITY_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// No relaunch for the permission within this long of the previous one.
const RELAUNCH_GUARD: Duration = Duration::from_secs(60);
/// `XPC_SERVICE_NAME` of the process launchd runs for the LaunchAgent.
const AGENT_SERVICE_NAME: &str = super::autostart::AGENT_LABEL;
/// Waits (up to 10 s) for process `$1` to end, then opens a new copy of the
/// app bundle `$2`; a copy opened earlier would find the instance lock held.
const REOPEN_SCRIPT: &str = "i=0; while kill -0 \"$1\" 2>/dev/null && [ \"$i\" -lt 100 ]; do sleep 0.1; i=$((i+1)); done; exec /usr/bin/open -n \"$2\"";

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
    /// `CGPreflightPostEventAccess`: cached per process, so it can be stale
    /// either way. Only used to decide whether a grant needs a relaunch.
    pub can_post: bool,
}

impl AccessState {
    /// The PC can control this Mac. `trusted` alone: a stale `can_post`
    /// true after a revocation would strand the PC's cursor here.
    pub fn ready(self) -> bool {
        self.trusted
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

/// How this process is started again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelaunchVia {
    /// launchd runs it (KeepAlive SuccessfulExit=false): exit non-zero.
    Launchd,
    /// Started by hand: open this app bundle again, then exit normally.
    Reopen(PathBuf),
}

/// How to relaunch, from `XPC_SERVICE_NAME` and the executable's path; None
/// if it cannot be (started by hand, outside an app bundle).
pub fn relaunch_via(xpc_service_name: Option<&str>, exe: &Path) -> Option<RelaunchVia> {
    if xpc_service_name == Some(AGENT_SERVICE_NAME) {
        return Some(RelaunchVia::Launchd);
    }
    app_bundle(exe).map(|b| RelaunchVia::Reopen(b.to_path_buf()))
}

/// The `.app` bundle holding `exe` (`X.app/Contents/MacOS/exe`).
fn app_bundle(exe: &Path) -> Option<&Path> {
    let macos = exe
        .parent()
        .filter(|d| d.file_name().is_some_and(|n| n == "MacOS"))?;
    let contents = macos
        .parent()
        .filter(|d| d.file_name().is_some_and(|n| n == "Contents"))?;
    contents
        .parent()
        .filter(|b| b.extension().is_some_and(|e| e == "app"))
}

/// A detached command that opens `bundle` again once process `pid` (this
/// one) has ended.
pub fn reopen_command(bundle: &Path, pid: u32) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", REOPEN_SCRIPT, "cursorjump-reopen"])
        .arg(pid.to_string())
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
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
    fn access_state_ffi_links_and_returns() {
        // Read-only TCC queries; the answer depends on the test binary's grant.
        let state = access_state();
        assert_eq!(state.ready(), state.trusted);
    }

    #[test]
    fn ready_follows_ax_trust_alone() {
        let state = |trusted, can_post| AccessState { trusted, can_post };
        assert!(!state(false, false).ready());
        assert!(state(true, false).ready(), "a stale preflight false");
        assert!(
            !state(false, true).ready(),
            "a stale preflight true must not hide a revocation"
        );
        assert!(state(true, true).ready());
    }

    const EXE: &str = "/Applications/CursorJump.app/Contents/MacOS/cursorjump";

    #[test]
    fn under_launchd_relaunch_by_exiting() {
        assert_eq!(
            relaunch_via(Some("com.cursorjump.agent"), Path::new(EXE)),
            Some(RelaunchVia::Launchd)
        );
        assert_eq!(
            relaunch_via(Some("com.cursorjump.agent"), Path::new("/tmp/cursorjump")),
            Some(RelaunchVia::Launchd),
            "launchd restarts whatever it runs"
        );
    }

    #[test]
    fn started_by_hand_relaunch_by_reopening_the_bundle() {
        let bundle = Some(RelaunchVia::Reopen(PathBuf::from(
            "/Applications/CursorJump.app",
        )));
        assert_eq!(relaunch_via(None, Path::new(EXE)), bundle);
        // Opened from Finder or `open`: another launchd job name.
        assert_eq!(
            relaunch_via(
                Some("application.com.cursorjump.app.123.456"),
                Path::new(EXE)
            ),
            bundle
        );
    }

    #[test]
    fn started_by_hand_outside_a_bundle_no_relaunch() {
        assert_eq!(
            relaunch_via(None, Path::new("/Users/me/target/release/cursorjump")),
            None
        );
        assert_eq!(
            relaunch_via(None, Path::new("/x/Contents/MacOS/cursorjump")),
            None
        );
        assert_eq!(
            relaunch_via(None, Path::new("/x/Foo.app/MacOS/cursorjump")),
            None
        );
        assert_eq!(relaunch_via(None, Path::new("cursorjump")), None);
    }

    #[test]
    fn reopen_waits_for_this_process_then_opens_a_new_copy() {
        let cmd = reopen_command(Path::new("/Applications/CursorJump.app"), 4242);
        assert_eq!(cmd.get_program(), "/bin/sh");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args[0], "-c");
        assert!(args[1].contains("kill -0 \"$1\""), "{}", args[1]);
        assert!(args[1].contains("/usr/bin/open -n \"$2\""), "{}", args[1]);
        assert_eq!(args[3..], ["4242", "/Applications/CursorJump.app"]);
    }
}
