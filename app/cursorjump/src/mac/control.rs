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
//!
//! A Mac drag going to the PC is caught at the entry edge
//! (`dnd::catcher`, Task 16):
//! - The strip shows (`CatchPort::zone`, on changes only) while the PC
//!   controls this Mac, the user holds only the left button (pressed with
//!   the PC's mouse, not for a ride), and the cursor is within
//!   `CATCH_ZONE_PT` (48 pt) of the entry edge.
//! - A push-out then (`CatchPort::push_out`) carries the strip's drag:
//!   the release goes out and drops it there, and the `Leave` carries it.
//!   With no drag there yet, the release goes out all the same, and the
//!   `Leave` waits up to [`LATE_DRAG_WAIT`] for a drag that starts late
//!   to drop on the strip (`InjectMsg::LateDrop`, R48). The wait is a
//!   deadline of the thread's loop, never a sleep: input keeps flowing
//!   (and is dropped, the PC no longer controlling this Mac).
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

use super::dnd::catcher::{LATE_DRAG_WAIT, in_catch_zone};
use super::dnd::source::KVK_ESCAPE;
use super::inject::{Injector, Rectf};
use super::power;

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
}

/// For a Mac without a drag source: no PC drag starts here.
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

/// What the thread needs from the Mac's edge catcher
/// (`dnd::catcher::Catcher`); a trait so the routing can be tested
/// without AppKit.
pub trait CatchPort {
    /// Some(edge) shows the strip along that edge; None hides it. Called
    /// only when that changes.
    fn zone(&mut self, show: Option<Side>);
    /// A push-out with only the left button held is about to post the
    /// release, over the strip. Returns the strip's drag, whose drop there
    /// is then taken; or None, and then a drag that drops on the strip
    /// before `end_late_wait` is taken (R48).
    fn push_out(&mut self) -> Option<DragId>;
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
    fn push_out(&mut self) -> Option<DragId> {
        match *self {}
    }
    fn end_late_wait(&mut self, _carry: bool) -> Option<DragId> {
        match *self {}
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Task 17 sends it for the pill's ✕ and DragCancel")
    )]
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

/// The PC's drag riding on this Mac, with the left button down for it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ride {
    drag: DragId,
    crossing: Crossing,
    /// The user let go and the up is held until the items are complete.
    hold: Option<Hold>,
}

impl Ride {
    fn is(&self, drag: DragId, crossing: Crossing) -> bool {
        self.drag == drag && self.crossing == crossing
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Hold {
    next_jiggle: Instant,
    /// The last jiggle left the cursor 1 pt off.
    nudged: bool,
}

/// The other buttons than the left one.
const NOT_LEFT: [MouseButton; 4] = [
    MouseButton::Right,
    MouseButton::Middle,
    MouseButton::Back,
    MouseButton::Forward,
];

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
                // the edge for a crossing that is undone at once. The drag
                // is left out: no session may start (Task 17 note a).
                let mut out = self.feed_quietly(
                    sid,
                    MacEvent::Enter {
                        edge,
                        frac,
                        drag: None,
                    },
                );
                out.extend(self.bounce(sid, frac));
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

    /// The cursor pushes out through the entry edge at `frac`. With only
    /// the user's left button held and a catcher here, the release goes
    /// out over the strip: carrying the strip's drag, or waiting for a
    /// late one (R48).
    fn push_out(&mut self, sid: u64, frac: f32) -> Vec<InjectEv> {
        let buttons_held = self.inj.buttons_held();
        let catching = self.catcher.is_some() && self.users_left_alone();
        let Some(edge) = self.machine.entry_edge().filter(|_| catching) else {
            let offer = None;
            return self.feed(
                sid,
                MacEvent::PushOut {
                    frac,
                    buttons_held,
                    offer,
                },
            );
        };
        // Up before the release goes out, and a while after it.
        self.linger = Some((Instant::now() + self.late_wait, edge));
        self.update_zone(Instant::now());
        let offer = self.catcher.as_mut().and_then(|c| c.push_out());
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
            "pushing out with the left button held: waiting for a late drag"
        );
        self.feed(sid, MacEvent::PushOutLate { frac })
    }

    /// Whether the only button held is the left one, pressed by the user
    /// here (not for a ride, nor left down after one).
    fn users_left_alone(&self) -> bool {
        self.left_is_users
            && self.ride.is_none()
            && self.inj.button_held(MouseButton::Left)
            && !NOT_LEFT.iter().any(|&b| self.inj.button_held(b))
    }

    /// The late wait is over: leave, with the drag the catcher caught.
    fn end_late_wait(&mut self, sid: u64) -> Vec<InjectEv> {
        self.late_until = None;
        let drag = self.catcher.as_mut().and_then(|c| c.end_late_wait(true));
        self.feed(sid, MacEvent::LateDrop { drag })
    }

