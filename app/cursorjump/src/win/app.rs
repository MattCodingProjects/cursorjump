//! The PC app. The tray runs on the main thread. A tokio loop on the
//! `cj-link` thread finds the Mac, keeps one session to it and drives
//! `PcMachine`: it feeds it events and carries out the actions it returns.
//!
//! The input thread and the clipboard watcher are started once and outlive
//! every reconnect. Two bridge threads move their output into the loop.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use cj_core::clipboard::{ClipboardData, EchoGuard, within_limit};
use cj_core::config::Config;
use cj_core::latency::LatencyStats;
use cj_core::pc_state::{Link, PcAction, PcEvent, PcMachine, Where};
use cj_core::proto::{Control, Datagram};
use cj_net::discovery::{DISCOVERY_PORT, QUIC_PORT, choose_peer, discover};
use cj_net::session::{Session, connect};
use cj_net::{Fingerprint, Identity, Trust};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

use super::clipboard::{read_clipboard, spawn_clipboard_watcher, write_clipboard};
use super::input_thread::{InputCmd, InputMsg, spawn_input_thread};
use crate::paths;
use crate::ui::dialogs;
use crate::ui::tray::{TrayCmd, TrayLoop, TrayModel, latency_text};

/// `discover` needs at least 3 s to get through mDNS and the broadcast fallback.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(3);
const PING_EVERY: Duration = Duration::from_millis(100);
const CONFIG_POLL: Duration = Duration::from_millis(500);
/// The latency readout is recomputed this often.
const LATENCY_REFRESH: Duration = Duration::from_secs(1);
/// After either side declines pairing, wait this long before asking again.
const PAIR_DECLINED_RETRY: Duration = Duration::from_secs(30);
/// How long Quit waits for the input thread to put the cursor back.
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Pongs reporting a longer round trip are ignored as bogus.
const MAX_RTT: Duration = Duration::from_secs(10);

