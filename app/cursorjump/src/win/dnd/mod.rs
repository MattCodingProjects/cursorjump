//! File drag-and-drop on the PC: the edge catcher for drags leaving the PC,
//! the shield that lets drags from the Mac start with the button down, and
//! those drags themselves (`source`).
//!
//! Two STA threads (OLE initialised on each) own CursorJump's own windows,
//! all of class `CursorJumpCatcher`: topmost, never activated, layered at
//! alpha 1 (invisible but hit-testable). No hooks, nothing drawn into other
//! processes, nothing injected.
//! - "cj-dnd-out" owns the catcher. Registered with `RegisterDragDrop`, it
//!   is hidden, or a 2 px strip along the Mac edge while the left button is
//!   held near it (`CatcherZone`), or the whole virtual screen while the
//!   PC's drag rides on the Mac (`Park`), so that the user's real release,
//!   which the input hook then lets through, lands on it and ends the drag
//!   with `Drop`, where it spools virtual files.
//! - "cj-dnd-in" (`inbound`) owns the shield, 64×64 logical px around the
//!   park point while the cursor is on the Mac and nothing rides (a riding
//!   catcher already covers the park point, and the shield above it would
//!   take the drag off it). The input hook lets a real left press through
//!   only while one of these windows lies under the park point
//!   ([`DndShared::under_park_is_ours`]), so the press goes to a window of
//!   ours while the button is physically down: what `DoDragDrop` needs (it
//!   takes the capture itself) for a drag from the Mac, and why it runs on
//!   the shield's thread. The shield ignores the clicks it gets. So
//!   `Shield(false)` keeps a shown shield while such a drag is starting
//!   (until its first `QueryContinueDrag`: OLE holds its own capture from
//!   then on), or (the button held) may yet start: the app's
//!   `StartIncoming` and the input thread's hide come in either order.
//!
//! Why two threads (R39): while an STA thread waits in an outgoing COM call
//! (a target's synchronous `Drop` reading the bytes, or the catcher reading
//! FILECONTENTS in its own `Drop`), COM's modal loop dispatches none of its
//! posted messages or timers (`probe_posted_messages_during_an_outgoing_com_call`).
//! That can last minutes. Apart, a long Explorer drop from the Mac doesn't
//! hold up the catcher, nor a spool the shield. What the split leaves open:
//! - A µs race at `Park(true)`: the shield is hidden by cj-dnd-out's
//!   `ShowWindowAsync`, which the shield's own thread carries out, so for a
//!   moment after `parked` is set the shield may still be up. Only a shield
//!   kept for a press (within `KEEP_LIMIT` of a return home) can be up
//!   then, and the input thread parks the cursor only after `AwaitParked`;
//!   a ride from the PC starting in that moment is the whole exposure.
//! - A press that went to the catcher, not the shield (the catcher lies
//!   under the park point when the hook passes it), then a drag from the
//!   Mac: R35 sees Windows holding the button and runs `DoDragDrop` on
//!   cj-dnd-in, whose thread did not get the press. Its loop then gets no
//!   mouse input (spike 5d) until a `CancelIncoming` or the next click.
//!   The catcher is under the park point only while a PC drag rides,
//!   which a drag from the Mac can't start during.
//!
//! Neither window shows while a game holds the mouse: the strip needs the
//! input thread's game check, and the shield needs the cursor on the Mac,
//! where it can't cross while a game holds it, and no foreground window
//! covering the edge monitor. Neither shows without a session
//! ([`DndShared::session_up`]).
//!
//! Commands go through one `Sender` to "cj-dnd-cmd", which routes each to
//! its thread ([`route`]) and posts `WM_APP` to that thread's window.
//! `DoDragDrop`'s own modal loop dispatches window messages, so commands
//! keep flowing inside it; they are also drained after every message a
//! thread's main loop dispatches. The relay itself acts on `CancelIncoming`
//! and refuses a `StartIncoming` while a drag from the Mac runs, since
//! cj-dnd-in may be held in a target's `Drop` and see neither for minutes.
mod catcher;
mod inbound;
mod source;

use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cj_core::config::Config;
use cj_core::dnd::{DragId, DragItem, FailReason};
use cj_core::layout::{Rect, Side};
use tokio::sync::oneshot;
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
    SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow, ShowWindowAsync, WM_APP,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_NULL, WM_RBUTTONDBLCLK,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WM_XBUTTONDBLCLK, WM_XBUTTONDOWN,
    WM_XBUTTONUP, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WindowFromPoint,
};
use windows::core::{PCWSTR, w};

pub use catcher::in_catch_zone;
use catcher::{Offered, Pending, Target, into_offer, strip_rect};

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
/// The shield's timer id for a kept shield's check.
const KEEP_TIMER: usize = 2;
/// Posted to the shield by cj-dnd-out: the catcher's park changed.
const WM_PARKED: u32 = WM_APP + 1;
/// The relay's last wake (the one that stops a thread) is retried this
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
    /// otherwise and on its return. true is refused without a session;
    /// false may be put off for a drag from the Mac (see the module doc).
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
    /// A drag from the Mac came home with the button held: `DoDragDrop`
    /// on cj-dnd-in for `items`, whose bytes are fetched only once a target
    /// takes the drop. `IncomingEnded` follows, whether it ran or not: at
    /// once, from the relay, while another drag from the Mac runs (cj-dnd-in
    /// may be held in its target's `Drop` for minutes).
    StartIncoming { drag: DragId, items: Vec<DragItem> },
    /// The drag from the Mac is cancelled: its `DoDragDrop` ends without a
    /// drop, and a target's wait for its bytes after the drop fails
    /// (`E_FAIL`) within `source::REPLY_POLL`, as it does when cj-dnd
    /// stops. The relay applies it to a drag already registered, so it
    /// works even while a target's `Drop` holds cj-dnd-in; cj-dnd-in
    /// applies it to one registered after the relay's try (it gets
    /// commands in order, so a cancel sent right after `StartIncoming` is
    /// drained inside that drag's `DoDragDrop`). The app should still fail
    /// its outstanding `Fetch` replies with `Err(Cancelled)`: that is the
    /// cancel's main path once the drop has happened.
    CancelIncoming { drag: DragId },
}

