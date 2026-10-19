//! macOS clipboard: read, write and change polling on the general pasteboard.
//!
//! `NSPasteboard` is usable from any thread; reads and writes run inside their
//! own autorelease pool so callers need not provide one. File copies
//! (`public.file-url`) are ignored in phase 1.

use std::io::Cursor;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cj_core::clipboard::ClipboardData;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use image::{DynamicImage, ImageFormat};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypeHTML, NSPasteboardTypePNG,
    NSPasteboardTypeRTF, NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSArray, NSData, NSString};
use tracing::{debug, warn};

/// How often the watcher checks the pasteboard's `changeCount`.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The pasteboard types CursorJump reads or writes.
struct Types {
    /// `public.utf8-plain-text`
    string: &'static NSString,
    /// `public.html`
    html: &'static NSString,
    /// `public.rtf`
    rtf: &'static NSString,
    /// `public.png`
    png: &'static NSString,
    /// `public.tiff`
    tiff: &'static NSString,
    /// `public.file-url`
    file_url: &'static NSString,
}

fn types() -> Types {
    // SAFETY: AppKit's pasteboard type constants are immutable NSStrings that
    // live for the whole process.
    unsafe {
        Types {
            string: NSPasteboardTypeString,
            html: NSPasteboardTypeHTML,
            rtf: NSPasteboardTypeRTF,
            png: NSPasteboardTypePNG,
            tiff: NSPasteboardTypeTIFF,
            file_url: NSPasteboardTypeFileURL,
        }
    }
}

fn string_for(pb: &NSPasteboard, ty: &NSString) -> Option<String> {
    pb.stringForType(ty)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn data_for(pb: &NSPasteboard, ty: &NSString) -> Option<Vec<u8>> {
    pb.dataForType(ty)
        .map(|d| d.to_vec())
        .filter(|b| !b.is_empty())
}

/// Image bytes as found on the pasteboard; conversion happens afterwards.
enum RawImage {
    Png(Vec<u8>),
    Tiff(Vec<u8>),
}

fn finish_image(raw: RawImage) -> Option<Vec<u8>> {
    match raw {
        RawImage::Png(png) => Some(png),
        RawImage::Tiff(tiff) => match tiff_to_png(&tiff) {
            Ok(png) => Some(png),
            Err(e) => {
                debug!("clipboard: TIFF not convertible to PNG: {e:#}");
                None
            }
        },
    }
}

fn tiff_to_png(tiff: &[u8]) -> Result<Vec<u8>> {
    let img =
        image::load_from_memory_with_format(tiff, ImageFormat::Tiff).context("decode TIFF")?;
    // PNG has no floating-point samples.
    let img = match img {
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => {
            DynamicImage::ImageRgba8(img.to_rgba8())
        }
        other => other,
    };
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)
        .context("encode PNG")?;
    Ok(out.into_inner())
}

/// Serializes this process's pasteboard access. `clearContents` bumps
/// `changeCount` before a write's data is set, so the watcher must not check
/// the count or read while `write_clipboard` is part way through.
static PASTEBOARD: Mutex<()> = Mutex::new(());

fn lock_pasteboard() -> MutexGuard<'static, ()> {
    PASTEBOARD.lock().unwrap_or_else(|e| e.into_inner())
}

/// The pasteboard-access half of a read: copies bytes only. Call it with
/// `PASTEBOARD` held. File-URL pasteboards read as empty.
fn read_raw(pb: &NSPasteboard) -> (ClipboardData, Option<RawImage>) {
    let t = types();
    if pb
        .availableTypeFromArray(&NSArray::from_slice(&[t.file_url]))
        .is_some()
    {
        return (ClipboardData::default(), None);
    }
    let data = ClipboardData {
        text: string_for(pb, t.string),
        html: string_for(pb, t.html),
        rtf: data_for(pb, t.rtf),
        png: None,
    };
    let raw_image = data_for(pb, t.png)
        .map(RawImage::Png)
        .or_else(|| data_for(pb, t.tiff).map(RawImage::Tiff));
    (data, raw_image)
}

