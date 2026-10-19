//! Who holds AWDL down, and what that means for the interface: down while
//! at least one connection holds it, back up when the last one lets go or
//! goes away. Only a state this daemon caused is undone.

use std::collections::BTreeSet;
use std::time::Instant;

use crate::iface::Iface;
use crate::logging::{KindLimit, LOG_EVERY, RateLimit, more};
use crate::protocol::Command;

/// A connection, numbered by the server.
pub type ConnId = u64;

pub struct Awdl<I: Iface> {
    iface: I,
    holders: BTreeSet<ConnId>,
    /// This daemon took the interface down and has not brought it back up.
    downed: bool,
    /// Times macOS brought the interface back up during the current hold.
    redowns: u64,
    /// Re-downs after the first of a hold.
    redown_log: RateLimit,
    check_log: RateLimit,
    /// Lines a client can cause by holding and releasing (kinds below).
    hold_log: KindLimit<4>,
}

/// `Awdl::hold_log` kinds.
const HELD: usize = 0;
const RELEASED: usize = 1;
const RESTORE_FAILED: usize = 2;
const FIRST_REDOWN: usize = 3;

impl<I: Iface> Awdl<I> {
    pub fn new(iface: I) -> Awdl<I> {
        Awdl {
            iface,
            holders: BTreeSet::new(),
            downed: false,
            redowns: 0,
            redown_log: RateLimit::new(LOG_EVERY),
            check_log: RateLimit::new(LOG_EVERY),
            hold_log: KindLimit::new(LOG_EVERY),
        }
    }

    pub fn is_held(&self) -> bool {
        !self.holders.is_empty()
    }

    /// Whether `recheck` has work: while held, or while a restore failed.
    pub fn wants_ticks(&self) -> bool {
        self.is_held() || self.downed
    }

    /// Runs `conn`'s command; Err is the reason for an `err` reply.
    pub fn command(&mut self, conn: ConnId, cmd: Command) -> Result<(), String> {
        match cmd {
            Command::AwdlDown => {
                if !self.is_held() {
                    self.take_down().map_err(|e| e.to_string())?;
                    self.redowns = 0;
                    self.hold_line(HELD, format_args!("AWDL held down"));
                }
                self.holders.insert(conn);
                Ok(())
            }
            Command::AwdlUp => self.release(conn),
        }
    }

    /// `conn` closed (or crashed): its hold, if any, ends.
    pub fn closed(&mut self, conn: ConnId) {
        if let Err(e) = self.release(conn) {
            self.hold_line(
                RESTORE_FAILED,
                format_args!("could not bring AWDL back up: {e}; retrying"),
            );
        }
    }

    /// Run every second: macOS brings awdl0 back up on its own, so take it
    /// down again while held. Also retries a failed restore.
    pub fn recheck(&mut self) {
        let result = if self.is_held() {
            match self.iface.is_up() {
                Ok(true) => self.take_down().inspect(|()| self.redowned()),
                Ok(false) => Ok(()),
                Err(e) => Err(e),
            }
        } else {
            self.restore()
        };
        if let Err(e) = result
            && let Some(n) = self.check_log.hit(Instant::now())
        {
            log!("AWDL check failed: {e}{}", more(n));
        }
    }

    /// Logs the first re-down of a hold (as a hold line), then a count at
    /// most every LOG_EVERY.
    fn redowned(&mut self) {
        self.redowns += 1;
        if self.redowns == 1 {
            self.hold_line(
                FIRST_REDOWN,
                format_args!(
                    "macOS brought AWDL back up; taking it down again (repeats are counted, not logged)"
                ),
            );
        } else if let Some(n) = self.redown_log.hit(Instant::now()) {
            log!(
                "macOS brought AWDL back up {} more times; took it down each time",
                n + 1
            );
        }
    }

    /// A line a client's holds and releases cause: the first of each kind,
    /// then at most one per LOG_EVERY across kinds, with a count.
    fn hold_line(&mut self, kind: usize, line: std::fmt::Arguments) {
        if let Some(n) = self.hold_log.hit(kind, Instant::now()) {
            log!("{line}{}", more(n));
        }
    }

    /// At exit: brings the interface back up if this daemon took it down.
    pub fn shutdown(&mut self) {
        self.holders.clear();
        if let Err(e) = self.restore() {
            log!("could not bring AWDL back up: {e}");
        }
    }

    /// Ends `conn`'s hold; the last one ends brings the interface back up.
    fn release(&mut self, conn: ConnId) -> Result<(), String> {
        if !self.holders.remove(&conn) || self.is_held() {
            return Ok(());
        }
        match self.redowns {
            0 => self.hold_line(RELEASED, format_args!("AWDL released")),
            n => self.hold_line(
                RELEASED,
                format_args!("AWDL released; macOS brought it back up {n} times during the hold"),
            ),
        }
        self.restore().map_err(|e| e.to_string())
    }

