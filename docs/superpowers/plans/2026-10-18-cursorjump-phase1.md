# CursorJump Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Windows PC's mouse and keyboard glide onto the MacBook as if it were a third monitor, and copy/paste works both ways. It must be encrypted, paired, auto-discovered, auto-starting, and invisible to game anti-cheat.

**Architecture:** One Rust workspace with two libraries and one binary.
- `cj-core` holds all pure, OS-independent logic: protocol, layout math, key mapping, gaming rule, click/modifier tracking, latency stats, clipboard model, config, and the PC/Mac state machines. It is unit-tested on any OS.
- `cj-net` is the encrypted transport. QUIC (quinn) carries unreliable datagrams for motion and pings, a reliable stream for discrete events, and one-shot streams for clipboard. Mutual TLS uses self-signed certs pinned at pairing. Discovery is mDNS with a broadcast fallback.
- The `cursorjump` binary contains `win/` (server: owns physical input) and `mac/` (client: injects input). Each is cfg-gated, with thin OS glue that drives the core state machines.

**Tech Stack:** Rust stable (1.97+), tokio, quinn 0.11 + rustls 0.23 (ring) + rcgen, sha2, serde + postcard, toml, mdns-sd, `windows` crate (Win32), `objc2` / `objc2-app-kit` / `core-graphics` / `core-foundation` (macOS), tao + tray-icon + muda (tray/menu bar), eframe (settings window), rfd (dialogs), image (PNG/DIB), tracing. Exact versions are pinned by `Cargo.lock` in Task 1.

**Spec:** `docs/superpowers/specs/2026-10-18-cursorjump-design.md`

**Deviation from spec suggestion (allowed, since the spec leaves transport to the builder):** QUIC + pinned mutual TLS replaces the suggested Noise/`snow`. QUIC provides unreliable datagrams and multiplexed reliable streams on a single encrypted connection, plus built-in idle timeout. Phase 2 file transfer becomes additional streams. Pairing security is the same: a 6-digit code derived from both certificate fingerprints, so a man-in-the-middle shows mismatched codes.

## Global Constraints

- Clean-room: no CursorHop binaries, code or decompilation. Public OS docs and license-compatible open-source crates only.
- Platforms: Windows 10/11 (PC, wired) and macOS Tahoe 26 on Apple silicon (MacBook Pro M4 Pro, Wi-Fi). Both are personally owned and unmanaged.
- Roles are fixed: Windows = server (owns physical input), macOS = client (injects). The Mac's own keyboard and trackpad keep working, on the Mac only.
- "If it isn't seamless, it gets binned": no added latency or stutter, same mouse feel, no stuck or doubled keys, never strand the cursor. Only file transfer may be slow (phase 2).
- Edge mapping is **proportional along the whole edge**. Entry is the right edge of the PC's main monitor. Return is the Mac's whole left edge. The other Mac edges are walls. Configurable: side, vertical offset, pointer speed, Ctrl/Cmd swap.
- Gaming wall: no crossing when the cursor is clipped (`GetClipCursor` smaller than the virtual screen), the cursor is hidden (`GetCursorInfo`), or `SHQueryUserNotificationState == QUNS_RUNNING_D3D_FULL_SCREEN`.
- Anti-cheat: **no global hooks while the cursor is on the PC.** No `SendInput` on Windows, ever. `SetCursorPos`/`SetSystemCursor` only while on the Mac or at the moment of return. No DLL injection, overlays, or reading other processes' memory.
- Keyboard: US ANSI physical keys mapped from scancode to macOS keycode. Default PC Ctrl → Cmd, Win → Control, Alt → Option. Release all held keys and buttons on every crossing and on disconnect.
- Pairing: one-time 6-digit code confirmed on both screens, then the peer fingerprint is pinned permanently.
- Disconnect while on the Mac: the cursor snaps back to the PC in well under a second (idle timeout 600 ms).
- Mac: keep the system awake while connected (the display may sleep), wake the display on enter, and the PC keyboard must work at the Mac lock screen.
- Discovery: mDNS `_cursorjump._udp.local.`, with UDP broadcast fallback. Manual address is a hidden last resort.
- Ports: QUIC on the Mac at UDP 47800, discovery responder on the Mac at UDP 47801. The Mac listens and the PC connects, so the Windows side needs no inbound firewall rule.
- Config files: `%APPDATA%\CursorJump\config.toml` (Windows) and `~/Library/Application Support/CursorJump/config.toml` (Mac), with identity files alongside. The PC is authoritative for shared settings (side, offset, speed, scroll, swap).
- Windows process is Per-Monitor-DPI-Aware v2 (manifest), so all coordinates are physical pixels.
- Mac app bundle id `com.cursorjump.app`, `LSUIElement`, signed with the stable self-signed identity "CursorJump Dev" so TCC grants survive rebuilds.

## Review Focus

1. **A key or mouse button held at the moment of crossing (either direction), for example holding W or Shift.** Nothing should stay stuck on either machine. Pinned by `key_router_held_key_released_locally` (Task 5) and `held_drain_releases_everything` (Task 6).
2. **Wi-Fi drop or Mac sleep while the cursor is on the Mac.** The cursor should return to the PC and hooks should come off within ~600 ms, and reconnection should start. Pinned by `pc_disconnect_while_on_mac_returns` (Task 8) and `session_ends_when_peer_vanishes` (Task 10).
3. **Monitors at negative coordinates or with non-100% scaling.** The vertical monitor sits left of the primary, so it has negative x. The edge should still be found and fractions should still be correct. Pinned by `edge_frac_negative_origin` (Task 3).
4. **Clipboard extremes:** a 40 MB screenshot, an empty clipboard, emoji/CJK text, a file copied in Explorer (phase 2). Oversized content should be skipped without hanging, files ignored, and Unicode preserved. Pinned by `clipboard_over_limit_skipped`, `cf_html_multibyte_offsets` (Task 7) and `control_roundtrip_unicode` (Task 2).
5. **A second or unknown device on the LAN after pairing, or the Mac's IP changing.** No new pairing prompt for the known peer, unknown peers silently rejected, and reconnection to the pinned peer at its new address. Pinned by `pinned_rejects_stranger` and `pinned_accepts_known_new_addr` (Task 9).

---

## File Structure

```
Cargo.toml                         workspace
.github/workflows/ci.yml           windows + macos: build, cj-core/cj-net tests
crates/cj-core/src/
  lib.rs
  proto.rs        wire messages + postcard codec
  layout.rs       Rect, Side, edge fractions, push detection
  gaming.rs       game-lock rule
  keymap.rs       PC scancode → mac keycode, KeyRouter
  modifiers.rs    mac modifier flags, Held set
  input.rs        ClickTracker, motion/scroll scaling
  latency.rs      LatencyStats
  clipboard.rs    ClipboardData, CF_HTML, EchoGuard, DIB↔PNG, size limit
  config.rs       Config load/save
  pc_state.rs     PC orchestration state machine (pure reducer)
  mac_state.rs    Mac orchestration state machine (pure reducer)
crates/cj-net/src/
  lib.rs
  identity.rs     cert gen/persist, Fingerprint, pairing_code
  verify.rs       pinned / pairing-mode rustls verifiers
  session.rs      listen/connect, Session channels
  discovery.rs    mDNS advertise/browse + broadcast fallback
crates/cj-net/tests/loopback.rs
app/cursorjump/
  Cargo.toml, build.rs (Windows manifest), cursorjump.manifest
  src/main.rs     arg parsing: (default) agent | --settings | --diagnose
  src/paths.rs    config/identity dirs
  src/ui/tray.rs  tray/menu bar (tao + tray-icon + muda), both OSes
  src/ui/dialogs.rs  pairing dialog (rfd)
  src/ui/settings.rs eframe settings window (separate process)
  src/win/{mod.rs, monitors.rs, probe.rs, input_thread.rs, cursor.rs, clipboard.rs, autostart.rs, app.rs}
  src/mac/{mod.rs, inject.rs, power.rs, permissions.rs, clipboard.rs, autostart.rs, app.rs}
scripts/win/install.ps1
scripts/mac/{make-signing-cert.sh, bundle.sh, install.sh, Info.plist}
docs/ACCEPTANCE.md                 end-to-end manual checklist
```

