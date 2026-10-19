//! What every live session runs on either computer: the clipboard tasks
//! (receiving items, and sending the latest local one) and round-trip
//! measurement from pongs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cj_core::clipboard::ClipboardData;
use cj_core::proto::Control;
use cj_net::session::Session;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Pongs reporting a longer round trip are ignored as bogus.
const MAX_RTT: Duration = Duration::from_secs(10);

/// What a session's tasks report to the app's loop.
#[derive(Debug)]
pub enum NetEv {
    Control(Control),
    /// Measured in the reader, so loop delays do not inflate it.
    Pong {
        rtt: Duration,
        at: Instant,
    },
    Clipboard(ClipboardData),
    Closed(String),
}

/// The round trip of a ping sent `sent_us` after `epoch` whose pong arrived
/// at `at`; None if it is impossible or implausibly long.
fn round_trip(epoch: Instant, sent_us: u64, at: Instant) -> Option<Duration> {
    let now_us = at.duration_since(epoch).as_micros() as u64;
    now_us
        .checked_sub(sent_us)
        .map(Duration::from_micros)
        .filter(|rtt| *rtt <= MAX_RTT)
}

/// The event for a pong just received for a ping sent `sent_us` after
/// `epoch` (this computer's ping clock); None for a bogus one.
pub fn pong(epoch: Instant, sent_us: u64) -> Option<NetEv> {
    let at = Instant::now();
    round_trip(epoch, sent_us, at).map(|rtt| NetEv::Pong { rtt, at })
}

/// The latest local clipboard item to send; a newer one replaces one not
/// yet sent.
pub struct ClipboardOut(watch::Sender<Option<Arc<ClipboardData>>>);

impl ClipboardOut {
    pub fn send(&self, d: ClipboardData) {
        self.0.send_replace(Some(Arc::new(d)));
    }
}

/// Starts the session's clipboard reader, which passes each item (of at
/// most `clip_max` bytes) to `on_item` until that returns false, and its
/// sender of the items queued on the returned `ClipboardOut`.
pub fn spawn_clipboard_tasks(
    session: &Session,
    clip_max: u64,
    on_item: impl Fn(ClipboardData) -> bool + Send + 'static,
) -> (ClipboardOut, [JoinHandle<()>; 2]) {
    let s = session.clone();
    let reader = tokio::spawn(async move {
        loop {
            match s.recv_clipboard(clip_max).await {
                Ok(d) => {
                    if !on_item(d) {
                        return;
                    }
                }
                // A closed session is reported by the other readers.
                Err(_) if is_closed(&s).await => return,
                Err(e) => warn!("clipboard from the other computer not accepted: {e:#}"),
            }
        }
    });

    let (out, mut rx) = watch::channel(None::<Arc<ClipboardData>>);
    let s = session.clone();
    let sender = tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let item = rx.borrow_and_update().clone();
            if let Some(d) = item
                && let Err(e) = s.send_clipboard(&d).await
            {
                debug!("clipboard not sent: {e:#}");
            }
        }
    });
    (ClipboardOut(out), [reader, sender])
}

async fn is_closed(s: &Session) -> bool {
    tokio::time::timeout(Duration::ZERO, s.closed())
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_net::session::{Listener, connect};
    use cj_net::{Identity, Trust};
    use tokio::time::timeout;

    const T: Duration = Duration::from_secs(10);

    #[test]
    fn round_trip_is_measured_from_the_epoch() {
        let epoch = Instant::now();
        let at = epoch + Duration::from_millis(50);
        assert_eq!(
            round_trip(epoch, 46_000, at),
            Some(Duration::from_millis(4))
        );
        assert_eq!(round_trip(epoch, 51_000, at), None, "sent after it arrived");
        let late = epoch + Duration::from_secs(20);
        assert_eq!(round_trip(epoch, 0, late), None, "implausibly long");
    }

    fn text(s: &str) -> ClipboardData {
        ClipboardData {
            text: Some(s.into()),
            ..ClipboardData::default()
        }
    }

    #[tokio::test]
    async fn clipboard_items_cross_and_oversized_ones_are_refused() {
        let ident = || {
            let dir = tempfile::tempdir().unwrap();
            Identity::load_or_create(dir.path()).unwrap()
        };
        let (a, b) = (ident(), ident());
        let any_port = (std::net::Ipv4Addr::LOCALHOST, 0).into();
        let listener = Listener::bind(&a, Trust::PairingMode, any_port, "A")
            .await
            .unwrap();
        let (sa, sb) = timeout(T, async {
            tokio::join!(
                listener.accept(),
                connect(&b, Trust::PairingMode, listener.local_addr(), "B")
            )
        })
        .await
        .unwrap();
        let (sa, sb) = (sa.unwrap(), sb.unwrap());

        let (got_tx, mut got_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_a_out, _a_tasks) = spawn_clipboard_tasks(&sa, 10, move |d| got_tx.send(d).is_ok());
        let (b_out, _b_tasks) = spawn_clipboard_tasks(&sb, 10, |_| true);

        b_out.send(text("this is far too long"));
        b_out.send(text("hello"));
        // Latest wins: the oversized item may be replaced before it is sent;
        // either way only "hello" arrives.
        let d = timeout(T, got_rx.recv()).await.unwrap().unwrap();
        assert_eq!(d, text("hello"));
    }
}
