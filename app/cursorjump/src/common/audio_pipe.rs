//! Pipes between the network and the audio devices. `AudioTx` is fed by the
//! datagram reader, `AudioRx` is pulled by a sound card's real-time callback,
//! and `AudioSender` turns captured samples into audio datagrams.
//!
//! `AudioRx::pull` runs on a real-time thread: it never allocates, frees or
//! waits for a lock (packets cross by value in fixed arrays).

use std::sync::{Arc, Mutex};

use cj_core::audio::{AudioReceiver, AudioStats, AudioStream, Packetizer, decode_pcm, encode_pcm};
use cj_core::proto::Datagram;
use cj_net::session::Session;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use tracing::{debug, warn};

/// Samples in one packet, for both streams.
const PACKET: usize = 480;
/// Packets the queue holds before new ones are dropped.
const QUEUE: usize = 64;

type Packet = (u32, [f32; PACKET]);

/// A queue from the network to the sound card, for one stream.
pub fn audio_pipe(stream: AudioStream) -> (AudioTx, AudioRx) {
    debug_assert_eq!(stream.packet_samples(), PACKET);
    let (tx, rx) = bounded(QUEUE);
    let stats = Arc::new(Mutex::new(AudioStats::default()));
    // On macOS std's Mutex boxes its pthread mutex on first use; lock it once
    // here so `publish`'s try_lock never allocates on the real-time thread.
    drop(stats.lock());
    (
        AudioTx { tx },
        AudioRx {
            rx,
            receiver: AudioReceiver::new(stream),
            stats: AudioStatsHandle(stats),
        },
    )
}

/// The network end: decodes packets and queues them.
#[derive(Clone)]
pub struct AudioTx {
    tx: Sender<Packet>,
}