**Machine column:** tasks marked **[Mac]** must be executed by Claude Code running on the MacBook (pull, build, test there). All other tasks run on the PC. CI (Task 1) compiles the macOS target on every push, so the PC session gets compile errors for cfg-gated Mac code.

---

### Task 1: Repository, workspace and CI

**Files:**
- Create: `Cargo.toml`, `crates/cj-core/{Cargo.toml,src/lib.rs}`, `crates/cj-net/{Cargo.toml,src/lib.rs}`, `app/cursorjump/{Cargo.toml,src/main.rs}`, `.github/workflows/ci.yml`, `rust-toolchain.toml` (stable), `README.md` (one paragraph + clean-room statement)
- Existing: `.gitignore`, `docs/`

**Interfaces:**
- Produces: crate names `cj-core`, `cj-net`, binary `cursorjump`; `cargo test -p cj-core -p cj-net` is the standard test command.

- [ ] **Step 1:** `git init`, add the workspace with three members. `main.rs` prints `cursorjump <version>`.
- [ ] **Step 2:** Run `cargo build --workspace`. Expected: success. `Cargo.lock` is committed.
- [ ] **Step 3:** Write `ci.yml`: jobs `windows-latest` and `macos-latest` each run `cargo build --workspace --locked` and `cargo test -p cj-core -p cj-net --locked`. Trigger: push and pull_request.
- [ ] **Step 4:** Commit, then `gh repo create cursorjump --private --source . --push`.
- [ ] **Step 5:** Run `gh run watch`. Expected: both jobs green.

---

### Task 2: Wire protocol (`cj-core::proto`)

**Files:** Create `crates/cj-core/src/proto.rs`; tests in-module.

**Interfaces:**
- Produces:
  ```rust
  pub const PROTOCOL_VERSION: u16 = 1;
  #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
  pub enum MouseButton { Left, Right, Middle, Back, Forward }
  #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
  pub enum Side { Left, Right, Top, Bottom }          // re-exported by layout
  #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
  pub enum Datagram {
      Motion { dx: i16, dy: i16 },                     // already speed-scaled by PC, Mac points
      Scroll { dx: i32, dy: i32 },                     // pixels, already scaled/reversed by PC
      Ping { seq: u32, sent_us: u64 },
      Pong { seq: u32, sent_us: u64 },
  }
  #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
  pub enum Control {
      Hello { version: u16, name: String },
      PairConfirm { accepted: bool },
      Enter { edge: Side, frac: f32 },                 // PC→Mac: edge of the MAC screen being entered
      Leave { frac: f32 },                             // Mac→PC: frac along the Mac's entry edge
      Button { button: MouseButton, down: bool },
      Key { mac_keycode: u16, down: bool },
      ReleaseAll,
  }
  pub fn encode_datagram(d: &Datagram) -> Vec<u8>;
  pub fn decode_datagram(b: &[u8]) -> Result<Datagram, ProtoError>;
  pub fn encode_control(c: &Control) -> Vec<u8>;      // u32 LE length prefix + postcard body
  pub fn decode_control_body(b: &[u8]) -> Result<Control, ProtoError>;  // body only, no prefix
  #[derive(Debug, thiserror::Error)] pub enum ProtoError { #[error("decode: {0}")] Decode(String) }
  ```

- [ ] **Step 1: Write the failing tests.**
  ```rust
  #[test] fn datagram_roundtrip_all_variants() { /* each variant: decode(encode(x)) == x */ }
  #[test] fn motion_datagram_is_tiny() { assert!(encode_datagram(&Datagram::Motion{dx:-300,dy:12}).len() <= 8); }
  #[test] fn control_roundtrip_unicode() {
      let c = Control::Hello{ version: 1, name: "Matt’s MacBook 🍎 東京".into() };
      let enc = encode_control(&c);
      assert_eq!(u32::from_le_bytes(enc[..4].try_into().unwrap()) as usize, enc.len() - 4);
      assert_eq!(decode_control_body(&enc[4..]).unwrap(), c);
  }
  #[test] fn garbage_is_error_not_panic() { assert!(decode_datagram(&[0xFF; 3]).is_err()); }
  ```
- [ ] **Step 2:** Run `cargo test -p cj-core proto`. Expected: FAIL (unresolved items).
- [ ] **Step 3:** Implement with `postcard::to_allocvec` / `from_bytes`.
- [ ] **Step 4:** Run `cargo test -p cj-core proto`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): wire protocol`.

---

### Task 3: Layout and edge math (`cj-core::layout`)

**Files:** Create `crates/cj-core/src/layout.rs`.

**Interfaces:**
- Consumes: `proto::Side`.
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct Rect { pub left: i32, pub top: i32, pub right: i32, pub bottom: i32 } // right/bottom exclusive
  pub fn opposite(s: Side) -> Side;
  pub fn edge_frac(r: &Rect, s: Side, x: i32, y: i32) -> f32;            // 0.0..=1.0 along the edge
  pub fn point_on_edge(r: &Rect, s: Side, frac: f32, inset: i32) -> (i32, i32); // `inset` px inside the edge
  pub fn is_pushing_out(r: &Rect, s: Side, x: i32, y: i32, dx: i32, dy: i32) -> bool;
  pub fn apply_offset(frac: f32, offset: f32) -> f32;                    // clamp(frac + offset, 0, 1)
  pub fn remove_offset(frac: f32, offset: f32) -> f32;                   // clamp(frac - offset, 0, 1)
  ```
  `edge_frac` for Left/Right uses `(y - top) / (height - 1)`. For Top/Bottom it uses x. `is_pushing_out` for Right means `x == right - 1 && dx > 0` and y is inside the rect (other sides are analogous).

- [ ] **Step 1: Write the failing tests.**
  ```rust
  const MAIN: Rect = Rect{ left: 0, top: 0, right: 2560, bottom: 1440 };
  #[test] fn edge_frac_right_middle() { assert!((edge_frac(&MAIN, Side::Right, 2559, 720) - 0.5).abs() < 0.001); }
  #[test] fn edge_frac_negative_origin() {
      let vert = Rect{ left: -1080, top: -400, right: 0, bottom: 1520 };
      assert!((edge_frac(&vert, Side::Left, -1080, -400)).abs() < 1e-6);
      assert!((edge_frac(&vert, Side::Left, -1080, 1519) - 1.0).abs() < 1e-6);
  }
  #[test] fn point_on_edge_inverts_frac() { assert_eq!(point_on_edge(&MAIN, Side::Right, 0.5, 2), (2557, 720)); }
  #[test] fn pushing_out_only_at_edge_moving_out() {
      assert!(is_pushing_out(&MAIN, Side::Right, 2559, 100, 3, 0));
      assert!(!is_pushing_out(&MAIN, Side::Right, 2559, 100, -3, 0));
      assert!(!is_pushing_out(&MAIN, Side::Right, 2558, 100, 3, 0));
  }
  #[test] fn offset_clamps() { assert_eq!(apply_offset(0.9, 0.2), 1.0); assert_eq!(remove_offset(0.1, 0.2), 0.0); }
  #[test] fn opposite_sides() { assert_eq!(opposite(Side::Right), Side::Left); }
  ```
- [ ] **Step 2:** Run `cargo test -p cj-core layout`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cj-core layout`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): edge layout math`.

---

### Task 4: Gaming rule (`cj-core::gaming`)

**Files:** Create `crates/cj-core/src/gaming.rs`.

