//! US ANSI key translation (Windows set-1 scancodes to macOS `kVK_*` codes)
//! and the routing logic that prevents stuck keys when crossing machines.
use std::collections::HashSet;

/// A physical Windows key: set-1 make code plus the `LLKHF_EXTENDED` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PcKey {
    pub scancode: u16,
    pub extended: bool,
}

/// Translate a PC key to a macOS virtual keycode. `None` means drop the key.
pub fn pc_to_mac(k: PcKey, swap_ctrl_cmd: bool) -> Option<u16> {
    // Ctrl and Win trade places when swapped; Alt/Shift never move.
    let (cmd_l, cmd_r, ctl_l, ctl_r) = (0x37, 0x36, 0x3B, 0x3E);
    let pick = |swapped: u16, plain: u16| if swap_ctrl_cmd { swapped } else { plain };
    let code = match (k.scancode, k.extended) {
        // Modifiers
        (0x1D, false) => pick(cmd_l, ctl_l),
        (0x1D, true) => pick(cmd_r, ctl_r),
        (0x5B, true) => pick(ctl_l, cmd_l),
        (0x5C, true) => pick(ctl_r, cmd_r),
        (0x38, false) => 0x3A,
        (0x38, true) => 0x3D,
        (0x2A, false) => 0x38, // extended 0x2A is the fake shift around PrintScreen: dropped
        (0x36, false) => 0x3C,
        (0x3A, false) => 0x39,
        (0x5D, true) => return None, // Apps/Menu

        // Extended-only keys
        (0x1C, true) => 0x4C, // Numpad Enter
        (0x35, true) => 0x4B, // Numpad /
        (0x37, true) => 0x69, // PrintScreen -> F13
        (0x52, true) => 0x72, // Insert -> Help
        (0x53, true) => 0x75, // Delete -> ForwardDelete
        (0x47, true) => 0x73, // Home
        (0x4F, true) => 0x77, // End
        (0x49, true) => 0x74, // PageUp
        (0x51, true) => 0x79, // PageDown
        (0x48, true) => 0x7E, // Up
        (0x50, true) => 0x7D, // Down
        (0x4B, true) => 0x7B, // Left
        (0x4D, true) => 0x7C, // Right
        (0x45, true) => 0x47, // NumLock -> KeypadClear

        // Numpad (non-extended)
        (0x37, false) => 0x43, // *
        (0x4A, false) => 0x4E, // -
        (0x4E, false) => 0x45, // +
        (0x53, false) => 0x41, // .
        (0x52, false) => 0x52, // 0
        (0x4F, false) => 0x53, // 1
        (0x50, false) => 0x54, // 2
        (0x51, false) => 0x55, // 3
        (0x4B, false) => 0x56, // 4
        (0x4C, false) => 0x57, // 5
        (0x4D, false) => 0x58, // 6
        (0x47, false) => 0x59, // 7
        (0x48, false) => 0x5B, // 8
        (0x49, false) => 0x5C, // 9

        (0x45, false) => 0x71, // Pause -> F15
        (0x46, false) => 0x6B, // ScrollLock -> F14

        // Main block (non-extended only)
        (sc, false) => match sc {
            0x01 => 0x35,
            0x02 => 0x12,
            0x03 => 0x13,
            0x04 => 0x14,
            0x05 => 0x15,
            0x06 => 0x17,
            0x07 => 0x16,
            0x08 => 0x1A,
            0x09 => 0x1C,
            0x0A => 0x19,
            0x0B => 0x1D,
            0x0C => 0x1B,
            0x0D => 0x18,
            0x0E => 0x33,
            0x0F => 0x30,
            0x10 => 0x0C,
            0x11 => 0x0D,
            0x12 => 0x0E,
            0x13 => 0x0F,
            0x14 => 0x11,
            0x15 => 0x10,
            0x16 => 0x20,
            0x17 => 0x22,
            0x18 => 0x1F,
            0x19 => 0x23,
            0x1A => 0x21,
            0x1B => 0x1E,
            0x1C => 0x24,
            0x1E => 0x00,
            0x1F => 0x01,
            0x20 => 0x02,
            0x21 => 0x03,
            0x22 => 0x05,
            0x23 => 0x04,
            0x24 => 0x26,
            0x25 => 0x28,
            0x26 => 0x25,
            0x27 => 0x29,
            0x28 => 0x27,
            0x29 => 0x32,
            0x2B => 0x2A,
            0x2C => 0x06,
            0x2D => 0x07,
            0x2E => 0x08,
            0x2F => 0x09,
            0x30 => 0x0B,
            0x31 => 0x2D,
            0x32 => 0x2E,
            0x33 => 0x2B,
            0x34 => 0x2F,
            0x35 => 0x2C,
            0x39 => 0x31,
            0x3B => 0x7A,
            0x3C => 0x78,
            0x3D => 0x63,
            0x3E => 0x76,
            0x3F => 0x60,
            0x40 => 0x61,
            0x41 => 0x62,
            0x42 => 0x64,
            0x43 => 0x65,
            0x44 => 0x6D,
            0x57 => 0x67,
            0x58 => 0x6F,
            _ => return None,
        },
        _ => return None,
    };
    Some(code)
}

