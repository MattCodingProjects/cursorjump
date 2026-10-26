//! Drags arriving from the Mac: the user drags a file there, the cursor
//! comes home with the button held, and cj-dnd-in runs a real `DoDragDrop`,
//! so the file drops into Explorer, WhatsApp, Gmail or Discord as a local one.
//!
//! - The press is already ours: the input hook let the real left-down
//!   through to the shield (R29), so Windows sees the button down and OLE's
//!   modal loop gets mouse input. Without that press `DoDragDrop` is not
//!   run at all (R35): it would get no mouse input.
//! - The data object offers `CF_HDROP` and [`SELF_FORMAT`] only (A4).
//!   Until the drop, `CF_HDROP` names empty placeholders under
//!   `%LOCALAPPDATA%\CursorJump\incoming\<drag>\`, at once: Chromium targets
//!   ask for it every 60 ms while hovering. The first ask after the drop
//!   removes them (the receiver never replaces a file), sends a `Fetch` per
//!   item and waits for every reply; the received paths are kept for
//!   repeat asks.
//! - "After the drop" is from our `QueryContinueDrag` saying DROP. Our COM
//!   objects are agile (windows-rs `#[implement]` aggregates the
//!   free-threaded marshaler), so a target's cross-process `GetData`
//!   arrives on a COM RPC thread, not on cj-dnd-in: the spike's logs show
//!   it, and `smoke_incoming_drag_from_local_file` logs the thread of each
//!   ask. The wait for the bytes happens there. On either of cj-dnd's
//!   threads it never waits. A target's synchronous `Drop` (Explorer's)
//!   still holds cj-dnd-in for the whole fetch, in an outgoing COM call
//!   whose modal loop takes no posted messages: hence cj-dnd-out, apart.
//! - `DoDragDrop` returns ~17 ms after the drop while the target's `GetData`
//!   still waits (spike 5f): the data object lives as long as the target
//!   holds it, and a target that asks only after `DoDragDrop` returned
//!   still gets its fetch. A drag that ends without a drop takes its
//!   placeholders with it, and later asks fail.
//! - The wait can be escaped from this side: `CancelIncoming` and cj-dnd
//!   stopping fail it (`E_FAIL`) within [`REPLY_POLL`]. Both are handled by
//!   the command relay through [`Live`], not on cj-dnd-in, whose message
//!   loop may be held in a target's synchronous `Drop` all along.
//! - Whatever is set on the data object (`IDragSourceHelper`'s image, a
//!   target helper's flags) is kept and served back.
//!
//! [`SELF_FORMAT`]: super::catcher::SELF_FORMAT
use std::cell::Cell;
use std::collections::HashSet;
use std::fs::File;
use std::io::ErrorKind;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cj_core::dnd::{DragId, DragItem, ItemKind, sanitize_component};
use tokio::sync::oneshot::{self, error::TryRecvError};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{
    COLORREF, DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
    DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, DV_E_TYMED, E_FAIL, E_INVALIDARG, E_NOTIMPL,
    E_OUTOFMEMORY, GlobalFree, HANDLE, HGLOBAL, OLE_E_ADVISENOTSUPPORTED, POINT, S_OK, SIZE,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, GdiFlush, HBITMAP, HENHMETAFILE, SelectObject,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemAlloc, DATADIR_GET, FORMATETC, IAdviseSink,
    IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0,
    STREAM_SEEK_SET, TYMED, TYMED_ENHMF, TYMED_FILE, TYMED_GDI, TYMED_HGLOBAL, TYMED_ISTORAGE,
    TYMED_ISTREAM, TYMED_MFPICT, TYMED_NULL,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_BITMAP, CF_ENHMETAFILE, CF_HDROP, CF_METAFILEPICT, CF_PALETTE, CLIPBOARD_FORMAT, DROPEFFECT,
    DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl, OleDuplicateData,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{
    CLSID_DragDropHelper, DROPFILES, IDragSourceHelper, SHCreateStdEnumFmtEtc, SHDRAGIMAGE,
    SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DI_NORMAL, DestroyIcon, DrawIconEx, GetSystemMetrics, HICON, SM_CXICON,
};
use windows::core::{BOOL, HRESULT, HSTRING, IUnknown, Interface, PWSTR, Ref, implement};

use super::catcher::{Medium, formatetc, self_format};
use super::inbound::incoming_let_go;
use super::{DndShared, Emit, WinDndEv, left_button_down};
use cj_core::dnd::FailReason;

/// A placeholder that won't go (a scanner holding it open) is retried this
/// often, this many times, before its fetch goes ahead regardless.
const REMOVE_STEP: Duration = Duration::from_millis(20);
const REMOVE_TRIES: u32 = 10;
/// Most an after-drop wait takes to see it was cancelled.
pub(super) const REPLY_POLL: Duration = Duration::from_millis(50);

thread_local! {
    static DND_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as one of cj-dnd's: nothing here waits for
/// bytes on it.
pub(super) fn mark_dnd_thread() {
    DND_THREAD.with(|t| t.set(true));
}

fn on_dnd_thread() -> bool {
    DND_THREAD.with(Cell::get)
}

/// What `QueryContinueDrag` answers: Esc or a cancel end the drag with no
/// drop; the button up drops.
pub(super) fn continue_decision(escape: bool, left_down: bool, cancelled: bool) -> HRESULT {
    if escape || cancelled {
        DRAGDROP_S_CANCEL
    } else if !left_down {
        DRAGDROP_S_DROP
    } else {
        S_OK
    }
}

/// A `CF_HDROP` block: DROPFILES (wide) and the paths, each NUL-ended, then
/// one more NUL.
pub(super) fn hdrop_bytes(paths: &[PathBuf]) -> Vec<u8> {
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT::default(),
        fNC: false.into(),
        fWide: true.into(),
    };
    // SAFETY: DROPFILES is a plain repr(C) struct with no padding.
    let mut bytes = unsafe {
        std::slice::from_raw_parts(
            (&header as *const DROPFILES).cast::<u8>(),
            size_of::<DROPFILES>(),
        )
    }
    .to_vec();
    let mut units: Vec<u16> = Vec::new();
    for p in paths {
        units.extend(p.as_os_str().encode_wide());
        units.push(0);
    }
    if paths.is_empty() {
        units.push(0);
    }
    units.push(0);
    bytes.extend(units.iter().flat_map(|u| u.to_le_bytes()));
    bytes
}

/// Whether the drag still holds the button: only while Raw Input
/// (`DndShared::left_down`) and OLE's key state (`MK_LBUTTON`) both say so.
/// Either one seeing the release drops, so neither a Raw Input up that went
/// missing nor a stale key state can strand the drag under the cursor. The
/// key state is the real button's: Windows saw our press (R29), and in the
/// spike the two flipped within 1 ms of each other.
pub(super) fn button_held(raw_down: bool, keystate_down: bool) -> bool {
    raw_down && keystate_down
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Start {
    Run,
    Skip(&'static str),
}

/// Whether a `StartIncoming` runs `DoDragDrop`. `windows_sees_down`:
/// `GetAsyncKeyState`, i.e. the press reached one of our windows (R35).
pub(super) fn start_decision(items: usize, session_up: bool, windows_sees_down: bool) -> Start {
    if items == 0 {
        Start::Skip("no items")
    } else if !session_up {
        Start::Skip("no session")
    } else if !windows_sees_down {
        Start::Skip(
            "Windows doesn't see the left button down: the press never reached \
             our window, so DoDragDrop would get no mouse input (R35)",
        )
    } else {
        Start::Run
    }
}

/// Where an item waits: the folder its `Fetch` receives into, and the path
/// its placeholder takes there (the name the receiver will give it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Slot {
    pub(super) dir: PathBuf,
    pub(super) path: PathBuf,
    pub(super) is_dir: bool,
}

/// Every item in `base` under its sanitized name, but for one whose name
/// another item already has (NTFS ignores case): that one gets a folder of
/// its own, named after its index (plus `_`s, if an item has that name).
pub(super) fn slots(base: &Path, items: &[DragItem]) -> Vec<Slot> {
    let names: Vec<String> = items.iter().map(|i| sanitize_component(&i.name)).collect();
    let key = |s: &str| s.to_lowercase();
    let mut taken: HashSet<String> = names.iter().map(|n| key(n)).collect();
    let mut seen = HashSet::new();
    names
        .into_iter()
        .zip(items)
        .enumerate()
        .map(|(i, (name, item))| {
            let dir = if seen.insert(key(&name)) {
                base.to_path_buf()
            } else {
                let mut sub = i.to_string();
                while !taken.insert(key(&sub)) {
                    sub.push('_');
                }
                base.join(sub)
            };
            Slot {
                path: dir.join(&name),
                dir,
                is_dir: item.kind == ItemKind::Dir,
            }
        })
        .collect()
}

/// `%LOCALAPPDATA%\CursorJump\incoming\<drag>`; Task 13 sweeps it.
fn incoming_dir(drag: DragId) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(base)
        .join("CursorJump")
        .join("incoming")
        .join(drag.to_string()))
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Before the drop: `CF_HDROP` names the placeholders.
    Hover,
    /// Our source said DROP; no target has asked yet.
    Dropped,
    /// An ask is waiting for the fetch.
    Fetching,
    /// The received paths, or `None` if a fetch failed.
    Fetched(Option<Vec<PathBuf>>),
    /// Ended without a drop: the placeholders are gone.
    Abandoned,
}

/// One drag from the Mac, shared by its data object and its drop source.
pub(super) struct Incoming {
    drag: DragId,
    base: PathBuf,
    slots: Vec<Slot>,
    events: Emit,
    phase: Mutex<Phase>,
    turned: Condvar,
    /// `CancelIncoming`, or cj-dnd stopping: the drag ends, a running wait
    /// fails, and later asks fail.
    cancelled: Mutex<bool>,
    stop: Condvar,
}

/// The drags from the Mac whose data objects may still be served (a
/// target can hold one long after `DoDragDrop` returned).
pub(super) type Live = Arc<Mutex<Vec<Weak<Incoming>>>>;

fn live_list(live: &Live) -> MutexGuard<'_, Vec<Weak<Incoming>>> {
    live.lock().unwrap_or_else(PoisonError::into_inner)
}

