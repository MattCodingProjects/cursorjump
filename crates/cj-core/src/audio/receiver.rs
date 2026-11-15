//! The receiving end of a stream: a jitter buffer, then a resampler whose
//! ratio is steered so the buffer's fill holds at its target however far
//! the sender's clock is from the sound card's.
//!
//! Nothing allocates or frees after `new`, so `push`, `pull` and `reset` are
//! safe on a real-time audio thread.
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use super::jitter::JitterBuffer;
use super::{AudioStream, RATE};

/// Proportional gain: correction per ms of (smoothed) fill error.
///
/// With `KI`, MacSound's loop (200 packets/s) is about critically damped
/// with a time constant of about 10 s; PcMic's is slower and overdamped.
const KP: f64 = 2e-4;
/// Integral gain: correction per ms of smoothed fill error per packet.
const KI: f64 = 5e-8;
/// Time constant of the low-pass on the fill error, in seconds. The raw
/// fill is a sawtooth of a packet or more (arrivals, and the device pulling
/// several packets at once) plus network jitter; steering on it directly
/// would wobble the pitch at 100-200 Hz. Smoothed, a 5 ms step moves the
/// correction by at most `KP · 5 · packet / SMOOTHING` (5 ppm for
/// MacSound).
const SMOOTHING_S: f64 = 1.0;
/// Largest correction either way: 1000 ppm.
const MAX_CORRECTION: f64 = 1e-3;
/// Headroom the resampler is built with, beyond `MAX_CORRECTION`.
const MAX_RELATIVE_RATIO: f64 = 1.01;
const FRAMES_PER_MS: f64 = RATE as f64 / 1000.0;

/// Counters and levels for one stream, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioStats {
    pub received: u64,
    pub lost: u64,
    pub late: u64,
    pub underruns: u64,
    pub skips: u64,
    pub target_ms: u32,
    pub fill_ms: u32,
}

impl AudioStats {
    /// For example "Mac sound: 12000 packets, 3 lost, 0 late, 1 underrun,
    /// 0 skips, buffer 34 of 40 ms".
    pub fn log_line(&self, what: &str) -> String {
        fn count(n: u64, one: &str) -> String {
            if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {one}s")
            }
        }
        format!(
            "{what}: {}, {} lost, {} late, {}, {}, buffer {} of {} ms",
            count(self.received, "packet"),
            self.lost,
            self.late,
            count(self.underruns, "underrun"),
            count(self.skips, "skip"),
            self.fill_ms,
            self.target_ms,
        )
    }
}

/// Packets in, an even stream of samples out.
pub struct AudioReceiver {
    stream: AudioStream,
    jitter: JitterBuffer,
    /// Fixed output of one packet's frames per call; input as needed.
    resampler: Async<f32>,
    /// Room for the most input one resampler call can need, interleaved.
    input: Vec<f32>,
    /// One packet of resampled output, interleaved.
    output: Vec<f32>,
    /// The part of `output` not yet pulled.
    ready: std::ops::Range<usize>,
    /// Low-passed fill error in ms; `None` until the first measurement
    /// after a (re)start.
    error_ms: Option<f64>,
    /// Sum of the smoothed fill error, in ms, once per packet.
    integral: f64,
    #[cfg(test)]
    corrections: Vec<f64>,
}

impl AudioReceiver {
    pub fn new(stream: AudioStream) -> Self {
        // At ratios this close to 1 a 128-tap filter is transparent, and
        // linear interpolation between 256 sub-steps of it is plenty.
        Self::with_sinc_len(stream, 128)
    }

    fn with_sinc_len(stream: AudioStream, sinc_len: usize) -> Self {
        let params = SincInterpolationParameters::new(sinc_len, WindowFunction::BlackmanHarris2)
            .oversampling_factor(256)
            .interpolation(SincInterpolationType::Linear);
        let resampler = Async::new_sinc(
            1.0,
            MAX_RELATIVE_RATIO,
            &params,
            stream.packet_frames(),
            stream.channels(),
            FixedAsync::Output,
        )
        .expect("valid resampler parameters");
        let ch = stream.channels();
        AudioReceiver {
            stream,
            jitter: JitterBuffer::new(stream),
            input: vec![0.0; resampler.input_frames_max() * ch],
            output: vec![0.0; resampler.output_frames_max() * ch],
            resampler,
            ready: 0..0,
            error_ms: None,
            integral: 0.0,
            #[cfg(test)]
            corrections: Vec::new(),
        }
    }

