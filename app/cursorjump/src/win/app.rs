//! The PC app. The tray runs on the main thread. A tokio loop on the
//! `cj-link` thread finds the Mac, keeps one session to it and drives
//! `PcMachine`: it feeds it events and carries out the actions it returns.
//!
//! The input thread, the clipboard watcher and the drag-and-drop threads
//! (cj-dnd and the pills) are started once and outlive every reconnect. Two
//! bridge threads (`bridges.rs` and `common::clipboard_bridge`) move their
//! output into the loop, and cj-dnd and the pills post to it themselves;
//! the session itself lives in `link.rs`, and the drags' part of the loop
//! in `drags.rs`.

mod drags;

use std::collections::VecDeque;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use cj_core::clipboard::ClipboardData;
use cj_core::config::Config;
use cj_core::dnd::DragId;
use cj_core::gaming::game_locked;
use cj_core::latency::LatencyStats;
use cj_core::pc_state::{Link, PcAction, PcEvent, PcMachine, Where};
use cj_core::proto::{Control, Datagram};
use cj_net::Identity;
use cj_net::session::Session;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at, sleep_until};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

use super::bridges::bridge_input;
use super::clipboard::{read_clipboard, spawn_clipboard_watcher, write_clipboard};
use super::dnd::WinDndEv;
use super::input_thread::{InputCmd, InputMsg, spawn_input_thread};
use super::link::{Backoff, Established, Live, establish};
use super::probe::probe_game;
use super::settings_window::bring_to_front;
use crate::common::clipboard_bridge::{ClipboardLink, bridge_clipboard};
use crate::common::config_file::ConfigFile;
use crate::common::dnd_link::DndEv;
use crate::common::paired_fingerprint;
use crate::common::session_tasks::NetEv;
use crate::common::settings_window::SettingsWindow;
use crate::paths;
use crate::ui::dialogs;
use crate::ui::tray::{TrayCmd, TrayLoop, TrayModel, latency_text};
use drags::{DRAG_TICK, Drags, SWEEP_EVERY, start_dnd, start_pills};

const PING_EVERY: Duration = Duration::from_millis(100);
const CONFIG_POLL: Duration = Duration::from_millis(500);
/// The latency readout is recomputed this often.
const LATENCY_REFRESH: Duration = Duration::from_secs(1);
/// A latency summary is logged this often during a paired session.
const LATENCY_LOG: Duration = Duration::from_secs(60);
/// After either side declines pairing, wait this long before asking again.
const PAIR_DECLINED_RETRY: Duration = Duration::from_secs(30);
/// How long Quit waits for the input thread to put the cursor back.
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs the PC app until Quit. Exits early (Ok) if another instance is running.
pub fn run_pc() -> Result<()> {
    let Some(_instance) = SingleInstance::acquire()? else {
        info!("CursorJump is already running; exiting");
        return Ok(());
    };
    let dir = paths::config_dir()?;
    let mut config_file = ConfigFile::new(paths::config_path()?, dialogs::notify_error);
    let identity = Identity::load_or_create(&dir).context("loading this PC's identity")?;
    let cfg = config_file.load_initial();
    info!(
        config = %config_file.path().display(),
        fingerprint = %identity.fingerprint().to_hex(),
        "CursorJump starting"
    );

    // Before the input thread: see `TrayLoop::new`.
    let tray = TrayLoop::new();
    let (events, events_rx) = unbounded_channel();
    let dnd = start_dnd(&cfg, &events);
    let pill = start_pills(&events);
    let (input, input_rx) = spawn_input_thread(cfg.clone(), dnd.clone());
    bridge_input(input_rx, events.clone())?;
    let clip_link = Arc::new(ClipboardLink::new(cfg.clipboard_max_bytes));
    let clip_events = events.clone();
    let clip_write = bridge_clipboard(
        spawn_clipboard_watcher(),
        clip_link.clone(),
        read_clipboard,
        write_clipboard,
        move |d| clip_events.send(Ev::LocalClipboard(d)).is_ok(),
    )?;
    let (model_tx, model_rx) = std::sync::mpsc::channel();

    let app = App::new(
        cfg,
        config_file,
        identity,
        Channels {
            input,
            clip_write,
            clip_link,
            tray: model_tx,
            events: events.clone(),
        },
        Drags::new(dnd, pill),
    );
    std::thread::Builder::new()
        .name("cj-link".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build();
            match runtime {
                Ok(rt) => rt.block_on(app.run(events_rx)),
                Err(e) => error!("starting the network runtime: {e}"),
            }
            // Dropping the app drops the tray's model sender, which ends the process.
        })
        .context("starting the link thread")?;
    tray.run(model_rx, move |cmd| {
        let _ = events.send(Ev::Tray(cmd));
    })
}

