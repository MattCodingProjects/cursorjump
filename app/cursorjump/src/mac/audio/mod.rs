//! The Mac's side of audio sharing: the thread `cj-audio`, which owns the
//! mic route, the device watch, the sound tap and the mic feed, and carries
//! out the app's [`AudioPlan`]s.
//!
//! - The app's loop decides (`cj_core::audio_state::MacAudio`); this thread
//!   only does, and reports what it learns back as [`MacAudioEv`]s.
//! - Starting the tap probes capture first, which blocks while macOS shows
//!   its permission prompt (or up to the probe's cap). So it runs on a
//!   short-lived helper, `cj-audio-tap`, one at a time, and only once a plan
//!   asks for the tap: `cj-audio` stays free to give the input back, and to
//!   answer Quit, meanwhile. Rebuilding and dropping the tap stay here.
//! - CursorJump Microphone is the default input only while the feed plays
//!   the PC's mic into it (ruling P14).
//! - The PC's mic reaches the feed through a pipe whose network end
//!   ([`MicTx`]) is swapped for a fresh one if a feed's teardown leaks the
//!   old receiver.

pub mod devices;
pub mod feed;
pub mod tap;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cj_core::audio::AudioStream;
use cj_core::audio_state::{AudioPlan, Capture};
use cj_net::session::Session;
use crossbeam_channel::{Receiver, Sender, after, never, select, unbounded};
use tracing::{error, info, warn};

use crate::common::audio_pipe::{AudioOut, AudioRx, AudioStatsHandle, AudioTx, audio_pipe};
use devices::{AudioDevEv, AudioWatch, CoreAudioDevices, InputDevices, MicRoute, driver_present};
use feed::MicFeed;
use tap::{SoundTap, TapError};

/// In the config directory: the input to give back, while engaged.
const RESTORE_FILE: &str = "audio-restore.toml";
/// After the tap failed to start, the soonest it is tried again (an output
/// change tries at once).
const TAP_RETRY: Duration = Duration::from_secs(5);
/// After Quit, how long `cj-audio` waits for a tap start still running, to
/// drop what it makes.
const START_DRAIN: Duration = Duration::from_secs(20);

pub enum AudioCmd {
    Apply(AudioPlan),
    /// The paired session the Mac's sound goes out on, if any.
    Session(Option<Session>),
    /// Gives the user's input back, stops the feed and drops the tap, then
    /// answers.
    Quit(std::sync::mpsc::Sender<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacAudioEv {
    /// Whether macOS lets us capture the Mac's sound.
    Capture(Capture),
    /// Whether the CursorJump audio driver is installed.
    Driver(bool),
    /// Whether some app records from CursorJump Microphone.
    MicListening(bool),
}

type Events = Box<dyn Fn(MacAudioEv) + Send>;

/// Where the datagram reader puts the PC's mic. Clones share one pipe end,
/// which `cj-audio` can swap; pushes come from the network, never from a
/// real-time thread, so a lock is fine.
#[derive(Clone)]
pub struct MicTx(Arc<Mutex<AudioTx>>);

impl MicTx {
    pub fn new(tx: AudioTx) -> MicTx {
        MicTx(Arc::new(Mutex::new(tx)))
    }

    /// Queues packet `seq` of the PC's mic.
    pub fn push(&self, seq: u32, pcm: &[u8]) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(seq, pcm);
    }

    fn set(&self, tx: AudioTx) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = tx;
    }
}

/// The PC's mic pipe as the app sees it: its network end, its counters and
/// whether the feed plays it. Shared between the app and `cj-audio`.
#[derive(Clone)]
struct MicPipe {
    tx: MicTx,
    stats: Arc<Mutex<AudioStatsHandle>>,
    running: Arc<AtomicBool>,
}

impl MicPipe {
    fn new() -> (MicPipe, AudioRx) {
        let (tx, rx) = audio_pipe(AudioStream::PcMic);
        let pipe = MicPipe {
            tx: MicTx::new(tx),
            stats: Arc::new(Mutex::new(rx.stats())),
            running: Arc::default(),
        };
        (pipe, rx)
    }

    /// A fresh pipe in place of one whose receiver was lost.
    fn renew(&self) -> AudioRx {
        let (tx, rx) = audio_pipe(AudioStream::PcMic);
        self.tx.set(tx);
        *self.stats.lock().unwrap_or_else(|e| e.into_inner()) = rx.stats();
        rx
    }

    fn tx(&self) -> MicTx {
        self.tx.clone()
    }

