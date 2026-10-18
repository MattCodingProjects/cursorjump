//! Wire protocol shared by the PC and the Mac client.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
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
    /// PC to Mac: edge of the Mac screen being entered.
    Enter {
        edge: Side,
        frac: f32,
    },
    /// Mac to PC: fraction along the Mac's entry edge.
    Leave {
        frac: f32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Key {
        mac_keycode: u16,
        down: bool,
    },
    ReleaseAll,
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
    fn garbage_is_error_not_panic() {
        assert!(decode_datagram(&[0xFF; 3]).is_err());
    }
}
