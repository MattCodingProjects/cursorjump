//! LAN discovery: the Mac advertises over mDNS and answers a UDP broadcast
//! query; the PC tries mDNS first and falls back to the broadcast query when
//! the router filters multicast.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::identity::Fingerprint;

pub const QUIC_PORT: u16 = 47800;
pub const DISCOVERY_PORT: u16 = 47801;
pub const SERVICE: &str = "_cursorjump._udp.local.";

const QUERY: &[u8] = b"CJ?1";
const REPLY_PREFIX: &[u8] = b"CJ!1";
const MDNS_PHASE: Duration = Duration::from_millis(1500);
const QUERY_INTERVAL: Duration = Duration::from_millis(500);
const EXTRA_COLLECT: Duration = Duration::from_millis(300);

/// A discovered Mac.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub addr: SocketAddr,
    pub fingerprint: Fingerprint,
    pub name: String,
}

#[derive(Serialize, Deserialize)]
struct BeaconReply {
    port: u16,
    fp: [u8; 32],
    name: String,
}

/// Keeps the mDNS registration and the broadcast responder alive until dropped.
pub struct Advertiser {
    mdns: Option<(ServiceDaemon, String)>,
    stop: Arc<AtomicBool>,
    responder: Option<JoinHandle<()>>,
}

impl Advertiser {
    /// Advertises over mDNS and answers broadcast queries on `responder_port`.
    pub fn start(
        name: &str,
        port: u16,
        fp: &Fingerprint,
        responder_port: u16,
    ) -> Result<Advertiser> {
        let mut adv = Self::start_responder_only(name, port, fp, responder_port)?;
        let label = sanitize(name);
        let daemon = ServiceDaemon::new()?;
        let props = HashMap::from([
            ("fp".to_string(), fp.to_hex()),
            ("v".to_string(), "1".to_string()),
        ]);
        let info = ServiceInfo::new(SERVICE, &label, &format!("{label}.local."), "", port, props)?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        adv.mdns = Some((daemon, fullname));
        Ok(adv)
    }

    /// Only the UDP broadcast responder, without mDNS.
    pub fn start_responder_only(
        name: &str,
        port: u16,
        fp: &Fingerprint,
        responder_port: u16,
    ) -> Result<Advertiser> {
        let sock = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, responder_port))?;
        sock.set_read_timeout(Some(Duration::from_millis(250)))?;
        let mut reply = REPLY_PREFIX.to_vec();
        reply.extend(postcard::to_allocvec(&BeaconReply {
            port,
            fp: fp.0,
            name: name.to_string(),
        })?);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let responder = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            while !flag.load(Ordering::Relaxed) {
                if let Ok((n, from)) = sock.recv_from(&mut buf)
                    && &buf[..n] == QUERY
                {
                    let _ = sock.send_to(&reply, from);
                }
            }
        });
        Ok(Advertiser {
            mdns: None,
            stop,
            responder: Some(responder),
        })
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        if let Some((daemon, fullname)) = self.mdns.take() {
            if let Ok(rx) = daemon.unregister(&fullname) {
                let _ = rx.recv_timeout(Duration::from_millis(500));
            }
            if let Ok(rx) = daemon.shutdown() {
                let _ = rx.recv_timeout(Duration::from_millis(500));
            }
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.responder.take() {
            let _ = h.join();
        }
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(63)
        .collect();
    if s.is_empty() { "cursorjump".into() } else { s }
}

/// Finds Macs on the LAN: mDNS first, then UDP broadcast for the remaining time.
pub async fn discover(timeout: Duration, broadcast_port: u16) -> Result<Vec<Peer>> {
    let deadline = Instant::now() + timeout;
    let mut peers = discover_mdns(timeout.min(MDNS_PHASE)).await;
    if peers.is_empty() {
        peers = discover_broadcast(deadline, broadcast_port).await?;
    }
    Ok(peers)
}

/// With a pairing, only the peer whose fingerprint matches; otherwise the first.
pub fn choose_peer<'a>(peers: &'a [Peer], paired: Option<&Fingerprint>) -> Option<&'a Peer> {
    match paired {
        Some(fp) => peers.iter().find(|p| &p.fingerprint == fp),
        None => peers.first(),
    }
}

