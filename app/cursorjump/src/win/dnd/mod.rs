//! File drag-and-drop on the PC: the edge catcher for drags leaving the PC,
//! and the shield that lets drags from the Mac start with the button down.
//!
//! One thread ("cj-dnd", OLE initialised: an STA) owns two of CursorJump's
//! own windows, both of class `CursorJumpCatcher`: topmost, never activated,
//! layered at alpha 1 (invisible but hit-testable). No hooks, nothing drawn
//! into other processes, nothing injected.
//! - The catcher, registered with `RegisterDragDrop`, is hidden, or a 2 px
//!   strip along the Mac edge while the left button is held near it
//!   (`CatcherZone`), or the whole virtual screen while the PC's drag rides
//!   on the Mac (`Park`), so that the user's real release, which the input
//!   hook then lets through, lands on it and ends the drag with `Drop`.
//! - The shield is 64×64 logical px around the park point while the cursor
//!   is on the Mac and nothing rides (a riding catcher already covers the
//!   park point, and the shield above it would take the drag off it). The
//!   input hook lets a real left press through only while one of these
//!   windows lies under the park point ([`DndShared::under_park_is_ours`]),
//!   so the press goes to a cj-dnd window while the button is physically
//!   down: what `DoDragDrop` on this thread needs (it takes the capture
//!   itself) for a drag from the Mac. The shield ignores the clicks it gets.
//!
//! Neither window shows while a game holds the mouse: the strip needs the
//! input thread's game check, and the shield needs the cursor on the Mac,
//! where it can't cross while a game holds it, and no foreground window
//! covering the edge monitor. Neither shows without a session
//! ([`DndShared::session_up`]).
//!
//! Commands are relayed by "cj-dnd-cmd", which posts `WM_APP` to the
//! catcher window. Modal loops (COM calls, `DoDragDrop`) dispatch window
//! messages, so commands keep flowing inside them; they are also drained
//! after every message the main loop dispatches.
mod catcher;

use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cj_core::config::Config;
use cj_core::dnd::{DragId, DragItem, FailReason};
use cj_core::layout::{Rect, Side};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, ERROR_INVALID_WINDOW_HANDLE, HINSTANCE, HWND, LPARAM,
    LRESULT, POINT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, GetStockObject, HBRUSH, MONITOR_DEFAULTTONEAREST, MonitorFromPoint,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, OleInitialize, OleUninitialize,
    RegisterDragDrop, RevokeDragDrop,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_TOPMOST,
    IDC_ARROW, KillTimer, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow, WM_APP, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETTINGCHANGE, WM_TIMER, WM_XBUTTONDBLCLK, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WindowFromPoint,
};
use windows::core::{PCWSTR, w};

pub use catcher::in_catch_zone;
use catcher::{Offered, Pending, Target, into_offer, strip_rect};

use super::input_thread::rect_center;
use super::monitors::{edge_monitor, monitors};
use super::probe::virtual_screen;
use crate::common::dnd_link::Source;

const CLASS: PCWSTR = w!("CursorJumpCatcher");
/// The shield's side, logical px.
const SHIELD_PX: f32 = 64.0;
/// While the catcher covers the screen, how often it checks that a drag
/// still holds the left button.
const STUCK_POLL: Duration = Duration::from_millis(250);
/// Left button up this long with the catcher still over the screen: no drag
/// can end on it any more, so it goes.
const STUCK_AFTER: Duration = Duration::from_millis(500);
/// The catcher's timer id for the stuck check.
const STUCK_TIMER: usize = 1;
/// The relay's last wake (the one that stops the thread) is retried this
/// often, for up to `LAST_WAKE_LIMIT`.
const LAST_WAKE_STEP: Duration = Duration::from_millis(10);
const LAST_WAKE_LIMIT: Duration = Duration::from_secs(1);

/// App and input thread → cj-dnd.
#[derive(Debug, Clone, PartialEq)]
pub enum DndCmd {
    /// Show (true) or hide the edge strip. The input thread sends it while on
    /// the PC: true only while the left button alone is held within
    /// `CATCH_ZONE_PX` of the Mac edge, a session is up and no game holds
    /// the mouse. It sends false on entering the Mac.
    CatcherZone(bool),
    /// true: grow the catcher over the virtual screen and accept the drop.
    /// The input thread sends it on entering the Mac with a drag, before it
    /// parks the cursor, and waits for [`DndShared::parked`]. Refused
    /// without a session. false: shrink it back to the strip, or hide it.
    Park(bool),
    /// The shield around the park point. The input thread sends true while
    /// on the Mac and no foreground window covers the edge monitor, false
    /// otherwise and on its return. true is refused without a session.
    Shield(bool),
    /// A forced return while `drag` rides: refuse the drop (`DragOver` says
    /// NONE) and stay over the screen until the user's real release ends the
    /// OS drag with a `DragLeave`. Its pending items fail `Cancelled`.
    Cancel { drag: DragId },
    /// The cursor came home carrying `drag`: back to the strip, offer
    /// cleared; the OS drag goes on locally.
    Home { drag: DragId },
    /// The Mac edge may have moved.
    ApplyConfig(Config),
}

/// One step of the input thread's crossing to the Mac, as far as cj-dnd
/// is concerned.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryStep {
    Send(DndCmd),
    /// Wait until [`DndShared::parked`] (bounded).
    AwaitParked,
    /// Move the cursor to the park point. A drag's source hit-tests there.
    ParkCursor,
}

/// The order of a crossing: with a drag, the catcher covers the screen
/// before anything hides the strip or moves the cursor, so the drag never
/// sees a moment without the catcher under it (OLE would send `DragLeave`
/// and the ride would be lost). `zone_shown`: the strip was last shown.
pub fn entry_steps(drag: bool, zone_shown: bool) -> Vec<EntryStep> {
    let mut steps = Vec::with_capacity(4);
    if drag {
        steps.push(EntryStep::Send(DndCmd::Park(true)));
        steps.push(EntryStep::AwaitParked);
    }
    if zone_shown {
        steps.push(EntryStep::Send(DndCmd::CatcherZone(false)));
    }
    steps.push(EntryStep::ParkCursor);
    steps
}

