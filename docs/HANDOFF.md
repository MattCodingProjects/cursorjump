# CursorJump — Handoff

## Audio sharing: everything built; both directions work; acceptance (Task 15) in progress

Last updated: 2026-11-16 (early morning SAST), on the Mac. Work is on branch **`audio`**, made from `phase2`. **All plan tasks 1–14 are done and reviewed, plus the follow-up P16.** Mac sound plays in the PC's headphones, and the PC's mic reaches Mac apps. The user's verdict on the first run: "working flawlessly". What's left is the Task 15 acceptance checklist, cleanup, the final review and the merge.

- **Spec:** `docs/superpowers/specs/2026-10-25-audio-sharing-design.md`
- **Plan:** `docs/superpowers/plans/2026-11-15-audio-sharing.md` (Task 12 carries an amendment block from the spike)
- **Spike findings:** `docs/superpowers/progress/audio-spike.md` (S1–S5, F1–F6)
- **Ledger:** `docs/superpowers/progress/audio-ledger.md`. It has rulings P1–P16b, per-task notes, deferred minors and hands-on results. **Read it first.** Pull before writing, and append only.

### What was built on the Mac
- **Task 10, the driver.** `app/cursorjump-audio-driver/` is a HAL plug-in with "CursorJump Microphone" (visible) and "CursorJump Mic Feed" (hidden), on one clock. Install it with `./scripts/mac/install-audio-driver.sh` (`--uninstall` to remove). **It is installed on the Mac.**
- **Task 11, the default input.** `mac/audio/devices.rs` switches the default input to CursorJump Microphone and always restores it: on release, on quit, and on the next launch after a crash. It falls back to the built-in mic if the saved device fails.
- **Task 12, the tap.** `mac/audio/tap.rs` runs a muted process tap of everything except CursorJump.
  - **P12:** a self-probe decides whether capture is allowed. A refused tap still mutes (F6), so when the probe says Denied, the app makes no tap and the Mac keeps its own sound.
  - Only the tap's input stream is used, and the IOProc guards the stream layout.
  - The sample rate is followed live.
- **Task 13, the feed.** `mac/audio/feed.rs` plays the PC mic into the feed device. `stop` returns `Option<AudioRx>` (P13).
- **Task 14, the wiring.** `mac/audio/mod.rs` (the `cj-audio` worker), `mac/app/audio.rs`, `mac/link.rs`, the tray switch and note, the settings checkbox, and `NSAudioCaptureUsageDescription` in Info.plist.
  - The tap starts on a helper thread, `cj-audio-tap`. Quit restores the input, with a fallback `recover()` if the worker doesn't answer.
  - **P14:** the mic route engages only while the feed runs.
  - **P15:** turning the switch off and on re-probes after a refusal.
- **P16 / P16b.** "A Mac app is listening" is worked out from Core Audio's process list. CursorJump itself and Siri (`com.apple.CoreSpeech`) don't count. Calls that record through private aggregates do count.
- **A fix to Task 6's audio pipe.** It allocated on the realtime thread on macOS; this is fixed in `add47f6`. CI now runs that test on macOS.

### What is installed right now
- **PC:** the `audio` build (protocol v4), installed 2026-11-16 around 01:35Z. There have been no PC code changes since, so it is current.
- **Mac:** `/Applications/CursorJump.app` is built from `63be3a9` (protocol v4). **It does not have P16 yet** (`f90d026`, `d04d6c6`). Reinstall it first: `./scripts/mac/install.sh`. Run it in the user's Terminal, not in Claude Code (see the tooling notes).
- The macOS "System Audio Recording" permission is granted to CursorJump. The first prompt worked: the probe waited 28 s for the click, then got Allowed.

### Task 15 acceptance so far
Recorded in the ledger. They still need to be written into `docs/ACCEPTANCE.md` under a new "Audio sharing acceptance" section.
- ✅ **YouTube on the Mac** plays in the PC headphones, and the Mac's speakers are silent.
- ✅ **The PC mic in a Mac app:** a QuickTime recording from CursorJump Microphone was clear.
- ✅ **The Windows mic indicator** goes off when no Mac app listens.
- ✅ **Cursor latency is unchanged** with audio on: median 3.1–3.4 ms, 0 spikes.
- ✅ **PC mic stream:** 0 lost, 0 underruns.
- ⏳ **Next batch, already sent to the user:**
  1. Reinstall the Mac.
  2. Lip sync, checked by eye.
  3. The switch, off and on, from the menu, plus the settings checkbox.
  4. A 2 s Wi-Fi blip on the Mac with no fallback.
  5. A Wi-Fi drop over 5 s: the Mac's speakers come back, then it returns to the PC.
  6. The PC volume keys while the cursor is on the Mac.
