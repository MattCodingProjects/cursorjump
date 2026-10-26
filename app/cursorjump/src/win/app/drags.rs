//! File drag-and-drop in the PC app's loop: the session's `DndLink` and
//! pacer, the catcher's offers, the drags from the Mac with their `Fetch`
//! replies and pills, and the sweep of the drags' temporary folders.
//!
//! - A drag from the PC rides with the cursor: the catcher covers the
//!   screen as the cursor crosses, and its offer goes to the Mac just
//!   before `Enter`. It ends with the Mac's `DragFinished`, a cancel from
//!   either side, or the session; the machine then hears `DragGone`. One
//!   that comes home, or whose OS drag ends on the PC, is cancelled.
//! - A drag from the Mac starts its `DoDragDrop` once it comes home. The
//!   first `Fetch` is the drop (the button is up by then): the machine's
//!   ride ends there (R42), so the edge isn't walled for a long copy, and
//!   the pill shows, at the cursor (R5). It is finished once
//!   `IncomingEnded` and every fetched item are in. Each way it can end
//!   finishes its pill, and every `Fetch` reply is answered, or dropped
//!   with the drag. If cj-dnd is gone, the relay or the watchdog in
//!   `drag_tick` ends it instead of `IncomingEnded`.
//! - `DndLink`'s sending calls (`offer`, `fetch`, `finish`, `cancel`) are
//!   not cancel-safe: they are awaited only in the loop's handlers, after
//!   its `select!` has picked a branch.
//!
//! Known gap (Task 18): a drag refused while the cursor is on the Mac (a
//! failed offer, or one that came back under a new id) leaves the catcher
//! over the screen refusing it until the real release. If the user comes
//! home still holding the button, that drag can't be dropped on the PC in
//! the same gesture; letting go ends it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use cj_core::config::Config;
use cj_core::dnd::{DragId, DragItem, DragOutcome, FailReason};
use cj_core::gaming::game_locked;
use cj_core::pacer::Pacer;
use cj_core::pc_state::{PcAction, PcEvent, Riding, Where};
use cj_core::pill::{DONE_HOLD, FADE};
use cj_core::proto::Control;
use cj_net::files::FilesIn;
use cj_net::session::Session;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use super::{App, Ev};
use crate::common::dnd_link::{
    DndEv, DndLink, Source, incoming_outcome, sweep_temp, sweep_temp_except,
};
use crate::common::transfer::{Dest, OsFreeSpace, SharedPacer};
use crate::win::dnd::{DndCmd, DndPort, DndShared, WinDndEv, left_button_down, spawn_dnd_thread};
use crate::win::input_thread::cursor_pos;
use crate::win::pill::{PillCmd, spawn_pill_thread};
use crate::win::probe::probe_game;

/// After a drop, how long the target may go without asking for the items
/// it hasn't asked for yet before the drag finishes without them.
const FETCH_WAIT: Duration = Duration::from_secs(10);
/// How long a riding drag whose OS drag ended on the PC waits for the
/// Mac's `DragDropped` or `DragFinished` before it is cancelled: the
/// catcher lets go 500 ms after a release it never got, and the Mac's
/// drop may still be on its way.
const GONE_GRACE: Duration = Duration::from_secs(3);
/// A drag from the Mac that hasn't ended, waits on nothing, and has had
/// the left button up this long is ended here: cj-dnd will never say so.
const STRANDED_AFTER: Duration = Duration::from_secs(3);
/// Spooled and downloaded items, read only while their drag goes on.
const OUTGOING_MAX_AGE: Duration = Duration::from_secs(60 * 60);
/// Received items: targets (WhatsApp, Gmail, Discord) may read them well
/// after the drop (R37).
const INCOMING_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The temporary folders are swept at startup and then this often.
pub(super) const SWEEP_EVERY: Duration = Duration::from_secs(60 * 60);
/// How often the drags' timers are looked at: the game lock while a pill
/// may show, `FETCH_WAIT`, `GONE_GRACE` and `STRANDED_AFTER`.
pub(super) const DRAG_TICK: Duration = Duration::from_secs(1);

/// A target's wait for one item's bytes.
type Reply = oneshot::Sender<Result<PathBuf, FailReason>>;

/// Starts cj-dnd, whose events go to the loop. None (with the error
/// logged) leaves drag-and-drop off and the rest as in phase 1.
pub(super) fn start_dnd(cfg: &Config, events: &UnboundedSender<Ev>) -> Option<DndPort> {
    let shared = Arc::new(DndShared::default());
    let tx = events.clone();
    let spawned = spawn_dnd_thread(cfg.clone(), shared.clone(), move |ev| {
        // The loop is gone: so is the app.
        let _ = tx.send(Ev::Dnd(ev));
    });
    match spawned {
        Ok(cmds) => Some(DndPort { shared, cmds }),
        Err(e) => {
            error!("drag-and-drop is off: {e:#}");
            None
        }
    }
}

/// Starts the pill thread, whose ✕ clicks go to the loop.
pub(super) fn start_pills(events: &UnboundedSender<Ev>) -> Option<mpsc::Sender<PillCmd>> {
    let (cancel, dismiss) = (events.clone(), events.clone());
    let spawned = spawn_pill_thread(
        move |drag| {
            let _ = cancel.send(Ev::PillCancel(drag));
        },
        move |drag| {
            let _ = dismiss.send(Ev::PillDismissed(drag));
        },
    );
    match spawned {
        Ok(pill) => Some(pill),
        Err(e) => {
            error!("no progress pills: {e:#}");
            None
        }
    }
}

/// The catcher's latest offer.
struct Offer {
    drag: DragId,
    items: Vec<DragItem>,
    sources: Vec<Source>,
}

/// Drag-and-drop's part of the app.
pub(super) struct Drags {
    /// cj-dnd; None if it didn't start.
    port: Option<DndPort>,
    pill: Option<mpsc::Sender<PillCmd>>,
    /// The live session's drags; None without one.
    link: Option<DndLink>,
    /// The live session's pacer, fed from every pong.
    pacer: Option<SharedPacer>,
    /// Where each session's link sends its events.
    events: UnboundedSender<DndEv>,
    offer: Option<Offer>,
    /// Drags from the PC offered to the Mac and not over yet.
    outgoing: BTreeSet<DragId>,
    /// The riding drag whose OS drag ended on the PC, and when.
    gone: Option<(DragId, Instant)>,
    /// Drags from the Mac since `StartIncoming`, until they finish.
    incoming: BTreeMap<DragId, InDrag>,
    pills: PillHold,
}

