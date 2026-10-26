//! One encrypted QUIC connection between two CursorJump machines.
//!
//! It carries unreliable datagrams (motion, pings), one long-lived reliable
//! control stream (opened by the connector and announced with `Hello`), and a
//! fresh unidirectional stream per clipboard item or dragged item, each
//! starting with a tag byte that says which. The control stream is sent
//! first, clipboard items next, and file data only when nothing else is
//! waiting.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use cj_core::clipboard::ClipboardData;
use cj_core::dnd::FilesHeader;
use cj_core::proto::{
    Control, Datagram, PROTOCOL_VERSION, decode_control_body, decode_datagram, encode_control,
    encode_datagram,
};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    Connection, Endpoint, ReadToEndError, RecvStream, SendStream, TransportConfig, VarInt,
};
use rustls::pki_types::CertificateDer;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::files::{
    FILES_PRIORITY, FilesIn, FilesOut, STOP_INVALID, STREAM_TAG_CLIPBOARD, STREAM_TAG_FILES,
};
use crate::identity::{
    Fingerprint, Identity, SERVER_NAME, pair_commit, pairing_code, random_nonce,
};
use crate::verify::{Trust, client_config, server_config};

const IDLE_TIMEOUT: Duration = Duration::from_millis(600);
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_millis(100);
const DATAGRAM_BUFFER_BYTES: usize = 64 * 1024;
/// Upper bound for QUIC handshake plus Hello exchange.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// Upper bound for the whole `pair()` exchange.
const PAIR_TIMEOUT: Duration = Duration::from_secs(3);
/// Largest control message body accepted from the peer.
const MAX_CONTROL_BODY: usize = 1024 * 1024;
/// Inbound handshakes (QUIC + Hello) driven at once; more are refused.
const MAX_PENDING_HANDSHAKES: usize = 8;
/// Good sessions waiting for `Listener::accept`; more are dropped (closed).
const ACCEPT_QUEUE: usize = 8;
/// Only the control stream may be bidirectional.
const MAX_BIDI_STREAMS: u32 = 1;
/// Clipboard items and files streams in flight at once.
const MAX_UNI_STREAMS: u32 = 16;
/// Connection-wide flow-control window (per-stream window stays at quinn's default).
const CONNECTION_RECEIVE_WINDOW: u32 = 48 * 1024 * 1024;
/// Postcard framing (option tags, length varints) on top of `ClipboardData::byte_len`.
const CLIPBOARD_FRAMING_ALLOWANCE: u64 = 64;
/// Ahead of clipboard items (0) and files streams (`FILES_PRIORITY`).
const CONTROL_PRIORITY: i32 = 100;

/// Application close / stream stop codes.
const CLOSE_BYE: u32 = 0;
const CLOSE_PROTOCOL: u32 = 1;
const CLOSE_PAIRING: u32 = 2;
const STOP_TOO_LARGE: u32 = 1;
// 2 and 3 are `files::STOP_CANCELLED` and `files::STOP_INVALID`.

fn transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(
        IDLE_TIMEOUT
            .try_into()
            .expect("600 ms is a valid idle timeout"),
    ))
    .keep_alive_interval(Some(KEEP_ALIVE_INTERVAL))
    .datagram_receive_buffer_size(Some(DATAGRAM_BUFFER_BYTES))
    .datagram_send_buffer_size(DATAGRAM_BUFFER_BYTES)
    .max_concurrent_bidi_streams(VarInt::from_u32(MAX_BIDI_STREAMS))
    .max_concurrent_uni_streams(VarInt::from_u32(MAX_UNI_STREAMS))
    .receive_window(VarInt::from_u32(CONNECTION_RECEIVE_WINDOW));
    Arc::new(t)
}