    /// Copies packet `seq` in; `samples` is one packet, interleaved.
    pub fn push(&mut self, seq: u32, samples: &[f32]) {
        self.jitter.push(seq, samples);
    }

    /// Fills `out` (interleaved, any length) with audio or silence; never blocks.
    pub fn pull(&mut self, out: &mut [f32]) {
        let n = out.len() - out.len() % self.stream.channels();
        let (whole, rest) = out.split_at_mut(n);
        let mut done = 0;
        while done < n {
            if self.ready.is_empty() {
                self.resample_packet();
            }
            let k = (n - done).min(self.ready.len());
            let from = self.ready.start;
            whole[done..done + k].copy_from_slice(&self.output[from..from + k]);
            self.ready.start += k;
            done += k;
        }
        rest.fill(0.0);
    }

    /// Back to the state of a new receiver, counters included.
    pub fn reset(&mut self) {
        self.jitter.reset();
        self.resampler.reset();
        self.ready = 0..0;
        self.error_ms = None;
        self.integral = 0.0;
    }

    pub fn stats(&self) -> AudioStats {
        self.jitter.stats()
    }

    /// Resamples one packet's worth of output from the jitter buffer, then
    /// steers the ratio.
    fn resample_packet(&mut self) {
        let ch = self.stream.channels();
        let frames_in = self.resampler.input_frames_next();
        let frames_out = self.resampler.output_frames_next();
        let input = &mut self.input[..frames_in * ch];
        self.jitter.pull(input);
        let input = InterleavedSlice::new(&*input, ch, frames_in).expect("sized in new");
        let mut output =
            InterleavedSlice::new_mut(&mut self.output[..frames_out * ch], ch, frames_out)
                .expect("sized in new");
        let (_, written) = self
            .resampler
            .process_into_buffer(&input, &mut output, None)
            .expect("buffers sized to the resampler");
        self.ready = 0..written * ch;
        self.steer();
    }

    /// A PI controller on the low-passed fill error. Waiting to (re)start
    /// resets it.
    fn steer(&mut self) {
        let correction = if self.jitter.is_playing() {
            let raw = (self.jitter.fill_frames() as f64 - self.jitter.target_frames() as f64)
                / FRAMES_PER_MS;
            let alpha = self.stream.packet_frames() as f64 / (SMOOTHING_S * f64::from(RATE));
            let error = match self.error_ms {
                Some(e) => e + alpha * (raw - e),
                None => raw,
            };
            self.error_ms = Some(error);
            // Never let the integral alone ask for more than the clamp.
            let limit = MAX_CORRECTION / KI;
            self.integral = (self.integral + error).clamp(-limit, limit);
            // A fill above target means the sender is ahead: play faster,
            // which is a ratio (output / input) below 1.
            -(KP * error + KI * self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION)
        } else {
            self.error_ms = None;
            self.integral = 0.0;
            0.0
        };
        #[cfg(test)]
        self.corrections.push(correction);
        self.resampler
            .set_resample_ratio_relative(1.0 + correction, true)
            .expect("correction within the resampler's range");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::pcm::{Packetizer, decode_pcm, encode_pcm};

    /// Frames per `pull`: 10 ms, like a sound card's period.
    const BLOCK: usize = 480;

    /// A sender whose clock runs `ppm` fast (or slow) for ten simulated
    /// minutes, against a sound card pulling 10 ms at a time.
    fn drift(stream: AudioStream, ppm: f64) {
        // A short filter: the controller doesn't depend on its length, and
        // ten minutes through the full one is slow in a debug build.
        let mut rx = AudioReceiver::with_sinc_len(stream, 8);
        let frames = stream.packet_frames() as f64;
        let silence = vec![0.0; stream.packet_samples()];
        let mut out = vec![0.0; BLOCK * stream.channels()];
        let (mut owed, mut seq) = (0.0, 0u32);
        for pull in 0..10 * 60 * 100 {
            owed += BLOCK as f64 * (1.0 + ppm * 1e-6);
            while owed >= frames {
                rx.push(seq, &silence);
                seq += 1;
                owed -= frames;
            }
            rx.pull(&mut out);
            let s = rx.stats();
            if pull >= 30 * 100 {
                let off = s.fill_ms.abs_diff(s.target_ms);
                assert!(off <= 10, "{ppm} ppm, {} s in: {s:?}", pull / 100);
            }
        }
        let s = rx.stats();
        assert_eq!((s.skips, s.underruns), (0, 0), "{ppm} ppm: {s:?}");
    }

    #[test]
    fn drift_of_500_ppm_is_absorbed_without_skips() {
        std::thread::scope(|s| {
            s.spawn(|| drift(AudioStream::MacSound, 500.0));
            drift(AudioStream::MacSound, -500.0);
        });
    }

    /// Packets sent on time but arriving 0-15 ms late at random, against a
    /// sound card pulling 10 ms (two packets) at a time: the correction
    /// must move smoothly, not chase every arrival.
    #[test]
    fn correction_moves_smoothly_under_jitter() {
        let mut rx = AudioReceiver::with_sinc_len(AudioStream::MacSound, 8);
        let packet = [0.0; 480];
        let mut x: u32 = 0x9e37_79b9;
        let mut arrivals: Vec<(f64, u32)> = (0..12_000)
            .map(|seq| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let late_ms = f64::from(x) / f64::from(u32::MAX) * 15.0;
                (f64::from(seq) * 5.0 + late_ms, seq)
            })
            .collect();
        arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut arrivals = arrivals.into_iter().peekable();
        let mut out = [0.0; BLOCK * 2];
        for pull in 0..5_900 {
            let now = f64::from(pull) * 10.0;
            while let Some((_, seq)) = arrivals.next_if(|&(t, _)| t <= now) {
                rx.push(seq, &packet);
            }
            rx.pull(&mut out);
        }
        let s = rx.stats();
        assert_eq!((s.underruns, s.skips), (0, 0), "{s:?}");
        // From 5 s on, past the start.
        let steps = rx.corrections[1000..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs());
        let worst_ppm = steps.fold(0.0, f64::max) * 1e6;
        assert!(
            worst_ppm <= 15.0,
            "the correction jumped {worst_ppm} ppm in one packet"
        );
    }

