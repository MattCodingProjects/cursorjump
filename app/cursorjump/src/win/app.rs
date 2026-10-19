//! The PC app. The tray runs on the main thread. A tokio loop on the
//! `cj-link` thread finds the Mac, keeps one session to it and drives
//! `PcMachine`: it feeds it events and carries out the actions it returns.
//!
//! The input thread and the clipboard watcher are started once and outlive
//! every reconnect. Two bridge threads (`bridges.rs`) move their output into
//! the loop; the session itself lives in `link.rs`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use cj_core::clipboard::ClipboardData;
use cj_core::config::Config;
use cj_core::latency::LatencyStats;
use cj_core::pc_state::{Link, PcAction, PcEvent, PcMachine, Where};
use cj_core::proto::{Control, Datagram};
use cj_net::Identity;
use cj_net::session::Session;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

use super::bridges::{ClipboardLink, bridge_clipboard, bridge_input};
use super::clipboard::spawn_clipboard_watcher;
use super::input_thread::{InputCmd, InputMsg, spawn_input_thread};
use super::link::{Backoff, Established, Live, NetEv, establish, paired_fingerprint};
use super::settings_window::SettingsWindow;
use crate::paths;
use crate::ui::dialogs;
use crate::ui::tray::{TrayCmd, TrayLoop, TrayModel, latency_text};

const PING_EVERY: Duration = Duration::from_millis(100);
const CONFIG_POLL: Duration = Duration::from_millis(500);
/// The latency readout is recomputed this often.
const LATENCY_REFRESH: Duration = Duration::from_secs(1);
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
    let mut config_file = ConfigFile::new(paths::config_path()?);
    let identity = Identity::load_or_create(&dir).context("loading this PC's identity")?;
    let cfg = config_file.load_initial();
    info!(
        config = %config_file.path.display(),
        fingerprint = %identity.fingerprint().to_hex(),
        "CursorJump starting"
    );

    // Before the input thread: see `TrayLoop::new`.
    let tray = TrayLoop::new();
    let (input, input_rx) = spawn_input_thread(cfg.clone());
    let (events, events_rx) = unbounded_channel();
    bridge_input(input_rx, events.clone())?;
    let clip_link = Arc::new(ClipboardLink::new(cfg.clipboard_max_bytes));
    let clip_write =
        bridge_clipboard(spawn_clipboard_watcher(), events.clone(), clip_link.clone())?;
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
}

