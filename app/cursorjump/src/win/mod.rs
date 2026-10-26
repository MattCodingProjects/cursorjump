//! Windows platform layer.
pub mod app;
pub mod autostart;
mod bridges;
pub mod clipboard;
pub mod cursor;
pub mod dnd;
pub mod input_thread;
mod link;
pub mod monitors;
mod pill;
pub mod probe;
mod settings_window;

use std::time::{Duration, Instant};

use cj_core::config::Config;
use cj_core::gaming::game_locked;
use cj_core::pc_state::PcEvent;
use crossbeam_channel::RecvTimeoutError;
use tracing::{debug, error, info};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

use cursor::restore_system_cursors;
use input_thread::{InputCmd, InputMsg, set_diagnose_logging, spawn_input_thread};

/// Process-wide setup: per-monitor DPI awareness, undo any cursor hiding a
/// previous crashed run left behind, and restore cursors on panic.
pub fn init_process() {
    // Ignore failure: the manifest already declares PerMonitorV2.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    restore_system_cursors();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_system_cursors();
        default_hook(info);
    }));
}

/// Release builds have no console of their own; when started from a
/// terminal, log to that terminal. Does nothing in debug builds, which have one.
pub fn attach_parent_console() {
    // SAFETY: plain FFI call. It fails harmlessly when there is no parent
    // console or this process already has one.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Longest stay on the "Mac" before diagnose forces a return.
const MAC_SAFETY: Duration = Duration::from_secs(8);

/// After any return to the PC, edge pushes are ignored this long so a user who
/// is still pushing does not cross straight back.
const RECROSS_COOLDOWN: Duration = Duration::from_secs(2);

/// `--diagnose`: run the input thread with a stub app that logs every message
/// and simulates crossing on edge pushes and returning on the hotkey.
pub fn run_diagnose(run_for: Duration) {
    set_diagnose_logging(true);
    let (cmd_tx, msg_rx) = spawn_input_thread(Config::default(), None);
    info!(
        seconds = run_for.as_secs(),
        "diagnose: push against the Mac edge to cross; Ctrl+Alt+Shift+H or Ctrl+Alt+Home returns (auto-return after {} s)",
        MAC_SAFETY.as_secs()
    );
    spawn_clipboard_logger();
    let mut diag = Diagnose {
        cmd_tx,
        on_mac_since: None,
        frac: 0.5,
        cooldown_until: None,
        cooldown_logged: false,
    };
    let deadline = Instant::now() + run_for;
    loop {
        let now = Instant::now();
        if diag
            .on_mac_since
            .is_some_and(|since| now.duration_since(since) >= MAC_SAFETY)
        {
            info!(
                "diagnose: safety return after {} s on the Mac",
                MAC_SAFETY.as_secs()
            );
            diag.return_to_pc();
        }
        if now >= deadline {
            if diag.on_mac_since.is_some() {
                info!("diagnose: time is up; returning to the PC");
                diag.return_to_pc();
            }
            break;
        }
        let wake_at = diag
            .on_mac_since
            .map_or(deadline, |since| (since + MAC_SAFETY).min(deadline));
        match msg_rx.recv_timeout(wake_at.saturating_duration_since(now)) {
            Ok(msg) => diag.handle(msg),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                error!("diagnose: input thread exited");
                return;
            }
        }
    }
    let _ = diag.cmd_tx.send(InputCmd::Quit);
    // The thread drops its sender once it has cleaned up; wait for that.
    let until = Instant::now() + Duration::from_secs(2);
    while msg_rx.recv_deadline(until).is_ok() {}
    info!("diagnose: done");
}

/// Log a one-line summary per clipboard change (never the data itself).
fn spawn_clipboard_logger() {
    let rx = clipboard::spawn_clipboard_watcher();
    let spawned = std::thread::Builder::new()
        .name("cj-clipboard-log".into())
        .spawn(move || {
            let field = |len: Option<usize>| len.map_or("-".to_string(), |n| format!("{n}B"));
            for d in rx {
                info!(
                    "clipboard: text={} html={} rtf={} png={}",
                    field(d.text.as_ref().map(|v| v.len())),
                    field(d.html.as_ref().map(|v| v.len())),
                    field(d.rtf.as_ref().map(|v| v.len())),
                    field(d.png.as_ref().map(|v| v.len())),
                );
            }
        });
    if let Err(e) = spawned {
        error!("clipboard log thread failed to start: {e}");
    }
}

struct Diagnose {
    cmd_tx: std::sync::mpsc::Sender<InputCmd>,
    on_mac_since: Option<Instant>,
    frac: f32,
    /// Edge pushes are ignored until this instant (set on every return).
    cooldown_until: Option<Instant>,
    /// Whether the current cooldown window already logged an ignored push.
    cooldown_logged: bool,
}

impl Diagnose {
    fn return_to_pc(&mut self) {
        let _ = self
            .cmd_tx
            .send(InputCmd::ReturnToPc { pc_frac: self.frac });
        self.on_mac_since = None;
        self.cooldown_until = Some(Instant::now() + RECROSS_COOLDOWN);
        self.cooldown_logged = false;
    }

    fn handle(&mut self, msg: InputMsg) {
        match msg {
            InputMsg::Motion { dx, dy } => debug!(dx, dy, "Motion"),
            InputMsg::Scroll { dx, dy } => debug!(dx, dy, "Scroll"),
            InputMsg::Button { button, down } => info!(?button, down, "Button"),
            InputMsg::Key { mac_keycode, down } => {
                info!("Key mac_keycode={mac_keycode:#04x} down={down}")
            }
            InputMsg::Edge(PcEvent::EdgePush {
                frac,
                game,
                buttons_held,
                ..
            }) => {
                let locked = game_locked(&game);
                info!(frac, buttons_held, locked, ?game, "EdgePush");
                if self.cooldown_until.is_some_and(|t| Instant::now() < t) {
                    if !self.cooldown_logged {
                        self.cooldown_logged = true;
                        info!("ignored: cooldown after returning to the PC");
                    }
                } else if locked {
                    info!("blocked: game");
                } else if buttons_held {
                    info!("blocked: mouse button held");
                } else if self.on_mac_since.is_none() {
                    self.frac = frac;
                    self.on_mac_since = Some(Instant::now());
                    let _ = self.cmd_tx.send(InputCmd::EnterMac { drag: None });
                }
            }
            InputMsg::Edge(ev @ (PcEvent::ReturnHotkey | PcEvent::HookLost)) => {
                info!(?ev, "returning to the PC");
                self.return_to_pc();
            }
            InputMsg::Edge(other) => info!(?other, "unexpected edge event"),
        }
    }
}
