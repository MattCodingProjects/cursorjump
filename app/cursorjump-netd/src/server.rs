//! The socket and its single-threaded `poll(2)` loop: accepts the allowed
//! user's connections, runs their commands, and rechecks the interface
//! every `tick`.

use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::awdl::{Awdl, ConnId};
use crate::iface::Iface;
use crate::logging::{LOG_EVERY, RateLimit, more};
use crate::protocol::{Next, next_command, peer_allowed, reply};

/// More connections than this are refused (the app needs one).
const MAX_CONNS: usize = 8;
/// At most this many connections are accepted per wake, so a flood cannot
/// starve the recheck or the stop.
const MAX_ACCEPTS: usize = 16;

/// A request to stop that also wakes the loop's poll(): safe to make from
/// a signal handler or another thread.
pub struct Stop {
    requested: AtomicBool,
    read: OwnedFd,
    write: OwnedFd,
}

impl Stop {
    pub fn new() -> io::Result<Stop> {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe(2) returns.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both are fresh descriptors owned by nothing else.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in [&read, &write] {
            // SAFETY: fcntl on a live descriptor.
            let ok = unsafe {
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) >= 0
                    && libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) >= 0
            };
            if !ok {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Stop {
            requested: AtomicBool::new(false),
            read,
            write,
        })
    }

    /// Async-signal-safe: an atomic store and a write(2).
    pub fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
        let byte = 1u8;
        // SAFETY: writes one byte from a live buffer; a full pipe (already
        // readable) fails harmlessly.
        unsafe { libc::write(self.write.as_raw_fd(), (&raw const byte).cast(), 1) };
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

/// Binds the socket at `path`, replacing a stale one, readable and writable
/// by its owner and `group` only (root:`group` 0660 when `group` is given).
pub fn bind(path: &Path, group: Option<u32>) -> io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Created 0660 from the start: no moment in which others may connect.
    // SAFETY: umask has no preconditions; the old mask is restored below.
    let old_mask = unsafe { libc::umask(0o117) };
    let bound = UnixListener::bind(path);
    // SAFETY: as above.
    unsafe { libc::umask(old_mask) };
    let listener = bound?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    if let Some(gid) = group {
        std::os::unix::fs::chown(path, Some(0), Some(gid))?;
    }
    Ok(listener)
}

/// Serves `listener` until `stop` is requested. The interface is brought
/// back up if this daemon took it down, on every way out (`Awdl`'s Drop).
pub fn serve<I: Iface>(
    listener: UnixListener,
    allowed_uid: u32,
    iface: I,
    tick: Duration,
    stop: &Stop,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let mut server = Server {
        listener,
        allowed_uid,
        awdl: Awdl::new(iface),
        conns: Vec::new(),
        last_id: 0,
        refused_log: RateLimit::new(LOG_EVERY),
    };
    let mut next_check = Instant::now() + tick;
    loop {
        if stop.is_requested() {
            return Ok(());
        }
        // Order: the stop pipe, the listener, then each connection.
        let mut fds: Vec<libc::pollfd> = [stop.read.as_raw_fd(), server.listener.as_raw_fd()]
            .into_iter()
            .chain(server.conns.iter().map(|c| c.stream.as_raw_fd()))
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // Idle (nothing held, nothing to restore): no timer, no wakeups.
        let timeout = if server.awdl.wants_ticks() {
            let wait = next_check.saturating_duration_since(Instant::now());
            wait.as_micros()
                .div_ceil(1000)
                .min(libc::c_int::MAX as u128) as libc::c_int
        } else {
            -1
        };
        // SAFETY: `fds` is a live array of `fds.len()` pollfds.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        // poll() is level-triggered: work left over wakes the next round.
        server.serve_ready(&fds[2..]);
        if fds[1].revents != 0 {
            server.accept_some();
        }
        if !server.awdl.wants_ticks() {
            next_check = Instant::now() + tick;
        } else if Instant::now() >= next_check {
            server.awdl.recheck();
            next_check = Instant::now() + tick;
        }
    }
}

