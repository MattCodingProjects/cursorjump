//! Posts the PC's mouse and keyboard input as macOS events.
//!
//! Decisions (which events to post, where the cursor goes) live in pure
//! helpers that are unit-tested; `Injector` only turns them into CGEvents.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use cj_core::input::ClickTracker;
use cj_core::layout::{Rect, edge_frac, is_pushing_out, point_on_edge};
use cj_core::modifiers::{FLAG_CAPS, Held, ModifierState, is_modifier};
use cj_core::proto::{MouseButton, Side};
use objc2_app_kit::NSEvent;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGDirectDisplayID, CGDisplayBounds,
    CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback, CGError, CGEvent,
    CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMainDisplayID, CGMouseButton, CGScrollEventUnit, CGWarpMouseCursorPosition,
};
use tracing::warn;

/// How far inside the entry edge `warp_to` places the cursor, in points.
const WARP_INSET: i32 = 2;
/// Clicks further apart than this (points) do not count as a double click.
const CLICK_MAX_DIST: f64 = 4.0;
/// Keycode of the closing flagsChanged in `release_all` (Left Shift).
const KC_SHIFT: u16 = 0x38;
/// A system cursor reading this close (points) to a position we posted is lag.
const LAG_TOLERANCE: f64 = 0.5;
/// How long a posted position counts as possibly not yet applied.
const RECENT_WINDOW: Duration = Duration::from_millis(100);
/// At most this many recently posted positions are remembered.
const RECENT_MAX: usize = 128;
/// Display bounds are re-read at least this often while moving, in case the
/// reconfiguration callback is not delivered.
const BOUNDS_MAX_AGE: Duration = Duration::from_millis(500);

/// Set by the display reconfiguration callback; the Injector re-reads its
/// display bounds before the next motion.
static DISPLAYS_CHANGED: AtomicBool = AtomicBool::new(false);
static REGISTER_DISPLAY_CALLBACK: Once = Once::new();

/// A display rectangle in points (origin plus size, like `CGRect`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rectf {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rectf {
    fn to_rect(self) -> Rect {
        Rect {
            left: self.x.round() as i32,
            top: self.y.round() as i32,
            right: (self.x + self.w).round() as i32,
            bottom: (self.y + self.h).round() as i32,
        }
    }
}

/// One event to post, decided by the pure planning helpers below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Post {
    Key {
        kc: u16,
        down: bool,
        flags: u64,
        repeat: bool,
    },
    Flags {
        kc: u16,
        flags: u64,
    },
    ButtonUp(MouseButton),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseAction {
    Down,
    Up,
    Drag,
}

/// Moves `pos` by (dx, dy), clamped to `bounds` (the last point inside is
/// `x + w - 1`). Returns the new position, and the fraction along `entry`
/// when the move tried to go past that edge. The other edges are walls.
fn step(
    pos: (f64, f64),
    bounds: Rectf,
    entry: Side,
    dx: f64,
    dy: f64,
) -> ((f64, f64), Option<f32>) {
    let r = bounds.to_rect();
    let last = |lo: i32, end: i32| f64::from((end - 1).max(lo));
    let (tx, ty) = (pos.0 + dx, pos.1 + dy);
    let x = tx.clamp(f64::from(r.left), last(r.left, r.right));
    let y = ty.clamp(f64::from(r.top), last(r.top, r.bottom));
    // Direction of the part of the move the clamp cut off: nonzero only
    // when the move went past an edge.
    let over = |target: f64, clamped: f64| match target.partial_cmp(&clamped) {
        Some(std::cmp::Ordering::Less) => -1,
        Some(std::cmp::Ordering::Greater) => 1,
        _ => 0,
    };
    let (ix, iy) = (x.floor() as i32, y.floor() as i32);
    let pushing = is_pushing_out(&r, entry, ix, iy, over(tx, x), over(ty, y));
    ((x, y), pushing.then(|| edge_frac(&r, entry, ix, iy)))
}

/// The event for one key from the PC, updating `held` and `mods`.
/// Modifiers become flagsChanged; a down for a key already held is an
/// auto-repeat, except for modifiers, whose repeats are dropped (a repeated
/// Caps Lock down would otherwise toggle Caps Lock again).
fn key_plan(kc: u16, down: bool, held: &mut Held, mods: &mut ModifierState) -> Option<Post> {
    let repeat = down && held.is_key_down(kc);
    held.key(kc, down);
    if is_modifier(kc) {
        if repeat {
            return None;
        }
        mods.update(kc, down);
        return Some(Post::Flags {
            kc,
            flags: mods.flags(),
        });
    }
    Some(Post::Key {
        kc,
        down,
        flags: mods.flags(),
        repeat,
    })
}

