//! Drags arriving from the PC (R47): the PC's file drag rides onto the Mac
//! and becomes a Mac drag session that drops like a local file.
//!
//! - `prepare` (cj-inject, at `BeginDrag`, within [`PREPARE_TIMEOUT`]): a
//!   placeholder per item in `~/Library/Caches/CursorJump/incoming/<drag>/`,
//!   an empty file or folder under the item's sanitized name, unique
//!   within the drag, and an 8×8 pt panel under the cursor. A drag that
//!   went home and came back keeps the items its earlier crossing
//!   completed; anything else in the way is moved aside, never deleted.
//!   Then a prefetch thread sends a `Fetch` per missing item, each into
//!   its own staging folder under `incoming/<drag>/.fetch/`, and renames
//!   each fetched item over its placeholder.
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
//! AppKit objects live on the main thread (`on_main`, `on_main_sync`;
//! `appkit`); the views and panels are kept in a main-thread list, so none
//! is ever released elsewhere. The rest is shared through `Arc`s, behind
//! locks.

mod appkit;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use cj_core::dnd::{DragId, DragItem, FailReason, ItemKind, sanitize_component};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_app_kit::NSRunningApplication;
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_graphics::{
    CGWindowLevelForKey, CGWindowLevelKey, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, ns_string};
use objc2_uniform_type_identifiers::UTType;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};
use unicode_normalization::UnicodeNormalization;

use super::{on_main, on_main_sync};
use crate::common::transfer::Dest;
use crate::mac::control::{Crossing, DragPort, InjectMsg};

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

/// How often `prepare` looks for its panel in the window list.
const ON_TOP_POLL: Duration = Duration::from_millis(5);
/// Earlier crossings remembered for a drag that comes back.
const PAST_MAX: usize = 8;

/// Numbers each `prepare`'s staging folder: a drag that comes back (it went
/// home and returned) never fetches where an old fetch may still write.
static ATTEMPT: AtomicU64 = AtomicU64::new(1);

/// What the drag source tells the app (Task 17).
#[derive(Debug)]
pub enum MacDndEv {
    /// The session of `drag`'s `crossing` ended here, on a target that
    /// took it (`dropped`) or not. Exactly one per crossing, a failed
    /// `prepare` included (crossing 0 if it never got a token). A drag
    /// that went home and came back ends once per crossing: only its
    /// latest crossing's end counts for the drag. A drop whose promise
    /// writes still wait is dropped: the outcome comes from the fetches
    /// (`incoming_outcome`).
    Ended {
        drag: DragId,
        crossing: Crossing,
        dropped: bool,
    },
    /// Fetch item `item` of `drag` into `dest`, a staging folder
    /// (`Dest::InDir`), and reply with the received path or why it failed.
    /// One per item not already here, sent from the prefetch thread just
    /// after `prepare`. A reply dropped unanswered fails the item. The source drops its receiver when the drag goes home or is
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
    /// Every item of `drag`'s `crossing` is in place.
    Ready { drag: DragId, crossing: Crossing },
    /// A session started just as its drag was cancelled: cj-inject posts
    /// Esc to end it undropped.
    NeedsEscape { drag: DragId, crossing: Crossing },
    /// Item `item` of `drag`'s `crossing` failed; cj-inject cancels the
    /// drag.
    #[cfg_attr(
        all(debug_assertions, not(test)),
        expect(dead_code, reason = "Task 17 tells the pill why")
    )]
    Failed {
        drag: DragId,
        crossing: Crossing,
        item: u32,
        reason: FailReason,
    },
}

