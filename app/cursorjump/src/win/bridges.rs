//! Threads that move the input thread's and the clipboard watcher's output
//! into the link loop. Both blocking sources stay off the tokio workers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use cj_core::clipboard::{ClipboardData, EchoGuard, within_limit};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use super::app::Ev;
use super::clipboard::{read_clipboard, write_clipboard};
use super::input_thread::InputMsg;

/// Moves input-thread messages into the loop.
pub(super) fn bridge_input(
    rx: crossbeam_channel::Receiver<InputMsg>,
    events: UnboundedSender<Ev>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("cj-input-bridge".into())
        .spawn(move || {
            for msg in rx {
                if events.send(Ev::Input(msg)).is_err() {
                    return;
                }
            }
            let _ = events.send(Ev::InputGone);
        })
        .context("starting the input bridge")?;
    Ok(())
}

/// What the loop tells the clipboard bridge.
#[derive(Debug)]
pub(super) struct ClipboardLink {
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
pub(super) enum Gate {
    Empty,
    TooLarge {
        bytes: usize,
    },
    /// Just sent, or the echo of an item from the Mac.
    Repeat,
    Send,
}

/// Whether to send a local clipboard item, in the required order: skip
/// empty, then the size limit, then the echo guard. The (costly) content
/// hash is only computed for items that pass the size limit.
pub(super) fn clipboard_gate(d: &ClipboardData, max_bytes: u64, guard: &mut EchoGuard) -> Gate {
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

/// Runs the clipboard side of the PC on one thread, and returns the channel
/// for clipboard data from the Mac.
///
/// - Local changes pass `clipboard_gate` here (hashing never runs on the
///   loop); items to send go to the loop as `Ev::LocalClipboard`.
/// - Data from the Mac is written here, then marked applied. Because the
///   watcher's report of that write is handled by this same thread afterwards,
///   the echo is always suppressed.
pub(super) fn bridge_clipboard(
    watch: crossbeam_channel::Receiver<ClipboardData>,
    events: UnboundedSender<Ev>,
    link: Arc<ClipboardLink>,
) -> Result<crossbeam_channel::Sender<ClipboardData>> {
    let (write_tx, write_rx) = crossbeam_channel::unbounded::<ClipboardData>();
    let apply = |d: ClipboardData, guard: &mut EchoGuard| {
        if let Err(e) = write_clipboard(&d) {
            warn!("clipboard from the Mac not applied: {e:#}");
            return;
        }
        // Hash what this PC now holds: that is what the watcher will report.
        let hash = read_clipboard().map_or_else(|_| d.content_hash(), |back| back.content_hash());
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
                                if events.send(Ev::LocalClipboard(d)).is_err() {
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
            warn!("clipboard watcher stopped; copies on this PC are no longer sent");
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
}