/// cj-dnd → app.
#[derive(Debug)]
pub enum WinDndEv {
    /// A file drag reached the strip. Item `i` is read from `sources[i]`.
    Offer {
        drag: DragId,
        items: Vec<DragItem>,
        sources: Vec<Source>,
    },
    /// The drag left the strip, or ended there or while riding without a drop.
    OfferGone { drag: DragId },
    /// The OS drag ended on the catcher: every virtual item is written out
    /// (or failed) and its source resolved.
    Spooled { drag: DragId },
}

/// Shared between cj-dnd, the input thread and the app.
#[derive(Debug, Default)]
pub struct DndShared {
    /// The drag the catcher holds; 0 = none.
    pub offer: AtomicU64,
    /// The physical left button, from Raw Input.
    pub left_down: AtomicBool,
    /// `WindowFromPoint(park point)` is the shield or the catcher, and no
    /// foreground window covers the edge monitor. Kept by the input thread
    /// while on the Mac (after parking, every 50 ms and on display
    /// changes), so the hook never asks Windows itself.
    pub under_park_is_ours: AtomicBool,
    /// The catcher covers the virtual screen.
    pub parked: AtomicBool,
    /// A session with the Mac is up. Without one there is no strip, no
    /// offer, no shield and no park. The app (Task 13) sets it on link up,
    /// before any crossing, and clears it on link down.
    pub session_up: AtomicBool,
    catcher: AtomicIsize,
    shield: AtomicIsize,
}

impl DndShared {
    fn is_ours(&self, hwnd: HWND) -> bool {
        let h = hwnd.0 as isize;
        h != 0
            && (h == self.catcher.load(Ordering::Acquire)
                || h == self.shield.load(Ordering::Acquire))
    }
}

/// Whether the window under `pt` is the shield or the catcher.
pub fn ours_under(shared: &DndShared, (x, y): (i32, i32)) -> bool {
    // SAFETY: plain FFI call.
    shared.is_ours(unsafe { WindowFromPoint(POINT { x, y }) })
}

/// What the input thread holds of cj-dnd.
#[derive(Debug, Clone)]
pub struct DndPort {
    pub shared: Arc<DndShared>,
    pub cmds: mpsc::Sender<DndCmd>,
}

impl DndPort {
    pub fn send(&self, cmd: DndCmd) {
        if self.cmds.send(cmd).is_err() {
            debug!("dnd thread gone; command dropped");
        }
    }
}

/// Start cj-dnd. Dropping every `Sender` stops it and closes its windows.
///
/// `events` runs on cj-dnd, inside OLE's calls: it must not block (the drag
/// source waits on it) and must not hold a clone of the returned `Sender`
/// (the thread would then never stop). A panic in it is caught and logged.
pub fn spawn_dnd_thread(
    cfg: Config,
    shared: Arc<DndShared>,
    events: impl Fn(WinDndEv) + Send + 'static,
) -> Result<mpsc::Sender<DndCmd>> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<DndCmd>();
    let (relay_tx, relay_rx) = mpsc::channel::<DndCmd>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<isize>>(1);
    std::thread::Builder::new()
        .name("cj-dnd".into())
        .spawn(move || run(cfg, shared, Box::new(events), relay_rx, ready_tx))
        .context("spawn dnd thread")?;
    // The thread reports its catcher window once it exists.
    let catcher = ready_rx.recv().context("dnd thread exited at start")??;
    std::thread::Builder::new()
        .name("cj-dnd-cmd".into())
        .spawn(move || relay(cmd_rx, relay_tx, catcher))
        .inspect_err(|_| {
            // The relay's sender is gone: let the thread see it and stop.
            last_wake(catcher);
        })
        .context("spawn dnd command relay")?;
    Ok(cmd_tx)
}

/// Forwards commands and wakes cj-dnd; once every sender is gone, drops its
/// own so the thread stops.
fn relay(from: mpsc::Receiver<DndCmd>, to: mpsc::Sender<DndCmd>, catcher: isize) {
    for cmd in from {
        if to.send(cmd).is_err() {
            return; // The dnd thread is gone.
        }
        if let Err(e) = wake(catcher) {
            // The next wake, or the next message, drains it.
            warn!("dnd thread wake failed: {e}");
        }
    }
    drop(to);
    last_wake(catcher);
}

fn wake(catcher: isize) -> windows::core::Result<()> {
    let hwnd = HWND(catcher as *mut _);
    // SAFETY: plain FFI call; a window that is gone just makes it fail.
    unsafe { PostMessageW(Some(hwnd), WM_APP, WPARAM(0), LPARAM(0)) }
}

/// The wake that lets the thread see the relay is gone: retried, so the
/// thread never waits for a message that isn't coming.
fn last_wake(catcher: isize) {
    let started = Instant::now();
    while let Err(e) = wake(catcher) {
        if e.code() == ERROR_INVALID_WINDOW_HANDLE.to_hresult() {
            return; // The thread has already gone.
        }
        if started.elapsed() >= LAST_WAKE_LIMIT {
            error!("dnd thread: last wake failed for {LAST_WAKE_LIMIT:?}: {e}");
            return;
        }
        std::thread::sleep(LAST_WAKE_STEP);
    }
}

/// 64×64 logical px (`scale` = monitor DPI / 96) centred on `park`.
fn shield_rect((x, y): (i32, i32), scale: f32) -> Rect {
    let side = (SHIELD_PX * scale).round() as i32;
    let (left, top) = (x - side / 2, y - side / 2);
    Rect {
        left,
        top,
        right: left + side,
        bottom: top + side,
    }
}

