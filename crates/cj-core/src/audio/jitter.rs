//! The jitter buffer: reorders packets, waits for a cushion of audio, and
//! hides losses, underruns and overflows with short fades.
//!
//! Every break in the audio (a lost packet, an underrun, skipped packets, a
//! start) fades the 1 ms before it out and the 1 ms after it in. To fade out
//! *before* a break, the output runs 1 ms behind: `Fader` holds the last
//! `FADE_FRAMES` frames produced, and a break scales them down before they
//! are handed out.
//!
//! Nothing allocates or frees after `new`: packets are copied into a ring of
//! fixed slots, so `push` and `pull` are safe on a real-time audio thread.
use super::receiver::AudioStats;
use super::{AudioStream, RATE};

const FRAMES_PER_MS: usize = RATE as usize / 1000;
/// Length of every fade-out and fade-in: 1 ms.
pub(crate) const FADE_FRAMES: usize = FRAMES_PER_MS;
const INITIAL_TARGET: usize = 30 * FRAMES_PER_MS;
const MIN_TARGET: usize = 20 * FRAMES_PER_MS;
const MAX_TARGET: usize = 80 * FRAMES_PER_MS;
/// How much an underrun raises the target.
const UNDERRUN_STEP: usize = 10 * FRAMES_PER_MS;
/// How much a calm minute lowers the target.
const CALM_STEP: usize = 5 * FRAMES_PER_MS;
/// Output with no underrun that counts as a calm minute: 60 s.
const CALM_FRAMES: usize = 60 * RATE as usize;
/// Fill above the target that triggers a skip.
const OVERFLOW_MARGIN: usize = 60 * FRAMES_PER_MS;
/// A packet further behind or ahead of the play position than this starts a
/// new sequence.
const MAX_BEHIND: i32 = 100;
const MAX_AHEAD: i32 = 1000;
/// The ring holds 1 s of packets: far more than the largest target plus the
/// overflow margin (140 ms). A packet too far ahead to fit also starts a new
/// sequence, since everything queued before it is stale.
const RING_FRAMES: usize = RATE as usize;
/// Most frames the fader holds beyond its delay; `pull` works in pieces of
/// this size.
const PIECE_FRAMES: usize = 480;

#[derive(Debug, Clone)]
pub(crate) struct JitterBuffer {
    channels: usize,
    packet_frames: usize,
    /// One slot of `packet_frames` frames per sequence number, `capacity`
    /// slots in all.
    ring: Vec<f32>,
    /// Whether each slot holds a packet. Always false outside the `len`
    /// slots from `head_slot`.
    present: Vec<bool>,
    capacity: usize,
    /// The ring slot of `head`.
    head_slot: usize,
    /// Slots in use from `head_slot` on; the last one is always present.
    len: usize,
    /// Sequence number of the slot playing or next to play.
    head: u32,
    /// Frames of the head slot already played.
    pos: usize,
    /// The last slot played, concealed or skipped; a packet at or before it
    /// is late. `None` until this sequence has played anything.
    last_played: Option<u32>,
    /// False while waiting for the fill to reach the target.
    playing: bool,
    /// In frames.
    target: usize,
    /// Frames played since the last underrun or calm step.
    calm: usize,
    fader: Fader,
    received: u64,
    lost: u64,
    late: u64,
    underruns: u64,
    skips: u64,
}

impl JitterBuffer {
    pub(crate) fn new(stream: AudioStream) -> Self {
        let channels = stream.channels();
        let capacity = RING_FRAMES / stream.packet_frames();
        let mut jb = JitterBuffer {
            channels,
            packet_frames: stream.packet_frames(),
            ring: vec![0.0; capacity * stream.packet_samples()],
            present: vec![false; capacity],
            capacity,
            head_slot: 0,
            len: 0,
            head: 0,
            pos: 0,
            last_played: None,
            playing: false,
            target: INITIAL_TARGET,
            calm: 0,
            fader: Fader::new(channels),
            received: 0,
            lost: 0,
            late: 0,
            underruns: 0,
            skips: 0,
        };
        jb.reset();
        jb
    }

