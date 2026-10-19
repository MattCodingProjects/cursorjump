//! Raising the settings window (`common::settings_window`) when it is
//! already open.

use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::w;

/// Best effort: raise the settings window if it belongs to process `pid`.
pub(super) fn bring_to_front(pid: u32) {
    // SAFETY: plain FFI calls; the title is a static NUL-terminated string.
    unsafe {
        let Ok(hwnd) = FindWindowW(None, w!("CursorJump Settings")) else {
            return;
        };
        let mut owner = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut owner));
        if owner != pid {
            return;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}
