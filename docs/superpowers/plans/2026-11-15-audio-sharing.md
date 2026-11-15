# CursorJump Audio Sharing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** While the two machines are connected, everything the Mac plays comes out of the PC's default output, mixed with the PC's own sound, and the Mac's speakers stay quiet. Mac apps that listen to the microphone hear the PC's default mic.

**Architecture:**
- **Mac → PC sound.**
  - A Core Audio *process tap* on the Mac captures every process except CursorJump, with mute on, so the Mac's speakers and AirPods go quiet.
  - The audio is cut into 5 ms packets of 48 kHz 16-bit stereo PCM, sent as QUIC datagrams.
  - The PC plays them through WASAPI shared mode on its default output.
- **PC mic → Mac.**
  - A small Core Audio HAL plug-in on the Mac adds two virtual devices that share one clock and one buffer: a visible input, "CursorJump Microphone", and a hidden output, "CursorJump Mic Feed".
  - When a Mac app starts listening to CursorJump Microphone, the Mac asks the PC for its mic.
  - The PC captures the mic with WASAPI and sends 10 ms mono packets. The Mac writes them into the hidden output, and the plug-in loops them round to the input.
- **Receiving side, both directions.** A jitter buffer plus a drift-correcting resampler. It absorbs Wi-Fi jitter and the two machines' slightly different clocks.
- **Who decides.** A pure state machine on the Mac decides what is routed. It covers the 5 s grace period, the settings switch, and missing devices or permissions.

**Tech Stack:**
- Existing: Rust (edition 2024), tokio, quinn 0.11, postcard, crossbeam-channel, `windows` 0.62, objc2.
- New:
  - `rubato`: a pure-Rust sinc resampler, used for drift correction and for a Mac tap rate other than 48 kHz.
  - `rtrb`: a lock-free ring, used for the Mac's realtime threads.
  - `objc2-core-audio` and `objc2-core-audio-types` (Mac).
  - Extra `windows` features: `Win32_Media_Audio`, `Win32_Media_KernelStreaming`, `Win32_Media_Multimedia`, `Win32_System_Com_StructuredStorage` (already present), `Win32_Devices_FunctionDiscovery`, `Win32_UI_Shell_PropertiesSystem`.
  - C (clang) for the HAL plug-in.

  Each task adds what it uses. Versions are pinned by `Cargo.lock`.

**Spec:** `docs/superpowers/specs/2026-10-25-audio-sharing-design.md`. It builds on `docs/superpowers/specs/2026-10-18-cursorjump-design.md`, whose rules all still hold.

**Execution notes:**
- **Branch.** Work on a new branch `audio`, made from `phase2`, which holds the final connection code. Once `phase2` is merged into `main`, rebase `audio` onto `main`.
- **SDD.**
  - The SDD contracts in `docs/superpowers/progress/contracts/` apply.
  - Use a new ledger, `docs/superpowers/progress/audio-ledger.md`, written by both machines: pull before writing, and append only.
  - Rulings are numbered from **A-R1** upwards.