    /// Back to the state of a new buffer, counters included, without
    /// allocating.
    pub(crate) fn reset(&mut self) {
        self.present.fill(false);
        self.head_slot = 0;
        self.len = 0;
        self.head = 0;
        self.pos = 0;
        self.last_played = None;
        self.playing = false;
        self.target = INITIAL_TARGET;
        self.calm = 0;
        self.fader.reset();
        self.received = 0;
        self.lost = 0;
        self.late = 0;
        self.underruns = 0;
        self.skips = 0;
    }

    /// Copies packet `seq` into its slot. A packet of the wrong length is
    /// padded with silence or cut to length.
    pub(crate) fn push(&mut self, seq: u32, samples: &[f32]) {
        self.received += 1;
        let fresh = self.len == 0 && self.last_played.is_none();
        let ahead = seq.wrapping_sub(self.head) as i32;
        if !fresh && !(-MAX_BEHIND..=MAX_AHEAD).contains(&ahead) {
            self.new_sequence();
        }
        if let Some(last) = self.last_played
            && seq.wrapping_sub(last) as i32 <= 0
        {
            self.late += 1;
            return;
        }
        if self.len == 0 {
            // A new sequence, or the first packet after an underrun: play
            // from here.
            self.anchor(seq, samples);
            return;
        }
        let ahead = seq.wrapping_sub(self.head) as i32;
        if ahead < 0 {
            // Earlier than anything queued, and nothing played since: this
            // only happens while waiting to start.
            let before = ahead.unsigned_abs() as usize;
            if self.len + before > self.capacity {
                self.late += 1;
                return;
            }
            self.head_slot = (self.head_slot + self.capacity - before) % self.capacity;
            self.len += before;
            self.head = seq;
            self.store(0, samples);
            return;
        }
        let i = ahead as usize;
        if i >= self.capacity {
            self.new_sequence();
            self.anchor(seq, samples);
            return;
        }
        self.len = self.len.max(i + 1);
        if self.present[self.slot(i)] {
            self.late += 1;
        } else {
            self.store(i, samples);
        }
    }

    /// Fills `out` (interleaved) with audio or silence.
    pub(crate) fn pull(&mut self, out: &mut [f32]) {
        let n = out.len() - out.len() % self.channels;
        let (whole, rest) = out.split_at_mut(n);
        for piece in whole.chunks_mut(PIECE_FRAMES * self.channels) {
            let mut left = piece.len() / self.channels;
            while left > 0 {
                left -= self.produce(left);
            }
            self.fader.take(piece);
        }
        rest.fill(0.0);
    }

    pub(crate) fn is_playing(&self) -> bool {
        self.playing
    }

    /// Frames queued from the play position on, gaps included.
    pub(crate) fn fill_frames(&self) -> usize {
        (self.len * self.packet_frames).saturating_sub(self.pos)
    }

    pub(crate) fn target_frames(&self) -> usize {
        self.target
    }

    pub(crate) fn stats(&self) -> AudioStats {
        AudioStats {
            received: self.received,
            lost: self.lost,
            late: self.late,
            underruns: self.underruns,
            skips: self.skips,
            target_ms: (self.target / FRAMES_PER_MS) as u32,
            fill_ms: (self.fill_frames() / FRAMES_PER_MS) as u32,
        }
    }

    /// The ring slot `i` slots after the head.
    fn slot(&self, i: usize) -> usize {
        (self.head_slot + i) % self.capacity
    }

    fn store(&mut self, i: usize, samples: &[f32]) {
        let slot = self.slot(i);
        let size = self.packet_frames * self.channels;
        let dst = &mut self.ring[slot * size..(slot + 1) * size];
        let n = samples.len().min(size);
        dst[..n].copy_from_slice(&samples[..n]);
        dst[n..].fill(0.0);
        self.present[slot] = true;
    }

    /// Starts an empty queue at `seq`.
    fn anchor(&mut self, seq: u32, samples: &[f32]) {
        self.head = seq;
        self.len = 1;
        self.store(0, samples);
    }

    fn pop_front(&mut self) {
        self.present[self.head_slot] = false;
        self.head_slot = (self.head_slot + 1) % self.capacity;
        self.len -= 1;
        self.head = self.head.wrapping_add(1);
    }

