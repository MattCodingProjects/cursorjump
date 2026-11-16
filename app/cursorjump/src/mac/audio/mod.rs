//! The Mac's side of audio sharing: the thread `cj-audio`, which owns the
//! mic route, the device watch, the sound tap and the mic feed, and carries
//! out the app's [`AudioPlan`]s.
//!
//! - The app's loop decides (`cj_core::audio_state::MacAudio`); this thread
//!   only does, and reports what it learns back as [`MacAudioEv`]s.
//! - Starting the tap probes capture first, which blocks while macOS shows
//!   its permission prompt: so it happens here, never on the loop, and only
//!   once a plan asks for the tap.
//! - The PC's mic reaches the feed through a pipe whose network end
//!   ([`MicTx`]) is swapped for a fresh one if a feed's teardown leaks the
//!   old receiver.

pub mod devices;
pub mod feed;
pub mod tap;

use std::path::Path;
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

/// The app's handle on `cj-audio`.
pub struct MacAudioIo {
    cmds: Sender<AudioCmd>,
    mic: MicPipe,
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
        let pipe = mic.clone();
        let spawned = std::thread::Builder::new()
            .name("cj-audio".into())
            .spawn(move || {
                let route = MicRoute::new(CoreAudioDevices, restore);
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
            });
        if let Err(e) = spawned {
            error!("audio: starting cj-audio failed: {e}; no audio sharing");
        }
        MacAudioIo { cmds, mic }
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

    pub fn send(&self, c: AudioCmd) {
        let _ = self.cmds.send(c);
    }
}

/// The Mac's sound going to the PC; dropping it destroys the tap.
trait Tap {
    /// Moves to the current default output; after an Err the tap is down.
    fn rebuild(&mut self) -> Result<(), TapError>;
}

