//! Drags arriving from the PC (R47): the PC's file drag rides onto the Mac
//! and becomes a Mac drag session that drops like a local file.
//!
//! - `prepare` (cj-inject, at `BeginDrag`): a placeholder per item in
//!   `~/Library/Caches/CursorJump/incoming/<drag>/`, an empty file or
//!   folder under the item's sanitized name, unique within the drag; an
//!   8×8 pt panel under the cursor; and a `Fetch` per item into its own
//!   staging folder under `incoming/<drag>/.fetch/` (prefetch). A fetched
//!   item is renamed over its placeholder.
//! - cj-inject then posts the left-down, which lands on the panel; its
//!   view's `mouseDown:` starts the session with one dragging item per
//!   item. Each item's writer is a file promise (Finder, the desktop and
//!   WhatsApp take it; the writer waits for the item, then copies it to
//!   the promise's URL on the promise's own queue) that also offers
//!   `public.file-url`, the placeholder (Chromium targets read that, at
//!   the drop, so the drop must wait until the item is in place).
//! - At the PC's left-up, `may_release` says whether the up may go out now:
//!   every item in place, or the target takes promises
//!   ([`PROMISE_TAKERS`]). Otherwise cj-inject holds it until `Ready`.
//! - `draggingSession:endedAtPoint:operation:` ends it (`Ended`). A drop
//!   whose promise writes still wait is dropped; one that ended without a
//!   drop stops its fetches, as does `cancel` (going home, Esc, ✕).
//! - A drag's folder stays after the drop: targets (WhatsApp, Gmail) read
//!   it later. Only the startup and hourly sweep removes it (R37a).
//!
//! AppKit objects live on the main thread (`on_main`, `on_main_sync`);
//! the views and panels are kept in a main-thread list, so none is ever
//! released elsewhere. The rest is shared through `Arc`s, behind locks.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use block2::DynBlock;
use cj_core::dnd::{DragId, DragItem, FailReason, ItemKind, sanitize_component};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2::{define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem,
    NSDraggingSession, NSDraggingSource, NSEvent, NSFilePromiseProvider,
    NSFilePromiseProviderDelegate, NSPanel, NSPasteboard, NSPasteboardTypeFileURL,
    NSPasteboardWritingOptions, NSResponder, NSRunningApplication, NSStatusWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGDisplayBounds, CGMainDisplayID, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowOwnerPID,
};
use objc2_foundation::{
    NSArray, NSCopying, NSDictionary, NSError, NSNumber, NSOperationQueue, NSPoint, NSRect, NSSize,
    NSString, NSURL, ns_string,
};
use objc2_uniform_type_identifiers::UTType;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};
use unicode_normalization::UnicodeNormalization;

use super::{on_main, on_main_sync};
use crate::common::transfer::Dest;
use crate::mac::control::{DragPort, InjectMsg};

/// The Escape key's virtual keycode: a posted Esc cancels a drag session.
pub const KVK_ESCAPE: u16 = 0x35;
/// The apps that take a file promise at the drop (spike 1b): the up need
/// not wait for the bytes over their windows.
pub const PROMISE_TAKERS: [&str; 2] = [FINDER, "net.whatsapp.WhatsApp"];
/// Finder, which also owns the desktop.
const FINDER: &str = "com.apple.finder";
/// How long `prepare` may hold cj-inject.
const PREPARE_TIMEOUT: Duration = Duration::from_millis(100);
/// The staging folder inside a drag's folder.
const STAGING: &str = ".fetch";
const DATA_UTT: &str = "public.data";
const FOLDER_UTT: &str = "public.folder";
/// The drag panel's side, in points: small, and nearly invisible.
const PANEL_SIZE: f64 = 8.0;
const PANEL_ALPHA: f64 = 0.01;
/// The drag image of one item, in points, and the offset between items.
const ICON_SIZE: f64 = 64.0;
const ICON_CASCADE: f64 = 6.0;
const MAX_CASCADE: usize = 4;
/// A closed drag's panel and promise objects are kept this long after its
/// session ended (while no promise write runs), for late promise writes.
const KEEP_AFTER_CLOSE: Duration = Duration::from_secs(600);
/// The longest file name APFS takes, in UTF-8 bytes.
const MAX_NAME_BYTES: usize = 255;

/// Numbers each `prepare`'s staging folder: a drag that comes back (it went
/// home and returned) never fetches where an old fetch may still write.
static ATTEMPT: AtomicU64 = AtomicU64::new(1);

/// What the drag source tells the app (Task 17).
#[derive(Debug)]
pub enum MacDndEv {
    /// The drag's session ended here, on a target that took it (`dropped`)
    /// or not. Exactly one per `prepare`d drag, a failed `prepare`
    /// included. A drop whose promise writes still wait is dropped: the
    /// outcome comes from the fetches (`incoming_outcome`).
    Ended { drag: DragId, dropped: bool },
    /// Fetch item `item` of `drag` into `dest`, a staging folder
    /// (`Dest::InDir`), and reply with the received path or why it failed.
    /// One per item, at `prepare`. A reply dropped unanswered fails the
    /// item. The source drops its receiver when the drag goes home or is
    /// cancelled: stop the fetch when `reply.closed()` resolves.
    Fetch {
        drag: DragId,
        item: u32,
        dest: Dest,
        reply: oneshot::Sender<Result<PathBuf, FailReason>>,
    },
    /// The user let go while the drag rode here, at `at` (global points,
    /// origin top left of the main display): R49, the pill starts now.
    #[cfg_attr(
        debug_assertions,
        expect(dead_code, reason = "Task 17 starts the pill from it")
    )]
    Released { drag: DragId, at: (f64, f64) },
    /// Every item is in place.
    Ready { drag: DragId },
    /// Item `item` failed; cj-inject cancels the drag.
    #[cfg_attr(
        all(debug_assertions, not(test)),
        expect(dead_code, reason = "Task 17 tells the pill why")
    )]
    Failed {
        drag: DragId,
        item: u32,
        reason: FailReason,
    },
}

impl MacDndEv {
    /// What cj-inject must be told of this event: Task 17's events
    /// callback sends it on the injection thread's channel.
    pub fn inject_msg(&self) -> Option<InjectMsg> {
        match *self {
            MacDndEv::Ended { drag, .. } => Some(InjectMsg::DragEnded { drag }),
            MacDndEv::Ready { drag } => Some(InjectMsg::DragReady { drag }),
            MacDndEv::Failed { drag, .. } => Some(InjectMsg::DragFailed { drag }),
            MacDndEv::Fetch { .. } | MacDndEv::Released { .. } => None,
        }
    }
}

/// `~/Library/Caches/CursorJump/incoming`: each drag from the PC has a
/// folder here, named after its id. The sweep (R37a) cleans it.
pub fn incoming_dir() -> PathBuf {
    let caches = std::env::var_os("HOME").map_or_else(std::env::temp_dir, |home| {
        PathBuf::from(home).join("Library").join("Caches")
    });
    caches.join("CursorJump").join("incoming")
}

// ------------------------------------------------------------ names

/// The UTType identifier for a file named `name`, from its extension:
/// `public.data` when there is none or the system doesn't know it.
fn utt_for_name(name: &str) -> String {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext.is_empty() {
        return DATA_UTT.into();
    }
    UTType::typeWithFilenameExtension(&NSString::from_str(&ext.to_lowercase()))
        .filter(|t| !t.isDynamic())
        .map_or_else(|| DATA_UTT.into(), |t| t.identifier().to_string())
}

/// A name as APFS compares it (R24): NFC, lowercase.
fn fold(name: &str) -> String {
    name.nfc().collect::<String>().to_lowercase()
}

/// `name` with ` n` before its extension (a file's) or at its end, cut to
/// fit [`MAX_NAME_BYTES`].
fn numbered(name: &str, n: u32, is_dir: bool) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if !is_dir && dot > 0 && dot + 1 < name.len() => name.split_at(dot),
        _ => (name, ""),
    };
    let suffix = format!(" {n}");
    let mut cut = stem
        .len()
        .min(MAX_NAME_BYTES.saturating_sub(suffix.len() + ext.len()));
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}{ext}", &stem[..cut])
}