impl MacDndEv {
    /// What cj-inject must be told of this event: Task 17's events
    /// callback sends it on the injection thread's channel.
    pub fn inject_msg(&self) -> Option<InjectMsg> {
        match *self {
            MacDndEv::Ended { drag, crossing, .. } => Some(InjectMsg::DragEnded { drag, crossing }),
            MacDndEv::Ready { drag, crossing } => Some(InjectMsg::DragReady { drag, crossing }),
            MacDndEv::Failed { drag, crossing, .. } => {
                Some(InjectMsg::DragFailed { drag, crossing })
            }
            MacDndEv::NeedsEscape { drag, crossing } => {
                Some(InjectMsg::EscapeSession { drag, crossing })
            }
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
    autoreleasepool(|_| {
        UTType::typeWithFilenameExtension(&NSString::from_str(&ext.to_lowercase()))
            .filter(|t| !t.isDynamic())
            .map_or_else(|| DATA_UTT.into(), |t| t.identifier().to_string())
    })
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
}

/// The slots of `items` in drag folder `dir`, staging under `staging`.
fn slots(dir: &Path, staging: &Path, items: &[DragItem]) -> Vec<Slot> {
    placeholder_names(items)
        .into_iter()
        .zip(items)
        .enumerate()
        .map(|(i, (name, item))| Slot {
            placeholder: dir.join(&name),
            staging: staging.join(i.to_string()),
            is_dir: item.kind == ItemKind::Dir,
            name,
        })
        .collect()
}

/// Creates the empty placeholders in `dir`, but for the items `keep`
/// says are already in place. Anything else in a placeholder's way (an
/// earlier crossing's) is moved into `aside` with one rename, never
/// deleted here: the sweep removes it. A few syscalls per item, no tree
/// walks: it runs within `prepare`'s budget.
fn make_placeholders(dir: &Path, slots: &[Slot], keep: &[bool], aside: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut aside_made = false;
    for (i, s) in slots.iter().enumerate() {
        if keep.get(i) == Some(&true) {
            continue;
        }
        if fs::symlink_metadata(&s.placeholder).is_ok() {
            if !aside_made {
                fs::create_dir_all(aside)?;
                aside_made = true;
            }
            fs::rename(&s.placeholder, aside.join(i.to_string()))?;
        }
        if s.is_dir {
            fs::create_dir(&s.placeholder)?;
        } else {
            fs::File::create_new(&s.placeholder)?;
        }
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

/// Copies a folder's tree (folders and files) to `dest`, made new.
pub(super) fn copy_tree(src: &Path, dest: &Path) -> io::Result<()> {
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
    /// Its window number.
    number: i64,
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

/// Whether the window server has our panel (`number`) as the frontmost
/// window holding `at` among those on its layer or below: only then does a
/// posted press there reach it. Windows on a higher layer are passed over:
/// those are overlays (screen tints, cursor and drag-image windows) that
/// let clicks through — a full-screen one at layer 2147483628 sat over the
/// panel in the first hands-on run while the spike's presses still landed.
fn panel_on_top(windows: &[WinInfo], at: (f64, f64), number: i64) -> bool {
    let Some(ours) = windows.iter().find(|w| w.number == number) else {
        return false;
    };
    windows
        .iter()
        .filter(|w| w.layer <= ours.layer)
        .find(|w| contains(w.bounds, at))
        .is_some_and(|w| w.number == number)
}

fn contains((x, y, wd, ht): (f64, f64, f64, f64), at: (f64, f64)) -> bool {
    at.0 >= x && at.0 < x + wd && at.1 >= y && at.1 < y + ht
}

/// The frontmost window holding `at` that isn't ours, whatever its layer;
/// None over the desktop.
fn window_at(windows: &[WinInfo], at: (f64, f64), own_pid: i32) -> Option<&WinInfo> {
    windows
        .iter()
        .find(|w| w.pid != own_pid && w.layer < OVERLAY_LAYER && contains(w.bounds, at))
}

/// Windows from the dragging level (kCGDraggingWindowLevel) up are passed
/// over when looking for the drop target: drag images, the cursor and
/// full-screen overlays (a screen tint app's sat at 2147483628 over every
/// drop in the first hands-on run) never take the drop.
const OVERLAY_LAYER: i64 = 500;

/// Whether the drop under `at` goes to an app that takes file promises
/// (R50): a normal window (layer 0) of a listed app, or the desktop
/// (Finder's) with no window over it. A window on another layer (the
/// Dock, the menu bar, a floating panel) is no taker. Unsure means no: the
/// drop then waits for the bytes, which is slower but never wrong.
fn promise_taker_at(at: (f64, f64), targets: &dyn Targets, own_pid: i32) -> bool {
    let Some(windows) = targets.windows() else {
        return false;
    };
    let bundle = match window_at(&windows, at, own_pid) {
        None => Some(FINDER.to_string()),
        Some(w) if w.layer != 0 => {
            debug!(
                layer = w.layer,
                pid = w.pid,
                "the drop target is no normal window"
            );
            return false;
        }
        Some(w) => targets.bundle_id(w.pid),
    };
    debug!(?bundle, "the drop target's app");
    bundle.is_some_and(|b| PROMISE_TAKERS.contains(&b.as_str()))
}

/// The window server's list and the running apps.
struct SystemTargets;

impl Targets for SystemTargets {
    /// Leaves out the windows of drag images and the cursor, which float
    /// over the drop point during a drag.
    fn windows(&self) -> Option<Vec<WinInfo>> {
        autoreleasepool(|_| self.windows_in_pool())
    }

    fn bundle_id(&self, pid: i32) -> Option<String> {
        autoreleasepool(|_| {
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?
                .bundleIdentifier()
                .map(|b| b.to_string())
        })
    }
}

impl SystemTargets {
    fn windows_in_pool(&self) -> Option<Vec<WinInfo>> {
        let floating = [
            CGWindowLevelForKey(CGWindowLevelKey::DraggingWindowLevelKey),
            CGWindowLevelForKey(CGWindowLevelKey::CursorWindowLevelKey),
        ]
        .map(i64::from);
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
        let key = |k: &'static CFString| -> &'static NSString {
            unsafe { &*(k as *const CFString).cast::<NSString>() }
        };
        // SAFETY: reading extern statics the framework defines.
        let (number_key, layer_key, pid_key, bounds_key) = unsafe {
            (
                key(kCGWindowNumber),
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
                    number: number(&w, number_key)?.integerValue() as i64,
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
            .filter(|w| !floating.contains(&w.layer))
            .collect();
        Some(windows)
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
    /// Its session is starting or started: the posted down reached the
    /// panel (set before `beginDraggingSession…`).
    began: bool,
    /// `beginDraggingSession…` returned: Esc reaches the session now.
    started: bool,
    /// `Ended` was sent.
    ended: bool,
    /// A later crossing of the same drag took over its placeholders: a
    /// fetch that finishes now is not put in place.
    superseded: bool,
}

/// One drag from the PC, shared by cj-inject, the main thread, the
/// prefetch thread and the promise writers.
struct Drag {
    id: DragId,
    /// This crossing's token (its attempt number).
    crossing: Crossing,
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
    /// `items`: each slot's state at the start (Complete if carried over).
    fn new(
        id: DragId,
        crossing: Crossing,
        slots: Vec<Slot>,
        staging: PathBuf,
        items: Vec<Item>,
    ) -> Drag {
        Drag {
            id,
            crossing,
            state: Mutex::new(DragState {
                items,
                cancelled: false,
                began: false,
                started: false,
                ended: false,
                superseded: false,
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

    /// Cancels it. Decided under the lock that starting a session takes,
    /// so the two can't cross. Returns (whether a session began, whether
    /// one is live: started and not ended, so that Esc ends it). A session
    /// still starting asks for its own Esc (`session_started`).
    fn cancel(&self) -> (bool, bool) {
        let (began, live) = {
            let mut st = self.lock();
            st.cancelled = true;
            (st.began, st.started && !st.ended)
        };
        self.changed.notify_all();
        self.stop.notify_one();
        (began, live)
    }

    /// Main thread, at the posted down: whether to start a session. Not
    /// once cancelled (nor twice).
    fn try_begin(&self) -> bool {
        let mut st = self.lock();
        if st.began || st.cancelled {
            return false;
        }
        st.began = true;
        true
    }

    /// Main thread, once `beginDraggingSession…` returned: whether the drag
    /// was cancelled meanwhile, so the new session needs an Esc.
    fn session_started(&self) -> bool {
        let mut st = self.lock();
        st.started = true;
        st.cancelled && !st.ended
    }

    /// Item `i`'s fetch finished: puts it in place. Returns what to tell
    /// the app (nothing once the drag is cancelled). The rename happens
    /// under the lock, so once a later crossing marked this one
    /// superseded, nothing lands on its placeholders any more.
    fn resolve(&self, i: usize, result: Result<PathBuf, FailReason>) -> Vec<MacDndEv> {
        let Some(slot) = self.slots.get(i) else {
            return Vec::new();
        };
        let mut st = self.lock();
        if st.items[i] != Item::Fetching {
            return Vec::new();
        }
        let outcome = if st.superseded {
            Err(FailReason::Cancelled)
        } else {
            result.and_then(|path| {
                put_in_place(&path, &slot.placeholder).map_err(|e| {
                    warn!(
                        drag = self.id,
                        item = i,
                        "placing a fetched item failed: {e}"
                    );
                    FailReason::Unwritable
                })
            })
        };
        st.items[i] = match &outcome {
            Ok(()) => Item::Complete,
            Err(reason) => Item::Failed(reason.clone()),
        };
        let all = st.items.iter().all(|x| *x == Item::Complete);
        let cancelled = st.cancelled;
        drop(st);
        self.changed.notify_all();
        let (drag, crossing, item) = (self.id, self.crossing, i as u32);
        match outcome {
            _ if cancelled => Vec::new(),
            Err(reason) => {
                warn!(
                    drag,
                    item,
                    ?reason,
                    "an item of the drag from the PC failed"
                );
                vec![MacDndEv::Failed {
                    drag,
                    crossing,
                    item,
                    reason,
                }]
            }
            Ok(()) if all => {
                info!(drag, "every item of the drag from the PC is here");
                // Only empty folders are left there.
                let _ = fs::remove_dir_all(&self.staging);
                vec![MacDndEv::Ready { drag, crossing }]
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

    /// Hands this crossing's placeholders over to a later crossing with
    /// `slots`: which of them are complete and stay. From now on none of
    /// this crossing's fetches lands on them.
    fn hand_over(&self, slots: &[Slot]) -> Vec<bool> {
        let mut st = self.lock();
        st.superseded = true;
        slots
            .iter()
            .enumerate()
            .map(|(i, s)| {
                self.slots
                    .get(i)
                    .is_some_and(|o| o.placeholder == s.placeholder && o.is_dir == s.is_dir)
                    && st.items.get(i) == Some(&Item::Complete)
            })
            .collect()
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
    /// Crossings that ended without a drop, latest last, for a drag that
    /// comes back.
    past: Mutex<Vec<Arc<Drag>>>,
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

    fn past(&self) -> MutexGuard<'_, Vec<Arc<Drag>>> {
        self.past.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, drag: DragId) -> Option<Arc<Drag>> {
        self.live().get(&drag).cloned()
    }

    /// Keeps a crossing that ended without a drop, in case its drag comes
    /// back.
    fn remember(&self, drag: &Arc<Drag>) {
        if drag.lock().superseded {
            return;
        }
        let mut past = self.past();
        if past.iter().any(|d| Arc::ptr_eq(d, drag)) {
            return;
        }
        if past.len() == PAST_MAX {
            past.remove(0);
        }
        past.push(drag.clone());
    }

    /// `drag`'s `crossing`, live or remembered.
    fn crossing(&self, drag: DragId, crossing: Crossing) -> Option<Arc<Drag>> {
        let is = |d: &Arc<Drag>| d.id == drag && d.crossing == crossing;
        self.get(drag)
            .filter(is)
            .or_else(|| self.past().iter().rev().find(|d| is(d)).cloned())
    }

    /// The latest earlier crossing of `drag`, left in the list.
    fn find_past(&self, drag: DragId) -> Option<Arc<Drag>> {
        self.past().iter().rev().find(|d| d.id == drag).cloned()
    }

    /// The latest earlier crossing of `drag`, taken out of the list.
    fn take_past(&self, drag: DragId) -> Option<Arc<Drag>> {
        let mut past = self.past();
        let i = past.iter().rposition(|d| d.id == drag)?;
        Some(past.remove(i))
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
            self.remember(drag);
        }
        info!(
            drag = drag.id,
            crossing = drag.crossing,
            dropped,
            "the drag from the PC ended here"
        );
        self.emit(MacDndEv::Ended {
            drag: drag.id,
            crossing: drag.crossing,
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
                past: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Arms `drag` under `at` (global points): its placeholders and the
    /// drag panel there; a prefetch thread then sends a `Fetch` per item
    /// still missing. All within [`PREPARE_TIMEOUT`]. Returns this
    /// crossing's token; None if it can't start (no items, the disk, the
    /// main thread too slow): `Ended` (not dropped) then follows, and
    /// nothing must be pressed.
    pub fn prepare(&self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> Option<Crossing> {
        let started = Instant::now();
        let fail = |why: &str| {
            warn!(drag, "the drag from the PC can't start here: {why}");
            self.inner.emit(MacDndEv::Ended {
                drag,
                crossing: 0,
                dropped: false,
            });
            None
        };
        if items.is_empty() {
            return fail("no items were offered for it");
        }
        let d = match self.stage(drag, &items) {
            Ok(d) => d,
            Err(e) => return fail(&format!("creating its placeholders: {e}")),
        };
        self.inner.live().insert(drag, d.clone());
        let left = PREPARE_TIMEOUT.saturating_sub(started.elapsed());
        let (inner, armed) = (self.inner.clone(), d.clone());
        let shown = if left.is_zero() {
            None
        } else {
            on_main_sync(left, move |mtm| appkit::arm(mtm, &inner, &armed, at)).flatten()
        };
        let on_top = shown.is_some_and(|n| self.wait_on_top(drag, n as i64, at, started));
        if !on_top {
            warn!(
                drag,
                ?shown,
                ms = started.elapsed().as_millis() as u64,
                "the drag panel did not come up under the cursor in time"
            );
            self.cancel(drag);
            return None;
        }
        if let Err(e) = spawn_prefetch(self.inner.clone(), d.clone()) {
            warn!(drag, "starting the prefetch thread failed: {e}");
            self.cancel(drag);
            return None;
        }
        info!(
            drag,
            crossing = d.crossing,
            items = d.slots.len(),
            ms = started.elapsed().as_millis() as u64,
            "the drag from the PC is armed"
        );
        Some(d.crossing)
    }

    /// Polls the window list until our panel `number` is the frontmost
    /// window under `at`, for what is left of the budget: a press posted
    /// before that falls through to the window behind.
    fn wait_on_top(&self, drag: DragId, number: i64, at: (f64, f64), started: Instant) -> bool {
        let deadline = started + PREPARE_TIMEOUT;
        loop {
            let windows = self.inner.targets.windows();
            if windows
                .as_ref()
                .is_some_and(|w| panel_on_top(w, at, number))
            {
                debug!(
                    drag,
                    ms = started.elapsed().as_millis() as u64,
                    "the drag panel is on top"
                );
                return true;
            }
            if Instant::now() >= deadline {
                let list = windows.unwrap_or_default();
                let ours = list.iter().position(|w| w.number == number);
                let top = list.iter().find(|w| contains(w.bounds, at));
                warn!(
                    drag,
                    ?ours,
                    top = ?top,
                    "the window server never showed the drag panel on top under the cursor"
                );
                return false;
            }
            std::thread::sleep(ON_TOP_POLL);
        }
    }

    /// This crossing's placeholders: an earlier crossing of the same drag
    /// hands over the items it completed; the rest start empty.
    fn stage(&self, drag: DragId, items: &[DragItem]) -> io::Result<Arc<Drag>> {
        let dir = self.inner.root.join(drag.to_string());
        let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
        let fetch = dir.join(STAGING);
        let staging = fetch.join(attempt.to_string());
        let slots = slots(&dir, &staging, items);
        // A crossing still live is over now.
        let live = self.inner.live().remove(&drag);
        if let Some(old) = live {
            self.inner.end(&old, false);
        }
        let keep = match self.inner.take_past(drag) {
            Some(old) => {
                let _ = old.cancel();
                old.hand_over(&slots)
            }
            None => vec![false; slots.len()],
        };
        let kept = keep.iter().filter(|k| **k).count();
        if kept > 0 {
            info!(drag, kept, "items from the drag's earlier crossing stay");
        }
        make_placeholders(&dir, &slots, &keep, &fetch.join(format!("{attempt}-old")))?;
        let states = keep
            .iter()
            .map(|&k| if k { Item::Complete } else { Item::Fetching })
            .collect();
        Ok(Arc::new(Drag::new(drag, attempt, slots, staging, states)))
    }

    /// The user let go while `drag` rode here, at `at`.
    pub fn released(&self, drag: DragId, at: (f64, f64)) {
        let Some(d) = self.inner.get(drag) else {
            return;
        };
        // Decided under one lock with starting the session: without one
        // yet, the drag is cancelled there and then, so none can start.
        let began = {
            let mut st = d.lock();
            if !st.began {
                st.cancelled = true;
            }
            st.began
        };
        if began {
            self.inner.emit(MacDndEv::Released { drag, at });
        } else {
            // No session: the press missed the panel. Nothing to drop.
            warn!(drag, "the drag from the PC never started here");
            self.cancel(drag);
        }
    }

    /// Whether the up may be posted now, over `at`: every item is in
    /// place, or the app there takes file promises. True for a drag that
    /// is over, or never started a session. Never for an incomplete drag
    /// whose session is still on after it was cancelled: its Esc ends it.
    pub fn may_release(&self, drag: DragId, at: (f64, f64)) -> bool {
        let found = self.inner.get(drag).or_else(|| self.inner.find_past(drag));
        let Some(d) = found else {
            return true; // Never here, or dropped and gone: no session.
        };
        {
            let st = d.lock();
            if !st.began || st.ended {
                return true;
            }
            if st.cancelled {
                return false;
            }
            if st.items.iter().all(|x| *x == Item::Complete) {
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
    /// goes. True if its session is live: cj-inject then posts the Esc that
    /// ends it. A session still starting asks for its Esc itself
    /// (`NeedsEscape`); one that never began is ended here. Idempotent.
    pub fn cancel(&self, drag: DragId) -> bool {
        let Some(d) = self.inner.live().remove(&drag) else {
            return false;
        };
        let (began, live) = d.cancel();
        self.inner.remember(&d);
        if !began {
            // No session will end it.
            self.inner.end(&d, false);
            on_main(move |_| appkit::close_panel(&d));
        }
        live
    }

    /// Whether the session of `drag`'s `crossing` is live: started and not
    /// ended, so that an Esc reaches it.
    pub fn session_live(&self, drag: DragId, crossing: Crossing) -> bool {
        self.inner.crossing(drag, crossing).is_some_and(|d| {
            let st = d.lock();
            st.started && !st.ended
        })
    }

    /// Whether every item of `drag` is in place (true once it is over).
    #[cfg(debug_assertions)]
    pub(super) fn all_complete(&self, drag: DragId) -> bool {
        self.inner.get(drag).is_none_or(|d| d.all_complete())
    }
}

impl DragPort for DragSource {
    fn prepare(&mut self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> Option<Crossing> {
        DragSource::prepare(self, drag, items, at)
    }
    fn released(&mut self, drag: DragId, at: (f64, f64)) {
        DragSource::released(self, drag, at);
    }
    fn may_release(&mut self, drag: DragId, at: (f64, f64)) -> bool {
        DragSource::may_release(self, drag, at)
    }
    fn cancel(&mut self, drag: DragId) -> bool {
        DragSource::cancel(self, drag)
    }
    fn session_live(&mut self, drag: DragId, crossing: Crossing) -> bool {
        DragSource::session_live(self, drag, crossing)
    }
}

/// The prefetch, on a thread of its own: makes each missing item's
/// staging folder, sends its `Fetch`, and puts each item in place as its
/// reply comes, until all are in or the drag is cancelled (which drops
/// the receivers).
fn spawn_prefetch(inner: Arc<Inner>, drag: Arc<Drag>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("cj-drag-prefetch".into())
        .spawn(move || prefetch(&inner, &drag))
        .map(drop)
}

fn prefetch(inner: &Inner, drag: &Drag) {
    let missing: Vec<usize> = {
        let st = drag.lock();
        (0..st.items.len())
            .filter(|&i| st.items[i] == Item::Fetching)
            .collect()
    };
    if missing.is_empty() {
        info!(
            drag = drag.id,
            "every item is here from an earlier crossing"
        );
        inner.emit(MacDndEv::Ready {
            drag: drag.id,
            crossing: drag.crossing,
        });
        return;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(e) => {
            warn!(drag = drag.id, "the prefetch can't run: {e}");
            for i in missing {
                for ev in drag.resolve(i, Err(FailReason::Unwritable)) {
                    inner.emit(ev);
                }
            }
            return;
        }
    };
    let mut replies = Vec::new();
    for i in missing {
        if drag.lock().cancelled {
            return;
        }
        let slot = &drag.slots[i];
        if let Err(e) = fs::create_dir_all(&slot.staging) {
            warn!(drag = drag.id, item = i, "making its staging folder: {e}");
            for ev in drag.resolve(i, Err(FailReason::Unwritable)) {
                inner.emit(ev);
            }
            continue;
        }
        let (reply, rx) = oneshot::channel();
        replies.push((i, rx));
        inner.emit(MacDndEv::Fetch {
            drag: drag.id,
            item: i as u32,
            dest: Dest::InDir(slot.staging.clone()),
            reply,
        });
    }
    runtime.block_on(async {
        let mut waiting = JoinSet::new();
        for (i, rx) in replies {
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
                },
                Slot {
                    name: "Album".into(),
                    placeholder: dir.join("Album"),
                    staging: staging.join("1"),
                    is_dir: true,
                },
            ]
        );
    }

    #[test]
    fn placeholders_are_empty_and_leftovers_are_moved_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("7");
        let staging = dir.join(STAGING).join("1");
        let aside = dir.join(STAGING).join("1-old");
        let s = slots(
            &dir,
            &staging,
            &[item("a.txt", ItemKind::File), item("Album", ItemKind::Dir)],
        );
        make_placeholders(&dir, &s, &[], &aside).unwrap();
        assert_eq!(fs::metadata(dir.join("a.txt")).unwrap().len(), 0);
        assert!(fs::read_dir(dir.join("Album")).unwrap().next().is_none());
        assert!(!aside.exists(), "nothing was in the way");
        assert!(!staging.exists(), "the prefetch thread makes it");
        // Left-overs from an earlier crossing are moved aside, whole.
        fs::write(dir.join("a.txt"), b"old").unwrap();
        fs::create_dir(dir.join("Album").join("deep")).unwrap();
        fs::write(dir.join("Album").join("deep").join("x"), b"old").unwrap();
        make_placeholders(&dir, &s, &[], &aside).unwrap();
        assert_eq!(fs::metadata(dir.join("a.txt")).unwrap().len(), 0);
        assert!(fs::read_dir(dir.join("Album")).unwrap().next().is_none());
        assert_eq!(fs::read(aside.join("0")).unwrap(), b"old");
        assert_eq!(
            fs::read(aside.join("1").join("deep").join("x")).unwrap(),
            b"old"
        );
        // An item to keep is left as it is.
        fs::write(dir.join("a.txt"), b"done").unwrap();
        make_placeholders(&dir, &s, &[true, false], &dir.join("aside2")).unwrap();
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"done");
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
        WinInfo {
            number: 0,
            layer,
            pid,
            bounds,
        }
    }

    #[test]
    fn the_press_waits_for_our_panel_on_top_under_the_cursor() {
        let at = (2.0, 500.0);
        let panel = WinInfo {
            number: 42,
            layer: 25,
            pid: 1,
            bounds: (-2.0, 496.0, 8.0, 8.0),
        };
        let other = |number, bounds| WinInfo {
            number,
            layer: 0,
            pid: 9,
            bounds,
        };
        let finder = other(7, (0.0, 0.0, 1000.0, 1000.0));
        assert!(panel_on_top(&[panel.clone(), finder.clone()], at, 42));
        assert!(
            !panel_on_top(std::slice::from_ref(&finder), at, 42),
            "not listed yet: the press would hit Finder"
        );
        let over = other(8, (0.0, 490.0, 50.0, 50.0));
        assert!(
            !panel_on_top(&[over, panel.clone(), finder.clone()], at, 42),
            "another window on top would eat the press"
        );
        assert!(
            !panel_on_top(&[panel, finder.clone()], (100.0, 500.0), 42),
            "listed, but not under the point"
        );
        assert!(!panel_on_top(&[], at, 42));
        let overlay = WinInfo {
            number: 9,
            layer: 2_147_483_628,
            pid: 3,
            bounds: (0.0, 0.0, 1800.0, 1169.0),
        };
        assert!(
            panel_on_top(&[overlay, panel_at_25(), finder.clone()], at, 42),
            "a higher-layer overlay lets the press through"
        );
        let same_layer = WinInfo {
            layer: 25,
            ..other(10, (0.0, 490.0, 50.0, 50.0))
        };
        assert!(
            !panel_on_top(&[same_layer, panel_at_25(), finder], at, 42),
            "a window on our layer in front eats the press"
        );
    }

    fn panel_at_25() -> WinInfo {
        WinInfo {
            number: 42,
            layer: 25,
            pid: 1,
            bounds: (-2.0, 496.0, 8.0, 8.0),
        }
    }

    #[test]
    fn the_frontmost_window_not_ours_is_the_target() {
        const OURS: i32 = 1;
        let windows = [
            win(25, OURS, (0.0, 0.0, 2000.0, 2000.0)), // our panel
            win(0, 10, (100.0, 100.0, 200.0, 100.0)),
            win(0, 20, (0.0, 0.0, 1000.0, 1000.0)),
        ];
        let pid = |x, y| window_at(&windows, (x, y), OURS).map(|w| w.pid);
        assert_eq!(pid(150.0, 150.0), Some(10));
        assert_eq!(pid(300.0, 150.0), Some(20), "right edge is out");
        assert_eq!(pid(99.0, 150.0), Some(20));
        assert_eq!(pid(1500.0, 10.0), None, "the desktop");
        let overlaid = [
            win(2_147_483_628, 30, (0.0, 0.0, 2000.0, 2000.0)), // a screen-tint overlay
            win(500, 31, (100.0, 100.0, 64.0, 64.0)),           // a drag image
            win(0, 20, (0.0, 0.0, 1000.0, 1000.0)),
        ];
        let under = |x, y| window_at(&overlaid, (x, y), OURS).map(|w| w.pid);
        assert_eq!(under(120.0, 120.0), Some(20), "overlays are passed over");
        assert_eq!(under(1500.0, 10.0), None, "the desktop under an overlay");
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

    /// R50: the Dock, the menu bar or a floating panel over the point is
    /// no taker, even when Finder owns it or a Finder window lies below.
    #[test]
    fn a_window_on_another_layer_is_no_promise_taker() {
        const DOCK: i32 = 30;
        let targets = FakeTargets {
            windows: Some(vec![
                win(20, DOCK, (0.0, 900.0, 1000.0, 100.0)), // the Dock
                win(24, 10, (0.0, 0.0, 1000.0, 30.0)),      // Finder's menu bar
                win(3, 11, (500.0, 400.0, 100.0, 100.0)),   // a floating WhatsApp panel
                win(0, 10, (0.0, 0.0, 1000.0, 1000.0)),     // a Finder window below
            ]),
            apps: HashMap::from([
                (DOCK, "com.apple.dock"),
                (10, "com.apple.finder"),
                (11, "net.whatsapp.WhatsApp"),
            ]),
        };
        let at = |x, y| promise_taker_at((x, y), &targets, 1);
        assert!(!at(500.0, 950.0), "the Dock");
        assert!(!at(500.0, 10.0), "the menu bar");
        assert!(!at(550.0, 450.0), "a floating panel");
        assert!(at(200.0, 500.0), "the Finder window itself");
        assert!(at(1500.0, 500.0), "the bare desktop");
    }

    /// A drag with `n` file items in `dir`, its placeholders made.
    fn drag_in(dir: &Path, n: usize) -> Drag {
        let items: Vec<DragItem> = (0..n)
            .map(|i| item(&format!("f{i}.txt"), ItemKind::File))
            .collect();
        let staging = dir.join(STAGING).join("1");
        let s = slots(dir, &staging, &items);
        make_placeholders(dir, &s, &[], &dir.join("aside")).unwrap();
        Drag::new(7, 1, s, staging, vec![Item::Fetching; n])
    }

    fn fetched(drag: &Drag, i: usize, bytes: &[u8]) -> PathBuf {
        fs::create_dir_all(&drag.slots[i].staging).unwrap();
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
        assert!(
            matches!(
                evs[..],
                [MacDndEv::Ready {
                    drag: 7,
                    crossing: 1
                }]
            ),
            "{evs:?}"
        );
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
                    crossing: 1,
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
                    crossing: 3,
                    dropped: true,
                },
                Some(InjectMsg::DragEnded {
                    drag: 7,
                    crossing: 3,
                }),
            ),
            (
                MacDndEv::Ready {
                    drag: 7,
                    crossing: 3,
                },
                Some(InjectMsg::DragReady {
                    drag: 7,
                    crossing: 3,
                }),
            ),
            (
                MacDndEv::NeedsEscape {
                    drag: 7,
                    crossing: 3,
                },
                Some(InjectMsg::EscapeSession {
                    drag: 7,
                    crossing: 3,
                }),
            ),
            (
                MacDndEv::Failed {
                    drag: 7,
                    crossing: 3,
                    item: 0,
                    reason: FailReason::Cancelled,
                },
                Some(InjectMsg::DragFailed {
                    drag: 7,
                    crossing: 3,
                }),
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
        assert_eq!(src.prepare(9, Vec::new(), (0.0, 0.0)), None);
        assert_eq!(
            rx.try_recv().unwrap(),
            "Ended { drag: 9, crossing: 0, dropped: false }"
        );
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
        assert_eq!(
            rx.try_recv().unwrap(),
            "Ended { drag: 7, crossing: 1, dropped: false }"
        );
        assert!(rx.try_recv().is_err(), "once");
        assert!(d.lock().cancelled);
    }

    fn source_in(root: &Path) -> (DragSource, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let src = DragSource::with(
            move |ev| {
                let _ = tx.send(format!("{ev:?}"));
            },
            root.into(),
            Box::new(FakeTargets {
                windows: None,
                apps: HashMap::new(),
            }),
        );
        (src, rx)
    }

    /// The press missed the panel: no session began, so the release ends
    /// the drag undropped, says nothing of a release, and lets the up go.
    #[test]
    fn released_without_a_session_ends_the_drag() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, rx) = source_in(tmp.path());
        let d = Arc::new(drag_in(&tmp.path().join("7"), 1));
        src.inner.live().insert(7, d.clone());
        src.released(7, (1.0, 2.0));
        assert_eq!(
            rx.try_recv().unwrap(),
            "Ended { drag: 7, crossing: 1, dropped: false }"
        );
        assert!(rx.try_recv().is_err(), "no Released");
        assert!(d.lock().cancelled);
        assert!(!d.try_begin(), "no session can start after it");
        assert!(src.may_release(7, (1.0, 2.0)), "over: the up goes out");
    }

    /// A session that began first: the release is a release, and the
    /// drag stays.
    #[test]
    fn released_with_a_session_is_a_release() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, rx) = source_in(tmp.path());
        let d = Arc::new(drag_in(&tmp.path().join("7"), 1));
        src.inner.live().insert(7, d.clone());
        assert!(d.try_begin());
        assert!(!d.try_begin(), "only the first down");
        src.released(7, (1.0, 2.0));
        assert_eq!(
            rx.try_recv().unwrap(),
            "Released { drag: 7, at: (1.0, 2.0) }"
        );
        assert!(!d.lock().cancelled);
    }

    /// Cancelled while its session was starting: no Esc yet (it might come
    /// before the session could take it); the session asks for one once
    /// started, and until it has ended the up is never posted early.
    #[test]
    fn a_session_starting_as_it_is_cancelled_asks_for_its_esc() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, rx) = source_in(tmp.path());
        let d = Arc::new(drag_in(&tmp.path().join("7"), 1));
        src.inner.live().insert(7, d.clone());
        assert!(d.try_begin());
        assert!(!src.cancel(7), "not live yet: no Esc from cj-inject");
        assert!(rx.try_recv().is_err(), "the session will end it");
        assert!(
            !src.may_release(7, (0.0, 0.0)),
            "cancelled, its session on, incomplete: hold"
        );
        assert!(!src.session_live(7, 1), "not started yet");
        assert!(d.session_started(), "it needs an Esc");
        assert!(src.session_live(7, 1), "started, not ended: the Esc goes");
        assert!(!src.session_live(7, 2), "another crossing");
        src.inner.end(&d, false);
        assert!(!src.session_live(7, 1), "ended: no stray Esc");
        assert!(src.may_release(7, (0.0, 0.0)), "ended: the up may go");
        // A session that started before the cancel is live: cj-inject's Esc.
        let d = Arc::new(drag_in(&tmp.path().join("8"), 1));
        src.inner.live().insert(7, d.clone());
        assert!(d.try_begin());
        assert!(!d.session_started());
        assert!(src.cancel(7));
    }

    /// Without a main thread serving its queue (as in libtest), the panel
    /// never comes up: `prepare` gives up within its budget, says the drag
    /// ended undropped, and sends no fetch.
    #[test]
    fn prepare_gives_up_within_its_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, rx) = source_in(tmp.path());
        let t0 = Instant::now();
        assert_eq!(
            src.prepare(7, vec![item("a.txt", ItemKind::File)], (5.0, 5.0)),
            None
        );
        let took = t0.elapsed();
        assert!(
            took < PREPARE_TIMEOUT + Duration::from_millis(50),
            "{took:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
        let evs: Vec<String> = rx.try_iter().collect();
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert!(
            evs[0].starts_with("Ended { drag: 7, crossing: ")
                && evs[0].ends_with("dropped: false }"),
            "{evs:?}"
        );
        assert!(src.inner.get(7).is_none());
        assert!(
            tmp.path().join("7").join("a.txt").exists(),
            "placeholder made"
        );
    }

    /// A drag that went home and came back keeps what its first crossing
    /// completed; the first crossing's late fetches land nowhere.
    #[test]
    fn a_drag_that_comes_back_keeps_its_completed_items() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, rx) = source_in(tmp.path());
        let items = vec![
            item("done.txt", ItemKind::File),
            item("late.txt", ItemKind::File),
        ];
        let first = src.stage(7, &items).unwrap();
        src.inner.live().insert(7, first.clone());
        let p = fetched(&first, 0, b"done");
        first.resolve(0, Ok(p));
        // Going home.
        assert!(!src.cancel(7), "no session");
        assert_eq!(
            rx.try_recv().unwrap(),
            format!(
                "Ended {{ drag: 7, crossing: {}, dropped: false }}",
                first.crossing
            )
        );
        // Coming back.
        let second = src.stage(7, &items).unwrap();
        assert_ne!(second.crossing, first.crossing, "a token per crossing");
        assert_eq!(
            second.lock().items,
            [Item::Complete, Item::Fetching],
            "the finished item stays"
        );
        assert_eq!(fs::read(&second.slots[0].placeholder).unwrap(), b"done");
        assert_ne!(first.staging, second.staging, "a staging folder each");
        // The first crossing's late fetch doesn't touch the placeholder now.
        let late = fetched(&first, 1, b"late, from the first crossing");
        assert!(first.resolve(1, Ok(late)).is_empty());
        assert_eq!(fs::metadata(&second.slots[1].placeholder).unwrap().len(), 0);
        // Its own fetch lands.
        let p = fetched(&second, 1, b"late");
        let evs = second.resolve(1, Ok(p));
        assert!(
            matches!(evs[..], [MacDndEv::Ready { drag: 7, .. }]),
            "{evs:?}"
        );
        // With no crossing to take over from, a stale placeholder moves aside.
        let (fresh, _rx) = source_in(tmp.path());
        let third = fresh.stage(7, &items).unwrap();
        assert_eq!(third.lock().items, [Item::Fetching, Item::Fetching]);
        assert_eq!(fs::metadata(&third.slots[0].placeholder).unwrap().len(), 0);
    }
}
