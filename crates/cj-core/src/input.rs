//! Click counting and PC-side motion/scroll scaling.

use crate::proto::MouseButton;
use std::time::{Duration, Instant};

/// Counts consecutive clicks (double, triple...) for synthesized mouse-down events.
#[derive(Debug)]
pub struct ClickTracker {
    interval: Duration,
    max_dist: f64,
    last: Option<(MouseButton, (f64, f64), Instant)>,
    count: i64,
}

impl ClickTracker {
    pub fn new(interval: Duration, max_dist: f64) -> Self {
        Self {
            interval,
            max_dist,
            last: None,
            count: 0,
        }
    }

    /// Returns the click count (1, 2, 3...) for this button-down.
    pub fn on_down(&mut self, b: MouseButton, pos: (f64, f64), now: Instant) -> i64 {
        let continues = self.last.is_some_and(|(lb, lp, lt)| {
            lb == b
                && now.saturating_duration_since(lt) <= self.interval
                && (pos.0 - lp.0).hypot(pos.1 - lp.1) <= self.max_dist
        });
        self.count = if continues { self.count + 1 } else { 1 };
        self.last = Some((b, pos, now));
        self.count
    }
}

/// Scale relative motion, rounding each axis and saturating to i16.
pub fn scale_motion(dx: i32, dy: i32, speed: f32) -> (i16, i16) {
    let f = |v: i32| {
        (v as f32 * speed)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16
    };
    (f(dx), f(dy))
}

/// Convert Windows wheel units (120 = one notch) to pixels (48 px per notch at speed 1.0).
pub fn wheel_to_pixels(delta: i32, scroll_speed: f32, reverse: bool) -> i32 {
    let px = (delta as f32 * 48.0 / 120.0 * scroll_speed).round() as i32;
    if reverse { -px } else { px }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::MouseButton;
    use std::time::{Duration, Instant};

    fn tracker() -> ClickTracker {
        ClickTracker::new(Duration::from_millis(500), 4.0)
    }

    #[test]
    fn double_click_within_interval() {
        let mut t = tracker();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(t.on_down(MouseButton::Left, (10.0, 10.0), t0), 1);
        assert_eq!(t.on_down(MouseButton::Left, (10.0, 10.0), t0 + ms(200)), 2);
        assert_eq!(t.on_down(MouseButton::Left, (10.0, 10.0), t0 + ms(400)), 3);
    }

    #[test]
    fn slow_second_click_resets() {
        let mut t = tracker();
        let t0 = Instant::now();
        t.on_down(MouseButton::Left, (0.0, 0.0), t0);
        let n = t.on_down(
            MouseButton::Left,
            (0.0, 0.0),
            t0 + Duration::from_millis(600),
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn moved_click_resets() {
        let mut t = tracker();
        let t0 = Instant::now();
        t.on_down(MouseButton::Left, (0.0, 0.0), t0);
        let n = t.on_down(
            MouseButton::Left,
            (10.0, 0.0),
            t0 + Duration::from_millis(100),
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn other_button_resets() {
        let mut t = tracker();
        let t0 = Instant::now();
        t.on_down(MouseButton::Left, (0.0, 0.0), t0);
        let n = t.on_down(
            MouseButton::Right,
            (0.0, 0.0),
            t0 + Duration::from_millis(100),
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn scale_saturates() {
        assert_eq!(scale_motion(40000, 0, 1.0).0, i16::MAX);
        assert_eq!(scale_motion(0, -40000, 1.0).1, i16::MIN);
    }

    #[test]
    fn scale_rounds() {
        assert_eq!(scale_motion(3, -3, 0.5), (2, -2));
        assert_eq!(scale_motion(10, 10, 1.0), (10, 10));
    }

    #[test]
    fn wheel_conversion() {
        assert_eq!(wheel_to_pixels(120, 1.0, false), 48);
        assert_eq!(wheel_to_pixels(60, 1.0, false), 24);
        assert_eq!(wheel_to_pixels(120, 1.0, true), -48);
        assert_eq!(wheel_to_pixels(120, 2.0, false), 96);
    }
}