struct Server<I: Iface> {
    listener: UnixListener,
    allowed_uid: u32,
    awdl: Awdl<I>,
    conns: Vec<Conn>,
    last_id: ConnId,
    /// For refused connections and invalid commands.
    refused_log: RateLimit,
}

struct Conn {
    id: ConnId,
    stream: UnixStream,
    /// Received bytes not yet making a complete line.
    buf: Vec<u8>,
}

impl<I: Iface> Server<I> {
    /// Serves the connections whose `polled` entries (in `conns` order) are ready.
    fn serve_ready(&mut self, polled: &[libc::pollfd]) {
        let mut closed = Vec::new();
        for (conn, pfd) in self.conns.iter_mut().zip(polled) {
            if pfd.revents != 0 && !serve_conn(conn, &mut self.awdl, &mut self.refused_log) {
                closed.push(conn.id);
            }
        }
        self.conns.retain(|c| !closed.contains(&c.id));
        for id in closed {
            self.awdl.closed(id);
        }
    }

    /// Accepts up to MAX_ACCEPTS waiting connections.
    fn accept_some(&mut self) {
        for _ in 0..MAX_ACCEPTS {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.refused(format_args!("accept failed: {e}"));
                    return;
                }
            };
            // Dropping `stream` closes a refused connection.
            let uid = match peer_uid(&stream) {
                Ok(uid) => uid,
                Err(e) => {
                    self.refused(format_args!("refused a connection: getpeereid: {e}"));
                    continue;
                }
            };
            if !peer_allowed(uid, self.allowed_uid) {
                self.refused(format_args!("refused a connection from uid {uid}"));
                continue;
            }
            if self.conns.len() >= MAX_CONNS {
                self.refused(format_args!(
                    "refused a connection from uid {uid}: {MAX_CONNS} already open"
                ));
                continue;
            }
            if let Err(e) = stream.set_nonblocking(true) {
                self.refused(format_args!("refused a connection: {e}"));
                continue;
            }
            self.last_id += 1;
            self.conns.push(Conn {
                id: self.last_id,
                stream,
                buf: Vec::new(),
            });
        }
    }

    fn refused(&mut self, what: std::fmt::Arguments) {
        if let Some(n) = self.refused_log.hit(Instant::now()) {
            log!("{what}{}", more(n));
        }
    }
}

/// The effective uid of the process at the other end.
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: a connected socket and two live out-parameters.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