**Interfaces:**
- Consumes: `layout::Rect`.
- Produces:
  ```rust
  pub struct GameProbe { pub clip: Rect, pub virtual_screen: Rect, pub cursor_showing: bool, pub d3d_fullscreen: bool }
  pub fn game_locked(p: &GameProbe) -> bool;
  ```

- [ ] **Step 1: Write the failing tests.** `desktop_not_locked` (clip == virtual, showing, not fullscreen → false), `clipped_is_locked` (clip = main monitor only → true), `hidden_cursor_is_locked`, `d3d_fullscreen_is_locked`.
- [ ] **Step 2:** Run `cargo test -p cj-core gaming`. Expected: FAIL.
- [ ] **Step 3:** Implement. Locked if the clip rect is strictly smaller than the virtual screen in any dimension, or `!cursor_showing`, or `d3d_fullscreen`.
- [ ] **Step 4:** Run `cargo test -p cj-core gaming`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): gaming wall rule`.

---

### Task 5: Key mapping and routing (`cj-core::keymap`)

**Files:** Create `crates/cj-core/src/keymap.rs`.

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
  pub struct PcKey { pub scancode: u16, pub extended: bool }
  pub fn pc_to_mac(k: PcKey, swap_ctrl_cmd: bool) -> Option<u16>;    // macOS kVK_* code
  pub enum Route { Local, Remote }
  pub struct KeyRouter { /* keys physically down on Windows before entering the Mac */ }
  impl KeyRouter {
      pub fn new() -> Self;
      pub fn on_enter_mac(&mut self, down_now: impl IntoIterator<Item = PcKey>);
      pub fn route(&mut self, k: PcKey, down: bool) -> Route;          // while on the Mac
      pub fn clear(&mut self);
  }
  pub fn is_return_hotkey(held: &std::collections::HashSet<PcKey>, k: PcKey, down: bool) -> bool; // Ctrl+Alt+Home
  ```
  The table covers the full US ANSI 104-key set (letters, digits, punctuation, F1–F15, navigation, arrows, numpad, modifiers). PrintScreen → F13 (0x69), ScrollLock → F14 (0x6B), Pause → F15 (0x71). Unmapped keys → `None` (dropped).

- [ ] **Step 1: Write the failing tests.**
  ```rust
  fn k(s: u16, e: bool) -> PcKey { PcKey{ scancode: s, extended: e } }
  #[test] fn letters_and_basics() {
      assert_eq!(pc_to_mac(k(0x1E,false), true), Some(0x00)); // A
      assert_eq!(pc_to_mac(k(0x1C,false), true), Some(0x24)); // Enter
      assert_eq!(pc_to_mac(k(0x29,false), true), Some(0x32)); // `
      assert_eq!(pc_to_mac(k(0x2B,false), true), Some(0x2A)); // backslash
      assert_eq!(pc_to_mac(k(0x3B,false), true), Some(0x7A)); // F1
  }
  #[test] fn extended_disambiguates() {
      assert_eq!(pc_to_mac(k(0x4B,true),  true), Some(0x7B)); // Left arrow
      assert_eq!(pc_to_mac(k(0x4B,false), true), Some(0x56)); // Numpad 4
      assert_eq!(pc_to_mac(k(0x53,true),  true), Some(0x75)); // Delete → forward delete
  }
  #[test] fn modifiers_swap() {
      assert_eq!(pc_to_mac(k(0x1D,false), true),  Some(0x37)); // LCtrl → Cmd
      assert_eq!(pc_to_mac(k(0x1D,true),  true),  Some(0x36)); // RCtrl → RCmd
      assert_eq!(pc_to_mac(k(0x5B,true),  true),  Some(0x3B)); // LWin → Control
      assert_eq!(pc_to_mac(k(0x1D,false), false), Some(0x3B)); // no swap: LCtrl → Control
      assert_eq!(pc_to_mac(k(0x5B,true),  false), Some(0x37)); // no swap: LWin → Cmd
      assert_eq!(pc_to_mac(k(0x38,false), true),  Some(0x3A)); // LAlt → Option
      assert_eq!(pc_to_mac(k(0x38,true),  true),  Some(0x3D)); // RAlt → ROption
  }
  #[test] fn unknown_is_none() { assert_eq!(pc_to_mac(k(0x7F,false), true), None); }
  #[test] fn key_router_held_key_released_locally() {
      let mut r = KeyRouter::new();
      r.on_enter_mac([k(0x11,false)]);                       // W held while crossing
      assert!(matches!(r.route(k(0x11,false), false), Route::Local));  // its key-up reaches Windows
      assert!(matches!(r.route(k(0x11,false), true),  Route::Remote)); // pressed again → Mac
  }
  #[test] fn return_hotkey() { /* held {LCtrl, LAlt} + Home(0x47,ext) down → true; without Alt → false */ }
  ```
- [ ] **Step 2:** Run `cargo test -p cj-core keymap`. Expected: FAIL.
- [ ] **Step 3:** Implement the table as a `match` on `(scancode, extended)`.
- [ ] **Step 4:** Run `cargo test -p cj-core keymap`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): US ANSI keymap and key routing`.

---

### Task 6: Mac-side input helpers (`cj-core::modifiers`, `cj-core::input`)

**Files:** Create `crates/cj-core/src/modifiers.rs`, `crates/cj-core/src/input.rs`.

**Interfaces:**
- Consumes: `proto::MouseButton`.
- Produces:
  ```rust
  // modifiers.rs — CGEventFlags bits
  pub const FLAG_CAPS: u64 = 0x10000; pub const FLAG_SHIFT: u64 = 0x20000; pub const FLAG_CONTROL: u64 = 0x40000;
  pub const FLAG_OPTION: u64 = 0x80000; pub const FLAG_COMMAND: u64 = 0x100000;
  pub fn is_modifier(mac_keycode: u16) -> bool;
  pub struct ModifierState; impl ModifierState { pub fn new() -> Self; pub fn update(&mut self, kc: u16, down: bool); pub fn flags(&self) -> u64; }
  pub struct Held; impl Held {
      pub fn new() -> Self; pub fn key(&mut self, kc: u16, down: bool); pub fn button(&mut self, b: MouseButton, down: bool);
      pub fn any_button(&self) -> bool;
      pub fn drain(&mut self) -> (Vec<u16>, Vec<MouseButton>);        // everything still down, then empty
  }
  // input.rs
  pub struct ClickTracker; impl ClickTracker {
      pub fn new(interval: std::time::Duration, max_dist: f64) -> Self;
      pub fn on_down(&mut self, b: MouseButton, pos: (f64, f64), now: std::time::Instant) -> i64;  // click count 1,2,3…
  }
  pub fn scale_motion(dx: i32, dy: i32, speed: f32) -> (i16, i16);       // round, saturate to i16
  pub fn wheel_to_pixels(delta: i32, scroll_speed: f32, reverse: bool) -> i32; // 120 units = 48 px at speed 1.0
  ```
  Left and right variants of a modifier each keep their own state, and a flag stays set while either is down. Caps Lock toggles on key-down.

- [ ] **Step 1: Write the failing tests.**
  - `cmd_flag_set_and_cleared` (0x37 down → `flags() & FLAG_COMMAND != 0`, up → 0).
  - `left_right_independent` (0x37 and 0x36 down, 0x37 up → still `FLAG_COMMAND`).
  - `caps_toggles`.
  - `held_drain_releases_everything` (keys 0x00 and 0x37 plus button Left down → drain returns them all, then a second drain is empty).
  - `double_click_within_interval` (500 ms, 4.0 px: downs 200 ms apart same spot → 1 then 2, a third → 3).
  - `slow_second_click_resets` (600 ms → 1).
  - `moved_click_resets` (10 px → 1).
  - `other_button_resets`.
  - `scale_saturates` (`scale_motion(40000,0,1.0).0 == i16::MAX`).
  - `wheel_conversion` (`wheel_to_pixels(120,1.0,false) == 48`, `(60,1.0,false) == 24`, `(120,1.0,true) == -48`, `(120,2.0,false) == 96`).
