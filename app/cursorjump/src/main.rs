// Release builds are GUI programs, so starting at login opens no console
// window. Debug builds keep their console for development.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod common;
#[cfg(target_os = "macos")]
mod mac;
mod paths;
mod ui;
#[cfg(windows)]
mod win;

use std::fs::{File, OpenOptions};

use tracing::error;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

/// The app's log file is moved aside at startup once it is this large.
const LOG_ROTATE_BYTES: u64 = 4 * 1024 * 1024;

/// Every option; no option runs the app.
const FLAGS: [&str; 5] = [
    "--diagnose",
    "--diagnose-seconds",
    "--settings",
    "--enable-autostart",
    "--disable-autostart",
];

/// Hidden options of debug builds, for a person to look at a piece of the app.
#[cfg(all(target_os = "macos", debug_assertions))]
const DEBUG_FLAGS: [&str; 3] = ["--smoke-pill", "--smoke-drag", "--smoke-catcher"];
#[cfg(not(all(target_os = "macos", debug_assertions)))]
const DEBUG_FLAGS: [&str; 0] = [];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let tool_mode = args.iter().any(|a| a.starts_with("--"));
    init_logging(!tool_mode);
    log_panics();

    if let Some(flag) = unknown_flag(&args) {
        fail(&format!(
            "Unknown option {flag}. Options: --settings, --diagnose [--diagnose-seconds N], \
             --enable-autostart, --disable-autostart (none starts CursorJump)."
        ));
        std::process::exit(2);
    }

    // The pills on a scripted board for ~10 s (mac/pill.rs `run_smoke`).
    #[cfg(all(target_os = "macos", debug_assertions))]
    if has("--smoke-pill") {
        if let Err(e) = mac::pill::run_smoke() {
            error!("{e:#}");
            std::process::exit(1);
        }
        return;
    }

    // A drag from a fake PC, driven through the real injection thread and
    // drag source (mac/dnd/smoke_drag.rs):
    // `--smoke-drag [auto|promise|hold] [file|dir] [drop|esc|home]`.
    #[cfg(all(target_os = "macos", debug_assertions))]
    if has("--smoke-drag") {
        let words = args
            .iter()
            .skip_while(|a| *a != "--smoke-drag")
            .skip(1)
            .take_while(|a| !a.starts_with("--"))
            .map(String::as_str);
        let Some(mode) = mac::SmokeMode::parse(words) else {
            error!("--smoke-drag takes auto|promise|hold, file|dir and drop|esc|home");
            std::process::exit(2);
        };
        if let Err(e) = mac::run_smoke_drag(mode) {
            error!("{e:#}");
            std::process::exit(1);
        }
        return;
    }

    // A drag leaving the Mac, onto the real catcher at the left edge
    // (mac/dnd/smoke_catcher.rs): `--smoke-catcher [posted|manual]`.
    #[cfg(all(target_os = "macos", debug_assertions))]
    if has("--smoke-catcher") {
        let words = args
            .iter()
            .skip_while(|a| *a != "--smoke-catcher")
            .skip(1)
            .take_while(|a| !a.starts_with("--"))
            .map(String::as_str);
        let Some(mode) = mac::CatcherSmoke::parse(words) else {
            error!("--smoke-catcher takes posted or manual");
            std::process::exit(2);
        };
        if let Err(e) = mac::run_smoke_catcher(mode) {
            error!("{e:#}");
            std::process::exit(1);
        }
        return;
    }

    if has("--diagnose") {
        #[cfg(windows)]
        {
            win::init_process();
            win::run_diagnose(std::time::Duration::from_secs(diagnose_seconds(&args)));
            return;
        }
        #[cfg(not(windows))]
        {
            error!("--diagnose is only available on Windows");
            std::process::exit(2);
        }
    }
    let result = if has("--settings") {
        let shown = paths::config_path()
            .and_then(|p| ui::settings::run_settings(&p, cfg!(target_os = "macos")));
        if let Err(e) = &shown {
            ui::dialogs::show_error(&format!("The settings window could not open.\n\n{e:#}"));
        }
        shown
    } else if has("--enable-autostart") || has("--disable-autostart") {
        set_autostart(has("--enable-autostart"))
    } else {
        // Errors here all happen before the tray exists.
        run_app().inspect_err(|e| fail(&format!("CursorJump could not start.\n\n{e:#}")))
    };
    if let Err(e) = result {
        error!("{e:#}");
        std::process::exit(1);
    }
}

/// The first `--` argument that is not a known option.
fn unknown_flag(args: &[String]) -> Option<&str> {
    args.iter()
        .map(String::as_str)
        .find(|a| a.starts_with("--") && !FLAGS.contains(a) && !DEBUG_FLAGS.contains(a))
}

/// Logs an error; release builds, which have no console, also show it in a box.
fn fail(text: &str) {
    error!("{text}");
    if cfg!(not(debug_assertions)) {
        ui::dialogs::show_error(text);
    }
}

