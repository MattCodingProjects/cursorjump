//! Moving one dragged item. The origin walks it and streams it over a files
//! stream, paced so the cursor never stutters; the receiver writes it under
//! a `.cjpart` name and renames it into place only once it is complete, so
//! a failed or cancelled copy leaves nothing behind.
//!
//! Received files are our own local copies (no quarantine flag on the Mac):
//! each is flushed and closed before the rename, but not fsynced.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use cj_core::dnd::{
    DragId, FailReason, FileEntry, FilesHeader, ItemKind, MAX_PATH_DEPTH, safe_rel_path,
    sanitize_component,
};
use cj_core::pacer::Pacer;
use cj_net::files::{FilesIn, FilesOut, STOP_CANCELLED, STOP_INVALID, stop_code};
use cj_net::session::Session;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Largest read or write of file data.
pub const CHUNK: usize = 256 * 1024;
/// Free space a received item must leave on its disk.
pub const SPACE_MARGIN: u64 = 64 * 1024 * 1024;
/// Shortest gap between progress reports.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// Longest file name, in UTF-8 bytes and in UTF-16 units.
const MAX_NAME_LEN: usize = 255;

/// The session's pacer, shared by all its items. Locked only to call
/// `delay_for`, never across an await.
pub type SharedPacer = Arc<std::sync::Mutex<Pacer>>;

/// Where a received item goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Dest {
    /// Exactly this path (a file promise's URL).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "R47: the Mac fetches into staging (InDir) and copies to the promise's URL"
        )
    )]
    Exact(PathBuf),
    /// In this folder, under the item's sanitized name.
    InDir(PathBuf),
}

/// Free space at a destination, as a trait so tests can fake a full disk.
/// `receive_item` asks it off the runtime's threads (`spawn_blocking`).
pub trait FreeSpace: Send + Sync {
    fn free_bytes(&self, dir: &Path) -> io::Result<u64>;
}

/// Free space as the OS reports it to this user.
pub struct OsFreeSpace;

impl FreeSpace for OsFreeSpace {
    #[cfg(windows)]
    fn free_bytes(&self, dir: &Path) -> io::Result<u64> {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        use windows::core::HSTRING;
        let mut free = 0u64;
        // SAFETY: the name is a valid wide string and `free` outlives the call.
        let name = HSTRING::from(verbatim(dir)?.as_path());
        unsafe { GetDiskFreeSpaceExW(&name, Some(&raw mut free), None, None) }
            .map_err(win_io_error)?;
        Ok(free)
    }

    #[cfg(not(windows))]
    fn free_bytes(&self, dir: &Path) -> io::Result<u64> {
        let path = c_path(dir)?;
        let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `path` is NUL-terminated and `st` is large enough.
        if unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: statvfs succeeded, so it filled `st` in.
        let st = unsafe { st.assume_init() };
        Ok(u64::from(st.f_bavail).saturating_mul(st.f_frsize))
    }
}

/// Why a transfer stopped, before it is told as a `FailReason`.
#[derive(Debug)]
enum Stop {
    /// The local token was cancelled.
    Cancelled,
    /// The files stream failed: the peer stopped or reset it, the link
    /// dropped, or the peer broke the stream protocol.
    Stream(anyhow::Error),
    /// Writing the destination failed.
    Disk(io::Error),
    Fail(FailReason),
}

impl Stop {
    fn reason(self, cancel: &CancellationToken) -> FailReason {
        match self {
            Stop::Cancelled => FailReason::Cancelled,
            Stop::Fail(r) => r,
            Stop::Disk(e) => disk_fail(&e),
            Stop::Stream(e) => match stop_code(&e) {
                Some(STOP_CANCELLED) => FailReason::Cancelled,
                Some(STOP_INVALID) => {
                    error!("the other computer refused a files stream: {e:#}");
                    FailReason::Cancelled
                }
                _ if cancel.is_cancelled() => FailReason::Cancelled,
                // A lost link, or the peer breaking the stream protocol:
                // the error doesn't say which.
                _ => {
                    warn!("files stream failed: {e:#}");
                    FailReason::Disconnected
                }
            },
        }
    }
}

/// What a local disk error tells the user.
fn disk_fail(e: &io::Error) -> FailReason {
    warn!("writing a received item failed: {e}");
    match e.kind() {
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => FailReason::NoSpace,
        // PermissionDenied and ReadOnlyFilesystem, and anything else.
        _ => FailReason::Unwritable,
    }
}

/// Runs `fut` unless `cancel` fires first.
async fn unless_cancelled<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = T>,
) -> Result<T, Stop> {
    cancel.run_until_cancelled(fut).await.ok_or(Stop::Cancelled)
}

/// The last component of `p`, as the user knows it.
fn display_name(p: &Path) -> String {
    match p.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => p.display().to_string(),
    }
}

/// Sends the file or folder at `root` as item `item` of drag `drag`.
/// Symlinks in it are skipped, not followed.
pub async fn send_item(
    s: &Session,
    drag: DragId,
    item: u32,
    root: &Path,
    pacer: &SharedPacer,
    cancel: &CancellationToken,
) -> Result<(), FailReason> {
    let name = display_name(root);
    let to_walk = root.to_path_buf();
    let sources = match tokio::task::spawn_blocking(move || walk(&to_walk)).await {
        Ok(walked) => walked?,
        Err(e) => {
            error!("walking {} failed: {e}", root.display());
            return Err(FailReason::Unreadable { name });
        }
    };
    let header = FilesHeader {
        drag,
        item,
        name,
        kind: sources[0].entry.kind,
        total_bytes: sources.iter().map(|s| s.entry.size).sum(),
        entries: sources.len() as u64,
    };
    // A single file is opened before its stream, so one that can't be read
    // fails without the other computer seeing anything. A folder's files
    // are opened as they are sent.
    let first = match header.kind {
        ItemKind::File => Some(
            open_source(&sources[0], cancel)
                .await
                .map_err(|stop| stop.reason(cancel))?,
        ),
        ItemKind::Dir => None,
    };
    let opened = unless_cancelled(cancel, s.open_files(&header))
        .await
        .and_then(|r| r.map_err(Stop::Stream));
    let mut out = opened.map_err(|stop| stop.reason(cancel))?;
    match send_entries(&mut out, &sources, first, pacer, cancel).await {
        Ok(()) => out.finish().map_err(|e| Stop::Stream(e).reason(cancel)),
        Err(stop) => {
            out.cancel();
            Err(stop.reason(cancel))
        }
    }
}