/// Holds the `Local\CursorJump` mutex for the life of the process.
struct SingleInstance(HANDLE);

impl SingleInstance {
    /// None if another process already holds the mutex.
    fn acquire() -> Result<Option<SingleInstance>> {
        // SAFETY: plain FFI call with a static name.
        let handle = unsafe { CreateMutexW(None, false, w!("Local\\CursorJump")) }
            .context("creating the single-instance mutex")?;
        // SAFETY: reads this thread's last error, set by CreateMutexW.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: the handle is ours and closed once.
            let _ = unsafe { CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(SingleInstance(handle)))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Everything the loop reacts to, apart from its own timers.
pub(super) enum Ev {
    Input(InputMsg),
    InputGone,
    /// A local copy that passed the clipboard gate.
    LocalClipboard(ClipboardData),
    Tray(TrayCmd),
    ForgetConfirmed(bool),
    Established(Result<Established>),
    PairAnswer {
        sid: u64,
        accepted: bool,
    },
    Net {
        sid: u64,
        ev: NetEv,
    },
    /// From cj-dnd.
    Dnd(WinDndEv),
    /// A running pill's close button.
    PillCancel(DragId),
    /// A failed pill's close button took it away.
    PillDismissed(DragId),
}

/// The tray's status line.
fn status_text(link: &Link, peer_name: Option<&str>, mac_ready: bool, paused: bool) -> String {
    let base = match link {
        Link::Searching => "Searching for your Mac…".to_string(),
        Link::Pairing { code } => format!("Pairing — check that both screens show {code}"),
        Link::Connected => {
            let peer = peer_name.unwrap_or("your Mac");
            if mac_ready {
                format!("Connected to {peer}")
            } else {
                format!("Connected to {peer} — Mac needs Accessibility")
            }
        }
    };
    if paused {
        format!("{base} (paused)")
    } else {
        base
    }
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".into())
}

/// Both pairing answers for the current pairing session. `PcMachine` needs
/// the local answer with every `PairAnswered`, so a Mac answer that arrives
/// first is held until this PC's user has answered.
#[derive(Debug, Default)]
struct PairAnswers {
    local: Option<bool>,
    remote: Option<bool>,
}

impl PairAnswers {
    fn local(&mut self, accepted: bool) -> PcEvent {
        self.local = Some(accepted);
        PcEvent::PairAnswered {
            local: accepted,
            remote: self.remote,
        }
    }

    /// None until the local answer is known.
    fn remote(&mut self, accepted: bool) -> Option<PcEvent> {
        self.remote = Some(accepted);
        self.local.map(|local| PcEvent::PairAnswered {
            local,
            remote: Some(accepted),
        })
    }

    fn declined(&self) -> bool {
        self.local == Some(false) || self.remote == Some(false)
    }
}

/// The loop's connections to the other threads.
struct Channels {
    input: std::sync::mpsc::Sender<InputCmd>,
    clip_write: crossbeam_channel::Sender<ClipboardData>,
    clip_link: Arc<ClipboardLink>,
    tray: std::sync::mpsc::Sender<TrayModel>,
    events: UnboundedSender<Ev>,
}

struct App {
    machine: PcMachine,
    cfg: Config,
    config_file: ConfigFile,
    identity: Arc<Identity>,
    name: String,
    ch: Channels,
    last_model: Option<TrayModel>,
    /// Machine actions not carried out yet.
    pending: VecDeque<PcAction>,
    link: Option<Live>,
    /// Set when the current session failed; handled by `settle`.
    dead: Option<String>,
    next_sid: u64,
    connecting: bool,
    retry_at: Option<Instant>,
    backoff: Backoff,
    pair_dialog_open: bool,
    pair: PairAnswers,
    latency: LatencyStats,
    latency_text: String,
    ping_seq: u32,
    /// Zero point for ping timestamps.
    epoch: std::time::Instant,
    settings: SettingsWindow,
    quit_by: Option<Instant>,
    done: bool,
    drags: Drags,
    /// Every session's drag events; taken by `run`.
    drag_events: Option<UnboundedReceiver<DndEv>>,
}

impl App {
    fn new(
        cfg: Config,
        config_file: ConfigFile,
        identity: Identity,
        ch: Channels,
        (drags, drag_events): (Drags, UnboundedReceiver<DndEv>),
    ) -> App {
        let latency = LatencyStats::new();
        App {
            machine: PcMachine::new(cfg.clone()),
            cfg,
            config_file,
            identity: Arc::new(identity),
            name: computer_name(),
            ch,
            last_model: None,
            pending: VecDeque::new(),
            link: None,
            dead: None,
            next_sid: 0,
            connecting: false,
            retry_at: Some(Instant::now()),
            backoff: Backoff::default(),
            pair_dialog_open: false,
            pair: PairAnswers::default(),
            latency_text: latency_text(&latency),
            latency,
            ping_seq: 0,
            epoch: std::time::Instant::now(),
            settings: SettingsWindow::default(),
            quit_by: None,
            done: false,
            drags,
            drag_events: Some(drag_events),
        }
    }

    async fn run(mut self, mut events: UnboundedReceiver<Ev>) {
        let mut ping = interval(PING_EVERY);
        ping.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut config_poll = interval(CONFIG_POLL);
        let mut latency_refresh = interval(LATENCY_REFRESH);
        let mut latency_log = interval(LATENCY_LOG);
        let mut drag_tick = interval(DRAG_TICK);
        drag_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut sweep = interval_at(Instant::now() + SWEEP_EVERY, SWEEP_EVERY);
        sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let Some(mut drag_events) = self.drag_events.take() else {
            return;
        };
        self.sweep_temp_dirs(true);
        self.publish();
        loop {
            let (retry_at, quit_by) = (self.retry_at, self.quit_by);
            tokio::select! {
                ev = events.recv() => match ev {
                    Some(ev) => self.handle(ev).await,
                    None => break,
                },
                _ = sleep_until(retry_at.unwrap_or_else(Instant::now)), if retry_at.is_some() => {
                    self.retry_at = None;
                    self.start_connect();
                }
                _ = ping.tick() => self.ping(),
                _ = config_poll.tick() => {
                    if let Some(cfg) = self.config_file.poll() {
                        self.apply_config(cfg);
                    }
                }
                _ = latency_refresh.tick() => self.latency_text = latency_text(&self.latency),
                _ = latency_log.tick() => self.log_latency(),
                Some(ev) = drag_events.recv() => self.on_link_dnd(ev).await,
                _ = drag_tick.tick() => self.drag_tick().await,
                _ = sweep.tick() => self.sweep_temp_dirs(false),
                _ = sleep_until(quit_by.unwrap_or_else(Instant::now)), if quit_by.is_some() => {
                    warn!("input thread did not stop in time");
                    break;
                }
            }
            self.settle().await;
            if self.done {
                break;
            }
            self.publish();
        }
        self.drags.disconnected();
        self.link = None;
        // Every way out (Quit, input thread gone) closes the settings window.
        self.settings.close();
        info!("stopped");
    }

    /// Carries out pending actions, and handles a failed session, until neither is left.
    async fn settle(&mut self) {
        loop {
            if let Some(action) = self.pending.pop_front() {
                self.exec(action).await;
            } else if let Some(reason) = self.dead.take() {
                info!("disconnected: {reason}");
                // Before the machine's cancels and returns.
                self.drags.disconnected();
                self.feed(PcEvent::Disconnected);
            } else {
                return;
            }
        }
    }

    fn feed(&mut self, ev: PcEvent) {
        self.pending.extend(self.machine.on(ev));
    }

    async fn exec(&mut self, action: PcAction) {
        match action {
            PcAction::ShowPairDialog { code } => self.show_pair_dialog(code),
            PcAction::PersistPairing => {
                if let Some(fp) = self.link.as_ref().map(|l| l.session.peer().to_hex()) {
                    info!("paired");
                    self.update_config(|c| c.paired_peer = Some(fp));
                }
            }
            PcAction::EnterMac {
                mac_edge,
                frac,
                drag,
            } => {
                // The input thread covers the screen with the catcher and
                // parks the cursor at once, whatever becomes of the offer,
                // which goes before `Enter` on the same control stream.
                self.send_input(InputCmd::EnterMac { drag });
                let drag = match drag {
                    Some(drag) => self.offer_ride(drag).await,
                    None => None,
                };
                self.control(Control::Enter {
                    edge: mac_edge,
                    frac,
                    drag,
                })
                .await;
            }
            PcAction::ReturnToPc { pc_frac } => self.send_input(InputCmd::ReturnToPc { pc_frac }),
            PcAction::SendReleaseAll => self.control(Control::ReleaseAll).await,
            PcAction::Reconnect => self.reconnect(),
            PcAction::CancelSourceDrag { .. }
            | PcAction::SourceDragHome { .. }
            | PcAction::StartIncomingDrag { .. }
            | PcAction::CancelIncomingDrag { .. }
            | PcAction::RefuseIncomingDrag { .. } => self.exec_drag(action).await,
        }
    }

    async fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Input(msg) => self.on_input(msg).await,
            Ev::InputGone => {
                if self.quit_by.is_none() {
                    error!("input thread stopped; quitting");
                }
                self.done = true;
            }
            Ev::LocalClipboard(d) => {
                // The bridge already gated it; the link may have dropped since.
                if self.paired_session().is_some()
                    && let Some(live) = &self.link
                {
                    live.send_clipboard(d);
                }
            }
            Ev::Tray(cmd) => self.on_tray(cmd).await,
            Ev::ForgetConfirmed(yes) => {
                if yes {
                    info!("forgetting the paired computer");
                    self.update_config(|c| c.paired_peer = None);
                }
            }
            Ev::Established(result) => {
                self.connecting = false;
                match result {
                    Ok(est) => self.on_established(est),
                    Err(e) => {
                        debug!("no connection: {e:#}");
                        self.retry_at = Some(Instant::now() + self.backoff.next_delay());
                    }
                }
            }
            Ev::PairAnswer { sid, accepted } => self.on_pair_answer(sid, accepted).await,
            Ev::Net { sid, ev } => {
                if self.link.as_ref().is_some_and(|l| l.sid == sid) {
                    self.on_net(ev);
                }
            }
            Ev::Dnd(ev) => self.on_dnd(ev).await,
            Ev::PillCancel(drag) => self.on_pill_cancel(drag).await,
            Ev::PillDismissed(drag) => self.on_pill_dismissed(drag),
        }
    }

