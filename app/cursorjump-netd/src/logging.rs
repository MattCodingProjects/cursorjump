//! The daemon's log: one timestamped line per event, never a panic, and
//! rate limits for anything that could repeat every second.
//!
//! Lines are appended to `/var/log/cursorjump-netd.log`, opened afresh for
//! each line so that newsyslog can rotate it under a running daemon; without
//! a log file (tests, a manual run) they go to stderr.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How often a repeating event is logged at most.
pub const LOG_EVERY: Duration = Duration::from_secs(600);

/// Where lines go; unset: stderr.
pub static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

/// Writes one line; a failure to write is ignored (a log must never take
/// the daemon down, and with it the AWDL restore).
pub fn write_line(message: std::fmt::Arguments) {
    emit(&format!("{} cursorjump-netd: {message}\n", timestamp()));
}

#[cfg(not(test))]
fn emit(line: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(path) = LOG_FILE.get() {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o644)
            .open(path);
        if let Ok(mut f) = file {
            let _ = f.write_all(line.as_bytes());
            return;
        }
    }
    let _ = std::io::stderr().write_all(line.as_bytes());
}

/// The test harness captures only the print macros.
#[cfg(test)]
fn emit(line: &str) {
    eprint!("{line}");
}

/// Lets an event through at most once per `every`, counting the rest.
#[derive(Debug)]
pub struct RateLimit {
    every: Duration,
    last: Option<Instant>,
    suppressed: u64,
}

impl RateLimit {
    pub fn new(every: Duration) -> RateLimit {
        RateLimit {
            every,
            last: None,
            suppressed: 0,
        }
    }

    /// An event at `now`: Some(number of events suppressed since the last
    /// one let through) if this one is to be logged, else None.
    pub fn hit(&mut self, now: Instant) -> Option<u64> {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < self.every)
        {
            self.suppressed += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }
}

/// " (N more since the last such line)" when N > 0.
pub fn more(suppressed: u64) -> String {
    if suppressed == 0 {
        String::new()
    } else {
        format!(" ({suppressed} more since the last such line)")
    }
}

/// Local time as `YYYY-MM-DD HH:MM:SS`.
pub fn timestamp() -> String {
    // SAFETY: time(NULL) and localtime_r with a live tm are always safe.
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn the_first_event_passes_then_one_per_period() {
        let t0 = Instant::now();
        let mut r = RateLimit::new(600 * S);
        assert_eq!(r.hit(t0), Some(0));
        assert_eq!(r.hit(t0 + S), None);
        assert_eq!(r.hit(t0 + 2 * S), None);
        assert_eq!(r.hit(t0 + 599 * S), None);
        assert_eq!(r.hit(t0 + 600 * S), Some(3), "three were suppressed");
        assert_eq!(r.hit(t0 + 601 * S), None);
        assert_eq!(r.hit(t0 + 1300 * S), Some(1));
    }

    #[test]
    fn a_quiet_period_lets_the_next_event_through() {
        let t0 = Instant::now();
        let mut r = RateLimit::new(600 * S);
        assert_eq!(r.hit(t0), Some(0));
        assert_eq!(r.hit(t0 + 5000 * S), Some(0));
    }

    #[test]
    fn suppressed_counts_read_naturally() {
        assert_eq!(more(0), "");
        assert_eq!(more(1), " (1 more since the last such line)");
        assert_eq!(more(42), " (42 more since the last such line)");
    }

    #[test]
    fn timestamps_look_like_dates() {
        let t = timestamp();
        assert_eq!(t.len(), 19, "{t}");
        assert!(t.starts_with("20"), "{t}");
    }
}