/// One entry to send and where to read it from.
#[derive(Debug)]
struct Source {
    entry: FileEntry,
    path: PathBuf,
}

/// `root` and everything in it, each folder before what is in it (a
/// folder's contents in name order), with the sizes `stat` gives.
/// Symlinks, and anything else that is neither a file nor a folder, are
/// skipped and logged.
fn walk(root: &Path) -> Result<Vec<Source>, FailReason> {
    let unreadable = |p: &Path, e: io::Error| {
        warn!("{} cannot be sent: {e}", p.display());
        FailReason::Unreadable {
            name: display_name(p),
        }
    };
    let meta = fs::symlink_metadata(root).map_err(|e| unreadable(root, e))?;
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), Vec::<String>::new(), meta)];
    while let Some((path, rel, meta)) = stack.pop() {
        if meta.is_file() {
            let entry = FileEntry {
                path: rel,
                kind: ItemKind::File,
                size: meta.len(),
            };
            out.push(Source { entry, path });
            continue;
        }
        if !meta.is_dir() {
            info!("{} is a symlink or special file; skipped", path.display());
            continue;
        }
        let mut children = Vec::new();
        for de in fs::read_dir(&path).map_err(|e| unreadable(&path, e))? {
            let p = de.map_err(|e| unreadable(&path, e))?.path();
            let m = fs::symlink_metadata(&p).map_err(|e| unreadable(&p, e))?;
            children.push((p, m));
        }
        if !children.is_empty() && rel.len() >= MAX_PATH_DEPTH {
            warn!(
                "{} is more than {MAX_PATH_DEPTH} folders deep",
                children[0].0.display()
            );
            return Err(FailReason::Unreadable {
                name: display_name(&children[0].0),
            });
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));
        // Reversed, so the stack hands them out in order.
        for (p, m) in children.into_iter().rev() {
            let mut r = rel.clone();
            r.push(display_name(&p));
            stack.push((p, r, m));
        }
        let entry = FileEntry {
            path: rel,
            kind: ItemKind::Dir,
            size: 0,
        };
        out.push(Source { entry, path });
    }
    if out.first().is_none_or(|s| !s.entry.path.is_empty()) {
        return Err(FailReason::Unreadable {
            name: display_name(root),
        });
    }
    Ok(out)
}

/// Sends every entry; `first` is the first file, if already open.
async fn send_entries(
    out: &mut FilesOut,
    sources: &[Source],
    mut first: Option<tokio::fs::File>,
    pacer: &SharedPacer,
    cancel: &CancellationToken,
) -> Result<(), Stop> {
    let mut buf = vec![0u8; CHUNK];
    for src in sources {
        unless_cancelled(cancel, out.entry(&src.entry))
            .await?
            .map_err(Stop::Stream)?;
        if src.entry.kind == ItemKind::File {
            let file = match first.take() {
                Some(f) => f,
                None => open_source(src, cancel).await?,
            };
            send_file(out, src, file, &mut buf, pacer, cancel).await?;
        }
    }
    Ok(())
}

async fn open_source(src: &Source, cancel: &CancellationToken) -> Result<tokio::fs::File, Stop> {
    unless_cancelled(cancel, tokio::fs::File::open(&src.path))
        .await?
        .map_err(|e| unreadable(src, e))
}

fn unreadable(src: &Source, e: io::Error) -> Stop {
    warn!("{} cannot be sent: {e}", src.path.display());
    Stop::Fail(FailReason::Unreadable {
        name: display_name(&src.path),
    })
}

/// Sends a file's bytes, reading to its end: a file that is not the size
/// `stat` gave when it was walked has changed under us.
async fn send_file(
    out: &mut FilesOut,
    src: &Source,
    mut file: tokio::fs::File,
    buf: &mut [u8],
    pacer: &SharedPacer,
    cancel: &CancellationToken,
) -> Result<(), Stop> {
    let changed = || {
        warn!("{} changed while it was being sent", src.path.display());
        Stop::Fail(FailReason::Changed {
            name: display_name(&src.path),
        })
    };
    let mut left = src.entry.size;
    loop {
        let n = unless_cancelled(cancel, file.read(buf))
            .await?
            .map_err(|e| unreadable(src, e))?;
        #[cfg(test)]
        hooks::after_read(&src.path);
        if n == 0 {
            break;
        }
        if n as u64 > left {
            return Err(changed());
        }
        let wait = pacer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .delay_for(n, Instant::now());
        if !wait.is_zero() {
            unless_cancelled(cancel, tokio::time::sleep(wait)).await?;
        }
        unless_cancelled(cancel, out.write(&buf[..n]))
            .await?
            .map_err(Stop::Stream)?;
        left -= n as u64;
    }
    if left > 0 {
        return Err(changed());
    }
    Ok(())
}

/// Receives the item on `files` into `dest` and returns where it landed.
/// It is written under a `.cjpart` name beside its final path and renamed
/// into place once complete; an existing file or folder at the final path
/// is never replaced (`Unwritable`). `progress` gets the bytes received so
/// far at most every 100 ms, and the total at the end.
pub async fn receive_item(
    mut files: FilesIn,
    dest: Dest,
    space: Arc<dyn FreeSpace>,
    mut progress: impl FnMut(u64) + Send,
    cancel: &CancellationToken,
) -> Result<PathBuf, FailReason> {
    let header = files.header().clone();
    let Some((parent, name, target)) = place(&dest, &header) else {
        files.cancel();
        return Err(FailReason::Unwritable);
    };
    let spot = Spot {
        parent: &parent,
        part: &parent.join(part_name(&name)),
        target: &target,
    };
    // Removes the part however this ends short of the rename: failure,
    // cancel, a panic, or the future being dropped.
    let mut guard = PartGuard(Some(spot.part.to_path_buf()));
    let mut report = Progress {
        report: &mut progress,
        done: 0,
        last: Instant::now(),
    };
    let received = receive_into(&mut files, &header, spot, space, &mut report, cancel).await;
    match received {
        Ok(()) => {
            guard.disarm();
            progress(header.total_bytes);
            Ok(target)
        }
        Err(stop) => {
            files.cancel();
            // Off the runtime's threads where it can be; the guard is for
            // when it can't.
            if let Err(e) = remove_any(spot.part).await {
                warn!("{} not cleaned up: {e}", spot.part.display());
            } else {
                guard.disarm();
            }
            Err(stop.reason(cancel))
        }
    }
}