    fn stats(&self) -> AudioStatsHandle {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn set_running(&self, on: bool) {
        self.running.store(on, Ordering::Relaxed);
    }
}

/// Gives back an input a crash (or a wedged `cj-audio`) left on our mic:
/// what `cj-audio` does at launch, for the app's shutdown to fall back on.
pub fn recover_input(restore_file: &Path) {
    MicRoute::new(CoreAudioDevices, restore_file.to_path_buf()).recover();
}

/// The app's handle on `cj-audio`.
pub struct MacAudioIo {
    cmds: Sender<AudioCmd>,
    mic: MicPipe,
    restore: PathBuf,
}

impl MacAudioIo {
    /// Starts `cj-audio`, which first gives back an input a crash left on
    /// our mic, then watches the devices and waits for plans. Nothing else
    /// starts until a plan asks for it. `events` is called on `cj-audio`
    /// and must not block.
    pub fn start(app_support: &Path, events: impl Fn(MacAudioEv) + Send + 'static) -> MacAudioIo {
        let (cmds, cmds_rx) = unbounded();
        let (mic, idle) = MicPipe::new();
        let restore = app_support.join(RESTORE_FILE);
        let (pipe, file) = (mic.clone(), restore.clone());
        let spawned = std::thread::Builder::new()
            .name("cj-audio".into())
            .spawn(move || {
                let route = MicRoute::new(CoreAudioDevices, file);
                let mut worker =
                    Worker::new(route, Factories::real(), pipe, idle, Box::new(events));
                worker.launch();
                let (devs, devs_rx) = unbounded();
                let watch = AudioWatch::start(move |ev| {
                    let _ = devs.send(ev);
                })
                .inspect_err(|e| warn!("audio: not watching the audio devices: {e:#}"))
                .ok();
                let reply = worker.run(&cmds_rx, devs_rx);
                drop(watch);
                if let Some(reply) = reply {
                    let _ = reply.send(());
                }
                // After the reply: a tap a start still makes is dropped here.
                worker.drain_start();
            });
        if let Err(e) = spawned {
            error!("audio: starting cj-audio failed: {e}; no audio sharing");
        }
        MacAudioIo { cmds, mic, restore }
    }

    /// A handle whose commands go to `cmds`, with no thread behind it.
    #[cfg(test)]
    pub fn detached(cmds: Sender<AudioCmd>) -> MacAudioIo {
        MacAudioIo {
            cmds,
            mic: MicPipe::new().0,
            restore: PathBuf::from("audio-restore.toml"),
        }
    }

    /// For the datagram reader: where the PC's mic goes.
    pub fn tx(&self) -> MicTx {
        self.mic.tx()
    }

    /// The PC mic pipe's counters.
    pub fn mic_stats(&self) -> AudioStatsHandle {
        self.mic.stats()
    }

    /// Whether the PC's mic plays into CursorJump Microphone.
    pub fn feed_running(&self) -> bool {
        self.mic.running()
    }

    /// The file [`recover_input`] reads.
    pub fn restore_file(&self) -> PathBuf {
        self.restore.clone()
    }

    pub fn send(&self, c: AudioCmd) {
        let _ = self.cmds.send(c);
    }
}

/// The Mac's sound going to the PC; dropping it destroys the tap. Made on
/// `cj-audio-tap`, then rebuilt and dropped on `cj-audio`.
trait Tap: Send {
    /// Moves to the current default output; after an Err the tap is down.
    fn rebuild(&mut self) -> Result<(), TapError>;
}

/// A [`SoundTap`] that may move from `cj-audio-tap` to `cj-audio`.
struct MovedTap(SoundTap);

// SAFETY: the only part of a SoundTap that is not Send is its IOProc's
// state pointer (`Io<Capturer>`'s `NonNull`, from `Box::leak`, as in
// `MicFeed`). That state is owned by the tap and used by Core Audio's
// IOProc alone while the IOProc exists; the thread holding the SoundTap
// only calls Core Audio's (thread-safe) stop, destroy and property calls on
// its IDs, and frees the state after the IOProc is destroyed. Every other
// field (IDs, an Arc of atomics, a crossbeam sender, a join handle, a
// dispatch queue and a SendBlock) is Send. The tap is only ever used by one
// thread at a time: moved once, then owned by `cj-audio`.
unsafe impl Send for MovedTap {}

impl Tap for MovedTap {
    fn rebuild(&mut self) -> Result<(), TapError> {
        self.0.rebuild()
    }
}

/// The PC's mic playing into the feed device.
trait Feed {
    /// Stops playing; None if the receiver had to be leaked.
    fn stop(self: Box<Self>) -> Option<AudioRx>;
}

impl Feed for MicFeed {
    fn stop(self: Box<Self>) -> Option<AudioRx> {
        MicFeed::stop(*self)
    }
}

type TapResult = Result<Box<dyn Tap>, TapError>;
/// Runs on `cj-audio-tap`.
type TapFactory = Arc<dyn Fn(AudioOut) -> TapResult + Send + Sync>;
/// The rx comes back with the error (boxed: it is large, and rare).
type FeedResult = Result<Box<dyn Feed>, Box<(anyhow::Error, AudioRx)>>;
type FeedFactory = Box<dyn Fn(AudioRx) -> FeedResult>;

/// How the worker starts the tap and the feed and looks for the driver:
/// Core Audio in the app, fakes in tests.
struct Factories {
    tap: TapFactory,
    feed: FeedFactory,
    driver: Box<dyn Fn() -> bool>,
}

impl Factories {
    fn real() -> Factories {
        Factories {
            tap: Arc::new(|out| Ok(Box::new(MovedTap(SoundTap::start(out)?)) as Box<dyn Tap>)),
            feed: Box::new(|rx| {
                MicFeed::start(rx)
                    .map(|f| Box::new(f) as Box<dyn Feed>)
                    .map_err(Box::new)
            }),
            driver: Box::new(driver_present),
        }
    }
}

/// `cj-audio`'s state: carries out the latest plan.
struct Worker<D: InputDevices> {
    route: MicRoute<D>,
    make: Factories,
    events: Events,
    /// The session the tap sends on.
    out: AudioOut,
    /// The latest plan's `tap` and `mic_default`.
    want_tap: bool,
    want_mic: bool,
    tap: Option<Box<dyn Tap>>,
    /// When the tap last failed to start, while it is still wanted.
    tap_failed_at: Option<Instant>,
    /// A tap start is running on `cj-audio-tap`; its result comes on `starts`.
    starting: bool,
    /// The default output changed while a start ran: rebuild what it makes.
    output_moved: bool,
    starts: (Sender<TapResult>, Receiver<TapResult>),
    feed: Option<Box<dyn Feed>>,
    mic: MicPipe,
    /// The PC mic's receiver while no feed plays it.
    idle: Option<AudioRx>,
    /// The driver's presence as last reported.
    driver: Option<bool>,
}

impl<D: InputDevices> Worker<D> {
    fn new(
        route: MicRoute<D>,
        make: Factories,
        mic: MicPipe,
        idle: AudioRx,
        events: Events,
    ) -> Worker<D> {
        Worker {
            route,
            make,
            events,
            out: AudioOut::default(),
            want_tap: false,
            want_mic: false,
            tap: None,
            tap_failed_at: None,
            starting: false,
            output_moved: false,
            starts: unbounded(),
            feed: None,
            mic,
            idle: Some(idle),
            driver: None,
        }
    }

