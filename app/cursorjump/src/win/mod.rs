//! Windows platform layer.
pub mod cursor;
pub mod input_thread;
pub mod monitors;
pub mod probe;

use std::time::{Duration, Instant};

use cj_core::config::Config;
use cj_core::gaming::game_locked;
use cj_core::pc_state::PcEvent;
use crossbeam_channel::RecvTimeoutError;
use tracing::{debug, error, info};
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

/// Longest stay on the "Mac" before diagnose forces a return.
const MAC_SAFETY: Duration = Duration::from_secs(20);

/// `--diagnose`: run the input thread with a stub app that logs every message
/// and simulates crossing on edge pushes and returning on the hotkey.
pub fn run_diagnose(run_for: Duration) {
    set_diagnose_logging(true);
    let (cmd_tx, msg_rx) = spawn_input_thread(Config::default());
    info!(
        seconds = run_for.as_secs(),
        "diagnose: push against the Mac edge to cross; Ctrl+Alt+Home returns"
    );
    let mut diag = Diagnose {
        cmd_tx,
        on_mac_since: None,
        frac: 0.5,
    };
    let deadline = Instant::now() + run_for;
    loop {
        let now = Instant::now();
        if diag
            .on_mac_since
            .is_some_and(|since| now.duration_since(since) >= MAC_SAFETY)
        {
            info!("diagnose: safety return after 20 s on the Mac");
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

struct Diagnose {
    cmd_tx: std::sync::mpsc::Sender<InputCmd>,
    on_mac_since: Option<Instant>,
    frac: f32,
}

impl Diagnose {
    fn return_to_pc(&mut self) {
        let _ = self
            .cmd_tx
            .send(InputCmd::ReturnToPc { pc_frac: self.frac });
        self.on_mac_since = None;
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
            }) => {
                let locked = game_locked(&game);
                info!(frac, buttons_held, locked, ?game, "EdgePush");
                if locked {
                    info!("blocked: game");
                } else if buttons_held {
                    info!("blocked: mouse button held");
                } else if self.on_mac_since.is_none() {
                    self.frac = frac;
                    self.on_mac_since = Some(Instant::now());
                    let _ = self.cmd_tx.send(InputCmd::EnterMac);
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