/// The slow half of a read, run without the lock: TIFF to PNG conversion.
fn finish_read((mut data, raw_image): (ClipboardData, Option<RawImage>)) -> ClipboardData {
    data.png = raw_image.and_then(finish_image);
    data
}

/// Read the general pasteboard. Absent types are `None`; a pasteboard holding
/// file URLs reads as empty, so a file's name or icon is never sent as content.
pub fn read_clipboard() -> Result<ClipboardData> {
    let raw = autoreleasepool(|_| {
        let _lock = lock_pasteboard();
        read_raw(&NSPasteboard::generalPasteboard())
    });
    Ok(finish_read(raw))
}

fn ensure_set(ok: bool, what: &str) -> Result<()> {
    if !ok {
        bail!("pasteboard rejected the {what}");
    }
    Ok(())
}

/// Replace the pasteboard with every format present in `d`. Images are
/// written as PNG only.
pub fn write_clipboard(d: &ClipboardData) -> Result<()> {
    autoreleasepool(|_| {
        let _lock = lock_pasteboard();
        let pb = NSPasteboard::generalPasteboard();
        let t = types();
        pb.clearContents();
        if let Some(text) = &d.text {
            ensure_set(
                pb.setString_forType(&NSString::from_str(text), t.string),
                "text",
            )?;
        }
        if let Some(html) = &d.html {
            ensure_set(
                pb.setString_forType(&NSString::from_str(html), t.html),
                "HTML",
            )?;
        }
        if let Some(rtf) = &d.rtf {
            ensure_set(
                pb.setData_forType(Some(&NSData::with_bytes(rtf)), t.rtf),
                "RTF",
            )?;
        }
        if let Some(png) = &d.png {
            ensure_set(
                pb.setData_forType(Some(&NSData::with_bytes(png)), t.png),
                "PNG",
            )?;
        }
        Ok(())
    })
}

// ------------------------------------------------------------------ watcher

/// Watch the pasteboard on a dedicated thread, polling `changeCount`. Every
/// change that reads back non-empty is sent. The thread exits only when a send
/// finds the receiver dropped: an idle watcher keeps polling after the
/// receiver is dropped, until the pasteboard's next non-empty change.
pub fn spawn_clipboard_watcher() -> Receiver<ClipboardData> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new()
        .name("cj-clipboard".into())
        .spawn(move || run_watcher(&tx));
    if let Err(e) = spawned {
        warn!("clipboard watcher thread failed to start: {e}");
    }
    rx
}

fn run_watcher(tx: &Sender<ClipboardData>) {
    let mut last = autoreleasepool(|_| {
        let _lock = lock_pasteboard();
        NSPasteboard::generalPasteboard().changeCount()
    });
    let mut pending = None;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let connected = autoreleasepool(|_| {
            // Check the count and copy the bytes under the lock, so a write in
            // progress is seen either not at all or complete.
            let raw = {
                let _lock = lock_pasteboard();
                let pb = NSPasteboard::generalPasteboard();
                let count = pb.changeCount();
                (count != last).then(|| {
                    last = count;
                    read_raw(&pb)
                })
            };
            if let Some(raw) = raw {
                // The newest contents replace anything not yet delivered;
                // empty contents leave nothing to send.
                pending = Some(finish_read(raw)).filter(|d| !d.is_empty());
            }
            deliver(tx, &mut pending)
        });
        if !connected {
            debug!("clipboard watcher: receiver dropped, stopping");
            return;
        }
    }
}

