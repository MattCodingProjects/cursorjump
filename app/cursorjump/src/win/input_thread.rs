//! The dedicated input thread.
//!
//! On the PC it only watches Raw Input (no hooks) and reports pushes against
//! the edge that leads to the Mac. Between `EnterMac` and `ReturnToPc` it owns
//! low-level mouse and keyboard hooks that swallow local input and forward it.
//!
//! All state lives in a thread-local: the hook procedures and the window
//! procedure run on this same thread, so no locks are needed.
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cj_core::config::Config;
use cj_core::input::{scale_motion, wheel_to_pixels};
use cj_core::keymap::{KeyRouter, PcKey, Route, is_return_hotkey, pc_to_mac};
use cj_core::layout::{Rect, edge_frac, is_pushing_out, point_on_edge};
use cj_core::pc_state::PcEvent;
use cj_core::proto::MouseButton;
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, PostThreadMessageW, RegisterClassExW, SetCursorPos, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WM_APP, WM_DISPLAYCHANGE,
    WM_INPUT, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETTINGCHANGE,
    WM_SYSKEYDOWN, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_POPUP, XBUTTON1, XBUTTON2,
};
use windows::core::w;

use super::cursor::{hide_system_cursors, restore_system_cursors};
use super::monitors::{edge_monitor, monitors};
use super::probe::probe_game;

/// Raw motion without hook motion for this long while on the Mac means the hook is gone.
const WATCHDOG: Duration = Duration::from_millis(250);
/// After an `EdgePush`, further pushes are suppressed until a command arrives or this passes.
const EDGE_DEBOUNCE: Duration = Duration::from_millis(250);

/// When set, the keyboard hook logs every raw key event (diagnose mode).
static LOG_RAW_KEYS: AtomicBool = AtomicBool::new(false);

pub fn set_raw_key_logging(on: bool) {
    LOG_RAW_KEYS.store(on, Ordering::Relaxed);
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
    EnterMac,
    ReturnToPc {
        pc_frac: f32,
    },
    #[allow(
        dead_code,
        reason = "sent by the app wiring; diagnose does not change config"
    )]
    ApplyConfig(Config),
    Quit,
}

/// Start the input thread. Commands sent on the returned `Sender` are relayed
/// to the thread and it is woken with `PostThreadMessageW(WM_APP)`. Dropping
/// every `Sender` stops the thread as if `Quit` had been sent.
pub fn spawn_input_thread(
    cfg: Config,
) -> (
    mpsc::Sender<InputCmd>,
    crossbeam_channel::Receiver<InputMsg>,
) {
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, cmd_rx) = mpsc::channel::<InputCmd>();
    let (relay_tx, relay_rx) = mpsc::channel::<InputCmd>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<u32>(1);

    std::thread::Builder::new()
        .name("cj-input".into())
        .spawn(move || run(cfg, msg_tx, relay_rx, ready_tx))
        .expect("spawn input thread");

    // The thread reports its id once its window (and so its message queue)
    // exists; only then can PostThreadMessageW reach it.
    match ready_rx.recv() {
        Ok(tid) => {
            std::thread::Builder::new()
                .name("cj-input-cmd".into())
                .spawn(move || relay_commands(cmd_rx, relay_tx, tid))
                .expect("spawn input command relay");
        }
        Err(_) => error!("input thread failed to start"),
    }
    (cmd_tx, msg_rx)
}

fn relay_commands(from: mpsc::Receiver<InputCmd>, to: mpsc::Sender<InputCmd>, tid: u32) {
    let wake = || unsafe { PostThreadMessageW(tid, WM_APP, WPARAM(0), LPARAM(0)) }.is_ok();
    for cmd in from {
        if to.send(cmd).is_err() || !wake() {
            return;
        }
    }
    let _ = to.send(InputCmd::Quit);
    wake();
}

struct Hooks {
    mouse: HHOOK,
    keyboard: HHOOK,
}

/// Present only while the cursor is on the Mac.
struct MacStay {
    hooks: Hooks,
    center: (i32, i32),
    entered_at: Instant,
    last_hook_motion: Instant,
    hook_lost_sent: bool,
}

