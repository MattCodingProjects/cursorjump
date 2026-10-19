//! The Mac app. The menu-bar icon runs on the main thread. A tokio loop on
//! the `cj-link` thread accepts the PC's sessions, runs pairing, and keeps
//! the menu, the settings file and the clipboard bridge up to date.
//!
//! Input never goes through the loop: the session readers (`link.rs`) send
//! it straight to the injection thread (`control.rs`), which reports back
//! only when to send `Leave`. The listener, the advertiser, the injection
//! thread and the clipboard bridge live as long as the process.

use std::fs::{File, OpenOptions, TryLockError};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use cj_core::clipboard::ClipboardData;
use cj_core::config::Config;
use cj_core::latency::LatencyStats;
use cj_core::proto::{Control, Datagram};
use cj_net::discovery::{Advertiser, DISCOVERY_PORT, QUIC_PORT};
use cj_net::session::{Listener, Session};
use cj_net::{Identity, Trust};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};
use tracing::{debug, error, info, warn};

use super::clipboard::{read_clipboard, spawn_clipboard_watcher, write_clipboard};
use super::control::{
    InjectEv, InjectMsg, set_user_interactive_qos, spawn_inject_thread, update_access,
};
use super::inject::Injector;
use super::link::{Live, run_pairing};
use super::permissions::{
    AccessState, access_state, may_relaunch, read_relaunch_stamp, request_post_event_access,
    write_relaunch_stamp,
};
use super::power::KeepAwake;
use crate::common::clipboard_bridge::{ClipboardLink, bridge_clipboard};
use crate::common::config_file::ConfigFile;
use crate::common::paired_fingerprint;
use crate::common::session_tasks::NetEv;
use crate::common::settings_window::SettingsWindow;
use crate::paths;
use crate::ui::dialogs;
use crate::ui::tray::{TrayCmd, TrayModel, latency_text, run_tray};

const PING_EVERY: Duration = Duration::from_millis(100);
const CONFIG_POLL: Duration = Duration::from_millis(500);
/// The latency readout is recomputed this often.
const LATENCY_REFRESH: Duration = Duration::from_secs(1);
/// How often the Accessibility permission is checked (menu, PC, injection).
const ACCESS_POLL: Duration = Duration::from_secs(2);
/// In the config directory: when CursorJump last relaunched itself to pick
/// up the Accessibility permission.
const RELAUNCH_STAMP: &str = "relaunch.stamp";
/// How long Quit waits for the injection thread to release everything.
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);
/// At quit, time for the endpoint to send CONNECTION_CLOSE, so the PC takes
/// its cursor back at once rather than after the idle timeout.
const CLOSE_FLUSH: Duration = Duration::from_millis(100);

