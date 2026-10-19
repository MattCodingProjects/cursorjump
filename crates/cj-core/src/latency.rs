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

    /// Number of samples in the window.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Median RTT in ms (lower middle value for an even count).
    pub fn median_ms(&self) -> Option<f32> {
        self.percentile_ms(50.0)
    }

    /// The `p`th percentile RTT in ms, `p` in 0..=100, by nearest rank:
    /// the smallest sample with at least `p`% of the samples at or below it.
    pub fn percentile_ms(&self, p: f32) -> Option<f32> {
        if self.samples.is_empty() {
            return None;
        }
        let mut v: Vec<Duration> = self.samples.iter().map(|&(_, d)| d).collect();
        v.sort();
        let n = v.len();
        let rank = (f64::from(p.clamp(0.0, 100.0)) * n as f64 / 100.0).ceil() as usize;
        Some(as_ms(v[rank.clamp(1, n) - 1]))
    }

    /// Largest RTT in the window, in ms.
    pub fn max_ms(&self) -> Option<f32> {
        self.samples.iter().map(|&(_, d)| d).max().map(as_ms)
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

    /// One line for the log, in a fixed wording that tools parse.
    pub fn log_line(&self) -> String {
        let (Some(median), Some(p95), Some(p99), Some(max)) = (
            self.median_ms(),
            self.percentile_ms(95.0),
            self.percentile_ms(99.0),
            self.max_ms(),
        ) else {
            return "latency over 60 s: no samples".to_string();
        };
        format!(
            "latency over 60 s: median {median:.1} ms, p95 {p95:.1} ms, p99 {p99:.1} ms, \
             max {max:.1} ms, spikes>40ms {}, samples {}",
            self.spikes(),
            self.len()
        )
    }
}

/// `d` in ms; exact for whole milliseconds.
fn as_ms(d: Duration) -> f32 {
    (d.as_nanos() as f64 / 1e6) as f32
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

    fn us(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    /// 100 samples of 1.0..=100.0 ms, recorded in shuffled order.
    fn hundred() -> LatencyStats {
        let base = Instant::now();
        let mut s = LatencyStats::new();
        for i in 0..100u64 {
            let r = (i * 37) % 100 + 1;
            s.record(base + Duration::from_millis(i * 100), ms(r));
        }
        s
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let s = hundred();
        assert_eq!(s.len(), 100);
        assert!(!s.is_empty());
        assert_eq!(s.percentile_ms(0.0), Some(1.0));
        assert_eq!(s.percentile_ms(50.0), Some(50.0));
        assert_eq!(s.percentile_ms(95.0), Some(95.0));
        assert_eq!(s.percentile_ms(99.0), Some(99.0));
        assert_eq!(s.percentile_ms(99.5), Some(100.0));
        assert_eq!(s.percentile_ms(100.0), Some(100.0));
        assert_eq!(s.max_ms(), Some(100.0));
        assert_eq!(s.percentile_ms(50.0), s.median_ms());
    }

    #[test]
    fn percentiles_of_a_few_samples() {
        let base = Instant::now();
        let mut s = LatencyStats::new();
        for (i, r) in [30, 10, 20, 40].into_iter().enumerate() {
            s.record(base + Duration::from_secs(i as u64), ms(r));
        }
        // Ranks: ceil(p/100 * 4), at least 1.
        assert_eq!(s.percentile_ms(0.0), Some(10.0));
        assert_eq!(s.percentile_ms(25.0), Some(10.0));
        assert_eq!(s.percentile_ms(26.0), Some(20.0));
        assert_eq!(s.percentile_ms(50.0), Some(20.0));
        assert_eq!(s.percentile_ms(75.0), Some(30.0));
        assert_eq!(s.percentile_ms(76.0), Some(40.0));
        assert_eq!(s.percentile_ms(100.0), Some(40.0));
        assert_eq!(s.max_ms(), Some(40.0));
    }

    #[test]
    fn empty_stats_have_no_percentiles() {
        let s = LatencyStats::new();
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert_eq!(s.percentile_ms(50.0), None);
        assert_eq!(s.max_ms(), None);
        assert_eq!(s.log_line(), "latency over 60 s: no samples");
    }

    #[test]
    fn the_log_line_has_one_decimal() {
        let base = Instant::now();
        let mut s = LatencyStats::new();
        let rtts = [4_200, 4_160, 3_900, 7_940, 31_000, 52_300, 41_000, 4_260];
        for (i, r) in rtts.into_iter().enumerate() {
            s.record(base + Duration::from_millis(i as u64 * 100), us(r));
        }
        // Sorted: 3.9 4.16 4.2 4.26 7.94 31.0 41.0 52.3; median rank 4,
        // p95 and p99 rank 8.
        assert_eq!(
            s.log_line(),
            "latency over 60 s: median 4.3 ms, p95 52.3 ms, p99 52.3 ms, max 52.3 ms, \
             spikes>40ms 2, samples 8"
        );
        assert_eq!(
            hundred().log_line(),
            "latency over 60 s: median 50.0 ms, p95 95.0 ms, p99 99.0 ms, max 100.0 ms, \
             spikes>40ms 60, samples 100"
        );
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