/// Which thread a command is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Out,
    In,
    Both,
}

/// cj-dnd-out takes the catcher's commands, cj-dnd-in the shield's and the
/// drags from the Mac; both follow the config.
fn route(cmd: &DndCmd) -> Route {
    match cmd {
        DndCmd::CatcherZone(_) | DndCmd::Park(_) | DndCmd::Cancel { .. } | DndCmd::Home { .. } => {
            Route::Out
        }
        DndCmd::Shield(_) | DndCmd::StartIncoming { .. } | DndCmd::CancelIncoming { .. } => {
            Route::In
        }
        DndCmd::ApplyConfig(_) => Route::Both,
    }
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
    /// A target took the drop of a drag from the Mac and asks for its
    /// bytes: receive `item` into `dir` (as `Dest::InDir`) and answer on
    /// `reply` with where it landed. Sent from the COM thread serving the
    /// target's `GetData`, which waits for every item's reply.
    ///
    /// The contract for the app:
    /// - A `Fetch` can come before `IncomingEnded`, and `IncomingEnded` can
    ///   wait on its reply (Explorer's `Drop` asks for the bytes and holds
    ///   `DoDragDrop` until it has them). Never hold a `Fetch` back until
    ///   `IncomingEnded`: Explorer's `Drop` → our `GetData` → the reply →
    ///   `IncomingEnded` → `DoDragDrop` returns → Explorer's `Drop` is a
    ///   deadlock.
    /// - Every reply is answered or dropped, and promptly so when the
    ///   session ends: a target waits on it.
    /// - Once the drop has happened, `CancelIncoming` ends nothing the
    ///   target sees by itself: cancel by failing the replies with
    ///   `Err(Cancelled)` (the source side's own escape is a backstop).
    /// - Rarely, a `Fetch` comes and then `IncomingEnded { dropped: false }`:
    ///   a Chromium hover ask racing a DROP the target then refused. Its
    ///   reply is still awaited until answered, dropped or cancelled.
    Fetch {
        drag: DragId,
        item: u32,
        dir: PathBuf,
        reply: oneshot::Sender<Result<PathBuf, FailReason>>,
    },
    /// `DoDragDrop` for `drag` returned, or never ran. `dropped`: a target
    /// took it. Its fetches may still be running: they outlive this.
    IncomingEnded { drag: DragId, dropped: bool },
}

/// Shared between cj-dnd's threads, the input thread and the app.
#[derive(Debug, Default)]
pub struct DndShared {
    /// The drag the catcher holds; 0 = none.
    pub offer: AtomicU64,
    /// The physical left button, from Raw Input.
    pub left_down: AtomicBool,
    /// `WindowFromPoint(park point)` is the shield or the catcher (either
    /// thread's window), and no foreground window covers the edge monitor.
    /// Kept by the input thread while on the Mac (after parking, every
    /// 50 ms and on display changes), so the hook never asks Windows itself.
    pub under_park_is_ours: AtomicBool,
    /// The catcher covers the virtual screen.
    pub parked: AtomicBool,
    /// A session with the Mac is up. Without one there is no strip, no
    /// offer, no shield and no park. The app (Task 13) sets it on link up,
    /// before any crossing, and clears it on link down.
    pub session_up: AtomicBool,
    /// cj-dnd-out's window.
    catcher: AtomicIsize,
    /// cj-dnd-in's window.
    shield: AtomicIsize,
    /// The drag from the Mac whose `DoDragDrop` runs on cj-dnd-in; 0 = none.
    incoming: AtomicU64,
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
    /// False if cj-dnd is gone and `cmd` was dropped.
    pub fn send(&self, cmd: DndCmd) -> bool {
        let sent = self.cmds.send(cmd).is_ok();
        if !sent {
            debug!("dnd threads gone; command dropped");
        }
        sent
    }
}

/// The app's event handler. cj-dnd's threads and its relay call it, and so
/// do the COM threads serving a drop target's `GetData` (`Fetch`).
#[derive(Clone)]
struct Emit(Arc<Mutex<Handler>>);

type Handler = Box<dyn Fn(WinDndEv) + Send>;

impl Emit {
    fn new(f: Handler) -> Emit {
        Emit(Arc::new(Mutex::new(f)))
    }

    fn send(&self, ev: WinDndEv) {
        let f = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if std::panic::catch_unwind(AssertUnwindSafe(|| f(ev))).is_err() {
            error!("dnd: the event handler panicked");
        }
    }
}

/// A thread of cj-dnd as the relay sees it.
struct Lane {
    name: &'static str,
    tx: mpsc::Sender<DndCmd>,
    /// The window its `WM_APP` goes to.
    hwnd: isize,
}

