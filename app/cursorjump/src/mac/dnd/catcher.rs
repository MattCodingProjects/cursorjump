//! Drags leaving the Mac (Task 16): the edge catcher.
//!
//! While the PC controls this Mac and the user drags with only the left
//! button near the entry edge, cj-inject shows the strip there
//! ([`Catcher::zone`]): an 8 pt, nearly invisible panel along that edge of
//! the main display, registered for file URLs and file promises.
//!
//! - `draggingEntered:` reads what the drag carries and emits `Offer`
//!   with a fresh [`DragId`]. A file URL is `Source::Ready` (its name and
//!   size from the file system; a folder's size is left unknown, so the
//!   main thread never walks a tree: its total comes with the transfer).
//!   A file promise is `Source::Pending`, named from `fileNames` when the
//!   source gives them before the drop; Chrome gives nil, so the name comes
//!   from the promise's type instead ("image.jpg", R52). A file in another
//!   app's container tmp (WhatsApp's) is pending too: it is copied at the
//!   drop, before its app can delete it (R53).
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
//!   drop receives each promise into its own folder,
//!   `~/Library/Caches/CursorJump/outgoing/<drag>/.recv-<n>/`, on the
//!   catcher's own queue; each file read is moved to its item's own folder
//!   `outgoing/<drag>/<item>/` under its offered name (R52), and resolves
//!   its item's source.
//!
//! The decisions live in [`Shared`], behind one lock, so cj-inject and the
//! main thread see one order of events; AppKit is in `appkit`, on the main
//! thread only.

mod appkit;

use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cj_core::dnd::{DragId, DragItem, FailReason, ItemKind, sanitize_component};
use cj_core::proto::Side;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardNameDrag};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::source::copy_item;
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
/// The strip's thickness along the edge, in points (R54): a posted drag
/// reaches an 8 pt strip before the release; at 2 or 4 pt Chrome's drag
/// only arrived after the up.
const STRIP_PT: f64 = 8.0;
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

/// What a promised file's type is, for its fallback name (R52).
#[derive(Debug, Clone, Copy, PartialEq)]
enum TypeKind {
    Image,
    Video,
    Folder,
    Other,
}

/// The name of a promised file whose source gave none before the drop
/// (R52): "image", "video", "folder" or "file", with its type's preferred
/// extension (none for a folder), made safe as the PC would make it.
fn fallback_name(kind: TypeKind, ext: Option<&str>) -> String {
    let stem = match kind {
        TypeKind::Image => "image",
        TypeKind::Video => "video",
        TypeKind::Folder => "folder",
        TypeKind::Other => "file",
    };
    let name = match ext.filter(|e| !e.is_empty() && kind != TypeKind::Folder) {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_string(),
    };
    sanitize_component(&name)
}

/// Whether `path` is inside another app's container tmp folder,
/// `<home>/Library/Containers/<app>/Data/tmp/…`, which its app may empty
/// as soon as the drag ends (R53: WhatsApp's dragged photos).
fn in_container_tmp(path: &Path, home: &Path) -> bool {
    let Ok(rest) = path.strip_prefix(home) else {
        return false;
    };
    let parts: Vec<_> = rest.components().map(|c| c.as_os_str()).collect();
    parts.len() > 5
        && parts[0] == "Library"
        && parts[1] == "Containers"
        && parts[3] == "Data"
        && parts[4] == "tmp"
}

/// One pasteboard item of a drag on the strip, as read on the main thread.
#[derive(Debug, Clone, PartialEq)]
enum Dragged {
    /// A file or folder on disk (`public.file-url`); `facts` is None if
    /// it can't be read (offered as a file of unknown size; sending it
    /// fails the drag). `scratch`: it is in another app's container tmp
    /// (`in_container_tmp`), so it is copied at the drop (R53).
    File {
        path: PathBuf,
        facts: Option<Facts>,
        scratch: bool,
    },
    /// A file promise: `receiver` is its index among the pasteboard's
    /// promise receivers; `names` are the files it promises, None when
    /// the source doesn't say before the drop; `dirs`, one per promised
    /// file type, whether it is `public.folder`; `fallback`, the name of a
    /// file the source doesn't name (R52).
    Promise {
        receiver: usize,
        names: Option<Vec<String>>,
        dirs: Vec<bool>,
        fallback: String,
    },
}

