//! The plan's `smoke_catcher_logs_offer` (mac/dnd/smoke_catcher.rs), run
//! as its own process: AppKit needs the process's main thread, which
//! libtest never gives a test. In the default `posted` mode it moves the
//! real cursor and presses the left button, so a person runs it, pointing
//! at the file or image to drag during the countdown:
//! `cargo test -p cursorjump --test smoke_catcher -- --ignored --nocapture`.
//! `CJ_SMOKE_CATCHER=manual` shows the strip for trackpad drags instead.
//! Debug builds only: release builds don't know `--smoke-catcher`.
#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
#[ignore = "drives a real drag with posted events; a person points at the file"]
fn smoke_catcher_logs_offer() {
    let mode = std::env::var("CJ_SMOKE_CATCHER").unwrap_or_else(|_| "posted".into());
    let status = Command::new(env!("CARGO_BIN_EXE_cursorjump"))
        .args(["--smoke-catcher", &mode])
        .status()
        .expect("run cursorjump --smoke-catcher");
    assert!(
        status.success(),
        "cursorjump --smoke-catcher {mode}: {status}"
    );
}
