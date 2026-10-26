# CursorJump — Handoff

## Phase 2 (file drag-and-drop): PC side done, Mac side next

Last updated: 2026-10-26, on the PC. Branch **`phase2`** is green in CI on Windows and macOS. It is not merged, and nothing from it is installed yet.

- **Spec:** `docs/superpowers/specs/2026-10-25-file-drag-and-drop-design.md`
- **Plan:** `docs/superpowers/plans/2026-10-25-file-drag-and-drop.md`
- **Spike findings:** `docs/superpowers/progress/dnd-spike.md`
- **Ledger:** `docs/superpowers/progress/phase2-dnd-ledger.md`. It holds every ruling (R1–R43), the per-task notes, and the hands-on list. Read it first.
- **Done on the PC:**
  - Task 1a (spike).
  - Tasks 2–13, plus Task 11b (dragged web links are downloaded). Each task went through an implementer, a reviewer and fix rounds.
- **Next, on the Mac:**
  1. Start a Mac session and seed the SDD ledger from `phase2-dnd-ledger.md`.
  2. Run Task 1b, the Mac spike for A1–A3 and A7 (file promises, a lazy file-url, NSDraggingSession from a posted press, the Mac catcher panel).
  3. Run Tasks 14–17 (Mac pill, source, catcher, wiring).
  4. On the Mac, run `cargo test -p cursorjump common::transfer -- --nocapture` and confirm there are no "skipped:" lines.
  5. Ledger notes to carry into those tasks:
     - The Task 17 bounce path must feed the real drag and suppress BeginDrag/CancelDrag.
     - The Mac injector must count the PC-held left button from BeginDrag on.
     - Reuse `common::link_fetch` for Discord/web links.
- **PC hands-on demos done (2026-10-26):** the pill, the catcher and the incoming drag all passed with the user. One cosmetic issue is open: while a PC drag rides on the Mac, Explorer's drag image and copy cursor stay visible at the PC park point (see the ledger's Hands-on lines). Fix or accept it before acceptance.
- **Then:** Task 18, acceptance with the user on both machines. This includes the hands-on smoke tests already built on the PC (pill, catcher, incoming drag; see the ledger's "Hands-on list"). After that, the final whole-branch review, then merge.
- **Protocol is v3.** Install both machines together, and only once phase 2 is finished. A v2 build and a v3 build won't connect to each other.

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