- [ ] **Step 2:** Run `cargo test -p cj-core modifiers input`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cj-core modifiers input`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): modifier, held-key, click and scroll helpers`.

---

### Task 7: Latency, clipboard model and config (`cj-core::{latency, clipboard, config}`)

**Files:** Create `crates/cj-core/src/latency.rs`, `clipboard.rs`, `config.rs`.

**Interfaces:**
- Produces:
  ```rust
  // latency.rs
  pub enum WifiVerdict { Good, Spiky }
  pub struct LatencyStats; impl LatencyStats {
      pub fn new() -> Self;                                   // 60 s rolling window
      pub fn record(&mut self, now: Instant, rtt: Duration);
      pub fn median_ms(&self) -> Option<f32>; pub fn spikes(&self) -> usize;   // rtt > 40 ms
      pub fn verdict(&self) -> WifiVerdict;                   // Spiky if spikes >= 3 in window
      pub fn summary(&self) -> String;                        // e.g. "Latency 3 ms · 0 spikes/min"
  }
  // clipboard.rs
  #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
  pub struct ClipboardData { pub text: Option<String>, pub html: Option<String>, pub rtf: Option<Vec<u8>>, pub png: Option<Vec<u8>> }
  impl ClipboardData { pub fn is_empty(&self) -> bool; pub fn byte_len(&self) -> usize; pub fn content_hash(&self) -> u64; }
  pub fn within_limit(d: &ClipboardData, max_bytes: u64) -> bool;
  pub fn cf_html_wrap(fragment: &str) -> String;              // Windows "HTML Format" with correct byte offsets
  pub fn cf_html_extract(cf: &str) -> Option<String>;         // fragment between StartFragment/EndFragment
  pub fn dib_to_png(dib: &[u8]) -> anyhow::Result<Vec<u8>>;   // CF_DIB/CF_DIBV5: 24/32 bpp, top-down & bottom-up, BI_RGB/BI_BITFIELDS
  pub fn png_to_dib(png: &[u8]) -> anyhow::Result<Vec<u8>>;   // 32 bpp BITMAPV5HEADER, bottom-up, with alpha
  pub struct EchoGuard; impl EchoGuard {
      pub fn new() -> Self; pub fn mark_applied(&mut self, hash: u64); // we just wrote remote data locally
      pub fn should_send(&mut self, hash: u64) -> bool;                // false if == last applied or last sent
  }
  // config.rs
  #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] #[serde(default)]
  pub struct Config {
      pub mac_side: Side,             // Right
      pub edge_offset: f32,           // 0.0, clamp -0.5..=0.5
      pub pointer_speed: f32,         // 1.0, clamp 0.25..=4.0
      pub scroll_speed: f32,          // 1.0, clamp 0.25..=4.0
      pub reverse_scroll: bool,       // false
      pub swap_ctrl_cmd: bool,        // true
      pub paused: bool,               // false
      pub autostart: bool,            // true
      pub reduce_wifi_hiccups: bool,  // false (Mac only)
      pub clipboard_max_bytes: u64,   // 33_554_432
      pub paired_peer: Option<String>,// hex fingerprint
      pub manual_peer_addr: Option<String>, // hidden last resort, e.g. "192.168.1.20:47800"
  }
  impl Config { pub fn load(path: &Path) -> Config; pub fn save(&self, path: &Path) -> std::io::Result<()>; pub fn clamped(self) -> Config; }
  ```
  `load` never panics: a missing or corrupt file returns the defaults (logged). `save` writes atomically (temp file + rename).

- [ ] **Step 1: Write the failing tests.**
  - `median_and_spikes` (record 10, 10, 50, 10 ms → median 10, spikes 1, Good; add two more 60 ms → Spiky).
  - `old_samples_expire` (a sample 61 s old is not counted).
  - `cf_html_multibyte_offsets`: `let s = cf_html_wrap("<b>café ☕</b>");` parse StartFragment/EndFragment and assert `&s.as_bytes()[sf..ef] == "<b>café ☕</b>".as_bytes()`, plus `cf_html_extract(&s).as_deref() == Some("<b>café ☕</b>")`.
  - `cf_html_extract_real_world` (a CF_HTML string captured from Chrome with `<!--StartFragment-->` markers, hard-coded in the test → the expected fragment).
  - `clipboard_over_limit_skipped` (`png` of 40 MB, limit 32 MiB → `within_limit == false`).
  - `dib_png_roundtrip_32bpp` and `dib_bottom_up_24bpp_to_png` (2×2 known pixels → `image::load_from_memory` gives the same RGBA).
  - `echo_guard_blocks_reflection` (`mark_applied(h)` → `should_send(h) == false`, `should_send(h2) == true`, then `should_send(h2) == false`).
  - `config_defaults_on_garbage` (write "not toml {{" → `load` == `Config::default()`).
  - `config_roundtrip` and `config_clamps` (pointer_speed 10.0 → 4.0).
