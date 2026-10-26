//! The plan's pill smoke (mac/pill.rs `run_smoke`), run as its own process:
//! AppKit needs the process's main thread, which libtest never gives a test.
//! It shows pill windows for about 10 s for a person to look at:
//! `cargo test -p cursorjump --test smoke_pill -- --ignored`. Debug builds
//! only: release builds don't know `--smoke-pill`.
#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
#[ignore = "shows pill windows for a person to look at"]
fn smoke_pill_runs_5s() {
    let status = Command::new(env!("CARGO_BIN_EXE_cursorjump"))
        .arg("--smoke-pill")
        .status()
        .expect("run cursorjump --smoke-pill");
    assert!(status.success(), "cursorjump --smoke-pill: {status}");
}