- ⏳ **Still to do after that:**
  - Music during Valorant/Roblox.
  - A Discord call on the Mac, with the PC mic indicator on only during the call. This is also the live proof of P16b.
  - Switching the PC's default output mid-song.
  - AirPods or wired headphones on the Mac mid-song (the I1/N4 gate: nothing from the Mac's mic, and AirPods must not drop to call quality).
  - A 5 GB copy during music.
  - A reboot of each machine, and `kill -9` CursorJump on the Mac (the speakers come back at once, and the mic comes back at the next launch).
  - An hour of playback (the PC log's "Mac sound" line shows 0 skips).
  - The Windows mic privacy switch off (the Mac menu note and the PC tray suffix).

### After acceptance
1. Fix anything that failed, through SDD.
2. Delete `app/cursorjump/examples/audio_spike_mac.rs`, and drop the spike's `[profile]`/dependency leftovers if unused.
3. Run `cargo test --workspace` on each machine.
4. Run the final whole-branch review. It should start from the "FINAL-REVIEW PRIORITY" and "deferred" lines in the ledger.
5. Merge. `phase2` still has to merge into `main` first: the Mac→PC drag-strip misses are still open (below). Then rebase `audio` onto `main`.

### Tooling notes from the Mac session (2026-11-16)
- **Core Audio IO:** commands from Claude Code need the sandbox off (`dangerouslyDisableSandbox`). They count as Terminal for TCC, and Terminal has System Audio Recording and Microphone.
- **Signing:** `codesign` with "CursorJump Dev" fails from Claude Code's shell (`errSecInternalComponent`, no keychain access). So `install.sh` and `install-audio-driver.sh` must be run by the user in Terminal.
- **Logs:** in zsh, `log` is a builtin, so use `/usr/bin/log show …`. The app log is in UTC; `log show` uses local time (SAST, UTC+2).
- **Tap hands-on tests** (`mac::audio::tap`) mute the Mac. Run them only with the user listening. The feed and devices tests only touch the virtual devices.
- `ParrotAudioPlugin.driver` is a third-party HAL plug-in on this Mac. Clean-room rule: don't read it.

---

## Phase 2 (file drag-and-drop): all code done; acceptance (Task 18) in progress

Last updated: 2026-10-26 (end of day), on the Mac, on top of the PC session's evening update. Branch **`phase2`** holds every task's code, reviewed. **Both machines now run the `phase2` build** (PC installed from 875f8ee and connected to the Mac). Task 18 acceptance is in progress, driven by the Mac session; where it stands is below. The repo is now **public** (the user's choice, for free CI minutes).

- **Spec:** `docs/superpowers/specs/2026-10-25-file-drag-and-drop-design.md`
- **Plan:** `docs/superpowers/plans/2026-10-25-file-drag-and-drop.md` (Tasks 15–16 carry amendment blocks from the Mac spike)
- **Spike findings:** `docs/superpowers/progress/dnd-spike.md` (PC 1a, Mac 1b, F1)
- **Ledger:** `docs/superpowers/progress/phase2-dnd-ledger.md` (rulings R1–R55, per-task notes, hands-on results). Read it first.
- **Done:**
  - **PC:** Task 1a, Tasks 2–13 and 11b; hands-on passed.
  - **Mac:** Task 1b (spike) and Tasks 14–17; hands-on smokes passed for the pill, drags arriving from the PC, and the edge catcher.
  - Each task went through an implementer, a reviewer and re-reviews.
- **How the Mac side works, in short:**
  - **PC → Mac:** one drag carries a file promise plus a file URL (R47). Finder, the Desktop and WhatsApp take the promise and drop at once. Chromium targets (Gmail) get the file URL, and the posted release is **held** until the file has fully arrived (approved by the user; ✕ or Esc cancels).
  - **Mac → PC:** an 8 pt invisible strip at the left edge catches the drag (R54). A late-starting drag is caught at the release instead (R48a). The edge stays a wall for window drags and text selections.
- **Task 18 status (end of 2026-10-26, Mac session).** Both machines run the `phase2` build (Mac 875f8ee at 17:50Z, PC 875f8ee at 17:52Z), connected on protocol v3. Full results with log times: `docs/ACCEPTANCE.md`, "Phase 2 acceptance".
  - **Passed:**
    - PC → Mac: a file, a folder and 3 files at once onto the Desktop; ✕ cancel (nothing left behind); dragging back to the PC mid-drag.
    - Mac → PC: single items and 3 files at once.
    - The cursor still feels normal; latency median 3.1–3.3 ms all session.
  - **Failed / flaky:** Mac → PC missed 2 of 8 drags (17:54:01Z, 18:04:17Z). Every Mac → PC drag was caught only through the late path (after the release), never by the strip beforehand. Likely cause: the strip only shows within 48 pt of the edge, and a quick PC-mouse sweep gets there first. **Investigate and fix before merge**; start from those two log windows in `~/Library/Application Support/CursorJump/cursorjump.log`.
  - **Still to do with the user:**
    - The 5 GB file with the latency check. On the PC: `fsutil file createnew $HOME\Desktop\big5gb.bin 5368709120`.
    - PC → Mac: drops into WhatsApp and Gmail seen in the installed app's log (the user reported them working; the log didn't show them); a Downloads Finder window; Esc mid-drag; Wi-Fi drop mid-copy; Mac sleep mid-copy; a Gmail drop while the file is still arriving (held release).
    - Mac → PC: drags out of WhatsApp, Discord and a web page.
    - Discord on the PC.
    - The gaming checks (Valorant/Roblox running; a Mac → PC copy finishing during a game, the pill held until Alt+Tab).
    - Hand checks:
      - the ghost image follows a PC-mouse Chrome drag on the Mac;
      - a WhatsApp photo is cloned out of its app's tmp folder (R53);
      - a Chrome image gets its fallback name (R52);
      - the PC handles a `Leave` while its left button is down;
      - no frozen icon or "+ Copy" at the PC's centre (39f70d9).
  - **New user request (polish backlog):** let a Mac drag that crossed to the PC come back to the Mac. Today the PC's edge is a wall for it, by design.
- **Next:**
  1. Fix the Mac → PC misses (SDD as usual), reinstall the Mac, and re-check.
  2. Finish the "still to do" list with the user, and record it in `ACCEPTANCE.md`.
  3. Delete the spike examples (`app/cursorjump/examples/dnd_spike_*.rs`). Finish this file (plan Task 18 Step 3). Run `cargo test --workspace` on each machine.
  4. The final whole-branch review (fable), then merge.
- **Open issue (user-reported, not yet worked on):** with **Roblox** open on the PC (at any window size), clicks made on the Mac also land in Roblox, and Roblox draws its cursor at the PC's park point (the screen centre). Only Roblox does it, and it has been there since the phase 1 build. Workaround: close or minimise Roblox before moving to the Mac. The user asked to leave it for now.
- **PC drag-image fix (39f70d9) still needs its eye check:** during a PC→Mac drag, confirm no frozen file icon or "+ Copy" stays at the centre of the PC screen. If it does, the smoke test compares three hiding modes: `CJ_SMOKE_MODE=ride`, `CJ_SMOKE_IMAGE=desc-noimage|nodesc|show`, then `cargo test -p cursorjump smoke_catcher_logs_offer -- --ignored --nocapture`.
- **Polish backlog** (in the ledger): carrying a Mac drag back from the PC (user, Task 18), Mac→PC web-link drags (R55), swapped mouse buttons, uniquifying sanitized names, full Unicode case folding, and a few rare few-ms races.
- **Quarantine:** files received on the Mac get no quarantine flag (R22). They are treated like a local copy.

---

Last updated: 2026-10-19 (end of session 3, on the Mac). **Phase 1 is complete and merged to `main`.**

## What this is

CursorJump is a personal, clean-room software KVM. The Windows PC's mouse and keyboard glide onto the MacBook as if the Mac were a third monitor, and copy/paste works both ways. File drag-and-drop is phase 2.

- Design: `docs/superpowers/specs/2026-10-18-cursorjump-design.md`
- Phase 1 plan: `docs/superpowers/plans/2026-10-18-cursorjump-phase1.md`
- Execution ledger, with every ruling and finding: `docs/superpowers/progress/phase1-ledger.md`
- Acceptance results: `docs/ACCEPTANCE.md`
- Open follow-ups, including the deferred hands-on checks: `docs/superpowers/progress/phase2-followups.md`
- SDD contracts: `docs/superpowers/progress/contracts/` (implementer, reviewer, re-review)
- Repo: https://github.com/MattCodingProjects/cursorjump (private). `main` and `phase1` both point at the merged phase 1. Start new work on a new branch from `main`.

## Status

| Task | What | Status |
|---|---|---|
| 1–11 | Core crates, protocol, net, discovery | ✅ session 1 |
| 12, 12b, 13, 14, 14c | Windows input thread, return hotkey Ctrl+Alt+Shift+H, clipboard, app (tray, settings, pairing, autostart), re-cross guard | ✅ session 2 (PC) |
| 15, 16 | Mac injection, power, permissions; Mac clipboard | ✅ session 3 (Mac) |
| 17a, 17b, 17c | Signing, bundle, LaunchAgent; Mac app wiring; "Mac not ready" wall + live Accessibility detection, **protocol v2** | ✅ session 3 |
| 18 | Lock-screen spike (passed with the user agent alone); Wi-Fi hiccup mode (root helper `cursorjump-netd`) | ✅ session 3 |
| 19 | Acceptance: checklist + latency (median ≈3.5 ms on Wi-Fi, 0 spikes, both modes) | ✅ partly; games, feel and hardening leftovers deferred by the user |
| — | Final whole-branch review (3 area reviewers) + fixes; CI gates (fmt, clippy `-D warnings`, Windows app tests, netd tests) | ✅ merged: `f155ecf`, `0756335` |

CI is green on Windows and macOS. The Windows app tests run in CI now (83 pass, 2 ignored because they write the live clipboard).

## What is installed right now

Both machines run builds from **before the final-review fixes**. They're compatible (both protocol v2) and working, but they lack the final fixes:
- remainder-keeping pointer/wheel scaling
- discovery fallback
- secure-desktop return (Ctrl+Alt+Del / Win+L)
- XBUTTON check
- private-clipboard skip
- Mac-only settings window

To pick those up, reinstall both from `main`:
- **PC:** `git pull`, then `powershell -ExecutionPolicy Bypass -File scripts\win\install.ps1` (installs to `%LOCALAPPDATA%\CursorJump`, autostart on). Config and identity are in `%APPDATA%\CursorJump`.
- **Mac:** `./scripts/mac/install.sh`. It installs `/Applications/CursorJump.app` and the LaunchAgent `com.cursorjump.agent`, signed "CursorJump Dev". Accessibility survives re-signs. If the build hangs at `codesign`, macOS is asking for keychain access: click **Always Allow**.
- **Mac Wi-Fi helper (root):** already installed. Re-run `./scripts/mac/install-netd.sh` in Terminal only if `app/cursorjump-netd` changes. `--uninstall` removes it.

Pairing is stored in both config files. It survived every reinstall so far, so no re-pairing is needed.

## Where to look when something's off

- **Mac app log:** `~/Library/Application Support/CursorJump/cursorjump.log`. It logs a latency summary every minute.
- **Mac helper log:** `/var/log/cursorjump-netd.log`.
- **PC log:** `%APPDATA%\CursorJump\cursorjump.log`.
- **Emergency return to the PC:** Ctrl+Alt+Shift+H.

## How to resume

- **Deferred hands-on checks** (games, deep sleep, reboots, monitor unplug, 150 % scaling, held keys at crossing, clipboard extremes, Ctrl+Alt+Del while on the Mac): see the first section of `phase2-followups.md`, and record results in `docs/ACCEPTANCE.md`.
- **Polish pass:** pick items from `phase2-followups.md` and run them through SDD as usual (implementer → reviewer → re-review, with the contracts in `docs/superpowers/progress/contracts/`).
- **Phase 2 (file drag-and-drop):** needs its own design pass first (brainstorm → spec → plan), as the spec says.

## Working rules that proved useful

- **Two sessions, one ledger:** both the PC and the Mac sessions write to the ledger. Pull before writing, and append; never overwrite the other session's lines.
- **Implementer contract machine lines:** adapt them per machine. The Mac compiles macOS code. Windows code type-checks on the Mac with `cargo clippy --target x86_64-pc-windows-msvc` and stub C compilers (ring's build only needs a CC for check/clippy). CI runs it for real.
- **Never run the real app from a test or smoke run while the other machine is live.** The PC will pair with it.
- **Live clipboard:** tests that touch it must save and restore it with a drop guard, or be `#[ignore]`d.
- **Protocol changes:** bump `PROTOCOL_VERSION` and reinstall both machines together.