- [ ] **Step 2:** Run `cargo test -p cj-core latency clipboard config`. Expected: FAIL.
- [ ] **Step 3:** Implement. Hash with `std::hash::DefaultHasher` over all fields.
- [ ] **Step 4:** Run `cargo test -p cj-core`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): latency stats, clipboard model, config`.

---

### Task 8: Orchestration state machines (`cj-core::{pc_state, mac_state}`)

These pure reducers own all behavioural decisions, so that OS glue stays thin and the decisions stay tested.

**Files:** Create `crates/cj-core/src/pc_state.rs`, `crates/cj-core/src/mac_state.rs`.

**Interfaces:**
- Consumes: `proto::{Control, Side}`, `layout::*`, `gaming::GameProbe`, `config::Config`.
- Produces:
  ```rust
  // pc_state.rs
  pub enum Link { Searching, Pairing { code: String }, Connected }
  pub enum Where { Pc, Mac }
  pub enum PcEvent {
      Connected { paired: bool, code: String }, PairAnswered { local: bool, remote: Option<bool> },
      Disconnected,
      EdgePush { frac: f32, game: GameProbe, buttons_held: bool },  // input thread saw a push past the configured edge
      MacLeave { frac: f32 },
      ReturnHotkey, HookLost, ConfigChanged(Config),
  }
  pub enum PcAction {
      ShowPairDialog { code: String }, PersistPairing,
      EnterMac { mac_edge: Side, frac: f32 },        // input thread: install hooks, park+hide cursor; net: send Enter
      ReturnToPc { pc_frac: f32 },                    // input thread: remove hooks, restore cursor at frac on the PC edge
      SendReleaseAll, Reconnect,
  }
  pub struct PcMachine; impl PcMachine {
      pub fn new(cfg: Config) -> Self; pub fn on(&mut self, ev: PcEvent) -> Vec<PcAction>;
      pub fn link(&self) -> &Link; pub fn where_(&self) -> &Where;
  }
  // mac_state.rs
  pub enum MacEvent { Enter { edge: Side, frac: f32 }, PushOut { frac: f32, buttons_held: bool }, ReleaseAll, Disconnected }
  pub enum MacAction { WakeDisplay, WarpTo { edge: Side, frac: f32 }, SendLeave { frac: f32 }, ReleaseAllInjected, StartControl, StopControl }
  pub struct MacMachine; impl MacMachine {
      pub fn new() -> Self; pub fn on(&mut self, ev: MacEvent) -> Vec<MacAction>;
      pub fn entry_edge(&self) -> Option<Side>; pub fn controlled(&self) -> bool;
  }
  ```
  PC rules:
  - `EdgePush` enters only if Connected, `!paused`, `!buttons_held` and `!game_locked`. It emits `EnterMac { mac_edge: opposite(cfg.mac_side), frac: apply_offset(frac, offset) }`.
  - `MacLeave` emits `ReturnToPc { pc_frac: remove_offset(frac, offset) }`.
  - `Disconnected` / `ReturnHotkey` / `HookLost` while on the Mac emit `ReturnToPc { pc_frac: last_frac }`, plus `SendReleaseAll` for the hotkey and hook-lost cases (not for disconnect). `Disconnected` always emits `Reconnect`.
  - Pairing completes only when both local and remote accept.

  Mac rules:
  - `Enter` emits `[WakeDisplay, WarpTo, StartControl]`.
  - `PushOut` with buttons held emits nothing. Otherwise it emits `[ReleaseAllInjected, SendLeave, StopControl]`.
  - `Disconnected` while controlled emits `[ReleaseAllInjected, StopControl]`.

- [ ] **Step 1: Write the failing tests.**
  - `pc_edge_push_enters_when_clear` (asserts `EnterMac{ mac_edge: Side::Left, frac ≈ 0.3 }` for frac 0.3, offset 0).
  - `pc_edge_push_blocked_in_game`.
  - `pc_edge_push_blocked_when_paused`.
  - `pc_edge_push_blocked_with_button_held`.
  - `pc_edge_push_ignored_while_pairing`.
  - `pc_offset_applied_and_removed` (offset 0.1: enter with frac 0.5 → 0.6; `MacLeave{0.6}` → `ReturnToPc{0.5}`).
  - `pc_disconnect_while_on_mac_returns` (→ contains `ReturnToPc` and `Reconnect`, `where_ == Pc`).
  - `pc_hotkey_returns_and_releases`.
  - `pc_hook_lost_returns`.
  - `pc_pairing_needs_both` (local yes + remote no → no `PersistPairing`, `Reconnect`).
  - `mac_enter_wakes_and_warps`.
  - `mac_push_out_with_button_held_is_wall`.
  - `mac_disconnect_releases_all`.
- [ ] **Step 2:** Run `cargo test -p cj-core pc_state mac_state`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cj-core`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): PC and Mac state machines`.

---

### Task 9: Identity, pairing code and pinned verifiers (`cj-net::{identity, verify}`)

**Files:** Create `crates/cj-net/src/identity.rs`, `crates/cj-net/src/verify.rs`; tests in-module.

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub struct Fingerprint(pub [u8; 32]);   // SHA-256 of cert DER
  impl Fingerprint { pub fn to_hex(&self) -> String; pub fn from_hex(s: &str) -> Option<Self>; }
  pub struct Identity { pub cert_der: Vec<u8>, pub key_der: Vec<u8> }
  impl Identity { pub fn load_or_create(dir: &Path) -> anyhow::Result<Identity>; pub fn fingerprint(&self) -> Fingerprint; }
  pub fn pairing_code(a: &Fingerprint, b: &Fingerprint) -> String;  // order-independent, "NNN NNN"
  #[derive(Clone)] pub enum Trust { PairingMode, Pinned(Fingerprint) }
  pub fn server_config(id: &Identity, trust: Trust) -> anyhow::Result<rustls::ServerConfig>;  // requires client cert
  pub fn client_config(id: &Identity, trust: Trust) -> anyhow::Result<rustls::ClientConfig>;
  ```
  - `pairing_code` is the first 4 bytes of SHA-256(min(a,b) ‖ max(a,b)), taken as a u32 mod 1 000 000 and zero-padded.
  - The verifiers accept any well-formed self-signed cert in `PairingMode` and only the matching fingerprint in `Pinned`. TLS handshake signatures are always verified with the ring provider, so the peer must hold the private key. The SNI name is fixed as `cursorjump.local`.
  - The identity is stored as `identity.der` and `identity.key` in `dir`.

- [ ] **Step 1: Write the failing tests.**
  - `identity_persists` (create in a temp dir, load again → same fingerprint).
  - `pairing_code_symmetric_and_formatted` (`code(a,b) == code(b,a)`, matches the regex `^\d{3} \d{3}$`).
  - `different_peer_different_code` (`code(a,b) != code(a,c)` for fixed test fingerprints).
  - `pinned_rejects_stranger` and `pinned_accepts_known_new_addr` are integration tests in Task 10, listed here because this task's verifiers decide them.
- [ ] **Step 2:** Run `cargo test -p cj-net identity`. Expected: FAIL.
- [ ] **Step 3:** Implement with rcgen (P-256 / ECDSA) and custom `ServerCertVerifier` / `ClientCertVerifier`.
- [ ] **Step 4:** Run `cargo test -p cj-net identity`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(net): identity, pairing code, pinned TLS verifiers`.

---

### Task 10: QUIC session (`cj-net::session`)

**Files:** Create `crates/cj-net/src/session.rs`, `crates/cj-net/tests/loopback.rs`.

**Interfaces:**
- Consumes: Task 9 configs, `cj-core::proto`, `cj-core::clipboard::ClipboardData`.
- Produces:
  ```rust
  pub struct Listener; impl Listener {
      pub async fn bind(id: &Identity, trust: Trust, addr: SocketAddr) -> anyhow::Result<Listener>;
      pub async fn accept(&self) -> anyhow::Result<Session>;
      pub fn set_trust(&self, trust: Trust) -> anyhow::Result<()>;           // after pairing
      pub fn local_addr(&self) -> SocketAddr;
  }
  pub async fn connect(id: &Identity, trust: Trust, addr: SocketAddr) -> anyhow::Result<Session>;
  pub struct Session; impl Session {                                         // Clone (Arc inside)
      pub fn peer(&self) -> Fingerprint;
      pub fn send_datagram(&self, d: &Datagram) -> anyhow::Result<()>;
      pub async fn recv_datagram(&self) -> anyhow::Result<Datagram>;
      pub async fn send_control(&self, c: &Control) -> anyhow::Result<()>;   // one long-lived bi stream, opened by connector
      pub async fn recv_control(&self) -> anyhow::Result<Control>;
      pub async fn send_clipboard(&self, d: &ClipboardData) -> anyhow::Result<()>; // fresh uni stream per item
      pub async fn recv_clipboard(&self, max_bytes: u64) -> anyhow::Result<ClipboardData>;
      pub async fn closed(&self);                                              // resolves on close/idle-timeout
      pub fn close(&self);
  }
  ```
  Transport settings:
  - `max_idle_timeout` 600 ms, `keep_alive_interval` 100 ms.
  - Datagram receive buffer 64 KiB.
  - Disable Nagle-like pacing delays for datagrams by keeping the defaults but sending immediately (no batching).
  - The control stream is opened by the connector and announced with an initial `Hello`.

- [ ] **Step 1: Write the failing tests** in `tests/loopback.rs` (all on `127.0.0.1:0`).
  - `pairing_mode_connects_and_codes_match` (both sides compute `pairing_code(own, peer)`, codes equal).
  - `pinned_accepts_known_new_addr` (pin B on the server, B connects from a fresh client port → ok).
  - `pinned_rejects_stranger` (pin B, C connects → `connect` errs, or the session closes before `Hello`).
  - `datagram_and_control_flow` (Motion datagram and Key control arrive intact).
  - `clipboard_transfer_and_limit` (1 MB text arrives; a 2 MB item with `max_bytes = 1 MB` → recv errs, connection stays up, a following small item still arrives).
  - `session_ends_when_peer_vanishes` (drop the server endpoint without closing → `client.closed()` resolves within 1 s).
- [ ] **Step 2:** Run `cargo test -p cj-net --test loopback`. Expected: FAIL.
- [ ] **Step 3:** Implement on quinn.
- [ ] **Step 4:** Run `cargo test -p cj-net`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(net): QUIC session with datagrams, control and clipboard streams`.

---

### Task 11: Discovery (`cj-net::discovery`)

**Files:** Create `crates/cj-net/src/discovery.rs`; add a test to `tests/loopback.rs`.

