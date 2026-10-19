//! macOS client: event injection, clipboard, power management and permissions.

// Nothing calls into this module until Task 17 wires up the Mac app;
// Task 17 removes this allow.
#![allow(dead_code)]

pub mod clipboard;
pub mod inject;
pub mod permissions;
pub mod power;
