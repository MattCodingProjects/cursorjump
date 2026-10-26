//! The catcher's drop target: what a drag arriving at the Mac edge offers,
//! and the virtual files it holds, written to disk inside `Drop`.
//!
//! - Explorer offers `CF_HDROP` with real paths: each item is `Source::Ready`.
//! - Other sources (Discord, mail attachments, zip contents) offer
//!   FILEGROUPDESCRIPTORW + FILECONTENTS: each top-level item is a
//!   `Source::Pending`, resolved once its tree is spooled under
//!   `%LOCALAPPDATA%\CursorJump\outgoing\<drag>\<item>\`. That happens inside
//!   `Drop`, because the source may free its data once the drag ends.
//! - Our own data objects carry [`SELF_FORMAT`] and are refused.
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use cj_core::dnd::{DragId, DragItem, FailReason, ItemKind, safe_rel_path, sanitize_component};
use cj_core::layout::{Rect, Side};
use tokio::sync::watch;
use tracing::{debug, warn};
use windows::Win32::Foundation::{HWND, POINT, POINTL, S_OK};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, TYMED, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
    ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{
    CFSTR_DROPDESCRIPTION, CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_INETURLW,
    DROPDESCRIPTION, DROPIMAGETYPE, DragQueryFileW, FD_ATTRIBUTES, FD_FILESIZE, FILEDESCRIPTORW,
    HDROP, IDropTargetHelper,
};
use windows::core::{HSTRING, Ref, implement};

use super::source::{hglobal_from, hglobal_medium};
use super::{OutState, WinDndEv, with_out};
use crate::common::dnd_link::Source;
use crate::common::link_fetch::{LINK_MAX_BYTES, LinkError, fetch_link, shortcut_url, url_host};

/// How near the Mac edge (physical px) a held left button shows the strip.
pub const CATCH_ZONE_PX: i32 = 48;
/// The registered clipboard format that marks CursorJump's own data objects.
pub const SELF_FORMAT: &str = "CursorJump.Self";
/// The strip's thickness along the Mac edge, physical px.
const STRIP_PX: i32 = 2;

/// A pending item's sender: its spooled root, or why there is none.
type Resolve = watch::Sender<Option<Result<PathBuf, FailReason>>>;

/// 2 px along the Mac edge of `monitor`, its full length.
pub fn strip_rect(monitor: Rect, side: Side) -> Rect {
    let Rect {
        left,
        top,
        right,
        bottom,
    } = monitor;
    match side {
        Side::Right => Rect {
            left: right - STRIP_PX,
            ..monitor
        },
        Side::Left => Rect {
            right: left + STRIP_PX,
            ..monitor
        },
        Side::Top => Rect {
            bottom: top + STRIP_PX,
            ..monitor
        },
        Side::Bottom => Rect {
            top: bottom - STRIP_PX,
            ..monitor
        },
    }
}

/// Whether (x, y) is on `edge` and less than [`CATCH_ZONE_PX`] in from its Mac side.
pub fn in_catch_zone(edge: &Rect, side: Side, x: i32, y: i32) -> bool {
    let on = x >= edge.left && x < edge.right && y >= edge.top && y < edge.bottom;
    let inward = match side {
        Side::Right => edge.right - 1 - x,
        Side::Left => x - edge.left,
        Side::Top => y - edge.top,
        Side::Bottom => edge.bottom - 1 - y,
    };
    on && inward < CATCH_ZONE_PX
}

/// Name, kind and size of each dragged path. A path that can't be read is
/// offered as a file of unknown size; reading it later fails the drag.
pub(super) fn items_from_paths(paths: &[PathBuf]) -> Vec<DragItem> {
    paths
        .iter()
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let meta = std::fs::metadata(p).ok();
            let dir = meta.as_ref().is_some_and(|m| m.is_dir());
            DragItem {
                name,
                kind: if dir { ItemKind::Dir } else { ItemKind::File },
                size: meta.filter(|m| m.is_file()).map(|m| m.len()),
            }
        })
        .collect()
}

/// Virtual files `(name, is_dir, size)`, names like `a\b.txt`: one item per
/// first component, in order of appearance and ignoring case, with the
/// descriptor indices of everything under it. A folder's size is `None`.
pub(super) fn items_from_descriptors(
    names: &[(String, bool, Option<u64>)],
) -> (Vec<DragItem>, Vec<Vec<u32>>) {
    let mut items: Vec<DragItem> = Vec::new();
    let mut groups: Vec<Vec<u32>> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for (i, (name, is_dir, size)) in names.iter().enumerate() {
        let mut parts = name.split(['\\', '/']);
        let top = parts.next().unwrap_or_default();
        let nested = parts.next().is_some();
        let key = top.to_lowercase();
        let at = match keys.iter().position(|k| *k == key) {
            Some(at) => at,
            None => {
                keys.push(key);
                items.push(DragItem {
                    name: top.to_string(),
                    kind: ItemKind::File,
                    size: *size,
                });
                groups.push(Vec::new());
                items.len() - 1
            }
        };
        if nested || *is_dir {
            items[at].kind = ItemKind::Dir;
            items[at].size = None;
        }
        groups[at].push(i as u32);
    }
    (items, groups)
}

