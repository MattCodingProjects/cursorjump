//! The injection thread (`cj-inject`). It owns the `Injector` and the
//! `MacMachine`, and gets every input-related message in arrival order
//! through one channel, straight from the session readers: input never
//! waits on the app's loop.
//!
//! Input that arrives while the PC is not controlling this Mac is dropped,
//! except the up of a key or button the Injector still holds: a PC key-down
//! can be queued ahead of this Mac's own `Leave`, and the PC then never
//! sends its up, so injecting the down would leave it stuck.
//!
//! A PC file drag riding along (`Enter { drag }`) becomes a Mac drag
//! session (`dnd::source`, R47):
//! - `BeginDrag`: the source arms the drag under the cursor (`prepare`,
//!   with the items of the latest `DragOffer`), then the left button goes
//!   down; it counts as held from then on, like any button.
//! - The PC's left-up is the user letting go (`released`). The up is posted
//!   at once if the source says the drop may go ahead (`may_release`);
//!   otherwise it is held: motion and scroll are ignored, the cursor
//!   jiggles 1 pt every [`JIGGLE_EVERY`], and the up goes out when the
//!   source reports every item complete (`InjectMsg::DragReady`).
//! - Cancelling (`CancelDrag` from the machine, `InjectMsg::CancelDrag`, a
//!   failed item) cancels the source first, so no session can start after
//!   it; then, only if a session is live, posts Esc, which ends it without
//!   a drop; then lets go of the button unless the PC still holds it.
//! - Each crossing of a drag has its own token: news from an earlier
//!   crossing of the same drag (it went home and came back) never touches
//!   the ride of a later one.
//! - What the source emits on this thread while it handles a message waits
//!   until the thread has reported (`DragPort::hold`): the `SendLeave` of a
//!   drag going home reaches the app before that drag's `Ended`.
//!
//! A Mac drag going to the PC is caught at the entry edge
//! (`dnd::catcher`, Task 16):
//! - The strip shows (`CatchPort::zone`, on changes only) while the PC
//!   controls this Mac, the user holds only the left button (pressed with
//!   the PC's mouse, not for a ride), and the cursor is within
//!   `CATCH_ZONE_PT` (48 pt) of the entry edge.
//! - A push-out then (`CatchPort::push_out`) carries the strip's drag:
//!   the release goes out and drops it there, and the `Leave` carries it.
//! - With no drag there yet, the edge is a wall (a window drag, a text
//!   selection), unless a drag began since the press that the strip
//!   hasn't seen (`CatchPort::drag_began`: the drag pasteboard changed,
//!   R48a). Then the release goes out, and the `Leave` waits up to
//!   [`LATE_DRAG_WAIT`] for that drag to drop on the strip
//!   (`InjectMsg::LateDrop`, R48). The wait is a deadline of the thread's
//!   loop, never a sleep: input keeps flowing (and is dropped, the PC no
//!   longer controlling this Mac).
//! - After either, the strip stays up for [`LATE_DRAG_WAIT`], so a late
//!   drag always ends on it.
//! - Without a catcher, holding a button at the edge is a wall, as before.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cj_core::dnd::{DragId, DragItem};
use cj_core::mac_state::{MacAction, MacEvent, MacMachine};
use cj_core::proto::{MouseButton, Side};
use crossbeam_channel::RecvTimeoutError;
use tracing::{error, info, warn};

use super::dnd::catcher::LATE_DRAG_WAIT;
use super::dnd::source::KVK_ESCAPE;
use super::inject::{Injector, Rectf};
use super::power;

mod catch;
mod ride;
#[cfg(test)]
pub(super) mod tests;

pub use catch::{CatchPort, NoCatcher};
use ride::Ride;

/// The "no Accessibility" bounce is logged at most this often.
const NO_ACCESS_LOG_EVERY: Duration = Duration::from_secs(10);
/// While a drop is held, the cursor jiggles this often to keep the drag
/// session alive (spike 1b, run 7).
pub const JIGGLE_EVERY: Duration = Duration::from_millis(500);

