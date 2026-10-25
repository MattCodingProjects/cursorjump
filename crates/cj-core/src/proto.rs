//! Wire protocol shared by the PC and the Mac client.

use crate::dnd::{DragId, DragItem, DragOutcome, FailReason};
use serde::{Deserialize, Serialize};

/// Peers must match exactly. 2: `Control::MacReady`, which a v1 peer
/// cannot decode (an undecodable message ends the session). 3: drag-and-drop.
pub const PROTOCOL_VERSION: u16 = 3;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// Unreliable, latency-sensitive messages (QUIC datagrams).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Datagram {
    /// Already speed-scaled by the PC; Mac points.
    Motion {
        dx: i16,
        dy: i16,
    },
    /// Pixels, already scaled/reversed by the PC.
    Scroll {
        dx: i32,
        dy: i32,
    },
    Ping {
        seq: u32,
        sent_us: u64,
    },
    Pong {
        seq: u32,
        sent_us: u64,
    },
}

/// Reliable messages (QUIC stream), framed with a u32 LE length prefix.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Control {
    Hello {
        version: u16,
        name: String,
    },
    PairConfirm {
        accepted: bool,
    },
    /// Pairing step 1, connector to listener: commitment to the connector's nonce.
    PairCommit {
        commit: [u8; 32],
    },
    /// Pairing step 2, listener to connector: the listener's nonce.
    PairNonce {
        nonce: [u8; 32],
    },
    /// Pairing step 3, connector to listener: reveals the committed nonce.
    PairReveal {
        nonce: [u8; 32],
    },
    /// PC to Mac: edge of the Mac screen being entered. `drag`: the
    /// `DragOffer` this crossing carries.
    Enter {
        edge: Side,
        frac: f32,
        drag: Option<DragId>,
    },
    /// Mac to PC: fraction along the Mac's entry edge. `drag`: the
    /// `DragOffer` this crossing carries.
    Leave {
        frac: f32,
        drag: Option<DragId>,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Key {
        mac_keycode: u16,
        down: bool,
    },
    /// PC to Mac: the PC took the cursor back (return hotkey, hook lost,
    /// pause or quit). The Mac releases everything held and stops being controlled.
    ReleaseAll,
    /// Mac to PC: whether the Mac can take the cursor (it has the
    /// Accessibility permission). Sent when a paired session starts and
    /// whenever it changes.
    MacReady {
        ready: bool,
    },
    // Drag-and-drop. New variants go last: postcard encodes the variant index.
    /// Origin to receiver, just before the crossing that carries it.
    DragOffer {
        drag: DragId,
        items: Vec<DragItem>,
    },
    /// Either side.
    DragCancel {
        drag: DragId,
        reason: FailReason,
    },
    /// Receiver to origin: released on a target that accepted.
    DragDropped {
        drag: DragId,
    },
    /// Receiver to origin.
    FileFetch {
        drag: DragId,
        item: u32,
    },
    /// Receiver to origin: the final result; the origin frees its temp files.
    DragFinished {
        drag: DragId,
        outcome: DragOutcome,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("decode: {0}")]
    Decode(String),
}

pub fn encode_datagram(d: &Datagram) -> Vec<u8> {
    postcard::to_allocvec(d).expect("datagram serialization is infallible")
}

pub fn decode_datagram(b: &[u8]) -> Result<Datagram, ProtoError> {
    postcard::from_bytes(b).map_err(|e| ProtoError::Decode(e.to_string()))
}

