//! Rolling round-trip latency statistics for the tray readout and Wi-Fi hint.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
const SPIKE_THRESHOLD: Duration = Duration::from_millis(40);
const SPIKY_AT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiVerdict {
    Good,
    Spiky,
}

#[derive(Debug, Clone, Default)]
pub struct LatencyStats {
    samples: VecDeque<(Instant, Duration)>,
}

impl LatencyStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a sample and drop any older than 60 s relative to `now`.
    pub fn record(&mut self, now: Instant, rtt: Duration) {
        self.samples.push_back((now, rtt));
        while let Some(&(t, _)) = self.samples.front() {
            if now.saturating_duration_since(t) > WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Median RTT in ms (lower middle value for an even count).
    pub fn median_ms(&self) -> Option<f32> {
        if self.samples.is_empty() {
            return None;
        }
        let mut v: Vec<Duration> = self.samples.iter().map(|&(_, d)| d).collect();
        v.sort();
        Some(v[(v.len() - 1) / 2].as_secs_f32() * 1000.0)
    }

    /// Number of samples in the window with rtt > 40 ms.
    pub fn spikes(&self) -> usize {
        self.samples
            .iter()
            .filter(|&&(_, d)| d > SPIKE_THRESHOLD)
            .count()
    }

    pub fn verdict(&self) -> WifiVerdict {
        if self.spikes() >= SPIKY_AT {
            WifiVerdict::Spiky
        } else {
            WifiVerdict::Good
        }
    }

    pub fn summary(&self) -> String {
        match self.median_ms() {
            Some(m) => format!("Latency {} ms · {} spikes/min", m.round(), self.spikes()),
            None => "Latency – ms".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn median_and_spikes() {
        let base = Instant::now();
        let mut s = LatencyStats::new();
        assert_eq!(s.median_ms(), None);
        assert_eq!(s.summary(), "Latency – ms");
        for (i, r) in [10, 10, 50, 10].into_iter().enumerate() {
            s.record(base + Duration::from_secs(i as u64), ms(r));
        }
        assert_eq!(s.median_ms().map(|m| m.round()), Some(10.0));
        assert_eq!(s.spikes(), 1);
        assert_eq!(s.verdict(), WifiVerdict::Good);
        assert_eq!(s.summary(), "Latency 10 ms · 1 spikes/min");
        s.record(base + Duration::from_secs(5), ms(60));
        s.record(base + Duration::from_secs(6), ms(60));
        assert_eq!(s.spikes(), 3);
        assert_eq!(s.verdict(), WifiVerdict::Spiky);
    }

    #[test]
    fn old_samples_expire() {
        let base = Instant::now();
        let mut s = LatencyStats::new();
        s.record(base, ms(100));
        s.record(base + Duration::from_secs(61), ms(5));
        assert_eq!(s.spikes(), 0);
        assert_eq!(s.median_ms().map(|m| m.round()), Some(5.0));
    }
}