    /// Tells the catcher where the strip should be, if that changed: on
    /// the entry edge while the user drags with only the left button near
    /// it, or lingering after a push-out; nowhere otherwise.
    fn update_zone(&mut self, now: Instant) {
        if self.catcher.is_none() {
            return;
        }
        if self.linger.is_some_and(|(until, _)| now >= until) {
            self.linger = None;
        }
        let near = match self.machine.entry_edge() {
            Some(edge) if self.machine.controlled() && self.users_left_alone() => {
                let (bounds, at) = (self.inj.bounds(), self.inj.cursor());
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

    /// `BeginDrag`: arm the drag under the cursor, then press for it.
    fn begin_drag(&mut self, drag: DragId) {
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
    fn let_go(&mut self, ride: Ride) {
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
    fn drop_now(&mut self, drag: DragId) {
        self.let_go_of_left();
        self.machine.on(MacEvent::DragEnded { drag });
    }

    /// Ends the ride, letting go of the left button: back on the drop spot
    /// first if the last jiggle left the cursor off it.
    fn let_go_of_left(&mut self) {
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
    fn abort_drag(&mut self, drag: DragId) {
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
    fn session_ended(&mut self, drag: DragId, crossing: Crossing) {
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
                MacAction::BeginDrag { drag } => self.begin_drag(drag),
                // Before any ReleaseAllInjected or BeginDrag of the list.
                MacAction::CancelDrag { drag } => {
                    self.abort_drag(drag);
                    cancelled = true;
                }
                // It ends with the strip's linger, set at the push-out.
                MacAction::AwaitLateDrop => {
                    let until = self.linger.map(|(until, _)| until);
                    self.late_until = until.or_else(|| Some(Instant::now() + self.late_wait));
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
                                for ev in router.tick(Instant::now()) {
                                    report(ev);
                                }
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
                    for ev in router.handle(msg) {
                        report(ev);
                    }
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Call {
        Wake,
        Warp(Side, f32),
        Motion(i16, i16),
        Scroll(i32, i32),
        Button(MouseButton, bool),
        Key(u16, bool),
        ReleaseAll,
        Jiggle(bool),
        // The drag source's calls.
        Prepare(DragId, usize),
        Released(DragId),
        MayRelease(DragId),
        CancelSource(DragId),
        // The catcher's calls.
        Zone(Option<Side>),
        CatchPushOut,
        EndLateWait(bool),
    }

    /// Records every call; holds keys and buttons like the Injector does.
    pub(crate) struct Fake {
        calls: Arc<Mutex<Vec<Call>>>,
        /// `key` panics, like a bug in the Injector would.
        key_panics: bool,
        /// `motion` panics.
        motion_panics: bool,
        /// What every `motion` returns.
        push_out: Option<f32>,
        /// What `entry_frac` returns: where the cursor is along the entry edge.
        at_frac: f32,
        keys: BTreeSet<u16>,
        buttons: Vec<MouseButton>,
        /// Where the cursor is, in global points.
        pos: (f64, f64),
    }

    impl Fake {
        pub(crate) fn new() -> (Fake, Arc<Mutex<Vec<Call>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let fake = Fake {
                calls: calls.clone(),
                key_panics: false,
                motion_panics: false,
                push_out: None,
                at_frac: 0.5,
                keys: BTreeSet::new(),
                buttons: Vec::new(),
                pos: (300.0, 200.0),
            };
            (fake, calls)
        }

        fn log(&self, c: Call) {
            self.calls.lock().unwrap().push(c);
        }
    }

    impl Inject for Fake {
        fn wake_display(&mut self) {
            self.log(Call::Wake);
        }
        fn warp_to(&mut self, edge: Side, frac: f32) {
            self.log(Call::Warp(edge, frac));
        }
        fn motion(&mut self, dx: i16, dy: i16) -> Option<f32> {
            assert!(!self.motion_panics, "injector bug");
            self.log(Call::Motion(dx, dy));
            self.push_out
        }
        fn scroll(&mut self, dx: i32, dy: i32) {
            self.log(Call::Scroll(dx, dy));
        }
        fn button(&mut self, b: MouseButton, down: bool) {
            self.buttons.retain(|&x| x != b);
            if down {
                self.buttons.push(b);
            }
            self.log(Call::Button(b, down));
        }
        fn key(&mut self, kc: u16, down: bool) {
            assert!(!self.key_panics, "injector bug");
            if down {
                self.keys.insert(kc);
            } else {
                self.keys.remove(&kc);
            }
            self.log(Call::Key(kc, down));
        }
        fn release_all(&mut self) {
            self.keys.clear();
            self.buttons.clear();
            self.log(Call::ReleaseAll);
        }
        fn buttons_held(&self) -> bool {
            !self.buttons.is_empty()
        }
        fn button_held(&self, b: MouseButton) -> bool {
            self.buttons.contains(&b)
        }
        fn key_held(&self, kc: u16) -> bool {
            self.keys.contains(&kc)
        }
        fn entry_frac(&self) -> f32 {
            self.at_frac
        }
        fn cursor(&mut self) -> (f64, f64) {
            self.pos
        }
        fn bounds(&self) -> Rectf {
            Rectf {
                x: 0.0,
                y: 0.0,
                w: 1512.0,
                h: 982.0,
            }
        }
        fn jiggle(&mut self, nudge: bool) {
            self.log(Call::Jiggle(nudge));
        }
    }

    /// A drag source that records its calls in the Fake's list.
    pub(crate) struct FakeDrags {
        calls: Arc<Mutex<Vec<Call>>>,
        prepare_ok: bool,
        may_release: bool,
        /// What `cancel` says: whether a session is live.
        session: bool,
        /// The last crossing token handed out.
        crossings: Crossing,
    }

    impl FakeDrags {
        pub(crate) fn new(calls: &Arc<Mutex<Vec<Call>>>) -> FakeDrags {
            FakeDrags {
                calls: calls.clone(),
                prepare_ok: true,
                may_release: false,
                session: true,
                crossings: 0,
            }
        }

        fn log(&self, c: Call) {
            self.calls.lock().unwrap().push(c);
        }
    }

    impl DragPort for FakeDrags {
        fn prepare(
            &mut self,
            drag: DragId,
            items: Vec<DragItem>,
            at: (f64, f64),
        ) -> Option<Crossing> {
            assert_eq!(at, (300.0, 200.0), "under the cursor");
            self.log(Call::Prepare(drag, items.len()));
            self.crossings += 1;
            self.prepare_ok.then_some(self.crossings)
        }
        fn released(&mut self, drag: DragId, at: (f64, f64)) {
            assert_eq!(at, (300.0, 200.0));
            self.log(Call::Released(drag));
        }
        fn may_release(&mut self, drag: DragId, _at: (f64, f64)) -> bool {
            self.log(Call::MayRelease(drag));
            self.may_release
        }
        fn cancel(&mut self, drag: DragId) -> bool {
            self.log(Call::CancelSource(drag));
            self.session
        }
        fn session_live(&mut self, _drag: DragId, _crossing: Crossing) -> bool {
            self.session
        }
    }

    fn input(sid: u64, input: Input) -> InjectMsg {
        InjectMsg::Input { sid, input }
    }

    fn enter(frac: f32) -> Input {
        Input::Enter {
            edge: Side::Left,
            frac,
            drag: None,
        }
    }

    fn key(kc: u16, down: bool) -> Input {
        Input::Key {
            mac_keycode: kc,
            down,
        }
    }

    fn button(down: bool) -> Input {
        Input::Button {
            button: MouseButton::Left,
            down,
        }
    }

    fn granted() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(true))
    }

    /// A router with session 1 begun, with or without Accessibility.
    fn begun_with(fake: Fake, access: Arc<AtomicBool>) -> Router<Fake, NoDrags> {
        let mut r = Router::new(fake, NoDrags, access);
        assert_eq!(r.handle(InjectMsg::Begin { sid: 1 }), vec![]);
        r
    }

    /// A router with session 1 begun.
    fn begun(fake: Fake) -> Router<Fake, NoDrags> {
        begun_with(fake, granted())
    }

    pub(crate) fn take(calls: &Arc<Mutex<Vec<Call>>>) -> Vec<Call> {
        std::mem::take(&mut *calls.lock().unwrap())
    }

    #[test]
    fn input_while_not_controlled_is_dropped() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        for i in [
            Input::Motion { dx: 5, dy: 5 },
            Input::Scroll { dx: 0, dy: 10 },
            key(0x00, true),
            key(0x00, false),
            button(true),
            button(false),
        ] {
            assert_eq!(r.handle(input(1, i)), vec![]);
        }
        assert_eq!(take(&calls), vec![]);
    }

    #[test]
    fn enter_wakes_and_warps_then_input_flows_in_order() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        assert_eq!(
            r.handle(input(1, enter(0.4))),
            vec![InjectEv::Controlled { sid: 1, on: true }]
        );
        for i in [
            key(0x00, true),
            key(0x00, false),
            Input::Motion { dx: 3, dy: -2 },
            button(true),
            button(false),
            Input::Scroll { dx: 1, dy: 2 },
        ] {
            assert_eq!(r.handle(input(1, i)), vec![]);
        }
        assert_eq!(
            take(&calls),
            vec![
                Call::Wake,
                Call::Warp(Side::Left, 0.4),
                Call::Key(0x00, true),
                Call::Key(0x00, false),
                Call::Motion(3, -2),
                Call::Button(MouseButton::Left, true),
                Call::Button(MouseButton::Left, false),
                Call::Scroll(1, 2),
            ]
        );
    }

    #[test]
    fn ups_while_not_controlled_only_if_still_held() {
        let (mut fake, calls) = Fake::new();
        fake.keys.insert(0x00);
        fake.buttons.push(MouseButton::Right);
        let mut r = begun(fake);
        let right = |down| Input::Button {
            button: MouseButton::Right,
            down,
        };
        for i in [
            key(0x00, false),
            key(0x01, false),
            key(0x02, true),
            right(false),
            button(false),
            right(true),
        ] {
            r.handle(input(1, i));
        }
        assert_eq!(
            take(&calls),
            vec![
                Call::Key(0x00, false),
                Call::Button(MouseButton::Right, false)
            ]
        );
    }

    #[test]
    fn push_out_releases_and_sends_leave() {
        let (mut fake, calls) = Fake::new();
        fake.push_out = Some(0.3);
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        take(&calls);
        assert_eq!(
            r.handle(input(1, Input::Motion { dx: -9, dy: 0 })),
            vec![
                InjectEv::SendLeave {
                    sid: 1,
                    frac: 0.3,
                    drag: None
                },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
        assert_eq!(r.handle(input(1, Input::Motion { dx: -9, dy: 0 })), vec![]);
        assert_eq!(take(&calls), vec![], "no input after leaving");
    }

    #[test]
    fn push_out_with_a_button_held_is_a_wall() {
        let (mut fake, calls) = Fake::new();
        fake.push_out = Some(0.3);
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, button(true)));
        take(&calls);
        assert_eq!(r.handle(input(1, Input::Motion { dx: -9, dy: 0 })), vec![]);
        r.handle(input(1, button(false)));
        assert_eq!(
            take(&calls),
            vec![Call::Motion(-9, 0), Call::Button(MouseButton::Left, false)],
            "still controlled: the drag goes on"
        );
    }

    #[test]
    fn release_all_from_the_pc_releases_and_stops_control() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, key(0x37, true)));
        take(&calls);
        assert_eq!(
            r.handle(input(1, Input::ReleaseAll)),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        r.handle(input(1, Input::Motion { dx: 1, dy: 1 }));
        assert_eq!(take(&calls), vec![Call::ReleaseAll]);
    }

    #[test]
    fn end_of_the_session_releases_everything() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, key(0x00, true)));
        take(&calls);
        assert_eq!(
            r.handle(InjectMsg::End { sid: 1 }),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        assert_eq!(take(&calls), vec![Call::ReleaseAll]);
        assert_eq!(r.handle(input(1, enter(0.5))), vec![], "session is over");
        assert_eq!(take(&calls), vec![]);
    }

    #[test]
    fn a_new_session_ends_the_old_one_and_drops_its_input() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        take(&calls);
        assert_eq!(
            r.handle(InjectMsg::Begin { sid: 2 }),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        assert_eq!(take(&calls), vec![Call::ReleaseAll]);
        r.handle(input(1, Input::Motion { dx: 1, dy: 1 }));
        r.handle(input(1, enter(0.5)));
        r.handle(InjectMsg::End { sid: 1 });
        assert_eq!(
            take(&calls),
            vec![],
            "the old session's leftovers do nothing"
        );
        assert_eq!(
            r.handle(input(2, enter(0.2))),
            vec![InjectEv::Controlled { sid: 2, on: true }]
        );
        assert_eq!(take(&calls), vec![Call::Wake, Call::Warp(Side::Left, 0.2)]);
    }

    #[test]
    fn without_accessibility_enter_leaves_at_once() {
        let (fake, calls) = Fake::new();
        let mut r = begun_with(fake, Arc::new(AtomicBool::new(false)));
        assert_eq!(
            r.handle(input(1, enter(0.7))),
            vec![
                InjectEv::Controlled { sid: 1, on: true },
                InjectEv::SendLeave {
                    sid: 1,
                    frac: 0.7,
                    drag: None
                },
                InjectEv::Controlled { sid: 1, on: false },
            ]
        );
        r.handle(input(1, key(0x00, true)));
        assert_eq!(
            take(&calls),
            vec![Call::ReleaseAll],
            "no wake and no warp: the Mac's cursor does not jump"
        );
    }

    #[test]
    fn enter_follows_the_cached_access_state() {
        let (fake, calls) = Fake::new();
        let access = Arc::new(AtomicBool::new(false));
        let mut r = begun_with(fake, access.clone());
        let bounced = r.handle(input(1, enter(0.7)));
        assert!(bounced.contains(&InjectEv::SendLeave {
            sid: 1,
            frac: 0.7,
            drag: None
        }));
        access.store(true, Ordering::Relaxed);
        take(&calls);
        assert_eq!(
            r.handle(input(1, enter(0.6))),
            vec![InjectEv::Controlled { sid: 1, on: true }],
            "granted since: the PC keeps the cursor here"
        );
    }

    #[test]
    fn losing_access_while_controlled_sends_the_cursor_back() {
        let (mut fake, calls) = Fake::new();
        fake.at_frac = 0.8;
        let access = granted();
        let mut r = begun_with(fake, access.clone());
        r.handle(input(1, enter(0.3)));
        r.handle(input(1, key(0x00, true)));
        take(&calls);
        access.store(false, Ordering::Relaxed);
        assert_eq!(
            r.handle(InjectMsg::AccessLost),
            vec![
                // Where the cursor is now along the entry edge, not where it came in.
                InjectEv::SendLeave {
                    sid: 1,
                    frac: 0.8,
                    drag: None
                },
                InjectEv::Controlled { sid: 1, on: false },
            ]
        );
        assert_eq!(take(&calls), vec![Call::ReleaseAll]);
        assert_eq!(r.handle(InjectMsg::AccessLost), vec![], "already gone");
    }

    #[test]
    fn log_limit_lets_one_line_through_per_period_and_counts_the_rest() {
        let t0 = std::time::Instant::now();
        let mut l = LogLimit::new(Duration::from_secs(10));
        assert_eq!(l.allow(t0), Some(0), "the first line goes through");
        assert_eq!(l.allow(t0 + Duration::from_secs(1)), None);
        assert_eq!(l.allow(t0 + Duration::from_millis(9_999)), None);
        assert_eq!(
            l.allow(t0 + Duration::from_secs(10)),
            Some(2),
            "after the period, with the count of lines held back"
        );
        assert_eq!(l.allow(t0 + Duration::from_secs(11)), None);
        assert_eq!(l.allow(t0 + Duration::from_secs(25)), Some(1));
        assert_eq!(l.allow(t0 + Duration::from_secs(40)), Some(0));
    }

    #[test]
    fn losing_access_while_not_controlled_does_nothing() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        assert_eq!(r.handle(InjectMsg::AccessLost), vec![]);
        assert_eq!(take(&calls), vec![]);
    }