/// What the thread needs from the Injector; a trait so the routing can be
/// tested without posting real events.
pub trait Inject {
    fn wake_display(&mut self);
    fn warp_to(&mut self, edge: Side, frac: f32);
    /// The fraction along the entry edge when the move pushes out through it.
    fn motion(&mut self, dx: i16, dy: i16) -> Option<f32>;
    fn scroll(&mut self, dx: i32, dy: i32);
    fn button(&mut self, b: MouseButton, down: bool);
    fn key(&mut self, mac_keycode: u16, down: bool);
    fn release_all(&mut self);
    fn buttons_held(&self) -> bool;
    fn button_held(&self, b: MouseButton) -> bool;
    fn key_held(&self, mac_keycode: u16) -> bool;
    /// Where the cursor is now, as a fraction along the entry edge.
    fn entry_frac(&self) -> f32;
    /// Where the cursor is now, in global points (origin top left).
    fn cursor(&mut self) -> (f64, f64);
    /// Where the last motion or warp put the cursor, without asking the
    /// system again (global points).
    fn pos(&self) -> (f64, f64);
    /// The main display's bounds, in global points.
    fn bounds(&self) -> Rectf;
    /// A dragged event of the held button 1 pt off the cursor (`nudge`) or
    /// back on it; the cursor itself stays put.
    fn jiggle(&mut self, nudge: bool);
}

/// One crossing of a drag onto this Mac: a drag that went home and came
/// back has a new one.
pub type Crossing = u64;

/// What the thread needs from the Mac's drag source
/// (`dnd::source::DragSource`); a trait so the routing can be tested
/// without AppKit.
pub trait DragPort {
    /// Arms drag `drag` under `at` (global points): this crossing's token,
    /// or None if it can't start.
    fn prepare(&mut self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> Option<Crossing>;
    /// The user let go while the drag rode here, at `at`.
    fn released(&mut self, drag: DragId, at: (f64, f64));
    /// Whether the up may be posted now, over `at`.
    fn may_release(&mut self, drag: DragId, at: (f64, f64)) -> bool;
    /// The drag is over here without a drop: stop its fetches. True if its
    /// session is live, so that Esc must end it; a session that is still
    /// starting asks for its Esc itself (`InjectMsg::EscapeSession`).
    fn cancel(&mut self, drag: DragId) -> bool;
    /// Whether the session of `drag`'s `crossing` is live: started and not
    /// ended, so that Esc reaches it.
    fn session_live(&mut self, drag: DragId, crossing: Crossing) -> bool;
    /// The thread is about to handle a message: what the source emits on
    /// it meanwhile waits for `release`, which comes once the thread has
    /// reported its own events (a `SendLeave` before an `Ended`).
    fn hold(&mut self) {}
    fn release(&mut self) {}
}

/// For a Mac without a drag source: no PC drag starts here.
#[cfg_attr(
    all(not(test), not(debug_assertions)),
    expect(
        dead_code,
        reason = "the tests and the catcher smoke run without a drag source"
    )
)]
pub struct NoDrags;

impl DragPort for NoDrags {
    fn prepare(
        &mut self,
        _drag: DragId,
        _items: Vec<DragItem>,
        _at: (f64, f64),
    ) -> Option<Crossing> {
        None
    }
    fn released(&mut self, _drag: DragId, _at: (f64, f64)) {}
    fn may_release(&mut self, _drag: DragId, _at: (f64, f64)) -> bool {
        true
    }
    fn cancel(&mut self, _drag: DragId) -> bool {
        false
    }
    fn session_live(&mut self, _drag: DragId, _crossing: Crossing) -> bool {
        false
    }
}

impl Inject for Injector {
    fn wake_display(&mut self) {
        power::wake_display();
    }
    fn warp_to(&mut self, edge: Side, frac: f32) {
        Injector::warp_to(self, edge, frac);
    }
    fn motion(&mut self, dx: i16, dy: i16) -> Option<f32> {
        Injector::motion(self, dx, dy)
    }
    fn scroll(&mut self, dx: i32, dy: i32) {
        Injector::scroll(self, dx, dy);
    }
    fn button(&mut self, b: MouseButton, down: bool) {
        Injector::button(self, b, down);
    }
    fn key(&mut self, mac_keycode: u16, down: bool) {
        Injector::key(self, mac_keycode, down);
    }
    fn release_all(&mut self) {
        Injector::release_all(self);
    }
    fn buttons_held(&self) -> bool {
        Injector::buttons_held(self)
    }
    fn button_held(&self, b: MouseButton) -> bool {
        Injector::button_held(self, b)
    }
    fn key_held(&self, mac_keycode: u16) -> bool {
        Injector::key_held(self, mac_keycode)
    }
    fn entry_frac(&self) -> f32 {
        Injector::entry_frac(self)
    }
    fn cursor(&mut self) -> (f64, f64) {
        Injector::cursor(self)
    }
    fn pos(&self) -> (f64, f64) {
        Injector::pos(self)
    }
    fn bounds(&self) -> Rectf {
        Injector::bounds(self)
    }
    fn jiggle(&mut self, nudge: bool) {
        Injector::jiggle(self, nudge);
    }
}

