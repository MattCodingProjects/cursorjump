//! "Reduce Wi-Fi hiccups": asks `cursorjump-netd`, the root helper from
//! `scripts/mac/install-netd.sh`, to hold AWDL (AirDrop's Wi-Fi interface)
//! down while the PC is connected. AWDL's channel hopping otherwise adds
//! latency spikes to the link.
//!
//! The hold is a connection to the helper: it lasts until `awdl-up` or
//! until the connection closes, so a crash of this app ends it too.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

/// The helper's socket (root:staff 0660).
pub const SOCKET: &str = "/var/run/cursorjump-netd.sock";
/// How long the helper has to answer.
const IO_TIMEOUT: Duration = Duration::from_millis(500);
/// While a hold is wanted: how often a missing one is retried (the helper
/// was installed or restarted) and a held one checked.
const RETRY: Duration = Duration::from_secs(5);
/// The longest reply read.
const MAX_REPLY: u64 = 256;

/// AWDL held down by the helper while this exists. Dropping it sends
/// `awdl-up` and closes the connection. Its calls block for up to
/// `IO_TIMEOUT`: keep them off the network loop.
#[derive(Debug)]
pub struct AwdlHold {
    stream: UnixStream,
}

impl AwdlHold {
    /// Connects to the helper at `socket` and asks for `awdl-down`.
    pub fn acquire(socket: &Path) -> Result<AwdlHold> {
        // A local connect does not wait: it fails at once when the
        // helper's backlog is full, and its replies have IO_TIMEOUT.
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting to {}", socket.display()))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        request(&stream, "awdl-down")?;
        Ok(AwdlHold { stream })
    }

    /// False once the helper closed the connection (it restarted, say).
    fn is_open(&self) -> bool {
        let mut byte = 0u8;
        // SAFETY: a live socket and a one-byte buffer; MSG_PEEK leaves the
        // data in place and MSG_DONTWAIT never blocks.
        let n = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        match n {
            0 => false,
            1.. => true,
            _ => io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock,
        }
    }
}

impl Drop for AwdlHold {
    fn drop(&mut self) {
        // Closing the connection releases the hold too; awdl-up just says so.
        if let Err(e) = request(&self.stream, "awdl-up") {
            debug!("releasing AWDL: {e:#}");
        }
    }
}

/// Sends `cmd` and waits for the helper's answer.
fn request(mut stream: &UnixStream, cmd: &str) -> Result<()> {
    stream
        .write_all(format!("{cmd}\n").as_bytes())
        .with_context(|| format!("sending {cmd} to the Wi-Fi helper"))?;
    let mut reply = String::new();
    BufReader::new(stream.take(MAX_REPLY))
        .read_line(&mut reply)
        .with_context(|| format!("waiting for the Wi-Fi helper to answer {cmd}"))?;
    match reply.strip_suffix('\n') {
        Some("ok") => Ok(()),
        Some(r) if r.starts_with("err ") => bail!("the Wi-Fi helper refused {cmd}: {}", &r[4..]),
        _ => bail!("unexpected answer from the Wi-Fi helper to {cmd}: {reply:?}"),
    }
}

/// True if `e` says the helper is not installed (its socket is missing).
pub fn helper_missing(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
    })
}

/// Holds AWDL down or lets it go as asked, on a thread of its own, so the
/// caller never waits for the helper. Dropping it releases any hold.
pub struct AwdlKeeper {
    tx: Option<Sender<bool>>,
    thread: Option<JoinHandle<()>>,
    wanted: bool,
    missing: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
}

impl AwdlKeeper {
    pub fn spawn() -> Result<AwdlKeeper> {
        AwdlKeeper::spawn_at(PathBuf::from(SOCKET), RETRY)
    }

    fn spawn_at(socket: PathBuf, retry: Duration) -> Result<AwdlKeeper> {
        let (tx, rx) = std::sync::mpsc::channel();
        let missing = Arc::new(AtomicBool::new(false));
        let held = Arc::new(AtomicBool::new(false));
        let (m, h) = (missing.clone(), held.clone());
        let thread = std::thread::Builder::new()
            .name("cj-netd".into())
            .spawn(move || keep(&socket, retry, &rx, &m, &h))
            .context("starting the Wi-Fi helper thread")?;
        Ok(AwdlKeeper {
            tx: Some(tx),
            thread: Some(thread),
            wanted: false,
            missing,
            held,
        })
    }

