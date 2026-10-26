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
//!   failed item) posts Esc, which ends the session without a drop, then
//!   lets go of the button unless the PC still holds it.

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

use super::dnd::source::KVK_ESCAPE;
use super::inject::Injector;
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
    /// A dragged event of the held button 1 pt off the cursor (`nudge`) or
    /// back on it; the cursor itself stays put.
    fn jiggle(&mut self, nudge: bool);
}

/// What the thread needs from the Mac's drag source
/// (`dnd::source::DragSource`); a trait so the routing can be tested
/// without AppKit.
pub trait DragPort {
    /// Arms drag `drag` under `at` (global points); false if it can't start.
    fn prepare(&mut self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> bool;
    /// The user let go while the drag rode here, at `at`.
    fn released(&mut self, drag: DragId, at: (f64, f64));
    /// Whether the up may be posted now, over `at`.
    fn may_release(&mut self, drag: DragId, at: (f64, f64)) -> bool;
    /// The drag is over here without a drop: stop its fetches.
    fn cancel(&mut self, drag: DragId);
}

/// For a Mac without a drag source: no PC drag starts here.
pub struct NoDrags;

impl DragPort for NoDrags {
    fn prepare(&mut self, _drag: DragId, _items: Vec<DragItem>, _at: (f64, f64)) -> bool {
        false
    }
    fn released(&mut self, _drag: DragId, _at: (f64, f64)) {}
    fn may_release(&mut self, _drag: DragId, _at: (f64, f64)) -> bool {
        true
    }
    fn cancel(&mut self, _drag: DragId) {}
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
    /// From the drag source (`MacDndEv::inject_msg`): every item of `drag`
    /// is complete, so a held drop may go ahead.
    DragReady {
        drag: DragId,
    },
    /// From the drag source: an item of `drag` failed; the drag is
    /// cancelled.
    DragFailed {
        drag: DragId,
    },
    /// From the drag source: `drag`'s session ended, dropped or not.
    DragEnded {
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
    /// Send `Control::Leave` with `frac` on session `sid`.
    SendLeave { sid: u64, frac: f32 },
    /// The PC started (true) or stopped controlling this Mac.
    Controlled { sid: u64, on: bool },
    /// The thread has released everything and exited.
    Stopped,
}

/// The PC's drag riding on this Mac, with the left button down for it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ride {
    drag: DragId,
    /// The user let go and the up is held until the items are complete.
    hold: Option<Hold>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Hold {
    next_jiggle: Instant,
    /// The last jiggle left the cursor 1 pt off.
    nudged: bool,
}

/// Routes messages to the Injector through the `MacMachine`.
pub struct Router<I: Inject, D: DragPort> {
    inj: I,
    drags: D,
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
}

impl<I: Inject, D: DragPort> Router<I, D> {
    pub fn new(inj: I, drags: D, access: Arc<AtomicBool>) -> Self {
        Router {
            inj,
            drags,
            machine: MacMachine::new(),
            sid: None,
            access,
            no_access_log: LogLimit::new(NO_ACCESS_LOG_EVERY),
            offer: None,
            ride: None,
        }
    }

    /// Lets go of every key and button the Injector holds.
    pub fn release_all(&mut self) {
        self.inj.release_all();
    }

    /// When `tick` must next run: the next jiggle of a held drop.
    pub fn next_tick(&self) -> Option<Instant> {
        self.ride.and_then(|r| r.hold).map(|h| h.next_jiggle)
    }

    /// Jiggles the cursor of a held drop when it is due.
    pub fn tick(&mut self, now: Instant) {
        let Some(Ride {
            hold: Some(hold), ..
        }) = &mut self.ride
        else {
            return;
        };
        if now < hold.next_jiggle {
            return;
        }
        hold.nudged = !hold.nudged;
        hold.next_jiggle = now + JIGGLE_EVERY;
        let nudge = hold.nudged;
        self.inj.jiggle(nudge);
    }