impl Drags {
    /// The drags, and where every session's link sends its events.
    pub fn new(
        port: Option<DndPort>,
        pill: Option<mpsc::Sender<PillCmd>>,
    ) -> (Drags, UnboundedReceiver<DndEv>) {
        let (events, rx) = unbounded_channel();
        let drags = Drags {
            port,
            pill,
            link: None,
            pacer: None,
            events,
            offer: None,
            outgoing: BTreeSet::new(),
            gone: None,
            incoming: BTreeMap::new(),
            pills: PillHold::default(),
        };
        (drags, rx)
    }

    /// A new session: its own link and pacer.
    pub fn connect(&mut self, session: &Session) {
        let pacer: SharedPacer = Arc::new(Mutex::new(Pacer::new(Instant::now())));
        let link = DndLink::new(
            session.clone(),
            pacer.clone(),
            Arc::new(OsFreeSpace),
            self.events.clone(),
        );
        self.link = Some(link);
        self.pacer = Some(pacer);
    }

    /// The session is gone: no more strip, offers or park, and every drag
    /// fails (its events follow). Each drag from the Mac is cancelled
    /// here too: once dropped it no longer rides, so the machine's
    /// `Disconnected` doesn't cancel it. Again without a session, this
    /// changes nothing.
    pub fn disconnected(&mut self) {
        self.set_session_up(false);
        if let Some(mut link) = self.link.take() {
            link.disconnected();
        }
        self.pacer = None;
        let drags: Vec<DragId> = self.incoming.keys().copied().collect();
        for drag in drags {
            self.send(DndCmd::CancelIncoming { drag });
            if let Some(inc) = self.incoming.get_mut(&drag) {
                inc.fail_replies(&FailReason::Disconnected);
            }
        }
    }

    /// Tells cj-dnd whether a paired session is up.
    pub fn set_session_up(&self, up: bool) {
        if let Some(port) = &self.port {
            let up = up && self.link.is_some();
            port.shared
                .session_up
                .store(up, std::sync::atomic::Ordering::Release);
        }
    }

    pub fn on_rtt(&self, rtt: Duration, at: Instant) {
        if let Some(pacer) = &self.pacer {
            pacer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .on_rtt(rtt, at);
        }
    }

    /// False if cj-dnd is gone, or never started.
    fn send(&self, cmd: DndCmd) -> bool {
        self.port.as_ref().is_some_and(|port| port.send(cmd))
    }

    fn pill(&self, cmd: PillCmd) {
        if let Some(pill) = &self.pill
            && pill.send(cmd).is_err()
        {
            warn!("pill thread is gone");
        }
    }

    /// Tells the pills whether a game holds the mouse.
    fn send_held(&mut self, now: Instant) {
        if self.pill.is_none() {
            return;
        }
        let held = game_locked(&probe_game());
        self.pills.set_held(held, now);
        self.pill(PillCmd::Held(held));
    }

    /// How the link stands with `drag`.
    fn known(&self, drag: DragId) -> Known {
        match &self.link {
            None => Known::NoLink,
            Some(link) if link.items(drag).is_some() => Known::Yes,
            Some(_) => Known::No,
        }
    }

    /// The drags whose temporary folders are in use.
    fn live(&self) -> Vec<DragId> {
        let offer = self.offer.as_ref().map(|o| o.drag);
        self.outgoing
            .iter()
            .chain(self.incoming.keys())
            .copied()
            .chain(offer)
            .collect()
    }
}

// ------------------------------------------------------------ decisions
//
// The handlers' choices, from the state alone, so they can be tested.

/// An offer made while the cursor is on the Mac is refused unless it is
/// the riding drag's: only a catcher over the screen meets a drag then,
/// one that left the strip before a late park and came back under a new
/// id. Its ride can't go on, and its release must not drop here.
fn refuse_offer_on_mac(riding: Option<Riding>, drag: DragId) -> bool {
    riding != Some(Riding::Outgoing(drag))
}

/// What a riding drag whose OS drag ended on the PC (`Drags::gone`) needs.
#[derive(Debug, PartialEq)]
enum GoneStep {
    Wait,
    /// No drop and no finish came within `GONE_GRACE`.
    Cancel(DragId),
    /// It rides no more: dropped, finished or homed meanwhile.
    Forget,
}

fn gone_step(gone: Option<(DragId, Instant)>, riding: Option<Riding>, now: Instant) -> GoneStep {
    let Some((drag, at)) = gone else {
        return GoneStep::Wait;
    };
    if riding != Some(Riding::Outgoing(drag)) {
        GoneStep::Forget
    } else if now.saturating_duration_since(at) >= GONE_GRACE {
        GoneStep::Cancel(drag)
    } else {
        GoneStep::Wait
    }
}

/// Which drag a link's `Finished` ended.
#[derive(Debug, PartialEq)]
enum FinishedStep {
    /// One from the PC. Still riding: it ended on the Mac's side before
    /// any drop, so the release that ends the PC's drag must not drop it
    /// on the catcher.
    Outgoing {
        cancel_catcher: bool,
    },
    Incoming,
    /// Already let go of here (refused, or closed without the link).
    Neither,
}

fn finished_step(
    outgoing: bool,
    incoming: bool,
    riding: Option<Riding>,
    drag: DragId,
) -> FinishedStep {
    if outgoing {
        FinishedStep::Outgoing {
            cancel_catcher: riding == Some(Riding::Outgoing(drag)),
        }
    } else if incoming {
        FinishedStep::Incoming
    } else {
        FinishedStep::Neither
    }
}

/// How the link stands with a drag from the Mac.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Known {
    /// No session: its link failed every drag, and those `Finished`
    /// events are on their way.
    NoLink,
    /// This session's link has it (it offered it and it isn't over).
    Yes,
    /// This session's link doesn't (it came over an older one).
    No,
}

/// How to end a drag from the Mac once its outcome is known.
#[derive(Debug, PartialEq)]
enum FinishStep {
    /// `finish` on the link; its `Finished` closes it here.
    OnLink,
    /// The old link's `Finished` closes it.
    Await,
    /// No link will: close it here.
    CloseHere,
}

fn finish_step(known: Known) -> FinishStep {
    match known {
        Known::Yes => FinishStep::OnLink,
        Known::NoLink => FinishStep::Await,
        Known::No => FinishStep::CloseHere,
    }
}

