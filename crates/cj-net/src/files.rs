//! Files streams: one unidirectional stream per dragged item.
//!
//! After its tag byte a files stream carries a frame holding the item's
//! `FilesHeader`, then a frame per `FileEntry`, each `File` entry followed by
//! exactly `size` raw bytes, and then the stream is finished. A frame is a
//! u32 little-endian length and that many bytes of postcard. The receiver
//! holds every entry to the header's promises and stops the stream
//! (`STOP_INVALID`) on the first one broken; the session stays up. The
//! first entry is the item's root (path `[]`, of the header's kind), and
//! only the first: a `File` item is that one entry, a `Dir` item its root
//! folder and then what is inside.

use anyhow::{Result, anyhow, bail};
use cj_core::dnd::{FileEntry, FilesHeader, ItemKind, MAX_PATH_DEPTH, safe_rel_path};
use quinn::{
    ReadError, ReadExactError, ReadToEndError, RecvStream, SendStream, VarInt, WriteError,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// First byte of every unidirectional stream: what it carries.
pub(crate) const STREAM_TAG_CLIPBOARD: u8 = 0;
pub(crate) const STREAM_TAG_FILES: u8 = 1;
/// Files streams yield to the control stream and clipboard items.
pub(crate) const FILES_PRIORITY: i32 = -100;
/// Largest header or entry frame body.
pub const MAX_FRAME: usize = 64 * 1024;
/// Stream stop / reset code: the transfer was cancelled.
pub const STOP_CANCELLED: u32 = 2;
/// Stream stop code: the peer broke the stream protocol (an unknown tag, a
/// bad frame, or an entry breaking its header's promises).
pub const STOP_INVALID: u32 = 3;

/// The code the peer stopped or reset a stream with, if that is what `e`
/// is: on the sending side, the receiver stopped it (`STOP_CANCELLED`,
/// `STOP_INVALID`, or 1 for a clipboard item over the receiver's limit); on
/// the receiving side, the sender reset it (`STOP_CANCELLED`). None for
/// anything else, such as a lost connection or a stream refused here.
pub fn stop_code(e: &anyhow::Error) -> Option<u32> {
    let code = e.chain().find_map(|cause| {
        if let Some(WriteError::Stopped(c)) = cause.downcast_ref() {
            return Some(*c);
        }
        let read = match cause.downcast_ref() {
            Some(ReadExactError::ReadError(r)) => Some(r),
            _ => match cause.downcast_ref() {
                Some(ReadToEndError::Read(r)) => Some(r),
                _ => cause.downcast_ref::<ReadError>(),
            },
        };
        match read {
            Some(ReadError::Reset(c)) => Some(*c),
            _ => None,
        }
    })?;
    u32::try_from(code.into_inner()).ok()
}

fn frame(v: &impl Serialize) -> Result<Vec<u8>> {
    let body = postcard::to_allocvec(v)?;
    if body.len() > MAX_FRAME {
        bail!(
            "frame of {} bytes exceeds the {MAX_FRAME}-byte limit",
            body.len()
        );
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Reads one frame; None if the stream finished cleanly where it would begin.
async fn read_frame<T: DeserializeOwned>(rx: &mut RecvStream) -> Result<Option<T>> {
    let mut prefix = [0u8; 4];
    match rx.read_exact(&mut prefix).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(prefix) as usize;
    if len > MAX_FRAME {
        bail!("frame of {len} bytes exceeds the {MAX_FRAME}-byte limit");
    }
    let mut body = vec![0u8; len];
    rx.read_exact(&mut body).await?;
    Ok(Some(postcard::from_bytes(&body)?))
}

/// The sending half of a files stream, from `Session::open_files`. It
/// keeps entries whole (each `File` entry gets exactly its `size` bytes);
/// the header's other promises are the receiver's to check. No method is
/// cancel-safe. Dropping it unfinished cancels the stream.
#[derive(Debug)]
pub struct FilesOut {
    tx: SendStream,
    /// Bytes of the current `File` entry not yet written.
    left: u64,
    /// Finished or reset.
    done: bool,
}

impl FilesOut {
    /// Writes the tag and header, the first thing on the stream.
    pub(crate) async fn start(mut tx: SendStream, h: &FilesHeader) -> Result<FilesOut> {
        let mut first = vec![STREAM_TAG_FILES];
        first.extend(frame(h)?);
        tx.write_all(&first).await?;
        Ok(FilesOut {
            tx,
            left: 0,
            done: false,
        })
    }

    /// Starts the next entry; a `File` entry's bytes follow with `write`.
    pub async fn entry(&mut self, e: &FileEntry) -> Result<()> {
        if self.left > 0 {
            bail!("{} bytes of the previous entry not yet written", self.left);
        }
        self.tx.write_all(&frame(e)?).await?;
        self.left = match e.kind {
            ItemKind::File => e.size,
            ItemKind::Dir => 0,
        };
        Ok(())
    }

    /// Writes bytes of the current `File` entry.
    pub async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        if chunk.len() as u64 > self.left {
            bail!(
                "{} bytes written to an entry with {} left",
                chunk.len(),
                self.left
            );
        }
        self.tx.write_all(chunk).await?;
        self.left -= chunk.len() as u64;
        Ok(())
    }

    /// Ends the item. Fails, cancelling the stream, if the current entry's
    /// bytes are not all written.
    pub fn finish(mut self) -> Result<()> {
        if self.left > 0 {
            bail!(
                "finished with {} bytes of the last entry unwritten",
                self.left
            );
        }
        self.tx.finish()?;
        self.done = true;
        Ok(())
    }

    /// Abandons the item: the receiver's reads fail.
    pub fn cancel(mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        if !self.done {
            let _ = self.tx.reset(VarInt::from_u32(STOP_CANCELLED));
            self.done = true;
        }
    }
}

impl Drop for FilesOut {
    fn drop(&mut self) {
        self.reset();
    }
}

/// The receiving half of a files stream, from `Session::accept_stream`.
/// It checks what the peer sends but never touches the disk: entry paths are
/// still to be sanitized (`dnd::safe_rel_path`) by whoever writes them.
/// Any error from the peer's side ends the stream. Dropping it before the
/// item is complete cancels the stream.
#[derive(Debug)]
pub struct FilesIn {
    rx: RecvStream,
    header: FilesHeader,
    /// Bytes of the current `File` entry not yet read.
    left: u64,
    /// Entries read so far.
    entries: u64,
    /// Sum of the sizes of the entries read so far.
    bytes: u64,
    /// The stream finished where the header said it would.
    ended: bool,
    /// Stopped by us: refused or cancelled.
    stopped: bool,
}

impl FilesIn {
    /// Reads the header, which follows the tag.
    pub(crate) async fn accept(mut rx: RecvStream) -> Result<FilesIn> {
        let header = match read_frame::<FilesHeader>(&mut rx).await {
            Ok(Some(h)) => h,
            Ok(None) => bail!("files stream ended before its header"),
            Err(e) => {
                let _ = rx.stop(VarInt::from_u32(STOP_INVALID));
                return Err(e.context("files stream header"));
            }
        };
        let fits = match header.kind {
            ItemKind::File => header.entries == 1,
            ItemKind::Dir => header.entries >= 1,
        };
        if !fits {
            let _ = rx.stop(VarInt::from_u32(STOP_INVALID));
            bail!(
                "a {:?} item cannot have {} entries",
                header.kind,
                header.entries
            );
        }
        Ok(FilesIn {
            rx,
            header,
            left: 0,
            entries: 0,
            bytes: 0,
            ended: false,
            stopped: false,
        })
    }

    pub fn header(&self) -> &FilesHeader {
        &self.header
    }

    /// The next entry; None once the item is complete. Fails if the current
    /// entry's bytes are not all read (that alone changes nothing), or if
    /// the peer breaks the header's promises: more entries or bytes than
    /// announced, a root that is missing, out of place or of the wrong kind,
    /// a `Dir` with a size, an unsafe path, or a stream that ends short. Not
    /// cancel-safe.
    pub async fn next_entry(&mut self) -> Result<Option<FileEntry>> {
        if self.left > 0 {
            bail!("{} bytes of the current entry not yet read", self.left);
        }
        if self.ended {
            return Ok(None);
        }
        if self.stopped {
            bail!("files stream already refused");
        }
        let got = self.read_entry().await;
        if got.is_err() {
            self.refuse();
        }
        got
    }

    async fn read_entry(&mut self) -> Result<Option<FileEntry>> {
        let h = &self.header;
        let Some(e) = read_frame::<FileEntry>(&mut self.rx).await? else {
            if self.entries != h.entries || self.bytes != h.total_bytes {
                bail!(
                    "files stream ended after {} of {} entries and {} of {} bytes",
                    self.entries,
                    h.entries,
                    self.bytes,
                    h.total_bytes
                );
            }
            self.ended = true;
            return Ok(None);
        };
        if self.entries >= h.entries {
            bail!("more than the {} entries announced", h.entries);
        }
        let first = self.entries == 0;
        if e.path.is_empty() != first {
            bail!("the root must be the first entry and only the first");
        }
        if first && e.kind != h.kind {
            bail!("a {:?} item with a {:?} root", h.kind, e.kind);
        }
        if e.kind == ItemKind::Dir && e.size != 0 {
            bail!("a folder entry claims {} bytes", e.size);
        }
        if safe_rel_path(&e.path).is_none() {
            bail!(
                "entry path of {} components is deeper than {MAX_PATH_DEPTH} \
                 or has an empty, . or .. component",
                e.path.len()
            );
        }
        let bytes = self
            .bytes
            .checked_add(e.size)
            .filter(|b| *b <= h.total_bytes)
            .ok_or_else(|| anyhow!("entries exceed the {} bytes announced", h.total_bytes))?;
        self.entries += 1;
        self.bytes = bytes;
        self.left = e.size;
        Ok(Some(e))
    }

    /// Reads bytes of the current entry into `buf`; 0 at the end of the
    /// entry (or for an empty `buf`). Cancel-safe.
    pub async fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        let want = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        if want == 0 {
            return Ok(0);
        }
        let got = match self.rx.read(&mut buf[..want]).await {
            Ok(Some(n)) => Ok(n),
            Ok(None) => Err(anyhow!(
                "files stream ended with {} bytes of an entry unread",
                self.left
            )),
            Err(e) => Err(e.into()),
        };
        match got {
            Ok(n) => self.left -= n as u64,
            Err(_) => self.refuse(),
        }
        got
    }

    /// Abandons the item: the sender's writes fail.
    pub fn cancel(mut self) {
        self.stop(STOP_CANCELLED);
    }

    fn refuse(&mut self) {
        self.stop(STOP_INVALID);
    }

    fn stop(&mut self, code: u32) {
        if !self.ended && !self.stopped {
            let _ = self.rx.stop(VarInt::from_u32(code));
            self.stopped = true;
        }
    }
}

impl Drop for FilesIn {
    fn drop(&mut self) {
        self.stop(STOP_CANCELLED);
    }
}