    /// Handles one message; returns what to report to the loop.
    pub fn handle(&mut self, msg: InjectMsg) -> Vec<InjectEv> {
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
            InjectMsg::DragReady { drag } => {
                if self.holding(drag) {
                    info!(drag, "every item is here: posting the held up");
                    self.drop_now(drag);
                }
                Vec::new()
            }
            InjectMsg::DragFailed { drag } | InjectMsg::CancelDrag { drag } => {
                self.abort_drag(drag);
                Vec::new()
            }
            InjectMsg::DragEnded { drag } => {
                self.session_ended(drag);
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
                Some(frac) => {
                    let buttons_held = self.inj.buttons_held();
                    self.feed(
                        sid,
                        MacEvent::PushOut {
                            frac,
                            buttons_held,
                            offer: None,
                        },
                    )
                }
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
                if let Some(Ride { drag, hold: None }) = self.ride
                    && !down
                {
                    self.let_go(drag);
                }
                Vec::new()
            }
            Input::Button { button, down } => {
                if controlled || (!down && self.inj.button_held(button)) {
                    self.inj.button(button, down);
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

    fn holding(&self, drag: DragId) -> bool {
        self.ride
            .is_some_and(|r| r.drag == drag && r.hold.is_some())
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
        let at = self.inj.cursor();
        if self.drags.prepare(drag, items, at) {
            self.inj.button(MouseButton::Left, true);
            self.ride = Some(Ride { drag, hold: None });
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
    fn let_go(&mut self, drag: DragId) {
        let at = self.inj.cursor();
        self.drags.released(drag, at);
        if self.drags.may_release(drag, at) {
            self.drop_now(drag);
            return;
        }
        info!(drag, "holding the drop until every item is here");
        self.ride = Some(Ride {
            drag,
            hold: Some(Hold {
                next_jiggle: Instant::now() + JIGGLE_EVERY,
                nudged: false,
            }),
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

    /// Cancels `drag` from this side: Esc ends its session undropped. A
    /// held up goes out too; while the drag still rides, the PC holds the
    /// button and sends the up itself.
    fn abort_drag(&mut self, drag: DragId) {
        match self.ride {
            Some(r) if r.drag == drag => {
                info!(drag, "cancelling the drag from the PC");
                self.escape();
                self.drags.cancel(drag);
                if r.hold.is_some() {
                    self.let_go_of_left();
                } else {
                    self.ride = None;
                }
                self.machine.on(MacEvent::DragEnded { drag });
            }
            // Dropped already, or never here: stop its fetches.
            _ => self.drags.cancel(drag),
        }
    }

    /// `drag`'s session ended (dropped, or Esc from the PC's keyboard).
    fn session_ended(&mut self, drag: DragId) {
        if let Some(r) = self.ride
            && r.drag == drag
        {
            if r.hold.is_some() {
                // Never leave a hold with the button still down.
                self.let_go_of_left();
            } else {
                // The PC still holds the button: it counts as held until
                // the PC's up (Task 17 note b), so pushing out is a wall.
                self.ride = None;
            }
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
        for action in self.machine.on(ev) {
            match action {
                MacAction::WakeDisplay | MacAction::WarpTo { .. } if !move_cursor => {}
                MacAction::WakeDisplay => self.inj.wake_display(),
                MacAction::WarpTo { edge, frac } => self.inj.warp_to(edge, frac),
                MacAction::SendLeave { frac, .. } => out.push(InjectEv::SendLeave { sid, frac }),
                MacAction::ReleaseAllInjected => self.inj.release_all(),
                MacAction::StartControl => out.push(InjectEv::Controlled { sid, on: true }),
                MacAction::StopControl => out.push(InjectEv::Controlled { sid, on: false }),
                MacAction::BeginDrag { drag } => self.begin_drag(drag),
                // Always before ReleaseAllInjected, which posts the up.
                MacAction::CancelDrag { drag } => {
                    if self.ride.is_some_and(|r| r.drag == drag) {
                        self.escape();
                    }
                    self.ride = None;
                    self.drags.cancel(drag);
                }
            }
        }
        out
    }
}

/// Starts the injection thread; `report` passes its events to the app's
/// loop. It always ends by reporting `Stopped`: after `Quit`, or, having
/// released everything, after a panic.
pub fn spawn_inject_thread<I: Inject + Send + 'static, D: DragPort + Send + 'static>(
    inj: I,
    drags: D,
    access: Arc<AtomicBool>,
    report: impl Fn(InjectEv) + Send + 'static,
) -> Result<crossbeam_channel::Sender<InjectMsg>> {
    let (tx, rx) = crossbeam_channel::unbounded::<InjectMsg>();
    std::thread::Builder::new()
        .name("cj-inject".into())
        .spawn(move || {
            set_user_interactive_qos();
            let mut router = Router::new(inj, drags, access);
            let run = catch_unwind(AssertUnwindSafe(|| {
                loop {
                    let msg = match router.next_tick() {
                        Some(at) => match rx.recv_deadline(at) {
                            Ok(msg) => msg,
                            Err(RecvTimeoutError::Timeout) => {
                                router.tick(Instant::now());
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
                if catch_unwind(AssertUnwindSafe(|| router.release_all())).is_err() {
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
    }

    /// Records every call; holds keys and buttons like the Injector does.
    pub(crate) struct Fake {
        calls: Arc<Mutex<Vec<Call>>>,
        /// `key` panics, like a bug in the Injector would.
        key_panics: bool,
        /// What every `motion` returns.
        push_out: Option<f32>,
        /// What `entry_frac` returns: where the cursor is along the entry edge.
        at_frac: f32,
        keys: BTreeSet<u16>,
        buttons: Vec<MouseButton>,
    }

    impl Fake {
        pub(crate) fn new() -> (Fake, Arc<Mutex<Vec<Call>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let fake = Fake {
                calls: calls.clone(),
                key_panics: false,
                push_out: None,
                at_frac: 0.5,
                keys: BTreeSet::new(),
                buttons: Vec::new(),
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
            (300.0, 200.0)
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
    }

    impl FakeDrags {
        pub(crate) fn new(calls: &Arc<Mutex<Vec<Call>>>) -> FakeDrags {
            FakeDrags {
                calls: calls.clone(),
                prepare_ok: true,
                may_release: false,
            }
        }

        fn log(&self, c: Call) {
            self.calls.lock().unwrap().push(c);
        }
    }

    impl DragPort for FakeDrags {
        fn prepare(&mut self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> bool {
            assert_eq!(at, (300.0, 200.0), "under the cursor");
            self.log(Call::Prepare(drag, items.len()));
            self.prepare_ok
        }
        fn released(&mut self, drag: DragId, at: (f64, f64)) {
            assert_eq!(at, (300.0, 200.0));
            self.log(Call::Released(drag));
        }
        fn may_release(&mut self, drag: DragId, _at: (f64, f64)) -> bool {
            self.log(Call::MayRelease(drag));
            self.may_release
        }
        fn cancel(&mut self, drag: DragId) {
            self.log(Call::CancelSource(drag));
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
                InjectEv::SendLeave { sid: 1, frac: 0.3 },
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
                InjectEv::SendLeave { sid: 1, frac: 0.7 },
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
        assert!(bounced.contains(&InjectEv::SendLeave { sid: 1, frac: 0.7 }));
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
                InjectEv::SendLeave { sid: 1, frac: 0.8 },
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
        let tx = spawn_inject_thread(fake, NoDrags, granted(), move |ev| {
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
        let tx = spawn_inject_thread(fake, NoDrags, granted(), move |ev| {
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
                InjectEv::SendLeave { sid: 1, frac: 0.3 },
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
            vec![ESC_DOWN, ESC_UP, Call::CancelSource(7), Call::ReleaseAll]
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
        assert_eq!(r.handle(InjectMsg::DragReady { drag: 8 }), vec![]);
        assert_eq!(take(&calls), vec![Call::Jiggle(true)], "another drag");
        assert_eq!(r.handle(InjectMsg::DragReady { drag: 7 }), vec![]);
        // Back on the drop spot, then the up.
        assert_eq!(take(&calls), vec![Call::Jiggle(false), LEFT_UP]);
        assert_eq!(r.next_tick(), None);
        // Motion flows again, and the session's end changes nothing: the
        // next push out is a plain leave, with no Esc.
        r.handle(InjectMsg::DragEnded { drag: 7 });
        r.handle(input(1, Input::Motion { dx: 2, dy: 2 }));
        assert_eq!(take(&calls), vec![Call::Motion(2, 2), Call::ReleaseAll]);
    }

    #[test]
    fn cancel_during_hold_posts_escape_then_up() {
        for msg in [
            InjectMsg::CancelDrag { drag: 7 },
            InjectMsg::DragFailed { drag: 7 },
        ] {
            let (mut r, calls) = holding();
            assert_eq!(r.handle(msg.clone()), vec![]);
            assert_eq!(
                take(&calls),
                vec![ESC_DOWN, ESC_UP, Call::CancelSource(7), LEFT_UP],
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
        r.handle(InjectMsg::DragEnded { drag: 7 });
        assert_eq!(take(&calls), vec![LEFT_UP]);
        assert_eq!(r.next_tick(), None);
    }

    #[test]
    fn cancel_while_riding_leaves_the_button_to_the_pc() {
        let (mut r, calls) = riding(false);
        r.handle(InjectMsg::DragFailed { drag: 7 });
        assert_eq!(take(&calls), vec![ESC_DOWN, ESC_UP, Call::CancelSource(7)]);
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
            vec![ESC_DOWN, ESC_UP, Call::CancelSource(7), Call::ReleaseAll]
        );
        assert_eq!(r.next_tick(), None);

        // Losing Accessibility during a hold sends the cursor back the same way.
        let (mut r, calls) = holding();
        assert_eq!(
            r.handle(InjectMsg::AccessLost),
            vec![
                InjectEv::SendLeave { sid: 1, frac: 0.5 },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(
            take(&calls),
            vec![ESC_DOWN, ESC_UP, Call::CancelSource(7), Call::ReleaseAll]
        );
    }

    #[test]
    fn left_counts_as_held_from_begin_drag() {
        let (mut r, calls) = riding(false);
        // Esc on the Mac's side ended the session; the PC still holds the
        // button, so pushing out is a wall.
        r.handle(InjectMsg::DragEnded { drag: 7 });
        assert_eq!(r.handle(input(1, Input::Motion { dx: -9, dy: 0 })), vec![]);
        assert_eq!(take(&calls), vec![Call::Motion(-9, 0)]);
        // Once the PC lets go, the cursor may leave.
        r.handle(input(1, button(false)));
        assert_eq!(
            r.handle(input(1, Input::Motion { dx: -9, dy: 0 })),
            vec![
                InjectEv::SendLeave { sid: 1, frac: 0.3 },
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
                InjectEv::SendLeave { sid: 1, frac: 0.3 },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(
            take(&calls),
            vec![
                Call::Motion(-9, 0),
                ESC_DOWN,
                ESC_UP,
                Call::CancelSource(7),
                Call::ReleaseAll
            ]
        );
        assert_eq!(r.next_tick(), None);
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
        assert_eq!(
            take(&calls),
            vec![
                ESC_DOWN,
                ESC_UP,
                Call::CancelSource(7),
                Call::Wake,
                Call::Warp(Side::Left, 0.5),
                Call::Prepare(8, 3),
                LEFT_DOWN
            ]
        );
    }

    #[test]
    fn thread_jiggles_a_held_drop() {
        let (mut fake, calls) = Fake::new();
        fake.push_out = None;
        let drags = FakeDrags::new(&calls);
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let tx = spawn_inject_thread(fake, drags, granted(), move |ev| {
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
        std::thread::sleep(JIGGLE_EVERY * 2 + Duration::from_millis(200));
        tx.send(InjectMsg::DragReady { drag: 7 }).unwrap();
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
}