/// A virtual `*.url` file is a web link (an InternetShortcut, as Discord
/// drags an image): offered under its name without the suffix.
pub(super) fn link_name(name: &str) -> Option<String> {
    let cut = name.len().checked_sub(4)?;
    let suffix = name.get(cut..)?;
    suffix
        .eq_ignore_ascii_case(".url")
        .then(|| name[..cut].to_string())
}

// ---------------------------------------------------------------- formats

struct Formats {
    descriptor: u16,
    contents: u16,
    url: u16,
    self_fmt: u16,
    description: u16,
}

fn formats() -> &'static Formats {
    static F: OnceLock<Formats> = OnceLock::new();
    F.get_or_init(|| {
        // SAFETY: registering a format name has no preconditions.
        let reg = |name| unsafe { RegisterClipboardFormatW(name) } as u16;
        Formats {
            descriptor: reg(CFSTR_FILEDESCRIPTORW),
            contents: reg(CFSTR_FILECONTENTS),
            url: reg(CFSTR_INETURLW),
            description: reg(CFSTR_DROPDESCRIPTION),
            // SAFETY: as above; the HSTRING outlives the call.
            self_fmt: unsafe { RegisterClipboardFormatW(&HSTRING::from(SELF_FORMAT)) } as u16,
        }
    })
}

/// Set the drop description on a drag's data object, as a drop target may
/// (the shell's drag image shows it under the image; with one set, a
/// source using the drag image manager shows a plain arrow cursor instead
/// of OLE's copy cursor). `DROPIMAGE_NOIMAGE` with no text: no badge and
/// no words. `DROPIMAGE_INVALID`: none set, so the defaults come back.
pub(super) fn set_drop_description(
    data: &IDataObject,
    kind: DROPIMAGETYPE,
) -> windows::core::Result<()> {
    let desc = DROPDESCRIPTION {
        r#type: kind,
        ..Default::default()
    };
    // SAFETY: a plain repr(C) struct viewed as bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&desc as *const DROPDESCRIPTION).cast::<u8>(),
            size_of::<DROPDESCRIPTION>(),
        )
    };
    let h = hglobal_from(bytes)?;
    let f = formatetc(formats().description, TYMED_HGLOBAL, -1);
    // SAFETY: with fRelease the data object owns the block once it says
    // yes; on a refusal it is still ours, freed below.
    unsafe {
        let r = data.SetData(&f, &hglobal_medium(h), true);
        if r.is_err() {
            let _ = windows::Win32::Foundation::GlobalFree(Some(h));
        }
        r
    }
}

/// [`SELF_FORMAT`], registered.
pub(super) fn self_format() -> u16 {
    formats().self_fmt
}

pub(super) fn formatetc(cf: u16, tymed: TYMED, lindex: i32) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex,
        tymed: tymed.0 as u32,
    }
}

/// Released on drop.
pub(super) struct Medium(pub(super) STGMEDIUM);

impl Drop for Medium {
    fn drop(&mut self) {
        // SAFETY: we own the medium GetData handed us, released once.
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

fn get(data: &IDataObject, f: &FORMATETC) -> windows::core::Result<Medium> {
    // SAFETY: plain COM call on a live data object; we own the result.
    unsafe { data.GetData(f) }.map(Medium)
}

fn offers(data: &IDataObject, f: &FORMATETC) -> bool {
    // SAFETY: plain COM call on a live data object.
    (unsafe { data.QueryGetData(f) }) == S_OK
}

/// An HGLOBAL medium's bytes, at most `limit` of them (the descriptor's
/// size: `GlobalSize` may round the block up past the data).
fn hglobal_bytes(m: &Medium, limit: Option<u64>) -> Result<Vec<u8>> {
    if m.0.tymed != TYMED_HGLOBAL.0 as u32 {
        bail!("not an HGLOBAL (tymed {})", m.0.tymed);
    }
    // SAFETY: tymed says the union holds an HGLOBAL; it is locked for the
    // copy and its size comes from GlobalSize.
    unsafe {
        let h = m.0.u.hGlobal;
        let p = GlobalLock(h);
        if p.is_null() {
            return Err(windows::core::Error::from_thread()).context("GlobalLock");
        }
        let mut n = GlobalSize(h);
        if let Some(limit) = limit {
            n = n.min(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        let v = std::slice::from_raw_parts(p.cast::<u8>(), n).to_vec();
        let _ = GlobalUnlock(h);
        Ok(v)
    }
}

fn hdrop_paths(data: &IDataObject) -> Vec<PathBuf> {
    let f = formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1);
    if !offers(data, &f) {
        return Vec::new();
    }
    let Ok(m) = get(data, &f).inspect_err(|e| debug!("catcher: CF_HDROP GetData failed: {e}"))
    else {
        return Vec::new();
    };
    if m.0.tymed != TYMED_HGLOBAL.0 as u32 {
        return Vec::new();
    }
    // SAFETY: tymed says the union holds an HGLOBAL; for CF_HDROP it holds a
    // DROPFILES, which DragQueryFileW reads.
    let h = HDROP(unsafe { m.0.u.hGlobal }.0);
    let n = unsafe { DragQueryFileW(h, u32::MAX, None) };
    (0..n)
        .filter_map(|i| {
            // SAFETY: as above; the buffer has room for the name and its nul.
            let len = unsafe { DragQueryFileW(h, i, None) } as usize;
            let mut buf = vec![0u16; len + 1];
            let got = unsafe { DragQueryFileW(h, i, Some(&mut buf)) } as usize;
            (got > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..got])))
        })
        .collect()
}

