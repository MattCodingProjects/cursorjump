//! Audio sharing core: the wire's PCM packets, and a receiver that turns
//! packets arriving with network jitter (and from a sender whose clock runs
//! a little fast or slow) into an even stream for the sound card. Pure
//! logic, no I/O.
use serde::{Deserialize, Serialize};

mod jitter;
pub mod pcm;
pub mod receiver;

pub use pcm::{Packetizer, decode_pcm, encode_pcm};
pub use receiver::{AudioReceiver, AudioStats};

/// Sample rate of both streams, in frames per second.
pub const RATE: u32 = 48_000;

/// One direction of audio.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioStream {
    /// The Mac's sound, played on the PC.
    MacSound,
    /// The PC's microphone, heard on the Mac.
    PcMic,
}

impl AudioStream {
    pub const fn channels(self) -> usize {
        match self {
            AudioStream::MacSound => 2,
            AudioStream::PcMic => 1,
        }
    }

    /// Frames in one packet: 5 ms of MacSound, 10 ms of PcMic.
    pub const fn packet_frames(self) -> usize {
        match self {
            AudioStream::MacSound => 240,
            AudioStream::PcMic => 480,
        }
    }

    /// Interleaved samples in one packet: 480 for both streams.
    pub const fn packet_samples(self) -> usize {
        self.channels() * self.packet_frames()
    }
}

/// Whether a PC audio device can be used.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Ok,
    Missing,
    Blocked,
}

/// The state of the PC's default output and microphone.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioStatus {
    pub output: DeviceState,
    pub mic: DeviceState,
}

/// Which streams the Mac wants right now.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioWant {
    pub sound: bool,
    pub mic: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_streams_send_480_samples_a_packet() {
        assert_eq!(AudioStream::MacSound.channels(), 2);
        assert_eq!(AudioStream::MacSound.packet_frames(), 240);
        assert_eq!(AudioStream::PcMic.channels(), 1);
        assert_eq!(AudioStream::PcMic.packet_frames(), 480);
        assert_eq!(AudioStream::MacSound.packet_samples(), 480);
        assert_eq!(AudioStream::PcMic.packet_samples(), 480);
    }
}