/// Runs the Mac app until Quit. Exits early (Ok) if another instance is running.
pub fn run_mac() -> Result<()> {
    let dir = paths::config_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let Some(_instance) = InstanceLock::acquire(&dir.join("cursorjump.lock"))? else {
        info!("CursorJump is already running; exiting");
        return Ok(());
    };
    let mut config_file = ConfigFile::new(paths::config_path()?, dialogs::notify_error);
    let identity = Identity::load_or_create(&dir).context("loading this Mac's identity")?;
    let cfg = config_file.load_initial();
    let name = computer_name();
    let fingerprint = identity.fingerprint();
    info!(
        config = %config_file.path().display(),
        fingerprint = %fingerprint.to_hex(),
        name = %name,
        "CursorJump starting"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        // Motion datagrams pass through a worker on their way to cj-inject.
        .on_thread_start(set_user_interactive_qos)
        .enable_all()
        .build()
        .context("starting the network runtime")?;
    let trust = paired_fingerprint(&cfg).map_or(Trust::PairingMode, Trust::Pinned);
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, QUIC_PORT));
    let listener = runtime.block_on(Listener::bind(&identity, trust, addr, &name))?;
    info!(addr = %listener.local_addr(), "listening for the PC");
    let advertiser = match Advertiser::start(&name, QUIC_PORT, &fingerprint, DISCOVERY_PORT) {
        Ok(a) => {
            info!("advertising on the local network");
            Some(a)
        }
        Err(e) => {
            warn!("advertising failed: {e:#}; the PC can still connect with manual_peer_addr");
            None
        }
    };

    let (events, events_rx) = unbounded_channel();
    let inject_events = events.clone();
    let state = access_state();
    let access = Arc::new(AtomicBool::new(state.ready()));
    if !state.ready() {
        warn!(
            "no Accessibility permission yet: the PC cannot control this Mac until it is granted"
        );
    } else if !state.can_post {
        warn!(
            ?state,
            "Accessibility is on, but posting events is not confirmed"
        );
    }
    let inject = spawn_inject_thread(Injector::new()?, access.clone(), move |ev| {
        let _ = inject_events.send(Ev::Inject(ev));
    })?;
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

    let channels = Channels {
        inject,
        access,
        clip_write,
        clip_link,
        tray: model_tx,
        events: events.clone(),
    };
    std::thread::Builder::new()
        .name("cj-link".into())
        .spawn(move || {
            let app = App::new(cfg, config_file, listener, advertiser, channels);
            runtime.block_on(app.run(events_rx));
            // Dropping the app drops the tray's model sender, which ends the process.
        })
        .context("starting the link thread")?;
    info!("menu bar icon starting");
    run_tray(model_rx, move |cmd| {
        let _ = events.send(Ev::Tray(cmd));
    })
}

/// An exclusive lock on a file, held for the life of the process, so that
/// launchd and a manual launch never run two copies (they would fight over
/// the listening port).
struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// None if another process holds the lock.
    fn acquire(path: &Path) -> Result<Option<InstanceLock>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        // An exclusive, non-blocking flock(2), released when the process ends.
        match file.try_lock() {
            Ok(()) => Ok(Some(InstanceLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }
    }
}

/// This Mac's name as the user set it in System Settings.
fn computer_name() -> String {
    let scutil = Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok());
    let host = || {
        Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
    };
    [scutil, host()]
        .into_iter()
        .flatten()
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "Mac".into())
}

/// Everything the loop reacts to, apart from its own timers.
pub(super) enum Ev {
    Tray(TrayCmd),
    ForgetConfirmed(bool),
    /// A PC completed the handshake.
    Accepted(Session),
    /// The pairing on session `sid` finished: true if both users accepted.
    PairDone {
        sid: u64,
        result: Result<bool>,
    },
    Net {
        sid: u64,
        ev: NetEv,
    },
    Inject(InjectEv),
    /// A local copy that passed the clipboard gate.
    LocalClipboard(ClipboardData),
    /// The result of Accessibility poll number `seq`.
    Access {
        seq: u64,
        state: AccessState,
    },
}

/// Resolves when SIGTERM arrives (launchctl bootout, logout); never without a handler.
async fn terminated(sigterm: &mut Option<Signal>) {
    if let Some(s) = sigterm
        && s.recv().await.is_some()
    {
        return;
    }
    std::future::pending().await
}

/// The connection as the menu shows it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LinkStatus<'a> {
    Waiting,
    Pairing,
    Connected(&'a str),
}

/// The menu's status line.
fn status_text(link: LinkStatus, needs_accessibility: bool) -> String {
    match link {
        // The pairing dialog is up; that comes first.
        LinkStatus::Pairing => "Pairing…".into(),
        // Nothing works without it; the menu offers "Grant Accessibility…".
        _ if needs_accessibility => "Accessibility needed".into(),
        LinkStatus::Waiting => "Waiting for your PC".into(),
        LinkStatus::Connected(name) => format!("Connected to {name}"),
    }
}

/// A pairing in progress. Dropping it stops the pairing and closes the session.
struct Pairing {
    sid: u64,
    session: Option<Session>,
    task: JoinHandle<()>,
}

