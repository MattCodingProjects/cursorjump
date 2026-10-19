//! The settings window process (`cursorjump --settings`) opened from the
//! tray: at most one at a time, and closed when the app quits.

use std::io;
use std::process::Child;
use std::time::{Duration, Instant};

use tracing::{info, warn};

/// After killing the window's process, wait at most this long for it to exit.
const CLOSE_WAIT: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct SettingsWindow {
    child: Option<Child>,
}

impl SettingsWindow {
    /// True while the process this app opened is still running.
    pub fn is_open(&mut self) -> bool {
        let running = self
            .child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if !running {
            // Exited (or unknown): drop the handle, which reaps it.
            self.child = None;
        }
        running
    }

    /// Starts the window with `spawn` unless one is already open, in which
    /// case `bring_to_front` gets that one's process id. True if `spawn`
    /// was called.
    pub fn open(
        &mut self,
        spawn: impl FnOnce() -> io::Result<Child>,
        bring_to_front: impl FnOnce(u32),
    ) -> bool {
        if self.is_open() {
            info!("settings window is already open");
            if let Some(c) = &self.child {
                bring_to_front(c.id());
            }
            return false;
        }
        match spawn() {
            Ok(child) => self.child = Some(child),
            Err(e) => warn!("opening settings: {e}"),
        }
        true
    }

    /// Ends the window's process, waiting at most `CLOSE_WAIT`. Its edits are
    /// already saved: the window writes each change as it is made.
    pub fn close(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        if let Err(e) = child.kill() {
            warn!("closing the settings window: {e}");
            return;
        }
        let deadline = Instant::now() + CLOSE_WAIT;
        while matches!(child.try_wait(), Ok(None)) {
            if Instant::now() >= deadline {
                warn!("settings window did not exit in time");
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        info!("settings window closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// A real process that runs until killed (about 30 s otherwise).
    fn long_running() -> io::Result<Child> {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        cmd.stdout(Stdio::null()).spawn()
    }

    /// A real process that exits at once.
    fn exits_at_once() -> io::Result<Child> {
        #[cfg(windows)]
        return Command::new("cmd").args(["/C", "exit 0"]).spawn();
        #[cfg(not(windows))]
        return Command::new("true").spawn();
    }

    /// True if no process with this id exists any more.
    fn process_gone(pid: u32) -> bool {
        #[cfg(windows)]
        {
            let out = Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output()
                .unwrap();
            !String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
        }
        #[cfg(not(windows))]
        {
            let status = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .unwrap();
            !status.success()
        }
    }

    #[test]
    fn second_open_while_running_does_not_spawn() {
        let mut w = SettingsWindow::default();
        assert!(w.open(long_running, |_| {}));
        assert!(w.is_open());
        let pid = w.child.as_ref().unwrap().id();
        let mut raised = None;
        assert!(!w.open(
            || panic!("must not spawn a second window"),
            |p| raised = Some(p)
        ));
        assert_eq!(raised, Some(pid), "the open window is brought to the front");
        w.close();
        assert!(!w.is_open());
    }

    #[test]
    fn close_kills_and_reaps_within_the_bound() {
        let mut w = SettingsWindow::default();
        w.open(long_running, |_| {});
        let pid = w.child.as_ref().unwrap().id();
        let started = Instant::now();
        w.close();
        assert!(started.elapsed() < CLOSE_WAIT + Duration::from_millis(200));
        assert!(w.child.is_none());
        assert!(process_gone(pid));
    }

    #[test]
    fn reopens_after_the_window_exited() {
        let mut w = SettingsWindow::default();
        w.open(exits_at_once, |_| {});
        w.child.as_mut().unwrap().wait().unwrap();
        assert!(!w.is_open());
        let mut spawned = false;
        assert!(w.open(
            || {
                spawned = true;
                long_running()
            },
            |_| {}
        ));
        assert!(spawned);
        w.close();
    }

    #[test]
    fn close_without_a_window_is_a_no_op() {
        let mut w = SettingsWindow::default();
        w.close();
        assert!(!w.is_open());
    }
}
