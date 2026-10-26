//! End-to-end tests of `cj_net::session` over real QUIC on 127.0.0.1.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cj_core::clipboard::{ClipboardData, within_limit};
use cj_core::dnd::{FileEntry, FilesHeader, ItemKind};
use cj_core::proto::{
    Control, Datagram, MouseButton, PROTOCOL_VERSION, decode_control_body, encode_control,
    encode_datagram,
};
use cj_net::files::{FilesIn, MAX_FRAME, STOP_CANCELLED, STOP_INVALID, stop_code};
use cj_net::session::{IncomingStream, Listener, Session, connect};
use cj_net::{ALPN, CjServerCertVerifier, Identity, SERVER_NAME, Trust, client_config};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use tokio::time::timeout;

/// Generous upper bound for anything that is expected to finish.
const T: Duration = Duration::from_secs(10);
/// How long we wait to be confident that something does NOT happen.
const QUIET: Duration = Duration::from_millis(1500);
const MB: usize = 1_000_000;

fn ident() -> Identity {
    let dir = tempfile::tempdir().unwrap();
    Identity::load_or_create(dir.path()).unwrap()
}

fn localhost_any_port() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 0).into()
}

/// Binds a listener for `server` and connects `client` to it.
async fn pair_up(
    server: &Identity,
    server_trust: Trust,
    client: &Identity,
    client_trust: Trust,
) -> (Listener, Session, Session) {
    let listener = Listener::bind(server, server_trust, localhost_any_port(), "Server")
        .await
        .unwrap();
    let (s, c) = timeout(T, async {
        tokio::join!(
            listener.accept(),
            connect(client, client_trust, listener.local_addr(), "Client")
        )
    })
    .await
    .unwrap();
    (listener, s.unwrap(), c.unwrap())
}

/// A hand-driven connector: the wire protocol spoken without `cj_net::session`.
struct RawPeer {
    _ep: quinn::Endpoint,
    conn: quinn::Connection,
    tx: quinn::SendStream,
    rx: quinn::RecvStream,
}

impl RawPeer {
    /// QUIC connect with an arbitrary rustls config and open the control stream.
    async fn connect(tls: rustls::ClientConfig, addr: SocketAddr) -> anyhow::Result<RawPeer> {
        let cfg = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
        ));
        let ep = quinn::Endpoint::client((Ipv4Addr::UNSPECIFIED, 0).into())?;
        let conn = ep.connect_with(cfg, addr, SERVER_NAME)?.await?;
        let (tx, rx) = conn.open_bi().await?;
        Ok(RawPeer {
            _ep: ep,
            conn,
            tx,
            rx,
        })
    }

    async fn send(&mut self, c: &Control) -> anyhow::Result<()> {
        self.tx.write_all(&encode_control(c)).await?;
        Ok(())
    }

    async fn recv(&mut self) -> anyhow::Result<Control> {
        let mut len = [0u8; 4];
        self.rx.read_exact(&mut len).await?;
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        self.rx.read_exact(&mut body).await?;
        Ok(decode_control_body(&body)?)
    }

    /// Sends Hello{version} and returns the listener's reply.
    async fn hello(&mut self, version: u16) -> anyhow::Result<Control> {
        self.send(&Control::Hello {
            version,
            name: "raw".into(),
        })
        .await?;
        self.recv().await
    }
}

/// Connects by hand, sends Hello{version}, returns the reply.
async fn raw_hello(
    tls: rustls::ClientConfig,
    addr: SocketAddr,
    version: u16,
) -> anyhow::Result<Control> {
    RawPeer::connect(tls, addr).await?.hello(version).await
}

/// Client cert resolver that always presents one fixed (cert, key), unchecked.
#[derive(Debug)]
struct FixedClientCert(Arc<CertifiedKey>);

