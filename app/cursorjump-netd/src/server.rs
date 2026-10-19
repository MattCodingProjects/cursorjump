//! The socket and its single-threaded `poll(2)` loop: accepts the allowed
//! user's connections, runs their commands, and rechecks the interface
//! every `tick`.

use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::awdl::{Awdl, ConnId};
use crate::iface::Iface;
use crate::protocol::{Next, next_command, peer_allowed, reply};

/// More connections than this are refused (the app needs one).
const MAX_CONNS: usize = 8;

/// Binds the socket at `path`, replacing a stale one, readable and writable
/// by its owner and `group` only (root:`group` 0660 when `group` is given).
pub fn bind(path: &Path, group: Option<u32>) -> io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    if let Some(gid) = group {
        std::os::unix::fs::chown(path, Some(0), Some(gid))?;
    }
    Ok(listener)
}

/// Serves `listener` until `stop` is set, then brings the interface back
/// up if this daemon took it down.
pub fn serve<I: Iface>(
    listener: UnixListener,
    allowed_uid: u32,
    iface: I,
    tick: Duration,
    stop: &AtomicBool,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let mut awdl = Awdl::new(iface);
    let mut conns: Vec<Conn> = Vec::new();
    let mut last_id: ConnId = 0;
    let mut next_check = Instant::now() + tick;
    let result = loop {
        // A signal between this check and poll() waits at most one tick.
        if stop.load(Ordering::SeqCst) {
            break Ok(());
        }
        let mut fds: Vec<libc::pollfd> = std::iter::once(listener.as_raw_fd())
            .chain(conns.iter().map(|c| c.stream.as_raw_fd()))
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let wait = next_check.saturating_duration_since(Instant::now());
        let wait_ms = wait
            .as_micros()
            .div_ceil(1000)
            .min(libc::c_int::MAX as u128);
        // SAFETY: `fds` is a live array of `fds.len()` pollfds.
        let n = unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                wait_ms as libc::c_int,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            break Err(e);
        }
        // fds[1..] are the connections open before this accept, in order.
        let mut closed = Vec::new();
        for (conn, pfd) in conns.iter_mut().zip(&fds[1..]) {
            if pfd.revents != 0 && !serve_conn(conn, &mut awdl) {
                closed.push(conn.id);
            }
        }
        conns.retain(|c| !closed.contains(&c.id));
        for id in closed {
            log!("connection {id} closed");
            awdl.closed(id);
        }
        if fds[0].revents != 0 {
            accept_all(&listener, allowed_uid, &mut conns, &mut last_id);
        }
        if Instant::now() >= next_check {
            awdl.recheck();
            next_check = Instant::now() + tick;
        }
    };
    awdl.shutdown();
    result
}

struct Conn {
    id: ConnId,
    stream: UnixStream,
    /// Received bytes not yet making a complete line.
    buf: Vec<u8>,
}

fn accept_all(
    listener: &UnixListener,
    allowed_uid: u32,
    conns: &mut Vec<Conn>,
    last_id: &mut ConnId,
) {
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                log!("accept failed: {e}");
                return;
            }
        };
        // Dropping `stream` closes a refused connection.
        let uid = match peer_uid(&stream) {
            Ok(uid) => uid,
            Err(e) => {
                log!("refused a connection: getpeereid: {e}");
                continue;
            }
        };
        if !peer_allowed(uid, allowed_uid) {
            log!("refused a connection from uid {uid}");
            continue;
        }
        if conns.len() >= MAX_CONNS {
            log!("refused a connection from uid {uid}: {MAX_CONNS} already open");
            continue;
        }
        if let Err(e) = stream.set_nonblocking(true) {
            log!("refused a connection: {e}");
            continue;
        }
        *last_id += 1;
        log!("connection {last_id} from uid {uid}");
        conns.push(Conn {
            id: *last_id,
            stream,
            buf: Vec::new(),
        });
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

/// Reads what `conn` sent and runs its commands; false to close it.
fn serve_conn<I: Iface>(conn: &mut Conn, awdl: &mut Awdl<I>) -> bool {
    let mut chunk = [0u8; 64];
    loop {
        match conn.stream.read(&mut chunk) {
            Ok(0) => return false,
            Ok(n) => conn.buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
        // After every read, so the buffer never grows past a line and a chunk.
        loop {
            match next_command(&mut conn.buf) {
                Next::Incomplete => break,
                Next::Command(cmd) => {
                    let result = awdl.command(conn.id, cmd);
                    // Replies are tiny: a full socket buffer means a client
                    // that does not read them, and it is closed.
                    if conn.stream.write_all(reply(&result).as_bytes()).is_err() {
                        return false;
                    }
                }
                Next::Invalid => {
                    log!("connection {}: invalid command; closing it", conn.id);
                    let _ = conn.stream.write_all(b"err unknown command\n");
                    return false;
                }
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
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<io::Result<()>>>,
    }

    impl Daemon {
        fn start(allowed_uid: u32) -> Daemon {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("netd.sock");
            let listener = bind(&path, None).unwrap();
            let fake = Fake::up();
            let stop = Arc::new(AtomicBool::new(false));
            let (f, s) = (fake.clone(), stop.clone());
            let thread = std::thread::spawn(move || serve(listener, allowed_uid, f, TICK, &s));
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
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap().unwrap();
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("netd.sock");
        std::fs::write(&path, b"stale").unwrap();
        let _l = bind(&path, None).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o660);
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
