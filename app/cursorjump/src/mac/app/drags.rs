//! File drag-and-drop in the Mac app's loop: the session's `DndLink` and
//! pacer, the catcher's offers, the drags from the PC with their `Fetch`
//! replies and pills, and the sweep of the drags' temporary folders.
//!
//! - A drag from the PC is known from its `DragOffer` until it finishes.
//!   cj-inject arms it at the crossing (`dnd::source`), whose prefetch
//!   asks the loop for every item at once (R47): each `Fetch` goes to the
//!   link, and its reply goes back through a relay task that also notices
//!   when the source stops waiting (the drag went home or was cancelled),
//!   which stops that fetch. The pill starts at the user's release (R49,
//!   `Released`), with the progress so far. Only the latest crossing's
//!   end counts: undropped, it finishes `NotDropped` at once; dropped, it
//!   sends `DragDropped`, then finishes once every item is in (often
//!   already, R47) and in place (`Ready`), or one failed. A cancel from
//!   either side before a drop finishes it at once. Each way it can end
//!   finishes its pill, and every `Fetch` reply is answered, or dropped
//!   with the drag.
//! - A drag from the Mac is the catcher's latest `Offer` until a `Leave`
//!   carries it (`leave_needs_offer`): it is offered to the PC first, so
//!   the PC has its items before the crossing. It then lives on the link
//!   until the PC's `DragFinished` or a cancel.
//! - `DndLink`'s sending calls (`offer`, `fetch`, `finish`, `cancel`) are
//!   not cancel-safe: they are awaited only in the loop's handlers, after
//!   its `select!` has picked a branch.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use cj_core::dnd::{DragId, DragItem, DragOutcome, FailReason};
use cj_core::pacer::Pacer;
use cj_core::proto::Control;
use cj_net::files::FilesIn;
use cj_net::session::Session;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use super::{App, Ev};
use crate::common::dnd_link::{
    DndEv, DndLink, Source, incoming_outcome, sweep_temp, sweep_temp_except,
};
use crate::common::transfer::{Dest, OsFreeSpace, SharedPacer};
use crate::mac::control::{Crossing, InjectMsg};
use crate::mac::dnd::MacDndEv;
use crate::mac::dnd::catcher::outgoing_dir;
use crate::mac::dnd::source::incoming_dir;
use crate::mac::pill::PillCmd;

/// Copies from the Mac, read only while their drag goes on.
const OUTGOING_MAX_AGE: Duration = Duration::from_secs(60 * 60);
/// Received items: targets (WhatsApp, Gmail) may read them well after the
/// drop (R37).
const INCOMING_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The temporary folders are swept at startup and then this often (R37a).
pub(super) const SWEEP_EVERY: Duration = Duration::from_secs(60 * 60);

/// The drag source's wait for one item's bytes.
type Reply = oneshot::Sender<Result<PathBuf, FailReason>>;
/// The pills' host (`pill::pill_host`).
pub(super) type Pills = Arc<dyn Fn(PillCmd) + Send + Sync>;

/// The catcher's latest offer.
struct Offer {
    drag: DragId,
    items: Vec<DragItem>,
    sources: Vec<Source>,
}

/// Drag-and-drop's part of the app.
pub(super) struct Drags {
    pills: Pills,
    inject: crossbeam_channel::Sender<InjectMsg>,
    /// The live session's drags; None without one.
    link: Option<DndLink>,
    /// The live session's pacer, fed from every pong.
    pacer: Option<SharedPacer>,
    /// Where each session's link sends its events.
    events: UnboundedSender<DndEv>,
    offer: Option<Offer>,
    /// Drags from the Mac offered to the PC and not over yet.
    outgoing: BTreeSet<DragId>,
    /// Drags from the PC, from their `DragOffer` until they finish.
    incoming: BTreeMap<DragId, InDrag>,
}