/// One virtual file: `(name, is_dir, size)`.
fn descriptors(data: &IDataObject) -> Vec<(String, bool, Option<u64>)> {
    let f = formatetc(formats().descriptor, TYMED_HGLOBAL, -1);
    if !offers(data, &f) {
        return Vec::new();
    }
    let Ok(m) = get(data, &f).inspect_err(|e| debug!("catcher: descriptor GetData failed: {e}"))
    else {
        return Vec::new();
    };
    match hglobal_bytes(&m, None) {
        Ok(bytes) => parse_descriptors(&bytes),
        Err(e) => {
            debug!("catcher: descriptor unreadable: {e:#}");
            Vec::new()
        }
    }
}

/// A FILEGROUPDESCRIPTORW: a u32 count, then that many FILEDESCRIPTORWs.
fn parse_descriptors(bytes: &[u8]) -> Vec<(String, bool, Option<u64>)> {
    let Some(count) = bytes.first_chunk::<4>().map(|c| u32::from_le_bytes(*c)) else {
        return Vec::new();
    };
    let sz = size_of::<FILEDESCRIPTORW>();
    (0..count as usize)
        .take_while(|i| 4 + (i + 1) * sz <= bytes.len())
        .map(|i| {
            // SAFETY: bounds checked above; read_unaligned copes with the
            // Vec's alignment and the struct's packing.
            let fd = unsafe {
                std::ptr::read_unaligned(bytes.as_ptr().add(4 + i * sz).cast::<FILEDESCRIPTORW>())
            };
            // Packed: copy the fields out before using them.
            let (flags, attrs, name) = (fd.dwFlags, fd.dwFileAttributes, fd.cFileName);
            let (hi, lo) = (fd.nFileSizeHigh, fd.nFileSizeLow);
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            let is_dir =
                flags & FD_ATTRIBUTES.0 as u32 != 0 && attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
            let size = (flags & FD_FILESIZE.0 as u32 != 0 && !is_dir)
                .then_some(((hi as u64) << 32) | lo as u64);
            (String::from_utf16_lossy(&name[..len]), is_dir, size)
        })
        .collect()
}

/// UniformResourceLocatorW, if offered (Discord sends it with its link).
fn offered_url(data: &IDataObject) -> Option<String> {
    let f = formatetc(formats().url, TYMED_HGLOBAL, -1);
    if !offers(data, &f) {
        return None;
    }
    let bytes = hglobal_bytes(&get(data, &f).ok()?, None).ok()?;
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    let url = String::from_utf16_lossy(&units);
    (!url.is_empty()).then_some(url)
}

// ---------------------------------------------------------------- offers

/// What a drag arriving at the catcher holds.
pub(super) enum Offered {
    Paths(Vec<PathBuf>),
    Virtual {
        names: Vec<(String, bool, Option<u64>)>,
        url: Option<String>,
    },
}

/// `None` for our own data objects and for drags with no files.
pub(super) fn read_offer(data: &IDataObject) -> Option<Offered> {
    if offers(data, &formatetc(formats().self_fmt, TYMED_HGLOBAL, -1)) {
        debug!("catcher: our own drag; refused");
        return None;
    }
    let paths = hdrop_paths(data);
    if !paths.is_empty() {
        return Some(Offered::Paths(paths));
    }
    let names = descriptors(data);
    if names.is_empty() {
        return None;
    }
    Some(Offered::Virtual {
        names,
        url: offered_url(data),
    })
}

/// A virtual item, to be written out in `Drop`.
pub(super) struct Pending {
    /// Its index in the drag.
    item: u32,
    /// For `FailReason`s.
    name: String,
    what: Spool,
    tx: Resolve,
}

enum Spool {
    /// Every entry of the item's tree.
    Tree(Vec<Entry>),
    /// A web link: its target is downloaded (`start_link_fetch`).
    Link {
        index: u32,
        /// From UniformResourceLocatorW at `DragEnter`; the shortcut's own
        /// `URL=` line, read in `Drop`, wins.
        link_url: Option<String>,
    },
}

struct Entry {
    /// Its descriptor index: FILECONTENTS' `lindex`.
    index: u32,
    /// Relative to the item's folder; the first component is the item.
    parts: Vec<String>,
    is_dir: bool,
    size: Option<u64>,
}

