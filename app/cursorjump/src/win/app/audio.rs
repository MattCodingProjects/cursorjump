//! Audio sharing in the PC app's loop: the Mac's sound played on the PC's
//! default output and the PC's default mic sent to the Mac, each only while
//! the Mac asks (`Control::AudioWant`), and the PC's device status for the
//! Mac (`Control::AudioStatus`), sent once paired and again on each change.
//!
//! - The device threads are started and stopped here; the datagram reader
//!   feeds the Mac's sound into [`PcAudio::tx`].
//! - `cj-audio-watch` (device changes) and `cj-audio-in` (the mic's state)
//!   only post to the loop: `Ev::AudioDevices` and `Ev::MicState`.
//! - The mic's state as the capture thread last reported it (Blocked or
//!   Missing) overrides the probe until it reports Ok. Once the mic stops,
//!   nothing reports any more, so the next tick or device change hands the
//!   mic's state back to the probe: the Mac may then ask again, at most
//!   once a tick, and a mic that really is missing or blocked reads so.
//! - While the mic is wanted but not working, [`PcAudio::tick`] pokes it
//!   (ruling P10); while the mic reads as not Ok, the tick probes again,
//!   which catches Windows' privacy switch, a change no device event reports.

use std::time::Duration;

use cj_core::audio::{AudioStatus, AudioStream, AudioWant, DeviceState};
use cj_net::session::Session;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use super::Ev;
use crate::common::audio_pipe::{AudioOut, AudioRx, AudioStatsHandle, AudioTx, audio_pipe};
use crate::win::audio::capture::PcMicIn;
use crate::win::audio::devices::{DeviceWatch, pc_status};
use crate::win::audio::render::MacSoundOut;

/// How often [`PcAudio::tick`] runs.
pub(super) const AUDIO_TICK: Duration = Duration::from_secs(2);

/// The Mac's sound playing on a PC output.
pub(super) trait SoundOut: Send {
    /// Moves to the default output if it changed.
    fn reopen(&self);
    /// Stops playing; returns the pipe.
    fn stop(self: Box<Self>) -> AudioRx;
}

impl SoundOut for MacSoundOut {
    fn reopen(&self) {
        MacSoundOut::reopen(self);
    }

    fn stop(self: Box<Self>) -> AudioRx {
        MacSoundOut::stop(*self)
    }
}

/// The PC's mic being captured; dropping it stops the capture.
pub(super) trait MicIn: Send {
    /// Moves to the default mic if it changed, or retries a failed one.
    fn reopen(&self);
}

impl MicIn for PcMicIn {
    fn reopen(&self) {
        PcMicIn::reopen(self);
    }
}

type OutFactory = Box<dyn Fn(AudioRx) -> Box<dyn SoundOut> + Send>;
type OnMicState = Box<dyn Fn(DeviceState) + Send>;
type MicFactory = Box<dyn Fn(AudioOut, OnMicState) -> Box<dyn MicIn> + Send>;
type StatusProbe = Box<dyn Fn() -> AudioStatus + Send>;

/// How [`PcAudio`] starts its devices and reads their status: the real
/// ones in the app, fakes in tests.
pub(super) struct Factories {
    pub output: OutFactory,
    pub mic: MicFactory,
    pub status: StatusProbe,
}

impl Factories {
    fn real() -> Factories {
        Factories {
            output: Box::new(|rx| Box::new(MacSoundOut::start(rx))),
            mic: Box::new(|out, on_state| Box::new(PcMicIn::start(out, on_state))),
            status: Box::new(pc_status),
        }
    }
}

