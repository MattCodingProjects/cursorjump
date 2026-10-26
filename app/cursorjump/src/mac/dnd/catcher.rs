//! Drags leaving the Mac (Task 16): the edge catcher.
//!
//! While the PC controls this Mac and the user drags with only the left
//! button near the entry edge, cj-inject shows the strip there
//! ([`Catcher::zone`]): a 2 pt, nearly invisible panel along that edge of
//! the main display, registered for file URLs and file promises.
//!
//! - `draggingEntered:` reads what the drag carries and emits `Offer`
//!   with a fresh [`DragId`]. A file URL is `Source::Ready` (its name and
//!   size from the file system; a folder's size is left unknown, so the
//!   main thread never walks a tree: its total comes with the transfer).
//!   A file promise is `Source::Pending`, named from `fileNames` when the
//!   source gives them before the drop (Chrome gives nil: name "").
//! - `draggingExited:` emits `OfferGone`, unless a push-out carries it.
//! - A push-out with only the left button held ([`CatchPort::push_out`],
//!   just before cj-inject posts the release) carries the strip's drag
//!   (the release drops it there). With none there yet, it is a wall,
//!   unless the drag pasteboard changed since the press
//!   ([`CatchPort::drag_began`], R48a): a drag began that the strip hasn't
//!   seen, such as Chrome's, which under posted input follows only from
//!   the release. Then cj-inject posts the release and opens the late
//!   window ([`CatchPort::await_late`]): a drag that drops on the strip
//!   before [`CatchPort::end_late_wait`] is caught (`LateDrop`).
//! - `performDragOperation:` takes only those two drops; any other drop on
//!   the strip is refused (NO), so the drag ends without dropping. A taken
//!   drop receives its promises into
//!   `~/Library/Caches/CursorJump/outgoing/<drag>/<item>/` on the
//!   catcher's own queue; each read resolves its item's source.
//!
//! The decisions live in [`Shared`], behind one lock, so cj-inject and the
//! main thread see one order of events; AppKit is in `appkit`, on the main
//! thread only.

mod appkit;

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cj_core::dnd::{DragId, DragItem, FailReason, ItemKind};
use cj_core::proto::Side;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardNameDrag};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::{MacDndEv, cache_dir, on_main};
use crate::common::dnd_link::{Source, fresh_drag_id};
use crate::mac::control::CatchPort;
use crate::mac::inject::Rectf;

/// How long a push-out with only the left button held waits for a late
/// drag's drop on the strip before leaving, and the least time the strip
/// stays up after such a push-out (R48).
pub const LATE_DRAG_WAIT: Duration = Duration::from_millis(150);
/// How near the entry edge (points) a held left button shows the strip.
pub const CATCH_ZONE_PT: f64 = 48.0;
/// The strip's thickness along the edge, in points.
const STRIP_PT: f64 = 2.0;
/// Nearly invisible, but drawn: the window server hit-tests per pixel.
const STRIP_ALPHA: f64 = 0.01;

/// Whether `pos` (global points) is on the display `bounds` and less than
/// [`CATCH_ZONE_PT`] in from its `edge`.
pub fn in_catch_zone(bounds: Rectf, edge: Side, pos: (f64, f64)) -> bool {
    let (x, y) = pos;
    let on = x >= bounds.x && x < bounds.x + bounds.w && y >= bounds.y && y < bounds.y + bounds.h;
    // The last point inside is `x + w - 1`, as for the Injector's walls.
    let inward = match edge {
        Side::Left => x - bounds.x,
        Side::Right => bounds.x + bounds.w - 1.0 - x,
        Side::Top => y - bounds.y,
        Side::Bottom => bounds.y + bounds.h - 1.0 - y,
    };
    on && inward < CATCH_ZONE_PT
}

