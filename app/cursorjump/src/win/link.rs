//! One connection to the Mac: finding and connecting to it (`establish`),
//! the tasks serving a live session (`Live`), and the reconnect schedule.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cj_core::clipboard::ClipboardData;
use cj_core::config::Config;
use cj_core::proto::{Control, Datagram};
use cj_net::discovery::{DISCOVERY_PORT, QUIC_PORT, choose_peer, discover};
use cj_net::session::{Session, connect};
use cj_net::{Identity, Trust};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::app::Ev;
use crate::common::paired_fingerprint;

/// `discover` needs at least 3 s to get through mDNS and the broadcast fallback.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(3);
/// Pongs reporting a longer round trip are ignored as bogus.
const MAX_RTT: Duration = Duration::from_secs(10);

/// What a session's tasks report to the loop.
pub(super) enum NetEv {
    Control(Control),
    /// Measured in the reader, so loop delays do not inflate it.
    Pong {
        rtt: Duration,
        at: std::time::Instant,
    },
    Clipboard(ClipboardData),
    Closed(String),
}

pub(super) struct Established {
    pub session: Session,
    /// The pairing code when the session was made in pairing mode.
    pub code: Option<String>,
}

/// Reconnect delays: 250 ms, 500 ms, 1 s, then 2 s until a connection succeeds.
#[derive(Debug, Default)]
pub(super) struct Backoff {
    failures: usize,
}

impl Backoff {
    const STEPS: [Duration; 3] = [
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_secs(1),
    ];
    const CAP: Duration = Duration::from_secs(2);

    pub fn next_delay(&mut self) -> Duration {
        let d = Self::STEPS.get(self.failures).copied().unwrap_or(Self::CAP);
        self.failures = self.failures.saturating_add(1);
        d
    }

    pub fn reset(&mut self) {
        self.failures = 0;
    }
}

/// `manual_peer_addr` as "ip:port", or a bare IP on the default port.
pub(super) fn manual_addr(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    s.parse().ok().or_else(|| {
        s.parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, QUIC_PORT))
    })
}

/// Discovery, then connect: pinned to the paired Mac, or in pairing mode
/// (running the pairing exchange before anything else uses the session).
pub(super) async fn establish(id: &Identity, cfg: &Config, name: &str) -> Result<Established> {
    let paired = paired_fingerprint(cfg);
    let peers = discover(DISCOVER_TIMEOUT, DISCOVERY_PORT)
        .await
        .context("discovery")?;
    let addr = match choose_peer(&peers, paired.as_ref()) {
        Some(peer) => peer.addr,
        None => match cfg.manual_peer_addr.as_deref() {
            Some(s) => manual_addr(s).with_context(|| format!("bad manual_peer_addr {s:?}"))?,
            None => bail!("no Mac found ({} other device(s) answered)", peers.len()),
        },
    };
    let trust = paired.map_or(Trust::PairingMode, Trust::Pinned);
    let session = connect(id, trust, addr, name).await?;
    let code = match paired {
        Some(_) => None,
        None => Some(session.pair().await.context("pairing exchange")?),
    };
    Ok(Established { session, code })
}

/// The current session and the tasks serving it. Dropping it closes the session.
pub(super) struct Live {
    pub sid: u64,
    pub session: Session,
    pub peer_name: String,
    /// Latest local clipboard item to send; a newer one replaces an unsent one.
    clip_out: watch::Sender<Option<Arc<ClipboardData>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Live {
    /// Starts the readers. Only call once any pairing exchange is done.
    /// `epoch` is the zero point of this PC's ping timestamps.
    pub fn start(
        sid: u64,
        session: Session,
        clip_max: u64,
        epoch: std::time::Instant,
        events: &UnboundedSender<Ev>,
    ) -> Live {
        let net = move |ev| Ev::Net { sid, ev };
        let mut tasks = Vec::new();

        let (s, tx) = (session.clone(), events.clone());
        tasks.push(tokio::spawn(async move {
            let reason = loop {
                match s.recv_control().await {
                    Ok(c) => {
                        if tx.send(net(NetEv::Control(c))).is_err() {
                            return;
                        }
                    }
                    Err(e) => break format!("control stream: {e:#}"),
                }
            };
            let _ = tx.send(net(NetEv::Closed(reason)));
        }));

        let (s, tx) = (session.clone(), events.clone());
        tasks.push(tokio::spawn(async move {
            let reason = loop {
                match s.recv_datagram().await {
                    Ok(Datagram::Pong { sent_us, .. }) => {
                        let at = std::time::Instant::now();
                        let now_us = at.duration_since(epoch).as_micros() as u64;
                        if let Some(rtt) = now_us.checked_sub(sent_us).map(Duration::from_micros)
                            && rtt <= MAX_RTT
                            && tx.send(net(NetEv::Pong { rtt, at })).is_err()
                        {
                            return;
                        }
                    }
                    // The Mac measures latency too.
                    Ok(Datagram::Ping { seq, sent_us }) => {
                        let _ = s.send_datagram(&Datagram::Pong { seq, sent_us });
                    }
                    Ok(_) => {}
                    Err(e) => break format!("connection: {e:#}"),
                }
            };
            let _ = tx.send(net(NetEv::Closed(reason)));
        }));

        let (s, tx) = (session.clone(), events.clone());
        tasks.push(tokio::spawn(async move {
            loop {
                match s.recv_clipboard(clip_max).await {
                    Ok(d) => {
                        if tx.send(net(NetEv::Clipboard(d))).is_err() {
                            return;
                        }
                    }
                    // A closed session is reported by the other readers.
                    Err(_) if is_closed(&s).await => return,
                    Err(e) => warn!("clipboard from the Mac not accepted: {e:#}"),
                }
            }
        }));

        let (clip_out, mut clip_rx) = watch::channel(None::<Arc<ClipboardData>>);
        let s = session.clone();
        tasks.push(tokio::spawn(async move {
            while clip_rx.changed().await.is_ok() {
                let item = clip_rx.borrow_and_update().clone();
                if let Some(d) = item
                    && let Err(e) = s.send_clipboard(&d).await
                {
                    debug!("clipboard not sent: {e:#}");
                }
            }
        }));

        Live {
            sid,
            peer_name: session.peer_name(),
            session,
            clip_out,
            tasks,
        }
    }

    /// Queues a local clipboard item, replacing one not yet sent.
    pub fn send_clipboard(&self, d: ClipboardData) {
        self.clip_out.send_replace(Some(Arc::new(d)));
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.session.close();
    }
}

async fn is_closed(s: &Session) -> bool {
    tokio::time::timeout(Duration::ZERO, s.closed())
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn backoff_schedule() {
        let mut b = Backoff::default();
        let delays: Vec<Duration> = (0..6).map(|_| b.next_delay()).collect();
        assert_eq!(
            delays,
            [ms(250), ms(500), ms(1000), ms(2000), ms(2000), ms(2000)]
        );
    }

    #[test]
    fn backoff_resets_after_connect() {
        let mut b = Backoff::default();
        for _ in 0..10 {
            b.next_delay();
        }
        b.reset();
        assert_eq!(b.next_delay(), ms(250));
        assert_eq!(b.next_delay(), ms(500));
    }

    #[test]
    fn manual_addr_with_and_without_port() {
        assert_eq!(
            manual_addr("192.168.1.20:47800"),
            Some(([192, 168, 1, 20], 47800).into())
        );
        assert_eq!(
            manual_addr(" 192.168.1.20 "),
            Some(([192, 168, 1, 20], QUIC_PORT).into())
        );
        assert_eq!(manual_addr("macbook.local"), None);
    }
}