/// The PC's half of audio sharing.
pub(super) struct PcAudio {
    events: UnboundedSender<Ev>,
    devices: Factories,
    _watch: Option<DeviceWatch>,
    tx: AudioTx,
    /// The Mac's sound pipe while nothing plays it.
    idle: Option<AudioRx>,
    stats: AudioStatsHandle,
    output: Option<Box<dyn SoundOut>>,
    mic: Option<Box<dyn MicIn>>,
    /// The session captured mic audio goes out on.
    out: AudioOut,
    want: AudioWant,
    /// Paired: statuses are sent.
    connected: bool,
    probed: AudioStatus,
    /// The capture thread's last report, if Blocked or Missing; it
    /// overrides the probe.
    mic_failed: Option<DeviceState>,
    /// The capture thread's last report since the mic started.
    mic_last: Option<DeviceState>,
    /// The status the Mac has.
    sent: Option<AudioStatus>,
}

impl PcAudio {
    /// Watches the PC's devices; each change posts `Ev::AudioDevices`.
    pub fn new(events: UnboundedSender<Ev>) -> PcAudio {
        let tx = events.clone();
        let watch = DeviceWatch::start(move || {
            let _ = tx.send(Ev::AudioDevices);
        })
        .map_err(|e| warn!("not watching audio devices: {e:#}"))
        .ok();
        PcAudio::build(events, Factories::real(), watch)
    }

    #[cfg(test)]
    pub fn with_factories(events: UnboundedSender<Ev>, devices: Factories) -> PcAudio {
        PcAudio::build(events, devices, None)
    }

    fn build(
        events: UnboundedSender<Ev>,
        devices: Factories,
        watch: Option<DeviceWatch>,
    ) -> PcAudio {
        let (tx, rx) = audio_pipe(AudioStream::MacSound);
        PcAudio {
            events,
            devices,
            _watch: watch,
            tx,
            stats: rx.stats(),
            idle: Some(rx),
            output: None,
            mic: None,
            out: AudioOut::default(),
            want: AudioWant::default(),
            connected: false,
            probed: AudioStatus {
                output: DeviceState::Missing,
                mic: DeviceState::Missing,
            },
            mic_failed: None,
            mic_last: None,
            sent: None,
        }
    }

    /// Where the datagram reader puts the Mac's sound.
    pub fn tx(&self) -> AudioTx {
        self.tx.clone()
    }

    /// Whether `connect` was called since the last disconnect.
    pub fn connected(&self) -> bool {
        self.connected
    }

    /// A paired session is up; returns the status to send it now.
    pub fn connect(&mut self, s: &Session) -> AudioStatus {
        self.out.set(Some(s.clone()));
        self.on_connect()
    }

    fn on_connect(&mut self) -> AudioStatus {
        self.connected = true;
        self.probed = (self.devices.status)();
        self.mic_failed = None;
        let status = self.status();
        info!(?status, "audio: the PC's devices");
        self.sent = Some(status);
        status
    }

    /// Stops the output and the mic at once and forgets the Mac's want.
    pub fn disconnected(&mut self) {
        self.out.set(None);
        self.want(AudioWant::default());
        self.connected = false;
        self.sent = None;
        self.mic_failed = None;
    }

    /// Starts and stops each direction as the Mac asks.
    pub fn want(&mut self, w: AudioWant) {
        if w != self.want {
            info!(sound = w.sound, mic = w.mic, "audio: the Mac's want");
        }
        self.want = w;
        match (w.sound, self.output.take()) {
            (true, None) => {
                if let Some(mut rx) = self.idle.take() {
                    // Nothing stale ahead of the first packet.
                    rx.reset();
                    self.output = Some((self.devices.output)(rx));
                }
            }
            (false, Some(output)) => self.idle = Some(output.stop()),
            (_, output) => self.output = output,
        }
        match (w.mic, self.mic.is_some()) {
            (true, false) => {
                let tx = self.events.clone();
                let on_state: OnMicState = Box::new(move |s| {
                    let _ = tx.send(Ev::MicState(s));
                });
                self.mic_last = None;
                self.mic = Some((self.devices.mic)(self.out.clone(), on_state));
            }
            (false, true) => {
                // Dropping it stops the capture (and Windows' mic indicator).
                self.mic = None;
                self.mic_last = None;
            }
            _ => {}
        }
    }

