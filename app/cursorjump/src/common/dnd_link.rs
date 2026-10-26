//! Drag-and-drop over one session. The origin (where a drag started)
//! offers its items and sends each one the receiver fetches; the receiver
//! (where it is dropped) fetches items into their destinations, adds up
//! their progress for the pill, and tells the origin how the drag ended.
//!
//! An item the origin can't send (unreadable, changed while it was sent,
//! or a pending source that failed) fails the whole drag: the origin ends
//! its side and sends `DragCancel` with the reason, and the receiver
//! reports that reason, not `Cancelled`, for every fetch still in flight.
//! When the receiver stops a stream, it knows why and the origin says
//! nothing. Transfers are only ever stopped through their cancellation
//! tokens, never aborted, so `transfer` always cleans up on its own path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, bail};
use cj_core::dnd::{DragId, DragItem, DragOutcome, FailReason, ItemKind, sanitize_component};
use cj_core::proto::Control;
use cj_net::files::FilesIn;
use cj_net::session::Session;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use unicode_normalization::UnicodeNormalization;

use super::transfer::{Dest, FreeSpace, SharedPacer, receive_item, send_item};

/// Shortest gap between progress events for one drag.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// How long a receiver whose files stream the origin reset waits for the
/// origin's `DragCancel` to say why, before it reports `Cancelled`.
const REASON_WAIT: Duration = Duration::from_secs(2);

/// Where the origin reads a dragged item from.
#[derive(Debug, Clone)]
pub enum Source {
    Ready(PathBuf),
    /// Not on disk yet (a spooled virtual file, a Mac file promise): its
    /// path once it is, or why it never will be. A sender dropped without
    /// a value makes the item unreadable.
    Pending(watch::Receiver<Option<Result<PathBuf, FailReason>>>),
}

/// A new id for a drag that starts on this computer: 64 random bits,
/// never 0. Both sides allocate them this way, so the PC's and the Mac's
/// ids collide only by chance (one in 2^64), and an id never repeats one
/// of an earlier run's leftovers.
pub fn fresh_drag_id() -> DragId {
    loop {
        if let Some(id) = drag_id_from(cj_net::random_nonce()) {
            return id;
        }
    }
}

/// The id made of the first 8 random bytes; None for 0.
fn drag_id_from(random: [u8; 32]) -> Option<DragId> {
    let mut b = [0u8; 8];
    b.copy_from_slice(&random[..8]);
    Some(u64::from_le_bytes(b)).filter(|&id| id != 0)
}

/// What the link tells the app.
#[derive(Debug, Clone, PartialEq)]
pub enum DndEv {
    /// Receiver side, over all the drag's items; at most 10 a second per
    /// drag. A change held back by that limit is sent as soon as it allows,
    /// so the last value always arrives, possibly just after its item's
    /// `Received`.
    Progress {
        drag: DragId,
        done: u64,
        total: Option<u64>,
    },
    /// Receiver side: exactly one per fetch.
    Received {
        drag: DragId,
        item: u32,
        result: Result<PathBuf, FailReason>,
    },
    /// Either side, once per drag.
    Finished { drag: DragId, outcome: DragOutcome },
}

/// One session's drags, both ways.
///
/// On the origin a drag lives from `offer` until the receiver's
/// `DragFinished` or a `DragCancel` from either side (or `disconnected`);
/// its sources are dropped then. On the receiver it lives from `on_offer`
/// until `finish` (or `disconnected`), even once cancelled, so a late fetch
/// still fails with the cancel's reason.
///
/// Two fetches in flight at once never write the same place: one into
/// the same `Exact` path, or into the same folder under the same sanitized
/// name, is refused with `Unwritable` rather than racing the first on its
/// `.cjpart`. Paths are compared folded (NFC, lowercase) on every OS, as
/// NTFS and APFS compare names, so "Foo.txt" and "foo.txt" clash too.
/// Callers normally pick a fresh folder per drag.
pub struct DndLink {
    session: Session,
    pacer: SharedPacer,
    space: Arc<dyn FreeSpace>,
    shared: Arc<Shared>,
    /// Incoming drags' items, as offered.
    offered: BTreeMap<DragId, Vec<DragItem>>,
    /// Parent of every drag's token; cancelled when the link is dropped.
    root: CancellationToken,
}

/// What the link shares with the transfers it starts.
struct Shared {
    events: UnboundedSender<DndEv>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    outgoing: BTreeMap<DragId, Outgoing>,
    incoming: BTreeMap<DragId, Incoming>,
    /// The last fetch's number, so a transfer that ends late can't touch
    /// a later fetch of its item.
    last_fetch: u64,
}

/// A drag that started here.
struct Outgoing {
    items: Vec<DragItem>,
    sources: Vec<Source>,
    cancel: CancellationToken,
}

/// A drag that started on the other computer.
struct Incoming {
    cancel: CancellationToken,
    /// Why it was cancelled, by either side; the first reason wins.
    reason: Option<FailReason>,
    /// Items fetched and not yet received.
    fetches: BTreeMap<u32, Fetch>,
    tally: Tally,
}

struct Fetch {
    id: u64,
    dest: Dest,
    /// Where the item lands, as far as is known yet (`dest_key`).
    key: String,
    /// Its files stream has arrived.
    started: bool,
    cancel: CancellationToken,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, ev: DndEv) {
        // No one listening: the app is shutting down.
        let _ = self.events.send(ev);
    }

    /// Ends a drag that started here because one of its items can't be
    /// sent; true if it was still going, so the receiver must be told.
    fn origin_failed(&self, drag: DragId, reason: &FailReason) -> bool {
        let Some(out) = self.lock().outgoing.remove(&drag) else {
            return false;
        };
        out.cancel.cancel();
        let outcome = DragOutcome::Failed(reason.clone());
        self.emit(DndEv::Finished { drag, outcome });
        true
    }

    /// Sends `drag`'s progress if a report is due, or has it sent once one
    /// is.
    fn report(self: &Arc<Self>, drag: DragId, tally: &mut Tally) {
        match tally.report(Instant::now()) {
            Report::Now((done, total)) => self.emit(DndEv::Progress { drag, done, total }),
            Report::At(at) => {
                let shared = Arc::clone(self);
                tokio::spawn(async move {
                    tokio::time::sleep_until(at.into()).await;
                    shared.flush(drag);
                });
            }
            Report::Not => {}
        }
    }

    /// Sends the change `report` held back, if the drag is still going.
    fn flush(self: &Arc<Self>, drag: DragId) {
        let mut st = self.lock();
        if let Some(inc) = st.incoming.get_mut(&drag) {
            inc.tally.flushed();
            self.report(drag, &mut inc.tally);
        }
    }

    /// Fetch `id` of `item` has `done` bytes so far.
    fn progress(self: &Arc<Self>, drag: DragId, item: u32, id: u64, done: u64) {
        let mut st = self.lock();
        let Some(inc) = st.incoming.get_mut(&drag) else {
            return;
        };
        if inc.fetches.get(&item).is_none_or(|f| f.id != id) {
            return;
        }
        inc.tally.set(item, done);
        self.report(drag, &mut inc.tally);
    }

    /// Fetch `id` of `item` ended; nothing if it was already settled.
    /// Events are sent under the lock, so they keep their order.
    fn received(
        self: &Arc<Self>,
        drag: DragId,
        item: u32,
        id: u64,
        result: Result<PathBuf, FailReason>,
    ) {
        let mut st = self.lock();
        let Some(inc) = st.incoming.get_mut(&drag) else {
            return;
        };
        if inc.fetches.get(&item).is_none_or(|f| f.id != id) {
            return;
        }
        inc.fetches.remove(&item);
        let result = match (result, &inc.reason) {
            (Err(FailReason::Cancelled), Some(why)) => Err(why.clone()),
            (r, _) => r,
        };
        self.report(drag, &mut inc.tally);
        self.emit(DndEv::Received { drag, item, result });
    }
}

/// Ends a drag that started here because an item can't be sent, and tells
/// the receiver why.
async fn fail_origin(shared: &Shared, session: &Session, drag: DragId, reason: FailReason) {
    if shared.origin_failed(drag, &reason) {
        let c = Control::DragCancel { drag, reason };
        if let Err(e) = session.send_control(&c).await {
            debug!("cancel of drag {drag} not sent: {e:#}");
        }
    }
}

/// Whether a fetch other than `except` is in flight to `key`.
fn lands_at(st: &State, key: &str, except: u64) -> bool {
    st.incoming
        .values()
        .flat_map(|inc| inc.fetches.values())
        .any(|f| f.id != except && f.key == key)
}

/// Where an item named `name` lands at `dest`, as `receive_item` places
/// it, folded so that two paths NTFS or APFS would take for one (and so
/// one `.cjpart`) are equal. Folding on every OS can only refuse a rare
/// second fetch that would have been safe.
fn dest_key(dest: &Dest, name: &str) -> String {
    let target = match dest {
        Dest::Exact(p) => p.clone(),
        Dest::InDir(d) => d.join(sanitize_component(name)),
    };
    target
        .to_string_lossy()
        .nfc()
        .collect::<String>()
        .to_lowercase()
}

