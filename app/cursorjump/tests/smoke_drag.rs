//! The plan's `smoke_drag_from_fake_pc` (mac/dnd/source.rs `run_smoke`),
//! run as its own process: AppKit needs the process's main thread, which
//! libtest never gives a test. It moves the real cursor and presses the
//! left button, so a person runs it, pointing at the drop target during
//! the countdown: `cargo test -p cursorjump --test smoke_drag -- --ignored`.
//! `CJ_SMOKE_DRAG=promise|hold` picks the release (default `auto`). Debug
//! builds only: release builds don't know `--smoke-drag`.
#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
#[ignore = "drives a real drag with posted events; a person points at the target"]
fn smoke_drag_from_fake_pc() {
    let mode = std::env::var("CJ_SMOKE_DRAG").unwrap_or_else(|_| "auto".into());
    let status = Command::new(env!("CARGO_BIN_EXE_cursorjump"))
        .args(["--smoke-drag", &mode])
        .status()
        .expect("run cursorjump --smoke-drag");
    assert!(status.success(), "cursorjump --smoke-drag {mode}: {status}");
}