    #[test]
    fn a_sine_passes_at_the_same_level_and_pitch() {
        for stream in [AudioStream::MacSound, AudioStream::PcMic] {
            let ch = stream.channels();
            let mut packetizer = Packetizer::new(stream);
            let mut rx = AudioReceiver::new(stream);
            let mut out = vec![0.0; BLOCK * ch];
            let mut played = Vec::new();
            for block in 0..200 {
                let sent: Vec<f32> = (0..BLOCK)
                    .flat_map(|i| {
                        let n = (block * BLOCK + i) as f64;
                        let v = 0.5 * (std::f64::consts::TAU * 1000.0 * n / RATE as f64).sin();
                        std::iter::repeat_n(v as f32, ch)
                    })
                    .collect();
                packetizer.push(&sent, |seq, pkt| {
                    rx.push(seq, &decode_pcm(&encode_pcm(pkt)))
                });
                rx.pull(&mut out);
                played.extend_from_slice(&out);
            }
            // The first channel, from 0.5 s on: well past the start.
            let x: Vec<f64> = played
                .chunks_exact(ch)
                .skip(RATE as usize / 2)
                .map(|f| f64::from(f[0]))
                .collect();
            let rms = (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt();
            let db = 20.0 * (rms / (0.5 / 2f64.sqrt())).log10();
            assert!(db.abs() < 0.1, "{stream:?}: level off by {db} dB");
            let crossings: Vec<f64> = x
                .windows(2)
                .enumerate()
                .filter(|(_, w)| (w[0] < 0.0) != (w[1] < 0.0))
                .map(|(i, w)| i as f64 + w[0] / (w[0] - w[1]))
                .collect();
            let span = (crossings[crossings.len() - 1] - crossings[0]) / RATE as f64;
            let hz = (crossings.len() - 1) as f64 / 2.0 / span;
            assert!((hz - 1000.0).abs() <= 1.0, "{stream:?}: pitch {hz} Hz");
        }
    }

    #[test]
    fn reset_starts_over() {
        let mut rx = AudioReceiver::new(AudioStream::PcMic);
        for seq in 0..20 {
            rx.push(seq, &[0.25; 480]);
        }
        let mut out = vec![0.0; 480];
        rx.pull(&mut out);
        assert!(out.iter().any(|&s| s != 0.0));
        rx.reset();
        assert_eq!(rx.stats(), AudioReceiver::new(AudioStream::PcMic).stats());
        rx.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn stats_read_as_one_log_line() {
        let s = AudioStats {
            received: 12000,
            lost: 3,
            late: 0,
            underruns: 1,
            skips: 0,
            target_ms: 40,
            fill_ms: 34,
        };
        assert_eq!(
            s.log_line("Mac sound"),
            "Mac sound: 12000 packets, 3 lost, 0 late, 1 underrun, 0 skips, buffer 34 of 40 ms"
        );
    }
}