/// The path a pending source resolves to, or why it never will; None if
/// cancelled first.
async fn resolve(
    mut rx: watch::Receiver<Option<Result<PathBuf, FailReason>>>,
    name: &str,
    cancel: &CancellationToken,
) -> Option<Result<PathBuf, FailReason>> {
    let value = match cancel
        .run_until_cancelled(rx.wait_for(Option::is_some))
        .await?
    {
        Ok(v) => v.clone(),
        // The sender is gone without a value.
        Err(_) => None,
    };
    Some(value.unwrap_or_else(|| {
        warn!("{name} never became ready to send");
        Err(FailReason::Unreadable { name: name.into() })
    }))
}

impl DndLink {
    pub fn new(
        session: Session,
        pacer: SharedPacer,
        space: Arc<dyn FreeSpace>,
        events: UnboundedSender<DndEv>,
    ) -> DndLink {
        DndLink {
            session,
            pacer,
            space,
            shared: Arc::new(Shared {
                events,
                state: Mutex::new(State::default()),
            }),
            offered: BTreeMap::new(),
            root: CancellationToken::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.lock()
    }

    /// Offers a drag that started here; item `i` is read from `sources[i]`.
    pub async fn offer(
        &mut self,
        drag: DragId,
        items: Vec<DragItem>,
        sources: Vec<Source>,
    ) -> Result<()> {
        if items.len() != sources.len() {
            bail!("{} items but {} sources", items.len(), sources.len());
        }
        {
            let mut st = self.lock();
            if st.outgoing.contains_key(&drag) {
                bail!("drag {drag} is already offered");
            }
            let cancel = self.root.child_token();
            let out = Outgoing {
                items: items.clone(),
                sources,
                cancel,
            };
            st.outgoing.insert(drag, out);
        }
        let sent = self
            .session
            .send_control(&Control::DragOffer { drag, items })
            .await;
        if sent.is_err() {
            self.lock().outgoing.remove(&drag);
        }
        sent
    }

    /// The receiver fetched `item`: sends it, on a task, once its source is
    /// ready.
    pub fn on_fetch(&mut self, drag: DragId, item: u32) {
        let st = self.lock();
        let Some(out) = st.outgoing.get(&drag) else {
            debug!("fetch for drag {drag}, which is over");
            return;
        };
        let i = item as usize;
        let found = match (out.sources.get(i), out.items.get(i)) {
            (Some(source), Some(it)) => {
                Some((source.clone(), it.name.clone(), out.cancel.child_token()))
            }
            _ => {
                warn!(
                    "fetch for item {item} of drag {drag}, which has {}",
                    out.items.len()
                );
                None
            }
        };
        drop(st);
        let (shared, session, pacer) = (
            self.shared.clone(),
            self.session.clone(),
            self.pacer.clone(),
        );
        let Some((source, name, cancel)) = found else {
            // Ends the drag on both sides rather than leave the fetch
            // waiting.
            let reason = FailReason::Unreadable {
                name: format!("item {item}"),
            };
            tokio::spawn(async move { fail_origin(&shared, &session, drag, reason).await });
            return;
        };
        tokio::spawn(async move {
            let root = match source {
                Source::Ready(p) => p,
                Source::Pending(rx) => match resolve(rx, &name, &cancel).await {
                    None => return,
                    Some(Ok(p)) => p,
                    // Whatever it is, even Cancelled: only this side knows.
                    Some(Err(why)) => return fail_origin(&shared, &session, drag, why).await,
                },
            };
            match send_item(&session, drag, item, &root, &pacer, &cancel).await {
                // Sent, stopped by the receiver (which knows why) or by
                // this side, or the link is gone.
                Ok(()) | Err(FailReason::Cancelled | FailReason::Disconnected) => {}
                Err(reason) => fail_origin(&shared, &session, drag, reason).await,
            }
        });
    }

    /// The receiver is done with a drag that started here.
    pub fn on_finished(&mut self, drag: DragId, outcome: DragOutcome) {
        let Some(out) = self.lock().outgoing.remove(&drag) else {
            debug!("drag {drag} finished, but it is already over");
            return;
        };
        out.cancel.cancel();
        drop(out);
        self.shared.emit(DndEv::Finished { drag, outcome });
    }

    /// The other computer offered a drag.
    pub fn on_offer(&mut self, drag: DragId, items: Vec<DragItem>) {
        let mut st = self.lock();
        if st.incoming.contains_key(&drag) {
            warn!("drag {drag} offered twice; the second offer is ignored");
            return;
        }
        let inc = Incoming {
            cancel: self.root.child_token(),
            reason: None,
            fetches: BTreeMap::new(),
            tally: Tally::new(&items),
        };
        st.incoming.insert(drag, inc);
        drop(st);
        self.offered.insert(drag, items);
    }

    /// An incoming drag's items, as offered; None once it is finished.
    pub fn items(&self, drag: DragId) -> Option<&[DragItem]> {
        self.offered.get(&drag).map(Vec::as_slice)
    }

    /// Asks the origin for `item` and receives it into `dest`; its
    /// `Received` follows. A drag already cancelled, or a destination
    /// another fetch is writing, fails the fetch at once. Err for an item
    /// not offered or already being fetched (with no `Received`), or if the
    /// request can't be sent (received as `Disconnected`).
    pub async fn fetch(&mut self, drag: DragId, item: u32, dest: Dest) -> Result<()> {
        let Some(id) = self.start_fetch(drag, item, dest)? else {
            return Ok(());
        };
        let sent = self
            .session
            .send_control(&Control::FileFetch { drag, item })
            .await;
        if sent.is_err() {
            let mut st = self.lock();
            if let Some(inc) = st.incoming.get_mut(&drag)
                && inc.fetches.get(&item).is_some_and(|f| f.id == id)
            {
                inc.fetches.remove(&item);
                let result = Err(FailReason::Disconnected);
                self.shared.emit(DndEv::Received { drag, item, result });
            }
        }
        sent
    }

    /// Records a fetch and returns its number, or None if it already
    /// failed.
    fn start_fetch(&self, drag: DragId, item: u32, dest: Dest) -> Result<Option<u64>> {
        let Some(it) = self.items(drag).and_then(|items| items.get(item as usize)) else {
            bail!("drag {drag} has no item {item}");
        };
        let key = dest_key(&dest, &it.name);
        let mut guard = self.shared.lock();
        let st = &mut *guard;
        let taken = lands_at(st, &key, 0);
        let Some(inc) = st.incoming.get_mut(&drag) else {
            bail!("drag {drag} is over");
        };
        if inc.fetches.contains_key(&item) {
            bail!("item {item} of drag {drag} is already being fetched");
        }
        let failed = if let Some(why) = &inc.reason {
            Some(why.clone())
        } else if taken {
            warn!("{key} is already being received; refused");
            Some(FailReason::Unwritable)
        } else {
            None
        };
        if let Some(why) = failed {
            let result = Err(why);
            self.shared.emit(DndEv::Received { drag, item, result });
            return Ok(None);
        }
        st.last_fetch += 1;
        let id = st.last_fetch;
        let Some(inc) = st.incoming.get_mut(&drag) else {
            bail!("drag {drag} is over");
        };
        let fetch = Fetch {
            id,
            dest,
            key,
            started: false,
            cancel: inc.cancel.child_token(),
        };
        inc.fetches.insert(item, fetch);
        Ok(Some(id))
    }

    /// A files stream from the origin: received, on a task, if it is one
    /// that was fetched, else cancelled. A stream the session's acceptor
    /// refuses before it gets here (a bad header, which only a broken peer
    /// sends) leaves its fetch waiting until a cancel, `finish` or
    /// `disconnected`.
    pub fn on_files(&mut self, files: FilesIn) {
        let h = files.header().clone();
        let (drag, item) = (h.drag, h.item);
        let mut guard = self.shared.lock();
        let st = &mut *guard;
        let found = st
            .incoming
            .get(&drag)
            .and_then(|inc| inc.fetches.get(&item))
            .filter(|f| !f.started)
            .map(|f| (f.id, dest_key(&f.dest, &h.name)));
        let Some((id, key)) = found else {
            drop(guard);
            debug!("files stream for item {item} of drag {drag}, which wasn't fetched");
            files.cancel();
            return;
        };
        // Its name may only be known now, from its header.
        let taken = lands_at(st, &key, id);
        let Some(inc) = st.incoming.get_mut(&drag) else {
            return;
        };
        if taken {
            warn!("{key} is already being received; refused");
            inc.fetches.remove(&item);
            let result = Err(FailReason::Unwritable);
            self.shared.emit(DndEv::Received { drag, item, result });
            drop(guard);
            files.cancel();
            return;
        }
        let Some(f) = inc.fetches.get_mut(&item) else {
            return;
        };
        f.key = key;
        f.started = true;
        let (dest, cancel) = (f.dest.clone(), f.cancel.clone());
        inc.tally.start(item, h.total_bytes);
        self.shared.report(drag, &mut inc.tally);
        drop(guard);

        let (shared, space) = (self.shared.clone(), self.space.clone());
        tokio::spawn(async move {
            let progress = {
                let shared = shared.clone();
                move |done| shared.progress(drag, item, id, done)
            };
            let result = receive_item(files, dest, space, progress, &cancel).await;
            if matches!(result, Err(FailReason::Cancelled)) && !cancel.is_cancelled() {
                // The origin reset the stream. If an item failed there, its
                // DragCancel saying why is on its way, and cancels us.
                let _ = tokio::time::timeout(REASON_WAIT, cancel.cancelled()).await;
            }
            shared.received(drag, item, id, result);
        });
    }

    /// Ends an incoming drag once the OS drag is over: tells the origin
    /// `outcome`. A fetch still in flight is cancelled and received as
    /// `Cancelled` (or the drag's cancel reason). Nothing for a drag
    /// already over.
    pub async fn finish(&mut self, drag: DragId, outcome: DragOutcome) -> Result<()> {
        if !self.end_incoming(drag, None, outcome.clone()) {
            debug!("drag {drag} is already over");
            return Ok(());
        }
        self.session
            .send_control(&Control::DragFinished { drag, outcome })
            .await
    }

    /// Ends an incoming drag here: what is in flight is cancelled and
    /// received as `leftover` (by default the drag's cancel reason, else
    /// `Cancelled`), then it finishes with `outcome`. False if unknown.
    /// The events go out before the cancelled transfers have removed their
    /// parts; each does so shortly after, on its own path.
    fn end_incoming(
        &mut self,
        drag: DragId,
        leftover: Option<FailReason>,
        outcome: DragOutcome,
    ) -> bool {
        self.offered.remove(&drag);
        let Some(mut inc) = self.lock().incoming.remove(&drag) else {
            return false;
        };
        inc.cancel.cancel();
        // Its pending flush will find the drag gone.
        if let Some((done, total)) = inc.tally.held() {
            self.shared.emit(DndEv::Progress { drag, done, total });
        }
        let why = leftover.or(inc.reason).unwrap_or(FailReason::Cancelled);
        for item in inc.fetches.into_keys() {
            let result = Err(why.clone());
            self.shared.emit(DndEv::Received { drag, item, result });
        }
        self.shared.emit(DndEv::Finished { drag, outcome });
        true
    }

    /// Stops one fetch of an incoming drag, quietly: the target no longer
    /// wants that item (the Mac's prefetch for a drag that went home or
    /// was cancelled). The origin is not told and the drag goes on; only
    /// that item's send stops. Received as `Cancelled` (or the drag's
    /// cancel reason). Nothing if the item isn't being fetched.
    #[cfg_attr(
        all(windows, not(test)),
        expect(dead_code, reason = "only the Mac's prefetch drops a single fetch")
    )]
    pub fn stop_fetch(&mut self, drag: DragId, item: u32) {
        let mut st = self.lock();
        let Some(inc) = st.incoming.get_mut(&drag) else {
            return;
        };
        let Some(f) = inc.fetches.get(&item) else {
            return;
        };
        if f.started {
            // Its transfer stops and is received then.
            f.cancel.cancel();
            return;
        }
        // Its stream, if it still comes, finds no fetch and is refused.
        inc.fetches.remove(&item);
        let result = Err(inc.reason.clone().unwrap_or(FailReason::Cancelled));
        self.shared.emit(DndEv::Received { drag, item, result });
    }

    /// Cancels a drag, either way, and tells the other computer.
    pub async fn cancel(&mut self, drag: DragId, reason: FailReason) -> Result<()> {
        if !self.cancel_here(drag, &reason) {
            debug!("cancel of drag {drag}, which is over");
            return Ok(());
        }
        self.session
            .send_control(&Control::DragCancel { drag, reason })
            .await
    }

    /// The other computer cancelled a drag.
    pub fn on_cancel(&mut self, drag: DragId, reason: FailReason) {
        if !self.cancel_here(drag, &reason) {
            debug!("drag {drag} cancelled, but it is already over");
        }
    }

    /// Cancels a drag on this side: one that started here ends; one that
    /// started there fails each fetch in flight, and any later one, with
    /// the first cancel's reason, until `finish`. False if unknown.
    fn cancel_here(&mut self, drag: DragId, reason: &FailReason) -> bool {
        let mut st = self.lock();
        let mut known = false;
        if let Some(out) = st.outgoing.remove(&drag) {
            known = true;
            out.cancel.cancel();
            let outcome = DragOutcome::Failed(reason.clone());
            self.shared.emit(DndEv::Finished { drag, outcome });
        }
        if let Some(inc) = st.incoming.get_mut(&drag) {
            known = true;
            let why = inc.reason.get_or_insert_with(|| reason.clone()).clone();
            inc.cancel.cancel();
            // Those still waiting for their stream fail now; the rest when
            // their transfer stops.
            inc.fetches.retain(|&item, f| {
                if !f.started {
                    let result = Err(why.clone());
                    self.shared.emit(DndEv::Received { drag, item, result });
                }
                f.started
            });
        }
        known
    }

    /// The session is gone: every fetch in flight is received as
    /// `Disconnected`, and every drag, either way, finishes as failed.
    pub fn disconnected(&mut self) {
        let outgoing = std::mem::take(&mut self.lock().outgoing);
        let lost = FailReason::Disconnected;
        for (drag, out) in outgoing {
            out.cancel.cancel();
            let outcome = DragOutcome::Failed(lost.clone());
            self.shared.emit(DndEv::Finished { drag, outcome });
        }
        let incoming: Vec<DragId> = self.lock().incoming.keys().copied().collect();
        for drag in incoming {
            let outcome = DragOutcome::Failed(lost.clone());
            self.end_incoming(drag, Some(lost.clone()), outcome);
        }
        self.offered.clear();
    }
}

