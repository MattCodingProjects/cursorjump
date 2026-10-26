//! The Router's side of the edge catcher (Task 16): the `CatchPort`
//! seam, the push-out that carries the strip's drag or waits for a late
//! one (R48, R48a), and where the strip should be.

use std::time::Instant;

use cj_core::dnd::DragId;
use cj_core::mac_state::MacEvent;
use cj_core::proto::{MouseButton, Side};
use tracing::info;

use super::{DragPort, Inject, InjectEv, Router};
use crate::mac::dnd::catcher::in_catch_zone;

/// What the thread needs from the Mac's edge catcher
/// (`dnd::catcher::Catcher`); a trait so the routing can be tested
/// without AppKit.
pub trait CatchPort {
    /// Some(edge) shows the strip along that edge; None hides it. Called
    /// only when that changes.
    fn zone(&mut self, show: Option<Side>);
    /// The user's left button went down here: remember the drag
    /// pasteboard's state, for `drag_began`.
    fn pressed(&mut self);
    /// Whether a drag began since `pressed` (R48a): the drag pasteboard
    /// changed, though the strip may not have seen the drag yet.
    fn drag_began(&mut self) -> bool;
    /// A push-out with only the left button held is about to post the
    /// release, over the strip: the strip's drag, whose drop there is then
    /// taken; None if there is none.
    fn push_out(&mut self) -> Option<DragId>;
    /// Opens the late window (R48): a drag that drops on the strip before
    /// `end_late_wait` is taken.
    fn await_late(&mut self);
    /// The late wait is over: the drag that dropped on the strip during
    /// it, if any; a later drop is refused. `carry`: whether a `Leave`
    /// carries that drag; if not, its offer is withdrawn.
    fn end_late_wait(&mut self, carry: bool) -> Option<DragId>;
}

/// The type of a Router without a catcher (it has none to give).
pub enum NoCatcher {}

impl CatchPort for NoCatcher {
    fn zone(&mut self, _show: Option<Side>) {
        match *self {}
    }
    fn pressed(&mut self) {
        match *self {}
    }
    fn drag_began(&mut self) -> bool {
        match *self {}
    }
    fn push_out(&mut self) -> Option<DragId> {
        match *self {}
    }
    fn await_late(&mut self) {
        match *self {}
    }
    fn end_late_wait(&mut self, _carry: bool) -> Option<DragId> {
        match *self {}
    }
}

/// Whether `b` is another button than the left one; exhaustive, so a new
/// button must be decided here.
pub(super) const fn not_left(b: MouseButton) -> bool {
    match b {
        MouseButton::Left => false,
        MouseButton::Right | MouseButton::Middle | MouseButton::Back | MouseButton::Forward => true,
    }
}

/// Every button, for `not_left`; a test checks that none is missing.
pub(super) const BUTTONS: [MouseButton; 5] = [
    MouseButton::Left,
    MouseButton::Right,
    MouseButton::Middle,
    MouseButton::Back,
    MouseButton::Forward,
];

impl<I: Inject, D: DragPort, C: CatchPort> Router<I, D, C> {
    /// The cursor pushes out through the entry edge at `frac`. With only
    /// the user's left button held and a catcher here, the release goes
    /// out over the strip carrying the strip's drag; or, with none there
    /// yet, only if a drag began since the press (R48a), and then the
    /// leave waits for a late drop. Otherwise the edge is a wall.
    pub(super) fn push_out(&mut self, sid: u64, frac: f32) -> Vec<InjectEv> {
        let buttons_held = self.inj.buttons_held();
        let catching = self.catcher.is_some() && self.users_left_alone();
        let edge = self.machine.entry_edge().filter(|_| catching);
        let wall = MacEvent::PushOut {
            frac,
            buttons_held,
            offer: None,
        };
        let (Some(edge), Some(c)) = (edge, self.catcher.as_mut()) else {
            return self.feed(sid, wall);
        };
        let offer = c.push_out();
        let late = offer.is_none() && c.drag_began();
        if offer.is_none() && !late {
            // A window drag or a text selection: it stays on the Mac.
            return self.feed(sid, wall);
        }
        // Up before the release goes out, and a while after it.
        self.linger = Some((Instant::now() + self.late_wait, edge));
        self.update_zone(Instant::now());
        if let Some(drag) = offer {
            info!(drag, "pushing out with the strip's drag");
            return self.feed(
                sid,
                MacEvent::PushOut {
                    frac,
                    buttons_held,
                    offer,
                },
            );
        }
        info!(
            ms = self.late_wait.as_millis() as u64,
            "pushing out with the left button held: a drag began that the strip hasn't seen; \
             releasing and waiting for it"
        );
        self.feed(sid, MacEvent::PushOutLate { frac })
    }

    /// Whether the only button held is the left one, pressed by the user
    /// here (not for a ride, nor left down after one).
    pub(super) fn users_left_alone(&self) -> bool {
        self.left_is_users
            && self.ride.is_none()
            && self.inj.button_held(MouseButton::Left)
            && !BUTTONS
                .iter()
                .any(|&b| not_left(b) && self.inj.button_held(b))
    }

    /// The late wait is over: leave, with the drag the catcher caught.
    pub(super) fn end_late_wait(&mut self, sid: u64) -> Vec<InjectEv> {
        self.late_until = None;
        let drag = self.catcher.as_mut().and_then(|c| c.end_late_wait(true));
        self.feed(sid, MacEvent::LateDrop { drag })
    }

    /// Tells the catcher where the strip should be, if that changed: on
    /// the entry edge while the user drags with only the left button near
    /// it, or lingering after a push-out; nowhere otherwise.
    pub(super) fn update_zone(&mut self, now: Instant) {
        if self.catcher.is_none() {
            return;
        }
        if self.linger.is_some_and(|(until, _)| now >= until) {
            self.linger = None;
        }
        let near = match self.machine.entry_edge() {
            Some(edge) if self.machine.controlled() && self.users_left_alone() => {
                // Where `motion` just put it: no second system query.
                let (bounds, at) = (self.inj.bounds(), self.inj.pos());
                in_catch_zone(bounds, edge, at).then_some(edge)
            }
            _ => None,
        };
        let want = near.or(self.linger.map(|(_, edge)| edge));
        if want != self.zone {
            self.zone = want;
            if let Some(c) = self.catcher.as_mut() {
                c.zone(want);
            }
        }
    }
}