/// Everything needed to let go of the drained keys and buttons: non-modifier
/// key-ups, then each modifier's flagsChanged, then a closing flagsChanged
/// that clears every flag except Caps Lock, then the button-ups.
fn release_plan(keys: &[u16], buttons: &[MouseButton], mods: &mut ModifierState) -> Vec<Post> {
    let (modifiers, others): (Vec<u16>, Vec<u16>) = keys.iter().partition(|&&kc| is_modifier(kc));
    let mut plan: Vec<Post> = others
        .iter()
        .map(|&kc| Post::Key {
            kc,
            down: false,
            flags: mods.flags(),
            repeat: false,
        })
        .collect();
    for &kc in &modifiers {
        mods.update(kc, false);
        plan.push(Post::Flags {
            kc,
            flags: mods.flags(),
        });
    }
    plan.push(Post::Flags {
        kc: KC_SHIFT,
        flags: mods.flags() & FLAG_CAPS,
    });
    plan.extend(buttons.iter().map(|&b| Post::ButtonUp(b)));
    plan
}

/// The CG event type and button number for a button action. Middle, Back
/// and Forward are "other" buttons 2, 3 and 4.
fn mouse_event_type(b: MouseButton, action: MouseAction) -> (CGEventType, CGMouseButton) {
    use MouseAction::*;
    let ty = match (b, action) {
        (MouseButton::Left, Down) => CGEventType::LeftMouseDown,
        (MouseButton::Left, Up) => CGEventType::LeftMouseUp,
        (MouseButton::Left, Drag) => CGEventType::LeftMouseDragged,
        (MouseButton::Right, Down) => CGEventType::RightMouseDown,
        (MouseButton::Right, Up) => CGEventType::RightMouseUp,
        (MouseButton::Right, Drag) => CGEventType::RightMouseDragged,
        (_, Down) => CGEventType::OtherMouseDown,
        (_, Up) => CGEventType::OtherMouseUp,
        (_, Drag) => CGEventType::OtherMouseDragged,
    };
    (ty, CGMouseButton(button_index(b) as u32))
}

/// Motion is a plain move, or a drag of the first held button.
fn motion_event_type(held: Option<MouseButton>) -> (CGEventType, CGMouseButton) {
    match held {
        Some(b) => mouse_event_type(b, MouseAction::Drag),
        None => (CGEventType::MouseMoved, CGMouseButton::Left),
    }
}

/// (wheel1, wheel2) for `CGEventCreateScrollWheelEvent2` from wire deltas.
/// Wire: dy > 0 scrolls up, dx > 0 scrolls right. CG: wheel1 > 0 scrolls
/// up, wheel2 > 0 scrolls left.
fn scroll_wheels(dx: i32, dy: i32) -> (i32, i32) {
    (dy, dx.saturating_neg())
}

/// The cursor position to continue from. `virt` is where our own posts put
/// the cursor; `sys` is the system's reading, which can lag behind recent
/// posts (`recent`). A reading near `virt` or a recent post is lag, so `virt`
/// stands; any other reading means something else (the Mac trackpad) moved
/// the cursor, so it is adopted.
fn reconcile(virt: (f64, f64), sys: Option<(f64, f64)>, recent: &[(f64, f64)]) -> (f64, f64) {
    let Some(sys) = sys else {
        return virt;
    };
    let near = |p: &(f64, f64)| (p.0 - sys.0).hypot(p.1 - sys.1) <= LAG_TOLERANCE;
    if near(&virt) || recent.iter().any(near) {
        virt
    } else {
        sys
    }
}