/// The strip's frame along `edge` of the main display `main` (global
/// points, origin top left), in Cocoa's coordinates (origin bottom left
/// of the main display).
fn strip_frame(main: Rectf, edge: Side) -> Rectf {
    let global = match edge {
        Side::Left => Rectf {
            w: STRIP_PT,
            ..main
        },
        Side::Right => Rectf {
            x: main.x + main.w - STRIP_PT,
            w: STRIP_PT,
            ..main
        },
        Side::Top => Rectf {
            h: STRIP_PT,
            ..main
        },
        Side::Bottom => Rectf {
            y: main.y + main.h - STRIP_PT,
            h: STRIP_PT,
            ..main
        },
    };
    Rectf {
        y: main.h - (global.y + global.h),
        ..global
    }
}

/// The drag pasteboard's change count: it moves when a drag session sets
/// up its items, even one that doesn't follow posted input yet (spike run
/// 19). NSPasteboard is not main-thread-only (the clipboard watcher reads
/// the general one off-main too): a cheap call to the pasteboard server.
fn drag_pasteboard_count() -> isize {
    autoreleasepool(|_| {
        // SAFETY: a static AppKit string.
        let name = unsafe { NSPasteboardNameDrag };
        NSPasteboard::pasteboardWithName(name).changeCount()
    })
}

/// `~/Library/Caches/CursorJump/outgoing`: each caught drag's received
/// promises are here, under its id (Task 17 sweeps it).
pub fn outgoing_dir() -> PathBuf {
    cache_dir("outgoing")
}

// ------------------------------------------------------------ the offer

/// What the file system says of a dragged file or folder: one stat, on
/// the main thread in `draggingEntered:` (the brief allows it; a folder
/// is never walked).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Facts {
    is_dir: bool,
    len: u64,
}

impl Facts {
    fn of(path: &Path) -> Option<Facts> {
        std::fs::metadata(path).ok().map(|m| Facts {
            is_dir: m.is_dir(),
            len: m.len(),
        })
    }
}

/// One pasteboard item of a drag on the strip, as read on the main thread.
#[derive(Debug, Clone, PartialEq)]
enum Dragged {
    /// A file or folder on disk (`public.file-url`); `facts` is None if
    /// it can't be read (offered as a file of unknown size; sending it
    /// fails the drag).
    File { path: PathBuf, facts: Option<Facts> },
    /// A file promise: `receiver` is its index among the pasteboard's
    /// promise receivers; `names` are the files it promises, None when
    /// the source doesn't say before the drop; `dirs`, one per promised
    /// file type, whether it is `public.folder`.
    Promise {
        receiver: usize,
        names: Option<Vec<String>>,
        dirs: Vec<bool>,
    },
}

/// The sending half of a pending item's source.
type Resolve = watch::Sender<Option<Result<PathBuf, FailReason>>>;

/// A promised file still to be received.
struct Promised {
    /// Its item in the offer: names its folder under `outgoing/<drag>/`.
    item: usize,
    /// "" when the source didn't say.
    name: String,
    tx: Resolve,
}

impl Promised {
    fn resolve(self, result: Result<PathBuf, FailReason>) {
        // No receiver left means nobody wants it any more.
        let _ = self.tx.send(Some(result));
    }
}

/// One promise receiver's files, in the order its reader is called.
struct Promise {
    receiver: usize,
    files: Vec<Promised>,
}

/// What `draggingEntered:` offers, and what a drop must receive.
struct Built {
    items: Vec<DragItem>,
    sources: Vec<Source>,
    promises: Vec<Promise>,
}