/// The folder a received item lands in, the name it is known by there, and
/// its final path; None for an `Exact` path without both.
fn place(dest: &Dest, header: &FilesHeader) -> Option<(PathBuf, String, PathBuf)> {
    let (parent, name, target) = match dest {
        Dest::Exact(p) => {
            let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
                warn!("cannot write a received item to {}", p.display());
                return None;
            };
            (parent, name.to_string_lossy().into_owned(), p.clone())
        }
        Dest::InDir(d) => {
            let name = sanitize_component(&header.name);
            (d.as_path(), name.clone(), d.join(name))
        }
    };
    if !parent.as_os_str().is_empty() {
        return Some((parent.to_path_buf(), name, target));
    }
    match std::path::absolute(".") {
        Ok(cwd) => Some((cwd, name, target)),
        Err(e) => {
            warn!("no current folder for {}: {e}", target.display());
            None
        }
    }
}

/// A part to remove when dropped, unless disarmed once the item is in
/// place. It removes synchronously, since a drop can't await.
pub(crate) struct PartGuard(Option<PathBuf>);

impl PartGuard {
    pub(crate) fn new(part: PathBuf) -> PartGuard {
        PartGuard(Some(part))
    }

    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        let Some(p) = self.0.take() else { return };
        let removed = match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&p),
            Ok(_) => fs::remove_file(&p),
            Err(e) => Err(e),
        };
        match removed {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                warn!("{} not cleaned up: {e}", p.display());
            }
            _ => {}
        }
    }
}

/// Where a received item is written.
#[derive(Clone, Copy)]
struct Spot<'a> {
    /// The folder it lands in.
    parent: &'a Path,
    /// Its path until complete.
    part: &'a Path,
    /// Its path once complete.
    target: &'a Path,
}

/// Reports cumulative bytes at most every `PROGRESS_EVERY`.
struct Progress<'a> {
    report: &'a mut (dyn FnMut(u64) + Send),
    done: u64,
    last: Instant,
}

impl Progress<'_> {
    fn add(&mut self, n: usize) {
        self.done += n as u64;
        let now = Instant::now();
        if now.duration_since(self.last) >= PROGRESS_EVERY {
            self.last = now;
            (self.report)(self.done);
        }
    }
}

/// Folders and files are created, and the part renamed into place,
/// synchronously, not on tokio's blocking pool: a creation still running
/// there when the future is dropped could land after the `PartGuard` had
/// cleaned up, and a rename could land a tree the guard was halfway through
/// removing.
async fn receive_into(
    files: &mut FilesIn,
    header: &FilesHeader,
    spot: Spot<'_>,
    space: Arc<dyn FreeSpace>,
    progress: &mut Progress<'_>,
    cancel: &CancellationToken,
) -> Result<(), Stop> {
    let dir = spot.parent.to_path_buf();
    let free = tokio::task::spawn_blocking(move || space.free_bytes(&dir))
        .await
        .map_err(|e| Stop::Disk(io::Error::other(e)))?
        .map_err(Stop::Disk)?;
    if free < header.total_bytes.saturating_add(SPACE_MARGIN) {
        info!(
            "{free} bytes free in {}; a {}-byte item needs {SPACE_MARGIN} more",
            spot.parent.display(),
            header.total_bytes
        );
        return Err(Stop::Fail(FailReason::NoSpace));
    }
    // Left by a crash.
    remove_any(spot.part).await.map_err(Stop::Disk)?;
    let mut buf = vec![0u8; CHUNK];
    let mut hidden = false;
    while let Some(entry) = unless_cancelled(cancel, files.next_entry())
        .await?
        .map_err(Stop::Stream)?
    {
        let Some(rel) = safe_rel_path(&entry.path) else {
            warn!("refused an entry path in {:?}", header.name);
            return Err(Stop::Fail(FailReason::Unreadable {
                name: header.name.clone(),
            }));
        };
        // Joining "" would add a trailing separator.
        let path = if entry.path.is_empty() {
            spot.part.to_path_buf()
        } else {
            spot.part.join(rel)
        };
        let root = entry.path.is_empty();
        match entry.kind {
            ItemKind::Dir => {
                fs::create_dir_all(&path).map_err(Stop::Disk)?;
                hidden |= root && hide(&path);
            }
            ItemKind::File => {
                if !root && let Some(dir) = path.parent() {
                    fs::create_dir_all(dir).map_err(Stop::Disk)?;
                }
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(Stop::Disk)?;
                hidden |= root && hide(&path);
                let file = tokio::fs::File::from_std(file);
                receive_file(files, file, &mut buf, progress, cancel).await?;
            }
        }
    }
    if hidden {
        unhide(spot.part).map_err(Stop::Disk)?;
    }
    match rename_new(spot.part, spot.target).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            info!("{} already exists; not replaced", spot.target.display());
            Err(Stop::Fail(FailReason::Unwritable))
        }
        Err(e) => Err(Stop::Disk(e)),
    }
}

/// Writes the current entry's bytes to `file`, just created, then flushes
/// and closes it.
async fn receive_file(
    files: &mut FilesIn,
    file: tokio::fs::File,
    buf: &mut [u8],
    progress: &mut Progress<'_>,
    cancel: &CancellationToken,
) -> Result<(), Stop> {
    let mut w = BufWriter::with_capacity(CHUNK, file);
    let written = async {
        loop {
            let n = unless_cancelled(cancel, files.read_chunk(buf))
                .await?
                .map_err(Stop::Stream)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n]).await.map_err(Stop::Disk)?;
            progress.add(n);
        }
        w.flush().await.map_err(Stop::Disk)
    }
    .await;
    if written.is_err() {
        // Waits for a write still in flight, which holds the file open, so
        // it is closed before the `.cjpart` is removed; what is still in the
        // buffer is dropped, not written.
        let _ = w.get_mut().flush().await;
    }
    written
}