    /// Drops the queue and waits for a new start.
    fn new_sequence(&mut self) {
        while self.len > 0 {
            self.pop_front();
        }
        self.pos = 0;
        self.last_played = None;
        self.playing = false;
    }

    /// Adds up to `max` frames to the fader and returns how many it added.
    fn produce(&mut self, max: usize) -> usize {
        if !self.playing {
            if self.len == 0 || self.fill_frames() < self.target {
                self.fader.silence(max);
                return max;
            }
            self.playing = true;
        }
        if self.pos == 0 {
            if self.len == 0 {
                self.underrun();
                return 0;
            }
            self.skip_overflow();
            self.last_played = Some(self.head);
            if !self.present[self.head_slot] {
                self.lost += 1;
            }
        }
        let n = max.min(self.packet_frames - self.pos);
        if self.present[self.head_slot] {
            let ch = self.channels;
            let start = (self.head_slot * self.packet_frames + self.pos) * ch;
            self.fader
                .audio(self.head, &self.ring[start..start + n * ch]);
        } else {
            self.fader.silence(n);
        }
        self.pos += n;
        if self.pos == self.packet_frames {
            self.pop_front();
            self.pos = 0;
        }
        self.calm += n;
        if self.calm >= CALM_FRAMES {
            self.calm = 0;
            self.target = self.target.saturating_sub(CALM_STEP).max(MIN_TARGET);
        }
        n
    }

    fn underrun(&mut self) {
        self.playing = false;
        self.underruns += 1;
        self.calm = 0;
        self.target = (self.target + UNDERRUN_STEP).min(MAX_TARGET);
    }

    /// At a packet boundary, with the fill over target + 60 ms: drops whole
    /// packets until it is back within a packet of the target.
    fn skip_overflow(&mut self) {
        if self.fill_frames() <= self.target + OVERFLOW_MARGIN {
            return;
        }
        while self.fill_frames() >= self.target + self.packet_frames {
            self.last_played = Some(self.head);
            self.pop_front();
        }
        self.skips += 1;
    }
}

/// Delays the output by `FADE_FRAMES` so that a break can fade out the
/// frames before it, and fades in the frames after it.
#[derive(Debug, Clone)]
struct Fader {
    channels: usize,
    /// Frames produced and not yet taken, interleaved: at least
    /// `FADE_FRAMES`, at most `FADE_FRAMES + PIECE_FRAMES`.
    buf: Vec<f32>,
    frames: usize,
    /// The packet the last frame produced came from; `None` for silence.
    last_source: Option<u32>,
    /// Frames of fade-in still to apply.
    fade_in_left: usize,
}

impl Fader {
    fn new(channels: usize) -> Self {
        Fader {
            channels,
            buf: vec![0.0; (FADE_FRAMES + PIECE_FRAMES) * channels],
            frames: FADE_FRAMES,
            last_source: None,
            fade_in_left: 0,
        }
    }

    fn reset(&mut self) {
        self.buf.fill(0.0);
        self.frames = FADE_FRAMES;
        self.last_source = None;
        self.fade_in_left = 0;
    }

    /// Adds frames of packet `seq`; a packet that doesn't follow the last
    /// one is a break.
    fn audio(&mut self, seq: u32, samples: &[f32]) {
        let follows =
            self.last_source == Some(seq) || self.last_source == Some(seq.wrapping_sub(1));
        if !follows {
            self.fade_out_tail();
            self.fade_in_left = FADE_FRAMES;
        }
        self.last_source = Some(seq);
        let start = self.frames * self.channels;
        let dst = &mut self.buf[start..start + samples.len()];
        dst.copy_from_slice(samples);
        let fading = self.fade_in_left.min(samples.len() / self.channels);
        for frame in dst[..fading * self.channels].chunks_mut(self.channels) {
            self.fade_in_left -= 1;
            let gain = (FADE_FRAMES - self.fade_in_left) as f32 / (FADE_FRAMES + 1) as f32;
            frame.iter_mut().for_each(|s| *s *= gain);
        }
        self.frames += samples.len() / self.channels;
    }

    fn silence(&mut self, frames: usize) {
        if self.last_source.take().is_some() {
            self.fade_out_tail();
        }
        let start = self.frames * self.channels;
        self.buf[start..start + frames * self.channels].fill(0.0);
        self.frames += frames;
    }

