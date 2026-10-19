//! UI shared by the PC and Mac apps: tray, dialogs and the settings window.
// The Mac app (Tasks 15–18) is the other consumer; until then parts are unused there.
#![cfg_attr(not(windows), allow(dead_code))]

pub mod dialogs;
pub mod settings;
pub mod tray;
