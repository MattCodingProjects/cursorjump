# CursorJump — Audio sharing design

Date: 2026-10-25
Status: Draft for review

Builds on the phase 1 design (`2026-10-18-cursorjump-design.md`). Everything there still holds, especially the anti-cheat rules and "seamless or binned". It gets built **after** file drag-and-drop (`2026-10-25-file-drag-and-drop-design.md`).

## Intent

The user sits at the PC wearing **wired headphones plugged into the PC**, while also working on the Mac through CursorJump. They want to hear the Mac in those same headphones (YouTube, music, calls and Discord on the Mac, notification sounds) without switching anything. When they talk on a call or in Discord on the Mac, the Mac should hear them through the **PC's microphone**.

The user's rule, in their words: **"All audio and microphone should be prioritised on PC."** The PC's audio devices are the home base for both machines.

## Goals (decided with the user)

1. **Mac sound plays on the PC.** Everything the Mac plays goes to the PC's current default output (the wired headphones), mixed in with whatever the PC is playing.
2. **The PC's mic is the Mac's mic.** Mac apps (Discord, calls, the browser) hear the user through the PC's current default microphone.
3. **The PC is untouched.** The PC's own sound and mic keep working exactly as today. Games and Discord on the PC behave as they do now.
4. **Mac speakers go quiet.** While the Mac's sound is going to the PC, nothing plays from the Mac's speakers or AirPods, so nothing plays twice.
5. **Always on while connected.** It doesn't depend on which screen the cursor is on. As long as the two machines are connected, the Mac's sound goes to the PC and the PC's mic serves the Mac.
6. **Fallback, only on two triggers.** The Mac goes back to its own speakers, AirPods and mic only when:
   - the two machines lose their connection to each other; or
   - the user turns audio sharing off by hand in settings (also in the Mac menu-bar menu, *Claude's call*).

   It switches back by itself when the connection returns, or when sharing is turned back on. Nothing else triggers it: not the PC being locked or idle, and not where the cursor is.
   - *(Claude's call)* **Brief blips don't count.** Phase 1 drops the session after 600 ms of silence, so a short Wi-Fi stall looks like a disconnect. For audio, the Mac stays silent through a gap of up to about 5 seconds rather than jumping to its speakers, and resumes on the PC if the connection comes back in time. It only falls back if the connection stays down. This stops a Wi-Fi blip from switching the mic mid-call or blasting a song out of the MacBook.
   - *(Claude's call)* **A missing PC device.** If the PC is connected but can't provide one direction, the Mac falls back for that direction only, and the menu bar says why in plain words. For example, Windows blocks mic access or there's no mic, or Windows has no output device at all.
7. **No noticeable delay.** Video sound must stay in sync with the picture (YouTube on the Mac must not look dubbed). The user's voice on calls must not lag noticeably. It must hold up over the Mac's Wi-Fi.
8. **Music quality.** Music sounds as good as it would played on the PC: stereo, full quality, no audible compression artefacts.
9. **Volume.** The PC's volume controls the overall loudness. The Mac's own volume slider still works, setting how loud the Mac is within the mix.
   - *(Claude's call)* **Volume keys.** Volume and mute keys on the PC keyboard (if it has them, for example on an Fn layer) always control the PC's master volume, even while the cursor is on the Mac. Today the phase 1 hook swallows them while on the Mac. Letting just these keys through to Windows must not disturb the phase 1 stuck-key and anti-cheat guarantees.

## Follows the PC's defaults (decided)

"Prioritised on PC" means the PC's **default** output and input, whatever they are at the moment. If the user switches the PC's default output (say, to speakers) or default mic, the Mac's sound and mic follow that switch, without restarting anything.

## Non-goals

- Sending PC sound to the Mac (the reverse direction).
- Per-app routing (for example "only Discord goes to the PC").
- Surround sound, or sharing audio when the machines aren't connected.
- Video, camera or screen sharing.

## Anti-goals (would make it a failure even if it technically works)

- Crackles, pops, dropouts or robotic sound, especially on Wi-Fi.
- Video sound drifting out of sync with the picture, or a delay that builds up over a long session.
- **Any effect on cursor latency or feel.** Audio must never delay input datagrams.
- Any effect on PC games, or anything an anti-cheat could flag (see Constraints).
- The Mac playing sound from its own speakers at the same time as the PC.
- The Mac stuck with no sound or no mic after a disconnect, sleep or reboot. The Mac's original devices must always come back.
- The user having to pick devices by hand in macOS Sound settings or in each app for normal use.

## Constraints

- **Anti-cheat (from phase 1, still binding).** On the PC, use only ordinary audio APIs, the way any media or voice app plays and records sound: WASAPI shared mode on the default devices. No hooking or injecting into other processes' audio, no audio drivers or virtual devices on the PC, and no reading other processes' memory.
- **The PC mic is only used when the Mac needs it** *(Claude's call)*. The PC captures its microphone only while a Mac app is actually listening to the shared mic, not all the time it's connected. Windows shows a "mic in use" indicator, and it shouldn't be on permanently. Windows' "let desktop apps access your microphone" privacy setting must be on, and if it's off the app should say so plainly.
- **Mac side needs a one-time install.** For Mac apps to see the PC's mic as a microphone, the Mac needs a virtual audio device, a Core Audio driver installed once with the admin password, like the Wi-Fi helper in phase 1. Capturing the Mac's sound may use the same device, or macOS's own audio-tap API (Core Audio process taps can capture system audio and mute local playback). That's the builder's call. Either way, it's installed by a script like `scripts/mac/install-netd.sh`, and it can be cleanly uninstalled.
- **Restoring Mac devices.** Whatever the app changes in the Mac's default output and input while connected, it must restore it on disconnect, on quit, on crash (next launch at the latest), and if the app is uninstalled.
- **Transport.** Use the existing QUIC session (`crates/cj-net/src/session.rs`). Input datagrams keep strict priority over audio. Audio travels both ways at once: the Mac's sound to the PC, and the PC's mic to the Mac.
- **Wi-Fi.** The Mac is on Wi-Fi. Phase 1 already counters AWDL scanning spikes with the root helper `cursorjump-netd`. The audio design must absorb whatever jitter remains without audible glitches.
- **Protocol changes:** bump `PROTOCOL_VERSION` and reinstall both machines together.

## How it gets made (Claude's calls)

- Built into the existing Rust app on both machines, plus the Mac's audio device install. No separate audio app.
- **Plan after drag-and-drop is built.** Both features change the same connection code: stream limits, flow control, priorities and `PROTOCOL_VERSION`. Write this plan once drag-and-drop has landed, so audio fits the final priority order: input first, then audio, then file data. Audio must not stutter during a 5 GB file copy.
- Recommended targets for the builder to confirm or tighten:
  - Mac-to-PC sound adds about 60 ms or less of delay. The Mac audio device reports its true latency to macOS, so video players compensate and lip sync stays right.
  - PC mic to a Mac app takes about 60 ms or less.
- The two machines' audio clocks run at very slightly different speeds. Over hours, that must not build up delay or cause periodic glitches.
- A **"Mac sound and mic through PC" on/off switch** in settings (decided with the user), mirrored in the Mac menu-bar menu *(Claude's call)*, and on by default. It's for the times the user wants the Mac's own speakers, AirPods or mic, for example after carrying the MacBook away from the desk.

## Left to the builder

- Codec (uncompressed PCM or high-bitrate Opus), packet size, jitter buffer, and loss handling.
- Virtual audio driver versus process taps on the Mac for capture, and the virtual input device design for the mic.
- How "a Mac app is listening to the mic" is detected, to start and stop PC capture.
- Sample rate handling and clock-drift correction.
- Test strategy. Acceptance should include:
  - YouTube on the Mac with lip sync checked by eye;
  - music on the Mac while a PC game is running;
  - a Discord call on the Mac using the PC's mic;
  - switching the PC's default output mid-song;
  - a 2-second Wi-Fi blip with no fallback;
  - a longer Wi-Fi drop and the Mac's devices coming back;
  - the settings switch both ways;
  - a 5 GB file copy running during music, with no stutter;
  - a reboot of each machine;
  - an hour of playback with no drift;
  - cursor latency unchanged with audio running.
