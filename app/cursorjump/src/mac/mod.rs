//! macOS client: event injection, clipboard, power management, permissions
//! and start at login.

pub mod app;
pub mod autostart;
pub mod clipboard;
pub mod control;
pub mod inject;
mod link;
mod netd;
pub mod permissions;
pub mod power;
