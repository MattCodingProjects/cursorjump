# Phase 1 acceptance (in progress)

Each line is a spec success bar or a plan Review Focus item. `[x]` = passed hands-on, `[ ]` = not yet checked, `[~]` = partly checked (see notes). Detailed results follow the checklist.

## Checklist

### The bar for success (spec)
- [x] No perceptible lag or stutter on the Mac cursor; motion as smooth as on the PC monitors. (user: "mouse feels smooth")
- [~] Mouse speed and scrolling on the Mac feel the same as on the PC. (scrolling works; a real work session still to judge feel)
- [x] Keys and shortcuts just work: no stuck keys, no doubled keys, Ctrl-shortcuts act as Cmd-shortcuts. (typing and shortcuts confirmed; held-key-at-crossing stress still to try, see Review Focus 1)
- [~] Looks after itself: auto-starts, auto-reconnects, never strands the cursor. (auto-reconnect after Wi-Fi off/on and sleep passed; reboots deferred)
- [ ] Gaming on the PC is never disturbed. (Valorant / Roblox, Step 3 — deferred by the user)
- [x] Nothing but file transfer takes a moment. (crossing, clipboard and reconnect are instant per user)

### Phase 1 goals (spec)
- [x] 1. Mouse handoff across the right edge of the main monitor, proportional.
- [x] 2. Keyboard follows the cursor; nothing reaches Windows while on the Mac.
- [x] 3. Copy/paste both ways: text, formatted text, images.
- [x] 4. Encrypted, paired connection with a 6-digit code confirmed on both screens.
- [x] 5. Zero-config discovery (mDNS; no addresses typed).
- [~] 6. Set and forget: starts at login on both machines and reconnects quietly. (reboots deferred)
- [x] 7. Tray / menu-bar status with a live latency readout and settings.

### Anti-goals (must not happen)
- [x] Added latency, jitter or stutter in cursor movement.
- [x] Mac mouse feel noticeably different from the PC.
- [x] Stuck modifiers, or keystrokes leaking to Windows while on the Mac.
- [ ] The cursor jumping to the Mac in the middle of a game. (Step 3)
- [ ] Input lag or interference in PC games while the cursor is on the PC. (Step 3)
- [x] Manual reconnection or fiddling in normal use.

### Daily life (spec)
- [x] Mac locked: the PC keyboard types the password at the lock screen.
- [x] Pushing onto a sleeping Mac display wakes it.
- [x] Mac drops off Wi-Fi while the cursor is on it: the cursor snaps back at once.
- [ ] Deep sleep (lid closed) while on the Mac: the cursor snaps back; reconnect on wake.
- [x] Emergency hotkey Ctrl+Alt+Shift+H returns the cursor (PC hands-on, Task 12b).
- [x] Pause toggle in the tray disables crossing (PC, Task 14).

### Review Focus (plan)
- [ ] 1. A key or mouse button held while crossing, both directions (hold W or Shift, drag across): nothing stays stuck.
- [~] 2. Wi-Fi drop or Mac sleep while on the Mac: return within ~600 ms, hooks off, reconnect. (Wi-Fi drop passed; deep sleep deferred)
- [ ] 3. Negative-x vertical monitor and non-100 % scaling: edge and fractions still right. (150 % scaling deferred; unplug/replug deferred)
- [ ] 4. Clipboard extremes: a ~40 MB screenshot is skipped without hanging, an empty clipboard, emoji/CJK text, a file copied in Explorer or Finder is ignored.
- [~] 5. Second/unknown device or the Mac's IP changing: no new prompt for the known peer. (pairing survived restarts and reinstalls; unknown-device rejection is covered by tests only)

### Latency (plan Task 19, Step 2)
- [x] Median RTT ≤ 10 ms and verdict Good over 5 minutes on Wi-Fi, with "Reduce Wi-Fi hiccups" off and on. (median 3.4–3.7 ms, 0 spikes; see Latency below)


## Lock screen

- **Date:** 2026-10-19
- **Method:** Locked the Mac with Ctrl+Cmd+Q, crossed over from the PC, and typed the password on the PC keyboard.
- **Result:** PASSED. The Mac unlocked with the user agent alone, so the LoginWindow injector (plan Task 18, Step 2) is not needed.
- **Not covered:** the login window after a logout or a reboot. The agent runs only inside a logged-in session. With FileVault, the pre-boot unlock screen is out of reach for any app.

## End to end (plan Task 17, Step 5)

