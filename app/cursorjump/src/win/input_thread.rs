//! The dedicated input thread.
//!
//! On the PC it only watches Raw Input (no hooks) and reports pushes against
//! the edge that leads to the Mac. Between `EnterMac` and `ReturnToPc` it owns
//! low-level mouse and keyboard hooks that swallow local input and forward it.
//! The one exception: real left presses and releases also reach Windows while
//! one of CursorJump's own drag-and-drop windows lies under the park point
//! (see `hook_verdict`), so a drag can end on, or start from, that window.
//!
//! All state lives in a thread-local: the hook procedures and the window
//! procedure run on this same thread, so no locks are needed.
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cj_core::config::Config;
use cj_core::dnd::DragId;
use cj_core::gaming::game_locked;
use cj_core::input::{MotionScaler, WheelAxis};
use cj_core::keymap::{KeyRouter, PcKey, Route, is_return_hotkey, pc_to_mac};
use cj_core::layout::{Rect, edge_frac, is_pushing_out, point_on_edge};
use cj_core::pc_state::PcEvent;
use cj_core::proto::{MouseButton, Side};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, ERROR_INVALID_THREAD_ID, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    POINT, RECT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW,
    OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
    VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW, GetShellWindow, GetWindowRect,
    HHOOK, KBDLLHOOKSTRUCT, KillTimer, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, PostThreadMessageW, RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP,
    RegisterClassExW, SetCursorPos, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WM_APP, WM_DISPLAYCHANGE, WM_INPUT, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_SYSKEYDOWN, WM_TIMER,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_POPUP, XBUTTON1, XBUTTON2,
};
use windows::core::w;

use super::cursor::{hide_system_cursors, restore_system_cursors};
use super::dnd::{DndCmd, DndPort, EntryStep, entry_steps, in_catch_zone, ours_under};
use super::monitors::{edge_monitor, monitors, rect_from};
use super::probe::probe_game;

/// Raw motion without hook motion for this long while on the Mac means the hook is gone.
const WATCHDOG: Duration = Duration::from_millis(250);
/// After an `EdgePush`, further pushes are suppressed until a command arrives or this passes.
const EDGE_DEBOUNCE: Duration = Duration::from_millis(250);

/// After a return to the PC, the cursor must get this far inside the Mac edge
/// (physical pixels, along the crossing axis) before the edge crosses again.
const REARM_DISTANCE_PX: i32 = 8;

/// After a failed `EnterMac`, edge pushes are suppressed for this long.
const ENTER_FAILED_DEBOUNCE: Duration = Duration::from_secs(1);
/// A failed wake-up is retried every `WAKE_RETRY_STEP` for up to `WAKE_RETRY_LIMIT`.
const WAKE_RETRY_STEP: Duration = Duration::from_millis(10);
const WAKE_RETRY_LIMIT: Duration = Duration::from_secs(1);
/// A single hook move larger than this on either axis is a warp, not motion: dropped.
const MAX_HOOK_DELTA: i32 = 200;

/// While on the Mac, how often the input desktop is checked (`DESKTOP_TIMER`).
const DESKTOP_POLL: Duration = Duration::from_millis(150);
/// The input window's timer id for the desktop check.
const DESKTOP_TIMER: usize = 1;
/// While on the Mac, how often the window under the park point is checked (`PARK_TIMER`).
const PARK_POLL: Duration = Duration::from_millis(50);
/// The input window's timer id for the park-point check.
const PARK_TIMER: usize = 2;
/// While the shield is wanted but our window isn't under the park point, it
/// is asked for again this often (a `Shield(true)` refused for want of a
/// session, or a window that went missing).
const SHIELD_REASSERT: Duration = Duration::from_secs(1);
/// Longest wait for the catcher to cover the screen before parking with a drag.
const PARK_WAIT: Duration = Duration::from_millis(100);
/// While the strip's other conditions hold, a game check is reused this long.
const ZONE_PROBE_EVERY: Duration = Duration::from_millis(100);

/// When set (diagnose mode), log every raw key event and the entry key snapshot.
static DIAGNOSE_LOG: AtomicBool = AtomicBool::new(false);

pub fn set_diagnose_logging(on: bool) {
    DIAGNOSE_LOG.store(on, Ordering::Relaxed);
}

/// Input thread → app.
#[derive(Debug, Clone)]
pub enum InputMsg {
    /// Only `PcEvent::EdgePush`, `PcEvent::ReturnHotkey` and `PcEvent::HookLost`.
    Edge(PcEvent),
    Motion {
        dx: i16,
        dy: i16,
    },
    /// Pixels. `dy > 0` = wheel away from the user (scroll up); `dx > 0` = scroll right.
    Scroll {
        dx: i32,
        dy: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Key {
        mac_keycode: u16,
        down: bool,
    },
}

/// App → input thread.
#[derive(Debug, Clone)]
pub enum InputCmd {
    /// `drag`: the PC's file drag rides along (the catcher then covers the
    /// screen before the cursor is parked).
    EnterMac {
        drag: Option<DragId>,
    },
    ReturnToPc {
        pc_frac: f32,
    },
    ApplyConfig(Config),
    Quit,
}

/// Start the input thread. Commands sent on the returned `Sender` are relayed
/// to the thread and it is woken with `PostThreadMessageW(WM_APP)`. Dropping
/// every `Sender` stops the thread as if `Quit` had been sent.
///
/// `dnd`: the drag-and-drop thread, for the edge catcher and the shield;
/// `None` keeps phase 1's behaviour (Task 13 passes it).
pub fn spawn_input_thread(
    cfg: Config,
    dnd: Option<DndPort>,
) -> (
    mpsc::Sender<InputCmd>,
    crossbeam_channel::Receiver<InputMsg>,
) {
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, cmd_rx) = mpsc::channel::<InputCmd>();
    let (relay_tx, relay_rx) = mpsc::channel::<InputCmd>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<u32>(1);
    // Cleared by the input thread when it drops `relay_rx`; std's Sender cannot
    // report disconnection on its own, and the wake retry needs to know.
    let receiver_alive = Arc::new(AtomicBool::new(true));
    let alive = receiver_alive.clone();

    std::thread::Builder::new()
        .name("cj-input".into())
        .spawn(move || run(cfg, dnd, msg_tx, relay_rx, alive, ready_tx))
        .expect("spawn input thread");

    // The thread reports its id once its window (and so its message queue)
    // exists; only then can PostThreadMessageW reach it.
    match ready_rx.recv() {
        Ok(tid) => {
            std::thread::Builder::new()
                .name("cj-input-cmd".into())
                .spawn(move || relay_commands(cmd_rx, relay_tx, &receiver_alive, tid))
                .expect("spawn input command relay");
        }
        Err(_) => error!("input thread failed to start"),
    }
    (cmd_tx, msg_rx)
}

