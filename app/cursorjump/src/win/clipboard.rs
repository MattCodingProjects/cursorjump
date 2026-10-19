//! Windows clipboard: read, write and change notification.
//!
//! The watcher runs on its own thread with a hidden window registered through
//! `AddClipboardFormatListener`. Reads and writes open the clipboard through a
//! short-lived hidden helper window, because `OpenClipboard(NULL)` makes
//! `SetClipboardData` fail after `EmptyClipboard`.
use std::cell::RefCell;
use std::ffi::c_void;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cj_core::clipboard::{ClipboardData, cf_html_extract, cf_html_wrap, dib_to_png, png_to_dib};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use tracing::{debug, warn};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PostQuitMessage, RegisterClassExW, WINDOW_EX_STYLE, WM_CLIPBOARDUPDATE, WNDCLASSEXW,
    WS_POPUP,
};
use windows::core::{PCWSTR, w};

// Standard clipboard formats (winuser.h).
const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;

/// `OpenClipboard` is retried when another app holds it.
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY_STEP: Duration = Duration::from_millis(10);

/// Registered (non-standard) clipboard format ids.
struct Formats {
    html: u32,
    rtf: u32,
    png: u32,
}

fn formats() -> Formats {
    // SAFETY: the names are valid NUL-terminated wide strings.
    unsafe {
        Formats {
            html: RegisterClipboardFormatW(w!("HTML Format")),
            rtf: RegisterClipboardFormatW(w!("Rich Text Format")),
            png: RegisterClipboardFormatW(w!("PNG")),
        }
    }
}

/// An open clipboard. Closes it, then destroys the helper window, on drop.
struct ClipboardGuard {
    hwnd: HWND,
}

impl ClipboardGuard {
    fn open() -> Result<Self> {
        // SAFETY: "STATIC" is a system window class; the window is never shown.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("CursorJump clipboard"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .context("create clipboard helper window")?;
        let mut last = None;
        for attempt in 0..OPEN_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(OPEN_RETRY_STEP);
            }
            // SAFETY: hwnd is a live window owned by this thread.
            match unsafe { OpenClipboard(Some(hwnd)) } {
                Ok(()) => return Ok(Self { hwnd }),
                Err(e) => last = Some(e),
            }
        }
        // SAFETY: hwnd was created above and is destroyed exactly once.
        let _ = unsafe { DestroyWindow(hwnd) };
        Err(anyhow!(last.expect("at least one attempt")).context("OpenClipboard"))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: the clipboard is open (guard invariant); hwnd is destroyed once.
        unsafe {
            let _ = CloseClipboard();
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Owns a movable global allocation until `SetClipboardData` takes it.
struct GlobalBuf(Option<HGLOBAL>);

impl GlobalBuf {
    fn with_bytes(bytes: &[u8]) -> Result<Self> {
        // SAFETY: allocate, lock, copy `len` bytes into the `len`-byte block, unlock.
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).context("GlobalAlloc")?;
            let buf = Self(Some(h));
            let p = GlobalLock(h);
            if p.is_null() {
                return Err(anyhow!(windows::core::Error::from_thread()).context("GlobalLock"));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(h);
            Ok(buf)
        }
    }

    /// Give the memory to the clipboard. On failure it is freed here.
    fn set_as(mut self, format: u32) -> Result<()> {
        let h = self.0.expect("not yet handed over");
        // SAFETY: the clipboard is open; h is a movable block.
        unsafe { SetClipboardData(format, Some(HANDLE(h.0))) }.context("SetClipboardData")?;
        self.0 = None; // owned by the system now
        Ok(())
    }
}

impl Drop for GlobalBuf {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // SAFETY: the block was never handed to the clipboard.
            let _ = unsafe { GlobalFree(Some(h)) };
        }
    }
}

/// Copy of the bytes behind a clipboard format, or `None` if absent/unreadable.
fn get_bytes(format: u32) -> Option<Vec<u8>> {
    // SAFETY: the clipboard is open; the handle stays owned by the clipboard and
    // is only read while locked, within `GlobalSize` bytes.
    unsafe {
        if IsClipboardFormatAvailable(format).is_err() {
            return None;
        }
        let h = GetClipboardData(format).ok()?;
        let hg = HGLOBAL(h.0);
        let size = GlobalSize(hg);
        let p: *mut c_void = GlobalLock(hg);
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(hg);
        Some(v)
    }
}

fn trim_nul(mut b: Vec<u8>) -> Vec<u8> {
    if let Some(i) = b.iter().position(|&c| c == 0) {
        b.truncate(i);
    }
    b
}