/// How to refuse a drag from the Mac.
#[derive(Debug, PartialEq)]
enum RefuseStep {
    /// `cancel` (the `DragCancel`), then `finish`, so the link lets go.
    OnLink,
    /// The link doesn't know it: a bare `DragCancel`.
    ByControl,
}

fn refuse_step(known: Known) -> RefuseStep {
    match known {
        Known::Yes => RefuseStep::OnLink,
        Known::NoLink | Known::No => RefuseStep::ByControl,
    }
}

/// The pill's result for a drag from the Mac that ended with `outcome`.
fn pill_result(outcome: &DragOutcome) -> Result<(), FailReason> {
    match outcome {
        DragOutcome::Done => Ok(()),
        DragOutcome::NotDropped => Err(FailReason::Cancelled),
        DragOutcome::Failed(why) => Err(why.clone()),
    }
}

/// A drag from the Mac, as the app follows it.
#[derive(Debug)]
struct InDrag {
    items: Vec<DragItem>,
    /// Dropped: its first `Fetch` came, and its pill started.
    pill: bool,
    /// `IncomingEnded` (or what stands for it): whether a target took
    /// it, and when.
    ended: Option<(bool, Instant)>,
    /// Each item asked for, and its result once in.
    fetched: BTreeMap<u32, Option<Result<(), FailReason>>>,
    /// The targets' waits not answered yet.
    replies: BTreeMap<u32, Reply>,
    last_fetch: Option<Instant>,
    /// Cancelled from either side: items not asked for yet never will be.
    cancelled: Option<FailReason>,
    /// Since when the left button has been up while it waits on nothing
    /// and hasn't ended (`stranded`).
    left_up_since: Option<Instant>,
    /// `finish` was called.
    finishing: bool,
}

impl InDrag {
    fn new(items: Vec<DragItem>) -> InDrag {
        InDrag {
            items,
            pill: false,
            ended: None,
            fetched: BTreeMap::new(),
            replies: BTreeMap::new(),
            last_fetch: None,
            cancelled: None,
            left_up_since: None,
            finishing: false,
        }
    }

    /// A target asks for `item`. True for the first ask, the drop: the
    /// ride ends and the pill starts then.
    fn fetch(&mut self, item: u32, reply: Reply, now: Instant) -> bool {
        if let Some(old) = self.replies.insert(item, reply) {
            let _ = old.send(Err(FailReason::Cancelled));
        }
        self.fetched.insert(item, None);
        self.last_fetch = Some(now);
        !std::mem::replace(&mut self.pill, true)
    }

    /// `item` is in, or failed: its wait is answered.
    fn received(&mut self, item: u32, result: Result<PathBuf, FailReason>) {
        if let Some(reply) = self.replies.remove(&item) {
            // The target may have stopped waiting.
            let _ = reply.send(result.clone());
        }
        self.fetched.insert(item, Some(result.map(drop)));
    }

    /// Fails every wait not answered yet.
    fn fail_replies(&mut self, why: &FailReason) {
        let items: Vec<u32> = self.replies.keys().copied().collect();
        for item in items {
            self.received(item, Err(why.clone()));
        }
    }

    fn cancel(&mut self, why: FailReason) {
        self.cancelled.get_or_insert(why);
    }

    fn ended(&mut self, dropped: bool, now: Instant) {
        self.ended.get_or_insert((dropped, now));
    }

    /// One sample of the safety net for an `IncomingEnded` that never
    /// comes (cj-dnd gone): true once the drag hasn't ended, waits on no
    /// reply, and the left button has been up for `STRANDED_AFTER`. A
    /// drag still held, or being fetched, starts the count again.
    fn stranded(&mut self, left_down: bool, now: Instant) -> bool {
        if self.ended.is_some() || !self.replies.is_empty() || left_down {
            self.left_up_since = None;
            return false;
        }
        let since = *self.left_up_since.get_or_insert(now);
        now.saturating_duration_since(since) >= STRANDED_AFTER
    }

    /// What to `finish` with, once known and not yet sent: `NotDropped` as
    /// soon as it ends without a drop; after a drop, its fetched items'
    /// outcome once each is in and either every item was asked for, or
    /// it was cancelled, or the target has asked for nothing more for
    /// `FETCH_WAIT` (Explorer asks before `IncomingEnded`, Chromium may
    /// ask after it). A cancelled drag with nothing failed fails with the
    /// cancel's reason.
    fn outcome(&self, now: Instant) -> Option<DragOutcome> {
        if self.finishing {
            return None;
        }
        let (dropped, at) = self.ended?;
        if !dropped {
            return Some(DragOutcome::NotDropped);
        }
        let quiet_from = self.last_fetch.map_or(at, |f| f.max(at));
        let asks_to_come = self.fetched.len() < self.items.len() && now < quiet_from + FETCH_WAIT;
        if self.cancelled.is_none() && asks_to_come {
            return None;
        }
        let results: Vec<_> = self.fetched.values().cloned().collect();
        match (incoming_outcome(true, &results)?, &self.cancelled) {
            (DragOutcome::Done, Some(why)) => Some(DragOutcome::Failed(why.clone())),
            (outcome, _) => Some(outcome),
        }
    }
}

/// Whether a pill may be on screen, so the game lock is worth probing.
#[derive(Debug, Default)]
struct PillHold {
    running: BTreeSet<DragId>,
    /// Failed pills stay until their ✕ (`dismiss`).
    failed: BTreeSet<DragId>,
    /// Finished pills show their ✓ and fade until then.
    fading_until: Option<Instant>,
    /// Held is what the pills were last told.
    held: bool,
}

impl PillHold {
    fn start(&mut self, drag: DragId) {
        self.running.insert(drag);
    }

    fn finish(&mut self, drag: DragId, result: &Result<(), FailReason>, now: Instant) {
        if !self.running.remove(&drag) {
            return;
        }
        match result {
            Err(why) if *why != FailReason::Cancelled => {
                self.failed.insert(drag);
            }
            _ => self.fade_from(now),
        }
    }

    /// A failed pill's ✕ took it off the board.
    fn dismiss(&mut self, drag: DragId) {
        self.failed.remove(&drag);
    }

    fn fade_from(&mut self, now: Instant) {
        let until = now + DONE_HOLD + FADE;
        self.fading_until = Some(self.fading_until.map_or(until, |t| t.max(until)));
    }

