//! Modifier flag tracking and held key/button bookkeeping for the Mac client.

use crate::proto::MouseButton;
use std::collections::BTreeSet;

/// CGEventFlags bits.
pub const FLAG_CAPS: u64 = 0x10000;
pub const FLAG_SHIFT: u64 = 0x20000;
pub const FLAG_CONTROL: u64 = 0x40000;
pub const FLAG_OPTION: u64 = 0x80000;
pub const FLAG_COMMAND: u64 = 0x100000;

const KC_CAPS: u16 = 0x39;
const KC_FUNCTION: u16 = 0x3F;

/// (keycode, flag) for every modifier key that contributes a flag, except Caps Lock.
const FLAG_KEYS: [(u16, u64); 8] = [
    (0x37, FLAG_COMMAND),
    (0x36, FLAG_COMMAND),
    (0x38, FLAG_SHIFT),
    (0x3C, FLAG_SHIFT),
    (0x3A, FLAG_OPTION),
    (0x3D, FLAG_OPTION),
    (0x3B, FLAG_CONTROL),
    (0x3E, FLAG_CONTROL),
];

/// True for Mac modifier keycodes (Function counts, though it sets no flag here).
pub fn is_modifier(mac_keycode: u16) -> bool {
    mac_keycode == KC_CAPS
        || mac_keycode == KC_FUNCTION
        || FLAG_KEYS.iter().any(|&(kc, _)| kc == mac_keycode)
}

/// Tracks which modifier keys are down. Left and right variants are independent.
#[derive(Debug, Default)]
pub struct ModifierState {
    down: BTreeSet<u16>,
    caps: bool,
}

impl ModifierState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, kc: u16, down: bool) {
        if kc == KC_CAPS {
            if down {
                self.caps = !self.caps;
            }
        } else if FLAG_KEYS.iter().any(|&(k, _)| k == kc) {
            if down {
                self.down.insert(kc);
            } else {
                self.down.remove(&kc);
            }
        }
    }

    pub fn flags(&self) -> u64 {
        let mut f = if self.caps { FLAG_CAPS } else { 0 };
        for &(kc, flag) in &FLAG_KEYS {
            if self.down.contains(&kc) {
                f |= flag;
            }
        }
        f
    }
}

const BUTTON_ORDER: [MouseButton; 5] = [
    MouseButton::Left,
    MouseButton::Right,
    MouseButton::Middle,
    MouseButton::Back,
    MouseButton::Forward,
];

/// Keys and buttons currently held, so they can all be released on handoff.
#[derive(Debug, Default)]
pub struct Held {
    keys: BTreeSet<u16>,
    buttons: Vec<MouseButton>,
}

impl Held {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn key(&mut self, kc: u16, down: bool) {
        if down {
            self.keys.insert(kc);
        } else {
            self.keys.remove(&kc);
        }
    }

    pub fn button(&mut self, b: MouseButton, down: bool) {
        self.buttons.retain(|&x| x != b);
        if down {
            self.buttons.push(b);
        }
    }

    pub fn any_button(&self) -> bool {
        !self.buttons.is_empty()
    }

    /// Everything still down: keys ascending, buttons in declaration order. Leaves it empty.
    pub fn drain(&mut self) -> (Vec<u16>, Vec<MouseButton>) {
        let keys = std::mem::take(&mut self.keys).into_iter().collect();
        let held = std::mem::take(&mut self.buttons);
        let buttons = BUTTON_ORDER
            .into_iter()
            .filter(|b| held.contains(b))
            .collect();
        (keys, buttons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::MouseButton;

    #[test]
    fn cmd_flag_set_and_cleared() {
        let mut m = ModifierState::new();
        m.update(0x37, true);
        assert_ne!(m.flags() & FLAG_COMMAND, 0);
        m.update(0x37, false);
        assert_eq!(m.flags() & FLAG_COMMAND, 0);
    }

    #[test]
    fn left_right_independent() {
        let mut m = ModifierState::new();
        m.update(0x37, true);
        m.update(0x36, true);
        m.update(0x37, false);
        assert_ne!(m.flags() & FLAG_COMMAND, 0);
        m.update(0x36, false);
        assert_eq!(m.flags(), 0);
    }

    #[test]
    fn caps_toggles() {
        let mut m = ModifierState::new();
        m.update(0x39, true);
        m.update(0x39, false);
        assert_ne!(m.flags() & FLAG_CAPS, 0);
        m.update(0x39, true);
        assert_eq!(m.flags() & FLAG_CAPS, 0);
    }

    #[test]
    fn is_modifier_recognises_keys() {
        for kc in [0x37, 0x36, 0x38, 0x3C, 0x3A, 0x3D, 0x3B, 0x3E, 0x39, 0x3F] {
            assert!(is_modifier(kc));
        }
        assert!(!is_modifier(0x00));
    }

    #[test]
    fn held_drain_releases_everything() {
        let mut h = Held::new();
        h.key(0x37, true);
        h.key(0x00, true);
        h.button(MouseButton::Left, true);
        assert!(h.any_button());
        let (keys, buttons) = h.drain();
        assert_eq!(keys, vec![0x00, 0x37]);
        assert_eq!(buttons, vec![MouseButton::Left]);
        assert!(!h.any_button());
        let (k2, b2) = h.drain();
        assert!(k2.is_empty() && b2.is_empty());
    }

    #[test]
    fn held_up_removes() {
        let mut h = Held::new();
        h.key(0x00, true);
        h.key(0x00, false);
        h.button(MouseButton::Right, true);
        h.button(MouseButton::Right, false);
        assert_eq!(h.drain(), (vec![], vec![]));
    }
}