/// Forwards commands and wakes the input thread, retrying a failed wake so a
/// command such as `ReturnToPc` is never stranded in the queue.
fn relay_commands(
    from: mpsc::Receiver<InputCmd>,
    to: mpsc::Sender<InputCmd>,
    receiver_alive: &AtomicBool,
    tid: u32,
) {
    let wake = || {
        let post = || unsafe { PostThreadMessageW(tid, WM_APP, WPARAM(0), LPARAM(0)) };
        wake_with_retry(post, receiver_alive, WAKE_RETRY_STEP, WAKE_RETRY_LIMIT)
    };
    for cmd in from {
        if to.send(cmd).is_err() {
            return; // The input thread is gone.
        }
        wake();
    }
    // Skip the wake if the input thread has already gone (it drops its receiver first).
    if to.send(InputCmd::Quit).is_ok() {
        wake();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WakeOutcome {
    Woken,
    ThreadGone,
    ReceiverGone,
    GaveUp,
}

/// Call `post` until it succeeds, retrying every `step` for up to `limit`.
/// Stops early when the thread id is invalid or the receiver has been dropped.
fn wake_with_retry(
    mut post: impl FnMut() -> windows::core::Result<()>,
    receiver_alive: &AtomicBool,
    step: Duration,
    limit: Duration,
) -> WakeOutcome {
    let started = Instant::now();
    let mut retried = false;
    loop {
        let e = match post() {
            Ok(()) => {
                if retried {
                    info!("input thread wake succeeded after retrying");
                }
                return WakeOutcome::Woken;
            }
            Err(e) => e,
        };
        if e.code() == ERROR_INVALID_THREAD_ID.to_hresult() {
            debug!("input thread gone; not waking it");
            return WakeOutcome::ThreadGone;
        }
        if !receiver_alive.load(Ordering::Acquire) {
            return WakeOutcome::ReceiverGone;
        }
        if started.elapsed() >= limit {
            error!("input thread wake failed for {limit:?}; giving up: {e}");
            return WakeOutcome::GaveUp;
        }
        if !retried {
            warn!("input thread wake failed, retrying: {e}");
            retried = true;
        }
        std::thread::sleep(step);
    }
}

struct Hooks {
    mouse: HHOOK,
    keyboard: HHOOK,
}

/// Present only while the cursor is on the Mac.
struct MacStay {
    hooks: Hooks,
    /// Where the cursor is parked (read back after SetCursorPos).
    center: (i32, i32),
    entered_at: Instant,
    last_hook_motion: Instant,
    hook_lost_sent: bool,
    /// The last real left press was passed to Windows, so its release must be too.
    left_passed: bool,
    /// The left button was down when the cursor crossed (a riding drag): its
    /// release may end the drag on the catcher.
    left_held_at_entry: bool,
    /// The foreground window at the last park check: a pass-through needs
    /// it unchanged (R31).
    park_fg: isize,
    /// What the shield was last told, and when.
    shield_shown: bool,
    shield_sent_at: Option<Instant>,
}

impl MacStay {
    fn new(hooks: Hooks, center: (i32, i32), now: Instant) -> Self {
        MacStay {
            hooks,
            center,
            entered_at: now,
            last_hook_motion: now,
            hook_lost_sent: false,
            left_passed: false,
            left_held_at_entry: false,
            park_fg: 0,
            shield_shown: false,
            shield_sent_at: None,
        }
    }

    /// Watchdog decision for one raw-input packet: true (once per stay) when raw
    /// input moved but the mouse hook has seen nothing for `WATCHDOG`, outside
    /// the first `WATCHDOG` after entry.
    fn watchdog_trips(&mut self, moved: bool, now: Instant) -> bool {
        let trips = moved
            && !self.hook_lost_sent
            && now.saturating_duration_since(self.entered_at) > WATCHDOG
            && now.saturating_duration_since(self.last_hook_motion) > WATCHDOG;
        self.hook_lost_sent |= trips;
        trips
    }
}

/// Stops an instant re-cross after a return while the user keeps pushing at the
/// edge: a return disarms it, moving `REARM_DISTANCE_PX` inward re-arms it.
struct RecrossGuard {
    armed: bool,
    logged: bool,
}

impl RecrossGuard {
    fn new() -> Self {
        RecrossGuard {
            armed: true,
            logged: false,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.logged = false;
    }

    /// Feed a cursor position; returns whether the edge is armed now.
    fn observe(&mut self, edge: &Rect, side: Side, x: i32, y: i32) -> bool {
        if !self.armed {
            let inward = match side {
                Side::Right => edge.right - 1 - x,
                Side::Left => x - edge.left,
                Side::Top => y - edge.top,
                Side::Bottom => edge.bottom - 1 - y,
            };
            self.armed = inward >= REARM_DISTANCE_PX;
        }
        self.armed
    }

    /// True the first time it is called after each `disarm`.
    fn first_swallow(&mut self) -> bool {
        !std::mem::replace(&mut self.logged, true)
    }
}

struct State {
    /// The input thread's hidden window (owns the desktop-check timer).
    hwnd: HWND,
    cfg: Config,
    out: crossbeam_channel::Sender<InputMsg>,
    edge: Option<Rect>,
    mac: Option<MacStay>,
    /// Debounce after an `EdgePush`; cleared by any command.
    edge_quiet_until: Option<Instant>,
    /// Debounce after a failed `EnterMac`; not cleared by commands.
    enter_failed_until: Option<Instant>,
    recross: RecrossGuard,
    router: KeyRouter,
    /// Physical keys currently held, for the return hotkey.
    held: HashSet<PcKey>,
    /// Entry keys seeded only as the other form of an ambiguous key; forgotten
    /// together with their partner so the key's first press on the Mac is kept.
    entry_twins: HashSet<PcKey>,
    /// Motion and wheel rounding remainders; reset at each entry.
    scaler: MotionScaler,
    /// The drag-and-drop thread; `None` until Task 13 wires it.
    dnd: Option<DndPort>,
    /// The last `CatcherZone` sent.
    zone: bool,
    /// The last game check made for the strip, and when.
    zone_game: Option<(Instant, bool)>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Run `f` on the thread state. `None` if it is absent or already borrowed
/// (a re-entrant callback), in which case callers fall back to pass-through.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

fn run(
    cfg: Config,
    dnd: Option<DndPort>,
    out: crossbeam_channel::Sender<InputMsg>,
    cmds: mpsc::Receiver<InputCmd>,
    receiver_alive: Arc<AtomicBool>,
    ready: mpsc::SyncSender<u32>,
) {
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            error!("input thread: window creation failed: {e}");
            return;
        }
    };
    if let Err(e) = register_raw_mouse(hwnd) {
        error!("input thread: raw input registration failed: {e}");
        let _ = unsafe { DestroyWindow(hwnd) };
        return;
    }
    let edge = edge_monitor(&monitors(), cfg.mac_side);
    info!(?edge, side = ?cfg.mac_side, "input thread: edge monitor");
    let mut state = State::new(cfg, out, edge);
    state.hwnd = hwnd;
    state.dnd = dnd;
    STATE.with(|cell| *cell.borrow_mut() = Some(state));

    if ready.send(unsafe { GetCurrentThreadId() }).is_ok() {
        message_loop(&cmds);
    }

    with_state(State::shutdown);
    let _ = unsafe { DestroyWindow(hwnd) };
    // Close the command channel before the message channel, so the relay never
    // tries to wake a thread the app already knows has stopped.
    receiver_alive.store(false, Ordering::Release);
    drop(cmds);
    info!("input thread: stopped");
    // Dropping the state drops the message sender, which tells the app we are done.
    STATE.with(|cell| cell.borrow_mut().take());
}

fn message_loop(cmds: &mpsc::Receiver<InputCmd>) {
    let mut msg = MSG::default();
    loop {
        // 0 = WM_QUIT, -1 = error.
        if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
            return;
        }
        if msg.hwnd.is_invalid() && msg.message == WM_APP {
            loop {
                match cmds.try_recv() {
                    Ok(InputCmd::Quit) | Err(mpsc::TryRecvError::Disconnected) => return,
                    Ok(cmd) => {
                        with_state(|s| s.on_command(cmd));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            continue;
        }
        unsafe { DispatchMessageW(&msg) };
    }
}

impl State {
    fn new(cfg: Config, out: crossbeam_channel::Sender<InputMsg>, edge: Option<Rect>) -> Self {
        State {
            hwnd: HWND::default(),
            cfg,
            out,
            edge,
            mac: None,
            edge_quiet_until: None,
            enter_failed_until: None,
            recross: RecrossGuard::new(),
            router: KeyRouter::new(),
            held: HashSet::with_capacity(32),
            entry_twins: HashSet::with_capacity(8),
            scaler: MotionScaler::default(),
            dnd: None,
            zone: false,
            zone_game: None,
        }
    }

    fn send_dnd(&self, cmd: DndCmd) {
        if let Some(dnd) = &self.dnd {
            dnd.send(cmd);
        }
    }

    fn send(&self, msg: InputMsg) {
        let _ = self.out.send(msg);
    }

    fn on_command(&mut self, cmd: InputCmd) {
        self.edge_quiet_until = None;
        match cmd {
            InputCmd::EnterMac { drag } => self.enter_mac(drag),
            InputCmd::ReturnToPc { pc_frac } => self.return_to_pc(pc_frac),
            InputCmd::ApplyConfig(cfg) => {
                self.send_dnd(DndCmd::ApplyConfig(cfg.clone()));
                self.cfg = cfg;
                self.refresh_monitors();
            }
            InputCmd::Quit => {}
        }
    }

    fn refresh_monitors(&mut self) {
        let edge = edge_monitor(&monitors(), self.cfg.mac_side);
        if edge != self.edge {
            info!(?edge, side = ?self.cfg.mac_side, "input thread: edge monitor changed");
        }
        self.edge = edge;
        if let (Some(stay), Some(edge)) = (&mut self.mac, edge) {
            stay.center = park_and_read_back(rect_center(&edge));
        }
        self.check_park();
    }

    /// For the mouse hook, which must not ask Windows itself: whether one of
    /// our drag-and-drop windows lies under the park point, and the
    /// foreground window as of now. No shield while the foreground window
    /// covers the edge monitor (a fullscreen game or video): a topmost
    /// window over it is what anti-cheat heuristics look for (R32).
    fn check_park(&mut self) {
        let (Some(stay), Some(dnd), Some(edge)) = (&mut self.mac, &self.dnd, self.edge) else {
            return;
        };
        let (fg, covered) = foreground_covering(&edge);
        stay.park_fg = fg;
        let want = !covered && dnd.shared.session_up.load(Ordering::Acquire);
        let ours = !covered && ours_under(&dnd.shared, stay.center);
        let now = Instant::now();
        // cj-dnd refuses `Shield(true)` without a session (Task 13 sets
        // `session_up` before any crossing), so a wanted shield that isn't
        // there is asked for again now and then.
        let missing = want
            && !ours
            && stay
                .shield_sent_at
                .is_none_or(|at| now.saturating_duration_since(at) >= SHIELD_REASSERT);
        if want != stay.shield_shown || missing {
            if want != stay.shield_shown {
                info!(shield = want, "park point: shield");
            }
            stay.shield_shown = want;
            stay.shield_sent_at = Some(now);
            dnd.send(DndCmd::Shield(want));
        }
        let was = dnd.shared.under_park_is_ours.swap(ours, Ordering::AcqRel);
        if ours != was {
            debug!(ours, "park point: our window under it changed");
        }
    }

    /// After `Park(true)`: wait (bounded) until the catcher covers the screen.
    fn await_parked(&self) {
        let Some(dnd) = &self.dnd else {
            return;
        };
        let until = Instant::now() + PARK_WAIT;
        while !dnd.shared.parked.load(Ordering::Acquire) {
            if Instant::now() >= until {
                warn!("catcher did not cover the screen in time; parking anyway");
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn enter_mac(&mut self, drag: Option<DragId>) {
        if self.mac.is_some() {
            debug!("EnterMac ignored: already on the Mac");
            return;
        }
        let Some(edge) = self.edge else {
            warn!("EnterMac refused: no edge monitor");
            self.enter_failed();
            return;
        };
        let snapshot = keys_down_now();
        if DIAGNOSE_LOG.load(Ordering::Relaxed) {
            info!(keys = ?snapshot.keys, twins = ?snapshot.twins, "entry snapshot: keys held");
        }
        self.seed_entry_keys(&snapshot);

        // Hide first and park before the hooks exist so the cursor never flashes
        // or jumps; park again in case it moved while the hooks were installed.
        let left_held_at_entry = is_down(VK_LBUTTON.0);
        let before = cursor_pos();
        hide_system_cursors();
        let target = rect_center(&edge);
        // With a drag, the catcher covers the screen before the strip goes
        // or the cursor moves (`entry_steps`).
        let zone_shown = std::mem::take(&mut self.zone);
        for step in entry_steps(drag.is_some(), zone_shown) {
            match step {
                EntryStep::Send(cmd) => self.send_dnd(cmd),
                EntryStep::AwaitParked => self.await_parked(),
                EntryStep::ParkCursor => park_cursor(target),
            }
        }
        let hooks = match install_hooks() {
            Ok(h) => h,
            Err(e) => {
                error!("hooks: install failed: {e}");
                // Undo the park, then show the cursors again.
                if let Some(pt) = before {
                    park_cursor(pt);
                }
                restore_system_cursors();
                self.forget_entry_keys();
                if drag.is_some() {
                    self.send_dnd(DndCmd::Park(false));
                }
                self.enter_failed();
                return;
            }
        };
        info!(keys_held = snapshot.keys.len(), "hooks: installed");
        let center = park_and_read_back(target);
        self.scaler.reset();
        let mut stay = MacStay::new(hooks, center, Instant::now());
        stay.left_held_at_entry = left_held_at_entry;
        self.mac = Some(stay);
        // SAFETY: `hwnd` is this thread's window; WM_TIMER arrives in `wnd_proc`.
        let ms = DESKTOP_POLL.as_millis() as u32;
        if unsafe { SetTimer(Some(self.hwnd), DESKTOP_TIMER, ms, None) } == 0 {
            warn!(
                "desktop check: SetTimer failed: {}",
                windows::core::Error::from_thread()
            );
        }
        if self.dnd.is_some() {
            // Shows the shield unless a fullscreen window is in front.
            self.check_park();
            // SAFETY: as above.
            let ms = PARK_POLL.as_millis() as u32;
            if unsafe { SetTimer(Some(self.hwnd), PARK_TIMER, ms, None) } == 0 {
                warn!(
                    "park check: SetTimer failed: {}",
                    windows::core::Error::from_thread()
                );
            }
        }
    }

    /// Ctrl+Alt+Del, Win+L and UAC switch the input desktop: our hooks then
    /// never see the key-ups for downs already sent to the Mac. Treat it as a
    /// lost hook, so the app releases everything and returns the cursor.
    fn check_desktop(&mut self) {
        let Some(stay) = &mut self.mac else {
            return;
        };
        if stay.hook_lost_sent {
            return;
        }
        let name = input_desktop_name();
        if desktop_verdict(name.as_deref()) == DesktopVerdict::HookLost {
            warn!(desktop = ?name, "input desktop switched away; hook lost");
            stay.hook_lost_sent = true;
            self.send(InputMsg::Edge(PcEvent::HookLost));
        }
    }

    /// Keys already down at entry: their releases go to Windows, and they count
    /// towards the return hotkey.
    fn seed_entry_keys(&mut self, snap: &EntrySnapshot) {
        let all = || snap.keys.iter().chain(&snap.twins).copied();
        self.router.on_enter_mac(all());
        self.held.clear();
        self.held.extend(all());
        self.entry_twins.clear();
        // A twin is a guess only if exactly one held VK produced it and no held VK
        // maps to it directly. Two held VKs on one scancode (e.g. VK_HOME and
        // VK_NUMPAD7) mean both physical keys are down, so neither form is a guess.
        let twin_only = snap.twins.iter().filter(|t| {
            !snap.keys.contains(t) && snap.twins.iter().filter(|u| u == t).count() == 1
        });
        self.entry_twins.extend(twin_only.copied());
    }

    fn forget_entry_keys(&mut self) {
        self.router.clear();
        self.held.clear();
        self.entry_twins.clear();
    }

    /// After `k`'s release went to Windows: if `k` was seeded as one of an
    /// ambiguous key's two forms, forget the other form too.
    fn forget_twin(&mut self, k: PcKey) {
        let twin = PcKey {
            extended: !k.extended,
            ..k
        };
        let k_was_twin = self.entry_twins.remove(&k);
        let partner_was_twin = self.entry_twins.remove(&twin);
        if k_was_twin || partner_was_twin {
            // A pending key's up only forgets it; the Route is irrelevant here.
            let _ = self.router.route(twin, false);
            self.held.remove(&twin);
        }
    }

    /// Tell the app the crossing failed and keep the edge quiet for a while.
    fn enter_failed(&mut self) {
        self.enter_failed_until = Some(Instant::now() + ENTER_FAILED_DEBOUNCE);
        self.recross.disarm();
        self.send(InputMsg::Edge(PcEvent::HookLost));
    }

    fn return_to_pc(&mut self, pc_frac: f32) {
        // Position the cursor before the cursors reappear, so it never shows at the centre.
        let target = self
            .edge
            .map(|edge| point_on_edge(&edge, self.cfg.mac_side, pc_frac, 2));
        if !self.leave_mac(target) {
            debug!("ReturnToPc ignored: already on the PC");
        } else if target.is_none() {
            warn!("ReturnToPc: no edge monitor; cursor left where it was");
        }
    }

    /// Remove hooks, move the cursor to `park_at`, restore cursors and forget
    /// held keys. False if not on the Mac.
    fn leave_mac(&mut self, park_at: Option<(i32, i32)>) -> bool {
        let Some(stay) = self.mac.take() else {
            return false;
        };
        remove_hooks(stay.hooks);
        info!("hooks: removed");
        // SAFETY: kills this thread's own timers; failure means one was not running.
        let _ = unsafe { KillTimer(Some(self.hwnd), DESKTOP_TIMER) };
        let _ = unsafe { KillTimer(Some(self.hwnd), PARK_TIMER) };
        if let Some(dnd) = &self.dnd {
            dnd.shared
                .under_park_is_ours
                .store(false, Ordering::Release);
            dnd.send(DndCmd::Shield(false));
        }
        self.recross.disarm();
        if let Some(pt) = park_at {
            park_cursor(pt);
        }
        restore_system_cursors();
        self.forget_entry_keys();
        true
    }

    fn shutdown(&mut self) {
        self.leave_mac(None);
        restore_system_cursors();
    }

    /// Relative motion from Raw Input (absolute devices report 0, 0) and its
    /// button flags (`RI_MOUSE_*`).
    fn on_raw_mouse(&mut self, dx: i32, dy: i32, buttons: u32) {
        let moved = dx != 0 || dy != 0;
        let now = Instant::now();
        if let Some(dnd) = &self.dnd {
            if buttons & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
                dnd.shared.left_down.store(true, Ordering::Release);
            }
            if buttons & RI_MOUSE_LEFT_BUTTON_UP != 0 {
                dnd.shared.left_down.store(false, Ordering::Release);
            }
        }
        if self.mac.is_none() && (moved || buttons != 0) {
            self.update_catch_zone(now);
        }
        if let Some(stay) = &mut self.mac {
            if stay.watchdog_trips(moved, now) {
                warn!("watchdog: raw motion but no mouse hook events; hook lost");
                self.send(InputMsg::Edge(PcEvent::HookLost));
                return;
            }
            // Something other than our swallowed input moved the cursor: re-park.
            if moved && cursor_pos().is_some_and(|p| p != stay.center) {
                park_cursor(stay.center);
            }
            return;
        }
        let quiet = |until: Option<Instant>| until.is_some_and(|t| now < t);
        if !moved || quiet(self.edge_quiet_until) || quiet(self.enter_failed_until) {
            return;
        }
        let Some(edge) = self.edge else {
            return;
        };
        let Some((x, y)) = cursor_pos() else {
            return;
        };
        let side = self.cfg.mac_side;
        let armed = self.recross.observe(&edge, side, x, y);
        if !is_pushing_out(&edge, side, x, y, dx, dy) {
            return;
        }
        if !armed {
            if self.recross.first_swallow() {
                info!("re-cross guard: move away from the edge to cross again");
            }
            return;
        }
        self.edge_quiet_until = Some(now + EDGE_DEBOUNCE);
        let (left, others) = (is_down(VK_LBUTTON.0), other_buttons_held());
        let offer = self
            .dnd
            .as_ref()
            .and_then(|d| offer_for(left, others, d.shared.offer.load(Ordering::Acquire)));
        self.send(InputMsg::Edge(PcEvent::EdgePush {
            frac: edge_frac(&edge, side, x, y),
            game: probe_game(),
            buttons_held: left || others,
            offer,
        }));
    }

    /// Show the edge strip while the left button alone is held near the Mac
    /// edge and no game holds the mouse; hide it once any of that stops.
    fn update_catch_zone(&mut self, now: Instant) {
        if self.dnd.is_none() {
            return;
        }
        let want = self.catch_zone_wanted(now);
        if want != self.zone {
            self.zone = want;
            debug!(want, "catcher zone");
            self.send_dnd(DndCmd::CatcherZone(want));
        }
    }

    fn catch_zone_wanted(&mut self, now: Instant) -> bool {
        let near = || {
            let (edge, (x, y)) = (self.edge?, cursor_pos()?);
            Some(in_catch_zone(&edge, self.cfg.mac_side, x, y))
        };
        let session = self
            .dnd
            .as_ref()
            .is_some_and(|d| d.shared.session_up.load(Ordering::Acquire));
        if !session || !is_down(VK_LBUTTON.0) || other_buttons_held() || near() != Some(true) {
            self.zone_game = None;
            return false;
        }
        // Probed only once the rest holds, and not on every packet.
        let locked = match self.zone_game {
            Some((at, locked)) if now.saturating_duration_since(at) < ZONE_PROBE_EVERY => locked,
            _ => {
                let locked = game_locked(&probe_game());
                self.zone_game = Some((now, locked));
                locked
            }
        };
        !locked
    }

    /// Returns true to swallow the event.
    fn on_mouse_hook(&mut self, msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
        let Some(stay) = &mut self.mac else {
            return false;
        };
        let over_ours = self
            .dnd
            .as_ref()
            .is_some_and(|d| d.shared.under_park_is_ours.load(Ordering::Acquire));
        let injected = info.flags & LLMHF_INJECTED != 0;
        let left = !injected && (msg == WM_LBUTTONDOWN || msg == WM_LBUTTONUP);
        let pass = Pass {
            over_ours,
            // A cheap, hook-safe read, made only for a real left button that
            // could pass: a window that came to the front since the last
            // park check may cover the park point.
            fg_match: left && over_ours && foreground() == stay.park_fg,
            down_passed: stay.left_passed,
            held_at_entry: stay.left_held_at_entry,
        };
        let verdict = hook_verdict(msg, injected, pass);
        if !verdict.forward {
            return !verdict.pass;
        }
        match msg {
            WM_LBUTTONDOWN => stay.left_passed = verdict.pass,
            WM_LBUTTONUP => {
                stay.left_passed = false;
                stay.left_held_at_entry = false;
            }
            _ => {}
        }
        let hiword = (info.mouseData >> 16) as u16;
        let button = |button, down| Some(InputMsg::Button { button, down });
        let out = match msg {
            WM_MOUSEMOVE => {
                // Any hook move (even sub-pixel, zero delta) proves the hook is alive.
                stay.last_hook_motion = Instant::now();
                let (dx, dy) = (info.pt.x - stay.center.0, info.pt.y - stay.center.1);
                if dx == 0 && dy == 0 {
                    None
                } else if dx.abs() > MAX_HOOK_DELTA || dy.abs() > MAX_HOOK_DELTA {
                    debug!(dx, dy, "hook: dropped oversized move");
                    None
                } else {
                    // A slow move may round to nothing; its remainder carries over.
                    match self.scaler.motion(dx, dy, self.cfg.pointer_speed) {
                        (0, 0) => None,
                        (dx, dy) => Some(InputMsg::Motion { dx, dy }),
                    }
                }
            }
            WM_MOUSEWHEEL => match self.wheel(WheelAxis::Vertical, hiword) {
                0 => None,
                dy => Some(InputMsg::Scroll { dx: 0, dy }),
            },
            WM_MOUSEHWHEEL => match self.wheel(WheelAxis::Horizontal, hiword) {
                0 => None,
                dx => Some(InputMsg::Scroll { dx, dy: 0 }),
            },
            WM_LBUTTONDOWN => button(MouseButton::Left, true),
            WM_LBUTTONUP => button(MouseButton::Left, false),
            WM_RBUTTONDOWN => button(MouseButton::Right, true),
            WM_RBUTTONUP => button(MouseButton::Right, false),
            WM_MBUTTONDOWN => button(MouseButton::Middle, true),
            WM_MBUTTONUP => button(MouseButton::Middle, false),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let down = msg == WM_XBUTTONDOWN;
                match hiword {
                    XBUTTON1 => button(MouseButton::Back, down),
                    XBUTTON2 => button(MouseButton::Forward, down),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(m) = out {
            self.send(m);
        }
        !verdict.pass
    }

    fn wheel(&mut self, axis: WheelAxis, hiword: u16) -> i32 {
        self.scaler.wheel(
            axis,
            hiword as i16 as i32,
            self.cfg.scroll_speed,
            self.cfg.reverse_scroll,
        )
    }

    /// Returns true to swallow the event.
    fn on_key_hook(&mut self, msg: u32, info: &KBDLLHOOKSTRUCT) -> bool {
        if self.mac.is_none() {
            return false;
        }
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        if DIAGNOSE_LOG.load(Ordering::Relaxed) {
            info!(
                "key raw: vk={:#04x} scan={:#04x} flags={:#04x} down={down}",
                info.vkCode, info.scanCode, info.flags.0
            );
        }
        if info.scanCode == 0 {
            return true;
        }
        let k = PcKey {
            scancode: info.scanCode as u16,
            extended: info.flags.contains(LLKHF_EXTENDED),
        };
        if down {
            self.held.insert(k);
        } else {
            self.held.remove(&k);
        }
        if is_return_hotkey(&self.held, k, down) {
            self.send(InputMsg::Edge(PcEvent::ReturnHotkey));
            return true;
        }
        match self.router.route(k, down) {
            Route::Local => {
                self.forget_twin(k);
                false
            }
            Route::Drop => true,
            Route::Remote => {
                if let Some(mac_keycode) = pc_to_mac(k, self.cfg.swap_ctrl_cmd) {
                    self.send(InputMsg::Key { mac_keycode, down });
                }
                true
            }
        }
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for HC_ACTION, lParam points to an MSLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        // Injected events pass untouched (`hook_verdict`).
        if with_state(|s| s.on_mouse_hook(wparam.0 as u32, info)) == Some(true) {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for HC_ACTION, lParam points to a KBDLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if !info.flags.contains(LLKHF_INJECTED)
            && with_state(|s| s.on_key_hook(wparam.0 as u32, info)) == Some(true)
        {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_INPUT => {
            if let Some((dx, dy, buttons)) = read_raw_mouse(HRAWINPUT(lparam.0 as _)) {
                with_state(|s| s.on_raw_mouse(dx, dy, buttons));
            }
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            with_state(State::refresh_monitors);
        }
        WM_TIMER if wparam.0 == DESKTOP_TIMER => {
            with_state(State::check_desktop);
        }
        WM_TIMER if wparam.0 == PARK_TIMER => {
            with_state(State::check_park);
        }
        _ => {}
    }
    // DefWindowProcW also performs the required WM_INPUT cleanup.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Relative (dx, dy) of a raw mouse packet (absolute devices yield (0, 0))
/// and its button flags.
fn read_raw_mouse(handle: HRAWINPUT) -> Option<(i32, i32, u32)> {
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    // SAFETY: `raw` is a RAWINPUT-sized buffer and `size` says so.
    let n = unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            Some((&mut raw as *mut RAWINPUT).cast()),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if n == 0 || n == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    // SAFETY: dwType says the union holds mouse data, whose button flags
    // are always valid.
    let mouse = unsafe { raw.data.mouse };
    let buttons = unsafe { mouse.Anonymous.Anonymous.usButtonFlags } as u32;
    if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
        return Some((0, 0, buttons));
    }
    Some((mouse.lLastX, mouse.lLastY, buttons))
}

fn module_instance() -> windows::core::Result<HINSTANCE> {
    // SAFETY: GetModuleHandleW(None) returns this executable's handle.
    Ok(unsafe { GetModuleHandleW(None) }?.into())
}

/// A hidden WS_POPUP top-level window (never shown). Not HWND_MESSAGE: message-only
/// windows do not receive WM_DISPLAYCHANGE. Creating it also creates the thread's
/// message queue.
fn create_window() -> windows::core::Result<HWND> {
    let hinst = module_instance()?;
    let class = w!("CursorJumpInput");
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: class,
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
            w!("CursorJump input"),
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

fn register_raw_mouse(hwnd: HWND) -> windows::core::Result<()> {
    let device = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    unsafe { RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32) }
}

fn install_hooks() -> windows::core::Result<Hooks> {
    let hmod = module_instance()?;
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(hmod), 0) }?;
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(hmod), 0) } {
        Ok(keyboard) => Ok(Hooks { mouse, keyboard }),
        Err(e) => {
            let _ = unsafe { UnhookWindowsHookEx(mouse) };
            Err(e)
        }
    }
}

fn remove_hooks(hooks: Hooks) {
    for h in [hooks.mouse, hooks.keyboard] {
        if let Err(e) = unsafe { UnhookWindowsHookEx(h) } {
            warn!("UnhookWindowsHookEx failed: {e}");
        }
    }
}

/// What the mouse hook does with one event while on the Mac.
#[derive(Debug, PartialEq, Eq)]
struct HookVerdict {
    /// Translate it and send it to the Mac.
    forward: bool,
    /// Hand it on to Windows (`CallNextHookEx`) instead of swallowing it.
    pass: bool,
}

/// What a real left button event may pass on.
#[derive(Debug, Clone, Copy, Default)]
struct Pass {
    /// One of CursorJump's own drag-and-drop windows lies under the park
    /// point (`DndShared::under_park_is_ours`).
    over_ours: bool,
    /// The foreground window is still the one the last park check saw.
    fg_match: bool,
    /// The last press was passed to Windows.
    down_passed: bool,
    /// The left button was down at the crossing (a riding drag).
    held_at_entry: bool,
}

/// Everything real is forwarded and swallowed, except that real left
/// buttons may also reach Windows (`CallNextHookEx`):
/// - a press, while our window is under the park point and the foreground
///   window hasn't changed since that was checked;
/// - a release, after a press that was passed (so the button never sticks
///   down), or under the same conditions when the button was already down
///   at the crossing (a riding drag's release, which ends it on the
///   catcher). If a foreign window is in front by then, that release is
///   swallowed and the PC's OS drag carries on once the cursor is back;
///   until then, a later Mac click that passes (once our window is in
///   front again) ends that drag on the catcher with its release, as a
///   drop.
///
/// Injected events pass untouched and are never forwarded. The hook only
/// ever swallows less than phase 1; it never makes input up.
fn hook_verdict(msg: u32, injected: bool, p: Pass) -> HookVerdict {
    if injected {
        return HookVerdict {
            forward: false,
            pass: true,
        };
    }
    let ours = p.over_ours && p.fg_match;
    let pass = match msg {
        WM_LBUTTONDOWN => ours,
        WM_LBUTTONUP => p.down_passed || (ours && p.held_at_entry),
        _ => false,
    };
    HookVerdict {
        forward: true,
        pass,
    }
}

/// The foreground window, as a number (0 = none). Cheap, and safe in a hook.
fn foreground() -> isize {
    // SAFETY: plain FFI call.
    unsafe { GetForegroundWindow() }.0 as isize
}

/// Whether `window` covers all of `monitor`.
fn covers_monitor(window: &Rect, monitor: &Rect) -> bool {
    window.left <= monitor.left
        && window.top <= monitor.top
        && window.right >= monitor.right
        && window.bottom >= monitor.bottom
}

/// The foreground window, and whether it covers all of `edge` (a fullscreen
/// or borderless game, a video). The desktop itself doesn't count.
fn foreground_covering(edge: &Rect) -> (isize, bool) {
    // SAFETY: plain FFI calls on window handles the system just gave us.
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || fg == GetShellWindow() {
            return (fg.0 as isize, false);
        }
        let mut class = [0u16; 16];
        let len = GetClassNameW(fg, &mut class) as usize;
        if matches!(
            String::from_utf16_lossy(&class[..len]).as_str(),
            "WorkerW" | "Progman"
        ) {
            return (fg.0 as isize, false);
        }
        let mut r = RECT::default();
        let covers = GetWindowRect(fg, &mut r).is_ok() && covers_monitor(&rect_from(r), edge);
        (fg.0 as isize, covers)
    }
}

/// The catcher's drag rides along only when the left button alone is held.
fn offer_for(left: bool, others: bool, offer: u64) -> Option<DragId> {
    (left && !others && offer != 0).then_some(offer)
}

pub(super) fn rect_center(r: &Rect) -> (i32, i32) {
    ((r.left + r.right) / 2, (r.top + r.bottom) / 2)
}

fn cursor_pos() -> Option<(i32, i32)> {
    let mut pt = POINT::default();
    // SAFETY: valid out-pointer to a stack local.
    unsafe { GetCursorPos(&mut pt) }.ok().map(|()| (pt.x, pt.y))
}

/// Park at `target` and return where the cursor actually is (Windows may
/// clamp or adjust it); that position is what hook deltas are measured from.
fn park_and_read_back(target: (i32, i32)) -> (i32, i32) {
    park_cursor(target);
    let actual = cursor_pos().unwrap_or(target);
    if actual != target {
        debug!(
            ?target,
            ?actual,
            "parked cursor landed elsewhere; adopting it"
        );
    }
    actual
}

fn park_cursor((x, y): (i32, i32)) {
    if let Err(e) = unsafe { SetCursorPos(x, y) } {
        warn!("SetCursorPos({x}, {y}) failed: {e}");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DesktopVerdict {
    Keep,
    HookLost,
}

/// `name` is the input desktop's name, `None` if it could not be opened or
/// read (the secure desktop denies access). Only "Default" is ours.
fn desktop_verdict(name: Option<&str>) -> DesktopVerdict {
    if name.is_some_and(|n| n.eq_ignore_ascii_case("Default")) {
        DesktopVerdict::Keep
    } else {
        DesktopVerdict::HookLost
    }
}

/// The name of the desktop receiving user input, if it can be read.
fn input_desktop_name() -> Option<String> {
    // SAFETY: no preconditions; the handle is closed below.
    let desk =
        unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) }.ok()?;
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is writable for the byte length passed.
    let read = unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            size_of_val(&buf) as u32,
            None,
        )
    };
    // SAFETY: `desk` came from OpenInputDesktop and is closed once.
    let _ = unsafe { CloseDesktop(desk) };
    read.ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn is_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState has no preconditions.
    (unsafe { GetAsyncKeyState(vk as i32) } as u16) & 0x8000 != 0
}

/// Any mouse button but the left.
fn other_buttons_held() -> bool {
    [VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
        .into_iter()
        .any(|vk| is_down(vk.0))
}

/// Keys down when crossing, in the forms the keyboard hook reports.
#[derive(Debug, Default)]
struct EntrySnapshot {
    /// One form per held VK.
    keys: Vec<PcKey>,
    /// The other form of ambiguous VKs (see `is_ambiguous_vk`).
    twins: Vec<PcKey>,
}

/// Physical keys down right now. Skips the generic VK_SHIFT/CONTROL/MENU
/// (0x10..=0x12) in favour of their left/right forms; mouse VKs lie below 0x08.
fn keys_down_now() -> EntrySnapshot {
    let mut snap = EntrySnapshot::default();
    for vk in (0x08u16..=0xFE).filter(|vk| !(0x10..=0x12).contains(vk) && is_down(*vk)) {
        // SAFETY: MapVirtualKeyW has no preconditions.
        let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
        let (key, twin) = entry_keys(vk, sc);
        snap.keys.extend(key);
        snap.twins.extend(twin);
    }
    snap
}

/// VKs whose key can reach the hook either with or without LLKHF_EXTENDED
/// (Enter vs numpad Enter, numpad digits/decimal vs the navigation cluster).
fn is_ambiguous_vk(vk: u16) -> bool {
    matches!(vk,
        0x0D // VK_RETURN
        | 0x21..=0x28 // VK_PRIOR, NEXT, END, HOME, LEFT, UP, RIGHT, DOWN
        | 0x2D | 0x2E // VK_INSERT, VK_DELETE
        | 0x60..=0x69 // VK_NUMPAD0..9
        | 0x6E // VK_DECIMAL
    )
}

/// The hook-form key for a held VK, plus its other form if the VK is ambiguous.
/// `sc` is `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX)`.
fn entry_keys(vk: u16, sc: u32) -> (Option<PcKey>, Option<PcKey>) {
    let key = |scancode, extended| Some(PcKey { scancode, extended });
    // Keys whose MapVirtualKeyW result differs from what the hook reports.
    match vk {
        0x90 => return (key(0x45, true), None), // VK_NUMLOCK (maps to 0x45)
        0x2C => return (key(0x37, true), None), // VK_SNAPSHOT (maps to 0x54)
        _ => {}
    }
    match sc >> 8 {
        _ if sc == 0 => (None, None),
        0xE1 => (key(0x45, false), None), // Pause (maps to 0xE11D)
        prefix => {
            let scancode = (sc & 0xFF) as u16;
            let extended = prefix == 0xE0;
            let twin = if is_ambiguous_vk(vk) {
                key(scancode, !extended)
            } else {
                None
            };
            (key(scancode, extended), twin)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEDGE: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    #[test]
    fn recross_guard_armed_initially() {
        let mut g = RecrossGuard::new();
        assert!(g.observe(&GEDGE, Side::Right, 1919, 500));
    }

    #[test]
    fn recross_guard_disarmed_until_eight_px_inward() {
        let mut g = RecrossGuard::new();
        g.disarm();
        assert!(!g.observe(&GEDGE, Side::Right, 1919, 500));
        assert!(!g.observe(&GEDGE, Side::Right, 1919 - 7, 500));
        assert!(g.observe(&GEDGE, Side::Right, 1919 - 8, 500));
        // Stays armed once re-armed, even back at the edge.
        assert!(g.observe(&GEDGE, Side::Right, 1919, 500));
    }

    #[test]
    fn recross_guard_mirrors_all_sides() {
        for (side, edge_pt, near, far) in [
            (Side::Right, (1919, 500), (1912, 500), (1911, 500)),
            (Side::Left, (0, 500), (7, 500), (8, 500)),
            (Side::Top, (500, 0), (500, 7), (500, 8)),
            (Side::Bottom, (500, 1079), (500, 1072), (500, 1071)),
        ] {
            let mut g = RecrossGuard::new();
            g.disarm();
            assert!(!g.observe(&GEDGE, side, edge_pt.0, edge_pt.1), "{side:?}");
            assert!(!g.observe(&GEDGE, side, near.0, near.1), "{side:?}");
            assert!(g.observe(&GEDGE, side, far.0, far.1), "{side:?}");
        }
    }

    #[test]
    fn recross_guard_logs_once_per_disarm() {
        let mut g = RecrossGuard::new();
        g.disarm();
        assert!(g.first_swallow());
        assert!(!g.first_swallow());
        g.disarm();
        assert!(g.first_swallow());
    }
    use windows::Win32::UI::WindowsAndMessaging::{KBDLLHOOKSTRUCT_FLAGS, WM_KEYUP};

    const CENTER: (i32, i32) = (960, 540);
    const W: PcKey = PcKey {
        scancode: 0x11,
        extended: false,
    };
    const LCTRL: PcKey = PcKey {
        scancode: 0x1D,
        extended: false,
    };
    const LALT: PcKey = PcKey {
        scancode: 0x38,
        extended: false,
    };

    fn dummy_hooks() -> Hooks {
        Hooks {
            mouse: HHOOK::default(),
            keyboard: HHOOK::default(),
        }
    }

    /// A state that is "on the Mac" with dummy hook handles (never unhooked).
    fn on_mac(entry_keys: &[PcKey]) -> (State, crossbeam_channel::Receiver<InputMsg>) {
        let snap = EntrySnapshot {
            keys: entry_keys.to_vec(),
            twins: Vec::new(),
        };
        on_mac_with(&snap)
    }

    fn on_mac_with(snap: &EntrySnapshot) -> (State, crossbeam_channel::Receiver<InputMsg>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut s = State::new(Config::default(), tx, None);
        s.seed_entry_keys(snap);
        s.mac = Some(MacStay::new(dummy_hooks(), CENTER, Instant::now()));
        (s, rx)
    }

    /// Feeds one keyboard-hook event; true = swallowed.
    fn key(s: &mut State, k: PcKey, down: bool) -> bool {
        let flags = if k.extended {
            LLKHF_EXTENDED
        } else {
            KBDLLHOOKSTRUCT_FLAGS(0)
        };
        let info = KBDLLHOOKSTRUCT {
            scanCode: k.scancode as u32,
            flags,
            ..Default::default()
        };
        s.on_key_hook(if down { WM_KEYDOWN } else { WM_KEYUP }, &info)
    }

    fn drain(rx: &crossbeam_channel::Receiver<InputMsg>) -> Vec<InputMsg> {
        rx.try_iter().collect()
    }

    #[test]
    fn key_hook_auto_repeat_of_entry_key_then_up_reaches_windows() {
        let (mut s, rx) = on_mac(&[W]);
        assert!(key(&mut s, W, true), "repeat-down swallowed");
        assert!(key(&mut s, W, true), "repeat-down swallowed");
        assert!(!key(&mut s, W, false), "up passes to Windows");
        assert!(drain(&rx).is_empty(), "nothing sent to the Mac");
        // A fresh press afterwards goes to the Mac.
        assert!(key(&mut s, W, true));
        assert!(matches!(
            drain(&rx)[..],
            [InputMsg::Key {
                mac_keycode: 0x0D,
                down: true
            }]
        ));
    }

    #[test]
    fn key_hook_remote_key_is_swallowed_and_sent() {
        let (mut s, rx) = on_mac(&[]);
        assert!(key(&mut s, W, true));
        assert!(key(&mut s, W, false));
        assert!(matches!(
            drain(&rx)[..],
            [
                InputMsg::Key {
                    mac_keycode: 0x0D,
                    down: true
                },
                InputMsg::Key {
                    mac_keycode: 0x0D,
                    down: false
                }
            ]
        ));
    }

    #[test]
    fn key_hook_return_hotkey_is_swallowed_and_reported() {
        let home = PcKey {
            scancode: 0x47,
            extended: true,
        };
        let (mut s, rx) = on_mac(&[]);
        key(&mut s, LCTRL, true);
        key(&mut s, LALT, true);
        drain(&rx);
        assert!(key(&mut s, home, true));
        assert!(matches!(
            drain(&rx)[..],
            [InputMsg::Edge(PcEvent::ReturnHotkey)]
        ));
    }

    #[test]
    fn key_hook_hotkey_works_with_modifiers_held_since_entry() {
        let home = PcKey {
            scancode: 0x47,
            extended: true,
        };
        let (mut s, rx) = on_mac(&[LCTRL, LALT]);
        // Auto-repeats of the entry-held modifiers are dropped...
        assert!(key(&mut s, LCTRL, true));
        assert!(drain(&rx).is_empty());
        // ...but still count towards the hotkey.
        assert!(key(&mut s, home, true));
        assert!(matches!(
            drain(&rx)[..],
            [InputMsg::Edge(PcEvent::ReturnHotkey)]
        ));
    }

    #[test]
    fn key_hook_scancode_zero_is_swallowed_silently() {
        let (mut s, rx) = on_mac(&[]);
        let zero = PcKey {
            scancode: 0,
            extended: false,
        };
        assert!(key(&mut s, zero, true));
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn key_hook_passes_through_on_the_pc() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut s = State::new(Config::default(), tx, None);
        assert!(!key(&mut s, W, true));
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn mouse_hook_sends_motion_and_drops_oversized_moves() {
        let (mut s, rx) = on_mac(&[]);
        let mv = |dx: i32, dy: i32| MSLLHOOKSTRUCT {
            pt: POINT {
                x: CENTER.0 + dx,
                y: CENTER.1 + dy,
            },
            ..Default::default()
        };
        assert!(s.on_mouse_hook(WM_MOUSEMOVE, &mv(5, -3)));
        assert!(s.on_mouse_hook(WM_MOUSEMOVE, &mv(201, 0)));
        assert!(s.on_mouse_hook(WM_MOUSEMOVE, &mv(0, -201)));
        assert!(s.on_mouse_hook(WM_MOUSEMOVE, &mv(200, -200)));
        assert!(s.on_mouse_hook(WM_MOUSEMOVE, &mv(0, 0)));
        assert!(matches!(
            drain(&rx)[..],
            [
                InputMsg::Motion { dx: 5, dy: -3 },
                InputMsg::Motion { dx: 200, dy: -200 }
            ]
        ));
    }

    #[test]
    fn mouse_hook_carries_slow_motion_and_wheel_remainders() {
        let (mut s, rx) = on_mac(&[]);
        s.cfg.pointer_speed = 0.4;
        let mv = MSLLHOOKSTRUCT {
            pt: POINT {
                x: CENTER.0 + 1,
                y: CENTER.1,
            },
            ..Default::default()
        };
        for _ in 0..5 {
            assert!(s.on_mouse_hook(WM_MOUSEMOVE, &mv));
        }
        // 0.4, 0.8, 0.2, 0.6, 0.0: two 1-px moves, no empty ones.
        assert!(matches!(
            drain(&rx)[..],
            [
                InputMsg::Motion { dx: 1, dy: 0 },
                InputMsg::Motion { dx: 1, dy: 0 }
            ]
        ));
        // A hi-res wheel delta of 1 (0.4 px) is held until it adds up.
        let wheel = MSLLHOOKSTRUCT {
            mouseData: 1 << 16,
            ..Default::default()
        };
        assert!(s.on_mouse_hook(WM_MOUSEWHEEL, &wheel));
        assert!(s.on_mouse_hook(WM_MOUSEWHEEL, &wheel));
        assert!(matches!(
            drain(&rx)[..],
            [InputMsg::Scroll { dx: 0, dy: 1 }]
        ));
    }

    #[test]
    fn desktop_default_is_kept() {
        assert_eq!(desktop_verdict(Some("Default")), DesktopVerdict::Keep);
        assert_eq!(desktop_verdict(Some("default")), DesktopVerdict::Keep);
    }

    #[test]
    fn desktop_secure_or_screen_saver_is_lost() {
        assert_eq!(desktop_verdict(Some("Winlogon")), DesktopVerdict::HookLost);
        assert_eq!(
            desktop_verdict(Some("Screen-saver")),
            DesktopVerdict::HookLost
        );
    }

    #[test]
    fn desktop_open_failure_is_lost() {
        assert_eq!(desktop_verdict(None), DesktopVerdict::HookLost);
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn watchdog_quiet_during_grace_after_entry() {
        let t0 = Instant::now();
        let mut stay = MacStay::new(dummy_hooks(), CENTER, t0);
        assert!(!stay.watchdog_trips(true, t0 + ms(200)));
    }

    #[test]
    fn watchdog_trips_once_when_hook_is_silent() {
        let t0 = Instant::now();
        let mut stay = MacStay::new(dummy_hooks(), CENTER, t0);
        assert!(stay.watchdog_trips(true, t0 + ms(300)));
        assert!(
            !stay.watchdog_trips(true, t0 + ms(400)),
            "only once per stay"
        );
    }

    #[test]
    fn watchdog_quiet_when_hook_saw_motion_recently() {
        let t0 = Instant::now();
        let mut stay = MacStay::new(dummy_hooks(), CENTER, t0);
        stay.last_hook_motion = t0 + ms(200);
        assert!(!stay.watchdog_trips(true, t0 + ms(300)));
        assert!(stay.watchdog_trips(true, t0 + ms(500)));
    }

    #[test]
    fn watchdog_ignores_raw_packets_without_motion() {
        let t0 = Instant::now();
        let mut stay = MacStay::new(dummy_hooks(), CENTER, t0);
        assert!(!stay.watchdog_trips(false, t0 + ms(1000)));
    }

    fn keys(vk: u16, sc: u32) -> Vec<PcKey> {
        let (key, twin) = entry_keys(vk, sc);
        key.into_iter().chain(twin).collect()
    }

    fn pk(scancode: u16, extended: bool) -> PcKey {
        PcKey { scancode, extended }
    }

    #[test]
    fn entry_keys_pause_is_plain_0x45() {
        assert_eq!(keys(0x13, 0xE11D), [pk(0x45, false)]);
    }

    // Inputs below are what MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX) returns on
    // Windows 11 (US layout), checked on this machine.

    #[test]
    fn entry_keys_numlock_and_snapshot_use_hook_forms() {
        assert_eq!(keys(0x90, 0x0045), [pk(0x45, true)]); // NumLock
        assert_eq!(keys(0x2C, 0x0054), [pk(0x37, true)]); // PrintScreen
    }

    #[test]
    fn entry_keys_ambiguous_vks_seed_both_forms() {
        assert_eq!(keys(0x24, 0x0047), [pk(0x47, false), pk(0x47, true)]); // Home
        assert_eq!(keys(0x2E, 0x0053), [pk(0x53, false), pk(0x53, true)]); // Delete
        assert_eq!(keys(0x0D, 0x001C), [pk(0x1C, false), pk(0x1C, true)]); // Enter
        assert_eq!(keys(0x67, 0x0047), [pk(0x47, false), pk(0x47, true)]); // Numpad 7
        assert_eq!(keys(0x6E, 0x0053), [pk(0x53, false), pk(0x53, true)]); // Decimal
    }

    /// Nav Home held at entry: VK_HOME maps to (0x47, plain), twin (0x47, ext).
    fn home_held_at_entry() -> EntrySnapshot {
        EntrySnapshot {
            keys: vec![pk(0x47, false)],
            twins: vec![pk(0x47, true)],
        }
    }

    #[test]
    fn twin_is_forgotten_when_the_real_form_is_released() {
        let (mut s, rx) = on_mac_with(&home_held_at_entry());
        // The physical nav Home key reports extended; its release goes to Windows.
        assert!(!key(&mut s, pk(0x47, true), false));
        // Numpad 7 (the plain form) pressed on the Mac is not lost.
        assert!(key(&mut s, pk(0x47, false), true));
        assert!(matches!(drain(&rx)[..], [InputMsg::Key { down: true, .. }]));
    }

    #[test]
    fn twin_is_forgotten_when_the_mapped_form_is_released() {
        let (mut s, rx) = on_mac_with(&home_held_at_entry());
        assert!(!key(&mut s, pk(0x47, false), false));
        assert!(key(&mut s, pk(0x47, true), true));
        assert!(matches!(drain(&rx)[..], [InputMsg::Key { down: true, .. }]));
    }

    #[test]
    fn both_forms_genuinely_held_each_release_reaches_windows() {
        // VK_HOME and VK_NUMPAD7 both down: two physical keys on scancode 0x47.
        let snap = EntrySnapshot {
            keys: vec![pk(0x47, false), pk(0x47, false)],
            twins: vec![pk(0x47, true), pk(0x47, true)],
        };
        let (mut s, rx) = on_mac_with(&snap);
        assert!(!key(&mut s, pk(0x47, true), false));
        assert!(!key(&mut s, pk(0x47, false), false), "still pending");
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn non_ambiguous_keys_are_independent() {
        // Left and right Ctrl share scancode 0x1D but are not twins.
        let rctrl = pk(0x1D, true);
        let (mut s, rx) = on_mac(&[LCTRL, rctrl]);
        assert!(!key(&mut s, LCTRL, false));
        assert!(!key(&mut s, rctrl, false), "right Ctrl still pending");
        assert!(drain(&rx).is_empty());
    }

    fn quota_error() -> windows::core::Error {
        windows::core::Error::from_hresult(
            windows::Win32::Foundation::ERROR_NOT_ENOUGH_QUOTA.to_hresult(),
        )
    }

    fn retry(post: impl FnMut() -> windows::core::Result<()>, alive: &AtomicBool) -> WakeOutcome {
        wake_with_retry(post, alive, ms(1), ms(50))
    }

    #[test]
    fn wake_retries_until_the_post_succeeds() {
        let alive = AtomicBool::new(true);
        let mut calls = 0;
        let post = || {
            calls += 1;
            if calls < 4 {
                Err(quota_error())
            } else {
                Ok(())
            }
        };
        assert_eq!(retry(post, &alive), WakeOutcome::Woken);
        assert_eq!(calls, 4);
    }

    #[test]
    fn wake_stops_at_once_on_invalid_thread_id() {
        let alive = AtomicBool::new(true);
        let mut calls = 0;
        let post = || {
            calls += 1;
            Err(windows::core::Error::from_hresult(
                ERROR_INVALID_THREAD_ID.to_hresult(),
            ))
        };
        assert_eq!(retry(post, &alive), WakeOutcome::ThreadGone);
        assert_eq!(calls, 1);
    }

    #[test]
    fn wake_stops_when_the_receiver_is_gone() {
        let alive = AtomicBool::new(false);
        assert_eq!(
            retry(|| Err(quota_error()), &alive),
            WakeOutcome::ReceiverGone
        );
    }

    #[test]
    fn wake_gives_up_after_the_limit() {
        let alive = AtomicBool::new(true);
        let started = Instant::now();
        assert_eq!(retry(|| Err(quota_error()), &alive), WakeOutcome::GaveUp);
        assert!(started.elapsed() >= ms(50));
    }

    #[test]
    fn wake_against_thread_id_zero_reports_thread_gone() {
        // Thread id 0 is never valid, so the real PostThreadMessageW must fail
        // with ERROR_INVALID_THREAD_ID (and posts nothing anywhere).
        let alive = AtomicBool::new(true);
        let post = || unsafe { PostThreadMessageW(0, WM_APP, WPARAM(0), LPARAM(0)) };
        assert_eq!(retry(post, &alive), WakeOutcome::ThreadGone);
    }

    #[test]
    fn edge_push_offer_only_with_left_alone() {
        assert_eq!(offer_for(true, false, 7), Some(7));
        assert_eq!(offer_for(true, true, 7), None, "another button too");
        assert_eq!(offer_for(false, true, 7), None);
        assert_eq!(offer_for(false, false, 7), None, "no button");
        assert_eq!(offer_for(true, false, 0), None, "the catcher holds nothing");
    }

    #[test]
    fn hook_passes_real_left_buttons_only_over_our_window() {
        let v = |forward, pass| HookVerdict { forward, pass };
        let all = [false, true];
        for over_ours in all {
            for fg_match in all {
                for down_passed in all {
                    for held_at_entry in all {
                        let p = Pass {
                            over_ours,
                            fg_match,
                            down_passed,
                            held_at_entry,
                        };
                        let ours = over_ours && fg_match;
                        let ctx = format!("{p:?}");
                        // A press: only onto our window, with the same window in front.
                        assert_eq!(
                            hook_verdict(WM_LBUTTONDOWN, false, p),
                            v(true, ours),
                            "{ctx}"
                        );
                        // A release: after a passed press, always; a ride's
                        // release only under the same conditions as a press.
                        assert_eq!(
                            hook_verdict(WM_LBUTTONUP, false, p),
                            v(true, down_passed || (ours && held_at_entry)),
                            "{ctx}"
                        );
                        // Anything else is still only forwarded.
                        for msg in [
                            WM_RBUTTONDOWN,
                            WM_RBUTTONUP,
                            WM_MBUTTONDOWN,
                            WM_XBUTTONUP,
                            WM_MOUSEMOVE,
                            WM_MOUSEWHEEL,
                            WM_MOUSEHWHEEL,
                        ] {
                            assert_eq!(hook_verdict(msg, false, p), v(true, false), "{ctx}");
                        }
                        // Injected events pass untouched and are never forwarded, as before.
                        for msg in [WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN] {
                            assert_eq!(hook_verdict(msg, true, p), v(false, true), "{ctx}");
                        }
                    }
                }
            }
        }
        // Spelled out: over our window with the same foreground, a press passes...
        let ours = Pass {
            over_ours: true,
            fg_match: true,
            ..Pass::default()
        };
        assert_eq!(hook_verdict(WM_LBUTTONDOWN, false, ours), v(true, true));
        // ...but not with another window in front, nor when not over ours.
        let new_fg = Pass {
            fg_match: false,
            ..ours
        };
        assert_eq!(hook_verdict(WM_LBUTTONDOWN, false, new_fg), v(true, false));
        assert_eq!(
            hook_verdict(WM_LBUTTONDOWN, false, Pass::default()),
            v(true, false)
        );
        // A release with no passed press and no ride is swallowed, even over ours.
        assert_eq!(hook_verdict(WM_LBUTTONUP, false, ours), v(true, false));
        // The release of a press Windows saw always reaches it.
        let passed = Pass {
            down_passed: true,
            ..Pass::default()
        };
        assert_eq!(hook_verdict(WM_LBUTTONUP, false, passed), v(true, true));
    }

    #[test]
    fn a_window_covering_the_edge_monitor_is_fullscreen() {
        let r = |left, top, right, bottom| Rect {
            left,
            top,
            right,
            bottom,
        };
        let mon = r(0, 0, 1920, 1080);
        assert!(covers_monitor(&mon, &mon));
        // Borderless with invisible resize borders, or spanning monitors.
        assert!(covers_monitor(&r(-8, -8, 1928, 1088), &mon));
        assert!(covers_monitor(&r(-1080, -400, 1920, 1520), &mon));
        // Maximized: the taskbar stays visible.
        assert!(!covers_monitor(&r(-8, -8, 1928, 1040), &mon));
        assert!(!covers_monitor(&r(0, 0, 1919, 1080), &mon));
        // Fullscreen on another monitor.
        assert!(!covers_monitor(&r(-1920, 0, 0, 1080), &mon));
    }

    #[test]
    fn mouse_hook_lets_left_buttons_through_while_our_window_is_under_the_park_point() {
        let (mut s, rx) = on_mac(&[]);
        let shared = Arc::new(super::super::dnd::DndShared::default());
        let (cmds, _cmd_rx) = mpsc::channel();
        s.dnd = Some(DndPort {
            shared: shared.clone(),
            cmds,
        });
        // As the last park check saw it.
        s.mac.as_mut().unwrap().park_fg = foreground();
        let at = MSLLHOOKSTRUCT {
            pt: POINT {
                x: CENTER.0,
                y: CENTER.1,
            },
            ..Default::default()
        };
        let swallowed = |s: &mut State, msg| s.on_mouse_hook(msg, &at);
        // Not ours under the park point: swallowed as in phase 1.
        assert!(swallowed(&mut s, WM_LBUTTONDOWN));
        assert!(swallowed(&mut s, WM_LBUTTONUP));
        shared.under_park_is_ours.store(true, Ordering::Release);
        assert!(!swallowed(&mut s, WM_LBUTTONDOWN));
        assert!(swallowed(&mut s, WM_RBUTTONDOWN));
        assert!(swallowed(&mut s, WM_RBUTTONUP));
        // A window covers the park point mid-click: the release still follows its press.
        shared.under_park_is_ours.store(false, Ordering::Release);
        assert!(!swallowed(&mut s, WM_LBUTTONUP));
        assert!(swallowed(&mut s, WM_LBUTTONDOWN));
        assert!(swallowed(&mut s, WM_LBUTTONUP));
        // A release with no press passed, over ours: swallowed...
        shared.under_park_is_ours.store(true, Ordering::Release);
        assert!(swallowed(&mut s, WM_LBUTTONUP));
        // ...unless the button was down at the crossing (a ride), once.
        s.mac.as_mut().unwrap().left_held_at_entry = true;
        assert!(!swallowed(&mut s, WM_LBUTTONUP));
        assert!(swallowed(&mut s, WM_LBUTTONUP));
        // Another window came to the front since the park check: no press passes.
        s.mac.as_mut().unwrap().park_fg = foreground() ^ 1;
        assert!(swallowed(&mut s, WM_LBUTTONDOWN));
        assert!(swallowed(&mut s, WM_LBUTTONUP));
        // Every real button still reached the Mac.
        let buttons: Vec<_> = drain(&rx)
            .into_iter()
            .filter_map(|m| match m {
                InputMsg::Button { button, down } => Some((button, down)),
                _ => None,
            })
            .collect();
        use MouseButton::{Left, Right};
        assert_eq!(
            buttons,
            [
                (Left, true),
                (Left, false),
                (Left, true),
                (Right, true),
                (Right, false),
                (Left, false),
                (Left, true),
                (Left, false),
                (Left, false),
                (Left, false),
                (Left, false),
                (Left, true),
                (Left, false)
            ]
        );
        // Injected: passed, not forwarded.
        let injected = MSLLHOOKSTRUCT {
            flags: LLMHF_INJECTED,
            ..at
        };
        assert!(!s.on_mouse_hook(WM_LBUTTONDOWN, &injected));
        assert!(!s.on_mouse_hook(WM_MOUSEMOVE, &injected));
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn entry_keys_plain_and_unmapped() {
        assert_eq!(keys(0x57, 0x11), [pk(0x11, false)]); // W
        assert_eq!(keys(0xA3, 0xE01D), [pk(0x1D, true)]); // Right Ctrl
        assert!(keys(0xFF, 0).is_empty());
    }
}