fn quinn_server_config(id: &Identity, trust: Trust) -> Result<quinn::ServerConfig> {
    let crypto = QuicServerConfig::try_from(server_config(id, trust)?)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn quinn_client_config(id: &Identity, trust: Trust) -> Result<quinn::ClientConfig> {
    let crypto = QuicClientConfig::try_from(client_config(id, trust)?)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn hello(name: &str) -> Control {
    Control::Hello {
        version: PROTOCOL_VERSION,
        name: name.to_string(),
    }
}

/// Checks the peer's Hello and returns its display name; closes `conn` otherwise.
fn expect_hello(conn: &Connection, msg: Control) -> Result<String> {
    match msg {
        Control::Hello { version, name } if version == PROTOCOL_VERSION => Ok(name),
        Control::Hello { version, .. } => {
            conn.close(
                VarInt::from_u32(CLOSE_PROTOCOL),
                b"protocol version mismatch",
            );
            bail!("peer speaks protocol v{version}, we speak v{PROTOCOL_VERSION}")
        }
        other => {
            conn.close(VarInt::from_u32(CLOSE_PROTOCOL), b"expected Hello");
            bail!("expected Hello, got {other:?}")
        }
    }
}

/// SHA-256 of the peer's end-entity certificate.
fn peer_fingerprint(conn: &Connection) -> Result<Fingerprint> {
    let certs = conn
        .peer_identity()
        .context("peer presented no certificate")?
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow!("unexpected peer identity type"))?;
    let leaf = certs.first().context("peer presented an empty chain")?;
    Ok(Fingerprint::of_cert(leaf))
}

fn copy_identity(id: &Identity) -> Identity {
    Identity {
        cert_der: id.cert_der.clone(),
        key_der: id.key_der.clone(),
    }
}

/// Reads length-prefixed control frames. Buffers partial frames across calls,
/// so a cancelled `next()` never loses or desynchronizes data.
struct ControlReader {
    rx: RecvStream,
    buf: Vec<u8>,
}

impl ControlReader {
    fn new(rx: RecvStream) -> Self {
        Self {
            rx,
            buf: Vec::new(),
        }
    }

    async fn next(&mut self) -> Result<Control> {
        loop {
            if let Some(c) = self.take_frame()? {
                return Ok(c);
            }
            // read_chunk is cancel-safe: data is only consumed when it is returned.
            match self.rx.read_chunk(usize::MAX, true).await? {
                Some(chunk) => self.buf.extend_from_slice(&chunk.bytes),
                None => bail!("control stream finished by peer"),
            }
        }
    }

    fn take_frame(&mut self) -> Result<Option<Control>> {
        let Some(prefix) = self.buf.get(..4) else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(prefix.try_into().expect("4 bytes")) as usize;
        if len > MAX_CONTROL_BODY {
            bail!("control message of {len} bytes exceeds the {MAX_CONTROL_BODY}-byte limit");
        }
        let Some(body) = self.buf.get(4..4 + len) else {
            return Ok(None);
        };
        let c = decode_control_body(body)?;
        self.buf.drain(..4 + len);
        Ok(Some(c))
    }
}

/// Accepts inbound sessions on one UDP port.
///
/// A background task drives up to `MAX_PENDING_HANDSHAKES` inbound handshakes
/// concurrently, so a stalled or hostile peer cannot hold up anyone else. It
/// stops when the `Listener` is dropped.
pub struct Listener {
    endpoint: Endpoint,
    identity: Identity,
    local_addr: SocketAddr,
    sessions: Mutex<mpsc::Receiver<Session>>,
}

impl Listener {
    /// Binds `addr`; `name` is this machine's display name sent in Hello.
    pub async fn bind(
        id: &Identity,
        trust: Trust,
        addr: SocketAddr,
        name: &str,
    ) -> Result<Listener> {
        let endpoint = Endpoint::server(quinn_server_config(id, trust)?, addr)
            .with_context(|| format!("binding QUIC listener on {addr}"))?;
        let local_addr = endpoint.local_addr()?;
        let (tx, rx) = mpsc::channel(ACCEPT_QUEUE);
        tokio::spawn(run_acceptor(
            endpoint.clone(),
            id.fingerprint(),
            Arc::from(name),
            tx,
        ));
        Ok(Listener {
            endpoint,
            identity: copy_identity(id),
            local_addr,
            sessions: Mutex::new(rx),
        })
    }

    /// Returns the next peer that completes the TLS handshake and Hello exchange.
    /// Peers that fail either (strangers, bad versions, stalls) are dropped
    /// silently. Cancel-safe.
    pub async fn accept(&self) -> Result<Session> {
        self.sessions
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| anyhow!("listener endpoint closed"))
    }

    /// Replaces the trust policy for future handshakes (e.g. pin after pairing).
    /// Established sessions are unaffected.
    pub fn set_trust(&self, trust: Trust) -> Result<()> {
        self.endpoint
            .set_server_config(Some(quinn_server_config(&self.identity, trust)?));
        Ok(())
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

/// Feeds good sessions into `sessions` until the endpoint closes or the
/// `Listener` (the receiver) is dropped.
async fn run_acceptor(
    endpoint: Endpoint,
    local: Fingerprint,
    name: Arc<str>,
    sessions: mpsc::Sender<Session>,
) {
    let mut pending: JoinSet<Result<Session>> = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                if !incoming.remote_address_validated() {
                    // Stateless retry: the peer must prove its address before
                    // it can occupy a handshake slot.
                    if let Err(e) = incoming.retry() {
                        e.into_incoming().ignore();
                    }
                } else if pending.len() >= MAX_PENDING_HANDSHAKES {
                    incoming.refuse();
                } else {
                    let (endpoint, name) = (endpoint.clone(), name.clone());
                    pending.spawn(async move {
                        timeout(HANDSHAKE_TIMEOUT, listener_handshake(incoming, local, &name, endpoint))
                            .await
                            .map_err(|_| anyhow!("handshake timed out"))?
                    });
                }
            }
            Some(done) = pending.join_next() => {
                if let Ok(Ok(session)) = done {
                    // Queue full means nobody is accepting; dropping closes the session.
                    let _ = sessions.try_send(session);
                }
            }
            _ = sessions.closed() => break,
        }
    }
    // No acceptor any more: refuse new connections instead of leaving them hanging.
    endpoint.set_server_config(None);
}