    fn take_down(&mut self) -> std::io::Result<()> {
        if self.iface.is_up()? {
            self.iface.set_up(false)?;
            self.downed = true;
        }
        Ok(())
    }

    /// Brings the interface up if this daemon took it down.
    fn restore(&mut self) -> std::io::Result<()> {
        if self.downed {
            self.iface.set_up(true)?;
            self.downed = false;
        }
        Ok(())
    }
}

/// At start, before any client can hold it: brings `iface` up if it is
/// down (an earlier run may have died holding it). True if it was down.
pub fn bring_up_if_down<I: Iface>(iface: &mut I) -> std::io::Result<bool> {
    if iface.is_up()? {
        return Ok(false);
    }
    iface.set_up(true)?;
    Ok(true)
}

impl<I: Iface> Drop for Awdl<I> {
    /// Every way out restores, a panic unwinding out of the loop included.
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// A fake interface: its state, plus every change made to it.
    #[derive(Debug, Default)]
    pub struct FakeState {
        pub up: bool,
        pub sets: Vec<bool>,
        pub fail: bool,
    }

    #[derive(Clone, Default)]
    pub struct Fake(pub Arc<Mutex<FakeState>>);

    impl Fake {
        pub fn up() -> Fake {
            let f = Fake::default();
            f.0.lock().unwrap().up = true;
            f
        }
        pub fn is_up(&self) -> bool {
            self.0.lock().unwrap().up
        }
        /// The changes made since the last call.
        pub fn take_sets(&self) -> Vec<bool> {
            std::mem::take(&mut self.0.lock().unwrap().sets)
        }
        /// macOS (or the user) changes it behind the daemon's back.
        pub fn set_externally(&self, up: bool) {
            self.0.lock().unwrap().up = up;
        }
        pub fn set_failing(&self, fail: bool) {
            self.0.lock().unwrap().fail = fail;
        }
    }

    impl Iface for Fake {
        fn is_up(&mut self) -> io::Result<bool> {
            Ok(self.0.lock().unwrap().up)
        }
        fn set_up(&mut self, up: bool) -> io::Result<()> {
            let mut s = self.0.lock().unwrap();
            if s.fail {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            s.up = up;
            s.sets.push(up);
            Ok(())
        }
    }

    use Command::{AwdlDown, AwdlUp};

    #[test]
    fn one_holder_downs_and_restores() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        assert_eq!(a.command(1, AwdlDown), Ok(()));
        assert!(a.is_held());
        assert_eq!(fake.take_sets(), [false]);
        assert_eq!(a.command(1, AwdlUp), Ok(()));
        assert!(!a.is_held());
        assert_eq!(fake.take_sets(), [true]);
    }