    /// A release replays the finished pills' ✓ and fade from `now`.
    fn set_held(&mut self, held: bool, now: Instant) {
        if self.held && !held {
            self.fade_from(now);
        }
        self.held = held;
    }

    /// While held this stays true, so the release is always seen.
    fn pending(&self, now: Instant) -> bool {
        !self.running.is_empty()
            || !self.failed.is_empty()
            || self.held
            || self.fading_until.is_some_and(|t| now < t)
    }
}

/// `%LOCALAPPDATA%\CursorJump`, which holds the drags' `outgoing` and
/// `incoming` folders.
fn temp_base() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("CursorJump"))
}

impl App {
    // -------------------------------------------------------- from cj-dnd

    pub(super) async fn on_dnd(&mut self, ev: WinDndEv) {
        match ev {
            WinDndEv::Offer {
                drag,
                items,
                sources,
            } => {
                if self.machine.where_() == &Where::Mac {
                    if refuse_offer_on_mac(self.machine.riding(), drag) {
                        // See the module doc's known gap.
                        info!(drag, "dnd: a new drag while on the Mac; refused");
                        self.drags.send(DndCmd::Cancel { drag });
                    }
                    return;
                }
                debug!(drag, items = items.len(), "dnd: offered at the edge");
                self.drags.offer = Some(Offer {
                    drag,
                    items,
                    sources,
                });
            }
            WinDndEv::OfferGone { drag } => {
                if self.drags.offer.as_ref().is_some_and(|o| o.drag == drag) {
                    self.drags.offer = None;
                }
                if self.machine.riding() == Some(Riding::Outgoing(drag)) {
                    // The OS drag ended on the PC, or left the catcher,
                    // while riding. The Mac's drop may be on its way: the
                    // catcher lets go of a release it never got late.
                    info!(drag, "dnd: the riding drag ended on the PC");
                    self.drags.gone = Some((drag, Instant::now()));
                }
            }
            WinDndEv::Spooled { drag } => debug!(drag, "dnd: spooled"),
            WinDndEv::Fetch {
                drag,
                item,
                dir,
                reply,
            } => self.on_fetch(drag, item, dir, reply).await,
            WinDndEv::IncomingEnded { drag, dropped } => {
                debug!(drag, dropped, "dnd: the drag from the Mac ended here");
                self.incoming_ended(drag, dropped).await;
            }
        }
    }

    /// A drag from the Mac ended here (or is taken to have). A second end
    /// changes nothing.
    async fn incoming_ended(&mut self, drag: DragId, dropped: bool) {
        self.feed(PcEvent::DragGone { drag });
        if let Some(inc) = self.drags.incoming.get_mut(&drag) {
            inc.ended(dropped, Instant::now());
        }
        self.try_finish(drag).await;
    }

    /// A target asks for an item's bytes. Never held back for
    /// `IncomingEnded`, which may wait on the reply.
    async fn on_fetch(&mut self, drag: DragId, item: u32, dir: PathBuf, reply: Reply) {
        let up = self.drags.link.is_some();
        let Some(inc) = self.drags.incoming.get_mut(&drag).filter(|_| up) else {
            let why = if up {
                info!(drag, item, "dnd: fetch for a drag that is over; refused");
                FailReason::Cancelled
            } else {
                debug!(drag, item, "dnd: fetch without a session; refused");
                FailReason::Disconnected
            };
            let _ = reply.send(Err(why));
            return;
        };
        let now = Instant::now();
        if inc.fetch(item, reply, now) {
            // The drop: the button is up, so the ride is over (R42), and
            // its pill shows on that monitor.
            let (items, at) = (inc.items.clone(), cursor_pos().unwrap_or_default());
            self.feed(PcEvent::DragGone { drag });
            self.drags.pills.start(drag);
            self.drags.pill(PillCmd::Start { drag, items, at });
            self.drags.send_held(now);
        }
        let Some(link) = self.drags.link.as_mut() else {
            return;
        };
        if let Err(e) = link.fetch(drag, item, Dest::InDir(dir)).await {
            warn!(drag, item, "dnd: fetch not started: {e:#}");
            // Its `Received`, if one comes, finds the reply answered.
            let why = if self.session().is_some() {
                FailReason::Cancelled
            } else {
                FailReason::Disconnected
            };
            if let Some(inc) = self.drags.incoming.get_mut(&drag) {
                inc.received(item, Err(why));
            }
        }
        self.try_finish(drag).await;
    }

    // ------------------------------------------------------ from the link

    pub(super) async fn on_link_dnd(&mut self, ev: DndEv) {
        match ev {
            DndEv::Progress { drag, done, total } => {
                if self.drags.incoming.get(&drag).is_some_and(|inc| inc.pill) {
                    self.drags.pill(PillCmd::Progress { drag, done, total });
                }
            }
            DndEv::Received { drag, item, result } => {
                if let Some(inc) = self.drags.incoming.get_mut(&drag) {
                    inc.received(item, result);
                }
                self.try_finish(drag).await;
            }
            DndEv::Finished { drag, outcome } => {
                debug!(drag, ?outcome, "dnd: finished");
                let step = finished_step(
                    self.drags.outgoing.remove(&drag),
                    self.drags.incoming.contains_key(&drag),
                    self.machine.riding(),
                    drag,
                );
                match step {
                    FinishedStep::Outgoing { cancel_catcher } => {
                        if cancel_catcher {
                            self.drags.send(DndCmd::Cancel { drag });
                        }
                        self.feed(PcEvent::DragGone { drag });
                    }
                    FinishedStep::Incoming => {
                        if let Some(inc) = self.drags.incoming.remove(&drag) {
                            self.close_incoming(drag, inc, &outcome);
                        }
                    }
                    FinishedStep::Neither => {}
                }
            }
        }
    }

    /// A drag from the Mac is over: any wait left fails and its pill ends.
    fn close_incoming(&mut self, drag: DragId, mut inc: InDrag, outcome: &DragOutcome) {
        let result = pill_result(outcome);
        inc.fail_replies(result.as_ref().err().unwrap_or(&FailReason::Cancelled));
        if inc.pill {
            self.drags.pills.finish(drag, &result, Instant::now());
            self.drags.pill(PillCmd::Finish { drag, result });
        }
    }

