//! The clipboard side of either app, on one thread: local copies pass the
//! gate here, and items from the other computer are written here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use cj_core::clipboard::{ClipboardData, EchoGuard, within_limit};
use tracing::{info, warn};

/// What the app's loop tells the clipboard bridge.
#[derive(Debug)]
pub struct ClipboardLink {
    /// Paired and connected: local copies are worth sending.
    pub connected: AtomicBool,
    pub max_bytes: AtomicU64,
}

impl ClipboardLink {
    pub fn new(max_bytes: u64) -> Self {
        ClipboardLink {
            connected: AtomicBool::new(false),
            max_bytes: AtomicU64::new(max_bytes),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    Empty,
    TooLarge {
        bytes: usize,
    },
    /// Just sent, or the echo of an item from the other computer.
    Repeat,
    Send,
}

/// Whether to send a local clipboard item, in the required order: skip
/// empty, then the size limit, then the echo guard. The (costly) content
/// hash is only computed for items that pass the size limit.
pub fn clipboard_gate(d: &ClipboardData, max_bytes: u64, guard: &mut EchoGuard) -> Gate {
    if d.is_empty() {
        Gate::Empty
    } else if !within_limit(d, max_bytes) {
        Gate::TooLarge {
            bytes: d.byte_len(),
        }
    } else if guard.should_send(d.content_hash()) {
        Gate::Send
    } else {
        Gate::Repeat
    }
}

/// Runs the clipboard side of the app on one thread, and returns the channel
/// for clipboard data from the other computer.
///
/// - Local changes from `watch` pass `clipboard_gate` here (hashing never
///   runs on the loop); items to send go to `send`, which returns false once
///   the loop is gone.
/// - Data from the other computer is written with `write`, then marked
///   applied (hashed as `read` reads it back). Because the watcher's report
///   of that write is handled by this same thread afterwards, the echo is
///   always suppressed.
pub fn bridge_clipboard(
    watch: crossbeam_channel::Receiver<ClipboardData>,
    link: Arc<ClipboardLink>,
    read: impl Fn() -> Result<ClipboardData> + Send + 'static,
    write: impl Fn(&ClipboardData) -> Result<()> + Send + 'static,
    send: impl Fn(ClipboardData) -> bool + Send + 'static,
) -> Result<crossbeam_channel::Sender<ClipboardData>> {
    let (write_tx, write_rx) = crossbeam_channel::unbounded::<ClipboardData>();
    let apply = move |d: ClipboardData, guard: &mut EchoGuard| {
        if let Err(e) = write(&d) {
            warn!("clipboard from the other computer not applied: {e:#}");
            return;
        }
        // Hash what this computer now holds: that is what the watcher will report.
        let hash = read().map_or_else(|_| d.content_hash(), |back| back.content_hash());
        guard.mark_applied(hash);
    };
    std::thread::Builder::new()
        .name("cj-clipboard-bridge".into())
        .spawn(move || {
            let mut guard = EchoGuard::new();
            loop {
                crossbeam_channel::select! {
                    recv(watch) -> d => {
                        let Ok(d) = d else { break };
                        // Not connected: nothing to send, and the guard stays untouched.
                        if !link.connected.load(Ordering::Relaxed) {
                            continue;
                        }
                        let max = link.max_bytes.load(Ordering::Relaxed);
                        match clipboard_gate(&d, max, &mut guard) {
                            Gate::Send => {
                                if !send(d) {
                                    return;
                                }
                            }
                            Gate::TooLarge { bytes } => {
                                info!(bytes, limit = max, "clipboard too large to send; skipped")
                            }
                            Gate::Empty | Gate::Repeat => {}
                        }
                    }
                    recv(write_rx) -> d => match d {
                        Ok(d) => apply(d, &mut guard),
                        Err(_) => return,
                    },
                }
            }
            warn!("clipboard watcher stopped; local copies are no longer sent");
            for d in write_rx {
                apply(d, &mut guard);
            }
        })
        .context("starting the clipboard bridge")?;
    Ok(write_tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    fn text(s: &str) -> ClipboardData {
        ClipboardData {
            text: Some(s.into()),
            ..ClipboardData::default()
        }
    }

    #[test]
    fn gate_skips_empty_without_touching_the_guard() {
        let mut g = EchoGuard::new();
        assert_eq!(
            clipboard_gate(&ClipboardData::default(), 100, &mut g),
            Gate::Empty
        );
        // Had the empty item been recorded as sent, it would now be a repeat.
        assert_eq!(
            clipboard_gate(&ClipboardData::default(), 100, &mut g),
            Gate::Empty
        );
    }

    #[test]
    fn gate_checks_size_before_the_guard() {
        let mut g = EchoGuard::new();
        let big = text("0123456789");
        assert_eq!(
            clipboard_gate(&big, 5, &mut g),
            Gate::TooLarge { bytes: 10 }
        );
        // The oversized item was never recorded: under a larger limit it is new.
        assert_eq!(clipboard_gate(&big, 100, &mut g), Gate::Send);
    }

    #[test]
    fn gate_suppresses_duplicates_and_echoes() {
        let mut g = EchoGuard::new();
        let x = text("x");
        assert_eq!(clipboard_gate(&x, 100, &mut g), Gate::Send);
        assert_eq!(clipboard_gate(&x, 100, &mut g), Gate::Repeat);
        let y = text("y");
        g.mark_applied(y.content_hash());
        assert_eq!(clipboard_gate(&y, 100, &mut g), Gate::Repeat);
        assert_eq!(clipboard_gate(&x, 100, &mut g), Gate::Send);
    }

    /// A bridge over an in-memory clipboard.
    struct FakeBridge {
        /// Feeds the bridge as the clipboard watcher would.
        watch: crossbeam_channel::Sender<ClipboardData>,
        /// Items from the other computer.
        remote: crossbeam_channel::Sender<ClipboardData>,
        /// What the bridge sent to the loop.
        sent: crossbeam_channel::Receiver<ClipboardData>,
        board: Arc<Mutex<Option<ClipboardData>>>,
    }

    fn fake_bridge() -> FakeBridge {
        let (watch, watch_rx) = crossbeam_channel::unbounded();
        let (sent_tx, sent) = crossbeam_channel::unbounded();
        let board = Arc::new(Mutex::new(None::<ClipboardData>));
        let link = Arc::new(ClipboardLink::new(1000));
        link.connected.store(true, Ordering::Relaxed);
        let (r, w) = (board.clone(), board.clone());
        let remote = bridge_clipboard(
            watch_rx,
            link,
            move || Ok(r.lock().unwrap().clone().unwrap_or_default()),
            move |d| {
                *w.lock().unwrap() = Some(d.clone());
                Ok(())
            },
            move |d| sent_tx.send(d).is_ok(),
        )
        .unwrap();
        FakeBridge {
            watch,
            remote,
            sent,
            board,
        }
    }

    #[test]
    fn bridge_sends_local_copies_and_suppresses_the_echo_of_remote_items() {
        let FakeBridge {
            watch,
            remote,
            sent,
            board,
        } = fake_bridge();
        watch.send(text("local")).unwrap();
        assert_eq!(
            sent.recv_timeout(Duration::from_secs(2)).unwrap(),
            text("local")
        );

        remote.send(text("from the other side")).unwrap();
        // The watcher then reports the bridge's own write.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while board.lock().unwrap().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        watch.send(text("from the other side")).unwrap();
        watch.send(text("next")).unwrap();
        assert_eq!(
            sent.recv_timeout(Duration::from_secs(2)).unwrap(),
            text("next"),
            "the echo must not be sent back"
        );
    }
}
