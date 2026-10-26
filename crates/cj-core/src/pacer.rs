//! Send pacing for file data: a token bucket whose rate backs off when the
//! round-trip time rises above its recent minimum, so a big copy never makes
//! the cursor feel different.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Rate a new pacer starts at, in bytes/s.
pub const START_RATE: u64 = 20_000_000;
/// Slowest the pacer ever goes, in bytes/s.
pub const FLOOR_RATE: u64 = 2_000_000;
/// Fastest the pacer ever goes, in bytes/s.
pub const CEIL_RATE: u64 = 1_000_000_000;
/// How far an RTT sample may sit above the baseline before it counts as a spike.
pub const RTT_HEADROOM: Duration = Duration::from_millis(4);

const BASELINE_WINDOW: Duration = Duration::from_secs(10);
const CUT_EVERY: Duration = Duration::from_millis(200);
const RAMP_STEP: u64 = 4_000_000;
const RAMP_PERIOD: Duration = Duration::from_millis(100);
const BUCKET_DEPTH: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Pacer {
    /// Bytes/s, always within `[FLOOR_RATE, CEIL_RATE]`.
    rate: u64,
    /// RTT samples from the last 10 s; their minimum is the baseline.
    samples: VecDeque<(Instant, Duration)>,
    last_cut: Option<Instant>,
    /// Start of the current quiet stretch. Every high sample restarts it, cut
    /// or not, so time spent congested never counts toward a ramp.
    ramp_from: Instant,
    /// When the bucket will be full again if nothing more is reserved. The
    /// bucket's contents are kept as time, so a reservation of `b` bytes
    /// pushes this `b / rate` later, and the 100 ms depth is always 100 ms of
    /// the current rate. Reservations already handed out keep the rate they
    /// were made at; a cut slows only what is asked for afterwards.
    full_at: Instant,
}

impl Pacer {
    /// A pacer at `START_RATE` with a full bucket.
    pub fn new(now: Instant) -> Pacer {
        Pacer {
            rate: START_RATE,
            samples: VecDeque::new(),
            last_cut: None,
            ramp_from: now,
            full_at: now,
        }
    }

    /// Record an RTT sample. Above baseline + `RTT_HEADROOM` cuts the rate by
    /// 30%, at most once per 200 ms; otherwise the rate ramps by 4 MB/s per
    /// 100 ms of quiet since the last increase.
    pub fn on_rtt(&mut self, rtt: Duration, now: Instant) {
        self.samples.push_back((now, rtt));
        while let Some(&(t, _)) = self.samples.front() {
            if now.saturating_duration_since(t) > BASELINE_WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        let baseline = self.samples.iter().map(|&(_, d)| d).min().unwrap_or(rtt);
        if rtt > baseline + RTT_HEADROOM {
            let due = self
                .last_cut
                .is_none_or(|t| now.saturating_duration_since(t) >= CUT_EVERY);
            if due {
                self.rate = (self.rate * 7 / 10).max(FLOOR_RATE);
                self.last_cut = Some(now);
            }
            self.ramp_from = now;
        } else {
            let quiet = now.saturating_duration_since(self.ramp_from);
            let step = u128::from(RAMP_STEP) * quiet.as_nanos() / RAMP_PERIOD.as_nanos();
            self.rate = (u128::from(self.rate) + step).min(u128::from(CEIL_RATE)) as u64;
            self.ramp_from = now;
        }
    }

    /// Reserve `bytes` and return how long to wait before sending them:
    /// `Duration::ZERO` means send now. The next call accounts for this
    /// reservation, whether or not the caller has waited yet.
    pub fn delay_for(&mut self, bytes: usize, now: Instant) -> Duration {
        let cost = u128::from(bytes as u64) * 1_000_000_000 / u128::from(self.rate);
        self.full_at = self.full_at.max(now) + Duration::from_nanos(cost as u64);
        self.full_at
            .saturating_duration_since(now)
            .saturating_sub(BUCKET_DEPTH)
    }

    /// The current rate in bytes/s, for logs.
    pub fn rate(&self) -> u64 {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn starts_at_start_rate() {
        let p = Pacer::new(Instant::now());
        assert_eq!(p.rate(), START_RATE);
    }

    #[test]
    fn rtt_spike_cuts_rate_once_per_200ms() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        p.on_rtt(ms(2), t0);
        // 6 ms is exactly baseline + headroom, which is not a spike.
        p.on_rtt(ms(6), t0);
        assert_eq!(p.rate(), START_RATE);
        p.on_rtt(ms(10), t0 + ms(10));
        assert_eq!(p.rate(), 14_000_000);
        p.on_rtt(ms(10), t0 + ms(60));
        assert_eq!(p.rate(), 14_000_000);
        p.on_rtt(ms(10), t0 + ms(210));
        assert_eq!(p.rate(), 9_800_000);
    }

    #[test]
    fn quiet_rtt_ramps_up_and_clamps() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        p.on_rtt(ms(2), t0);
        assert_eq!(p.rate(), START_RATE);
        p.on_rtt(ms(2), t0 + ms(100));
        assert_eq!(p.rate(), 24_000_000);
        // The ramp is proportional to the time since the last increase.
        p.on_rtt(ms(3), t0 + ms(150));
        assert_eq!(p.rate(), 26_000_000);
        p.on_rtt(ms(2), t0 + Duration::from_secs(60));
        assert_eq!(p.rate(), CEIL_RATE);
        p.on_rtt(ms(2), t0 + Duration::from_secs(61));
        assert_eq!(p.rate(), CEIL_RATE);
    }