impl Drags {
    /// The drags, and where every session's link sends its events.
    pub fn new(
        pills: Pills,
        inject: crossbeam_channel::Sender<InjectMsg>,
    ) -> (Drags, UnboundedReceiver<DndEv>) {
        let (events, rx) = unbounded_channel();
        let drags = Drags {
            pills,
            inject,
            link: None,
            pacer: None,
            events,
            offer: None,
            outgoing: BTreeSet::new(),
            incoming: BTreeMap::new(),
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

    /// The session is gone, before the injection thread hears it: every
    /// drag fails, and each drag from the PC ends here and now, its pill
    /// failing (`Disconnected`) and its source's waits failing too. The
    /// riding one is cancelled by the machine's `Disconnected`. Again
    /// without a session, this changes nothing.
    pub fn disconnected(&mut self) {
        if let Some(mut link) = self.link.take() {
            link.disconnected();
        }
        self.pacer = None;
        self.offer = None;
        self.outgoing.clear();
        let lost = DragOutcome::Failed(FailReason::Disconnected);
        for (drag, inc) in std::mem::take(&mut self.incoming) {
            self.close_incoming(drag, inc, &lost);
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

    fn inject(&self, msg: InjectMsg) {
        if self.inject.send(msg).is_err() {
            warn!("dnd: the injection thread is gone");
        }
    }

    /// A drag from the PC is over: any wait left fails and its pill ends.
    fn close_incoming(&self, drag: DragId, mut inc: InDrag, outcome: &DragOutcome) {
        let result = inc.pill_result(outcome);
        inc.fail_replies(result.as_ref().err().unwrap_or(&FailReason::Cancelled));
        if inc.pill {
            (self.pills)(PillCmd::Finish { drag, result });
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

/// Whether a `Leave` carrying `drag` must first offer it to the PC: it is
/// the catcher's offer (`local_offer`), not the PC's own drag going home
/// (`incoming`), which the PC already has.
fn leave_needs_offer(
    drag: Option<DragId>,
    local_offer: Option<DragId>,
    incoming: Option<DragId>,
) -> bool {
    drag.is_some() && drag == local_offer && drag != incoming
}

/// What a push-out's `Leave` needs.
#[derive(Debug, PartialEq)]
enum LeaveStep {
    /// Offer the catcher's drag, then the `Leave` with it if that worked
    /// (`leave_after_offer`).
    OfferThenLeave(DragId),
    /// The `Leave`, carrying this: the PC's drag going home, or nothing.
    Leave(Option<DragId>),
}

fn leave_step(
    drag: Option<DragId>,
    local_offer: Option<DragId>,
    incoming: Option<DragId>,
) -> LeaveStep {
    match drag {
        Some(d) if leave_needs_offer(drag, local_offer, incoming) => LeaveStep::OfferThenLeave(d),
        _ => LeaveStep::Leave(drag),
    }
}

/// The drag the `Leave` carries once its offer was `sent` or not: a drag
/// the PC wasn't offered must not ride along.
fn leave_after_offer(drag: DragId, sent: bool) -> Option<DragId> {
    sent.then_some(drag)
}

/// How to end a drag from the PC once its outcome is known.
#[derive(Debug, PartialEq)]
enum FinishStep {
    /// `finish` on the link; its `Finished` closes it here.
    OnLink,
    /// The link no longer has it (gone, or a new session): close it here.
    CloseHere,
}

fn finish_step(link_knows: bool) -> FinishStep {
    if link_knows {
        FinishStep::OnLink
    } else {
        FinishStep::CloseHere
    }
}

/// A call on the link, for a drag.
#[derive(Debug, Clone, PartialEq)]
enum LinkCall {
    Cancel(FailReason),
    Finish(DragOutcome),
}

/// What a pill's ✕ does on the link. Before the drop, cj-inject's cancel
/// ends the session undropped and that end finishes it; after the drop
/// the session is over, so the copy is cancelled on the link.
fn pill_cancel_calls(ended: Option<bool>) -> Vec<LinkCall> {
    match ended {
        Some(true) => vec![LinkCall::Cancel(FailReason::Cancelled)],
        _ => Vec::new(),
    }
}

/// What going to sleep does to one drag.
#[derive(Debug, PartialEq)]
enum SleepStep {
    /// A drag from the PC: cj-inject cancels it, its pill fails with
    /// `Sleep`, and the link cancels it (telling the PC why), then
    /// finishes it.
    EndIncoming(DragId),
    /// A drag from the Mac: the link cancels it, telling the PC why.
    CancelOutgoing(DragId),
}

impl SleepStep {
    fn link_calls(&self) -> Vec<LinkCall> {
        let slept = FailReason::Sleep;
        match self {
            SleepStep::EndIncoming(_) => vec![
                LinkCall::Cancel(slept.clone()),
                LinkCall::Finish(DragOutcome::Failed(slept)),
            ],
            SleepStep::CancelOutgoing(_) => vec![LinkCall::Cancel(slept)],
        }
    }
}

/// Every drag going to sleep ends, but one from the PC already finishing:
/// its link's `Finished` closes it.
fn sleep_steps(incoming: &BTreeMap<DragId, InDrag>, outgoing: &BTreeSet<DragId>) -> Vec<SleepStep> {
    let ends = incoming
        .iter()
        .filter(|(_, inc)| !inc.finishing)
        .map(|(&drag, _)| SleepStep::EndIncoming(drag));
    let cancels = outgoing.iter().map(|&drag| SleepStep::CancelOutgoing(drag));
    ends.chain(cancels).collect()
}

/// The pill's result for a drag from the PC that ended with `outcome`;
/// `why` says why one that wasn't dropped ended (an item failed, or a
/// cancel), if known.
fn pill_result(outcome: &DragOutcome, why: Option<&FailReason>) -> Result<(), FailReason> {
    match outcome {
        DragOutcome::Done => Ok(()),
        DragOutcome::NotDropped => Err(why.cloned().unwrap_or(FailReason::Cancelled)),
        DragOutcome::Failed(why) => Err(why.clone()),
    }
}

/// A drag from the PC, as the app follows it.
#[derive(Debug)]
struct InDrag {
    items: Vec<DragItem>,
    /// The latest crossing heard of; news of an earlier one is ignored.
    crossing: Crossing,
    /// That crossing's end: whether it was dropped.
    ended: Option<bool>,
    /// Every item of that crossing is in place (`Ready`).
    ready: bool,
    /// Released: its pill started.
    pill: bool,
    /// The latest progress, for a pill that starts later.
    progress: Option<(u64, Option<u64>)>,
    /// Each item asked for, and its result once in.
    fetched: BTreeMap<u32, Option<Result<(), FailReason>>>,
    /// The source's waits not answered yet.
    replies: BTreeMap<u32, Reply>,
    /// The first item that failed here, and why.
    failed: Option<FailReason>,
    /// Cancelled from either side.
    cancelled: Option<FailReason>,
    /// `finish` was called.
    finishing: bool,
}

impl InDrag {
    fn new(items: Vec<DragItem>) -> InDrag {
        InDrag {
            items,
            crossing: 0,
            ended: None,
            ready: false,
            pill: false,
            progress: None,
            fetched: BTreeMap::new(),
            replies: BTreeMap::new(),
            failed: None,
            cancelled: None,
            finishing: false,
        }
    }

    /// News of `crossing`: true if it is the latest (a later one starts
    /// afresh), false for an earlier crossing's, which changes nothing.
    fn crossing(&mut self, crossing: Crossing) -> bool {
        if crossing < self.crossing {
            return false;
        }
        if crossing > self.crossing {
            self.crossing = crossing;
            self.ended = None;
            self.ready = false;
        }
        true
    }

    /// The source asks for `item`.
    fn fetch(&mut self, item: u32, reply: Reply) {
        if let Some(old) = self.replies.insert(item, reply) {
            let _ = old.send(Err(FailReason::Cancelled));
        }
        self.fetched.insert(item, None);
    }

    /// `item` is in, or failed: its wait is answered.
    fn received(&mut self, item: u32, result: Result<PathBuf, FailReason>) {
        if let Some(reply) = self.replies.remove(&item) {
            // The source may have stopped waiting.
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

    /// `crossing`'s session ended; false if that is no news for the drag
    /// (an earlier crossing's, a second end, or the drag is finishing).
    fn ended(&mut self, crossing: Crossing, dropped: bool) -> bool {
        if self.finishing || !self.crossing(crossing) || self.ended.is_some() {
            return false;
        }
        self.ended = Some(dropped);
        true
    }

    /// Every item of `crossing` is in place.
    fn ready(&mut self, crossing: Crossing) {
        if self.crossing(crossing) {
            self.ready = true;
        }
    }

    /// Item `item` of `crossing` failed here (fetched, or put in place).
    fn item_failed(&mut self, crossing: Crossing, item: u32, why: FailReason) {
        if !self.crossing(crossing) {
            return;
        }
        self.failed.get_or_insert(why.clone());
        self.fetched.insert(item, Some(Err(why)));
    }

    fn cancel(&mut self, why: FailReason) {
        self.cancelled.get_or_insert(why);
    }

    /// What to `finish` with, once known and not yet sent:
    /// - `NotDropped` as soon as the latest crossing ends without a drop,
    ///   or a cancel comes before any end;
    /// - after a drop, once every item is accounted for: the first failure
    ///   in item order, else `Done` once the items are in place (`Ready`).
    ///   A cancel stops the wait for items never asked for; one that comes
    ///   once every item is in can't undo the drop.
    fn outcome(&self) -> Option<DragOutcome> {
        if self.finishing {
            return None;
        }
        let dropped = match self.ended {
            Some(dropped) => dropped,
            None if self.cancelled.is_some() => false,
            None => return None,
        };
        if !dropped {
            return Some(DragOutcome::NotDropped);
        }
        // Every item is fetched at the crossing (R47); items handed over
        // from an earlier crossing of the same id were fetched then, into
        // this same `InDrag`, and are never fetched again.
        let short = self.fetched.len() < self.items.len();
        if short && self.cancelled.is_none() {
            return None;
        }
        let results: Vec<_> = self.fetched.values().cloned().collect();
        match (incoming_outcome(true, &results)?, &self.cancelled) {
            (DragOutcome::Done, Some(why)) if short => Some(DragOutcome::Failed(why.clone())),
            (DragOutcome::Done, _) if !self.ready => None,
            (outcome, _) => Some(outcome),
        }
    }

    fn pill_result(&self, outcome: &DragOutcome) -> Result<(), FailReason> {
        pill_result(outcome, self.failed.as_ref().or(self.cancelled.as_ref()))
    }
}

/// Answers the source's `reply` with what the loop sends on the returned
/// sender; if the source stops waiting first (its receiver dropped: the
/// drag went home or was cancelled), tells the loop (`FetchUnwanted`).
fn relay(mut reply: Reply, drag: DragId, item: u32, events: UnboundedSender<Ev>) -> Reply {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::select! {
            result = rx => {
                if let Ok(result) = result {
                    let _ = reply.send(result);
                }
            }
            () = reply.closed() => {
                let _ = events.send(Ev::FetchUnwanted { drag, item });
            }
        }
    });
    tx
}

impl App {
    // ------------------------------------------- from the source and catcher

    pub(super) async fn on_mac_dnd(&mut self, ev: MacDndEv) {
        match ev {
            MacDndEv::Offer {
                drag,
                items,
                sources,
            } => {
                debug!(drag, items = items.len(), "dnd: on the strip");
                self.drags.offer = Some(Offer {
                    drag,
                    items,
                    sources,
                });
            }
            MacDndEv::OfferGone { drag } => {
                if self.drags.offer.as_ref().is_some_and(|o| o.drag == drag) {
                    self.drags.offer = None;
                }
            }
            MacDndEv::Fetch {
                drag,
                item,
                dest,
                reply,
            } => self.on_source_fetch(drag, item, dest, reply).await,
            MacDndEv::Released { drag, at } => {
                debug!(drag, ?at, "dnd: let go of over the Mac");
                self.on_released(drag);
            }
            MacDndEv::Ended {
                drag,
                crossing,
                dropped,
            } => {
                let Some(inc) = self.drags.incoming.get_mut(&drag) else {
                    return;
                };
                if !inc.ended(crossing, dropped) {
                    debug!(drag, crossing, "dnd: an earlier crossing's end; ignored");
                    return;
                }
                info!(
                    drag,
                    crossing, dropped, "dnd: the drag from the PC ended here"
                );
                if dropped {
                    self.control(Control::DragDropped { drag }).await;
                }
                self.try_finish(drag).await;
            }
            MacDndEv::Ready { drag, crossing } => {
                if let Some(inc) = self.drags.incoming.get_mut(&drag) {
                    inc.ready(crossing);
                }
                self.try_finish(drag).await;
            }
            MacDndEv::Failed {
                drag,
                crossing,
                item,
                reason,
            } => {
                if let Some(inc) = self.drags.incoming.get_mut(&drag) {
                    inc.item_failed(crossing, item, reason);
                }
                self.try_finish(drag).await;
            }
            // cj-inject's alone.
            MacDndEv::NeedsEscape { .. } | MacDndEv::LateDrop { .. } => {}
        }
    }

    /// The source asks for an item's bytes, at the crossing (R47).
    async fn on_source_fetch(&mut self, drag: DragId, item: u32, dest: Dest, reply: Reply) {
        let up = self.drags.link.is_some();
        let Some(inc) = self.drags.incoming.get_mut(&drag).filter(|_| up) else {
            let why = if up {
                FailReason::Cancelled
            } else {
                FailReason::Disconnected
            };
            debug!(
                drag,
                item,
                ?why,
                "dnd: fetch for a drag that is over; refused"
            );
            let _ = reply.send(Err(why));
            return;
        };
        inc.fetch(item, relay(reply, drag, item, self.ch.events.clone()));
        let Some(link) = self.drags.link.as_mut() else {
            return;
        };
        if let Err(e) = link.fetch(drag, item, dest).await {
            warn!(drag, item, "dnd: fetch not started: {e:#}");
            // Its `Received`, if one comes, finds the reply answered.
            let why = if self.live().is_some() {
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

    /// The source stopped waiting for an item: its fetch stops, quietly
    /// (the drag's end tells the PC).
    pub(super) fn on_fetch_unwanted(&mut self, drag: DragId, item: u32) {
        debug!(drag, item, "dnd: the source no longer wants the item");
        if let Some(link) = self.drags.link.as_mut() {
            link.stop_fetch(drag, item);
        }
    }

    /// The user let go of a drag riding here: its pill shows (R49), with
    /// the progress so far.
    fn on_released(&mut self, drag: DragId) {
        let Some(inc) = self.drags.incoming.get_mut(&drag) else {
            return;
        };
        if inc.pill || inc.finishing {
            return;
        }
        inc.pill = true;
        let (items, progress) = (inc.items.clone(), inc.progress);
        (self.drags.pills)(PillCmd::Start { drag, items });
        if let Some((done, total)) = progress {
            (self.drags.pills)(PillCmd::Progress { drag, done, total });
        }
    }

    // ------------------------------------------------------ from the link

    pub(super) async fn on_link_dnd(&mut self, ev: DndEv) {
        match ev {
            DndEv::Progress { drag, done, total } => {
                if let Some(inc) = self.drags.incoming.get_mut(&drag) {
                    inc.progress = Some((done, total));
                    if inc.pill {
                        (self.drags.pills)(PillCmd::Progress { drag, done, total });
                    }
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
                self.drags.outgoing.remove(&drag);
                if let Some(inc) = self.drags.incoming.remove(&drag) {
                    self.drags.close_incoming(drag, inc, &outcome);
                }
            }
        }
    }

    /// Finishes a drag from the PC once its outcome is known.
    async fn try_finish(&mut self, drag: DragId) {
        let Some(inc) = self.drags.incoming.get_mut(&drag) else {
            return;
        };
        let Some(outcome) = inc.outcome() else {
            return;
        };
        inc.finishing = true;
        info!(drag, ?outcome, "dnd: the drag from the PC is over");
        let knows = self
            .drags
            .link
            .as_ref()
            .is_some_and(|l| l.items(drag).is_some());
        match finish_step(knows) {
            FinishStep::OnLink => self.on_link(drag, vec![LinkCall::Finish(outcome)]).await,
            FinishStep::CloseHere => {
                if let Some(inc) = self.drags.incoming.remove(&drag) {
                    self.drags.close_incoming(drag, inc, &outcome);
                }
            }
        }
    }

    /// Makes `calls` for `drag` on the link, in order, until one fails.
    async fn on_link(&mut self, drag: DragId, calls: Vec<LinkCall>) {
        let Some(link) = self.drags.link.as_mut() else {
            return;
        };
        for call in calls {
            let sent = match call.clone() {
                LinkCall::Cancel(why) => link.cancel(drag, why).await,
                LinkCall::Finish(outcome) => link.finish(drag, outcome).await,
            };
            if let Err(e) = sent {
                debug!(drag, ?call, "dnd: not sent: {e:#}");
                return;
            }
        }
    }

    /// Sends a control message on the live session, if any.
    async fn control(&mut self, c: Control) {
        let Some(session) = self.live().map(|l| l.session.clone()) else {
            return;
        };
        // Awaited here, outside any select!: send_control is not cancel-safe.
        if let Err(e) = session.send_control(&c).await {
            debug!(?c, "dnd: not sent: {e:#}");
        }
    }

    // --------------------------------------------------- from the session

    /// A drag message from the PC.
    pub(super) async fn on_drag_control(&mut self, c: Control) {
        let Some(link) = self.drags.link.as_mut() else {
            debug!(?c, "dnd: no session; ignored");
            return;
        };
        match c {
            Control::DragOffer { drag, items } => {
                // cj-inject has its own copy (link.rs), for the crossing.
                link.on_offer(drag, items.clone());
                self.drags
                    .incoming
                    .entry(drag)
                    .or_insert_with(|| InDrag::new(items));
            }
            Control::FileFetch { drag, item } => link.on_fetch(drag, item),
            Control::DragFinished { drag, outcome } => link.on_finished(drag, outcome),
            Control::DragCancel { drag, reason } => {
                link.on_cancel(drag, reason.clone());
                if let Some(inc) = self.drags.incoming.get_mut(&drag) {
                    info!(drag, ?reason, "dnd: the PC cancelled its drag");
                    inc.cancel(reason);
                    self.drags.inject(InjectMsg::CancelDrag { drag });
                    self.try_finish(drag).await;
                }
            }
            other => debug!(?other, "dnd: not a drag message"),
        }
    }

    /// A files stream from the PC; dropped (refused) without a session.
    pub(super) fn on_files(&mut self, files: FilesIn) {
        match self.drags.link.as_mut() {
            Some(link) => link.on_files(files),
            None => debug!("dnd: files stream without a session; refused"),
        }
    }

    // --------------------------------------------------- from cj-inject

    /// The `Leave` for a push-out carrying `drag`: the catcher's drag is
    /// offered to the PC first (so it has the items before the crossing);
    /// the PC's own drag going home just rides along. None if the offer
    /// couldn't be made.
    pub(super) async fn leave_drag(&mut self, drag: Option<DragId>) -> Option<DragId> {
        let local = self.drags.offer.as_ref().map(|o| o.drag);
        let incoming = drag.filter(|d| self.drags.incoming.contains_key(d));
        let d = match leave_step(drag, local, incoming) {
            LeaveStep::Leave(carried) => return carried,
            LeaveStep::OfferThenLeave(d) => d,
        };
        let o = self.drags.offer.take()?;
        let link = self.drags.link.as_mut()?;
        let sent = match link.offer(o.drag, o.items, o.sources).await {
            Ok(()) => {
                info!(drag = d, "dnd: the drag from the Mac goes to the PC");
                self.drags.outgoing.insert(d);
                true
            }
            Err(e) => {
                warn!(drag = d, "dnd: offer not sent; leaving without it: {e:#}");
                false
            }
        };
        leave_after_offer(d, sent)
    }

    // ------------------------------------------------------------- pills

    /// A running pill's ✕: the drag stops here, and its copy too once it
    /// was dropped.
    pub(super) async fn on_pill_cancel(&mut self, drag: DragId) {
        let Some(inc) = self.drags.incoming.get_mut(&drag) else {
            return;
        };
        info!(drag, "dnd: cancelled from its pill");
        inc.cancel(FailReason::Cancelled);
        let calls = pill_cancel_calls(inc.ended);
        self.drags.inject(InjectMsg::CancelDrag { drag });
        self.on_link(drag, calls).await;
        self.try_finish(drag).await;
    }

    // ------------------------------------------------------------- sleep

    /// The Mac is about to sleep: every drag ends now, telling the PC why,
    /// while the session is still up; each drag from the PC fails its pill
    /// with `Sleep`.
    pub(super) async fn on_will_sleep(&mut self) {
        // The catcher's drag can't be carried any more.
        self.drags.offer = None;
        let steps = sleep_steps(&self.drags.incoming, &self.drags.outgoing);
        if steps.is_empty() {
            return;
        }
        info!(
            drags = steps.len(),
            "dnd: the Mac is going to sleep; its drags end"
        );
        let slept = DragOutcome::Failed(FailReason::Sleep);
        for step in steps {
            let calls = step.link_calls();
            let drag = match step {
                SleepStep::EndIncoming(drag) => {
                    self.drags.inject(InjectMsg::CancelDrag { drag });
                    if let Some(mut inc) = self.drags.incoming.remove(&drag) {
                        inc.cancel(FailReason::Sleep);
                        self.drags.close_incoming(drag, inc, &slept);
                    }
                    drag
                }
                SleepStep::CancelOutgoing(drag) => drag,
            };
            self.on_link(drag, calls).await;
        }
    }

    // ------------------------------------------------------------- sweep

    /// Removes old copies and received items, off the loop (R37a). At
    /// startup no drag is live yet.
    pub(super) fn sweep_temp_dirs(&self, startup: bool) {
        let live = self.drags.live();
        tokio::task::spawn_blocking(move || {
            let now = SystemTime::now();
            for (root, max_age) in [
                (outgoing_dir(), OUTGOING_MAX_AGE),
                (incoming_dir(), INCOMING_MAX_AGE),
            ] {
                let removed = if startup {
                    sweep_temp(&root, max_age, now)
                } else {
                    sweep_temp_except(&root, max_age, now, &live)
                };
                if removed > 0 {
                    info!(removed, root = %root.display(), "swept old drag folders");
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

    /// Both items asked for at the crossing, as the prefetch does.
    fn fetching(
        crossing: Crossing,
    ) -> (InDrag, [oneshot::Receiver<Result<PathBuf, FailReason>>; 2]) {
        let mut inc = two();
        let (a, ra) = oneshot::channel();
        let (b, rb) = oneshot::channel();
        inc.fetch(0, a);
        inc.fetch(1, b);
        assert!(inc.crossing(crossing));
        (inc, [ra, rb])
    }

    #[test]
    fn going_home_leave_sends_no_offer() {
        // The PC's drag 7 goes home with the cursor: the PC has it.
        assert!(!leave_needs_offer(Some(7), None, Some(7)));
        // Even with a drag of the Mac's on the strip meanwhile.
        assert!(!leave_needs_offer(Some(7), Some(9), Some(7)));
        // The PC's own drag is never offered back to it.
        assert!(!leave_needs_offer(Some(7), Some(7), Some(7)));
    }

    #[test]
    fn a_leave_offers_the_catchers_drag_first_or_carries_it_not() {
        assert_eq!(
            leave_step(Some(9), Some(9), None),
            LeaveStep::OfferThenLeave(9)
        );
        assert_eq!(
            leave_step(Some(7), Some(9), Some(7)),
            LeaveStep::Leave(Some(7)),
            "the PC's drag goes home"
        );
        assert_eq!(leave_step(None, Some(9), None), LeaveStep::Leave(None));
        assert_eq!(leave_after_offer(9, true), Some(9));
        assert_eq!(
            leave_after_offer(9, false),
            None,
            "not offered: the PC would refuse it"
        );
    }

    #[test]
    fn a_drag_finishes_on_the_link_that_has_it_or_here() {
        assert_eq!(finish_step(true), FinishStep::OnLink);
        assert_eq!(finish_step(false), FinishStep::CloseHere);
    }

    #[test]
    fn a_pills_cancel_stops_the_copy_on_the_link_only_after_the_drop() {
        assert_eq!(
            pill_cancel_calls(Some(true)),
            [LinkCall::Cancel(FailReason::Cancelled)]
        );
        assert_eq!(pill_cancel_calls(Some(false)), [], "ended already");
        assert_eq!(pill_cancel_calls(None), [], "its session's end finishes it");
    }

    #[test]
    fn sleep_cancels_then_finishes_every_drag_but_one_finishing() {
        let mut incoming = BTreeMap::new();
        incoming.insert(1, two());
        let mut finishing = two();
        finishing.finishing = true;
        incoming.insert(2, finishing);
        let outgoing = BTreeSet::from([3]);
        let steps = sleep_steps(&incoming, &outgoing);
        assert_eq!(
            steps,
            [SleepStep::EndIncoming(1), SleepStep::CancelOutgoing(3)]
        );
        let slept = FailReason::Sleep;
        assert_eq!(
            steps[0].link_calls(),
            [
                LinkCall::Cancel(slept.clone()),
                LinkCall::Finish(DragOutcome::Failed(slept.clone()))
            ]
        );
        assert_eq!(steps[1].link_calls(), [LinkCall::Cancel(slept)]);
        assert!(sleep_steps(&BTreeMap::new(), &BTreeSet::new()).is_empty());
    }

    #[test]
    fn only_the_catchers_drag_is_offered_before_its_leave() {
        assert!(leave_needs_offer(Some(9), Some(9), None));
        assert!(!leave_needs_offer(None, Some(9), None), "a plain leave");
        assert!(!leave_needs_offer(None, None, None));
        assert!(
            !leave_needs_offer(Some(9), Some(8), None),
            "another drag's offer"
        );
        assert!(!leave_needs_offer(Some(9), None, None), "no offer for it");
    }

    #[test]
    fn a_drag_rides_until_its_latest_crossing_ends() {
        let (mut inc, _r) = fetching(3);
        assert_eq!(inc.outcome(), None, "still riding");
        // An earlier crossing's late news changes nothing.
        assert!(!inc.ended(2, false));
        inc.ready(2);
        assert!(!inc.ready);
        inc.item_failed(2, 0, FailReason::Unwritable);
        assert_eq!(inc.failed, None);
        assert_eq!(inc.outcome(), None);
        // The latest crossing's own end counts, once.
        assert!(inc.ended(3, false));
        assert!(!inc.ended(3, true), "once per crossing");
        assert_eq!(inc.outcome(), Some(DragOutcome::NotDropped));
    }

    #[test]
    fn a_later_crossing_starts_afresh() {
        let (mut inc, _r) = fetching(3);
        inc.ready(3);
        assert!(inc.crossing(5));
        assert!(!inc.ready, "this crossing's items aren't placed yet");
        assert!(inc.ended(5, true));
        assert!(!inc.ended(3, false), "the earlier end came late");
        assert_eq!(inc.ended, Some(true));
        // A failed prepare (crossing 0) is no news either, now.
        assert!(!inc.ended(0, false));
    }

    #[test]
    fn an_end_after_the_finish_is_no_news() {
        // Cancelled by the PC, and finished, before its session ended.
        let (mut inc, _r) = fetching(1);
        inc.cancel(FailReason::Cancelled);
        assert_eq!(inc.outcome(), Some(DragOutcome::NotDropped));
        inc.finishing = true;
        assert!(!inc.ended(1, true), "no DragDropped after the finish");
    }

    #[test]
    fn a_failed_prepare_ends_a_drag_never_heard_of() {
        let mut inc = two();
        assert!(inc.ended(0, false));
        assert_eq!(inc.outcome(), Some(DragOutcome::NotDropped));
    }

    #[test]
    fn a_drop_whose_items_are_already_here_finishes_at_once() {
        // R47: prefetched before the drop.
        let (mut inc, [mut ra, mut rb]) = fetching(1);
        inc.received(0, Ok("/in/a.txt".into()));
        inc.received(1, Ok("/in/b.txt".into()));
        assert_eq!(ra.try_recv().unwrap(), Ok("/in/a.txt".into()));
        assert_eq!(rb.try_recv().unwrap(), Ok("/in/b.txt".into()));
        inc.ready(1);
        assert!(inc.ended(1, true));
        assert_eq!(inc.outcome(), Some(DragOutcome::Done));
    }

    #[test]
    fn a_drop_waits_for_every_item_in_place() {
        let (mut inc, _r) = fetching(1);
        assert!(inc.ended(1, true));
        assert_eq!(inc.outcome(), None, "nothing is in yet");
        inc.received(0, Ok("a".into()));
        inc.received(1, Ok("b".into()));
        assert_eq!(inc.outcome(), None, "in, but not over the placeholders yet");
        inc.ready(1);
        assert_eq!(inc.outcome(), Some(DragOutcome::Done));
        inc.finishing = true;
        assert_eq!(inc.outcome(), None, "finished once");
    }

    #[test]
    fn a_dropped_item_that_fails_fails_the_drag() {
        let (mut inc, _r) = fetching(1);
        assert!(inc.ended(1, true));
        inc.received(1, Err(FailReason::NoSpace));
        assert_eq!(inc.outcome(), None, "item 0 is still coming");
        inc.received(0, Ok("a".into()));
        assert_eq!(
            inc.outcome(),
            Some(DragOutcome::Failed(FailReason::NoSpace))
        );

        // In, but it couldn't be put in place.
        let (mut inc, _r) = fetching(1);
        inc.received(0, Ok("a".into()));
        inc.received(1, Ok("b".into()));
        inc.item_failed(1, 1, FailReason::Unwritable);
        assert!(inc.ended(1, true));
        assert_eq!(
            inc.outcome(),
            Some(DragOutcome::Failed(FailReason::Unwritable))
        );
    }

    #[test]
    fn a_drop_never_finishes_before_every_item_was_asked_for() {
        let mut inc = two();
        assert!(inc.ended(1, true));
        assert_eq!(inc.outcome(), None, "the prefetch's asks are on their way");
    }

    #[test]
    fn a_cancel_before_any_end_finishes_at_once() {
        // The pill's ✕ during a held drop, or the PC's DragCancel.
        let (mut inc, _r) = fetching(1);
        inc.cancel(FailReason::Unreadable {
            name: "a.txt".into(),
        });
        assert_eq!(inc.outcome(), Some(DragOutcome::NotDropped));
        assert_eq!(
            inc.pill_result(&DragOutcome::NotDropped),
            Err(FailReason::Unreadable {
                name: "a.txt".into()
            }),
            "the pill says why"
        );
        // And a drag that never crossed: nothing will end it otherwise.
        let mut inc = two();
        inc.cancel(FailReason::Cancelled);
        assert_eq!(inc.outcome(), Some(DragOutcome::NotDropped));
    }

    #[test]
    fn a_cancel_after_the_drop_waits_for_the_fetches_in_flight() {
        let (mut inc, _r) = fetching(1);
        inc.received(0, Ok("a".into()));
        assert!(inc.ended(1, true));
        inc.cancel(FailReason::Cancelled);
        assert_eq!(inc.outcome(), None, "item 1 is still in flight");
        inc.received(1, Err(FailReason::Cancelled));
        assert_eq!(
            inc.outcome(),
            Some(DragOutcome::Failed(FailReason::Cancelled))
        );

        // Once every item is in and placed, a late cancel can't undo it.
        let (mut inc, _r) = fetching(1);
        inc.received(0, Ok("a".into()));
        inc.received(1, Ok("b".into()));
        inc.ready(1);
        assert!(inc.ended(1, true));
        inc.cancel(FailReason::Cancelled);
        assert_eq!(inc.outcome(), Some(DragOutcome::Done));

        // A cancel stops the wait for items never asked for.
        let mut inc = two();
        let (a, _ra) = oneshot::channel();
        inc.fetch(0, a);
        inc.received(0, Ok("a".into()));
        inc.ready(1);
        assert!(inc.ended(1, true));
        assert_eq!(inc.outcome(), None);
        inc.cancel(FailReason::Sleep);
        assert_eq!(inc.outcome(), Some(DragOutcome::Failed(FailReason::Sleep)));
    }

    #[test]
    fn failing_the_waits_answers_each_once() {
        let (mut inc, [mut ra, mut rb]) = fetching(1);
        inc.received(0, Ok("a.txt".into()));
        inc.fail_replies(&FailReason::Disconnected);
        assert_eq!(ra.try_recv().unwrap(), Ok("a.txt".into()));
        assert_eq!(rb.try_recv().unwrap(), Err(FailReason::Disconnected));
        assert!(inc.replies.is_empty());
        // A second ask for an item fails the first wait.
        let (mut inc, [mut ra, _rb]) = fetching(1);
        let (again, _) = oneshot::channel();
        inc.fetch(0, again);
        assert_eq!(ra.try_recv().unwrap(), Err(FailReason::Cancelled));
    }

    #[test]
    fn pill_results() {
        assert_eq!(pill_result(&DragOutcome::Done, None), Ok(()));
        assert_eq!(
            pill_result(&DragOutcome::NotDropped, None),
            Err(FailReason::Cancelled)
        );
        assert_eq!(
            pill_result(&DragOutcome::NotDropped, Some(&FailReason::NoSpace)),
            Err(FailReason::NoSpace),
            "an item failed during a held drop"
        );
        assert_eq!(
            pill_result(&DragOutcome::Failed(FailReason::Disconnected), None),
            Err(FailReason::Disconnected)
        );
        // An item's failure comes before a cancel's reason.
        let (mut inc, _r) = fetching(1);
        inc.item_failed(1, 0, FailReason::NoSpace);
        inc.cancel(FailReason::Cancelled);
        assert_eq!(
            inc.pill_result(&DragOutcome::NotDropped),
            Err(FailReason::NoSpace)
        );
    }

    fn drags() -> (
        Drags,
        Arc<Mutex<Vec<String>>>,
        crossbeam_channel::Receiver<InjectMsg>,
    ) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let pills: Pills = Arc::new(move |cmd| l.lock().unwrap().push(format!("{cmd:?}")));
        let (tx, rx) = crossbeam_channel::unbounded();
        let (d, _events) = Drags::new(pills, tx);
        (d, log, rx)
    }

    #[test]
    fn a_lost_session_ends_every_drag_from_the_pc_and_its_pill() {
        let (mut d, log, _rx) = drags();
        let (mut shown, [mut ra, _rb]) = fetching(1);
        shown.pill = true;
        d.incoming.insert(9, shown);
        d.incoming.insert(10, two());
        d.outgoing.insert(11);
        d.disconnected();
        assert!(d.incoming.is_empty() && d.outgoing.is_empty());
        assert_eq!(ra.try_recv().unwrap(), Err(FailReason::Disconnected));
        assert_eq!(
            *log.lock().unwrap(),
            ["Finish { drag: 9, result: Err(Disconnected) }"],
            "only a pill that showed is finished"
        );
        assert!(d.live().is_empty());
    }

    #[test]
    fn live_drags_keep_their_folders() {
        let (mut d, _log, _rx) = drags();
        d.incoming.insert(1, two());
        d.outgoing.insert(2);
        d.offer = Some(Offer {
            drag: 3,
            items: Vec::new(),
            sources: Vec::new(),
        });
        let mut live = d.live();
        live.sort();
        assert_eq!(live, [1, 2, 3]);
    }
}