/// The DPI scale of the monitor under `pt`.
fn scale_at((x, y): (i32, i32)) -> f32 {
    // SAFETY: plain FFI calls on a monitor handle the system just gave us.
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let (mut dpi, mut dpi_y) = (96, 96);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y).is_err() {
            dpi = 96;
        }
        dpi as f32 / 96.0
    }
}

fn left_button_down() -> bool {
    // SAFETY: GetAsyncKeyState has no preconditions.
    (unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16) & 0x8000 != 0
}

fn fresh_drag_id() -> DragId {
    loop {
        let n = cj_net::random_nonce();
        let id = u64::from_le_bytes([n[0], n[1], n[2], n[3], n[4], n[5], n[6], n[7]]);
        if id != 0 {
            return id;
        }
    }
}

/// Where the catcher is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatcherAt {
    Hidden,
    Strip,
    Screen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placement {
    catcher: CatcherAt,
    shield: bool,
}

/// An offer the modes let go of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Released {
    drag: DragId,
    /// The app doesn't know yet: emit `OfferGone`.
    tell: bool,
}

/// The catcher's modes and the drag it holds, apart from the windows and
/// the data (pure, so it can be tested).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Modes {
    zone: bool,
    parked: bool,
    /// While parked: refuse the drop (after `Cancel`).
    reject: bool,
    shield_on: bool,
    offer: Option<DragId>,
}

impl Modes {
    fn placement(&self) -> Placement {
        let catcher = if self.parked {
            CatcherAt::Screen
        } else if self.zone {
            CatcherAt::Strip
        } else {
            CatcherAt::Hidden
        };
        Placement {
            catcher,
            // A riding catcher covers the park point: the shield above it
            // would take the drag off the drop target.
            shield: self.shield_on && !self.parked,
        }
    }

    /// While the drag rides on the Mac its image would sit at the park point.
    fn image_hidden(&self) -> bool {
        self.parked && !self.reject
    }

    fn unpark(&mut self) {
        self.parked = false;
        self.reject = false;
    }

    fn release(&mut self, tell: bool) -> Option<Released> {
        self.offer.take().map(|drag| Released { drag, tell })
    }

    fn command(&mut self, cmd: &DndCmd, session_up: bool) -> Option<Released> {
        match *cmd {
            DndCmd::CatcherZone(on) => self.zone = on,
            DndCmd::Park(true) | DndCmd::Shield(true) if !session_up => {
                debug!(?cmd, "dnd: refused without a session");
            }
            DndCmd::Park(true) => {
                self.parked = true;
                self.reject = false;
            }
            DndCmd::Park(false) => self.unpark(),
            DndCmd::Shield(on) => self.shield_on = on,
            DndCmd::Cancel { drag } => {
                if self.parked && self.offer == Some(drag) {
                    info!(drag, "catcher: cancelled while riding; refusing the drop");
                    self.reject = true;
                    return self.release(false);
                }
                debug!(drag, "catcher: Cancel for a drag it doesn't hold");
            }
            DndCmd::Home { drag } => {
                if self.reject {
                    debug!(drag, "catcher: Home while refusing a cancelled drag");
                    return None;
                }
                self.unpark();
                if self.offer == Some(drag) {
                    return self.release(false);
                }
            }
            DndCmd::ApplyConfig(_) => {}
        }
        None
    }

    /// A drag arrived: `fresh` is its new id if it may be offered (files, a
    /// copy allowed, a session up). Returns whether it is taken, and a stale
    /// offer let go of.
    fn enter(&mut self, fresh: Option<DragId>) -> (bool, Option<Released>) {
        if self.reject {
            return (false, None);
        }
        let stale = self.release(true);
        self.offer = fresh;
        (fresh.is_some(), stale)
    }

    fn over(&self) -> bool {
        self.offer.is_some() && !self.reject
    }

    /// The drag left, or ended without a drop.
    fn leave(&mut self) -> Option<Released> {
        if self.parked && !self.reject {
            info!("catcher: the drag ended while riding, without a drop");
        }
        self.unpark();
        self.release(true)
    }

    /// Whether a release now would be taken (see `take_drop`).
    fn takes_drop(&self) -> bool {
        self.offer.is_some() && self.parked && !self.reject
    }

    /// The release landed on the catcher. Taken only while riding and not
    /// refusing; it unparks at once, so the catcher never covers the PC
    /// while the items are written out. Otherwise it is a leave.
    fn take_drop(&mut self) -> Result<DragId, Option<Released>> {
        match self.offer {
            Some(drag) if self.parked && !self.reject => {
                self.unpark();
                self.offer = None;
                Ok(drag)
            }
            _ => Err(self.leave()),
        }
    }

    /// Over the screen with the button up for a while: no drag can end on it.
    fn stuck(&mut self) -> Option<Released> {
        self.unpark();
        self.release(true)
    }
}

/// One of our windows and where it is now (`None` = hidden).
struct Win {
    hwnd: HWND,
    at: Option<Rect>,
    /// Windows may have moved it (a display change): place it even if `at` matches.
    stale: bool,
}

impl Win {
    fn new(hwnd: HWND) -> Win {
        Win {
            hwnd,
            at: None,
            stale: false,
        }
    }

    fn place(&mut self, at: Option<Rect>) {
        if at == self.at && !self.stale {
            return;
        }
        self.at = at;
        self.stale = false;
        match at {
            // SAFETY: moves and shows our own window without activating it.
            Some(r) => {
                if let Err(e) = unsafe {
                    SetWindowPos(
                        self.hwnd,
                        Some(HWND_TOPMOST),
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    )
                } {
                    warn!("dnd: SetWindowPos failed: {e}");
                }
            }
            // SAFETY: hides our own window.
            None => {
                let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            }
        }
    }
}

