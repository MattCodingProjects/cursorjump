//! Plays the PC's microphone into the hidden "CursorJump Mic Feed", which
//! the CursorJump audio driver loops back as "CursorJump Microphone".
//!
//! The feed's IOProc, on Core Audio's real-time thread, pulls from the
//! `AudioRx` straight into the output buffer: `pull` never allocates or
//! waits for a lock.

use std::ffi::c_void;
use std::ptr::{NonNull, addr_of};

use anyhow::anyhow;
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioObjectID, kAudioDevicePropertyBufferFrameSize,
};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};
use tracing::{info, warn};

use super::devices::{FEED_UID, device_id, get, global, set};
use crate::common::audio_pipe::AudioRx;

/// The feed's IO buffer: 5.3 ms.
const BUFFER_FRAMES: u32 = 256;

/// The PC's mic playing into the feed device. `stop` (or drop) stops it.
pub struct MicFeed {
    dev: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    started: bool,
    /// From `Box::leak`; the IOProc's alone while it runs. `None` once
    /// taken back.
    rx: Option<NonNull<AudioRx>>,
}

// SAFETY: the `rx` is owned, and is touched only by the IOProc while it
// runs and by `take_back` after it is destroyed; `AudioRx` is Send.
unsafe impl Send for MicFeed where AudioRx: Send {}

impl MicFeed {
    /// Opens the feed device by UID and starts playing `rx` into it. On
    /// failure the `rx` comes back with the error, untouched.
    #[expect(
        clippy::result_large_err,
        reason = "the rx comes back by value; start fails rarely, and once"
    )]
    pub fn start(rx: AudioRx) -> Result<MicFeed, (anyhow::Error, AudioRx)> {
        let Some(dev) = device_id(FEED_UID) else {
            return Err((
                anyhow!("the CursorJump Mic Feed ({FEED_UID}) isn't there"),
                rx,
            ));
        };
        let frames = global(kAudioDevicePropertyBufferFrameSize);
        let st = set(dev, frames, BUFFER_FRAMES);
        if st != 0 {
            warn!("PC mic: setting the feed's buffer to {BUFFER_FRAMES} frames: OSStatus {st}");
        }
        let frames = match get::<u32>(dev, frames, None) {
            Ok(n) => n.to_string(),
            Err(st) => format!("unknown (OSStatus {st})"),
        };

        let state = NonNull::from(Box::leak(Box::new(rx)));
        let mut feed = MicFeed {
            dev,
            proc_id: None,
            started: false,
            rx: Some(state),
        };
        // SAFETY: the rx stays valid until after the IOProc is destroyed
        // (take_back), and only the IOProc uses it until then.
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                dev,
                Some(play_proc),
                state.as_ptr().cast(),
                NonNull::from(&mut feed.proc_id),
            )
        };
        if st != 0 {
            let e = anyhow!("creating an IOProc on the CursorJump Mic Feed: OSStatus {st}");
            return Err((e, feed.take_back()));
        }
        // SAFETY: our own IOProc on this device.
        let st = unsafe { AudioDeviceStart(dev, feed.proc_id) };
        if st != 0 {
            let e = anyhow!("starting the CursorJump Mic Feed: OSStatus {st}");
            return Err((e, feed.take_back()));
        }
        feed.started = true;
        info!(
            "PC mic: playing into the CursorJump Mic Feed (device {dev}, buffer {frames} frames)"
        );
        Ok(feed)
    }

    /// Stops and destroys the IOProc, then hands the `rx` back.
    pub fn stop(mut self) -> AudioRx {
        let rx = self.take_back();
        info!("PC mic: stopped playing into the CursorJump Mic Feed");
        rx
    }

    /// Stops and destroys the IOProc, if any, then takes the rx back. Once
    /// both calls have returned, Core Audio never calls the IOProc again;
    /// if either fails, the device is gone and the IOProc with it.
    fn take_back(&mut self) -> AudioRx {
        let rx = self.rx.take().expect("the rx is taken back once");
        // SAFETY: our own IOProc on this device, each step done once.
        unsafe {
            if self.started {
                let st = AudioDeviceStop(self.dev, self.proc_id);
                if st != 0 {
                    warn!("PC mic: stopping the feed's IOProc: OSStatus {st}");
                }
            }
            if self.proc_id.is_some() {
                let st = AudioDeviceDestroyIOProcID(self.dev, self.proc_id);
                if st != 0 {
                    warn!("PC mic: destroying the feed's IOProc: OSStatus {st}");
                }
            }
        }
        self.started = false;
        self.proc_id = None;
        // SAFETY: from Box::leak in `start`; no IOProc can run on it now.
        *unsafe { Box::from_raw(rx.as_ptr()) }
    }
}

impl Drop for MicFeed {
    fn drop(&mut self) {
        if self.rx.is_some() {
            drop(self.take_back());
            info!("PC mic: stopped playing into the CursorJump Mic Feed");
        }
    }
}

