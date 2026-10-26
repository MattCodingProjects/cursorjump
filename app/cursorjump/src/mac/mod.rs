//! macOS client: event injection, clipboard, power management, permissions,
//! start at login and the drag-and-drop pills.

pub mod app;
pub mod autostart;
pub mod clipboard;
pub mod control;
mod dnd;
pub mod inject;
mod link;
mod netd;
pub mod permissions;
pub mod pill;
pub mod power;

/// The catcher smoke (`--smoke-catcher`, debug builds).
#[cfg(debug_assertions)]
pub use dnd::smoke_catcher::{CatcherSmoke, run_smoke as run_smoke_catcher};
/// The drag smoke (`--smoke-drag`, debug builds).
#[cfg(debug_assertions)]
pub use dnd::smoke_drag::{SmokeMode, run_smoke as run_smoke_drag};