struct State {
    shared: Arc<DndShared>,
    events: Box<dyn Fn(WinDndEv) + Send>,
    side: Side,
    edge: Option<Rect>,
    catcher: Win,
    shield: Win,
    helper: Option<IDropTargetHelper>,
    modes: Modes,
    /// The offer's virtual items, written out in `Drop`.
    pending: Vec<Pending>,
    /// Whether `IDropTargetHelper::Show(false)` is in force.
    image_hidden: bool,
    stuck_timer: bool,
    /// Since when the left button has been up with the catcher over the screen.
    up_since: Option<Instant>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static CMDS: RefCell<Option<mpsc::Receiver<DndCmd>>> = const { RefCell::new(None) };
}

/// Run `f` on the thread state. `None` if it is absent or already borrowed:
/// a call made while the state is busy gets default handling.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

/// Run the queued commands. While the state is busy (a modal loop inside
/// one of its calls) they wait for the next drain. Once every sender is
/// gone, the message loop is told to quit.
fn drain_commands() {
    loop {
        if STATE.with(|cell| cell.try_borrow_mut().is_err()) {
            return;
        }
        let next = CMDS.with(|cell| cell.borrow().as_ref().map(mpsc::Receiver::try_recv));
        match next {
            Some(Ok(cmd)) => {
                with_state(|s| s.on_command(cmd));
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => return,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                CMDS.with(|cell| cell.borrow_mut().take());
                // SAFETY: plain FFI call on this thread.
                unsafe { PostQuitMessage(0) };
                return;
            }
        }
    }
}

fn run(
    cfg: Config,
    shared: Arc<DndShared>,
    events: Box<dyn Fn(WinDndEv) + Send>,
    cmds: mpsc::Receiver<DndCmd>,
    ready: mpsc::SyncSender<Result<isize>>,
) {
    // SAFETY: OLE (an STA) on this thread for its whole life; paired below.
    if let Err(e) = unsafe { OleInitialize(None) } {
        let _ = ready.send(Err(anyhow!(e).context("OleInitialize")));
        return;
    }
    match State::new(cfg, shared, events) {
        Ok(state) => {
            let catcher = state.catcher.hwnd.0 as isize;
            STATE.with(|cell| *cell.borrow_mut() = Some(state));
            CMDS.with(|cell| *cell.borrow_mut() = Some(cmds));
            if ready.send(Ok(catcher)).is_ok() {
                message_loop();
            }
            CMDS.with(|cell| cell.borrow_mut().take());
            // Dropping the state revokes the target and destroys the windows.
            STATE.with(|cell| cell.borrow_mut().take());
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
    // SAFETY: pairs the OleInitialize above, after every COM object is gone.
    unsafe { OleUninitialize() };
    info!("dnd thread: stopped");
}

fn message_loop() {
    let mut msg = MSG::default();
    // 0 = WM_QUIT, -1 = error.
    // SAFETY: standard message pump on the thread that owns the windows.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: `msg` was just filled in by GetMessageW on this thread.
        unsafe { DispatchMessageW(&msg) };
        drain_commands();
    }
}

fn register_class() -> windows::core::Result<HINSTANCE> {
    // SAFETY: GetModuleHandleW(None) returns this executable's handle.
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(catcher_proc),
        hInstance: hinst,
        lpszClassName: CLASS,
        // What the windows draw, at alpha 1.
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

/// A hidden alpha-1 popup of our class.
fn create_window(hinst: HINSTANCE, title: PCWSTR) -> windows::core::Result<HWND> {
    // SAFETY: the class is registered; the window starts hidden.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            CLASS,
            title,
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
    }?;
    // SAFETY: our own window.
    if let Err(e) = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA) } {
        // SAFETY: our own window, destroyed once.
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(e);
    }
    Ok(hwnd)
}

