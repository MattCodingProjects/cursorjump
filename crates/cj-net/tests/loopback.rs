//! End-to-end tests of `cj_net::session` over real QUIC on 127.0.0.1.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cj_core::clipboard::{ClipboardData, within_limit};
use cj_core::proto::{
    Control, Datagram, MouseButton, PROTOCOL_VERSION, decode_control_body, encode_control,
    encode_datagram,
};
use cj_net::session::{Listener, Session, connect};
use cj_net::{ALPN, CjServerCertVerifier, Identity, SERVER_NAME, Trust, client_config};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
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
                server.recv_clipboard(MB as u64)
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
            server.recv_clipboard(MB as u64)
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
            server.recv_clipboard(MB as u64)
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

    // At most 4 unaccepted clipboard streams in flight.
    let mut unis = Vec::new();
    for _ in 0..4 {
        let mut s = timeout(T, raw.conn.open_uni()).await.unwrap().unwrap();
        s.write_all(b"x").await.unwrap();
        unis.push(s);
    }
    assert!(
        timeout(short, raw.conn.open_uni()).await.is_err(),
        "a 5th uni stream must wait for credit"
    );
}