/// One item per file URL and per promised file (one per promise whose
/// names are unknown).
fn build_offer(dragged: Vec<Dragged>) -> Built {
    let mut b = Built {
        items: Vec::new(),
        sources: Vec::new(),
        promises: Vec::new(),
    };
    for d in dragged {
        match d {
            Dragged::File { path, facts } => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let is_dir = facts.is_some_and(|f| f.is_dir);
                b.items.push(DragItem {
                    name,
                    kind: if is_dir {
                        ItemKind::Dir
                    } else {
                        ItemKind::File
                    },
                    // A folder's total comes with its transfer.
                    size: facts.filter(|f| !f.is_dir).map(|f| f.len),
                });
                b.sources.push(Source::Ready(path));
            }
            Dragged::Promise {
                receiver,
                names,
                dirs,
            } => {
                let names = names
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| vec![String::new()]);
                let n = names.len();
                // A type per file; otherwise, any folder type makes every
                // file of the promise a folder.
                let is_dir = |i: usize| match dirs.len() {
                    len if len == n => dirs[i],
                    _ => dirs.contains(&true),
                };
                let files = names
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let (tx, rx) = watch::channel(None);
                        let item = b.items.len();
                        b.items.push(DragItem {
                            name: name.clone(),
                            kind: if is_dir(i) {
                                ItemKind::Dir
                            } else {
                                ItemKind::File
                            },
                            size: None,
                        });
                        b.sources.push(Source::Pending(rx));
                        Promised { item, name, tx }
                    })
                    .collect();
                b.promises.push(Promise { receiver, files });
            }
        }
    }
    b
}

/// A promise receiver's files as its reader gets them: each read resolves
/// the next. Files it never reads fail when this is dropped.
struct Reading {
    drag: DragId,
    files: Mutex<VecDeque<Promised>>,
}

impl Reading {
    fn new(drag: DragId, files: Vec<Promised>) -> Reading {
        Reading {
            drag,
            files: Mutex::new(files.into()),
        }
    }

    /// The reader got `path`, received or (`ok` false) not.
    fn read(&self, path: PathBuf, ok: bool) {
        let next = self
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let Some(p) = next else {
            warn!(
                drag = self.drag,
                ?path,
                "a promise read more files than it named"
            );
            return;
        };
        if ok {
            info!(
                drag = self.drag,
                item = p.item,
                ?path,
                "promised file received"
            );
            p.resolve(Ok(path));
        } else {
            let name = if p.name.is_empty() {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                p.name.clone()
            };
            warn!(
                drag = self.drag,
                item = p.item,
                ?path,
                "a promised file could not be received"
            );
            p.resolve(Err(FailReason::Unreadable { name }));
        }
    }
}

// ------------------------------------------------------------ the state

/// Which drop on the strip `performDragOperation:` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// None: no `Leave` would carry it.
    Closed,
    /// This drag's: the release that drops it went out with its `Leave`.
    Carried(DragId),
    /// Any drag's, until the late wait ends (R48).
    Open,
    /// The late wait caught this drag.
    Caught(DragId),
}

/// The drag on the strip now.
struct Catch {
    drag: DragId,
    /// Its session's `draggingSequenceNumber`.
    seq: isize,
    promises: Vec<Promise>,
}

struct State {
    gate: Gate,
    catch: Option<Catch>,
}

/// What the catcher's handles and its AppKit side share.
struct Shared {
    events: Box<dyn Fn(MacDndEv) + Send + Sync>,
    /// Where promises are received: `<root>/<drag>/<item>/`.
    root: PathBuf,
    /// The strip's drag, 0 for none: `Catcher::offer` reads it on any thread.
    offer: AtomicU64,
    state: Mutex<State>,
    /// Reads the drag pasteboard's change count.
    drag_count: Box<dyn Fn() -> isize + Send + Sync>,
    /// The drag pasteboard's change count at the user's left-down.
    count_at_press: Mutex<Option<isize>>,
}

impl Shared {
    fn new(
        events: impl Fn(MacDndEv) + Send + Sync + 'static,
        root: PathBuf,
        drag_count: impl Fn() -> isize + Send + Sync + 'static,
    ) -> Shared {
        Shared {
            events: Box::new(events),
            root,
            offer: AtomicU64::new(0),
            state: Mutex::new(State {
                gate: Gate::Closed,
                catch: None,
            }),
            drag_count: Box::new(drag_count),
            count_at_press: Mutex::new(None),
        }
    }

