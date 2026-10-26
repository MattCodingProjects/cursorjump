//! The progress pills on the PC, for drags coming from the Mac.
//!
//! One thread ("cj-pill") owns the [`PillBoard`] and, per visible pill, two
//! of CursorJump's own topmost, non-activating popups (no hooks, nothing
//! drawn into other processes):
//! - a click-through layered window (`WS_EX_TRANSPARENT`), painted with
//!   `UpdateLayeredWindow` from the renderer's pixmap, shadow included;
//! - an input window over the body only, drawn at alpha 1, which takes the
//!   hover and the ✕ clicks while the pill is running or failed.
//!   `HTTRANSPARENT` only hands the mouse on to windows of the same thread,
//!   so the painted window can't hit-test its shadow away for the app below;
//!   this way the halo never takes a click, from any process.
//!
//! - A pill stays on the monitor its drop was on (the one under `at`). Each
//!   monitor stacks its own pills, bottom-most first in start order.
//! - Commands are relayed by "cj-pill-cmd", which wakes the thread with a
//!   thread message. The thread sleeps in `MsgWaitForMultipleObjectsEx`
//!   until then or the board's next frame, at most about 30 fps, and with
//!   no timeout at all when nothing is showing or due.
//! - While held (a game has the mouse) the board shows nothing, so every
//!   window is destroyed at once; they come back on release.
use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cj_core::dnd::{DragId, DragItem, FailReason};
use cj_core::layout::Rect;
use cj_core::pill::{Peer, PillBoard, PillView, SHOW_AFTER};
use tracing::{info, warn};
use windows::Win32::Foundation::{
    COLORREF, CloseHandle, ERROR_CLASS_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    POINT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, CreateRoundRectRgn, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetMonitorInfoW, GetStockObject, HBITMAP, HBRUSH, HDC, HGDIOBJ,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, SelectObject, SetWindowRgn,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThreadId, INFINITE,
    SetWaitableTimer, TIMER_ALL_ACCESS,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, IDC_ARROW, LWA_ALPHA,
    LoadCursorW, MA_NOACTIVATE, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_NOREMOVE,
    PM_REMOVE, PeekMessageW, PostThreadMessageW, QS_ALLINPUT, RegisterClassExW, SPI_SETWORKAREA,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow,
    WINDOW_EX_STYLE, WM_APP, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDOWN, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_QUIT, WM_SETTINGCHANGE, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use super::monitors::rect_from;
use crate::common::pill_render::{
    BOTTOM_MARGIN, Backdrop, PILL_GAP, close_rect, close_shown, pill_px, pixmap_px, render,
};

/// Least time between two redraws: about 30 fps.
const MIN_FRAME: Duration = Duration::from_millis(33);
/// Sent once the cursor leaves a window tracked with `TME_LEAVE` (winuser.h).
const WM_MOUSELEAVE: u32 = 0x02A3;
const CLASS: PCWSTR = w!("CursorJumpPill");

/// App → pill thread.
#[derive(Debug, Clone)]
pub enum PillCmd {
    /// A drag from the Mac was dropped at `at` (screen px), which picks the monitor.
    Start {
        drag: DragId,
        items: Vec<DragItem>,
        at: (i32, i32),
    },
    Progress {
        drag: DragId,
        done: u64,
        total: Option<u64>,
    },
    Finish {
        drag: DragId,
        result: Result<(), FailReason>,
    },
    /// Whether a game holds the mouse: while it does, no pill shows.
    Held(bool),
}

/// Start the pill thread. Dropping every `Sender` stops the thread and
/// closes its windows.
///
/// `on_cancel` runs on the pill thread, inside its window procedure, when a
/// running pill's ✕ is clicked, and `on_dismiss` when a failed pill's ✕
/// takes it off the board. Neither may block (the pills freeze until it
/// returns) nor hold a clone of the returned `Sender` (the thread would
/// then never stop). A panic in either is caught and logged.
pub fn spawn_pill_thread(
    on_cancel: impl Fn(DragId) + Send + 'static,
    on_dismiss: impl Fn(DragId) + Send + 'static,
) -> Result<mpsc::Sender<PillCmd>> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<PillCmd>();
    let (relay_tx, relay_rx) = mpsc::channel::<PillCmd>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<u32>>(1);
    // Set while a wake is posted and not yet seen, so a burst of progress
    // posts one message, not one each.
    let pending = Arc::new(AtomicBool::new(false));
    let thread_pending = pending.clone();
    std::thread::Builder::new()
        .name("cj-pill".into())
        .spawn(move || {
            let on = Callbacks {
                cancel: Box::new(on_cancel),
                dismiss: Box::new(on_dismiss),
            };
            run(on, relay_rx, &thread_pending, ready_tx)
        })
        .context("spawn pill thread")?;
    // The thread reports its id once its message queue exists.
    let tid = ready_rx.recv().context("pill thread exited at start")??;
    std::thread::Builder::new()
        .name("cj-pill-cmd".into())
        .spawn(move || relay(cmd_rx, relay_tx, &pending, tid))
        .inspect_err(|_| {
            // The relay's sender is gone: let the thread see it and stop.
            wake(tid);
        })
        .context("spawn pill command relay")?;
    Ok(cmd_tx)
}