    async fn on_input(&mut self, msg: InputMsg) {
        if let InputMsg::Edge(ev) = msg {
            // Once quitting, the input thread is stopping: never cross again.
            if self.quit_by.is_none() {
                self.feed(ev);
            }
            return;
        }
        // Forwarded input only means something while the cursor is on the Mac.
        if self.machine.where_() != &Where::Mac {
            return;
        }
        match msg {
            InputMsg::Motion { dx, dy } => self.datagram(&Datagram::Motion { dx, dy }),
            InputMsg::Scroll { dx, dy } => self.datagram(&Datagram::Scroll { dx, dy }),
            InputMsg::Button { button, down } => {
                self.control(Control::Button { button, down }).await
            }
            InputMsg::Key { mac_keycode, down } => {
                self.control(Control::Key { mac_keycode, down }).await
            }
            InputMsg::Edge(_) => {}
        }
    }

    fn on_net(&mut self, ev: NetEv) {
        match ev {
            NetEv::Control(Control::Leave { frac, drag }) => self.feed(PcEvent::MacLeave {
                frac,
                drag,
                game: game_locked(&probe_game()),
            }),
            NetEv::Control(Control::MacReady { ready }) => {
                if ready != self.machine.mac_ready() {
                    info!(ready, "the Mac's readiness changed");
                }
                self.feed(PcEvent::MacReady(ready));
            }
            NetEv::Control(Control::PairConfirm { accepted }) => {
                self.on_remote_pair_answer(accepted)
            }
            NetEv::Control(
                c @ (Control::DragOffer { .. }
                | Control::DragCancel { .. }
                | Control::DragDropped { .. }
                | Control::FileFetch { .. }
                | Control::DragFinished { .. }),
            ) => self.on_drag_control(c),
            NetEv::Control(other) => debug!(?other, "ignoring unexpected control message"),
            NetEv::Pong { rtt, at } => {
                self.latency.record(at, rtt);
                self.on_rtt(rtt, at);
            }
            NetEv::Clipboard(d) => {
                if self.paired_session().is_some()
                    && !d.is_empty()
                    && self.ch.clip_write.send(d).is_err()
                {
                    warn!("clipboard bridge is gone; clipboard from the Mac dropped");
                }
            }
            NetEv::Files(files) => self.on_files(files),
            NetEv::Closed(reason) => self.mark_dead(reason),
        }
    }