    /// Gives back an input a crash left on our mic, and reports the driver.
    fn launch(&mut self) {
        self.route.recover();
        self.check_driver();
    }

    /// Until Quit (whose reply it returns) or the app is gone.
    fn run(
        &mut self,
        cmds: &Receiver<AudioCmd>,
        mut devs: Receiver<AudioDevEv>,
    ) -> Option<std::sync::mpsc::Sender<()>> {
        let starts = self.starts.1.clone();
        loop {
            let timer = match self.retry_at() {
                Some(at) => after(at.saturating_duration_since(Instant::now())),
                None => never(),
            };
            select! {
                recv(cmds) -> c => match c {
                    Ok(AudioCmd::Apply(plan)) => self.apply(&plan, Instant::now()),
                    Ok(AudioCmd::Session(s)) => self.out.set(s),
                    Ok(AudioCmd::Quit(reply)) => {
                        self.quit();
                        return Some(reply);
                    }
                    Err(_) => {
                        self.quit();
                        return None;
                    }
                },
                recv(devs) -> ev => match ev {
                    Ok(ev) => self.device(ev, Instant::now()),
                    // No watch (it failed to start): never ready again.
                    Err(_) => devs = never(),
                },
                // The worker holds a sender, so this never disconnects.
                recv(starts) -> r => {
                    if let Ok(r) = r {
                        self.tap_started(r, Instant::now());
                    }
                }
                recv(timer) -> _ => self.tick(Instant::now()),
            }
        }
    }

    fn apply(&mut self, plan: &AudioPlan, now: Instant) {
        self.want_tap = plan.tap;
        self.want_mic = plan.mic_default;
        self.reconcile(now, false);
    }

    fn device(&mut self, ev: AudioDevEv, now: Instant) {
        let mut retry_tap = false;
        match ev {
            AudioDevEv::DefaultInputChanged => self.route.reassert(),
            AudioDevEv::DefaultOutputChanged => match &mut self.tap {
                Some(tap) => {
                    if let Err(e) = tap.rebuild() {
                        warn!("audio: moving the tap to the new output failed: {e}");
                        self.drop_tap();
                        self.tap_failed_at = Some(now);
                    }
                }
                // The tap being made may be on the old output.
                None if self.starting => self.output_moved = true,
                // A tap that failed to start may work on the new output.
                None => retry_tap = true,
            },
            AudioDevEv::DevicesChanged => self.check_driver(),
            AudioDevEv::MicListening(on) => {
                (self.events)(MacAudioEv::MicListening(on));
                return;
            }
        }
        self.reconcile(now, retry_tap);
    }

    /// When the tap's retry is due, if one is waiting.
    fn retry_at(&self) -> Option<Instant> {
        match (&self.tap, self.tap_failed_at) {
            (None, Some(t)) if self.want_tap && !self.starting => Some(t + TAP_RETRY),
            _ => None,
        }
    }

    fn tick(&mut self, now: Instant) {
        self.reconcile(now, false);
    }

    /// Brings the feed, the route and the tap in line with the plan.
    /// `retry_tap`: try a failed tap again without waiting.
    fn reconcile(&mut self, now: Instant, retry_tap: bool) {
        if self.want_mic {
            self.start_feed();
        }
        // Our mic is the default input only while the feed plays into it.
        let engage = self.want_mic && self.feed.is_some();
        if engage && !self.route.engaged() {
            match self.route.engage() {
                Ok(()) => info!("audio: mic on (CursorJump Microphone is the default input)"),
                Err(e) => warn!("audio: making CursorJump Microphone the default input: {e:#}"),
            }
        } else if !engage && self.route.engaged() {
            // The user's input comes back before the feed stops.
            self.route.release();
            info!("audio: mic off (the Mac's own input is back)");
        }
        if !self.want_mic {
            self.stop_feed();
        }
        if self.want_tap {
            let due = retry_tap || self.retry_at().is_none_or(|at| now >= at);
            if self.tap.is_none() && !self.starting && due {
                self.start_tap(now);
            }
        } else {
            self.tap_failed_at = None;
            self.drop_tap();
        }
    }