fn register(live: &Live, incoming: &Arc<Incoming>) {
    let mut list = live_list(live);
    list.retain(|w| w.strong_count() > 0);
    list.push(Arc::downgrade(incoming));
}

/// Cancels the live drag `drag` (every one for `None`); how many it reached.
pub(super) fn cancel_live(live: &Live, drag: Option<DragId>) -> usize {
    let found: Vec<Arc<Incoming>> = {
        let mut list = live_list(live);
        list.retain(|w| w.strong_count() > 0);
        list.iter()
            .filter_map(Weak::upgrade)
            .filter(|i| drag.is_none_or(|d| i.drag == d))
            .collect()
    };
    found.iter().for_each(|i| i.cancel());
    found.len()
}

/// How the wait for one reply ended.
#[derive(Debug, PartialEq)]
enum Reply {
    Got(Result<PathBuf, FailReason>),
    /// The app dropped it unanswered.
    Dropped,
    Cancelled,
}

impl Incoming {
    /// The drag's folder `base`, with a placeholder for each item.
    fn create(drag: DragId, base: PathBuf, items: &[DragItem], events: Emit) -> Result<Arc<Self>> {
        let slots = slots(&base, items);
        for (i, s) in slots.iter().enumerate() {
            let made = std::fs::create_dir_all(&s.dir)
                .with_context(|| format!("creating {}", s.dir.display()))
                .and_then(|()| {
                    if s.is_dir {
                        std::fs::create_dir_all(&s.path)
                    } else {
                        File::create(&s.path).map(drop)
                    }
                    .with_context(|| format!("placeholder {}", s.path.display()))
                });
            if made.is_err() {
                undo_create(&base, &slots, i);
            }
            made?;
        }
        Ok(Arc::new(Incoming {
            drag,
            base,
            slots,
            events,
            phase: Mutex::new(Phase::Hover),
            turned: Condvar::new(),
            cancelled: Mutex::new(false),
            stop: Condvar::new(),
        }))
    }

    fn cancel(&self) {
        let mut cancelled = self
            .cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !*cancelled {
            info!(drag = self.drag, "incoming: cancelled");
        }
        *cancelled = true;
        self.stop.notify_all();
    }

    fn is_cancelled(&self) -> bool {
        *self
            .cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// One reply, or the cancel, whichever comes first (the cancel within
    /// `REPLY_POLL`).
    fn await_reply(&self, mut rx: oneshot::Receiver<Result<PathBuf, FailReason>>) -> Reply {
        loop {
            match rx.try_recv() {
                Ok(got) => return Reply::Got(got),
                Err(TryRecvError::Closed) => return Reply::Dropped,
                Err(TryRecvError::Empty) => {}
            }
            let cancelled = self
                .cancelled
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if *cancelled {
                return Reply::Cancelled;
            }
            let _ = self.stop.wait_timeout(cancelled, REPLY_POLL);
        }
    }

    fn phase(&self) -> MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Our source said DROP: the next `CF_HDROP` ask fetches.
    fn dropped(&self) {
        let mut phase = self.phase();
        if *phase == Phase::Hover {
            *phase = Phase::Dropped;
        }
    }

    /// `DoDragDrop` returned. Without a drop (or with one the target
    /// refused) the placeholders go, and later asks fail; with one, the
    /// fetch goes on, or waits for the target's ask.
    fn ended(&self, dropped: bool) {
        if dropped {
            return;
        }
        let mut phase = self.phase();
        if !matches!(*phase, Phase::Hover | Phase::Dropped) {
            return;
        }
        *phase = Phase::Abandoned;
        drop(phase);
        remove_all(&self.base, &self.slots);
        debug!(drag = self.drag, "incoming: placeholders removed");
    }

    /// What `GetData(CF_HDROP)` hands out. After the drop the first ask
    /// fetches every item and waits for them; asks meanwhile wait for it,
    /// and later ones get its result.
    fn hdrop_paths(&self) -> Result<Vec<PathBuf>, HRESULT> {
        let on_dnd = on_dnd_thread();
        let mut phase = self.phase();
        loop {
            let fetch = match &*phase {
                Phase::Hover => return Ok(self.slots.iter().map(|s| s.path.clone()).collect()),
                Phase::Abandoned => return Err(E_FAIL),
                Phase::Fetched(got) => return got.clone().ok_or(E_FAIL),
                Phase::Dropped | Phase::Fetching if on_dnd => {
                    warn!(
                        drag = self.drag,
                        "incoming: the bytes asked for on a cj-dnd thread, which must not wait; refused"
                    );
                    return Err(E_FAIL);
                }
                Phase::Dropped if self.is_cancelled() => {
                    *phase = Phase::Fetched(None);
                    return Err(E_FAIL);
                }
                Phase::Dropped => true,
                Phase::Fetching => false,
            };
            if fetch {
                break;
            }
            phase = self
                .turned
                .wait(phase)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *phase = Phase::Fetching;
        drop(phase);
        let got = self.fetch();
        *self.phase() = Phase::Fetched(got.clone());
        self.turned.notify_all();
        got.ok_or(E_FAIL)
    }

    /// Every item, received where its placeholder was (which goes first:
    /// the receiver never replaces an existing file, R20).
    fn fetch(&self) -> Option<Vec<PathBuf>> {
        let drag = self.drag;
        let started = Instant::now();
        let replies: Vec<_> = self
            .slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                remove_placeholder(slot, REMOVE_TRIES);
                let (reply, rx) = oneshot::channel();
                self.events.send(WinDndEv::Fetch {
                    drag,
                    item: i as u32,
                    dir: slot.dir.clone(),
                    reply,
                });
                rx
            })
            .collect();
        info!(
            drag,
            items = replies.len(),
            "incoming: fetching after the drop"
        );
        let mut paths = Vec::with_capacity(replies.len());
        let mut ok = true;
        for (item, rx) in replies.into_iter().enumerate() {
            // A COM thread, never one of cj-dnd's: waiting here is fine.
            match self.await_reply(rx) {
                Reply::Got(Ok(path)) => paths.push(path),
                Reply::Got(Err(why)) => {
                    warn!(drag, item, ?why, "incoming: fetch failed");
                    ok = false;
                }
                Reply::Dropped => {
                    warn!(drag, item, "incoming: fetch dropped unanswered");
                    ok = false;
                }
                Reply::Cancelled => {
                    // The rest are dropped with the receivers.
                    warn!(drag, item, "incoming: the wait for the bytes was cancelled");
                    ok = false;
                    break;
                }
            }
        }
        info!(drag, ok, took = ?started.elapsed(), "incoming: fetched");
        ok.then_some(paths)
    }
}

/// Removes a placeholder (empty, so a folder one goes with `remove_dir`).
fn remove_placeholder(slot: &Slot, tries: u32) {
    for attempt in 1..=tries {
        let removed = if slot.is_dir {
            std::fs::remove_dir(&slot.path)
        } else {
            std::fs::remove_file(&slot.path)
        };
        match removed {
            Ok(()) => return,
            Err(e) if e.kind() == ErrorKind::NotFound => return,
            Err(_) if attempt < tries => std::thread::sleep(REMOVE_STEP),
            Err(e) => warn!(
                "incoming: placeholder {} stays ({e}); its fetch will fail",
                slot.path.display()
            ),
        }
    }
}

/// What a `create` made before slot `failed` failed, and only that: the
/// earlier placeholders and their folders, and a collision folder made for
/// slot `failed` itself (`remove_dir` takes only an empty one).
fn undo_create(base: &Path, slots: &[Slot], failed: usize) {
    if let Some(s) = slots.get(failed)
        && s.dir != base
    {
        let _ = std::fs::remove_dir(&s.dir);
    }
    remove_all(base, &slots[..failed]);
}

/// The placeholders of `slots` and the folders made for them. On cj-dnd-in
/// (or with a create failing): one try each, no waiting.
fn remove_all(base: &Path, slots: &[Slot]) {
    for s in slots {
        remove_placeholder(s, 1);
    }
    for s in slots {
        if s.dir != base {
            let _ = std::fs::remove_dir(&s.dir);
        }
    }
    let _ = std::fs::remove_dir(base);
}

// ---------------------------------------------------------------- media

pub(super) fn hglobal_from(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: a fresh movable block, locked only for the copy.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let p = GlobalLock(h);
        if p.is_null() {
            let e = windows::core::Error::from_thread();
            let _ = GlobalFree(Some(h));
            return Err(e);
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

pub(super) fn hglobal_medium(h: HGLOBAL) -> STGMEDIUM {
    STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: h },
        pUnkForRelease: ManuallyDrop::new(None),
    }
}

fn copy_hglobal(h: HGLOBAL) -> windows::core::Result<HGLOBAL> {
    // SAFETY: the block is locked for the copy, its size from GlobalSize.
    unsafe {
        let p = GlobalLock(h);
        if p.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        let copy = hglobal_from(std::slice::from_raw_parts(p.cast::<u8>(), GlobalSize(h)));
        let _ = GlobalUnlock(h);
        copy
    }
}

/// A GDI or metafile handle, copied by OLE as `cf`.
fn copy_handle(h: HANDLE, cf: CLIPBOARD_FORMAT) -> windows::core::Result<HANDLE> {
    // SAFETY: plain FFI call; the copy is new and ours.
    let copy = unsafe { OleDuplicateData(h, cf, GMEM_MOVEABLE) };
    if copy.is_invalid() {
        return Err(E_OUTOFMEMORY.into());
    }
    Ok(copy)
}

fn copy_name(name: PWSTR) -> windows::core::Result<PWSTR> {
    // SAFETY: a NUL-ended name, copied with its NUL into CoTaskMem memory,
    // which ReleaseStgMedium frees.
    unsafe {
        let units = name.as_wide();
        let p = CoTaskMemAlloc((units.len() + 1) * 2).cast::<u16>();
        if p.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
        *p.add(units.len()) = 0;
        Ok(PWSTR(p))
    }
}