struct State {
    cfg: Config,
    out: crossbeam_channel::Sender<InputMsg>,
    edge: Option<Rect>,
    mac: Option<MacStay>,
    edge_quiet_until: Option<Instant>,
    router: KeyRouter,
    /// Physical keys currently held, for the return hotkey.
    held: HashSet<PcKey>,
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
    out: crossbeam_channel::Sender<InputMsg>,
    cmds: mpsc::Receiver<InputCmd>,
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
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            cfg,
            out,
            edge,
            mac: None,
            edge_quiet_until: None,
            router: KeyRouter::new(),
            held: HashSet::with_capacity(32),
        })
    });

    if ready.send(unsafe { GetCurrentThreadId() }).is_ok() {
        message_loop(&cmds);
    }

    with_state(State::shutdown);
    let _ = unsafe { DestroyWindow(hwnd) };
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
            while let Ok(cmd) = cmds.try_recv() {
                if matches!(cmd, InputCmd::Quit) {
                    return;
                }
                with_state(|s| s.on_command(cmd));
            }
            continue;
        }
        unsafe { DispatchMessageW(&msg) };
    }
}

impl State {
    fn send(&self, msg: InputMsg) {
        let _ = self.out.send(msg);
    }

    fn on_command(&mut self, cmd: InputCmd) {
        self.edge_quiet_until = None;
        match cmd {
            InputCmd::EnterMac => self.enter_mac(),
            InputCmd::ReturnToPc { pc_frac } => self.return_to_pc(pc_frac),
            InputCmd::ApplyConfig(cfg) => {
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
            stay.center = rect_center(&edge);
            park_cursor(stay.center);
        }
    }

    fn enter_mac(&mut self) {
        if self.mac.is_some() {
            debug!("EnterMac ignored: already on the Mac");
            return;
        }
        let Some(edge) = self.edge else {
            warn!("EnterMac refused: no edge monitor");
            self.send(InputMsg::Edge(PcEvent::HookLost));
            return;
        };
        let down = keys_down_now();
        self.router.on_enter_mac(down.iter().copied());
        self.held.clear();
        self.held.extend(down.iter().copied());

        let hooks = match install_hooks() {
            Ok(h) => h,
            Err(e) => {
                error!("hooks: install failed: {e}");
                self.router.clear();
                self.held.clear();
                self.send(InputMsg::Edge(PcEvent::HookLost));
                return;
            }
        };
        info!(keys_held = down.len(), "hooks: installed");

        let center = rect_center(&edge);
        park_cursor(center);
        hide_system_cursors();
        let now = Instant::now();
        self.mac = Some(MacStay {
            hooks,
            center,
            entered_at: now,
            last_hook_motion: now,
            hook_lost_sent: false,
        });
    }

    fn return_to_pc(&mut self, pc_frac: f32) {
        if !self.leave_mac() {
            debug!("ReturnToPc ignored: already on the PC");
            return;
        }
        match self.edge {
            Some(edge) => {
                let (x, y) = point_on_edge(&edge, self.cfg.mac_side, pc_frac, 2);
                if let Err(e) = unsafe { SetCursorPos(x, y) } {
                    warn!("SetCursorPos on return failed: {e}");
                }
            }
            None => warn!("ReturnToPc: no edge monitor; cursor left where it was"),
        }
    }

    /// Remove hooks, restore cursors and forget held keys. False if not on the Mac.
    fn leave_mac(&mut self) -> bool {
        let Some(stay) = self.mac.take() else {
            return false;
        };
        remove_hooks(stay.hooks);
        info!("hooks: removed");
        restore_system_cursors();
        self.router.clear();
        self.held.clear();
        true
    }

    fn shutdown(&mut self) {
        self.leave_mac();
        restore_system_cursors();
    }

    /// Relative motion from Raw Input (absolute devices report 0, 0).
    fn on_raw_mouse(&mut self, dx: i32, dy: i32) {
        let moved = dx != 0 || dy != 0;
        let now = Instant::now();
        if let Some(stay) = &mut self.mac {
            if moved
                && !stay.hook_lost_sent
                && now.duration_since(stay.entered_at) > WATCHDOG
                && now.duration_since(stay.last_hook_motion) > WATCHDOG
            {
                stay.hook_lost_sent = true;
                warn!("watchdog: raw motion but no mouse hook events; hook lost");
                self.send(InputMsg::Edge(PcEvent::HookLost));
            }
            return;
        }
        if !moved || self.edge_quiet_until.is_some_and(|t| now < t) {
            return;
        }
        let Some(edge) = self.edge else {
            return;
        };
        let mut pt = POINT::default();
        if unsafe { GetCursorPos(&mut pt) }.is_err() {
            return;
        }
        let side = self.cfg.mac_side;
        if !is_pushing_out(&edge, side, pt.x, pt.y, dx, dy) {
            return;
        }
        self.edge_quiet_until = Some(now + EDGE_DEBOUNCE);
        self.send(InputMsg::Edge(PcEvent::EdgePush {
            frac: edge_frac(&edge, side, pt.x, pt.y),
            game: probe_game(),
            buttons_held: mouse_buttons_held(),
        }));
    }

    /// Returns true to swallow the event.
    fn on_mouse_hook(&mut self, msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
        let Some(stay) = &mut self.mac else {
            return false;
        };
        let hiword = (info.mouseData >> 16) as u16;
        let button = |button, down| Some(InputMsg::Button { button, down });
        let out = match msg {
            WM_MOUSEMOVE => {
                // Any hook move (even sub-pixel, zero delta) proves the hook is alive.
                stay.last_hook_motion = Instant::now();
                let (dx, dy) = (info.pt.x - stay.center.0, info.pt.y - stay.center.1);
                if dx == 0 && dy == 0 {
                    None
                } else {
                    let (dx, dy) = scale_motion(dx, dy, self.cfg.pointer_speed);
                    Some(InputMsg::Motion { dx, dy })
                }
            }
            WM_MOUSEWHEEL => Some(InputMsg::Scroll {
                dx: 0,
                dy: self.wheel(hiword),
            }),
            WM_MOUSEHWHEEL => Some(InputMsg::Scroll {
                dx: self.wheel(hiword),
                dy: 0,
            }),
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
        true
    }

    fn wheel(&self, hiword: u16) -> i32 {
        wheel_to_pixels(
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
        if LOG_RAW_KEYS.load(Ordering::Relaxed) {
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
            Route::Local => false,
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
        if info.flags & LLMHF_INJECTED == 0
            && with_state(|s| s.on_mouse_hook(wparam.0 as u32, info)) == Some(true)
        {
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
            if let Some((dx, dy)) = read_raw_mouse(HRAWINPUT(lparam.0 as _)) {
                with_state(|s| s.on_raw_mouse(dx, dy));
            }
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            with_state(State::refresh_monitors);
        }
        _ => {}
    }
    // DefWindowProcW also performs the required WM_INPUT cleanup.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Relative (dx, dy) of a raw mouse packet; absolute devices yield (0, 0).
fn read_raw_mouse(handle: HRAWINPUT) -> Option<(i32, i32)> {
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
    // SAFETY: dwType says the union holds mouse data.
    let mouse = unsafe { raw.data.mouse };
    if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
        return Some((0, 0));
    }
    Some((mouse.lLastX, mouse.lLastY))
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

fn rect_center(r: &Rect) -> (i32, i32) {
    ((r.left + r.right) / 2, (r.top + r.bottom) / 2)
}

fn park_cursor((x, y): (i32, i32)) {
    if let Err(e) = unsafe { SetCursorPos(x, y) } {
        warn!("SetCursorPos({x}, {y}) failed: {e}");
    }
}

fn is_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState has no preconditions.
    (unsafe { GetAsyncKeyState(vk as i32) } as u16) & 0x8000 != 0
}

fn mouse_buttons_held() -> bool {
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
        .into_iter()
        .any(|vk| is_down(vk.0))
}

/// Physical keys down right now. Skips the generic VK_SHIFT/CONTROL/MENU
/// (0x10..=0x12) in favour of their left/right forms; mouse VKs lie below 0x08.
fn keys_down_now() -> Vec<PcKey> {
    (0x08u16..=0xFE)
        .filter(|vk| !(0x10..=0x12).contains(vk) && is_down(*vk))
        .filter_map(|vk| {
            // SAFETY: MapVirtualKeyW has no preconditions.
            let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
            (sc != 0).then_some(PcKey {
                scancode: (sc & 0xFF) as u16,
                extended: matches!((sc >> 8) & 0xFF, 0xE0 | 0xE1),
            })
        })
        .collect()
}