/// Real-time: plays the PC's mic into the feed's output buffer.
unsafe extern "C-unwind" fn play_proc(
    _dev: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    _input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the MicFeed's rx, this IOProc's alone while it runs.
    let rx = unsafe { &mut *client.cast::<AudioRx>() };
    // SAFETY: Core Audio passes a valid list for this callback.
    unsafe { fill(output, |out| rx.pull(out)) };
    0
}

/// Fills one mono float32 buffer from `pull`; any other shape gets silence
/// wherever it has memory.
///
/// # Safety
/// `list` must point at a valid AudioBufferList whose buffers hold
/// `mDataByteSize` writable bytes each.
unsafe fn fill(list: NonNull<AudioBufferList>, mut pull: impl FnMut(&mut [f32])) {
    let list = list.as_ptr();
    // SAFETY: the list holds `mNumberBuffers` buffers; the pointer comes
    // from the list itself, not from its one-element array.
    let buffers = unsafe {
        let n = (*list).mNumberBuffers as usize;
        std::slice::from_raw_parts(addr_of!((*list).mBuffers).cast::<AudioBuffer>(), n)
    };
    let mono = matches!(buffers, [b] if b.mNumberChannels == 1);
    for b in buffers {
        if b.mData.is_null() {
            continue;
        }
        // SAFETY: an output buffer of `mDataByteSize` bytes of float32.
        let out = unsafe {
            std::slice::from_raw_parts_mut(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
        };
        if mono {
            pull(out);
        } else {
            out.fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    use std::time::{Duration, Instant};

    use cj_core::audio::{AudioStream, RATE, encode_pcm};

    use super::*;
    use crate::common::audio_pipe::audio_pipe;
    use crate::mac::audio::devices::MIC_UID;

    const TONE_HZ: f64 = 1000.0;
    const AMPLITUDE: f64 = 0.5;
    const RECORD_S: usize = 2;
    /// Measured: the last 1.5 s, after the jitter buffer's fill and fade-in.
    const MEASURE_FROM: usize = RATE as usize / 2;

    /// An AudioBufferList with room for two buffers.
    #[repr(C)]
    struct TwoBuffers {
        n: u32,
        buffers: [AudioBuffer; 2],
    }

    fn buffer(channels: u32, data: &mut [f32]) -> AudioBuffer {
        AudioBuffer {
            mNumberChannels: channels,
            mDataByteSize: (data.len() * 4) as u32,
            mData: data.as_mut_ptr().cast(),
        }
    }

    /// Runs `fill` on `list`, pulling 0.5s; says how many samples it pulled.
    fn fill_list(list: &mut TwoBuffers) -> usize {
        let mut pulled = 0;
        // SAFETY: TwoBuffers starts like an AudioBufferList, and each buffer
        // points at live samples of its size.
        unsafe {
            fill(NonNull::from(list).cast(), |out| {
                out.fill(0.5);
                pulled += out.len();
            });
        }
        pulled
    }

    #[test]
    fn fill_plays_into_one_mono_buffer() {
        let mut a = [1.0f32; 256];
        let mut list = TwoBuffers {
            n: 1,
            buffers: [buffer(1, &mut a), buffer(1, &mut [])],
        };
        assert_eq!(fill_list(&mut list), 256);
        assert!(a.iter().all(|&v| v == 0.5));
    }

    #[test]
    fn fill_plays_silence_into_any_other_shape() {
        // Two buffers.
        let (mut a, mut b) = ([1.0f32; 8], [1.0f32; 8]);
        let mut list = TwoBuffers {
            n: 2,
            buffers: [buffer(1, &mut a), buffer(1, &mut b)],
        };
        assert_eq!(fill_list(&mut list), 0);
        assert!(a.iter().chain(&b).all(|&v| v == 0.0));

        // One stereo buffer.
        let mut a = [1.0f32; 8];
        let mut list = TwoBuffers {
            n: 1,
            buffers: [buffer(2, &mut a), buffer(1, &mut [])],
        };
        assert_eq!(fill_list(&mut list), 0);
        assert!(a.iter().all(|&v| v == 0.0));

        // No buffers, and a buffer with no memory.
        let mut list = TwoBuffers {
            n: 0,
            buffers: [buffer(1, &mut a), buffer(1, &mut [])],
        };
        assert_eq!(fill_list(&mut list), 0);
        list.n = 1;
        list.buffers[0].mData = std::ptr::null_mut();
        assert_eq!(fill_list(&mut list), 0);
    }

    /// The test recorder's IOProc state: a preallocated buffer it fills.
    struct Recording {
        samples: Box<[f32]>,
        len: AtomicUsize,
        odd_shape: AtomicBool,
    }

    /// Real-time: appends the mic's mono input to the recording, until full.
    unsafe extern "C-unwind" fn record_proc(
        _: AudioObjectID,
        _: NonNull<AudioTimeStamp>,
        input: NonNull<AudioBufferList>,
        _: NonNull<AudioTimeStamp>,
        _: NonNull<AudioBufferList>,
        _: NonNull<AudioTimeStamp>,
        client: *mut c_void,
    ) -> i32 {
        let r = client.cast::<Recording>();
        // SAFETY: `client` is the leaked Recording. Only this IOProc touches
        // its samples until it is destroyed; the test thread only reads `len`.
        let (samples, len, odd_shape) = unsafe { (&mut (*r).samples, &(*r).len, &(*r).odd_shape) };
        let list = input.as_ptr();
        // SAFETY: Core Audio passes a valid list for this callback.
        let (n, b) = unsafe {
            let first = addr_of!((*list).mBuffers).cast::<AudioBuffer>();
            ((*list).mNumberBuffers, *first)
        };
        if n != 1 || b.mNumberChannels != 1 || b.mData.is_null() {
            odd_shape.store(true, Relaxed);
            return 0;
        }
        // SAFETY: an input buffer of `mDataByteSize` bytes of float32.
        let got = unsafe {
            std::slice::from_raw_parts(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
        };
        let at = len.load(Relaxed);
        let k = got.len().min(samples.len() - at);
        samples[at..at + k].copy_from_slice(&got[..k]);
        len.store(at + k, Relaxed);
        0
    }

    /// Times of the rising zero crossings, in samples, interpolated.
    fn rising_crossings(x: &[f32]) -> Vec<f64> {
        x.windows(2)
            .enumerate()
            .filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0)
            .map(|(i, w)| i as f64 + f64::from(w[0] / (w[0] - w[1])))
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    #[ignore = "plays into the CursorJump Mic Feed and records the Microphone; needs the driver and no sandbox"]
    fn a_tone_in_the_feed_comes_out_of_the_microphone() {
        let mic = device_id(MIC_UID).expect("the CursorJump audio driver is installed");
        let (tx, rx) = audio_pipe(AudioStream::PcMic);
        let stats = rx.stats();
        let feed = match MicFeed::start(rx) {
            Ok(f) => f,
            Err((e, _)) => panic!("MicFeed::start: {e:#}"),
        };

        // The network: one 10 ms packet of the tone every 10 ms, on a
        // monotonic schedule.
        let done = Arc::new(AtomicBool::new(false));
        let feeder = std::thread::spawn({
            let done = done.clone();
            move || {
                let frames = AudioStream::PcMic.packet_frames();
                let period = Duration::from_millis(10);
                let t0 = Instant::now();
                let mut n = 0u64;
                for seq in 0u32.. {
                    if done.load(Relaxed) {
                        break;
                    }
                    let packet: Vec<f32> = (0..frames)
                        .map(|_| {
                            let phase = n as f64 * TONE_HZ / f64::from(RATE);
                            n += 1;
                            ((phase * std::f64::consts::TAU).sin() * AMPLITUDE) as f32
                        })
                        .collect();
                    tx.push(seq, &encode_pcm(&packet));
                    let due = t0 + period * (seq + 1);
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                }
            }
        });

        let recording = Box::into_raw(Box::new(Recording {
            samples: vec![0.0; RECORD_S * RATE as usize].into_boxed_slice(),
            len: AtomicUsize::new(0),
            odd_shape: AtomicBool::new(false),
        }));
        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: `proc_id` outlives the call; the recording outlives the
        // IOProc (taken back only after it is destroyed).
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                mic,
                Some(record_proc),
                recording.cast(),
                NonNull::from(&mut proc_id),
            )
        };
        assert_eq!(st, 0, "AudioDeviceCreateIOProcID");
        // SAFETY: the IOProc just created on this device.
        let st = unsafe { AudioDeviceStart(mic, proc_id) };
        assert_eq!(st, 0, "AudioDeviceStart");

        // SAFETY: `recording` is live until taken back below; `len` is atomic.
        let len = unsafe { &(*recording).len };
        let deadline = Instant::now() + Duration::from_secs(RECORD_S as u64 + 3);
        while len.load(Relaxed) < RECORD_S * RATE as usize && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: as above; stopped, then destroyed once.
        unsafe {
            AudioDeviceStop(mic, proc_id);
            AudioDeviceDestroyIOProcID(mic, proc_id);
        }
        // SAFETY: from Box::into_raw above; its IOProc is destroyed.
        let recording = unsafe { Box::from_raw(recording) };
        let _rx = feed.stop();
        done.store(true, Relaxed);
        feeder.join().unwrap();

        let got = &recording.samples[..recording.len.load(Relaxed)];
        println!("{}", stats.snapshot().log_line("PC mic"));
        assert!(
            !recording.odd_shape.load(Relaxed),
            "the mic's buffers weren't one mono buffer"
        );
        assert_eq!(got.len(), RECORD_S * RATE as usize, "recorded 2 s in time");
        let measured = &got[MEASURE_FROM..];
        let crossings = rising_crossings(measured);
        assert!(crossings.len() > 2, "no tone in the recording");
        let span = crossings[crossings.len() - 1] - crossings[0];
        let pitch = (crossings.len() - 1) as f64 * f64::from(RATE) / span;
        let level_db = 20.0 * (rms(measured) / (AMPLITUDE / 2f64.sqrt())).log10();
        println!("pitch {pitch:.3} Hz, level {level_db:+.3} dB");
        assert!((pitch - TONE_HZ).abs() <= 2.0, "pitch {pitch} Hz");
        assert!(level_db.abs() <= 1.0, "level off by {level_db} dB");
    }
}