fn button_index(b: MouseButton) -> usize {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

/// The event source handle, which can move between threads.
struct SendSource(CFRetained<CGEventSource>);

// SAFETY: Core Foundation objects may be used from any thread as long as
// they are not used from two threads at once. A SendSource is owned by one
// Injector, never shared or cloned, and only used through `&self`/`&mut self`
// of that Injector, so moving it to another thread moves sole access with
// it. It is deliberately not Sync.
unsafe impl Send for SendSource {}

/// Posts mouse and keyboard events on the main display.
pub struct Injector {
    source: SendSource,
    bounds: Rectf,
    bounds_read_at: Instant,
    /// The edge the cursor came in through; set by `warp_to`.
    entry: Option<Side>,
    /// The virtual cursor: where our posts put the cursor.
    pos: (f64, f64),
    /// Positions posted in the last `RECENT_WINDOW`, oldest first.
    recent: VecDeque<(Instant, (f64, f64))>,
    mods: ModifierState,
    held: Held,
    clicks: ClickTracker,
    /// Click count of each button's latest down, repeated on its up.
    click_state: [i64; 5],
}

impl Injector {
    pub fn new() -> anyhow::Result<Injector> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .context("CGEventSourceCreate(HIDSystemState) failed")?;
        // Events posted from this source would otherwise suppress the Mac's
        // own mouse and keyboard for 250 ms each.
        CGEventSource::set_local_events_suppression_interval(Some(&source), 0.0);
        register_display_callback();
        let interval = NSEvent::doubleClickInterval();
        let interval = Duration::try_from_secs_f64(interval).unwrap_or(Duration::from_millis(500));
        Ok(Injector {
            source: SendSource(source),
            bounds: main_display_bounds(),
            bounds_read_at: Instant::now(),
            entry: None,
            pos: cursor_location().unwrap_or((0.0, 0.0)),
            recent: VecDeque::with_capacity(RECENT_MAX),
            mods: ModifierState::new(),
            held: Held::new(),
            clicks: ClickTracker::new(interval, CLICK_MAX_DIST),
            click_state: [0; 5],
        })
    }

    /// Puts the cursor 2 pt inside `edge` at fraction `frac` along it.
    pub fn warp_to(&mut self, edge: Side, frac: f32) {
        self.refresh_bounds();
        self.entry = Some(edge);
        let (x, y) = point_on_edge(&self.bounds.to_rect(), edge, frac, WARP_INSET);
        let target = (f64::from(x), f64::from(y));
        let err = CGWarpMouseCursorPosition(CGPoint::new(target.0, target.1));
        if err != CGError::Success {
            warn!("CGWarpMouseCursorPosition failed: {err:?}");
        }
        // A warp briefly disconnects the Mac's own mouse from the cursor;
        // reconnect it at once.
        let err = CGAssociateMouseAndMouseCursorPosition(true);
        if err != CGError::Success {
            warn!("CGAssociateMouseAndMouseCursorPosition failed: {err:?}");
        }
        self.pos = target;
        self.recent.clear();
        // A moved event so apps under the cursor see it arrive.
        let (ty, button) = motion_event_type(self.held.first_button());
        self.post_mouse(ty, button, target, (0, 0), 0);
    }

    /// Moves the cursor by (dx, dy) points from the virtual cursor (or from
    /// where a Mac trackpad move left it). Returns the fraction along the
    /// entry edge when the move pushes out through it.
    pub fn motion(&mut self, dx: i16, dy: i16) -> Option<f32> {
        if DISPLAYS_CHANGED.swap(false, Ordering::Relaxed)
            || self.bounds_read_at.elapsed() >= BOUNDS_MAX_AGE
        {
            self.refresh_bounds();
        }
        let from = self.current_pos();
        let entry = self.entry.unwrap_or(Side::Left);
        let (to, push) = step(from, self.bounds, entry, f64::from(dx), f64::from(dy));
        self.pos = to;
        let applied = (
            (to.0 - from.0).round() as i64,
            (to.1 - from.1).round() as i64,
        );
        if to != from {
            let held = self.held.first_button();
            let (ty, button) = motion_event_type(held);
            let clicks = held.map_or(0, |b| self.click_state[button_index(b)]);
            self.post_mouse(ty, button, to, applied, clicks);
        }
        push.filter(|_| self.entry.is_some())
    }

    /// Scrolls by pixels in the wire convention (dy > 0 up, dx > 0 right).
    pub fn scroll(&mut self, dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        let (wheel1, wheel2) = scroll_wheels(dx, dy);
        let Some(ev) = CGEvent::new_scroll_wheel_event2(
            Some(&self.source.0),
            CGScrollEventUnit::Pixel,
            2,
            wheel1,
            wheel2,
            0,
        ) else {
            warn!("CGEventCreateScrollWheelEvent2 failed");
            return;
        };
        self.post_with_flags(&ev, self.mods.flags());
    }

    pub fn button(&mut self, b: MouseButton, down: bool) {
        let at = self.current_pos();
        let i = button_index(b);
        if down {
            self.click_state[i] = self.clicks.on_down(b, at, Instant::now());
        }
        self.held.button(b, down);
        let action = if down {
            MouseAction::Down
        } else {
            MouseAction::Up
        };
        let (ty, button) = mouse_event_type(b, action);
        self.post_mouse(ty, button, at, (0, 0), self.click_state[i]);
    }

    pub fn key(&mut self, mac_keycode: u16, down: bool) {
        if let Some(post) = key_plan(mac_keycode, down, &mut self.held, &mut self.mods) {
            self.post(post);
        }
    }

    /// Lets go of every held key and button.
    pub fn release_all(&mut self) {
        let (keys, buttons) = self.held.drain();
        for post in release_plan(&keys, &buttons, &mut self.mods) {
            self.post(post);
        }
    }

    pub fn buttons_held(&self) -> bool {
        self.held.any_button()
    }

    pub fn button_held(&self, b: MouseButton) -> bool {
        self.held.is_button_down(b)
    }

    pub fn key_held(&self, mac_keycode: u16) -> bool {
        self.held.is_key_down(mac_keycode)
    }

    fn refresh_bounds(&mut self) {
        DISPLAYS_CHANGED.store(false, Ordering::Relaxed);
        self.bounds = main_display_bounds();
        self.bounds_read_at = Instant::now();
    }

    /// The virtual cursor, after adopting the system position if something
    /// other than our own posts moved the cursor.
    fn current_pos(&mut self) -> (f64, f64) {
        let now = Instant::now();
        while self
            .recent
            .front()
            .is_some_and(|&(at, _)| now.duration_since(at) > RECENT_WINDOW)
        {
            self.recent.pop_front();
        }
        let recent: Vec<(f64, f64)> = self.recent.iter().map(|&(_, p)| p).collect();
        let pos = reconcile(self.pos, cursor_location(), &recent);
        if pos != self.pos {
            self.pos = pos;
            self.recent.clear();
        }
        self.pos
    }

    /// Remembers a posted position until the system has surely applied it.
    fn record_post(&mut self, at: (f64, f64)) {
        if self.recent.len() == RECENT_MAX {
            self.recent.pop_front();
        }
        self.recent.push_back((Instant::now(), at));
    }

    fn post(&mut self, post: Post) {
        match post {
            Post::Key {
                kc,
                down,
                flags,
                repeat,
            } => {
                let Some(ev) = CGEvent::new_keyboard_event(Some(&self.source.0), kc, down) else {
                    warn!("CGEventCreateKeyboardEvent failed for keycode {kc:#x}");
                    return;
                };
                if repeat {
                    CGEvent::set_integer_value_field(
                        Some(&ev),
                        CGEventField::KeyboardEventAutorepeat,
                        1,
                    );
                }
                self.post_with_flags(&ev, flags);
            }
            Post::Flags { kc, flags } => {
                let Some(ev) = CGEvent::new_keyboard_event(Some(&self.source.0), kc, true) else {
                    warn!("CGEventCreateKeyboardEvent failed for keycode {kc:#x}");
                    return;
                };
                CGEvent::set_type(Some(&ev), CGEventType::FlagsChanged);
                self.post_with_flags(&ev, flags);
            }
            Post::ButtonUp(b) => {
                let at = self.current_pos();
                let (ty, button) = mouse_event_type(b, MouseAction::Up);
                self.post_mouse(ty, button, at, (0, 0), self.click_state[button_index(b)]);
            }
        }
    }

    /// Posts a mouse event carrying the current modifier flags.
    fn post_mouse(
        &mut self,
        ty: CGEventType,
        button: CGMouseButton,
        at: (f64, f64),
        delta: (i64, i64),
        click_state: i64,
    ) {
        let point = CGPoint::new(at.0, at.1);
        let Some(ev) = CGEvent::new_mouse_event(Some(&self.source.0), ty, point, button) else {
            warn!("CGEventCreateMouseEvent failed for {ty:?}");
            return;
        };
        let set = |field, value| CGEvent::set_integer_value_field(Some(&ev), field, value);
        set(CGEventField::MouseEventDeltaX, delta.0);
        set(CGEventField::MouseEventDeltaY, delta.1);
        set(CGEventField::MouseEventButtonNumber, i64::from(button.0));
        if click_state > 0 {
            set(CGEventField::MouseEventClickState, click_state);
        }
        self.post_with_flags(&ev, self.mods.flags());
        self.record_post(at);
    }

    fn post_with_flags(&self, ev: &CGEvent, flags: u64) {
        CGEvent::set_flags(Some(ev), CGEventFlags(flags));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(ev));
    }
}