impl Lane {
    fn send(&self, cmd: DndCmd) -> bool {
        if self.tx.send(cmd).is_err() {
            return false; // The thread is gone.
        }
        if let Err(e) = wake(self.hwnd) {
            // The next wake, or the next message, drains it.
            warn!("{}: wake failed: {e}", self.name);
        }
        true
    }

    /// Drops its sender and wakes it, so the thread sees it and stops.
    fn stop(self) {
        drop(self.tx);
        last_wake(self.name, self.hwnd);
    }
}

/// Start cj-dnd: cj-dnd-out, cj-dnd-in and the relay between them. The one
/// `Sender` takes every command; dropping every clone of it stops both
/// threads (cancelling any drag from the Mac still served) and closes
/// their windows.
///
/// `events` runs on either thread inside OLE's calls, on the relay, and
/// (for `Fetch`) on the COM thread serving a drop target: it must not block
/// (a drag source or target waits on it) and must not hold a clone of the
/// returned `Sender` (the threads would then never stop). A panic in it is
/// caught and logged.
pub fn spawn_dnd_thread(
    cfg: Config,
    shared: Arc<DndShared>,
    events: impl Fn(WinDndEv) + Send + 'static,
) -> Result<mpsc::Sender<DndCmd>> {
    let events = Emit::new(Box::new(events));
    let live = source::Live::default();
    let out = {
        let (cfg, shared, events) = (cfg.clone(), shared.clone(), events.clone());
        spawn_lane("cj-dnd-out", Role::Out, move || {
            let state = OutState::new(cfg, shared, events)?;
            let hwnd = state.catcher.hwnd.0 as isize;
            OUT.with(|cell| *cell.borrow_mut() = Some(state));
            Ok(hwnd)
        })?
    };
    let inc = {
        let (shared, events, live) = (shared.clone(), events.clone(), live.clone());
        spawn_lane("cj-dnd-in", Role::In, move || {
            inbound::start(cfg, shared, events, live)
        })
    };
    let inc = match inc {
        Ok(inc) => inc,
        Err(e) => {
            out.stop();
            return Err(e);
        }
    };
    let (cmd_tx, cmd_rx) = mpsc::channel::<DndCmd>();
    let lanes = Arc::new(Mutex::new(Some((out, inc))));
    let spawned = {
        let lanes = lanes.clone();
        std::thread::Builder::new()
            .name("cj-dnd-cmd".into())
            .spawn(move || {
                if let Some((out, inc)) =
                    lanes.lock().unwrap_or_else(PoisonError::into_inner).take()
                {
                    relay(cmd_rx, out, inc, shared, events, live);
                }
            })
    };
    if let Err(e) = spawned {
        // Not started: its lanes are still here to stop.
        if let Some((out, inc)) = lanes.lock().unwrap_or_else(PoisonError::into_inner).take() {
            out.stop();
            inc.stop();
        }
        return Err(anyhow!(e).context("spawn dnd command relay"));
    }
    Ok(cmd_tx)
}

/// Spawns one of cj-dnd's threads and waits for its window.
fn spawn_lane(
    name: &'static str,
    role: Role,
    start: impl FnOnce() -> Result<isize> + Send + 'static,
) -> Result<Lane> {
    let (tx, rx) = mpsc::channel::<DndCmd>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<isize>>(1);
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || run(name, role, rx, ready_tx, start))
        .with_context(|| format!("spawn {name}"))?;
    let hwnd = ready_rx
        .recv()
        .with_context(|| format!("{name} exited at start"))??;
    Ok(Lane { name, tx, hwnd })
}

/// Routes commands to their threads and wakes them; once every sender is
/// gone, stops both. It acts on two commands itself, since cj-dnd-in may be
/// held in a target's `Drop`, where its loop takes no `WM_APP`:
/// `CancelIncoming` (and the end, for every live drag from the Mac), and a
/// `StartIncoming` while another drag from the Mac runs, refused at once.
fn relay(
    from: mpsc::Receiver<DndCmd>,
    out: Lane,
    inc: Lane,
    shared: Arc<DndShared>,
    events: Emit,
    live: source::Live,
) {
    for cmd in from {
        match &cmd {
            DndCmd::CancelIncoming { drag } => {
                let reached = source::cancel_live(&live, Some(*drag));
                debug!(drag, reached, "dnd relay: CancelIncoming");
                // OLE asks QueryContinueDrag after each message its loop
                // takes: one more, now the flag is set, so a still mouse
                // can't hold the drag off.
                // SAFETY: plain FFI call; a window that is gone just makes it fail.
                let _ = unsafe {
                    PostMessageW(
                        Some(HWND(inc.hwnd as *mut _)),
                        WM_NULL,
                        WPARAM(0),
                        LPARAM(0),
                    )
                };
            }
            DndCmd::StartIncoming { drag, .. } => {
                let running = shared.incoming.load(Ordering::Acquire);
                if running != 0 {
                    warn!(
                        drag,
                        running, "dnd relay: a drag from the Mac runs; StartIncoming refused"
                    );
                    events.send(WinDndEv::IncomingEnded {
                        drag: *drag,
                        dropped: false,
                    });
                    continue;
                }
            }
            _ => {}
        }
        let start = started_drag(&cmd);
        let sent = match route(&cmd) {
            Route::Out => out.send(cmd),
            Route::In => inc.send(cmd),
            Route::Both => out.send(cmd.clone()) & inc.send(cmd),
        };
        if !sent {
            debug!("dnd relay: a thread is gone; command dropped");
            if let Some(drag) = start {
                // As promised: `IncomingEnded` follows every `StartIncoming`.
                error!(drag, "dnd relay: cj-dnd-in is gone; StartIncoming refused");
                events.send(WinDndEv::IncomingEnded {
                    drag,
                    dropped: false,
                });
            }
        }
    }
    source::cancel_live(&live, None);
    out.stop();
    inc.stop();
}