/// Input from the PC, as the session readers pass it on.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// `drag`: the PC's file drag riding along (its `DragOffer` came first).
    Enter {
        edge: Side,
        frac: f32,
        drag: Option<DragId>,
    },
    Motion {
        dx: i16,
        dy: i16,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Key {
        mac_keycode: u16,
        down: bool,
    },
    /// The PC took the cursor back.
    ReleaseAll,
    /// The PC's next crossing carries this drag. A copy of the `DragOffer`
    /// the loop gets too, sent by the control reader so it is here, in
    /// order, before that crossing's `Enter`.
    DragOffer {
        drag: DragId,
        items: Vec<DragItem>,
    },
}

/// What the thread is sent. Input is tagged with its session, so anything
/// a replaced session's readers still deliver is dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum InjectMsg {
    /// Session `sid`'s input follows; a previous session ends first.
    Begin {
        sid: u64,
    },
    Input {
        sid: u64,
        input: Input,
    },
    /// Session `sid` is gone: release everything.
    End {
        sid: u64,
    },
    /// The Accessibility permission was revoked: if the PC is controlling
    /// this Mac, send its cursor back.
    AccessLost,
    /// From the drag source (`MacDndEv::inject_msg`): every item of
    /// `drag`'s `crossing` is complete, so a held drop may go ahead.
    DragReady {
        drag: DragId,
        crossing: Crossing,
    },
    /// From the drag source: an item of `drag`'s `crossing` failed; the
    /// drag is cancelled.
    DragFailed {
        drag: DragId,
        crossing: Crossing,
    },
    /// From the drag source: the session of `drag`'s `crossing` ended,
    /// dropped or not.
    DragEnded {
        drag: DragId,
        crossing: Crossing,
    },
    /// From the drag source: the session of `drag`'s `crossing` started as
    /// its drag was being cancelled: post Esc so that it ends undropped,
    /// if it is still on by now.
    EscapeSession {
        drag: DragId,
        crossing: Crossing,
    },
    /// From the catcher (`MacDndEv::LateDrop`): `drag` dropped on the
    /// strip while a push-out waits for a late drag: leave with it now.
    LateDrop {
        drag: DragId,
    },
    /// Cancel `drag` (the pill's ✕, the PC's `DragCancel`): Esc, then the
    /// button goes up if the PC doesn't hold it any more.
    CancelDrag {
        drag: DragId,
    },
    /// Release everything and stop the thread.
    Quit,
}

/// Records whether this process may post events (`has`, from
/// `AccessState::ready`) for the injection thread, which reads `access`
/// instead of asking the system (a TCC query, milliseconds) on every Enter.
/// When the permission goes away, tells the thread. True if it changed.
pub fn update_access(
    access: &AtomicBool,
    has: bool,
    inject: &crossbeam_channel::Sender<InjectMsg>,
) -> bool {
    if access.swap(has, Ordering::Relaxed) == has {
        return false;
    }
    if !has {
        let _ = inject.send(InjectMsg::AccessLost);
    }
    true
}

/// Lets a repeated log line through at most once per period, counting the
/// lines held back meanwhile.
pub struct LogLimit {
    every: Duration,
    last: Option<Instant>,
    suppressed: u32,
}

impl LogLimit {
    pub fn new(every: Duration) -> Self {
        LogLimit {
            every,
            last: None,
            suppressed: 0,
        }
    }

    /// Some(lines held back since the last one) if a line may be logged at `now`.
    pub fn allow(&mut self, now: Instant) -> Option<u32> {
        if self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) < self.every)
        {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }
}

/// What the thread reports to the app's loop.
#[derive(Debug, Clone, PartialEq)]
pub enum InjectEv {
    /// Send `Control::Leave` with `frac` on session `sid`; `drag`: the
    /// drag it carries, the PC's going home or one from the catcher.
    SendLeave {
        sid: u64,
        frac: f32,
        drag: Option<DragId>,
    },
    /// The PC started (true) or stopped controlling this Mac.
    Controlled { sid: u64, on: bool },
    /// The thread has released everything and exited.
    Stopped,
}

