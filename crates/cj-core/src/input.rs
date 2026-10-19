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

/// Wheel pixels per Windows wheel unit at speed 1.0 (120 units = one notch = 48 px).
const PX_PER_WHEEL_UNIT: f64 = 48.0 / 120.0;

/// Which wheel a delta came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelAxis {
    Vertical,
    Horizontal,
}

/// Scales PC motion and wheel deltas, carrying each axis's rounding remainder
/// into the next event so slow moves and hi-res wheels add up exactly.
/// `reset` it at each crossing to the Mac.
#[derive(Debug, Default)]
pub struct MotionScaler {
    motion: [f64; 2],
    wheel: [f64; 2],
}

impl MotionScaler {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Scaled relative motion, saturated to i16 (the overflow is dropped).
    pub fn motion(&mut self, dx: i32, dy: i32, speed: f32) -> (i16, i16) {
        let speed = f64::from(speed);
        let [rx, ry] = &mut self.motion;
        let f = |v: i32, rem: &mut f64| {
            let px = carry(f64::from(v) * speed, rem);
            px.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
        };
        (f(dx, rx), f(dy, ry))
    }

    /// Windows wheel units (120 = one notch) to pixels (48 px per notch at speed 1.0).
    pub fn wheel(&mut self, axis: WheelAxis, delta: i32, speed: f32, reverse: bool) -> i32 {
        let sign = if reverse { -1.0 } else { 1.0 };
        let px = f64::from(delta) * PX_PER_WHEEL_UNIT * f64::from(speed) * sign;
        carry(px, &mut self.wheel[axis as usize]) as i32
    }
}

/// Round `v + rem`, leaving the rounding error in `rem`.
fn carry(v: f64, rem: &mut f64) -> f64 {
    let total = v + *rem;
    let out = total.round();
    *rem = total - out;
    out
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

    fn moves(s: &mut MotionScaler, speed: f32, steps: &[(i32, i32)]) -> Vec<(i16, i16)> {
        steps.iter().map(|&(x, y)| s.motion(x, y, speed)).collect()
    }

    #[test]
    fn scaler_keeps_slow_remainders() {
        let mut s = MotionScaler::default();
        let out = moves(&mut s, 0.4, &[(1, 1); 5]);
        assert_eq!(out, [(0, 0), (1, 1), (0, 0), (1, 1), (0, 0)]);
    }

    #[test]
    fn scaler_half_speed_alternates() {
        let mut s = MotionScaler::default();
        let out = moves(&mut s, 0.5, &[(1, -1); 4]);
        assert_eq!(out, [(1, -1), (0, 0), (1, -1), (0, 0)]);
    }

    #[test]
    fn scaler_fast_speed_does_not_amplify_small_moves() {
        let mut s = MotionScaler::default();
        let out = moves(&mut s, 1.5, &[(1, 1); 4]);
        assert_eq!(out, [(2, 2), (1, 1), (2, 2), (1, 1)]);
    }

    #[test]
    fn scaler_negative_direction_mirrors() {
        let mut s = MotionScaler::default();
        let out = moves(&mut s, 0.4, &[(-1, -1); 5]);
        assert_eq!(out, [(0, 0), (-1, -1), (0, 0), (-1, -1), (0, 0)]);
    }

    #[test]
    fn scaler_axes_are_independent_and_reversals_cancel() {
        let mut s = MotionScaler::default();
        let out = moves(&mut s, 0.4, &[(1, 0), (1, 0), (-1, 1), (-1, 1)]);
        // x: 0.4, 0.8, -0.6, 0.0; y: 0.0, 0.0, 0.4, 0.8 (running totals before rounding).
        assert_eq!(out, [(0, 0), (1, 0), (-1, 0), (0, 1)]);
    }

    #[test]
    fn scaler_reset_drops_remainders() {
        let mut s = MotionScaler::default();
        assert_eq!(s.motion(1, 1, 0.4), (0, 0));
        s.reset();
        assert_eq!(s.motion(1, 1, 0.4), (0, 0));
        assert_eq!(s.wheel(WheelAxis::Vertical, 1, 1.0, false), 0);
        s.reset();
        assert_eq!(s.wheel(WheelAxis::Vertical, 1, 1.0, false), 0);
    }

    #[test]
    fn scaler_saturates_without_carrying_the_overflow() {
        let mut s = MotionScaler::default();
        assert_eq!(s.motion(40000, -40000, 1.0), (i16::MAX, i16::MIN));
        assert_eq!(s.motion(1, -1, 1.0), (1, -1));
    }

    #[test]
    fn scaler_whole_moves_at_unit_speed() {
        let mut s = MotionScaler::default();
        assert_eq!(s.motion(10, -7, 1.0), (10, -7));
    }

    #[test]
    fn wheel_sub_notch_deltas_add_up() {
        let mut s = MotionScaler::default();
        let px: i32 = (0..8)
            .map(|_| s.wheel(WheelAxis::Vertical, 15, 1.0, false))
            .sum();
        assert_eq!(px, 48);
        // Hi-res wheels: 120 deltas of 1 make one notch, 48 px.
        let steps: Vec<i32> = (0..120)
            .map(|_| s.wheel(WheelAxis::Vertical, 1, 1.0, false))
            .collect();
        assert_eq!(steps[..5], [0, 1, 0, 1, 0]);
        assert_eq!(steps.iter().sum::<i32>(), 48);
    }

    #[test]
    fn wheel_axes_and_reverse() {
        let mut s = MotionScaler::default();
        assert_eq!(s.wheel(WheelAxis::Vertical, 1, 1.0, false), 0);
        assert_eq!(s.wheel(WheelAxis::Horizontal, 1, 1.0, false), 0);
        assert_eq!(s.wheel(WheelAxis::Vertical, 1, 1.0, false), 1);
        assert_eq!(s.wheel(WheelAxis::Horizontal, 1, 1.0, false), 1);
        let mut r = MotionScaler::default();
        let px: i32 = (0..5)
            .map(|_| r.wheel(WheelAxis::Vertical, 1, 1.0, true))
            .sum();
        assert_eq!(px, -2);
    }

    #[test]
    fn wheel_conversion() {
        let px = |delta, speed, reverse| {
            MotionScaler::default().wheel(WheelAxis::Vertical, delta, speed, reverse)
        };
        assert_eq!(px(120, 1.0, false), 48);
        assert_eq!(px(60, 1.0, false), 24);
        assert_eq!(px(120, 1.0, true), -48);
        assert_eq!(px(120, 2.0, false), 96);
    }
}
