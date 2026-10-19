// Release builds are GUI programs, so starting at login opens no console
// window. Debug builds keep their console for development.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod paths;
mod ui;
#[cfg(windows)]
mod win;

use std::fs::{File, OpenOptions};

use tracing::error;
use tracing_subscriber::EnvFilter;

/// The app's log file is moved aside at startup once it is this large.
const LOG_ROTATE_BYTES: u64 = 4 * 1024 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let tool_mode = [
        "--diagnose",
        "--settings",
        "--enable-autostart",
        "--disable-autostart",
    ]
    .into_iter()
    .any(has);
    init_logging(!tool_mode);

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
        run_app()
    };
    if let Err(e) = result {
        error!("{e:#}");
        std::process::exit(1);
    }
}

/// Logs go to stderr, except for the release app on Windows: it has no
/// console, so it appends to `cursorjump.log` in the config directory.
fn init_logging(app: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let to_file = app && cfg!(all(windows, not(debug_assertions)));
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

#[cfg(not(windows))]
fn set_autostart(_enabled: bool) -> anyhow::Result<()> {
    anyhow::bail!("--enable-autostart and --disable-autostart are Windows-only")
}

#[cfg(windows)]
fn run_app() -> anyhow::Result<()> {
    win::init_process();
    win::app::run_pc()
}

#[cfg(not(windows))]
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