    /// A device came or went, or a default changed; Some when the status
    /// the Mac has changed.
    pub fn devices_changed(&mut self) -> Option<AudioStatus> {
        if let Some(o) = &self.output {
            o.reopen();
        }
        match &self.mic {
            Some(m) => m.reopen(),
            // Nothing is left to report on the mic: back to the probe.
            None => self.mic_failed = None,
        }
        self.probed = (self.devices.status)();
        self.changed()
    }

    /// The capture thread's report; Some when the status changed.
    pub fn mic_state(&mut self, s: DeviceState) -> Option<AudioStatus> {
        // A late report from a mic already stopped.
        self.mic.as_ref()?;
        self.mic_last = Some(s);
        self.mic_failed = (s != DeviceState::Ok).then_some(s);
        self.changed()
    }

    /// Every [`AUDIO_TICK`]: pokes a wanted mic that isn't working, hands a
    /// stopped mic's state back to the probe, and probes again while the
    /// mic reads as not Ok.
    pub fn tick(&mut self) -> Option<AudioStatus> {
        if !self.connected {
            return None;
        }
        match &self.mic {
            Some(m) if self.mic_last != Some(DeviceState::Ok) => m.reopen(),
            Some(_) => {}
            // Nothing reports on a stopped mic: a failure it reported (a
            // one-off, or a format change no device event follows) would
            // otherwise stand for the whole session.
            None => self.mic_failed = None,
        }
        if self.status().mic != DeviceState::Ok {
            self.probed = (self.devices.status)();
        }
        self.changed()
    }

    /// With the latency log, while the Mac's sound plays.
    pub fn log_stats(&self) {
        if self.output.is_some() {
            info!("{}", self.stats.snapshot().log_line("Mac sound"));
        }
    }

    /// Windows keeps the mic from desktop apps (or refused it).
    pub fn mic_blocked(&self) -> bool {
        self.connected && self.status().mic == DeviceState::Blocked
    }

    fn status(&self) -> AudioStatus {
        AudioStatus {
            output: self.probed.output,
            mic: self.mic_failed.unwrap_or(self.probed.mic),
        }
    }

