//! `cursorjump-netd`: the root helper behind CursorJump's "Reduce Wi-Fi
//! hiccups". It holds the AWDL interface (`awdl0`, used by AirDrop and
//! friends) down while the CursorJump app asks it to, because AWDL's
//! channel hopping adds latency spikes to the Wi-Fi link to the PC.
//!
//!     cursorjump-netd --allowed-uid <uid>
//!
//! It listens on `/var/run/cursorjump-netd.sock` (root:staff 0660) and
//! talks only to `<uid>` (and root). A LaunchDaemon installed by
//! `scripts/mac/install-netd.sh` runs it. Deliberately tiny: std and libc
//! only, nothing of the GUI app.

#[cfg(target_os = "macos")]
macro_rules! log {
    ($($arg:tt)*) => {
        crate::logging::write_line(format_args!($($arg)*))
    };
}

#[cfg(target_os = "macos")]
mod awdl;
#[cfg(target_os = "macos")]
mod iface;
#[cfg(target_os = "macos")]
mod logging;
#[cfg(target_os = "macos")]
mod protocol;
#[cfg(target_os = "macos")]
mod server;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("cursorjump-netd only runs on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let allowed_uid = match parse_args(&args) {
        Ok(uid) => uid,
        Err(e) => {
            eprintln!("cursorjump-netd: {e}\nusage: cursorjump-netd --allowed-uid <uid>");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(allowed_uid) {
        log!("stopping: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
const SOCKET: &str = "/var/run/cursorjump-netd.sock";
#[cfg(target_os = "macos")]
const INTERFACE: &str = "awdl0";
/// How often a held interface is checked (macOS brings it back up).
#[cfg(target_os = "macos")]
const RECHECK: std::time::Duration = std::time::Duration::from_secs(1);
/// The plist's StandardErrorPath too (for anything written to stderr).
#[cfg(target_os = "macos")]
const LOG_FILE: &str = "/var/log/cursorjump-netd.log";

/// Requested by SIGTERM (launchctl bootout) or SIGINT.
#[cfg(target_os = "macos")]
static STOP: std::sync::OnceLock<server::Stop> = std::sync::OnceLock::new();

/// The uid from `--allowed-uid <uid>`, the only arguments accepted.
#[cfg(target_os = "macos")]
fn parse_args(args: &[String]) -> Result<u32, String> {
    match args {
        [flag, uid] if flag == "--allowed-uid" => {
            if uid.is_empty() || !uid.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("not a uid: {uid:?}"));
            }
            uid.parse().map_err(|_| format!("not a uid: {uid:?}"))
        }
        _ => Err("expected exactly --allowed-uid <uid>".into()),
    }
}

#[cfg(target_os = "macos")]
fn run(allowed_uid: u32) -> std::io::Result<()> {
    let _ = logging::LOG_FILE.set(LOG_FILE.into());
    let stop = server::Stop::new().map_err(ctx("creating the stop pipe"))?;
    let stop = STOP.get_or_init(|| stop);
    stop_on_signals().map_err(ctx("installing the signal handlers"))?;
    let mut iface = iface::Interface::open(INTERFACE).map_err(ctx(INTERFACE))?;
    // Before any client can hold it: an unclean exit (SIGKILL, a crash)
    // may have left the interface down, and KeepAlive started this copy.
    match awdl::bring_up_if_down(&mut iface) {
        Ok(true) => {
            log!("{INTERFACE} was down at start (an earlier run did not finish); brought it up")
        }
        Ok(false) => {}
        Err(e) => log!("could not check {INTERFACE} at start: {e}"),
    }
    let staff = group_id("staff").map_err(ctx("looking up the staff group"))?;
    let listener = server::bind(std::path::Path::new(SOCKET), Some(staff))
        .map_err(ctx(&format!("binding {SOCKET}")))?;
    log!("listening on {SOCKET} for uid {allowed_uid}");
    let result = server::serve(listener, allowed_uid, iface, RECHECK, stop);
    let _ = std::fs::remove_file(SOCKET);
    log!("stopped");
    result
}

/// Prefixes an error with what was being done.
#[cfg(target_os = "macos")]
fn ctx(what: &str) -> impl FnOnce(std::io::Error) -> std::io::Error {
    move |e| std::io::Error::new(e.kind(), format!("{what}: {e}"))
}

#[cfg(target_os = "macos")]
extern "C" fn on_stop_signal(_: libc::c_int) {
    if let Some(stop) = STOP.get() {
        stop.request();
    }
}

/// SIGTERM and SIGINT request `STOP`, which also wakes the loop's poll().
#[cfg(target_os = "macos")]
fn stop_on_signals() -> std::io::Result<()> {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: sigaction is plain data; all zeroes is an empty mask and no flags.
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        sa.sa_sigaction = on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: the handler only loads an initialized OnceLock and calls
        // Stop::request (an atomic store and a write(2)): async-signal-safe.
        if unsafe { libc::sigaction(sig, &sa, std::ptr::null_mut()) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The id of the group called `name`.
#[cfg(target_os = "macos")]
fn group_id(name: &str) -> std::io::Result<u32> {
    let cname = std::ffi::CString::new(name)?;
    // SAFETY: a valid C string; the result is read at once, before any
    // other getgr* call (the daemon is single-threaded here).
    let gr = unsafe { libc::getgrnam(cname.as_ptr()) };
    if gr.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no group {name:?}"),
        ));
    }
    // SAFETY: getgrnam returned a valid group entry.
    Ok(unsafe { (*gr).gr_gid })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_allowed_uid_is_required() {
        assert_eq!(parse_args(&args(&["--allowed-uid", "501"])), Ok(501));
        for bad in [
            &[][..],
            &["--allowed-uid"],
            &["--allowed-uid", ""],
            &["--allowed-uid", "-1"],
            &["--allowed-uid", "abc"],
            &["--allowed-uid", "+501"],
            &["--allowed-uid", "99999999999"],
            &["--allowed-uid", "501", "--extra"],
            &["--other", "501"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_staff_group_exists() {
        assert_eq!(group_id("staff").unwrap(), 20);
        assert!(group_id("no-such-group-cursorjump").is_err());
    }
}