impl rustls::client::ResolvesClientCert for FixedClientCert {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[rustls::SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// Client config presenting `cert_of`'s certificate but signing with `key_of`'s key.
fn swapped_key_client_config(cert_of: &Identity, key_of: &Identity) -> rustls::ClientConfig {
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_of.key_der.clone()));
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
    // CertifiedKey::new does not check that the key matches the certificate.
    let certified = CertifiedKey::new(vec![CertificateDer::from(cert_of.cert_der.clone())], signer);
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(CjServerCertVerifier::new(Trust::PairingMode)))
    .with_client_cert_resolver(Arc::new(FixedClientCert(Arc::new(certified))));
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_mode_connects_and_codes_match() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    assert!(client.is_connector());
    assert!(!server.is_connector());
    assert_eq!(client.peer(), a.fingerprint());
    assert_eq!(server.peer(), b.fingerprint());
    assert_eq!(client.peer_name(), "Server");
    assert_eq!(server.peer_name(), "Client");

    let (sc, cc) = timeout(T, async { tokio::join!(server.pair(), client.pair()) })
        .await
        .unwrap();
    let (sc, cc) = (sc.unwrap(), cc.unwrap());
    assert_eq!(sc, cc);
    assert_eq!(sc.len(), 7, "code {sc:?} should look like \"NNN NNN\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_rejects_reveal_that_breaks_commitment() {
    let (a, b) = (ident(), ident());
    let listener = Listener::bind(&a, Trust::PairingMode, localhost_any_port(), "A")
        .await
        .unwrap();
    let addr = listener.local_addr();

    let cheat = async {
        let mut raw =
            RawPeer::connect(client_config(&b, Trust::PairingMode).unwrap(), addr).await?;
        raw.hello(PROTOCOL_VERSION).await?;
        let committed = cj_net::random_nonce();
        raw.send(&Control::PairCommit {
            commit: cj_net::pair_commit(&committed),
        })
        .await?;
        assert!(matches!(raw.recv().await?, Control::PairNonce { .. }));
        // Reveal a different nonce than the one committed to.
        raw.send(&Control::PairReveal {
            nonce: cj_net::random_nonce(),
        })
        .await?;
        anyhow::Ok(raw)
    };
    let listener_side = async {
        let s = listener.accept().await.unwrap();
        (s.pair().await, s)
    };
    let (raw, (code, server)) = timeout(T, async { tokio::join!(cheat, listener_side) })
        .await
        .unwrap();
    let raw = raw.unwrap();
    assert!(code.is_err(), "a broken commitment must not yield a code");
    timeout(T, server.closed())
        .await
        .expect("listener closes the session on a broken commitment");
    timeout(T, raw.conn.closed()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_accepts_known_new_addr() {
    let (a, b) = (ident(), ident());
    let listener = Listener::bind(
        &a,
        Trust::Pinned(b.fingerprint()),
        localhost_any_port(),
        "A",
    )
    .await
    .unwrap();
    // Every connect() binds a fresh client endpoint, i.e. a new source port.
    for _ in 0..2 {
        let (s, c) = timeout(T, async {
            tokio::join!(
                listener.accept(),
                connect(
                    &b,
                    Trust::Pinned(a.fingerprint()),
                    listener.local_addr(),
                    "B"
                )
            )
        })
        .await
        .unwrap();
        let (s, c) = (s.unwrap(), c.unwrap());
        assert_eq!(s.peer(), b.fingerprint());
        assert_eq!(c.peer(), a.fingerprint());
        c.close();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_rejects_stranger() {
    let (a, b, c) = (ident(), ident(), ident());
    let listener = Listener::bind(
        &a,
        Trust::Pinned(b.fingerprint()),
        localhost_any_port(),
        "A",
    )
    .await
    .unwrap();

    let (accepted, stranger) = tokio::join!(
        timeout(QUIET, listener.accept()),
        timeout(
            T,
            connect(&c, Trust::PairingMode, listener.local_addr(), "C")
        )
    );
    assert!(
        stranger
            .expect("stranger connect should fail, not hang")
            .is_err(),
        "stranger must not get a session"
    );
    assert!(accepted.is_err(), "accept must not surface the stranger");

    // The listener is still healthy for the pinned peer.
    let (s, k) = timeout(T, async {
        tokio::join!(
            listener.accept(),
            connect(&b, Trust::PairingMode, listener.local_addr(), "B")
        )
    })
    .await
    .unwrap();
    assert_eq!(s.unwrap().peer(), b.fingerprint());
    k.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_rejects_key_swap() {
    let (a, b, c) = (ident(), ident(), ident());
    let listener = Listener::bind(
        &a,
        Trust::Pinned(b.fingerprint()),
        localhost_any_port(),
        "A",
    )
    .await
    .unwrap();
    let addr = listener.local_addr();

    // Control: the raw harness with B's genuine cert and key gets in.
    let (accepted, reply) = timeout(T, async {
        tokio::join!(
            listener.accept(),
            raw_hello(swapped_key_client_config(&b, &b), addr, PROTOCOL_VERSION)
        )
    })
    .await
    .unwrap();
    assert_eq!(accepted.unwrap().peer(), b.fingerprint());
    assert!(matches!(reply.unwrap(), Control::Hello { .. }));

    // B's pinned cert with C's private key must be rejected.
    let (accepted, reply) = tokio::join!(
        timeout(QUIET, listener.accept()),
        timeout(
            T,
            raw_hello(swapped_key_client_config(&b, &c), addr, PROTOCOL_VERSION)
        )
    );
    assert!(
        reply
            .expect("key-swap connect should fail, not hang")
            .is_err(),
        "key swap must not complete the Hello exchange"
    );
    assert!(accepted.is_err(), "accept must not surface the impostor");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_trust_applies_to_new_handshakes() {
    let (a, b, c) = (ident(), ident(), ident());
    let listener = Listener::bind(&a, Trust::PairingMode, localhost_any_port(), "A")
        .await
        .unwrap();
    listener.set_trust(Trust::Pinned(b.fingerprint())).unwrap();

    let (accepted, stranger) = tokio::join!(
        timeout(QUIET, listener.accept()),
        timeout(
            T,
            connect(&c, Trust::PairingMode, listener.local_addr(), "C")
        )
    );
    assert!(stranger.expect("should fail, not hang").is_err());
    assert!(accepted.is_err());

    let (s, k) = timeout(T, async {
        tokio::join!(
            listener.accept(),
            connect(&b, Trust::PairingMode, listener.local_addr(), "B")
        )
    })
    .await
    .unwrap();
    assert_eq!(s.unwrap().peer(), b.fingerprint());
    k.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_control_frame_is_error() {
    let (a, b) = (ident(), ident());
    let listener = Listener::bind(&a, Trust::PairingMode, localhost_any_port(), "A")
        .await
        .unwrap();
    let addr = listener.local_addr();
    let (accepted, raw) = timeout(T, async {
        tokio::join!(listener.accept(), async {
            let mut raw =
                RawPeer::connect(client_config(&b, Trust::PairingMode).unwrap(), addr).await?;
            raw.hello(PROTOCOL_VERSION).await?;
            anyhow::Ok(raw)
        })
    })
    .await
    .unwrap();
    let (server, mut raw) = (accepted.unwrap(), raw.unwrap());

    // Announce a body one byte over the 1 MiB cap; the reader must refuse it
    // from the prefix alone instead of waiting for (or buffering) the body.
    let len = (1024 * 1024 + 1) as u32;
    raw.tx.write_all(&len.to_le_bytes()).await.unwrap();
    assert!(timeout(T, server.recv_control()).await.unwrap().is_err());
    // A control-stream error is fatal: the session is closed.
    timeout(T, server.closed())
        .await
        .expect("recv_control error closes the session");
    timeout(T, raw.conn.closed()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_from_a_pre_mac_ready_peer_is_rejected() {
    // Protocol v1 peers predate Control::MacReady: a v1 PC would drop the
    // session on the Mac's first MacReady, so it is refused at Hello.
    const { assert!(PROTOCOL_VERSION >= 2) };
    let (a, b) = (ident(), ident());
    let listener = Listener::bind(&a, Trust::PairingMode, localhost_any_port(), "A")
        .await
        .unwrap();
    let addr = listener.local_addr();
    let (accepted, reply) = tokio::join!(
        timeout(QUIET, listener.accept()),
        timeout(
            T,
            raw_hello(client_config(&b, Trust::PairingMode).unwrap(), addr, 1)
        )
    );
    assert!(reply.expect("should fail, not hang").is_err());
    assert!(accepted.is_err(), "accept must not surface a v1 Hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_version_mismatch_is_rejected() {
    let (a, b) = (ident(), ident());
    let listener = Listener::bind(&a, Trust::PairingMode, localhost_any_port(), "A")
        .await
        .unwrap();
    let addr = listener.local_addr();

    let (accepted, reply) = tokio::join!(
        timeout(QUIET, listener.accept()),
        timeout(
            T,
            raw_hello(
                client_config(&b, Trust::PairingMode).unwrap(),
                addr,
                PROTOCOL_VERSION + 1
            )
        )
    );
    assert!(reply.expect("should fail, not hang").is_err());
    assert!(accepted.is_err(), "accept must not surface a bad Hello");

    let (accepted, reply) = timeout(T, async {
        tokio::join!(
            listener.accept(),
            raw_hello(
                client_config(&b, Trust::PairingMode).unwrap(),
                addr,
                PROTOCOL_VERSION
            )
        )
    })
    .await
    .unwrap();
    accepted.unwrap();
    assert_eq!(
        reply.unwrap(),
        Control::Hello {
            version: PROTOCOL_VERSION,
            name: "A".into()
        }
    );
}

/// Re-sends `d` from `from` until `to` receives a datagram (datagrams are unreliable).
async fn deliver_datagram(from: &Session, to: &Session, d: &Datagram) -> Datagram {
    timeout(T, async {
        tokio::select! {
            got = to.recv_datagram() => got.unwrap(),
            _ = async {
                loop {
                    from.send_datagram(d).unwrap();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            } => unreachable!(),
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datagram_and_control_flow() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    let motion = Datagram::Motion { dx: -300, dy: 12 };
    assert_eq!(deliver_datagram(&client, &server, &motion).await, motion);
    let pong = Datagram::Pong {
        seq: 9,
        sent_us: 123_456,
    };
    assert_eq!(deliver_datagram(&server, &client, &pong).await, pong);

    let sent = [
        Control::Key {
            mac_keycode: 0x7E,
            down: true,
        },
        Control::Key {
            mac_keycode: 0x7E,
            down: false,
        },
        Control::ReleaseAll,
    ];
    for c in &sent {
        client.send_control(c).await.unwrap();
    }
    for c in &sent {
        assert_eq!(
            &timeout(T, server.recv_control()).await.unwrap().unwrap(),
            c
        );
    }

    let back = Control::Button {
        button: MouseButton::Left,
        down: true,
    };
    server.send_control(&back).await.unwrap();
    assert_eq!(
        timeout(T, client.recv_control()).await.unwrap().unwrap(),
        back
    );
}

/// The next stream from the peer, which must be a clipboard item.
async fn recv_clipboard(s: &Session, max_bytes: u64) -> anyhow::Result<ClipboardData> {
    match s.accept_stream().await? {
        IncomingStream::Clipboard(c) => c.read(max_bytes).await,
        IncomingStream::Files(f) => anyhow::bail!("expected a clipboard item, got {f:?}"),
    }
}

/// The next stream from the peer, which must be a files stream.
async fn recv_files(s: &Session) -> anyhow::Result<FilesIn> {
    match timeout(T, s.accept_stream()).await?? {
        IncomingStream::Files(f) => Ok(f),
        IncomingStream::Clipboard(_) => {
            anyhow::bail!("expected a files stream, got a clipboard item")
        }
    }
}

/// `n` deterministic pseudo-random bytes (xorshift64).
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(n + 8);
    while out.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(n);
    out
}

fn header(kind: ItemKind, total_bytes: u64, entries: u64) -> FilesHeader {
    FilesHeader {
        drag: 7,
        item: 0,
        name: "item".into(),
        kind,
        total_bytes,
        entries,
    }
}

fn entry(path: &[&str], kind: ItemKind, size: u64) -> FileEntry {
    FileEntry {
        path: path.iter().map(|c| c.to_string()).collect(),
        kind,
        size,
    }
}

/// Reads the rest of the current entry; returns its SHA-256 and length.
async fn hash_entry(f: &mut FilesIn) -> anyhow::Result<([u8; 32], u64)> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut len = 0;
    loop {
        let n = f.read_chunk(&mut buf).await?;
        if n == 0 {
            return Ok((h.finalize().into(), len));
        }
        h.update(&buf[..n]);
        len += n as u64;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_transfer_and_limit() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    // Items exactly at the sender's `within_limit` must pass the receiver's
    // limit despite the postcard framing overhead.
    let big_text = ClipboardData {
        text: Some("x".repeat(MB)),
        ..Default::default()
    };
    let quarter = MB / 4;
    let all_formats = ClipboardData {
        text: Some("t".repeat(quarter)),
        html: Some("h".repeat(quarter)),
        rtf: Some(vec![1u8; quarter]),
        png: Some(vec![2u8; quarter]),
    };
    for item in [&big_text, &all_formats] {
        assert!(within_limit(item, MB as u64));
        let (sent, got) = timeout(T, async {
            tokio::join!(
                client.send_clipboard(item),
                recv_clipboard(&server, MB as u64)
            )
        })
        .await
        .unwrap();
        sent.unwrap();
        assert_eq!(&got.unwrap(), item);
    }

    let too_big = ClipboardData {
        png: Some(vec![7u8; 2 * MB]),
        ..Default::default()
    };
    let (_sent, got) = timeout(T, async {
        tokio::join!(
            client.send_clipboard(&too_big),
            recv_clipboard(&server, MB as u64)
        )
    })
    .await
    .unwrap();
    assert!(got.is_err(), "a 2 MB item must exceed a 1 MB limit");

    let small = ClipboardData {
        text: Some("hi".into()),
        html: Some("<b>hi</b>".into()),
        ..Default::default()
    };
    let (sent, got) = timeout(T, async {
        tokio::join!(
            client.send_clipboard(&small),
            recv_clipboard(&server, MB as u64)
        )
    })
    .await
    .unwrap();
    sent.unwrap();
    assert_eq!(got.unwrap(), small);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_ends_when_peer_vanishes() {
    let (addr_tx, addr_rx) = tokio::sync::oneshot::channel();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    // The server lives on its own runtime so we can kill it without a close frame.
    let server = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (listener, session) = rt.block_on(async {
            let a = ident();
            let l = Listener::bind(&a, Trust::PairingMode, localhost_any_port(), "A")
                .await
                .unwrap();
            addr_tx.send(l.local_addr()).unwrap();
            let s = l.accept().await.unwrap();
            accepted_tx.send(()).unwrap();
            (l, s)
        });
        stop_rx.recv().unwrap();
        // Never drop the handles (dropping would implicitly close the connection);
        // just stop every task so the endpoint goes silent.
        std::mem::forget(session);
        std::mem::forget(listener);
        rt.shutdown_background();
    });

    let b = ident();
    let addr = timeout(T, addr_rx).await.unwrap().unwrap();
    let client = timeout(T, connect(&b, Trust::PairingMode, addr, "B"))
        .await
        .unwrap()
        .unwrap();
    timeout(T, accepted_rx).await.unwrap().unwrap();

    // Keep-alives hold an otherwise idle session open well past the idle timeout.
    assert!(
        timeout(Duration::from_millis(1000), client.closed())
            .await
            .is_err(),
        "an idle but live session must stay open"
    );

    stop_tx.send(()).unwrap();
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    timeout(Duration::from_secs(1), client.closed())
        .await
        .expect("closed() must resolve within 1 s of the peer vanishing");
}

/// Binds a PairingMode listener and returns a raw peer that has completed Hello.
async fn listener_with_raw_peer(a: &Identity, b: &Identity) -> (Listener, Session, RawPeer) {
    let listener = Listener::bind(a, Trust::PairingMode, localhost_any_port(), "A")
        .await
        .unwrap();
    let addr = listener.local_addr();
    let (accepted, raw) = timeout(T, async {
        tokio::join!(listener.accept(), async {
            let mut raw =
                RawPeer::connect(client_config(b, Trust::PairingMode).unwrap(), addr).await?;
            raw.hello(PROTOCOL_VERSION).await?;
            anyhow::Ok(raw)
        })
    })
    .await
    .unwrap();
    (listener, accepted.unwrap(), raw.unwrap())
}

/// A stranger that completes TLS (PairingMode trusts anyone) but never sends Hello.
async fn stalled_stranger(addr: SocketAddr) -> anyhow::Result<RawPeer> {
    RawPeer::connect(client_config(&ident(), Trust::PairingMode)?, addr).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_strangers_do_not_block_paired_peer() {
    let (a, b) = (ident(), ident());
    let listener = Listener::bind(&a, Trust::PairingMode, localhost_any_port(), "A")
        .await
        .unwrap();
    let addr = listener.local_addr();

    let (accepted, (_stallers, joined)) = tokio::join!(timeout(T, listener.accept()), async {
        let s1 = timeout(T, stalled_stranger(addr)).await.unwrap().unwrap();
        let s2 = timeout(T, stalled_stranger(addr)).await.unwrap().unwrap();
        let started = Instant::now();
        let c = timeout(
            Duration::from_secs(2),
            connect(&b, Trust::PairingMode, addr, "B"),
        )
        .await;
        ((s1, s2), (c, started.elapsed()))
    });
    let (c, took) = joined;
    c.unwrap_or_else(|_| panic!("B must connect within 2 s (gave up after {took:?})"))
        .unwrap();
    assert_eq!(accepted.unwrap().unwrap().peer(), b.fingerprint());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_handshakes_are_capped() {
    let (a, b) = (ident(), ident());
    let listener = Listener::bind(&a, Trust::PairingMode, localhost_any_port(), "A")
        .await
        .unwrap();
    let addr = listener.local_addr();

    let (accepted, ()) = tokio::join!(timeout(T, listener.accept()), async {
        let mut stallers = Vec::new();
        for _ in 0..8 {
            stallers.push(timeout(T, stalled_stranger(addr)).await.unwrap().unwrap());
        }
        let ninth = timeout(T, stalled_stranger(addr)).await.unwrap();
        assert!(ninth.is_err(), "a 9th pending handshake must be refused");
        // B is refused too while all slots are taken; once the stallers' Hello
        // deadline passes, slots free up and B gets in.
        timeout(T, async {
            while connect(&b, Trust::PairingMode, addr, "B").await.is_err() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("B gets in once the stalled handshakes expire");
        drop(stallers);
    });
    assert_eq!(accepted.unwrap().unwrap().peer(), b.fingerprint());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undecodable_datagram_is_skipped() {
    let (a, b) = (ident(), ident());
    let (_l, server, raw) = listener_with_raw_peer(&a, &b).await;

    let motion = Datagram::Motion { dx: 3, dy: -4 };
    raw.conn
        .send_datagram(bytes::Bytes::from_static(&[0xFF, 0xFF, 0xFF]))
        .unwrap();
    raw.conn
        .send_datagram(bytes::Bytes::from(encode_datagram(&motion)))
        .unwrap();
    assert_eq!(
        timeout(T, server.recv_datagram()).await.unwrap().unwrap(),
        motion
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_times_out_and_closes() {
    let (a, b) = (ident(), ident());
    let (_l, server, raw) = listener_with_raw_peer(&a, &b).await;

    // The raw peer stays connected but never sends PairCommit.
    let started = Instant::now();
    let code = timeout(T, server.pair()).await.unwrap();
    assert!(code.is_err(), "pair() must give up without a code");
    assert!(
        started.elapsed() >= Duration::from_millis(2500),
        "pair() gave up after only {:?}",
        started.elapsed()
    );
    timeout(T, server.closed())
        .await
        .expect("a timed-out pairing closes the session");
    timeout(T, raw.conn.closed()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_limits_are_enforced() {
    let (a, b) = (ident(), ident());
    let (_l, _server, raw) = listener_with_raw_peer(&a, &b).await;
    let short = Duration::from_millis(500);

    // The control stream is the only bidirectional stream allowed.
    assert!(
        timeout(short, raw.conn.open_bi()).await.is_err(),
        "a second bi stream must not be granted"
    );

    // At most 16 unaccepted uni streams in flight.
    let mut unis = Vec::new();
    for _ in 0..16 {
        let mut s = timeout(T, raw.conn.open_uni()).await.unwrap().unwrap();
        s.write_all(b"x").await.unwrap();
        unis.push(s);
    }
    assert!(
        timeout(short, raw.conn.open_uni()).await.is_err(),
        "a 17th uni stream must wait for credit"
    );
}

fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    s.local_addr().unwrap().port()
}

#[tokio::test]
async fn broadcast_fallback_finds_responder() {
    use cj_net::discovery::{Advertiser, discover};
    let fp = ident().fingerprint();
    let responder_port = free_udp_port();
    let _adv = Advertiser::start_responder_only("test-mac", 47800, &fp, responder_port).unwrap();
    let peers = discover(Duration::from_secs(3), responder_port, Some(&fp))
        .await
        .unwrap();
    let found = peers
        .iter()
        .find(|p| p.fingerprint == fp)
        .expect("responder found via broadcast fallback");
    assert_eq!(found.addr.port(), 47800);
    assert_eq!(found.name, "test-mac");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_stream_roundtrip_20mb() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    let data = noise(20 * MB, 1);
    let h = header(ItemKind::Dir, data.len() as u64, 3);
    let entries = [
        entry(&[], ItemKind::Dir, 0),
        entry(&["big.bin"], ItemKind::File, data.len() as u64),
        entry(&["empty.txt"], ItemKind::File, 0),
    ];
    let send = async {
        let mut out = client.open_files(&h).await?;
        for e in &entries {
            out.entry(e).await?;
            if e.size > 0 {
                for chunk in data.chunks(256 * 1024) {
                    out.write(chunk).await?;
                }
            }
        }
        out.finish()
    };
    let recv = async {
        let mut f = recv_files(&server).await?;
        assert_eq!(f.header(), &h);
        let mut got = Vec::new();
        while let Some(e) = f.next_entry().await? {
            let (sum, len) = hash_entry(&mut f).await?;
            got.push((e, sum, len));
        }
        assert!(f.next_entry().await?.is_none(), "the end stays the end");
        anyhow::Ok(got)
    };
    let started = Instant::now();
    let (sent, got) = timeout(T, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    eprintln!("20 MB files stream took {:?}", started.elapsed());
    sent.unwrap();
    let empty: [u8; 32] = Sha256::digest(b"").into();
    let [root, big, zero] = entries;
    assert_eq!(
        got.unwrap(),
        [
            (root, empty, 0),
            (big, Sha256::digest(&data).into(), data.len() as u64),
            (zero, empty, 0),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_still_works_alongside_files() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    let data = noise(20 * MB, 2);
    let half = data.len() / 2;
    let clip = ClipboardData {
        png: Some(noise(MB, 3)),
        ..Default::default()
    };
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (clip_tx, clip_rx) = tokio::sync::oneshot::channel();

    // The file stream holds back its second half until the clipboard item
    // has crossed, so the item certainly arrives mid-transfer.
    let send_files = async {
        let len = data.len() as u64;
        let mut out = client.open_files(&header(ItemKind::File, len, 1)).await?;
        out.entry(&entry(&[], ItemKind::File, len)).await?;
        out.write(&data[..half]).await?;
        clip_rx.await?;
        out.write(&data[half..]).await?;
        out.finish()
    };
    let recv_files = async {
        let mut f = recv_files(&server).await?;
        f.next_entry().await?.expect("one entry");
        started_tx.send(()).unwrap();
        let (sum, _) = hash_entry(&mut f).await?;
        assert!(f.next_entry().await?.is_none());
        anyhow::Ok(sum)
    };
    let clipboard = async {
        started_rx.await?;
        let (sent, got) = tokio::join!(
            client.send_clipboard(&clip),
            recv_clipboard(&server, MB as u64)
        );
        sent?;
        clip_tx.send(()).unwrap();
        got
    };
    let started = Instant::now();
    let (sent, sum, got) = timeout(T, async { tokio::join!(send_files, recv_files, clipboard) })
        .await
        .unwrap();
    eprintln!(
        "20 MB files stream with a 1 MB clipboard item took {:?}",
        started.elapsed()
    );
    sent.unwrap();
    assert_eq!(got.unwrap(), clip);
    assert_eq!(sum.unwrap(), <[u8; 32]>::from(Sha256::digest(&data)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_cancel_resets_receiver() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    let (entry_tx, entry_rx) = tokio::sync::oneshot::channel();
    let send = async {
        let mut out = client
            .open_files(&header(ItemKind::File, MB as u64, 1))
            .await?;
        out.entry(&entry(&[], ItemKind::File, MB as u64)).await?;
        out.write(&noise(100_000, 4)).await?;
        entry_rx.await?;
        out.cancel();
        anyhow::Ok(())
    };
    let recv = async {
        let mut f = recv_files(&server).await?;
        f.next_entry().await?.expect("one entry");
        entry_tx.send(()).unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match f.read_chunk(&mut buf).await {
                Ok(0) => anyhow::bail!("a cancelled entry must not look complete"),
                Ok(_) => {}
                Err(e) => return anyhow::Ok(stop_code(&e)),
            }
        }
    };
    let (sent, got) = timeout(T, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    sent.unwrap();
    assert_eq!(got.unwrap(), Some(STOP_CANCELLED));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_files_out_cancels_the_stream() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    // Dropped mid-entry, and dropped between entries: never Ok(0) or None.
    for mid_entry in [true, false] {
        let sent_bytes: &[u8] = if mid_entry { b"ab" } else { b"abcd" };
        let (entry_tx, entry_rx) = tokio::sync::oneshot::channel();
        let send = async {
            let mut out = client.open_files(&header(ItemKind::Dir, 8, 3)).await?;
            out.entry(&entry(&[], ItemKind::Dir, 0)).await?;
            out.entry(&entry(&["a"], ItemKind::File, 4)).await?;
            out.write(sent_bytes).await?;
            entry_rx.await?;
            drop(out);
            anyhow::Ok(())
        };
        let recv = async {
            let mut f = recv_files(&server).await?;
            f.next_entry().await?.expect("root");
            f.next_entry().await?.expect("a");
            // Every byte sent before the drop arrives first, so the drop
            // lands exactly where the test means it to.
            let mut buf = [0u8; 4];
            let mut read = 0;
            while read < sent_bytes.len() {
                read += f.read_chunk(&mut buf[read..]).await?;
            }
            entry_tx.send(()).unwrap();
            let err = if mid_entry {
                match f.read_chunk(&mut buf[read..]).await {
                    Ok(n) => anyhow::bail!("read {n} bytes of a dropped entry"),
                    Err(e) => e,
                }
            } else {
                match f.next_entry().await {
                    Ok(e) => anyhow::bail!("a dropped stream gave {e:?}"),
                    Err(e) => e,
                }
            };
            anyhow::Ok(stop_code(&err))
        };
        let (sent, code) = timeout(T, async { tokio::join!(send, recv) })
            .await
            .unwrap();
        sent.unwrap();
        assert_eq!(code.unwrap(), Some(STOP_CANCELLED), "mid_entry {mid_entry}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_files_in_cancels_the_sender() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    let len = 20 * MB as u64;
    let send = async {
        let mut out = client.open_files(&header(ItemKind::File, len, 1)).await?;
        out.entry(&entry(&[], ItemKind::File, len)).await?;
        let chunk = vec![0u8; 64 * 1024];
        for _ in 0..len / chunk.len() as u64 {
            if let Err(e) = out.write(&chunk).await {
                return anyhow::Ok(stop_code(&e));
            }
        }
        anyhow::bail!("all 20 MB written to a dropped receiver")
    };
    let recv = async {
        drop(recv_files(&server).await?);
        anyhow::Ok(())
    };
    let (code, got) = timeout(T, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    got.unwrap();
    assert_eq!(code.unwrap(), Some(STOP_CANCELLED));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_entry_path_is_refused() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    // 64 components is the deepest allowed; 65 is refused.
    for (depth, ok) in [(64, true), (65, false)] {
        let path = vec!["d"; depth];
        let send = async {
            let mut out = client.open_files(&header(ItemKind::Dir, 0, 2)).await?;
            out.entry(&entry(&[], ItemKind::Dir, 0)).await?;
            out.entry(&entry(&path, ItemKind::Dir, 0)).await?;
            out.finish()
        };
        let recv = async {
            let mut f = recv_files(&server).await?;
            f.next_entry().await?.expect("root");
            let got = f.next_entry().await;
            if ok {
                assert_eq!(got?.unwrap().path.len(), depth);
                assert!(f.next_entry().await?.is_none());
            } else {
                assert!(got.is_err(), "a {depth}-deep path must be refused");
            }
            anyhow::Ok(())
        };
        let (_sent, got) = timeout(T, async { tokio::join!(send, recv) })
            .await
            .unwrap();
        got.unwrap();
    }

    // The session survives the refused stream.
    let small = ClipboardData {
        text: Some("still here".into()),
        ..Default::default()
    };
    let (sent, got) = timeout(T, async {
        tokio::join!(
            client.send_clipboard(&small),
            recv_clipboard(&server, MB as u64)
        )
    })
    .await
    .unwrap();
    sent.unwrap();
    assert_eq!(got.unwrap(), small);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_bytes_over_header_total_are_refused() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    // One entry larger than the whole item.
    let send = async {
        let mut out = client.open_files(&header(ItemKind::File, 10, 1)).await?;
        out.entry(&entry(&[], ItemKind::File, 11)).await?;
        out.write(&[0; 11]).await?;
        out.finish()
    };
    let recv = async {
        let mut f = recv_files(&server).await?;
        anyhow::Ok(f.next_entry().await.is_err())
    };
    let (_sent, refused) = timeout(T, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    assert!(refused.unwrap(), "an 11-byte entry in a 10-byte item");

    // Entries that only add up to more than the whole item.
    let send = async {
        let mut out = client.open_files(&header(ItemKind::Dir, 10, 3)).await?;
        out.entry(&entry(&[], ItemKind::Dir, 0)).await?;
        out.entry(&entry(&["a"], ItemKind::File, 6)).await?;
        out.write(b"aaaaaa").await?;
        out.entry(&entry(&["b"], ItemKind::File, 5)).await?;
        out.write(b"bbbbb").await?;
        out.finish()
    };
    let recv = async {
        let mut f = recv_files(&server).await?;
        f.next_entry().await?.expect("root");
        f.next_entry().await?.expect("a");
        assert_eq!(hash_entry(&mut f).await?.1, 6);
        anyhow::Ok(f.next_entry().await.is_err())
    };
    let (_sent, refused) = timeout(T, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    assert!(refused.unwrap(), "6 + 5 bytes in a 10-byte item");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extra_entries_and_sized_dirs_are_refused() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    // A second entry in a one-entry item.
    let send = async {
        let mut out = client.open_files(&header(ItemKind::Dir, 0, 1)).await?;
        out.entry(&entry(&[], ItemKind::Dir, 0)).await?;
        out.entry(&entry(&["x"], ItemKind::Dir, 0)).await?;
        out.finish()
    };
    let recv = async {
        let mut f = recv_files(&server).await?;
        f.next_entry().await?.expect("root");
        anyhow::Ok(f.next_entry().await.is_err())
    };
    let (_sent, refused) = timeout(T, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    assert!(refused.unwrap(), "more entries than the header announced");

    // A folder that claims bytes.
    let send = async {
        let mut out = client.open_files(&header(ItemKind::Dir, 5, 1)).await?;
        out.entry(&entry(&[], ItemKind::Dir, 5)).await?;
        out.finish()
    };
    let recv = async {
        let mut f = recv_files(&server).await?;
        anyhow::Ok(f.next_entry().await.is_err())
    };
    let (_sent, refused) = timeout(T, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    assert!(refused.unwrap(), "a Dir entry with a size");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_stream_short_of_its_header_is_error() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    // Fewer entries than announced, then fewer bytes than announced.
    for (h, last) in [
        (header(ItemKind::Dir, 0, 2), entry(&[], ItemKind::Dir, 0)),
        (header(ItemKind::File, 10, 1), entry(&[], ItemKind::File, 4)),
    ] {
        let send = async {
            let mut out = client.open_files(&h).await?;
            out.entry(&last).await?;
            out.write(&vec![1; last.size as usize]).await?;
            out.finish()
        };
        let recv = async {
            let mut f = recv_files(&server).await?;
            f.next_entry().await?.expect("the one entry sent");
            assert_eq!(hash_entry(&mut f).await?.1, last.size);
            anyhow::Ok(f.next_entry().await.is_err())
        };
        let (sent, short) = timeout(T, async { tokio::join!(send, recv) })
            .await
            .unwrap();
        sent.unwrap();
        assert!(short.unwrap(), "{h:?} ended early but looked complete");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skipped_entry_bytes_are_an_error() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    let send = async {
        let mut out = client.open_files(&header(ItemKind::Dir, 8, 3)).await?;
        out.entry(&entry(&[], ItemKind::Dir, 0)).await?;
        out.entry(&entry(&["a"], ItemKind::File, 4)).await?;
        out.write(b"abcd").await?;
        out.entry(&entry(&["b"], ItemKind::File, 4)).await?;
        out.write(b"efgh").await?;
        out.finish()
    };
    let recv = async {
        let mut f = recv_files(&server).await?;
        f.next_entry().await?.expect("root");
        f.next_entry().await?.expect("a");
        let mut two = [0u8; 2];
        assert_eq!(f.read_chunk(&mut two).await?, 2);
        // Half of "a" is still unread: moving on must fail, not read "cd" as
        // the start of the next frame.
        anyhow::Ok(f.next_entry().await.is_err())
    };
    let (sent, refused) = timeout(T, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    sent.unwrap();
    assert!(refused.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_out_keeps_entries_whole() {
    let (a, b) = (ident(), ident());
    let (_l, _server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    let mut out = client
        .open_files(&header(ItemKind::Dir, 8, 3))
        .await
        .unwrap();
    assert!(out.write(b"x").await.is_err(), "bytes before any entry");
    out.entry(&entry(&[], ItemKind::Dir, 0)).await.unwrap();
    assert!(out.write(b"x").await.is_err(), "bytes for a folder");
    out.entry(&entry(&["a"], ItemKind::File, 4)).await.unwrap();
    assert!(
        out.write(b"abcde").await.is_err(),
        "more than the entry's size"
    );
    out.write(b"ab").await.unwrap();
    assert!(
        out.entry(&entry(&["b"], ItemKind::File, 4)).await.is_err(),
        "a new entry before the last one's bytes"
    );
    assert!(out.finish().is_err(), "finishing mid-entry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_stream_tag_is_refused() {
    let (a, b) = (ident(), ident());
    let (_l, server, raw) = listener_with_raw_peer(&a, &b).await;

    let mut bad = timeout(T, raw.conn.open_uni()).await.unwrap().unwrap();
    bad.write_all(&[9, 1, 2, 3]).await.unwrap();
    assert!(timeout(T, server.accept_stream()).await.unwrap().is_err());
    assert_eq!(
        timeout(T, bad.stopped()).await.unwrap().unwrap(),
        Some(quinn::VarInt::from_u32(STOP_INVALID))
    );

    // The session survives: a well-formed clipboard stream still lands.
    let small = ClipboardData {
        text: Some("after".into()),
        ..Default::default()
    };
    let mut good = timeout(T, raw.conn.open_uni()).await.unwrap().unwrap();
    let mut wire = vec![0u8];
    wire.extend(postcard::to_allocvec(&small).unwrap());
    good.write_all(&wire).await.unwrap();
    good.finish().unwrap();
    let got = timeout(T, recv_clipboard(&server, MB as u64))
        .await
        .unwrap();
    assert_eq!(got.unwrap(), small);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_files_frames_are_refused() {
    let (a, b) = (ident(), ident());
    let (_l, server, raw) = listener_with_raw_peer(&a, &b).await;
    let too_long = (MAX_FRAME as u32 + 1).to_le_bytes();

    // An oversized header frame: refused from its length prefix alone.
    let mut s = timeout(T, raw.conn.open_uni()).await.unwrap().unwrap();
    s.write_all(&[1]).await.unwrap();
    s.write_all(&too_long).await.unwrap();
    assert!(timeout(T, server.accept_stream()).await.unwrap().is_err());
    assert_eq!(
        timeout(T, s.stopped()).await.unwrap().unwrap(),
        Some(quinn::VarInt::from_u32(STOP_INVALID))
    );

    // A good header, then an oversized entry frame.
    let body = postcard::to_allocvec(&header(ItemKind::Dir, 0, 1)).unwrap();
    let mut s = timeout(T, raw.conn.open_uni()).await.unwrap().unwrap();
    s.write_all(&[1]).await.unwrap();
    s.write_all(&(body.len() as u32).to_le_bytes())
        .await
        .unwrap();
    s.write_all(&body).await.unwrap();
    s.write_all(&too_long).await.unwrap();
    let mut f = recv_files(&server).await.unwrap();
    assert!(timeout(T, f.next_entry()).await.unwrap().is_err());
    assert_eq!(
        timeout(T, s.stopped()).await.unwrap().unwrap(),
        Some(quinn::VarInt::from_u32(STOP_INVALID))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roots_out_of_place_or_of_the_wrong_kind_are_refused() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    let root = |kind| entry(&[], kind, 0);
    let child = || entry(&["a"], ItemKind::Dir, 0);
    // The entries sent, and the index of the one refused.
    let cases = [
        // A file item's root is a file, a folder item's a folder.
        (header(ItemKind::File, 0, 1), vec![root(ItemKind::Dir)], 0),
        (header(ItemKind::Dir, 0, 1), vec![root(ItemKind::File)], 0),
        // The root comes first, and only once.
        (
            header(ItemKind::Dir, 0, 2),
            vec![child(), root(ItemKind::Dir)],
            0,
        ),
        (
            header(ItemKind::Dir, 0, 2),
            vec![root(ItemKind::Dir), root(ItemKind::Dir)],
            1,
        ),
        // A file item is its root alone.
        (
            header(ItemKind::File, 0, 1),
            vec![root(ItemKind::File), child()],
            1,
        ),
    ];
    for (h, entries, refused) in cases {
        let send = async {
            let mut out = client.open_files(&h).await?;
            for e in &entries {
                out.entry(e).await?;
            }
            out.finish()
        };
        let recv = async {
            let mut f = recv_files(&server).await?;
            for i in 0..refused {
                f.next_entry()
                    .await?
                    .unwrap_or_else(|| panic!("no entry {i}"));
            }
            anyhow::Ok(f.next_entry().await.is_err())
        };
        let (_sent, was_refused) = timeout(T, async { tokio::join!(send, recv) })
            .await
            .unwrap();
        assert!(
            was_refused.unwrap(),
            "{h:?} {entries:?}: entry {refused} accepted"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headers_with_impossible_entry_counts_are_refused() {
    let (a, b) = (ident(), ident());
    let (_l, server, client) = pair_up(&a, Trust::PairingMode, &b, Trust::PairingMode).await;

    // A file item is exactly its root; a folder item has at least its root.
    for h in [
        header(ItemKind::File, 0, 2),
        header(ItemKind::File, 0, 0),
        header(ItemKind::Dir, 0, 0),
    ] {
        let (_out, got) = timeout(T, async {
            tokio::join!(client.open_files(&h), server.accept_stream())
        })
        .await
        .unwrap();
        assert!(got.is_err(), "{h:?} accepted");
    }

    // The session survives the refused streams.
    let small = ClipboardData {
        text: Some("ok".into()),
        ..Default::default()
    };
    let (sent, got) = timeout(T, async {
        tokio::join!(
            client.send_clipboard(&small),
            recv_clipboard(&server, MB as u64)
        )
    })
    .await
    .unwrap();
    sent.unwrap();
    assert_eq!(got.unwrap(), small);
}

#[test]
fn stop_code_ignores_other_errors() {
    assert_eq!(
        stop_code(&anyhow::anyhow!("entries exceed the header")),
        None
    );
    let lost = quinn::ReadError::ConnectionLost(quinn::ConnectionError::TimedOut);
    assert_eq!(stop_code(&lost.into()), None);

    let reset = quinn::ReadError::Reset(quinn::VarInt::from_u32(STOP_CANCELLED));
    let wrapped = anyhow::Error::from(quinn::ReadExactError::ReadError(reset)).context("header");
    assert_eq!(stop_code(&wrapped), Some(STOP_CANCELLED));
    let stopped = quinn::WriteError::Stopped(quinn::VarInt::from_u32(STOP_INVALID));
    assert_eq!(stop_code(&stopped.into()), Some(STOP_INVALID));
}
