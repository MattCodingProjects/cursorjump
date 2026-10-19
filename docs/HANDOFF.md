# CursorJump — Handoff

Last updated: 2026-10-19 (session 3, on the Mac)

## What this is

CursorJump is a personal, clean-room software KVM. It lets the Windows PC's mouse and keyboard glide onto the MacBook as if the Mac were a third monitor. Copy/paste works both ways now, and file drag-and-drop comes in phase 2.

- Design: `docs/superpowers/specs/2026-10-18-cursorjump-design.md`
- Phase 1 plan (19 tasks): `docs/superpowers/plans/2026-10-18-cursorjump-phase1.md`
- Execution ledger, with every ruling and deferred finding: `docs/superpowers/progress/phase1-ledger.md`
- SDD contracts used for every dispatch: `docs/superpowers/progress/contracts/` (implementer, reviewer, re-review)
- Repo: https://github.com/MattCodingProjects/cursorjump (private). Work is on branch `phase1`; `main` is still at the end of session 1.

## How to resume (Mac: Tasks 15–18)

On the MacBook, in Claude Code, in `~/Projects/cursorjump`, say:

> git pull, check out phase1, read docs/HANDOFF.md, then continue the phase 1 plan from Task 15 using subagent-driven development.

The controller should:

1. Run `git fetch && git checkout phase1 && git pull`.
2. Recreate the SDD workspace by running `bash scripts/sdd-workspace <plan>` from the subagent-driven-development skill.
3. Copy `docs/superpowers/progress/phase1-ledger.md` to `<workspace>/progress.md`, and the three files in `docs/superpowers/progress/contracts/` into the workspace. Every task marked `complete` in the ledger is done, so don't redo it.
4. Adapt the implementer contract's machine lines for the Mac:
   - It's macOS on Apple silicon, so macOS code compiles locally.
   - Windows code (`cfg(windows)`) cannot compile there; CI covers it.
   - Use the Mac repo path.
5. Keep working on branch `phase1`.

## Status

| Task | What | Status |
|---|---|---|
| 1–11 | Core crates, protocol, net, discovery | ✅ done (session 1) |
| 12 | Windows input thread | ✅ done, **hands-on passed** (session 2, after 12b) |
| 12b | Reachable return hotkey **Ctrl+Alt+Shift+H** (the user's 60 % keyboard has no Home), safer diagnose | ✅ done |
| 13 | Windows clipboard | ✅ done, manual check passed (snip → PNG, text) |
| 14 | Windows app: tray, settings, pairing dialog, autostart, wiring | ✅ done (2 fix rounds), manual pass with user passed |
| 15–18 | Mac side: injection, clipboard, app + bundle/signing/autostart (17a/17b), Mac-readiness wall + protocol v2 (17c), Wi-Fi hiccup helper `cursorjump-netd` (18) | ✅ done (session 3, on the Mac); end-to-end, lock screen, Wi-Fi off/on and sleep checks passed; remaining hardening checks deferred to Task 19 |
| 19 | Acceptance pass (latency, Valorant/Roblox, feel) + the deferred Task 18 checks (deep sleep, reboots, monitor unplug, 150 % scaling) | ⏭ next |

Tests at handoff: `cargo test --workspace` passes (184), clippy and fmt are clean, and CI is green on Windows and macOS (commit `2fae075`).

The PC app is installed for testing at `%LOCALAPPDATA%\CursorJump\cursorjump.exe`, with autostart **off**. Config and identity live in `%APPDATA%\CursorJump`. To install properly later, run `powershell -ExecutionPolicy Bypass -File scripts\win\install.ps1`, which also turns autostart on.

## Carried notes for the Mac tasks (binding)

- **Task 15, injector:**
  - `release_all` must call `ModifierState::update(kc, false)` for each drained key, then post a final flagsChanged with flags == 0 (Caps Lock is excepted).
  - Scroll convention: `Scroll.dy > 0` is wheel-away (scroll up) and `Scroll.dx > 0` is scroll right.
- **Tasks 15/17, ignore input while not controlled:** the Mac must **ignore Key/Button downs (and motion) that arrive while it is not controlled**. A PC key-down can be queued ahead of the Mac's own `Leave`. The PC then drops the matching key-up, so the key would stick on the Mac.
- **R26:** `Control::ReleaseAll` (PC → Mac) now means "the PC took the cursor back", via hotkey, hook lost, pause or quit. `MacEvent::ReleaseAll` while controlled emits `[ReleaseAllInjected, StopControl]` and ends control. This is already implemented in `cj-core::mac_state`.
- **R27:** `EchoGuard::mark_applied` clears the last-sent hash. Clipboard send order on both sides:
  1. skip empty
  2. `within_limit`
  3. `EchoGuard::should_send`
  4. send

  On receive, `write_clipboard` then `mark_applied`. Spawn the clipboard watcher once per process.
- **Task 17 (from Task 10/11 reviews):**
  - `send_control` is not cancel-safe, so never put it inside `select!`.
  - Call `Session::pair()` before any `recv_control` loop.
  - Call `discover()` with a timeout of at least 3 s.
  - Drop `Advertiser` off the async path, since dropping it can block for up to about 1.25 s.
- **Reuse from Task 14:** `ui/tray.rs` (`run_tray`, `TrayModel`, `TrayCmd`), `ui/dialogs.rs` (`ask_pairing` uses rfd on macOS), `ui/settings.rs` (with the Mac-only "Reduce Wi-Fi hiccups" toggle), and `paths.rs` (Mac config dir `~/Library/Application Support/CursorJump`). All of these already compile on macOS in CI.
- **Deferred minors:** the final whole-branch review must triage them. They're listed in the ledger as `minor (deferred)`.

## Things the user still has to do (before the first real PC↔Mac test)

- **Set the PC's network to Private.** Go to Settings → Network & internet → Ethernet, and set the network profile type to **Private**. It is currently **Public**.
- **Clear old firewall Block answers.** Windows has saved inbound **Block** rules for CursorJump exes, including the installed one. Run this in Terminal (Admin):
  ```powershell
  Get-NetFirewallApplicationFilter | Where-Object { $_.Program -match 'cursorjump' } | Get-NetFirewallRule | Remove-NetFirewallRule
  ```
  The next time CursorJump starts, tick **Private** and click **Allow**.
- On the Mac, Tasks 15/17 will need the user to grant **Accessibility** (and possibly Input Monitoring) to CursorJump in System Settings → Privacy & Security.

## Session 3 notes (Mac)

- Both apps speak **protocol v2** now (Task 17c). Update the PC and the Mac together, or they refuse each other at Hello.
- Mac install: `./scripts/mac/install.sh` (app + LaunchAgent, Accessibility survives re-signs), and optionally `./scripts/mac/install-netd.sh` in Terminal (root AWDL helper for "Reduce Wi-Fi hiccups"). Both are installed on the user's Mac.
- Mac logs: `~/Library/Application Support/CursorJump/cursorjump.log`; helper log: `/var/log/cursorjump-netd.log`.
- Both the PC and the Mac sessions append to the ledger: pull before writing and append, never overwrite.
- Pending housekeeping: push `cargo test -p cursorjump --no-run` for the Windows CI job (needs a GitHub token with the `workflow` scope).
- After Task 19: the final whole-branch review triages every `minor (deferred)` in the ledger, then merge `phase1` → `main`.