/// Where a key event goes while the cursor is on the Mac.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Local,
    Remote,
}

/// Tracks keys that were already down on Windows when the cursor crossed, so
/// their releases reach Windows instead of leaving them stuck.
#[derive(Debug, Default)]
pub struct KeyRouter {
    held_at_entry: HashSet<PcKey>,
}

impl KeyRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_enter_mac(&mut self, down_now: impl IntoIterator<Item = PcKey>) {
        self.held_at_entry = down_now.into_iter().collect();
    }

    pub fn route(&mut self, k: PcKey, down: bool) -> Route {
        if self.held_at_entry.remove(&k) && !down {
            Route::Local
        } else {
            Route::Remote
        }
    }

    pub fn clear(&mut self) {
        self.held_at_entry.clear();
    }
}

/// Ctrl+Alt+Home (either Ctrl, either Alt) pressed.
pub fn is_return_hotkey(held: &HashSet<PcKey>, k: PcKey, down: bool) -> bool {
    if !down || k.scancode != 0x47 || !k.extended {
        return false;
    }
    let has = |sc: u16| held.iter().any(|h| h.scancode == sc);
    has(0x1D) && has(0x38)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: u16, e: bool) -> PcKey {
        PcKey {
            scancode: s,
            extended: e,
        }
    }

    #[test]
    fn letters_and_basics() {
        assert_eq!(pc_to_mac(k(0x1E, false), true), Some(0x00));
        assert_eq!(pc_to_mac(k(0x1C, false), true), Some(0x24));
        assert_eq!(pc_to_mac(k(0x29, false), true), Some(0x32));
        assert_eq!(pc_to_mac(k(0x2B, false), true), Some(0x2A));
        assert_eq!(pc_to_mac(k(0x3B, false), true), Some(0x7A));
        assert_eq!(pc_to_mac(k(0x0E, false), true), Some(0x33));
        assert_eq!(pc_to_mac(k(0x01, false), true), Some(0x35));
        assert_eq!(pc_to_mac(k(0x58, false), true), Some(0x6F));
    }

    #[test]
    fn extended_disambiguates() {
        assert_eq!(pc_to_mac(k(0x4B, true), true), Some(0x7B));
        assert_eq!(pc_to_mac(k(0x4B, false), true), Some(0x56));
        assert_eq!(pc_to_mac(k(0x53, true), true), Some(0x75));
        assert_eq!(pc_to_mac(k(0x1C, true), true), Some(0x4C));
        assert_eq!(pc_to_mac(k(0x35, true), true), Some(0x4B));
        assert_eq!(pc_to_mac(k(0x37, true), true), Some(0x69));
        assert_eq!(pc_to_mac(k(0x46, false), true), Some(0x6B));
        assert_eq!(pc_to_mac(k(0x45, false), true), Some(0x71));
        assert_eq!(pc_to_mac(k(0x45, true), true), Some(0x47));
        assert_eq!(pc_to_mac(k(0x52, true), true), Some(0x72));
    }

    #[test]
    fn modifiers_swap() {
        assert_eq!(pc_to_mac(k(0x1D, false), true), Some(0x37));
        assert_eq!(pc_to_mac(k(0x1D, true), true), Some(0x36));
        assert_eq!(pc_to_mac(k(0x5B, true), true), Some(0x3B));
        assert_eq!(pc_to_mac(k(0x5C, true), true), Some(0x3E));
        assert_eq!(pc_to_mac(k(0x1D, false), false), Some(0x3B));
        assert_eq!(pc_to_mac(k(0x1D, true), false), Some(0x3E));
        assert_eq!(pc_to_mac(k(0x5B, true), false), Some(0x37));
        assert_eq!(pc_to_mac(k(0x5C, true), false), Some(0x36));
        assert_eq!(pc_to_mac(k(0x38, false), true), Some(0x3A));
        assert_eq!(pc_to_mac(k(0x38, true), true), Some(0x3D));
        assert_eq!(pc_to_mac(k(0x38, false), false), Some(0x3A));
        assert_eq!(pc_to_mac(k(0x2A, false), true), Some(0x38));
        assert_eq!(pc_to_mac(k(0x36, false), true), Some(0x3C));
    }

    #[test]
    fn unknown_is_none() {
        assert_eq!(pc_to_mac(k(0x7F, false), true), None);
        assert_eq!(pc_to_mac(k(0x5D, true), true), None);
    }

    #[test]
    fn mappings_are_unique_and_complete() {
        let mut seen = HashSet::new();
        for sc in 0u16..0x80 {
            for ext in [false, true] {
                if let Some(m) = pc_to_mac(k(sc, ext), true) {
                    assert!(seen.insert(m), "duplicate mac code {m:#x} at {sc:#x}/{ext}");
                }
            }
        }
        assert!(seen.len() >= 100, "only {} mapped", seen.len());
    }

    #[test]
    fn key_router_held_key_released_locally() {
        let mut r = KeyRouter::new();
        r.on_enter_mac([k(0x11, false)]);
        assert!(matches!(r.route(k(0x11, false), false), Route::Local));
        assert!(matches!(r.route(k(0x11, false), true), Route::Remote));
    }

    #[test]
    fn key_router_repress_without_seeing_up_goes_remote_and_forgets() {
        let mut r = KeyRouter::new();
        r.on_enter_mac([k(0x11, false)]);
        assert_eq!(r.route(k(0x11, false), true), Route::Remote);
        assert_eq!(r.route(k(0x11, false), false), Route::Remote);
        assert_eq!(r.route(k(0x12, false), true), Route::Remote);
    }

    #[test]
    fn key_router_clear_forgets() {
        let mut r = KeyRouter::new();
        r.on_enter_mac([k(0x11, false)]);
        r.clear();
        assert_eq!(r.route(k(0x11, false), false), Route::Remote);
    }

    #[test]
    fn return_hotkey() {
        let home = k(0x47, true);
        let lctrl_lalt: HashSet<PcKey> = [k(0x1D, false), k(0x38, false)].into();
        assert!(is_return_hotkey(&lctrl_lalt, home, true));
        let rctrl_ralt: HashSet<PcKey> = [k(0x1D, true), k(0x38, true)].into();
        assert!(is_return_hotkey(&rctrl_ralt, home, true));
        let ctrl_only: HashSet<PcKey> = [k(0x1D, false)].into();
        assert!(!is_return_hotkey(&ctrl_only, home, true));
        assert!(!is_return_hotkey(&lctrl_lalt, home, false));
        assert!(!is_return_hotkey(&lctrl_lalt, k(0x47, false), true));
    }
}
