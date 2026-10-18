//! Gathers the cursor observations the gaming rule needs.
use super::monitors::rect_from;
use cj_core::gaming::GameProbe;
use cj_core::layout::Rect;
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::Shell::{QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, GetClipCursor, GetCursorInfo, GetSystemMetrics, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

fn virtual_screen() -> Rect {
    // SAFETY: GetSystemMetrics has no preconditions.
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    Rect {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// Snapshot of clip rect, virtual screen, cursor visibility and D3D fullscreen state.
/// Any failed query falls back to the "not a game" value for that field.
pub fn probe_game() -> GameProbe {
    let virtual_screen = virtual_screen();
    let mut clip = RECT::default();
    // SAFETY: valid out-pointers to stack locals.
    let clip = match unsafe { GetClipCursor(&mut clip) } {
        Ok(()) => rect_from(clip),
        Err(_) => virtual_screen,
    };
    let mut info = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // CURSOR_SUPPRESSED leaves CURSOR_SHOWING clear, so it counts as hidden.
    let cursor_showing = match unsafe { GetCursorInfo(&mut info) } {
        Ok(()) => info.flags.0 & CURSOR_SHOWING.0 != 0,
        Err(_) => true,
    };
    let d3d_fullscreen = matches!(
        unsafe { SHQueryUserNotificationState() },
        Ok(QUNS_RUNNING_D3D_FULL_SCREEN)
    );
    GameProbe {
        clip,
        virtual_screen,
        cursor_showing,
        d3d_fullscreen,
    }
}