impl Pending {
    pub(super) fn resolve(self, result: Result<PathBuf, FailReason>) {
        // No receiver left means nobody wants it any more.
        let _ = self.tx.send(Some(result));
    }
}

/// The items, their sources, and what `Drop` must write out.
pub(super) fn into_offer(offered: Offered) -> (Vec<DragItem>, Vec<Source>, Vec<Pending>) {
    match offered {
        Offered::Paths(paths) => {
            let items = items_from_paths(&paths);
            (
                items,
                paths.into_iter().map(Source::Ready).collect(),
                vec![],
            )
        }
        Offered::Virtual { names, url } => {
            let (mut items, groups) = items_from_descriptors(&names);
            let mut sources = Vec::with_capacity(items.len());
            let mut pending = Vec::with_capacity(items.len());
            for (i, (item, group)) in items.iter_mut().zip(groups).enumerate() {
                let (tx, rx) = watch::channel(None);
                sources.push(Source::Pending(rx));
                let link = (item.kind == ItemKind::File)
                    .then(|| link_name(&item.name))
                    .flatten();
                let what = match link {
                    Some(name) => {
                        item.name = name;
                        item.size = None;
                        Spool::Link {
                            index: group[0],
                            link_url: url.clone(),
                        }
                    }
                    None => Spool::Tree(
                        group
                            .iter()
                            .map(|&index| {
                                let (name, is_dir, size) = &names[index as usize];
                                // A trailing separator ("Photos\") is not a component.
                                let parts = name
                                    .split(['\\', '/'])
                                    .filter(|c| !c.is_empty())
                                    .map(String::from)
                                    .collect();
                                Entry {
                                    index,
                                    parts,
                                    is_dir: *is_dir,
                                    size: *size,
                                }
                            })
                            .collect(),
                    ),
                };
                pending.push(Pending {
                    item: i as u32,
                    name: item.name.clone(),
                    what,
                    tx,
                });
            }
            (items, sources, pending)
        }
    }
}

// ---------------------------------------------------------------- spooling

/// `%LOCALAPPDATA%\CursorJump\outgoing\<drag>`; Task 13 sweeps it.
fn spool_dir(drag: DragId) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(base)
        .join("CursorJump")
        .join("outgoing")
        .join(drag.to_string()))
}

/// Write every pending item out and resolve it, or start its download (a
/// web link). Runs inside `Drop`.
pub(super) fn spool(drag: DragId, data: &IDataObject, pending: Vec<Pending>) {
    let dir = spool_dir(drag);
    for p in pending {
        let entries = match &p.what {
            Spool::Tree(entries) => entries,
            Spool::Link { index, link_url } => {
                // Read while the data is there; the download runs on its own.
                let url = read_link(data, *index).or_else(|| link_url.clone());
                start_link_fetch(drag, &dir, url, p);
                continue;
            }
        };
        let unreadable = || FailReason::Unreadable {
            name: p.name.clone(),
        };
        let started = Instant::now();
        let result = match &dir {
            Err(e) => {
                warn!("catcher: no spool folder: {e:#}");
                Err(FailReason::Unwritable)
            }
            Ok(dir) => {
                let item_dir = dir.join(p.item.to_string());
                spool_tree(data, &item_dir, entries).map_err(|e| {
                    warn!(drag, item = p.item, "catcher: spooling failed: {e:#}");
                    unreadable()
                })
            }
        };
        let took = started.elapsed();
        debug!(
            drag,
            item = p.item,
            ok = result.is_ok(),
            ?took,
            "catcher: spooled"
        );
        p.resolve(result);
    }
}

/// Where a web link's target is downloaded: under the item's folder, by
/// the name offered at `DragEnter`.
fn link_dest(dir: &Path, item: u32, name: &str) -> PathBuf {
    let name = if name.is_empty() { "download" } else { name };
    dir.join(item.to_string()).join(sanitize_component(name))
}

/// Download a web link's target on a thread of its own ("cj-link-fetch",
/// never cj-dnd-out: `Drop` must not wait), then resolve the item: its
/// path, or `Unreadable` on any failure. The download stops once nobody
/// holds the item's source any more (the drag ended or was cancelled).
fn start_link_fetch(drag: DragId, dir: &Result<PathBuf>, url: Option<String>, p: Pending) {
    let Pending { item, name, tx, .. } = p;
    let unreadable = FailReason::Unreadable { name: name.clone() };
    let Some(url) = url else {
        warn!(drag, item, "catcher: a web link with no URL");
        let _ = tx.send(Some(Err(unreadable)));
        return;
    };
    let dir = match dir {
        Ok(dir) => dir.clone(),
        Err(e) => {
            warn!("catcher: no spool folder: {e:#}");
            let _ = tx.send(Some(Err(FailReason::Unwritable)));
            return;
        }
    };
    let host = url_host(&url);
    debug!(drag, item, %host, "catcher: fetching a web link");
    let fetch = move || {
        let dest = link_dest(&dir, item, &name);
        let cancelled = || tx.is_closed();
        let fetched = match dest.parent().map(std::fs::create_dir_all) {
            Some(Err(e)) => Err(LinkError::Io(e)),
            _ => fetch_link(&url, &dest, LINK_MAX_BYTES, &cancelled),
        };
        let result = match fetched {
            Ok(bytes) => {
                debug!(drag, item, %host, bytes, "catcher: web link fetched");
                Ok(dest)
            }
            Err(e) => {
                warn!(drag, item, %host, error = e.kind(), "catcher: web link not fetched");
                Err(unreadable)
            }
        };
        // No receiver left means nobody wants it any more.
        let _ = tx.send(Some(result));
    };
    let spawned = std::thread::Builder::new()
        .name("cj-link-fetch".into())
        .spawn(fetch);
    if let Err(e) = spawned {
        // The sender went with the closure: the item reads as unreadable.
        warn!(drag, item, "catcher: no thread for a web link: {e}");
    }
}

