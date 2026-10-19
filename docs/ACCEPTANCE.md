# Phase 1 acceptance (in progress)

## Lock screen

- **Date:** 2026-10-19
- **Method:** Locked the Mac with Ctrl+Cmd+Q, crossed over from the PC, and typed the password on the PC keyboard.
- **Result:** PASSED. The Mac unlocked with the user agent alone, so the LoginWindow injector (plan Task 18, Step 2) is not needed.
- **Not covered:** the login window after a logout or a reboot. The agent runs only inside a logged-in session. With FileVault, the pre-boot unlock screen is out of reach for any app.
