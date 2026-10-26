//! One session with the PC: the pairing exchange (`run_pairing`) and the
//! tasks serving a paired session (`Live`).
//!
//! Input goes from the readers straight to the injection thread, in arrival
//! order; only the rest (pongs, clipboard, other control messages, the
//! session ending) goes to the app's loop.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use cj_core::clipboard::ClipboardData;
use cj_core::proto::{Control, Datagram};
use cj_net::session::Session;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tracing::{debug, info};

use super::app::Ev;
use super::control::{InjectMsg, Input};
use crate::common::session_tasks::{ClipboardOut, NetEv, pong, spawn_stream_tasks};

/// After declining a pairing, how long to wait for the PC to close the
/// session (so the answer is delivered) before closing it ourselves.
const DECLINE_FLUSH: Duration = Duration::from_secs(1);

/// Input for the injection thread, or (Err) a message for the loop.
pub(super) fn route_control(c: Control) -> Result<Input, Control> {
    match c {
        Control::Enter { edge, frac, .. } => Ok(Input::Enter { edge, frac }),
        Control::Button { button, down } => Ok(Input::Button { button, down }),
        Control::Key { mac_keycode, down } => Ok(Input::Key { mac_keycode, down }),
        Control::ReleaseAll => Ok(Input::ReleaseAll),
        other => Err(other),
    }
}

/// Pairing with a PC that is not paired yet, on the session it opened in
/// pairing mode: runs the code exchange, asks this Mac's user with
/// `ask(code)` while listening for the PC's answer, then sends this Mac's
/// answer. True only if both users accepted; the session is then ready for
/// `Live`. Nothing else may use the session meanwhile. `ask`'s future may
/// be dropped unfinished (when the PC declines or goes away first).
pub(super) async fn run_pairing<F, Fut>(session: &Session, ask: F) -> Result<bool>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = bool>,
{
    let code = session.pair().await.context("pairing exchange")?;
    let dialog = ask(code);
    tokio::pin!(dialog);
    // The PC's answer, if it came first. A decline or a lost session ends
    // the pairing at once; the dialog stays up until answered (its answer
    // is then ignored).
    let mut remote: Option<bool> = None;
    let local = loop {
        tokio::select! {
            local = &mut dialog => break local,
            // recv_control is cancel-safe.
            c = session.recv_control(), if remote.is_none() => match c {
                Ok(Control::PairConfirm { accepted }) => {
                    info!(accepted, "pairing: answered on the PC");
                    if !accepted {
                        return Ok(false);
                    }
                    remote = Some(true);
                }
                Ok(other) => debug!(?other, "pairing: ignoring an unexpected message"),
                Err(e) => return Err(e.context("the PC went away during pairing")),
            },
        }
    };
    info!(accepted = local, "pairing: answered on this Mac");
    // Never inside select!: send_control is not cancel-safe.
    session
        .send_control(&Control::PairConfirm { accepted: local })
        .await
        .context("sending this Mac's pairing answer")?;
    if !local {
        // Closing at once would discard the unsent answer, and the PC would
        // ask again within seconds instead of after its decline back-off.
        // The PC closes once both answers are in; give it a moment.
        let _ = tokio::time::timeout(DECLINE_FLUSH, session.closed()).await;
        return Ok(false);
    }
    match remote {
        Some(accepted) => Ok(accepted),
        None => wait_for_pair_answer(session).await,
    }
}

async fn wait_for_pair_answer(session: &Session) -> Result<bool> {
    loop {
        match session
            .recv_control()
            .await
            .context("waiting for the PC's pairing answer")?
        {
            Control::PairConfirm { accepted } => {
                info!(accepted, "pairing: answered on the PC");
                return Ok(accepted);
            }
            other => debug!(?other, "pairing: ignoring an unexpected message"),
        }
    }
}

/// The paired session and the tasks serving it. Dropping it ends the
/// session on the injection thread (releasing everything) and closes it.
pub(super) struct Live {
    pub sid: u64,
    pub session: Session,
    pub peer_name: String,
    inject: crossbeam_channel::Sender<InjectMsg>,
    clip_out: ClipboardOut,
    tasks: Vec<JoinHandle<()>>,
}

