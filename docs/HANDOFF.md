# CursorJump — Handoff

Last updated: 2026-10-19 (end of session 1)

## What this is

CursorJump is a personal, clean-room software KVM. It lets the Windows PC's mouse and keyboard glide onto the MacBook as if the Mac were a third monitor. Copy/paste works both ways now, and file drag-and-drop comes in phase 2.

- Design: `docs/superpowers/specs/2026-10-18-cursorjump-design.md`
- Phase 1 plan (19 tasks): `docs/superpowers/plans/2026-10-18-cursorjump-phase1.md`
- Execution ledger, with every ruling and deferred finding: `docs/superpowers/progress/phase1-ledger.md`
- Repo: https://github.com/MattCodingProjects/cursorjump (private). Branches `main` and `phase1` are identical at the end of this session.

## How to resume

In Claude Code on the PC, in this folder, say:

> Read docs/HANDOFF.md, then continue the phase 1 plan from Task 13 using subagent-driven development.

The controller should:

1. Recreate the SDD workspace by running `bash scripts/sdd-workspace <plan>` from the subagent-driven-development skill.
2. Copy `docs/superpowers/progress/phase1-ledger.md` to `<workspace>/progress.md`. Every task marked `complete` there is done, so don't redo it.
3. Recreate the shared contracts `implementer-contract.md`, `reviewer-contract.md` and `rereview-contract.md`. They were derived from the skill's prompt templates plus the project constraints listed in this file.
4. Keep working on branch `phase1`.

## Status

| Task | What | Status |
|---|---|---|
| 1 | Repo, workspace, CI (Windows + macOS) | ✅ done |
| 2 | Wire protocol | ✅ done |
| 3 | Edge/layout math | ✅ done |
| 4 | Gaming wall rule | ✅ done |
| 5 | US ANSI keymap + KeyRouter | ✅ done (amended by R12) |
| 6 | Mac modifier/held/click/scroll helpers | ✅ done |
| 7 | Latency stats, clipboard model, config | ✅ done |
| 8 | PC + Mac state machines | ✅ done (1 fix round) |
| 9 | Identity, pairing code, pinned TLS | ✅ done (security fix: commit-reveal pairing, R8) |
| 10 | QUIC session | ✅ done (fix: concurrent capped accept, limits, R10) |
| 11 | Discovery (mDNS + broadcast fallback) | ✅ done |
| 12 | Windows input thread | ✅ done (2 fix rounds, review clean). **Hands-on test with the user still to do.** |
| 13 | Windows clipboard | ⏭ next |
| 14 | Windows app wiring, tray, settings, autostart | pending |
| 15–18 | Mac side (run by Claude Code **on the MacBook**) | pending |
| 19 | Acceptance pass (latency, Valorant/Roblox, feel) | pending |

Tests at handoff: `cargo test --workspace` passes. That's cj-core 84, cj-net 14 + 17 loopback, and cursorjump 29. CI is green on Windows and macOS.

## Next steps, in order

1. **Hands-on check of Task 12 with the user at the PC.**
   - Run `RUST_LOG=debug cargo run -p cursorjump -- --diagnose` (set `$env:RUST_LOG="debug"` in PowerShell).
   - Push the mouse off the right edge of the main monitor. The cursor should vanish, and the log should show `hooks: installed`, then Motion/Key messages.
   - Ctrl+Alt+Home brings the cursor back at the same height, and the log shows `hooks: removed`.
   - Hold Shift for over 1 s while crossing, release it "on the Mac", return, then type on the PC. Nothing should be stuck.
   - Check that the left vertical monitor behaves normally, and that idle CPU is under 1 %.
   - Diagnose auto-returns after 20 s as a safety net.
   - The primary monitor reports as 1920×1080, so expect frac ≈ y / 1079.
2. Task 13, then Task 14, on the PC.
3. Tasks 15–18 on the Mac. The user's Mac is set up: Xcode CLT, Homebrew, Rust, gh logged in, repo cloned at `~/Projects/cursorjump`, Claude Code installed. Hand the user a single prompt to paste into Claude Code there, pointing it at this file and the plan.

## Decisions made on the user's behalf (rulings)

All of these are in the ledger with what each costs if it turns out wrong. The headline ones:

- **Transport:** QUIC with pinned mutual TLS instead of the Noise protocol the spec suggested. The spec left transport to the builder.
- **R8, pairing:** the pairing code uses a commit-reveal exchange of per-session nonces. Fixed-fingerprint codes could be brute-forced offline by an attacker on the LAN.
- **R10, session semantics:** any `Err` from `recv_*` means the session is dead. Bad datagrams are skipped. Each session allows 1 bidirectional stream, 4 unidirectional streams and a 48 MiB receive window. The clipboard limit allows 64 bytes of extra wire overhead.
- **R12, keys held across the crossing:** a key held at entry has its auto-repeat dropped, and its key-up goes to Windows, so it never sticks.
- **R13, cursor parking:** on entry the order is hide cursors → park → install hooks → park again. Any single move over 200 px is dropped. The parked position is read back and re-parked if it drifts.
- **R4, commit trailer:** `Co-Authored-By: Claude Opus 5.5`.

## Carried notes for upcoming tasks (from reviews)

- **Task 14/17:**
  - `send_control` is not cancel-safe, so never put it inside `select!`.
  - Call `Session::pair()` before starting any `recv_control` loop.
  - Call `discover()` with a timeout of at least 3 s.
  - Drop `Advertiser` off the async path, since dropping it can block for up to about 1.25 s.
- **Task 15 (Mac injector):**
  - `release_all` must call `ModifierState::update(kc, false)` for each drained key, then post a final flagsChanged with flags == 0 (Caps Lock is excepted).
  - Scroll convention: `Scroll.dy > 0` is wheel-away (scroll up) and `Scroll.dx > 0` is scroll right.
- **Deferred minor findings:** the final whole-branch review must triage them. They're listed in the ledger as `minor (deferred)`.

## Things the user still has to do (not urgent)

- **Clear leftover firewall rules.** Old test programs left Windows Firewall rules behind. Run this in Terminal (Admin):
  ```powershell
  Get-NetFirewallApplicationFilter | Where-Object { $_.Program -match 'cursorjump\\target\\debug\\deps\\' } | Get-NetFirewallRule | Remove-NetFirewallRule
  ```
  Future prompts for test programs can just be cancelled.
- **Check the network profile.** In Settings → Network & internet → Ethernet, the network should be **Private**, not Public. Otherwise discovery of the Mac may be blocked.