impl AudioTx {
    /// Queues packet `seq`; dropped if it is malformed or 64 are queued.
    pub fn push(&self, seq: u32, pcm: &[u8]) {
        let samples = decode_pcm(pcm);
        let Ok(samples) = <[f32; PACKET]>::try_from(samples.as_slice()) else {
            debug!("audio packet {seq} has {} samples; dropped", samples.len());
            return;
        };
        match self.tx.try_send((seq, samples)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => debug!("audio queue full; packet {seq} dropped"),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// The sound card's end. Pull from the real-time callback only.
pub struct AudioRx {
    rx: Receiver<Packet>,
    receiver: AudioReceiver,
    stats: AudioStatsHandle,
}

impl AudioRx {
    /// Fills `out` with audio or silence. Never allocates or blocks.
    pub fn pull(&mut self, out: &mut [f32]) {
        while let Ok((seq, samples)) = self.rx.try_recv() {
            self.receiver.push(seq, &samples);
        }
        self.receiver.pull(out);
        self.publish();
    }

    /// Forgets everything queued and buffered.
    pub fn reset(&mut self) {
        while self.rx.try_recv().is_ok() {}
        self.receiver.reset();
        self.publish();
    }

    pub fn stats(&self) -> AudioStatsHandle {
        self.stats.clone()
    }

    /// A missed publish is fine; the next pull publishes again.
    fn publish(&self) {
        if let Ok(mut s) = self.stats.0.try_lock() {
            *s = self.receiver.stats();
        }
    }
}

/// Reads the receiver's counters from any thread.
#[derive(Clone)]
pub struct AudioStatsHandle(Arc<Mutex<AudioStats>>);

impl AudioStatsHandle {
    /// The counters as of a recent pull.
    pub fn snapshot(&self) -> AudioStats {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Sends captured audio on the current session, if any; swapped on reconnect.
#[derive(Clone, Default)]
pub struct AudioOut(Arc<Mutex<Option<Session>>>);

impl AudioOut {
    pub fn set(&self, s: Option<Session>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    pub fn sender(&self, stream: AudioStream) -> AudioSender {
        AudioSender {
            out: self.clone(),
            stream,
            packetizer: Packetizer::new(stream),
        }
    }

    fn send(&self, d: &Datagram) {
        let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = guard.as_ref()
            && let Err(e) = s.send_audio(d)
        {
            warn!("sending audio: {e:#}");
        }
    }
}

/// Packetizes one capture thread's samples and sends them. Silence is sent
/// too; the sequence numbers continue across session swaps.
pub struct AudioSender {
    out: AudioOut,
    stream: AudioStream,
    packetizer: Packetizer,
}

impl AudioSender {
    pub fn push(&mut self, interleaved: &[f32]) {
        let (out, stream) = (&self.out, self.stream);
        self.packetizer.push(interleaved, |seq, samples| {
            out.send(&Datagram::Audio {
                stream,
                seq,
                pcm: encode_pcm(samples),
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_net::session::{Listener, connect};
    use cj_net::{Identity, Trust};
    use std::time::Duration;
    use tokio::time::timeout;

    const T: Duration = Duration::from_secs(10);

    async fn session_pair() -> (Listener, Session, Session) {
        let ident = || {
            let dir = tempfile::tempdir().unwrap();
            Identity::load_or_create(dir.path()).unwrap()
        };
        let (a, b) = (ident(), ident());
        let any_port = (std::net::Ipv4Addr::LOCALHOST, 0).into();
        let listener = Listener::bind(&a, Trust::PairingMode, any_port, "A")
            .await
            .unwrap();
        let (sa, sb) = timeout(T, async {
            tokio::join!(
                listener.accept(),
                connect(&b, Trust::PairingMode, listener.local_addr(), "B")
            )
        })
        .await
        .unwrap();
        (listener, sa.unwrap(), sb.unwrap())
    }

    fn packet_bytes(v: f32) -> Vec<u8> {
        encode_pcm(&[v; PACKET])
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[tokio::test]
    async fn pipe_carries_a_sine_end_to_end() {
        let (_l, sa, sb) = session_pair().await;
        let (tx, mut rx) = audio_pipe(AudioStream::MacSound);
        let reader = tokio::spawn(async move {
            while let Ok(d) = sb.recv_datagram().await {
                if let Datagram::Audio { seq, pcm, .. } = d {
                    tx.push(seq, &pcm);
                }
            }
        });
        let out = AudioOut::default();
        out.set(Some(sa));
        let mut sender = out.sender(AudioStream::MacSound);

        // 10 ms steps: 480 frames of a 1 kHz stereo sine at 0.5.
        let mut n = 0u64;
        let mut sent = Vec::new();
        let mut got = Vec::new();
        for step in 0..400 {
            let block: Vec<f32> = (0..480)
                .flat_map(|_| {
                    let v = (n as f64 * 2.0 * std::f64::consts::PI * 1000.0 / 48000.0).sin() * 0.5;
                    n += 1;
                    [v as f32; 2]
                })
                .collect();
            sender.push(&block);
            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut o = vec![0.0f32; 960];
            rx.pull(&mut o);
            if step >= 200 {
                sent.extend_from_slice(&block);
                got.extend_from_slice(&o);
            }
        }
        reader.abort();
        let db = 20.0 * (rms(&got) / rms(&sent)).log10();
        assert!(db.abs() < 0.1, "level off by {db} dB");
        assert!(rms(&got) > 0.3, "silent output");
    }

    #[test]
    fn no_session_drops_quietly() {
        let out = AudioOut::default();
        let mut s = out.sender(AudioStream::PcMic);
        s.push(&[0.1; 2000]);
        out.set(None);
        s.push(&[0.0; 480]);
    }

    #[test]
    fn reset_forgets_queued_audio() {
        let (tx, mut rx) = audio_pipe(AudioStream::MacSound);
        for seq in 0..20 {
            tx.push(seq, &packet_bytes(0.5));
        }
        rx.reset();
        let mut o = vec![1.0f32; 960];
        rx.pull(&mut o);
        assert!(o.iter().all(|&v| v == 0.0), "audio survived the reset");
        assert_eq!(rx.stats().snapshot().received, 0);
    }

    #[test]
    fn malformed_and_excess_packets_are_dropped() {
        let (tx, mut rx) = audio_pipe(AudioStream::MacSound);
        tx.push(0, &[0u8; 10]);
        for seq in 0..100 {
            tx.push(seq, &packet_bytes(0.1));
        }
        rx.pull(&mut [0.0; 10]);
        assert_eq!(rx.stats().snapshot().received, 64);
    }

    #[test]
    fn stats_snapshot_follows_pulls() {
        let (tx, mut rx) = audio_pipe(AudioStream::MacSound);
        let h = rx.stats();
        assert_eq!(h.snapshot().received, 0);
        for seq in 0..5 {
            tx.push(seq, &packet_bytes(0.1));
        }
        rx.pull(&mut [0.0; 480]);
        assert_eq!(h.snapshot().received, 5);
    }
}