    fn changed(&mut self) -> Option<AudioStatus> {
        let status = self.status();
        if !self.connected || self.sent == Some(status) {
            return None;
        }
        info!(?status, "audio: the PC's devices changed");
        self.sent = Some(status);
        Some(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use cj_core::audio::{AudioStatus, AudioWant, DeviceState};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use crate::common::audio_pipe::{AudioOut, AudioRx};
    use crate::win::app::Ev;

    const OK: AudioStatus = AudioStatus {
        output: DeviceState::Ok,
        mic: DeviceState::Ok,
    };

    /// What the fakes did, in order.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<&'static str>>>);

    impl Log {
        fn push(&self, what: &'static str) {
            self.0.lock().unwrap().push(what);
        }

        fn take(&self) -> Vec<&'static str> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    struct FakeOut {
        rx: AudioRx,
        log: Log,
    }

    impl SoundOut for FakeOut {
        fn reopen(&self) {
            self.log.push("out reopen");
        }

        fn stop(self: Box<Self>) -> AudioRx {
            self.log.push("out stop");
            self.rx
        }
    }

    struct FakeMic {
        log: Log,
    }

    impl MicIn for FakeMic {
        fn reopen(&self) {
            self.log.push("mic reopen");
        }
    }

    impl Drop for FakeMic {
        fn drop(&mut self) {
            self.log.push("mic stop");
        }
    }

    struct Rig {
        audio: PcAudio,
        log: Log,
        probe: Arc<Mutex<AudioStatus>>,
        _events: UnboundedReceiver<Ev>,
    }

    /// A PcAudio on fakes: no real device is ever opened.
    fn rig() -> Rig {
        let (events, events_rx) = unbounded_channel();
        let log = Log::default();
        let probe = Arc::new(Mutex::new(OK));
        let factories = Factories {
            output: Box::new({
                let log = log.clone();
                move |rx: AudioRx| -> Box<dyn SoundOut> {
                    log.push("out start");
                    Box::new(FakeOut {
                        rx,
                        log: log.clone(),
                    })
                }
            }),
            mic: Box::new({
                let log = log.clone();
                move |_out: AudioOut, _on_state| -> Box<dyn MicIn> {
                    log.push("mic start");
                    Box::new(FakeMic { log: log.clone() })
                }
            }),
            status: Box::new({
                let probe = probe.clone();
                move || *probe.lock().unwrap()
            }),
        };
        Rig {
            audio: PcAudio::with_factories(events, factories),
            log,
            probe,
            _events: events_rx,
        }
    }

    fn want(sound: bool, mic: bool) -> AudioWant {
        AudioWant { sound, mic }
    }

    #[test]
    fn want_starts_and_stops_each_direction() {
        let mut r = rig();
        assert_eq!(r.audio.on_connect(), OK);
        r.audio.want(want(true, false));
        assert_eq!(r.log.take(), ["out start"]);
        r.audio.want(want(true, true));
        assert_eq!(r.log.take(), ["mic start"]);
        r.audio.want(want(true, true));
        assert!(r.log.take().is_empty(), "the same want changes nothing");
        r.audio.want(want(false, true));
        assert_eq!(r.log.take(), ["out stop"]);
        r.audio.want(want(false, false));
        assert_eq!(r.log.take(), ["mic stop"]);
        r.audio.want(want(true, false));
        assert_eq!(r.log.take(), ["out start"], "the pipe comes back for reuse");
    }

    #[test]
    fn disconnect_stops_the_mic() {
        let mut r = rig();
        r.audio.on_connect();
        r.audio.want(want(true, true));
        r.log.take();
        r.audio.disconnected();
        let mut stopped = r.log.take();
        stopped.sort_unstable();
        assert_eq!(stopped, ["mic stop", "out stop"]);
        r.audio.disconnected();
        assert!(r.log.take().is_empty(), "a second disconnect does nothing");
        r.audio.on_connect();
        assert!(
            r.log.take().is_empty(),
            "the want is forgotten on disconnect"
        );
        r.audio.want(want(true, false));
        assert_eq!(r.log.take(), ["out start"]);
    }

    #[test]
    fn status_is_resent_only_on_change() {
        let mut r = rig();
        assert_eq!(r.audio.devices_changed(), None, "nothing is sent unpaired");
        assert_eq!(r.audio.on_connect(), OK);
        assert_eq!(r.audio.devices_changed(), None);

        let no_output = AudioStatus {
            output: DeviceState::Missing,
            ..OK
        };
        *r.probe.lock().unwrap() = no_output;
        assert_eq!(r.audio.devices_changed(), Some(no_output));
        assert_eq!(r.audio.devices_changed(), None);
        *r.probe.lock().unwrap() = OK;
        assert_eq!(r.audio.devices_changed(), Some(OK));

        // The capture thread's report overrides the probe until it says Ok.
        assert_eq!(
            r.audio.mic_state(DeviceState::Blocked),
            None,
            "no mic is running"
        );
        r.audio.want(want(false, true));
        let blocked = AudioStatus {
            mic: DeviceState::Blocked,
            ..OK
        };
        assert_eq!(r.audio.mic_state(DeviceState::Blocked), Some(blocked));
        assert!(r.audio.mic_blocked());
        assert_eq!(r.audio.mic_state(DeviceState::Blocked), None);
        assert_eq!(r.audio.devices_changed(), None, "still blocked");
        assert_eq!(r.audio.mic_state(DeviceState::Ok), Some(OK));
        assert!(!r.audio.mic_blocked());

        r.audio.disconnected();
        *r.probe.lock().unwrap() = no_output;
        assert_eq!(r.audio.devices_changed(), None, "nothing is sent unpaired");
        assert_eq!(r.audio.on_connect(), no_output, "sent again on connect");
    }

    #[test]
    fn device_changes_reopen_what_runs() {
        let mut r = rig();
        r.audio.on_connect();
        r.audio.devices_changed();
        assert!(r.log.take().is_empty(), "nothing runs");
        r.audio.want(want(true, true));
        r.log.take();
        r.audio.devices_changed();
        let mut reopened = r.log.take();
        reopened.sort_unstable();
        assert_eq!(reopened, ["mic reopen", "out reopen"]);
    }

    #[test]
    fn a_failing_mic_is_poked_on_every_tick() {
        let mut r = rig();
        r.audio.on_connect();
        r.audio.want(want(false, true));
        r.log.take();
        r.audio.mic_state(DeviceState::Ok);
        assert_eq!(r.audio.tick(), None);
        assert!(r.log.take().is_empty(), "a working mic is left alone");
        r.audio.mic_state(DeviceState::Missing);
        r.audio.tick();
        r.audio.tick();
        assert_eq!(r.log.take(), ["mic reopen", "mic reopen"]);
    }

    #[test]
    fn a_tick_notices_the_privacy_switch() {
        let mut r = rig();
        let blocked = AudioStatus {
            mic: DeviceState::Blocked,
            ..OK
        };
        *r.probe.lock().unwrap() = blocked;
        assert_eq!(r.audio.on_connect(), blocked);
        assert!(r.audio.mic_blocked());
        assert_eq!(r.audio.tick(), None);
        *r.probe.lock().unwrap() = OK;
        assert_eq!(r.audio.tick(), Some(OK));
        assert!(!r.audio.mic_blocked());
    }

    #[test]
    fn a_stopped_mic_falls_back_to_the_probe_on_the_next_tick() {
        let mut r = rig();
        r.audio.on_connect();
        r.audio.want(want(false, true));
        let missing = AudioStatus {
            mic: DeviceState::Missing,
            ..OK
        };
        // A one-off failure, with the probe still saying Ok.
        assert_eq!(r.audio.mic_state(DeviceState::Missing), Some(missing));
        // The Mac stops asking once the mic is reported missing.
        r.audio.want(want(false, false));
        assert_eq!(r.audio.tick(), Some(OK), "the Mac may ask again");
        assert_eq!(r.audio.tick(), None);
    }

    #[test]
    fn a_stopped_mic_falls_back_to_the_probe_on_a_device_change() {
        let mut r = rig();
        r.audio.on_connect();
        r.audio.want(want(false, true));
        r.audio.mic_state(DeviceState::Missing);
        r.audio.want(want(false, false));
        // A mic plugged in: nothing runs to say so, so the probe decides.
        assert_eq!(r.audio.devices_changed(), Some(OK));
    }

    #[test]
    fn a_mic_blocked_while_running_ends_up_blocked() {
        let mut r = rig();
        r.audio.on_connect();
        r.audio.want(want(false, true));
        // The privacy switch goes off: the stream dies as Missing.
        let missing = AudioStatus {
            mic: DeviceState::Missing,
            ..OK
        };
        assert_eq!(r.audio.mic_state(DeviceState::Missing), Some(missing));
        let blocked = AudioStatus {
            mic: DeviceState::Blocked,
            ..OK
        };
        *r.probe.lock().unwrap() = blocked;
        // The running mic's report still stands; the tick records the probe.
        assert_eq!(r.audio.tick(), None);
        r.audio.want(want(false, false));
        assert_eq!(r.audio.tick(), Some(blocked));
        assert!(r.audio.mic_blocked(), "the tray says so");
        assert_eq!(r.audio.tick(), None, "a blocked mic does not loop");
    }
}