impl Tap for SoundTap {
    fn rebuild(&mut self) -> Result<(), TapError> {
        SoundTap::rebuild(self)
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

type TapFactory = Box<dyn Fn(AudioOut) -> Result<Box<dyn Tap>, TapError>>;
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
            tap: Box::new(|out| Ok(Box::new(SoundTap::start(out)?) as Box<dyn Tap>)),
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
            (None, Some(t)) if self.want_tap => Some(t + TAP_RETRY),
            _ => None,
        }
    }

    fn tick(&mut self, now: Instant) {
        self.reconcile(now, false);
    }

    /// Brings the route, the feed and the tap in line with the plan.
    /// `retry_tap`: try a failed tap again without waiting.
    fn reconcile(&mut self, now: Instant, retry_tap: bool) {
        if self.want_mic {
            // The feed plays before apps are moved onto our mic.
            self.start_feed();
            if !self.route.engaged() {
                match self.route.engage() {
                    Ok(()) => info!("audio: mic on (CursorJump Microphone is the default input)"),
                    Err(e) => warn!("audio: making CursorJump Microphone the default input: {e:#}"),
                }
            }
        } else {
            // The user's input comes back first.
            if self.route.engaged() {
                self.route.release();
                info!("audio: mic off (the Mac's own input is back)");
            }
            self.stop_feed();
        }
        // Last: starting the tap may wait on macOS's permission prompt.
        if self.want_tap {
            let due = retry_tap || self.retry_at().is_none_or(|at| now >= at);
            if self.tap.is_none() && due {
                self.start_tap(now);
            }
        } else {
            self.tap_failed_at = None;
            self.drop_tap();
        }
    }

    fn start_tap(&mut self, now: Instant) {
        match (self.make.tap)(self.out.clone()) {
            Ok(tap) => {
                info!("audio: tap on (the Mac's sound goes to the PC)");
                self.tap = Some(tap);
                self.tap_failed_at = None;
                (self.events)(MacAudioEv::Capture(Capture::Allowed));
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
                warn!("audio: starting the mic feed failed: {e:#}");
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

    /// Gives the user's input back, stops the feed and drops the tap.
    fn quit(&mut self) {
        self.want_tap = false;
        self.want_mic = false;
        self.reconcile(Instant::now(), false);
        self.out.set(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::anyhow;
    use cj_core::audio::{AudioWant, encode_pcm};
    use cj_core::audio_state::{AudioPlan, Capture};

    use crate::common::audio_pipe::{AudioOut, AudioRx};
    use devices::{AudioDevEv, InputDevices, MIC_UID, MicRoute};
    use tap::TapError;

    const BUILTIN: &str = "BuiltInMicrophoneDevice";

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
        let rebuild_ok = Arc::new(Mutex::new(true));
        let feed_ok = Arc::new(Mutex::new(true));
        let feed_leaks = Arc::new(Mutex::new(false));
        let driver = Arc::new(Mutex::new(true));
        let make = Factories {
            tap: Box::new({
                let (log, answer, rebuild_ok) =
                    (log.clone(), tap_answer.clone(), rebuild_ok.clone());
                move |_out: AudioOut| {
                    log.push("tap start");
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
        assert!(r.log.take().is_empty(), "nothing is needed: nothing starts");
    }

    #[test]
    fn the_tap_follows_the_plan() {
        let mut r = rig();
        let t = Instant::now();
        r.worker.apply(&plan(true, false), t);
        assert_eq!(r.log.take(), ["tap start"]);
        assert_eq!(r.events(), [MacAudioEv::Capture(Capture::Allowed)]);
        r.worker.apply(&plan(true, false), t);
        assert!(r.log.take().is_empty(), "the same plan changes nothing");
        r.worker.apply(&plan(false, false), t);
        assert_eq!(r.log.take(), ["tap drop"]);
    }

    #[test]
    fn a_denied_tap_is_reported_and_not_retried_at_once() {
        let mut r = rig();
        *r.tap_answer.lock().unwrap() = Answer::Denied;
        let t = Instant::now();
        r.worker.apply(&plan(true, false), t);
        assert_eq!(r.log.take(), ["tap start"]);
        assert_eq!(r.events(), [MacAudioEv::Capture(Capture::Denied)]);
        // The app's next plan (tap off) is on its way.
        r.worker
            .apply(&plan(true, true), t + Duration::from_secs(1));
        assert!(!r.log.take().contains(&"tap start".to_string()));
    }

    #[test]
    fn a_failed_tap_is_retried_every_5_s_and_on_an_output_change() {
        let mut r = rig();
        *r.tap_answer.lock().unwrap() = Answer::Fail;
        let t = Instant::now();
        r.worker.apply(&plan(true, false), t);
        assert_eq!(r.log.take(), ["tap start"]);
        assert!(r.events().is_empty(), "a failure is no Capture answer");
        assert_eq!(r.worker.retry_at(), Some(t + TAP_RETRY));
        r.worker.tick(t + Duration::from_secs(4));
        r.worker
            .apply(&plan(true, true), t + Duration::from_secs(4));
        assert!(!r.log.take().contains(&"tap start".to_string()));
        r.worker.tick(t + TAP_RETRY);
        assert_eq!(r.log.take(), ["tap start"]);
        let t2 = t + TAP_RETRY + Duration::from_secs(1);
        r.worker.device(AudioDevEv::DefaultOutputChanged, t2);
        assert_eq!(r.log.take(), ["tap start"], "a new output is worth a try");
        *r.tap_answer.lock().unwrap() = Answer::Ok;
        r.worker.tick(t2 + TAP_RETRY);
        assert_eq!(r.log.take(), ["tap start"]);
        assert_eq!(r.events(), [MacAudioEv::Capture(Capture::Allowed)]);
        assert_eq!(r.worker.retry_at(), None);
        r.worker.apply(&plan(false, true), t2 + TAP_RETRY);
        assert!(r.log.take().contains(&"tap drop".to_string()));
    }

    #[test]
    fn an_output_change_rebuilds_the_tap_and_a_failed_rebuild_waits() {
        let mut r = rig();
        let t = Instant::now();
        r.worker.apply(&plan(true, false), t);
        r.log.take();
        r.worker.device(AudioDevEv::DefaultOutputChanged, t);
        assert_eq!(r.log.take(), ["tap rebuild"]);
        *r.rebuild_ok.lock().unwrap() = false;
        r.worker.device(AudioDevEv::DefaultOutputChanged, t);
        assert_eq!(r.log.take(), ["tap rebuild", "tap drop"]);
        assert_eq!(r.worker.retry_at(), Some(t + TAP_RETRY));
        r.worker.tick(t + TAP_RETRY);
        assert_eq!(r.log.take(), ["tap start"]);
    }

    #[test]
    fn the_feed_and_the_route_follow_mic_default() {
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
    fn no_feed_without_the_driver() {
        let mut r = rig();
        *r.driver.lock().unwrap() = false;
        r.worker.apply(&plan(false, true), Instant::now());
        assert!(!r.log.take().contains(&"feed start".to_string()));
        assert_eq!(r.events(), [MacAudioEv::Driver(false)]);
    }

    #[test]
    fn a_failed_feed_keeps_its_pipe_and_retries_on_the_next_event() {
        let mut r = rig();
        *r.feed_ok.lock().unwrap() = false;
        let t = Instant::now();
        r.worker.apply(&plan(false, true), t);
        assert!(r.log.take().contains(&"feed start".to_string()));
        assert!(!r.mic.running());
        *r.feed_ok.lock().unwrap() = true;
        r.worker.device(AudioDevEv::DevicesChanged, t);
        assert_eq!(r.log.take(), ["feed start"]);
        assert!(r.mic.running());
    }

    #[test]
    fn a_leaked_feed_gets_a_fresh_pipe() {
        let mut r = rig();
        *r.feed_leaks.lock().unwrap() = true;
        let t = Instant::now();
        r.worker.apply(&plan(false, true), t);
        r.worker.apply(&plan(false, false), t);
        assert!(r.log.take().contains(&"feed stop".to_string()));
        // The network's handle now feeds the new pipe, and the stats follow.
        let tx = r.mic.tx();
        for seq in 0..3 {
            tx.push(seq, &encode_pcm(&[0.1; 480]));
        }
        let rx = r.worker.idle.as_mut().expect("a fresh pipe");
        rx.pull(&mut [0.0; 480]);
        assert_eq!(rx.stats().snapshot().received, 3);
        assert_eq!(r.mic.stats().snapshot().received, 3);
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
        let (cmds, cmds_rx) = crossbeam_channel::unbounded();
        let (devs, devs_rx) = crossbeam_channel::unbounded::<AudioDevEv>();
        // The watch failed to start: its sender is gone.
        drop(devs);
        let (reply, _replied) = std::sync::mpsc::channel();
        cmds.send(AudioCmd::Apply(plan(true, false))).unwrap();
        cmds.send(AudioCmd::Session(None)).unwrap();
        cmds.send(AudioCmd::Quit(reply)).unwrap();
        assert!(r.worker.run(&cmds_rx, devs_rx).is_some(), "Quit's reply");
        assert_eq!(r.log.take(), ["tap start", "tap drop"]);
    }

    #[test]
    fn quit_gives_the_input_back_and_stops_everything() {
        let mut r = rig();
        let t = Instant::now();
        r.worker.apply(&plan(true, true), t);
        r.log.take();
        r.worker.quit();
        assert_eq!(
            r.log.take(),
            [format!("input {BUILTIN}").as_str(), "feed stop", "tap drop"]
        );
        assert_eq!(r.default_input(), BUILTIN);
    }
}