impl State {
    fn new(
        cfg: Config,
        shared: Arc<DndShared>,
        events: Box<dyn Fn(WinDndEv) + Send>,
    ) -> Result<State> {
        let hinst = register_class().context("register catcher window class")?;
        let catcher = create_window(hinst, w!("CursorJump catcher")).context("create catcher")?;
        let shield = match create_window(hinst, w!("CursorJump shield")) {
            Ok(h) => h,
            Err(e) => {
                // SAFETY: our own window, destroyed once.
                let _ = unsafe { DestroyWindow(catcher) };
                return Err(anyhow!(e).context("create shield"));
            }
        };
        // Built now, so `Drop` cleans up whatever fails below.
        let mut state = State {
            shared,
            events,
            side: cfg.mac_side,
            edge: edge_monitor(&monitors(), cfg.mac_side),
            catcher: Win::new(catcher),
            shield: Win::new(shield),
            helper: None,
            modes: Modes::default(),
            pending: Vec::new(),
            image_hidden: false,
            stuck_timer: false,
            up_since: None,
        };
        let target: IDropTarget = Target { hwnd: catcher }.into();
        // SAFETY: OLE is initialised on this thread; the window is ours.
        unsafe { RegisterDragDrop(catcher, &target) }.context("RegisterDragDrop")?;
        // SAFETY: the shell's drag image helper, used on this thread only.
        state.helper =
            unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }
                .inspect_err(|e| warn!("dnd: no IDropTargetHelper: {e}"))
                .ok();
        let shared = &state.shared;
        shared.catcher.store(catcher.0 as isize, Ordering::Release);
        shared.shield.store(shield.0 as isize, Ordering::Release);
        info!(edge = ?state.edge, side = ?state.side, "dnd thread: ready");
        Ok(state)
    }

    fn emit(&self, ev: WinDndEv) {
        if std::panic::catch_unwind(AssertUnwindSafe(|| (self.events)(ev))).is_err() {
            error!("dnd: the event handler panicked");
        }
    }

    fn session_up(&self) -> bool {
        self.shared.session_up.load(Ordering::Acquire)
    }

    fn on_command(&mut self, cmd: DndCmd) {
        debug!(?cmd, "dnd: command");
        let released = self.modes.command(&cmd, self.session_up());
        self.settle(released);
        if let DndCmd::ApplyConfig(cfg) = cmd {
            self.side = cfg.mac_side;
            self.refresh_edge();
        }
        self.apply();
    }

    /// An offer the modes let go of: its pending items fail `Cancelled`.
    fn settle(&mut self, released: Option<Released>) {
        let Some(Released { drag, tell }) = released else {
            return;
        };
        self.shared.offer.store(0, Ordering::Release);
        for p in self.pending.drain(..) {
            p.resolve(Err(FailReason::Cancelled));
        }
        if tell {
            self.emit(WinDndEv::OfferGone { drag });
        }
    }

    fn refresh_edge(&mut self) {
        let edge = edge_monitor(&monitors(), self.side);
        if edge != self.edge {
            info!(?edge, side = ?self.side, "dnd: edge monitor changed");
        }
        self.edge = edge;
        self.catcher.stale = true;
        self.shield.stale = true;
    }

    /// Put both windows where the modes say, and keep the drag image and
    /// the stuck check in step.
    fn apply(&mut self) {
        let placement = self.modes.placement();
        let catcher = match placement.catcher {
            CatcherAt::Hidden => None,
            CatcherAt::Strip => self.edge.map(|e| strip_rect(e, self.side)),
            CatcherAt::Screen => Some(virtual_screen()),
        };
        let shield = self
            .edge
            .filter(|_| placement.shield)
            .map(|e| rect_center(&e))
            .map(|park| shield_rect(park, scale_at(park)));
        self.catcher.place(catcher);
        self.shield.place(shield);
        self.shared
            .parked
            .store(self.modes.parked, Ordering::Release);
        self.set_image_hidden(self.modes.image_hidden());
        self.set_stuck_timer(self.modes.parked);
    }

    fn set_image_hidden(&mut self, hide: bool) {
        if hide == self.image_hidden {
            return;
        }
        self.image_hidden = hide;
        if let Some(h) = &self.helper {
            // SAFETY: plain helper call on this thread.
            let r = unsafe { h.Show(!hide) };
            info!(hidden = hide, result = ?r, "catcher: drag image (IDropTargetHelper::Show)");
        }
    }

    fn set_stuck_timer(&mut self, on: bool) {
        if on == self.stuck_timer {
            return;
        }
        self.stuck_timer = on;
        self.up_since = None;
        let hwnd = self.catcher.hwnd;
        if on {
            // SAFETY: our own window's timer; WM_TIMER arrives in `catcher_proc`.
            if unsafe { SetTimer(Some(hwnd), STUCK_TIMER, STUCK_POLL.as_millis() as u32, None) }
                == 0
            {
                warn!(
                    "catcher: SetTimer failed: {}",
                    windows::core::Error::from_thread()
                );
            }
        } else {
            // SAFETY: kills our own timer; failure means it was not running.
            let _ = unsafe { KillTimer(Some(hwnd), STUCK_TIMER) };
        }
    }

    /// Covering the screen with no button held, no drag can end on the
    /// catcher any more (the source died, or an event went missing). On the
    /// Mac too: a ride's real release reaches Windows, so the button state is
    /// right there as well.
    fn check_stuck(&mut self) {
        if !self.modes.parked || left_button_down() {
            self.up_since = None;
            return;
        }
        let since = *self.up_since.get_or_insert_with(Instant::now);
        if since.elapsed() < STUCK_AFTER {
            return;
        }
        warn!("catcher: over the screen with no drag holding the button; unparking");
        let released = self.modes.stuck();
        self.settle(released);
        self.apply();
    }

    // ------------------------------------------------ IDropTarget decisions

    fn enter(&mut self, offered: Option<Offered>, allowed: DROPEFFECT) -> DROPEFFECT {
        let copy = allowed.0 & DROPEFFECT_COPY.0 != 0;
        if offered.is_some() && !copy {
            debug!("catcher: the source doesn't allow a copy; refused");
        }
        let offered = offered.filter(|_| copy && self.session_up());
        let fresh = offered.as_ref().map(|_| fresh_drag_id());
        let (taken, stale) = self.modes.enter(fresh);
        self.settle(stale);
        let (true, Some(drag), Some(offered)) = (taken, fresh, offered) else {
            return DROPEFFECT_NONE;
        };
        let (items, sources, pending) = into_offer(offered);
        info!(drag, ?items, "catcher: offer");
        self.pending = pending;
        self.shared.offer.store(drag, Ordering::Release);
        self.emit(WinDndEv::Offer {
            drag,
            items,
            sources,
        });
        DROPEFFECT_COPY
    }

    fn over(&self, allowed: DROPEFFECT) -> DROPEFFECT {
        if self.modes.over() && allowed.0 & DROPEFFECT_COPY.0 != 0 {
            DROPEFFECT_COPY
        } else {
            DROPEFFECT_NONE
        }
    }

    fn leave(&mut self) {
        let released = self.modes.leave();
        self.settle(released);
        self.apply();
    }

    /// The release landed on the catcher. Taken only while riding: the drag
    /// and the items to write out, with the catcher already off the screen.
    fn take_drop(&mut self) -> Option<(DragId, Vec<Pending>)> {
        match self.modes.take_drop() {
            Ok(drag) => {
                self.shared.offer.store(0, Ordering::Release);
                let pending = std::mem::take(&mut self.pending);
                info!(drag, pending = pending.len(), "catcher: drop");
                self.apply();
                Some((drag, pending))
            }
            Err(released) => {
                self.settle(released);
                self.apply();
                None
            }
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let released = self.modes.release(false);
        self.settle(released);
        self.shared.catcher.store(0, Ordering::Release);
        self.shared.shield.store(0, Ordering::Release);
        self.shared.parked.store(false, Ordering::Release);
        // SAFETY: undoes RegisterDragDrop on the same thread (harmless if it
        // never happened), then destroys our own windows once.
        unsafe {
            let _ = RevokeDragDrop(self.catcher.hwnd);
            let _ = KillTimer(Some(self.catcher.hwnd), STUCK_TIMER);
            let _ = DestroyWindow(self.catcher.hwnd);
            let _ = DestroyWindow(self.shield.hwnd);
        }
    }
}