/// Returns a u32 LE length prefix followed by the postcard body.
pub fn encode_control(c: &Control) -> Vec<u8> {
    let body = postcard::to_allocvec(c).expect("control serialization is infallible");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Decodes a body only, without the length prefix.
pub fn decode_control_body(b: &[u8]) -> Result<Control, ProtoError> {
    postcard::from_bytes(b).map_err(|e| ProtoError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dnd::ItemKind;

    #[test]
    fn datagram_roundtrip_all_variants() {
        let all = [
            Datagram::Motion { dx: -300, dy: 12 },
            Datagram::Scroll {
                dx: -5,
                dy: 120_000,
            },
            Datagram::Ping {
                seq: 7,
                sent_us: u64::MAX,
            },
            Datagram::Pong {
                seq: u32::MAX,
                sent_us: 1,
            },
        ];
        for d in all {
            assert_eq!(decode_datagram(&encode_datagram(&d)).unwrap(), d);
        }
    }

    #[test]
    fn motion_datagram_is_tiny() {
        assert!(encode_datagram(&Datagram::Motion { dx: -300, dy: 12 }).len() <= 8);
    }

    #[test]
    fn control_roundtrip_unicode() {
        let c = Control::Hello {
            version: 1,
            name: "Matt’s MacBook 🍎 東京".into(),
        };
        let enc = encode_control(&c);
        assert_eq!(
            u32::from_le_bytes(enc[..4].try_into().unwrap()) as usize,
            enc.len() - 4
        );
        assert_eq!(decode_control_body(&enc[4..]).unwrap(), c);
    }

    #[test]
    fn control_roundtrip_all_variants() {
        let all = [
            Control::Hello {
                version: PROTOCOL_VERSION,
                name: "PC".into(),
            },
            Control::PairConfirm { accepted: true },
            Control::PairCommit { commit: [0xAB; 32] },
            Control::PairNonce { nonce: [0x01; 32] },
            Control::PairReveal { nonce: [0xFE; 32] },
            Control::Enter {
                edge: Side::Left,
                frac: 0.25,
                drag: None,
            },
            Control::Enter {
                edge: Side::Top,
                frac: 0.5,
                drag: Some(7),
            },
            Control::Leave {
                frac: 1.0,
                drag: None,
            },
            Control::Leave {
                frac: 0.0,
                drag: Some(7),
            },
            Control::Button {
                button: MouseButton::Forward,
                down: true,
            },
            Control::Key {
                mac_keycode: 0x7E,
                down: false,
            },
            Control::ReleaseAll,
            Control::MacReady { ready: false },
            Control::MacReady { ready: true },
            Control::DragOffer {
                drag: 7,
                items: vec![
                    DragItem {
                        name: "Photos".into(),
                        kind: ItemKind::Dir,
                        size: None,
                    },
                    DragItem {
                        name: "".into(),
                        kind: ItemKind::File,
                        size: Some(u64::MAX),
                    },
                ],
            },
            Control::DragCancel {
                drag: 7,
                reason: FailReason::Unreadable {
                    name: "a.txt".into(),
                },
            },
            Control::DragCancel {
                drag: u64::MAX,
                reason: FailReason::Cancelled,
            },
            Control::DragDropped { drag: 7 },
            Control::FileFetch {
                drag: 7,
                item: u32::MAX,
            },
            Control::DragFinished {
                drag: 7,
                outcome: DragOutcome::Done,
            },
            Control::DragFinished {
                drag: 7,
                outcome: DragOutcome::NotDropped,
            },
            Control::DragFinished {
                drag: 7,
                outcome: DragOutcome::Failed(FailReason::Changed {
                    name: "東京.mov".into(),
                }),
            },
        ];
        for c in all {
            let enc = encode_control(&c);
            assert_eq!(
                u32::from_le_bytes(enc[..4].try_into().unwrap()) as usize,
                enc.len() - 4
            );
            assert_eq!(decode_control_body(&enc[4..]).unwrap(), c);
        }
    }

    /// Postcard encodes the variant index: appending keeps older ones stable.
    #[test]
    fn control_variant_indices_are_stable() {
        assert_eq!(encode_control(&Control::ReleaseAll)[4], 9);
        assert_eq!(
            &encode_control(&Control::MacReady { ready: true })[4..],
            &[10, 1]
        );
        assert_eq!(
            encode_control(&Control::DragOffer {
                drag: 1,
                items: vec![]
            })[4],
            11
        );
        assert_eq!(
            encode_control(&Control::DragFinished {
                drag: 1,
                outcome: DragOutcome::Done
            })[4],
            15
        );
    }

    #[test]
    fn garbage_is_error_not_panic() {
        assert!(decode_datagram(&[0xFF; 3]).is_err());
    }
}