impl Drop for DndLink {
    fn drop(&mut self) {
        self.root.cancel();
    }
}

/// Bytes done, and the total if known.
type Sample = (u64, Option<u64>);

/// What `Tally::report` says to do.
#[derive(Debug, PartialEq)]
enum Report {
    Now(Sample),
    /// Throttled: report again at this time.
    At(Instant),
    /// Unchanged, or already to be reported again.
    Not,
}

/// An incoming drag's progress over all its items.
struct Tally {
    /// Each item's size: its header's once its stream has started, else a
    /// file's offered size (None for a folder or an unsized file).
    sizes: Vec<Option<u64>>,
    done: Vec<u64>,
    /// When the last report was made, and what it said.
    last: Option<(Instant, Sample)>,
    /// A throttled change waits for a report at `Report::At`.
    flushing: bool,
}

impl Tally {
    fn new(items: &[DragItem]) -> Tally {
        let sizes = items
            .iter()
            .map(|i| match i.kind {
                ItemKind::File => i.size,
                ItemKind::Dir => None,
            })
            .collect();
        Tally {
            sizes,
            done: vec![0; items.len()],
            last: None,
            flushing: false,
        }
    }

    fn start(&mut self, item: u32, total: u64) {
        if let Some(s) = self.sizes.get_mut(item as usize) {
            *s = Some(total);
        }
    }

    fn set(&mut self, item: u32, done: u64) {
        if let Some(d) = self.done.get_mut(item as usize) {
            *d = done;
        }
    }

    /// Whether to report at `now`: a change is reported at most once per
    /// `PROGRESS_EVERY`; one that comes sooner is to be reported once that
    /// has passed.
    fn report(&mut self, now: Instant) -> Report {
        let value = self.sample();
        match self.last {
            Some((_, said)) if said == value => Report::Not,
            Some((at, _)) if now.saturating_duration_since(at) < PROGRESS_EVERY => {
                if self.flushing {
                    Report::Not
                } else {
                    self.flushing = true;
                    Report::At(at + PROGRESS_EVERY)
                }
            }
            _ => {
                self.last = Some((now, value));
                Report::Now(value)
            }
        }
    }

    /// The report a throttled change waited for is being made.
    fn flushed(&mut self) {
        self.flushing = false;
    }

    /// The change a throttled report holds back, if any, to send now
    /// instead of at its flush.
    fn held(&mut self) -> Option<Sample> {
        if !std::mem::take(&mut self.flushing) {
            return None;
        }
        let value = self.sample();
        let said = self.last.map(|(_, said)| said);
        (said != Some(value)).then_some(value)
    }

    fn sample(&self) -> Sample {
        let done = self.done.iter().fold(0u64, |a, d| a.saturating_add(*d));
        let total = self
            .sizes
            .iter()
            .try_fold(0u64, |a, s| Some(a.saturating_add((*s)?)));
        (done, total)
    }
}