impl Live {
    /// Tells the injection thread that session `sid` begins, then starts the
    /// readers. Only call once any pairing exchange is done. `epoch` is the
    /// zero point of this Mac's ping timestamps.
    pub fn start(
        sid: u64,
        session: Session,
        clip_max: u64,
        epoch: std::time::Instant,
        inject: crossbeam_channel::Sender<InjectMsg>,
        events: &UnboundedSender<Ev>,
    ) -> Live {
        let _ = inject.send(InjectMsg::Begin { sid });
        let net = move |ev| Ev::Net { sid, ev };
        let mut tasks = Vec::new();

        let (s, tx, inj) = (session.clone(), events.clone(), inject.clone());
        tasks.push(tokio::spawn(async move {
            let reason = loop {
                match s.recv_control().await {
                    Ok(c) => match route_control(c) {
                        Ok(input) => {
                            if inj.send(InjectMsg::Input { sid, input }).is_err() {
                                break "the injection thread is gone".to_string();
                            }
                        }
                        Err(other) => {
                            if tx.send(net(NetEv::Control(other))).is_err() {
                                return;
                            }
                        }
                    },
                    Err(e) => break format!("control stream: {e:#}"),
                }
            };
            let _ = tx.send(net(NetEv::Closed(reason)));
        }));

        let (s, tx, inj) = (session.clone(), events.clone(), inject.clone());
        tasks.push(tokio::spawn(async move {
            let reason = loop {
                let input = match s.recv_datagram().await {
                    Ok(Datagram::Motion { dx, dy }) => Input::Motion { dx, dy },
                    Ok(Datagram::Scroll { dx, dy }) => Input::Scroll { dx, dy },
                    // The PC measures latency too.
                    Ok(Datagram::Ping { seq, sent_us }) => {
                        let _ = s.send_datagram(&Datagram::Pong { seq, sent_us });
                        continue;
                    }
                    Ok(Datagram::Pong { sent_us, .. }) => {
                        if let Some(ev) = pong(epoch, sent_us)
                            && tx.send(net(ev)).is_err()
                        {
                            return;
                        }
                        continue;
                    }
                    Err(e) => break format!("connection: {e:#}"),
                };
                if inj.send(InjectMsg::Input { sid, input }).is_err() {
                    break "the injection thread is gone".to_string();
                }
            };
            let _ = tx.send(net(NetEv::Closed(reason)));
        }));

        let tx = events.clone();
        let (clip_out, stream_tasks) = spawn_stream_tasks(
            &session,
            clip_max,
            move |d| tx.send(net(NetEv::Clipboard(d))).is_ok(),
            |_| true,
        );
        tasks.extend(stream_tasks);

        Live {
            sid,
            peer_name: session.peer_name(),
            session,
            inject,
            clip_out,
            tasks,
        }
    }