    // ------------------------------------------------------------ connecting

    fn start_connect(&mut self) {
        if self.link.is_some() || self.connecting || self.quit_by.is_some() {
            return;
        }
        // Pairing again while an old pairing dialog is still up would stack
        // dialogs; `on_pair_answer` restarts connecting once it closes.
        if self.pair_dialog_open && self.cfg.paired_peer.is_none() {
            return;
        }
        self.connecting = true;
        let (id, cfg, name) = (self.identity.clone(), self.cfg.clone(), self.name.clone());
        let tx = self.ch.events.clone();
        tokio::spawn(async move {
            let result = establish(&id, &cfg, &name).await;
            let _ = tx.send(Ev::Established(result));
        });
    }

    fn on_established(&mut self, est: Established) {
        let Established { session, code } = est;
        // The pairing may have changed while connecting.
        let fits = match (paired_fingerprint(&self.cfg), &code) {
            (Some(fp), None) => session.peer() == fp,
            (None, Some(_)) => true,
            _ => false,
        };
        if self.quit_by.is_some() || !fits {
            session.close();
            if self.quit_by.is_none() {
                self.retry_at = Some(Instant::now());
            }
            return;
        }
        self.backoff.reset();
        let sid = self.next_sid;
        self.next_sid += 1;
        let live = Live::start(
            sid,
            session,
            self.cfg.clipboard_max_bytes,
            self.epoch,
            &self.ch.events,
        );
        info!(peer = %live.peer_name, paired = code.is_none(), "connected");
        self.drags.connect(&live.session);
        self.link = Some(live);
        self.reset_latency();
        match code {
            None => self.feed(PcEvent::Connected {
                paired: true,
                code: String::new(),
            }),
            Some(code) => {
                self.pair = PairAnswers::default();
                self.feed(PcEvent::Connected {
                    paired: false,
                    code,
                });
            }
        }
    }