    /// Finishes a drag from the Mac once its outcome is known.
    async fn try_finish(&mut self, drag: DragId) {
        let known = self.drags.known(drag);
        let Some(inc) = self.drags.incoming.get_mut(&drag) else {
            return;
        };
        let Some(outcome) = inc.outcome(Instant::now()) else {
            return;
        };
        inc.finishing = true;
        info!(drag, ?outcome, "dnd: the drag from the Mac is over");
        match finish_step(known) {
            FinishStep::OnLink => {
                if let Some(link) = self.drags.link.as_mut()
                    && let Err(e) = link.finish(drag, outcome).await
                {
                    debug!(drag, "dnd: finish not sent: {e:#}");
                }
            }
            FinishStep::Await => {}
            FinishStep::CloseHere => {
                if let Some(inc) = self.drags.incoming.remove(&drag) {
                    self.close_incoming(drag, inc, &outcome);
                }
            }
        }
    }

    /// Ends a drag either way, telling the Mac; the machine hears
    /// `DragGone` once it is over.
    async fn cancel_drag(&mut self, drag: DragId) {
        match self.drags.link.as_mut() {
            Some(link) => {
                if let Err(e) = link.cancel(drag, FailReason::Cancelled).await {
                    debug!(drag, "dnd: cancel not sent: {e:#}");
                }
            }
            None => self.feed(PcEvent::DragGone { drag }),
        }
    }

    // --------------------------------------------------- from the session

    /// A drag message from the Mac.
    pub(super) fn on_drag_control(&mut self, c: Control) {
        let paired = self.paired_session().is_some();
        let Some(link) = self.drags.link.as_mut().filter(|_| paired) else {
            debug!(?c, "dnd: no paired session; ignored");
            return;
        };
        match c {
            Control::DragOffer { drag, items } => link.on_offer(drag, items),
            Control::FileFetch { drag, item } => link.on_fetch(drag, item),
            Control::DragFinished { drag, outcome } => link.on_finished(drag, outcome),
            Control::DragCancel { drag, reason } => {
                link.on_cancel(drag, reason.clone());
                if let Some(inc) = self.drags.incoming.get_mut(&drag) {
                    inc.cancel(reason);
                    self.drags.send(DndCmd::CancelIncoming { drag });
                }
            }
            Control::DragDropped { drag } => self.feed(PcEvent::DragDropped { drag }),
            other => debug!(?other, "dnd: not a drag message"),
        }
    }

    /// A files stream from the Mac; dropped (refused) without a paired session.
    pub(super) fn on_files(&mut self, files: FilesIn) {
        let paired = self.paired_session().is_some();
        match self.drags.link.as_mut() {
            Some(link) if paired => link.on_files(files),
            _ => debug!("dnd: files stream without a paired session; refused"),
        }
    }

    pub(super) fn on_rtt(&self, rtt: Duration, at: Instant) {
        self.drags.on_rtt(rtt, at);
    }

    // --------------------------------------------------- from the machine

    /// Offers the riding drag to the Mac, after the input thread was told
    /// to cross with it (so the catcher already covers the screen) and
    /// before `Enter`. None to cross without it (no offer for it, or it
    /// couldn't be sent): the catcher then refuses its drop until the real
    /// release, so the button held on the Mac drops nothing on the PC.
    pub(super) async fn offer_ride(&mut self, drag: DragId) -> Option<DragId> {
        let offer = self.drags.offer.take_if(|o| o.drag == drag);
        let sent = match (offer, self.drags.link.as_mut()) {
            (Some(o), Some(link)) => match link.offer(drag, o.items, o.sources).await {
                Ok(()) => true,
                Err(e) => {
                    warn!(drag, "dnd: offer not sent: {e:#}");
                    false
                }
            },
            _ => {
                warn!(drag, "dnd: no offer for the drag; crossing without it");
                false
            }
        };
        if sent {
            self.drags.outgoing.insert(drag);
            Some(drag)
        } else {
            // See the module doc's known gap.
            self.drags.send(DndCmd::Cancel { drag });
            self.feed(PcEvent::DragGone { drag });
            None
        }
    }

    /// The machine's drag actions.
    pub(super) async fn exec_drag(&mut self, action: PcAction) {
        match action {
            PcAction::CancelSourceDrag { drag } => {
                self.drags.send(DndCmd::Cancel { drag });
                self.cancel_drag(drag).await;
            }
            PcAction::SourceDragHome { drag } => {
                // Only moves the catcher back for a drag already over.
                self.drags.send(DndCmd::Home { drag });
                // The OS drag goes on on the PC; the Mac can't drop it now.
                self.cancel_drag(drag).await;
            }
            PcAction::StartIncomingDrag { drag } => {
                let items = self
                    .drags
                    .link
                    .as_ref()
                    .and_then(|l| l.items(drag))
                    .filter(|_| self.drags.port.is_some())
                    .map(<[DragItem]>::to_vec);
                let Some(items) = items else {
                    warn!(drag, "dnd: no offer for the drag from the Mac; refused");
                    self.refuse_drag(drag).await;
                    self.feed(PcEvent::DragGone { drag });
                    return;
                };
                let cmd = DndCmd::StartIncoming {
                    drag,
                    items: items.clone(),
                };
                self.drags.incoming.insert(drag, InDrag::new(items));
                if !self.drags.send(cmd) {
                    // No `IncomingEnded` will come.
                    error!(drag, "dnd: cj-dnd is gone; the drag from the Mac ends");
                    self.incoming_ended(drag, false).await;
                }
            }
            PcAction::CancelIncomingDrag { drag } => {
                // Only the machine's `Disconnected` asks for it.
                self.drags.send(DndCmd::CancelIncoming { drag });
                if let Some(inc) = self.drags.incoming.get_mut(&drag) {
                    inc.fail_replies(&FailReason::Disconnected);
                }
            }
            PcAction::RefuseIncomingDrag { drag } => self.refuse_drag(drag).await,
            other => debug!(?other, "dnd: not a drag action"),
        }
    }