/// A copy of `m` the caller owns and frees with `ReleaseStgMedium`. An
/// interface is shared (one more reference); handles are copied.
fn duplicate(m: &STGMEDIUM, cf: u16) -> windows::core::Result<STGMEDIUM> {
    let mut holder: Option<IUnknown> = None;
    // SAFETY: `tymed` says which union member is live.
    let u = unsafe {
        match TYMED(m.tymed as i32) {
            TYMED_NULL => STGMEDIUM_0 {
                hGlobal: HGLOBAL::default(),
            },
            TYMED_HGLOBAL => STGMEDIUM_0 {
                hGlobal: copy_hglobal(m.u.hGlobal)?,
            },
            TYMED_ISTREAM => STGMEDIUM_0 {
                pstm: ManuallyDrop::new((*m.u.pstm).clone()),
            },
            TYMED_ISTORAGE => STGMEDIUM_0 {
                pstg: ManuallyDrop::new((*m.u.pstg).clone()),
            },
            TYMED_GDI => {
                let as_cf = if cf == CF_PALETTE.0 {
                    CF_PALETTE
                } else {
                    CF_BITMAP
                };
                STGMEDIUM_0 {
                    hBitmap: HBITMAP(copy_handle(HANDLE(m.u.hBitmap.0), as_cf)?.0),
                }
            }
            TYMED_MFPICT => STGMEDIUM_0 {
                hMetaFilePict: copy_handle(HANDLE(m.u.hMetaFilePict), CF_METAFILEPICT)?.0,
            },
            TYMED_ENHMF => STGMEDIUM_0 {
                hEnhMetaFile: HENHMETAFILE(
                    copy_handle(HANDLE(m.u.hEnhMetaFile.0), CF_ENHMETAFILE)?.0,
                ),
            },
            TYMED_FILE => {
                // ReleaseStgMedium deletes a file it gets with no
                // pUnkForRelease: with one (any object) it frees only the
                // name, and the file stays ours.
                holder = Some(CreateStreamOnHGlobal(HGLOBAL::default(), true)?.cast()?);
                STGMEDIUM_0 {
                    lpszFileName: copy_name(m.u.lpszFileName)?,
                }
            }
            _ => return Err(DV_E_TYMED.into()),
        }
    };
    Ok(STGMEDIUM {
        tymed: m.tymed,
        u,
        pUnkForRelease: ManuallyDrop::new(holder),
    })
}

/// A stream handed out from what was set: a clone with a seek pointer of
/// its own, at the start, so readers don't move each other's. A stream
/// that can't clone is shared, rewound.
///
/// # Safety
/// `m` holds a stream (`TYMED_ISTREAM`).
unsafe fn own_seek(m: &mut STGMEDIUM) {
    // SAFETY: per the contract; `pstm` is replaced by our own reference.
    unsafe {
        let Some(shared) = (*m.u.pstm).clone() else {
            return;
        };
        let stream = match shared.Clone() {
            Ok(own) => {
                ManuallyDrop::drop(&mut m.u.pstm);
                m.u.pstm = ManuallyDrop::new(Some(own.clone()));
                own
            }
            Err(_) => shared,
        };
        let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    }
}

// ---------------------------------------------------------------- IDataObject

/// A format set on the data object, and its medium.
struct Stored {
    format: FORMATETC,
    medium: Medium,
}

#[implement(IDataObject)]
pub(super) struct DataObject {
    incoming: Arc<Incoming>,
    offered: [FORMATETC; 2],
    /// Whatever was set on it, one per format.
    stored: Mutex<Vec<Stored>>,
    /// Hover asks are logged once, not every 60 ms.
    hover_logged: AtomicBool,
}