/// Removes the file or folder tree at `p`, if there is one.
async fn remove_any(p: &Path) -> io::Result<()> {
    let removed = match tokio::fs::symlink_metadata(p).await {
        Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(p).await,
        Ok(_) => tokio::fs::remove_file(p).await,
        Err(e) => Err(e),
    };
    match removed {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The name an item is written under until it is complete:
/// `.{name}.cjpart`, or, if that would be too long for a file name, `name`
/// cut short with a hash of it added, so long names that differ only past
/// the cut still get different part names.
pub(crate) fn part_name(name: &str) -> String {
    let fits = |bytes: usize, units: usize| bytes <= MAX_NAME_LEN && units <= MAX_NAME_LEN;
    let plain = format!(".{name}.cjpart");
    if fits(plain.len(), plain.encode_utf16().count()) {
        return plain;
    }
    let tail = format!("~{:016x}.cjpart", fnv1a(name.as_bytes()));
    let mut out = String::from(".");
    let (mut bytes, mut units) = (1 + tail.len(), 1 + tail.len());
    for c in name.chars() {
        bytes += c.len_utf8();
        units += c.len_utf16();
        if !fits(bytes, units) {
            break;
        }
        out.push(c);
    }
    out.push_str(&tail);
    out
}

/// 64-bit FNV-1a: unlike std's hashers, the same in every Rust release,
/// so a part left by a crash is still found after an update.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Hides a part in Explorer so one left by a hard kill isn't in plain
/// sight; best effort. Whether it is now hidden.
#[cfg(windows)]
fn hide(p: &Path) -> bool {
    match set_hidden(p, true) {
        Ok(()) => true,
        Err(e) => {
            info!("{} not hidden: {e}", p.display());
            false
        }
    }
}

/// On the Mac its leading dot already hides it.
#[cfg(not(windows))]
fn hide(_p: &Path) -> bool {
    false
}

#[cfg(windows)]
fn unhide(p: &Path) -> io::Result<()> {
    set_hidden(p, false)
}

#[cfg(not(windows))]
fn unhide(_p: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn set_hidden(p: &Path, hidden: bool) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, SetFileAttributesW,
    };
    use windows::core::HSTRING;
    // The attributes SetFileAttributesW takes: read-only, hidden, system,
    // archive, normal, temporary, offline and not-content-indexed.
    const SETTABLE: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x80 | 0x100 | 0x1000 | 0x2000;
    let attrs = fs::symlink_metadata(p)?.file_attributes() & SETTABLE;
    let attrs = match hidden {
        true => attrs | FILE_ATTRIBUTE_HIDDEN.0,
        false => attrs & !FILE_ATTRIBUTE_HIDDEN.0,
    };
    let attrs = if attrs == 0 {
        FILE_ATTRIBUTE_NORMAL
    } else {
        FILE_FLAGS_AND_ATTRIBUTES(attrs)
    };
    let name = HSTRING::from(verbatim(p)?.as_path());
    // SAFETY: the name is a valid wide string.
    unsafe { SetFileAttributesW(&name, attrs) }.map_err(win_io_error)
}

/// Renames `from` to `to`, failing with `AlreadyExists` rather than
/// replacing anything already at `to`. Each try runs on this thread (see
/// `receive_into`); only the waits between them yield.
#[cfg(windows)]
async fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
    use windows::core::HSTRING;
    let from = HSTRING::from(verbatim(from)?.as_path());
    let to = HSTRING::from(verbatim(to)?.as_path());
    let mut tries = 0;
    loop {
        // Without MOVEFILE_REPLACE_EXISTING an existing target is an error.
        // SAFETY: both names are valid wide strings.
        let moved = unsafe { MoveFileExW(&from, &to, MOVE_FILE_FLAGS(0)) }.map_err(win_io_error);
        match moved {
            // A virus scanner may still hold a just-written file open, which
            // blocks renaming the folder it is in for a moment.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && tries < 10 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => return other,
        }
    }
}

/// Renames `from` to `to`, failing with `AlreadyExists` rather than
/// replacing anything already at `to`.
/// Synchronous, fallback included (see `receive_into`).
#[cfg(target_os = "macos")]
async fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (c_path(from)?, c_path(to)?);
    // SAFETY: both paths are NUL-terminated.
    if unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    // A volume without RENAME_EXCL (some network and FAT volumes): look
    // first instead, which leaves a moment for something to appear.
    if e.raw_os_error() != Some(libc::ENOTSUP) {
        return Err(e);
    }
    if fs::symlink_metadata(to).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    fs::rename(from, to)
}

/// `p` made absolute with a `\\?\` prefix, so the Win32 calls made
/// directly here (std's own already do this) take paths beyond MAX_PATH:
/// the app isn't marked long-path aware.
#[cfg(windows)]
fn verbatim(p: &Path) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Prefix};
    // On Windows this also resolves `.` and `..` and turns `/` into `\`,
    // as a verbatim path needs.
    let abs = std::path::absolute(p)?;
    let wide: Vec<u16> = match abs.components().next() {
        Some(Component::Prefix(pre)) => match pre.kind() {
            Prefix::Disk(_) => r"\\?\"
                .encode_utf16()
                .chain(abs.as_os_str().encode_wide())
                .collect(),
            // `\\server\share\x` becomes `\\?\UNC\server\share\x`.
            Prefix::UNC(..) => r"\\?\UNC"
                .encode_utf16()
                .chain(abs.as_os_str().encode_wide().skip(1))
                .collect(),
            // Already verbatim, or a device path.
            _ => return Ok(abs),
        },
        _ => return Ok(abs),
    };
    Ok(OsString::from_wide(&wide).into())
}

#[cfg(windows)]
fn win_io_error(e: windows::core::Error) -> io::Error {
    // A Win32 error arrives as an HRESULT with FACILITY_WIN32 (7) and the
    // code in the low 16 bits; anything else isn't an OS error code.
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(e)
    }
}

#[cfg(not(windows))]
fn c_path(p: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(std::ffi::CString::new(p.as_os_str().as_bytes())?)
}

/// Test seam: something to do to a file each time the sender has read from
/// it, to change it mid-send.
#[cfg(test)]
pub(super) mod hooks {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Called with the file's path; registered per path, since tests run
    /// in parallel.
    pub type Hook = fn(&Path);

    pub static AFTER_READ: Mutex<Vec<(PathBuf, Hook)>> = Mutex::new(Vec::new());