    /// Starts the tap on `cj-audio-tap`; the result comes back on `starts`.
    fn start_tap(&mut self, now: Instant) {
        let (make, out, done) = (
            Arc::clone(&self.make.tap),
            self.out.clone(),
            self.starts.0.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("cj-audio-tap".into())
            .spawn(move || {
                // If cj-audio is gone, the tap is dropped here.
                let _ = done.send(make(out));
            });
        match spawned {
            Ok(_) => {
                self.starting = true;
                self.output_moved = false;
            }
            Err(e) => {
                warn!("audio: starting cj-audio-tap failed: {e}; trying again later");
                self.tap_failed_at = Some(now);
            }
        }
    }

    /// A start finished on `cj-audio-tap`.
    fn tap_started(&mut self, result: TapResult, now: Instant) {
        self.starting = false;
        match result {
            Ok(mut tap) => {
                (self.events)(MacAudioEv::Capture(Capture::Allowed));
                if !self.want_tap {
                    // The plan dropped it (or Quit came) meanwhile.
                    info!("audio: the tap is no longer wanted; dropping it");
                    return;
                }
                if std::mem::take(&mut self.output_moved)
                    && let Err(e) = tap.rebuild()
                {
                    warn!("audio: moving the new tap to the new output failed: {e}");
                    self.tap_failed_at = Some(now);
                    return;
                }
                info!("audio: tap on (the Mac's sound goes to the PC)");
                self.tap = Some(tap);
                self.tap_failed_at = None;
            }
            Err(TapError::Denied) => {
                info!("audio: capturing the Mac's sound was refused; it stays on the Mac");
                self.tap_failed_at = Some(now);
                (self.events)(MacAudioEv::Capture(Capture::Denied));
            }
            Err(TapError::Failed(e)) => {
                warn!("audio: starting the tap failed: {e:#}; trying again later");
                self.tap_failed_at = Some(now);
            }
        }
    }

    fn drop_tap(&mut self) {
        if self.tap.take().is_some() {
            info!("audio: tap off (the Mac plays its own sound)");
        }
    }

    fn start_feed(&mut self) {
        if self.feed.is_some() {
            return;
        }
        self.check_driver();
        if self.driver != Some(true) {
            return;
        }
        let Some(mut rx) = self.idle.take() else {
            return;
        };
        // Nothing stale ahead of the first packet.
        rx.reset();
        match (self.make.feed)(rx) {
            Ok(feed) => {
                info!("audio: mic feed started");
                self.feed = Some(feed);
                self.mic.set_running(true);
            }
            Err(failed) => {
                let (e, rx) = *failed;
                warn!("audio: starting the mic feed failed: {e:#}; the mic stays on the Mac");
                self.idle = Some(rx);
            }
        }
    }

    fn stop_feed(&mut self) {
        let Some(feed) = self.feed.take() else {
            return;
        };
        self.mic.set_running(false);
        let rx = feed.stop().unwrap_or_else(|| {
            warn!("audio: the mic feed kept its pipe; the PC's mic goes to a new one");
            self.mic.renew()
        });
        self.idle = Some(rx);
    }

    /// Reads whether the driver is installed; reports a change.
    fn check_driver(&mut self) {
        let present = (self.make.driver)();
        if self.driver != Some(present) {
            info!(present, "audio: the CursorJump audio driver");
            self.driver = Some(present);
            (self.events)(MacAudioEv::Driver(present));
        }
    }

    /// Gives the user's input back, stops the feed and drops the tap. A tap
    /// start still running is not waited for (see `drain_start`).
    fn quit(&mut self) {
        self.want_tap = false;
        self.want_mic = false;
        self.reconcile(Instant::now(), false);
        self.out.set(None);
    }

    /// After Quit: waits (up to `START_DRAIN`) for a tap start still
    /// running, and drops what it made.
    fn drain_start(&mut self) {
        if !self.starting {
            return;
        }
        match self.starts.1.recv_timeout(START_DRAIN) {
            Ok(r) => self.tap_started(r, Instant::now()),
            Err(_) => warn!("audio: a tap start did not finish after quitting"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use anyhow::anyhow;
    use cj_core::audio::{AudioWant, encode_pcm};

    use devices::MIC_UID;

    const BUILTIN: &str = "BuiltInMicrophoneDevice";
    /// Upper bound for a fake tap start to come back.
    const T: Duration = Duration::from_secs(10);

    /// What the fakes did, in order.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Log {
        fn push(&self, what: impl Into<String>) {
            self.0.lock().unwrap().push(what.into());
        }

        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    fn has(log: &[String], what: &str) -> bool {
        log.iter().any(|l| l == what)
    }

    #[derive(Default)]
    struct Inputs {
        default: String,
        exists: HashSet<String>,
        /// The next `set_default_input` calls that fail.
        failing_sets: u32,
    }

    #[derive(Clone)]
    struct FakeInputs {
        state: Arc<Mutex<Inputs>>,
        log: Log,
    }

    impl InputDevices for FakeInputs {
        fn default_input_uid(&self) -> Option<String> {
            Some(self.state.lock().unwrap().default.clone())
        }

        fn set_default_input(&mut self, uid: &str) -> anyhow::Result<()> {
            let mut s = self.state.lock().unwrap();
            if s.failing_sets > 0 {
                s.failing_sets -= 1;
                return Err(anyhow!("refused"));
            }
            s.default = uid.into();
            self.log.push(format!("input {uid}"));
            Ok(())
        }

        fn exists(&self, uid: &str) -> bool {
            self.state.lock().unwrap().exists.contains(uid)
        }

        fn builtin_input_uid(&self) -> Option<String> {
            Some(BUILTIN.into())
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Answer {
        Ok,
        Denied,
        Fail,
    }

    struct FakeTap {
        log: Log,
        rebuild_ok: Arc<Mutex<bool>>,
    }

    impl Tap for FakeTap {
        fn rebuild(&mut self) -> Result<(), TapError> {
            self.log.push("tap rebuild");
            if *self.rebuild_ok.lock().unwrap() {
                Ok(())
            } else {
                Err(TapError::Failed(anyhow!("no output")))
            }
        }
    }

    impl Drop for FakeTap {
        fn drop(&mut self) {
            self.log.push("tap drop");
        }
    }

    struct FakeFeed {
        rx: AudioRx,
        log: Log,
        leaks: Arc<Mutex<bool>>,
    }

    impl Feed for FakeFeed {
        fn stop(self: Box<Self>) -> Option<AudioRx> {
            self.log.push("feed stop");
            if *self.leaks.lock().unwrap() {
                std::mem::forget(self.rx);
                None
            } else {
                Some(self.rx)
            }
        }
    }

    struct Rig {
        worker: Worker<FakeInputs>,
        log: Log,
        inputs: Arc<Mutex<Inputs>>,
        events: Arc<Mutex<Vec<MacAudioEv>>>,
        tap_answer: Arc<Mutex<Answer>>,
        /// While Some, a tap start waits for a message on it.
        tap_gate: Arc<Mutex<Option<Receiver<()>>>>,
        rebuild_ok: Arc<Mutex<bool>>,
        feed_ok: Arc<Mutex<bool>>,
        feed_leaks: Arc<Mutex<bool>>,
        driver: Arc<Mutex<bool>>,
        mic: MicPipe,
        _dir: tempfile::TempDir,
    }

    impl Rig {
        fn events(&self) -> Vec<MacAudioEv> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }

        fn default_input(&self) -> String {
            self.inputs.lock().unwrap().default.clone()
        }

        /// Waits for the tap start running on `cj-audio-tap` and hands its
        /// result to the worker, as `run` does.
        fn started(&mut self, now: Instant) {
            assert!(self.worker.starting, "no start is running");
            let r = self.worker.starts.1.recv_timeout(T).expect("a tap start");
            self.worker.tap_started(r, now);
        }

        /// Makes tap starts wait until the returned sender sends or drops.
        fn hold_tap_starts(&self) -> Sender<()> {
            let (release, gate) = unbounded();
            *self.tap_gate.lock().unwrap() = Some(gate);
            release
        }
    }

    /// A worker on fakes, the default input on the built-in mic, with a
    /// restore file holding `leftover` if Some.
    fn rig_with(leftover: Option<&str>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let restore = dir.path().join("audio-restore.toml");
        if let Some(uid) = leftover {
            std::fs::write(&restore, format!("previous_input = \"{uid}\"\n")).unwrap();
        }
        let log = Log::default();
        let inputs = Arc::new(Mutex::new(Inputs {
            default: leftover.map_or(BUILTIN, |_| MIC_UID).into(),
            exists: [BUILTIN, MIC_UID, "usb-mic"]
                .into_iter()
                .map(String::from)
                .collect(),
            failing_sets: 0,
        }));
        let route = MicRoute::new(
            FakeInputs {
                state: inputs.clone(),
                log: log.clone(),
            },
            restore,
        );
        let tap_answer = Arc::new(Mutex::new(Answer::Ok));
        let tap_gate: Arc<Mutex<Option<Receiver<()>>>> = Arc::default();
        let rebuild_ok = Arc::new(Mutex::new(true));
        let feed_ok = Arc::new(Mutex::new(true));
        let feed_leaks = Arc::new(Mutex::new(false));
        let driver = Arc::new(Mutex::new(true));
        let make = Factories {
            tap: Arc::new({
                let (log, answer, gate, rebuild_ok) = (
                    log.clone(),
                    tap_answer.clone(),
                    tap_gate.clone(),
                    rebuild_ok.clone(),
                );
                move |_out: AudioOut| {
                    log.push("tap start");
                    let gate = gate.lock().unwrap().clone();
                    if let Some(gate) = gate {
                        let _ = gate.recv();
                    }
                    match *answer.lock().unwrap() {
                        Answer::Ok => Ok(Box::new(FakeTap {
                            log: log.clone(),
                            rebuild_ok: rebuild_ok.clone(),
                        }) as Box<dyn Tap>),
                        Answer::Denied => Err(TapError::Denied),
                        Answer::Fail => Err(TapError::Failed(anyhow!("busy"))),
                    }
                }
            }),
            feed: Box::new({
                let (log, ok, leaks) = (log.clone(), feed_ok.clone(), feed_leaks.clone());
                move |rx: AudioRx| {
                    log.push("feed start");
                    if *ok.lock().unwrap() {
                        Ok(Box::new(FakeFeed {
                            rx,
                            log: log.clone(),
                            leaks: leaks.clone(),
                        }) as Box<dyn Feed>)
                    } else {
                        Err(Box::new((anyhow!("no feed device"), rx)))
                    }
                }
            }),
            driver: Box::new({
                let driver = driver.clone();
                move || *driver.lock().unwrap()
            }),
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let on_event = {
            let events = events.clone();
            move |ev| events.lock().unwrap().push(ev)
        };
        let (mic, idle) = MicPipe::new();
        Rig {
            worker: Worker::new(route, make, mic.clone(), idle, Box::new(on_event)),
            log,
            inputs,
            events,
            tap_answer,
            tap_gate,
            rebuild_ok,
            feed_ok,
            feed_leaks,
            driver,
            mic,
            _dir: dir,
        }
    }

    fn rig() -> Rig {
        let mut r = rig_with(None);
        r.worker.launch();
        r.events();
        r
    }

    fn plan(tap: bool, mic_default: bool) -> AudioPlan {
        AudioPlan {
            tap,
            mic_default,
            want: AudioWant::default(),
            note: None,
            recheck_at: None,
        }
    }

    #[test]
    fn launch_recovers_the_input_and_starts_nothing() {
        let mut r = rig_with(Some("usb-mic"));
        r.worker.launch();
        assert_eq!(
            r.log.take(),
            ["input usb-mic"],
            "recovered; no tap, no feed"
        );
        assert_eq!(r.events(), [MacAudioEv::Driver(true)]);
        assert_eq!(r.worker.retry_at(), None);
        r.worker.apply(&plan(false, false), Instant::now());
        assert!(!r.worker.starting);
        assert!(r.log.take().is_empty(), "nothing is needed: nothing starts");
    }

    #[test]
    fn the_tap_follows_the_plan() {
        let mut r = rig();
        let t = Instant::now();
        r.worker.apply(&plan(true, false), t);
        r.started(t);
        assert_eq!(r.log.take(), ["tap start"]);
        assert_eq!(r.events(), [MacAudioEv::Capture(Capture::Allowed)]);
        r.worker.apply(&plan(true, false), t);
        assert!(!r.worker.starting, "the same plan changes nothing");
        assert!(r.log.take().is_empty());
        r.worker.apply(&plan(false, false), t);
        assert_eq!(r.log.take(), ["tap drop"]);
    }

    #[test]
    fn a_denied_tap_is_reported_and_probed_again_once_switched_back_on() {
        let mut r = rig();
        *r.tap_answer.lock().unwrap() = Answer::Denied;
        let t = Instant::now();
        r.worker.apply(&plan(true, false), t);
        r.started(t);
        assert_eq!(r.log.take(), ["tap start"]);
        assert_eq!(r.events(), [MacAudioEv::Capture(Capture::Denied)]);
        // A plan sent before the app heard of the refusal.
        r.worker
            .apply(&plan(true, true), t + Duration::from_secs(1));
        assert!(!r.worker.starting, "not probed again at once");
        // The app's next plan: tap off. Then the user, having allowed
        // capture, switches sharing off and on (P15).
        r.worker
            .apply(&plan(false, true), t + Duration::from_secs(2));
        *r.tap_answer.lock().unwrap() = Answer::Ok;
        r.worker
            .apply(&plan(true, true), t + Duration::from_secs(3));
        r.started(t + Duration::from_secs(3));
        assert!(has(&r.log.take(), "tap start"), "a fresh probe and tap");
        assert_eq!(r.events(), [MacAudioEv::Capture(Capture::Allowed)]);
        assert!(r.worker.tap.is_some());
    }

    #[test]
    fn a_failed_tap_is_retried_every_5_s_and_on_an_output_change() {
        let mut r = rig();
        *r.tap_answer.lock().unwrap() = Answer::Fail;
        let t = Instant::now();
        r.worker.apply(&plan(true, false), t);
        assert_eq!(r.worker.retry_at(), None, "no retry while a start runs");
        r.started(t);
        assert_eq!(r.log.take(), ["tap start"]);
        assert!(r.events().is_empty(), "a failure is no Capture answer");
        assert_eq!(r.worker.retry_at(), Some(t + TAP_RETRY));
        r.worker.tick(t + Duration::from_secs(4));
        r.worker
            .apply(&plan(true, true), t + Duration::from_secs(4));
        assert!(!r.worker.starting);
        assert!(!has(&r.log.take(), "tap start"), "not before 5 s");
        r.worker.tick(t + TAP_RETRY);
        r.started(t + TAP_RETRY);
        assert_eq!(r.log.take(), ["tap start"]);
        let t2 = t + TAP_RETRY + Duration::from_secs(1);
        r.worker.device(AudioDevEv::DefaultOutputChanged, t2);
        assert!(r.worker.starting, "a new output is worth a try");
        r.started(t2);
        *r.tap_answer.lock().unwrap() = Answer::Ok;
        r.worker.tick(t2 + TAP_RETRY);
        r.started(t2 + TAP_RETRY);
        assert_eq!(r.log.take(), ["tap start", "tap start"]);
        assert_eq!(r.events(), [MacAudioEv::Capture(Capture::Allowed)]);
        assert_eq!(r.worker.retry_at(), None);
        r.worker.apply(&plan(false, true), t2 + TAP_RETRY);
        assert!(has(&r.log.take(), "tap drop"));
    }

    #[test]
    fn an_output_change_rebuilds_the_tap_and_a_failed_rebuild_waits() {
        let mut r = rig();
        let t = Instant::now();
        r.worker.apply(&plan(true, false), t);
        r.started(t);
        r.log.take();
        r.worker.device(AudioDevEv::DefaultOutputChanged, t);
        assert_eq!(r.log.take(), ["tap rebuild"]);
        *r.rebuild_ok.lock().unwrap() = false;
        r.worker.device(AudioDevEv::DefaultOutputChanged, t);
        assert_eq!(r.log.take(), ["tap rebuild", "tap drop"]);
        assert!(!r.worker.starting);
        assert_eq!(r.worker.retry_at(), Some(t + TAP_RETRY));
        r.worker.tick(t + TAP_RETRY);
        r.started(t + TAP_RETRY);
        assert_eq!(r.log.take(), ["tap start"]);
    }

    #[test]
    fn an_output_change_during_a_start_rebuilds_the_new_tap() {
        let mut r = rig();
        let release = r.hold_tap_starts();
        let t = Instant::now();
        r.worker.apply(&plan(true, false), t);
        r.worker.device(AudioDevEv::DefaultOutputChanged, t);
        drop(release);
        r.started(t);
        assert_eq!(r.log.take(), ["tap start", "tap rebuild"]);
        assert!(r.worker.tap.is_some());
    }

    #[test]
    fn a_blocked_tap_start_holds_up_neither_the_mic_nor_quit() {
        let mut r = rig();
        let release = r.hold_tap_starts();
        let t = Instant::now();
        r.worker.apply(&plan(true, true), t);
        assert!(r.worker.starting);
        // The start is stuck (a permission prompt); the mic still routes.
        assert_eq!(r.default_input(), MIC_UID);
        r.worker.apply(&plan(true, true), t + TAP_RETRY);
        r.worker.tick(t + TAP_RETRY);
        r.worker
            .device(AudioDevEv::DefaultOutputChanged, t + TAP_RETRY);
        // The switch goes off: the input comes back at once.
        r.worker.apply(&plan(false, false), t + TAP_RETRY);
        assert_eq!(r.default_input(), BUILTIN);
        let log = r.log.take();
        assert_eq!(
            log.iter().filter(|l| *l == "tap start").count(),
            1,
            "one start at a time: {log:?}"
        );
        assert!(has(&log, "feed stop"));
        // The late tap is dropped as soon as it arrives.
        drop(release);
        r.started(t + TAP_RETRY);
        assert_eq!(r.log.take(), ["tap drop"]);
        assert!(r.worker.tap.is_none());
    }

    #[test]
    fn quit_answers_while_a_tap_start_is_blocked() {
        let mut r = rig();
        let release = r.hold_tap_starts();
        let (cmds, cmds_rx) = unbounded();
        let (_devs, devs_rx) = unbounded::<AudioDevEv>();
        let (reply, _replied) = std::sync::mpsc::channel();
        cmds.send(AudioCmd::Apply(plan(true, true))).unwrap();
        cmds.send(AudioCmd::Quit(reply)).unwrap();
        assert!(r.worker.run(&cmds_rx, devs_rx).is_some(), "Quit's reply");
        assert_eq!(r.default_input(), BUILTIN, "the input is back first");
        assert!(r.worker.starting, "the start is still stuck");
        drop(release);
        r.worker.drain_start();
        assert!(r.worker.tap.is_none());
        let log = r.log.take();
        assert_eq!(log.last().map(String::as_str), Some("tap drop"), "{log:?}");
    }

    #[test]
    fn the_route_follows_a_running_feed() {
        let mut r = rig();
        let t = Instant::now();
        r.worker.apply(&plan(false, true), t);
        assert_eq!(
            r.log.take(),
            ["feed start", format!("input {MIC_UID}").as_str()],
            "the feed plays before apps are moved onto our mic"
        );
        assert!(r.mic.running());
        r.worker.apply(&plan(false, true), t);
        assert!(r.log.take().is_empty());
        r.worker.apply(&plan(false, false), t);
        assert_eq!(
            r.log.take(),
            [format!("input {BUILTIN}").as_str(), "feed stop"],
            "the user's input comes back first"
        );
        assert!(!r.mic.running());
    }

    #[test]
    fn no_feed_and_no_route_without_the_driver() {
        let mut r = rig();
        *r.driver.lock().unwrap() = false;
        r.worker.apply(&plan(false, true), Instant::now());
        assert!(r.log.take().is_empty(), "no feed, so no engage");
        assert_eq!(r.default_input(), BUILTIN);
        assert_eq!(r.events(), [MacAudioEv::Driver(false)]);
    }

    #[test]
    fn a_failed_feed_leaves_the_input_alone_until_a_retry_works() {
        let mut r = rig();
        *r.feed_ok.lock().unwrap() = false;
        let t = Instant::now();
        r.worker.apply(&plan(false, true), t);
        assert_eq!(r.log.take(), ["feed start"], "no engage (P14)");
        assert_eq!(r.default_input(), BUILTIN);
        assert!(!r.mic.running());
        *r.feed_ok.lock().unwrap() = true;
        r.worker.device(AudioDevEv::DevicesChanged, t);
        assert_eq!(
            r.log.take(),
            ["feed start", format!("input {MIC_UID}").as_str()]
        );
        assert!(r.mic.running());
    }

    #[test]
    fn a_leaked_feed_gets_a_fresh_pipe() {
        let mut r = rig();
        *r.feed_leaks.lock().unwrap() = true;
        let t = Instant::now();
        r.worker.apply(&plan(false, true), t);
        r.worker.apply(&plan(false, false), t);
        assert_eq!(
            r.log.take(),
            [
                "feed start",
                format!("input {MIC_UID}").as_str(),
                format!("input {BUILTIN}").as_str(),
                "feed stop"
            ],
            "released, then stopped"
        );
        // The network's handle now feeds the new pipe, and the stats follow.
        let tx = r.mic.tx();
        for seq in 0..3 {
            tx.push(seq, &encode_pcm(&[0.1; 480]));
        }
        let rx = r.worker.idle.as_mut().expect("a fresh pipe");
        rx.pull(&mut [0.0; 480]);
        assert_eq!(rx.stats().snapshot().received, 3);
        assert_eq!(r.mic.stats().snapshot().received, 3);
        // And it plays again on the next plan.
        r.worker.apply(&plan(false, true), t);
        assert_eq!(
            r.log.take(),
            ["feed start", format!("input {MIC_UID}").as_str()]
        );
    }

    #[test]
    fn a_failed_engage_is_retried_on_the_next_event() {
        let mut r = rig();
        r.inputs.lock().unwrap().failing_sets = 1;
        let t = Instant::now();
        r.worker.apply(&plan(false, true), t);
        assert_eq!(r.log.take(), ["feed start"]);
        assert_eq!(r.default_input(), BUILTIN);
        r.worker.device(AudioDevEv::DevicesChanged, t);
        assert_eq!(r.log.take(), [format!("input {MIC_UID}")]);
    }

    #[test]
    fn a_moved_default_input_is_set_back_while_engaged() {
        let mut r = rig();
        let t = Instant::now();
        r.worker.device(AudioDevEv::DefaultInputChanged, t);
        assert!(r.log.take().is_empty(), "not engaged: the user's choice");
        r.worker.apply(&plan(false, true), t);
        r.log.take();
        r.inputs.lock().unwrap().default = "usb-mic".into();
        r.worker.device(AudioDevEv::DefaultInputChanged, t);
        assert_eq!(r.log.take(), [format!("input {MIC_UID}")]);
    }

    #[test]
    fn device_events_reach_the_app() {
        let mut r = rig();
        let t = Instant::now();
        r.worker.device(AudioDevEv::MicListening(true), t);
        assert_eq!(r.events(), [MacAudioEv::MicListening(true)]);
        r.worker.device(AudioDevEv::DevicesChanged, t);
        assert!(r.events().is_empty(), "the driver is still there");
        *r.driver.lock().unwrap() = false;
        r.worker.device(AudioDevEv::DevicesChanged, t);
        assert_eq!(r.events(), [MacAudioEv::Driver(false)]);
    }

    #[test]
    fn run_carries_out_commands_until_quit_even_without_a_watch() {
        let mut r = rig();
        let (cmds, cmds_rx) = unbounded();
        let (devs, devs_rx) = unbounded::<AudioDevEv>();
        // The watch failed to start: its sender is gone.
        drop(devs);
        let (reply, _replied) = std::sync::mpsc::channel();
        cmds.send(AudioCmd::Apply(plan(true, false))).unwrap();
        cmds.send(AudioCmd::Session(None)).unwrap();
        cmds.send(AudioCmd::Quit(reply)).unwrap();
        assert!(r.worker.run(&cmds_rx, devs_rx).is_some(), "Quit's reply");
        // Before or after Quit, the tap that start makes is dropped.
        r.worker.drain_start();
        assert_eq!(r.log.take(), ["tap start", "tap drop"]);
    }

    #[test]
    fn quit_gives_the_input_back_and_stops_everything() {
        let mut r = rig();
        let t = Instant::now();
        r.worker.apply(&plan(true, true), t);
        r.started(t);
        r.log.take();
        r.worker.quit();
        assert_eq!(
            r.log.take(),
            [format!("input {BUILTIN}").as_str(), "feed stop", "tap drop"]
        );
        assert_eq!(r.default_input(), BUILTIN);
    }
}