/// The sending half of a pending item's source.
type Resolve = watch::Sender<Option<Result<PathBuf, FailReason>>>;

/// A promised file still to be received.
struct Promised {
    /// Its item in the offer: names its folder under `outgoing/<drag>/`.
    item: usize,
    /// The name it is offered under.
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

/// A file in another app's container tmp, copied at the drop (R53).
struct Scratch {
    from: PathBuf,
    to: Promised,
}

/// What `draggingEntered:` offers, and what a drop must receive or copy.
struct Built {
    items: Vec<DragItem>,
    sources: Vec<Source>,
    promises: Vec<Promise>,
    scratch: Vec<Scratch>,
}

/// One item per file URL and per promised file (one per promise whose
/// names are unknown).
fn build_offer(dragged: Vec<Dragged>) -> Built {
    let mut b = Built {
        items: Vec::new(),
        sources: Vec::new(),
        promises: Vec::new(),
        scratch: Vec::new(),
    };
    for d in dragged {
        match d {
            Dragged::File {
                path,
                facts,
                scratch,
            } => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let is_dir = facts.is_some_and(|f| f.is_dir);
                b.items.push(DragItem {
                    name: name.clone(),
                    kind: if is_dir {
                        ItemKind::Dir
                    } else {
                        ItemKind::File
                    },
                    // A folder's total comes with its transfer.
                    size: facts.filter(|f| !f.is_dir).map(|f| f.len),
                });
                if scratch {
                    let (tx, rx) = watch::channel(None);
                    let item = b.items.len() - 1;
                    b.sources.push(Source::Pending(rx));
                    let to = Promised { item, name, tx };
                    b.scratch.push(Scratch { from: path, to });
                } else {
                    b.sources.push(Source::Ready(path));
                }
            }
            Dragged::Promise {
                receiver,
                names,
                dirs,
                fallback,
            } => {
                let names: Vec<String> = names
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| vec![String::new()])
                    .into_iter()
                    .map(|n| if n.is_empty() { fallback.clone() } else { n })
                    .collect();
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
    /// Each item's folder is `<root>/<drag>/<item>/`.
    root: PathBuf,
    files: Mutex<VecDeque<Promised>>,
}

impl Reading {
    fn new(drag: DragId, root: PathBuf, files: Vec<Promised>) -> Reading {
        Reading {
            drag,
            root,
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
            let dir = item_dir(&self.root, self.drag, p.item);
            match place(&path, &dir, &p.name) {
                Ok(placed) => p.resolve(Ok(placed)),
                Err(e) => {
                    warn!(
                        drag = self.drag,
                        item = p.item,
                        ?path,
                        "putting a promised file under its offered name failed: {e}"
                    );
                    let name = p.name.clone();
                    p.resolve(Err(FailReason::Unreadable { name }));
                }
            }
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

/// `<root>/<drag>/<item>/`: where an item's promised or copied file is
/// served from, alone, under its offered name.
fn item_dir(root: &Path, drag: DragId, item: usize) -> PathBuf {
    root.join(drag.to_string()).join(item.to_string())
}

/// Puts a promised file, received as `received` into its promise's own
/// folder, into its item's own empty folder `dir` under its offered name
/// `offered` (R52): the PC made its placeholder from that name, and the
/// transfer names the file after the path it reads, so the two never
/// differ. Never overwrites. Returns where it is now.
fn place(received: &Path, dir: &Path, offered: &str) -> io::Result<PathBuf> {
    let to = dir.join(sanitize_component(offered));
    std::fs::create_dir_all(dir)?;
    if std::fs::symlink_metadata(&to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists", to.display()),
        ));
    }
    std::fs::rename(received, &to)?;
    Ok(to)
}

/// Copies a dragged file or folder out of another app's container tmp
/// into its item's folder `dir`, under its own name (R53); a file copy is
/// a clone on APFS. Never overwrites. Returns the copy.
fn copy_scratch(from: &Path, dir: &Path) -> io::Result<PathBuf> {
    let name = from
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    std::fs::create_dir_all(dir)?;
    let to = dir.join(name);
    copy_item(from, &to)?;
    Ok(to)
}

/// A file's APFS clone into `dir`, made at once (`clonefile` is a
/// metadata operation); None when it can't be (a folder, another volume,
/// the name taken): `copy_scratch` does it then, off the main thread.
fn clone_now(from: &Path, dir: &Path) -> Option<PathBuf> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    if !std::fs::symlink_metadata(from).ok()?.is_file() {
        return None;
    }
    let to = dir.join(from.file_name()?);
    std::fs::create_dir_all(dir).ok()?;
    let (src, dst) = (
        CString::new(from.as_os_str().as_bytes()).ok()?,
        CString::new(to.as_os_str().as_bytes()).ok()?,
    );
    // SAFETY: two valid NUL-terminated paths; no flags. It never
    // overwrites: an existing `to` is an error.
    let ok = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) } == 0;
    ok.then_some(to)
}