- **Machines.** Tasks marked **[Mac]** run in Claude Code on the MacBook; everything else runs on the PC. Windows code type-checks on the Mac with `cargo clippy --target x86_64-pc-windows-msvc` (see `docs/HANDOFF.md`).
- **Live testing.** Never run the real app from a test or smoke run while the other machine's app is live.
- **Choices this plan makes (the spec left them to the builder):**
  - **A-R1. Codec:** uncompressed 16-bit PCM at 48 kHz.
    - It is lossless, needs no C codec build, and costs 1.5 Mbit/s, which is trivial on the LAN.
    - Packets of 960 bytes fit in one QUIC datagram (quinn's floor is 1200 bytes).
  - **A-R2. Mac capture:** a process tap, not a virtual output device.
    - A tap needs no change to the Mac's default output, and it dies with the process, so a crash can never leave the Mac silent.
    - It catches sound headed for any device, AirPods included.
    - **Cost:** the tap can't report its extra delay to video players. Lip sync instead relies on keeping the whole path at about 50 ms or less. Sound that is late by that much is well inside what viewers accept (ITU-R BT.1359 tolerates up to about 90 ms of audio lag). If the eye check fails, the fallback is a virtual output device that reports its latency. That needs a new task, and it goes to the user first.
  - **A-R3. Mac mic:** one HAL plug-in with two devices on one clock, a visible input and a hidden output feed.
    - The app writes to the feed like any output device, so it needs no shared memory and no other inter-process channel.
    - The input device's `IsRunningSomewhere` says exactly when a Mac app is listening.
  - **A-R4. Priority:**
    - Input datagrams keep `send_datagram`.
    - Audio uses `send_audio`, which drops the packet when more than 4 KiB of datagrams are already queued. Audio can therefore never pile up ahead of motion.
    - QUIC sends datagram frames before stream frames, so file data already yields to both.

## Global Constraints

- **Anti-cheat (phase 1, binding):** "On the PC, use only ordinary audio APIs, the way any media or voice app plays and records sound: WASAPI shared mode on the default devices. No hooking or injecting into other processes' audio, no audio drivers or virtual devices on the PC, and no reading other processes' memory." No `SendInput`, ever.
- **Priority:** "Input datagrams keep strict priority over audio." The order is "input first, then audio, then file data". "Audio must not stutter during a 5 GB file copy." Audio must have **no effect on cursor latency or feel**.
- **The PC's defaults:** "the PC's **default** output and input, whatever they are at the moment. If the user switches the PC's default output … or default mic, the Mac's sound and mic follow that switch, without restarting anything." That's the eConsole default for both.
- **PC mic only on demand:** "The PC captures its microphone only while a Mac app is actually listening to the shared mic." If Windows' "let desktop apps access your microphone" setting is off, "the app should say so plainly".
- **Always on while connected**, wherever the cursor is. Fall back to the Mac's own devices only when the connection is down for more than **5 s** (`GRACE`) or the user turns sharing off. Switch back by itself when either recovers. "A missing PC device" falls back for that direction only, with a plain reason in the Mac menu.
- **Restore the Mac's devices** on disconnect (after the grace period), on quit, on crash (at the next launch at the latest) and on uninstall.
- **Quality:** stereo, full quality, with no crackles, pops, dropouts or robotic sound. No delay that builds up over hours. Targets: Mac sound to the PC in about 60 ms or less; PC mic to a Mac app in about 60 ms or less.
- **Volume:** the PC's volume sets the overall level. "The Mac's own volume slider still works, setting how loud the Mac is within the mix." The PC keyboard's volume and mute keys "always control the PC's master volume, even while the cursor is on the Mac".
- **Switch:** "Mac sound and mic through PC", on by default. It lives in the Mac settings window and is mirrored in the Mac menu-bar menu.
- **Mac install:** "installed by a script like `scripts/mac/install-netd.sh`, and it can be cleanly uninstalled."
- **Protocol:** `PROTOCOL_VERSION` becomes **4**. Reinstall both machines together.
- **Non-goals:** PC sound to the Mac, per-app routing, surround, and sharing audio while disconnected.

## Review Focus

1. **A Wi-Fi stall or a burst of lost packets mid-song.** Expected: a short gap that fades out and back in, never a click. The buffer grows to ride out the next stall, and there is no delay creep afterwards. Pinned by `single_loss_fades_without_a_click` and `underrun_rebuffers_and_raises_the_target` (Task 2).
2. **CursorJump crashes or is killed while sharing.** Expected: the Mac's speakers come back at once, because the tap dies with the process. The previous default mic comes back at the next launch. Pinned by `recover_after_a_crash_restores_the_saved_input` (Task 11).
3. **The default device changes mid-stream.** On the PC, headphones are unplugged and the speakers take over. On the Mac, AirPods connect. Expected:
   - The PC reopens on the new default within about 200 ms.
   - The Mac stays quiet, and CursorJump Microphone is set back as the default input.

   Pinned by `reassert_switches_back_when_the_default_moves` (Task 11), `the_tap_stays_muted_across_an_output_switch` (Task 12, hands-on) and `render_follows_a_default_device_change` (Task 7, hands-on).
4. **Clock drift over a long session.** Expected: the buffer depth stays put and nothing is skipped. Pinned by `drift_of_500_ppm_is_absorbed_without_skips` (Task 2).
5. **Sleep, reboot or a long drop while a Mac app holds the mic.** Expected:
   - Within 5 s of a drop, nothing changes.
   - Past 5 s, the Mac falls back, and it comes back by itself on reconnect.
   - The PC never leaves its mic open without a session.

   Pinned by `a_blip_within_grace_changes_nothing`, `a_drop_beyond_grace_falls_back` and `reconnecting_after_a_fallback_waits_for_the_pc_status` (Task 4), and `disconnect_stops_the_mic` (Task 9).

---

## File Structure

```
crates/cj-core/src/
  audio/mod.rs        RATE, AudioStream, DeviceState, AudioStatus, AudioWant; re-exports            (new)
  audio/pcm.rs        encode_pcm / decode_pcm, Packetizer                                            (new)
  audio/jitter.rs     JitterBuffer: order, loss, concealment, target, overflow                       (new)
  audio/receiver.rs   AudioReceiver (jitter + drift resampler), AudioStats                           (new)
  audio_state.rs      MacAudio state machine and AudioPlan, menu-note copy, GRACE                    (new)
  proto.rs            v4: Datagram::Audio, Control::AudioWant, Control::AudioStatus
  config.rs           audio_sharing (default true; Mac only)
crates/cj-net/src/session.rs   send_audio (backlog-capped)
app/cursorjump/src/
  common/audio_pipe.rs  audio_pipe → AudioTx / AudioRx; AudioOut (swappable session sender)         (new)
  win/audio/{mod,devices,render,capture}.rs   WASAPI output and mic, device watch, privacy check     (new)
  win/app/audio.rs      PcAudio: wiring to the session and the app loop                              (new)
  win/input_thread.rs   volume keys pass through to Windows
  win/link.rs, win/app.rs   routing and wiring
  mac/audio/{mod,devices,tap,feed}.rs   cj-audio thread, MicRoute, SoundTap, MicFeed                 (new)
  mac/link.rs, mac/app.rs   routing and wiring
  ui/tray.rs, ui/settings.rs   the switch and the menu note
app/cursorjump/examples/audio_spike_mac.rs   throwaway (Task 1; deleted in Task 15)                  (new)
app/cursorjump-audio-driver/   HAL plug-in in C: src/plugin.c, src/ring.{c,h}, tests/ring_test.c, Info.plist   (new)
scripts/mac/build-audio-driver.sh, scripts/mac/install-audio-driver.sh                               (new)
scripts/mac/Info.plist   NSAudioCaptureUsageDescription
docs/superpowers/progress/audio-spike.md                                                             (new)
```

**Spike-dependent assumptions.** Task 1 checks these, and the later tasks are written against them. If one is false, record the fallback in `audio-spike.md`, amend the named task before running it, and note the change in the ledger.

| Id | Assumption | Used by | Fallback if false |
|---|---|---|---|
| S1 | A global tap with `CATapMuted` (excluding our own process) silences the built-in speakers and AirPods. It goes on capturing when the default output changes, if its aggregate device is rebuilt on the change. | Task 12 | If muting fails on some device, stop and rethink with the user. |
| S2 | The tap delivers audio **before** the output device's volume. | Task 12 (`mix_gain`) | If it's after the volume, use a gain of 1. |
| S3 | When "Screen & System Audio Recording" is refused, the app can tell: either creating the tap fails, or the stream reads as exact zeros while the default output `IsRunningSomewhere`. | Tasks 12, 14 | The note shows after 3 s of exact zeros while output is running. |
| S4 | The aggregate's IOProc runs at 512 frames or less. The tap reports its rate, and it is 48 kHz on built-in speakers. | Task 12 | A rate other than 48 kHz goes through `rubato` at a fixed ratio. A larger buffer gets recorded in the latency budget. |
| S5 | A HAL plug-in signed with "CursorJump Dev" loads in coreaudiod on macOS Tahoe. | Task 10 | Ad-hoc signing (`CJ_SIGN_IDENTITY=-`). |

---

### Task 1 [Mac]: Spike — prove the process tap (throwaway)

Throwaway code, with no tests and no review gate beyond the user listening. It runs with the user.

**Files:**
- Create `app/cursorjump/examples/audio_spike_mac.rs`, `#![cfg(target_os = "macos")]`-gated, with an empty `main` elsewhere.
- Create `docs/superpowers/progress/audio-spike.md`.

- [ ] **Step 1: Write the spike.**
  - It creates a `CATapDescription` with `initStereoGlobalTapButExcludeProcesses:`, excluding its own process object (via `kAudioHardwarePropertyTranslatePIDToProcessObject`), and with `muteBehavior = CATapMuted`, `private = true`.
  - It calls `AudioHardwareCreateProcessTap`.
  - It builds a private aggregate device with:
    - the tap in `kAudioAggregateDeviceTapListKey` (drift compensation on);
    - the default output as its main sub-device;
    - `kAudioAggregateDeviceTapAutoStartKey = true`.
  - It runs an IOProc that logs every 1 s: the RMS, the frames per callback, and the tap format from `kAudioTapPropertyFormat`.
  - Flags:
    - `--rebuild-on-switch`: listens to `kAudioHardwarePropertyDefaultOutputDevice` and rebuilds the aggregate on a change.
    - `--seconds N`: runs for N seconds, then tears the tap down.
  - If `objc2-core-audio` lacks `CATapDescription`, declare it with `objc2::extern_class!`.
- [ ] **Step 2: Run S1 with the user.** Start YouTube in Safari, then run `cargo run --example audio_spike_mac -- --seconds 60`. The user confirms the speakers go silent while the RMS stays above zero. Repeat with AirPods connected mid-run, with and without `--rebuild-on-switch`.
- [ ] **Step 3: Run S2.** Play a steady tone (`afplay /System/Library/Sounds/Submarine.aiff` in a loop). Note the logged RMS at volume 100 %, 50 % and 0 %, and with mute on.
- [ ] **Step 4: Run S3.**
  - Run `tccutil reset AudioCapture`. Run the spike from the bundled app path (`scripts/mac/bundle.sh`, then run the binary inside the `.app`), with `NSAudioCaptureUsageDescription` added to a copy of `Info.plist`.
  - Deny the prompt and record exactly what happens: an error code, or silence.
  - Then allow it, and record what changes.
- [ ] **Step 5: Run S4.** Record the frames per callback and the format with the built-in speakers and with AirPods. Try asking for `kAudioDevicePropertyBufferFrameSize = 256` on the aggregate, and record whether it holds.
- [ ] **Step 6: Record the results.** Write S1 to S4 (held or failed, with the numbers) to `audio-spike.md`, amend Task 12 if any assumption failed, and append a ledger line. Commit with `docs: audio spike findings (S1-S4)`.

---

### Task 2: Audio core (`cj-core::audio`)

Pure Rust, with no I/O. It runs on both machines and in CI.

**Files:**
- Create `crates/cj-core/src/audio/{mod.rs, pcm.rs, jitter.rs, receiver.rs}`.
- Modify `crates/cj-core/src/lib.rs` (`pub mod audio;`) and `crates/cj-core/Cargo.toml` (add `rubato`).
- Tests go in each file's `#[cfg(test)] mod tests`.

**Interfaces:**
- Produces, in `audio/mod.rs`:
  ```rust
  pub const RATE: u32 = 48_000;
  #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
  pub enum AudioStream { MacSound, PcMic }
  impl AudioStream {
      pub const fn channels(self) -> usize;       // MacSound 2, PcMic 1
      pub const fn packet_frames(self) -> usize;  // MacSound 240 (5 ms), PcMic 480 (10 ms)
      pub const fn packet_samples(self) -> usize; // channels × frames = 480 for both
  }
  #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
  pub enum DeviceState { Ok, Missing, Blocked }
  #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
  pub struct AudioStatus { pub output: DeviceState, pub mic: DeviceState }
  #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
  pub struct AudioWant { pub sound: bool, pub mic: bool }
  ```
- Produces, in `pcm.rs`:
  - `pub fn encode_pcm(samples: &[f32]) -> Vec<u8>`: i16 little-endian, clamped to ±1.0 and rounded.
  - `pub fn decode_pcm(bytes: &[u8]) -> Vec<f32>`: an odd trailing byte is ignored.
  - `pub struct Packetizer`, with `new(stream: AudioStream)` and `push(&mut self, interleaved: &[f32], emit: impl FnMut(u32, &[f32]))`. It emits whole packets with a wrapping sequence number that starts at 0, and keeps the remainder for the next call.
- Produces, in `jitter.rs`, `pub(crate) struct JitterBuffer`. Its rules, in sample frames:
  - **Order:** packets are reordered by sequence number. A duplicate, or a packet behind the play position, is counted as `late` and dropped.
  - **New sequence:** a packet more than 100 packets behind or more than 1000 ahead of the play position resets the buffer (a new session).
  - **Start:** nothing plays until the fill reaches the target.
  - **Loss:** a missing packet whose successor is already queued is concealed. The last 48 frames (1 ms) before the gap fade to 0, the gap plays as silence, and the next packet's first 48 frames fade in. It is counted as `lost`.
  - **Underrun:** when the buffer runs empty, the output is silence after the same 1 ms fade-out. The target grows by 10 ms (capped at 80 ms), the buffer waits to refill to the target, and it is counted as `underruns`.
  - **Calm:** the target shrinks by 5 ms (never below 20 ms) after each 60 s of output (2 880 000 frames pulled) with no underrun. The initial target is **30 ms**.
  - **Overflow:** when the fill exceeds target + 60 ms, the buffer drops whole packets down to the target, with the same fades. It is counted as `skips`.
- Produces, in `receiver.rs`:
  ```rust
  pub struct AudioReceiver;
  impl AudioReceiver {
      pub fn new(stream: AudioStream) -> Self;
      pub fn push(&mut self, seq: u32, samples: Vec<f32>);
      /// Fills `out` (interleaved, any length) with audio or silence; never blocks.
      pub fn pull(&mut self, out: &mut [f32]);
      pub fn reset(&mut self);
      pub fn stats(&self) -> AudioStats;
  }
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
  pub struct AudioStats { pub received: u64, pub lost: u64, pub late: u64, pub underruns: u64, pub skips: u64, pub target_ms: u32, pub fill_ms: u32 }
  impl AudioStats { pub fn log_line(&self, what: &str) -> String }
  // "<what>: 12000 packets, 3 lost, 0 late, 1 underrun, 0 skips, buffer 34 of 40 ms"
  ```
  - **Drift:** `pull` runs the buffer through a `rubato` sinc resampler whose ratio is `1 + correction`.
  - `correction` is a PI controller on (fill − target), measured once per packet. Its gains: proportional 2e-6 per ms of error, integral 1e-8 per ms per packet. It is clamped to **±1000 ppm**, and reset by `reset` and by every rebuffer.

- [ ] **Step 1: Write the failing tests.**
  - `pcm_roundtrip_is_within_one_step`: random samples in [-1, 1] come back within 1/32767.
  - `pcm_clamps_out_of_range`: `encode_pcm(&[2.0, -2.0, f32::NAN])` decodes to `[32767/32768, -1.0, 0.0]`. NaN encodes as 0.
  - `packetizer_emits_whole_packets_across_odd_blocks`: MacSound, blocks of 100, 333 and 47 samples (480 in all) emit exactly one 480-sample packet, seq 0. A further 480 emits seq 1.
  - `jitter_waits_for_the_target_then_plays_in_order`: packets 0..=9 pushed as 3, 1, 2, 0, … produce silence until 30 ms of fill, then the samples in sequence order.
  - `duplicates_and_late_packets_are_dropped`: `late == 2`, and the output is unchanged.
  - `single_loss_fades_without_a_click`: a 1 kHz sine at 0.5 with packet 5 missing. No adjacent-sample jump exceeds 0.1 (the clean sine's largest jump is about 0.066), and `lost == 1`.
  - `underrun_rebuffers_and_raises_the_target`: starving the buffer for 50 ms gives `underruns == 1` and `target_ms == 40`, and output resumes once refilled.
  - `calm_lowers_the_target`: after 60 s of clean pulls, the target is 5 ms lower, and never below 20 ms.
  - `overflow_skips_down_to_the_target`: pushing 200 ms at once gives `skips > 0` and leaves the fill within target + 5 ms.
  - `a_new_sequence_resets`: seq 50 000 after seq 10 resets with no `late` count.
  - `drift_of_500_ppm_is_absorbed_without_skips`: a simulated sender pushes 1.0005 × as many frames as are pulled, over 10 simulated minutes. After the first 30 s, `fill_ms` stays within target ± 10 ms, with `skips == 0` and `underruns == 0`. Repeat at −500 ppm.
  - `a_sine_passes_at_the_same_level_and_pitch`: a 1 kHz sine through `Packetizer` → `encode_pcm` → `decode_pcm` → `AudioReceiver` comes out at the same RMS within 0.1 dB, and at 1 kHz ± 1 Hz by zero crossings.
- [ ] **Step 2: Run `cargo test -p cj-core audio`.** Expected: FAIL, nothing defined yet.
- [ ] **Step 3: Implement** `mod.rs`, `pcm.rs`, `jitter.rs` and `receiver.rs` to the interfaces above. Use rubato's fixed-output sinc resampler (in rubato 0.16, `SincFixedOut` with `set_resample_ratio_relative`; use the current crate's equivalent), sized for one packet of output.
- [ ] **Step 4: Run `cargo test -p cj-core audio` and `cargo clippy -p cj-core --all-targets -- -D warnings`.** Expected: PASS, clean.
- [ ] **Step 5: Commit** with `feat(core): audio packets, jitter buffer and drift-correcting receiver`.

---

### Task 3: Protocol v4 and `send_audio` (`cj-core::proto`, `cj-net::session`)

**Files:**
- Modify `crates/cj-core/src/proto.rs` and `crates/cj-net/src/session.rs`.
- Test: `proto.rs` tests and `crates/cj-net/tests/loopback.rs`.

**Interfaces:**
- Consumes: `AudioStream`, `AudioStatus` and `AudioWant` (Task 2).
- Produces:
  - `PROTOCOL_VERSION = 4`. Doc line: `4: audio sharing`.
  - `Datagram::Audio { stream: AudioStream, seq: u32, pcm: Vec<u8> }`, appended last (variant index 4).
  - `Control::AudioWant(AudioWant)` (Mac → PC, index 16) and `Control::AudioStatus(AudioStatus)` (PC → Mac, index 17), appended last.
  - `pub fn send_audio(&self, d: &Datagram) -> Result<bool>` on `Session`. It returns `Ok(false)`, sending nothing, when `conn.datagram_send_buffer_space() < DATAGRAM_BUFFER_BYTES - AUDIO_BACKLOG_BYTES`. The constant is `const AUDIO_BACKLOG_BYTES: usize = 4 * 1024;`.

- [ ] **Step 1: Write the failing tests.**
  - In `proto.rs`:
    - Extend `datagram_roundtrip_all_variants` and `control_roundtrip_all_variants` with the new variants.
    - Add to `control_variant_indices_are_stable`: AudioWant at `[4] == 16`, AudioStatus at 17.
    - `audio_datagram_fits_one_packet`: `encode_datagram(&Datagram::Audio { stream: MacSound, seq: u32::MAX, pcm: vec![0; 960] }).len() <= 1000`.
  - In `loopback.rs`:
    - `audio_datagrams_cross`: 10 audio datagrams arrive and decode intact.
    - `send_audio_drops_once_the_backlog_is_full`: on a `current_thread` runtime, 50 back-to-back `send_audio` calls with no await give between 1 and 5 `true` results, and the rest `false`. A `Motion` datagram sent right after still arrives.
- [ ] **Step 2: Run `cargo test -p cj-core proto` and `cargo test -p cj-net --test loopback audio`.** Expected: FAIL.
- [ ] **Step 3: Implement.** Add the variants and the constant. `send_audio` mirrors `send_datagram`, with the space check first.
- [ ] **Step 4: Run `cargo test -p cj-core -p cj-net`.** Expected: PASS. The version-mismatch tests still pass at v4.
- [ ] **Step 5: Commit** with `feat(net): protocol v4 — audio datagrams and backlog-capped send_audio`.

---

### Task 4: Mac routing rules (`cj-core::audio_state`, `cj-core::config`)

Pure and clock-injected, like `mac_state.rs`.

**Files:**
- Create `crates/cj-core/src/audio_state.rs`.
- Modify `crates/cj-core/src/lib.rs` and `crates/cj-core/src/config.rs` (add `pub audio_sharing: bool`, default `true`, doc `Mac only.`).
- Modify `app/cursorjump/src/ui/settings.rs`, but only to add `audio_sharing` to `merge_edits`'s list so it compiles. The checkbox comes in Task 14.

**Interfaces:**
- Consumes: `AudioStatus`, `DeviceState` and `AudioWant` (Task 2).
- Produces:
  ```rust
  pub const GRACE: Duration = Duration::from_secs(5);
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Capture { Unknown, Allowed, Denied }
  pub struct MacAudio;
  impl MacAudio {
      pub fn new(sharing: bool) -> Self;              // link never up, capture Unknown, driver false
      pub fn connected(&mut self, now: Instant);      // forgets the PC status unless still within GRACE of the drop
      pub fn disconnected(&mut self, now: Instant);
      pub fn set_sharing(&mut self, on: bool);
      pub fn set_capture(&mut self, c: Capture);
      pub fn set_driver(&mut self, present: bool);
      pub fn set_pc(&mut self, s: AudioStatus);
      pub fn set_mic_listening(&mut self, on: bool);
      pub fn plan(&self, now: Instant) -> AudioPlan;
  }
  #[derive(Debug, Clone, PartialEq)]
  pub struct AudioPlan {
      pub tap: bool,                  // muted tap running (sound leaves the Mac)
      pub mic_default: bool,          // CursorJump Microphone is the default input
      pub want: AudioWant,            // what to ask the PC for
      pub note: Option<&'static str>, // the menu line
      pub recheck_at: Option<Instant>,// when the grace ends, if a drop is pending
  }
  ```
- The rules ("alive" means connected, or dropped less than `GRACE` ago):
  - `tap = sharing && capture != Denied && alive && pc.output == Ok`.
  - `mic_default = sharing && driver && alive && pc.mic == Ok`.
  - `want.sound = tap && connected`.
  - `want.mic = mic_default && mic_listening && connected`.
  - With no PC status yet, both directions count as not `Ok`.
- `note` applies only while sharing is on and the link is connected. Its exact copy, as `pub const`s, with the first match shown:
  1. `NOTE_CAPTURE_DENIED` = "Mac sound stays on the Mac: allow CursorJump in System Settings › Privacy & Security › Screen & System Audio Recording"
  2. `NOTE_NO_PC_OUTPUT` = "Mac sound stays on the Mac: the PC has no sound output"
  3. `NOTE_NO_DRIVER` = "Mic stays on the Mac: the CursorJump audio driver isn't installed"
  4. `NOTE_PC_MIC_BLOCKED` = "Mic stays on the Mac: on the PC, turn on Settings › Privacy & security › Microphone › Let desktop apps access your microphone"
  5. `NOTE_NO_PC_MIC` = "Mic stays on the Mac: the PC has no microphone"
  6. Otherwise `NOTE_ROUTED` = "Mac sound and mic go through the PC"

- [ ] **Step 1: Write the failing tests.**
  - `routes_both_ways_once_the_pc_reports_ok`.
  - `a_blip_within_grace_changes_nothing`: disconnected at t; at t + 4.9 s `tap` and `mic_default` are still true, `want` is all false, and `recheck_at == t + 5 s`.
  - `a_drop_beyond_grace_falls_back`: at t + 5 s both are false and `recheck_at` is None.
  - `reconnecting_within_grace_keeps_the_last_status`.
  - `reconnecting_after_a_fallback_waits_for_the_pc_status`: `tap` stays false until `set_pc`.
  - `sharing_off_falls_back_at_once`.
  - `missing_pc_mic_falls_back_for_the_mic_only`: `tap` true, `mic_default` false, note `NOTE_NO_PC_MIC`.
  - `blocked_mic_says_how_to_fix_it`.
  - `denied_capture_keeps_sound_local_but_routes_the_mic`.
  - `the_pc_mic_is_asked_for_only_while_an_app_listens`.
  - `no_driver_keeps_the_mic_local`.
  - In `config.rs`: `audio_sharing_defaults_on`, and an old file without the key loads as `true`.
- [ ] **Step 2: Run `cargo test -p cj-core audio_state config`.** Expected: FAIL.
- [ ] **Step 3: Implement** the rules above.
- [ ] **Step 4: Run `cargo test -p cj-core` and `cargo build -p cursorjump`.** Expected: PASS.
- [ ] **Step 5: Commit** with `feat(core): Mac audio routing rules with a 5 s grace`.

---

### Task 5: PC volume keys reach Windows while on the Mac (`win/input_thread.rs`)

**Files:** Modify `app/cursorjump/src/win/input_thread.rs`: `on_key_hook` and its tests.

**Interfaces:** Produces `fn is_volume_key(vk: u32) -> bool`. It is true for `VK_VOLUME_MUTE` (0xAD), `VK_VOLUME_DOWN` (0xAE) and `VK_VOLUME_UP` (0xAF).

- [ ] **Step 1: Write the failing test** `key_hook_volume_keys_pass_to_windows_and_stay_off_the_mac`. While on the Mac, down and up for each volume key:
  - return `false` (not swallowed);
  - send no `InputMsg::Key`;
  - leave `held` empty.

  A following `W` key is still swallowed and sent.
- [ ] **Step 2: Run `cargo test -p cursorjump volume`.** Expected: FAIL (swallowed).
- [ ] **Step 3: Implement.** In `on_key_hook`, right after the `self.mac.is_none()` check, `if is_volume_key(info.vkCode) { return false; }`. That's before the held set, the return hotkey and the router, so the stuck-key rules never see these keys.
- [ ] **Step 4: Run `cargo test -p cursorjump input_thread`.** Expected: PASS.
- [ ] **Step 5: Commit** with `feat(win): volume and mute keys keep working on the PC while on the Mac`.

---

### Task 6: Audio pipes (`common/audio_pipe.rs`)

**Files:**
- Create `app/cursorjump/src/common/audio_pipe.rs`.
- Modify `app/cursorjump/src/common/mod.rs`.

**Interfaces:**
- Consumes: `AudioReceiver`, `AudioStats`, `decode_pcm`, `encode_pcm` and `Packetizer` (Task 2); `Session::send_audio` and `Datagram::Audio` (Task 3).
- Produces:
  ```rust
  pub fn audio_pipe(stream: AudioStream) -> (AudioTx, AudioRx);
  #[derive(Clone)] pub struct AudioTx;
  impl AudioTx { pub fn push(&self, seq: u32, pcm: &[u8]); }        // decodes; drops when 64 packets are queued
  pub struct AudioRx;
  impl AudioRx {
      pub fn pull(&mut self, out: &mut [f32]);   // drains the queue into the receiver, then pulls; no locks held while waiting
      pub fn reset(&mut self);                   // empties the queue and the receiver
      pub fn stats(&self) -> AudioStatsHandle;
  }
  #[derive(Clone)] pub struct AudioStatsHandle;
  impl AudioStatsHandle { pub fn snapshot(&self) -> AudioStats; }  // published by pull with try_lock
  /// Sends captured audio on the current session, if any; swapped on reconnect.
  #[derive(Clone, Default)] pub struct AudioOut;
  impl AudioOut {
      pub fn set(&self, s: Option<Session>);
      pub fn sender(&self, stream: AudioStream) -> AudioSender;
  }
  pub struct AudioSender;   // Packetizer + AudioOut, owned by one capture thread
  impl AudioSender { pub fn push(&mut self, interleaved: &[f32]); }  // packets go through send_audio; dropped with no session
  ```
- The queue is a `crossbeam_channel::bounded(64)` of `(u32, Vec<f32>)`.

- [ ] **Step 1: Write the failing tests.**
  - `pipe_carries_a_sine_end_to_end`: an `AudioSender` on a loopback session pair, whose datagram reader feeds `AudioTx`. An `AudioRx` pulled in 10 ms steps gets the sine back at the same RMS within 0.1 dB.
  - `no_session_drops_quietly`.
  - `reset_forgets_queued_audio`.
  - `stats_snapshot_follows_pulls`.
- [ ] **Step 2: Run `cargo test -p cursorjump audio_pipe`.** Expected: FAIL.
- [ ] **Step 3: Implement** to the interfaces.
- [ ] **Step 4: Run `cargo test -p cursorjump audio_pipe`.** Expected: PASS.
- [ ] **Step 5: Commit** with `feat(app): audio pipes between the session and the audio devices`.

---

### Task 7: PC output and device watch (`win/audio/{mod,devices,render}.rs`)

**Files:**
- Create `app/cursorjump/src/win/audio/mod.rs`, `devices.rs` and `render.rs`.
- Modify `app/cursorjump/src/win/mod.rs` and `app/cursorjump/Cargo.toml` (the `windows` features above).

**Interfaces:**
- Consumes: `AudioRx` (Task 6); `AudioStatus` and `DeviceState` (Task 2).
- Produces:
  - In `devices.rs`:
    - `pub fn pc_status() -> AudioStatus`.
      - `output` is `Ok` if `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)` succeeds, else `Missing`.
      - `mic` is `Blocked` if `mic_blocked_by_privacy()`, else `Ok` or `Missing` the same way for `eCapture`.
    - `pub fn mic_blocked_by_privacy() -> bool`. True if `HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone` value `Value` is `"Deny"`, or its `NonPackaged` subkey's `Value` is `"Deny"`. A missing key means not blocked.
    - `pub struct DeviceWatch`, with `pub fn start(on_change: impl Fn() + Send + Sync + 'static) -> Result<DeviceWatch>`. It is an `IMMNotificationClient` (`#[implement]`) that calls `on_change` on `OnDefaultDeviceChanged` (eConsole only), `OnDeviceAdded`, `OnDeviceRemoved` and `OnDeviceStateChanged`. Drop unregisters it.
  - In `render.rs`:
    - `pub struct MacSoundOut`, with `pub fn start(rx: AudioRx) -> MacSoundOut`, `pub fn reopen(&self)` and `pub fn stop(self) -> AudioRx`.
    - It runs a thread named `cj-audio-out`, in MMCSS "Pro Audio" (`AvSetMmThreadCharacteristicsW`).
    - It uses shared-mode, event-driven `IAudioClient` on the default eConsole render endpoint, with `WAVEFORMATEXTENSIBLE` 48 kHz stereo float, `AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`, and a requested buffer of 10 ms.
    - Each event fills the free frames from `rx.pull`.
    - It reopens on `reopen()` or `AUDCLNT_E_DEVICE_INVALIDATED`, then calls `rx.reset()`. With no device, it retries every 500 ms.
- All COM and WASAPI calls are ordinary shared-mode use (Global Constraints).

- [ ] **Step 1: Write the failing tests.**
  - `privacy_values_are_read_as_blocked_only_for_deny`: the value parser is a pure `fn consent_denied(value: Option<&str>) -> bool`.
  - `pc_status_does_not_panic_without_devices`.
  - `#[ignore]` hands-on, `render_plays_a_tone_through_the_default_output`: 2 s of 440 Hz fed through an `audio_pipe`. The user hears it.
  - `#[ignore]` hands-on, `render_follows_a_default_device_change`: a 20 s tone. The user switches the default output in the Windows sound settings, and the tone moves within about 200 ms with no restart.
- [ ] **Step 2: Run `cargo test -p cursorjump win::audio`.** Expected: FAIL.
- [ ] **Step 3: Implement** to the interfaces.
- [ ] **Step 4: Run** `cargo test -p cursorjump win::audio`, then `cargo test -p cursorjump win::audio -- --ignored --nocapture` with the user listening. Expected: PASS, and the user confirms both.
- [ ] **Step 5: Commit** with `feat(win): play Mac sound through the PC's default output`.

---

### Task 8: PC mic capture (`win/audio/capture.rs`)

**Files:** Create `app/cursorjump/src/win/audio/capture.rs`.

**Interfaces:**
- Consumes: `AudioOut`, `AudioSender` (Task 6); `DeviceState` (Task 2).
- Produces:
  - `pub struct PcMicIn`, with `pub fn start(out: AudioOut, on_state: impl Fn(DeviceState) + Send + 'static) -> PcMicIn` and `pub fn reopen(&self)`. Drop stops and joins the thread.
  - It runs a thread named `cj-audio-in`, in MMCSS "Pro Audio".
  - It uses shared-mode, event-driven capture on the default eConsole capture endpoint, as 48 kHz mono float with autoconvert. Each event's frames go to `out.sender(AudioStream::PcMic)`.
  - It calls `on_state(Ok)` once capture runs. `E_ACCESSDENIED` from `Initialize` gives `on_state(Blocked)`, and no endpoint gives `on_state(Missing)`. In both cases it waits for `reopen()`.
  - Windows' own "mic in use" indicator shows only while a `PcMicIn` exists.

- [ ] **Step 1: Write the failing tests.**
  - `capture_error_maps_to_device_state`: a pure `fn state_for(hr: HRESULT) -> DeviceState`, where `E_ACCESSDENIED` gives Blocked and `E_NOTFOUND` or `AUDCLNT_E_DEVICE_INVALIDATED` gives Missing.
  - `#[ignore]` hands-on, `capture_records_the_default_mic`: 3 s of capture into a loopback session. The RMS while the user talks is more than 10 × the RMS while silent.
- [ ] **Step 2: Run `cargo test -p cursorjump capture`.** Expected: FAIL.
- [ ] **Step 3: Implement** to the interfaces.
- [ ] **Step 4: Run** the tests, then the ignored one with the user. Expected: PASS.
- [ ] **Step 5: Commit** with `feat(win): capture the PC's default mic on demand`.

---

### Task 9: PC app wiring (`win/app/audio.rs`, `win/app.rs`, `win/link.rs`)

**Files:**
- Create `app/cursorjump/src/win/app/audio.rs`.
- Modify `app/cursorjump/src/win/app.rs` and `app/cursorjump/src/win/link.rs`.

**Interfaces:**
- Consumes: Tasks 3, 6, 7 and 8.
- Produces:
  ```rust
  pub(super) struct PcAudio;
  impl PcAudio {
      pub fn new(events: UnboundedSender<Ev>) -> PcAudio;  // starts DeviceWatch → Ev::AudioDevices
      pub fn tx(&self) -> AudioTx;                         // for Live's datagram reader
      pub fn connect(&mut self, s: &Session) -> AudioStatus;  // the status to send now
      pub fn disconnected(&mut self);                      // stops output and mic at once
      pub fn want(&mut self, w: AudioWant);
      pub fn devices_changed(&mut self) -> Option<AudioStatus>;   // reopen() on the output and the mic if running; Some when the status changed
      pub fn mic_state(&mut self, s: DeviceState) -> Option<AudioStatus>;
      pub fn log_stats(&self);                             // with the latency log, only while sound runs: log_line("Mac sound")
      pub fn mic_blocked(&self) -> bool;
  }
  ```
- New `Ev` variants: `AudioDevices` and `MicState(DeviceState)`.
- `Live::start` takes an `AudioTx`. Its datagram reader passes `Datagram::Audio { stream: MacSound, .. }` to it.
- `App` sends `Control::AudioStatus` after a paired connect, and whenever `devices_changed` or `mic_state` returns `Some`. It routes `Control::AudioWant` to `want`. `reconnect` and `settle`'s disconnect call `disconnected`.
- When `mic_blocked()`, `status_text` gets the suffix " — Windows is blocking the mic for desktop apps".

- [ ] **Step 1: Write the failing tests.**
  - `status_mentions_a_blocked_mic`.
  - `disconnect_stops_the_mic`: with a fake mic factory injected through a `PcAudio::with_factories` test constructor, `disconnected()` drops the mic and the output.
  - `want_starts_and_stops_each_direction`.
  - `status_is_resent_only_on_change`.
- [ ] **Step 2: Run `cargo test -p cursorjump win::app`.** Expected: FAIL.
- [ ] **Step 3: Implement** the wiring.
- [ ] **Step 4: Run `cargo test -p cursorjump` and `cargo clippy --workspace --all-targets -- -D warnings`.** Expected: PASS and clean.
- [ ] **Step 5: Commit** with `feat(win): wire audio sharing into the PC app`.

---

### Task 10 [Mac]: The HAL plug-in (`app/cursorjump-audio-driver/`)

C, structured like Apple's public NullAudio AudioServerPlugIn sample. No code from other drivers.

**Files:**
- Create `app/cursorjump-audio-driver/src/plugin.c`, `src/ring.h`, `src/ring.c`, `tests/ring_test.c` and `Info.plist`.
- Create `scripts/mac/build-audio-driver.sh` and `scripts/mac/install-audio-driver.sh`.
- Modify `.github/workflows/ci.yml` (macOS: build the driver unsigned, and run `ring_test`).

**Interfaces:**
- Produces the bundle `CursorJumpAudio.driver`, with `CFBundleIdentifier` `com.cursorjump.audio`. It has one factory, with a fixed UUID generated once and written in both `Info.plist` and `plugin.c`. It contains two devices on one shared clock: the same anchor host time, the same `kAudioDevicePropertyZeroTimeStampPeriod` of 48 000 frames, and 48 kHz float32 mono only.

  | | Microphone | Mic Feed |
  |---|---|---|
  | Name | "CursorJump Microphone" | "CursorJump Mic Feed" |
  | UID | `com.cursorjump.audio.mic` | `com.cursorjump.audio.feed` |
  | Direction | input, 1 ch | output, 1 ch |
  | `CanBeDefaultDevice` | true | false |
  | `CanBeDefaultSystemDevice` | false | false |
  | `IsHidden` | false | **true** |
  | `IsRunningSomewhere` | true while its StartIO count is above 0; changes sent with `PropertiesChanged` | as normal |
- `ring.h`:
  ```c
  #define CJ_RING_FRAMES 48000
  typedef struct { float data[CJ_RING_FRAMES]; _Atomic uint64_t stamp[CJ_RING_FRAMES]; } cj_ring;
  void cj_ring_reset(cj_ring *r);
  void cj_ring_write(cj_ring *r, uint64_t sample_time, const float *frames, uint32_t n);
  void cj_ring_read(cj_ring *r, uint64_t sample_time, float *out, uint32_t n); /* frames never written for this exact time read as 0 */
  ```
  - Each slot's stamp is stored `sample_time + 1`, with release order, after its data.
  - A read checks the stamp with acquire order before the data and again after it. A mismatch reads as 0.
  - Mic Feed's `WriteMix` writes at its output sample time. Microphone's `ReadInput` reads at its input sample time, the same time base, so there is no offset.
- `build-audio-driver.sh [--no-sign]`:
  - `clang -bundle -arch arm64 -O2 -framework CoreAudio -framework CoreFoundation`, writing to `target/audio-driver/CursorJumpAudio.driver`.
  - It signs with `${CJ_SIGN_IDENTITY:-CursorJump Dev}`.
- `install-audio-driver.sh [--uninstall]`:
  - It follows `install-netd.sh`: run as the user, and use sudo only for the copy to `/Library/Audio/Plug-Ins/HAL/` and for `killall coreaudiod`.
  - `--uninstall` removes the bundle and restarts coreaudiod. Every step is idempotent.

- [ ] **Step 1: Write the failing test** `tests/ring_test.c`, which runs these cases and ends with a non-zero exit on failure:
  - write then read at the same time round-trips;
  - a time never written reads 0;
  - a time overwritten one lap later reads 0 for the old time;
  - a write across the wrap point round-trips;
  - `reset` reads as all 0.
- [ ] **Step 2: Run** `clang -std=c11 -O2 app/cursorjump-audio-driver/src/ring.c app/cursorjump-audio-driver/tests/ring_test.c -o target/ring_test && target/ring_test`. Expected: FAIL (link errors).
- [ ] **Step 3: Implement** `ring.c`, then `plugin.c`, the plist and the two scripts.
- [ ] **Step 4: Verify.**
  - `target/ring_test` passes.
  - `./scripts/mac/install-audio-driver.sh` succeeds (S5).
  - Audio MIDI Setup shows "CursorJump Microphone" and not the feed.
  - `system_profiler SPAudioDataType` lists the microphone.
  - Recording from it in QuickTime gives silence, with no error.
  - `--uninstall` removes it, and a rerun of the install works.
- [ ] **Step 5: Commit** with `feat(mac): CursorJump audio driver (virtual mic plus hidden feed)`.

---

### Task 11 [Mac]: Mac device control (`mac/audio/devices.rs`)

**Files:**
- Create `app/cursorjump/src/mac/audio/mod.rs` (module declarations only, for now) and `devices.rs`.
- Modify `app/cursorjump/src/mac/mod.rs` and `app/cursorjump/Cargo.toml` (`objc2-core-audio`, `objc2-core-audio-types`).

**Interfaces:**
- Produces:
  ```rust
  pub const MIC_UID: &str = "com.cursorjump.audio.mic";
  pub const FEED_UID: &str = "com.cursorjump.audio.feed";
  pub trait InputDevices {                                   // CoreAudio in the app, a fake in tests
      fn default_input_uid(&self) -> Option<String>;
      fn set_default_input(&mut self, uid: &str) -> Result<()>;
      fn exists(&self, uid: &str) -> bool;
  }
  pub struct CoreAudioDevices;                               // impl InputDevices
  pub struct MicRoute<D: InputDevices>;
  impl<D: InputDevices> MicRoute<D> {
      pub fn new(devices: D, restore_file: PathBuf) -> Self;  // restore_file = <app support>/audio-restore.toml
      pub fn recover(&mut self);       // at launch: a leftover file means we crashed; restore it, then delete it
      pub fn engage(&mut self) -> Result<()>;   // saves the current default's UID once (if not ours), then sets MIC_UID
      pub fn release(&mut self);       // sets the saved UID back if it still exists, else leaves macOS's choice; deletes the file
      pub fn reassert(&mut self);      // while engaged: if the default isn't MIC_UID, set it back (logged)
      pub fn engaged(&self) -> bool;
  }
  pub enum AudioDevEv { DefaultInputChanged, DefaultOutputChanged, DevicesChanged, MicListening(bool) }
  pub struct AudioWatch;   // AudioObjectAddPropertyListenerBlock on the system object and on the mic's IsRunningSomewhere
  impl AudioWatch { pub fn start(on: impl Fn(AudioDevEv) + Send + Sync + 'static) -> Result<AudioWatch>; }  // Drop removes the listeners
  pub fn driver_present() -> bool;   // both UIDs resolve via kAudioHardwarePropertyTranslateUIDToDevice
  ```
- The restore file holds one line, `previous_input = "<uid>"`, written atomically like `Config::save`.
- When `DevicesChanged` makes the mic appear, `AudioWatch` re-registers its mic listener.

- [ ] **Step 1: Write the failing tests** (fake `InputDevices`, temp dir):
  - `engage_saves_the_previous_input_once_and_switches`: a second `engage` doesn't overwrite the file with our own UID.
  - `release_restores_the_previous_input_and_deletes_the_file`.
  - `release_when_the_previous_input_is_gone_leaves_macos_choice`.
  - `recover_after_a_crash_restores_the_saved_input`.
  - `reassert_switches_back_when_the_default_moves`.
  - `nothing_is_written_while_never_engaged`.
- [ ] **Step 2: Run `cargo test -p cursorjump mac::audio::devices`.** Expected: FAIL.
- [ ] **Step 3: Implement**: `MicRoute`, then `CoreAudioDevices`, `AudioWatch` and `driver_present`.
- [ ] **Step 4: Run the tests.** Then, with the driver installed, run `#[ignore]` `engage_and_release_on_this_mac`. Expected: PASS, and System Settings › Sound › Input shows the switch and the restore.
- [ ] **Step 5: Commit** with `feat(mac): switch the default input to the PC's mic and always restore it`.

---

### Task 12 [Mac]: Mac sound capture (`mac/audio/tap.rs`)

Written against S1 to S4. Apply any amendment from Task 1 first.

**Files:** Create `app/cursorjump/src/mac/audio/tap.rs`.

**Interfaces:**
- Consumes: `AudioOut` (Task 6); `RATE` and `AudioStream` (Task 2).
- Produces:
  ```rust
  #[derive(Debug)] pub enum TapError { Denied, Failed(anyhow::Error) }
  pub struct SoundTap;
  impl SoundTap {
      pub fn start(out: AudioOut) -> Result<SoundTap, TapError>;  // global muted tap excluding our process + private aggregate + IOProc
      pub fn rebuild(&mut self) -> Result<(), TapError>;          // on DefaultOutputChanged (S1)
  }  // Drop: stop the IOProc, destroy the aggregate, then the tap
  /// The gain that keeps the Mac's volume slider working (S2): 10^(dB/20), 0 when muted, 1 without a volume control.
  pub fn mix_gain(volume_db: Option<f32>, muted: bool) -> f32;
  ```
- The IOProc runs on Core Audio's realtime thread. It applies `mix_gain`, which is read on `DefaultOutputChanged` and on the output device's volume and mute listeners, and kept in an `AtomicU32` as f32 bits.
- It pushes interleaved stereo into an `rtrb` ring of 48 000 samples, then unparks the thread `cj-audio-send`.
- That thread owns the `AudioSender` for `MacSound`. If the tap rate isn't 48 kHz (S4), it resamples with `rubato` at a fixed ratio. It parks with a 2 ms timeout.
- Denial is reported per S3 as `TapError::Denied`.

- [ ] **Step 1: Write the failing tests.**
  - `mix_gain_follows_volume_and_mute`: (Some(0.0), false) → 1.0; (Some(−6.0206), false) → 0.5 ± 1e-4; (Some(−20.0), true) → 0.0; (None, false) → 1.0.
  - `#[ignore]` hands-on, `the_tap_captures_and_the_speakers_stay_quiet`: a 10 s tap into a loopback session while `afplay` plays. The RMS is above 0, and the user hears nothing from the Mac.
  - `#[ignore]` hands-on, `the_tap_stays_muted_across_an_output_switch`: the same while the user connects AirPods. Nothing plays from them, and the RMS resumes within 1 s.
- [ ] **Step 2: Run `cargo test -p cursorjump mac::audio::tap`.** Expected: FAIL.
- [ ] **Step 3: Implement** to the interfaces.
- [ ] **Step 4: Run** the tests, then the ignored ones with the user. Expected: PASS, and the user confirms.
- [ ] **Step 5: Commit** with `feat(mac): capture Mac sound with a muted process tap`.

---

### Task 13 [Mac]: Feeding the PC's mic into the virtual mic (`mac/audio/feed.rs`)

**Files:** Create `app/cursorjump/src/mac/audio/feed.rs`.

**Interfaces:**
- Consumes: `AudioRx` (Task 6); `FEED_UID` (Task 11).
- Produces `pub struct MicFeed`, with `pub fn start(rx: AudioRx) -> Result<MicFeed>` and `pub fn stop(self) -> AudioRx`.
  - It opens the hidden feed device by UID and sets its buffer to 256 frames.
  - Its IOProc calls `rx.pull` straight into the output buffer: `AudioRx::pull` takes no locks while waiting, so it's safe on the realtime thread.
  - `stop` stops and destroys the IOProc, then hands back the `rx`.

- [ ] **Step 1: Write the failing test.** `#[ignore]` hands-on, `a_tone_in_the_feed_comes_out_of_the_microphone`:
  - Feed a 1 kHz tone through an `audio_pipe` into a `MicFeed`.
  - At the same time, record 2 s from "CursorJump Microphone" with a second IOProc in the test.
  - The recording's zero-crossing pitch is 1 kHz ± 2 Hz, and its RMS is within 1 dB of the input.
- [ ] **Step 2: Run** `cargo test -p cursorjump mac::audio::feed -- --ignored`. Expected: FAIL.
- [ ] **Step 3: Implement** to the interface.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** with `feat(mac): play the PC's mic into CursorJump Microphone`.

---

### Task 14 [Mac]: Mac app wiring, switch and menu (`mac/audio/mod.rs`, `mac/app.rs`, `mac/link.rs`, `ui/*`)

**Files:**
- Modify `app/cursorjump/src/mac/audio/mod.rs`, `mac/app.rs`, `mac/link.rs`, `ui/tray.rs`, `ui/settings.rs` and `scripts/mac/Info.plist`.

**Interfaces:**
- Consumes: everything above.
- Produces, in `mac/audio/mod.rs`, a thread named `cj-audio` that owns `MicRoute`, `AudioWatch`, `Option<SoundTap>`, `Option<MicFeed>` and the `AudioRx` for `PcMic`:
  ```rust
  pub enum AudioCmd { Apply(AudioPlan), Session(Option<Session>), Quit(std::sync::mpsc::Sender<()>) }
  pub enum MacAudioEv { Capture(Capture), Driver(bool), MicListening(bool) }
  pub struct MacAudioIo;
  impl MacAudioIo {
      pub fn start(app_support: &Path, events: impl Fn(MacAudioEv) + Send + 'static) -> MacAudioIo;  // runs MicRoute::recover first
      pub fn tx(&self) -> AudioTx;       // for Live's datagram reader (PcMic)
      pub fn mic_stats(&self) -> AudioStatsHandle;
      pub fn send(&self, c: AudioCmd);
  }
  ```
- The thread reconciles each `Apply`:
  - `tap` starts or drops `SoundTap`. `TapError::Denied` gives `Capture(Denied)`, and a success gives `Capture(Allowed)`.
  - `mic_default` calls `engage` or `release`.
  - `MicFeed` runs while `mic_default`.
  - `DefaultInputChanged` calls `reassert` while engaged. `DefaultOutputChanged` calls `SoundTap::rebuild`. `DevicesChanged` re-reads `driver_present`.
- The app (`mac/app.rs`):
  - It holds `MacAudio` (Task 4) and feeds it `connected`/`disconnected` (from `set_conn`), the config's `audio_sharing`, `Control::AudioStatus`, and the `MacAudioEv`s.
  - After each change it computes `plan(now)`, sends `Apply` when the plan differs, and sends `Control::AudioWant` when `want` differs (also on every new session).
  - It adds a `sleep_until(recheck_at)` arm to the loop.
  - Quit sends `Quit`, and waits up to 1 s for the reply before exiting, so the mic is restored and the tap destroyed.
  - `mac/link.rs`: the datagram reader passes `Datagram::Audio { stream: PcMic, .. }` to the `AudioTx`.
  - While the mic feed runs, `log_latency` also logs `mic_stats().snapshot().log_line("PC mic")`. The "Mac sound" line is in the PC's log (Task 9).
- Tray and settings:
  - `TrayModel` gains `audio: Option<AudioMenu>`, where `pub struct AudioMenu { pub on: bool, pub note: Option<String> }`. It is None on Windows.
  - The Mac menu shows a `CheckMenuItem` "Mac sound and mic through PC", which sends `TrayCmd::ToggleAudioSharing`. Below it, a disabled item shows the note.
  - The settings window gets `Setting::AudioSharing`, a checkbox "Mac sound and mic through PC", listed in `settings_shown(true)` after `ReduceWifiHiccups`.
- `Info.plist` gets `NSAudioCaptureUsageDescription`: "CursorJump plays your Mac's sound through your PC's headphones."

- [ ] **Step 1: Write the failing tests.**
  - `plan_changes_send_apply_and_want_only_on_change`: the app-side helper `fn audio_updates(prev: &AudioPlan, next: &AudioPlan) -> (bool, bool)`.
  - `tray_shows_the_audio_switch_and_note_on_the_mac_only`: the model-to-items function.
  - Settings: extend the existing `settings_shown` test to `[ReduceWifiHiccups, AudioSharing]`.
  - `merge_edits` keeps an `audio_sharing` change.
- [ ] **Step 2: Run `cargo test -p cursorjump` on the Mac.** Expected: FAIL.
- [ ] **Step 3: Implement** the wiring.
- [ ] **Step 4: Verify.**
  - `cargo test -p cursorjump` passes, and `cargo clippy --workspace --all-targets -- -D warnings` is clean.
  - `cargo clippy --target x86_64-pc-windows-msvc --workspace --all-targets -- -D warnings` is clean.
  - Reinstall the Mac (`./scripts/mac/install.sh`) and the PC (`scripts\win\install.ps1`) together; this is v4. With both connected, YouTube on the Mac plays in the PC headphones and the Mac is silent.
- [ ] **Step 5: Commit** with `feat(mac): wire audio sharing into the Mac app, with its switch and menu note`.

---

### Task 15: Acceptance, cleanup and handoff (both machines, with the user)

**Files:**
- Modify `docs/ACCEPTANCE.md` (new section "Audio sharing acceptance") and `docs/HANDOFF.md`.
- Delete `app/cursorjump/examples/audio_spike_mac.rs`.

- [ ] **Step 1: Run each check with the user, and record the result and log times.** These are the spec's list, plus the Review Focus items:
  1. YouTube on the Mac: lip sync checked by eye (A-R2), and the Mac speakers silent.
  2. Music on the Mac while Valorant or Roblox runs on the PC.
  3. A Discord call on the Mac using the PC's mic. The Windows mic indicator is on only during the call.
  4. Switching the PC's default output mid-song.
  5. Connecting AirPods to the Mac mid-song: nothing plays from them, and the mic stays CursorJump's.
  6. A 2 s Wi-Fi blip (turn Wi-Fi off and on) with no fallback.
  7. A drop longer than 5 s: the Mac's own devices come back. They return to the PC once reconnected.
  8. The switch, both ways, from the menu and from settings.
  9. A 5 GB file copy (`fsutil file createnew $HOME\Desktop\big5gb.bin 5368709120`) running during music: no stutter, and cursor latency unchanged.
  10. A reboot of each machine. On the Mac, `kill -9` CursorJump while sharing: the speakers come back at once, and the mic comes back at the next launch.
  11. An hour of playback: the PC log's "Mac sound" line shows `0 skips` and a steady buffer.
  12. Cursor latency with audio running: the minute medians stay within 0.5 ms of the session without audio.
  13. The PC volume keys while on the Mac.
  14. The Windows privacy switch off: the Mac menu note and the PC tray suffix show.
- [ ] **Step 2: Fix anything that failed** through SDD as usual, and re-check it.
- [ ] **Step 3: Clean up and test.** Delete the spike. Run `cargo test --workspace` on each machine. Update `HANDOFF.md` with the audio section: what's installed, `install-audio-driver.sh`, the logs to read, and the open items.
- [ ] **Step 4: Commit** with `docs: audio sharing acceptance and handoff`. Then the final whole-branch review, then merge.