    /// Fades the last `FADE_FRAMES` frames produced down towards 0.
    fn fade_out_tail(&mut self) {
        let ch = self.channels;
        let end = self.frames * ch;
        for (i, s) in self.buf[end - FADE_FRAMES * ch..end].iter_mut().enumerate() {
            *s *= (FADE_FRAMES - i / ch) as f32 / (FADE_FRAMES + 1) as f32;
        }
    }

    /// Hands out the oldest `out.len()` samples, keeping the delay.
    fn take(&mut self, out: &mut [f32]) {
        let n = out.len();
        out.copy_from_slice(&self.buf[..n]);
        self.buf.copy_within(n..self.frames * self.channels, 0);
        self.frames -= n / self.channels;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CH: usize = 2;
    const PF: usize = 240;

    fn buffer() -> JitterBuffer {
        JitterBuffer::new(AudioStream::MacSound)
    }

    /// A MacSound packet whose samples are all distinct and nonzero.
    fn packet(seq: u32) -> Vec<f32> {
        (0..PF * CH)
            .map(|i| 0.01 * (seq % 50 + 1) as f32 + 1e-5 * i as f32)
            .collect()
    }

    /// Packet `seq` of a continuous 1 kHz sine at 0.5, in both channels.
    fn sine_packet(seq: u32) -> Vec<f32> {
        (0..PF)
            .flat_map(|i| {
                let n = (seq as usize * PF + i) as f32;
                let v = 0.5 * (std::f32::consts::TAU * 1000.0 * n / RATE as f32).sin();
                [v, v]
            })
            .collect()
    }

    fn pull(jb: &mut JitterBuffer, frames: usize) -> Vec<f32> {
        let mut out = vec![f32::NAN; frames * CH];
        jb.pull(&mut out);
        out
    }

    /// Push one packet and pull one packet's worth, like a sender and a
    /// sound card running in step.
    fn step(jb: &mut JitterBuffer, seq: u32) -> Vec<f32> {
        jb.push(seq, &packet(seq));
        pull(jb, PF)
    }

    #[test]
    fn jitter_waits_for_the_target_then_plays_in_order() {
        let mut jb = buffer();
        for seq in [3, 1, 2, 0, 4] {
            jb.push(seq, &packet(seq));
            assert!(
                pull(&mut jb, 10).iter().all(|&s| s == 0.0),
                "25 ms is short of the target"
            );
        }
        for seq in 5..=9 {
            jb.push(seq, &packet(seq));
        }
        let out = pull(&mut jb, 10 * PF);
        let fade = FADE_FRAMES * CH;
        // The output runs 1 ms behind, so a gap's fade-out can start before it.
        assert!(out[..fade].iter().all(|&s| s == 0.0));
        let played = &out[fade..];
        let expected: Vec<f32> = (0..=9).flat_map(packet).collect();
        assert_eq!(played[fade..], expected[fade..played.len()]);
        for (p, e) in played[..fade].iter().zip(&expected) {
            assert!(*p > 0.0 && p < e, "the start fades in");
        }
    }

    #[test]
    fn duplicates_and_late_packets_are_dropped() {
        let mut clean = buffer();
        let mut messy = buffer();
        for seq in 0..6 {
            clean.push(seq, &packet(seq));
            messy.push(seq, &packet(seq));
        }
        messy.push(2, &[0.9; PF * CH]);
        let mut a = pull(&mut clean, 3 * PF);
        let mut b = pull(&mut messy, 3 * PF);
        messy.push(0, &packet(0));
        for seq in 6..10 {
            clean.push(seq, &packet(seq));
            messy.push(seq, &packet(seq));
        }
        a.extend(pull(&mut clean, 6 * PF));
        b.extend(pull(&mut messy, 6 * PF));
        assert_eq!(messy.stats().late, 2);
        assert_eq!(clean.stats().late, 0);
        assert_eq!(a, b);
    }

    #[test]
    fn single_loss_fades_without_a_click() {
        let mut jb = buffer();
        let mut out = Vec::new();
        for seq in 0..40 {
            if seq != 5 {
                jb.push(seq, &sine_packet(seq));
            }
            out.extend(pull(&mut jb, PF));
        }
        let stats = jb.stats();
        assert_eq!((stats.lost, stats.underruns), (1, 0));
        assert!(out.iter().any(|s| s.abs() > 0.45), "the sine played");
        let frames = out.as_chunks::<CH>().0;
        let worst = frames
            .windows(2)
            .flat_map(|w| w[0].iter().zip(&w[1]).map(|(a, b)| (b - a).abs()))
            .fold(0.0, f32::max);
        assert!(worst <= 0.1, "a jump of {worst}");
        // The gap itself is silent.
        let start = frames.iter().position(|f| f[0] != 0.0).unwrap();
        let mut run = 0;
        let mut longest = 0;
        for f in &frames[start..] {
            run = if f.iter().all(|&s| s == 0.0) {
                run + 1
            } else {
                0
            };
            longest = longest.max(run);
        }
        assert!(longest >= PF, "the lost packet plays as silence");
    }

    #[test]
    fn underrun_rebuffers_and_raises_the_target() {
        let mut jb = buffer();
        let mut seq = 0;
        while seq < 20 {
            step(&mut jb, seq);
            seq += 1;
        }
        assert_eq!(jb.stats().underruns, 0);
        let starved = pull(&mut jb, 50 * 48);
        let stats = jb.stats();
        assert_eq!((stats.underruns, stats.target_ms), (1, 40));
        assert!(starved[starved.len() - PF * CH..].iter().all(|&s| s == 0.0));
        let mut resumed = None;
        for i in 0..20 {
            let out = step(&mut jb, seq);
            seq += 1;
            if resumed.is_none() && out.iter().any(|&s| s != 0.0) {
                resumed = Some(i);
            }
        }
        assert_eq!(resumed, Some(7), "plays again once 40 ms is queued");
        assert_eq!(jb.stats().underruns, 1);
    }

    #[test]
    fn calm_lowers_the_target() {
        let mut jb = buffer();
        // Playing starts on the 6th packet; 60 s is 12 000 packets later.
        for seq in 0..11_990 {
            step(&mut jb, seq);
        }
        assert_eq!(jb.stats().target_ms, 30);
        for seq in 11_990..12_100 {
            step(&mut jb, seq);
        }
        assert_eq!(jb.stats().target_ms, 25);
        // 20 ms after two minutes, and still 20 after three.
        for seq in 12_100..36_100 {
            step(&mut jb, seq);
        }
        let stats = jb.stats();
        assert_eq!((stats.target_ms, stats.underruns), (20, 0));
    }

    #[test]
    fn overflow_skips_down_to_the_target() {
        let mut jb = buffer();
        for seq in 0..40 {
            jb.push(seq, &packet(seq));
        }
        pull(&mut jb, PF);
        let stats = jb.stats();
        assert!(stats.skips > 0);
        assert_eq!(stats.underruns, 0);
        assert!(stats.fill_ms <= stats.target_ms + 5, "{stats:?}");
    }

    #[test]
    fn a_new_sequence_resets() {
        let mut jb = buffer();
        for seq in 0..=10 {
            jb.push(seq, &packet(seq));
        }
        jb.push(50_000, &packet(50_000));
        let stats = jb.stats();
        assert_eq!(
            (stats.late, stats.fill_ms),
            (0, 5),
            "only the new packet is left"
        );
        jb.push(3, &packet(3));
        let stats = jb.stats();
        assert_eq!((stats.late, stats.fill_ms), (0, 5), "far behind is new too");
        // So is a packet too far ahead to fit in the ring (1 s).
        let mut jb = buffer();
        jb.push(0, &packet(0));
        jb.push(199, &packet(199));
        assert_eq!(jb.stats().fill_ms, 1000, "199 ahead still fits");
        jb.push(200, &packet(200));
        let stats = jb.stats();
        assert_eq!((stats.late, stats.fill_ms), (0, 5));
        // A wrapping sequence number is not a new sequence.
        let mut jb = buffer();
        jb.push(u32::MAX, &packet(0));
        jb.push(0, &packet(1));
        let stats = jb.stats();
        assert_eq!((stats.late, stats.fill_ms), (0, 10));
    }
}