/// Forwards commands and wakes the pill thread; once every app sender is
/// gone, drops its own so the thread stops.
fn relay(from: mpsc::Receiver<PillCmd>, to: mpsc::Sender<PillCmd>, pending: &AtomicBool, tid: u32) {
    for cmd in from {
        if to.send(cmd).is_err() {
            return; // The pill thread is gone.
        }
        // SeqCst against the thread's swap: either it sees this command in its
        // drain, or this swap sees its `false` and posts a wake.
        if !pending.swap(true, Ordering::SeqCst) && !wake(tid) {
            // The next command (or frame) picks this one up.
            pending.store(false, Ordering::SeqCst);
        }
    }
    drop(to);
    wake(tid);
}

fn wake(tid: u32) -> bool {
    // SAFETY: plain FFI call; a thread that is gone just makes it fail.
    match unsafe { PostThreadMessageW(tid, WM_APP, WPARAM(0), LPARAM(0)) } {
        Ok(()) => true,
        Err(e) => {
            warn!("pill thread wake failed: {e}");
            false
        }
    }
}

/// Where a drag's pill goes: the drop point and when it was heard of.
struct Anchor {
    drag: DragId,
    at: (i32, i32),
    since: Instant,
}

/// The monitor a pill is stacked on.
struct Place {
    monitor: isize,
    work: Rect,
    scale: f32,
}

/// What a pill's windows show now.
#[derive(Clone, PartialEq)]
struct Shown {
    view: PillView,
    scale: f32,
    hover: bool,
    origin: (i32, i32),
}

/// A 32-bit top-down DIB section selected into its own memory DC.
struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: u32,
    h: u32,
}

impl Dib {
    fn new(w: u32, h: u32) -> Result<Dib> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                // Negative: rows run top-down, as in the pixmap.
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: bmi describes a w x h 32 bpp DIB; `bits` receives its pixels.
        let bitmap = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
            .context("CreateDIBSection")?;
        // SAFETY: a memory DC compatible with the screen.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            // SAFETY: the bitmap was created above and is not selected anywhere.
            let _ = unsafe { DeleteObject(bitmap.into()) };
            return Err(anyhow!("CreateCompatibleDC failed"));
        }
        // SAFETY: both are live and ours; `Drop` selects `old` back.
        let old = unsafe { SelectObject(dc, bitmap.into()) };
        if old.is_invalid() {
            // SAFETY: both were created above; the bitmap is not selected.
            unsafe {
                let _ = DeleteDC(dc);
                let _ = DeleteObject(bitmap.into());
            }
            return Err(anyhow!("SelectObject failed"));
        }
        Ok(Dib {
            dc,
            bitmap,
            old,
            bits: bits.cast(),
            w,
            h,
        })
    }

    fn pixels(&mut self) -> &mut [u8] {
        // SAFETY: the section holds w * h 4-byte pixels and lives as long as self.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.w as usize * self.h as usize * 4) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: undo `new` in reverse; each handle is released once.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.bitmap.into());
        }
    }
}

/// One pill's windows. Dropping it destroys them.
struct PillWin {
    drag: DragId,
    /// Painted, click-through.
    paint: HWND,
    /// Over the body; shown while the pill takes clicks.
    input: HWND,
    /// Reused while the size holds.
    dib: Option<Dib>,
    hover: bool,
    shown: Option<Shown>,
    /// Where the input window is, or `None` while it is hidden.
    input_at: Option<(i32, i32, i32, i32)>,
    /// The size the input window's capsule region was made for.
    region: Option<(i32, i32)>,
    /// A failed draw is logged once per pill.
    warned: bool,
}

impl PillWin {
    fn new(hinst: HINSTANCE, drag: DragId) -> Result<PillWin> {
        let ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let paint = create_window(hinst, ex | WS_EX_TRANSPARENT).context("create pill window")?;
        let input = match create_window(hinst, ex) {
            Ok(h) => h,
            Err(e) => {
                // SAFETY: our own window, destroyed once.
                let _ = unsafe { DestroyWindow(paint) };
                return Err(anyhow!(e).context("create pill input window"));
            }
        };
        let win = PillWin {
            drag,
            paint,
            input,
            dib: None,
            hover: false,
            shown: None,
            input_at: None,
            region: None,
            warned: false,
        };
        // Alpha 1: never seen, but it takes the mouse over the body. On
        // failure, dropping `win` destroys both windows.
        // SAFETY: our own layered window.
        unsafe { SetLayeredWindowAttributes(win.input, COLORREF(0), 1, LWA_ALPHA) }
            .context("SetLayeredWindowAttributes")?;
        Ok(win)
    }