/// Each item's placeholder name: sanitized, and unique within the drag as
/// APFS compares names ("a 2.txt" after "a.txt"); never the staging
/// folder's.
fn placeholder_names(items: &[DragItem]) -> Vec<String> {
    let mut taken: HashSet<String> = HashSet::from([fold(STAGING)]);
    items
        .iter()
        .map(|item| {
            let base = sanitize_component(&item.name);
            let is_dir = item.kind == ItemKind::Dir;
            let mut name = base.clone();
            let mut n = 2;
            while !taken.insert(fold(&name)) {
                name = numbered(&base, n, is_dir);
                n += 1;
            }
            name
        })
        .collect()
}

/// Where an item waits.
#[derive(Debug, Clone, PartialEq)]
struct Slot {
    /// The placeholder's file name, which the promise offers too.
    name: String,
    placeholder: PathBuf,
    /// The folder its fetch receives into.
    staging: PathBuf,
    is_dir: bool,
    /// Its UTType identifier.
    uti: String,
}

/// The slots of `items` in drag folder `dir`, staging under `staging`.
fn slots(dir: &Path, staging: &Path, items: &[DragItem]) -> Vec<Slot> {
    placeholder_names(items)
        .into_iter()
        .zip(items)
        .enumerate()
        .map(|(i, (name, item))| {
            let is_dir = item.kind == ItemKind::Dir;
            Slot {
                placeholder: dir.join(&name),
                staging: staging.join(i.to_string()),
                is_dir,
                uti: if is_dir {
                    FOLDER_UTT.into()
                } else {
                    utt_for_name(&name)
                },
                name,
            }
        })
        .collect()
}

/// Creates the empty placeholders and the staging folders. A placeholder
/// left by an earlier crossing of the same drag is replaced.
fn make_placeholders(slots: &[Slot]) -> io::Result<()> {
    for s in slots {
        match fs::symlink_metadata(&s.placeholder) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&s.placeholder)?,
            Ok(_) => fs::remove_file(&s.placeholder)?,
            Err(_) => {}
        }
        if let Some(parent) = s.placeholder.parent() {
            fs::create_dir_all(parent)?;
        }
        if s.is_dir {
            fs::create_dir(&s.placeholder)?;
        } else {
            fs::File::create_new(&s.placeholder)?;
        }
        fs::create_dir_all(&s.staging)?;
    }
    Ok(())
}

/// Moves a fetched item over its placeholder: rename(2) replaces an empty
/// file, or an empty folder with a folder. If the kinds differ, the
/// placeholder makes room first.
fn put_in_place(fetched: &Path, placeholder: &Path) -> io::Result<()> {
    if fs::rename(fetched, placeholder).is_ok() {
        return Ok(());
    }
    match fs::symlink_metadata(placeholder) {
        Ok(m) if m.is_dir() => fs::remove_dir(placeholder)?,
        Ok(_) => fs::remove_file(placeholder)?,
        Err(_) => {}
    }
    fs::rename(fetched, placeholder)
}

/// Copies a completed item (a file, or a folder's tree) to `dest`, which
/// must not exist yet: a promise never overwrites (R20). On APFS a file
/// copy is a clone.
fn copy_item(src: &Path, dest: &Path) -> io::Result<()> {
    if fs::symlink_metadata(dest).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists", dest.display()),
        ));
    }
    if fs::metadata(src)?.is_dir() {
        copy_tree(src, dest)
    } else {
        fs::copy(src, dest).map(drop)
    }
}