fn read_text() -> Option<String> {
    let b = get_bytes(CF_UNICODETEXT)?;
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units)).filter(|s| !s.is_empty())
}

/// Image bytes as found on the clipboard; conversion happens after it is closed.
enum RawImage {
    Png(Vec<u8>),
    Dib(u32, Vec<u8>),
}

fn read_raw_image(f: &Formats) -> Option<RawImage> {
    if let Some(png) = get_bytes(f.png)
        .map(trim_nul_tail)
        .filter(|b| !b.is_empty())
    {
        return Some(RawImage::Png(png));
    }
    [CF_DIBV5, CF_DIB]
        .into_iter()
        .find_map(|fmt| get_bytes(fmt).map(|dib| RawImage::Dib(fmt, dib)))
}

fn finish_image(raw: RawImage) -> Option<Vec<u8>> {
    match raw {
        RawImage::Png(png) => Some(png),
        RawImage::Dib(fmt, dib) => match dib_to_png(&dib) {
            Ok(png) => Some(png),
            Err(e) => {
                debug!("clipboard: format {fmt} not convertible to PNG: {e:#}");
                None
            }
        },
    }
}

/// PNG data is binary: only strip trailing NULs some writers append past the IEND chunk.
fn trim_nul_tail(mut b: Vec<u8>) -> Vec<u8> {
    while b.last() == Some(&0) {
        b.pop();
    }
    b
}

/// Read the current clipboard. Formats that are absent are `None`; `CF_HDROP` is ignored.
pub fn read_clipboard() -> Result<ClipboardData> {
    // Under the guard only copy bytes; the (slow) image conversion runs after it is dropped.
    let (mut data, raw_image) = {
        let _guard = ClipboardGuard::open()?;
        let f = formats();
        let data = ClipboardData {
            text: read_text(),
            html: get_bytes(f.html)
                .map(|b| String::from_utf8_lossy(&trim_nul(b)).into_owned())
                .and_then(|cf| cf_html_extract(&cf))
                .filter(|h| !h.is_empty()),
            rtf: get_bytes(f.rtf).map(trim_nul).filter(|b| !b.is_empty()),
            png: None,
        };
        (data, read_raw_image(&f))
    };
    data.png = raw_image.and_then(finish_image);
    Ok(data)
}

/// Replace the clipboard with every format present in `d`.
#[allow(
    dead_code,
    reason = "used by the app wiring (Task 14); covered by tests"
)]
pub fn write_clipboard(d: &ClipboardData) -> Result<()> {
    // Convert before touching the clipboard so a bad image cannot leave it emptied.
    let dib = d
        .png
        .as_deref()
        .map(png_to_dib)
        .transpose()
        .context("convert PNG to DIB")?;
    let f = formats();
    let _guard = ClipboardGuard::open()?;
    // SAFETY: the clipboard is open.
    unsafe { EmptyClipboard() }.context("EmptyClipboard")?;
    if let Some(text) = &d.text {
        let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        bytes.extend_from_slice(&[0, 0]);
        GlobalBuf::with_bytes(&bytes)?.set_as(CF_UNICODETEXT)?;
    }
    if let Some(html) = &d.html {
        let mut bytes = cf_html_wrap(html).into_bytes();
        bytes.push(0);
        GlobalBuf::with_bytes(&bytes)?.set_as(f.html)?;
    }
    if let Some(rtf) = &d.rtf {
        let mut bytes = rtf.clone();
        bytes.push(0);
        GlobalBuf::with_bytes(&bytes)?.set_as(f.rtf)?;
    }
    if let (Some(png), Some(dib)) = (&d.png, &dib) {
        GlobalBuf::with_bytes(png)?.set_as(f.png)?;
        GlobalBuf::with_bytes(dib)?.set_as(CF_DIBV5)?;
    }
    Ok(())
}

// ------------------------------------------------------------------ watcher

thread_local! {
    static WATCH_TX: RefCell<Option<(Sender<ClipboardData>, Receiver<ClipboardData>)>> =
        const { RefCell::new(None) };
}

/// Watch the clipboard on a dedicated thread. Every change that reads back
/// non-empty is sent; failed reads are logged and skipped. The thread exits
/// when the receiver is dropped (noticed on the next change).
pub fn spawn_clipboard_watcher() -> Receiver<ClipboardData> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    // The watcher's own handle, used only to drop a stale item when the slot is full.
    let keep_rx = rx.clone();
    let spawned = std::thread::Builder::new()
        .name("cj-clipboard".into())
        .spawn(move || {
            if let Err(e) = run_watcher(tx, keep_rx) {
                warn!("clipboard watcher stopped: {e:#}");
            }
        });
    if let Err(e) = spawned {
        warn!("clipboard watcher thread failed to start: {e}");
    }
    rx
}