    /// Bring the windows up to `view`, at `origin` on a monitor at `scale`.
    fn show(&mut self, view: &PillView, scale: f32, origin: (i32, i32)) -> Result<()> {
        let want = Shown {
            view: view.clone(),
            scale,
            hover: self.hover,
            origin,
        };
        if self.shown.as_ref() == Some(&want) {
            return Ok(());
        }
        let first = self.shown.is_none();
        let pm = render(view, scale, Backdrop::Opaque, self.hover);
        let (w, h) = (pm.width(), pm.height());
        if !self.dib.as_ref().is_some_and(|d| (d.w, d.h) == (w, h)) {
            self.dib = None;
            self.dib = Some(Dib::new(w, h)?);
        }
        let dib = self.dib.as_mut().expect("just made");
        to_bgra(pm.data(), dib.pixels());
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let (dst, size, src) = (
            POINT {
                x: origin.0,
                y: origin.1,
            },
            SIZE {
                cx: w as i32,
                cy: h as i32,
            },
            POINT::default(),
        );
        // SAFETY: our own layered window; the DC holds a w x h premultiplied BGRA DIB.
        unsafe {
            UpdateLayeredWindow(
                self.paint,
                None,
                Some(&dst),
                Some(&size),
                Some(dib.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        }
        .context("UpdateLayeredWindow")?;
        if first {
            // SAFETY: shows our own window without activating it.
            let _ = unsafe { ShowWindow(self.paint, SW_SHOWNOACTIVATE) };
        }
        let input_at = (view.running || view.failed).then(|| body_rect(origin, scale));
        if input_at != self.input_at {
            match input_at {
                Some((x, y, w, h)) => {
                    // Clipped before it is shown, so it never takes a corner click.
                    if self.region != Some((w, h)) {
                        set_capsule_region(self.input, w, h)?;
                        self.region = Some((w, h));
                    }
                    // SAFETY: moves and shows our own window without activating it.
                    unsafe {
                        SetWindowPos(
                            self.input,
                            None,
                            x,
                            y,
                            w,
                            h,
                            SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
                        )
                    }
                    .context("SetWindowPos")?;
                }
                None => {
                    // SAFETY: hides our own window.
                    let _ = unsafe { ShowWindow(self.input, SW_HIDE) };
                    self.hover = false;
                }
            }
            self.input_at = input_at;
        }
        self.shown = Some(want);
        Ok(())
    }
}

impl Drop for PillWin {
    fn drop(&mut self) {
        // SAFETY: our own windows, each destroyed once.
        unsafe {
            let _ = DestroyWindow(self.input);
            let _ = DestroyWindow(self.paint);
        }
    }
}

/// Clip a `w` x `h` input window to the pill's capsule (corner radius h / 2),
/// so the corners outside the outline take no clicks.
fn set_capsule_region(hwnd: HWND, w: i32, h: i32) -> Result<()> {
    // SAFETY: a new region, which the system owns once SetWindowRgn takes it.
    unsafe {
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, h, h);
        if rgn.is_invalid() {
            return Err(anyhow!("CreateRoundRectRgn failed"));
        }
        if SetWindowRgn(hwnd, Some(rgn), true) == 0 {
            let _ = DeleteObject(rgn.into());
            return Err(anyhow!("SetWindowRgn failed"));
        }
    }
    Ok(())
}

fn create_window(hinst: HINSTANCE, ex: WINDOW_EX_STYLE) -> windows::core::Result<HWND> {
    // SAFETY: the class is registered by `run`; the window starts hidden.
    unsafe {
        CreateWindowExW(
            ex,
            CLASS,
            w!("CursorJump pill"),
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
    }
}

fn register_class() -> windows::core::Result<HINSTANCE> {
    // SAFETY: GetModuleHandleW(None) returns this executable's handle.
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(pill_proc),
        hInstance: hinst,
        lpszClassName: CLASS,
        // What the input window draws, at alpha 1.
        // SAFETY: a stock brush.
        hbrBackground: HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0),
        // SAFETY: a stock system cursor.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        ..Default::default()
    };
    // SAFETY: wc is fully initialised.
    if unsafe { RegisterClassExW(&wc) } == 0 {
        let e = windows::core::Error::from_thread();
        if e.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
            return Err(e);
        }
    }
    Ok(hinst)
}

/// The monitor under `at`, its work area and its scale.
fn place_at(at: (i32, i32)) -> Place {
    let pt = POINT { x: at.0, y: at.1 };
    // SAFETY: plain FFI calls on a monitor handle the system just gave us.
    unsafe {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let work = if GetMonitorInfoW(mon, &mut info).as_bool() {
            rect_from(info.rcWork)
        } else {
            // Not expected: the nearest monitor always exists.
            Rect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            }
        };
        let (mut dpi, mut dpi_y) = (96, 96);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y).is_err() {
            dpi = 96;
        }
        Place {
            monitor: mon.0 as isize,
            work,
            scale: dpi as f32 / 96.0,
        }
    }
}

