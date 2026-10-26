//! The Router's side of a PC drag riding on this Mac (R47): arming and
//! pressing for it, the user's release (at once or held), cancelling it,
//! and its session's end.

use std::time::Instant;

use cj_core::dnd::DragId;
use cj_core::mac_state::MacEvent;
use cj_core::proto::MouseButton;
use tracing::{info, warn};

use super::{CatchPort, Crossing, DragPort, Inject, Router};

/// The PC's drag riding on this Mac, with the left button down for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Ride {
    pub(super) drag: DragId,
    pub(super) crossing: Crossing,
    /// The user let go and the up is held until the items are complete.
    pub(super) hold: Option<Hold>,
}

impl Ride {
    pub(super) fn is(&self, drag: DragId, crossing: Crossing) -> bool {
        self.drag == drag && self.crossing == crossing
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Hold {
    pub(super) next_jiggle: Instant,
    /// The last jiggle left the cursor 1 pt off.
    pub(super) nudged: bool,
}

impl<I: Inject, D: DragPort, C: CatchPort> Router<I, D, C> {
    /// `BeginDrag`: arm the drag under the cursor, then press for it.
    pub(super) fn begin_drag(&mut self, drag: DragId) {
        if self.ride.is_some_and(|r| r.drag == drag) {
            return; // Already riding.
        }
        let items = match self.offer.take() {
            Some((d, items)) if d == drag => items,
            other => {
                self.offer = other;
                Vec::new()
            }
        };
        // A ride cancelled by this crossing's Enter leaves the button down
        // (no ReleaseAllInjected comes): let go before pressing again.
        if self.inj.button_held(MouseButton::Left) {
            self.inj.button(MouseButton::Left, false);
        }
        let at = self.inj.cursor();
        if let Some(crossing) = self.drags.prepare(drag, items, at) {
            self.inj.button(MouseButton::Left, true);
            self.left_is_users = false;
            self.ride = Some(Ride {
                drag,
                crossing,
                hold: None,
            });
        } else {
            // Nothing is pressed, so nothing stays held or stranded.
            warn!(
                drag,
                "the PC's drag could not start here; it ends undropped"
            );
            self.machine.on(MacEvent::DragEnded { drag });
        }
    }

    /// The PC's left-up while the drag rides: drop now, or hold the up.
    pub(super) fn let_go(&mut self, ride: Ride) {
        let (drag, at) = (ride.drag, self.inj.cursor());
        self.drags.released(drag, at);
        if self.drags.may_release(drag, at) {
            self.drop_now(drag);
            return;
        }
        info!(drag, "holding the drop until every item is here");
        self.ride = Some(Ride {
            hold: Some(Hold {
                next_jiggle: Instant::now() + self.jiggle_every,
                nudged: false,
            }),
            ..ride
        });
    }

    /// Posts the up that drops the drag: it is over as far as input goes.
    pub(super) fn drop_now(&mut self, drag: DragId) {
        self.let_go_of_left();
        self.machine.on(MacEvent::DragEnded { drag });
    }

    /// Ends the ride, letting go of the left button: back on the drop spot
    /// first if the last jiggle left the cursor off it.
    pub(super) fn let_go_of_left(&mut self) {
        if let Some(Ride {
            hold: Some(Hold { nudged: true, .. }),
            ..
        }) = self.ride
        {
            self.inj.jiggle(false);
        }
        self.ride = None;
        self.inj.button(MouseButton::Left, false);
    }

    /// Cancels `drag` from this side. The source first, so no session can
    /// start after it; then Esc, only if a session is live (a stray Esc
    /// would reach the user's front-most app). A held up goes out too;
    /// while the drag still rides, the PC holds the button and sends the
    /// up itself (or `carry_out` lets go when the PC moved on).
    pub(super) fn abort_drag(&mut self, drag: DragId) {
        let live = self.drags.cancel(drag);
        let Some(r) = self.ride.filter(|r| r.drag == drag) else {
            return; // Dropped already, or never here: its fetches stop.
        };
        info!(drag, live, "cancelling the drag from the PC");
        if live {
            self.escape();
        }
        if r.hold.is_some() {
            self.let_go_of_left();
        } else {
            self.ride = None;
        }
        self.machine.on(MacEvent::DragEnded { drag });
    }

    /// The session of `drag`'s `crossing` ended (dropped, or Esc from the
    /// PC's keyboard). An earlier crossing's end leaves a later ride be.
    pub(super) fn session_ended(&mut self, drag: DragId, crossing: Crossing) {
        let Some(r) = self.ride.filter(|r| r.is(drag, crossing)) else {
            return;
        };
        if r.hold.is_some() {
            // Never leave a hold with the button still down.
            self.let_go_of_left();
        } else {
            // The PC still holds the button: it counts as held until the
            // PC's up (Task 17 note b), so pushing out is a wall.
            self.ride = None;
        }
        self.machine.on(MacEvent::DragEnded { drag });
    }
}
