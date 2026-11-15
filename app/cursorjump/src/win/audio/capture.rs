//! The PC's default microphone, sent to the Mac while a Mac app listens.
//!
//! One thread, `cj-audio-in`, in MMCSS "Pro Audio", runs a shared-mode,
//! event-driven `IAudioClient` on the eConsole default capture endpoint,
//! as 48 kHz mono float. Each event drains every ready packet into the
//! pipe's sender, silence included, so the stream never goes quiet while
//! it runs. Windows' "mic in use" indicator shows only while a [`PcMicIn`]
//! exists. When the mic is blocked or missing the thread waits for
//! [`PcMicIn::reopen`]; it is reopened if the default capture endpoint
//! changed, and always after Windows invalidates the stream.
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cj_core::audio::{AudioStream, DeviceState};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, bounded};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{E_ACCESSDENIED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient, eCapture,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::HRESULT;

use super::{Com, Event, Mmcss, default_endpoint, endpoint_id, invalidated, stream_format};
use crate::common::audio_pipe::{AudioOut, AudioSender};

const STREAM: AudioStream = AudioStream::PcMic;
/// The buffer asked for: 10 ms, in 100 ns units.
const BUFFER_HNS: i64 = 100_000;
/// How long to wait before trying again after an unexpected failure.
const RETRY: Duration = Duration::from_millis(500);
/// Longest wait for a buffer event before checking the stream anyway.
const EVENT_WAIT_MS: u32 = 100;

/// Captures the PC's default microphone while it exists.
#[cfg_attr(not(test), expect(dead_code, reason = "wired into the app in Task 9"))]
pub struct PcMicIn {
    reopen: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired into the app in Task 9"))]
impl PcMicIn {
    /// Starts capturing; `on_state` is told of each change in the mic's state.
    pub fn start(out: AudioOut, on_state: impl Fn(DeviceState) + Send + 'static) -> PcMicIn {
        let (reopen, cmds) = bounded::<()>(1);
        let spawned = std::thread::Builder::new()
            .name("cj-audio-in".into())
            .spawn(move || run(out.sender(STREAM), &cmds, &mut Reporter::new(on_state)));
        let thread = spawned
            .map_err(|e| warn!("cj-audio-in failed to start; no PC mic: {e}"))
            .ok();
        PcMicIn {
            reopen: Some(reopen),
            thread,
        }
    }

    /// Moves to the default capture endpoint if it changed, or retries a
    /// missing or blocked mic (call on any device change).
    #[cfg_attr(test, expect(dead_code, reason = "wired into the app in Task 9"))]
    pub fn reopen(&self) {
        if let Some(tx) = &self.reopen {
            // Full means a reopen is already pending.
            let _ = tx.try_send(());
        }
    }
}

impl Drop for PcMicIn {
    fn drop(&mut self) {
        // The thread ends once the sender is gone.
        drop(self.reopen.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Tells `on_state` only when the state changes.
struct Reporter<F: Fn(DeviceState)> {
    last: Option<DeviceState>,
    on_state: F,
}

impl<F: Fn(DeviceState)> Reporter<F> {
    fn new(on_state: F) -> Self {
        Reporter {
            last: None,
            on_state,
        }
    }

    fn set(&mut self, state: DeviceState) {
        if self.last != Some(state) {
            self.last = Some(state);
            (self.on_state)(state);
        }
    }
}

/// The device state a failed open or capture means.
fn state_for(hr: HRESULT) -> DeviceState {
    if hr == E_ACCESSDENIED {
        DeviceState::Blocked
    } else {
        // No endpoint (E_NOTFOUND), an invalidated device, or anything else.
        DeviceState::Missing
    }
}

/// The state an error stands for, from the Windows error inside it.
fn error_state(e: &anyhow::Error) -> DeviceState {
    e.downcast_ref::<windows::core::Error>()
        .map_or(DeviceState::Missing, |e| state_for(e.code()))
}

/// Why a stream ended.
enum End {
    /// The PcMicIn was dropped.
    Stop,
    /// Reopen now: the default mic changed.
    Reopen,
    /// Windows invalidated the device: wait for a reopen.
    Gone,
    /// Something failed; try again after [`RETRY`].
    Failed(anyhow::Error),
}

/// cj-audio-in: open, capture, reopen, until stopped.
fn run(mut sender: AudioSender, cmds: &Receiver<()>, state: &mut Reporter<impl Fn(DeviceState)>) {
    let _com = Com::init();
    let _mmcss = Mmcss::join();
    let event = match Event::new() {
        Ok(e) => e,
        Err(e) => {
            warn!("no PC mic: {e:#}");
            state.set(DeviceState::Missing);
            // Nothing to capture on; wait for the stop.
            while cmds.recv().is_ok() {}
            return;
        }
    };
    loop {
        // Whether to wait for a reopen (true) or just retry soon (false).
        let wait_for_reopen = match Input::open(&event) {
            Ok(input) => {
                info!("PC mic: capturing the default microphone");
                state.set(DeviceState::Ok);
                match input.capture(&mut sender, cmds, &event) {
                    End::Stop => return,
                    End::Reopen => continue,
                    End::Gone => {
                        info!("PC mic: the microphone went away");
                        state.set(DeviceState::Missing);
                        true
                    }
                    End::Failed(e) => {
                        warn!("PC mic: {e:#}; reopening");
                        state.set(DeviceState::Missing);
                        false
                    }
                }
            }
            Err(e) => {
                info!("PC mic: not available ({e:#}); waiting for a device change");
                state.set(error_state(&e));
                true
            }
        };
        if wait_for_reopen {
            if cmds.recv().is_err() {
                return;
            }
        } else {
            match cmds.recv_timeout(RETRY) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// An open, running capture stream.
struct Input {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// The endpoint's id, to tell whether the default changed.
    id: Vec<u16>,
}

impl Input {
    fn open(event: &Event) -> Result<Input> {
        // SAFETY: plain COM calls on interfaces this function holds; the
        // format and event outlive the calls that take them.
        unsafe {
            let device = default_endpoint(eCapture)?;
            let id = endpoint_id(&device)?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .context("activating the audio client")?;
            let format = stream_format(STREAM);
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    BUFFER_HNS,
                    0,
                    (&raw const format).cast(),
                    None,
                )
                .context("initializing the stream")?;
            client.SetEventHandle(event.0).context("SetEventHandle")?;
            let capture: IAudioCaptureClient = client.GetService().context("capture client")?;
            client.Start().context("starting the stream")?;
            Ok(Input {
                client,
                capture,
                id,
            })
        }
    }

    /// Sends packets as events arrive until stopped, reopened or failed.
    fn capture(self, sender: &mut AudioSender, cmds: &Receiver<()>, event: &Event) -> End {
        // Silence for flagged buffers, reused between events.
        let mut zeros: Vec<f32> = Vec::new();
        loop {
            // SAFETY: waits on our own event handle.
            let woke = unsafe { WaitForSingleObject(event.0, EVENT_WAIT_MS) };
            match cmds.try_recv() {
                Ok(()) => {
                    if !self.is_default() {
                        info!("PC mic: the default microphone changed; reopening");
                        return End::Reopen;
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return End::Stop,
            }
            if woke != WAIT_OBJECT_0 && woke != WAIT_TIMEOUT {
                return End::Failed(anyhow!("waiting for the buffer event: {woke:?}"));
            }
            // A timeout drains too: an invalidated stream reports it here.
            if let Err(e) = self.drain(sender, &mut zeros) {
                return if invalidated(&e) {
                    End::Gone
                } else {
                    End::Failed(e)
                };
            }
        }
    }

    /// Sends every packet that is ready.
    fn drain(&self, sender: &mut AudioSender, zeros: &mut Vec<f32>) -> Result<()> {
        // SAFETY: plain calls on our live stream; the slice covers exactly
        // the frames GetBuffer returned and is read before ReleaseBuffer.
        unsafe {
            while self
                .capture
                .GetNextPacketSize()
                .context("GetNextPacketSize")?
                > 0
            {
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .context("GetBuffer")?;
                let n = frames as usize * STREAM.channels();
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    debug!("PC mic: capture discontinuity");
                }
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    // Keep sending through silence; the data is not valid.
                    zeros.clear();
                    zeros.resize(n, 0.0);
                    sender.push(zeros);
                } else {
                    sender.push(std::slice::from_raw_parts(data.cast::<f32>(), n));
                }
                self.capture
                    .ReleaseBuffer(frames)
                    .context("ReleaseBuffer")?;
            }
        }
        Ok(())
    }

    /// Whether this stream's endpoint is still the default microphone.
    fn is_default(&self) -> bool {
        // SAFETY: plain COM calls.
        let id = unsafe { default_endpoint(eCapture).and_then(|d| endpoint_id(&d)) };
        id.is_ok_and(|id| id == self.id)
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: plain call on our live client.
        let _ = unsafe { self.client.Stop() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_core::audio::decode_pcm;
    use cj_core::proto::Datagram;
    use cj_net::session::{Listener, connect};
    use cj_net::{Identity, Trust};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED;

    #[test]
    fn capture_error_maps_to_device_state() {
        assert_eq!(state_for(E_ACCESSDENIED), DeviceState::Blocked);
        // E_NOTFOUND as the endpoint enumerator reports it: no endpoint.
        assert_eq!(
            state_for(ERROR_NOT_FOUND.to_hresult()),
            DeviceState::Missing
        );
        assert_eq!(
            state_for(AUDCLNT_E_DEVICE_INVALIDATED),
            DeviceState::Missing
        );
    }

    #[test]
    fn wrapped_errors_keep_their_state() {
        let blocked = anyhow::Error::new(windows::core::Error::from(E_ACCESSDENIED))
            .context("initializing the stream");
        assert_eq!(error_state(&blocked), DeviceState::Blocked);
        assert_eq!(
            error_state(&anyhow!("no windows error")),
            DeviceState::Missing
        );
    }

    #[test]
    fn state_is_reported_only_on_change() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut r = {
            let seen = seen.clone();
            Reporter::new(move |s| seen.lock().unwrap().push(s))
        };
        for s in [
            DeviceState::Missing,
            DeviceState::Missing,
            DeviceState::Ok,
            DeviceState::Ok,
            DeviceState::Missing,
        ] {
            r.set(s);
        }
        assert_eq!(
            *seen.lock().unwrap(),
            [DeviceState::Missing, DeviceState::Ok, DeviceState::Missing]
        );
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
    }

    #[test]
    #[ignore = "hands-on: someone talks into the PC's default microphone"]
    fn capture_records_the_default_mic() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        println!(
            "TALK/SILENT: the PC's default microphone is captured for 3 s into a loopback \
             session. Stay SILENT for the first 1.5 s, then TALK steadily at normal volume for \
             the last 1.5 s. Capture starts in 3 s."
        );
        let rt = tokio::runtime::Runtime::new().expect("a tokio runtime");
        let ident = || {
            let dir = tempfile::tempdir().unwrap();
            Identity::load_or_create(dir.path()).unwrap()
        };
        let (a, b) = (ident(), ident());
        let any_port = (std::net::Ipv4Addr::LOCALHOST, 0).into();
        let (listener, sa, sb) = rt.block_on(async {
            let listener = Listener::bind(&a, Trust::PairingMode, any_port, "A")
                .await
                .unwrap();
            let (sa, sb) = tokio::join!(
                listener.accept(),
                connect(&b, Trust::PairingMode, listener.local_addr(), "B")
            );
            (listener, sa.unwrap(), sb.unwrap())
        });
        // (arrival time, RMS) of each packet that reached the "Mac" side.
        let packets: Arc<Mutex<Vec<(Instant, f64)>>> = Arc::default();
        let reader = {
            let packets = packets.clone();
            rt.spawn(async move {
                while let Ok(d) = sb.recv_datagram().await {
                    if let Datagram::Audio { pcm, .. } = d {
                        packets
                            .lock()
                            .unwrap()
                            .push((Instant::now(), rms(&decode_pcm(&pcm))));
                    }
                }
            })
        };
        let out = AudioOut::default();
        out.set(Some(sa));

        std::thread::sleep(Duration::from_secs(3));
        let states = Arc::new(Mutex::new(Vec::new()));
        let mic = {
            let states = states.clone();
            PcMicIn::start(out, move |s| {
                println!("mic state: {s:?}");
                states.lock().unwrap().push(s);
            })
        };
        println!("SILENT: say nothing for 1.5 s.");
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(1500));
        println!("TALK NOW: keep talking for 1.5 s.");
        std::thread::sleep(Duration::from_millis(1500));
        drop(mic);
        reader.abort();
        drop(listener);
        println!("DONE: you can stop talking.");

        assert!(
            states.lock().unwrap().contains(&DeviceState::Ok),
            "the mic never opened ({:?}): is there a default microphone, and is \
             Settings > Privacy > Microphone > Let desktop apps access your microphone on?",
            states.lock().unwrap()
        );
        // Skip the first 0.5 s of each half: start-up, and the cue to talk.
        let window = |from_ms: u64, to_ms: u64| {
            let levels: Vec<f64> = packets
                .lock()
                .unwrap()
                .iter()
                .filter(|(t, _)| {
                    let ms = t.duration_since(start).as_millis() as u64;
                    (from_ms..to_ms).contains(&ms)
                })
                .map(|&(_, level)| level)
                .collect();
            assert!(
                !levels.is_empty(),
                "no packets between {from_ms} and {to_ms} ms"
            );
            levels.iter().sum::<f64>() / levels.len() as f64
        };
        let silent = window(500, 1500).max(1e-6);
        let talking = window(2000, 3000);
        println!("average packet RMS: silent {silent:.6}, talking {talking:.6}");
        assert!(
            talking > 10.0 * silent,
            "talking ({talking:.6}) was not more than 10x silent ({silent:.6}): \
             did you talk, and is this the microphone you mean?"
        );
    }
}
