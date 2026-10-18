//! Pure geometry for monitor edges: mapping a cursor position to a fraction
//! along an edge and back, and detecting pushes against an edge.

pub use crate::proto::Side;

/// A monitor rectangle in physical pixels. `right` and `bottom` are exclusive;
/// coordinates may be negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

pub fn opposite(s: Side) -> Side {
    match s {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
        Side::Top => Side::Bottom,
        Side::Bottom => Side::Top,
    }
}

fn is_vertical_edge(s: Side) -> bool {
    matches!(s, Side::Left | Side::Right)
}

/// Fraction (0.0..=1.0) of the way along the given edge for point (x, y).
/// A 1-pixel edge yields 0.0.
pub fn edge_frac(r: &Rect, s: Side, x: i32, y: i32) -> f32 {
    let (pos, start, len) = if is_vertical_edge(s) {
        (y, r.top, r.bottom - r.top)
    } else {
        (x, r.left, r.right - r.left)
    };
    if len <= 1 {
        return 0.0;
    }
    (((pos - start) as f32) / ((len - 1) as f32)).clamp(0.0, 1.0)
}

/// Point on the given edge at fraction `frac`, `inset` pixels inside the edge.
pub fn point_on_edge(r: &Rect, s: Side, frac: f32, inset: i32) -> (i32, i32) {
    let frac = frac.clamp(0.0, 1.0);
    let along =
        |start: i32, end: i32| start + (frac * ((end - start - 1).max(0) as f32)).round() as i32;
    match s {
        Side::Left => (r.left + inset, along(r.top, r.bottom)),
        Side::Right => (r.right - 1 - inset, along(r.top, r.bottom)),
        Side::Top => (along(r.left, r.right), r.top + inset),
        Side::Bottom => (along(r.left, r.right), r.bottom - 1 - inset),
    }
}

/// True if (x, y) sits on the given edge and the motion (dx, dy) heads out of the rect.
pub fn is_pushing_out(r: &Rect, s: Side, x: i32, y: i32, dx: i32, dy: i32) -> bool {
    let x_inside = x >= r.left && x < r.right;
    let y_inside = y >= r.top && y < r.bottom;
    match s {
        Side::Left => x == r.left && dx < 0 && y_inside,
        Side::Right => x == r.right - 1 && dx > 0 && y_inside,
        Side::Top => y == r.top && dy < 0 && x_inside,
        Side::Bottom => y == r.bottom - 1 && dy > 0 && x_inside,
    }
}

pub fn apply_offset(frac: f32, offset: f32) -> f32 {
    (frac + offset).clamp(0.0, 1.0)
}

pub fn remove_offset(frac: f32, offset: f32) -> f32 {
    (frac - offset).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MAIN: Rect = Rect {
        left: 0,
        top: 0,
        right: 2560,
        bottom: 1440,
    };
    #[test]
    fn edge_frac_right_middle() {
        assert!((edge_frac(&MAIN, Side::Right, 2559, 720) - 0.5).abs() < 0.001);
    }
    #[test]
    fn edge_frac_negative_origin() {
        let vert = Rect {
            left: -1080,
            top: -400,
            right: 0,
            bottom: 1520,
        };
        assert!((edge_frac(&vert, Side::Left, -1080, -400)).abs() < 1e-6);
        assert!((edge_frac(&vert, Side::Left, -1080, 1519) - 1.0).abs() < 1e-6);
    }
    #[test]
    fn point_on_edge_inverts_frac() {
        assert_eq!(point_on_edge(&MAIN, Side::Right, 0.5, 2), (2557, 720));
    }
    #[test]
    fn pushing_out_only_at_edge_moving_out() {
        assert!(is_pushing_out(&MAIN, Side::Right, 2559, 100, 3, 0));
        assert!(!is_pushing_out(&MAIN, Side::Right, 2559, 100, -3, 0));
        assert!(!is_pushing_out(&MAIN, Side::Right, 2558, 100, 3, 0));
    }
    #[test]
    fn offset_clamps() {
        assert_eq!(apply_offset(0.9, 0.2), 1.0);
        assert_eq!(remove_offset(0.1, 0.2), 0.0);
    }
    #[test]
    fn opposite_sides() {
        assert_eq!(opposite(Side::Right), Side::Left);
    }
    #[test]
    fn other_sides() {
        assert_eq!(point_on_edge(&MAIN, Side::Left, 0.0, 3), (3, 0));
        assert_eq!(point_on_edge(&MAIN, Side::Top, 1.0, 0), (2559, 0));
        assert_eq!(point_on_edge(&MAIN, Side::Bottom, 0.5, 1), (1280, 1438));
        assert!(is_pushing_out(&MAIN, Side::Top, 10, 0, 0, -1));
        assert!(is_pushing_out(&MAIN, Side::Bottom, 10, 1439, 0, 1));
        assert!(is_pushing_out(&MAIN, Side::Left, 0, 10, -1, 0));
        assert!(!is_pushing_out(&MAIN, Side::Right, 2559, 1440, 3, 0));
        assert!(!is_pushing_out(&MAIN, Side::Top, 2560, 0, 0, -1));
    }
    #[test]
    fn edge_frac_clamps_and_degenerate() {
        assert_eq!(edge_frac(&MAIN, Side::Right, 2559, 5000), 1.0);
        assert_eq!(edge_frac(&MAIN, Side::Right, 2559, -5), 0.0);
        let one = Rect {
            left: 0,
            top: 0,
            right: 10,
            bottom: 1,
        };
        assert_eq!(edge_frac(&one, Side::Left, 0, 0), 0.0);
    }
}