/// The receiver's outcome for a drag once the OS drag has ended, from
/// whether it was dropped and each fetched item's result: None while any
/// is pending; then `NotDropped`, `Done` if every one arrived, or the
/// first failure in item order. Dropped with nothing fetched is `Done`:
/// the target took the drop and nothing failed.
pub fn incoming_outcome(
    dropped: bool,
    results: &[Option<Result<(), FailReason>>],
) -> Option<DragOutcome> {
    if results.iter().any(Option::is_none) {
        return None;
    }
    if !dropped {
        return Some(DragOutcome::NotDropped);
    }
    let failed = results.iter().flatten().find_map(|r| r.as_ref().err());
    Some(match failed {
        Some(why) => DragOutcome::Failed(why.clone()),
        None => DragOutcome::Done,
    })
}

/// Removes every entry of `root` last modified more than `max_age`
/// before `now`, folders with everything in them, and returns how many
/// went. For the drags' own temporary folders (spooled and received items,
/// and parts a hard kill left behind). An entry that can't be read or
/// removed is skipped; a missing `root` has nothing to remove.
pub fn sweep_temp(root: &Path, max_age: Duration, now: SystemTime) -> usize {
    sweep_temp_except(root, max_age, now, &[])
}

/// `sweep_temp`, but never the folder of a drag in `live`: the drags'
/// folders are named after their ids, and a long transfer can outlast any
/// age.
pub fn sweep_temp_except(
    root: &Path,
    max_age: Duration,
    now: SystemTime,
    live: &[DragId],
) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let live: Vec<String> = live.iter().map(DragId::to_string).collect();
    let mut removed = 0;
    for entry in entries.flatten() {
        if live.iter().any(|id| entry.file_name() == id.as_str()) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age > max_age);
        if !old {
            continue;
        }
        let gone = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match gone {
            Ok(()) => removed += 1,
            Err(e) => debug!("{} not swept: {e}", path.display()),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::session_tasks::{ClipboardOut, spawn_stream_tasks};
    use crate::common::transfer::CHUNK;
    use cj_core::dnd::{FileEntry, FilesHeader, ItemKind};
    use cj_core::pacer::Pacer;
    use cj_core::proto::Control;
    use cj_net::files::{STOP_CANCELLED, stop_code};
    use cj_net::session::{Listener, connect};
    use cj_net::{Identity, Trust};
    use std::fs;
    use std::io::{self, Write};
    use std::path::Path;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc::{self, UnboundedReceiver};
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    #[test]
    fn drag_ids_are_random_and_never_zero() {
        let mut r = [0u8; 32];
        assert_eq!(drag_id_from(r), None, "0 means no drag");
        r[8..].fill(0xFF);
        assert_eq!(drag_id_from(r), None, "only the first 8 bytes count");
        r[0] = 1;
        assert_eq!(drag_id_from(r), Some(1));
        let ids: std::collections::BTreeSet<DragId> = (0..1000).map(|_| fresh_drag_id()).collect();
        assert_eq!(ids.len(), 1000, "no repeats");
        assert!(!ids.contains(&0));
    }

    const T: Duration = Duration::from_secs(30);
    /// Big enough to still be on its way 100 ms in: the pacer starts at
    /// 20 MB/s.
    const BIG: usize = 16 * 1024 * 1024;

    /// Two live sessions over loopback, and the listener that keeps them up.
    async fn session_pair() -> (Listener, Session, Session) {
        let ident = || {
            let dir = tempfile::tempdir().unwrap();
            Identity::load_or_create(dir.path()).unwrap()
        };
        let (a, b) = (ident(), ident());
        let any_port = (std::net::Ipv4Addr::LOCALHOST, 0).into();
        let listener = Listener::bind(&a, Trust::PairingMode, any_port, "A")
            .await
            .unwrap();
        let (sa, sb) = timeout(T, async {
            tokio::join!(
                listener.accept(),
                connect(&b, Trust::PairingMode, listener.local_addr(), "B")
            )
        })
        .await
        .unwrap();
        (listener, sa.unwrap(), sb.unwrap())
    }

    /// A disk with all the space anyone could want.
    struct Lots;

    impl FreeSpace for Lots {
        fn free_bytes(&self, _dir: &Path) -> io::Result<u64> {
            Ok(u64::MAX)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum At {
        Origin,
        Receiver,
    }

    /// One computer: its link, and what its session and link hand the app.
    struct Side {
        link: DndLink,
        session: Session,
        events: UnboundedReceiver<DndEv>,
        files: UnboundedReceiver<FilesIn>,
        _tasks: (ClipboardOut, [JoinHandle<()>; 2]),
    }

    fn side(session: Session) -> Side {
        let (ev_tx, events) = mpsc::unbounded_channel();
        let (files_tx, files) = mpsc::unbounded_channel();
        let tasks = spawn_stream_tasks(&session, 1024, |_| true, move |f| files_tx.send(f).is_ok());
        let pacer = Arc::new(std::sync::Mutex::new(Pacer::new(Instant::now())));
        let link = DndLink::new(session.clone(), pacer, Arc::new(Lots), ev_tx);
        Side {
            link,
            session,
            events,
            files,
            _tasks: tasks,
        }
    }

    /// What one turn of the pump got.
    enum Got {
        /// A Control for this side.
        Control(At, Control),
        Files(At, FilesIn),
        Event(At, DndEv),
        Tick,
    }

    /// The origin and the receiver, with a pump that plays the apps' part:
    /// each side's Controls and files streams go to the other side's link.
    struct Pair {
        _l: Listener,
        origin: Side,
        receiver: Side,
        /// Every event so far: where, and when the pump got it.
        events: Vec<(At, DndEv, Instant)>,
        /// Every Control delivered, by whom it was delivered to.
        controls: Vec<(At, Control)>,
        /// While set, Controls for the receiver wait in `held`.
        hold: bool,
        held: Vec<Control>,
        /// Every files stream the receiver's link was given.
        streams: Vec<(DragId, u32)>,
    }

    impl Pair {
        async fn new() -> Pair {
            let (l, so, sr) = session_pair().await;
            Pair {
                _l: l,
                origin: side(so),
                receiver: side(sr),
                events: Vec::new(),
                controls: Vec::new(),
                hold: false,
                held: Vec::new(),
                streams: Vec::new(),
            }
        }

        fn side(&mut self, at: At) -> &mut Side {
            match at {
                At::Origin => &mut self.origin,
                At::Receiver => &mut self.receiver,
            }
        }

        async fn step(&mut self) {
            let (o, r) = (&mut self.origin, &mut self.receiver);
            // Events first: one a link emitted before sending a Control is
            // in its channel by the time that Control arrives, so it is
            // logged before the Control is delivered and causes more.
            let got = tokio::select! {
                biased;
                Some(e) = o.events.recv() => Got::Event(At::Origin, e),
                Some(e) = r.events.recv() => Got::Event(At::Receiver, e),
                c = o.session.recv_control() => Got::Control(At::Origin, c.unwrap()),
                c = r.session.recv_control() => Got::Control(At::Receiver, c.unwrap()),
                Some(f) = o.files.recv() => Got::Files(At::Origin, f),
                Some(f) = r.files.recv() => Got::Files(At::Receiver, f),
                () = sleep(Duration::from_millis(10)) => Got::Tick,
            };
            match got {
                Got::Control(At::Receiver, c) if self.hold => self.held.push(c),
                Got::Control(to, c) => self.deliver(to, c),
                Got::Files(to, f) => {
                    if to == At::Receiver {
                        let h = f.header();
                        self.streams.push((h.drag, h.item));
                    }
                    self.side(to).link.on_files(f);
                }
                Got::Event(at, e) => self.events.push((at, e, Instant::now())),
                Got::Tick => {}
            }
        }

        /// What an app does with a drag Control.
        fn deliver(&mut self, to: At, c: Control) {
            self.controls.push((to, c.clone()));
            let link = &mut self.side(to).link;
            match c {
                Control::DragOffer { drag, items } => link.on_offer(drag, items),
                Control::FileFetch { drag, item } => link.on_fetch(drag, item),
                Control::DragFinished { drag, outcome } => link.on_finished(drag, outcome),
                Control::DragCancel { drag, reason } => link.on_cancel(drag, reason),
                _ => {}
            }
        }

        /// Delivers the Controls held for the receiver, and stops holding.
        fn release(&mut self) {
            self.hold = false;
            for c in std::mem::take(&mut self.held) {
                self.deliver(At::Receiver, c);
            }
        }

        /// Pumps until `done` holds; fails after `T`.
        async fn until(&mut self, mut done: impl FnMut(&Pair) -> bool) {
            timeout(T, async {
                while !done(self) {
                    self.step().await;
                }
            })
            .await
            .expect("timed out");
        }

        async fn pump_for(&mut self, d: Duration) {
            let end = Instant::now() + d;
            while Instant::now() < end {
                self.step().await;
            }
        }

        /// Every result received for `item` of `drag`.
        fn received(&self, drag: DragId, item: u32) -> Vec<&Result<PathBuf, FailReason>> {
            self.events
                .iter()
                .filter_map(|(at, e, _)| match e {
                    DndEv::Received {
                        drag: d,
                        item: i,
                        result,
                    } if *at == At::Receiver && *d == drag && *i == item => Some(result),
                    _ => None,
                })
                .collect()
        }

        /// Every outcome `drag` finished with at `at`.
        fn finished(&self, at: At, drag: DragId) -> Vec<&DragOutcome> {
            self.events
                .iter()
                .filter_map(|(a, e, _)| match e {
                    DndEv::Finished { drag: d, outcome } if *a == at && *d == drag => Some(outcome),
                    _ => None,
                })
                .collect()
        }

        /// Every progress event for `drag`, and when the pump got it.
        fn progress(&self, drag: DragId) -> Vec<(u64, Option<u64>, Instant)> {
            self.events
                .iter()
                .filter_map(|(_, e, when)| match e {
                    DndEv::Progress {
                        drag: d,
                        done,
                        total,
                    } if *d == drag => Some((*done, *total, *when)),
                    _ => None,
                })
                .collect()
        }

        /// The latest progress for `drag`.
        fn last_progress(&self, drag: DragId) -> Option<(u64, Option<u64>)> {
            let (done, total, _) = *self.progress(drag).last()?;
            Some((done, total))
        }

        /// Checks `drag`'s progress events kept to 10 a second. Times are
        /// the pump's, which can bunch two up, hence one to spare.
        fn assert_capped(&self, drag: DragId) {
            let progress = self.progress(drag);
            let (Some(first), Some(last)) = (progress.first(), progress.last()) else {
                panic!("no progress for drag {drag}");
            };
            let over = last.2.duration_since(first.2).as_millis() as usize;
            assert!(
                progress.len() <= over / 100 + 2,
                "{} events in {over} ms",
                progress.len()
            );
        }

        fn has_progress(&self, drag: DragId) -> bool {
            self.progress(drag).iter().any(|(done, _, _)| *done > 0)
        }

        /// The DragCancels delivered to `to` for `drag`.
        fn cancels_to(&self, to: At, drag: DragId) -> Vec<&FailReason> {
            self.controls
                .iter()
                .filter_map(|(t, c)| match c {
                    Control::DragCancel { drag: d, reason } if *t == to && *d == drag => {
                        Some(reason)
                    }
                    _ => None,
                })
                .collect()
        }

        /// Offers `items` from the origin and waits for the receiver to have
        /// them.
        async fn offer(&mut self, drag: DragId, items: Vec<DragItem>, sources: Vec<Source>) {
            self.origin.link.offer(drag, items, sources).await.unwrap();
            self.until(|p| p.receiver.link.items(drag).is_some()).await;
        }
    }

    fn bytes(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn file(name: &str, size: usize) -> DragItem {
        DragItem {
            name: name.into(),
            kind: ItemKind::File,
            size: Some(size as u64),
        }
    }

    /// Writes a file of `size` bytes at `dir/name` and returns its path.
    fn write(dir: &Path, name: &str, size: usize, seed: u8) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, bytes(size, seed)).unwrap();
        p
    }

    /// What is directly in `dir`.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|de| de.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn unreadable(name: &str) -> FailReason {
        FailReason::Unreadable { name: name.into() }
    }

    #[tokio::test]
    async fn offer_fetch_receive_finish_roundtrip() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let notes = write(src.path(), "notes.txt", 300_000, 1);
        let photos = src.path().join("Photos");
        fs::create_dir(&photos).unwrap();
        write(&photos, "a.jpg", 5000, 2);
        let items = vec![
            file("notes.txt", 300_000),
            DragItem {
                name: "Photos".into(),
                kind: ItemKind::Dir,
                size: None,
            },
        ];
        let mut p = Pair::new().await;
        p.offer(
            7,
            items.clone(),
            vec![Source::Ready(notes), Source::Ready(photos)],
        )
        .await;
        assert_eq!(p.receiver.link.items(7), Some(&items[..]));

        for item in 0..2 {
            let dest = Dest::InDir(dest.path().into());
            p.receiver.link.fetch(7, item, dest).await.unwrap();
        }
        p.until(|p| p.received(7, 0).len() + p.received(7, 1).len() == 2)
            .await;
        assert_eq!(p.received(7, 0), [&Ok(dest.path().join("notes.txt"))]);
        assert_eq!(p.received(7, 1), [&Ok(dest.path().join("Photos"))]);
        assert_eq!(
            fs::read(dest.path().join("notes.txt")).unwrap(),
            bytes(300_000, 1)
        );
        assert_eq!(
            fs::read(dest.path().join("Photos").join("a.jpg")).unwrap(),
            bytes(5000, 2)
        );
        p.until(|p| p.last_progress(7) == Some((305_000, Some(305_000))))
            .await;

        let outcome = incoming_outcome(true, &[Some(Ok(())), Some(Ok(()))]).unwrap();
        assert_eq!(outcome, DragOutcome::Done);
        p.receiver.link.finish(7, outcome).await.unwrap();
        p.until(|p| !p.finished(At::Origin, 7).is_empty()).await;
        p.pump_for(Duration::from_millis(100)).await;
        assert_eq!(p.finished(At::Origin, 7), [&DragOutcome::Done]);
        assert_eq!(p.finished(At::Receiver, 7), [&DragOutcome::Done]);
        assert_eq!(p.receiver.link.items(7), None);
    }

    #[tokio::test]
    async fn pending_source_waits_then_sends() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (tx, rx) = watch::channel(None);
        let mut p = Pair::new().await;
        p.offer(
            3,
            vec![file("later.bin", 70_000)],
            vec![Source::Pending(rx)],
        )
        .await;
        let into = Dest::InDir(dest.path().into());
        p.receiver.link.fetch(3, 0, into).await.unwrap();
        p.pump_for(Duration::from_millis(300)).await;
        assert!(
            p.controls
                .contains(&(At::Origin, Control::FileFetch { drag: 3, item: 0 }))
        );
        assert!(p.received(3, 0).is_empty(), "sent before it was ready");
        assert!(names(dest.path()).is_empty());

        let later = write(src.path(), "later.bin", 70_000, 4);
        tx.send(Some(Ok(later))).unwrap();
        p.until(|p| !p.received(3, 0).is_empty()).await;
        assert_eq!(p.received(3, 0), [&Ok(dest.path().join("later.bin"))]);
        assert_eq!(
            fs::read(dest.path().join("later.bin")).unwrap(),
            bytes(70_000, 4)
        );
    }

    /// A pending source dropped without a value is unreadable; one that
    /// fails says why. Either way the origin cancels the drag with that
    /// reason.
    #[tokio::test]
    async fn pending_source_failure_reports_unreadable() {
        let dest = tempfile::tempdir().unwrap();
        let (dropped_tx, dropped_rx) = watch::channel(None);
        let (failed_tx, failed_rx) = watch::channel(None);
        let mut p = Pair::new().await;
        p.offer(
            4,
            vec![file("promised.pdf", 10)],
            vec![Source::Pending(dropped_rx)],
        )
        .await;
        p.offer(5, vec![file("", 10)], vec![Source::Pending(failed_rx)])
            .await;
        for drag in [4, 5] {
            let into = Dest::InDir(dest.path().join(drag.to_string()));
            p.receiver.link.fetch(drag, 0, into).await.unwrap();
        }
        p.pump_for(Duration::from_millis(100)).await;
        drop(dropped_tx);
        failed_tx.send(Some(Err(unreadable("spool.doc")))).unwrap();
        p.until(|p| !p.received(4, 0).is_empty() && !p.received(5, 0).is_empty())
            .await;

        assert_eq!(p.received(4, 0), [&Err(unreadable("promised.pdf"))]);
        assert_eq!(p.received(5, 0), [&Err(unreadable("spool.doc"))]);
        let failed = |name| DragOutcome::Failed(unreadable(name));
        assert_eq!(p.finished(At::Origin, 4), [&failed("promised.pdf")]);
        assert_eq!(p.finished(At::Origin, 5), [&failed("spool.doc")]);
        assert_eq!(p.cancels_to(At::Receiver, 4), [&unreadable("promised.pdf")]);
        assert_eq!(p.cancels_to(At::Receiver, 5), [&unreadable("spool.doc")]);
    }

    #[tokio::test]
    async fn cancel_from_receiver_stops_sender_and_cleans_dest() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let big = write(src.path(), "big.iso", BIG, 5);
        let mut p = Pair::new().await;
        p.offer(9, vec![file("big.iso", BIG)], vec![Source::Ready(big)])
            .await;
        let into = Dest::InDir(dest.path().into());
        p.receiver.link.fetch(9, 0, into).await.unwrap();
        p.until(|p| p.has_progress(9)).await;

        p.receiver
            .link
            .cancel(9, FailReason::Cancelled)
            .await
            .unwrap();
        p.until(|p| !p.received(9, 0).is_empty() && !p.finished(At::Origin, 9).is_empty())
            .await;
        assert_eq!(p.received(9, 0), [&Err(FailReason::Cancelled)]);
        let cancelled = DragOutcome::Failed(FailReason::Cancelled);
        assert_eq!(p.finished(At::Origin, 9), [&cancelled]);
        assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));

        // The origin stops quietly: the receiver knows why.
        p.receiver.link.finish(9, cancelled.clone()).await.unwrap();
        p.pump_for(Duration::from_millis(200)).await;
        assert!(p.cancels_to(At::Receiver, 9).is_empty());
        assert_eq!(p.finished(At::Origin, 9), [&cancelled]);
        assert_eq!(p.finished(At::Receiver, 9), [&cancelled]);
        assert_eq!(p.received(9, 0).len(), 1);
    }

    #[tokio::test]
    async fn a_stopped_fetch_ends_quietly_and_the_drag_goes_on() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let big = write(src.path(), "big.iso", BIG, 5);
        let small = write(src.path(), "small.txt", 1000, 6);
        let (_never, never_rx) = watch::channel(None);
        let mut p = Pair::new().await;
        let items = vec![
            file("big.iso", BIG),
            file("small.txt", 1000),
            file("later.txt", 10),
        ];
        let sources = vec![
            Source::Ready(big),
            Source::Ready(small),
            Source::Pending(never_rx),
        ];
        p.offer(9, items, sources).await;
        for item in 0..3 {
            fs::create_dir(dest.path().join(item.to_string())).unwrap();
        }
        for item in [0, 2] {
            let into = Dest::InDir(dest.path().join(item.to_string()));
            p.receiver.link.fetch(9, item, into).await.unwrap();
        }
        p.until(|p| p.has_progress(9)).await;

        // In flight, and not started yet (its source isn't ready).
        p.receiver.link.stop_fetch(9, 0);
        p.receiver.link.stop_fetch(9, 2);
        p.receiver.link.stop_fetch(9, 1); // Not fetched: nothing.
        p.until(|p| !p.received(9, 0).is_empty() && !p.received(9, 2).is_empty())
            .await;
        assert_eq!(p.received(9, 0), [&Err(FailReason::Cancelled)]);
        assert_eq!(p.received(9, 2), [&Err(FailReason::Cancelled)]);
        assert!(names(&dest.path().join("0")).is_empty(), "no part left");

        // The drag goes on: nobody was told, and another item still comes.
        let into = Dest::InDir(dest.path().join("1"));
        p.receiver.link.fetch(9, 1, into).await.unwrap();
        p.until(|p| !p.received(9, 1).is_empty()).await;
        assert_eq!(
            p.received(9, 1),
            [&Ok(dest.path().join("1").join("small.txt"))]
        );
        assert!(p.cancels_to(At::Receiver, 9).is_empty());
        assert!(p.cancels_to(At::Origin, 9).is_empty());
        assert!(p.finished(At::Origin, 9).is_empty());
        assert!(p.receiver.link.items(9).is_some());
    }

    #[tokio::test]
    async fn cancel_from_origin_fails_pending_fetch_with_cancelled() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (_never, never_rx) = watch::channel(None);
        let big = write(src.path(), "big.iso", BIG, 6);
        let small = write(src.path(), "after.txt", 10, 7);
        let mut p = Pair::new().await;
        p.offer(
            11,
            vec![
                file("never.txt", 10),
                file("big.iso", BIG),
                file("after.txt", 10),
            ],
            vec![
                Source::Pending(never_rx),
                Source::Ready(big),
                Source::Ready(small),
            ],
        )
        .await;
        for item in 0..2 {
            let into = Dest::InDir(dest.path().into());
            p.receiver.link.fetch(11, item, into).await.unwrap();
        }
        p.until(|p| p.has_progress(11)).await;

        p.origin
            .link
            .cancel(11, FailReason::Cancelled)
            .await
            .unwrap();
        p.until(|p| !p.received(11, 0).is_empty() && !p.received(11, 1).is_empty())
            .await;
        assert_eq!(p.received(11, 0), [&Err(FailReason::Cancelled)]);
        assert_eq!(p.received(11, 1), [&Err(FailReason::Cancelled)]);
        let cancelled = DragOutcome::Failed(FailReason::Cancelled);
        assert_eq!(p.finished(At::Origin, 11), [&cancelled]);
        assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));

        // A fetch after the cancel fails at once.
        let into = Dest::InDir(dest.path().into());
        p.receiver.link.fetch(11, 2, into).await.unwrap();
        p.until(|p| !p.received(11, 2).is_empty()).await;
        assert_eq!(p.received(11, 2), [&Err(FailReason::Cancelled)]);
        p.pump_for(Duration::from_millis(100)).await;
        assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));
    }

    #[tokio::test]
    async fn disconnect_fails_pending_fetches() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (_never, never_rx) = watch::channel(None);
        let big = write(src.path(), "big.iso", BIG, 8);
        let mut p = Pair::new().await;
        p.offer(
            12,
            vec![file("never.txt", 10), file("big.iso", BIG)],
            vec![Source::Pending(never_rx), Source::Ready(big)],
        )
        .await;
        for item in 0..2 {
            let into = Dest::InDir(dest.path().into());
            p.receiver.link.fetch(12, item, into).await.unwrap();
        }
        p.until(|p| p.has_progress(12)).await;

        p.receiver.link.disconnected();
        p.origin.link.disconnected();
        p.until(|_| names(dest.path()).is_empty()).await;
        p.pump_for(Duration::from_millis(300)).await;
        let lost = FailReason::Disconnected;
        assert_eq!(p.received(12, 0), [&Err(lost.clone())]);
        assert_eq!(p.received(12, 1), [&Err(lost.clone())]);
        let failed = DragOutcome::Failed(lost);
        assert_eq!(p.finished(At::Receiver, 12), [&failed]);
        assert_eq!(p.finished(At::Origin, 12), [&failed]);
        assert_eq!(p.receiver.link.items(12), None);
        let into = Dest::InDir(dest.path().into());
        assert!(p.receiver.link.fetch(12, 0, into).await.is_err());
    }

    /// Writes an 8 MB file item on `s` as `drag`/`item` until the receiver
    /// stops it, and returns why it stopped.
    fn spray(s: &Session, drag: DragId, item: u32) -> JoinHandle<anyhow::Result<()>> {
        let s = s.clone();
        tokio::spawn(async move {
            let size = 8 * 1024 * 1024;
            let h = FilesHeader {
                drag,
                item,
                name: "x.bin".into(),
                kind: ItemKind::File,
                total_bytes: size,
                entries: 1,
            };
            let mut out = s.open_files(&h).await?;
            out.entry(&FileEntry {
                path: vec![],
                kind: ItemKind::File,
                size,
            })
            .await?;
            let chunk = vec![0u8; CHUNK];
            for _ in 0..size / CHUNK as u64 {
                out.write(&chunk).await?;
            }
            out.finish()
        })
    }

    #[tokio::test]
    async fn unknown_files_stream_is_cancelled() {
        let mut p = Pair::new().await;
        p.offer(
            13,
            vec![file("x.bin", 8 * 1024 * 1024)],
            vec![Source::Ready("x.bin".into())],
        )
        .await;
        // An offered item that was never fetched, and a drag never offered.
        let sprays = [
            spray(&p.origin.session, 13, 0),
            spray(&p.origin.session, 99, 0),
        ];
        p.until(|_| sprays.iter().all(JoinHandle::is_finished))
            .await;
        for s in sprays {
            let e = s.await.unwrap().unwrap_err();
            assert_eq!(stop_code(&e), Some(STOP_CANCELLED), "{e:#}");
        }
        assert!(p.received(13, 0).is_empty());
        assert!(p.received(99, 0).is_empty());
    }

    #[tokio::test]
    async fn progress_aggregates_two_items_and_is_throttled() {
        // The rules, on fake time: a folder's size is unknown until its
        // stream starts, and its header's total then replaces it.
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let items = [
            file("a", 10),
            DragItem {
                name: "B".into(),
                kind: ItemKind::Dir,
                size: None,
            },
        ];
        let mut tally = Tally::new(&items);
        assert_eq!(tally.report(t0), Report::Now((0, None)));
        tally.start(1, 50);
        tally.set(1, 20);
        assert_eq!(tally.report(ms(50)), Report::At(ms(100)), "throttled");
        tally.set(1, 30);
        assert_eq!(tally.report(ms(60)), Report::Not, "already to be sent");
        tally.flushed();
        assert_eq!(tally.report(ms(100)), Report::Now((30, Some(60))));
        assert_eq!(tally.report(ms(300)), Report::Not, "unchanged");
        tally.start(0, 12);
        tally.set(0, 5);
        assert_eq!(tally.report(ms(350)), Report::Now((35, Some(62))));

        // Over a session: two files sent together, reported as one.
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let size = 3 * 1024 * 1024;
        let a = write(src.path(), "a.bin", size, 9);
        let b = write(src.path(), "b.bin", size, 10);
        let mut p = Pair::new().await;
        p.offer(
            21,
            vec![file("a.bin", size), file("b.bin", size)],
            vec![Source::Ready(a), Source::Ready(b)],
        )
        .await;
        for item in 0..2 {
            let into = Dest::InDir(dest.path().into());
            p.receiver.link.fetch(21, item, into).await.unwrap();
        }
        let all = 2 * size as u64;
        p.until(|p| p.last_progress(21) == Some((all, Some(all))))
            .await;
        p.until(|p| !p.received(21, 0).is_empty() && !p.received(21, 1).is_empty())
            .await;
        assert!(p.received(21, 0)[0].is_ok() && p.received(21, 1)[0].is_ok());

        let progress = p.progress(21);
        assert!(progress.iter().all(|(_, total, _)| *total == Some(all)));
        assert!(progress.windows(2).all(|w| w[0].0 <= w[1].0), "went back");
        assert!(progress.len() >= 2, "{progress:?}");
        p.assert_capped(21);
    }

    /// Many items ending within 100 ms, at once or one after another, still
    /// make at most 10 events a second, and the last value still arrives.
    #[tokio::test]
    async fn progress_cap_holds_for_many_small_items() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut items = Vec::new();
        let mut sources = Vec::new();
        for i in 0..20u8 {
            let name = format!("f{i}.txt");
            sources.push(Source::Ready(write(src.path(), &name, 2000, i)));
            items.push(file(&name, 2000));
        }
        let mut p = Pair::new().await;
        p.offer(50, items.clone(), sources.clone()).await;
        p.offer(51, items, sources).await;

        // All at once.
        let all = 20 * 2000;
        let at_once = dest.path().join("at once");
        let serial = dest.path().join("serial");
        fs::create_dir(&at_once).unwrap();
        fs::create_dir(&serial).unwrap();
        for item in 0..20 {
            let into = Dest::InDir(at_once.clone());
            p.receiver.link.fetch(50, item, into).await.unwrap();
        }
        p.until(|p| p.last_progress(50) == Some((all, Some(all))))
            .await;
        p.until(|p| (0..20).all(|i| !p.received(50, i).is_empty()))
            .await;
        p.assert_capped(50);

        // One after another.
        for item in 0..20 {
            let into = Dest::InDir(serial.clone());
            p.receiver.link.fetch(51, item, into).await.unwrap();
            p.until(|p| !p.received(51, item).is_empty()).await;
        }
        p.until(|p| p.last_progress(51) == Some((all, Some(all))))
            .await;
        p.assert_capped(51);
        assert_eq!(names(&serial).len(), 20);
    }

    /// The apps may run the link's calls on their own tasks.
    #[expect(dead_code, reason = "only has to compile")]
    fn link_and_its_calls_are_send(link: &mut DndLink) {
        fn send<T: Send>(_: T) {}
        fn send_type<T: Send>() {}
        send_type::<DndLink>();
        send(link.offer(1, vec![], vec![]));
        send(link.fetch(1, 0, Dest::InDir(PathBuf::new())));
        send(link.finish(1, DragOutcome::Done));
        send(link.cancel(1, FailReason::Cancelled));
    }

    #[test]
    fn incoming_outcome_rules() {
        use DragOutcome::*;
        let ok = || Some(Ok(()));
        assert_eq!(incoming_outcome(true, &[ok(), None]), None);
        assert_eq!(incoming_outcome(false, &[None]), None);
        assert_eq!(incoming_outcome(false, &[ok()]), Some(NotDropped));
        assert_eq!(incoming_outcome(false, &[]), Some(NotDropped));
        assert_eq!(incoming_outcome(true, &[ok(), ok()]), Some(Done));
        assert_eq!(incoming_outcome(true, &[]), Some(Done), "nothing fetched");
        let results = [
            ok(),
            Some(Err(FailReason::NoSpace)),
            Some(Err(FailReason::Cancelled)),
        ];
        assert_eq!(
            incoming_outcome(true, &results),
            Some(Failed(FailReason::NoSpace))
        );
    }

    fn grow(p: &Path) {
        let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(&[0; 1024]).unwrap();
    }

    /// The origin's reason for failing an item, not "Cancelled", reaches
    /// the receiver whichever of the stream's reset and the DragCancel
    /// arrives first, and when no stream arrived at all; with no reason in
    /// time after a reset, it is Cancelled. The file that changes grows
    /// only once its item is under way, so the receiver has its stream: a
    /// failure at once could reset it before the receiver's acceptor reads
    /// it, leaving no stream at all, which is the third case.
    #[tokio::test]
    async fn origin_failure_reason_reaches_receiver() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let changed = |name: &str| FailReason::Changed { name: name.into() };
        let mut p = Pair::new().await;

        // The reset first: the DragCancel is held until the receiver has
        // had time to see the stream fail.
        let growing = write(src.path(), "growing.log", BIG, 11);
        p.offer(
            31,
            vec![file("growing.log", BIG)],
            vec![Source::Ready(growing.clone())],
        )
        .await;
        let into = Dest::InDir(dest.path().into());
        p.receiver.link.fetch(31, 0, into).await.unwrap();
        p.hold = true;
        p.until(|p| p.has_progress(31)).await;
        grow(&growing);
        p.until(|p| !p.finished(At::Origin, 31).is_empty()).await;
        p.pump_for(Duration::from_millis(300)).await;
        assert!(p.received(31, 0).is_empty(), "{:?}", p.received(31, 0));
        p.release();
        p.until(|p| !p.received(31, 0).is_empty()).await;
        assert_eq!(p.received(31, 0), [&Err(changed("growing.log"))]);
        let failed = DragOutcome::Failed(changed("growing.log"));
        assert_eq!(p.finished(At::Origin, 31), [&failed]);

        // The DragCancel first: it arrives while the item is still coming.
        let big = write(src.path(), "big.iso", BIG, 12);
        p.offer(32, vec![file("big.iso", BIG)], vec![Source::Ready(big)])
            .await;
        let into = Dest::InDir(dest.path().into());
        p.receiver.link.fetch(32, 0, into).await.unwrap();
        p.until(|p| p.has_progress(32)).await;
        p.receiver.link.on_cancel(32, changed("big.iso"));
        p.until(|p| !p.received(32, 0).is_empty()).await;
        assert_eq!(p.received(32, 0), [&Err(changed("big.iso"))]);

        // No stream at all: the file can't be opened, so the origin fails
        // before it opens one, and only the DragCancel settles the fetch.
        let missing = src.path().join("missing.txt");
        p.offer(
            34,
            vec![file("missing.txt", 10)],
            vec![Source::Ready(missing)],
        )
        .await;
        let into = Dest::InDir(dest.path().into());
        p.receiver.link.fetch(34, 0, into).await.unwrap();
        p.hold = true;
        p.until(|p| !p.finished(At::Origin, 34).is_empty()).await;
        p.pump_for(Duration::from_millis(200)).await;
        assert!(!p.streams.contains(&(34, 0)), "a stream was opened");
        assert!(p.received(34, 0).is_empty(), "{:?}", p.received(34, 0));
        p.release();
        p.until(|p| !p.received(34, 0).is_empty()).await;
        assert_eq!(p.received(34, 0), [&Err(unreadable("missing.txt"))]);
        let failed = DragOutcome::Failed(unreadable("missing.txt"));
        assert_eq!(p.finished(At::Origin, 34), [&failed]);

        // No reason in time.
        let late = write(src.path(), "late.log", BIG, 13);
        p.offer(
            33,
            vec![file("late.log", BIG)],
            vec![Source::Ready(late.clone())],
        )
        .await;
        let into = Dest::InDir(dest.path().into());
        p.receiver.link.fetch(33, 0, into).await.unwrap();
        p.hold = true;
        p.until(|p| p.has_progress(33)).await;
        grow(&late);
        p.until(|p| !p.received(33, 0).is_empty()).await;
        assert_eq!(p.received(33, 0), [&Err(FailReason::Cancelled)]);
        p.release();
        p.pump_for(Duration::from_millis(100)).await;
        assert_eq!(p.received(33, 0).len(), 1);
        assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));
    }

    /// Two fetches never write the same place at once: the later one fails.
    #[tokio::test]
    async fn concurrent_fetches_to_one_destination_are_refused() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (one, two) = (src.path().join("1"), src.path().join("2"));
        fs::create_dir(&one).unwrap();
        fs::create_dir(&two).unwrap();
        let first = write(&one, "same.txt", BIG / 2, 14);
        let second = write(&two, "same.txt", 10, 15);
        let mut p = Pair::new().await;
        p.offer(
            40,
            vec![file("same.txt", BIG / 2)],
            vec![Source::Ready(first)],
        )
        .await;
        p.offer(
            41,
            vec![file("same.txt", 10)],
            vec![Source::Ready(second.clone())],
        )
        .await;
        // Not named until fetched: refused once its header names it.
        p.offer(42, vec![file("", 10)], vec![Source::Ready(second.clone())])
            .await;
        p.offer(
            43,
            vec![file("other.txt", 10)],
            vec![Source::Ready(second.clone())],
        )
        .await;
        // Names NTFS and APFS take for the same one share a part too.
        p.offer(
            44,
            vec![file("Same.txt", 10)],
            vec![Source::Ready(second.clone())],
        )
        .await;
        p.offer(45, vec![file("other.txt", 10)], vec![Source::Ready(second)])
            .await;
        let in_dir = || Dest::InDir(dest.path().into());
        p.receiver.link.fetch(40, 0, in_dir()).await.unwrap();
        p.receiver.link.fetch(41, 0, in_dir()).await.unwrap();
        p.receiver.link.fetch(42, 0, in_dir()).await.unwrap();
        let exact = Dest::Exact(dest.path().join("same.txt"));
        p.receiver.link.fetch(43, 0, exact).await.unwrap();
        p.receiver.link.fetch(44, 0, in_dir()).await.unwrap();
        let upper = Dest::Exact(dest.path().join("SAME.TXT"));
        p.receiver.link.fetch(45, 0, upper).await.unwrap();
        // The same item twice at once is the caller's mistake.
        assert!(p.receiver.link.fetch(40, 0, in_dir()).await.is_err());
        p.until(|p| {
            [40, 41, 42, 43, 44, 45]
                .iter()
                .all(|drag| !p.received(*drag, 0).is_empty())
        })
        .await;

        for drag in [41, 42, 43, 44, 45] {
            assert_eq!(p.received(drag, 0), [&Err(FailReason::Unwritable)]);
        }
        assert_eq!(p.received(40, 0), [&Ok(dest.path().join("same.txt"))]);
        assert_eq!(names(dest.path()), ["same.txt"]);
        assert_eq!(
            fs::read(dest.path().join("same.txt")).unwrap(),
            bytes(BIG / 2, 14)
        );
    }

    #[test]
    fn destination_keys_fold_case_and_normalization() {
        let d = PathBuf::from("drop");
        let key = dest_key(&Dest::InDir(d.clone()), "Cafe\u{301}.TXT");
        assert_eq!(key, dest_key(&Dest::Exact(d.join("caf\u{e9}.txt")), ""));
        assert_ne!(key, dest_key(&Dest::InDir(d), "cafe.txt"));
    }

    /// A pending source that resolves to Cancelled or Disconnected still
    /// ends the drag on both sides.
    #[tokio::test]
    async fn pending_source_cancelled_still_ends_the_drag() {
        let dest = tempfile::tempdir().unwrap();
        let mut p = Pair::new().await;
        let reasons = [(6, FailReason::Cancelled), (8, FailReason::Disconnected)];
        let mut senders = Vec::new();
        for (drag, _) in &reasons {
            let (tx, rx) = watch::channel(None);
            senders.push(tx);
            p.offer(*drag, vec![file("doc.txt", 10)], vec![Source::Pending(rx)])
                .await;
            let into = Dest::InDir(dest.path().join(drag.to_string()));
            p.receiver.link.fetch(*drag, 0, into).await.unwrap();
        }
        p.pump_for(Duration::from_millis(100)).await;
        for (tx, (_, why)) in senders.iter().zip(&reasons) {
            tx.send(Some(Err(why.clone()))).unwrap();
        }
        p.until(|p| reasons.iter().all(|(d, _)| !p.received(*d, 0).is_empty()))
            .await;
        for (drag, why) in &reasons {
            assert_eq!(p.received(*drag, 0), [&Err(why.clone())]);
            let failed = DragOutcome::Failed(why.clone());
            assert_eq!(p.finished(At::Origin, *drag), [&failed]);
            assert_eq!(p.cancels_to(At::Receiver, *drag), [why]);
        }
    }

    /// A fetch for an item the origin never offered ends the drag on both
    /// sides.
    #[tokio::test]
    async fn out_of_range_fetch_ends_the_drag() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let one = write(src.path(), "one.txt", 10, 16);
        let mut p = Pair::new().await;
        p.offer(14, vec![file("one.txt", 10)], vec![Source::Ready(one)])
            .await;
        let bad = Control::FileFetch { drag: 14, item: 5 };
        p.receiver.session.send_control(&bad).await.unwrap();
        p.until(|p| {
            !p.cancels_to(At::Receiver, 14).is_empty() && !p.finished(At::Origin, 14).is_empty()
        })
        .await;
        let why = unreadable("item 5");
        assert_eq!(p.cancels_to(At::Receiver, 14), [&why]);
        assert_eq!(
            p.finished(At::Origin, 14),
            [&DragOutcome::Failed(why.clone())]
        );
        let into = Dest::InDir(dest.path().into());
        p.receiver.link.fetch(14, 0, into).await.unwrap();
        p.until(|p| !p.received(14, 0).is_empty()).await;
        assert_eq!(p.received(14, 0), [&Err(why)]);
    }

    /// A drag finished while its last progress is still held back by the
    /// throttle sends that progress first.
    #[tokio::test]
    async fn held_progress_is_sent_before_the_drag_finishes() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let small = write(src.path(), "small.txt", 2000, 17);
        let mut p = Pair::new().await;
        p.offer(
            15,
            vec![file("small.txt", 2000)],
            vec![Source::Ready(small)],
        )
        .await;
        let into = Dest::InDir(dest.path().into());
        p.receiver.link.fetch(15, 0, into).await.unwrap();
        p.until(|p| !p.received(15, 0).is_empty()).await;
        // Within 100 ms of the first report, the last one is held back.
        let done = Some((2000, Some(2000)));
        assert_ne!(
            p.last_progress(15),
            done,
            "sent before the test could see it held"
        );

        let outcome = DragOutcome::Done;
        p.receiver.link.finish(15, outcome.clone()).await.unwrap();
        p.until(|p| !p.finished(At::Receiver, 15).is_empty()).await;
        p.pump_for(Duration::from_millis(200)).await;
        assert_eq!(p.last_progress(15), done);
        let at = |want: fn(&DndEv) -> bool| {
            p.events
                .iter()
                .rposition(|(a, e, _)| *a == At::Receiver && want(e))
                .unwrap()
        };
        let progress = at(|e| matches!(e, DndEv::Progress { drag: 15, .. }));
        let finished = at(|e| matches!(e, DndEv::Finished { drag: 15, .. }));
        assert!(progress < finished, "progress after the drag finished");
        assert_eq!(p.finished(At::Receiver, 15), [&outcome]);
    }

    /// Sets `path`'s modification time to `age` before `now`.
    fn age(path: &Path, age: Duration, now: std::time::SystemTime) {
        let mut opts = fs::File::options();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_WRITE_ATTRIBUTES, and FILE_FLAG_BACKUP_SEMANTICS to open
            // a folder.
            opts.access_mode(0x100).custom_flags(0x0200_0000);
        }
        #[cfg(not(windows))]
        opts.read(true);
        let f = opts.open(path).unwrap();
        f.set_modified(now - age).unwrap();
    }

    #[test]
    fn sweep_temp_removes_only_old_entries() {
        let root = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now();
        let hour = Duration::from_secs(3600);
        let (old, fresh) = (root.path().join("old"), root.path().join("fresh"));
        fs::create_dir(&old).unwrap();
        fs::write(old.join("inside.bin"), b"x").unwrap();
        fs::write(&fresh, b"y").unwrap();
        age(&old, 2 * hour, now);
        age(&fresh, Duration::from_secs(60), now);

        assert_eq!(sweep_temp(root.path(), hour, now), 1);
        assert!(!old.exists(), "the 2 h old folder stays");
        assert!(fresh.exists(), "the fresh file went");
        assert_eq!(sweep_temp(root.path(), hour, now), 0);
        let missing = root.path().join("not there");
        assert_eq!(sweep_temp(&missing, hour, now), 0);
    }

    #[test]
    fn sweep_temp_keeps_a_live_drag() {
        let root = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now();
        let hour = Duration::from_secs(3600);
        let (live, done) = (root.path().join("42"), root.path().join("43"));
        for dir in [&live, &done] {
            fs::create_dir(dir).unwrap();
            age(dir, 2 * hour, now);
        }

        assert_eq!(sweep_temp_except(root.path(), hour, now, &[42]), 1);
        assert!(live.exists(), "a live drag's folder went");
        assert!(!done.exists());
    }
}