/// Routes messages to the Injector through the `MacMachine`.
pub struct Router<I: Inject, D: DragPort, C: CatchPort = NoCatcher> {
    inj: I,
    drags: D,
    /// The edge catcher, if this Mac has one.
    catcher: Option<C>,
    machine: MacMachine,
    /// The session whose input is accepted.
    sid: Option<u64>,
    /// Whether events may be posted (Accessibility), kept up to date by the loop.
    access: Arc<AtomicBool>,
    /// For the "no Accessibility" bounce, which serves races (an Enter sent
    /// before the PC got `MacReady { ready: false }`) and can repeat.
    no_access_log: LogLimit,
    /// The latest `DragOffer` from the PC, until its crossing.
    offer: Option<(DragId, Vec<DragItem>)>,
    ride: Option<Ride>,
    /// How often a held drop jiggles.
    jiggle_every: Duration,
    /// The held left button was pressed by the PC's own left-down (the
    /// user drags something here), not for a ride.
    left_is_users: bool,
    /// Where the strip is, as last told to the catcher.
    zone: Option<Side>,
    /// After a push-out with only the left button held, the strip stays
    /// up on this edge until then (R48).
    linger: Option<(Instant, Side)>,
    /// Until when a push-out waits for a late drop before leaving (R48).
    late_until: Option<Instant>,
    /// How long that wait and the strip's linger last.
    late_wait: Duration,
    /// The drag of a crossing bounced for want of Accessibility, while
    /// the bounce runs: its `BeginDrag` and `CancelDrag` do nothing, as no
    /// session ever starts for it (Task 17 note a).
    no_session: Option<DragId>,
}

#[cfg(test)]
impl<I: Inject, D: DragPort> Router<I, D> {
    /// A router without a catcher.
    pub fn new(inj: I, drags: D, access: Arc<AtomicBool>) -> Self {
        Router::with_catcher(inj, drags, None, access)
    }
}

impl<I: Inject, D: DragPort, C: CatchPort> Router<I, D, C> {
    pub fn with_catcher(inj: I, drags: D, catcher: Option<C>, access: Arc<AtomicBool>) -> Self {
        Router {
            inj,
            drags,
            catcher,
            machine: MacMachine::new(),
            sid: None,
            access,
            no_access_log: LogLimit::new(NO_ACCESS_LOG_EVERY),
            offer: None,
            ride: None,
            jiggle_every: JIGGLE_EVERY,
            left_is_users: false,
            zone: None,
            linger: None,
            late_until: None,
            late_wait: LATE_DRAG_WAIT,
            no_session: None,
        }
    }

    #[cfg(test)]
    fn with_jiggle_every(mut self, every: Duration) -> Self {
        self.jiggle_every = every;
        self
    }

    #[cfg(test)]
    fn with_late_wait(mut self, wait: Duration) -> Self {
        self.late_wait = wait;
        self
    }

    /// After a panic: cancels a drag still riding (Esc ends its session
    /// undropped), then always lets go of every key and button the
    /// Injector holds, even if the cancel panics too.
    pub fn let_go_after_panic(&mut self) {
        if let Some(r) = self.ride.take() {
            let cancelled = catch_unwind(AssertUnwindSafe(|| {
                if self.drags.cancel(r.drag) {
                    self.escape();
                }
            }));
            if cancelled.is_err() {
                error!(drag = r.drag, "cancelling the drag after the panic failed");
            }
        }
        self.inj.release_all();
    }

    /// When `tick` must next run: the next jiggle of a held drop, the end
    /// of a late wait, or of the strip's linger.
    pub fn next_tick(&self) -> Option<Instant> {
        let jiggle = self.ride.and_then(|r| r.hold).map(|h| h.next_jiggle);
        [jiggle, self.late_until, self.linger.map(|(until, _)| until)]
            .into_iter()
            .flatten()
            .min()
    }

