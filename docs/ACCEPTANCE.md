# Phase 1 acceptance (in progress)

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