/// An item's tree under `item_dir`; returns the item's root.
fn spool_tree(data: &IDataObject, item_dir: &Path, entries: &[Entry]) -> Result<PathBuf> {
    let Some(top) = entries.first().and_then(|e| e.parts.first()) else {
        bail!("an item with no entries");
    };
    let root = item_dir.join(sanitize_component(top));
    std::fs::create_dir_all(item_dir)
        .with_context(|| format!("creating {}", item_dir.display()))?;
    for e in entries {
        let Some(rel) = safe_rel_path(&e.parts) else {
            bail!("unsafe path {:?}", e.parts);
        };
        let path = item_dir.join(rel);
        if e.is_dir {
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating {}", path.display()))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        read_contents(data, e.index, e.size, &path)?;
    }
    Ok(root)
}

/// FILECONTENTS `lindex` (an IStream or an HGLOBAL) into a new file at `to`.
/// `size`, from the descriptor, trims an HGLOBAL that `GlobalSize` rounded up.
fn read_contents(data: &IDataObject, lindex: u32, size: Option<u64>, to: &Path) -> Result<u64> {
    let f = formatetc(
        formats().contents,
        TYMED(TYMED_ISTREAM.0 | TYMED_HGLOBAL.0),
        lindex as i32,
    );
    let m = get(data, &f).with_context(|| format!("FILECONTENTS {lindex}"))?;
    let mut out = File::create(to).with_context(|| format!("creating {}", to.display()))?;
    let written = write_medium(&m, size, &mut out);
    if written.is_err() {
        drop(out);
        let _ = std::fs::remove_file(to);
    }
    written.with_context(|| format!("writing {}", to.display()))
}

fn write_medium(m: &Medium, size: Option<u64>, out: &mut File) -> Result<u64> {
    if m.0.tymed == TYMED_HGLOBAL.0 as u32 {
        let bytes = hglobal_bytes(m, size)?;
        out.write_all(&bytes)?;
        return Ok(bytes.len() as u64);
    }
    if m.0.tymed != TYMED_ISTREAM.0 as u32 {
        bail!("FILECONTENTS came as tymed {}", m.0.tymed);
    }
    // SAFETY: tymed says the union holds an IStream; the clone is our own reference.
    let Some(stream) = (unsafe { (*m.0.u.pstm).clone() }) else {
        bail!("FILECONTENTS came as a null IStream");
    };
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let mut got = 0u32;
        let (ptr, len) = (buf.as_mut_ptr().cast(), buf.len() as u32);
        // SAFETY: buf is writable for `len` bytes.
        let hr = unsafe { stream.Read(ptr, len, Some(&mut got)) };
        hr.ok()?;
        if got == 0 {
            return Ok(total);
        }
        out.write_all(&buf[..got as usize])?;
        total += got as u64;
        if total > stream_limit(size) {
            bail!("the stream ran past {} bytes", stream_limit(size));
        }
    }
}

/// Most a stream may run past the size its descriptor gave.
const STREAM_SLACK: u64 = 1 << 20;
/// Most a stream of unknown size may hold: 16 GiB, past any attachment or
/// archive entry a person would drag, so a runaway source can't fill the disk.
const UNSIZED_STREAM_CAP: u64 = 16 << 30;

fn stream_limit(size: Option<u64>) -> u64 {
    size.map_or(UNSIZED_STREAM_CAP, |s| s.saturating_add(STREAM_SLACK))
}

/// A web link's target, from its shortcut file's contents.
fn read_link(data: &IDataObject, lindex: u32) -> Option<String> {
    let f = formatetc(formats().contents, TYMED_HGLOBAL, lindex as i32);
    let m = get(data, &f).ok()?;
    shortcut_url(&hglobal_bytes(&m, None).ok()?)
}

// ---------------------------------------------------------------- IDropTarget

/// Every call arrives on cj-dnd-out; the decisions are `OutState`'s. A
/// call on another thread (our own drag from the Mac, should OLE call this
/// agile object on cj-dnd-in) finds no state there and is refused, as our
/// own drags are anyway.
#[implement(IDropTarget)]
pub(super) struct Target {
    pub(super) hwnd: HWND,
}