/// At the drop, on the main thread: clones each of `drag`'s files that
/// sit in another app's container tmp, resolving its source with the
/// clone, before the app can delete it (R53). Returns what must be copied
/// on a thread instead (folders, or a clone that failed).
fn clone_scratch_now(root: &Path, drag: DragId, scratch: Vec<Scratch>) -> Vec<Scratch> {
    let mut rest = Vec::new();
    for s in scratch {
        match clone_now(&s.from, &item_dir(root, drag, s.to.item)) {
            Some(clone) => {
                info!(
                    drag,
                    item = s.to.item,
                    ?clone,
                    "cloned out of its app's tmp folder"
                );
                s.to.resolve(Ok(clone));
            }
            None => rest.push(s),
        }
    }
    rest
}

/// Copies each of `drag`'s files that sit in another app's container tmp,
/// resolving its source with the copy (R53). Off the main thread.
fn copy_all_scratch(root: &Path, drag: DragId, scratch: Vec<Scratch>) {
    for Scratch { from, to } in scratch {
        let dir = item_dir(root, drag, to.item);
        match copy_scratch(&from, &dir) {
            Ok(copy) => {
                info!(
                    drag,
                    item = to.item,
                    ?copy,
                    "copied out of its app's tmp folder"
                );
                to.resolve(Ok(copy));
            }
            Err(e) => {
                warn!(
                    drag,
                    item = to.item,
                    ?from,
                    "copying out of its app's tmp folder failed: {e}"
                );
                let name = to.name.clone();
                to.resolve(Err(FailReason::Unreadable { name }));
            }
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
    scratch: Vec<Scratch>,
}

/// A drop the strip took: what it must receive or copy.
struct Taken {
    drag: DragId,
    promises: Vec<Promise>,
    scratch: Vec<Scratch>,
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

    #[cfg_attr(
        all(not(test), not(debug_assertions)),
        expect(dead_code, reason = "only the tests and the catcher smoke read it")
    )]
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
            scratch,
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
            scratch,
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
    /// to receive or copy if the drop is taken; None to refuse it.
    fn perform(&self, seq: isize) -> Option<Taken> {
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
        Some(Taken {
            drag: c.drag,
            promises: c.promises,
            scratch: c.scratch,
        })
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

    /// Where promise receiver `receiver` of `drag` receives its files,
    /// before each goes to its item's folder.
    fn receive_dir(&self, drag: DragId, receiver: usize) -> PathBuf {
        self.root
            .join(drag.to_string())
            .join(format!(".recv-{receiver}"))
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
    #[cfg_attr(
        not(debug_assertions),
        expect(dead_code, reason = "only the catcher smoke reads it")
    )]
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
        assert_eq!(strip_frame(MBP, Side::Left), at(0.0, 0.0, 8.0, 982.0));
        assert_eq!(strip_frame(MBP, Side::Right), at(1504.0, 0.0, 8.0, 982.0));
        // Cocoa's y grows upwards: the top edge is at the frame's top.
        assert_eq!(strip_frame(MBP, Side::Top), at(0.0, 974.0, 1512.0, 8.0));
        assert_eq!(strip_frame(MBP, Side::Bottom), at(0.0, 0.0, 1512.0, 8.0));
    }

    fn file(path: &str, facts: Option<(bool, u64)>) -> Dragged {
        Dragged::File {
            path: path.into(),
            facts: facts.map(|(is_dir, len)| Facts { is_dir, len }),
            scratch: false,
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
            fallback: String::new(),
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
        let tmp = tempfile::tempdir().unwrap();
        let (root, recv) = (tmp.path(), tmp.path().join("5").join(".recv-0"));
        std::fs::create_dir_all(&recv).unwrap();
        std::fs::write(recv.join("a.pdf"), b"a").unwrap();
        let b = build_offer(vec![promise(0, Some(&["a.pdf", ""]))]);
        let (a, unnamed) = (
            pending(&b.sources[0]).clone(),
            pending(&b.sources[1]).clone(),
        );
        let [p] = <[Promise; 1]>::try_from(b.promises).ok().unwrap();
        let reading = Reading::new(5, root.into(), p.files);
        reading.read(recv.join("a.pdf"), true);
        let a_pdf = root.join("5").join("0").join("a.pdf");
        assert_eq!(*a.borrow(), Some(Ok(a_pdf.clone())));
        assert_eq!(std::fs::read(a_pdf).unwrap(), b"a");
        reading.read(recv.join("b.pdf"), false);
        assert_eq!(
            *unnamed.borrow(),
            Some(Err(FailReason::Unreadable {
                name: "b.pdf".into()
            })),
            "named after the file it would have been"
        );
        reading.read(recv.join("c.pdf"), true); // One too many: ignored.
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
        let taken = s.perform(1).expect("taken");
        assert_eq!(taken.drag, drag);
        assert!(taken.promises.is_empty() && taken.scratch.is_empty());
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
        assert_eq!(s.perform(4).map(|t| t.drag), Some(drag));
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
    fn a_nameless_promise_is_named_after_its_type() {
        use TypeKind::{Folder, Image, Other, Video};
        assert_eq!(fallback_name(Image, Some("jpeg")), "image.jpeg");
        assert_eq!(fallback_name(Video, Some("mp4")), "video.mp4");
        assert_eq!(fallback_name(Folder, None), "folder");
        assert_eq!(fallback_name(Folder, Some("x")), "folder", "no extension");
        assert_eq!(fallback_name(Other, Some("pdf")), "file.pdf");
        assert_eq!(fallback_name(Other, None), "file");
        assert_eq!(fallback_name(Image, Some("")), "image");
        assert_eq!(
            fallback_name(Other, Some("a:b")),
            "file.a_b",
            "made safe as the PC would"
        );

        let named = |fallback: &str, names: Option<&[&str]>| {
            let mut d = promise(0, names);
            if let Dragged::Promise { fallback: f, .. } = &mut d {
                *f = fallback.into();
            }
            let b = build_offer(vec![d]);
            let promised: Vec<String> =
                b.promises[0].files.iter().map(|f| f.name.clone()).collect();
            let offered: Vec<String> = b.items.into_iter().map(|i| i.name).collect();
            assert_eq!(offered, promised, "offered as promised");
            offered
        };
        assert_eq!(named("image.jpeg", None), ["image.jpeg"]);
        assert_eq!(named("image.jpeg", Some(&[])), ["image.jpeg"]);
        assert_eq!(
            named("image.jpeg", Some(&["a.png", ""])),
            ["a.png", "image.jpeg"]
        );
    }

    #[test]
    fn container_tmp_files_are_scratch() {
        let home = Path::new("/Users/me");
        let wa = "/Users/me/Library/Containers/net.whatsapp.WhatsApp/Data/tmp/x/IMG.jpg";
        assert!(in_container_tmp(Path::new(wa), home));
        assert!(in_container_tmp(
            Path::new("/Users/me/Library/Containers/a/Data/tmp/IMG.jpg"),
            home
        ));
        for not in [
            "/Users/me/Library/Containers/a/Data/tmp",
            "/Users/me/Library/Containers/a/Data/Documents/IMG.jpg",
            "/Users/me/Library/Caches/a/tmp/x/IMG.jpg",
            "/Users/me/Desktop/IMG.jpg",
            "/Users/other/Library/Containers/a/Data/tmp/IMG.jpg",
            "/private/var/folders/x/T/IMG.jpg",
        ] {
            assert!(!in_container_tmp(Path::new(not), home), "{not}");
        }

        let mut d = file(wa, Some((false, 7)));
        if let Dragged::File { scratch, .. } = &mut d {
            *scratch = true;
        }
        let b = build_offer(vec![file("/Users/me/a.txt", Some((false, 1))), d]);
        assert_eq!(b.items[1], item("IMG.jpg", ItemKind::File, Some(7)));
        assert!(matches!(&b.sources[0], Source::Ready(_)));
        let rx = pending(&b.sources[1]).clone();
        assert_eq!(b.scratch.len(), 1);
        assert_eq!(b.scratch[0].from, Path::new(wa));
        assert_eq!(
            (b.scratch[0].to.item, b.scratch[0].to.name.as_str()),
            (1, "IMG.jpg")
        );
        drop(b.scratch);
        assert!(rx.has_changed().is_err(), "a drop that never came fails it");
    }

    #[test]
    fn a_file_is_cloned_at_once_and_never_over_another() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("IMG.jpg");
        std::fs::write(&from, b"jpeg").unwrap();
        let dir = tmp.path().join("out").join("0");
        assert_eq!(clone_now(&from, &dir), Some(dir.join("IMG.jpg")));
        assert_eq!(std::fs::read(dir.join("IMG.jpg")).unwrap(), b"jpeg");
        assert_eq!(clone_now(&from, &dir), None, "the name is taken");
        assert_eq!(clone_now(tmp.path(), &dir), None, "a folder");
        assert_eq!(clone_now(&tmp.path().join("gone"), &dir), None);
    }

    #[test]
    fn scratch_files_are_copied_into_their_item_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let app_tmp = tmp.path().join("app");
        std::fs::create_dir_all(app_tmp.join("Album")).unwrap();
        std::fs::write(app_tmp.join("IMG.jpg"), b"jpeg").unwrap();
        std::fs::write(app_tmp.join("Album").join("a.jpg"), b"a").unwrap();
        let root = tmp.path().join("outgoing");
        let mut d = file(&app_tmp.join("IMG.jpg").to_string_lossy(), Some((false, 4)));
        let mut album = file(&app_tmp.join("Album").to_string_lossy(), Some((true, 0)));
        let mut gone = file(&app_tmp.join("gone.jpg").to_string_lossy(), None);
        for x in [&mut d, &mut album, &mut gone] {
            if let Dragged::File { scratch, .. } = x {
                *scratch = true;
            }
        }
        let b = build_offer(vec![d, album, gone]);
        let rxs: Vec<_> = b.sources.iter().map(|s| pending(s).clone()).collect();
        // At the drop: the file is cloned there and then.
        let rest = clone_scratch_now(&root, 9, b.scratch);
        let copy = root.join("9").join("0").join("IMG.jpg");
        assert_eq!(*rxs[0].borrow(), Some(Ok(copy.clone())));
        assert_eq!(std::fs::read(&copy).unwrap(), b"jpeg");
        assert_eq!(rest.len(), 2, "the folder and the missing file are left");
        assert!(rxs[1].borrow().is_none() && rxs[2].borrow().is_none());
        // Then on the thread.
        copy_all_scratch(&root, 9, rest);
        let tree = root.join("9").join("1").join("Album");
        assert_eq!(*rxs[1].borrow(), Some(Ok(tree.clone())));
        assert_eq!(std::fs::read(tree.join("a.jpg")).unwrap(), b"a");
        assert_eq!(
            *rxs[2].borrow(),
            Some(Err(FailReason::Unreadable {
                name: "gone.jpg".into()
            }))
        );
        // The app emptying its tmp folder no longer matters.
        std::fs::remove_dir_all(&app_tmp).unwrap();
        assert!(copy.exists());
    }

    #[test]
    fn each_promised_file_is_served_alone_under_its_offered_name() {
        // One promise, four files, received into its own folder under
        // whatever names the source chose.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let recv = root.join("5").join(".recv-0");
        std::fs::create_dir_all(&recv).unwrap();
        let mut d = promise(0, Some(&["", "b.pdf", "c?.pdf", "b.pdf"]));
        if let Dragged::Promise { fallback, .. } = &mut d {
            *fallback = "image.jpeg".into();
        }
        let b = build_offer(vec![d]);
        let rxs: Vec<_> = b.sources.iter().map(|s| pending(s).clone()).collect();
        let [p] = <[Promise; 1]>::try_from(b.promises).ok().unwrap();
        let reading = Reading::new(5, root.clone(), p.files);
        let got = |name: &str| {
            let path = recv.join(name);
            std::fs::write(&path, name).unwrap();
            path
        };
        // Chrome's image, nameless before the drop.
        reading.read(got("images.jpg"), true);
        reading.read(got("b.pdf"), true);
        reading.read(got("c_.pdf"), true);
        // A second "b.pdf", which the source had to name another way.
        reading.read(got("b 2.pdf"), true);
        let served = |item: usize, name: &str| item_dir(&root, 5, item).join(name);
        let want = [
            (served(0, "image.jpeg"), "images.jpg"),
            (served(1, "b.pdf"), "b.pdf"),
            (served(2, "c_.pdf"), "c_.pdf"),
            (served(3, "b.pdf"), "b 2.pdf"),
        ];
        for (rx, (path, bytes)) in rxs.iter().zip(want) {
            assert_eq!(*rx.borrow(), Some(Ok(path.clone())));
            assert_eq!(std::fs::read(&path).unwrap(), bytes.as_bytes());
        }
        assert!(
            std::fs::read_dir(&recv).unwrap().next().is_none(),
            "every file moved out"
        );
    }

    #[test]
    fn a_promised_file_that_cant_take_its_name_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let recv = root.join("5").join(".recv-0");
        std::fs::create_dir_all(&recv).unwrap();
        std::fs::create_dir_all(item_dir(&root, 5, 0)).unwrap();
        std::fs::write(item_dir(&root, 5, 0).join("a.pdf"), b"other").unwrap();
        std::fs::write(recv.join("x.pdf"), b"x").unwrap();
        let b = build_offer(vec![promise(0, Some(&["a.pdf"]))]);
        let rx = pending(&b.sources[0]).clone();
        let [p] = <[Promise; 1]>::try_from(b.promises).ok().unwrap();
        Reading::new(5, root.clone(), p.files).read(recv.join("x.pdf"), true);
        assert_eq!(
            *rx.borrow(),
            Some(Err(FailReason::Unreadable {
                name: "a.pdf".into()
            })),
            "never served under another name"
        );
        assert_eq!(
            std::fs::read(item_dir(&root, 5, 0).join("a.pdf")).unwrap(),
            b"other",
            "never overwrites"
        );
    }

    #[test]
    fn promises_are_received_apart_and_served_per_item() {
        let (s, _) = shared();
        assert_eq!(s.receive_dir(42, 3), Path::new("/out/42/.recv-3"));
        assert_eq!(item_dir(&s.root, 42, 3), Path::new("/out/42/3"));
    }
}