    /// Drops the session (if any) and schedules the next attempt.
    fn reconnect(&mut self) {
        self.drags.disconnected();
        self.link = None;
        self.dead = None;
        self.reset_latency();
        let delay = if std::mem::take(&mut self.pair).declined() {
            PAIR_DECLINED_RETRY
        } else {
            self.backoff.next_delay()
        };
        self.retry_at = Some(Instant::now() + delay);
    }

    fn reset_latency(&mut self) {
        self.latency = LatencyStats::new();
        self.latency_text = latency_text(&self.latency);
    }

    /// Logs the latency summary during a paired session with samples.
    fn log_latency(&self) {
        if self.paired_session().is_some() && !self.latency.is_empty() {
            info!("{}", self.latency.log_line());
        }
    }

    // --------------------------------------------------------------- pairing

    fn show_pair_dialog(&mut self, code: String) {
        let Some(live) = &self.link else {
            return;
        };
        let (sid, peer, tx) = (live.sid, live.peer_name.clone(), self.ch.events.clone());
        info!(peer = %peer, "pairing: confirm the code on both screens");
        self.pair_dialog_open = true;
        tokio::spawn(async move {
            let accepted = dialogs::ask_pairing(&code, &peer).await;
            let _ = tx.send(Ev::PairAnswer { sid, accepted });
        });
    }