fn copy_tree(src: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------ the target

/// An on-screen window, as the window list describes it.
#[derive(Debug, Clone, PartialEq)]
struct WinInfo {
    layer: i64,
    pid: i32,
    /// x, y, width, height in global points (origin top left).
    bounds: (f64, f64, f64, f64),
}

/// The windows under the cursor and their apps; a trait so the decision
/// can be tested.
trait Targets: Send + Sync {
    /// On-screen windows, front to back; None if the list can't be had.
    fn windows(&self) -> Option<Vec<WinInfo>>;
    fn bundle_id(&self, pid: i32) -> Option<String>;
}

/// The app of the frontmost normal window (layer 0, not ours) holding
/// `at`; None over the desktop.
fn owner_at(windows: &[WinInfo], at: (f64, f64), own_pid: i32) -> Option<i32> {
    windows
        .iter()
        .find(|w| {
            let (x, y, wd, ht) = w.bounds;
            w.layer == 0
                && w.pid != own_pid
                && at.0 >= x
                && at.0 < x + wd
                && at.1 >= y
                && at.1 < y + ht
        })
        .map(|w| w.pid)
}

/// Whether the app under `at` takes file promises (the desktop is
/// Finder's). Unsure means no: the drop then waits for the bytes, which
/// is slower but never wrong.
fn promise_taker_at(at: (f64, f64), targets: &dyn Targets, own_pid: i32) -> bool {
    let Some(windows) = targets.windows() else {
        return false;
    };
    let bundle = match owner_at(&windows, at, own_pid) {
        Some(pid) => targets.bundle_id(pid),
        None => Some(FINDER.to_string()),
    };
    debug!(?bundle, "the drop target's app");
    bundle.is_some_and(|b| PROMISE_TAKERS.contains(&b.as_str()))
}

/// The window server's list and the running apps.
struct SystemTargets;

impl Targets for SystemTargets {
    fn windows(&self) -> Option<Vec<WinInfo>> {
        let options =
            CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
        let list = CGWindowListCopyWindowInfo(options, kCGNullWindowID)?;
        // SAFETY: CFArray is toll-free bridged to NSArray, and the window
        // list holds one CFDictionary (NSDictionary) per window; `list`
        // outlives the borrow.
        let list: &NSArray<NSDictionary<NSString, AnyObject>> =
            unsafe { CFRetained::as_ptr(&list).cast().as_ref() };
        // SAFETY: CFString is toll-free bridged to NSString; the keys are
        // immutable statics.
        let key = |k: &'static objc2_core_foundation::CFString| -> &'static NSString {
            unsafe { &*(k as *const objc2_core_foundation::CFString).cast::<NSString>() }
        };
        // SAFETY: reading extern statics the framework defines.
        let (layer_key, pid_key, bounds_key) = unsafe {
            (
                key(kCGWindowLayer),
                key(kCGWindowOwnerPID),
                key(kCGWindowBounds),
            )
        };
        let number = |d: &NSDictionary<NSString, AnyObject>, k: &NSString| {
            d.objectForKey(k)?.downcast::<NSNumber>().ok()
        };
        let windows = list
            .iter()
            .filter_map(|w| {
                let bounds = w
                    .objectForKey(bounds_key)?
                    .downcast::<NSDictionary>()
                    .ok()?;
                // SAFETY: a window's bounds are a dictionary of NSNumbers
                // keyed by strings; the values are checked as they are read.
                let bounds: &NSDictionary<NSString, AnyObject> =
                    unsafe { &*Retained::as_ptr(&bounds).cast() };
                let f = |k: &NSString| number(bounds, k).map(|n| n.doubleValue());
                Some(WinInfo {
                    layer: number(&w, layer_key)?.integerValue() as i64,
                    pid: number(&w, pid_key)?.intValue(),
                    bounds: (
                        f(ns_string!("X"))?,
                        f(ns_string!("Y"))?,
                        f(ns_string!("Width"))?,
                        f(ns_string!("Height"))?,
                    ),
                })
            })
            .collect();
        Some(windows)
    }

    fn bundle_id(&self, pid: i32) -> Option<String> {
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?
            .bundleIdentifier()
            .map(|b| b.to_string())
    }
}

// ------------------------------------------------------------ one drag

#[derive(Debug, Clone, PartialEq)]
enum Item {
    Fetching,
    Complete,
    Failed(FailReason),
}

#[derive(Debug)]
struct DragState {
    items: Vec<Item>,
    /// Gone home, cancelled or ended undropped: fetches stop, waiting
    /// promise writes fail.
    cancelled: bool,
    /// Its session started (the posted down reached the panel).
    began: bool,
    /// `Ended` was sent.
    ended: bool,
}

/// One drag from the PC, shared by cj-inject, the main thread, the
/// prefetch thread and the promise writers.
struct Drag {
    id: DragId,
    slots: Vec<Slot>,
    /// This crossing's staging folder.
    staging: PathBuf,
    state: Mutex<DragState>,
    changed: Condvar,
    /// Stops the prefetch thread, dropping the replies' receivers.
    stop: Notify,
    /// Promise writes running now.
    writes: AtomicUsize,
}

impl Drag {
    fn new(id: DragId, slots: Vec<Slot>, staging: PathBuf) -> Drag {
        Drag {
            id,
            state: Mutex::new(DragState {
                items: vec![Item::Fetching; slots.len()],
                cancelled: false,
                began: false,
                ended: false,
            }),
            slots,
            staging,
            changed: Condvar::new(),
            stop: Notify::new(),
            writes: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, DragState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cancel(&self) {
        self.lock().cancelled = true;
        self.changed.notify_all();
        self.stop.notify_one();
    }

    /// Item `i`'s fetch finished: puts it in place. Returns what to tell
    /// the app (nothing once the drag is cancelled).
    fn resolve(&self, i: usize, result: Result<PathBuf, FailReason>) -> Vec<MacDndEv> {
        let Some(slot) = self.slots.get(i) else {
            return Vec::new();
        };
        let outcome = result.and_then(|path| {
            put_in_place(&path, &slot.placeholder).map_err(|e| {
                warn!(
                    drag = self.id,
                    item = i,
                    "placing a fetched item failed: {e}"
                );
                FailReason::Unwritable
            })
        });
        let mut st = self.lock();
        if st.items[i] != Item::Fetching {
            return Vec::new();
        }
        st.items[i] = match &outcome {
            Ok(()) => Item::Complete,
            Err(reason) => Item::Failed(reason.clone()),
        };
        let all = st.items.iter().all(|x| *x == Item::Complete);
        let cancelled = st.cancelled;
        drop(st);
        self.changed.notify_all();
        let (drag, item) = (self.id, i as u32);
        match outcome {
            _ if cancelled => Vec::new(),
            Err(reason) => {
                warn!(
                    drag,
                    item,
                    ?reason,
                    "an item of the drag from the PC failed"
                );
                vec![MacDndEv::Failed { drag, item, reason }]
            }
            Ok(()) if all => {
                info!(drag, "every item of the drag from the PC is here");
                // Only empty folders are left there.
                let _ = fs::remove_dir_all(&self.staging);
                vec![MacDndEv::Ready { drag }]
            }
            Ok(()) => Vec::new(),
        }
    }

    /// Waits until item `i` is in place, or why it never will be.
    fn wait_item(&self, i: usize) -> Result<(), FailReason> {
        let mut st = self.lock();
        loop {
            match st.items.get(i) {
                Some(Item::Complete) => return Ok(()),
                Some(Item::Failed(reason)) => return Err(reason.clone()),
                None => return Err(FailReason::Cancelled),
                Some(Item::Fetching) if st.cancelled => return Err(FailReason::Cancelled),
                Some(Item::Fetching) => {
                    st = self
                        .changed
                        .wait(st)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }

    fn all_complete(&self) -> bool {
        self.lock().items.iter().all(|x| *x == Item::Complete)
    }
}

/// Counts a running promise write.
struct Writing<'a>(&'a AtomicUsize);

impl<'a> Writing<'a> {
    fn new(n: &'a AtomicUsize) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Writing(n)
    }
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A promise write: waits for the item, then copies it to `dest`.
fn write_promise(drag: &Drag, item: usize, dest: &Path) -> Result<(), String> {
    let _writing = Writing::new(&drag.writes);
    drag.wait_item(item)
        .map_err(|r| format!("the item never arrived: {r:?}"))?;
    let slot = drag.slots.get(item).ok_or("no such item")?;
    copy_item(&slot.placeholder, dest).map_err(|e| e.to_string())
}

// ------------------------------------------------------------ the source

/// What the source's handles share.
struct Inner {
    events: Box<dyn Fn(MacDndEv) + Send + Sync>,
    root: PathBuf,
    targets: Box<dyn Targets>,
    drags: Mutex<HashMap<DragId, Arc<Drag>>>,
}

impl Inner {
    fn emit(&self, ev: MacDndEv) {
        if catch_unwind(AssertUnwindSafe(|| (self.events)(ev))).is_err() {
            warn!("the drag source's events callback panicked");
        }
    }

    fn live(&self) -> MutexGuard<'_, HashMap<DragId, Arc<Drag>>> {
        self.drags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, drag: DragId) -> Option<Arc<Drag>> {
        self.live().get(&drag).cloned()
    }

    /// Sends `Ended`, once per drag; one that ended undropped stops its
    /// fetches.
    fn end(&self, drag: &Arc<Drag>, dropped: bool) {
        {
            let mut st = drag.lock();
            if st.ended {
                return;
            }
            st.ended = true;
        }
        {
            let mut live = self.live();
            if live.get(&drag.id).is_some_and(|d| Arc::ptr_eq(d, drag)) {
                live.remove(&drag.id);
            }
        }
        if !dropped {
            drag.cancel();
        }
        info!(drag = drag.id, dropped, "the drag from the PC ended here");
        self.emit(MacDndEv::Ended {
            drag: drag.id,
            dropped,
        });
    }
}

/// The Mac's side of drags from the PC; clones share it. Every method may
/// be called from any thread; `prepare` waits for the main thread.
#[derive(Clone)]
pub struct DragSource {
    inner: Arc<Inner>,
}

impl DragSource {
    /// `events` gets the source's events, on any thread; it must not block.
    pub fn new(events: impl Fn(MacDndEv) + Send + Sync + 'static) -> DragSource {
        DragSource::with(events, incoming_dir(), Box::new(SystemTargets))
    }

    fn with(
        events: impl Fn(MacDndEv) + Send + Sync + 'static,
        root: PathBuf,
        targets: Box<dyn Targets>,
    ) -> DragSource {
        DragSource {
            inner: Arc::new(Inner {
                events: Box::new(events),
                root,
                targets,
                drags: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Arms `drag` under `at` (global points): its placeholders, the drag
    /// panel there, and a `Fetch` per item. Within about 100 ms. False if
    /// it can't (no items, the disk, the main thread too slow): `Ended`
    /// (not dropped) then follows, and nothing must be pressed.
    pub fn prepare(&self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> bool {
        let started = Instant::now();
        let fail = |why: &str| {
            warn!(drag, "the drag from the PC can't start here: {why}");
            self.inner.emit(MacDndEv::Ended {
                drag,
                dropped: false,
            });
            false
        };
        if items.is_empty() {
            return fail("no items were offered for it");
        }
        let dir = self.inner.root.join(drag.to_string());
        let staging = dir
            .join(STAGING)
            .join(ATTEMPT.fetch_add(1, Ordering::Relaxed).to_string());
        let slots = slots(&dir, &staging, &items);
        if let Err(e) = make_placeholders(&slots) {
            return fail(&format!("creating its placeholders: {e}"));
        }
        let runtime = match tokio::runtime::Builder::new_current_thread().build() {
            Ok(rt) => rt,
            Err(e) => return fail(&format!("starting its prefetch: {e}")),
        };
        let d = Arc::new(Drag::new(drag, slots, staging));
        if let Some(old) = self.inner.live().insert(drag, d.clone()) {
            old.cancel();
        }
        let (inner, armed) = (self.inner.clone(), d.clone());
        let shown = on_main_sync(PREPARE_TIMEOUT, move |mtm| arm(mtm, &inner, &armed, at));
        if shown != Some(true) {
            self.cancel(drag);
            warn!(drag, ?shown, "the drag panel did not come up in time");
            return false;
        }
        let mut replies = Vec::with_capacity(d.slots.len());
        let fetches: Vec<MacDndEv> = d
            .slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let (reply, rx) = oneshot::channel();
                replies.push(rx);
                MacDndEv::Fetch {
                    drag,
                    item: i as u32,
                    dest: Dest::InDir(slot.staging.clone()),
                    reply,
                }
            })
            .collect();
        if let Err(e) = spawn_prefetch(runtime, self.inner.clone(), d.clone(), replies) {
            warn!(drag, "starting the prefetch thread failed: {e}");
            self.cancel(drag);
            return false;
        }
        for ev in fetches {
            self.inner.emit(ev);
        }
        info!(
            drag,
            items = d.slots.len(),
            ms = started.elapsed().as_millis() as u64,
            "the drag from the PC is armed"
        );
        true
    }

    /// The user let go while `drag` rode here, at `at`.
    pub fn released(&self, drag: DragId, at: (f64, f64)) {
        let Some(d) = self.inner.get(drag) else {
            return;
        };
        if d.lock().began {
            self.inner.emit(MacDndEv::Released { drag, at });
        } else {
            // No session: the press missed the panel. Nothing to drop.
            warn!(drag, "the drag from the PC never started here");
            self.cancel(drag);
        }
    }

    /// Whether the up may be posted now, over `at`: every item is in
    /// place, or the app there takes file promises. True for a drag that
    /// is over, or never started a session.
    pub fn may_release(&self, drag: DragId, at: (f64, f64)) -> bool {
        let Some(d) = self.inner.get(drag) else {
            return true;
        };
        {
            let st = d.lock();
            if !st.began || st.ended || st.items.iter().all(|x| *x == Item::Complete) {
                return true;
            }
            if st.items.iter().any(|x| matches!(x, Item::Failed(_))) {
                return false; // cj-inject cancels it.
            }
        }
        promise_taker_at(at, &*self.inner.targets, std::process::id() as i32)
    }

    /// `drag` went home or was cancelled: its fetches stop (their replies'
    /// receivers are dropped), waiting promise writes fail and its panel
    /// goes. The session itself ends by the Esc cj-inject posts. Idempotent.
    pub fn cancel(&self, drag: DragId) {
        let Some(d) = self.inner.live().remove(&drag) else {
            return;
        };
        d.cancel();
        if !d.lock().began {
            // No session will end it.
            self.inner.end(&d, false);
            on_main(move |_| close_panel(drag));
        }
    }

    /// Whether every item of `drag` is in place (true once it is over).
    #[cfg(debug_assertions)]
    fn all_complete(&self, drag: DragId) -> bool {
        self.inner.get(drag).is_none_or(|d| d.all_complete())
    }
}

impl DragPort for DragSource {
    fn prepare(&mut self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> bool {
        DragSource::prepare(self, drag, items, at)
    }
    fn released(&mut self, drag: DragId, at: (f64, f64)) {
        DragSource::released(self, drag, at);
    }
    fn may_release(&mut self, drag: DragId, at: (f64, f64)) -> bool {
        DragSource::may_release(self, drag, at)
    }
    fn cancel(&mut self, drag: DragId) {
        DragSource::cancel(self, drag);
    }
}

/// Waits for the replies on a thread of its own, putting each item in
/// place as it comes, until all are in or the drag is cancelled (which
/// drops the receivers).
fn spawn_prefetch(
    runtime: tokio::runtime::Runtime,
    inner: Arc<Inner>,
    drag: Arc<Drag>,
    replies: Vec<oneshot::Receiver<Result<PathBuf, FailReason>>>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("cj-drag-prefetch".into())
        .spawn(move || {
            runtime.block_on(async {
                let mut waiting = JoinSet::new();
                for (i, rx) in replies.into_iter().enumerate() {
                    waiting.spawn(async move { (i, rx.await) });
                }
                loop {
                    tokio::select! {
                        () = drag.stop.notified() => break,
                        next = waiting.join_next() => match next {
                            Some(Ok((i, reply))) => {
                                let result = reply.unwrap_or(Err(FailReason::Cancelled));
                                for ev in drag.resolve(i, result) {
                                    inner.emit(ev);
                                }
                            }
                            Some(Err(e)) => warn!(drag = drag.id, "a prefetch wait failed: {e}"),
                            None => break,
                        },
                    }
                }
            });
            debug!(drag = drag.id, "prefetch thread done");
        })
        .map(drop)
}

// ------------------------------------------------------------ AppKit

/// A drag's panel and the objects its session uses, kept on the main
/// thread until well after the session ended (a promise's delegate is
/// held weakly by AppKit).
struct Kept {
    drag: Arc<Drag>,
    panel: Retained<NSPanel>,
    _view: Retained<DragView>,
    closed_at: Option<Instant>,
}

thread_local! {
    /// Only ever touched on the main thread.
    static KEPT: RefCell<Vec<Kept>> = const { RefCell::new(Vec::new()) };
}

/// Hides `drag`'s panel; its objects stay a while for late promise writes.
fn close_panel(drag: DragId) {
    KEPT.with(|k| {
        let Ok(mut kept) = k.try_borrow_mut() else {
            return;
        };
        for e in kept
            .iter_mut()
            .filter(|e| e.drag.id == drag && e.closed_at.is_none())
        {
            e.panel.orderOut(None);
            e.closed_at = Some(Instant::now());
        }
    });
}

/// Lets go of closed drags' objects once no promise write can need them.
fn prune() {
    let gone: Vec<Kept> = KEPT.with(|k| {
        let Ok(mut kept) = k.try_borrow_mut() else {
            return Vec::new();
        };
        let (gone, keep) = std::mem::take(&mut *kept).into_iter().partition(|e| {
            e.closed_at.is_some_and(|t| t.elapsed() >= KEEP_AFTER_CLOSE)
                && e.drag.writes.load(Ordering::SeqCst) == 0
        });
        *kept = keep;
        gone
    });
    drop(gone); // Outside the borrow.
}

/// Main thread: the panel under `at` with the drag's items armed.
fn arm(mtm: MainThreadMarker, inner: &Arc<Inner>, drag: &Arc<Drag>, at: (f64, f64)) -> bool {
    if drag.lock().cancelled {
        return false; // `prepare` gave up waiting.
    }
    prune();
    let armed = drag
        .slots
        .iter()
        .enumerate()
        .map(|(i, slot)| Armed::new(drag, i, slot))
        .collect();
    let view = DragView::new(
        mtm,
        ViewIvars {
            inner: inner.clone(),
            drag: drag.clone(),
            armed,
            started: Cell::new(false),
        },
    );
    let panel = drag_panel(mtm, at, &view);
    KEPT.with(|k| {
        k.borrow_mut().push(Kept {
            drag: drag.clone(),
            panel,
            _view: view,
            closed_at: None,
        });
    });
    true
}

/// An 8×8 pt borderless, non-activating, nearly invisible panel centred
/// on `at` (global points), at the status level on every Space.
fn drag_panel(mtm: MainThreadMarker, at: (f64, f64), view: &NSView) -> Retained<NSPanel> {
    let main_h = CGDisplayBounds(CGMainDisplayID()).size.height;
    let half = PANEL_SIZE / 2.0;
    let frame = NSRect::new(
        NSPoint::new(at.0 - half, main_h - at.1 - half),
        NSSize::new(PANEL_SIZE, PANEL_SIZE),
    );
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: `Kept` owns the panel; closing must not release it.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    panel.setLevel(NSStatusWindowLevel);
    panel.setHidesOnDeactivate(false);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.5, PANEL_ALPHA)));
    panel.setIgnoresMouseEvents(false);
    panel.setContentView(Some(view));
    panel.orderFrontRegardless();
    panel
}

/// Run `f`, catching and logging a panic: for code AppKit calls, which
/// must not be unwound into.
fn guarded(what: &str, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        warn!("drag source: {what} panicked");
    }
}

/// One item's pasteboard writer and its delegate.
struct Armed {
    provider: Retained<PromiseProvider>,
    _delegate: Retained<PromiseDelegate>,
    uti: String,
}

impl Armed {
    fn new(drag: &Arc<Drag>, item: usize, slot: &Slot) -> Armed {
        let delegate = PromiseDelegate::new(drag.clone(), item);
        let provider = PromiseProvider::new(slot, &delegate);
        Armed {
            provider,
            _delegate: delegate,
            uti: slot.uti.clone(),
        }
    }
}

struct ViewIvars {
    inner: Arc<Inner>,
    drag: Arc<Drag>,
    armed: Vec<Armed>,
    /// The session started (only the first down counts).
    started: Cell<bool>,
}

define_class!(
    /// The drag panel's view: the posted down starts the session here.
    // SAFETY: NSView has no subclassing requirements; DragView has no Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CJIncomingDragView"]
    #[ivars = ViewIvars]
    struct DragView;

    unsafe impl NSObjectProtocol for DragView {}

    impl DragView {
        /// The down reaches us though CursorJump is never the active app.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            guarded("mouseDown:", || self.start(event));
        }
    }

    unsafe impl NSDraggingSource for DragView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _at: NSPoint, op: NSDragOperation) {
            guarded("endedAtPoint:", || {
                let iv = self.ivars();
                iv.inner.end(&iv.drag, op != NSDragOperation::None);
                close_panel(iv.drag.id);
            });
        }
    }
);

impl DragView {
    fn new(mtm: MainThreadMarker, ivars: ViewIvars) -> Retained<DragView> {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_SIZE, PANEL_SIZE));
        let this = DragView::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Starts the session with one dragging item per item, each with its
    /// type's icon, fanned out a little.
    fn start(&self, event: &NSEvent) {
        let iv = self.ivars();
        if iv.started.replace(true) || iv.drag.lock().cancelled {
            return;
        }
        let at = self.convertPoint_fromView(event.locationInWindow(), None);
        let items: Vec<Retained<NSDraggingItem>> = iv
            .armed
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let writer: &NSFilePromiseProvider = &a.provider;
                let item = NSDraggingItem::initWithPasteboardWriter(
                    NSDraggingItem::alloc(),
                    ProtocolObject::from_ref(writer),
                );
                let k = i.min(MAX_CASCADE) as f64 * ICON_CASCADE;
                let half = ICON_SIZE / 2.0;
                let frame = NSRect::new(
                    NSPoint::new(at.x - half + k, at.y - half - k),
                    NSSize::new(ICON_SIZE, ICON_SIZE),
                );
                let ut = UTType::typeWithIdentifier(&NSString::from_str(&a.uti))
                    .or_else(|| UTType::typeWithIdentifier(ns_string!("public.data")));
                match ut {
                    Some(ut) => {
                        let icon = NSWorkspace::sharedWorkspace().iconForContentType(&ut);
                        icon.setSize(NSSize::new(ICON_SIZE, ICON_SIZE));
                        let contents: &AnyObject = &icon;
                        // SAFETY: an NSImage is a valid dragging-frame content.
                        unsafe { item.setDraggingFrame_contents(frame, Some(contents)) };
                    }
                    None => item.setDraggingFrame(frame),
                }
                item
            })
            .collect();
        let items = NSArray::from_retained_slice(&items);
        let session = self.beginDraggingSessionWithItems_event_source(
            &items,
            event,
            ProtocolObject::from_ref(self),
        );
        session.setAnimatesToStartingPositionsOnCancelOrFail(false);
        iv.drag.lock().began = true;
        info!(drag = iv.drag.id, "the drag session from the PC began");
    }
}