    /// Queues a local clipboard item, replacing one not yet sent.
    pub fn send_clipboard(&self, d: ClipboardData) {
        self.clip_out.send(d);
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.inject.send(InjectMsg::End { sid: self.sid });
        for t in &self.tasks {
            t.abort();
        }
        // Sends CONNECTION_CLOSE, so the PC takes its cursor back at once
        // instead of after the idle timeout.
        self.session.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_core::proto::{MouseButton, Side};

    #[test]
    fn input_goes_to_the_injection_thread_and_the_rest_to_the_loop() {
        assert_eq!(
            route_control(Control::Enter {
                edge: Side::Left,
                frac: 0.25,
                drag: None
            }),
            Ok(Input::Enter {
                edge: Side::Left,
                frac: 0.25
            })
        );
        assert_eq!(
            route_control(Control::Button {
                button: MouseButton::Right,
                down: true
            }),
            Ok(Input::Button {
                button: MouseButton::Right,
                down: true
            })
        );
        assert_eq!(
            route_control(Control::Key {
                mac_keycode: 0x37,
                down: false
            }),
            Ok(Input::Key {
                mac_keycode: 0x37,
                down: false
            })
        );
        assert_eq!(route_control(Control::ReleaseAll), Ok(Input::ReleaseAll));
        for other in [
            Control::PairConfirm { accepted: true },
            Control::Leave {
                frac: 0.5,
                drag: None,
            },
        ] {
            assert_eq!(route_control(other.clone()), Err(other));
        }
    }

    /// Real QUIC sessions on 127.0.0.1 between this Mac's code and a fake PC.
    mod loopback {
        use super::*;
        use crate::mac::control::Router;
        use crate::mac::control::tests::{Call, Fake, take};
        use cj_net::session::{Listener, connect};
        use cj_net::{Identity, Trust};
        use tokio::sync::{mpsc, oneshot};
        use tokio::time::{sleep, timeout};

        /// Upper bound for anything expected to finish.
        const T: Duration = Duration::from_secs(10);
        /// Long enough for the other side to act first.
        const LATER: Duration = Duration::from_millis(200);
        /// Longer than any test may take.
        const NEVER: Duration = Duration::from_secs(3600);

        fn ident() -> Identity {
            let dir = tempfile::tempdir().unwrap();
            Identity::load_or_create(dir.path()).unwrap()
        }

        /// A listening Mac and a fake PC connected to it, pinned to each
        /// other or in pairing mode. Returns (listener, Mac side, PC side).
        async fn connected(paired: bool) -> (Listener, Session, Session) {
            let (mac, pc) = (ident(), ident());
            let (mac_trust, pc_trust) = if paired {
                (
                    Trust::Pinned(pc.fingerprint()),
                    Trust::Pinned(mac.fingerprint()),
                )
            } else {
                (Trust::PairingMode, Trust::PairingMode)
            };
            let any_port = (std::net::Ipv4Addr::LOCALHOST, 0).into();
            let listener = Listener::bind(&mac, mac_trust, any_port, "Mac")
                .await
                .unwrap();
            let (m, p) = timeout(T, async {
                tokio::join!(
                    listener.accept(),
                    connect(&pc, pc_trust, listener.local_addr(), "PC")
                )
            })
            .await
            .unwrap();
            (listener, m.unwrap(), p.unwrap())
        }

        /// The PC side of pairing: the code exchange, then its answer after
        /// `delay`. Returns the code and what the Mac sent, if anything, within T.
        async fn pc_pairs(
            pc: &Session,
            delay: Duration,
            accept: bool,
        ) -> (String, Option<Control>) {
            let code = pc.pair().await.unwrap();
            sleep(delay).await;
            pc.send_control(&Control::PairConfirm { accepted: accept })
                .await
                .unwrap();
            let mac_answer = timeout(T, pc.recv_control())
                .await
                .ok()
                .and_then(Result::ok);
            (code, mac_answer)
        }

        #[tokio::test]
        async fn pairing_succeeds_when_both_accept_pc_first() {
            let (_l, mac, pc) = connected(false).await;
            let (code_tx, code_rx) = oneshot::channel();
            let ask = |code: String| async move {
                let _ = code_tx.send(code);
                sleep(LATER).await;
                true
            };
            let (result, (pc_code, mac_answer)) = timeout(T, async {
                tokio::join!(run_pairing(&mac, ask), pc_pairs(&pc, Duration::ZERO, true))
            })
            .await
            .unwrap();
            assert!(result.unwrap());
            assert_eq!(
                code_rx.await.unwrap(),
                pc_code,
                "both screens show the same code"
            );
            assert_eq!(mac_answer, Some(Control::PairConfirm { accepted: true }));
        }

        #[tokio::test]
        async fn pairing_succeeds_when_both_accept_mac_first() {
            let (_l, mac, pc) = connected(false).await;
            let ask = |_code: String| async { true };
            let (result, (_, mac_answer)) = timeout(T, async {
                tokio::join!(run_pairing(&mac, ask), pc_pairs(&pc, LATER, true))
            })
            .await
            .unwrap();
            assert!(result.unwrap());
            assert_eq!(mac_answer, Some(Control::PairConfirm { accepted: true }));
        }

        #[tokio::test]
        async fn pairing_fails_when_the_mac_declines() {
            let (_l, mac, pc) = connected(false).await;
            let ask = |_code: String| async { false };
            // Like the app: the session is closed as soon as the pairing ends.
            let mac_side = async {
                let result = run_pairing(&mac, ask).await;
                mac.close();
                result
            };
            // The PC's user is still deciding; its reader sees the Mac's answer.
            let pc_side = async {
                pc.pair().await.unwrap();
                let answer = pc.recv_control().await.ok();
                pc.close();
                answer
            };
            let (result, mac_answer) = timeout(T, async { tokio::join!(mac_side, pc_side) })
                .await
                .unwrap();
            assert!(!result.unwrap());
            assert_eq!(
                mac_answer,
                Some(Control::PairConfirm { accepted: false }),
                "the decline reaches the PC before the session closes"
            );
        }

        #[tokio::test]
        async fn pairing_fails_when_the_pc_declines() {
            let (_l, mac, pc) = connected(false).await;
            let ask = |_code: String| async {
                // Never answered: the pairing must end without it.
                sleep(NEVER).await;
                true
            };
            let pc_side = async {
                pc.pair().await.unwrap();
                pc.send_control(&Control::PairConfirm { accepted: false })
                    .await
                    .unwrap();
            };
            let (result, ()) = timeout(T, async { tokio::join!(run_pairing(&mac, ask), pc_side) })
                .await
                .unwrap();
            assert!(!result.unwrap());
        }

        #[tokio::test]
        async fn pairing_errs_when_the_pc_goes_away() {
            let (_l, mac, pc) = connected(false).await;
            let ask = |_code: String| async {
                // Never answered: the pairing must end without it.
                sleep(NEVER).await;
                true
            };
            let pc_side = async {
                pc.pair().await.unwrap();
                pc.close();
            };
            let (result, ()) = timeout(T, async { tokio::join!(run_pairing(&mac, ask), pc_side) })
                .await
                .unwrap();
            assert!(result.is_err());
        }

        /// The next message for the injection thread, without blocking the runtime.
        async fn next(rx: &crossbeam_channel::Receiver<InjectMsg>) -> InjectMsg {
            let rx = rx.clone();
            tokio::task::spawn_blocking(move || rx.recv_timeout(T))
                .await
                .unwrap()
                .expect("a message for the injection thread")
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn live_session_feeds_the_injector_in_order() {
            let (_l, mac, pc) = connected(true).await;
            let (inject, inject_rx) = crossbeam_channel::unbounded();
            let (events, mut events_rx) = mpsc::unbounded_channel();
            let live = Live::start(5, mac, 1000, std::time::Instant::now(), inject, &events);

            for c in [
                Control::Enter {
                    edge: Side::Left,
                    frac: 0.25,
                    drag: None,
                },
                Control::Key {
                    mac_keycode: 0x00,
                    down: true,
                },
                Control::Key {
                    mac_keycode: 0x00,
                    down: false,
                },
                Control::PairConfirm { accepted: true },
            ] {
                pc.send_control(&c).await.unwrap();
            }
            let mut got = Vec::new();
            for _ in 0..4 {
                got.push(next(&inject_rx).await);
            }
            // Datagrams are not ordered with the control stream: send motion
            // only once the keys have arrived.
            pc.send_datagram(&Datagram::Motion { dx: 3, dy: -1 })
                .unwrap();
            got.push(next(&inject_rx).await);

            // Non-input goes to the loop instead.
            let ev = timeout(T, events_rx.recv()).await.unwrap().unwrap();
            assert!(matches!(
                ev,
                Ev::Net {
                    sid: 5,
                    ev: NetEv::Control(Control::PairConfirm { accepted: true })
                }
            ));
            // The PC's pings are echoed.
            pc.send_datagram(&Datagram::Ping {
                seq: 9,
                sent_us: 42,
            })
            .unwrap();
            let pong = timeout(T, pc.recv_datagram()).await.unwrap().unwrap();
            assert_eq!(
                pong,
                Datagram::Pong {
                    seq: 9,
                    sent_us: 42
                }
            );

            // Ending the session ends it on the injection thread and closes
            // it at once (well before the 600 ms idle timeout).
            let ended = std::time::Instant::now();
            drop(live);
            got.push(next(&inject_rx).await);
            timeout(T, pc.closed()).await.unwrap();
            assert!(ended.elapsed() < Duration::from_millis(400));

            let (fake, calls) = Fake::new();
            let granted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let mut router = Router::new(fake, granted);
            for msg in got {
                router.handle(msg);
            }
            assert_eq!(
                take(&calls),
                vec![
                    Call::Wake,
                    Call::Warp(Side::Left, 0.25),
                    Call::Key(0x00, true),
                    Call::Key(0x00, false),
                    Call::Motion(3, -1),
                    Call::ReleaseAll,
                ]
            );
        }
    }
}