impl Pairing {
    /// The session, kept open, once the pairing succeeded.
    fn into_session(mut self) -> Option<Session> {
        self.session.take()
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(s) = &self.session {
            s.close();
        }
    }
}

enum Conn {
    Waiting,
    Pairing(Pairing),
    Live(Live),
}

/// The loop's connections to the other threads.
struct Channels {
    inject: crossbeam_channel::Sender<InjectMsg>,
    /// Whether events may be posted, shared with the injection thread.
    access: Arc<AtomicBool>,
    clip_write: crossbeam_channel::Sender<ClipboardData>,
    clip_link: Arc<ClipboardLink>,
    tray: std::sync::mpsc::Sender<TrayModel>,
    events: UnboundedSender<Ev>,
}

struct App {
    cfg: Config,
    config_file: ConfigFile,
    listener: Arc<Listener>,
    /// Dropped off the async path at quit: its Drop blocks for up to ~1.25 s.
    advertiser: Option<Advertiser>,
    ch: Channels,
    conn: Conn,
    /// Held while a paired session is live.
    awake: Option<KeepAwake>,
    next_sid: u64,
    /// Set while a pairing dialog is on screen, which can outlive its session.
    pair_dialog_open: Arc<AtomicBool>,
    latency: LatencyStats,
    latency_text: String,
    ping_seq: u32,
    /// Zero point for ping timestamps.
    epoch: std::time::Instant,
    settings: SettingsWindow,
    last_model: Option<TrayModel>,
    /// The latest Accessibility poll started, and the latest one applied.
    access_polls: (u64, u64),
    quit_by: Option<Instant>,
    done: bool,
    /// Exit non-zero so launchd (KeepAlive SuccessfulExit=false) starts a
    /// fresh copy: the injection thread stopped without Quit, or the
    /// Accessibility grant needs a new process.
    restart: bool,
}

impl App {
    fn new(
        cfg: Config,
        config_file: ConfigFile,
        listener: Listener,
        advertiser: Option<Advertiser>,
        ch: Channels,
    ) -> App {
        let latency = LatencyStats::new();
        App {
            cfg,
            config_file,
            listener: Arc::new(listener),
            advertiser,
            ch,
            conn: Conn::Waiting,
            awake: None,
            next_sid: 0,
            pair_dialog_open: Arc::new(AtomicBool::new(false)),
            latency_text: latency_text(&latency),
            latency,
            ping_seq: 0,
            epoch: std::time::Instant::now(),
            settings: SettingsWindow::default(),
            last_model: None,
            access_polls: (0, 0),
            quit_by: None,
            done: false,
            restart: false,
        }
    }

    async fn run(mut self, mut events: UnboundedReceiver<Ev>) {
        self.spawn_acceptor();
        let mut ping = interval(PING_EVERY);
        ping.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut config_poll = interval(CONFIG_POLL);
        let mut latency_refresh = interval(LATENCY_REFRESH);
        let mut access_poll = interval(ACCESS_POLL);
        access_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut sigterm = signal(SignalKind::terminate())
            .inspect_err(|e| warn!("SIGTERM will not quit cleanly: {e}"))
            .ok();
        self.publish();
        loop {
            let quit_by = self.quit_by;
            tokio::select! {
                ev = events.recv() => match ev {
                    Some(ev) => self.handle(ev).await,
                    None => break,
                },
                _ = ping.tick() => self.ping(),
                _ = config_poll.tick() => {
                    if let Some(cfg) = self.config_file.poll() {
                        self.apply_config(cfg);
                    }
                }
                _ = latency_refresh.tick() => self.latency_text = latency_text(&self.latency),
                _ = access_poll.tick() => self.poll_access(),
                () = terminated(&mut sigterm) => {
                    info!("SIGTERM received");
                    self.begin_quit();
                }
                _ = sleep_until(quit_by.unwrap_or_else(Instant::now)), if quit_by.is_some() => {
                    warn!("injection thread did not stop in time");
                    break;
                }
            }
            if self.done {
                break;
            }
            self.publish();
        }
        self.set_conn(Conn::Waiting);
        // Every way out closes the settings window.
        self.settings.close();
        // The advertiser's Drop blocks (mDNS goodbye), so it runs off the
        // async workers; meanwhile the endpoint sends the session's
        // CONNECTION_CLOSE before the process ends.
        let advertiser = self.advertiser.take();
        let _ = tokio::join!(
            tokio::time::sleep(CLOSE_FLUSH),
            tokio::task::spawn_blocking(move || drop(advertiser)),
        );
        if self.restart {
            error!("exiting with an error so that CursorJump restarts");
            std::process::exit(1);
        }
        info!("stopped");
    }