**Interfaces:**
- Produces:
  ```rust
  pub const QUIC_PORT: u16 = 47800; pub const DISCOVERY_PORT: u16 = 47801;
  pub const SERVICE: &str = "_cursorjump._udp.local.";
  pub struct Peer { pub addr: SocketAddr, pub fingerprint: Fingerprint, pub name: String }
  pub struct Advertiser; impl Advertiser {                          // Mac
      pub fn start(name: &str, port: u16, fp: &Fingerprint, responder_port: u16) -> anyhow::Result<Advertiser>; // mDNS + broadcast responder
  }
  pub async fn discover(timeout: Duration, broadcast_port: u16) -> anyhow::Result<Vec<Peer>>;  // PC
  ```
  - mDNS TXT records `fp=<hex>` and `v=1`.
  - Fallback: if mDNS yields nothing within 1.5 s, send the UDP broadcast `b"CJ?1"` to `255.255.255.255:broadcast_port` and to every interface's directed broadcast. The responder replies unicast `b"CJ!1" + postcard(Peer)`.
  - The PC prefers the peer whose fingerprint equals `paired_peer`. With no pairing it takes the first one found.

- [ ] **Step 1: Write the failing test.** `broadcast_fallback_finds_responder`: start a responder only (no mDNS) on a test port, then `discover(3s, test_port)` returns its fingerprint. (mDNS is verified manually in Task 18, since CI runners often block multicast.)
- [ ] **Step 2:** Run `cargo test -p cj-net --test loopback broadcast`. Expected: FAIL.
- [ ] **Step 3:** Implement with `mdns-sd` and `tokio::net::UdpSocket` (`set_broadcast(true)`).
- [ ] **Step 4:** Run `cargo test -p cj-net`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(net): mDNS discovery with broadcast fallback`.

---

### Task 12: Windows input thread (`app/cursorjump/src/win/`)

**Files:** Create `win/{mod.rs, monitors.rs, probe.rs, input_thread.rs, cursor.rs}`, `build.rs`, `cursorjump.manifest`.

**Interfaces:**
- Consumes: `layout`, `gaming::GameProbe`, `keymap::{PcKey, KeyRouter, pc_to_mac, is_return_hotkey}`, `input::{scale_motion, wheel_to_pixels}`, `pc_state::{PcEvent, PcAction}`.
- Produces:
  ```rust
  pub enum InputMsg {                         // input thread → app
      Edge(PcEvent),                          // only PcEvent::EdgePush / ReturnHotkey / HookLost
      Motion { dx: i16, dy: i16 }, Scroll { dx: i32, dy: i32 },
      Button { button: MouseButton, down: bool }, Key { mac_keycode: u16, down: bool },
  }
  pub enum InputCmd { EnterMac, ReturnToPc { pc_frac: f32 }, ApplyConfig(Config), Quit }   // app → input thread
  pub fn spawn_input_thread(cfg: Config) -> (std::sync::mpsc::Sender<InputCmd>, crossbeam_channel::Receiver<InputMsg>);
  pub fn monitors() -> Vec<(Rect, bool /*primary*/)>;     // monitors.rs, physical px
  pub fn edge_monitor(mons: &[(Rect,bool)], side: Side) -> Option<Rect>;  // primary monitor, if `side` is not adjacent to another monitor
  pub fn probe_game() -> GameProbe;                        // probe.rs
  pub fn restore_system_cursors();                         // cursor.rs: SystemParametersInfoW(SPI_SETCURSORS)
  ```
  Behaviour (the decisions the implementer can't infer):
  - The thread owns a hidden message-only window with Raw Input registered for mouse (`RIDEV_INPUTSINK`), plus its own `GetMessage` loop. `InputCmd` is delivered via a channel and a `PostThreadMessageW(WM_APP)` wake-up.
  - **On the PC:** no hooks. On each `WM_INPUT` with relative motion, read `GetCursorPos`. If `is_pushing_out(edge_rect, cfg.mac_side, x, y, dx, dy)`, emit `EdgePush { frac, game: probe_game(), buttons_held }`, where `buttons_held` comes from `GetAsyncKeyState` on VK_LBUTTON/RBUTTON/MBUTTON.
  - **`EnterMac`:**
    1. Record the keys currently down (`GetAsyncKeyState` over VK 0x08..0xFE mapped with `MapVirtualKeyW(MAPVK_VK_TO_VSC_EX)`) into `KeyRouter::on_enter_mac`.
    2. Install `WH_MOUSE_LL` and `WH_KEYBOARD_LL`.
    3. `SetCursorPos` to the edge monitor's centre.
    4. Hide the cursor with `SetSystemCursor` (a blank 32×32 cursor for every `OCR_*`).
  - **Hooks:** skip events flagged `LLMHF_INJECTED` / `LLKHF_INJECTED`. For motion, `delta = pt − centre`, then `scale_motion` and emit `Motion`. Swallow (`return 1`). Wheel and hwheel go through `wheel_to_pixels`, then `Scroll`. Buttons map to `Button` (XBUTTON1 = Back, XBUTTON2 = Forward). Keys go to `KeyRouter::route`: `Local` → `CallNextHookEx`, `Remote` → `pc_to_mac` then `Key`, and swallow. The return hotkey → `Edge(ReturnHotkey)`. **Hook callbacks only push to the channel and return. No allocation-heavy work and no locks beyond the channel.**
  - **Watchdog:** while on the Mac, if raw input reports motion but the mouse hook has seen nothing for 250 ms, emit `HookLost`.
  - **`ReturnToPc`:** remove the hooks, `restore_system_cursors()`, `SetCursorPos(point_on_edge(edge_rect, mac_side, pc_frac, 2))`, `KeyRouter::clear()`.
  - At process start and in a panic hook, call `restore_system_cursors()`.
  - The manifest declares `<dpiAwareness>PerMonitorV2</dpiAwareness>`. `build.rs` embeds it (the `embed-manifest` crate).
  - `cursorjump --diagnose` runs this thread with a stub app that logs every `InputMsg` and simulates `EnterMac`/`ReturnToPc` on the edge push and the return hotkey.

- [ ] **Step 1: Write the failing tests.** `edge_monitor_picks_primary_right` (layout `[vertical(-1080..0), primary(0..2560)]`, Right → primary); `edge_monitor_none_when_adjacent` (a monitor to the right of the primary → None for Right). These are pure functions over rect lists and run under `cargo test -p cursorjump` on Windows.
- [ ] **Step 2:** Run `cargo test -p cursorjump win::monitors`. Expected: FAIL.
- [ ] **Step 3:** Implement `monitors.rs` and `probe.rs`, then `input_thread.rs` and `cursor.rs` as described.
- [ ] **Step 4:** Run `cargo test -p cursorjump`. Expected: PASS.
- [ ] **Step 5: Manual verification.** Run `cargo run -p cursorjump -- --diagnose` and check each item:
  - Pushing right on the main monitor logs `EdgePush` with a frac that matches the height (top ≈ 0, bottom ≈ 1), and the cursor disappears.
  - Moving the mouse logs `Motion`, and typing logs `Key` with Mac codes. Windows receives no input meanwhile.
  - Ctrl+Alt+Home brings the cursor back at the same height.
  - Holding W while crossing leaves no stuck W in Notepad.
  - The left vertical monitor behaves as before.
  - The process uses under 1 % CPU when idle.
- [ ] **Step 6:** Commit `feat(win): input thread with passive edge detection and on-Mac capture`.

---

### Task 13: Windows clipboard (`win/clipboard.rs`)

**Files:** Create `app/cursorjump/src/win/clipboard.rs`.

**Interfaces:**
- Consumes: `clipboard::{ClipboardData, cf_html_wrap, cf_html_extract, dib_to_png, png_to_dib, EchoGuard}`.
- Produces:
  ```rust
  pub fn spawn_clipboard_watcher() -> crossbeam_channel::Receiver<ClipboardData>;  // AddClipboardFormatListener on a hidden window
  pub fn read_clipboard() -> anyhow::Result<ClipboardData>;
  pub fn write_clipboard(d: &ClipboardData) -> anyhow::Result<()>;
  ```
  - Formats read: `CF_UNICODETEXT`, registered `"HTML Format"` (via `cf_html_extract`), `"Rich Text Format"`, registered `"PNG"` (preferred) else `CF_DIBV5`/`CF_DIB` (via `dib_to_png`).
  - Formats written: all present formats. Images are written as both `"PNG"` and `CF_DIBV5`.
  - `CF_HDROP` (files) is ignored in phase 1.
  - `OpenClipboard` is retried up to 10 × 10 ms when another app holds it.

- [ ] **Step 1: Write the failing test** (Windows-only, `#[cfg(windows)]`): `clipboard_roundtrip_text_html` writes `{text:"héllo ✓", html:"<i>héllo</i>"}`, reads it back, and both fields match.
- [ ] **Step 2:** Run `cargo test -p cursorjump win::clipboard -- --test-threads=1`. Expected: FAIL.
- [ ] **Step 3:** Implement with the `windows` crate.
- [ ] **Step 4:** Run the same command. Expected: PASS. Then manual: copy an image in Paint, `--diagnose` logs `ClipboardData { png: Some(..) }`.
- [ ] **Step 5:** Commit `feat(win): clipboard read/write/watch`.

