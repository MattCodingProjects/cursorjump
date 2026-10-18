//! Monitor enumeration and selection of the monitor whose edge leads to the Mac.
use cj_core::layout::{Rect, Side};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::BOOL;

pub(crate) fn rect_from(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

/// All monitors in physical pixels, each flagged with whether it is the primary.
pub fn monitors() -> Vec<(Rect, bool)> {
    unsafe extern "system" fn collect(mon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec` passed to EnumDisplayMonitors below,
        // which outlives the synchronous enumeration.
        let out = unsafe { &mut *(data.0 as *mut Vec<(Rect, bool)>) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(mon, &mut info) }.as_bool() {
            out.push((
                rect_from(info.rcMonitor),
                info.dwFlags & MONITORINFOF_PRIMARY != 0,
            ));
        }
        true.into()
    }
    let mut out: Vec<(Rect, bool)> = Vec::new();
    // SAFETY: the callback only touches `out` through the pointer we pass.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut out as *mut Vec<(Rect, bool)> as isize),
        )
    };
    out
}

/// True if the half-open ranges `[a0, a1)` and `[b0, b1)` share any point.
fn overlaps(a0: i32, a1: i32, b0: i32, b1: i32) -> bool {
    a0 < b1 && b0 < a1
}

/// True if `other` touches `primary` along `side` with an overlapping span.
fn adjacent_on(primary: &Rect, other: &Rect, side: Side) -> bool {
    let vertical = overlaps(primary.top, primary.bottom, other.top, other.bottom);
    let horizontal = overlaps(primary.left, primary.right, other.left, other.right);
    match side {
        Side::Right => other.left == primary.right && vertical,
        Side::Left => other.right == primary.left && vertical,
        Side::Top => other.bottom == primary.top && horizontal,
        Side::Bottom => other.top == primary.bottom && horizontal,
    }
}

/// The primary monitor, if no other monitor is adjacent to it on `side`.
pub fn edge_monitor(mons: &[(Rect, bool)], side: Side) -> Option<Rect> {
    let primary = mons.iter().find(|(_, is_primary)| *is_primary)?.0;
    let blocked = mons
        .iter()
        .any(|(r, is_primary)| !*is_primary && adjacent_on(&primary, r, side));
    (!blocked).then_some(primary)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERTICAL: Rect = Rect {
        left: -1080,
        top: -400,
        right: 0,
        bottom: 1520,
    };
    const PRIMARY: Rect = Rect {
        left: 0,
        top: 0,
        right: 2560,
        bottom: 1440,
    };

    #[test]
    fn edge_monitor_picks_primary_right() {
        let mons = [(VERTICAL, false), (PRIMARY, true)];
        assert_eq!(edge_monitor(&mons, Side::Right), Some(PRIMARY));
    }

    #[test]
    fn edge_monitor_none_when_adjacent() {
        let right = Rect {
            left: 2560,
            top: 0,
            right: 4480,
            bottom: 1080,
        };
        let mons = [(VERTICAL, false), (PRIMARY, true), (right, false)];
        assert_eq!(edge_monitor(&mons, Side::Right), None);
    }

    #[test]
    fn edge_monitor_none_on_side_of_vertical() {
        let mons = [(VERTICAL, false), (PRIMARY, true)];
        assert_eq!(edge_monitor(&mons, Side::Left), None);
        assert_eq!(edge_monitor(&mons, Side::Top), Some(PRIMARY));
        assert_eq!(edge_monitor(&mons, Side::Bottom), Some(PRIMARY));
    }

    #[test]
    fn edge_monitor_touching_only_at_corner_is_not_adjacent() {
        let diagonal = Rect {
            left: 2560,
            top: 1440,
            right: 4480,
            bottom: 2520,
        };
        let mons = [(PRIMARY, true), (diagonal, false)];
        assert_eq!(edge_monitor(&mons, Side::Right), Some(PRIMARY));
        assert_eq!(edge_monitor(&mons, Side::Bottom), Some(PRIMARY));
    }

    #[test]
    fn edge_monitor_none_without_primary() {
        assert_eq!(edge_monitor(&[(PRIMARY, false)], Side::Right), None);
        assert_eq!(edge_monitor(&[], Side::Right), None);
    }
}