    /// Feeds every session that completes the handshake into the loop.
    fn spawn_acceptor(&self) {
        let (listener, tx) = (self.listener.clone(), self.ch.events.clone());
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(session) => {
                        if tx.send(Ev::Accepted(session)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        error!("no longer accepting connections: {e:#}");
                        return;
                    }
                }
            }
        });
    }

    async fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Tray(cmd) => self.on_tray(cmd).await,
            Ev::ForgetConfirmed(yes) => {
                if yes {
                    info!("forgetting the paired computer");
                    self.update_config(|c| c.paired_peer = None);
                }
            }
            Ev::Accepted(session) => self.on_accepted(session).await,
            Ev::PairDone { sid, result } => self.on_pair_done(sid, result).await,
            Ev::Net { sid, ev } => {
                if self.live().is_some_and(|l| l.sid == sid) {
                    self.on_net(ev);
                }
            }
            Ev::Inject(ev) => self.on_inject(ev).await,
            Ev::LocalClipboard(d) => {
                // The bridge already gated it; the session may have ended since.
                if let Some(live) = self.live() {
                    live.send_clipboard(d);
                }
            }
            Ev::Access { seq, state } => self.on_access(seq, state).await,
        }
    }

    fn on_net(&mut self, ev: NetEv) {
        match ev {
            NetEv::Control(c) => debug!(?c, "ignoring unexpected control message"),
            NetEv::Pong { rtt, at } => self.latency.record(at, rtt),
            NetEv::Clipboard(d) => {
                if !d.is_empty() && self.ch.clip_write.send(d).is_err() {
                    warn!("clipboard bridge is gone; clipboard from the PC dropped");
                }
            }
            NetEv::Closed(reason) => {
                info!("disconnected: {reason}");
                self.set_conn(Conn::Waiting);
            }
        }
    }

    async fn on_inject(&mut self, ev: InjectEv) {
        match ev {
            InjectEv::SendLeave { sid, frac } => {
                let Some(session) = self
                    .live()
                    .filter(|l| l.sid == sid)
                    .map(|l| l.session.clone())
                else {
                    return;
                };
                // Awaited here, outside any select!: send_control is not cancel-safe.
                if let Err(e) = session.send_control(&Control::Leave { frac }).await {
                    info!("disconnected: control stream: {e:#}");
                    self.set_conn(Conn::Waiting);
                }
            }
            InjectEv::Controlled { sid, on } => {
                if on {
                    info!(sid, "the PC's cursor came over");
                } else {
                    info!(sid, "the PC's cursor went back");
                }
            }
            InjectEv::Stopped => {
                if self.quit_by.is_none() {
                    error!("injection thread stopped; quitting so that CursorJump restarts");
                    self.restart = true;
                }
                self.done = true;
            }
        }
    }

    // ------------------------------------------------------------ sessions

    fn has_access(&self) -> bool {
        self.ch.access.load(Ordering::Relaxed)
    }

    fn live(&self) -> Option<&Live> {
        match &self.conn {
            Conn::Live(l) => Some(l),
            _ => None,
        }
    }

    fn take_sid(&mut self) -> u64 {
        let sid = self.next_sid;
        self.next_sid += 1;
        sid
    }

    /// Replaces the connection; the old one is closed (a live one first
    /// releases everything on the injection thread).
    fn set_conn(&mut self, conn: Conn) {
        self.conn = conn;
        match (&self.conn, &self.awake) {
            (Conn::Live(_), None) => match KeepAwake::start() {
                Ok(k) => self.awake = Some(k),
                Err(e) => warn!("could not keep the Mac awake: {e:#}"),
            },
            (Conn::Live(_), Some(_)) => {}
            _ => self.awake = None,
        }
        self.latency = LatencyStats::new();
        self.latency_text = latency_text(&self.latency);
    }

    async fn on_accepted(&mut self, session: Session) {
        if self.quit_by.is_some() {
            session.close();
            return;
        }
        let peer = session.peer();
        match paired_fingerprint(&self.cfg) {
            Some(fp) if peer == fp => {
                // A newer session replaces the current one.
                self.set_conn(Conn::Waiting);
                self.start_live(session).await;
            }
            Some(_) => {
                // The listener's trust lagged a change of the paired computer.
                info!("refusing a computer that is not the paired one");
                session.close();
            }
            None if self.pair_dialog_open.load(Ordering::SeqCst) => {
                info!("a pairing dialog is still open; refusing a new pairing for now");
                session.close();
            }
            None => {
                self.set_conn(Conn::Waiting);
                self.start_pairing(session);
            }
        }
    }

    async fn start_live(&mut self, session: Session) {
        let sid = self.take_sid();
        let live = Live::start(
            sid,
            session,
            self.cfg.clipboard_max_bytes,
            self.epoch,
            self.ch.inject.clone(),
            &self.ch.events,
        );
        info!(peer = %live.peer_name, "connected");
        self.set_conn(Conn::Live(live));
        self.send_mac_ready().await;
    }

    /// Tells the PC whether it can take the cursor here.
    async fn send_mac_ready(&mut self) {
        let Some(session) = self.live().map(|l| l.session.clone()) else {
            return;
        };
        let ready = self.has_access();
        // Awaited here, outside any select!: send_control is not cancel-safe.
        if let Err(e) = session.send_control(&Control::MacReady { ready }).await {
            info!("disconnected: control stream: {e:#}");
            self.set_conn(Conn::Waiting);
        }
    }

    fn start_pairing(&mut self, session: Session) {
        let sid = self.take_sid();
        let peer = session.peer_name();
        info!(peer = %peer, "pairing with a new computer");
        let (s, tx) = (session.clone(), self.ch.events.clone());
        let dialog_open = self.pair_dialog_open.clone();
        let task = tokio::spawn(async move {
            // The dialog runs in its own task: it stays on screen until
            // answered, even if this pairing is abandoned meanwhile.
            let ask = move |code: String| {
                let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
                dialog_open.store(true, Ordering::SeqCst);
                tokio::spawn(async move {
                    let accepted = dialogs::ask_pairing(&code, &peer).await;
                    dialog_open.store(false, Ordering::SeqCst);
                    let _ = answer_tx.send(accepted);
                });
                async move { answer_rx.await.unwrap_or(false) }
            };
            let result = run_pairing(&s, ask).await;
            let _ = tx.send(Ev::PairDone { sid, result });
        });
        self.set_conn(Conn::Pairing(Pairing {
            sid,
            session: Some(session),
            task,
        }));
    }

    async fn on_pair_done(&mut self, sid: u64, result: Result<bool>) {
        if !matches!(&self.conn, Conn::Pairing(p) if p.sid == sid) {
            return;
        }
        // Waiting directly: a pairing holds no KeepAwake and no latency samples.
        let Conn::Pairing(pairing) = std::mem::replace(&mut self.conn, Conn::Waiting) else {
            return;
        };
        match result {
            Ok(true) => {
                let Some(session) = pairing.into_session() else {
                    return;
                };
                info!(peer = %session.peer_name(), "paired");
                let fp = session.peer().to_hex();
                // Also pins the listener to this PC (apply_config).
                self.update_config(|c| c.paired_peer = Some(fp));
                if self.quit_by.is_some() {
                    session.close();
                } else {
                    // The PC goes on using the same session.
                    self.start_live(session).await;
                }
            }
            // Dropping `pairing` closes the session; the PC retries on its own schedule.
            Ok(false) => info!("pairing declined"),
            Err(e) => info!("pairing failed: {e:#}"),
        }
    }

    fn ping(&mut self) {
        let Some(live) = self.live() else {
            return;
        };
        let seq = self.ping_seq.wrapping_add(1);
        let sent_us = self.epoch.elapsed().as_micros() as u64;
        if let Err(e) = live.session.send_datagram(&Datagram::Ping { seq, sent_us }) {
            debug!("ping not sent: {e:#}");
        }
        self.ping_seq = seq;
    }

    /// Asks the system about the Accessibility permission off this thread:
    /// the queries take milliseconds, which must not delay a `Leave`.
    fn poll_access(&mut self) {
        self.access_polls.0 += 1;
        let (seq, tx) = (self.access_polls.0, self.ch.events.clone());
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(Ev::Access {
                seq,
                state: access_state(),
            });
        });
    }

    /// Applies a poll's result: the injection thread's view (losing the
    /// permission sends the PC's cursor back), the PC's, and a relaunch if
    /// the grant needs a new process.
    async fn on_access(&mut self, seq: u64, state: AccessState) {
        // A late result must not undo a newer one.
        if seq <= self.access_polls.1 {
            return;
        }
        self.access_polls.1 = seq;
        let ready = state.ready();
        if !update_access(&self.ch.access, ready, &self.ch.inject) {
            return;
        }
        info!(granted = ready, ?state, "Accessibility permission changed");
        self.send_mac_ready().await;
        if ready && !state.can_post {
            self.relaunch_for_access();
        }
    }

    /// Accessibility was just switched on, but this process may still be
    /// refused posting events until it restarts: quit so that launchd
    /// starts a new copy, unless that was done less than a minute ago.
    fn relaunch_for_access(&mut self) {
        warn!("Accessibility granted, but posting events is not allowed yet");
        if self.quit_by.is_some() {
            return;
        }
        let stamp = match paths::config_dir() {
            Ok(dir) => dir.join(RELAUNCH_STAMP),
            Err(e) => {
                warn!("not restarting: {e:#}");
                return;
            }
        };
        let now = SystemTime::now();
        if !may_relaunch(read_relaunch_stamp(&stamp), now) {
            warn!(
                "not restarting again so soon; if the PC's input does not arrive, quit and reopen CursorJump"
            );
            return;
        }
        if let Err(e) = write_relaunch_stamp(&stamp, now) {
            // Without the stamp a relaunch loop is possible: do not risk it.
            warn!("not restarting: could not write {}: {e}", stamp.display());
            return;
        }
        info!("restarting CursorJump to pick up the Accessibility permission");
        self.restart = true;
        self.begin_quit();
    }

    // ----------------------------------------------------------------- config

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
        self.cfg = cfg;
        self.ch
            .clip_link
            .max_bytes
            .store(self.cfg.clipboard_max_bytes, Ordering::Relaxed);
        if !pairing_changed {
            return;
        }
        let paired = paired_fingerprint(&self.cfg);
        let trust = paired.map_or(Trust::PairingMode, Trust::Pinned);
        if let Err(e) = self.listener.set_trust(trust) {
            error!("could not update whom to accept: {e:#}");
        }
        // A session with a computer that is not (or no longer) the paired one must go.
        let keep = match (&self.conn, paired) {
            (Conn::Waiting, _) => true,
            (Conn::Pairing(_), None) => true,
            (Conn::Live(l), Some(fp)) => l.session.peer() == fp,
            _ => false,
        };
        if !keep {
            info!("the paired computer changed; closing the session");
            self.set_conn(Conn::Waiting);
        }
    }

    // ------------------------------------------------------------------- tray

    async fn on_tray(&mut self, cmd: TrayCmd) {
        match cmd {
            TrayCmd::OpenSettings => self.open_settings(),
            TrayCmd::ForgetPairing => {
                let tx = self.ch.events.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Ev::ForgetConfirmed(dialogs::confirm_forget().await));
                });
            }
            TrayCmd::GrantAccessibility => {
                info!("asking for the Accessibility permission");
                // Runs `open`, which is not instant.
                tokio::task::spawn_blocking(request_post_event_access);
            }
            TrayCmd::Quit => self.begin_quit(),
            // Never sent on macOS: the PC owns crossing, so the Mac's menu has
            // no pause item. The arm only keeps the shared enum's match exhaustive.
            TrayCmd::TogglePause => {}
        }
    }

    fn open_settings(&mut self) {
        // macOS: an already open window is not brought to the front.
        self.settings.open(
            || std::env::current_exe().and_then(|exe| Command::new(exe).arg("--settings").spawn()),
            |_| {},
        );
    }

    /// Releases everything and ends the session, then stops the injection
    /// thread; the loop ends when it has (`Stopped`) or after `QUIT_TIMEOUT`.
    fn begin_quit(&mut self) {
        if self.quit_by.is_some() {
            return;
        }
        info!("quitting");
        self.set_conn(Conn::Waiting);
        if self.ch.inject.send(InjectMsg::Quit).is_err() {
            self.done = true;
        }
        self.settings.close();
        self.quit_by = Some(Instant::now() + QUIT_TIMEOUT);
    }

    /// Updates the menu, and tells the clipboard bridge whether copies are wanted.
    fn publish(&mut self) {
        self.ch
            .clip_link
            .connected
            .store(self.live().is_some(), Ordering::Relaxed);
        let link = match &self.conn {
            Conn::Waiting => LinkStatus::Waiting,
            Conn::Pairing(_) => LinkStatus::Pairing,
            Conn::Live(l) => LinkStatus::Connected(&l.peer_name),
        };
        let model = TrayModel {
            status: status_text(link, !self.has_access()),
            latency: self.latency_text.clone(),
            paused: false,
            needs_accessibility: !self.has_access(),
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
    fn status_follows_the_connection() {
        assert_eq!(
            status_text(LinkStatus::Waiting, false),
            "Waiting for your PC"
        );
        assert_eq!(status_text(LinkStatus::Pairing, false), "Pairing…");
        assert_eq!(
            status_text(LinkStatus::Connected("GAMING-PC"), false),
            "Connected to GAMING-PC"
        );
    }

    #[test]
    fn status_asks_for_accessibility_except_while_pairing() {
        assert_eq!(
            status_text(LinkStatus::Waiting, true),
            "Accessibility needed"
        );
        assert_eq!(
            status_text(LinkStatus::Connected("GAMING-PC"), true),
            "Accessibility needed"
        );
        assert_eq!(status_text(LinkStatus::Pairing, true), "Pairing…");
    }

    #[test]
    fn a_second_instance_is_refused_until_the_first_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursorjump.lock");
        let first = InstanceLock::acquire(&path).unwrap();
        assert!(first.is_some(), "the first instance gets the lock");
        assert!(
            InstanceLock::acquire(&path).unwrap().is_none(),
            "a second instance is refused"
        );
        drop(first);
        // A process another test spawns at that moment briefly shares the
        // lock's descriptor (until its exec closes it), so allow a moment.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let again = loop {
            let lock = InstanceLock::acquire(&path).unwrap();
            if lock.is_some() || std::time::Instant::now() >= deadline {
                break lock;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            again.is_some(),
            "free again once the first instance is gone"
        );
    }

    #[test]
    fn computer_name_is_never_empty() {
        assert!(!computer_name().is_empty());
    }
}