/// A file promise that also offers `public.file-url`: the placeholder.
struct ProviderIvars {
    /// The placeholder's file URL, as a string.
    url: String,
}

define_class!(
    // SAFETY: NSFilePromiseProvider may be subclassed (AppKit documents
    // overriding its NSPasteboardWriting methods to add types); no Drop.
    #[unsafe(super(NSFilePromiseProvider, NSObject))]
    #[name = "CJIncomingPromiseProvider"]
    #[ivars = ProviderIvars]
    struct PromiseProvider;

    unsafe impl NSObjectProtocol for PromiseProvider {}

    impl PromiseProvider {
        #[unsafe(method_id(writableTypesForPasteboard:))]
        fn writable_types(&self, pasteboard: &NSPasteboard) -> Retained<NSArray<NSString>> {
            // SAFETY: the superclass's own method with the same arguments.
            let base: Retained<NSArray<NSString>> =
                unsafe { msg_send![super(self), writableTypesForPasteboard: pasteboard] };
            let mut types = base.to_vec();
            types.push(file_url_type());
            NSArray::from_retained_slice(&types)
        }

        #[unsafe(method(writingOptionsForType:pasteboard:))]
        fn writing_options(
            &self,
            ty: &NSString,
            pasteboard: &NSPasteboard,
        ) -> NSPasteboardWritingOptions {
            self.options_for(ty, pasteboard)
        }

        #[unsafe(method_id(pasteboardPropertyListForType:))]
        fn property_list(&self, ty: &NSString) -> Option<Retained<AnyObject>> {
            self.value_for(ty)
        }
    }
);