/// Logs go to stderr, except for the release app: it appends to
/// `cursorjump.log` in the config directory, because on Windows it has no
/// console and on macOS launchd discards its stderr.
fn init_logging(app: bool) {
    let filter = log_filter(EnvFilter::try_from_default_env().ok());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let to_file = app
        && cfg!(all(
            any(windows, target_os = "macos"),
            not(debug_assertions)
        ));
    match to_file.then(open_log_file).flatten() {
        Some(file) => builder
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => {
            #[cfg(windows)]
            win::attach_parent_console();
            builder.init();
        }
    }
}

/// `RUST_LOG` if set, else "info". ureq and ureq-proto never log at trace,
/// whatever `RUST_LOG` says: at trace they print request paths and queries,
/// and a dragged link's query (a Discord CDN URL) is a signed token. At
/// debug ureq masks the path.
fn log_filter(from_env: Option<EnvFilter>) -> EnvFilter {
    let mut filter = from_env.unwrap_or_else(|| EnvFilter::new("info"));
    // Only when something may log at trace, so a quieter filter stays as it is.
    if filter
        .max_level_hint()
        .is_none_or(|max| max == LevelFilter::TRACE)
    {
        for cap in ["ureq=debug", "ureq_proto=debug"] {
            // Replaces any directive for the same target.
            filter = filter.add_directive(cap.parse().expect("a valid directive"));
        }
    }
    filter
}

/// Logs every panic through `tracing` too: release builds have no stderr
/// to read (macOS discards it, Windows has none). The default hook still runs.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map_or_else(|| "unknown location".to_string(), ToString::to_string);
        let message = info.payload_as_str().unwrap_or("(not a string)");
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        error!(%location, thread, "panic: {message}");
        default(info);
    }));
}

fn open_log_file() -> Option<File> {
    let dir = paths::config_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("cursorjump.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&path, dir.join("cursorjump.old.log"));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

#[cfg(windows)]
fn set_autostart(enabled: bool) -> anyhow::Result<()> {
    win::autostart::set_autostart(enabled)
}

#[cfg(target_os = "macos")]
fn set_autostart(enabled: bool) -> anyhow::Result<()> {
    mac::autostart::set_autostart(enabled)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn set_autostart(_enabled: bool) -> anyhow::Result<()> {
    anyhow::bail!(
        "--enable-autostart and --disable-autostart are only available on Windows and macOS"
    )
}

#[cfg(windows)]
fn run_app() -> anyhow::Result<()> {
    win::init_process();
    win::app::run_pc()
}

#[cfg(target_os = "macos")]
fn run_app() -> anyhow::Result<()> {
    mac::app::run_mac()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn run_app() -> anyhow::Result<()> {
    println!("cursorjump {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

/// `--diagnose-seconds N` (default 120).
#[cfg(windows)]
fn diagnose_seconds(args: &[String]) -> u64 {
    args.iter()
        .position(|a| a == "--diagnose-seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(120)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Which of ureq trace, ureq debug and our own trace `env` enables.
    fn ureq_levels(env: Option<&str>) -> [bool; 3] {
        use tracing::Level;
        use tracing_subscriber::prelude::*;
        let filter = log_filter(env.map(EnvFilter::new));
        let subscriber = tracing_subscriber::registry().with(filter);
        tracing::subscriber::with_default(subscriber, || {
            [
                tracing::enabled!(target: "ureq::unversioned::transport", Level::TRACE),
                tracing::enabled!(target: "ureq::run", Level::DEBUG),
                tracing::enabled!(target: "cursorjump::common", Level::TRACE),
            ]
        })
    }

    #[test]
    fn ureq_never_logs_at_trace() {
        assert_eq!(ureq_levels(Some("trace")), [false, true, true]);
        assert_eq!(ureq_levels(Some("info,ureq=trace")), [false, true, false]);
        assert_eq!(
            ureq_levels(Some("ureq_proto=trace,ureq=trace")),
            [false, true, false]
        );
        // A quieter filter is left alone: no ureq debug under plain info.
        assert_eq!(ureq_levels(None), [false, false, false]);
        assert_eq!(ureq_levels(Some("debug")), [false, true, false]);
    }

    #[test]
    fn known_flags_and_values_pass() {
        assert_eq!(unknown_flag(&args(&[])), None);
        assert_eq!(unknown_flag(&args(&["--settings"])), None);
        assert_eq!(
            unknown_flag(&args(&["--diagnose", "--diagnose-seconds", "30"])),
            None
        );
    }

    #[test]
    fn unknown_flag_is_reported() {
        assert_eq!(unknown_flag(&args(&["--setting"])), Some("--setting"));
        // Only debug Mac builds know the pill smoke.
        let smoke = unknown_flag(&args(&["--smoke-pill"])).is_none();
        assert_eq!(smoke, cfg!(all(target_os = "macos", debug_assertions)));
        let smoke = unknown_flag(&args(&["--smoke-drag", "hold"])).is_none();
        assert_eq!(smoke, cfg!(all(target_os = "macos", debug_assertions)));
        let smoke = unknown_flag(&args(&["--smoke-catcher", "manual"])).is_none();
        assert_eq!(smoke, cfg!(all(target_os = "macos", debug_assertions)));
        assert_eq!(
            unknown_flag(&args(&["--enable-autostart", "--bogus"])),
            Some("--bogus")
        );
    }
}