/// One read from `conn` (more waits for the next wake), then its complete
/// commands; false to close it.
fn serve_conn<I: Iface>(conn: &mut Conn, awdl: &mut Awdl<I>, refused_log: &mut RateLimit) -> bool {
    let mut chunk = [0u8; 64];
    match conn.stream.read(&mut chunk) {
        Ok(0) => return false,
        Ok(n) => conn.buf.extend_from_slice(&chunk[..n]),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
            return true;
        }
        Err(_) => return false,
    }
    // The buffer never grows past a line and a chunk.
    loop {
        match next_command(&mut conn.buf) {
            Next::Incomplete => return true,
            Next::Command(cmd) => {
                let result = awdl.command(conn.id, cmd);
                // Replies are tiny: a full socket buffer means a client
                // that does not read them, and it is closed.
                if conn.stream.write_all(reply(&result).as_bytes()).is_err() {
                    return false;
                }
            }
            Next::Invalid => {
                if let Some(n) = refused_log.hit(Instant::now()) {
                    log!(
                        "connection {}: invalid command; closing it{}",
                        conn.id,
                        more(n)
                    );
                }
                let _ = conn.stream.write_all(b"err unknown command\n");
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awdl::tests::Fake;
    use std::io::{BufRead, BufReader};
    use std::sync::Arc;
    use std::thread::JoinHandle;

    const TICK: Duration = Duration::from_millis(20);
    /// Upper bound for anything expected to happen.
    const T: Duration = Duration::from_secs(5);

    fn my_uid() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }

    struct Daemon {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        fake: Fake,
        stop: Arc<Stop>,
        thread: Option<JoinHandle<io::Result<()>>>,
    }

    impl Daemon {
        fn start(allowed_uid: u32) -> Daemon {
            Daemon::start_with(allowed_uid, TICK)
        }

        fn start_with(allowed_uid: u32, tick: Duration) -> Daemon {
            let (dir, path, listener) = temp_listener();
            let fake = Fake::up();
            let stop = Arc::new(Stop::new().unwrap());
            let (f, s) = (fake.clone(), stop.clone());
            let thread = std::thread::spawn(move || serve(listener, allowed_uid, f, tick, &s));
            Daemon {
                _dir: dir,
                path,
                fake,
                stop,
                thread: Some(thread),
            }
        }

        fn connect(&self) -> Client {
            let s = UnixStream::connect(&self.path).unwrap();
            s.set_read_timeout(Some(T)).unwrap();
            Client(BufReader::new(s))
        }

        fn stop(&mut self) {
            self.stop.request();
            self.thread.take().unwrap().join().unwrap().unwrap();
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            self.stop.request();
        }
    }

    /// bind() changes the process-wide umask for a moment: no other test
    /// may create files meanwhile (a directory made then would lack +x).
    static FILES: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A temporary directory and a socket bound in it.
    fn temp_listener() -> (tempfile::TempDir, std::path::PathBuf, UnixListener) {
        let _files = FILES.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("netd.sock");
        let listener = bind(&path, None).unwrap();
        (dir, path, listener)
    }

    struct Client(BufReader<UnixStream>);

    impl Client {
        fn send(&mut self, line: &str) -> String {
            self.0.get_mut().write_all(line.as_bytes()).unwrap();
            let mut reply = String::new();
            self.0.read_line(&mut reply).unwrap();
            reply
        }

        /// True once the daemon closed the connection.
        fn is_closed(&mut self) -> bool {
            let mut rest = Vec::new();
            match self.0.read_to_end(&mut rest) {
                Ok(_) => true,
                Err(e) => e.kind() == io::ErrorKind::ConnectionReset,
            }
        }
    }

    /// Waits until `cond` holds, or fails the test.
    fn eventually(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + T;
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_socket_is_private_to_owner_and_group() {
        let _files = FILES.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("netd.sock");
        std::fs::write(&path, b"stale").unwrap();
        // SAFETY: umask has no preconditions; FILES keeps other tests out.
        let before = unsafe { libc::umask(0o022) };
        let _l = bind(&path, None).unwrap();
        // SAFETY: as above; this also reads back what bind left.
        let after = unsafe { libc::umask(before) };
        assert_eq!(after, 0o022, "bind restores the umask");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o660);
    }

    #[test]
    fn an_idle_daemon_stops_at_once() {
        // Idle, it sleeps in poll() without a timeout: the stop wakes it.
        let mut d = Daemon::start_with(my_uid(), Duration::from_secs(3600));
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        d.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_held_daemon_stops_at_once_and_restores() {
        let mut d = Daemon::start_with(my_uid(), Duration::from_secs(3600));
        let mut c = d.connect();
        assert_eq!(c.send("awdl-down\n"), "ok\n");
        let started = Instant::now();
        d.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(d.fake.is_up());
    }

    #[test]
    fn a_flooding_client_does_not_hold_up_others() {
        let d = Daemon::start(my_uid());
        let mut flood = d.connect();
        // Many commands in one write: served a chunk per wake, all answered.
        let burst = "awdl-down\n".repeat(200);
        let writer = {
            let mut s = flood.0.get_ref().try_clone().unwrap();
            std::thread::spawn(move || s.write_all(burst.as_bytes()))
        };
        let mut other = d.connect();
        assert_eq!(other.send("awdl-down\n"), "ok\n");
        for _ in 0..200 {
            let mut reply = String::new();
            flood.0.read_line(&mut reply).unwrap();
            assert_eq!(reply, "ok\n");
        }
        writer.join().unwrap().unwrap();
    }

    #[test]
    fn a_hold_lasts_until_awdl_up() {
        let mut d = Daemon::start(my_uid());
        let mut c = d.connect();
        assert_eq!(c.send("awdl-down\n"), "ok\n");
        assert!(!d.fake.is_up());
        assert_eq!(c.send("awdl-up\n"), "ok\n");
        assert!(d.fake.is_up());
        d.stop();
    }

    #[test]
    fn a_closed_or_crashed_client_releases_its_hold() {
        let mut d = Daemon::start(my_uid());
        let mut c = d.connect();
        assert_eq!(c.send("awdl-down\n"), "ok\n");
        drop(c);
        let fake = d.fake.clone();
        eventually("awdl0 back up", || fake.is_up());
        d.stop();
    }

    #[test]
    fn the_last_of_several_holders_restores() {
        let mut d = Daemon::start(my_uid());
        let (mut a, mut b) = (d.connect(), d.connect());
        assert_eq!(a.send("awdl-down\n"), "ok\n");
        assert_eq!(b.send("awdl-down\n"), "ok\n");
        drop(a);
        // A round trip on b: a's close has been handled by then.
        assert_eq!(b.send("awdl-down\n"), "ok\n");
        assert!(!d.fake.is_up());
        drop(b);
        let fake = d.fake.clone();
        eventually("awdl0 back up", || fake.is_up());
        d.stop();
    }

    #[test]
    fn a_bad_command_closes_the_connection() {
        let mut d = Daemon::start(my_uid());
        let mut c = d.connect();
        assert_eq!(c.send("awdl-down\n"), "ok\n");
        assert_eq!(c.send("rm -rf /\n"), "err unknown command\n");
        assert!(c.is_closed());
        let fake = d.fake.clone();
        eventually("its hold released", || fake.is_up());
        d.stop();
    }

    #[test]
    fn an_over_long_line_closes_the_connection() {
        let mut d = Daemon::start(my_uid());
        let mut c = d.connect();
        c.0.get_mut().write_all(&[b'a'; 100]).unwrap();
        assert!(c.is_closed());
        d.stop();
    }

    #[test]
    fn another_user_is_refused() {
        let mine = my_uid();
        assert_ne!(mine, 0, "run the tests as a normal user");
        let mut d = Daemon::start(mine + 1);
        let mut c = d.connect();
        assert!(c.is_closed());
        assert!(d.fake.is_up());
        d.stop();
    }

    #[test]
    fn macos_bringing_it_back_up_is_undone() {
        let mut d = Daemon::start(my_uid());
        let mut c = d.connect();
        assert_eq!(c.send("awdl-down\n"), "ok\n");
        d.fake.take_sets();
        d.fake.set_externally(true);
        let fake = d.fake.clone();
        eventually("awdl0 down again", || !fake.is_up());
        assert_eq!(d.fake.take_sets(), [false]);
        d.stop();
    }

    #[test]
    fn stopping_restores_a_held_interface() {
        let mut d = Daemon::start(my_uid());
        let mut c = d.connect();
        assert_eq!(c.send("awdl-down\n"), "ok\n");
        d.stop();
        assert!(d.fake.is_up());
    }

    #[test]
    fn a_failed_change_is_reported_and_the_connection_kept() {
        let mut d = Daemon::start(my_uid());
        d.fake.set_failing(true);
        let mut c = d.connect();
        assert_eq!(c.send("awdl-down\n"), "err permission denied\n");
        d.fake.set_failing(false);
        assert_eq!(c.send("awdl-down\n"), "ok\n");
        d.stop();
    }
}