    #[test]
    fn a_spike_restarts_the_ramp_clock() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        p.on_rtt(ms(2), t0);
        p.on_rtt(ms(20), t0 + Duration::from_secs(1));
        assert_eq!(p.rate(), 14_000_000);
        // The quiet second before the cut doesn't count toward the ramp.
        p.on_rtt(ms(2), t0 + ms(1100));
        assert_eq!(p.rate(), 18_000_000);
        // Nor does time spent in a spike the 200 ms rule didn't act on.
        p.on_rtt(ms(20), t0 + ms(1150));
        assert_eq!(p.rate(), 18_000_000);
        p.on_rtt(ms(2), t0 + ms(1250));
        assert_eq!(p.rate(), 22_000_000);
    }

    #[test]
    fn never_below_floor() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        p.on_rtt(ms(2), t0);
        for i in 1..=50 {
            p.on_rtt(ms(50), t0 + ms(200 * i));
            assert!(p.rate() >= FLOOR_RATE);
        }
        assert_eq!(p.rate(), FLOOR_RATE);
    }

    #[test]
    fn delay_for_spreads_bytes_at_rate() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        // The bucket starts full: 100 ms of 20 MB/s goes out at once.
        assert_eq!(p.delay_for(2_000_000, t0), Duration::ZERO);
        let mut now = t0;
        let mut total = Duration::ZERO;
        for _ in 0..10 {
            let d = p.delay_for(256 * 1024, now);
            total += d;
            now += d;
        }
        // 10 × 256 KiB at 20 MB/s is 131.07 ms.
        assert!(
            total >= ms(126) && total <= ms(136),
            "total delay {total:?}"
        );
    }

    #[test]
    fn delay_for_reserves_bytes_not_yet_sent() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        assert_eq!(p.delay_for(2_000_000, t0), Duration::ZERO);
        // Asking again before waiting stacks behind the earlier reservation.
        assert_eq!(p.delay_for(200_000, t0), ms(10));
        assert_eq!(p.delay_for(200_000, t0), ms(20));
        // Waiting out the debt and then some refills the bucket.
        assert_eq!(p.delay_for(200_000, t0 + ms(40)), Duration::ZERO);
    }

    #[test]
    fn idle_bucket_holds_at_most_100ms() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        let later = t0 + Duration::from_secs(10);
        assert_eq!(p.delay_for(2_000_000, later), Duration::ZERO);
        assert_eq!(p.delay_for(200_000, later), ms(10));
    }

    #[test]
    fn baseline_forgets_after_10s() {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        p.on_rtt(ms(2), t0);
        p.on_rtt(ms(20), t0 + Duration::from_secs(5));
        assert_eq!(p.rate(), 14_000_000);
        // Still within 10 s of the 2 ms sample (inclusive): 20 ms is a spike.
        p.on_rtt(ms(20), t0 + ms(9_000));
        assert_eq!(p.rate(), 9_800_000);
        p.on_rtt(ms(20), t0 + ms(10_000));
        assert_eq!(p.rate(), 6_860_000);
        // 2 ms has aged out, so 20 ms is the baseline and the rate ramps.
        p.on_rtt(ms(20), t0 + ms(10_100));
        assert_eq!(p.rate(), 10_860_000);
    }
}
