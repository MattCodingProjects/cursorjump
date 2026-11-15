//! The Mac's sound, played on the PC's default output.
//!
//! One thread, `cj-audio-out`, in MMCSS "Pro Audio", runs a shared-mode,
//! event-driven `IAudioClient` on the eConsole default output. Each buffer
//! event fills the free frames from the pipe (silence included, so the
//! stream never stops while it runs). The stream is reopened on
//! [`MacSoundOut::reopen`] if the default output changed, and when Windows
//! invalidates it; with no output it retries every 500 ms.
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cj_core::audio::{AudioStream, RATE};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, bounded};
use tracing::{info, warn};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{
    SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::core::w;

use super::Com;
use crate::common::audio_pipe::AudioRx;

const STREAM: AudioStream = AudioStream::MacSound;
/// The buffer asked for: 10 ms, in 100 ns units.
const BUFFER_HNS: i64 = 100_000;
/// How long to wait before trying again with no output, or after a failure.
const RETRY: Duration = Duration::from_millis(500);
/// Longest wait for a buffer event before checking the stream anyway.
const EVENT_WAIT_MS: u32 = 100;

/// Plays an [`AudioRx`] on the PC's default output until stopped.
pub struct MacSoundOut {
    reopen: Sender<()>,
    thread: Option<JoinHandle<()>>,
    /// The pipe: taken by the thread while it runs, put back when it ends.
    rx: Arc<Mutex<Option<AudioRx>>>,
}

impl MacSoundOut {
    pub fn start(rx: AudioRx) -> MacSoundOut {
        let slot = Arc::new(Mutex::new(Some(rx)));
        let (reopen, cmds) = bounded::<()>(1);
        let spawned = {
            let slot = slot.clone();
            std::thread::Builder::new()
                .name("cj-audio-out".into())
                .spawn(move || {
                    if let Some(mut lent) = Lent::take(slot) {
                        run(lent.rx(), &cmds);
                    }
                })
        };
        let thread = spawned
            .map_err(|e| warn!("cj-audio-out failed to start; no Mac sound: {e}"))
            .ok();
        MacSoundOut {
            reopen,
            thread,
            rx: slot,
        }
    }

    /// Moves to the default output if it changed (call on any device change).
    pub fn reopen(&self) {
        // Full means a reopen is already pending.
        let _ = self.reopen.try_send(());
    }

    /// Stops playing and closes the output; returns the pipe.
    pub fn stop(self) -> AudioRx {
        let MacSoundOut { reopen, thread, rx } = self;
        // The thread ends once the sender is gone.
        drop(reopen);
        if let Some(t) = thread {
            let _ = t.join();
        }
        rx.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("cj-audio-out returns the pipe when it ends")
    }
}

/// The pipe, lent to cj-audio-out; returned to the slot when the thread
/// ends, even by a panic.
struct Lent {
    slot: Arc<Mutex<Option<AudioRx>>>,
    rx: Option<AudioRx>,
}

impl Lent {
    fn take(slot: Arc<Mutex<Option<AudioRx>>>) -> Option<Lent> {
        let rx = slot.lock().unwrap_or_else(PoisonError::into_inner).take()?;
        Some(Lent { slot, rx: Some(rx) })
    }

    fn rx(&mut self) -> &mut AudioRx {
        self.rx.as_mut().expect("lent until dropped")
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = self.rx.take();
    }
}

/// 48 kHz stereo 32-bit float, front left and right.
fn stream_format() -> WAVEFORMATEXTENSIBLE {
    let channels = STREAM.channels() as u16;
    let bits = 32u16;
    let block = channels * bits / 8;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: bits,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: bits,
        },
        dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// Why a stream ended.
enum End {
    /// The MacSoundOut was stopped.
    Stop,
    /// Reopen now: the default output changed, or Windows invalidated it.
    Reopen,
    /// Something failed; try again after [`RETRY`].
    Failed(anyhow::Error),
}

/// cj-audio-out: open, play, reopen, until stopped.
fn run(rx: &mut AudioRx, cmds: &Receiver<()>) {
    let _com = Com::init();
    let _mmcss = Mmcss::join();
    let event = match Event::new() {
        Ok(e) => e,
        Err(e) => {
            warn!("no Mac sound: {e:#}");
            // Nothing to play on; wait for the stop.
            while cmds.recv().is_ok() {}
            return;
        }
    };
    // Whether the last attempt found no output, so it is logged once.
    let mut missing = false;
    loop {
        let end = match Output::open(&event) {
            Ok(out) => {
                missing = false;
                info!("Mac sound: playing on the default output");
                rx.reset();
                out.play(rx, cmds, &event)
            }
            Err(e) => {
                if !missing {
                    info!("Mac sound: no output to play on ({e:#}); retrying");
                }
                missing = true;
                End::Failed(e)
            }
        };
        match end {
            End::Stop => return,
            End::Reopen => {}
            End::Failed(e) => {
                if !missing {
                    warn!("Mac sound: {e:#}; reopening");
                }
                match cmds.recv_timeout(RETRY) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }
}

/// An open, running output stream.
struct Output {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
    /// The endpoint's id, to tell whether the default changed.
    id: Vec<u16>,
}

impl Output {
    fn open(event: &Event) -> Result<Output> {
        // SAFETY: plain COM calls on interfaces this function holds; the
        // format and event outlive the calls that take them.
        unsafe {
            let device = default_output()?;
            let id = endpoint_id(&device)?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .context("activating the audio client")?;
            let format = stream_format();
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
            let frames = client.GetBufferSize().context("GetBufferSize")?;
            let render: IAudioRenderClient = client.GetService().context("render client")?;
            let out = Output {
                client,
                render,
                frames,
                id,
            };
            // Start on a full buffer (silence, as the pipe was just reset).
            out.fill(None)?;
            out.client.Start().context("starting the stream")?;
            Ok(out)
        }
    }

    /// Fills buffers as events arrive until stopped, reopened or failed.
    fn play(self, rx: &mut AudioRx, cmds: &Receiver<()>, event: &Event) -> End {
        loop {
            // SAFETY: waits on our own event handle.
            let woke = unsafe { WaitForSingleObject(event.0, EVENT_WAIT_MS) };
            match cmds.try_recv() {
                Ok(()) => {
                    if !self.is_default() {
                        info!("Mac sound: the default output changed; reopening");
                        return End::Reopen;
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return End::Stop,
            }
            if woke != WAIT_OBJECT_0 && woke != WAIT_TIMEOUT {
                return End::Failed(anyhow!("waiting for the buffer event: {woke:?}"));
            }
            // A timeout fills too: an invalidated stream reports it here.
            if let Err(e) = self.fill(Some(&mut *rx)) {
                return if invalidated(&e) {
                    info!("Mac sound: the output went away; reopening");
                    End::Reopen
                } else {
                    End::Failed(e)
                };
            }
        }
    }

    /// Writes the buffer's free frames: from `rx`, or silence.
    fn fill(&self, rx: Option<&mut AudioRx>) -> Result<()> {
        // SAFETY: plain calls on our live stream; the slice covers exactly
        // the frames GetBuffer returned, written before ReleaseBuffer.
        unsafe {
            let padding = self
                .client
                .GetCurrentPadding()
                .context("GetCurrentPadding")?;
            let free = self.frames.saturating_sub(padding);
            if free == 0 {
                return Ok(());
            }
            let data = self.render.GetBuffer(free).context("GetBuffer")?;
            let samples = free as usize * STREAM.channels();
            let out = std::slice::from_raw_parts_mut(data.cast::<f32>(), samples);
            match rx {
                Some(rx) => rx.pull(out),
                None => out.fill(0.0),
            }
            self.render
                .ReleaseBuffer(free, 0)
                .context("ReleaseBuffer")?;
        }
        Ok(())
    }

    /// Whether this stream's endpoint is still the default output.
    fn is_default(&self) -> bool {
        // SAFETY: plain COM calls.
        let id = unsafe { default_output().and_then(|d| endpoint_id(&d)) };
        id.is_ok_and(|id| id == self.id)
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: plain call on our live client.
        let _ = unsafe { self.client.Stop() };
    }
}

fn invalidated(e: &anyhow::Error) -> bool {
    e.downcast_ref::<windows::core::Error>()
        .is_some_and(|e| e.code() == AUDCLNT_E_DEVICE_INVALIDATED)
}

/// The eConsole default output.
unsafe fn default_output() -> Result<IMMDevice> {
    // SAFETY: plain COM calls; COM is initialized on this thread.
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("audio device enumerator")?;
        en.GetDefaultAudioEndpoint(eRender, eConsole)
            .context("no default output")
    }
}

/// The endpoint's id string, without its NUL.
unsafe fn endpoint_id(device: &IMMDevice) -> Result<Vec<u16>> {
    // SAFETY: GetId returns a NUL-terminated CoTaskMem string, copied then freed.
    unsafe {
        let p = device.GetId().context("endpoint id")?;
        let id = p.as_wide().to_vec();
        CoTaskMemFree(Some(p.0.cast()));
        Ok(id)
    }
}

/// An auto-reset event for the stream's buffer events, closed on drop.
struct Event(HANDLE);

impl Event {
    fn new() -> Result<Event> {
        // SAFETY: plain FFI call; the handle is closed by Drop.
        let h = unsafe { CreateEventW(None, false, false, None) }.context("CreateEventW")?;
        Ok(Event(h))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: our own handle, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The thread's MMCSS "Pro Audio" membership, left on drop.
struct Mmcss(Option<HANDLE>);

impl Mmcss {
    fn join() -> Mmcss {
        let mut index = 0u32;
        // SAFETY: plain FFI call; `index` outlives it.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) } {
            Ok(h) => Mmcss(Some(h)),
            Err(e) => {
                warn!("MMCSS Pro Audio: {e}; playing at normal priority");
                Mmcss(None)
            }
        }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            // SAFETY: the handle AvSetMmThreadCharacteristicsW returned, on the same thread.
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::audio_pipe::{AudioTx, audio_pipe};
    use crate::win::audio::devices::DeviceWatch;
    use cj_core::audio::encode_pcm;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    #[test]
    fn stream_format_is_48k_stereo_float() {
        let f = stream_format();
        let base = f.Format;
        assert_eq!({ base.nChannels }, 2);
        assert_eq!({ base.nSamplesPerSec }, 48_000);
        assert_eq!({ base.wBitsPerSample }, 32);
        assert_eq!({ base.nBlockAlign }, 8);
        assert_eq!({ base.nAvgBytesPerSec }, 48_000 * 8);
        assert_eq!({ base.wFormatTag }, WAVE_FORMAT_EXTENSIBLE as u16);
        assert_eq!({ base.cbSize }, 22);
        assert_eq!({ f.dwChannelMask }, 3);
        assert_eq!({ f.SubFormat }, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    }

    #[test]
    fn stop_returns_the_pipe_promptly() {
        // With or without an output (CI may have none), it plays silence or
        // waits to retry; stop ends either at once and hands the pipe back.
        let (_tx, rx) = audio_pipe(STREAM);
        let out = MacSoundOut::start(rx);
        std::thread::sleep(Duration::from_millis(100));
        out.reopen();
        out.reopen();
        let t = Instant::now();
        let mut rx = out.stop();
        assert!(
            t.elapsed() < Duration::from_millis(400),
            "stop took {:?}",
            t.elapsed()
        );
        rx.pull(&mut [0.0; 480]);
    }

    /// Feeds `secs` of a 440 Hz stereo tone at a quarter of full scale into
    /// `tx`, one packet every 5 ms of real time, as the network would.
    fn feed_tone(tx: &AudioTx, secs: f64) {
        let frames = STREAM.packet_frames();
        let packets = (secs * f64::from(RATE) / frames as f64) as u32;
        let start = Instant::now();
        let mut n = 0u64;
        for seq in 0..packets {
            let due =
                start + Duration::from_secs_f64(f64::from(seq) * frames as f64 / f64::from(RATE));
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let samples: Vec<f32> = (0..frames)
                .flat_map(|_| {
                    let phase = n as f64 * std::f64::consts::TAU * 440.0 / f64::from(RATE);
                    n += 1;
                    [(phase.sin() * 0.25) as f32; 2]
                })
                .collect();
            tx.push(seq, &encode_pcm(&samples));
        }
    }

    fn log_to_test_output() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    }

    #[test]
    #[ignore = "hands-on: someone listens to the PC's default output"]
    fn render_plays_a_tone_through_the_default_output() {
        log_to_test_output();
        println!(
            "LISTEN: a steady 440 Hz tone (an A) plays for 2 s on the PC's default output.              It should be clean: no crackles, pops or gaps."
        );
        let (tx, rx) = audio_pipe(STREAM);
        let stats = rx.stats();
        let out = MacSoundOut::start(rx);
        feed_tone(&tx, 2.0);
        std::thread::sleep(Duration::from_millis(300));
        let _rx = out.stop();
        let s = stats.snapshot();
        println!("{}", s.log_line("Mac sound"));
        assert!(
            s.received > 0,
            "the output never pulled: is there a default output?"
        );
        println!("DONE: report whether you heard a clean 2 s tone.");
    }

    #[test]
    #[ignore = "hands-on: someone switches the default output while listening"]
    fn render_follows_a_default_device_change() {
        log_to_test_output();
        println!(
            "LISTEN: a 440 Hz tone plays for 20 s. While it plays, open Settings > System >              Sound and pick a different output (or use the speaker icon in the taskbar).              The tone should move to the new output within about 200 ms, with nothing              restarted; switching back should move it back."
        );
        let (tx, rx) = audio_pipe(STREAM);
        let stats = rx.stats();
        let out = Arc::new(MacSoundOut::start(rx));
        let changes = Arc::new(AtomicUsize::new(0));
        let watch = {
            let out = Arc::downgrade(&out);
            let changes = changes.clone();
            DeviceWatch::start(move || {
                changes.fetch_add(1, Ordering::Relaxed);
                if let Some(out) = out.upgrade() {
                    out.reopen();
                }
            })
            .expect("watching audio devices")
        };
        feed_tone(&tx, 20.0);
        drop(watch);
        let out = Arc::try_unwrap(out)
            .ok()
            .expect("the watch is gone, so this is the last owner");
        let _rx = out.stop();
        println!("{}", stats.snapshot().log_line("Mac sound"));
        let seen = changes.load(Ordering::Relaxed);
        println!("device notifications seen: {seen}");
        assert!(
            seen > 0,
            "no device change was seen: was the default output switched?"
        );
        println!("DONE: report whether the tone moved promptly each time, with no restart.");
    }
}