    pub fn after_read(path: &Path) {
        let hook = AFTER_READ
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, f)| *f);
        if let Some(f) = hook {
            f(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_core::dnd::ItemKind;
    use cj_net::session::{IncomingStream, Listener, connect};
    use cj_net::{Identity, Trust};
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tokio::sync::watch;
    use tokio::time::timeout;

    const T: Duration = Duration::from_secs(30);

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

    fn pacer() -> SharedPacer {
        Arc::new(std::sync::Mutex::new(Pacer::new(Instant::now())))
    }

    async fn files_in(s: &Session) -> FilesIn {
        match timeout(T, s.accept_stream()).await.unwrap().unwrap() {
            IncomingStream::Files(f) => f,
            other => panic!("expected a files stream, got {other:?}"),
        }
    }

    /// A disk with this much free space.
    struct Space(u64);

    impl FreeSpace for Space {
        fn free_bytes(&self, _dir: &Path) -> io::Result<u64> {
            Ok(self.0)
        }
    }

    /// A disk with all the space anyone could want.
    fn lots() -> Arc<dyn FreeSpace> {
        Arc::new(Space(u64::MAX))
    }

    /// Sends `root` from one end of a fresh session and receives it into
    /// `dest` at the other.
    async fn transfer(
        root: &Path,
        dest: Dest,
        space: Arc<dyn FreeSpace>,
    ) -> (Result<(), FailReason>, Result<PathBuf, FailReason>) {
        let (_l, sa, sb) = session_pair().await;
        let never = CancellationToken::new();
        let p = pacer();
        timeout(T, async {
            tokio::join!(send_item(&sa, 7, 0, root, &p, &never), async {
                let f = files_in(&sb).await;
                receive_item(f, dest, space, |_| {}, &never).await
            })
        })
        .await
        .unwrap()
    }

    /// Like `transfer`, for a sender that fails once the stream is open.
    /// Its reset may reach the receiver before the stream's tag is read,
    /// so accepting it fails (None), or after, so the receive is cancelled.
    async fn transfer_cut_short(
        root: &Path,
        dest: Dest,
    ) -> (Result<(), FailReason>, Option<Result<PathBuf, FailReason>>) {
        let (_l, sa, sb) = session_pair().await;
        let never = CancellationToken::new();
        let p = pacer();
        timeout(T, async {
            tokio::join!(send_item(&sa, 7, 0, root, &p, &never), async {
                match sb.accept_stream().await {
                    Ok(IncomingStream::Files(f)) => {
                        Some(receive_item(f, dest, lots(), |_| {}, &never).await)
                    }
                    Ok(other) => panic!("expected a files stream, got {other:?}"),
                    Err(e) => {
                        let e = format!("{e:#}");
                        assert!(e.contains("reset by peer"), "{e}");
                        None
                    }
                }
            })
        })
        .await
        .unwrap()
    }

    fn bytes(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// Everything under `dir` by path relative to it: None for a folder,
    /// the contents for a file.
    fn tree(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for de in fs::read_dir(&d).unwrap() {
                let p = de.unwrap().path();
                let rel = p.strip_prefix(dir).unwrap().to_path_buf();
                if p.is_dir() {
                    stack.push(p);
                    out.insert(rel, None);
                } else {
                    out.insert(rel, Some(fs::read(&p).unwrap()));
                }
            }
        }
        out
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

    /// 300 files over three levels, an empty folder and a 0-byte file.
    fn album(at: &Path) -> PathBuf {
        let root = at.join("Album");
        for a in 0..3 {
            for b in 0..10 {
                let d = root.join(format!("d{a}")).join(format!("s{b}"));
                fs::create_dir_all(&d).unwrap();
                for c in 0..10 {
                    let n = a * 1000 + b * 100 + c;
                    fs::write(d.join(format!("f{c}.bin")), bytes(1000 + n, n as u8)).unwrap();
                }
            }
        }
        fs::create_dir(root.join("empty")).unwrap();
        fs::write(root.join("zero.txt"), b"").unwrap();
        root
    }

    #[tokio::test]
    async fn single_file_roundtrip_in_dir() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = src.path().join("report.pdf");
        let data = bytes(CHUNK * 4 + 17, 3);
        fs::write(&root, &data).unwrap();

        let (sent, received) = transfer(
            &root,
            Dest::InDir(dest.path().into()),
            Arc::new(OsFreeSpace),
        )
        .await;
        assert_eq!(sent, Ok(()));
        assert_eq!(received, Ok(dest.path().join("report.pdf")));
        assert_eq!(fs::read(dest.path().join("report.pdf")).unwrap(), data);
        assert_eq!(names(dest.path()), ["report.pdf"]);
        #[cfg(windows)]
        assert!(!is_hidden(&dest.path().join("report.pdf")), "still hidden");
    }

    #[cfg(windows)]
    fn is_hidden(p: &Path) -> bool {
        use std::os::windows::fs::MetadataExt;
        fs::metadata(p).unwrap().file_attributes() & 0x2 != 0
    }

    #[tokio::test]
    async fn exact_dest_is_respected() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = src.path().join("orig.bin");
        fs::write(&root, bytes(5000, 9)).unwrap();
        let exact = dest.path().join("Promised Name.bin");

        let (sent, received) = transfer(&root, Dest::Exact(exact.clone()), lots()).await;
        assert_eq!(sent, Ok(()));
        assert_eq!(received, Ok(exact.clone()));
        assert_eq!(fs::read(&exact).unwrap(), bytes(5000, 9));
        assert_eq!(names(dest.path()), ["Promised Name.bin"]);
    }

    #[tokio::test]
    async fn folder_tree_roundtrip_keeps_empty_dirs_and_zero_byte_files() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = album(src.path());

        let (sent, received) = transfer(
            &root,
            Dest::InDir(dest.path().into()),
            Arc::new(OsFreeSpace),
        )
        .await;
        assert_eq!(sent, Ok(()));
        assert_eq!(received, Ok(dest.path().join("Album")));
        let (want, got) = (tree(&root), tree(&dest.path().join("Album")));
        assert_eq!(want.values().filter(|v| v.is_some()).count(), 301);
        assert_eq!(got.get(Path::new("empty")), Some(&None));
        assert_eq!(got.get(Path::new("zero.txt")), Some(&Some(vec![])));
        assert!(got == want, "trees differ");
        assert_eq!(names(dest.path()), ["Album"]);
    }

    #[tokio::test]
    async fn walk_puts_each_folder_before_what_is_in_it() {
        let src = tempfile::tempdir().unwrap();
        let root = album(src.path());
        let entries = walk(&root).unwrap();
        assert_eq!(entries.len(), 1 + 3 + 30 + 300 + 2);
        assert!(entries[0].entry.path.is_empty());
        for (i, e) in entries.iter().enumerate().skip(1) {
            let parent = &e.entry.path[..e.entry.path.len() - 1];
            assert!(
                entries[..i]
                    .iter()
                    .any(|p| p.entry.kind == ItemKind::Dir && p.entry.path == parent),
                "{:?} comes before its folder",
                e.entry.path
            );
        }
    }

    /// Makes symlinks (or, on Windows, a junction) in `root` to `file` and
    /// `dir`; false if this system won't let us make any.
    fn make_links(root: &Path, file: &Path, dir: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(file, root.join("link.txt")).unwrap();
            std::os::unix::fs::symlink(dir, root.join("linkdir")).unwrap();
            true
        }
        #[cfg(windows)]
        {
            // Symlinks need Developer Mode on Windows; junctions don't.
            let file_link = std::os::windows::fs::symlink_file(file, root.join("link.txt")).is_ok();
            let junction = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(root.join("linkdir"))
                .arg(dir)
                .output()
                .is_ok_and(|o| o.status.success());
            file_link || junction
        }
    }

    #[tokio::test]
    async fn symlinks_are_skipped() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let outside = src.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"not sent").unwrap();
        let root = src.path().join("Linked");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.txt"), b"sent").unwrap();
        if !make_links(&root, &outside.join("secret.txt"), &outside) {
            eprintln!("skipped: no symlink or junction could be made here");
            return;
        }

        let (sent, received) = transfer(&root, Dest::InDir(dest.path().into()), lots()).await;
        assert_eq!(sent, Ok(()));
        assert_eq!(received, Ok(dest.path().join("Linked")));
        let got = tree(&dest.path().join("Linked"));
        assert_eq!(
            got,
            BTreeMap::from([(PathBuf::from("a.txt"), Some(b"sent".to_vec()))])
        );
    }

    fn grow(p: &Path) {
        let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(&[0; 1024]).unwrap();
    }

    fn shrink(p: &Path) {
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_len(f.metadata().unwrap().len() / 2).unwrap();
    }

    #[tokio::test]
    async fn file_that_changes_size_fails_with_changed() {
        for change in [grow as fn(&Path), shrink] {
            let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let root = src.path().join("growing.log");
            fs::write(&root, bytes(CHUNK * 4, 1)).unwrap();
            hooks::AFTER_READ
                .lock()
                .unwrap()
                .push((root.clone(), change));

            let (sent, received) = transfer_cut_short(&root, Dest::InDir(dest.path().into())).await;
            assert_eq!(
                sent,
                Err(FailReason::Changed {
                    name: "growing.log".into()
                })
            );
            assert!(
                matches!(received, None | Some(Err(FailReason::Cancelled))),
                "{received:?}"
            );
            assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));
        }
    }

    #[tokio::test]
    async fn unopenable_file_fails_with_unreadable() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = src.path().join("Docs");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.txt"), bytes(CHUNK + 1, 2)).unwrap();
        let locked = root.join("locked.txt");
        fs::write(&locked, b"private").unwrap();
        // Held open with no sharing on Windows; mode 000 on unix.
        #[cfg(windows)]
        let _hold = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&locked)
                .unwrap()
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::read(&locked).is_ok() {
                eprintln!("skipped: running as root, which reads anything");
                return;
            }
        }

        let unreadable = Err(FailReason::Unreadable {
            name: "locked.txt".into(),
        });
        let (sent, received) = transfer_cut_short(&root, Dest::InDir(dest.path().into())).await;
        assert_eq!(sent, unreadable);
        assert!(
            matches!(received, None | Some(Err(FailReason::Cancelled))),
            "{received:?}"
        );
        assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));

        // Dragged on its own, it fails before any stream is opened.
        let (_l, sa, sb) = session_pair().await;
        let never = CancellationToken::new();
        let sent = send_item(&sa, 7, 1, &locked, &pacer(), &never).await;
        assert_eq!(sent, unreadable);
        let stream = timeout(Duration::from_millis(300), sb.accept_stream()).await;
        assert!(stream.is_err(), "a stream arrived: {stream:?}");
    }

    #[tokio::test]
    async fn missing_source_fails_before_opening_a_stream() {
        let (_l, sa, _sb) = session_pair().await;
        let src = tempfile::tempdir().unwrap();
        let got = send_item(
            &sa,
            7,
            0,
            &src.path().join("gone.txt"),
            &pacer(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            got,
            Err(FailReason::Unreadable {
                name: "gone.txt".into()
            })
        );
    }

    #[tokio::test]
    async fn receive_aborted_mid_file_leaves_nothing() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = src.path().join("big.bin");
        fs::write(&root, bytes(32 << 20, 1)).unwrap();
        let (_l, sa, sb) = session_pair().await;
        let never = CancellationToken::new();
        let sender = tokio::spawn({
            let (sa, root, never) = (sa.clone(), root.clone(), never.clone());
            async move { send_item(&sa, 7, 0, &root, &pacer(), &never).await }
        });

        let f = files_in(&sb).await;
        let (got_tx, mut got_rx) = watch::channel(0u64);
        let (received, ()) = timeout(T, async {
            tokio::join!(
                receive_item(
                    f,
                    Dest::InDir(dest.path().into()),
                    lots(),
                    move |n| {
                        got_tx.send_replace(n);
                    },
                    &never
                ),
                async {
                    if got_rx.wait_for(|n| *n >= 5 << 20).await.is_ok() {
                        sa.close();
                    }
                }
            )
        })
        .await
        .unwrap();
        assert_eq!(received, Err(FailReason::Disconnected));
        assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));
        let sent = timeout(T, sender).await.unwrap().unwrap();
        assert_eq!(sent, Err(FailReason::Disconnected));
    }

    /// A folder of four 4 MB files.
    fn videos(at: &Path) -> PathBuf {
        let root = at.join("Videos");
        fs::create_dir_all(root.join("raw")).unwrap();
        for i in 0..4u8 {
            fs::write(
                root.join("raw").join(format!("v{i}.mov")),
                bytes(4 << 20, i),
            )
            .unwrap();
        }
        root
    }

    #[tokio::test]
    async fn cancel_cleans_up_and_reports_cancelled() {
        // The receiver cancels, then the sender does; both mid-folder.
        for receiver_cancels in [true, false] {
            let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let root = videos(src.path());
            let (_l, sa, sb) = session_pair().await;
            let (send_cancel, recv_cancel) = (CancellationToken::new(), CancellationToken::new());
            let trip = if receiver_cancels {
                recv_cancel.clone()
            } else {
                send_cancel.clone()
            };
            // The receiver's session clone, so its end stays up when it
            // is done.
            let receiver = tokio::spawn({
                let (sb, dest) = (sb.clone(), dest.path().to_path_buf());
                let recv_cancel = recv_cancel.clone();
                async move {
                    let f = files_in(&sb).await;
                    let progress = move |n| {
                        if n >= 6 << 20 {
                            trip.cancel();
                        }
                    };
                    receive_item(f, Dest::InDir(dest), lots(), progress, &recv_cancel).await
                }
            });
            let sent = timeout(T, send_item(&sa, 7, 0, &root, &pacer(), &send_cancel))
                .await
                .unwrap();
            let received = timeout(T, receiver).await.unwrap().unwrap();
            assert_eq!(sent, Err(FailReason::Cancelled), "{receiver_cancels}");
            assert_eq!(received, Err(FailReason::Cancelled), "{receiver_cancels}");
            assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));
        }
    }

    /// The receive is aborted, or panics, mid-folder: its part is removed
    /// as the future is dropped. (On Windows the part is hidden meanwhile.)
    #[tokio::test]
    async fn dropped_or_panicking_receive_leaves_nothing() {
        for abort in [true, false] {
            let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let root = videos(src.path());
            let (_l, sa, sb) = session_pair().await;
            let never = CancellationToken::new();
            let (got_tx, mut got_rx) = watch::channel(0u64);
            let receiver = tokio::spawn({
                let (sb, dest, never) = (sb.clone(), dest.path().to_path_buf(), never.clone());
                async move {
                    let f = files_in(&sb).await;
                    let progress = move |n| {
                        if !abort && n >= 6 << 20 {
                            panic!("progress panics on purpose");
                        }
                        got_tx.send_replace(n);
                    };
                    receive_item(f, Dest::InDir(dest), lots(), progress, &never).await
                }
            });
            let sender = tokio::spawn({
                let (sa, root, never) = (sa.clone(), root.clone(), never.clone());
                async move { send_item(&sa, 7, 0, &root, &pacer(), &never).await }
            });
            if abort {
                timeout(T, got_rx.wait_for(|n| *n >= 6 << 20))
                    .await
                    .unwrap()
                    .unwrap();
                let part = dest.path().join(".Videos.cjpart");
                assert!(part.is_dir(), "{:?}", names(dest.path()));
                #[cfg(windows)]
                assert!(is_hidden(&part), "part not hidden");
                receiver.abort();
            }
            let joined = timeout(T, receiver).await.unwrap();
            assert!(joined.is_err(), "{joined:?}");
            // The drop has run by the time the join reports it; poll anyway
            // in case removal is slow.
            let deadline = Instant::now() + Duration::from_secs(5);
            while !names(dest.path()).is_empty() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));
            let sent = timeout(T, sender).await.unwrap().unwrap();
            assert_eq!(sent, Err(FailReason::Cancelled), "abort {abort}");
        }
    }

    /// A disk with this much free space that notes where it was asked about.
    struct Recorded(u64, Mutex<Vec<PathBuf>>);

    impl FreeSpace for Recorded {
        fn free_bytes(&self, dir: &Path) -> io::Result<u64> {
            self.1.lock().unwrap().push(dir.to_path_buf());
            Ok(self.0)
        }
    }

    #[tokio::test]
    async fn insufficient_space_fails_before_writing() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = src.path().join("disk.img");
        fs::write(&root, bytes(1000, 0)).unwrap();

        let space = Arc::new(Recorded(1000 + SPACE_MARGIN - 1, Mutex::default()));
        let (sent, received) =
            transfer(&root, Dest::InDir(dest.path().into()), space.clone()).await;
        assert_eq!(received, Err(FailReason::NoSpace));
        assert!(
            matches!(sent, Ok(()) | Err(FailReason::Cancelled)),
            "{sent:?}"
        );
        assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));
        assert_eq!(*space.1.lock().unwrap(), [dest.path()]);

        // Exactly enough is enough.
        let space = Arc::new(Recorded(1000 + SPACE_MARGIN, Mutex::default()));
        let exact = dest.path().join("x.img");
        let (sent, received) = transfer(&root, Dest::Exact(exact.clone()), space.clone()).await;
        assert_eq!((sent, received), (Ok(()), Ok(exact)));
        assert_eq!(*space.1.lock().unwrap(), [dest.path()]);
    }

    /// Makes `dir` a folder nothing can be created in, until dropped.
    struct Unwritable(PathBuf);

    impl Unwritable {
        /// None (the test is skipped) if this user can create files in it
        /// anyway, as root can on unix, or its ACL can't be changed.
        ///
        /// `set_readonly` on a Windows folder doesn't stop files being
        /// created in it, so on Windows the folder instead gets an ACL entry
        /// denying everyone (S-1-1-0) the right to add files and folders.
        fn new(dir: &Path) -> Option<Unwritable> {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
            }
            #[cfg(windows)]
            {
                let ok = std::process::Command::new("icacls")
                    .arg(dir)
                    .args(["/deny", "*S-1-1-0:(WD,AD)"])
                    .output()
                    .unwrap()
                    .status
                    .success();
                if !ok {
                    eprintln!("skipped: icacls could not change this folder's ACL");
                    return None;
                }
            }
            let guard = Unwritable(dir.to_path_buf());
            match fs::write(dir.join("probe"), b"") {
                Ok(()) => {
                    let _ = fs::remove_file(dir.join("probe"));
                    eprintln!("skipped: this user can write to a read-only folder");
                    None
                }
                Err(_) => Some(guard),
            }
        }
    }

    impl Drop for Unwritable {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
            #[cfg(windows)]
            let _ = std::process::Command::new("icacls")
                .arg(&self.0)
                .args(["/remove:d", "*S-1-1-0"])
                .output();
        }
    }

    #[tokio::test]
    async fn unwritable_destination_fails_cleanly() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = album(src.path());
        let file = src.path().join("one.txt");
        fs::write(&file, b"one").unwrap();
        let Some(_locked) = Unwritable::new(dest.path()) else {
            return;
        };

        for (root, dest) in [
            (&root, Dest::InDir(dest.path().into())),
            (&file, Dest::Exact(dest.path().join("one.txt"))),
        ] {
            // The sender may have finished before the refusal reached it.
            let (sent, received) = transfer(root, dest, lots()).await;
            assert_eq!(received, Err(FailReason::Unwritable));
            assert!(
                matches!(sent, Ok(()) | Err(FailReason::Cancelled)),
                "{sent:?}"
            );
        }
        assert!(names(dest.path()).is_empty(), "{:?}", names(dest.path()));
    }

    #[tokio::test]
    async fn existing_destination_is_never_overwritten() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let file = src.path().join("report.pdf");
        fs::write(&file, b"new").unwrap();
        let root = album(src.path());
        fs::write(dest.path().join("report.pdf"), b"keep me").unwrap();
        fs::create_dir(dest.path().join("Album")).unwrap();
        fs::write(dest.path().join("Album").join("mine.txt"), b"mine").unwrap();

        for (root, dest) in [
            (&file, Dest::InDir(dest.path().into())),
            (&file, Dest::Exact(dest.path().join("report.pdf"))),
            (&root, Dest::InDir(dest.path().into())),
        ] {
            let (_, received) = transfer(root, dest.clone(), lots()).await;
            assert_eq!(received, Err(FailReason::Unwritable), "{dest:?}");
        }
        assert_eq!(names(dest.path()), ["Album", "report.pdf"]);
        assert_eq!(
            fs::read(dest.path().join("report.pdf")).unwrap(),
            b"keep me"
        );
        assert_eq!(names(&dest.path().join("Album")), ["mine.txt"]);
    }

    #[tokio::test]
    async fn stale_part_from_a_crash_is_replaced() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = src.path().join("report.pdf");
        fs::write(&root, b"fresh").unwrap();
        let stale = dest.path().join(".report.pdf.cjpart");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("junk"), b"junk").unwrap();

        let (sent, received) = transfer(&root, Dest::InDir(dest.path().into()), lots()).await;
        assert_eq!(sent, Ok(()));
        assert_eq!(received, Ok(dest.path().join("report.pdf")));
        assert_eq!(names(dest.path()), ["report.pdf"]);
        assert_eq!(fs::read(dest.path().join("report.pdf")).unwrap(), b"fresh");
    }

    #[tokio::test]
    async fn progress_is_monotonic_and_ends_at_total() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = src.path().join("movie.mp4");
        let total = 6 << 20;
        fs::write(&root, bytes(total, 5)).unwrap();
        let (_l, sa, sb) = session_pair().await;
        let never = CancellationToken::new();
        let mut seen: Vec<(Instant, u64)> = Vec::new();
        let p = pacer();

        let (sent, received) = timeout(T, async {
            tokio::join!(send_item(&sa, 7, 0, &root, &p, &never), async {
                let f = files_in(&sb).await;
                let progress = |n| seen.push((Instant::now(), n));
                receive_item(f, Dest::InDir(dest.path().into()), lots(), progress, &never).await
            })
        })
        .await
        .unwrap();
        assert_eq!(sent, Ok(()));
        assert!(received.is_ok());
        assert_eq!(seen.last().map(|s| s.1), Some(total as u64));
        assert!(seen.windows(2).all(|w| w[0].1 <= w[1].1), "{seen:?}");
        // Every report but the final one is at least 100 ms after the last.
        let early = &seen[..seen.len() - 1];
        assert!(
            early
                .windows(2)
                .all(|w| w[1].0 - w[0].0 >= Duration::from_millis(100)),
            "{seen:?}"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn destination_paths_beyond_max_path_work() {
        let (src, dest) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = src.path().join("report.pdf");
        fs::write(&root, b"far down").unwrap();
        let deep = dest
            .path()
            .join("a".repeat(100))
            .join("b".repeat(100))
            .join("c".repeat(100));
        fs::create_dir_all(&deep).unwrap();
        let target = deep.join("report.pdf");
        assert!(target.as_os_str().len() > 260);

        // OsFreeSpace and the rename are the raw Win32 calls; the folder
        // item also gets hidden and shown again.
        let (sent, received) =
            transfer(&root, Dest::InDir(deep.clone()), Arc::new(OsFreeSpace)).await;
        assert_eq!((sent, received), (Ok(()), Ok(target.clone())));
        assert_eq!(fs::read(&target).unwrap(), b"far down");
        assert!(!is_hidden(&target));
        let album = album(src.path());
        let (sent, received) =
            transfer(&album, Dest::InDir(deep.clone()), Arc::new(OsFreeSpace)).await;
        assert_eq!((sent, received), (Ok(()), Ok(deep.join("Album"))));
        assert!(!is_hidden(&deep.join("Album")));
        assert_eq!(names(&deep), ["Album", "report.pdf"]);
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_paths() {
        let v = |p: &str| verbatim(Path::new(p)).unwrap();
        assert_eq!(v(r"C:\a\..\b/c"), Path::new(r"\\?\C:\b\c"));
        assert_eq!(v(r"\\server\share\x"), Path::new(r"\\?\UNC\server\share\x"));
        assert_eq!(v(r"\\?\D:\y"), Path::new(r"\\?\D:\y"));
        assert!(v("rel").is_absolute());
    }

    #[test]
    fn fnv1a_is_the_standard_one() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn part_names_fit_a_file_name() {
        assert_eq!(part_name("report.pdf"), ".report.pdf.cjpart");
        let fits = |s: &str| s.len() <= 255 && s.encode_utf16().count() <= 255;
        let (a, b) = (
            format!("{}a.mp4", "東".repeat(80)),
            format!("{}b.mp4", "東".repeat(80)),
        );
        let (pa, pb) = (part_name(&a), part_name(&b));
        for p in [&pa, &pb] {
            assert!(fits(p), "{} bytes", p.len());
            assert!(p.starts_with(".東") && p.ends_with(".cjpart"), "{p}");
        }
        assert_ne!(pa, pb, "names differing only at the end");
        assert!(fits(&part_name(&"x".repeat(255))));
    }

    #[test]
    fn disk_errors_map_to_reasons() {
        use io::ErrorKind::*;
        for (kind, want) in [
            (PermissionDenied, FailReason::Unwritable),
            (ReadOnlyFilesystem, FailReason::Unwritable),
            (NotFound, FailReason::Unwritable),
            (StorageFull, FailReason::NoSpace),
            (QuotaExceeded, FailReason::NoSpace),
        ] {
            assert_eq!(disk_fail(&io::Error::from(kind)), want, "{kind:?}");
        }
    }

    #[test]
    fn os_free_space_reads_a_real_disk() {
        let dir = tempfile::tempdir().unwrap();
        assert!(OsFreeSpace.free_bytes(dir.path()).unwrap() > 0);
        assert!(OsFreeSpace.free_bytes(&dir.path().join("missing")).is_err());
    }
}