impl PromiseProvider {
    /// The file URL is written at once; the promise's own types as the
    /// superclass says.
    fn options_for(&self, ty: &NSString, pasteboard: &NSPasteboard) -> NSPasteboardWritingOptions {
        if *ty == *file_url_type() {
            return NSPasteboardWritingOptions(0);
        }
        // SAFETY: the superclass's own method with the same arguments.
        unsafe { msg_send![super(self), writingOptionsForType: ty, pasteboard: pasteboard] }
    }

    /// The placeholder's URL for `public.file-url`; the promise's own
    /// types from the superclass.
    fn value_for(&self, ty: &NSString) -> Option<Retained<AnyObject>> {
        if *ty == *file_url_type() {
            let url = NSString::from_str(&self.ivars().url);
            return Some(url.into_super().into_super());
        }
        // SAFETY: the superclass's own method with the same argument.
        unsafe { msg_send![super(self), pasteboardPropertyListForType: ty] }
    }

    fn new(slot: &Slot, delegate: &PromiseDelegate) -> Retained<PromiseProvider> {
        let path = NSString::from_str(&slot.placeholder.to_string_lossy());
        let url = NSURL::fileURLWithPath_isDirectory(&path, slot.is_dir)
            .absoluteString()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let this = PromiseProvider::alloc().set_ivars(ProviderIvars { url });
        let uti = NSString::from_str(&slot.uti);
        let delegate = ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(delegate);
        // SAFETY: NSFilePromiseProvider's designated initializer.
        unsafe { msg_send![super(this), initWithFileType: &*uti, delegate: delegate] }
    }
}

fn file_url_type() -> Retained<NSString> {
    // SAFETY: a static AppKit string.
    unsafe { NSPasteboardTypeFileURL }.copy()
}