    async fn on_pair_answer(&mut self, sid: u64, accepted: bool) {
        self.pair_dialog_open = false;
        let current = self.link.as_ref().is_some_and(|l| l.sid == sid)
            && matches!(self.machine.link(), Link::Pairing { .. });
        if !current {
            // Its session is gone; connecting may have been waiting for this dialog.
            if self.link.is_none() && !self.connecting && self.retry_at.is_none() {
                self.retry_at = Some(Instant::now());
            }
            return;
        }
        info!(accepted, "pairing: answered on this PC");
        self.control(Control::PairConfirm { accepted }).await;
        if self.dead.is_some() {
            // The Mac never heard the answer: do not persist a pairing it may lack.
            return;
        }
        let ev = self.pair.local(accepted);
        self.feed(ev);
    }

    fn on_remote_pair_answer(&mut self, accepted: bool) {
        if !matches!(self.machine.link(), Link::Pairing { .. }) {
            return;
        }
        info!(accepted, "pairing: answered on the Mac");
        if let Some(ev) = self.pair.remote(accepted) {
            self.feed(ev);
        }
    }

    // ---------------------------------------------------------------- sending

    /// The session, unless it has already failed.
    fn session(&self) -> Option<&Session> {
        match self.dead {
            Some(_) => None,
            None => self.link.as_ref().map(|l| &l.session),
        }
    }

    /// The session, once it is paired (not while pairing).
    fn paired_session(&self) -> Option<&Session> {
        match self.machine.link() {
            Link::Connected => self.session(),
            _ => None,
        }
    }

    fn mark_dead(&mut self, reason: String) {
        if self.link.is_some() && self.dead.is_none() {
            self.dead = Some(reason);
        }
    }