- **Date:** 2026-10-19
- **Result:** PASSED. Pairing codes matched on both screens and the pairing survived restarts and reinstalls of both apps. The cursor crosses and returns; mouse motion feels smooth; typing, Ctrl→Cmd shortcuts, scrolling and copy/paste both ways all work.
- **Signing:** reinstalling a rebuilt, re-signed app kept the Accessibility grant (no new prompt).

## Wi-Fi hiccup mode (plan Task 18, Step 3)

- **Date:** 2026-10-19
- **Result:** PASSED. With "Reduce Wi-Fi hiccups" on, `awdl0` is down while connected (helper log: "AWDL held down"), is released on disconnect, and is held again on reconnect. The helper re-downs AWDL when macOS brings it back up.

## Hardening (plan Task 18, Step 4)

| Check | Result |
|---|---|
| Wi-Fi off/on while the cursor is on the Mac | PASSED: the cursor snapped back to the PC at once; reconnected by itself when Wi-Fi returned. |
| Mac sleep (Apple menu) while the cursor is on the Mac | PASSED: the cursor came back; crossing again woke the Mac's display. The Mac only reached dark wake, so the session stayed up. |
| Deep sleep (lid closed ≥ 1 min) | Deferred |
| PC reboot (autostart) | Deferred |
| Mac reboot (autostart) | Deferred |
| Unplug/replug the vertical monitor | Deferred |
| Windows scaling at 150 % | Deferred |

Deferred checks were postponed by the user ("it's all working fantastically as intended") and belong to the Task 19 acceptance pass.

## Latency (plan Task 19, Step 2)

- **Date:** 2026-10-19, 14:13–14:25 UTC. MacBook on Wi-Fi, PC wired. Measured from the Mac app's own pings to the PC (every 100 ms, about 600 per minute). The app logs one summary line per minute.
- **Result:** PASSED in both modes. The median is far under the 10 ms bar, and there were no spikes over 40 ms, so the verdict is Good every minute.

| Minute ending (UTC) | Wi-Fi hiccup mode | Median | p95 | p99 | Max | Spikes > 40 ms |
|---|---|---|---|---|---|---|
| 14:14:31 | on (AWDL held) | 3.6 ms | 4.0 ms | 4.5 ms | 7.5 ms | 0 |
| 14:15:31 | on | 3.5 ms | 4.2 ms | 8.3 ms | 19.7 ms | 0 |
| 14:16:31 | on | 3.5 ms | 3.9 ms | 4.3 ms | 6.1 ms | 0 |
| 14:17:31 | on | 3.4 ms | 3.9 ms | 6.2 ms | 8.4 ms | 0 |
| 14:18:31 | on | 3.4 ms | 3.9 ms | 4.3 ms | 8.0 ms | 0 |
| 14:19:31 | off (AWDL released at 14:19:10) | 3.5 ms | 4.1 ms | 5.4 ms | 7.2 ms | 0 |
| 14:20:31 | off | 3.6 ms | 4.1 ms | 4.7 ms | 6.9 ms | 0 |
| 14:21:31 | off | 3.6 ms | 4.1 ms | 5.8 ms | 7.6 ms | 0 |
| 14:22:31 | off | 3.7 ms | 4.2 ms | 6.2 ms | 12.7 ms | 0 |
| 14:23:31 | off | 3.7 ms | 4.1 ms | 4.5 ms | 6.2 ms | 0 |
| 14:24:31 | off | 3.6 ms | 4.1 ms | 5.1 ms | 6.5 ms | 0 |

The minute ending 14:19:31 includes about 20 s from before the switch.

- **Takeaway:** on this network today, Wi-Fi alone is well within the bar, so a USB-C Ethernet adapter isn't needed. Hiccup mode made only a small difference (median about 0.2 ms lower). The off run's p99 and max weren't worse. "Reduce Wi-Fi hiccups" is left on as a safeguard against AWDL bursts on busier days.

## Deferred by the user

- Games (Step 3): Valorant and Roblox, the in-game wall, Alt+Tab, and no lag or anti-cheat warnings.
- Hardening (Task 18 Step 4): deep sleep, reboots of both machines, unplugging the vertical monitor, 150 % scaling.
- Review Focus 1 (held key or button while crossing) and 4 (clipboard extremes).
- Step 4 (feel): a real work session.

---

# Phase 2 acceptance: file drag-and-drop (Task 18, in progress)

Started 2026-10-26 with the user at both machines. Both run `phase2` (Mac installed from 875f8ee at 17:50Z, PC at 17:52Z; protocol v3, connected 17:51:44Z). Times are the Mac log's (UTC). Same marks as above: `[x]` passed, `[ ]` not yet checked, `[~]` partly, `[!]` failed or flaky.