async fn listener_handshake(
    incoming: quinn::Incoming,
    local: Fingerprint,
    name: &str,
    endpoint: Endpoint,
) -> Result<Session> {
    let conn = incoming.await?;
    let (mut tx, rx) = conn.accept_bi().await?;
    let mut reader = ControlReader::new(rx);
    let peer_name = expect_hello(&conn, reader.next().await?)?;
    tx.write_all(&encode_control(&hello(name))).await?;
    Session::new(conn, tx, reader, local, peer_name, false, endpoint)
}

/// Connects to a listener at `addr`; `name` is this machine's display name sent in Hello.
pub async fn connect(id: &Identity, trust: Trust, addr: SocketAddr, name: &str) -> Result<Session> {
    let bind: SocketAddr = if addr.is_ipv6() {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let endpoint = Endpoint::client(bind).context("binding QUIC client endpoint")?;
    let connecting = endpoint.connect_with(quinn_client_config(id, trust)?, addr, SERVER_NAME)?;
    let handshake = async {
        let conn = connecting.await?;
        let (mut tx, rx) = conn.open_bi().await?;
        tx.write_all(&encode_control(&hello(name))).await?;
        let mut reader = ControlReader::new(rx);
        let peer_name = expect_hello(&conn, reader.next().await?)?;
        Session::new(
            conn,
            tx,
            reader,
            id.fingerprint(),
            peer_name,
            true,
            endpoint.clone(),
        )
    };
    timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| anyhow!("handshake with {addr} timed out"))?
        .with_context(|| format!("connecting to {addr}"))
}

struct Inner {
    conn: Connection,
    control_tx: Mutex<SendStream>,
    control_rx: Mutex<ControlReader>,
    local: Fingerprint,
    peer: Fingerprint,
    peer_name: String,
    is_connector: bool,
    /// Keeps the UDP endpoint alive as long as any handle to the session exists.
    _endpoint: Endpoint,
}

/// A unidirectional stream from the peer, from `Session::accept_stream`.
#[derive(Debug)]
pub enum IncomingStream {
    Clipboard(ClipboardIn),
    Files(FilesIn),
}