impl DataObject {
    fn new(incoming: Arc<Incoming>) -> DataObject {
        DataObject {
            incoming,
            offered: [
                formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1),
                formatetc(self_format(), TYMED_HGLOBAL, -1),
            ],
            stored: Mutex::new(Vec::new()),
            hover_logged: AtomicBool::new(false),
        }
    }

    fn stored(&self) -> MutexGuard<'_, Vec<Stored>> {
        self.stored.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn hdrop(&self) -> windows::core::Result<Vec<PathBuf>> {
        let drag = self.incoming.drag;
        let hover = *self.incoming.phase() == Phase::Hover;
        let thread = std::thread::current();
        let (name, id) = (thread.name().unwrap_or("?"), thread.id());
        if !hover || !self.hover_logged.swap(true, Ordering::Relaxed) {
            let on_dnd = on_dnd_thread();
            debug!(
                drag,
                hover,
                thread = name,
                ?id,
                on_dnd,
                "incoming: GetData(CF_HDROP)"
            );
        }
        let started = Instant::now();
        let paths = self.incoming.hdrop_paths();
        if !hover {
            let ok = paths.is_ok();
            debug!(drag, ok, took = ?started.elapsed(), thread = name, "incoming: GetData(CF_HDROP) answered");
        }
        paths.map_err(Into::into)
    }
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if pformatetcin.is_null() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: OLE passes a valid FORMATETC.
        let f = unsafe { &*pformatetcin };
        if f.cfFormat == CF_HDROP.0 || f.cfFormat == self_format() {
            if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                return Err(DV_E_TYMED.into());
            }
            let bytes = if f.cfFormat == CF_HDROP.0 {
                hdrop_bytes(&self.hdrop()?)
            } else {
                std::process::id().to_le_bytes().to_vec()
            };
            return Ok(hglobal_medium(hglobal_from(&bytes)?));
        }
        let stored = self.stored();
        let s = stored
            .iter()
            .find(|s| s.format.cfFormat == f.cfFormat && s.format.tymed & f.tymed != 0)
            .ok_or(DV_E_FORMATETC)?;
        let mut copy = duplicate(&s.medium.0, f.cfFormat)?;
        if copy.tymed == TYMED_ISTREAM.0 as u32 {
            // SAFETY: tymed says the union holds a stream; the copy is ours.
            unsafe { own_seek(&mut copy) };
        }
        Ok(copy)
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if pformatetc.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: OLE passes a valid FORMATETC.
        let f = unsafe { &*pformatetc };
        let tymed = if f.cfFormat == CF_HDROP.0 || f.cfFormat == self_format() {
            TYMED_HGLOBAL.0 as u32
        } else {
            match self
                .stored()
                .iter()
                .find(|s| s.format.cfFormat == f.cfFormat)
            {
                Some(s) => s.format.tymed,
                None => return DV_E_FORMATETC,
            }
        };
        if tymed & f.tymed != 0 {
            S_OK
        } else {
            DV_E_TYMED
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        if pformatectin.is_null() || pformatetcout.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: OLE passes valid pointers.
        unsafe {
            *pformatetcout = FORMATETC {
                ptd: std::ptr::null_mut(),
                ..*pformatectin
            };
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> windows::core::Result<()> {
        if pformatetc.is_null() || pmedium.is_null() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: OLE passes valid pointers.
        let (f, m) = unsafe { (&*pformatetc, &*pmedium) };
        let medium = if frelease.as_bool() {
            // SAFETY: with fRelease the medium is ours from here on.
            Medium(unsafe { std::ptr::read(m) })
        } else {
            Medium(duplicate(m, f.cfFormat)?)
        };
        debug!(
            drag = self.incoming.drag,
            format = f.cfFormat,
            tymed = m.tymed,
            "incoming: SetData kept"
        );
        let format = FORMATETC {
            ptd: std::ptr::null_mut(),
            tymed: m.tymed,
            ..*f
        };
        let mut stored = self.stored();
        // The old medium, if any, is released here.
        stored.retain(|s| s.format.cfFormat != f.cfFormat);
        stored.push(Stored { format, medium });
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let mut all = self.offered.to_vec();
        all.extend(self.stored().iter().map(|s| s.format));
        // SAFETY: the shell copies the slice.
        unsafe { SHCreateStdEnumFmtEtc(&all) }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

// ---------------------------------------------------------------- IDropSource

/// Called on cj-dnd-in, from `DoDragDrop`'s modal loop.
#[implement(IDropSource)]
struct DropSource {
    shared: Arc<DndShared>,
    incoming: Arc<Incoming>,
    /// `QueryContinueDrag` was called: `DoDragDrop` has the mouse.
    started: AtomicBool,
}

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        let drag = self.incoming.drag;
        // The first call: OLE holds its own capture now, so the shield
        // (whose press started it) can go. Fallback, should the hands-on
        // smoke show the drag stop following once the shield vanishes: let
        // go at DROP/CANCEL instead (`hr != S_OK` below).
        if !self.started.swap(true, Ordering::Relaxed) {
            incoming_let_go(drag);
        }
        let raw = self.shared.left_down.load(Ordering::Acquire);
        let keys = grfkeystate.0 & MK_LBUTTON.0 != 0;
        let escape = fescapepressed.as_bool();
        let cancelled = self.incoming.is_cancelled();
        let hr = continue_decision(escape, button_held(raw, keys), cancelled);
        if hr == DRAGDROP_S_DROP {
            info!(drag, raw, keys, "incoming: released; drop");
            self.incoming.dropped();
        } else if hr == DRAGDROP_S_CANCEL {
            info!(drag, escape, cancelled, "incoming: drag cancelled");
        }
        hr
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

// ---------------------------------------------------------------- the drag

/// One `StartIncoming`, as cj-dnd-in runs it.
pub(super) struct Run {
    pub(super) drag: DragId,
    pub(super) items: Vec<DragItem>,
    pub(super) live: Live,
    pub(super) shared: Arc<DndShared>,
    pub(super) events: Emit,
}

/// `DoDragDrop` for a drag from the Mac, on cj-dnd-in and outside its state
/// (the modal loop dispatches commands: `CancelIncoming`). `IncomingEnded`
/// follows, whether it ran or not.
pub(super) fn run_incoming(run: Run) {
    let Run {
        drag,
        items,
        live,
        shared,
        events,
    } = run;
    let ended = |dropped| events.send(WinDndEv::IncomingEnded { drag, dropped });
    let session_up = shared.session_up.load(Ordering::Acquire);
    if let Start::Skip(why) = start_decision(items.len(), session_up, left_button_down()) {
        info!(drag, why, "incoming: DoDragDrop not run");
        return ended(false);
    }
    let made =
        incoming_dir(drag).and_then(|base| Incoming::create(drag, base, &items, events.clone()));
    let incoming = match made {
        Ok(incoming) => incoming,
        Err(e) => {
            warn!(drag, "incoming: no placeholders: {e:#}");
            return ended(false);
        }
    };
    // Before DoDragDrop: a CancelIncoming reaches it from here on.
    register(&live, &incoming);
    let data: IDataObject = DataObject::new(incoming.clone()).into();
    let source: IDropSource = DropSource {
        shared,
        incoming: incoming.clone(),
        started: AtomicBool::new(false),
    }
    .into();
    set_drag_image(&data, &items[0]);
    info!(drag, items = items.len(), "incoming: DoDragDrop");
    let started = Instant::now();
    let mut effect = DROPEFFECT_NONE;
    // SAFETY: both objects are live; this is cj-dnd-in, an STA with OLE.
    let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
    let dropped = effect != DROPEFFECT_NONE;
    let took = started.elapsed();
    info!(
        drag,
        ?hr,
        effect = effect.0,
        dropped,
        ?took,
        "incoming: DoDragDrop returned"
    );
    incoming.ended(dropped);
    ended(dropped);
}

/// The first item's shell icon as the drag image. Failing costs only the image.
fn set_drag_image(data: &IDataObject, item: &DragItem) {
    let Some((bitmap, side)) = icon_bitmap(item) else {
        debug!("incoming: no icon for the drag image");
        return;
    };
    let shdi = SHDRAGIMAGE {
        sizeDragImage: SIZE { cx: side, cy: side },
        ptOffset: POINT {
            x: side / 2,
            y: side / 2,
        },
        hbmpDragImage: bitmap,
        // CLR_NONE: the bitmap's own alpha.
        crColorKey: COLORREF(u32::MAX),
    };
    // SAFETY: the shell's drag image helper, on this thread; it calls our
    // SetData, and on success owns the bitmap.
    let set = unsafe {
        CoCreateInstance::<_, IDragSourceHelper>(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER)
            .and_then(|h| h.InitializeFromBitmap(&shdi, data))
    };
    if let Err(e) = set {
        warn!("incoming: no drag image: {e}");
        // SAFETY: our own bitmap, which the helper didn't take.
        let _ = unsafe { DeleteObject(bitmap.into()) };
    }
}

/// The item's large shell icon, by its name and kind only (nothing is
/// read from disk), on a bitmap `side` px square.
fn icon_bitmap(item: &DragItem) -> Option<(HBITMAP, i32)> {
    let attrs = if item.kind == ItemKind::Dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let name = HSTRING::from(if item.name.is_empty() {
        "file"
    } else {
        item.name.as_str()
    });
    let mut info = SHFILEINFOW::default();
    // SAFETY: `info` is as large as we say.
    let got = unsafe {
        SHGetFileInfoW(
            &name,
            attrs,
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_USEFILEATTRIBUTES | SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if got == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let bitmap = draw_icon(info.hIcon);
    // SAFETY: the icon SHGetFileInfoW made for us, destroyed once.
    let _ = unsafe { DestroyIcon(info.hIcon) };
    bitmap
}

/// `icon` on a new 32-bit premultiplied top-down bitmap, `SM_CXICON` square.
fn draw_icon(icon: HICON) -> Option<(HBITMAP, i32)> {
    // SAFETY: plain GDI calls on objects made here: the DC is freed, the
    // bitmap returned or freed; its bits are read after GdiFlush.
    unsafe {
        let side = GetSystemMetrics(SM_CXICON).max(16);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: side,
                biHeight: -side,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, bitmap.into());
        let drawn = DrawIconEx(dc, 0, 0, icon, side, side, 0, None, DI_NORMAL);
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let _ = GdiFlush();
        if drawn.is_err() || bits.is_null() {
            let _ = DeleteObject(bitmap.into());
            return None;
        }
        let px = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (side * side) as usize);
        // An icon with no alpha channel: shown opaque.
        if px.iter().all(|p| p >> 24 == 0) {
            px.iter_mut().for_each(|p| *p |= 0xFF00_0000);
        }
        Some((bitmap, side))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use windows::Win32::System::Com::IAgileObject;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;

    #[test]
    fn continue_decision_table() {
        // (escape, left_down, cancelled) -> result
        let table = [
            ((false, true, false), S_OK),
            ((false, false, false), DRAGDROP_S_DROP),
            ((true, true, false), DRAGDROP_S_CANCEL),
            ((true, false, false), DRAGDROP_S_CANCEL),
            ((false, true, true), DRAGDROP_S_CANCEL),
            ((false, false, true), DRAGDROP_S_CANCEL),
            ((true, true, true), DRAGDROP_S_CANCEL),
        ];
        for ((escape, down, cancelled), want) in table {
            assert_eq!(
                continue_decision(escape, down, cancelled),
                want,
                "escape {escape}, left_down {down}, cancelled {cancelled}"
            );
        }
    }

    /// The paths in a DROPFILES block, read as `DragQueryFileW` would.
    fn dropfiles_paths(bytes: &[u8]) -> Vec<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        let start = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let units: Vec<u16> = bytes[start..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        units
            .split(|&u| u == 0)
            .take_while(|s| !s.is_empty())
            .map(|s| PathBuf::from(std::ffi::OsString::from_wide(s)))
            .collect()
    }

    #[test]
    fn hdrop_bytes_layout() {
        let a = PathBuf::from(r"C:\Users\x\AppData\Local\CursorJump\incoming\7\report.pdf");
        let b = PathBuf::from(r"C:\in\7\東京 photos");
        let bytes = hdrop_bytes(&[a.clone(), b.clone()]);
        // DROPFILES: pFiles, pt (x, y), fNC, fWide.
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 20);
        assert_eq!(&bytes[4..12], &[0; 8]);
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1);
        let wide = |p: &Path| p.to_str().unwrap().encode_utf16().count() * 2;
        assert_eq!(bytes.len(), 20 + wide(&a) + 2 + wide(&b) + 2 + 2);
        assert_eq!(&bytes[bytes.len() - 4..], &[0; 4], "double NUL at the end");
        assert_eq!(dropfiles_paths(&bytes), [a, b]);
    }

    #[test]
    fn start_needs_the_press_windows_saw() {
        assert_eq!(start_decision(1, true, true), Start::Run);
        // R35: without our own press DoDragDrop gets no mouse input at all.
        assert!(matches!(start_decision(1, true, false), Start::Skip(_)));
        assert!(matches!(start_decision(1, false, true), Start::Skip(_)));
        assert!(matches!(start_decision(0, true, true), Start::Skip(_)));
    }

    fn item(name: &str, kind: ItemKind) -> DragItem {
        DragItem {
            name: name.into(),
            kind,
            size: None,
        }
    }

    #[test]
    fn placeholders_take_the_sanitized_names() {
        let base = Path::new(r"C:\in\7");
        let got = slots(
            base,
            &[
                item("report.pdf", ItemKind::File),
                item("Photos", ItemKind::Dir),
                item("a:b?.txt", ItemKind::File),
                item("CON", ItemKind::File),
                item("", ItemKind::File),
            ],
        );
        let slot = |name: &str, is_dir| Slot {
            dir: base.into(),
            path: base.join(name),
            is_dir,
        };
        assert_eq!(
            got,
            [
                slot("report.pdf", false),
                slot("Photos", true),
                slot("a_b_.txt", false),
                slot("_CON", false),
                slot("_", false),
            ]
        );
    }

    #[test]
    fn colliding_names_get_a_folder_of_their_own() {
        let base = Path::new(r"C:\in\7");
        let got = slots(
            base,
            &[
                item("2", ItemKind::File),
                item("a.txt", ItemKind::File),
                // NTFS ignores case: the same file as item 1.
                item("A.TXT", ItemKind::File),
                // Sanitizes to item 1's name too.
                item("a.txt.", ItemKind::Dir),
            ],
        );
        assert_eq!(got[0].path, base.join("2"));
        assert_eq!(got[1].path, base.join("a.txt"));
        // Its own folder, named after its index, unless an item has that name.
        assert_eq!(got[2].dir, base.join("2_"));
        assert_eq!(got[2].path, base.join("2_").join("A.TXT"));
        assert_eq!(got[3].dir, base.join("3"));
        assert_eq!(got[3].path, base.join("3").join("a.txt"));
        assert!(got[3].is_dir);
    }

    // ------------------------------------------------ the data object

    fn emitter() -> (Emit, mpsc::Receiver<WinDndEv>) {
        let (tx, rx) = mpsc::channel();
        let emit = Emit::new(Box::new(move |ev| {
            let _ = tx.send(ev);
        }));
        (emit, rx)
    }

    fn get(data: &IDataObject, cf: u16, tymed: TYMED) -> windows::core::Result<Medium> {
        // SAFETY: plain COM call; we own the result.
        unsafe { data.GetData(&formatetc(cf, tymed, -1)) }.map(Medium)
    }

    fn query(data: &IDataObject, cf: u16, tymed: TYMED) -> HRESULT {
        // SAFETY: plain COM call.
        unsafe { data.QueryGetData(&formatetc(cf, tymed, -1)) }
    }

    fn bytes_of(m: &Medium) -> Vec<u8> {
        assert_eq!(m.0.tymed, TYMED_HGLOBAL.0 as u32);
        // SAFETY: an HGLOBAL medium, locked for the copy.
        unsafe {
            let h = m.0.u.hGlobal;
            let p = GlobalLock(h);
            let v = std::slice::from_raw_parts(p.cast::<u8>(), GlobalSize(h)).to_vec();
            let _ = GlobalUnlock(h);
            v
        }
    }

    fn hdrop_of(data: &IDataObject) -> windows::core::Result<Vec<PathBuf>> {
        get(data, CF_HDROP.0, TYMED_HGLOBAL).map(|m| dropfiles_paths(&bytes_of(&m)))
    }

    fn fetch_of(
        ev: WinDndEv,
    ) -> (
        DragId,
        u32,
        PathBuf,
        oneshot::Sender<Result<PathBuf, FailReason>>,
    ) {
        match ev {
            WinDndEv::Fetch {
                drag,
                item,
                dir,
                reply,
            } => (drag, item, dir, reply),
            other => panic!("not a fetch: {other:?}"),
        }
    }

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn hover_names_placeholders_then_the_drop_fetches_once() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("7");
        let (emit, events) = emitter();
        let items = [
            item("report.pdf", ItemKind::File),
            item("Photos", ItemKind::Dir),
        ];
        let incoming = Incoming::create(7, base.clone(), &items, emit).unwrap();
        let data: IDataObject = DataObject::new(incoming.clone()).into();
        let hover = hdrop_of(&data).unwrap();
        assert_eq!(hover, [base.join("report.pdf"), base.join("Photos")]);
        assert_eq!(hover[0].metadata().unwrap().len(), 0);
        assert!(hover[1].is_dir());
        assert!(events.try_recv().is_err(), "no fetch while hovering");
        incoming.dropped();
        // The app's side: each item received where it is asked for.
        let app = std::thread::spawn(move || {
            for _ in 0..2 {
                let (drag, item, dir, reply) = fetch_of(events.recv_timeout(WAIT).unwrap());
                assert_eq!(drag, 7);
                let to = dir.join(["report.pdf", "Photos"][item as usize]);
                assert!(!to.exists(), "the placeholder goes before the fetch");
                if item == 0 {
                    std::fs::write(&to, b"pdf").unwrap();
                } else {
                    std::fs::create_dir(&to).unwrap();
                }
                reply.send(Ok(to)).unwrap();
            }
            events
        });
        let got = hdrop_of(&data).unwrap();
        let events = app.join().unwrap();
        assert_eq!(got, hover);
        assert_eq!(std::fs::read(&got[0]).unwrap(), b"pdf");
        // Kept: a repeat ask fetches nothing.
        assert_eq!(hdrop_of(&data).unwrap(), got);
        // DoDragDrop returning (~17 ms after the drop) changes nothing.
        incoming.ended(true);
        assert_eq!(hdrop_of(&data).unwrap(), got);
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn asks_during_the_fetch_wait_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (emit, events) = emitter();
        let items = [item("a.bin", ItemKind::File)];
        let incoming = Incoming::create(8, tmp.path().join("8"), &items, emit).unwrap();
        incoming.dropped();
        let asks: Vec<_> = (0..3)
            .map(|_| {
                let incoming = incoming.clone();
                std::thread::spawn(move || incoming.hdrop_paths())
            })
            .collect();
        let (_, _, dir, reply) = fetch_of(events.recv_timeout(WAIT).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        let to = dir.join("a.bin");
        std::fs::write(&to, b"x").unwrap();
        reply.send(Ok(to.clone())).unwrap();
        for ask in asks {
            assert_eq!(ask.join().unwrap(), Ok(vec![to.clone()]));
        }
        assert!(events.try_recv().is_err(), "one fetch for every ask");
    }

    #[test]
    fn a_failed_fetch_fails_every_ask() {
        let tmp = tempfile::tempdir().unwrap();
        let (emit, events) = emitter();
        let items = [item("a.txt", ItemKind::File), item("b.txt", ItemKind::File)];
        let incoming = Incoming::create(9, tmp.path().join("9"), &items, emit).unwrap();
        let data: IDataObject = DataObject::new(incoming.clone()).into();
        incoming.dropped();
        let app = std::thread::spawn(move || {
            let (_, _, _, a) = fetch_of(events.recv_timeout(WAIT).unwrap());
            let (_, _, _, b) = fetch_of(events.recv_timeout(WAIT).unwrap());
            a.send(Err(FailReason::Unreadable {
                name: "a.txt".into(),
            }))
            .unwrap();
            // Never answered: the app dropped it.
            drop(b);
            events
        });
        assert_eq!(hdrop_of(&data).unwrap_err().code(), E_FAIL);
        let events = app.join().unwrap();
        assert_eq!(hdrop_of(&data).unwrap_err().code(), E_FAIL);
        assert!(events.try_recv().is_err(), "no fetch again");
    }

    #[test]
    fn a_drag_with_no_drop_takes_its_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("10");
        let (emit, events) = emitter();
        let items = [
            item("a.txt", ItemKind::File),
            item("A.txt", ItemKind::File),
            item("Photos", ItemKind::Dir),
        ];
        let incoming = Incoming::create(10, base.clone(), &items, emit).unwrap();
        let data: IDataObject = DataObject::new(incoming.clone()).into();
        assert_eq!(hdrop_of(&data).unwrap().len(), 3);
        assert!(base.join("1").join("A.txt").is_file());
        incoming.ended(false);
        assert!(!base.exists(), "placeholders and folders gone");
        // An ask after the end gets nothing, and fetches nothing.
        assert_eq!(hdrop_of(&data).unwrap_err().code(), E_FAIL);
        incoming.dropped();
        assert_eq!(hdrop_of(&data).unwrap_err().code(), E_FAIL);
        assert!(events.try_recv().is_err());
        // A drop the target refused (DROP, then effect NONE): the same.
        let incoming = Incoming::create(11, tmp.path().join("11"), &items, emitter().0).unwrap();
        incoming.dropped();
        incoming.ended(false);
        assert!(!tmp.path().join("11").exists());
        assert_eq!(incoming.hdrop_paths(), Err(E_FAIL));
    }

    #[test]
    fn the_catchers_drop_description_is_what_the_shell_reads() {
        use windows::Win32::System::Com::{DVASPECT_CONTENT, TYMED_HGLOBAL};
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::Win32::UI::Shell::{
            CFSTR_DROPDESCRIPTION, DROPDESCRIPTION, DROPIMAGE_INVALID, DROPIMAGE_NOIMAGE,
        };
        let tmp = tempfile::tempdir().unwrap();
        let incoming = Incoming::create(
            13,
            tmp.path().join("13"),
            &[item("a.txt", ItemKind::File)],
            emitter().0,
        )
        .unwrap();
        // Our own data object keeps any SetData, as a shell one does.
        let data: IDataObject = DataObject::new(incoming).into();
        let read = || {
            // SAFETY: registering a name; then plain COM calls on our own
            // object, the medium read while it is held and released after.
            unsafe {
                let f = FORMATETC {
                    cfFormat: RegisterClipboardFormatW(CFSTR_DROPDESCRIPTION) as u16,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                };
                let mut m = data.GetData(&f).unwrap();
                let h = m.u.hGlobal;
                assert_eq!(GlobalSize(h), size_of::<DROPDESCRIPTION>());
                let p = GlobalLock(h).cast::<DROPDESCRIPTION>();
                let d = std::ptr::read_unaligned(p);
                let _ = GlobalUnlock(h);
                windows::Win32::System::Ole::ReleaseStgMedium(&mut m);
                d
            }
        };
        super::super::catcher::set_drop_description(&data, DROPIMAGE_NOIMAGE).unwrap();
        // Packed: copy the fields out before looking at them.
        let d = read();
        let (kind, message, insert) = (d.r#type, d.szMessage, d.szInsert);
        assert_eq!(kind, DROPIMAGE_NOIMAGE);
        assert!(message.iter().all(|&c| c == 0), "no words");
        assert!(insert.iter().all(|&c| c == 0));
        super::super::catcher::set_drop_description(&data, DROPIMAGE_INVALID).unwrap();
        let kind = read().r#type;
        assert_eq!(kind, DROPIMAGE_INVALID);
    }

    #[test]
    fn the_bytes_are_never_awaited_on_cj_dnd() {
        let tmp = tempfile::tempdir().unwrap();
        let (emit, events) = emitter();
        let items = [item("a.txt", ItemKind::File)];
        let incoming = Incoming::create(12, tmp.path().join("12"), &items, emit).unwrap();
        let on_dnd = incoming.clone();
        std::thread::spawn(move || {
            mark_dnd_thread();
            // Hover asks are answered on it: they don't wait.
            assert_eq!(on_dnd.hdrop_paths().unwrap().len(), 1);
            on_dnd.dropped();
            assert_eq!(on_dnd.hdrop_paths(), Err(E_FAIL));
        })
        .join()
        .unwrap();
        assert!(events.try_recv().is_err(), "no fetch from cj-dnd");
        assert_eq!(
            *incoming.phase(),
            Phase::Dropped,
            "another thread may still fetch"
        );
    }

    fn register_format(name: &str) -> u16 {
        // SAFETY: registering a format name has no preconditions.
        unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) as u16 }
    }

    fn enum_formats(data: &IDataObject) -> Vec<u16> {
        // SAFETY: plain COM calls.
        let e = unsafe { data.EnumFormatEtc(DATADIR_GET.0 as u32) }.unwrap();
        let mut out = vec![];
        loop {
            let mut f = [FORMATETC::default()];
            let mut n = 0;
            if unsafe { e.Next(&mut f, Some(&mut n)) } != S_OK || n == 0 {
                return out;
            }
            out.push(f[0].cfFormat);
        }
    }

    #[test]
    fn anything_set_is_kept_and_served_back() {
        let tmp = tempfile::tempdir().unwrap();
        let items = [item("a.txt", ItemKind::File)];
        let incoming = Incoming::create(13, tmp.path().join("13"), &items, emitter().0).unwrap();
        let data: IDataObject = DataObject::new(incoming).into();
        let blob = register_format("CursorJump.Test.Blob");
        let fmt = formatetc(blob, TYMED_HGLOBAL, -1);
        // fRelease: the data object owns the medium from here on.
        let m = hglobal_medium(hglobal_from(b"first").unwrap());
        unsafe { data.SetData(&fmt, &m, true) }.unwrap();
        assert_eq!(query(&data, blob, TYMED_HGLOBAL), S_OK);
        assert_eq!(
            bytes_of(&get(&data, blob, TYMED_HGLOBAL).unwrap()),
            b"first"
        );
        // Without it, a copy is kept, and the caller frees its own.
        let m = Medium(hglobal_medium(hglobal_from(b"second").unwrap()));
        unsafe { data.SetData(&fmt, &m.0, false) }.unwrap();
        drop(m);
        assert_eq!(
            bytes_of(&get(&data, blob, TYMED_HGLOBAL).unwrap()),
            b"second"
        );
        // An IStream, as IDragSourceHelper sets its DragContext.
        let ctx = register_format("CursorJump.Test.Stream");
        let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }.unwrap();
        unsafe { stream.Write(b"ctx".as_ptr().cast(), 3, None) }
            .ok()
            .unwrap();
        let m = Medium(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        });
        unsafe { data.SetData(&formatetc(ctx, TYMED_ISTREAM, -1), &m.0, false) }.unwrap();
        drop(m);
        assert_eq!(query(&data, ctx, TYMED_ISTREAM), S_OK);
        assert_eq!(query(&data, ctx, TYMED_HGLOBAL), DV_E_TYMED);
        // Each reader gets the stream from its start, with a seek pointer
        // of its own (the setter left it at the end, after writing).
        let read_all = |m: &Medium| {
            assert_eq!(m.0.tymed, TYMED_ISTREAM.0 as u32);
            let s = unsafe { (*m.0.u.pstm).clone() }.unwrap();
            let mut buf = [0u8; 8];
            let mut n = 0;
            unsafe { s.Read(buf.as_mut_ptr().cast(), 8, Some(&mut n)) }
                .ok()
                .unwrap();
            buf[..n as usize].to_vec()
        };
        let first = get(&data, ctx, TYMED_ISTREAM).unwrap();
        let second = get(&data, ctx, TYMED_ISTREAM).unwrap();
        assert_eq!(read_all(&first), b"ctx");
        assert_eq!(read_all(&second), b"ctx", "not moved by the first reader");
        assert_eq!(read_all(&first), b"");
        // Listed after what is offered, each format once.
        assert_eq!(enum_formats(&data), [CF_HDROP.0, self_format(), blob, ctx]);
        assert_eq!(
            query(
                &data,
                register_format("CursorJump.Test.Never"),
                TYMED_HGLOBAL
            ),
            DV_E_FORMATETC
        );
        assert_eq!(query(&data, self_format(), TYMED_HGLOBAL), S_OK);
        assert_eq!(
            bytes_of(&get(&data, self_format(), TYMED_HGLOBAL).unwrap()),
            std::process::id().to_le_bytes()
        );
    }

    #[test]
    fn both_sources_must_hold_the_button() {
        // (Raw Input down, OLE key state down) -> held
        assert!(button_held(true, true));
        // A Raw Input up that went missing: the key state still drops.
        assert!(!button_held(true, false));
        // A stale key state: Raw Input's up still drops.
        assert!(!button_held(false, true));
        assert!(!button_held(false, false));
    }

    #[test]
    fn the_drop_source_reads_both_buttons_and_the_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let items = [item("a.txt", ItemKind::File)];
        let incoming = Incoming::create(15, tmp.path().join("15"), &items, emitter().0).unwrap();
        let shared = Arc::new(DndShared::default());
        let source: IDropSource = DropSource {
            shared: shared.clone(),
            incoming: incoming.clone(),
            started: AtomicBool::new(false),
        }
        .into();
        let ask = |keys| unsafe { source.QueryContinueDrag(false, keys) };
        shared.left_down.store(true, Ordering::Release);
        assert_eq!(ask(MK_LBUTTON), S_OK);
        assert_eq!(ask(MODIFIERKEYS_FLAGS(0)), DRAGDROP_S_DROP);
        assert_eq!(*incoming.phase(), Phase::Dropped);
        incoming.cancel();
        assert_eq!(ask(MK_LBUTTON), DRAGDROP_S_CANCEL);
    }

    type Ask = std::thread::JoinHandle<Result<Vec<PathBuf>, HRESULT>>;

    /// An after-drop ask, waiting on `item 0`'s reply on its own thread.
    fn ask_after_drop(drag: DragId, tmp: &Path) -> (Arc<Incoming>, mpsc::Receiver<WinDndEv>, Ask) {
        let (emit, events) = emitter();
        let items = [item("a.bin", ItemKind::File)];
        let incoming = Incoming::create(drag, tmp.join(drag.to_string()), &items, emit).unwrap();
        incoming.dropped();
        let asking = incoming.clone();
        let ask = std::thread::spawn(move || asking.hdrop_paths());
        (incoming, events, ask)
    }

    #[test]
    fn a_cancel_ends_the_wait_for_an_unanswered_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let (incoming, events, ask) = ask_after_drop(16, tmp.path());
        // Held, never sent.
        let (_, _, _, _reply) = fetch_of(events.recv_timeout(WAIT).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!ask.is_finished(), "still waiting for the reply");
        let cancelled = Instant::now();
        incoming.cancel();
        assert_eq!(ask.join().unwrap(), Err(E_FAIL));
        assert!(
            cancelled.elapsed() < REPLY_POLL * 4,
            "{:?}",
            cancelled.elapsed()
        );
        // Kept: later asks fail at once, fetching nothing.
        assert_eq!(incoming.hdrop_paths(), Err(E_FAIL));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn a_drag_cancelled_before_the_ask_fetches_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (emit, events) = emitter();
        let items = [item("a.bin", ItemKind::File)];
        let incoming = Incoming::create(17, tmp.path().join("17"), &items, emit).unwrap();
        incoming.dropped();
        incoming.cancel();
        assert_eq!(incoming.hdrop_paths(), Err(E_FAIL));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn the_relay_cancels_live_drags_and_all_at_its_end() {
        let tmp = tempfile::tempdir().unwrap();
        let items = [item("a.bin", ItemKind::File)];
        let make = |drag: DragId| {
            Incoming::create(drag, tmp.path().join(drag.to_string()), &items, emitter().0).unwrap()
        };
        let (a, b) = (make(18), make(19));
        let live = Live::default();
        register(&live, &a);
        register(&live, &b);
        // A drag no target holds any more is dropped from the list.
        register(&live, &make(20));
        assert_eq!(cancel_live(&live, Some(20)), 0);
        assert_eq!(live_list(&live).len(), 2);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let (in_tx, in_rx) = mpsc::channel();
        // No windows: their wakes go nowhere.
        let lane = |name, tx| super::super::Lane { name, tx, hwnd: 0 };
        let (out, inc) = (lane("out", out_tx), lane("in", in_tx));
        let relay = {
            let live = live.clone();
            let shared = Arc::new(DndShared::default());
            std::thread::spawn(move || {
                super::super::relay(cmd_rx, out, inc, shared, emitter().0, live)
            })
        };
        cmd_tx
            .send(super::super::DndCmd::CancelIncoming { drag: 18 })
            .unwrap();
        // Forwarded too, to cj-dnd-in only, for it to note.
        assert!(matches!(
            in_rx.recv_timeout(WAIT),
            Ok(super::super::DndCmd::CancelIncoming { drag: 18 })
        ));
        assert!(out_rx.try_recv().is_err());
        assert!(a.is_cancelled() && !b.is_cancelled());
        // Every sender gone: both threads stop, and every live drag is cancelled.
        drop(cmd_tx);
        relay.join().unwrap();
        assert!(b.is_cancelled());
    }

    #[test]
    fn a_fetch_running_at_the_end_still_lands() {
        // R37: DROP, then the target's ask, then DoDragDrop says NONE (or
        // returns) while the reply is pending: the received file stays.
        let tmp = tempfile::tempdir().unwrap();
        let (incoming, events, ask) = ask_after_drop(21, tmp.path());
        let (_, _, dir, reply) = fetch_of(events.recv_timeout(WAIT).unwrap());
        incoming.ended(false);
        let to = dir.join("a.bin");
        std::fs::write(&to, b"bytes").unwrap();
        reply.send(Ok(to.clone())).unwrap();
        assert_eq!(ask.join().unwrap(), Ok(vec![to.clone()]));
        assert_eq!(std::fs::read(&to).unwrap(), b"bytes");
        assert_eq!(incoming.hdrop_paths(), Ok(vec![to]));
    }

    #[test]
    fn a_failed_create_leaves_nothing_of_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("22");
        // In the way of the second placeholder (a file can't take it).
        std::fs::create_dir_all(base.join("b.txt").join("keep")).unwrap();
        let items = [
            item("a.txt", ItemKind::File),
            item("A.txt", ItemKind::File),
            item("b.txt", ItemKind::File),
        ];
        assert!(Incoming::create(22, base.clone(), &items, emitter().0).is_err());
        assert!(!base.join("a.txt").exists());
        assert!(!base.join("1").exists(), "its collision folder too");
        assert!(
            base.join("b.txt").join("keep").is_dir(),
            "what was there stays"
        );
    }

    /// A drop target in another STA whose `DragLeave` takes a while: a call
    /// to it waits in COM's modal loop, as cj-dnd-in waits in Explorer's `Drop`.
    #[implement(windows::Win32::System::Ole::IDropTarget, Agile = false)]
    struct SlowTarget;

    impl windows::Win32::System::Ole::IDropTarget_Impl for SlowTarget_Impl {
        fn DragEnter(
            &self,
            _: Ref<IDataObject>,
            _: MODIFIERKEYS_FLAGS,
            _: &windows::Win32::Foundation::POINTL,
            _: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn DragOver(
            &self,
            _: MODIFIERKEYS_FLAGS,
            _: &windows::Win32::Foundation::POINTL,
            _: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn DragLeave(&self) -> windows::core::Result<()> {
            // SAFETY: plain FFI call.
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            PROBE_CALLEE_TID.store(tid, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(800));
            Ok(())
        }
        fn Drop(
            &self,
            _: Ref<IDataObject>,
            _: MODIFIERKEYS_FLAGS,
            _: &windows::Win32::Foundation::POINTL,
            _: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            Ok(())
        }
    }

    static PROBE_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    static PROBE_APP_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static PROBE_TICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    static PROBE_CALLEE_TID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn probe_ms() -> u64 {
        PROBE_START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
    }

    unsafe extern "system" fn probe_proc(
        hwnd: windows::Win32::Foundation::HWND,
        msg: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_APP, WM_TIMER};
        match msg {
            WM_APP => PROBE_APP_MS.store(probe_ms(), Ordering::SeqCst),
            WM_TIMER => {
                PROBE_TICKS.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        // SAFETY: default handling for our own window.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// Settles, without a person, what the hands-on smoke's PROBE checks for
    /// real: while an STA thread (cj-dnd-in) waits in an outgoing COM call (a
    /// target's synchronous `Drop`), does COM's modal loop dispatch a
    /// `WM_APP` posted to one of its windows (our commands), or `WM_TIMER`
    /// (our stuck and keep checks)? The call here is cross-apartment in one
    /// process, through the same channel and modal loop as cross-process.
    #[test]
    #[ignore = "probes COM's modal loop; run with --nocapture to read the answer"]
    fn probe_posted_messages_during_an_outgoing_com_call() {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize, IGlobalInterfaceTable,
        };
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Ole::IDropTarget;
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, MSG,
            PM_REMOVE, PeekMessageW, PostMessageW, PostThreadMessageW, RegisterClassExW, SetTimer,
            WINDOW_EX_STYLE, WM_APP, WM_QUIT, WNDCLASSEXW, WS_POPUP,
        };
        use windows::core::w;

        // CLSID_StdGlobalInterfaceTable: hands an interface to another
        // apartment as a proxy.
        const GIT: windows::core::GUID =
            windows::core::GUID::from_u128(0x00000323_0000_0000_c000_000000000046);
        let git = || unsafe {
            CoCreateInstance::<_, IGlobalInterfaceTable>(&GIT, None, CLSCTX_INPROC_SERVER).unwrap()
        };
        probe_ms();
        // The callee's STA: the object lives there and its loop serves calls.
        let (tx, rx) = mpsc::channel::<(usize, u32)>();
        let callee = std::thread::spawn(move || unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let target: IDropTarget = SlowTarget.into();
            let cookie = git()
                .RegisterInterfaceInGlobal(&target, &IDropTarget::IID)
                .unwrap();
            tx.send((cookie as usize, GetCurrentThreadId())).unwrap();
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                DispatchMessageW(&msg);
            }
            drop(target);
            CoUninitialize();
        });
        let (raw, callee_tid) = rx.recv().unwrap();
        // The caller's STA, like cj-dnd-in: a window, a timer, a posted WM_APP.
        let (during, call_ms, ticks_during, app_ms) = std::thread::spawn(move || unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let hinst = GetModuleHandleW(None).unwrap().into();
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(probe_proc),
                hInstance: hinst,
                lpszClassName: w!("CursorJumpComProbe"),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("CursorJumpComProbe"),
                w!("probe"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst),
                None,
            )
            .unwrap();
            let mut p = std::ptr::null_mut();
            git()
                .GetInterfaceFromGlobal(raw as u32, &IDropTarget::IID, &mut p)
                .unwrap();
            let proxy = IDropTarget::from_raw(p);
            SetTimer(Some(hwnd), 1, 50, None);
            PostMessageW(Some(hwnd), WM_APP, WPARAM(0), LPARAM(0)).unwrap();
            let t0 = probe_ms();
            proxy.DragLeave().unwrap();
            let t1 = probe_ms();
            let ticks_during = PROBE_TICKS.load(Ordering::SeqCst);
            let app = PROBE_APP_MS.load(Ordering::SeqCst);
            let during = app != 0 && app <= t1;
            // Afterwards, a plain pump gets whatever was held back.
            let mut msg = MSG::default();
            let until = Instant::now() + Duration::from_millis(200);
            while Instant::now() < until {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let app_after = PROBE_APP_MS.load(Ordering::SeqCst);
            let _ = KillTimer(Some(hwnd), 1);
            drop(proxy);
            let _ = git().RevokeInterfaceFromGlobal(raw as u32);
            let _ = DestroyWindow(hwnd);
            CoUninitialize();
            (during, t1 - t0, ticks_during, app_after.saturating_sub(t0))
        })
        .join()
        .unwrap();
        unsafe { PostThreadMessageW(callee_tid, WM_QUIT, WPARAM(0), LPARAM(0)) }.unwrap();
        callee.join().unwrap();
        // Through a proxy, served by the callee's own loop: a direct call
        // would pump nothing either, and prove nothing.
        assert_eq!(PROBE_CALLEE_TID.load(Ordering::SeqCst), callee_tid);
        eprintln!(
            "probe: an outgoing COM call of {call_ms} ms from an STA: posted WM_APP dispatched \
             during it: {} (seen at +{app_ms} ms); WM_TIMER ticks during it: {ticks_during}",
            if during { "YES" } else { "NO" },
        );
    }

    #[test]
    fn a_failed_placeholder_takes_its_collision_folder_too() {
        // Slot 1's folder was made, then its placeholder failed: create's
        // undo leaves nothing of the drag's.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("23");
        let items = [item("a.txt", ItemKind::File), item("A.txt", ItemKind::File)];
        let got = slots(&base, &items);
        assert_ne!(got[1].dir, base);
        std::fs::create_dir_all(&got[1].dir).unwrap();
        File::create(&got[0].path).unwrap();
        undo_create(&base, &got, 1);
        assert!(
            !base.exists(),
            "{:?}",
            std::fs::read_dir(&base).map(|d| d.count())
        );
    }

    #[test]
    fn cj_dnd_in_cancels_a_drag_the_relay_missed() {
        // The relay's cancel came while the drag was still being set up
        // (nothing registered: it reached nothing); the drag registered;
        // then cj-dnd-in drains the forwarded CancelIncoming in its
        // DoDragDrop. That one must reach it.
        use super::super::DndCmd;
        use super::super::inbound::InState;
        let tmp = tempfile::tempdir().unwrap();
        let live = Live::default();
        let mut state = InState::new(
            cj_core::config::Config::default(),
            Arc::default(),
            emitter().0,
            live.clone(),
        )
        .unwrap();
        assert_eq!(cancel_live(&live, Some(24)), 0, "the relay's try");
        let items = [item("a.txt", ItemKind::File)];
        let incoming = Incoming::create(24, tmp.path().join("24"), &items, emitter().0).unwrap();
        register(&live, &incoming);
        state.on_command(DndCmd::CancelIncoming { drag: 25 });
        assert!(!incoming.is_cancelled(), "another drag's cancel");
        state.on_command(DndCmd::CancelIncoming { drag: 24 });
        assert!(incoming.is_cancelled());
    }

    #[test]
    fn our_objects_are_agile() {
        // windows-rs aggregates the free-threaded marshaler into
        // `#[implement]` objects: a target's GetData comes on a COM
        // thread of ours, not through cj-dnd's message loop.
        let tmp = tempfile::tempdir().unwrap();
        let items = [item("a.txt", ItemKind::File)];
        let incoming = Incoming::create(14, tmp.path().join("14"), &items, emitter().0).unwrap();
        let data: IDataObject = DataObject::new(incoming.clone()).into();
        let source: IDropSource = DropSource {
            shared: Arc::default(),
            incoming,
            started: AtomicBool::new(false),
        }
        .into();
        assert!(data.cast::<IAgileObject>().is_ok());
        assert!(source.cast::<IAgileObject>().is_ok());
    }

    /// `CJ_SMOKE_FETCH_SECS` long: `from` copied to `to` (a new file, as the
    /// receiver writes it), in 1 MiB steps.
    fn slow_copy(from: &Path, to: &Path, over: Duration) -> std::io::Result<()> {
        use std::io::{Read, Write};
        let total = std::fs::metadata(from)?.len();
        let steps = total.div_ceil(1 << 20).max(1);
        let pause = over / steps as u32;
        let mut src = File::open(from)?;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                return out.flush();
            }
            out.write_all(&buf[..n])?;
            std::thread::sleep(pause);
        }
    }

    /// A text file of `mb` MiB under `%TEMP%\cursorjump-smoke`.
    fn smoke_file(mb: u64) -> PathBuf {
        use std::io::Write;
        let dir = std::env::temp_dir().join("cursorjump-smoke");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("cj-smoke-{mb}MB.txt"));
        let want = mb << 20;
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == want) {
            return path;
        }
        let line = "CursorJump incoming drag smoke test file, one line of many\n";
        let block = line.repeat((1 << 20) / line.len() + 1);
        let mut f = File::create(&path).unwrap();
        for _ in 0..mb {
            f.write_all(&block.as_bytes()[..1 << 20]).unwrap();
        }
        path
    }

    /// R39, for real: 2 s into a long fetch, `CatcherZone(true)` (for
    /// cj-dnd-out) and `Shield(true)` (for cj-dnd-in) go through the real
    /// relay and `WM_APP` path; each window showing says its thread took
    /// it. Decisive with Explorer, whose synchronous `Drop` holds cj-dnd-in
    /// in an outgoing COM call (with `DoDragDrop` running) for the whole
    /// fetch; with Chromium targets `DoDragDrop` has returned by then.
    fn probe_commands(
        drag: DragId,
        cmds: &mpsc::Sender<super::super::DndCmd>,
        shared: &DndShared,
        ended: &Mutex<HashSet<DragId>>,
        fetch_for: Duration,
    ) {
        use super::super::DndCmd;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
        let shown = |h: isize| {
            // SAFETY: plain FFI call on a window handle.
            unsafe { IsWindowVisible(HWND(h as *mut _)) }.as_bool()
        };
        let wait = |h: isize, within: Duration| {
            let started = Instant::now();
            while !shown(h) && started.elapsed() < within {
                std::thread::sleep(Duration::from_millis(20));
            }
            shown(h).then(|| started.elapsed())
        };
        std::thread::sleep(Duration::from_secs(2));
        let returned = || ended.lock().unwrap().contains(&drag);
        let running = !returned();
        let catcher = shared.catcher.load(Ordering::Acquire);
        let shield = shared.shield.load(Ordering::Acquire);
        let _ = cmds.send(DndCmd::CatcherZone(true));
        let _ = cmds.send(DndCmd::Shield(true));
        let out = wait(catcher, Duration::from_secs(2));
        let out_while_running = out.is_some() && !returned();
        let verdict = match (running, out_while_running) {
            (false, _) => "INCONCLUSIVE: DoDragDrop had already returned (a Chromium target?)",
            (true, true) => {
                "R39 HOLDS: cj-dnd-out takes commands while the target's Drop holds cj-dnd-in"
            }
            (true, false) => "R39 BROKEN: cj-dnd-out did not take its command within 2 s",
        };
        eprintln!(
            "smoke: PROBE {drag}: with DoDragDrop {}, cj-dnd-out applied CatcherZone(true) \
             after {out:?} => {verdict}",
            if running { "RUNNING" } else { "returned" }
        );
        let inc = wait(shield, fetch_for + Duration::from_secs(10));
        eprintln!(
            "smoke: PROBE {drag}: cj-dnd-in applied Shield(true) after {inc:?}, DoDragDrop \
             then {} (expected: only once it returned)",
            if returned() { "returned" } else { "RUNNING" }
        );
        let _ = cmds.send(DndCmd::CatcherZone(false));
        let _ = cmds.send(DndCmd::Shield(false));
    }

    /// cj-dnd alone (the app isn't running) with a session marked up: drags
    /// "from the Mac" backed by a local 50 MB file, fetched slowly through
    /// the real `Fetch` path after the drop.
    ///
    /// The shield shows, made visible: a dark square at the centre of the
    /// Mac-side monitor (its place is printed). Press and hold the left
    /// button *on it*, as the input hook lets a press through on the Mac;
    /// the drag starts with the icon under the cursor. Drop it into
    /// WhatsApp, Gmail, Discord or an Explorer folder. Rounds repeat: the
    /// shield comes back for the next drag.
    ///
    /// - `CJ_SMOKE_FETCH_SECS` (default 5): how long each fetch takes; try
    ///   60 to see targets survive a long wait. From 10 s up, "PROBE" lines
    ///   say whether cj-dnd-out still takes commands while the target holds
    ///   cj-dnd-in (R39): drop into an Explorer folder for the answer.
    /// - `CJ_SMOKE_MODE`: unset; `shield-first` (the shield's hide is sent
    ///   before `StartIncoming`, as the input thread may); `cancel`
    ///   (`CancelIncoming` 2 s in: the drag must end with no drop, even
    ///   with the mouse held still); `esc` is just pressing Esc.
    /// - `CJ_SMOKE_SECS` (default 180): how long it runs.
    ///
    /// Watch the log: each "GetData(CF_HDROP)" line names its thread; after
    /// the drop it must not be cj-dnd-in (`on_dnd=false`).
    #[test]
    #[ignore = "needs a person to drag files"]
    fn smoke_incoming_drag_from_local_file() {
        use super::super::{DndCmd, ours_under, scale_at, shield_rect, spawn_dnd_thread};
        use crate::win::input_thread::rect_center;
        use crate::win::monitors::{edge_monitor, monitors};
        use cj_core::config::Config;
        use tracing_subscriber::EnvFilter;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetCursorPos, LWA_ALPHA, SetLayeredWindowAttributes,
        };

        let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "cursorjump=debug".into());
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(filter))
            .with_thread_names(true)
            .try_init();
        // As the app does in `init_process`. Fails harmlessly if already set.
        // SAFETY: plain FFI call, before any window exists.
        let _ = unsafe {
            windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        };
        let env = |k: &str, default: u64| {
            std::env::var(k)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default)
        };
        let mode = std::env::var("CJ_SMOKE_MODE").unwrap_or_default();
        let fetch_for = Duration::from_secs(env("CJ_SMOKE_FETCH_SECS", 5));
        let deadline = Instant::now() + Duration::from_secs(env("CJ_SMOKE_SECS", 180));
        let file = smoke_file(50);
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        let items = vec![DragItem {
            name: name.clone(),
            kind: ItemKind::File,
            size: Some(std::fs::metadata(&file).unwrap().len()),
        }];

        let cfg = Config::default();
        let Some(edge) = edge_monitor(&monitors(), cfg.mac_side) else {
            panic!("no monitor on the {:?} side", cfg.mac_side);
        };
        let park = rect_center(&edge);
        let at = shield_rect(park, scale_at(park));
        let (ev_tx, ev_rx) = mpsc::channel::<WinDndEv>();
        let shared = Arc::new(DndShared::default());
        shared.session_up.store(true, Ordering::Release);
        let cmds = spawn_dnd_thread(cfg, shared.clone(), move |ev| {
            let _ = ev_tx.send(ev);
        })
        .expect("spawn");
        // The app's side, on its own thread: a fetch may be asked for at
        // any time, even after `IncomingEnded`.
        let stop = Arc::new(AtomicBool::new(false));
        let fetching = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (ended_tx, ended_rx) = mpsc::channel::<(DragId, bool)>();
        // The drags whose DoDragDrop returned, and a PROBE running (I2).
        let ended = Arc::new(Mutex::new(HashSet::<DragId>::new()));
        let probing = Arc::new(AtomicBool::new(false));
        let app = {
            let (stop, fetching, file, name) =
                (stop.clone(), fetching.clone(), file.clone(), name.clone());
            let (ended, probing, probe_cmds) = (ended.clone(), probing.clone(), cmds.clone());
            let shield = shared.clone();
            std::thread::spawn(move || {
                loop {
                    let ev = match ev_rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(ev) => ev,
                        Err(mpsc::RecvTimeoutError::Timeout) if stop.load(Ordering::Acquire) => {
                            return;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    };
                    match ev {
                        WinDndEv::Fetch {
                            drag,
                            item,
                            dir,
                            reply,
                        } => {
                            let (from, to) = (file.clone(), dir.join(&name));
                            eprintln!(
                                "smoke: Fetch {drag}/{item} into {} over {fetch_for:?}",
                                dir.display()
                            );
                            if item == 0 && fetch_for >= Duration::from_secs(10) {
                                probing.store(true, Ordering::Release);
                                let (ended, probing) = (ended.clone(), probing.clone());
                                let (cmds, shield) = (probe_cmds.clone(), shield.clone());
                                std::thread::spawn(move || {
                                    probe_commands(drag, &cmds, &shield, &ended, fetch_for);
                                    probing.store(false, Ordering::Release);
                                });
                            }
                            fetching.fetch_add(1, Ordering::AcqRel);
                            let fetching = fetching.clone();
                            std::thread::spawn(move || {
                                let result =
                                    slow_copy(&from, &to, fetch_for).map(|()| to).map_err(|e| {
                                        eprintln!("smoke: fetch failed: {e}");
                                        FailReason::Unwritable
                                    });
                                eprintln!("smoke: Fetch {drag}/{item} answered: {result:?}");
                                let _ = reply.send(result);
                                fetching.fetch_sub(1, Ordering::AcqRel);
                            });
                        }
                        WinDndEv::IncomingEnded { drag, dropped } => {
                            eprintln!("smoke: IncomingEnded {drag}, dropped {dropped}");
                            ended.lock().unwrap().insert(drag);
                            let _ = ended_tx.send((drag, dropped));
                        }
                        other => eprintln!("smoke: {other:?}"),
                    }
                }
            })
        };
        let send = |cmd: DndCmd| cmds.send(cmd).expect("dnd thread alive");
        // Stands in for the input thread's Raw Input.
        let mirror = {
            let (shared, stop) = (shared.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    shared
                        .left_down
                        .store(left_button_down(), Ordering::Release);
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        let cursor = || {
            let mut pt = POINT::default();
            // SAFETY: reads the cursor position only.
            let _ = unsafe { GetCursorPos(&mut pt) };
            (pt.x, pt.y)
        };
        eprintln!(
            "smoke: mode {:?}, fetch {fetch_for:?}, file {}",
            if mode.is_empty() { "normal" } else { &mode },
            file.display()
        );
        let mut round = 0u64;
        'rounds: while Instant::now() < deadline {
            round += 1;
            send(DndCmd::Shield(true));
            std::thread::sleep(Duration::from_millis(100));
            let shield = HWND(shared.shield.load(Ordering::Acquire) as *mut _);
            // SAFETY: our own window, made visible so it can be found.
            let _ = unsafe { SetLayeredWindowAttributes(shield, COLORREF(0), 160, LWA_ALPHA) };
            eprintln!(
                "smoke: round {round}: press and HOLD the left button on the dark square \
                 at ({}, {})-({}, {}), then drag out and drop",
                at.left, at.top, at.right, at.bottom
            );
            while !(left_button_down() && ours_under(&shared, cursor())) {
                if Instant::now() >= deadline {
                    break 'rounds;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let drag = 1000 + round;
            if mode == "shield-first" {
                send(DndCmd::Shield(false));
                std::thread::sleep(Duration::from_millis(30));
            }
            eprintln!("smoke: StartIncoming {drag}");
            send(DndCmd::StartIncoming {
                drag,
                items: items.clone(),
            });
            if mode != "shield-first" {
                // On the way home, as the input thread does.
                send(DndCmd::Shield(false));
            }
            let mut cancel_at = (mode == "cancel").then(|| Instant::now() + Duration::from_secs(2));
            loop {
                if let Some(t) = cancel_at
                    && Instant::now() >= t
                {
                    eprintln!("smoke: CancelIncoming {drag}");
                    send(DndCmd::CancelIncoming { drag });
                    cancel_at = None;
                }
                match ended_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok((d, _)) if d == drag => break,
                    Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => panic!("smoke app thread stopped"),
                }
            }
            // The shield must be gone once the drag has ended (a PROBE
            // shows it for a moment).
            let until = Instant::now() + fetch_for + Duration::from_secs(15);
            while probing.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            std::thread::sleep(Duration::from_millis(100));
            eprintln!(
                "smoke: shield {} after the drag",
                if ours_under(&shared, park) {
                    "STILL UP"
                } else {
                    "gone"
                }
            );
            while left_button_down() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        send(DndCmd::Shield(false));
        // Fetches still running are let finish (their targets wait on them).
        let until = Instant::now() + fetch_for + Duration::from_secs(10);
        while fetching.load(Ordering::Acquire) > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(cmds);
        stop.store(true, Ordering::Release);
        mirror.join().unwrap();
        app.join().unwrap();
        eprintln!("smoke: stopped cleanly");
    }

    #[test]
    fn the_drag_image_is_the_shell_icon() {
        for it in [
            item("report.pdf", ItemKind::File),
            item("Photos", ItemKind::Dir),
        ] {
            let (bitmap, side) = icon_bitmap(&it).expect("an icon");
            assert!(side >= 16);
            // SAFETY: our own bitmap, freed once.
            let _ = unsafe { DeleteObject(bitmap.into()) };
        }
    }
}