/// The tray's status line.
fn status_text(link: &Link, peer_name: Option<&str>, paused: bool) -> String {
    let base = match link {
        Link::Searching => "Searching for your Mac…".to_string(),
        Link::Pairing { code } => format!("Pairing — check that both screens show {code}"),
        Link::Connected => format!("Connected to {}", peer_name.unwrap_or("your Mac")),
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

/// Shows an error box without blocking the caller.
fn notify_user(text: String) {
    let spawned = std::thread::Builder::new()
        .name("cj-error-box".into())
        .spawn(move || dialogs::show_error(&text));
    if let Err(e) = spawned {
        error!("could not show an error box: {e}");
    }
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

/// The settings file as the loop sees it. A file that cannot be parsed is
/// never applied and never written over.
struct ConfigFile {
    path: PathBuf,
    stamp: Option<SystemTime>,
}

impl ConfigFile {
    fn new(path: PathBuf) -> Self {
        ConfigFile {
            stamp: modified(&path),
            path,
        }
    }

    /// The config to start with. An unreadable file is reported and the
    /// defaults are used in memory only.
    fn load_initial(&mut self) -> Config {
        match Config::try_load(&self.path) {
            Ok(cfg) => cfg.unwrap_or_default(),
            Err(e) => {
                warn!("{e:#}; using default settings until the file is fixed");
                notify_user(format!(
                    "CursorJump's settings file can't be read:\n\n{e:#}\n\n\
                     It is using default settings and won't change the file until \
                     it is fixed or deleted."
                ));
                Config::default()
            }
        }
    }

    /// The new config if the file changed and parses. None keeps the current one.
    fn poll(&mut self) -> Option<Config> {
        let stamp = modified(&self.path);
        if stamp == self.stamp {
            return None;
        }
        self.stamp = stamp;
        match Config::try_load(&self.path) {
            Ok(Some(cfg)) => Some(cfg),
            Ok(None) => {
                info!("settings file removed; keeping the current settings");
                None
            }
            Err(e) => {
                warn!("{e:#}; keeping the current settings");
                None
            }
        }
    }

    /// Applies `change` to the file as it is on disk (the settings window
    /// may have written it) and saves it. A missing file starts from
    /// `current`. Errs, leaving the file untouched, if it cannot be parsed.
    fn update(&mut self, current: &Config, change: impl FnOnce(&mut Config)) -> Result<Config> {
        let mut cfg = Config::try_load(&self.path)?.unwrap_or_else(|| current.clone());
        change(&mut cfg);
        let cfg = cfg.clamped();
        cfg.save(&self.path)
            .with_context(|| format!("saving {}", self.path.display()))?;
        self.stamp = modified(&self.path);
        Ok(cfg)
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
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
}

impl App {
    fn new(cfg: Config, config_file: ConfigFile, identity: Identity, ch: Channels) -> App {
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
        }
    }

    async fn run(mut self, mut events: UnboundedReceiver<Ev>) {
        let mut ping = interval(PING_EVERY);
        ping.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut config_poll = interval(CONFIG_POLL);
        let mut latency_refresh = interval(LATENCY_REFRESH);
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
            PcAction::EnterMac { mac_edge, frac } => {
                self.send_input(InputCmd::EnterMac);
                self.control(Control::Enter {
                    edge: mac_edge,
                    frac,
                })
                .await;
            }
            PcAction::ReturnToPc { pc_frac } => self.send_input(InputCmd::ReturnToPc { pc_frac }),
            PcAction::SendReleaseAll => self.control(Control::ReleaseAll).await,
            PcAction::Reconnect => self.reconnect(),
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
            NetEv::Control(Control::Leave { frac }) => self.feed(PcEvent::MacLeave { frac }),
            NetEv::Control(Control::PairConfirm { accepted }) => {
                self.on_remote_pair_answer(accepted)
            }
            NetEv::Control(other) => debug!(?other, "ignoring unexpected control message"),
            NetEv::Pong { rtt, at } => self.latency.record(at, rtt),
            NetEv::Clipboard(d) => {
                if self.paired_session().is_some()
                    && !d.is_empty()
                    && self.ch.clip_write.send(d).is_err()
                {
                    warn!("clipboard bridge is gone; clipboard from the Mac dropped");
                }
            }
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
        match self.config_file.update(&self.cfg, change.clone()) {
            Ok(cfg) => self.apply_config(cfg),
            Err(e) => {
                warn!("not saving the settings file: {e:#}");
                notify_user(format!(
                    "CursorJump could not save this change, because its settings file \
                     can't be read:\n\n{e:#}\n\nThe change applies until CursorJump \
                     restarts. Fix or delete the file to save settings again."
                ));
                let mut cfg = self.cfg.clone();
                change(&mut cfg);
                self.apply_config(cfg.clamped());
            }
        }
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
        }
    }

    fn open_settings(&mut self) {
        self.settings.open(|| {
            std::env::current_exe().and_then(|exe| Command::new(exe).arg("--settings").spawn())
        });
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
        let peer = self.link.as_ref().map(|l| l.peer_name.as_str());
        let model = TrayModel {
            status: status_text(self.machine.link(), peer, self.cfg.paused),
            latency: self.latency_text.clone(),
            paused: self.cfg.paused,
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
            status_text(&Link::Searching, None, false),
            "Searching for your Mac…"
        );
    }

    #[test]
    fn status_pairing_shows_code() {
        let link = Link::Pairing {
            code: "123 456".into(),
        };
        assert_eq!(
            status_text(&link, Some("MacBook"), false),
            "Pairing — check that both screens show 123 456"
        );
    }

    #[test]
    fn status_connected_names_peer() {
        assert_eq!(
            status_text(&Link::Connected, Some("Matt's MacBook Pro"), false),
            "Connected to Matt's MacBook Pro"
        );
        assert_eq!(
            status_text(&Link::Connected, None, true),
            "Connected to your Mac (paused)"
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

    fn file_with(dir: &tempfile::TempDir, text: &str) -> ConfigFile {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        ConfigFile { path, stamp: None }
    }

    #[test]
    fn config_poll_applies_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = file_with(&dir, "paused = true\n");
        assert!(f.poll().is_some_and(|c| c.paused));
        assert!(f.poll().is_none(), "unchanged file is not re-applied");
    }

    #[test]
    fn config_poll_keeps_current_on_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = file_with(&dir, "paired_peer = \"ab\npaused = ");
        assert!(f.poll().is_none());
    }

    #[test]
    fn config_update_never_overwrites_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let broken = "paired_peer = \"ab\npaused = ";
        let mut f = file_with(&dir, broken);
        let current = Config {
            paired_peer: Some("ab".into()),
            ..Config::default()
        };
        assert!(f.update(&current, |c| c.paused = true).is_err());
        assert_eq!(std::fs::read_to_string(&f.path).unwrap(), broken);
    }

    #[test]
    fn config_update_merges_onto_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = file_with(&dir, "mac_side = \"Left\"\n");
        let cfg = f.update(&Config::default(), |c| c.paused = true).unwrap();
        assert!(cfg.paused);
        assert_eq!(cfg.mac_side, cj_core::proto::Side::Left);
        assert_eq!(Config::try_load(&f.path).unwrap(), Some(cfg));
    }

    #[test]
    fn config_update_without_a_file_starts_from_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = ConfigFile::new(path);
        let current = Config {
            pointer_speed: 2.0,
            ..Config::default()
        };
        let cfg = f.update(&current, |c| c.paused = true).unwrap();
        assert_eq!(cfg.pointer_speed, 2.0);
        assert!(cfg.paused);
    }
}