    /// Asks for AWDL to be held down (true) or released. Never blocks.
    pub fn hold(&mut self, on: bool) {
        if on == self.wanted {
            return;
        }
        self.wanted = on;
        if let Some(tx) = &self.tx {
            let _ = tx.send(on);
        }
    }

    /// The last attempt found the helper not installed.
    pub fn helper_missing(&self) -> bool {
        self.missing.load(Ordering::Relaxed)
    }

    /// The helper holds AWDL down (as of its last check).
    pub fn held(&self) -> bool {
        self.held.load(Ordering::Relaxed)
    }
}

impl Drop for AwdlKeeper {
    fn drop(&mut self) {
        // Closing the channel ends the thread, which drops the hold.
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The keeper's thread: holds AWDL down while the latest wish says so,
/// until the channel closes.
fn keep(
    socket: &Path,
    retry: Duration,
    wishes: &Receiver<bool>,
    missing: &AtomicBool,
    held: &AtomicBool,
) {
    let mut wanted = false;
    let mut hold: Option<AwdlHold> = None;
    let mut told_missing = false;
    // A failure is logged as a warning once until the next wish or success.
    let mut quiet = false;
    loop {
        match wishes.recv_timeout(retry) {
            Ok(w) => {
                // Only the latest wish matters.
                wanted = wishes.try_iter().last().unwrap_or(w);
                quiet = false;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if !wanted {
            if let Some(h) = hold.take() {
                held.store(false, Ordering::Relaxed);
                drop(h);
                info!("AWDL released");
            }
            continue;
        }
        if hold.as_ref().is_some_and(|h| !h.is_open()) {
            warn!("the Wi-Fi helper dropped the AWDL hold; asking again");
            held.store(false, Ordering::Relaxed);
            hold = None;
        }
        if hold.is_some() {
            continue;
        }
        match AwdlHold::acquire(socket) {
            Ok(h) => {
                info!("AWDL held down (Reduce Wi-Fi hiccups)");
                missing.store(false, Ordering::Relaxed);
                held.store(true, Ordering::Relaxed);
                quiet = false;
                hold = Some(h);
            }
            Err(e) if helper_missing(&e) => {
                missing.store(true, Ordering::Relaxed);
                if !told_missing {
                    info!("Reduce Wi-Fi hiccups needs the helper: run scripts/mac/install-netd.sh");
                    told_missing = true;
                }
            }
            Err(e) => {
                missing.store(false, Ordering::Relaxed);
                if quiet {
                    debug!("could not hold AWDL down: {e:#}");
                } else {
                    warn!("could not hold AWDL down: {e:#}");
                    quiet = true;
                }
            }
        }
    }
    if let Some(h) = hold.take() {
        drop(h);
        info!("AWDL released");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;
    use std::time::Instant;

    /// Upper bound for anything expected to happen.
    const T: Duration = Duration::from_secs(5);

    /// How a fake helper answers each line.
    #[derive(Clone, Copy)]
    enum Answer {
        Ok,
        Refuse,
        Silent,
    }

    /// A fake helper on a temporary socket: it records every line each
    /// connection sent, and "EOF" when one closes.
    struct FakeHelper {
        _dir: tempfile::TempDir,
        path: PathBuf,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl FakeHelper {
        fn start(answer: Answer) -> FakeHelper {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("netd.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let log = Arc::new(Mutex::new(Vec::new()));
            let l = log.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let l = l.clone();
                    std::thread::spawn(move || serve(stream, answer, &l));
                }
            });
            FakeHelper {
                _dir: dir,
                path,
                log,
            }
        }

        fn lines(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn wait_for(&self, want: &[&str]) {
            let deadline = Instant::now() + T;
            while self.lines() != want {
                assert!(
                    Instant::now() < deadline,
                    "helper saw {:?}, wanted {want:?}",
                    self.lines()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn serve(stream: UnixStream, answer: Answer, log: &Mutex<Vec<String>>) {
        let mut out = stream.try_clone().unwrap();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            log.lock().unwrap().push(line);
            match answer {
                Answer::Ok => out.write_all(b"ok\n").unwrap(),
                Answer::Refuse => out.write_all(b"err Operation not permitted\n").unwrap(),
                Answer::Silent => {}
            }
        }
        log.lock().unwrap().push("EOF".into());
    }

    /// Accepts the next connection on `listener` (within T), expects
    /// `awdl-down` and answers `ok`.
    fn accept_hold(listener: &UnixListener) -> UnixStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + T;
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no connection");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(T)).unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        assert_eq!(line, "awdl-down\n");
        (&stream).write_all(b"ok\n").unwrap();
        stream
    }

    fn wait_until(cond: impl Fn() -> bool) {
        let deadline = Instant::now() + T;
        while !cond() {
            assert!(Instant::now() < deadline, "condition never held");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_hold_sends_down_then_up_and_closes() {
        let helper = FakeHelper::start(Answer::Ok);
        let hold = AwdlHold::acquire(&helper.path).unwrap();
        assert_eq!(
            helper.lines(),
            ["awdl-down"],
            "answered before acquire returns"
        );
        assert!(hold.is_open());
        drop(hold);
        helper.wait_for(&["awdl-down", "awdl-up", "EOF"]);
    }

    #[test]
    fn a_refused_hold_is_an_error_with_the_reason() {
        let helper = FakeHelper::start(Answer::Refuse);
        let err = AwdlHold::acquire(&helper.path).unwrap_err();
        assert!(
            format!("{err:#}").contains("Operation not permitted"),
            "{err:#}"
        );
        assert!(!helper_missing(&err));
    }

    #[test]
    fn a_silent_helper_times_out_quickly() {
        let helper = FakeHelper::start(Answer::Silent);
        let started = Instant::now();
        assert!(AwdlHold::acquire(&helper.path).is_err());
        assert!(
            started.elapsed() < IO_TIMEOUT * 3,
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_missing_socket_means_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let err = AwdlHold::acquire(&dir.path().join("netd.sock")).unwrap_err();
        assert!(helper_missing(&err), "{err:#}");
    }

    #[test]
    fn a_hold_sees_the_helper_go_away() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("netd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let helper = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut out = stream.try_clone().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            out.write_all(b"ok\n").unwrap();
            // Then the helper exits.
        });
        let hold = AwdlHold::acquire(&path).unwrap();
        helper.join().unwrap();
        // A process another test spawns at that moment briefly shares the
        // helper's descriptor (until its exec closes it), so allow a moment.
        let deadline = Instant::now() + T;
        while hold.is_open() {
            assert!(Instant::now() < deadline, "the hold never saw the close");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_keeper_follows_what_is_wanted() {
        let helper = FakeHelper::start(Answer::Ok);
        let mut keeper = AwdlKeeper::spawn_at(helper.path.clone(), T).unwrap();
        assert!(!keeper.held());
        keeper.hold(true);
        helper.wait_for(&["awdl-down"]);
        wait_until(|| keeper.held());
        keeper.hold(true);
        keeper.hold(false);
        helper.wait_for(&["awdl-down", "awdl-up", "EOF"]);
        assert!(!keeper.held(), "cleared before the release is sent");
        keeper.hold(true);
        helper.wait_for(&["awdl-down", "awdl-up", "EOF", "awdl-down"]);
        assert!(!keeper.helper_missing());
        drop(keeper);
        assert_eq!(
            helper.lines()[..5],
            ["awdl-down", "awdl-up", "EOF", "awdl-down", "awdl-up"],
            "dropping the keeper released the hold before returning"
        );
        helper.wait_for(&["awdl-down", "awdl-up", "EOF", "awdl-down", "awdl-up", "EOF"]);
    }

    #[test]
    fn the_keeper_reports_a_missing_helper_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("netd.sock");
        let mut keeper = AwdlKeeper::spawn_at(path.clone(), Duration::from_millis(20)).unwrap();
        keeper.hold(true);
        let deadline = Instant::now() + T;
        while !keeper.helper_missing() {
            assert!(Instant::now() < deadline, "never reported missing");
            std::thread::sleep(Duration::from_millis(5));
        }
        // The helper gets installed while connected.
        let listener = UnixListener::bind(&path).unwrap();
        let _held = accept_hold(&listener);
        let deadline = Instant::now() + T;
        while keeper.helper_missing() {
            assert!(Instant::now() < deadline, "still reported missing");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_keeper_holds_again_after_the_helper_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("netd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut keeper = AwdlKeeper::spawn_at(path.clone(), Duration::from_millis(20)).unwrap();
        keeper.hold(true);
        for _ in 0..2 {
            // Each round: a hold arrives, then the helper drops it.
            drop(accept_hold(&listener));
        }
    }
}