    #[test]
    fn update_access_tells_the_thread_only_when_access_goes() {
        let access = AtomicBool::new(true);
        let (tx, rx) = crossbeam_channel::unbounded();
        assert!(!update_access(&access, true, &tx), "unchanged");
        assert!(update_access(&access, false, &tx));
        assert!(!access.load(Ordering::Relaxed));
        assert_eq!(rx.try_recv(), Ok(InjectMsg::AccessLost));
        assert!(!update_access(&access, false, &tx));
        assert!(update_access(&access, true, &tx));
        assert!(access.load(Ordering::Relaxed));
        assert!(
            rx.try_recv().is_err(),
            "nothing to do when access comes back"
        );
    }

    #[test]
    fn quit_releases_everything() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, key(0x00, true)));
        take(&calls);
        assert_eq!(
            r.handle(InjectMsg::Quit),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        let calls = take(&calls);
        assert!(!calls.is_empty() && calls.iter().all(|c| *c == Call::ReleaseAll));
    }

    #[test]
    fn thread_injects_in_order_and_reports_until_quit() {
        let (fake, calls) = Fake::new();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let tx = spawn_inject_thread(fake, NoDrags, None::<NoCatcher>, granted(), move |ev| {
            let _ = ev_tx.send(ev);
        })
        .unwrap();
        for msg in [
            InjectMsg::Begin { sid: 7 },
            input(7, enter(0.5)),
            input(7, key(0x00, true)),
            input(7, Input::Motion { dx: 1, dy: 0 }),
            input(7, key(0x00, false)),
            InjectMsg::Quit,
        ] {
            tx.send(msg).unwrap();
        }
        let t = Duration::from_secs(2);
        assert_eq!(
            ev_rx.recv_timeout(t).unwrap(),
            InjectEv::Controlled { sid: 7, on: true }
        );
        assert_eq!(
            ev_rx.recv_timeout(t).unwrap(),
            InjectEv::Controlled { sid: 7, on: false }
        );
        assert_eq!(ev_rx.recv_timeout(t).unwrap(), InjectEv::Stopped);
        let calls = take(&calls);
        assert_eq!(
            calls[..5],
            [
                Call::Wake,
                Call::Warp(Side::Left, 0.5),
                Call::Key(0x00, true),
                Call::Motion(1, 0),
                Call::Key(0x00, false),
            ]
        );
        assert!(calls[5..].iter().all(|c| *c == Call::ReleaseAll));
    }

    #[test]
    fn a_panic_releases_everything_and_reports_stopped() {
        let (mut fake, calls) = Fake::new();
        fake.key_panics = true;
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let tx = spawn_inject_thread(fake, NoDrags, None::<NoCatcher>, granted(), move |ev| {
            let _ = ev_tx.send(ev);
        })
        .unwrap();
        for msg in [
            InjectMsg::Begin { sid: 3 },
            input(3, enter(0.5)),
            input(
                3,
                Input::Button {
                    button: MouseButton::Left,
                    down: true,
                },
            ),
            input(3, key(0x00, true)),
        ] {
            tx.send(msg).unwrap();
        }
        let t = Duration::from_secs(2);
        assert_eq!(
            ev_rx.recv_timeout(t).unwrap(),
            InjectEv::Controlled { sid: 3, on: true }
        );
        assert_eq!(ev_rx.recv_timeout(t).unwrap(), InjectEv::Stopped);
        assert_eq!(
            take(&calls).last(),
            Some(&Call::ReleaseAll),
            "the held button is let go"
        );
    }

    // ------------------------------------------------ drags from the PC

    use cj_core::dnd::ItemKind;

    const LEFT_DOWN: Call = Call::Button(MouseButton::Left, true);
    const LEFT_UP: Call = Call::Button(MouseButton::Left, false);
    const ESC_DOWN: Call = Call::Key(KVK_ESCAPE, true);
    const ESC_UP: Call = Call::Key(KVK_ESCAPE, false);

    fn items(n: usize) -> Vec<DragItem> {
        (0..n)
            .map(|i| DragItem {
                name: format!("f{i}.jpg"),
                kind: ItemKind::File,
                size: Some(10),
            })
            .collect()
    }

    fn enter_with(drag: DragId) -> Input {
        Input::Enter {
            edge: Side::Left,
            frac: 0.5,
            drag: Some(drag),
        }
    }

    /// Session 1 begun on a fake drag source set up by `setup`.
    fn begun_drags(
        fake: Fake,
        calls: &Arc<Mutex<Vec<Call>>>,
        setup: impl FnOnce(&mut FakeDrags),
    ) -> Router<Fake, FakeDrags> {
        let mut drags = FakeDrags::new(calls);
        setup(&mut drags);
        let mut r = Router::new(fake, drags, granted());
        assert_eq!(r.handle(InjectMsg::Begin { sid: 1 }), vec![]);
        r
    }

    /// Drag 7 (two items) riding on the Mac, its source saying `may_release`
    /// at the release; the calls so far are taken.
    fn riding(may_release: bool) -> (Router<Fake, FakeDrags>, Arc<Mutex<Vec<Call>>>) {
        let (mut fake, calls) = Fake::new();
        fake.push_out = Some(0.3);
        let mut r = begun_drags(fake, &calls, |d| d.may_release = may_release);
        r.handle(input(
            1,
            Input::DragOffer {
                drag: 7,
                items: items(2),
            },
        ));
        r.handle(input(1, enter_with(7)));
        take(&calls);
        (r, calls)
    }

    /// Drag 7 let go of while incomplete: the up is held.
    fn holding() -> (Router<Fake, FakeDrags>, Arc<Mutex<Vec<Call>>>) {
        let (mut r, calls) = riding(false);
        r.handle(input(1, button(false)));
        assert!(r.next_tick().is_some(), "jiggling");
        take(&calls);
        (r, calls)
    }

    #[test]
    fn begin_drag_prepares_then_presses_left() {
        let (fake, calls) = Fake::new();
        let mut r = begun_drags(fake, &calls, |_| {});
        r.handle(input(
            1,
            Input::DragOffer {
                drag: 7,
                items: items(2),
            },
        ));
        assert_eq!(take(&calls), vec![], "an offer alone does nothing");
        assert_eq!(
            r.handle(input(1, enter_with(7))),
            vec![InjectEv::Controlled { sid: 1, on: true }]
        );
        assert_eq!(
            take(&calls),
            vec![
                Call::Wake,
                Call::Warp(Side::Left, 0.5),
                Call::Prepare(7, 2),
                LEFT_DOWN
            ]
        );
        // Motion drags the session along.
        r.handle(input(1, Input::Motion { dx: 4, dy: 1 }));
        assert_eq!(take(&calls), vec![Call::Motion(4, 1)]);
    }

    #[test]
    fn prepare_failure_presses_nothing() {
        let (mut fake, calls) = Fake::new();
        fake.push_out = Some(0.3);
        let mut r = begun_drags(fake, &calls, |d| d.prepare_ok = false);
        r.handle(input(
            1,
            Input::DragOffer {
                drag: 7,
                items: items(1),
            },
        ));
        r.handle(input(1, enter_with(7)));
        assert_eq!(
            take(&calls),
            vec![Call::Wake, Call::Warp(Side::Left, 0.5), Call::Prepare(7, 1)]
        );
        // The drag is over: the PC's up passes as a plain up, and pushing
        // out is a plain leave, with no Esc.
        r.handle(input(1, button(false)));
        assert_eq!(
            r.handle(input(1, Input::Motion { dx: -9, dy: 0 })),
            vec![
                InjectEv::SendLeave {
                    sid: 1,
                    frac: 0.3,
                    drag: None
                },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(
            take(&calls),
            vec![LEFT_UP, Call::Motion(-9, 0), Call::ReleaseAll]
        );

        // A crossing whose offer never came: no items, so prepare fails.
        r.handle(input(1, enter_with(8)));
        assert_eq!(
            take(&calls),
            vec![Call::Wake, Call::Warp(Side::Left, 0.5), Call::Prepare(8, 0)]
        );
    }

    #[test]
    fn cancel_drag_posts_escape_before_release() {
        let (mut r, calls) = riding(false);
        assert_eq!(
            r.handle(input(1, Input::ReleaseAll)),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        assert_eq!(
            take(&calls),
            vec![Call::CancelSource(7), ESC_DOWN, ESC_UP, Call::ReleaseAll]
        );
    }

    #[test]
    fn release_over_a_taker_or_when_complete_drops_at_once() {
        let (mut fake, calls) = Fake::new();
        fake.push_out = Some(0.3);
        let mut r = begun_drags(fake, &calls, |d| d.may_release = true);
        r.handle(input(
            1,
            Input::DragOffer {
                drag: 7,
                items: items(1),
            },
        ));
        r.handle(input(1, enter_with(7)));
        take(&calls);
        r.handle(input(1, button(false)));
        assert_eq!(
            take(&calls),
            vec![Call::Released(7), Call::MayRelease(7), LEFT_UP]
        );
        assert_eq!(r.next_tick(), None);
        // Dropped: the next push out is a plain leave.
        r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
        assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
    }

    #[test]
    fn release_while_incomplete_holds_the_up_and_freezes_motion() {
        let (mut r, calls) = riding(false);
        r.handle(input(1, button(false)));
        assert_eq!(take(&calls), vec![Call::Released(7), Call::MayRelease(7)]);
        // The cursor stays on the drop spot; repeated ups do nothing.
        for i in [
            Input::Motion { dx: -50, dy: 3 },
            Input::Scroll { dx: 0, dy: 5 },
            button(false),
            button(true),
        ] {
            assert_eq!(r.handle(input(1, i)), vec![]);
        }
        assert_eq!(take(&calls), vec![]);
        // Keys still pass (Esc from the PC's keyboard cancels).
        r.handle(input(1, key(0x00, true)));
        assert_eq!(take(&calls), vec![Call::Key(0x00, true)]);

        // The jiggle: due every JIGGLE_EVERY, nudged then back.
        let due = r.next_tick().unwrap();
        r.tick(due - Duration::from_millis(1));
        assert_eq!(take(&calls), vec![], "not yet");
        r.tick(due);
        let next = r.next_tick().unwrap();
        assert_eq!(next, due + JIGGLE_EVERY);
        r.tick(next);
        assert_eq!(take(&calls), vec![Call::Jiggle(true), Call::Jiggle(false)]);
    }

    #[test]
    fn hold_ends_with_the_up_when_complete() {
        let (mut r, calls) = holding();
        let due = r.next_tick().unwrap();
        r.tick(due);
        assert_eq!(
            r.handle(InjectMsg::DragReady {
                drag: 8,
                crossing: 1
            }),
            vec![]
        );
        assert_eq!(take(&calls), vec![Call::Jiggle(true)], "another drag");
        assert_eq!(
            r.handle(InjectMsg::DragReady {
                drag: 7,
                crossing: 1
            }),
            vec![]
        );
        // Back on the drop spot, then the up.
        assert_eq!(take(&calls), vec![Call::Jiggle(false), LEFT_UP]);
        assert_eq!(r.next_tick(), None);
        // Motion flows again, and the session's end changes nothing: the
        // next push out is a plain leave, with no Esc.
        r.handle(InjectMsg::DragEnded {
            drag: 7,
            crossing: 1,
        });
        r.handle(input(1, Input::Motion { dx: 2, dy: 2 }));
        assert_eq!(take(&calls), vec![Call::Motion(2, 2), Call::ReleaseAll]);
    }

    #[test]
    fn cancel_during_hold_posts_escape_then_up() {
        for msg in [
            InjectMsg::CancelDrag { drag: 7 },
            InjectMsg::DragFailed {
                drag: 7,
                crossing: 1,
            },
        ] {
            let (mut r, calls) = holding();
            assert_eq!(r.handle(msg.clone()), vec![]);
            assert_eq!(
                take(&calls),
                vec![Call::CancelSource(7), ESC_DOWN, ESC_UP, LEFT_UP],
                "{msg:?}"
            );
            assert_eq!(r.next_tick(), None);
            // Over: pushing out is a plain leave.
            r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
            assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
        }
        // A session that ends by itself during a hold (Esc typed on the PC)
        // lets go too.
        let (mut r, calls) = holding();
        r.handle(InjectMsg::DragEnded {
            drag: 7,
            crossing: 1,
        });
        assert_eq!(take(&calls), vec![LEFT_UP]);
        assert_eq!(r.next_tick(), None);
    }

    #[test]
    fn cancel_while_riding_leaves_the_button_to_the_pc() {
        let (mut r, calls) = riding(false);
        r.handle(InjectMsg::DragFailed {
            drag: 7,
            crossing: 1,
        });
        assert_eq!(take(&calls), vec![Call::CancelSource(7), ESC_DOWN, ESC_UP]);
        // The PC's up is its own.
        r.handle(input(1, button(false)));
        assert_eq!(take(&calls), vec![LEFT_UP]);
        // A cancel for a drag no longer riding only stops its fetches.
        r.handle(InjectMsg::CancelDrag { drag: 7 });
        assert_eq!(take(&calls), vec![Call::CancelSource(7)]);
    }

    #[test]
    fn disconnect_during_hold_releases_everything() {
        let (mut r, calls) = holding();
        assert_eq!(
            r.handle(InjectMsg::End { sid: 1 }),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        assert_eq!(
            take(&calls),
            vec![
                Call::CancelSource(7),
                ESC_DOWN,
                ESC_UP,
                LEFT_UP,
                Call::ReleaseAll
            ]
        );
        assert_eq!(r.next_tick(), None);

        // Losing Accessibility during a hold sends the cursor back the same way.
        let (mut r, calls) = holding();
        assert_eq!(
            r.handle(InjectMsg::AccessLost),
            vec![
                InjectEv::SendLeave {
                    sid: 1,
                    frac: 0.5,
                    drag: Some(7)
                },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(
            take(&calls),
            vec![
                Call::CancelSource(7),
                ESC_DOWN,
                ESC_UP,
                LEFT_UP,
                Call::ReleaseAll
            ]
        );
    }

    #[test]
    fn disconnect_while_riding_cancels_then_releases() {
        let (mut r, calls) = riding(false);
        assert_eq!(
            r.handle(InjectMsg::End { sid: 1 }),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        assert_eq!(
            take(&calls),
            vec![Call::CancelSource(7), ESC_DOWN, ESC_UP, Call::ReleaseAll]
        );
        assert_eq!(r.next_tick(), None);
    }

    #[test]
    fn left_counts_as_held_from_begin_drag() {
        let (mut r, calls) = riding(false);
        // Esc on the Mac's side ended the session; the PC still holds the
        // button, so pushing out is a wall.
        r.handle(InjectMsg::DragEnded {
            drag: 7,
            crossing: 1,
        });
        assert_eq!(r.handle(input(1, Input::Motion { dx: -9, dy: 0 })), vec![]);
        assert_eq!(take(&calls), vec![Call::Motion(-9, 0)]);
        // Once the PC lets go, the cursor may leave.
        r.handle(input(1, button(false)));
        assert_eq!(
            r.handle(input(1, Input::Motion { dx: -9, dy: 0 })),
            vec![
                InjectEv::SendLeave {
                    sid: 1,
                    frac: 0.3,
                    drag: None
                },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(
            take(&calls),
            vec![LEFT_UP, Call::Motion(-9, 0), Call::ReleaseAll]
        );
    }

    #[test]
    fn going_home_cancels_the_prefetch() {
        let (mut r, calls) = riding(false);
        assert_eq!(
            r.handle(input(1, Input::Motion { dx: -9, dy: 0 })),
            vec![
                InjectEv::SendLeave {
                    sid: 1,
                    frac: 0.3,
                    drag: Some(7)
                },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(
            take(&calls),
            vec![
                Call::Motion(-9, 0),
                Call::CancelSource(7),
                ESC_DOWN,
                ESC_UP,
                Call::ReleaseAll
            ]
        );
        assert_eq!(r.next_tick(), None);
    }

    /// Never a second left-down without an up in between.
    fn assert_downs_and_ups_alternate(calls: &[Call]) {
        let mut down = true; // Drag 7's down came first.
        for c in calls {
            match c {
                c if *c == LEFT_DOWN => {
                    assert!(!down, "a second down: {calls:?}");
                    down = true;
                }
                c if *c == LEFT_UP => down = false,
                _ => {}
            }
        }
    }

    #[test]
    fn a_new_crossing_over_a_riding_drag_cancels_it_first() {
        let (mut r, calls) = riding(false);
        r.handle(input(
            1,
            Input::DragOffer {
                drag: 8,
                items: items(3),
            },
        ));
        r.handle(input(1, enter_with(8)));
        let got = take(&calls);
        assert_eq!(
            got,
            vec![
                Call::CancelSource(7),
                ESC_DOWN,
                ESC_UP,
                Call::Wake,
                Call::Warp(Side::Left, 0.5),
                LEFT_UP,
                Call::Prepare(8, 3),
                LEFT_DOWN
            ]
        );
        assert_downs_and_ups_alternate(&got);
    }

    #[test]
    fn a_new_crossing_over_a_held_drop_lets_go_first() {
        let (mut r, calls) = holding();
        let due = r.next_tick().unwrap();
        r.tick(due);
        r.handle(input(
            1,
            Input::DragOffer {
                drag: 8,
                items: items(1),
            },
        ));
        r.handle(input(1, enter_with(8)));
        let got = take(&calls);
        assert_eq!(
            got,
            vec![
                Call::Jiggle(true),
                Call::CancelSource(7),
                ESC_DOWN,
                ESC_UP,
                Call::Jiggle(false),
                LEFT_UP,
                Call::Wake,
                Call::Warp(Side::Left, 0.5),
                Call::Prepare(8, 1),
                LEFT_DOWN
            ]
        );
        assert_downs_and_ups_alternate(&got);
        assert_eq!(r.next_tick(), None, "the new drag rides, not held");
    }

    #[test]
    fn a_panic_during_a_ride_cancels_the_drag_then_releases() {
        let (mut fake, calls) = Fake::new();
        fake.motion_panics = true;
        let drags = FakeDrags::new(&calls);
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let tx = spawn_inject_thread(fake, drags, None::<NoCatcher>, granted(), move |ev| {
            let _ = ev_tx.send(ev);
        })
        .unwrap();
        for msg in [
            InjectMsg::Begin { sid: 4 },
            input(
                4,
                Input::DragOffer {
                    drag: 7,
                    items: items(1),
                },
            ),
            input(4, enter_with(7)),
            input(4, Input::Motion { dx: 1, dy: 0 }),
        ] {
            tx.send(msg).unwrap();
        }
        let t = Duration::from_secs(2);
        while ev_rx.recv_timeout(t).unwrap() != InjectEv::Stopped {}
        let calls = take(&calls);
        assert_eq!(
            calls[calls.len() - 4..],
            [Call::CancelSource(7), ESC_DOWN, ESC_UP, Call::ReleaseAll],
            "{calls:?}"
        );
    }

    #[test]
    fn thread_jiggles_a_held_drop() {
        let (mut fake, calls) = Fake::new();
        fake.push_out = None;
        let drags = FakeDrags::new(&calls);
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let every = Duration::from_millis(5);
        let router = Router::new(fake, drags, granted()).with_jiggle_every(every);
        let tx = spawn_router(router, move |ev| {
            let _ = ev_tx.send(ev);
        })
        .unwrap();
        for msg in [
            InjectMsg::Begin { sid: 2 },
            input(
                2,
                Input::DragOffer {
                    drag: 7,
                    items: items(1),
                },
            ),
            input(2, enter_with(7)),
            input(2, button(false)),
        ] {
            tx.send(msg).unwrap();
        }
        std::thread::sleep(every * 2 + Duration::from_millis(40));
        tx.send(InjectMsg::DragReady {
            drag: 7,
            crossing: 1,
        })
        .unwrap();
        tx.send(InjectMsg::Quit).unwrap();
        let t = Duration::from_secs(2);
        while ev_rx.recv_timeout(t).unwrap() != InjectEv::Stopped {}
        let calls = take(&calls);
        let jiggles = calls
            .iter()
            .filter(|c| matches!(c, Call::Jiggle(_)))
            .count();
        assert!(jiggles >= 2, "{calls:?}");
        let up = calls.iter().position(|c| *c == LEFT_UP).unwrap();
        assert!(calls[..up].contains(&Call::Jiggle(true)));
        assert_eq!(calls[up - 1], Call::Jiggle(false), "{calls:?}");
    }

    #[test]
    fn a_cancel_posts_no_escape_without_a_live_session() {
        let (mut r, calls) = riding(false);
        r.drags.session = false;
        r.handle(InjectMsg::CancelDrag { drag: 7 });
        assert_eq!(take(&calls), vec![Call::CancelSource(7)], "no stray Esc");
        let (mut r, calls) = holding();
        r.drags.session = false;
        r.handle(InjectMsg::CancelDrag { drag: 7 });
        assert_eq!(take(&calls), vec![Call::CancelSource(7), LEFT_UP]);
        // A session that started late asks for its Esc.
        r.drags.session = true;
        r.handle(InjectMsg::EscapeSession {
            drag: 7,
            crossing: 1,
        });
        assert_eq!(take(&calls), vec![ESC_DOWN, ESC_UP]);
    }

    #[test]
    fn a_late_escape_request_for_an_ended_session_posts_nothing() {
        // Going home: the up ended the session before its Esc request came.
        let (mut r, calls) = riding(false);
        r.drags.session = false;
        r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
        take(&calls);
        r.handle(InjectMsg::EscapeSession {
            drag: 7,
            crossing: 1,
        });
        assert_eq!(take(&calls), vec![], "no Esc for the front-most app");
    }

    #[test]
    fn an_earlier_crossing_never_touches_a_later_ride() {
        let (mut r, calls) = riding(false);
        // Home (crossing 1 cancelled), then back: crossing 2.
        r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
        r.handle(input(
            1,
            Input::DragOffer {
                drag: 7,
                items: items(2),
            },
        ));
        r.handle(input(1, enter_with(7)));
        take(&calls);
        // Crossing 1's late news.
        r.handle(InjectMsg::DragEnded {
            drag: 7,
            crossing: 1,
        });
        r.handle(InjectMsg::DragFailed {
            drag: 7,
            crossing: 1,
        });
        assert_eq!(take(&calls), vec![], "ignored");
        // Crossing 2 still rides: the PC's up is its release.
        r.handle(input(1, button(false)));
        assert_eq!(take(&calls), vec![Call::Released(7), Call::MayRelease(7)]);
        r.handle(InjectMsg::DragReady {
            drag: 7,
            crossing: 1,
        });
        assert_eq!(take(&calls), vec![], "an earlier crossing's Ready");
        r.handle(InjectMsg::DragReady {
            drag: 7,
            crossing: 2,
        });
        assert_eq!(take(&calls), vec![LEFT_UP]);
    }

    #[test]
    fn enter_without_a_drag_over_a_ride_lets_go() {
        let (mut r, calls) = riding(false);
        r.handle(input(1, enter(0.5)));
        let got = take(&calls);
        assert_eq!(
            got,
            vec![
                Call::CancelSource(7),
                ESC_DOWN,
                ESC_UP,
                Call::Wake,
                Call::Warp(Side::Left, 0.5),
                LEFT_UP
            ]
        );
        assert_downs_and_ups_alternate(&got);
        // Nothing stays held: pushing out is a plain leave.
        r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
        assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
    }

    #[test]
    fn a_panic_while_cancelling_still_releases_everything() {
        let (mut fake, calls) = Fake::new();
        fake.key_panics = true;
        let drags = FakeDrags::new(&calls);
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let tx = spawn_inject_thread(fake, drags, None::<NoCatcher>, granted(), move |ev| {
            let _ = ev_tx.send(ev);
        })
        .unwrap();
        for msg in [
            InjectMsg::Begin { sid: 4 },
            input(
                4,
                Input::DragOffer {
                    drag: 7,
                    items: items(1),
                },
            ),
            input(4, enter_with(7)),
            // Panics; so does the Esc of the cancel after it.
            input(4, key(0x00, true)),
        ] {
            tx.send(msg).unwrap();
        }
        let t = Duration::from_secs(2);
        while ev_rx.recv_timeout(t).unwrap() != InjectEv::Stopped {}
        let calls = take(&calls);
        assert_eq!(
            calls[calls.len() - 2..],
            [Call::CancelSource(7), Call::ReleaseAll],
            "{calls:?}"
        );
    }

    // ------------------------------------------------ drags leaving the Mac

    const RIGHT_DOWN: Call = Call::Button(MouseButton::Right, true);
    const RIGHT_UP: Call = Call::Button(MouseButton::Right, false);
    /// A short late wait, so the thread tests run fast.
    const WAIT: Duration = Duration::from_millis(40);

    /// A catcher that records its calls in the Fake's list.
    pub(crate) struct FakeCatcher {
        calls: Arc<Mutex<Vec<Call>>>,
        /// The drag on the strip: what `push_out` carries.
        offer: Option<DragId>,
        /// What dropped during the late wait: what `end_late_wait` gives.
        caught: Arc<Mutex<Option<DragId>>>,
    }

    impl CatchPort for FakeCatcher {
        fn zone(&mut self, show: Option<Side>) {
            self.calls.lock().unwrap().push(Call::Zone(show));
        }
        fn push_out(&mut self) -> Option<DragId> {
            self.calls.lock().unwrap().push(Call::CatchPushOut);
            self.offer
        }
        fn end_late_wait(&mut self, carry: bool) -> Option<DragId> {
            self.calls.lock().unwrap().push(Call::EndLateWait(carry));
            self.caught.lock().unwrap().take()
        }
    }

    type Catching = Router<Fake, FakeDrags, FakeCatcher>;
    /// A router with a fake catcher, the call list, and the catcher's
    /// `caught` slot.
    type CatchSetup = (Catching, Arc<Mutex<Vec<Call>>>, Arc<Mutex<Option<DragId>>>);

    /// Session 1 begun with a fake drag source and catcher; the cursor is
    /// near the left edge and pushes out at 0.3. Returns the catcher's
    /// `caught` slot.
    fn begun_catching(offer: Option<DragId>) -> CatchSetup {
        let (mut fake, calls) = Fake::new();
        fake.push_out = Some(0.3);
        fake.pos = (10.0, 200.0);
        let caught = Arc::new(Mutex::new(None));
        let catcher = FakeCatcher {
            calls: calls.clone(),
            offer,
            caught: caught.clone(),
        };
        let drags = FakeDrags::new(&calls);
        let mut r =
            Router::with_catcher(fake, drags, Some(catcher), granted()).with_late_wait(WAIT);
        assert_eq!(r.handle(InjectMsg::Begin { sid: 1 }), vec![]);
        (r, calls, caught)
    }

    /// Controlled, with the user's left button down near the left edge:
    /// the strip is up; the calls so far are taken.
    fn dragging_near_the_edge(offer: Option<DragId>) -> CatchSetup {
        let (mut r, calls, caught) = begun_catching(offer);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, button(true)));
        assert_eq!(take(&calls).last(), Some(&Call::Zone(Some(Side::Left))));
        (r, calls, caught)
    }

    fn zones(calls: &[Call]) -> Vec<Option<Side>> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::Zone(z) => Some(*z),
                _ => None,
            })
            .collect()
    }

    fn push(r: &mut Catching) -> Vec<InjectEv> {
        r.handle(input(1, Input::Motion { dx: -9, dy: 0 }))
    }

    #[test]
    fn push_out_carries_catcher_offer_only_with_left_alone() {
        // Only Left: the strip's drag goes with the release and the leave.
        let (mut r, calls, _) = dragging_near_the_edge(Some(9));
        assert_eq!(
            push(&mut r),
            vec![
                InjectEv::SendLeave {
                    sid: 1,
                    frac: 0.3,
                    drag: Some(9)
                },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(
            take(&calls),
            vec![Call::Motion(-9, 0), Call::CatchPushOut, Call::ReleaseAll],
            "asked just before the release"
        );
        assert!(r.next_tick().is_some(), "the strip lingers");

        // Right held too: a wall, and the catcher is not asked.
        let (mut r, calls, _) = dragging_near_the_edge(Some(9));
        r.handle(input(
            1,
            Input::Button {
                button: MouseButton::Right,
                down: true,
            },
        ));
        assert_eq!(push(&mut r), vec![]);
        let c = take(&calls);
        assert!(!c.contains(&Call::CatchPushOut), "{c:?}");
        assert!(!c.contains(&Call::ReleaseAll), "{c:?}");

        // Nothing held: a plain leave, no offer.
        let (mut r, calls, _) = begun_catching(Some(9));
        r.handle(input(1, enter(0.5)));
        take(&calls);
        assert_eq!(
            push(&mut r),
            vec![
                InjectEv::SendLeave {
                    sid: 1,
                    frac: 0.3,
                    drag: None
                },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
        assert_eq!(r.next_tick(), None);
    }

    #[test]
    fn zone_shown_only_while_controlled_with_left_alone_near_the_edge() {
        let (mut r, calls, _) = begun_catching(None);
        let left = |down| input(1, button(down));
        let right = |down| {
            input(
                1,
                Input::Button {
                    button: MouseButton::Right,
                    down,
                },
            )
        };
        let mv = || input(1, Input::Motion { dx: 1, dy: 0 });
        r.inj.push_out = None;
        // Not controlled: nothing, whatever is pressed.
        r.handle(left(true));
        r.handle(mv());
        assert_eq!(zones(&take(&calls)), vec![]);
        r.handle(input(1, enter(0.5)));
        r.inj.pos = (300.0, 200.0);
        r.handle(left(true));
        r.handle(mv());
        assert_eq!(zones(&take(&calls)), vec![], "far from the edge");
        r.inj.pos = (47.0, 200.0);
        r.handle(mv());
        r.handle(mv());
        assert_eq!(
            zones(&take(&calls)),
            vec![Some(Side::Left)],
            "once, at the change"
        );
        r.handle(right(true));
        assert_eq!(take(&calls), vec![RIGHT_DOWN, Call::Zone(None)]);
        r.handle(right(false));
        assert_eq!(take(&calls), vec![RIGHT_UP, Call::Zone(Some(Side::Left))]);
        r.inj.pos = (49.0, 200.0);
        r.handle(mv());
        assert_eq!(zones(&take(&calls)), vec![None]);
        r.inj.pos = (2.0, 200.0);
        r.handle(mv());
        assert_eq!(zones(&take(&calls)), vec![Some(Side::Left)]);
        r.handle(left(false));
        assert_eq!(zones(&take(&calls)), vec![None], "let go");
        r.handle(left(true));
        assert_eq!(zones(&take(&calls)), vec![Some(Side::Left)]);
        // The PC takes the cursor back; then the session ends.
        r.handle(input(1, Input::ReleaseAll));
        assert_eq!(zones(&take(&calls)), vec![None]);
        r.handle(input(1, enter(0.5)));
        r.handle(left(true));
        assert_eq!(zones(&take(&calls)), vec![Some(Side::Left)]);
        r.handle(InjectMsg::End { sid: 1 });
        assert_eq!(zones(&take(&calls)), vec![None]);
    }

    #[test]
    fn zone_hidden_while_an_incoming_drag_rides() {
        let (mut r, calls, _) = begun_catching(None);
        r.inj.push_out = None;
        r.inj.pos = (300.0, 200.0);
        r.handle(input(
            1,
            Input::DragOffer {
                drag: 7,
                items: items(1),
            },
        ));
        r.handle(input(1, enter_with(7)));
        r.inj.pos = (10.0, 200.0);
        r.handle(input(1, Input::Motion { dx: -1, dy: 0 }));
        assert!(r.inj.button_held(MouseButton::Left), "pressed for the ride");
        assert_eq!(zones(&take(&calls)), vec![]);
        // The ride's session ends (Esc on the PC); the PC still holds the
        // button, which is no drag of the user's here.
        r.handle(InjectMsg::DragEnded {
            drag: 7,
            crossing: 1,
        });
        r.handle(input(1, Input::Motion { dx: -1, dy: 0 }));
        assert_eq!(zones(&take(&calls)), vec![]);
        // Pushing out with it is still a wall.
        r.inj.push_out = Some(0.3);
        assert_eq!(push(&mut r), vec![]);
        assert!(!take(&calls).contains(&Call::CatchPushOut));
    }

    #[test]
    fn late_perform_within_wait_becomes_the_offer() {
        let (mut r, calls, caught) = dragging_near_the_edge(None);
        assert_eq!(
            push(&mut r),
            vec![InjectEv::Controlled { sid: 1, on: false }],
            "no leave yet"
        );
        assert_eq!(
            take(&calls),
            vec![Call::Motion(-9, 0), Call::CatchPushOut, Call::ReleaseAll],
            "the release went out over the strip"
        );
        let due = r.next_tick().unwrap();
        *caught.lock().unwrap() = Some(11);
        assert_eq!(
            r.handle(InjectMsg::LateDrop { drag: 11 }),
            vec![InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: Some(11)
            }]
        );
        assert_eq!(take(&calls), vec![Call::EndLateWait(true)]);
        // The strip stays up for the rest of the wait, then goes.
        assert_eq!(r.next_tick(), Some(due));
        assert_eq!(r.tick(due - Duration::from_millis(1)), vec![]);
        assert_eq!(take(&calls), vec![]);
        assert_eq!(r.tick(due), vec![], "the leave went already");
        assert_eq!(take(&calls), vec![Call::Zone(None)]);
        assert_eq!(r.next_tick(), None);
        // A late message after that is no news.
        assert_eq!(r.handle(InjectMsg::LateDrop { drag: 11 }), vec![]);
    }

    #[test]
    fn no_perform_within_wait_leaves_without_offer() {
        let (mut r, calls, _) = dragging_near_the_edge(None);
        push(&mut r);
        take(&calls);
        let due = r.next_tick().unwrap();
        assert_eq!(r.tick(due - Duration::from_millis(1)), vec![]);
        assert_eq!(take(&calls), vec![], "the strip is still up");
        assert_eq!(
            r.tick(due),
            vec![InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: None
            }]
        );
        assert_eq!(
            take(&calls),
            vec![Call::EndLateWait(true), Call::Zone(None)]
        );
        assert_eq!(r.next_tick(), None);
    }

    #[test]
    fn taking_the_cursor_back_during_the_wait_leaves_nothing_behind() {
        for msg in [
            input(1, Input::ReleaseAll),
            InjectMsg::End { sid: 1 },
            InjectMsg::Begin { sid: 2 },
        ] {
            let (mut r, calls, caught) = dragging_near_the_edge(None);
            push(&mut r);
            take(&calls);
            *caught.lock().unwrap() = Some(11);
            let out = r.handle(msg.clone());
            assert!(
                !out.iter().any(|e| matches!(e, InjectEv::SendLeave { .. })),
                "{msg:?}: {out:?}"
            );
            let c = take(&calls);
            assert!(c.contains(&Call::EndLateWait(false)), "{msg:?}: {c:?}");
            // The strip may linger (a late drag still ends on it, refused),
            // but no leave comes.
            assert_eq!(r.tick(Instant::now() + WAIT * 2), vec![], "{msg:?}");
            let c = take(&calls);
            assert!(!c.contains(&Call::EndLateWait(true)), "{msg:?}: {c:?}");
            assert_eq!(r.next_tick(), None, "{msg:?}");
            assert_eq!(r.zone, None, "{msg:?}");
        }
    }

    #[test]
    fn late_wait_does_not_block_other_input() {
        let (mut r, calls, _) = dragging_near_the_edge(None);
        let t0 = Instant::now();
        push(&mut r);
        // Input during the wait is handled at once: dropped, the PC no
        // longer controlling this Mac.
        for i in [
            Input::Motion { dx: 5, dy: 0 },
            key(0x00, true),
            button(true),
            Input::Scroll { dx: 0, dy: 3 },
        ] {
            assert_eq!(r.handle(input(1, i)), vec![]);
        }
        assert!(t0.elapsed() < WAIT, "nothing waited");
        let c = take(&calls);
        assert_eq!(
            c,
            vec![Call::Motion(-9, 0), Call::CatchPushOut, Call::ReleaseAll],
            "nothing injected after the release"
        );
        assert!(r.next_tick().is_some(), "the leave is still due");
    }

    #[test]
    fn thread_sends_the_late_leave_after_the_wait_meanwhile_handling_input() {
        let (r, calls, _) = dragging_near_the_edge(None);
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let tx = spawn_router(r, move |ev| {
            let _ = ev_tx.send((Instant::now(), ev));
        })
        .unwrap();
        let t0 = Instant::now();
        tx.send(input(1, Input::Motion { dx: -9, dy: 0 })).unwrap();
        let t = Duration::from_secs(2);
        let (at, ev) = ev_rx.recv_timeout(t).unwrap();
        assert_eq!(ev, InjectEv::Controlled { sid: 1, on: false });
        assert!(at - t0 < WAIT, "reported before the wait is over");
        // Input sent during the wait is taken off the channel at once.
        tx.send(input(1, key(0x00, true))).unwrap();
        tx.send(InjectMsg::DragReady {
            drag: 3,
            crossing: 1,
        })
        .unwrap();
        let (at, ev) = ev_rx.recv_timeout(t).unwrap();
        assert_eq!(
            ev,
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: None
            }
        );
        assert!(at - t0 >= WAIT, "the leave waited");
        tx.send(InjectMsg::Quit).unwrap();
        while ev_rx.recv_timeout(t).unwrap().1 != InjectEv::Stopped {}
        let c = take(&calls);
        assert!(!c.contains(&Call::Key(0x00, true)), "{c:?}");
        assert_eq!(
            zones(&c),
            vec![None],
            "the strip went at the end of the wait: {c:?}"
        );
    }

    #[test]
    fn without_a_catcher_left_held_at_the_edge_is_still_a_wall() {
        let (mut fake, calls) = Fake::new();
        fake.push_out = Some(0.3);
        fake.pos = (0.0, 200.0);
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, button(true)));
        take(&calls);
        assert_eq!(r.handle(input(1, Input::Motion { dx: -9, dy: 0 })), vec![]);
        assert_eq!(take(&calls), vec![Call::Motion(-9, 0)]);
        assert_eq!(r.next_tick(), None);
    }
}