    /// Tells the Mac a drag of its won't start here (`DragCancel`), and
    /// ends it on the link.
    async fn refuse_drag(&mut self, drag: DragId) {
        let reason = FailReason::Cancelled;
        match refuse_step(self.drags.known(drag)) {
            RefuseStep::ByControl => self.control(Control::DragCancel { drag, reason }).await,
            RefuseStep::OnLink => {
                let Some(link) = self.drags.link.as_mut() else {
                    return;
                };
                let refused = DragOutcome::Failed(reason.clone());
                let sent = match link.cancel(drag, reason).await {
                    Ok(()) => link.finish(drag, refused).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = sent {
                    debug!(drag, "dnd: refusal not sent: {e:#}");
                }
            }
        }
    }

    // ------------------------------------------------------------- pills

    /// A running pill's ✕: the copy stops, and the target's waits fail.
    pub(super) async fn on_pill_cancel(&mut self, drag: DragId) {
        let Some(inc) = self.drags.incoming.get_mut(&drag) else {
            return;
        };
        info!(drag, "dnd: cancelled from its pill");
        inc.cancel(FailReason::Cancelled);
        inc.fail_replies(&FailReason::Cancelled);
        self.drags.send(DndCmd::CancelIncoming { drag });
        if let Some(link) = self.drags.link.as_mut()
            && let Err(e) = link.cancel(drag, FailReason::Cancelled).await
        {
            debug!(drag, "dnd: cancel not sent: {e:#}");
        }
        self.try_finish(drag).await;
    }

    /// A failed pill's ✕ took it away.
    pub(super) fn on_pill_dismissed(&mut self, drag: DragId) {
        self.drags.pills.dismiss(drag);
    }

    /// Every `DRAG_TICK`: the game lock for the pills, a riding drag the
    /// PC let go of, drags from the Mac whose end cj-dnd won't tell, and
    /// drops whose targets have stopped asking.
    pub(super) async fn drag_tick(&mut self) {
        let now = Instant::now();
        if self.drags.pills.pending(now) {
            self.drags.send_held(now);
        }
        match gone_step(self.drags.gone, self.machine.riding(), now) {
            GoneStep::Wait => {}
            GoneStep::Forget => self.drags.gone = None,
            GoneStep::Cancel(drag) => {
                self.drags.gone = None;
                info!(drag, "dnd: no drop on the Mac came; the ride is cancelled");
                self.cancel_drag(drag).await;
            }
        }
        let drags: Vec<DragId> = self.drags.incoming.keys().copied().collect();
        if !drags.is_empty() {
            let left_down = left_button_down();
            for &drag in &drags {
                let Some(inc) = self.drags.incoming.get_mut(&drag) else {
                    continue;
                };
                if inc.stranded(left_down, now) {
                    let dropped = !inc.fetched.is_empty();
                    warn!(
                        drag,
                        dropped, "dnd: cj-dnd never ended the drag from the Mac; ended here"
                    );
                    self.drags.send(DndCmd::CancelIncoming { drag });
                    self.incoming_ended(drag, dropped).await;
                }
            }
        }
        for drag in drags {
            self.try_finish(drag).await;
        }
    }

    // ------------------------------------------------------------- sweep

    /// Removes old spooled and received items, off the loop. At startup
    /// no drag is live yet.
    pub(super) fn sweep_temp_dirs(&self, startup: bool) {
        let Some(base) = temp_base() else {
            return;
        };
        let live = self.drags.live();
        tokio::task::spawn_blocking(move || {
            let now = SystemTime::now();
            for (kind, max_age) in [
                ("outgoing", OUTGOING_MAX_AGE),
                ("incoming", INCOMING_MAX_AGE),
            ] {
                let root = base.join(kind);
                let removed = if startup {
                    sweep_temp(&root, max_age, now)
                } else {
                    sweep_temp_except(&root, max_age, now, &live)
                };
                if removed > 0 {
                    info!(removed, "swept old {kind} drag folders");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_core::dnd::ItemKind;

    fn file(name: &str) -> DragItem {
        DragItem {
            name: name.into(),
            kind: ItemKind::File,
            size: Some(10),
        }
    }

    fn two() -> InDrag {
        InDrag::new(vec![file("a.txt"), file("b.txt")])
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_pill_starts_at_the_first_fetch_only() {
        let (t0, mut inc) = (Instant::now(), two());
        let (a, _ra) = oneshot::channel();
        let (b, _rb) = oneshot::channel();
        assert!(inc.fetch(0, a, t0), "the first ask is the drop");
        assert!(!inc.fetch(1, b, t0), "a second ask starts no second pill");
    }

    #[test]
    fn a_received_item_answers_its_wait() {
        let (t0, mut inc) = (Instant::now(), two());
        let (a, mut ra) = oneshot::channel();
        inc.fetch(0, a, t0);
        assert!(ra.try_recv().is_err(), "answered early");
        inc.received(0, Ok(PathBuf::from(r"C:\x\a.txt")));
        assert_eq!(ra.try_recv().unwrap(), Ok(PathBuf::from(r"C:\x\a.txt")));
        // A late second result for it has no wait left to answer.
        inc.received(0, Err(FailReason::Disconnected));
    }

    #[test]
    fn failing_the_waits_answers_each_once() {
        let (t0, mut inc) = (Instant::now(), two());
        let (a, mut ra) = oneshot::channel();
        let (b, mut rb) = oneshot::channel();
        inc.fetch(0, a, t0);
        inc.fetch(1, b, t0);
        inc.received(0, Ok(PathBuf::from("a.txt")));
        inc.fail_replies(&FailReason::Cancelled);
        assert_eq!(ra.try_recv().unwrap(), Ok(PathBuf::from("a.txt")));
        assert_eq!(rb.try_recv().unwrap(), Err(FailReason::Cancelled));
        assert!(inc.replies.is_empty());
        inc.ended(true, t0);
        assert_eq!(
            inc.outcome(t0),
            Some(DragOutcome::Failed(FailReason::Cancelled))
        );
    }

    #[test]
    fn nothing_finishes_before_the_drag_ends() {
        let (t0, mut inc) = (Instant::now(), two());
        let (a, _ra) = oneshot::channel();
        let (b, _rb) = oneshot::channel();
        inc.fetch(0, a, t0);
        inc.fetch(1, b, t0);
        inc.received(0, Ok("a".into()));
        inc.received(1, Ok("b".into()));
        assert_eq!(
            inc.outcome(t0),
            None,
            "a Fetch can come before IncomingEnded"
        );
        inc.ended(true, t0);
        assert_eq!(inc.outcome(t0), Some(DragOutcome::Done));
    }

    #[test]
    fn not_dropped_finishes_at_once() {
        let (t0, mut inc) = (Instant::now(), two());
        let (a, _ra) = oneshot::channel();
        // A hover ask that raced a refused drop.
        inc.fetch(0, a, t0);
        inc.ended(false, t0);
        assert_eq!(inc.outcome(t0), Some(DragOutcome::NotDropped));
    }

    #[test]
    fn a_drop_waits_for_every_item_it_asked_for() {
        let (t0, mut inc) = (Instant::now(), two());
        let (a, _ra) = oneshot::channel();
        let (b, _rb) = oneshot::channel();
        inc.fetch(0, a, t0);
        inc.fetch(1, b, t0);
        inc.ended(true, t0);
        inc.received(1, Err(FailReason::NoSpace));
        assert_eq!(
            inc.outcome(t0 + FETCH_WAIT * 2),
            None,
            "item 0 is still coming"
        );
        inc.received(0, Ok("a".into()));
        assert_eq!(
            inc.outcome(t0),
            Some(DragOutcome::Failed(FailReason::NoSpace))
        );
    }

    #[test]
    fn a_drop_waits_a_while_for_asks_still_to_come() {
        let (t0, mut inc) = (Instant::now(), two());
        inc.ended(true, t0);
        assert_eq!(
            inc.outcome(t0),
            None,
            "Chromium asks after DoDragDrop returns"
        );
        let (a, _ra) = oneshot::channel();
        inc.fetch(0, a, t0 + ms(500));
        inc.received(0, Ok("a".into()));
        let quiet = t0 + ms(500) + FETCH_WAIT;
        assert_eq!(
            inc.outcome(quiet - ms(1)),
            None,
            "item 1 may still be asked for"
        );
        assert_eq!(inc.outcome(quiet), Some(DragOutcome::Done));

        let mut never = two();
        never.ended(true, t0);
        assert_eq!(
            never.outcome(t0 + FETCH_WAIT),
            Some(DragOutcome::Done),
            "a target that took the drop and never asked"
        );
    }

    #[test]
    fn a_cancel_stops_waiting_for_asks() {
        let (t0, mut inc) = (Instant::now(), two());
        let (a, _ra) = oneshot::channel();
        inc.fetch(0, a, t0);
        inc.ended(true, t0);
        // The pill's ✕.
        inc.cancel(FailReason::Cancelled);
        inc.fail_replies(&FailReason::Cancelled);
        assert_eq!(
            inc.outcome(t0),
            Some(DragOutcome::Failed(FailReason::Cancelled)),
            "item 1 will never be asked for"
        );

        // Cancelled by the Mac after item 0 arrived: still a failure.
        let mut inc = two();
        let (a, _ra) = oneshot::channel();
        inc.fetch(0, a, t0);
        inc.received(0, Ok("a".into()));
        inc.ended(true, t0);
        inc.cancel(FailReason::Unreadable { name: "b".into() });
        assert_eq!(
            inc.outcome(t0),
            Some(DragOutcome::Failed(FailReason::Unreadable {
                name: "b".into()
            }))
        );

        // A fetch still in flight is waited for.
        let mut inc = two();
        let (a, _ra) = oneshot::channel();
        inc.fetch(0, a, t0);
        inc.ended(true, t0);
        inc.cancel(FailReason::Cancelled);
        assert_eq!(inc.outcome(t0), None);
    }

    #[test]
    fn finish_is_asked_for_once() {
        let (t0, mut inc) = (Instant::now(), two());
        inc.ended(false, t0);
        assert!(inc.outcome(t0).is_some());
        inc.finishing = true;
        assert_eq!(inc.outcome(t0), None);
        inc.ended(true, t0);
        assert_eq!(inc.ended, Some((false, t0)), "the first end counts");
    }

    #[test]
    fn a_stranded_drag_is_caught_once_the_button_has_been_up() {
        let (t0, mut inc) = (Instant::now(), two());
        assert!(!inc.stranded(true, t0), "still held: DoDragDrop runs");
        assert!(!inc.stranded(false, t0 + ms(100)));
        assert!(!inc.stranded(false, t0 + ms(100) + STRANDED_AFTER - ms(1)));
        assert!(inc.stranded(false, t0 + ms(100) + STRANDED_AFTER));

        // Pressed again in between: the count starts over.
        let mut inc = two();
        assert!(!inc.stranded(false, t0));
        assert!(!inc.stranded(true, t0 + ms(2000)));
        assert!(!inc.stranded(false, t0 + STRANDED_AFTER));
        assert!(inc.stranded(false, t0 + STRANDED_AFTER * 2));
    }

    #[test]
    fn a_drag_being_fetched_or_ended_is_never_stranded() {
        let (t0, mut inc) = (Instant::now(), two());
        let (a, _ra) = oneshot::channel();
        inc.fetch(0, a, t0);
        let late = t0 + STRANDED_AFTER * 10;
        assert!(!inc.stranded(false, t0));
        assert!(!inc.stranded(false, late), "Explorer's copy holds the drag");

        let mut ended = two();
        ended.ended(true, t0);
        assert!(!ended.stranded(false, t0));
        assert!(!ended.stranded(false, late));

        // Taken as ended, a late real `IncomingEnded` changes nothing.
        let mut inc = two();
        inc.stranded(false, t0);
        assert!(inc.stranded(false, late));
        inc.ended(false, late);
        inc.finishing = true;
        inc.ended(true, late + ms(5));
        assert_eq!(inc.ended, Some((false, late)));
        assert_eq!(inc.outcome(late + ms(5)), None, "finished only once");
    }

    #[test]
    fn pill_results() {
        assert_eq!(pill_result(&DragOutcome::Done), Ok(()));
        assert_eq!(
            pill_result(&DragOutcome::NotDropped),
            Err(FailReason::Cancelled)
        );
        assert_eq!(
            pill_result(&DragOutcome::Failed(FailReason::Disconnected)),
            Err(FailReason::Disconnected)
        );
    }

    #[test]
    fn pills_are_pending_while_they_may_show() {
        let (t0, mut p) = (Instant::now(), PillHold::default());
        assert!(!p.pending(t0));
        p.start(1);
        assert!(p.pending(t0));
        p.finish(1, &Ok(()), t0);
        assert!(p.pending(t0 + DONE_HOLD), "still fading");
        assert!(!p.pending(t0 + DONE_HOLD + FADE));

        p.start(2);
        p.finish(2, &Err(FailReason::Cancelled), t0);
        assert!(!p.pending(t0 + DONE_HOLD + FADE), "a cancel fades too");

        p.start(3);
        p.start(4);
        p.finish(3, &Err(FailReason::NoSpace), t0);
        p.finish(4, &Err(FailReason::Unwritable), t0);
        let later = t0 + DRAG_TICK * 1000;
        assert!(p.pending(later), "a failure stays up");
        p.dismiss(3);
        assert!(p.pending(later), "until each is dismissed");
        p.dismiss(4);
        assert!(!p.pending(later));
    }

    #[test]
    fn a_hold_is_always_released() {
        let (t0, mut p) = (Instant::now(), PillHold::default());
        p.start(1);
        p.set_held(true, t0);
        p.finish(1, &Ok(()), t0);
        let later = t0 + Duration::from_secs(60);
        assert!(p.pending(later), "held: the release must still be sent");
        p.set_held(false, later);
        assert!(
            p.pending(later + DONE_HOLD),
            "the ✓ replays from the release"
        );
        assert!(!p.pending(later + DONE_HOLD + FADE));
    }

    #[test]
    fn a_finish_for_an_unknown_pill_is_ignored() {
        let (t0, mut p) = (Instant::now(), PillHold::default());
        p.finish(9, &Err(FailReason::NoSpace), t0);
        assert!(!p.pending(t0));
    }

    /// The machine, connected, with the Mac's drag 9 come home and riding.
    fn riding_incoming() -> cj_core::pc_state::PcMachine {
        use cj_core::gaming::GameProbe;
        use cj_core::layout::Rect;
        let screen = Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let free = GameProbe {
            clip: screen,
            virtual_screen: screen,
            cursor_showing: true,
            d3d_fullscreen: false,
        };
        let mut m = cj_core::pc_state::PcMachine::new(Config::default());
        m.on(PcEvent::Connected {
            paired: true,
            code: String::new(),
        });
        m.on(PcEvent::EdgePush {
            frac: 0.5,
            game: free,
            buttons_held: false,
            offer: None,
        });
        m.on(PcEvent::MacLeave {
            frac: 0.5,
            drag: Some(9),
            game: false,
        });
        assert_eq!(m.riding(), Some(Riding::Incoming(9)));
        m
    }

    #[test]
    fn the_drop_ends_the_ride_so_the_edge_opens() {
        let mut m = riding_incoming();
        let (t0, mut inc) = (Instant::now(), two());
        let (a, _ra) = oneshot::channel();
        // What `on_fetch` feeds at the drop, long before `IncomingEnded`.
        assert!(inc.fetch(0, a, t0));
        m.on(PcEvent::DragGone { drag: 9 });
        assert_eq!(m.riding(), None, "the edge stays walled for the copy");
        // The later `IncomingEnded` feeds it again: nothing changes.
        assert_eq!(m.on(PcEvent::DragGone { drag: 9 }), vec![]);
        // So the machine's `Disconnected` no longer cancels the drag ...
        let actions = m.on(PcEvent::Disconnected);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, PcAction::CancelIncomingDrag { .. })),
            "{actions:?}"
        );
    }

    #[test]
    fn a_lost_session_cancels_every_drag_from_the_mac() {
        // ... `Drags::disconnected` does, and fails their waits.
        let (tx, rx) = mpsc::channel();
        let port = DndPort {
            shared: Arc::new(DndShared::default()),
            cmds: tx,
        };
        let (mut drags, _events) = Drags::new(Some(port), None);
        let t0 = Instant::now();
        let mut dropped = two();
        let (a, mut ra) = oneshot::channel();
        dropped.fetch(0, a, t0);
        drags.incoming.insert(9, dropped);
        drags.incoming.insert(10, two());

        drags.disconnected();
        let sent: Vec<DndCmd> = rx.try_iter().collect();
        assert_eq!(
            sent,
            [
                DndCmd::CancelIncoming { drag: 9 },
                DndCmd::CancelIncoming { drag: 10 }
            ]
        );
        assert_eq!(ra.try_recv().unwrap(), Err(FailReason::Disconnected));
        assert!(
            !drags
                .port
                .as_ref()
                .unwrap()
                .shared
                .session_up
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn only_the_riding_drag_is_offered_while_on_the_mac() {
        assert!(refuse_offer_on_mac(None, 8), "came back under a new id");
        assert!(refuse_offer_on_mac(Some(Riding::Outgoing(7)), 8));
        assert!(refuse_offer_on_mac(Some(Riding::Incoming(8)), 8));
        assert!(!refuse_offer_on_mac(Some(Riding::Outgoing(8)), 8));
    }

    #[test]
    fn a_ride_the_pc_let_go_of_is_cancelled_only_after_the_grace() {
        let t0 = Instant::now();
        let riding = Some(Riding::Outgoing(7));
        assert_eq!(gone_step(None, riding, t0), GoneStep::Wait);
        let gone = Some((7, t0));
        assert_eq!(
            gone_step(gone, riding, t0 + GONE_GRACE - ms(1)),
            GoneStep::Wait
        );
        assert_eq!(
            gone_step(gone, riding, t0 + GONE_GRACE),
            GoneStep::Cancel(7)
        );
        // `DragDropped`, a `Finished` (`DragGone`) or a `MacLeave` (which
        // homes and cancels it) ended the ride first.
        assert_eq!(gone_step(gone, None, t0 + GONE_GRACE), GoneStep::Forget);
        let other = Some(Riding::Outgoing(8));
        assert_eq!(gone_step(gone, other, t0), GoneStep::Forget);
    }

    #[test]
    fn a_finished_ride_cancels_the_catcher_only_while_riding() {
        let riding = Some(Riding::Outgoing(7));
        assert_eq!(
            finished_step(true, false, riding, 7),
            FinishedStep::Outgoing {
                cancel_catcher: true
            },
            "over on the Mac's side before a drop"
        );
        assert_eq!(
            finished_step(true, false, None, 7),
            FinishedStep::Outgoing {
                cancel_catcher: false
            },
            "dropped (DragDropped) or homed first"
        );
        assert_eq!(
            finished_step(false, true, Some(Riding::Incoming(7)), 7),
            FinishedStep::Incoming
        );
        assert_eq!(finished_step(false, false, None, 7), FinishedStep::Neither);
    }

    #[test]
    fn finishing_and_refusing_follow_what_the_link_knows() {
        assert_eq!(finish_step(Known::Yes), FinishStep::OnLink);
        assert_eq!(finish_step(Known::NoLink), FinishStep::Await);
        assert_eq!(finish_step(Known::No), FinishStep::CloseHere);
        assert_eq!(refuse_step(Known::Yes), RefuseStep::OnLink);
        assert_eq!(refuse_step(Known::No), RefuseStep::ByControl);
        assert_eq!(refuse_step(Known::NoLink), RefuseStep::ByControl);
    }
}