fn run_watcher(tx: Sender<ClipboardData>, rx: Receiver<ClipboardData>) -> Result<()> {
    WATCH_TX.with(|c| *c.borrow_mut() = Some((tx, rx)));
    let hwnd = create_watcher_window().context("create clipboard listener window")?;
    // SAFETY: hwnd is a live window of this thread.
    unsafe { AddClipboardFormatListener(hwnd) }.context("AddClipboardFormatListener")?;
    let mut msg = MSG::default();
    // SAFETY: standard message loop on the thread that owns the window.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe { DispatchMessageW(&msg) };
    }
    // SAFETY: undo the registration, then destroy our own window.
    unsafe {
        let _ = RemoveClipboardFormatListener(hwnd);
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}

fn create_watcher_window() -> windows::core::Result<HWND> {
    // SAFETY: GetModuleHandleW(None) returns this executable's handle.
    let hinst = unsafe { GetModuleHandleW(None) }?.into();
    let class = w!("CursorJumpClipboard");
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(watcher_proc),
        hInstance: hinst,
        lpszClassName: PCWSTR(class.0),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        let e = windows::core::Error::from_thread();
        if e.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
            return Err(e);
        }
    }
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("CursorJump clipboard watcher"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst),
            None,
        )
    }
}

/// Keep-latest send into a one-slot channel: a full slot holds a stale item, so
/// drop it and send the new one. Returns true when the channel is disconnected.
fn send_latest(tx: &Sender<ClipboardData>, rx: &Receiver<ClipboardData>, d: ClipboardData) -> bool {
    match tx.try_send(d) {
        Ok(()) => false,
        Err(TrySendError::Full(d)) => {
            let _ = rx.try_recv();
            matches!(tx.try_send(d), Err(TrySendError::Disconnected(_)))
        }
        Err(TrySendError::Disconnected(_)) => true,
    }
}

unsafe extern "system" fn watcher_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        match read_clipboard() {
            Ok(d) if d.is_empty() => {}
            Ok(d) => {
                let closed = WATCH_TX.with(|c| {
                    c.borrow()
                        .as_ref()
                        .is_some_and(|(tx, rx)| send_latest(tx, rx, d))
                });
                if closed {
                    // SAFETY: ends this thread's message loop.
                    unsafe { PostQuitMessage(0) };
                }
            }
            Err(e) => warn!("clipboard read failed: {e:#}"),
        }
        return LRESULT(0);
    }
    // SAFETY: default handling for every other message.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_roundtrip_text_html() {
        // Keep the user's clipboard: restore it best-effort at the end.
        let original = read_clipboard().ok();
        let want = ClipboardData {
            text: Some("héllo ✓ 😀 漢字".into()),
            html: Some("<i>héllo 😀 漢字</i>".into()),
            ..Default::default()
        };
        let result = write_clipboard(&want).and_then(|()| read_clipboard());
        if let Some(orig) = original.filter(|o| !o.is_empty()) {
            let _ = write_clipboard(&orig);
        }
        let got = result.expect("roundtrip");
        assert_eq!(got.text, want.text);
        assert_eq!(got.html, want.html);
    }

    /// A 2x2 bottom-up 32bpp BITMAPINFOHEADER DIB, converted to PNG.
    fn tiny_png() -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&[0u8; 24]);
        for px in [
            [0, 0, 255, 255],
            [0, 255, 0, 255],
            [255, 0, 0, 255],
            [9, 9, 9, 255],
        ] {
            dib.extend_from_slice(&px);
        }
        dib_to_png(&dib).expect("tiny dib")
    }

    #[test]
    fn clipboard_roundtrip_image() {
        let original = read_clipboard().ok();
        let want = ClipboardData {
            png: Some(tiny_png()),
            ..Default::default()
        };
        let result = write_clipboard(&want).and_then(|()| read_clipboard());
        let dib = {
            let _g = ClipboardGuard::open().expect("open");
            get_bytes(CF_DIBV5)
        };
        if let Some(orig) = original.filter(|o| !o.is_empty()) {
            let _ = write_clipboard(&orig);
        }
        assert_eq!(result.expect("roundtrip").png, want.png);
        let dib = dib.expect("CF_DIBV5 present");
        assert!(dib_to_png(&dib).is_ok());
    }
}