/// A clipboard item whose body is still to be read.
#[derive(Debug)]
pub struct ClipboardIn(RecvStream);

impl ClipboardIn {
    /// Reads the item. `max_bytes` is a content limit (as in
    /// `clipboard::within_limit`); the wire encoding may exceed it by
    /// 64 bytes of postcard framing. A larger item is refused (its stream
    /// is stopped) and returns Err.
    pub async fn read(mut self, max_bytes: u64) -> Result<ClipboardData> {
        let wire_limit = max_bytes.saturating_add(CLIPBOARD_FRAMING_ALLOWANCE);
        let limit = usize::try_from(wire_limit).unwrap_or(usize::MAX);
        match self.0.read_to_end(limit).await {
            Ok(body) => Ok(postcard::from_bytes(&body)?),
            Err(ReadToEndError::TooLong) => {
                let _ = self.0.stop(VarInt::from_u32(STOP_TOO_LARGE));
                bail!("clipboard item exceeds the {max_bytes}-byte limit")
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// An established, authenticated connection. Cheap to clone.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

impl Session {
    fn new(
        conn: Connection,
        control_tx: SendStream,
        control_rx: ControlReader,
        local: Fingerprint,
        peer_name: String,
        is_connector: bool,
        endpoint: Endpoint,
    ) -> Result<Session> {
        let peer = peer_fingerprint(&conn)?;
        control_tx.set_priority(CONTROL_PRIORITY)?;
        Ok(Session {
            inner: Arc::new(Inner {
                conn,
                control_tx: Mutex::new(control_tx),
                control_rx: Mutex::new(control_rx),
                local,
                peer,
                peer_name,
                is_connector,
                _endpoint: endpoint,
            }),
        })
    }

    /// Fingerprint of the peer's certificate.
    pub fn peer(&self) -> Fingerprint {
        self.inner.peer
    }

    /// The display name the peer sent in its Hello.
    pub fn peer_name(&self) -> String {
        self.inner.peer_name.clone()
    }

    /// True on the side that called `connect`, false on the listener side.
    pub fn is_connector(&self) -> bool {
        self.inner.is_connector
    }

    /// Sends one unreliable datagram immediately.
    pub fn send_datagram(&self, d: &Datagram) -> Result<()> {
        self.inner
            .conn
            .send_datagram(Bytes::from(encode_datagram(d)))?;
        Ok(())
    }

    /// Next datagram from the peer. Datagrams that fail to decode are skipped.
    /// Cancel-safe. An error means the connection is gone.
    pub async fn recv_datagram(&self) -> Result<Datagram> {
        loop {
            let b = self.inner.conn.read_datagram().await?;
            if let Ok(d) = decode_datagram(&b) {
                return Ok(d);
            }
        }
    }

    /// Sends one message on the control stream. Not cancel-safe: a cancelled
    /// call may leave a partial frame behind.
    pub async fn send_control(&self, c: &Control) -> Result<()> {
        let mut tx = self.inner.control_tx.lock().await;
        tx.write_all(&encode_control(c)).await?;
        Ok(())
    }

    /// Next control message from the peer. Cancel-safe.
    ///
    /// Any error is fatal: on a framing error, an undecodable message, a
    /// repeated Hello (the initial one is consumed by the handshake) or a lost
    /// stream, the connection is closed and the session is dead.
    pub async fn recv_control(&self) -> Result<Control> {
        let mut rx = self.inner.control_rx.lock().await;
        let result = match rx.next().await {
            Ok(Control::Hello { .. }) => Err(anyhow!("unexpected Hello after handshake")),
            other => other,
        };
        if result.is_err() {
            self.inner
                .conn
                .close(VarInt::from_u32(CLOSE_PROTOCOL), b"control stream error");
        }
        result
    }

    /// Sends one clipboard item on a fresh unidirectional stream.
    pub async fn send_clipboard(&self, d: &ClipboardData) -> Result<()> {
        let mut wire = vec![STREAM_TAG_CLIPBOARD];
        wire.extend(postcard::to_allocvec(d)?);
        let mut s = self.inner.conn.open_uni().await?;
        s.write_all(&wire).await?;
        s.finish()?;
        Ok(())
    }

    /// Opens a files stream for one dragged item and sends its header.
    pub async fn open_files(&self, h: &FilesHeader) -> Result<FilesOut> {
        let s = self.inner.conn.open_uni().await?;
        s.set_priority(FILES_PRIORITY)?;
        FilesOut::start(s, h).await
    }

    /// Next unidirectional stream from the peer, by its tag; a files
    /// stream's header is read before it is returned.
    ///
    /// Single consumer: the app's one stream acceptor calls this, since a
    /// second caller would take some of the streams. Not cancel-safe: a
    /// cancelled call may drop a stream. A stream with an unknown tag or a
    /// bad header is stopped (`STOP_INVALID`) and returns Err, but unlike
    /// the other `recv_*` errors the session stays up.
    pub async fn accept_stream(&self) -> Result<IncomingStream> {
        let mut s = self.inner.conn.accept_uni().await?;
        let mut tag = [0u8];
        if let Err(e) = s.read_exact(&mut tag).await {
            let _ = s.stop(VarInt::from_u32(STOP_INVALID));
            bail!("stream without a tag: {e}");
        }
        match tag[0] {
            STREAM_TAG_CLIPBOARD => Ok(IncomingStream::Clipboard(ClipboardIn(s))),
            STREAM_TAG_FILES => Ok(IncomingStream::Files(FilesIn::accept(s).await?)),
            other => {
                let _ = s.stop(VarInt::from_u32(STOP_INVALID));
                bail!("stream with unknown tag {other}")
            }
        }
    }

    /// Runs the commit-reveal pairing exchange and returns the 6-digit code
    /// both sides must show. The control stream must be otherwise idle.
    /// Gives up after `PAIR_TIMEOUT`, closing the connection.
    pub async fn pair(&self) -> Result<String> {
        match timeout(PAIR_TIMEOUT, self.pair_exchange()).await {
            Ok(result) => result,
            Err(_) => {
                self.inner
                    .conn
                    .close(VarInt::from_u32(CLOSE_PAIRING), b"pairing timed out");
                bail!("pairing timed out after {PAIR_TIMEOUT:?}")
            }
        }
    }

    async fn pair_exchange(&self) -> Result<String> {
        let me = &self.inner.local;
        let peer = &self.inner.peer;
        if self.inner.is_connector {
            let n_c = random_nonce();
            self.send_control(&Control::PairCommit {
                commit: pair_commit(&n_c),
            })
            .await?;
            let n_l = match self.recv_control().await? {
                Control::PairNonce { nonce } => nonce,
                other => bail!("pairing: expected PairNonce, got {other:?}"),
            };
            self.send_control(&Control::PairReveal { nonce: n_c })
                .await?;
            Ok(pairing_code(me, peer, &n_c, &n_l))
        } else {
            let commit = match self.recv_control().await? {
                Control::PairCommit { commit } => commit,
                other => bail!("pairing: expected PairCommit, got {other:?}"),
            };
            let n_l = random_nonce();
            self.send_control(&Control::PairNonce { nonce: n_l })
                .await?;
            let n_c = match self.recv_control().await? {
                Control::PairReveal { nonce } => nonce,
                other => bail!("pairing: expected PairReveal, got {other:?}"),
            };
            if pair_commit(&n_c) != commit {
                self.inner.conn.close(
                    VarInt::from_u32(CLOSE_PAIRING),
                    b"pairing commitment mismatch",
                );
                bail!("pairing: peer's reveal does not match its commitment");
            }
            Ok(pairing_code(peer, me, &n_c, &n_l))
        }
    }

    /// Resolves once the connection is closed for any reason, including idle timeout.
    pub async fn closed(&self) {
        self.inner.conn.closed().await;
    }

    /// Closes the connection gracefully.
    pub fn close(&self) {
        self.inner.conn.close(VarInt::from_u32(CLOSE_BYE), b"bye");
    }
}