    fn emit(&self, ev: MacDndEv) {
        if catch_unwind(AssertUnwindSafe(|| (self.events)(ev))).is_err() {
            warn!("the catcher's events callback panicked");
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn offer(&self) -> Option<DragId> {
        Some(self.offer.load(Ordering::SeqCst)).filter(|&d| d != 0)
    }

    /// `draggingEntered:`: drag session `seq` carrying `dragged` is on the
    /// strip. Its offer's id; None if there is nothing to catch.
    ///
    /// `Offer` is emitted under the lock, so it always comes before a
    /// `Leave` carrying it (cj-inject's `push_out` waits for the lock).
    fn entered(&self, seq: isize, dragged: Vec<Dragged>) -> Option<DragId> {
        if dragged.is_empty() {
            return None;
        }
        let drag = fresh_drag_id();
        let Built {
            items,
            sources,
            promises,
        } = build_offer(dragged);
        let mut st = self.lock();
        let old = st.catch.take();
        if let Some(old) = old.filter(|c| st.gate != Gate::Carried(c.drag)) {
            self.emit(MacDndEv::OfferGone { drag: old.drag });
        }
        info!(drag, items = items.len(), "a drag is on the strip");
        self.emit(MacDndEv::Offer {
            drag,
            items,
            sources,
        });
        st.catch = Some(Catch {
            drag,
            seq,
            promises,
        });
        self.offer.store(drag, Ordering::SeqCst);
        Some(drag)
    }

    /// `draggingUpdated:`: whether drag session `seq` is the strip's.
    fn updated(&self, seq: isize) -> bool {
        self.lock().catch.as_ref().is_some_and(|c| c.seq == seq)
    }

    /// Drag session `seq` (None: whichever) is off the strip without a
    /// drop: its offer is gone, unless a `Leave` carries it. Its promises
    /// fail.
    fn left(&self, seq: Option<isize>) {
        let mut st = self.lock();
        let Some(c) = st.catch.take_if(|c| seq.is_none_or(|s| s == c.seq)) else {
            return;
        };
        self.offer.store(0, Ordering::SeqCst);
        if st.gate == Gate::Carried(c.drag) {
            st.gate = Gate::Closed;
            warn!(
                drag = c.drag,
                "the carried drag left the strip without dropping"
            );
        } else {
            info!(drag = c.drag, "the drag left the strip");
            self.emit(MacDndEv::OfferGone { drag: c.drag });
        }
    }

    /// `performDragOperation:` of drag session `seq`: the drag and what
    /// to receive if the drop is taken; None to refuse it.
    fn perform(&self, seq: isize) -> Option<(DragId, Vec<Promise>)> {
        let mut st = self.lock();
        let c = st.catch.take_if(|c| c.seq == seq)?;
        self.offer.store(0, Ordering::SeqCst);
        match st.gate {
            Gate::Carried(d) if d == c.drag => {
                st.gate = Gate::Closed;
                info!(drag = c.drag, "the carried drag dropped on the strip");
            }
            Gate::Open => {
                st.gate = Gate::Caught(c.drag);
                info!(drag = c.drag, "a late drag dropped on the strip");
                self.emit(MacDndEv::LateDrop { drag: c.drag });
            }
            gate => {
                warn!(drag = c.drag, ?gate, "refusing a drop no leave carries");
                self.emit(MacDndEv::OfferGone { drag: c.drag });
                return None;
            }
        }
        Some((c.drag, c.promises))
    }

    /// See [`CatchPort::push_out`].
    fn push_out(&self) -> Option<DragId> {
        let mut st = self.lock();
        let drag = st.catch.as_ref().map(|c| c.drag)?;
        st.gate = Gate::Carried(drag);
        Some(drag)
    }

    /// See [`CatchPort::await_late`].
    fn await_late(&self) {
        self.lock().gate = Gate::Open;
    }

    /// See [`CatchPort::pressed`].
    fn pressed(&self) {
        let count = (self.drag_count)();
        *self
            .count_at_press
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(count);
    }

    /// See [`CatchPort::drag_began`]: false without a press.
    fn drag_began(&self) -> bool {
        let at_press = *self
            .count_at_press
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = (self.drag_count)();
        debug!(?at_press, now, "the drag pasteboard's change count");
        at_press.is_some_and(|c| c != now)
    }

    /// See [`CatchPort::end_late_wait`].
    fn end_late_wait(&self, carry: bool) -> Option<DragId> {
        let mut st = self.lock();
        let caught = match st.gate {
            Gate::Open => None,
            Gate::Caught(drag) => Some(drag),
            Gate::Closed | Gate::Carried(_) => return None,
        };
        st.gate = Gate::Closed;
        if let Some(drag) = caught.filter(|_| !carry) {
            info!(drag, "no leave carries the late drag");
            self.emit(MacDndEv::OfferGone { drag });
        }
        caught
    }

    /// Where promise receiver files of `drag`'s item `item` go.
    fn promise_dir(&self, drag: DragId, item: usize) -> PathBuf {
        self.root.join(drag.to_string()).join(item.to_string())
    }
}

// ------------------------------------------------------------ the handle

/// The Mac's edge catcher; clones share it. Every method may be called
/// from any thread and never waits for the main thread.
#[derive(Clone)]
pub struct Catcher {
    shared: Arc<Shared>,
}

impl Catcher {
    /// `events` gets the catcher's events, on any thread (the main thread
    /// and the promise queue); it must not block, nor call the catcher.
    pub fn new(events: impl Fn(MacDndEv) + Send + Sync + 'static) -> Catcher {
        let shared = Arc::new(Shared::new(events, outgoing_dir(), drag_pasteboard_count));
        // Made once, now (hidden), so a show only moves, orders front,
        // draws and flushes it.
        let made = shared.clone();
        on_main(move |mtm| appkit::make(mtm, &made));
        Catcher { shared }
    }

    /// Some(edge): shows the strip along that edge of the main display;
    /// None hides it. Queued to the main thread.
    pub fn zone(&self, show: Option<Side>) {
        let shared = self.shared.clone();
        on_main(move |mtm| appkit::zone(mtm, &shared, show));
    }

    /// The drag on the strip now, if any.
    pub fn offer(&self) -> Option<DragId> {
        self.shared.offer()
    }
}

impl CatchPort for Catcher {
    fn zone(&mut self, show: Option<Side>) {
        Catcher::zone(self, show);
    }
    fn pressed(&mut self) {
        self.shared.pressed();
    }
    fn drag_began(&mut self) -> bool {
        self.shared.drag_began()
    }
    fn push_out(&mut self) -> Option<DragId> {
        self.shared.push_out()
    }
    fn await_late(&mut self) {
        self.shared.await_late();
    }
    fn end_late_wait(&mut self, carry: bool) -> Option<DragId> {
        self.shared.end_late_wait(carry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicIsize;

    const MBP: Rectf = Rectf {
        x: 0.0,
        y: 0.0,
        w: 1512.0,
        h: 982.0,
    };

    #[test]
    fn in_catch_zone_each_edge() {
        let (r, b) = (MBP.w - 1.0, MBP.h - 1.0);
        for (edge, inside, outside) in [
            (Side::Left, (47.0, 500.0), (49.0, 500.0)),
            (Side::Right, (r - 47.0, 500.0), (r - 49.0, 500.0)),
            (Side::Top, (700.0, 47.0), (700.0, 49.0)),
            (Side::Bottom, (700.0, b - 47.0), (700.0, b - 49.0)),
        ] {
            assert!(in_catch_zone(MBP, edge, inside), "{edge:?} 47 pt in");
            assert!(!in_catch_zone(MBP, edge, outside), "{edge:?} 49 pt in");
        }
        assert!(in_catch_zone(MBP, Side::Left, (0.0, 0.0)), "on the edge");
        assert!(
            !in_catch_zone(MBP, Side::Left, (-1.0, 500.0)),
            "off the display"
        );
        assert!(
            !in_catch_zone(MBP, Side::Right, (MBP.w, 500.0)),
            "off the display"
        );
    }

    #[test]
    fn the_strip_runs_along_the_edge_in_cocoa_coordinates() {
        let at = |x, y, w, h| Rectf { x, y, w, h };
        assert_eq!(strip_frame(MBP, Side::Left), at(0.0, 0.0, 2.0, 982.0));
        assert_eq!(strip_frame(MBP, Side::Right), at(1510.0, 0.0, 2.0, 982.0));
        // Cocoa's y grows upwards: the top edge is at the frame's top.
        assert_eq!(strip_frame(MBP, Side::Top), at(0.0, 980.0, 1512.0, 2.0));
        assert_eq!(strip_frame(MBP, Side::Bottom), at(0.0, 0.0, 1512.0, 2.0));
    }

    fn file(path: &str, facts: Option<(bool, u64)>) -> Dragged {
        Dragged::File {
            path: path.into(),
            facts: facts.map(|(is_dir, len)| Facts { is_dir, len }),
        }
    }

    fn promise(receiver: usize, names: Option<&[&str]>) -> Dragged {
        promise_of(receiver, names, &[])
    }

    fn promise_of(receiver: usize, names: Option<&[&str]>, dirs: &[bool]) -> Dragged {
        Dragged::Promise {
            receiver,
            names: names.map(|n| n.iter().map(|s| s.to_string()).collect()),
            dirs: dirs.to_vec(),
        }
    }

    fn item(name: &str, kind: ItemKind, size: Option<u64>) -> DragItem {
        DragItem {
            name: name.into(),
            kind,
            size,
        }
    }

    fn pending(source: &Source) -> &watch::Receiver<Option<Result<PathBuf, FailReason>>> {
        match source {
            Source::Pending(rx) => rx,
            Source::Ready(p) => panic!("ready: {p:?}"),
        }
    }

    #[test]
    fn items_come_from_file_facts_and_promise_names() {
        let b = build_offer(vec![
            file("/Users/me/photo.jpg", Some((false, 1234))),
            file("/Users/me/Photos", Some((true, 96))),
            file("/gone/lost.txt", None),
            promise(0, None),
            promise(1, Some(&["a.pdf", "b.pdf"])),
            promise(2, Some(&[])),
        ]);
        assert_eq!(
            b.items,
            vec![
                item("photo.jpg", ItemKind::File, Some(1234)),
                item("Photos", ItemKind::Dir, None),
                item("lost.txt", ItemKind::File, None),
                item("", ItemKind::File, None),
                item("a.pdf", ItemKind::File, None),
                item("b.pdf", ItemKind::File, None),
                item("", ItemKind::File, None),
            ]
        );
        assert!(matches!(&b.sources[0], Source::Ready(p) if p == Path::new("/Users/me/photo.jpg")));
        assert!(matches!(&b.sources[1], Source::Ready(p) if p == Path::new("/Users/me/Photos")));
        assert!(
            b.sources[3..]
                .iter()
                .all(|s| matches!(s, Source::Pending(_)))
        );
        let shape: Vec<(usize, Vec<usize>)> = b
            .promises
            .iter()
            .map(|p| (p.receiver, p.files.iter().map(|f| f.item).collect()))
            .collect();
        assert_eq!(shape, vec![(0, vec![3]), (1, vec![4, 5]), (2, vec![6])]);
    }

    #[test]
    fn a_promise_read_resolves_its_files_in_order() {
        let b = build_offer(vec![promise(0, Some(&["a.pdf", ""]))]);
        let (a, unnamed) = (
            pending(&b.sources[0]).clone(),
            pending(&b.sources[1]).clone(),
        );
        let [p] = <[Promise; 1]>::try_from(b.promises).ok().unwrap();
        let reading = Reading::new(5, p.files);
        reading.read("/out/5/0/a.pdf".into(), true);
        assert_eq!(*a.borrow(), Some(Ok("/out/5/0/a.pdf".into())));
        reading.read("/out/5/0/b.pdf".into(), false);
        assert_eq!(
            *unnamed.borrow(),
            Some(Err(FailReason::Unreadable {
                name: "b.pdf".into()
            })),
            "named after the file it would have been"
        );
        reading.read("/out/5/0/c.pdf".into(), true); // One too many: ignored.
    }

    #[test]
    fn promised_files_never_read_fail() {
        let b = build_offer(vec![promise(0, None)]);
        let rx = pending(&b.sources[0]).clone();
        drop(b.promises);
        assert!(rx.has_changed().is_err(), "the sender is gone: unreadable");
    }

    /// A catcher's shared state that records its events.
    fn shared() -> (Shared, Arc<Mutex<Vec<String>>>) {
        let (s, log, _) = shared_counting();
        (s, log)
    }

    /// Also returns the fake drag pasteboard's change count.
    fn shared_counting() -> (Shared, Arc<Mutex<Vec<String>>>, Arc<AtomicIsize>) {
        let count = Arc::new(AtomicIsize::new(40));
        let c = count.clone();
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let s = Shared::new(
            move |ev| {
                let line = match ev {
                    MacDndEv::Offer { items, .. } => format!("Offer({})", items.len()),
                    MacDndEv::OfferGone { .. } => "OfferGone".into(),
                    MacDndEv::LateDrop { .. } => "LateDrop".into(),
                    other => format!("{other:?}"),
                };
                l.lock().unwrap().push(line);
            },
            "/out".into(),
            move || c.load(Ordering::SeqCst),
        );
        (s, log, count)
    }

    fn events(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    fn photo() -> Vec<Dragged> {
        vec![file("/Users/me/photo.jpg", Some((false, 10)))]
    }

    #[test]
    fn an_offer_lasts_while_the_drag_is_on_the_strip() {
        let (s, log) = shared();
        assert_eq!(s.entered(1, vec![]), None, "nothing to catch");
        let drag = s.entered(1, photo()).unwrap();
        assert_eq!(s.offer(), Some(drag));
        assert!(s.updated(1));
        assert!(!s.updated(2), "another session");
        s.left(Some(2));
        assert_eq!(s.offer(), Some(drag), "another session's exit");
        s.left(Some(1));
        assert_eq!(s.offer(), None);
        assert!(!s.updated(1));
        assert_eq!(events(&log), ["Offer(1)", "OfferGone"]);
        // Each entry is a new offer, with a new id.
        let again = s.entered(1, photo()).unwrap();
        assert_ne!(again, drag);
        s.left(None);
        assert_eq!(events(&log), ["Offer(1)", "OfferGone"]);
    }

    #[test]
    fn a_drop_no_leave_carries_is_refused() {
        let (s, log) = shared();
        let b = vec![promise(0, None)];
        s.entered(1, b).unwrap();
        assert!(s.perform(1).is_none());
        assert_eq!(s.offer(), None);
        assert_eq!(events(&log), ["Offer(1)", "OfferGone"]);
        assert!(s.perform(1).is_none(), "nothing on the strip");
    }

    #[test]
    fn a_push_out_carries_the_strip_drag_to_its_drop() {
        let (s, log) = shared();
        let drag = s.entered(1, photo()).unwrap();
        assert_eq!(s.push_out(), Some(drag));
        let (got, promises) = s.perform(1).expect("taken");
        assert_eq!(got, drag);
        assert!(promises.is_empty());
        assert_eq!(s.offer(), None);
        assert_eq!(events(&log), ["Offer(1)"], "no OfferGone, no LateDrop");
        assert_eq!(s.end_late_wait(true), None, "no late wait");
        // The next drop is not carried.
        s.entered(2, photo()).unwrap();
        assert!(s.perform(2).is_none());
    }

    #[test]
    fn a_carried_drag_that_leaves_without_dropping_is_not_withdrawn() {
        let (s, log) = shared();
        s.entered(1, photo()).unwrap();
        s.push_out();
        s.left(Some(1));
        assert_eq!(events(&log), ["Offer(1)"], "the Leave has it: no OfferGone");
        s.entered(2, photo()).unwrap();
        assert!(s.perform(2).is_none(), "the gate closed");
    }

    #[test]
    fn a_push_out_without_an_offer_opens_nothing_by_itself() {
        let (s, log) = shared();
        assert_eq!(s.push_out(), None, "nothing on the strip");
        s.entered(4, photo()).unwrap();
        assert!(s.perform(4).is_none(), "a wall: no late window");
        assert_eq!(events(&log), ["Offer(1)", "OfferGone"]);
    }

    #[test]
    fn a_drag_began_when_the_drag_pasteboard_changed_since_the_press() {
        let (s, _, count) = shared_counting();
        assert!(!s.drag_began(), "no press yet");
        s.pressed();
        assert!(!s.drag_began(), "a window drag or a text selection");
        count.fetch_add(1, Ordering::SeqCst);
        assert!(s.drag_began(), "a drag session set up its items");
        s.pressed();
        assert!(!s.drag_began(), "the next press starts afresh");
    }

    #[test]
    fn promised_folders_are_dirs() {
        let b = build_offer(vec![
            promise_of(0, Some(&["Photos"]), &[true]),
            promise_of(1, Some(&["a", "b"]), &[false, true]),
            promise_of(2, None, &[true]),
            promise_of(3, Some(&["c", "d"]), &[true]),
            promise_of(4, Some(&["e", "f"]), &[true, false, true]),
            promise_of(5, None, &[false, true]),
            promise_of(6, Some(&["g", "h"]), &[false]),
            promise_of(7, Some(&["i"]), &[]),
        ]);
        let kinds: Vec<ItemKind> = b.items.iter().map(|i| i.kind).collect();
        use ItemKind::{Dir, File};
        assert_eq!(
            kinds,
            [
                Dir, // one name, one folder type
                File, Dir, // a type per file
                Dir, // unnamed, a folder type
                Dir, Dir, // one folder type for two files
                Dir, Dir, // counts differ, a folder among the types
                Dir, // unnamed, counts differ, a folder among the types
                File, File, // no folder type
                File  // no types at all
            ]
        );
    }

    #[test]
    fn a_late_drop_within_the_wait_is_caught() {
        let (s, log) = shared();
        assert_eq!(s.push_out(), None, "nothing on the strip yet");
        s.await_late();
        let drag = s.entered(4, photo()).unwrap();
        assert_eq!(s.perform(4).map(|(d, _)| d), Some(drag));
        assert_eq!(events(&log), ["Offer(1)", "LateDrop"]);
        assert_eq!(s.end_late_wait(true), Some(drag));
        assert_eq!(s.end_late_wait(true), None, "once");
        assert_eq!(events(&log), Vec::<String>::new());
    }

    #[test]
    fn a_late_drop_after_the_wait_is_refused() {
        let (s, log) = shared();
        s.await_late();
        assert_eq!(s.end_late_wait(true), None);
        s.entered(4, photo()).unwrap();
        assert!(s.perform(4).is_none());
        assert_eq!(events(&log), ["Offer(1)", "OfferGone"]);
    }

    #[test]
    fn a_caught_drag_no_leave_carries_is_withdrawn() {
        let (s, log) = shared();
        s.await_late();
        let drag = s.entered(4, photo()).unwrap();
        s.perform(4).unwrap();
        events(&log);
        assert_eq!(s.end_late_wait(false), Some(drag));
        assert_eq!(events(&log), ["OfferGone"]);
    }

    #[test]
    fn promises_go_to_a_folder_per_item() {
        let (s, _) = shared();
        assert_eq!(s.promise_dir(42, 3), Path::new("/out/42/3"));
    }
}