/// Runs the PC app until Quit. Exits early (Ok) if another instance is running.
pub fn run_pc() -> Result<()> {
    let Some(_instance) = SingleInstance::acquire()? else {
        info!("CursorJump is already running; exiting");
        return Ok(());
    };
    let dir = paths::config_dir()?;
    let cfg_path = paths::config_path()?;
    let identity = Identity::load_or_create(&dir).context("loading this PC's identity")?;
    let cfg = Config::load(&cfg_path);
    info!(
        config = %cfg_path.display(),
        fingerprint = %identity.fingerprint().to_hex(),
        "CursorJump starting"
    );

    // Before the input thread: see `TrayLoop::new`.
    let tray = TrayLoop::new();
    let (input, input_rx) = spawn_input_thread(cfg.clone());
    let (events, events_rx) = unbounded_channel();
    bridge_input(input_rx, events.clone())?;
    let clip_write = bridge_clipboard(spawn_clipboard_watcher(), events.clone())?;
    let (model_tx, model_rx) = std::sync::mpsc::channel();

    let app = App::new(
        cfg,
        cfg_path,
        identity,
        input,
        clip_write,
        model_tx,
        events.clone(),
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
enum Ev {
    Input(InputMsg),
    InputGone,
    /// The user copied something on this PC.
    LocalClipboard(ClipboardData),
    /// Clipboard data from the Mac was written here; this is its local hash.
    ClipboardApplied(u64),
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

enum NetEv {
    Control(Control),
    Pong { sent_us: u64 },
    Clipboard(ClipboardData),
    Closed(String),
}

struct Established {
    session: Session,
    /// The pairing code when the session was made in pairing mode.
    code: Option<String>,
}

/// Reconnect delays: 250 ms, 500 ms, 1 s, then 2 s until a connection succeeds.
#[derive(Debug, Default)]
struct Backoff {
    failures: usize,
}

impl Backoff {
    const STEPS: [Duration; 3] = [
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_secs(1),
    ];
    const CAP: Duration = Duration::from_secs(2);

    fn next_delay(&mut self) -> Duration {
        let d = Self::STEPS.get(self.failures).copied().unwrap_or(Self::CAP);
        self.failures = self.failures.saturating_add(1);
        d
    }

    fn reset(&mut self) {
        self.failures = 0;
    }
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

/// `manual_peer_addr` as "ip:port", or a bare IP on the default port.
fn manual_addr(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    s.parse().ok().or_else(|| {
        s.parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, QUIC_PORT))
    })
}

fn paired_fingerprint(cfg: &Config) -> Option<Fingerprint> {
    let hex = cfg.paired_peer.as_deref()?;
    let fp = Fingerprint::from_hex(hex);
    if fp.is_none() {
        warn!("paired_peer in the config is not a fingerprint; treating as unpaired");
    }
    fp
}

fn modified(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".into())
}

/// Discovery, then connect: pinned to the paired Mac, or in pairing mode
/// (running the pairing exchange before anything else uses the session).
async fn establish(id: &Identity, cfg: &Config, name: &str) -> Result<Established> {
    let paired = paired_fingerprint(cfg);
    let peers = discover(DISCOVER_TIMEOUT, DISCOVERY_PORT)
        .await
        .context("discovery")?;
    let addr = match choose_peer(&peers, paired.as_ref()) {
        Some(peer) => peer.addr,
        None => match cfg.manual_peer_addr.as_deref() {
            Some(s) => manual_addr(s).with_context(|| format!("bad manual_peer_addr {s:?}"))?,
            None => bail!("no Mac found ({} other device(s) answered)", peers.len()),
        },
    };
    let trust = paired.map_or(Trust::PairingMode, Trust::Pinned);
    let session = connect(id, trust, addr, name).await?;
    let code = match paired {
        Some(_) => None,
        None => Some(session.pair().await.context("pairing exchange")?),
    };
    Ok(Established { session, code })
}

/// The current session and the tasks serving it. Dropping it closes the session.
struct Live {
    sid: u64,
    session: Session,
    peer_name: String,
    /// Latest local clipboard item to send; a newer one replaces an unsent one.
    clip_out: watch::Sender<Option<Arc<ClipboardData>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Live {
    /// Starts the readers. Only call once any pairing exchange is done.
    fn start(sid: u64, session: Session, clip_max: u64, events: &UnboundedSender<Ev>) -> Live {
        let net = move |ev| Ev::Net { sid, ev };
        let mut tasks = Vec::new();

        let (s, tx) = (session.clone(), events.clone());
        tasks.push(tokio::spawn(async move {
            let reason = loop {
                match s.recv_control().await {
                    Ok(c) => {
                        if tx.send(net(NetEv::Control(c))).is_err() {
                            return;
                        }
                    }
                    Err(e) => break format!("control stream: {e:#}"),
                }
            };
            let _ = tx.send(net(NetEv::Closed(reason)));
        }));

        let (s, tx) = (session.clone(), events.clone());
        tasks.push(tokio::spawn(async move {
            let reason = loop {
                match s.recv_datagram().await {
                    Ok(Datagram::Pong { sent_us, .. }) => {
                        if tx.send(net(NetEv::Pong { sent_us })).is_err() {
                            return;
                        }
                    }
                    // The Mac measures latency too.
                    Ok(Datagram::Ping { seq, sent_us }) => {
                        let _ = s.send_datagram(&Datagram::Pong { seq, sent_us });
                    }
                    Ok(_) => {}
                    Err(e) => break format!("connection: {e:#}"),
                }
            };
            let _ = tx.send(net(NetEv::Closed(reason)));
        }));

        let (s, tx) = (session.clone(), events.clone());
        tasks.push(tokio::spawn(async move {
            loop {
                match s.recv_clipboard(clip_max).await {
                    Ok(d) => {
                        if tx.send(net(NetEv::Clipboard(d))).is_err() {
                            return;
                        }
                    }
                    // A closed session is reported by the other readers.
                    Err(_) if is_closed(&s).await => return,
                    Err(e) => warn!("clipboard from the Mac not accepted: {e:#}"),
                }
            }
        }));

        let (clip_out, mut clip_rx) = watch::channel(None::<Arc<ClipboardData>>);
        let s = session.clone();
        tasks.push(tokio::spawn(async move {
            while clip_rx.changed().await.is_ok() {
                let item = clip_rx.borrow_and_update().clone();
                if let Some(d) = item
                    && let Err(e) = s.send_clipboard(&d).await
                {
                    debug!("clipboard not sent: {e:#}");
                }
            }
        }));

        Live {
            sid,
            peer_name: session.peer_name(),
            session,
            clip_out,
            tasks,
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.session.close();
    }
}

async fn is_closed(s: &Session) -> bool {
    tokio::time::timeout(Duration::ZERO, s.closed())
        .await
        .is_ok()
}

struct App {
    machine: PcMachine,
    cfg: Config,
    cfg_path: PathBuf,
    cfg_stamp: Option<SystemTime>,
    identity: Arc<Identity>,
    name: String,
    input: std::sync::mpsc::Sender<InputCmd>,
    clip_write: crossbeam_channel::Sender<ClipboardData>,
    tray: std::sync::mpsc::Sender<TrayModel>,
    last_model: Option<TrayModel>,
    events: UnboundedSender<Ev>,
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
    pair_local: Option<bool>,
    pair_remote: Option<bool>,
    pair_declined: bool,
    echo: EchoGuard,
    latency: LatencyStats,
    latency_text: String,
    ping_seq: u32,
    /// Zero point for ping timestamps.
    epoch: std::time::Instant,
    settings: Option<Child>,
    quit_by: Option<Instant>,
    done: bool,
}

impl App {
    fn new(
        cfg: Config,
        cfg_path: PathBuf,
        identity: Identity,
        input: std::sync::mpsc::Sender<InputCmd>,
        clip_write: crossbeam_channel::Sender<ClipboardData>,
        tray: std::sync::mpsc::Sender<TrayModel>,
        events: UnboundedSender<Ev>,
    ) -> App {
        let latency = LatencyStats::new();
        App {
            machine: PcMachine::new(cfg.clone()),
            cfg_stamp: modified(&cfg_path),
            cfg,
            cfg_path,
            identity: Arc::new(identity),
            name: computer_name(),
            input,
            clip_write,
            tray,
            last_model: None,
            events,
            pending: VecDeque::new(),
            link: None,
            dead: None,
            next_sid: 0,
            connecting: false,
            retry_at: Some(Instant::now()),
            backoff: Backoff::default(),
            pair_dialog_open: false,
            pair_local: None,
            pair_remote: None,
            pair_declined: false,
            echo: EchoGuard::new(),
            latency_text: latency_text(&latency),
            latency,
            ping_seq: 0,
            epoch: std::time::Instant::now(),
            settings: None,
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
                _ = config_poll.tick() => self.poll_config(),
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
            Ev::LocalClipboard(d) => self.send_clipboard(d),
            Ev::ClipboardApplied(hash) => self.echo.mark_applied(hash),
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
            NetEv::Pong { sent_us } => {
                let now_us = self.epoch.elapsed().as_micros() as u64;
                if let Some(rtt) = now_us.checked_sub(sent_us).map(Duration::from_micros)
                    && rtt <= MAX_RTT
                {
                    self.latency.record(std::time::Instant::now(), rtt);
                }
            }
            NetEv::Clipboard(d) => {
                if self.paired_session().is_some()
                    && !d.is_empty()
                    && self.clip_write.send(d).is_err()
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
        let tx = self.events.clone();
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
        let live = Live::start(sid, session, self.cfg.clipboard_max_bytes, &self.events);
        info!(peer = %live.peer_name, paired = code.is_none(), "connected");
        self.link = Some(live);
        self.reset_latency();
        match code {
            None => self.feed(PcEvent::Connected {
                paired: true,
                code: String::new(),
            }),
            Some(code) => {
                self.pair_local = None;
                self.pair_remote = None;
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
        let delay = if std::mem::take(&mut self.pair_declined) {
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
        let (sid, peer, tx) = (live.sid, live.peer_name.clone(), self.events.clone());
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
        self.pair_local = Some(accepted);
        self.pair_declined |= !accepted;
        self.feed(PcEvent::PairAnswered {
            local: accepted,
            remote: self.pair_remote,
        });
    }

    fn on_remote_pair_answer(&mut self, accepted: bool) {
        if !matches!(self.machine.link(), Link::Pairing { .. }) {
            return;
        }
        info!(accepted, "pairing: answered on the Mac");
        self.pair_remote = Some(accepted);
        self.pair_declined |= !accepted;
        // The machine takes both answers together; until this PC's user has
        // answered, the Mac's answer waits here.
        if let Some(local) = self.pair_local {
            self.feed(PcEvent::PairAnswered {
                local,
                remote: Some(accepted),
            });
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
        if self.input.send(cmd).is_err() {
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

    /// Skip empty, enforce the size limit, suppress echoes and repeats, then send.
    fn send_clipboard(&mut self, d: ClipboardData) {
        if self.paired_session().is_none() || d.is_empty() {
            return;
        }
        if !within_limit(&d, self.cfg.clipboard_max_bytes) {
            info!(
                bytes = d.byte_len(),
                limit = self.cfg.clipboard_max_bytes,
                "clipboard too large to send; skipped"
            );
            return;
        }
        if !self.echo.should_send(d.content_hash()) {
            return;
        }
        if let Some(live) = &self.link {
            live.clip_out.send_replace(Some(Arc::new(d)));
        }
    }

    // ----------------------------------------------------------------- config

    fn poll_config(&mut self) {
        let stamp = modified(&self.cfg_path);
        if stamp == self.cfg_stamp {
            return;
        }
        self.cfg_stamp = stamp;
        self.apply_config(Config::load(&self.cfg_path));
    }

    /// Changes the file as it is on disk (the settings window may have
    /// written it), then applies the result.
    fn update_config(&mut self, change: impl FnOnce(&mut Config)) {
        let mut cfg = Config::load(&self.cfg_path);
        change(&mut cfg);
        let cfg = cfg.clamped();
        match cfg.save(&self.cfg_path) {
            Ok(()) => self.cfg_stamp = modified(&self.cfg_path),
            Err(e) => warn!("saving {}: {e}", self.cfg_path.display()),
        }
        self.apply_config(cfg);
    }

    fn apply_config(&mut self, cfg: Config) {
        if cfg == self.cfg {
            return;
        }
        info!("settings changed");
        let pairing_changed = cfg.paired_peer != self.cfg.paired_peer;
        self.cfg = cfg.clone();
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
                self.update_config(|c| c.paused = paused);
            }
            TrayCmd::OpenSettings => self.open_settings(),
            TrayCmd::ForgetPairing => {
                let tx = self.events.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Ev::ForgetConfirmed(dialogs::confirm_forget().await));
                });
            }
            TrayCmd::Quit => self.begin_quit().await,
        }
    }

    fn open_settings(&mut self) {
        if let Some(child) = &mut self.settings
            && matches!(child.try_wait(), Ok(None))
        {
            info!("settings window is already open");
            return;
        }
        let spawned =
            std::env::current_exe().and_then(|exe| Command::new(exe).arg("--settings").spawn());
        match spawned {
            Ok(child) => self.settings = Some(child),
            Err(e) => error!("opening settings: {e}"),
        }
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
        self.retry_at = None;
        self.quit_by = Some(Instant::now() + QUIT_TIMEOUT);
    }

    fn publish(&mut self) {
        let peer = self.link.as_ref().map(|l| l.peer_name.as_str());
        let model = TrayModel {
            status: status_text(self.machine.link(), peer, self.cfg.paused),
            latency: self.latency_text.clone(),
            paused: self.cfg.paused,
        };
        if self.last_model.as_ref() != Some(&model) {
            let _ = self.tray.send(model.clone());
            self.last_model = Some(model);
        }
    }
}

/// Moves input-thread messages into the loop.
fn bridge_input(
    rx: crossbeam_channel::Receiver<InputMsg>,
    events: UnboundedSender<Ev>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("cj-input-bridge".into())
        .spawn(move || {
            for msg in rx {
                if events.send(Ev::Input(msg)).is_err() {
                    return;
                }
            }
            let _ = events.send(Ev::InputGone);
        })
        .context("starting the input bridge")?;
    Ok(())
}

/// Moves local clipboard changes into the loop, and writes clipboard data
/// from the Mac (sent on the returned channel). Writing on this thread keeps
/// the order right: `ClipboardApplied` reaches the loop before the watcher's
/// report of the same write, so the echo guard always suppresses it.
fn bridge_clipboard(
    watch: crossbeam_channel::Receiver<ClipboardData>,
    events: UnboundedSender<Ev>,
) -> Result<crossbeam_channel::Sender<ClipboardData>> {
    let (write_tx, write_rx) = crossbeam_channel::unbounded::<ClipboardData>();
    let apply = move |d: ClipboardData, events: &UnboundedSender<Ev>| {
        if let Err(e) = write_clipboard(&d) {
            warn!("clipboard from the Mac not applied: {e:#}");
            return;
        }
        // Hash what this PC now holds: that is what the watcher will report.
        let hash = read_clipboard().map_or_else(|_| d.content_hash(), |back| back.content_hash());
        let _ = events.send(Ev::ClipboardApplied(hash));
    };
    std::thread::Builder::new()
        .name("cj-clipboard-bridge".into())
        .spawn(move || {
            loop {
                crossbeam_channel::select! {
                    recv(watch) -> d => match d {
                        Ok(d) => {
                            if events.send(Ev::LocalClipboard(d)).is_err() {
                                return;
                            }
                        }
                        Err(_) => break,
                    },
                    recv(write_rx) -> d => match d {
                        Ok(d) => apply(d, &events),
                        Err(_) => return,
                    },
                }
            }
            warn!("clipboard watcher stopped; copies on this PC are no longer sent");
            for d in write_rx {
                apply(d, &events);
            }
        })
        .context("starting the clipboard bridge")?;
    Ok(write_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn backoff_schedule() {
        let mut b = Backoff::default();
        let delays: Vec<Duration> = (0..6).map(|_| b.next_delay()).collect();
        assert_eq!(
            delays,
            [ms(250), ms(500), ms(1000), ms(2000), ms(2000), ms(2000)]
        );
    }

    #[test]
    fn backoff_resets_after_connect() {
        let mut b = Backoff::default();
        for _ in 0..10 {
            b.next_delay();
        }
        b.reset();
        assert_eq!(b.next_delay(), ms(250));
        assert_eq!(b.next_delay(), ms(500));
    }

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

    #[test]
    fn manual_addr_with_and_without_port() {
        assert_eq!(
            manual_addr("192.168.1.20:47800"),
            Some(([192, 168, 1, 20], 47800).into())
        );
        assert_eq!(
            manual_addr(" 192.168.1.20 "),
            Some(([192, 168, 1, 20], QUIC_PORT).into())
        );
        assert_eq!(manual_addr("macbook.local"), None);
    }

    #[test]
    fn paired_fingerprint_parses_hex_only() {
        let fp = Fingerprint([0xab; 32]);
        let cfg = Config {
            paired_peer: Some(fp.to_hex()),
            ..Config::default()
        };
        assert_eq!(paired_fingerprint(&cfg), Some(fp));
        let bad = Config {
            paired_peer: Some("not hex".into()),
            ..Config::default()
        };
        assert_eq!(paired_fingerprint(&bad), None);
        assert_eq!(paired_fingerprint(&Config::default()), None);
    }
}
