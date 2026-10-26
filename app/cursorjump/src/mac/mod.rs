//! macOS client: event injection, clipboard, power management, permissions,
//! start at login and the drag-and-drop pills.

pub mod app;
pub mod autostart;
pub mod clipboard;
pub mod control;
#[cfg_attr(
    not(debug_assertions),
    expect(
        dead_code,
        reason = "the pills and the drag source are used once Task 17 wires them in"
    )
)]
mod dnd;
pub mod inject;
mod link;
mod netd;
pub mod permissions;
#[cfg_attr(
    not(debug_assertions),
    expect(
        dead_code,
        reason = "Task 17 wires the pills into the app; debug builds run them in --smoke-pill"
    )
)]
pub mod pill;
pub mod power;

/// The drag smoke (`--smoke-drag`, debug builds).
#[cfg(debug_assertions)]
pub use dnd::source::{SmokeMode, run_smoke as run_smoke_drag};