unsafe extern "system" fn catcher_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_APP => {
            drain_commands();
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        // Presses passed through by the input hook land here: nothing to do.
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONUP
        | WM_RBUTTONDBLCLK | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MBUTTONDBLCLK | WM_XBUTTONDOWN
        | WM_XBUTTONUP | WM_XBUTTONDBLCLK => return LRESULT(0),
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_DPICHANGED => {
            with_state(|s| {
                s.refresh_edge();
                s.apply();
            });
        }
        WM_TIMER if wparam.0 == STUCK_TIMER => {
            with_state(State::check_stuck);
        }
        _ => {}
    }
    // SAFETY: default handling for our own window.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

    #[test]
    fn shield_rect_is_centred_on_the_park_point_and_scaled() {
        let r = |left, top, right, bottom| Rect {
            left,
            top,
            right,
            bottom,
        };
        assert_eq!(shield_rect((960, 540), 1.0), r(928, 508, 992, 572));
        assert_eq!(shield_rect((960, 540), 1.5), r(912, 492, 1008, 588));
        assert_eq!(shield_rect((1280, 720), 1.25), r(1240, 680, 1320, 760));
        assert_eq!(shield_rect((-540, 560), 2.0), r(-604, 496, -476, 624));
    }

    fn place(catcher: CatcherAt, shield: bool) -> Placement {
        Placement { catcher, shield }
    }

    fn cmd(m: &mut Modes, c: DndCmd) -> Option<Released> {
        m.command(&c, true)
    }

    #[test]
    fn placement_keeps_the_shield_off_a_riding_catcher() {
        let mut m = Modes::default();
        assert_eq!(m.placement(), place(CatcherAt::Hidden, false));
        cmd(&mut m, DndCmd::CatcherZone(true));
        assert_eq!(m.placement(), place(CatcherAt::Strip, false));
        cmd(&mut m, DndCmd::Park(true));
        assert_eq!(m.placement(), place(CatcherAt::Screen, false));
        // On the Mac while riding: the catcher alone covers the park point.
        cmd(&mut m, DndCmd::Shield(true));
        assert_eq!(m.placement(), place(CatcherAt::Screen, false));
        cmd(&mut m, DndCmd::CatcherZone(false));
        assert_eq!(m.placement(), place(CatcherAt::Screen, false));
        // The ride ends on the Mac: the shield comes back.
        cmd(&mut m, DndCmd::Park(false));
        assert_eq!(m.placement(), place(CatcherAt::Hidden, true));
        cmd(&mut m, DndCmd::Shield(false));
        assert_eq!(m.placement(), place(CatcherAt::Hidden, false));
    }

    #[test]
    fn modes_refuse_shield_and_park_without_a_session() {
        let mut m = Modes::default();
        m.command(&DndCmd::Park(true), false);
        m.command(&DndCmd::Shield(true), false);
        m.command(&DndCmd::CatcherZone(true), false);
        assert_eq!(m.placement(), place(CatcherAt::Strip, false));
        assert!(!m.parked);
    }

    #[test]
    fn modes_offer_then_leave_tells_the_app() {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::CatcherZone(true));
        assert_eq!(m.enter(Some(7)), (true, None));
        assert!(m.over());
        // A new drag replaces a stale offer, which the app hears about.
        assert_eq!(
            m.enter(Some(8)),
            (
                true,
                Some(Released {
                    drag: 7,
                    tell: true
                })
            )
        );
        assert_eq!(
            m.leave(),
            Some(Released {
                drag: 8,
                tell: true
            })
        );
        assert!(!m.over());
        // Not files (or no session): nothing offered.
        assert_eq!(m.enter(None), (false, None));
        assert!(!m.over());
        assert_eq!(m.leave(), None);
    }

    #[test]
    fn modes_drop_on_the_strip_takes_nothing() {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::CatcherZone(true));
        m.enter(Some(7));
        assert_eq!(
            m.take_drop(),
            Err(Some(Released {
                drag: 7,
                tell: true
            }))
        );
        assert_eq!(m.placement(), place(CatcherAt::Strip, false));
    }

    #[test]
    fn modes_ride_drop_unparks_before_spooling() {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::CatcherZone(true));
        m.enter(Some(7));
        cmd(&mut m, DndCmd::Park(true));
        cmd(&mut m, DndCmd::CatcherZone(false));
        cmd(&mut m, DndCmd::Shield(true));
        assert!(m.image_hidden());
        assert_eq!(m.take_drop(), Ok(7));
        assert_eq!(m.placement(), place(CatcherAt::Hidden, true));
        assert!(!m.image_hidden());
        assert_eq!(m.offer, None);
    }

    #[test]
    fn modes_ride_without_a_drop_ends_on_leave() {
        let mut m = Modes::default();
        m.enter(Some(7));
        cmd(&mut m, DndCmd::Park(true));
        assert_eq!(
            m.leave(),
            Some(Released {
                drag: 7,
                tell: true
            })
        );
        assert!(!m.parked);
    }

    #[test]
    fn modes_cancel_refuses_until_the_release() {
        let mut m = Modes::default();
        m.enter(Some(7));
        // Not riding: not its business.
        assert_eq!(cmd(&mut m, DndCmd::Cancel { drag: 7 }), None);
        cmd(&mut m, DndCmd::Park(true));
        assert_eq!(cmd(&mut m, DndCmd::Cancel { drag: 9 }), None);
        assert_eq!(
            cmd(&mut m, DndCmd::Cancel { drag: 7 }),
            Some(Released {
                drag: 7,
                tell: false
            })
        );
        assert_eq!(m.placement().catcher, CatcherAt::Screen);
        assert!(!m.over());
        assert!(!m.image_hidden(), "the user sees the drag refused");
        // Nothing new is offered while refusing, and Home doesn't unpark.
        assert_eq!(m.enter(Some(8)), (false, None));
        assert_eq!(cmd(&mut m, DndCmd::Home { drag: 7 }), None);
        assert!(m.parked);
        // The real release: no drop, back off the screen.
        assert_eq!(m.take_drop(), Err(None));
        assert!(!m.parked && !m.reject);
    }

    #[test]
    fn modes_home_unparks_and_clears_the_offer() {
        let mut m = Modes::default();
        m.enter(Some(7));
        cmd(&mut m, DndCmd::Park(true));
        assert_eq!(
            cmd(&mut m, DndCmd::Home { drag: 7 }),
            Some(Released {
                drag: 7,
                tell: false
            })
        );
        assert!(!m.parked);
        assert_eq!(m.offer, None);
        // Home for another drag still unparks, and keeps the offer.
        m.enter(Some(8));
        cmd(&mut m, DndCmd::Park(true));
        assert_eq!(cmd(&mut m, DndCmd::Home { drag: 7 }), None);
        assert!(!m.parked);
        assert_eq!(m.offer, Some(8));
    }

    #[test]
    fn modes_stuck_unparks_and_tells() {
        let mut m = Modes::default();
        m.enter(Some(7));
        cmd(&mut m, DndCmd::Park(true));
        assert_eq!(
            m.stuck(),
            Some(Released {
                drag: 7,
                tell: true
            })
        );
        assert_eq!(m.placement(), place(CatcherAt::Hidden, false));
    }

    /// Runs a crossing's steps against the modes of a catcher holding a
    /// drag on the strip; returns every placement up to the cursor's park.
    fn placements_until_parked(steps: &[EntryStep]) -> Vec<Placement> {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::CatcherZone(true));
        m.enter(Some(7));
        let mut seen = vec![m.placement()];
        for step in steps {
            match step {
                EntryStep::Send(c) => {
                    assert_eq!(cmd(&mut m, c.clone()), None, "{c:?} let the offer go");
                    seen.push(m.placement());
                }
                EntryStep::AwaitParked => assert!(m.parked, "awaited before Park(true)"),
                EntryStep::ParkCursor => return seen,
            }
        }
        panic!("the cursor was never parked");
    }

    #[test]
    fn a_crossing_with_a_drag_never_leaves_it_without_the_catcher() {
        let steps = entry_steps(true, true);
        assert_eq!(
            steps,
            [
                EntryStep::Send(DndCmd::Park(true)),
                EntryStep::AwaitParked,
                EntryStep::Send(DndCmd::CatcherZone(false)),
                EntryStep::ParkCursor,
            ]
        );
        let seen = placements_until_parked(&steps);
        assert!(
            seen.iter().all(|p| p.catcher != CatcherAt::Hidden),
            "{seen:?}"
        );
        assert_eq!(seen.last().unwrap().catcher, CatcherAt::Screen);
        // The other order would pass through Hidden: what the steps rule out.
        let wrong = [
            EntryStep::Send(DndCmd::CatcherZone(false)),
            EntryStep::Send(DndCmd::Park(true)),
            EntryStep::ParkCursor,
        ];
        assert!(
            placements_until_parked(&wrong)
                .iter()
                .any(|p| p.catcher == CatcherAt::Hidden)
        );
        // And no step order from `entry_steps` hides the strip first.
        for zone in [false, true] {
            let steps = entry_steps(true, zone);
            let park = steps.iter().position(|s| *s == EntryStep::AwaitParked);
            let hide = steps
                .iter()
                .position(|s| *s == EntryStep::Send(DndCmd::CatcherZone(false)));
            assert!(park.is_some());
            assert!(hide.is_none_or(|h| Some(h) > park), "{steps:?}");
        }
    }

    #[test]
    fn a_crossing_without_a_drag_just_hides_the_strip() {
        assert_eq!(
            entry_steps(false, true),
            [
                EntryStep::Send(DndCmd::CatcherZone(false)),
                EntryStep::ParkCursor
            ]
        );
        assert_eq!(entry_steps(false, false), [EntryStep::ParkCursor]);
        assert_eq!(
            entry_steps(true, false),
            [
                EntryStep::Send(DndCmd::Park(true)),
                EntryStep::AwaitParked,
                EntryStep::ParkCursor
            ]
        );
    }

    #[test]
    fn takes_drop_matches_take_drop() {
        let mut m = Modes::default();
        assert!(!m.takes_drop());
        m.enter(Some(7));
        assert!(!m.takes_drop(), "on the strip");
        cmd(&mut m, DndCmd::Park(true));
        assert!(m.takes_drop());
        assert_eq!(m.clone().take_drop(), Ok(7));
        cmd(&mut m, DndCmd::Cancel { drag: 7 });
        assert!(!m.takes_drop());
    }

    #[test]
    fn thread_stops_when_the_sender_is_dropped() {
        // The thread owns `events`; its channel closes once the thread ends.
        let (tx, rx) = mpsc::channel::<DragId>();
        let shared = Arc::new(DndShared::default());
        let cmds = spawn_dnd_thread(Config::default(), shared.clone(), move |ev| {
            if let WinDndEv::OfferGone { drag } = ev {
                let _ = tx.send(drag);
            }
        })
        .expect("spawn");
        assert_ne!(shared.catcher.load(Ordering::Acquire), 0);
        // Commands arrive through the catcher window: the strip shows.
        let catcher = HWND(shared.catcher.load(Ordering::Acquire) as *mut _);
        // SAFETY: plain FFI call on a window handle.
        let visible = || unsafe { IsWindowVisible(catcher) }.as_bool();
        cmds.send(DndCmd::CatcherZone(true)).expect("send");
        if edge_monitor(&monitors(), Config::default().mac_side).is_some() {
            let until = Instant::now() + Duration::from_secs(5);
            while !visible() {
                assert!(Instant::now() < until, "CatcherZone(true) never arrived");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        cmds.send(DndCmd::CatcherZone(false)).expect("send");
        drop(cmds);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        assert_eq!(shared.catcher.load(Ordering::Acquire), 0);
    }

    /// cj-dnd alone (the app isn't running), with a session marked up and
    /// the strip shown at the right edge of the primary monitor for the
    /// whole run. Drag files from Explorer (or a zip, Discord, an Outlook
    /// attachment) to that edge: each `Offer` is logged with its names,
    /// sizes and sources.
    ///
    /// `CJ_SMOKE_MODE`:
    /// - unset: offers only; let go anywhere and nothing is dropped.
    /// - `park`: each offer parks the catcher over the screen; let go
    ///   anywhere to see `Spooled`, what was written out, and the line
    ///   "drag image ... hidden: true" (look: did the image vanish?).
    /// - `ride`: as `park`, then the shield is turned on as on the Mac: the
    ///   shield must stay hidden, so letting go still gives `Spooled`.
    /// - `cancel`: parks, then cancels after 1 s: a "no" cursor until you let go.
    /// - `home`: parks, then sends it home after 1 s: the drag goes on locally.
    ///
    /// `CJ_SMOKE_SECS` (default 60) sets how long it runs.
    #[test]
    #[ignore = "needs a person to drag files"]
    fn smoke_catcher_logs_offer() {
        use std::collections::HashMap;
        use tracing_subscriber::EnvFilter;

        let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "cursorjump=debug".into());
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(filter))
            .try_init();
        // As the app does in `init_process`. Fails harmlessly if already set.
        // SAFETY: plain FFI call, before any window exists.
        let _ = unsafe {
            windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        };
        let mode = std::env::var("CJ_SMOKE_MODE").unwrap_or_default();
        let secs = std::env::var("CJ_SMOKE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        let (ev_tx, ev_rx) = mpsc::channel::<WinDndEv>();
        let shared = Arc::new(DndShared::default());
        shared.session_up.store(true, Ordering::Release);
        let cmds = spawn_dnd_thread(Config::default(), shared.clone(), move |ev| {
            let _ = ev_tx.send(ev);
        })
        .expect("spawn");
        let send = |cmd: DndCmd| cmds.send(cmd).expect("dnd thread alive");
        send(DndCmd::CatcherZone(true));
        eprintln!(
            "smoke: mode {:?}, {secs} s; drag files to the right edge of the primary monitor",
            if mode.is_empty() { "offers" } else { &mode }
        );
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut sources: HashMap<DragId, Vec<Source>> = HashMap::new();
        let mut later: Option<(Instant, DndCmd)> = None;
        while Instant::now() < deadline {
            if let Some((at, _)) = &later
                && Instant::now() >= *at
                && let Some((_, cmd)) = later.take()
            {
                eprintln!("smoke: sending {cmd:?}");
                send(cmd);
            }
            let ev = match ev_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) => ev,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("dnd thread stopped"),
            };
            match ev {
                WinDndEv::Offer {
                    drag,
                    items,
                    sources: srcs,
                } => {
                    eprintln!("smoke: Offer {drag}: {} item(s)", items.len());
                    for (item, src) in items.iter().zip(&srcs) {
                        let from = match src {
                            Source::Ready(p) => format!("ready {}", p.display()),
                            Source::Pending(_) => "pending (written out on Drop)".into(),
                        };
                        eprintln!(
                            "smoke:   {:?} {:?} size {:?} <- {from}",
                            item.name, item.kind, item.size
                        );
                    }
                    sources.insert(drag, srcs);
                    let after = Instant::now() + Duration::from_secs(1);
                    match mode.as_str() {
                        "park" => send(DndCmd::Park(true)),
                        "ride" => {
                            send(DndCmd::Park(true));
                            send(DndCmd::Shield(true));
                        }
                        "cancel" => {
                            send(DndCmd::Park(true));
                            later = Some((after, DndCmd::Cancel { drag }));
                        }
                        "home" => {
                            send(DndCmd::Park(true));
                            later = Some((after, DndCmd::Home { drag }));
                        }
                        _ => {}
                    }
                }
                WinDndEv::OfferGone { drag } => {
                    eprintln!("smoke: OfferGone {drag}");
                    sources.remove(&drag);
                }
                WinDndEv::Spooled { drag } => {
                    eprintln!("smoke: Spooled {drag}");
                    for (i, src) in sources.remove(&drag).unwrap_or_default().iter().enumerate() {
                        match src {
                            Source::Ready(p) => eprintln!("smoke:   [{i}] ready {}", p.display()),
                            Source::Pending(rx) => {
                                eprintln!("smoke:   [{i}] -> {:?}", *rx.borrow())
                            }
                        }
                    }
                    if mode == "ride" {
                        send(DndCmd::Shield(false));
                    }
                }
            }
        }
        send(DndCmd::CatcherZone(false));
        drop(cmds);
        // The thread drops `events` (and so `ev_tx`) once it has cleaned up.
        while ev_rx.recv_timeout(Duration::from_secs(5)).is_ok() {}
        assert_eq!(
            shared.catcher.load(Ordering::Acquire),
            0,
            "windows destroyed"
        );
        eprintln!("smoke: stopped cleanly");
    }
}