/// Offer the pending item to the one-slot channel. While the slot is full the
/// item stays pending, to be retried on the next poll or replaced by a newer
/// change, so the receiver always ends up with the latest contents. (The
/// watcher holds no receiver of its own to drop a stale item with: that would
/// keep the channel connected forever.) Returns false once the receiver is gone.
fn deliver(tx: &Sender<ClipboardData>, pending: &mut Option<ClipboardData>) -> bool {
    let Some(d) = pending.take() else {
        return true;
    };
    match tx.try_send(d) {
        Ok(()) => true,
        Err(TrySendError::Full(d)) => {
            *pending = Some(d);
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use image::{Rgba, RgbaImage};
    use objc2::rc::Retained;

    /// The pasteboard is global: tests that write it must not run in parallel.
    static CLIPBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Every type's raw bytes on the pasteboard, to restore exactly what the
    /// user had (including types `read_clipboard` ignores, like file URLs).
    /// Multi-item pasteboards come back as their first item.
    fn snapshot() -> Vec<(Retained<NSString>, Retained<NSData>)> {
        let _lock = lock_pasteboard();
        let pb = NSPasteboard::generalPasteboard();
        pb.types()
            .map(|types| {
                types
                    .iter()
                    .filter_map(|ty| pb.dataForType(&ty).map(|data| (ty, data)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn restore(saved: &[(Retained<NSString>, Retained<NSData>)]) {
        let _lock = lock_pasteboard();
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        for (ty, data) in saved {
            pb.setData_forType(Some(data), ty);
        }
    }

    /// Restores a snapshot when dropped, so a panicking test still puts the
    /// user's pasteboard back.
    struct RestoreOnDrop(Vec<(Retained<NSString>, Retained<NSData>)>);

    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            restore(&self.0);
        }
    }

    /// Runs `f` with the pasteboard to itself, restoring the user's contents
    /// best-effort afterwards, even if `f` panics.
    fn with_saved_clipboard<T>(f: impl FnOnce() -> T) -> T {
        let _serial = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
        // Declared after `_serial`, so it drops (restores) while the lock is held.
        let _restore = RestoreOnDrop(snapshot());
        f()
    }

    fn tiny_image() -> RgbaImage {
        let mut img = RgbaImage::new(2, 2);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, Rgba([0, 255, 0, 255]));
        img.put_pixel(0, 1, Rgba([0, 0, 255, 255]));
        img.put_pixel(1, 1, Rgba([9, 9, 9, 255]));
        img
    }

    fn encode(img: &RgbaImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, format).expect("encode");
        out.into_inner()
    }

    #[test]
    fn clipboard_roundtrip_text_html() {
        let want = ClipboardData {
            text: Some("héllo ✓ 😀 漢字".into()),
            html: Some("<i>héllo 😀 漢字</i>".into()),
            ..Default::default()
        };
        let got = with_saved_clipboard(|| write_clipboard(&want).and_then(|()| read_clipboard()));
        let got = got.expect("roundtrip");
        assert_eq!(got.text, want.text);
        assert_eq!(got.html, want.html);
    }

    #[test]
    fn clipboard_roundtrip_image() {
        let want = ClipboardData {
            png: Some(encode(&tiny_image(), ImageFormat::Png)),
            ..Default::default()
        };
        let got = with_saved_clipboard(|| write_clipboard(&want).and_then(|()| read_clipboard()));
        assert_eq!(got.expect("roundtrip").png, want.png);
    }

    #[test]
    fn tiff_only_pasteboard_reads_back_as_png() {
        let tiff = encode(&tiny_image(), ImageFormat::Tiff);
        let got = with_saved_clipboard(|| {
            {
                let _lock = lock_pasteboard();
                let pb = NSPasteboard::generalPasteboard();
                pb.clearContents();
                assert!(pb.setData_forType(Some(&NSData::with_bytes(&tiff)), types().tiff));
            }
            read_clipboard()
        });
        let png = got.expect("read").png.expect("PNG converted from TIFF");
        let decoded = image::load_from_memory_with_format(&png, ImageFormat::Png)
            .expect("valid PNG")
            .to_rgba8();
        assert_eq!(decoded, tiny_image());
    }

    #[test]
    fn file_url_pasteboard_reads_empty() {
        // A Finder copy carries the file URL, its name as text and an icon TIFF.
        let tiff = encode(&tiny_image(), ImageFormat::Tiff);
        let got = with_saved_clipboard(|| {
            {
                let _lock = lock_pasteboard();
                let pb = NSPasteboard::generalPasteboard();
                let t = types();
                pb.clearContents();
                pb.setString_forType(
                    &NSString::from_str("file:///tmp/cursorjump-test.txt"),
                    t.file_url,
                );
                pb.setString_forType(&NSString::from_str("cursorjump-test.txt"), t.string);
                pb.setData_forType(Some(&NSData::with_bytes(&tiff)), t.tiff);
            }
            read_clipboard()
        });
        assert_eq!(got.expect("read"), ClipboardData::default());
    }

    #[test]
    fn watcher_sends_each_change() {
        let want = ClipboardData {
            text: Some("cursorjump watcher test".into()),
            ..Default::default()
        };
        let got = with_saved_clipboard(|| {
            let rx = spawn_clipboard_watcher();
            // Let the watcher record the starting change count first.
            std::thread::sleep(POLL_INTERVAL * 2);
            write_clipboard(&want).expect("write");
            rx.recv_timeout(Duration::from_secs(3))
        });
        assert_eq!(got.expect("watcher sent the change"), want);
    }

    /// A watcher polling while writes are in flight must never see a write
    /// half done (`clearContents` bumps `changeCount` before the data is set).
    #[test]
    fn watcher_never_sees_a_partial_write() {
        let png = encode(&tiny_image(), ImageFormat::Png);
        let item = |n: u32| ClipboardData {
            text: Some(format!("cj-{n}")),
            html: Some(format!("<b>cj-{n}</b>")),
            rtf: Some(format!("{{\\rtf1 cj-{n}}}").into_bytes()),
            png: Some(png.clone()),
        };
        let received = with_saved_clipboard(|| {
            let rx = spawn_clipboard_watcher();
            std::thread::sleep(POLL_INTERVAL * 2);
            let mut received = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let mut n = 0;
            while std::time::Instant::now() < deadline {
                write_clipboard(&item(n)).expect("write");
                received.extend(rx.try_iter());
                n += 1;
            }
            received.extend(rx.recv_timeout(POLL_INTERVAL * 4));
            received
        });
        assert!(!received.is_empty(), "the watcher saw no change");
        for d in &received {
            let n: u32 = d
                .text
                .as_deref()
                .and_then(|t| t.strip_prefix("cj-"))
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("partial item (text): {:?}", d.text));
            assert_eq!(d, &item(n), "partial or mixed item");
        }
    }

    #[test]
    fn deliver_keeps_the_latest_until_the_slot_frees() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let item = |s: &str| ClipboardData {
            text: Some(s.into()),
            ..Default::default()
        };
        let mut pending = Some(item("a"));
        assert!(deliver(&tx, &mut pending));
        assert!(pending.is_none());
        pending = Some(item("b"));
        assert!(
            deliver(&tx, &mut pending),
            "a full slot is not a disconnect"
        );
        assert_eq!(pending, Some(item("b")));
        assert_eq!(rx.try_recv().ok(), Some(item("a")));
        assert!(deliver(&tx, &mut pending));
        assert_eq!(rx.try_recv().ok(), Some(item("b")));
        drop(rx);
        pending = Some(item("c"));
        assert!(!deliver(&tx, &mut pending), "receiver gone");
    }

    /// Manual check: `cargo test -p cursorjump watcher_logs_changes -- --ignored --nocapture`,
    /// then copy things (e.g. Cmd+Shift+Ctrl+4). Runs for `CJ_WATCH_SECS` (default 30) seconds.
    #[test]
    #[ignore = "manual: watches the real pasteboard"]
    fn watcher_logs_changes() {
        let secs = std::env::var("CJ_WATCH_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let rx = spawn_clipboard_watcher();
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            if let Ok(d) = rx.recv_timeout(left) {
                println!(
                    "clipboard: text={:?} html={:?} rtf={:?} png={:?} bytes",
                    d.text.as_ref().map(String::len),
                    d.html.as_ref().map(String::len),
                    d.rtf.as_ref().map(Vec::len),
                    d.png.as_ref().map(Vec::len),
                );
            }
        }
    }
}