/// The drag a `StartIncoming` starts: if the command never reaches its
/// thread, the relay ends it itself.
fn started_drag(cmd: &DndCmd) -> Option<DragId> {
    match cmd {
        DndCmd::StartIncoming { drag, .. } => Some(*drag),
        _ => None,
    }
}

fn wake(hwnd: isize) -> windows::core::Result<()> {
    // SAFETY: plain FFI call; a window that is gone just makes it fail.
    unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), WM_APP, WPARAM(0), LPARAM(0)) }
}

/// The wake that lets a thread see the relay is gone: retried, so the
/// thread never waits for a message that isn't coming.
fn last_wake(name: &str, hwnd: isize) {
    let started = Instant::now();
    while let Err(e) = wake(hwnd) {
        if e.code() == ERROR_INVALID_WINDOW_HANDLE.to_hresult() {
            return; // The thread has already gone.
        }
        if started.elapsed() >= LAST_WAKE_LIMIT {
            error!("{name}: last wake failed for {LAST_WAKE_LIMIT:?}: {e}");
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

/// The monitor on the Mac's side.
fn edge_of(side: Side) -> Option<Rect> {
    edge_monitor(&monitors(), side)
}

/// Whether Windows sees the left button down (a read-only query).
pub(super) fn left_button_down() -> bool {
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

/// An offer the modes let go of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Released {
    drag: DragId,
    /// The app doesn't know yet: emit `OfferGone`.
    tell: bool,
}

/// The catcher's modes and the drag it holds, apart from the window and the
/// data (pure, so it can be tested).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct OutModes {
    zone: bool,
    parked: bool,
    /// While parked: refuse the drop (after `Cancel`).
    reject: bool,
    offer: Option<DragId>,
    /// A `Cancel` for the offer that came before its `Park(true)`: the
    /// park refuses it at once. (The app cancels a crossing whose offer it
    /// couldn't send, and the input thread parks for it: two senders.)
    refuse: Option<DragId>,
}

impl OutModes {
    fn catcher_at(&self) -> CatcherAt {
        if self.parked {
            CatcherAt::Screen
        } else if self.zone {
            CatcherAt::Strip
        } else {
            CatcherAt::Hidden
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
        self.refuse = None;
        self.offer.take().map(|drag| Released { drag, tell })
    }

    /// Refuse the drop of the drag ridden now, until the real release.
    fn refuse_drop(&mut self, drag: DragId) -> Option<Released> {
        info!(drag, "catcher: cancelled while riding; refusing the drop");
        self.reject = true;
        self.release(false)
    }

    /// The catcher's commands, as routed here; anything else is cj-dnd-in's.
    fn command(&mut self, cmd: &DndCmd, session_up: bool) -> Option<Released> {
        match *cmd {
            DndCmd::CatcherZone(on) => self.zone = on,
            DndCmd::Park(true) if !session_up => {
                debug!(?cmd, "dnd: refused without a session");
            }
            DndCmd::Park(true) => {
                self.parked = true;
                self.reject = false;
                if let Some(drag) = self.refuse.take().filter(|&d| self.offer == Some(d)) {
                    return self.refuse_drop(drag);
                }
            }
            DndCmd::Park(false) => self.unpark(),
            DndCmd::Cancel { drag } => {
                if self.offer != Some(drag) {
                    debug!(drag, "catcher: Cancel for a drag it doesn't hold");
                } else if self.parked {
                    return self.refuse_drop(drag);
                } else {
                    debug!(drag, "catcher: Cancel before the park; refused once parked");
                    self.refuse = Some(drag);
                }
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
            DndCmd::Shield(_)
            | DndCmd::ApplyConfig(_)
            | DndCmd::StartIncoming { .. }
            | DndCmd::CancelIncoming { .. } => {}
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

/// cj-dnd-out's state.
struct OutState {
    shared: Arc<DndShared>,
    events: Emit,
    side: Side,
    edge: Option<Rect>,
    catcher: Win,
    helper: Option<IDropTargetHelper>,
    modes: OutModes,
    /// The offer's virtual items, written out in `Drop`.
    pending: Vec<Pending>,
    /// Whether `IDropTargetHelper::Show(false)` is in force.
    image_hidden: bool,
    stuck_timer: bool,
    /// Since when the left button has been up with the catcher over the screen.
    up_since: Option<Instant>,
}

/// Which of cj-dnd's threads this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Out,
    In,
}

thread_local! {
    static ROLE: Cell<Option<Role>> = const { Cell::new(None) };
    static OUT: RefCell<Option<OutState>> = const { RefCell::new(None) };
    static CMDS: RefCell<Option<mpsc::Receiver<DndCmd>>> = const { RefCell::new(None) };
}

/// Run `f` on cj-dnd-out's state. `None` on another thread, or if the state
/// is already borrowed: a call made while it is busy gets default handling.
fn with_out<R>(f: impl FnOnce(&mut OutState) -> R) -> Option<R> {
    OUT.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

/// Run this thread's queued commands. While its state is busy (a modal
/// loop inside one of its calls) they wait for the next drain. Once every
/// sender is gone, the message loop is told to quit.
fn drain_commands() {
    loop {
        if OUT.with(|cell| cell.try_borrow_mut().is_err()) || inbound::busy() {
            return;
        }
        let next = CMDS.with(|cell| cell.borrow().as_ref().map(mpsc::Receiver::try_recv));
        match next {
            Some(Ok(cmd)) => match ROLE.get() {
                Some(Role::Out) => {
                    with_out(|s| s.on_command(cmd));
                }
                Some(Role::In) => inbound::on_command(cmd),
                None => {}
            },
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

/// One of cj-dnd's threads: OLE, its state (`start`, which reports its
/// window), its message loop, then the state's end.
fn run(
    name: &'static str,
    role: Role,
    cmds: mpsc::Receiver<DndCmd>,
    ready: mpsc::SyncSender<Result<isize>>,
    start: impl FnOnce() -> Result<isize>,
) {
    source::mark_dnd_thread();
    ROLE.set(Some(role));
    // SAFETY: OLE (an STA) on this thread for its whole life; paired below.
    if let Err(e) = unsafe { OleInitialize(None) } {
        let _ = ready.send(Err(anyhow!(e).context("OleInitialize")));
        return;
    }
    match start() {
        Ok(hwnd) => {
            CMDS.with(|cell| *cell.borrow_mut() = Some(cmds));
            if ready.send(Ok(hwnd)).is_ok() {
                message_loop();
            }
            CMDS.with(|cell| cell.borrow_mut().take());
            // Dropping a state revokes its target and destroys its window.
            OUT.with(|cell| cell.borrow_mut().take());
            inbound::stop();
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
    // SAFETY: pairs the OleInitialize above, after every COM object is gone.
    unsafe { OleUninitialize() };
    info!("{name}: stopped");
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

impl OutState {
    fn new(cfg: Config, shared: Arc<DndShared>, events: Emit) -> Result<OutState> {
        let hinst = register_class().context("register catcher window class")?;
        let catcher = create_window(hinst, w!("CursorJump catcher")).context("create catcher")?;
        // Built now, so `Drop` cleans up whatever fails below.
        let mut state = OutState {
            shared,
            events,
            side: cfg.mac_side,
            edge: edge_of(cfg.mac_side),
            catcher: Win::new(catcher),
            helper: None,
            modes: OutModes::default(),
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
        state
            .shared
            .catcher
            .store(catcher.0 as isize, Ordering::Release);
        info!(edge = ?state.edge, side = ?state.side, "cj-dnd-out: ready");
        Ok(state)
    }

    fn emit(&self, ev: WinDndEv) {
        self.events.send(ev);
    }

    fn session_up(&self) -> bool {
        self.shared.session_up.load(Ordering::Acquire)
    }

    fn on_command(&mut self, cmd: DndCmd) {
        debug!(?cmd, "cj-dnd-out: command");
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
        let edge = edge_of(self.side);
        if edge != self.edge {
            info!(?edge, side = ?self.side, "dnd: edge monitor changed");
        }
        self.edge = edge;
        self.catcher.stale = true;
    }

    /// Put the catcher where the modes say, and keep the drag image, the
    /// stuck check and cj-dnd-in's shield in step.
    fn apply(&mut self) {
        let catcher = match self.modes.catcher_at() {
            CatcherAt::Hidden => None,
            CatcherAt::Strip => self.edge.map(|e| strip_rect(e, self.side)),
            CatcherAt::Screen => Some(virtual_screen()),
        };
        self.catcher.place(catcher);
        let parked = self.modes.parked;
        if self.shared.parked.swap(parked, Ordering::AcqRel) != parked {
            self.tell_shield(parked);
        }
        self.set_image_hidden(self.modes.image_hidden());
        self.set_stuck_timer(parked);
    }

    /// The shield (cj-dnd-in) goes while the catcher covers the screen, and
    /// may come back after. Posted, never sent: cj-dnd-in may be held in a
    /// target's `Drop`. On parking it is hidden at once, without waiting
    /// for that thread, so a shield kept for a press never sits above the
    /// riding catcher when the input thread parks the cursor.
    fn tell_shield(&self, parked: bool) {
        let shield = HWND(self.shared.shield.load(Ordering::Acquire) as *mut _);
        if shield.0.is_null() {
            return;
        }
        // SAFETY: plain FFI calls on a window handle; a window that is gone
        // just makes them fail.
        unsafe {
            if parked {
                let _ = ShowWindowAsync(shield, SW_HIDE);
            }
            let _ = PostMessageW(Some(shield), WM_PARKED, WPARAM(0), LPARAM(0));
        }
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

impl Drop for OutState {
    fn drop(&mut self) {
        let released = self.modes.release(false);
        self.settle(released);
        self.shared.catcher.store(0, Ordering::Release);
        self.shared.parked.store(false, Ordering::Release);
        // SAFETY: undoes RegisterDragDrop on the same thread (harmless if it
        // never happened), then destroys our own window once.
        unsafe {
            let _ = RevokeDragDrop(self.catcher.hwnd);
            let _ = KillTimer(Some(self.catcher.hwnd), STUCK_TIMER);
            let _ = DestroyWindow(self.catcher.hwnd);
        }
    }
}

/// Test only: a message that holds its thread for `wparam` ms, as a
/// target's `Drop` holds cj-dnd-in.
#[cfg(test)]
const WM_TEST_HOLD: u32 = WM_APP + 9;

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
        WM_PARKED => {
            inbound::parked_changed();
            return LRESULT(0);
        }
        #[cfg(test)]
        WM_TEST_HOLD => {
            std::thread::sleep(Duration::from_millis(wparam.0 as u64));
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        // Presses passed through by the input hook land here: nothing to do.
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONUP
        | WM_RBUTTONDBLCLK | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MBUTTONDBLCLK | WM_XBUTTONDOWN
        | WM_XBUTTONUP | WM_XBUTTONDBLCLK => return LRESULT(0),
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_DPICHANGED => {
            // Each thread's window: whichever state this thread has.
            with_out(|s| {
                s.refresh_edge();
                s.apply();
            });
            inbound::display_changed();
        }
        WM_TIMER if wparam.0 == STUCK_TIMER => {
            with_out(OutState::check_stuck);
        }
        WM_TIMER if wparam.0 == KEEP_TIMER => inbound::keep_tick(),
        _ => {}
    }
    // SAFETY: default handling for our own window.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inbound::InModes;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

    /// Both threads' modes, each command routed as the relay routes it.
    #[derive(Debug, Default, Clone)]
    struct Modes {
        out: OutModes,
        inc: InModes,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Placement {
        catcher: CatcherAt,
        shield: bool,
    }

    impl Modes {
        fn command(&mut self, cmd: &DndCmd, session_up: bool) -> Option<Released> {
            let route = route(cmd);
            let released = (route != Route::In)
                .then(|| self.out.command(cmd, session_up))
                .flatten();
            if route != Route::Out {
                // cj-dnd-in reads the park from `DndShared::parked`.
                self.inc.command(cmd, session_up, self.out.parked);
            }
            released
        }

        fn placement(&self) -> Placement {
            Placement {
                catcher: self.out.catcher_at(),
                shield: self.inc.shield(self.out.parked),
            }
        }

        fn image_hidden(&self) -> bool {
            self.out.image_hidden()
        }
        fn enter(&mut self, fresh: Option<DragId>) -> (bool, Option<Released>) {
            self.out.enter(fresh)
        }
        fn over(&self) -> bool {
            self.out.over()
        }
        fn leave(&mut self) -> Option<Released> {
            self.out.leave()
        }
        fn takes_drop(&self) -> bool {
            self.out.takes_drop()
        }
        fn take_drop(&mut self) -> Result<DragId, Option<Released>> {
            self.out.take_drop()
        }
        fn stuck(&mut self) -> Option<Released> {
            self.out.stuck()
        }
        fn begin_incoming(&mut self, drag: DragId) -> bool {
            self.inc.begin_incoming(drag)
        }
        fn let_go_incoming(&mut self, drag: DragId) {
            self.inc.let_go_incoming(drag)
        }
        fn end_incoming(&mut self, drag: DragId) {
            self.inc.end_incoming(drag)
        }
        fn release_kept(&mut self) {
            self.inc.release_kept()
        }
    }

    #[test]
    fn each_command_goes_to_its_thread() {
        for c in [
            DndCmd::CatcherZone(true),
            DndCmd::Park(true),
            DndCmd::Cancel { drag: 1 },
            DndCmd::Home { drag: 1 },
        ] {
            assert_eq!(route(&c), Route::Out, "{c:?}");
        }
        for c in [
            DndCmd::Shield(true),
            DndCmd::StartIncoming {
                drag: 1,
                items: vec![],
            },
            DndCmd::CancelIncoming { drag: 1 },
        ] {
            assert_eq!(route(&c), Route::In, "{c:?}");
        }
        assert_eq!(route(&DndCmd::ApplyConfig(Config::default())), Route::Both);
    }

    #[test]
    fn only_a_start_is_ended_by_the_relay_when_unsent() {
        let start = DndCmd::StartIncoming {
            drag: 4,
            items: vec![],
        };
        assert_eq!(started_drag(&start), Some(4));
        assert_eq!(started_drag(&DndCmd::CancelIncoming { drag: 4 }), None);
        assert_eq!(started_drag(&DndCmd::Shield(true)), None);
    }

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

    fn shield_shown(m: &Modes) -> bool {
        m.placement().shield
    }

    #[test]
    fn shield_hide_waits_for_an_incoming_drag_that_started_first() {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::Shield(true));
        assert!(m.begin_incoming(5));
        // The input thread's hide on the way home: DoDragDrop holds the press.
        cmd(&mut m, DndCmd::Shield(false));
        assert!(shield_shown(&m));
        m.release_kept();
        assert!(shield_shown(&m), "not while the drag runs");
        m.end_incoming(5);
        assert!(!shield_shown(&m));
    }

    #[test]
    fn shield_goes_once_dodragdrop_has_the_mouse() {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::Shield(true));
        assert!(m.begin_incoming(5));
        cmd(&mut m, DndCmd::Shield(false));
        // At the first QueryContinueDrag OLE holds its own capture; and
        // Explorer's Drop may hold DoDragDrop for the whole fetch after.
        m.let_go_incoming(6);
        assert!(shield_shown(&m), "another drag's start");
        m.let_go_incoming(5);
        assert!(!shield_shown(&m));
        // Still one drag at a time, and no keeping on its behalf.
        assert!(!m.begin_incoming(7));
        cmd(&mut m, DndCmd::Shield(true));
        m.inc.held = true;
        cmd(&mut m, DndCmd::Shield(false));
        assert!(!shield_shown(&m));
        m.end_incoming(5);
        assert!(m.begin_incoming(7));
    }

    #[test]
    fn shield_hide_before_the_start_is_kept_while_windows_sees_the_press() {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::Shield(true));
        m.inc.held = true;
        cmd(&mut m, DndCmd::Shield(false));
        assert!(shield_shown(&m), "a StartIncoming may be on its way");
        assert!(m.begin_incoming(5));
        m.inc.held = false;
        m.release_kept();
        assert!(shield_shown(&m));
        m.end_incoming(5);
        assert!(!shield_shown(&m));
    }

    #[test]
    fn kept_shield_goes_once_the_button_is_up() {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::Shield(true));
        m.inc.held = true;
        cmd(&mut m, DndCmd::Shield(false));
        assert!(shield_shown(&m));
        m.inc.held = false;
        m.release_kept();
        assert!(!shield_shown(&m));
    }

    #[test]
    fn shield_hide_with_nothing_pending_is_at_once() {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::Shield(true));
        cmd(&mut m, DndCmd::Shield(false));
        assert!(!shield_shown(&m));
        // A shield that wasn't shown is never kept.
        m.inc.held = true;
        cmd(&mut m, DndCmd::Shield(false));
        assert!(!shield_shown(&m));
        assert!(m.begin_incoming(5));
        cmd(&mut m, DndCmd::Shield(false));
        assert!(!shield_shown(&m));
    }

    #[test]
    fn a_new_shield_makes_the_kept_hide_moot() {
        let mut m = Modes::default();
        cmd(&mut m, DndCmd::Shield(true));
        assert!(m.begin_incoming(5));
        cmd(&mut m, DndCmd::Shield(false));
        // Back on the Mac before the drag ended: the shield stays.
        cmd(&mut m, DndCmd::Shield(true));
        m.end_incoming(5);
        assert!(shield_shown(&m));
    }

    #[test]
    fn one_incoming_drag_at_a_time() {
        let mut m = Modes::default();
        assert!(m.begin_incoming(5));
        assert!(!m.begin_incoming(6));
        // Another drag's end changes nothing.
        m.end_incoming(6);
        assert_eq!(m.inc.incoming, Some(5));
        m.end_incoming(5);
        assert_eq!(m.inc.incoming, None);
        assert!(m.begin_incoming(6));
    }

    #[test]
    fn modes_refuse_shield_and_park_without_a_session() {
        let mut m = Modes::default();
        m.command(&DndCmd::Park(true), false);
        m.command(&DndCmd::Shield(true), false);
        m.command(&DndCmd::CatcherZone(true), false);
        assert_eq!(m.placement(), place(CatcherAt::Strip, false));
        assert!(!m.out.parked);
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
        assert_eq!(m.out.offer, None);
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
        assert!(!m.out.parked);
    }

    #[test]
    fn modes_cancel_refuses_until_the_release() {
        let mut m = Modes::default();
        m.enter(Some(7));
        // Another drag: not its business.
        assert_eq!(cmd(&mut m, DndCmd::Cancel { drag: 9 }), None);
        cmd(&mut m, DndCmd::Park(true));
        assert!(m.over(), "a Cancel for another drag refuses nothing");
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
        assert!(m.out.parked);
        // The real release: no drop, back off the screen.
        assert_eq!(m.take_drop(), Err(None));
        assert!(!m.out.parked && !m.out.reject);
    }

    #[test]
    fn modes_cancel_before_the_park_refuses_once_parked() {
        let mut m = Modes::default();
        m.enter(Some(7));
        assert_eq!(cmd(&mut m, DndCmd::Cancel { drag: 7 }), None);
        assert_eq!(m.out.offer, Some(7), "still held until the park");
        assert_eq!(
            cmd(&mut m, DndCmd::Park(true)),
            Some(Released {
                drag: 7,
                tell: false
            })
        );
        assert_eq!(m.placement().catcher, CatcherAt::Screen);
        assert!(!m.over() && !m.takes_drop());
        assert_eq!(m.take_drop(), Err(None), "the release drops nothing");

        // Let go of before any park: a later offer's park is not refused,
        // even one under the same id.
        for later in [7, 8] {
            let mut m = Modes::default();
            m.enter(Some(7));
            cmd(&mut m, DndCmd::Cancel { drag: 7 });
            m.leave();
            m.enter(Some(later));
            assert_eq!(cmd(&mut m, DndCmd::Park(true)), None);
            assert!(m.takes_drop());
        }
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
        assert!(!m.out.parked);
        assert_eq!(m.out.offer, None);
        // Home for another drag still unparks, and keeps the offer.
        m.enter(Some(8));
        cmd(&mut m, DndCmd::Park(true));
        assert_eq!(cmd(&mut m, DndCmd::Home { drag: 7 }), None);
        assert!(!m.out.parked);
        assert_eq!(m.out.offer, Some(8));
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
                EntryStep::AwaitParked => assert!(m.out.parked, "awaited before Park(true)"),
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
        assert_eq!(shared.shield.load(Ordering::Acquire), 0);
    }

    fn visible(h: isize) -> bool {
        // SAFETY: plain FFI call on a window handle.
        unsafe { IsWindowVisible(HWND(h as *mut _)) }.as_bool()
    }

    fn until(what: &str, within: Duration, done: impl Fn() -> bool) -> Duration {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < within, "{what} not within {within:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
        started.elapsed()
    }

    #[test]
    fn cj_dnd_out_takes_commands_while_cj_dnd_in_is_held() {
        // R39: cj-dnd-in held as a target's synchronous Drop holds it (its
        // loop takes nothing), cj-dnd-out goes on.
        let shared = Arc::new(DndShared::default());
        shared.session_up.store(true, Ordering::Release);
        let cmds = spawn_dnd_thread(Config::default(), shared.clone(), |_| {}).expect("spawn");
        let (catcher, shield) = (
            shared.catcher.load(Ordering::Acquire),
            shared.shield.load(Ordering::Acquire),
        );
        assert!(catcher != 0 && shield != 0 && catcher != shield);
        if edge_monitor(&monitors(), Config::default().mac_side).is_none() {
            return; // Nowhere to show either window.
        }
        const HOLD: Duration = Duration::from_secs(2);
        // SAFETY: plain FFI call on cj-dnd-in's own window.
        unsafe {
            PostMessageW(
                Some(HWND(shield as *mut _)),
                WM_TEST_HOLD,
                WPARAM(HOLD.as_millis() as usize),
                LPARAM(0),
            )
        }
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let held = Instant::now();
        cmds.send(DndCmd::Shield(true)).unwrap();
        cmds.send(DndCmd::CatcherZone(true)).unwrap();
        let took = until("the strip", Duration::from_secs(1), || visible(catcher));
        assert!(took < HOLD / 2, "{took:?}");
        assert!(!visible(shield), "cj-dnd-in is really held");
        // Once let go, cj-dnd-in catches up.
        until("the shield", HOLD * 2, || visible(shield));
        assert!(held.elapsed() >= HOLD - Duration::from_millis(100));
        cmds.send(DndCmd::CatcherZone(false)).unwrap();
        cmds.send(DndCmd::Shield(false)).unwrap();
        until("both hidden", Duration::from_secs(2), || {
            !visible(catcher) && !visible(shield)
        });
    }

    #[test]
    fn a_drag_from_the_mac_while_one_runs_is_refused_at_once() {
        let (tx, rx) = mpsc::channel::<WinDndEv>();
        let shared = Arc::new(DndShared::default());
        let cmds = spawn_dnd_thread(Config::default(), shared.clone(), move |ev| {
            let _ = tx.send(ev);
        })
        .expect("spawn");
        // As if a target's Drop held cj-dnd-in in a drag from the Mac.
        shared.incoming.store(30, Ordering::Release);
        let shield = shared.shield.load(Ordering::Acquire);
        // SAFETY: plain FFI call on cj-dnd-in's own window.
        unsafe {
            PostMessageW(
                Some(HWND(shield as *mut _)),
                WM_TEST_HOLD,
                WPARAM(3000),
                LPARAM(0),
            )
        }
        .unwrap();
        let sent = Instant::now();
        cmds.send(DndCmd::StartIncoming {
            drag: 31,
            items: vec![],
        })
        .unwrap();
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(WinDndEv::IncomingEnded { drag, dropped }) => {
                assert_eq!((drag, dropped), (31, false));
            }
            other => panic!("{other:?}"),
        }
        assert!(sent.elapsed() < Duration::from_secs(1));
        shared.incoming.store(0, Ordering::Release);
        drop(cmds);
    }

    #[test]
    fn a_cancel_right_after_the_start_ends_the_drag_without_a_drop() {
        // Through the real relay and cj-dnd-in, back to back. Without a
        // session DoDragDrop never runs here, whatever the mouse does (a
        // real drag from a test could drop onto the desktop), so this pins
        // the order and the outcome; `cj_dnd_in_cancels_a_drag_the_relay_missed`
        // pins that the cancel reaches a drag registered after the relay.
        let (tx, rx) = mpsc::channel::<WinDndEv>();
        let shared = Arc::new(DndShared::default());
        let cmds = spawn_dnd_thread(Config::default(), shared, move |ev| {
            let _ = tx.send(ev);
        })
        .expect("spawn");
        let items = vec![DragItem {
            name: "a.txt".into(),
            kind: cj_core::dnd::ItemKind::File,
            size: Some(1),
        }];
        cmds.send(DndCmd::StartIncoming { drag: 40, items })
            .unwrap();
        cmds.send(DndCmd::CancelIncoming { drag: 40 }).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(WinDndEv::IncomingEnded { drag, dropped }) => {
                assert_eq!((drag, dropped), (40, false));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "nothing else, no fetch"
        );
    }

    #[test]
    fn start_incoming_without_a_session_ends_at_once() {
        // No session, so `DoDragDrop` never runs, whatever the mouse does.
        let (tx, rx) = mpsc::channel::<WinDndEv>();
        let shared = Arc::new(DndShared::default());
        let cmds = spawn_dnd_thread(Config::default(), shared, move |ev| {
            let _ = tx.send(ev);
        })
        .expect("spawn");
        let items = vec![DragItem {
            name: "a.txt".into(),
            kind: cj_core::dnd::ItemKind::File,
            size: Some(1),
        }];
        for drag in [21, 22] {
            cmds.send(DndCmd::StartIncoming {
                drag,
                items: items.clone(),
            })
            .expect("send");
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(WinDndEv::IncomingEnded { drag: d, dropped }) => {
                    assert_eq!((d, dropped), (drag, false));
                }
                other => panic!("{other:?}"),
            }
        }
        // Not running: nothing to cancel, nothing said.
        cmds.send(DndCmd::CancelIncoming { drag: 22 })
            .expect("send");
        drop(cmds);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).map(|_| ()),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
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
                other => eprintln!("smoke: {other:?}"),
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
