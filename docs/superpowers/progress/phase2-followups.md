# Phase 1 follow-ups (triaged in the final review, 2026-10-19)

The final whole-branch review marked these PHASE 2. They are real but low impact. Each is a candidate for a polish pass before or alongside phase 2 (file drag-and-drop). Ledger line numbers refer to `phase1-ledger.md`.

## Not yet checked by hand (from `docs/ACCEPTANCE.md`)
- Games: Valorant and Roblox, wall in game, Alt+Tab, no lag or anti-cheat warnings.
- Hardening: deep sleep (lid closed ≥ 1 min), PC reboot, Mac reboot, unplugging the vertical monitor, 150 % scaling.
- Review Focus 1 (key or button held while crossing) and 4 (clipboard extremes: ~40 MB screenshot, a copied file).
- Ctrl+Alt+Del / Win+L while on the Mac: check that Mac modifiers are released (new desktop check, f842928).
- Caps Lock state across machines (Mac `ModifierState` starts with Caps off).
- Mouse movement on the Mac while the PC sends a ~30 MB clipboard image (motion shares the connection with clipboard data).

## Windows app
- Keyboard-hook loss is not detected (the watchdog only watches the mouse). Fix: raw keyboard input while on the Mac.
- Desktop check compares against the literal name "Default"; use the name from `GetThreadDesktop`.
- `CanIncludeInClipboardHistory = 0` alone stops clipboard sync; consider honouring only the exclude/cloud flags.
- Copy the clipboard watcher's keep-latest design from the Mac (it currently never exits); fix its doc comment.
- `read_raw_image` should fall back from a bad CF_DIBV5 to CF_DIB.
- The return hotkey depends on the app loop. Return locally if still on the Mac 1 s after the hotkey.
- PC sleep while on the Mac: check whether input swallowed by the hooks resets the idle timer, and use `SetThreadExecutionState` if not.
- The gaming rule is not re-checked during a stay on the Mac.
- The pairing MessageBox can take focus over a game, and stays up after the session drops.
- Logs rotate only at startup; Forget dialogs can stack; the error-box wording is wrong; an empty config file applies the defaults; `--diagnose-seconds` alone is misleading.
- `input_thread.rs` size (≈1400 lines); custom app cursors stay visible while on the Mac.

## Mac app, helper, scripts
- netd: keep a marker in `/var/run` so the restore at start only undoes our own AWDL down.
- `wake_display` is a synchronous call before the warp on every crossing. Measure it; warp first, or wake only when the display is asleep.
- The clipboard watcher reads and converts images even while disconnected.
- A quit Apple event (logout, Activity Monitor) may skip the clean quit.
- `release_plan` posts button-ups last (an interrupted Cmd-drag ends without Cmd).
- Clipboard: re-read once after ~50 ms to catch multi-step writes by other apps.
- The settings window is too large now that the Mac shows one setting.
- Scripts: bootstrap retry after EIO; `bundle.sh` identity check (SHA-1 hashes, `CARGO_TARGET_DIR`); install.sh should fail if the old copy keeps running; install via `$dest.new` then mv; remove the netd plist on a failed verify; make-signing-cert wording.
- The "Accessibility checks are not answering" warning needs a rate limit; the manual-start reopen gives up after 10 s; KindLimit summary wording.
- A real "Start at login" toggle on the Mac (write or remove the LaunchAgent plist; never boot itself out).

## Core and network
- A stale Leave can split the state when a Mac push-out races a PC force-return. Needs a stay id in Enter/Leave, i.e. protocol v3.
- A protocol-version mismatch is invisible to the user. Log it, and show a tray hint ("update the other computer").
- The identity is regenerated on any read error. Regenerate only on NotFound or a parse error.
- mDNS picks an arbitrary address on a multi-homed Mac. Return every address and try them in turn.
- The broadcast responder is lost if mDNS registration fails.
- Forgetting the pairing on one side only causes reconnect churn. Add a "paired" flag in Hello.
- The broadcast responder answers anyone (small amplification). Rate-limit it, or answer private subnets only.
- `Config::save`: unique temp name plus fsync. `try_load` should reject an empty file.
- DIB V2/V3 header offsets; odd-width 24 bpp test; beacon name cap; vanish-test slack; garbage-datagram and decode-error tests; client-verifier intermediates test; `identity.key` mode 0600.
- Give the control stream priority over clipboard data (`SendStream::set_priority`).
- CI: `actions/checkout` v5; the toolchain is defined twice.