/// Main display bounds in points.
fn main_display_bounds() -> Rectf {
    let r = CGDisplayBounds(CGMainDisplayID());
    Rectf {
        x: r.origin.x,
        y: r.origin.y,
        w: r.size.width,
        h: r.size.height,
    }
}

/// Where the system cursor is now (location of a fresh null event).
fn cursor_location() -> Option<(f64, f64)> {
    let ev = CGEvent::new(None)?;
    let p = CGEvent::location(Some(&ev));
    Some((p.x, p.y))
}

fn register_display_callback() {
    REGISTER_DISPLAY_CALLBACK.call_once(|| {
        // SAFETY: the callback is a plain function that only stores to a
        // static, and it ignores `user_info`, so a null pointer is fine.
        let err = unsafe {
            CGDisplayRegisterReconfigurationCallback(
                Some(on_display_reconfigured),
                std::ptr::null_mut(),
            )
        };
        if err != CGError::Success {
            warn!("CGDisplayRegisterReconfigurationCallback failed: {err:?}");
        }
    });
}

unsafe extern "C-unwind" fn on_display_reconfigured(
    _display: CGDirectDisplayID,
    _flags: CGDisplayChangeSummaryFlags,
    _user_info: *mut c_void,
) {
    DISPLAYS_CHANGED.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_core::modifiers::{FLAG_CAPS, FLAG_COMMAND, FLAG_SHIFT};

    const MBP: Rectf = Rectf {
        x: 0.0,
        y: 0.0,
        w: 1512.0,
        h: 982.0,
    };

    #[test]
    fn motion_reports_push_out_only_on_entry_edge() {
        let ((x, y), push) = step((0.0, 491.0), MBP, Side::Left, -5.0, 0.0);
        assert_eq!((x, y), (0.0, 491.0));
        let frac = push.expect("pushing out of the entry edge");
        assert!((frac - 491.0 / 981.0).abs() < 1e-6);

        let ((x, y), push) = step((1511.0, 100.0), MBP, Side::Left, 5.0, 0.0);
        assert_eq!(push, None, "the right edge is a wall");
        assert_eq!((x, y), (1511.0, 100.0));
    }

    #[test]
    fn step_overshooting_the_entry_edge_pushes_landing_on_it_does_not() {
        let ((x, _), push) = step((3.0, 10.0), MBP, Side::Left, -5.0, 0.0);
        assert_eq!(x, 0.0);
        assert!(push.is_some());

        let ((x, _), push) = step((5.0, 10.0), MBP, Side::Left, -5.0, 0.0);
        assert_eq!(x, 0.0);
        assert_eq!(push, None);
    }

    #[test]
    fn step_moves_freely_inside_and_clamps_at_walls() {
        assert_eq!(
            step((100.0, 100.0), MBP, Side::Left, 7.0, -3.0),
            ((107.0, 97.0), None)
        );
        assert_eq!(
            step((100.0, 2.0), MBP, Side::Left, 0.0, -9.0),
            ((100.0, 0.0), None)
        );
        assert_eq!(
            step((100.0, 980.0), MBP, Side::Left, 0.0, 9.0),
            ((100.0, 981.0), None)
        );
    }

    #[test]
    fn step_other_entry_edges() {
        let ((_, y), push) = step((756.0, 981.0), MBP, Side::Bottom, 0.0, 4.0);
        assert_eq!(y, 981.0);
        assert!((push.unwrap() - 756.0 / 1511.0).abs() < 1e-6);
        let (_, push) = step((1511.0, 0.0), MBP, Side::Right, 1.0, 0.0);
        assert_eq!(push, Some(0.0));
        let (_, push) = step((0.0, 0.0), MBP, Side::Top, 0.0, -1.0);
        assert_eq!(push, Some(0.0));
        let (_, push) = step((0.0, 0.0), MBP, Side::Top, -1.0, 0.0);
        assert_eq!(
            push, None,
            "the left edge is a wall when entering from the top"
        );
    }

    #[test]
    fn key_plan_modifiers_post_flags_changed() {
        let (mut held, mut mods) = (Held::new(), ModifierState::new());
        assert_eq!(
            key_plan(0x37, true, &mut held, &mut mods),
            Some(Post::Flags {
                kc: 0x37,
                flags: FLAG_COMMAND
            })
        );
        assert_eq!(
            key_plan(0x00, true, &mut held, &mut mods),
            Some(Post::Key {
                kc: 0x00,
                down: true,
                flags: FLAG_COMMAND,
                repeat: false
            })
        );
        assert_eq!(
            key_plan(0x00, false, &mut held, &mut mods),
            Some(Post::Key {
                kc: 0x00,
                down: false,
                flags: FLAG_COMMAND,
                repeat: false
            })
        );
        assert_eq!(
            key_plan(0x37, false, &mut held, &mut mods),
            Some(Post::Flags { kc: 0x37, flags: 0 })
        );
    }

    #[test]
    fn key_plan_repeat_down_is_autorepeat() {
        let (mut held, mut mods) = (Held::new(), ModifierState::new());
        key_plan(0x00, true, &mut held, &mut mods);
        assert_eq!(
            key_plan(0x00, true, &mut held, &mut mods),
            Some(Post::Key {
                kc: 0x00,
                down: true,
                flags: 0,
                repeat: true
            })
        );
    }

    #[test]
    fn key_plan_ignores_modifier_repeats() {
        let (mut held, mut mods) = (Held::new(), ModifierState::new());
        key_plan(0x38, true, &mut held, &mut mods);
        assert_eq!(key_plan(0x38, true, &mut held, &mut mods), None);
        // A repeated Caps Lock down must not toggle Caps Lock again.
        assert_eq!(
            key_plan(0x39, true, &mut held, &mut mods),
            Some(Post::Flags {
                kc: 0x39,
                flags: FLAG_SHIFT | FLAG_CAPS
            })
        );
        assert_eq!(key_plan(0x39, true, &mut held, &mut mods), None);
        assert_eq!(mods.flags(), FLAG_SHIFT | FLAG_CAPS);
    }

    #[test]
    fn release_plan_orders_keys_modifiers_final_flags_then_buttons() {
        let (mut held, mut mods) = (Held::new(), ModifierState::new());
        for kc in [0x39, 0x39, 0x37, 0x38, 0x00, 0x01] {
            key_plan(kc, true, &mut held, &mut mods);
        }
        held.button(MouseButton::Left, true);
        held.button(MouseButton::Middle, true);
        let (keys, buttons) = held.drain();
        let plan = release_plan(&keys, &buttons, &mut mods);
        let all = FLAG_CAPS | FLAG_COMMAND | FLAG_SHIFT;
        assert_eq!(
            plan,
            vec![
                Post::Key {
                    kc: 0x00,
                    down: false,
                    flags: all,
                    repeat: false
                },
                Post::Key {
                    kc: 0x01,
                    down: false,
                    flags: all,
                    repeat: false
                },
                Post::Flags {
                    kc: 0x37,
                    flags: FLAG_CAPS | FLAG_SHIFT
                },
                Post::Flags {
                    kc: 0x38,
                    flags: FLAG_CAPS
                },
                Post::Flags {
                    kc: 0x39,
                    flags: FLAG_CAPS
                },
                Post::Flags {
                    kc: 0x38,
                    flags: FLAG_CAPS
                },
                Post::ButtonUp(MouseButton::Left),
                Post::ButtonUp(MouseButton::Middle),
            ]
        );
        assert_eq!(mods.flags(), FLAG_CAPS, "Caps Lock stays as it is");
    }

    #[test]
    fn release_plan_with_nothing_held_still_clears_flags() {
        let mut mods = ModifierState::new();
        assert_eq!(
            release_plan(&[], &[], &mut mods),
            vec![Post::Flags { kc: 0x38, flags: 0 }]
        );
    }

    #[test]
    fn mouse_event_types_per_button() {
        use MouseAction::*;
        let t = |b, a| mouse_event_type(b, a);
        assert_eq!(
            t(MouseButton::Left, Down),
            (CGEventType::LeftMouseDown, CGMouseButton::Left)
        );
        assert_eq!(
            t(MouseButton::Left, Up),
            (CGEventType::LeftMouseUp, CGMouseButton::Left)
        );
        assert_eq!(
            t(MouseButton::Left, Drag),
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        );
        assert_eq!(
            t(MouseButton::Right, Down),
            (CGEventType::RightMouseDown, CGMouseButton::Right)
        );
        assert_eq!(
            t(MouseButton::Right, Up),
            (CGEventType::RightMouseUp, CGMouseButton::Right)
        );
        assert_eq!(
            t(MouseButton::Right, Drag),
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        );
        assert_eq!(
            t(MouseButton::Middle, Down),
            (CGEventType::OtherMouseDown, CGMouseButton(2))
        );
        assert_eq!(
            t(MouseButton::Back, Up),
            (CGEventType::OtherMouseUp, CGMouseButton(3))
        );
        assert_eq!(
            t(MouseButton::Forward, Drag),
            (CGEventType::OtherMouseDragged, CGMouseButton(4))
        );
    }

    #[test]
    fn motion_is_dragged_while_a_button_is_held() {
        assert_eq!(
            motion_event_type(None),
            (CGEventType::MouseMoved, CGMouseButton::Left)
        );
        assert_eq!(
            motion_event_type(Some(MouseButton::Right)),
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        );
        assert_eq!(
            motion_event_type(Some(MouseButton::Back)),
            (CGEventType::OtherMouseDragged, CGMouseButton(3))
        );
    }

    #[test]
    fn reconcile_keeps_virtual_position_when_system_reading_is_stale() {
        let recent = [(100.0, 50.0), (103.0, 50.0)];
        assert_eq!(
            reconcile((106.0, 50.0), Some((103.0, 50.0)), &recent),
            (106.0, 50.0)
        );
        assert_eq!(
            reconcile((106.0, 50.0), Some((100.3, 50.2)), &recent),
            (106.0, 50.0)
        );
        assert_eq!(
            reconcile((106.0, 50.0), Some((106.0, 50.0)), &recent),
            (106.0, 50.0)
        );
    }

    #[test]
    fn reconcile_adopts_a_foreign_system_position() {
        let recent = [(100.0, 50.0), (103.0, 50.0)];
        assert_eq!(
            reconcile((106.0, 50.0), Some((400.0, 300.0)), &recent),
            (400.0, 300.0)
        );
        assert_eq!(
            reconcile((106.0, 50.0), Some((101.5, 50.0)), &recent),
            (101.5, 50.0)
        );
        assert_eq!(
            reconcile((106.0, 50.0), Some((90.0, 50.0)), &[]),
            (90.0, 50.0)
        );
    }

    #[test]
    fn reconcile_without_a_system_reading_keeps_virtual() {
        assert_eq!(reconcile((106.0, 50.0), None, &[(1.0, 1.0)]), (106.0, 50.0));
    }

    #[test]
    fn injector_is_send_and_starts_with_nothing_held() {
        fn assert_send<T: Send>() {}
        assert_send::<Injector>();
        let inj = Injector::new().expect("event source");
        assert!(!inj.buttons_held());
        assert!(inj.bounds.w > 0.0 && inj.bounds.h > 0.0);
    }

    /// Moves the real cursor a little and back. Needs Accessibility
    /// permission for the test binary: `cargo test -p cursorjump -- --ignored`.
    #[test]
    #[ignore]
    fn smoke_wiggle_posts_real_events() {
        let mut inj = Injector::new().expect("event source");
        for _ in 0..20 {
            inj.motion(5, 0);
            std::thread::sleep(Duration::from_millis(10));
        }
        for _ in 0..20 {
            inj.motion(-5, 0);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn scroll_wheels_follow_wire_convention() {
        // Wire: dy > 0 scrolls up, dx > 0 scrolls right.
        // CG: wheel1 > 0 scrolls up, wheel2 > 0 scrolls left.
        assert_eq!(scroll_wheels(0, 48), (48, 0));
        assert_eq!(scroll_wheels(0, -48), (-48, 0));
        assert_eq!(scroll_wheels(30, 0), (0, -30));
        assert_eq!(scroll_wheels(-30, 5), (5, 30));
    }
}