---

### Task 14: Windows app wiring, tray, settings, autostart (`win/app.rs`, `ui/*`)

**Files:** Create `win/app.rs`, `win/autostart.rs`, `ui/tray.rs`, `ui/dialogs.rs`, `ui/settings.rs`, `paths.rs`, `scripts/win/install.ps1`; modify `main.rs`.

**Interfaces:**
- Consumes: everything above.
- Produces:
  ```rust
  pub fn run_pc() -> anyhow::Result<()>;          // win/app.rs; called from main on Windows
  pub struct TrayModel { pub status: String, pub latency: String, pub paused: bool }   // ui/tray.rs, shared with Mac
  pub fn run_tray(model_rx: Receiver<TrayModel>, on_menu: impl Fn(TrayCmd) + 'static) -> !;   // tao event loop on main thread
  pub enum TrayCmd { TogglePause, OpenSettings, ForgetPairing, Quit }
  pub async fn ask_pairing(code: &str, peer_name: &str) -> bool;          // ui/dialogs.rs (rfd)
  pub fn run_settings(cfg_path: &Path, is_mac: bool) -> anyhow::Result<()>;  // ui/settings.rs, eframe
  pub fn set_autostart(enabled: bool) -> anyhow::Result<()>;               // HKCU\...\Run "CursorJump"
  ```
  Wiring:
  - Main thread runs the tray. A tokio runtime on a worker thread runs the loop below, and `PcMachine` lives inside that loop.
  - Loop: `discover` (prefer the paired fingerprint, fall back to `manual_peer_addr`) → `connect` with `Trust::Pinned(paired)` or `PairingMode` → feed `PcEvent`s, execute `PcAction`s.
  - `EnterMac` → `InputCmd::EnterMac` + `send_control(Enter)`. Forward `InputMsg::Motion/Scroll` as datagrams and `Button/Key` as control messages. `ReturnToPc` → `InputCmd::ReturnToPc`.
  - Pings: every 100 ms send `Ping`. On `Pong`, `LatencyStats::record`. The tray shows `summary()`, plus "Wi-Fi is spiky — an Ethernet adapter would help" when `Spiky`.
  - Clipboard: on a watcher item → `within_limit` && `EchoGuard::should_send` → `send_clipboard`. On receive → `write_clipboard` + `mark_applied`.
  - Reconnect backoff: 250 ms, 500 ms, 1 s, then every 2 s.
  - Config hot-reload: poll the mtime every 500 ms → `ConfigChanged` + `InputCmd::ApplyConfig`.
  - Single instance: named mutex `Local\CursorJump`.
  - `OpenSettings` spawns `cursorjump --settings`.
  - Settings window controls:
    - Mac side (Left/Right/Top/Bottom)
    - edge offset slider −50..+50 %
    - pointer speed 0.25–4×
    - scroll speed 0.25–4×
    - reverse scroll
    - swap Ctrl/Cmd
    - start at login
    - "Forget paired computer"
    - Mac only: "Reduce Wi-Fi hiccups (turns off AirDrop while connected)"
  - `install.ps1`: builds release, copies to `%LOCALAPPDATA%\CursorJump\cursorjump.exe`, calls `cursorjump --enable-autostart`, starts it.

- [ ] **Step 1: Write the failing test.** `paths_are_per_user` (the Windows config path ends with `CursorJump\config.toml` under `%APPDATA%`).
- [ ] **Step 2:** Run `cargo test -p cursorjump paths`. Expected: FAIL.
- [ ] **Step 3:** Implement the wiring, tray, dialogs, settings and autostart.
- [ ] **Step 4:** Run `cargo test -p cursorjump`, then `cargo build --release -p cursorjump`. Expected: PASS and build succeeds. Manual: run it → tray shows "Searching for your Mac…". The settings window opens, and edits persist and hot-reload. Autostart registry value present after `--enable-autostart`.
- [ ] **Step 5:** Commit `feat(win): app wiring, tray, settings, autostart`.

---

### Task 15 [Mac]: Mac injection, power and permissions (`mac/inject.rs`, `mac/power.rs`, `mac/permissions.rs`)

**Files:** Create those three files.

**Interfaces:**
- Consumes: `modifiers::{ModifierState, Held, is_modifier}`, `input::ClickTracker`, `proto::{MouseButton, Side}`, `layout::{edge_frac, is_pushing_out}`.
- Produces:
  ```rust
  pub struct Injector; impl Injector {
      pub fn new() -> anyhow::Result<Injector>;               // CGEventSource(HIDSystemState), main display bounds in points
      pub fn warp_to(&mut self, edge: Side, frac: f32);       // 2 pt inside the edge, CGWarpMouseCursorPosition + moved event
      pub fn motion(&mut self, dx: i16, dy: i16) -> Option<f32>;   // moves virtual cursor (clamped); Some(frac) if pushing out of the entry edge
      pub fn scroll(&mut self, dx: i32, dy: i32);             // CGEventCreateScrollWheelEvent2, kCGScrollEventUnitPixel
      pub fn button(&mut self, b: MouseButton, down: bool);   // correct Down/Up/Dragged types, click state via ClickTracker(NSEvent.doubleClickInterval, 4 pt)
      pub fn key(&mut self, mac_keycode: u16, down: bool);    // modifiers → kCGEventFlagsChanged with ModifierState flags; others keyDown/Up with flags
      pub fn release_all(&mut self);                          // drain Held, post ups
      pub fn buttons_held(&self) -> bool;
  }
  pub fn wake_display();                                       // IOPMAssertionDeclareUserActivity(kIOPMUserActiveLocal)
  pub struct StayAwake; impl StayAwake { pub fn new() -> anyhow::Result<StayAwake>; }  // PreventUserIdleSystemSleep, released on Drop
  pub fn has_post_event_access() -> bool;                      // CGPreflightPostEventAccess
  pub fn request_post_event_access();                          // CGRequestPostEventAccess + open Privacy_Accessibility pane
  ```
  - While a button is held, motion posts `*MouseDragged`.
  - The virtual cursor position starts from `CGEventGetLocation` of a null event at `warp_to`, so Mac trackpad moves in between are respected.
  - The display bounds are re-read on `kCGDisplayReconfigured` and resolution changes.

- [ ] **Step 1: Write the failing test.** `motion_reports_push_out_only_on_entry_edge` uses a pure helper `fn step(pos:(f64,f64), bounds:Rectf, entry:Side, dx,dy) -> ((f64,f64), Option<f32>)` extracted for testability. Bounds 1512×982, entry Left: at x=0 with dx=−5 → `Some(frac)`; at the right edge with dx=+5 → None (wall) and x clamped.
- [ ] **Step 2:** Run `cargo test -p cursorjump mac::inject`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cursorjump`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(mac): event injection, display wake, permissions`.