struct DelegateIvars {
    drag: Arc<Drag>,
    item: usize,
    /// The promise's writes run here, never on the main thread.
    queue: Retained<NSOperationQueue>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop.
    #[unsafe(super(NSObject))]
    #[name = "CJIncomingPromiseDelegate"]
    #[ivars = DelegateIvars]
    struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(
            &self,
            _provider: &NSFilePromiseProvider,
            _file_type: &NSString,
        ) -> Retained<NSString> {
            let iv = self.ivars();
            let name = iv.drag.slots.get(iv.item).map_or("_", |s| s.name.as_str());
            NSString::from_str(name)
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_to(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            completion: &DynBlock<dyn Fn(*mut NSError)>,
        ) {
            let iv = self.ivars();
            let (drag, item) = (iv.drag.id, iv.item);
            let dest = url.path().map(|p| PathBuf::from(p.to_string()));
            let result = catch_unwind(AssertUnwindSafe(|| match &dest {
                Some(dest) => write_promise(&iv.drag, item, dest),
                None => Err("the promise's URL has no path".into()),
            }))
            .unwrap_or_else(|_| Err("the promise write panicked".into()));
            match &result {
                Ok(()) => info!(drag, item, ?dest, "promise written"),
                Err(e) => warn!(drag, item, ?dest, "promise write failed: {e}"),
            }
            let error = result
                .err()
                .map(|_| NSError::new(-1, ns_string!("CursorJump")));
            let error = error
                .as_ref()
                .map_or(std::ptr::null_mut(), |e| Retained::as_ptr(e).cast_mut());
            completion.call((error,));
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);

impl PromiseDelegate {
    fn new(drag: Arc<Drag>, item: usize) -> Retained<PromiseDelegate> {
        let queue = NSOperationQueue::new();
        queue.setName(Some(ns_string!("cj-promise-writes")));
        let this = PromiseDelegate::alloc().set_ivars(DelegateIvars { drag, item, queue });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

// ------------------------------------------------------------ smoke

/// How the smoke's release behaves.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SmokeMode {
    /// The real rule: at once over Finder, the desktop or WhatsApp, else held.
    Auto,
    /// Always at once: promise-only targets.
    Promise,
    /// Always held until the file is complete.
    Hold,
}

#[cfg(debug_assertions)]
impl SmokeMode {
    pub fn parse(s: Option<&str>) -> Option<SmokeMode> {
        match s {
            None | Some("auto") => Some(SmokeMode::Auto),
            Some("promise") => Some(SmokeMode::Promise),
            Some("hold") => Some(SmokeMode::Hold),
            Some(_) => None,
        }
    }
}

/// The real source, with the release rule the smoke asks for.
#[cfg(debug_assertions)]
struct SmokeDrags {
    source: DragSource,
    mode: SmokeMode,
}

#[cfg(debug_assertions)]
impl DragPort for SmokeDrags {
    fn prepare(&mut self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> bool {
        self.source.prepare(drag, items, at)
    }
    fn released(&mut self, drag: DragId, at: (f64, f64)) {
        self.source.released(drag, at);
    }
    fn may_release(&mut self, drag: DragId, at: (f64, f64)) -> bool {
        let may = match self.mode {
            SmokeMode::Auto => self.source.may_release(drag, at),
            SmokeMode::Promise => true,
            SmokeMode::Hold => self.source.all_complete(drag),
        };
        info!(?self.mode, may, "smoke: may the up go out now?");
        may
    }
    fn cancel(&mut self, drag: DragId) {
        self.source.cancel(drag);
    }
}

/// The plan's `smoke_drag_from_fake_pc`, for a person to run on the real
/// main thread: `cargo run -p cursorjump -- --smoke-drag [auto|promise|hold]`
/// (debug builds only; `tests/smoke_drag.rs` runs it as an `#[ignore]`d
/// test). It moves the real cursor and presses the left button.
///
/// 1. A 50 MB file is made in the temp folder.
/// 2. Five seconds to point at the drop target (a WhatsApp chat, a Gmail
///    compose window, a Finder folder, the desktop); then leave the mouse.
/// 3. A fake PC crossing at the left edge at the target's height carries
///    a drag of that file into the real injection thread and this source;
///    the fake `Fetch` streams the file over about 10 s.
/// 4. Posted moves drag it to the target, it hovers 1.5 s, then the PC's
///    up: posted at once (`promise`, or `auto` over Finder/WhatsApp), or
///    held with the jiggle until the file is complete (`hold`, or `auto`
///    elsewhere).
/// 5. It exits a few seconds after the session ended.
#[cfg(debug_assertions)]
pub fn run_smoke(mode: SmokeMode) -> anyhow::Result<()> {
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicBool;

    use anyhow::Context;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    use crate::mac::control::spawn_inject_thread;
    use crate::mac::inject::Injector;

    let mtm = MainThreadMarker::new().context("--smoke-drag must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let payload = smoke_payload().context("making the 50 MB file")?;
    info!(?mode, payload = %payload.display(), "smoke: drag from a fake PC");

    let inject: Arc<OnceLock<crossbeam_channel::Sender<InjectMsg>>> = Arc::new(OnceLock::new());
    let (ended_tx, ended_rx) = crossbeam_channel::unbounded::<bool>();
    let (to_inject, src) = (inject.clone(), payload.clone());
    let source = DragSource::new(move |ev| {
        if let Some(msg) = ev.inject_msg()
            && let Some(tx) = to_inject.get()
        {
            let _ = tx.send(msg);
        }
        match ev {
            MacDndEv::Fetch {
                drag,
                item,
                dest,
                reply,
            } => {
                info!(drag, item, ?dest, "smoke: fetch (fake, about 10 s)");
                smoke_fetch(src.clone(), dest, reply);
            }
            MacDndEv::Ended { drag, dropped } => {
                info!(drag, dropped, "smoke: ended");
                let _ = ended_tx.send(dropped);
            }
            other => info!(?other, "smoke: source event"),
        }
    });
    let tx = spawn_inject_thread(
        Injector::new()?,
        SmokeDrags { source, mode },
        Arc::new(AtomicBool::new(true)),
        |ev| info!(?ev, "smoke: injection thread event"),
    )?;
    let _ = inject.set(tx.clone());
    let size = fs::metadata(&payload)?.len();
    let name = payload
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::thread::Builder::new()
        .name("cj-drag-smoke".into())
        .spawn(move || {
            smoke_script(&tx, &ended_rx, name, size);
            on_main(|_| std::process::exit(0));
        })
        .context("spawn the smoke script")?;
    app.run();
    Ok(())
}

#[cfg(debug_assertions)]
fn smoke_payload() -> io::Result<PathBuf> {
    use std::io::Write;

    let dir = std::env::temp_dir().join("cursorjump-smoke-drag");
    fs::create_dir_all(&dir)?;
    let path = dir.join("smoke-50MB.bin");
    if fs::metadata(&path).is_ok_and(|m| m.len() == 50 << 20) {
        return Ok(path);
    }
    let mut f = fs::File::create(&path)?;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut buf = vec![0u8; 1 << 20];
    for _ in 0..50 {
        for chunk in buf.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
        }
        f.write_all(&buf)?;
    }
    Ok(path)
}

/// A fake fetch: copies `src` into `dest` in 20 parts over about 10 s.
#[cfg(debug_assertions)]
fn smoke_fetch(src: PathBuf, dest: Dest, reply: oneshot::Sender<Result<PathBuf, FailReason>>) {
    use std::io::Write;

    std::thread::spawn(move || {
        let Dest::InDir(dir) = dest else {
            return;
        };
        let out = dir.join(src.file_name().unwrap_or_default());
        let result = (|| -> io::Result<PathBuf> {
            fs::create_dir_all(&dir)?;
            let data = fs::read(&src)?;
            let mut f = fs::File::create_new(&out)?;
            for part in data.chunks(data.len().div_ceil(20).max(1)) {
                if reply.is_closed() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                f.write_all(part)?;
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(out.clone())
        })();
        let result = result.map_err(|e| {
            warn!("smoke: the fake fetch stopped: {e}");
            if e.kind() == io::ErrorKind::Interrupted {
                FailReason::Cancelled
            } else {
                FailReason::Unwritable
            }
        });
        let _ = reply.send(result);
    });
}

#[cfg(debug_assertions)]
fn smoke_script(
    tx: &crossbeam_channel::Sender<InjectMsg>,
    ended: &crossbeam_channel::Receiver<bool>,
    name: String,
    size: u64,
) {
    use cj_core::proto::{MouseButton, Side};

    use crate::mac::control::Input;

    const DRAG: DragId = 0x5EED;
    let sleep = |ms| std::thread::sleep(Duration::from_millis(ms));
    let send = |input| {
        let _ = tx.send(InjectMsg::Input { sid: 1, input });
    };
    for s in (1..=5).rev() {
        info!("smoke: point at the drop target, then leave the mouse alone: {s}");
        sleep(1000);
    }
    let target = objc2_core_graphics::CGEvent::new(None).map_or((400.0, 400.0), |ev| {
        let p = objc2_core_graphics::CGEvent::location(Some(&ev));
        (p.x, p.y)
    });
    let b = CGDisplayBounds(CGMainDisplayID());
    let frac = ((target.1 - b.origin.y) / (b.size.height - 1.0)).clamp(0.0, 1.0) as f32;
    info!(
        ?target,
        frac, "smoke: the PC's cursor and drag cross at the left edge"
    );
    let _ = tx.send(InjectMsg::Begin { sid: 1 });
    send(Input::DragOffer {
        drag: DRAG,
        items: vec![DragItem {
            name,
            kind: ItemKind::File,
            size: Some(size),
        }],
    });
    send(Input::Enter {
        edge: Side::Left,
        frac,
        drag: Some(DRAG),
    });
    sleep(300);
    // Three small steps cross the drag threshold, then about 8 pt every 8 ms.
    let mut left = (target.0 - b.origin.x - 2.0).max(0.0) as i64;
    for _ in 0..3 {
        send(Input::Motion { dx: 3, dy: 0 });
        left -= 3;
        sleep(8);
    }
    while left > 0 {
        let dx = left.min(8);
        send(Input::Motion {
            dx: dx as i16,
            dy: 0,
        });
        left -= dx;
        sleep(8);
    }
    info!("smoke: hovering 1.5 s, then the PC's up");
    sleep(1500);
    send(Input::Button {
        button: MouseButton::Left,
        down: false,
    });
    match ended.recv_timeout(Duration::from_secs(90)) {
        Ok(dropped) => info!(dropped, "smoke: the session ended"),
        Err(_) => warn!("smoke: no end of the session within 90 s"),
    }
    sleep(3000);
    let _ = tx.send(InjectMsg::Quit);
    sleep(300);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, kind: ItemKind) -> DragItem {
        DragItem {
            name: name.into(),
            kind,
            size: None,
        }
    }

    #[test]
    fn utt_for_name_cases() {
        assert_eq!(utt_for_name("clip.mp4"), "public.mpeg-4");
        assert_eq!(utt_for_name("photo.jpg"), "public.jpeg");
        assert_eq!(utt_for_name("PHOTO.JPG"), "public.jpeg");
        assert_eq!(utt_for_name("Clip.MP4"), "public.mpeg-4");
        assert_eq!(utt_for_name("notes.txt"), "public.plain-text");
        assert_eq!(utt_for_name("data.zzqx9"), "public.data", "unknown");
        assert_eq!(utt_for_name("README"), "public.data", "no extension");
        assert_eq!(utt_for_name("_"), "public.data");
    }

    #[test]
    fn placeholder_names_are_sanitized_and_unique_within_the_drag() {
        let names = placeholder_names(&[
            item("a.txt", ItemKind::File),
            // APFS ignores case: the same name.
            item("A.TXT", ItemKind::File),
            // Sanitizes to "a.txt" too.
            item("a.txt.", ItemKind::File),
            item("a:b?.txt", ItemKind::File),
            item("Photos", ItemKind::Dir),
            item("photos", ItemKind::Dir),
            // The staging folder's name is never a placeholder's.
            item(".fetch", ItemKind::File),
            item("", ItemKind::File),
            item("v1.2", ItemKind::Dir),
            item("v1.2", ItemKind::Dir),
        ]);
        assert_eq!(
            names,
            [
                "a.txt", "A 2.TXT", "a 3.txt", "a_b_.txt", "Photos", "photos 2", ".fetch 2", "_",
                "v1.2", "v1.2 2",
            ]
        );
    }

    #[test]
    fn numbered_names_stay_within_the_length_limit() {
        let long = format!("{}.jpeg", "é".repeat(125)); // 250 + 5 bytes
        let n = numbered(&long, 12, false);
        assert!(n.len() <= MAX_NAME_BYTES, "{}", n.len());
        assert!(n.ends_with("é 12.jpeg"), "{n}");
        let names = placeholder_names(&[item(&long, ItemKind::File), item(&long, ItemKind::File)]);
        assert_ne!(fold(&names[0]), fold(&names[1]));
        assert!(names[1].len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn slots_put_placeholders_in_the_drag_folder_and_stage_apart() {
        let dir = Path::new("/c/incoming/7");
        let staging = dir.join(".fetch").join("3");
        let got = slots(
            dir,
            &staging,
            &[
                item("clip.mp4", ItemKind::File),
                item("Album", ItemKind::Dir),
            ],
        );
        assert_eq!(
            got,
            [
                Slot {
                    name: "clip.mp4".into(),
                    placeholder: dir.join("clip.mp4"),
                    staging: staging.join("0"),
                    is_dir: false,
                    uti: "public.mpeg-4".into(),
                },
                Slot {
                    name: "Album".into(),
                    placeholder: dir.join("Album"),
                    staging: staging.join("1"),
                    is_dir: true,
                    uti: "public.folder".into(),
                },
            ]
        );
    }

    #[test]
    fn placeholders_are_empty_and_replace_a_crossing_before() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("7");
        let staging = dir.join(STAGING).join("1");
        let s = slots(
            &dir,
            &staging,
            &[item("a.txt", ItemKind::File), item("Album", ItemKind::Dir)],
        );
        make_placeholders(&s).unwrap();
        assert_eq!(fs::metadata(dir.join("a.txt")).unwrap().len(), 0);
        assert!(fs::read_dir(dir.join("Album")).unwrap().next().is_none());
        assert!(staging.join("0").is_dir() && staging.join("1").is_dir());
        // A left-over from the drag's earlier crossing goes.
        fs::write(dir.join("a.txt"), b"old").unwrap();
        fs::write(dir.join("Album").join("x"), b"old").unwrap();
        make_placeholders(&s).unwrap();
        assert_eq!(fs::metadata(dir.join("a.txt")).unwrap().len(), 0);
        assert!(fs::read_dir(dir.join("Album")).unwrap().next().is_none());
    }

    #[test]
    fn a_fetched_item_is_renamed_over_its_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        // A file over an empty file.
        fs::write(t.join("a.txt"), b"").unwrap();
        fs::create_dir(t.join("stage")).unwrap();
        fs::write(t.join("stage").join("a.txt"), b"bytes").unwrap();
        put_in_place(&t.join("stage").join("a.txt"), &t.join("a.txt")).unwrap();
        assert_eq!(fs::read(t.join("a.txt")).unwrap(), b"bytes");
        assert!(!t.join("stage").join("a.txt").exists());
        // A folder over an empty folder.
        fs::create_dir(t.join("Album")).unwrap();
        fs::create_dir_all(t.join("stage").join("Album").join("sub")).unwrap();
        fs::write(
            t.join("stage").join("Album").join("sub").join("p.jpg"),
            b"jpg",
        )
        .unwrap();
        put_in_place(&t.join("stage").join("Album"), &t.join("Album")).unwrap();
        assert_eq!(
            fs::read(t.join("Album").join("sub").join("p.jpg")).unwrap(),
            b"jpg"
        );
        // Kinds that differ: the placeholder makes room.
        fs::create_dir(t.join("odd")).unwrap();
        fs::write(t.join("stage").join("odd"), b"file").unwrap();
        put_in_place(&t.join("stage").join("odd"), &t.join("odd")).unwrap();
        assert_eq!(fs::read(t.join("odd")).unwrap(), b"file");
    }

    #[test]
    fn promise_copies_never_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        fs::write(t.join("a.txt"), b"bytes").unwrap();
        copy_item(&t.join("a.txt"), &t.join("b.txt")).unwrap();
        assert_eq!(fs::read(t.join("b.txt")).unwrap(), b"bytes");
        let e = copy_item(&t.join("a.txt"), &t.join("b.txt")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        fs::create_dir_all(t.join("Album").join("sub")).unwrap();
        fs::write(t.join("Album").join("sub").join("p.jpg"), b"jpg").unwrap();
        copy_item(&t.join("Album"), &t.join("Copy")).unwrap();
        assert_eq!(
            fs::read(t.join("Copy").join("sub").join("p.jpg")).unwrap(),
            b"jpg"
        );
    }

    struct FakeTargets {
        windows: Option<Vec<WinInfo>>,
        apps: HashMap<i32, &'static str>,
    }

    impl Targets for FakeTargets {
        fn windows(&self) -> Option<Vec<WinInfo>> {
            self.windows.clone()
        }
        fn bundle_id(&self, pid: i32) -> Option<String> {
            self.apps.get(&pid).map(|s| s.to_string())
        }
    }

    fn win(layer: i64, pid: i32, bounds: (f64, f64, f64, f64)) -> WinInfo {
        WinInfo { layer, pid, bounds }
    }

    #[test]
    fn the_frontmost_normal_window_not_ours_owns_the_point() {
        const OURS: i32 = 1;
        let windows = [
            win(25, 50, (0.0, 0.0, 2000.0, 2000.0)), // a status-level panel
            win(0, OURS, (0.0, 0.0, 2000.0, 2000.0)), // ours
            win(0, 10, (100.0, 100.0, 200.0, 100.0)),
            win(0, 20, (0.0, 0.0, 1000.0, 1000.0)),
        ];
        assert_eq!(owner_at(&windows, (150.0, 150.0), OURS), Some(10));
        assert_eq!(
            owner_at(&windows, (300.0, 150.0), OURS),
            Some(20),
            "right edge is out"
        );
        assert_eq!(owner_at(&windows, (99.0, 150.0), OURS), Some(20));
        assert_eq!(
            owner_at(&windows, (1500.0, 10.0), OURS),
            None,
            "the desktop"
        );
    }

    #[test]
    fn known_promise_takers_get_the_up_at_once() {
        let targets = FakeTargets {
            windows: Some(vec![
                win(0, 10, (0.0, 0.0, 100.0, 100.0)),
                win(0, 11, (100.0, 0.0, 100.0, 100.0)),
                win(0, 12, (200.0, 0.0, 100.0, 100.0)),
                win(0, 13, (300.0, 0.0, 100.0, 100.0)),
            ]),
            apps: HashMap::from([
                (10, "com.apple.finder"),
                (11, "net.whatsapp.WhatsApp"),
                (12, "com.google.Chrome"),
            ]),
        };
        let at = |x| promise_taker_at((x, 50.0), &targets, 1);
        assert!(at(50.0), "Finder");
        assert!(at(150.0), "WhatsApp");
        assert!(!at(250.0), "Chrome reads the file URL");
        assert!(!at(350.0), "an app without a bundle id");
        assert!(at(950.0), "the desktop is Finder's");
        let unknown = FakeTargets {
            windows: None,
            apps: HashMap::new(),
        };
        assert!(
            !promise_taker_at((50.0, 50.0), &unknown, 1),
            "no window list: wait for the bytes"
        );
    }

    /// A drag with `n` file items in `dir`, its placeholders made.
    fn drag_in(dir: &Path, n: usize) -> Drag {
        let items: Vec<DragItem> = (0..n)
            .map(|i| item(&format!("f{i}.txt"), ItemKind::File))
            .collect();
        let staging = dir.join(STAGING).join("1");
        let s = slots(dir, &staging, &items);
        make_placeholders(&s).unwrap();
        Drag::new(7, s, staging)
    }

    fn fetched(drag: &Drag, i: usize, bytes: &[u8]) -> PathBuf {
        let p = drag.slots[i].staging.join(&drag.slots[i].name);
        fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn fetched_items_land_in_their_placeholders_then_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let d = drag_in(tmp.path(), 2);
        let p = fetched(&d, 1, b"one");
        assert!(d.resolve(1, Ok(p)).is_empty(), "item 0 still fetching");
        assert_eq!(fs::read(&d.slots[1].placeholder).unwrap(), b"one");
        assert!(!d.all_complete());
        let p = fetched(&d, 0, b"zero");
        let evs = d.resolve(0, Ok(p));
        assert!(matches!(evs[..], [MacDndEv::Ready { drag: 7 }]), "{evs:?}");
        assert!(d.all_complete());
        assert_eq!(d.wait_item(0), Ok(()));
        assert!(
            !tmp.path().join(STAGING).join("1").exists(),
            "staging tidied"
        );
        // A second answer for an item changes nothing.
        assert!(d.resolve(0, Err(FailReason::Unwritable)).is_empty());
    }

    #[test]
    fn a_failed_item_is_reported_and_fails_its_promise() {
        let tmp = tempfile::tempdir().unwrap();
        let d = drag_in(tmp.path(), 2);
        let evs = d.resolve(1, Err(FailReason::NoSpace));
        assert!(
            matches!(
                evs[..],
                [MacDndEv::Failed {
                    drag: 7,
                    item: 1,
                    reason: FailReason::NoSpace
                }]
            ),
            "{evs:?}"
        );
        assert_eq!(d.wait_item(1), Err(FailReason::NoSpace));
        let err = write_promise(&d, 1, &tmp.path().join("out.txt")).unwrap_err();
        assert!(err.contains("NoSpace"), "{err}");
    }

    #[test]
    fn a_promise_write_waits_for_its_item_then_copies_it() {
        let tmp = tempfile::tempdir().unwrap();
        let d = Arc::new(drag_in(tmp.path(), 1));
        let out = tmp.path().join("Downloads-f0.txt");
        let writer = {
            let (d, out) = (d.clone(), out.clone());
            std::thread::spawn(move || write_promise(&d, 0, &out))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!writer.is_finished(), "waits for the fetch");
        assert_eq!(d.writes.load(Ordering::SeqCst), 1);
        let p = fetched(&d, 0, b"payload");
        d.resolve(0, Ok(p));
        writer.join().unwrap().unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"payload");
        assert_eq!(d.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancelling_fails_waiting_writes_and_silences_late_results() {
        let tmp = tempfile::tempdir().unwrap();
        let d = Arc::new(drag_in(tmp.path(), 1));
        let writer = {
            let (d, out) = (d.clone(), tmp.path().join("out.txt"));
            std::thread::spawn(move || write_promise(&d, 0, &out))
        };
        std::thread::sleep(Duration::from_millis(50));
        d.cancel();
        assert!(writer.join().unwrap().is_err());
        assert!(d.resolve(0, Err(FailReason::Cancelled)).is_empty());
    }

    #[test]
    fn the_source_tells_the_injection_thread_what_it_needs() {
        let (reply, _rx) = oneshot::channel();
        let cases = [
            (
                MacDndEv::Ended {
                    drag: 7,
                    dropped: true,
                },
                Some(InjectMsg::DragEnded { drag: 7 }),
            ),
            (
                MacDndEv::Ready { drag: 7 },
                Some(InjectMsg::DragReady { drag: 7 }),
            ),
            (
                MacDndEv::Failed {
                    drag: 7,
                    item: 0,
                    reason: FailReason::Cancelled,
                },
                Some(InjectMsg::DragFailed { drag: 7 }),
            ),
            (
                MacDndEv::Released {
                    drag: 7,
                    at: (1.0, 2.0),
                },
                None,
            ),
            (
                MacDndEv::Fetch {
                    drag: 7,
                    item: 0,
                    dest: Dest::InDir("/x".into()),
                    reply,
                },
                None,
            ),
        ];
        for (ev, want) in cases {
            assert_eq!(ev.inject_msg(), want, "{ev:?}");
        }
    }

    /// The source without AppKit: what happens to a drag that never
    /// prepared or is over.
    #[test]
    fn an_unknown_drag_may_be_released_and_cancelling_it_does_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let src = DragSource::with(
            move |ev| {
                let _ = tx.send(format!("{ev:?}"));
            },
            tmp.path().into(),
            Box::new(FakeTargets {
                windows: None,
                apps: HashMap::new(),
            }),
        );
        assert!(src.may_release(9, (0.0, 0.0)));
        src.cancel(9);
        src.released(9, (0.0, 0.0));
        assert!(rx.try_recv().is_err(), "no events");
        // No items: it can't start, and says it ended undropped.
        assert!(!src.prepare(9, Vec::new(), (0.0, 0.0)));
        assert_eq!(rx.try_recv().unwrap(), "Ended { drag: 9, dropped: false }");
    }

    /// A drag registered without its panel: cancelling it before a session
    /// began ends it, undropped, exactly once, and stops its fetches.
    #[test]
    fn cancelling_before_the_session_began_ends_it_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let src = DragSource::with(
            move |ev| {
                let _ = tx.send(format!("{ev:?}"));
            },
            tmp.path().into(),
            Box::new(FakeTargets {
                windows: None,
                apps: HashMap::new(),
            }),
        );
        let d = Arc::new(drag_in(&tmp.path().join("7"), 1));
        src.inner.live().insert(7, d.clone());
        assert!(src.may_release(7, (0.0, 0.0)), "no session to hold for");
        // Pretend the session began: incomplete, no window list → hold.
        d.lock().began = true;
        assert!(!src.may_release(7, (0.0, 0.0)));
        d.lock().began = false;
        src.cancel(7);
        src.cancel(7);
        assert_eq!(rx.try_recv().unwrap(), "Ended { drag: 7, dropped: false }");
        assert!(rx.try_recv().is_err(), "once");
        assert!(d.lock().cancelled);
    }
}
