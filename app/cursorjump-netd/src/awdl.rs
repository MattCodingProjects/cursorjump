//! Who holds AWDL down, and what that means for the interface: down while
//! at least one connection holds it, back up when the last one lets go or
//! goes away. Only a state this daemon caused is undone.

use std::collections::BTreeSet;

use crate::iface::Iface;
use crate::protocol::Command;

/// A connection, numbered by the server.
pub type ConnId = u64;

pub struct Awdl<I> {
    iface: I,
    holders: BTreeSet<ConnId>,
    /// This daemon took the interface down and has not brought it back up.
    downed: bool,
}

impl<I: Iface> Awdl<I> {
    pub fn new(iface: I) -> Awdl<I> {
        Awdl {
            iface,
            holders: BTreeSet::new(),
            downed: false,
        }
    }

    pub fn is_held(&self) -> bool {
        !self.holders.is_empty()
    }

    /// Runs `conn`'s command; Err is the reason for an `err` reply.
    pub fn command(&mut self, conn: ConnId, cmd: Command) -> Result<(), String> {
        match cmd {
            Command::AwdlDown => {
                if !self.is_held() {
                    self.take_down().map_err(|e| e.to_string())?;
                    log!("AWDL held down");
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
            log!("could not bring AWDL back up: {e}; retrying");
        }
    }

    /// Run every second: macOS brings awdl0 back up on its own, so take it
    /// down again while held. Also retries a failed restore.
    pub fn recheck(&mut self) {
        let result = if self.is_held() {
            match self.iface.is_up() {
                Ok(true) => {
                    log!("macOS brought AWDL back up; taking it down again");
                    self.take_down()
                }
                Ok(false) => Ok(()),
                Err(e) => Err(e),
            }
        } else {
            self.restore()
        };
        if let Err(e) = result {
            log!("AWDL check failed: {e}");
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
        log!("AWDL released");
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