## Checklist (plan Task 18 Step 2, plus the hand checks from the reviews)

### PC → Mac
- [x] A single file onto the Mac desktop (17:53:52Z: a 213 KB PDF, every byte here 15 ms after the drag armed, dropped and done).
- [x] A folder (17:57:49Z: `sangfroid-netlify-v2` onto the Desktop).
- [x] Several files at once (18:03:41Z: 3 PDFs/docs in one drag, all three written).
- [~] Drops into WhatsApp and Gmail (Chrome): the user reported "all three worked" for the round that asked for them, but the log shows only the Desktop drop in that round. Both passed in the Task 15 smoke (`--smoke-drag`), not yet seen in the installed app's log.
- [ ] Downloads in a Finder window (the drops so far went to the Desktop).
- [ ] Discord (skipped on the Mac by the user's choice; still to do on the PC side).
- [x] Cancel with ✕ (18:06:45Z: a Roblox `.mp4` dropped on the Desktop, ✕ 5 s later → `Failed(Cancelled)`, nothing left on the Desktop, no `.cjpart`; the incoming cache kept only the 0-byte placeholder and empty `.fetch` dirs, which the 24 h sweep removes).
- [x] Dragging back across the edge mid-drag (17:53:49Z, from the log: the drag went home to the PC, `NotDropped`; the user reported nothing stuck).
- [ ] Esc mid-drag.
- [ ] A 5 GB file with the cursor feeling unchanged (latency median during the copy within 2 ms of idle). The user was given the steps (`fsutil file createnew $HOME\Desktop\big5gb.bin 5368709120`); not run yet. Idle reference today: median 3.1–3.3 ms, p95 3.4–3.7 ms, 0 spikes.
- [ ] Wi-Fi drop mid-copy (pill "Copy failed — … disconnected", no `.cjpart` left).
- [ ] Mac sleep mid-copy ("went to sleep").
- [ ] A Chromium target (Gmail) while the file is still arriving: the release is held until it has, ✕/Esc cancel.
- [ ] Hand check: the ghost image follows a PC-mouse Chrome drag on the Mac.
- [ ] Hand check (PC fix 39f70d9): no frozen file icon or "+ Copy" left at the PC screen's centre during a PC→Mac drag.

### Mac → PC
- [x] Single items (an image at 17:57:06Z and 17:57:16Z, then 17:58:24Z and 18:04:21Z) and 3 files at once (18:03:48Z): each caught by the strip's late path and sent; the user confirmed they landed on the PC.
- [!] Two drags didn't go (17:54:01Z and 18:04:17Z, out of 8 tries). The first reached the strip 13 ms after the posted release but was never dropped on it, and left the strip when the wait ended. The second never reached the strip. Every drag so far, good or bad, was caught only after the release (the late path, R48a), never before it, even though the strip is 8 pt (R54). Likely cause: the strip only shows within 48 pt of the edge, and a quick PC-mouse sweep reaches the edge before it does. To investigate before merge.
- [ ] Drags out of WhatsApp, Discord and a web page in the installed app (all three passed in the Task 16 smoke).
- [ ] Hand check: a WhatsApp photo is cloned out of its app's tmp folder (R53).
- [ ] Hand check: a Chrome image arrives under its fallback name, e.g. `image.jpg` (R52).
- [ ] Hand check: the PC handles a `Leave` while its left button is still down.
- [ ] A Mac-to-PC copy finishing during a game, the pill held back until Alt+Tab.

### Both / gaming
- [ ] The gaming wall during a drag, with Valorant or Roblox running in the background and no change in behaviour.
- [x] Crossing still feels normal on the new build (the user moved across and back before the first drag).

## Found during acceptance
- **Mac → PC drags sometimes don't go** (see the `[!]` line). To investigate before merge.
- **A Mac drag can't come back to the Mac** (user, 2026-10-26). Once a Mac drag has crossed to the PC, the PC's edge is a wall for it (by design: `pc_state` `incoming_drag_makes_the_edge_a_wall`), because handing it to the PC dropped it on the Mac's strip. Overshooting means dropping it on the PC and starting again. The user wants it carried back, to the polish backlog: the PC would hand it back and the Mac would start a new drag of the same files under the cursor.
- **Roblox takes clicks made on the Mac** (user, reported by the PC session). Since phase 1; workaround: close or minimise Roblox first. Left for now at the user's request.