    #[test]
    fn down_while_any_holder_remains() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        a.command(1, AwdlDown).unwrap();
        a.command(2, AwdlDown).unwrap();
        assert_eq!(fake.take_sets(), [false], "the second hold changes nothing");
        a.command(1, AwdlUp).unwrap();
        a.closed(1);
        assert!(!fake.is_up(), "connection 2 still holds it");
        a.closed(2);
        assert!(fake.is_up(), "the last holder went away");
        assert_eq!(fake.take_sets(), [true]);
    }

    #[test]
    fn repeated_commands_are_idempotent() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        a.command(1, AwdlDown).unwrap();
        a.command(1, AwdlDown).unwrap();
        a.command(1, AwdlUp).unwrap();
        assert!(fake.is_up(), "one awdl-up undoes any number of awdl-down");
        assert_eq!(a.command(1, AwdlUp), Ok(()), "releasing twice is fine");
        assert_eq!(
            a.command(7, AwdlUp),
            Ok(()),
            "so is releasing without a hold"
        );
        assert_eq!(fake.take_sets(), [false, true]);
    }

    #[test]
    fn a_crashed_holder_is_released() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        a.command(1, AwdlDown).unwrap();
        a.closed(1);
        assert!(fake.is_up());
        a.closed(3);
        assert_eq!(
            fake.take_sets(),
            [false, true],
            "closing a non-holder does nothing"
        );
    }

    #[test]
    fn a_flapping_client_logs_a_bounded_number_of_lines() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        crate::logging::take_lines();
        for _ in 0..100 {
            a.command(1, AwdlDown).unwrap();
            // macOS re-ups it during each hold.
            fake.set_externally(true);
            a.recheck();
            a.command(1, AwdlUp).unwrap();
        }
        let lines = crate::logging::take_lines();
        assert!(lines.len() <= 4, "{lines:#?}");
        for first in [
            "AWDL held down",
            "macOS brought AWDL back up",
            "AWDL released",
        ] {
            assert!(
                lines.iter().any(|l| l.contains(first)),
                "the first {first:?} shows: {lines:#?}"
            );
        }
    }

    #[test]
    fn a_failing_restore_logs_a_bounded_number_of_lines() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        crate::logging::take_lines();
        for conn in 0..100 {
            fake.set_failing(false);
            a.command(conn, AwdlDown).unwrap();
            fake.set_failing(true);
            a.closed(conn);
        }
        fake.set_failing(false);
        let lines = crate::logging::take_lines();
        assert!(lines.len() <= 4, "{lines:#?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("could not bring AWDL back up"))
        );
    }

    #[test]
    fn dropping_restores_a_held_interface() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        a.command(1, AwdlDown).unwrap();
        drop(a);
        assert!(fake.is_up());
    }

    #[test]
    fn a_panic_while_held_restores() {
        let fake = Fake::up();
        let f = fake.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut a = Awdl::new(f);
            a.command(1, AwdlDown).unwrap();
            panic!("a bug while held");
        }));
        assert!(result.is_err());
        assert!(fake.is_up(), "unwinding dropped the Awdl, which restored");
    }

    #[test]
    fn a_down_interface_is_brought_up_at_start() {
        let fake = Fake::default();
        assert!(bring_up_if_down(&mut fake.clone()).unwrap());
        assert!(fake.is_up());
        assert_eq!(fake.take_sets(), [true]);
    }

    #[test]
    fn an_up_interface_is_left_alone_at_start() {
        let fake = Fake::up();
        assert!(!bring_up_if_down(&mut fake.clone()).unwrap());
        assert_eq!(fake.take_sets(), Vec::<bool>::new());
    }

    #[test]
    fn ticks_are_wanted_only_while_held_or_restoring() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        assert!(!a.wants_ticks());
        a.command(1, AwdlDown).unwrap();
        assert!(a.wants_ticks());
        fake.set_failing(true);
        assert!(a.command(1, AwdlUp).is_err());
        assert!(a.wants_ticks(), "the failed restore is retried");
        fake.set_failing(false);
        a.recheck();
        assert!(fake.is_up());
        assert!(!a.wants_ticks());
    }

    /// Within one run, a state this daemon did not cause is not undone.
    /// (Across runs, `bring_up_if_down` at start covers an unclean exit.)
    #[test]
    fn an_interface_already_down_is_left_down() {
        let fake = Fake::default();
        let mut a = Awdl::new(fake.clone());
        a.command(1, AwdlDown).unwrap();
        a.closed(1);
        a.shutdown();
        assert!(!fake.is_up(), "this daemon did not take it down");
        assert_eq!(fake.take_sets(), Vec::<bool>::new());
    }

    #[test]
    fn recheck_downs_it_again_while_held() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        a.recheck();
        assert_eq!(
            fake.take_sets(),
            Vec::<bool>::new(),
            "not held: nothing to do"
        );
        a.command(1, AwdlDown).unwrap();
        fake.take_sets();
        a.recheck();
        assert_eq!(fake.take_sets(), Vec::<bool>::new(), "still down");
        fake.set_externally(true);
        a.recheck();
        assert_eq!(fake.take_sets(), [false]);
        a.closed(1);
        assert!(fake.is_up());
    }

    #[test]
    fn recheck_downs_it_when_it_was_down_at_the_first_hold() {
        let fake = Fake::default();
        let mut a = Awdl::new(fake.clone());
        a.command(1, AwdlDown).unwrap();
        fake.set_externally(true);
        a.recheck();
        assert!(!fake.is_up());
        a.closed(1);
        assert!(fake.is_up(), "this time the daemon took it down");
    }

    #[test]
    fn a_failed_down_is_reported_and_holds_nothing() {
        let fake = Fake::up();
        fake.set_failing(true);
        let mut a = Awdl::new(fake.clone());
        let err = a.command(1, AwdlDown).unwrap_err();
        assert!(err.contains("permission denied"), "{err}");
        assert!(!a.is_held());
        fake.set_failing(false);
        a.recheck();
        assert!(fake.is_up(), "no hold, so no retry");
    }

    #[test]
    fn a_failed_restore_is_retried() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        a.command(1, AwdlDown).unwrap();
        fake.set_failing(true);
        assert!(a.command(1, AwdlUp).is_err());
        assert!(!a.is_held(), "the hold ends even so");
        fake.set_failing(false);
        a.recheck();
        assert!(fake.is_up());
    }

    #[test]
    fn shutdown_restores_only_what_it_downed() {
        let fake = Fake::up();
        let mut a = Awdl::new(fake.clone());
        a.shutdown();
        assert_eq!(fake.take_sets(), Vec::<bool>::new());
        a.command(1, AwdlDown).unwrap();
        a.shutdown();
        assert!(fake.is_up());
        assert_eq!(fake.take_sets(), [false, true]);
    }
}
