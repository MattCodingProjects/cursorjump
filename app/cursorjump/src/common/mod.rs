//! OS-neutral pieces of the app loop, shared by the PC and Mac apps.
// The Mac app (Task 17) is the other consumer; until it is wired up these are unused there.
#![cfg_attr(not(windows), allow(dead_code))]

pub mod clipboard_bridge;
pub mod config_file;
pub mod settings_window;