    /// Jiggles the cursor of a held drop, ends a late wait and hides a
    /// lingering strip, when due; returns what to report to the loop.
    pub fn tick(&mut self, now: Instant) -> Vec<InjectEv> {
        if let Some(Ride {
            hold: Some(hold), ..
        }) = &mut self.ride
            && now >= hold.next_jiggle
        {
            hold.nudged = !hold.nudged;
            hold.next_jiggle = now + self.jiggle_every;
            let nudge = hold.nudged;
            self.inj.jiggle(nudge);
        }
        let out = match self.sid {
            Some(sid) if self.late_until.is_some_and(|t| now >= t) => {
                info!("no late drop came: leaving without a drag");
                self.end_late_wait(sid)
            }
            _ => Vec::new(),
        };
        self.update_zone(now);
        out
    }

    /// Handles one message; returns what to report to the loop.
    pub fn handle(&mut self, msg: InjectMsg) -> Vec<InjectEv> {
        let out = self.handle_msg(msg);
        self.update_zone(Instant::now());
        out
    }

    fn handle_msg(&mut self, msg: InjectMsg) -> Vec<InjectEv> {
        match msg {
            InjectMsg::Begin { sid } => {
                let out = self.end_session();
                self.sid = Some(sid);
                out
            }
            InjectMsg::Input { sid, input } if self.sid == Some(sid) => self.input(sid, input),
            InjectMsg::End { sid } if self.sid == Some(sid) => self.end_session(),
            InjectMsg::Input { .. } | InjectMsg::End { .. } => Vec::new(),
            InjectMsg::AccessLost => match self.sid {
                Some(sid) if self.machine.controlled() => {
                    warn!("Accessibility permission revoked: sending the cursor back to the PC");
                    let frac = self.inj.entry_frac();
                    self.bounce(sid, frac)
                }
                _ => Vec::new(),
            },
            InjectMsg::DragReady { drag, crossing } => {
                if self
                    .ride
                    .is_some_and(|r| r.is(drag, crossing) && r.hold.is_some())
                {
                    info!(drag, "every item is here: posting the held up");
                    self.drop_now(drag);
                }
                Vec::new()
            }
            InjectMsg::DragFailed { drag, crossing } => {
                // An earlier crossing's failure is no news for this one.
                if self.ride.is_some_and(|r| r.is(drag, crossing)) {
                    self.abort_drag(drag);
                }
                Vec::new()
            }
            InjectMsg::CancelDrag { drag } => {
                self.abort_drag(drag);
                Vec::new()
            }
            InjectMsg::LateDrop { drag } => match self.sid {
                Some(sid) if self.late_until.is_some() => {
                    info!(drag, "a late drag dropped on the strip: leaving with it");
                    self.end_late_wait(sid)
                }
                _ => Vec::new(), // The wait is over; the catcher refused it.
            },
            InjectMsg::DragEnded { drag, crossing } => {
                self.session_ended(drag, crossing);
                Vec::new()
            }
            InjectMsg::EscapeSession { drag, crossing } => {
                // An up may have ended it meanwhile (going home, a forced
                // return): then an Esc would reach the front-most app.
                if self.drags.session_live(drag, crossing) {
                    info!(drag, crossing, "ending a cancelled drag's late session");
                    self.escape();
                }
                Vec::new()
            }
            InjectMsg::Quit => {
                let out = self.end_session();
                self.inj.release_all();
                out
            }
        }
    }

    fn end_session(&mut self) -> Vec<InjectEv> {
        self.offer = None;
        self.linger = None;
        match self.sid.take() {
            Some(sid) => self.feed(sid, MacEvent::Disconnected),
            None => Vec::new(),
        }
    }