    /// Sends one control message; a failure means the session is dead.
    /// `send_control` is not cancel-safe, so it is only ever awaited here,
    /// outside any `select!`.
    async fn control(&mut self, c: Control) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        if let Err(e) = session.send_control(&c).await {
            self.mark_dead(format!("control stream: {e:#}"));
        }
    }

    fn datagram(&self, d: &Datagram) {
        if let Some(s) = self.session()
            && let Err(e) = s.send_datagram(d)
        {
            debug!("datagram not sent: {e:#}");
        }
    }

    fn send_input(&self, cmd: InputCmd) {
        if self.ch.input.send(cmd).is_err() {
            error!("input thread is gone");
        }
    }

    fn ping(&mut self) {
        if self.session().is_none() {
            return;
        }
        self.ping_seq = self.ping_seq.wrapping_add(1);
        let sent_us = self.epoch.elapsed().as_micros() as u64;
        self.datagram(&Datagram::Ping {
            seq: self.ping_seq,
            sent_us,
        });
    }

    // ----------------------------------------------------------------- config

    /// Saves `change` to the settings file and applies it. If the file
    /// cannot be parsed it is left alone: the change applies in memory only
    /// and the user is told.
    fn update_config(&mut self, change: impl FnOnce(&mut Config) + Clone) {
        let cfg = self.config_file.change(&self.cfg, change);
        self.apply_config(cfg);
    }

    fn apply_config(&mut self, cfg: Config) {
        if cfg == self.cfg {
            return;
        }
        info!("settings changed");
        let pairing_changed = cfg.paired_peer != self.cfg.paired_peer;
        self.cfg = cfg.clone();
        self.ch
            .clip_link
            .max_bytes
            .store(cfg.clipboard_max_bytes, Ordering::Relaxed);
        self.send_input(InputCmd::ApplyConfig(cfg.clone()));
        self.feed(PcEvent::ConfigChanged(cfg));
        // A session pinned to a computer that is no longer the paired one must go.
        if pairing_changed
            && let Some(live) = &self.link
            && self.machine.link() == &Link::Connected
            && paired_fingerprint(&self.cfg) != Some(live.session.peer())
        {
            self.mark_dead("the paired computer changed".into());
        }
    }

    // ------------------------------------------------------------------- tray

    async fn on_tray(&mut self, cmd: TrayCmd) {
        match cmd {
            TrayCmd::TogglePause => {
                let paused = !self.cfg.paused;
                info!(paused, "pause toggled");
                self.update_config(move |c| c.paused = paused);
            }
            TrayCmd::OpenSettings => self.open_settings(),
            TrayCmd::ForgetPairing => {
                let tx = self.ch.events.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Ev::ForgetConfirmed(dialogs::confirm_forget().await));
                });
            }
            TrayCmd::Quit => self.begin_quit().await,
            // Only the Mac's menu offers it.
            TrayCmd::GrantAccessibility => {}
        }
    }

    fn open_settings(&mut self) {
        self.settings.open(
            || std::env::current_exe().and_then(|exe| Command::new(exe).arg("--settings").spawn()),
            bring_to_front,
        );
    }

    /// Puts the cursor back on the PC, then stops the input thread; the loop
    /// ends when it has (`InputGone`) or after `QUIT_TIMEOUT`.
    async fn begin_quit(&mut self) {
        if self.quit_by.is_some() {
            return;
        }
        info!("quitting");
        self.feed(PcEvent::ReturnHotkey);
        self.settle().await;
        self.send_input(InputCmd::Quit);
        self.settings.close();
        self.retry_at = None;
        self.quit_by = Some(Instant::now() + QUIT_TIMEOUT);
    }

    /// Updates the tray, and tells the clipboard bridge whether copies are wanted.
    fn publish(&mut self) {
        self.ch
            .clip_link
            .connected
            .store(self.paired_session().is_some(), Ordering::Relaxed);
        // Before any crossing: the machine crosses only once paired.
        self.drags.set_session_up(self.paired_session().is_some());
        let peer = self.link.as_ref().map(|l| l.peer_name.as_str());
        let model = TrayModel {
            status: status_text(
                self.machine.link(),
                peer,
                self.machine.mac_ready(),
                self.cfg.paused,
            ),
            latency: self.latency_text.clone(),
            paused: self.cfg.paused,
            needs_accessibility: false,
        };
        if self.last_model.as_ref() != Some(&model) {
            let _ = self.ch.tray.send(model.clone());
            self.last_model = Some(model);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_searching() {
        assert_eq!(
            status_text(&Link::Searching, None, true, false),
            "Searching for your Mac…"
        );
    }

    #[test]
    fn status_pairing_shows_code() {
        let link = Link::Pairing {
            code: "123 456".into(),
        };
        assert_eq!(
            status_text(&link, Some("MacBook"), true, false),
            "Pairing — check that both screens show 123 456"
        );
    }

    #[test]
    fn status_connected_names_peer() {
        assert_eq!(
            status_text(&Link::Connected, Some("Matt's MacBook Pro"), true, false),
            "Connected to Matt's MacBook Pro"
        );
        assert_eq!(
            status_text(&Link::Connected, None, true, true),
            "Connected to your Mac (paused)"
        );
    }

    #[test]
    fn status_connected_says_when_the_mac_needs_accessibility() {
        assert_eq!(
            status_text(&Link::Connected, Some("MacBook"), false, false),
            "Connected to MacBook — Mac needs Accessibility"
        );
        assert_eq!(
            status_text(&Link::Connected, None, false, true),
            "Connected to your Mac — Mac needs Accessibility (paused)"
        );
        assert_eq!(
            status_text(&Link::Searching, None, false, false),
            "Searching for your Mac…",
            "only a connected Mac's readiness is shown"
        );
    }

    fn pairing_machine() -> PcMachine {
        let mut m = PcMachine::new(Config::default());
        m.on(PcEvent::Connected {
            paired: false,
            code: "123 456".into(),
        });
        m
    }

    #[test]
    fn pair_answers_mac_first_waits_for_local() {
        let mut m = pairing_machine();
        let mut p = PairAnswers::default();
        assert!(p.remote(true).is_none(), "nothing to feed yet");
        let ev = p.local(true);
        assert_eq!(m.on(ev), vec![PcAction::PersistPairing]);
        assert!(!p.declined());
    }

    #[test]
    fn pair_answers_local_first_then_mac() {
        let mut m = pairing_machine();
        let mut p = PairAnswers::default();
        assert_eq!(m.on(p.local(true)), vec![]);
        let ev = p.remote(true).expect("local answer known");
        assert_eq!(m.on(ev), vec![PcAction::PersistPairing]);
    }

    #[test]
    fn pair_answers_decline_on_either_side() {
        let mut m = pairing_machine();
        let mut p = PairAnswers::default();
        assert!(p.remote(false).is_none());
        assert!(p.declined());
        assert_eq!(m.on(p.local(true)), vec![PcAction::Reconnect]);

        let mut p = PairAnswers::default();
        let _ = p.local(false);
        assert!(p.declined());
    }
}
