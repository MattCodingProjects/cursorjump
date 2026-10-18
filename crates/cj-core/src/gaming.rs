//! Gaming rule: decides whether a game currently holds the mouse, in which
//! case the cursor must not cross from the PC to the other machine.

use crate::layout::Rect;

/// Observations about the PC's cursor state, gathered by the platform layer.
#[derive(Debug, Clone, Copy)]
pub struct GameProbe {
    /// The current cursor clip rectangle (`ClipCursor` on Windows).
    pub clip: Rect,
    /// The bounding rectangle of all monitors combined.
    pub virtual_screen: Rect,
    /// Whether the system cursor is currently visible.
    pub cursor_showing: bool,
    /// Whether a Direct3D application is running in exclusive fullscreen.
    pub d3d_fullscreen: bool,
}

/// True if a game appears to hold the mouse: the cursor is clipped to a region
/// strictly smaller than the virtual screen, the cursor is hidden, or a
/// Direct3D exclusive-fullscreen application is running.
pub fn game_locked(p: &GameProbe) -> bool {
    let vs = &p.virtual_screen;
    let clip = &p.clip;
    let clipped = clip.left > vs.left
        || clip.top > vs.top
        || clip.right < vs.right
        || clip.bottom < vs.bottom;
    clipped || !p.cursor_showing || p.d3d_fullscreen
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIRTUAL: Rect = Rect {
        left: -1080,
        top: -400,
        right: 2560,
        bottom: 1520,
    };
    const MAIN: Rect = Rect {
        left: 0,
        top: 0,
        right: 2560,
        bottom: 1440,
    };

    fn probe(clip: Rect, cursor_showing: bool, d3d_fullscreen: bool) -> GameProbe {
        GameProbe {
            clip,
            virtual_screen: VIRTUAL,
            cursor_showing,
            d3d_fullscreen,
        }
    }

    #[test]
    fn desktop_not_locked() {
        assert!(!game_locked(&probe(VIRTUAL, true, false)));
    }

    #[test]
    fn clipped_is_locked() {
        assert!(game_locked(&probe(MAIN, true, false)));
    }

    #[test]
    fn hidden_cursor_is_locked() {
        assert!(game_locked(&probe(VIRTUAL, false, false)));
    }

    #[test]
    fn d3d_fullscreen_is_locked() {
        assert!(game_locked(&probe(VIRTUAL, true, true)));
    }

    #[test]
    fn clip_strictly_smaller_in_any_edge_is_locked() {
        let vs = VIRTUAL;
        let shrink_left = Rect {
            left: vs.left + 1,
            ..vs
        };
        let shrink_top = Rect {
            top: vs.top + 1,
            ..vs
        };
        let shrink_right = Rect {
            right: vs.right - 1,
            ..vs
        };
        let shrink_bottom = Rect {
            bottom: vs.bottom - 1,
            ..vs
        };
        for clip in [shrink_left, shrink_top, shrink_right, shrink_bottom] {
            assert!(game_locked(&probe(clip, true, false)), "clip {clip:?}");
        }
    }

    #[test]
    fn clip_larger_than_virtual_screen_not_locked() {
        let bigger = Rect {
            left: VIRTUAL.left - 500,
            top: VIRTUAL.top - 500,
            right: VIRTUAL.right + 500,
            bottom: VIRTUAL.bottom + 500,
        };
        assert!(!game_locked(&probe(bigger, true, false)));
    }
}