    fn input(&mut self, sid: u64, input: Input) -> Vec<InjectEv> {
        let controlled = self.machine.controlled();
        let held = self.ride.is_some_and(|r| r.hold.is_some());
        match input {
            Input::Enter { edge, frac, drag } => {
                // A new crossing: the strip of an earlier push-out must
                // not show during it (a ride). A late wait still running
                // keeps it, for that drag to end on; the Enter ends it.
                if self.late_until.is_none() {
                    self.linger = None;
                }
                // The cached state: asking the system costs milliseconds.
                if self.access.load(Ordering::Relaxed) {
                    return self.feed(sid, MacEvent::Enter { edge, frac, drag });
                }
                if let Some(suppressed) = self.no_access_log.allow(Instant::now()) {
                    warn!(
                        suppressed,
                        "no Accessibility permission: sending the cursor back to the PC"
                    );
                }
                // Neither wake nor warp: the Mac's cursor must not jump to
                // the edge for a crossing that is undone at once. The
                // machine hears the real drag, so the `Leave` carries it
                // home, but no session starts or is cancelled for it
                // (Task 17 note a); one already riding here is left to the
                // machine as usual.
                self.no_session = drag.filter(|&d| self.ride.is_none_or(|r| r.drag != d));
                let mut out = self.feed_quietly(sid, MacEvent::Enter { edge, frac, drag });
                out.extend(self.bounce(sid, frac));
                self.no_session = None;
                out
            }
            // A held drop keeps the cursor on the drop spot.
            Input::Motion { .. } | Input::Scroll { .. } if held => Vec::new(),
            Input::Motion { dx, dy } if controlled => match self.inj.motion(dx, dy) {
                Some(frac) => self.push_out(sid, frac),
                None => Vec::new(),
            },
            Input::Scroll { dx, dy } if controlled => {
                self.inj.scroll(dx, dy);
                Vec::new()
            }
            Input::Button {
                button: MouseButton::Left,
                down,
            } if self.ride.is_some() => {
                // The left button is the drag's: only its first up counts.
                if let Some(r @ Ride { hold: None, .. }) = self.ride
                    && !down
                {
                    self.let_go(r);
                }
                Vec::new()
            }
            Input::Button { button, down } => {
                if controlled || (!down && self.inj.button_held(button)) {
                    if button == MouseButton::Left && down && controlled {
                        // For R48a: did a drag begin since this press? The
                        // baseline comes first, as an app may start its drag
                        // on the down itself.
                        if let Some(c) = self.catcher.as_mut() {
                            c.pressed();
                        }
                    }
                    self.inj.button(button, down);
                    if button == MouseButton::Left {
                        self.left_is_users = down;
                    }
                }
                Vec::new()
            }
            Input::Key { mac_keycode, down } => {
                if controlled || (!down && self.inj.key_held(mac_keycode)) {
                    self.inj.key(mac_keycode, down);
                }
                Vec::new()
            }
            Input::ReleaseAll => self.feed(sid, MacEvent::ReleaseAll),
            Input::DragOffer { drag, items } => {
                self.offer = Some((drag, items));
                Vec::new()
            }
            Input::Motion { .. } | Input::Scroll { .. } => Vec::new(),
        }
    }

    fn escape(&mut self) {
        self.inj.key(KVK_ESCAPE, true);
        self.inj.key(KVK_ESCAPE, false);
    }

    /// Sends the PC's cursor back at `frac` along the entry edge, as if
    /// pushed out: never strand it on a Mac that cannot be controlled.
    fn bounce(&mut self, sid: u64, frac: f32) -> Vec<InjectEv> {
        self.feed(
            sid,
            MacEvent::PushOut {
                frac,
                buttons_held: false,
                offer: None,
            },
        )
    }

    /// Feeds the machine and carries out its actions.
    fn feed(&mut self, sid: u64, ev: MacEvent) -> Vec<InjectEv> {
        self.carry_out(sid, ev, true)
    }

    /// Like `feed`, but neither wakes the display nor moves the cursor.
    fn feed_quietly(&mut self, sid: u64, ev: MacEvent) -> Vec<InjectEv> {
        self.carry_out(sid, ev, false)
    }