/// The app's handlers for a pill's ✕.
struct Callbacks {
    cancel: Box<dyn Fn(DragId) + Send>,
    dismiss: Box<dyn Fn(DragId) + Send>,
}

struct State {
    hinst: HINSTANCE,
    board: PillBoard,
    on: Callbacks,
    anchors: Vec<Anchor>,
    /// In no particular order; matched to the views by drag.
    wins: Vec<PillWin>,
    held: bool,
    schedule: Schedule,
}

/// When the windows are next redrawn.
#[derive(Debug, Default)]
struct Schedule {
    /// Something changed that the windows don't show yet.
    dirty: bool,
    last_draw: Option<Instant>,
    /// The board's next frame as of the last draw. Asking again later
    /// would not do: while animating it is always 16 ms from the asking.
    next_frame: Option<Instant>,
}

impl Schedule {
    /// The windows now show `board` as of `now`.
    fn drawn(&mut self, board: &PillBoard, now: Instant) {
        self.dirty = false;
        self.last_draw = Some(now);
        self.next_frame = board.next_frame(now);
    }

    /// When the next redraw is due; `None` sleeps until a message comes.
    fn due(&self, now: Instant) -> Option<Instant> {
        redraw_at(now, self.dirty, self.next_frame, self.last_draw)
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Set by the window procedure when monitors, DPI or the work area
    /// change; the loop then redraws, which moves every pill.
    static LAYOUT_STALE: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` on the thread state. `None` if it is absent or already borrowed:
/// a message sent while the state is busy gets default handling.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

fn run(
    on: Callbacks,
    cmds: mpsc::Receiver<PillCmd>,
    pending: &AtomicBool,
    ready: mpsc::SyncSender<Result<u32>>,
) {
    let hinst = match register_class() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(anyhow!(e).context("register pill window class")));
            return;
        }
    };
    // Make the message queue now, so the first wake can't be lost.
    let mut msg = MSG::default();
    // SAFETY: peeks this thread's own queue without removing anything.
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            hinst,
            board: PillBoard::new(Peer::Mac, Peer::Pc),
            on,
            anchors: Vec::new(),
            wins: Vec::new(),
            held: false,
            schedule: Schedule::default(),
        })
    });
    // SAFETY: plain FFI call.
    if ready.send(Ok(unsafe { GetCurrentThreadId() })).is_ok() {
        message_loop(&cmds, pending);
    }
    // Dropping the state destroys every window.
    STATE.with(|cell| cell.borrow_mut().take());
    info!("pill thread: stopped");
}

fn message_loop(cmds: &mpsc::Receiver<PillCmd>, pending: &AtomicBool) {
    let timer = FrameTimer::new();
    let timer = timer.as_ref();
    let mut msg = MSG::default();
    loop {
        // SAFETY: standard message pump on the thread that owns the windows.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                return;
            }
            if msg.hwnd.is_invalid() && msg.message == WM_APP {
                // Cleared before the drain below, so a later command posts again.
                // A swap, not a store: it is ordered against the relay's swap.
                pending.swap(false, Ordering::SeqCst);
                continue;
            }
            // SAFETY: `msg` was just filled in by PeekMessageW on this thread.
            unsafe { DispatchMessageW(&msg) };
        }
        if LAYOUT_STALE.take() {
            with_state(|s| s.schedule.dirty = true);
        }
        // Drained on every wake, so a failed wake only delays a command.
        loop {
            match cmds.try_recv() {
                Ok(cmd) => {
                    with_state(|s| s.on_command(cmd, Instant::now()));
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        let now = Instant::now();
        let due = with_state(|s| s.schedule.due(now)).flatten();
        if due.is_some_and(|t| t <= now) {
            with_state(|s| s.redraw(now));
            continue;
        }
        wait(timer, due.map(|t| t.saturating_duration_since(now)));
    }
}

/// A high-resolution waitable timer for the frames: a plain wait timeout
/// rounds up to the 15.6 ms system tick, which turns 33 ms frames into 47 ms.
struct FrameTimer(HANDLE);

impl FrameTimer {
    fn new() -> Option<FrameTimer> {
        // SAFETY: an unnamed timer with default security, closed on drop.
        let made = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        };
        made.inspect_err(|e| warn!("pill: no high-resolution timer, frames may run slow: {e}"))
            .ok()
            .map(FrameTimer)
    }
}

impl Drop for FrameTimer {
    fn drop(&mut self) {
        // SAFETY: our own handle, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Sleep until a message comes or `wait` is over; with no `wait`, until a message.
fn wait(timer: Option<&FrameTimer>, wait: Option<Duration>) {
    let flags = (QS_ALLINPUT, MWMO_INPUTAVAILABLE);
    let Some(wait) = wait else {
        // SAFETY: waits on this thread's queue only.
        unsafe { MsgWaitForMultipleObjectsEx(None, INFINITE, flags.0, flags.1) };
        return;
    };
    if let Some(timer) = timer {
        // Negative: relative, in 100 ns units.
        let due = -(i64::try_from(wait.as_nanos() / 100)
            .unwrap_or(i64::MAX)
            .max(1));
        // SAFETY: arms our own timer; no completion routine.
        if unsafe { SetWaitableTimer(timer.0, &due, 0, None, None, false) }.is_ok() {
            // SAFETY: the handle is live for the call.
            unsafe { MsgWaitForMultipleObjectsEx(Some(&[timer.0]), INFINITE, flags.0, flags.1) };
            return;
        }
    }
    // Round up, so an early wake doesn't spin on a 0 ms wait.
    let ms = u32::try_from(wait.as_micros().div_ceil(1_000)).unwrap_or(INFINITE - 1);
    // SAFETY: waits on this thread's queue only.
    unsafe { MsgWaitForMultipleObjectsEx(None, ms, flags.0, flags.1) };
}

impl State {
    fn on_command(&mut self, cmd: PillCmd, now: Instant) {
        match cmd {
            PillCmd::Start { drag, items, at } => {
                self.board.start(drag, &items, now);
                if !self.anchors.iter().any(|a| a.drag == drag) {
                    self.anchors.push(Anchor {
                        drag,
                        at,
                        since: now,
                    });
                }
            }
            PillCmd::Progress { drag, done, total } => self.board.progress(drag, done, total, now),
            PillCmd::Finish { drag, result } => self.board.finish(drag, result, now),
            PillCmd::Held(held) => {
                self.board.set_held(held, now);
                self.held = held;
                if held {
                    // Gone at once, not at the next frame.
                    self.redraw(now);
                    return;
                }
            }
        }
        self.schedule.dirty = true;
    }

    fn redraw(&mut self, now: Instant) {
        self.schedule.drawn(&self.board, now);
        let views = self.board.views(now);
        let on_board = |drag: DragId| views.iter().any(|v| v.drag == drag);
        // A drag not yet shown, or hidden while held, keeps its anchor.
        let held = self.held;
        self.anchors.retain(|a| {
            held || on_board(a.drag) || now.saturating_duration_since(a.since) < SHOW_AFTER
        });
        self.wins.retain(|w| on_board(w.drag));
        let places: Vec<Place> = views
            .iter()
            .map(|v| {
                let anchor = self.anchors.iter().find(|a| a.drag == v.drag);
                place_at(anchor.map_or((0, 0), |a| a.at))
            })
            .collect();
        let monitors: Vec<isize> = places.iter().map(|p| p.monitor).collect();
        for ((view, place), slot) in views.iter().zip(&places).zip(stack_slots(&monitors)) {
            let i = match self.wins.iter().position(|w| w.drag == view.drag) {
                Some(i) => i,
                None => match PillWin::new(self.hinst, view.drag) {
                    Ok(w) => {
                        self.wins.push(w);
                        self.wins.len() - 1
                    }
                    Err(e) => {
                        warn!("pill: {e:#}");
                        continue;
                    }
                },
            };
            let win = &mut self.wins[i];
            let origin = stack_origin(place.work, place.scale, slot);
            if let Err(e) = win.show(view, place.scale, origin)
                && !std::mem::replace(&mut win.warned, true)
            {
                warn!("pill: draw failed: {e:#}");
            }
        }
    }

    fn win(&mut self, input: HWND) -> Option<&mut PillWin> {
        self.wins.iter_mut().find(|w| w.input == input)
    }

    fn on_hover(&mut self, input: HWND, hover: bool) {
        let Some(win) = self.win(input) else {
            return;
        };
        if win.hover == hover {
            return;
        }
        if hover {
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: input,
                dwHoverTime: 0,
            };
            // SAFETY: asks for WM_MOUSELEAVE on our own window.
            if let Err(e) = unsafe { TrackMouseEvent(&mut track) } {
                warn!("pill: TrackMouseEvent failed: {e}");
                return;
            }
        }
        win.hover = hover;
        self.schedule.dirty = true;
    }

    /// A click at `at` in the input window: only a drawn ✕ acts.
    fn on_click(&mut self, input: HWND, at: (i32, i32)) {
        let Some(win) = self.win(input) else {
            return;
        };
        let Some(shown) = win.shown.as_ref() else {
            return;
        };
        let (view, hit) = (
            &shown.view,
            // As drawn: a ✕ that hover was about to show isn't there yet.
            close_hit(&shown.view, shown.hover, shown.scale, at),
        );
        let (drag, running, failed) = (view.drag, view.running, view.failed);
        if hit && running {
            // Never unwind out of the window procedure.
            let cancel = &self.on.cancel;
            if std::panic::catch_unwind(AssertUnwindSafe(|| cancel(drag))).is_err() {
                warn!("pill: on_cancel panicked for drag {drag}");
            }
        } else if hit && failed {
            self.board.dismiss(drag);
            self.schedule.dirty = true;
            let dismiss = &self.on.dismiss;
            if std::panic::catch_unwind(AssertUnwindSafe(|| dismiss(drag))).is_err() {
                warn!("pill: on_dismiss panicked for drag {drag}");
            }
        }
        self.on_hover(input, true);
    }
}

unsafe extern "system" fn pill_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE | WM_MOUSELEAVE => {
            with_state(|s| s.on_hover(hwnd, msg == WM_MOUSEMOVE));
            return LRESULT(0);
        }
        // Sent to top-level windows; the work area or DPI under a pill may
        // have moved. Default handling follows.
        WM_DISPLAYCHANGE | WM_DPICHANGED => LAYOUT_STALE.set(true),
        WM_SETTINGCHANGE if wparam.0 == SPI_SETWORKAREA.0 as usize => LAYOUT_STALE.set(true),
        WM_LBUTTONDOWN => {
            // The client coordinates, as signed 16-bit halves.
            let at = (lparam.0 as i16 as i32, (lparam.0 >> 16) as i16 as i32);
            with_state(|s| s.on_click(hwnd, at));
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: default handling for every other message.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// The top-left corner of the `index`th pill's window (shadow pad included)
/// on a monitor whose work area is `work`: centred, `BOTTOM_MARGIN` above
/// the bottom, stacking upward `PILL_GAP` apart.
fn stack_origin(work: Rect, scale: f32, index: usize) -> (i32, i32) {
    let (w, h) = pill_px(scale);
    let (w, h) = (w as i32, h as i32);
    let px = |v: f32| (v * scale).round() as i32;
    let pad = pad_px(scale);
    let left = work.left + (work.right - work.left - w) / 2;
    let bottom = work.bottom - px(BOTTOM_MARGIN) - index as i32 * (h + px(PILL_GAP));
    (left - pad, bottom - h - pad)
}

/// The shadow pad, in px, on each side of the body.
fn pad_px(scale: f32) -> i32 {
    let ((pw, _), (bw, _)) = (pixmap_px(scale), pill_px(scale));
    ((pw - bw) / 2) as i32
}

/// The body, as (x, y, width, height) in screen px, of a pill window at `origin`.
fn body_rect(origin: (i32, i32), scale: f32) -> (i32, i32, i32, i32) {
    let (w, h) = pill_px(scale);
    let pad = pad_px(scale);
    (origin.0 + pad, origin.1 + pad, w as i32, h as i32)
}

/// For each pill, bottom-most first, its place in its monitor's stack.
fn stack_slots(monitors: &[isize]) -> Vec<usize> {
    monitors
        .iter()
        .enumerate()
        .map(|(i, m)| monitors[..i].iter().filter(|o| *o == m).count())
        .collect()
}

/// Whether a click at `at` in the input window (body px) lands on a drawn ✕.
fn close_hit(view: &PillView, hover: bool, scale: f32, at: (i32, i32)) -> bool {
    let r = close_rect(scale);
    let pad = pad_px(scale) as f32;
    // The clicked pixel's centre, in pixmap px.
    let (x, y) = (at.0 as f32 + pad + 0.5, at.1 as f32 + pad + 0.5);
    close_shown(view, hover) && x >= r.left() && x < r.right() && y >= r.top() && y < r.bottom()
}

/// When to redraw next: now on a change, else at the board's next frame,
/// but never within `MIN_FRAME` of the last draw. `None` sleeps until a
/// command or a mouse message comes.
fn redraw_at(
    now: Instant,
    dirty: bool,
    next_frame: Option<Instant>,
    last_draw: Option<Instant>,
) -> Option<Instant> {
    let want = if dirty { Some(now) } else { next_frame };
    want.map(|t| last_draw.map_or(t, |last| t.max(last + MIN_FRAME)))
}

/// Premultiplied RGBA (tiny-skia) to premultiplied BGRA (a GDI DIB).
fn to_bgra(rgba: &[u8], bgra: &mut [u8]) {
    for (d, s) in bgra
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(rgba.as_chunks::<4>().0)
    {
        *d = [s[2], s[1], s[0], s[3]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_core::dnd::ItemKind;
    use cj_core::pill::PillIcon;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    const WORK: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };

    fn view(running: bool, failed: bool) -> PillView {
        PillView {
            drag: 1,
            icon: PillIcon::Video,
            name: "Holiday.mov".into(),
            right: String::new(),
            sub: String::new(),
            fraction: None,
            sheen_phase: 0.0,
            check_scale: 0.0,
            opacity: 1.0,
            failed,
            running,
        }
    }

    #[test]
    fn stack_origin_bottom_centre_and_upward() {
        // Scale 1: a 360x52 body with a 12 px pad, 24 px above the bottom.
        assert_eq!(stack_origin(WORK, 1.0, 0), (780 - 12, 1016 - 52 - 12));
        // The next one sits PILL_GAP above it.
        assert_eq!(stack_origin(WORK, 1.0, 1), (768, 1016 - 60 - 52 - 12));
        // Scale 1.5: a 540x78 body, an 18 px pad, 36 px margin and 12 px gap.
        assert_eq!(stack_origin(WORK, 1.5, 0), (690 - 18, 1004 - 78 - 18));
        assert_eq!(stack_origin(WORK, 1.5, 1), (672, 1004 - 90 - 78 - 18));
    }

    #[test]
    fn stack_origin_follows_the_work_area() {
        let right = Rect {
            left: 1920,
            top: -200,
            right: 3840,
            bottom: 880,
        };
        assert_eq!(
            stack_origin(right, 1.0, 0),
            (1920 + 768, 880 - 24 - 52 - 12)
        );
    }

    #[test]
    fn body_rect_is_inside_the_pad() {
        assert_eq!(pad_px(1.0), 12);
        assert_eq!(pad_px(1.5), 18);
        assert_eq!(body_rect((768, 952), 1.0), (780, 964, 360, 52));
        assert_eq!(body_rect((672, 908), 1.5), (690, 926, 540, 78));
    }

    #[test]
    fn neighbouring_bodies_dont_overlap() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let lower = body_rect(stack_origin(WORK, scale, 0), scale);
            let upper = body_rect(stack_origin(WORK, scale, 1), scale);
            assert!(upper.1 + upper.3 < lower.1, "scale {scale}");
        }
    }

    #[test]
    fn each_monitor_stacks_its_own_pills() {
        assert_eq!(stack_slots(&[]), Vec::<usize>::new());
        assert_eq!(stack_slots(&[7, 7, 7]), [0, 1, 2]);
        assert_eq!(stack_slots(&[7, 9, 7, 9, 9]), [0, 0, 1, 1, 2]);
    }

    #[test]
    fn close_hit_only_on_a_shown_close() {
        // The ✕ centre in input-window px: the close rect less the pad.
        let at = |scale: f32| {
            let r = close_rect(scale);
            let pad = pad_px(scale) as f32;
            (
                ((r.left() + r.right()) / 2.0 - pad) as i32,
                ((r.top() + r.bottom()) / 2.0 - pad) as i32,
            )
        };
        for scale in [1.0, 1.5] {
            let hit = at(scale);
            assert!(close_hit(&view(true, false), true, scale, hit));
            assert!(!close_hit(&view(true, false), false, scale, hit));
            assert!(close_hit(&view(false, true), false, scale, hit));
            // Done or cancelled: no ✕, so no click.
            assert!(!close_hit(&view(false, false), true, scale, hit));
            // Elsewhere on the body.
            assert!(!close_hit(&view(false, true), true, scale, (10, 10)));
        }
    }

    #[test]
    fn close_hit_edges_follow_close_rect() {
        let r = close_rect(1.0);
        let pad = pad_px(1.0) as f32;
        let failed = view(false, true);
        let (l, t) = ((r.left() - pad) as i32, (r.top() - pad) as i32);
        let (rt, b) = ((r.right() - pad) as i32, (r.bottom() - pad) as i32);
        assert!(close_hit(&failed, false, 1.0, (l, t)));
        assert!(close_hit(&failed, false, 1.0, (rt - 1, b - 1)));
        assert!(!close_hit(&failed, false, 1.0, (l - 1, t)));
        assert!(!close_hit(&failed, false, 1.0, (rt, t)));
        assert!(!close_hit(&failed, false, 1.0, (l, b)));
    }

    #[test]
    fn redraws_are_capped_and_idle_sleeps() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // Nothing to do: no wake at all.
        assert_eq!(redraw_at(t0, false, None, Some(t0)), None);
        // A change draws now, unless the last draw was under 33 ms ago.
        assert_eq!(redraw_at(t0, true, None, None), Some(t0));
        assert_eq!(
            redraw_at(t0 + ms(50), true, None, Some(t0)),
            Some(t0 + ms(50))
        );
        assert_eq!(
            redraw_at(t0 + ms(5), true, None, Some(t0)),
            Some(t0 + ms(33))
        );
        // An animation frame 16 ms out waits for the cap.
        assert_eq!(
            redraw_at(t0, false, Some(t0 + ms(16)), Some(t0)),
            Some(t0 + ms(33))
        );
        // A far frame is kept as it is.
        assert_eq!(
            redraw_at(t0, false, Some(t0 + ms(500)), Some(t0)),
            Some(t0 + ms(500))
        );
        // A change wins over a later frame.
        assert_eq!(
            redraw_at(t0 + ms(40), true, Some(t0 + ms(500)), Some(t0)),
            Some(t0 + ms(40))
        );
    }

    #[test]
    fn an_animating_board_is_redrawn_every_frame() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Mac, Peer::Pc);
        let item = DragItem {
            name: "Holiday.mov".into(),
            kind: ItemKind::File,
            size: Some(10_000_000),
        };
        board.start(1, &[item], t0);
        board.progress(1, 1_000, Some(10_000_000), t0 + ms(600));
        // What the loop does: a command marks it dirty, then it sleeps until
        // `due` and redraws whenever `due` has come.
        let mut schedule = Schedule {
            dirty: true,
            ..Schedule::default()
        };
        let mut now = t0 + ms(600);
        for frame in 0..10 {
            let due = schedule.due(now).expect("animating");
            assert!(due <= now, "frame {frame} not due at its wake");
            schedule.drawn(&board, now);
            let next = schedule.due(now).expect("animating");
            assert_eq!(next, now + MIN_FRAME, "frame {frame}");
            now = next;
        }
        // Asked again at the wake, the frame is always ahead: it would never come.
        assert!(board.next_frame(now).is_some_and(|t| t > now));
    }

    #[test]
    fn bgra_swaps_red_and_blue() {
        let rgba = [1, 2, 3, 4, 10, 20, 30, 40];
        let mut bgra = [0u8; 8];
        to_bgra(&rgba, &mut bgra);
        assert_eq!(bgra, [3, 2, 1, 4, 30, 20, 10, 40]);
    }

    #[test]
    fn thread_stops_when_the_sender_is_dropped() {
        // The thread owns its callbacks; their channel closes once it ends.
        let (tx, rx) = mpsc::channel::<DragId>();
        let pills = spawn_pill_thread(
            move |d| {
                let _ = tx.send(d);
            },
            |_| {},
        )
        .expect("spawn");
        // Not shown within SHOW_AFTER, so no window is made.
        pills
            .send(PillCmd::Start {
                drag: 1,
                items: Vec::new(),
                at: (0, 0),
            })
            .expect("send");
        drop(pills);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }

    /// A fake transfer to look at: one pill, a second stacked over it, the
    /// first finishing (✓ and fade), the second failing (✕ stays), a 1 s
    /// game hold, then a click on the failed ✕ dismisses it. Hovering a
    /// running pill and clicking its ✕ cancels it.
    #[test]
    #[ignore = "shows windows for a person to look at"]
    fn smoke_pill_runs_5s() {
        // Shows a failed draw, which is only logged.
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        // As the app does in `init_process` (without its cursor reset and
        // panic hook). Fails harmlessly if the manifest already set it.
        // SAFETY: plain FFI call, before any window exists.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let ms = Duration::from_millis;
        let mut cursor = POINT::default();
        // SAFETY: reads the cursor position.
        let _ = unsafe { GetCursorPos(&mut cursor) };
        let at = (cursor.x, cursor.y);
        let (cancel_tx, cancel_rx) = mpsc::channel::<DragId>();
        let pills = spawn_pill_thread(
            move |d| {
                let _ = cancel_tx.send(d);
            },
            |d| info!(drag = d, "smoke: dismissed"),
        )
        .expect("spawn");
        let send = |cmd: PillCmd| pills.send(cmd).expect("pill thread alive");
        let file = |name: &str, size: Option<u64>| DragItem {
            name: name.into(),
            kind: ItemKind::File,
            size,
        };
        const TOTAL: u64 = 250_000_000;
        let t0 = Instant::now();
        send(PillCmd::Start {
            drag: 1,
            items: vec![file("Holiday.mov", Some(TOTAL))],
            at,
        });
        let mut second = false;
        let mut running = [true, false];
        while t0.elapsed() < ms(5_500) {
            let t = t0.elapsed();
            if !second && t >= ms(1_500) {
                second = true;
                running[1] = true;
                send(PillCmd::Start {
                    drag: 2,
                    items: vec![file("a.png", None), file("b.png", None)],
                    at,
                });
            }
            while let Ok(drag) = cancel_rx.try_recv() {
                eprintln!("smoke: ✕ clicked on drag {drag}; cancelling it");
                running[(drag - 1) as usize] = false;
                send(PillCmd::Finish {
                    drag,
                    result: Err(FailReason::Cancelled),
                });
            }
            if running[0] {
                let done = (TOTAL as f64 * (t.as_secs_f64() / 5.0).min(1.0)) as u64;
                send(PillCmd::Progress {
                    drag: 1,
                    done,
                    total: Some(TOTAL),
                });
                if t >= ms(5_000) {
                    running[0] = false;
                    send(PillCmd::Finish {
                        drag: 1,
                        result: Ok(()),
                    });
                }
            }
            if running[1] {
                let done = (t.saturating_sub(ms(1_500)).as_millis() as u64) * 9_000;
                send(PillCmd::Progress {
                    drag: 2,
                    done,
                    total: None,
                });
            }
            std::thread::sleep(ms(50));
        }
        if running[1] {
            send(PillCmd::Finish {
                drag: 2,
                result: Err(FailReason::Disconnected),
            });
        }
        std::thread::sleep(ms(2_500));
        eprintln!("smoke: game holds the mouse for 1 s; the pills hide");
        send(PillCmd::Held(true));
        std::thread::sleep(ms(1_000));
        send(PillCmd::Held(false));
        eprintln!("smoke: released; click the failed pill's ✕ to dismiss it");
        std::thread::sleep(ms(4_000));
        drop(pills);
        std::thread::sleep(ms(300));
    }
}