---

### Task 16 [Mac]: Mac clipboard (`mac/clipboard.rs`)

**Files:** Create `app/cursorjump/src/mac/clipboard.rs`.

**Interfaces:**
- Produces the same three functions as Task 13 (`spawn_clipboard_watcher`, `read_clipboard`, `write_clipboard`), using `NSPasteboard.generalPasteboard`.
  - Types: `public.utf8-plain-text`, `public.html`, `public.rtf`, `public.png`. On read, fall back to `public.tiff`, converted to PNG with `image`.
  - The watcher polls `changeCount` every 250 ms on its own thread, inside an autorelease pool.
  - File URLs (`public.file-url`) are ignored in phase 1.

- [ ] **Step 1: Write the failing test.** `clipboard_roundtrip_text_html` (same assertions as Task 13).
- [ ] **Step 2:** Run `cargo test -p cursorjump mac::clipboard -- --test-threads=1`. Expected: FAIL.
- [ ] **Step 3:** Implement with `objc2-app-kit`.
- [ ] **Step 4:** Run it again. Expected: PASS. Manual: Cmd+Shift+Ctrl+4 screenshot → watcher logs a PNG.
- [ ] **Step 5:** Commit `feat(mac): clipboard`.

---

### Task 17 [Mac]: Mac app, bundle, signing, autostart (`mac/app.rs`, `mac/autostart.rs`, `scripts/mac/*`)

**Files:** Create `mac/app.rs`, `mac/autostart.rs`, `scripts/mac/{make-signing-cert.sh, Info.plist, bundle.sh, install.sh}`.

**Interfaces:**
- Consumes: Tasks 8, 10, 11, 14 (`run_tray`, `ask_pairing`, `run_settings`), 15, 16.
- Produces: `pub fn run_mac() -> anyhow::Result<()>; pub fn set_autostart(enabled: bool) -> anyhow::Result<()>;`
  - Wiring: `Listener::bind(0.0.0.0:47800)` + `Advertiser::start(hostname, 47800, fp, 47801)`. Accept, then `Hello`/pairing (`ask_pairing`, `Listener::set_trust(Pinned)` after mutual accept). Feed `MacMachine`.
  - `Enter` → `wake_display`, `warp_to`. Datagrams → `Injector::motion/scroll`. When `motion` returns `Some(frac)` → `MacEvent::PushOut`. Echo `Ping` as `Pong`.
  - `StayAwake` is held while a session is connected.
  - Menu bar shows connection status, the Accessibility state ("Grant Accessibility…" item when missing), Settings and Quit.
  - Clipboard is handled as in Task 14.
  - `Info.plist`:
    - `CFBundleIdentifier com.cursorjump.app`
    - `CFBundleExecutable cursorjump`
    - `LSUIElement true`
    - `NSLocalNetworkUsageDescription` ("CursorJump connects to your PC on your home network.")
    - `NSBonjourServices [_cursorjump._udp]`
  - `make-signing-cert.sh` creates the self-signed code-signing identity "CursorJump Dev" in the login keychain, using openssl + `security import` + `security add-trusted-cert -p codeSign`. It is idempotent.
  - `bundle.sh` builds `--release` for `aarch64-apple-darwin`, assembles `target/CursorJump.app`, and runs `codesign --force --options runtime --sign "CursorJump Dev"`.
  - `install.sh` copies the app to `/Applications` and writes and loads `~/Library/LaunchAgents/com.cursorjump.agent.plist` (RunAtLoad, KeepAlive SuccessfulExit=false, `LimitLoadToSessionType` `Aqua`).

- [ ] **Step 1: Write the failing test.** `launch_agent_plist_contents` (the generated plist string contains the bundle path, `RunAtLoad` true and the `KeepAlive` dict).
- [ ] **Step 2:** Run `cargo test -p cursorjump mac::autostart`. Expected: FAIL.
- [ ] **Step 3:** Implement the code and scripts.
- [ ] **Step 4:** Run `./scripts/mac/make-signing-cert.sh && ./scripts/mac/install.sh`. Expected: the menu-bar icon appears, and the Local Network and Accessibility prompts show (the user grants them). Rebuild and reinstall. Expected: **no new Accessibility prompt** (stable signature).
- [ ] **Step 5:** End-to-end with the PC running Task 14's build:
  - Pairing codes match on both screens and pairing persists across restarts of both apps.
  - The cursor crosses at the matching height and returns.
  - Typing, Cmd shortcuts via Ctrl, scrolling, double-click and drag-select all work.
  - Text and images copy and paste both ways.
- [ ] **Step 6:** Commit `feat(mac): app wiring, bundle, signing, LaunchAgent`.

---

### Task 18 [Mac]: Lock screen, Wi-Fi hiccups, hardening

**Files:** Modify `mac/app.rs`, `mac/power.rs`. Create `mac/netd.rs` and `scripts/mac/com.cursorjump.netd.plist` (only if needed, see below).

**Interfaces:**
- Produces: `pub fn set_awdl(enabled: bool) -> anyhow::Result<()>` (talks to the root helper over a unix socket at `/var/run/cursorjump-netd.sock`, mode 0660, group `staff`, accepting only the commands `awdl-down` / `awdl-up`).

- [ ] **Step 1: Lock screen spike.** Lock the Mac (Ctrl+Cmd+Q), then from the PC cross over and type the password.
  - If it works with the user agent alone, record that in `docs/ACCEPTANCE.md` and skip Step 2.
  - If keystrokes don't reach the password field, add a second LaunchAgent entry with `LimitLoadToSessionType` `LoginWindow` that runs `cursorjump --loginwindow-injector`. That process receives input from the main agent over a unix socket owned by the user. Then repeat the check.
- [ ] **Step 2 (conditional):** Implement the LoginWindow injector and verify again.
- [ ] **Step 3: Wi-Fi hiccups.** Implement `cursorjump-netd` (a root LaunchDaemon installed by `install.sh` with a `sudo` prompt) and `set_awdl`. When `reduce_wifi_hiccups` is on, run `awdl-down` on connect and `awdl-up` on disconnect, quit, or crash (the daemon restores `up` if the client socket closes).
- [ ] **Step 4: Hardening checks.** Mac sleep while the cursor is on it, Wi-Fi off/on, PC reboot (autostart), Mac reboot (autostart), unplugging/replugging the vertical monitor, changing Windows scaling to 150 %. Each must recover with no manual action.
- [ ] **Step 5:** Commit `feat(mac): lock screen input, Wi-Fi hiccup mode, hardening`.

---

### Task 19: Acceptance pass and latency measurement

**Files:** Create `docs/ACCEPTANCE.md`.

- [ ] **Step 1: Write the checklist.** One line per spec success bar and per Review Focus item, each with a pass/fail box.
- [ ] **Step 2: Latency.** With the tray readout, record the median and spikes over 5 minutes on Wi-Fi. Pass: median RTT ≤ 10 ms and verdict `Good`. If `Spiky`, enable "Reduce Wi-Fi hiccups" and re-measure. Record both.
- [ ] **Step 3: Games.** Launch Valorant (practice range) and Roblox with CursorJump running:
  - In-game flicks against the right edge never cross.
  - Alt+Tab to the desktop, then cross works.
  - Return, re-enter the game: no input lag and no anti-cheat warnings.
  - Confirm CursorJump holds no hooks while on the PC: the input thread logs "hooks: installed" / "hooks: removed" on every transition, and the log shows "removed" for the whole game session.
- [ ] **Step 4: Feel.** Have the user use it for a real work session and note anything that doesn't feel like a third monitor. File each as a follow-up task.
- [ ] **Step 5:** Commit `docs: phase 1 acceptance results`.