fn helper() -> Option<IDropTargetHelper> {
    with_out(|s| s.helper.clone()).flatten()
}

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // SAFETY: OLE passes a valid in/out effect pointer.
        let allowed = unsafe { *pdweffect };
        let data = pdataobj.ok().ok();
        // Read before taking the state: GetData may pump messages.
        let offered = data.and_then(read_offer);
        let effect = with_out(|s| s.enter(offered, allowed, data)).unwrap_or(DROPEFFECT_NONE);
        // SAFETY: as above.
        unsafe { *pdweffect = effect };
        if let (Some(h), Some(data)) = (helper(), data) {
            // SAFETY: our own window and the live data object.
            let _ = unsafe { h.DragEnter(self.hwnd, data, &POINT { x: pt.x, y: pt.y }, effect) };
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // SAFETY: OLE passes a valid in/out effect pointer.
        let allowed = unsafe { *pdweffect };
        let (effect, held) =
            with_out(|s| (s.over(allowed), s.image_held())).unwrap_or((DROPEFFECT_NONE, false));
        // SAFETY: as above.
        unsafe { *pdweffect = effect };
        if let Some(h) = helper() {
            // While the image is held hidden the helper isn't told of the
            // move (that could show it again); the hiding is asked again.
            // SAFETY: plain helper calls.
            let _ = if held {
                unsafe { h.Show(false) }
            } else {
                unsafe { h.DragOver(&POINT { x: pt.x, y: pt.y }, effect) }
            };
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if let Some(h) = helper() {
            // SAFETY: plain helper call.
            let _ = unsafe { h.DragLeave() };
        }
        with_out(OutState::leave);
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let data = pdataobj.ok().ok();
        let takes = with_out(|s| s.modes.takes_drop()).unwrap_or(false);
        let effect = if takes {
            DROPEFFECT_COPY
        } else {
            DROPEFFECT_NONE
        };
        // Before unparking: the image ends here, hidden, and never flashes
        // back at the park point.
        if let (Some(h), Some(data)) = (helper(), data) {
            // SAFETY: plain helper call with the live data object.
            let _ = unsafe { h.Drop(data, &POINT { x: pt.x, y: pt.y }, effect) };
        }
        let taken = with_out(OutState::take_drop).flatten();
        let effect = taken.as_ref().map_or(DROPEFFECT_NONE, |_| DROPEFFECT_COPY);
        if let Some((drag, pending)) = taken {
            // The catcher is already off the screen. Outside the state: the
            // reads are cross-process calls, which pump this thread's
            // messages (and so its commands) while they wait.
            match data {
                Some(data) => spool(drag, data, pending),
                None => pending
                    .into_iter()
                    .for_each(|p| p.resolve(Err(FailReason::Cancelled))),
            }
            with_out(|s| s.emit(WinDndEv::Spooled { drag }));
        }
        // Never MOVE: the source keeps its files.
        // SAFETY: OLE passes a valid in/out effect pointer.
        unsafe { *pdweffect = effect };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    fn file(name: &str, size: Option<u64>) -> DragItem {
        DragItem {
            name: name.into(),
            kind: ItemKind::File,
            size,
        }
    }

    fn dir(name: &str) -> DragItem {
        DragItem {
            name: name.into(),
            kind: ItemKind::Dir,
            size: None,
        }
    }

    #[test]
    fn strip_rect_on_each_side() {
        let r = |left, top, right, bottom| Rect {
            left,
            top,
            right,
            bottom,
        };
        assert_eq!(strip_rect(MON, Side::Right), r(1918, 0, 1920, 1080));
        assert_eq!(strip_rect(MON, Side::Left), r(0, 0, 2, 1080));
        assert_eq!(strip_rect(MON, Side::Top), r(0, 0, 1920, 2));
        assert_eq!(strip_rect(MON, Side::Bottom), r(0, 1078, 1920, 1080));
        // A monitor left of the primary, taller and above it.
        let west = r(-1080, -400, 0, 1520);
        assert_eq!(strip_rect(west, Side::Left), r(-1080, -400, -1078, 1520));
        assert_eq!(strip_rect(west, Side::Right), r(-2, -400, 0, 1520));
        assert_eq!(strip_rect(west, Side::Top), r(-1080, -400, 0, -398));
    }

    #[test]
    fn catcher_zone_logic() {
        // 47 px in from the edge pixel is inside the zone; 49 is not.
        assert!(in_catch_zone(&MON, Side::Right, 1919 - 47, 500));
        assert!(!in_catch_zone(&MON, Side::Right, 1919 - 49, 500));
        assert!(in_catch_zone(&MON, Side::Right, 1919, 500));
        assert!(in_catch_zone(&MON, Side::Left, 47, 500));
        assert!(!in_catch_zone(&MON, Side::Left, 49, 500));
        assert!(in_catch_zone(&MON, Side::Top, 500, 47));
        assert!(!in_catch_zone(&MON, Side::Top, 500, 49));
        assert!(in_catch_zone(&MON, Side::Bottom, 500, 1079 - 47));
        assert!(!in_catch_zone(&MON, Side::Bottom, 500, 1079 - 49));
        // Off the monitor: never, even near the edge line.
        assert!(!in_catch_zone(&MON, Side::Right, 1920, 500));
        assert!(!in_catch_zone(&MON, Side::Right, 1910, 1080));
        assert!(!in_catch_zone(&MON, Side::Right, 1910, -1));
        let west = Rect {
            left: -1080,
            top: -400,
            right: 0,
            bottom: 1520,
        };
        assert!(in_catch_zone(&west, Side::Left, -1080 + 47, -399));
        assert!(!in_catch_zone(&west, Side::Left, -1080 + 49, -399));
    }

    #[test]
    fn items_from_paths_files_and_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let notes = tmp.path().join("notes.txt");
        std::fs::write(&notes, b"hello").unwrap();
        let photos = tmp.path().join("Photos");
        std::fs::create_dir(&photos).unwrap();
        std::fs::write(photos.join("a.jpg"), b"jpeg").unwrap();
        let gone = tmp.path().join("gone.bin");
        assert_eq!(
            items_from_paths(&[notes, photos, gone]),
            [
                file("notes.txt", Some(5)),
                dir("Photos"),
                file("gone.bin", None)
            ]
        );
    }

    #[test]
    fn items_from_descriptors_groups_by_top_component() {
        let d = |name: &str, is_dir: bool, size: Option<u64>| (name.to_string(), is_dir, size);
        let (items, groups) = items_from_descriptors(&[
            d("Photos", true, None),
            d("report.pdf", false, Some(10)),
            d("Photos\\a.jpg", false, Some(3)),
            d("Photos\\2024\\b.jpg", false, Some(4)),
            // A tree with no entry of its own for its folder.
            d("Music\\x.mp3", false, Some(7)),
            // The same folder, in another case.
            d("photos\\c.jpg", false, None),
            d("unknown.bin", false, None),
        ]);
        assert_eq!(
            items,
            [
                dir("Photos"),
                file("report.pdf", Some(10)),
                dir("Music"),
                file("unknown.bin", None)
            ]
        );
        assert_eq!(groups, [vec![0, 2, 3, 5], vec![1], vec![4], vec![6]]);
        assert_eq!(items_from_descriptors(&[]), (vec![], vec![]));
    }

    #[test]
    fn link_names_drop_the_url_suffix() {
        assert_eq!(link_name("image.png.url"), Some("image.png".into()));
        assert_eq!(link_name("Some Page.URL"), Some("Some Page".into()));
        assert_eq!(link_name(".url"), Some(String::new()));
        assert_eq!(link_name("image.png"), None);
        assert_eq!(link_name("url"), None);
        assert_eq!(link_name("notes.urls"), None);
        assert_eq!(link_name("東京.url"), Some("東京".into()));
    }

    #[test]
    fn virtual_links_are_offered_without_suffix_or_size() {
        let names = vec![
            ("image.png.url".to_string(), false, Some(218)),
            ("Folder.url".to_string(), true, None),
            ("doc.txt".to_string(), false, Some(3)),
        ];
        let (items, sources, pending) = into_offer(Offered::Virtual {
            names,
            url: Some("https://x/y".into()),
        });
        assert_eq!(
            items,
            [
                file("image.png", None),
                dir("Folder.url"),
                file("doc.txt", Some(3))
            ]
        );
        assert_eq!(sources.len(), 3);
        assert!(sources.iter().all(|s| matches!(s, Source::Pending(_))));
        assert!(matches!(
            &pending[0].what,
            Spool::Link { index: 0, link_url: Some(u) } if u == "https://x/y"
        ));
        assert!(matches!(&pending[1].what, Spool::Tree(e) if e.len() == 1));
        assert_eq!(pending[2].name, "doc.txt");
        let Spool::Tree(entries) = &pending[2].what else {
            panic!("doc.txt is a file to write out");
        };
        assert_eq!(entries[0].parts, ["doc.txt"]);
        assert_eq!(entries[0].size, Some(3));
    }

    #[test]
    fn streams_are_bounded_by_their_size_or_a_cap() {
        assert_eq!(stream_limit(Some(10)), 10 + (1 << 20));
        assert_eq!(stream_limit(Some(u64::MAX)), u64::MAX);
        assert_eq!(stream_limit(None), 16 << 30);
    }

    #[test]
    fn web_links_land_in_the_item_folder_by_their_offered_name() {
        let dir = Path::new(r"C:\spool\7");
        assert_eq!(
            link_dest(dir, 2, "image.png"),
            dir.join("2").join("image.png")
        );
        assert_eq!(link_dest(dir, 0, ""), dir.join("0").join("download"));
        assert_eq!(
            link_dest(dir, 0, "a:b?.png"),
            dir.join("0").join("a_b_.png")
        );
    }

    /// A link item's pending source, as `into_offer` makes it.
    fn link_item(
        name: &str,
    ) -> (
        Pending,
        watch::Receiver<Option<Result<PathBuf, FailReason>>>,
    ) {
        let (_, mut sources, mut pending) = into_offer(Offered::Virtual {
            names: vec![(format!("{name}.url"), false, Some(218))],
            url: None,
        });
        let Some(Source::Pending(rx)) = sources.pop() else {
            panic!("a link is pending");
        };
        (pending.remove(0), rx)
    }

    fn resolved(
        rx: &mut watch::Receiver<Option<Result<PathBuf, FailReason>>>,
    ) -> Result<PathBuf, FailReason> {
        let until = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(r) = rx.borrow_and_update().clone() {
                return r;
            }
            assert!(Instant::now() < until, "never resolved");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn a_web_link_is_fetched_off_the_drop_thread() {
        use std::io::{Read, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/image.png?ex=1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf);
            // Slow, so that returning before it ends shows Drop doesn't wait.
            std::thread::sleep(std::time::Duration::from_millis(500));
            let _ = conn.write_all(
                b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 4\r\n\r\npng!",
            );
        });
        let spool = tempfile::tempdir().unwrap();
        let (p, mut rx) = link_item("image.png");
        let started = Instant::now();
        start_link_fetch(7, &Ok(spool.path().to_path_buf()), Some(url), p);
        assert!(started.elapsed() < std::time::Duration::from_millis(400));
        let path = resolved(&mut rx).unwrap();
        assert_eq!(path, spool.path().join("0").join("image.png"));
        assert_eq!(std::fs::read(&path).unwrap(), b"png!");
        server.join().unwrap();
    }

    #[test]
    fn a_web_link_that_cant_be_fetched_is_unreadable() {
        let spool = tempfile::tempdir().unwrap();
        let unreadable = Err(FailReason::Unreadable {
            name: "image.png".into(),
        });
        let (p, mut rx) = link_item("image.png");
        start_link_fetch(
            7,
            &Ok(spool.path().to_path_buf()),
            Some("ftp://x/y.png".into()),
            p,
        );
        assert_eq!(resolved(&mut rx), unreadable);
        let (p, mut rx) = link_item("image.png");
        start_link_fetch(7, &Ok(spool.path().to_path_buf()), None, p);
        assert_eq!(resolved(&mut rx), unreadable);
        // Nothing half-written is left behind.
        let left: Vec<_> = walk(spool.path());
        assert!(left.iter().all(|p| p.is_dir()), "{left:?}");
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.extend(walk(&p));
            }
            out.push(p);
        }
        out
    }

    #[test]
    fn virtual_tree_entries_skip_empty_components() {
        let names = vec![
            ("Photos\\".to_string(), true, None),
            ("Photos\\a.jpg".to_string(), false, Some(1)),
        ];
        let (items, _, pending) = into_offer(Offered::Virtual { names, url: None });
        assert_eq!(items, [dir("Photos")]);
        let Spool::Tree(entries) = &pending[0].what else {
            panic!("a folder is a tree");
        };
        assert_eq!(entries[0].parts, ["Photos"]);
        assert_eq!(entries[1].parts, ["Photos", "a.jpg"]);
        assert!(entries.iter().all(|e| safe_rel_path(&e.parts).is_some()));
    }

    #[test]
    fn descriptor_bytes_parse_names_sizes_and_folders() {
        let fd = |name: &str, flags: u32, attrs: u32, size: u64| {
            let mut fd = FILEDESCRIPTORW {
                dwFlags: flags,
                dwFileAttributes: attrs,
                nFileSizeHigh: (size >> 32) as u32,
                nFileSizeLow: size as u32,
                ..Default::default()
            };
            for (i, u) in name.encode_utf16().enumerate() {
                fd.cFileName[i] = u;
            }
            fd
        };
        let both = (FD_FILESIZE.0 | FD_ATTRIBUTES.0) as u32;
        let fds = [
            fd("big.iso", both, 0x80, 5 << 32),
            fd("Photos", both, FILE_ATTRIBUTE_DIRECTORY.0, 0),
            fd("image.png.url", 0, 0, 0),
        ];
        let mut bytes = (fds.len() as u32).to_le_bytes().to_vec();
        for f in &fds {
            // SAFETY: a plain repr(C) struct viewed as bytes.
            bytes.extend_from_slice(unsafe {
                std::slice::from_raw_parts(
                    (f as *const FILEDESCRIPTORW).cast::<u8>(),
                    size_of_val(f),
                )
            });
        }
        assert_eq!(
            parse_descriptors(&bytes),
            [
                ("big.iso".to_string(), false, Some(5 << 32)),
                ("Photos".to_string(), true, None),
                ("image.png.url".to_string(), false, None),
            ]
        );
        // A count larger than the data: only the whole entries.
        bytes[0] = 9;
        assert_eq!(parse_descriptors(&bytes).len(), 3);
        assert!(parse_descriptors(&[1, 0]).is_empty());
    }
}