fn push_unique(peers: &mut Vec<Peer>, peer: Peer) {
    if !peers.iter().any(|p| p.fingerprint == peer.fingerprint) {
        peers.push(peer);
    }
}

async fn discover_mdns(window: Duration) -> Vec<Peer> {
    let mut peers = Vec::new();
    let Ok(daemon) = ServiceDaemon::new() else {
        return peers;
    };
    let Ok(rx) = daemon.browse(SERVICE) else {
        let _ = daemon.shutdown();
        return peers;
    };
    let end = Instant::now() + window;
    let mut extra_until: Option<Instant> = None;
    loop {
        let stop_at = extra_until.map_or(end, |e| e.min(end));
        let Ok(Ok(event)) = tokio::time::timeout_at(stop_at, rx.recv_async()).await else {
            break;
        };
        if let ServiceEvent::ServiceResolved(info) = event
            && info.get_property_val_str("v") == Some("1")
            && let Some(fingerprint) = info
                .get_property_val_str("fp")
                .and_then(Fingerprint::from_hex)
        {
            let addrs: Vec<IpAddr> = info
                .get_addresses()
                .iter()
                .map(|a| a.to_ip_addr())
                .collect();
            let ip = addrs.iter().find(|a| a.is_ipv4()).or(addrs.first());
            if let Some(ip) = ip {
                push_unique(
                    &mut peers,
                    Peer {
                        addr: SocketAddr::new(*ip, info.get_port()),
                        fingerprint,
                        name: info
                            .get_fullname()
                            .split('.')
                            .next()
                            .unwrap_or("")
                            .to_string(),
                    },
                );
                extra_until.get_or_insert_with(|| Instant::now() + EXTRA_COLLECT);
            }
        }
    }
    let _ = daemon.shutdown();
    peers
}

async fn discover_broadcast(deadline: Instant, port: u16) -> Result<Vec<Peer>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    sock.set_broadcast(true)?;
    let mut targets: Vec<SocketAddr> = vec![
        (Ipv4Addr::BROADCAST, port).into(),
        (Ipv4Addr::LOCALHOST, port).into(),
    ];
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = iface.addr
            && let Some(b) = v4.broadcast
        {
            let t = SocketAddr::new(b.into(), port);
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
    }

    let mut peers: Vec<Peer> = Vec::new();
    let mut extra_until: Option<Instant> = None;
    let mut next_query = Instant::now();
    let mut buf = [0u8; 512];
    loop {
        let now = Instant::now();
        let end = extra_until.map_or(deadline, |e| e.min(deadline));
        if now >= end {
            break;
        }
        if now >= next_query {
            for t in &targets {
                let _ = sock.send_to(QUERY, t).await;
            }
            next_query = now + QUERY_INTERVAL;
        }
        let wake = next_query.min(end);
        let Ok(res) = tokio::time::timeout_at(wake, sock.recv_from(&mut buf)).await else {
            continue;
        };
        let Ok((n, from)) = res else { continue };
        let Some(body) = buf[..n].strip_prefix(REPLY_PREFIX) else {
            continue;
        };
        let Ok(reply) = postcard::from_bytes::<BeaconReply>(body) else {
            continue;
        };
        push_unique(
            &mut peers,
            Peer {
                addr: SocketAddr::new(from.ip(), reply.port),
                fingerprint: Fingerprint(reply.fp),
                name: reply.name,
            },
        );
        extra_until.get_or_insert_with(|| Instant::now() + EXTRA_COLLECT);
    }
    Ok(peers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(b: u8) -> Peer {
        Peer {
            addr: ([10, 0, 0, b], 47800).into(),
            fingerprint: Fingerprint([b; 32]),
            name: format!("p{b}"),
        }
    }

    #[test]
    fn picks_paired_among_several() {
        let peers = [peer(1), peer(2), peer(3)];
        let want = Fingerprint([2; 32]);
        assert_eq!(choose_peer(&peers, Some(&want)), Some(&peers[1]));
    }

    #[test]
    fn none_when_paired_absent() {
        let peers = [peer(1), peer(2)];
        assert_eq!(choose_peer(&peers, Some(&Fingerprint([9; 32]))), None);
    }

    #[test]
    fn first_when_unpaired() {
        let peers = [peer(1), peer(2)];
        assert_eq!(choose_peer(&peers, None), Some(&peers[0]));
    }
}
