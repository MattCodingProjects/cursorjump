//! The wire format: 16-bit little-endian PCM, cut into fixed-size packets.
use super::AudioStream;

/// Full scale of an i16 sample: `-1.0` is -32768, and `+1.0` saturates to
/// 32767.
const SCALE: f32 = 32768.0;

/// Encodes samples as i16 little-endian, clamped to ±1.0 and rounded. NaN
/// encodes as 0.
pub fn encode_pcm(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        // `as` saturates to the i16 range and maps NaN to 0.
        .flat_map(|&x| ((x.clamp(-1.0, 1.0) * SCALE).round() as i16).to_le_bytes())
        .collect()
}

/// Decodes i16 little-endian samples; an odd trailing byte is ignored.
pub fn decode_pcm(bytes: &[u8]) -> Vec<f32> {
    let (pairs, _odd) = bytes.as_chunks::<2>();
    pairs
        .iter()
        .map(|&b| f32::from(i16::from_le_bytes(b)) / SCALE)
        .collect()
}

/// Cuts a stream of interleaved samples, delivered in blocks of any size,
/// into whole packets numbered from 0 (wrapping).
#[derive(Debug, Clone)]
pub struct Packetizer {
    samples_per_packet: usize,
    pending: Vec<f32>,
    seq: u32,
}

impl Packetizer {
    pub fn new(stream: AudioStream) -> Self {
        Packetizer {
            samples_per_packet: stream.packet_samples(),
            pending: Vec::with_capacity(stream.packet_samples()),
            seq: 0,
        }
    }

    /// Adds `interleaved` and calls `emit(seq, packet)` for each packet it
    /// completes. A partial packet is kept for the next call.
    pub fn push(&mut self, mut interleaved: &[f32], mut emit: impl FnMut(u32, &[f32])) {
        while !interleaved.is_empty() {
            let take = (self.samples_per_packet - self.pending.len()).min(interleaved.len());
            let (now, rest) = interleaved.split_at(take);
            self.pending.extend_from_slice(now);
            interleaved = rest;
            if self.pending.len() == self.samples_per_packet {
                emit(self.seq, &self.pending);
                self.seq = self.seq.wrapping_add(1);
                self.pending.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_roundtrip_is_within_one_step() {
        // A fixed xorshift sequence, so a failure always reproduces.
        let mut x: u32 = 0x1234_5678;
        let mut samples: Vec<f32> = (0..10_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as f32 / u32::MAX as f32 * 2.0 - 1.0
            })
            .collect();
        samples.extend([-1.0, 1.0, 0.0]);
        let back = decode_pcm(&encode_pcm(&samples));
        assert_eq!(back.len(), samples.len());
        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() <= 1.0 / 32767.0, "{a} came back as {b}");
        }
        // An odd trailing byte is ignored.
        assert_eq!(decode_pcm(&[0x00, 0x40, 0x7f]), [0.5]);
    }

    #[test]
    fn pcm_clamps_out_of_range() {
        let bytes = encode_pcm(&[2.0, -2.0, f32::NAN]);
        assert_eq!(&bytes[4..], [0, 0], "NaN encodes as 0");
        assert_eq!(decode_pcm(&bytes), [32767.0 / 32768.0, -1.0, 0.0]);
    }

    #[test]
    fn packetizer_emits_whole_packets_across_odd_blocks() {
        let data: Vec<f32> = (0..960).map(|i| i as f32 / 960.0).collect();
        let mut packets: Vec<(u32, Vec<f32>)> = Vec::new();
        let mut p = Packetizer::new(AudioStream::MacSound);
        for block in [&data[..100], &data[100..433], &data[433..480]] {
            p.push(block, |seq, pkt| packets.push((seq, pkt.to_vec())));
        }
        assert_eq!(packets, [(0, data[..480].to_vec())]);
        p.push(&data[480..], |seq, pkt| packets.push((seq, pkt.to_vec())));
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[1], (1, data[480..].to_vec()));
    }
}