    fn carry_out(&mut self, sid: u64, ev: MacEvent, move_cursor: bool) -> Vec<InjectEv> {
        let mut out = Vec::new();
        let actions = self.machine.on(ev);
        // A cancelled ride must not leave the left button posted down:
        // ReleaseAllInjected, or BeginDrag (which lets go before pressing),
        // takes care of it when one follows; otherwise (an Enter without a
        // drag) it is done after the actions.
        let lets_go = actions.iter().any(|a| {
            matches!(
                a,
                MacAction::ReleaseAllInjected | MacAction::BeginDrag { .. }
            )
        });
        let mut cancelled = false;
        for action in actions {
            match action {
                MacAction::WakeDisplay | MacAction::WarpTo { .. } if !move_cursor => {}
                MacAction::WakeDisplay => self.inj.wake_display(),
                MacAction::WarpTo { edge, frac } => self.inj.warp_to(edge, frac),
                MacAction::SendLeave { frac, drag } => {
                    out.push(InjectEv::SendLeave { sid, frac, drag })
                }
                MacAction::ReleaseAllInjected => self.inj.release_all(),
                MacAction::StartControl => out.push(InjectEv::Controlled { sid, on: true }),
                MacAction::StopControl => out.push(InjectEv::Controlled { sid, on: false }),
                // A bounced crossing's drag: no session, so nothing to do.
                MacAction::BeginDrag { drag } | MacAction::CancelDrag { drag }
                    if self.no_session == Some(drag) => {}
                MacAction::BeginDrag { drag } => self.begin_drag(drag),
                // Before any ReleaseAllInjected or BeginDrag of the list.
                MacAction::CancelDrag { drag } => {
                    self.abort_drag(drag);
                    cancelled = true;
                }
                // Before the release goes out, so a drop at the release is
                // taken. It ends with the strip's linger, set at the
                // push-out.
                MacAction::AwaitLateDrop => {
                    let until = self.linger.map(|(until, _)| until);
                    self.late_until = until.or_else(|| Some(Instant::now() + self.late_wait));
                    if let Some(c) = self.catcher.as_mut() {
                        c.await_late();
                    }
                }
            }
        }
        // The PC took the cursor back, or the session ended, during the
        // wait: no leave comes, and none carries what the strip caught.
        if self.late_until.is_some() && !self.machine.awaiting_late_drop() {
            self.late_until = None;
            if let Some(c) = self.catcher.as_mut() {
                c.end_late_wait(false);
            }
        }
        if cancelled && !lets_go && self.ride.is_none() && self.inj.button_held(MouseButton::Left) {
            self.inj.button(MouseButton::Left, false);
        }
        out
    }
}

/// Starts the injection thread; `report` passes its events to the app's
/// loop. It always ends by reporting `Stopped`: after `Quit`, or, having
/// released everything, after a panic.
pub fn spawn_inject_thread<I, D, C>(
    inj: I,
    drags: D,
    catcher: Option<C>,
    access: Arc<AtomicBool>,
    report: impl Fn(InjectEv) + Send + 'static,
) -> Result<crossbeam_channel::Sender<InjectMsg>>
where
    I: Inject + Send + 'static,
    D: DragPort + Send + 'static,
    C: CatchPort + Send + 'static,
{
    spawn_router(Router::with_catcher(inj, drags, catcher, access), report)
}

fn spawn_router<I, D, C>(
    mut router: Router<I, D, C>,
    report: impl Fn(InjectEv) + Send + 'static,
) -> Result<crossbeam_channel::Sender<InjectMsg>>
where
    I: Inject + Send + 'static,
    D: DragPort + Send + 'static,
    C: CatchPort + Send + 'static,
{
    let (tx, rx) = crossbeam_channel::unbounded::<InjectMsg>();
    std::thread::Builder::new()
        .name("cj-inject".into())
        .spawn(move || {
            set_user_interactive_qos();
            let run = catch_unwind(AssertUnwindSafe(|| {
                loop {
                    let msg = match router.next_tick() {
                        Some(at) => match rx.recv_deadline(at) {
                            Ok(msg) => msg,
                            Err(RecvTimeoutError::Timeout) => {
                                router.drags.hold();
                                for ev in router.tick(Instant::now()) {
                                    report(ev);
                                }
                                router.drags.release();
                                continue;
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        },
                        None => match rx.recv() {
                            Ok(msg) => msg,
                            Err(_) => break,
                        },
                    };
                    let quit = msg == InjectMsg::Quit;
                    router.drags.hold();
                    for ev in router.handle(msg) {
                        report(ev);
                    }
                    router.drags.release();
                    if quit {
                        break;
                    }
                }
            }));
            if run.is_err() {
                // Never leave a key or button held down: let go of
                // everything, then report Stopped like any other ending.
                error!("injection thread panicked; releasing everything");
                if catch_unwind(AssertUnwindSafe(|| router.let_go_after_panic())).is_err() {
                    error!("releasing after the panic failed too");
                }
                // What the source emitted before or during the panic.
                if catch_unwind(AssertUnwindSafe(|| router.drags.release())).is_err() {
                    error!("sending the drag source's held events failed");
                }
            }
            report(InjectEv::Stopped);
        })
        .context("starting the injection thread")?;
    Ok(tx)
}

/// Runs this thread at the highest QoS class, so input is never delayed
/// behind background work.
pub fn set_user_interactive_qos() {
    // SAFETY: plain call that only changes the calling thread's QoS.
    let err = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if err != 0 {
        warn!("pthread_set_qos_class_self_np failed: {err}");
    }
}
