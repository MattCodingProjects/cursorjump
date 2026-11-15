# Audio spike findings (Task 1, Mac)

Date: 2026-11-16, on the MacBook (macOS 26.5.1, arm64, built-in speakers only: no AirPods or headphones at hand). Runs used `app/cursorjump/examples/audio_spike_mac.rs`. The user listened and answered the permission prompt. Runs were started from the Claude Code session, which runs under Terminal, so TCC charged them to Terminal: "System Audio Recording Only" on, Microphone allowed (tccd `authValue=2` for both). The S3 runs used a throwaway, ad hoc-signed wrapper app ("CursorJump Audio Spike", `com.cursorjump.audiospike`, with `NSAudioCaptureUsageDescription`), launched with `open` so that it was its own responsible process. `objc2-core-audio` 0.3.2 binds `CATapDescription` and `AudioHardwareCreateProcessTap`, so no `extern_class!` was needed.

## Verdicts

| Id | Assumption | Verdict |
|---|---|---|
| S1 | A global muted tap (excluding our process) silences the speakers and AirPods, and keeps capturing across a default-output change if the aggregate is rebuilt | **Holds for the built-in speakers.** A 30 s run during a YouTube video in Safari captured −28 to −40 dBFS the whole time, about 94 callbacks/s, with none lost. The user confirmed the speakers went silent and came back when the tap was destroyed. coreaudiod logged every other process as `muted by CursorJump spike`. **The output-switch half is untested** (no second output device). Task 12's hands-on `the_tap_stays_muted_across_an_output_switch` covers it, with AirPods or wired headphones. |
| S2 | The tap delivers audio before the output device's volume | **Holds.** The same looped sound peaked at exactly 0.359 at volume 100 %, 50 % and 0 %, and while muted. `mix_gain` is needed, as planned. |
| S3 | When capture is refused, the app can tell: either creating the tap fails, or the stream reads as exact zeros while the default output `IsRunningSomewhere` | **Half holds.** The tap, the aggregate and `AudioDeviceStart` all succeed, and the IOProc runs at full rate delivering **exact zeros**. There is no error anywhere. **But the plan's test can't tell this apart from silence:** our own aggregate keeps the default output running (`IsRunningSomewhere` = 1 throughout), and an allowed tap with nothing playing also gives exact zeros. See amendment T12-c. |
| S4 | The IOProc runs at 512 frames or less; the tap reports its rate, 48 kHz on built-in speakers | **Holds.** Tap format: 48 000 Hz, 2 ch, float32, interleaved (flags 0x9), 8 bytes/frame. The aggregate's default is 512 frames (10.7 ms). Requests for 256 and 128 frames both took effect (callbacks of 256 and 128 frames at 188 and 375/s). The AirPods rate is unchecked. |

## Other findings

- **F1. Tap auto-start withholds the IOProc during silence.** With `kAudioAggregateDeviceTapAutoStartKey = true` (as the plan specifies), `AudioDeviceStart` returns noErr in about 25 µs, but the IOProc never runs, and the device never reports running, until some process plays sound. It runs only while sound plays. With it `false`, the IOProc runs continuously, giving exact zeros in silence. Ruling P4 needs packets through silence, so Task 12 sets it `false` (T12-a). This cost most of the session: the first no-callback runs looked like a TCC problem.
- **F2. The first refusal blocks `AudioDeviceStart`.** While the TCC prompt is on screen, `AudioDeviceStart` blocked for **6.1 s** (until the user clicked Don't Allow). Later runs with the refusal remembered got no prompt: the start returned in about 8 ms, and the stream was exact zeros. So Task 12 must never start the tap on the app's async loop (T12-d).
- **F3. Resetting the grant needs a registered bundle.** `tccutil reset AudioCapture com.cursorjump.audiospike` failed with -10814 for the ad hoc app in a temp directory, even after `lsregister -f`. So the deny-then-allow transition was not seen in one run. The allowed path itself is proven by the Terminal runs. Use the real signed bundle in `/Applications` for that check (Task 14 / Task 15 item 14).
- **F4. The tap UID** comes from `kAudioTapPropertyUID`, and equals the description's UUID. The private aggregate (main sub-device = default output UID, one sub-tap with drift compensation) came up alive at once, with one input stream and one output stream.
- **F5. Tooling notes.**
  - In zsh, `log` is a builtin; use `/usr/bin/log show …` for Core Audio and TCC logs.
  - `codesign` with the "CursorJump Dev" identity fails with `errSecInternalComponent` from the Claude Code shell (no keychain access, even unsandboxed). Ad hoc (`-`) works.
  - The successful runs were made with the Claude Code sandbox off. The sandboxed runs all had auto-start on (F1), so whether the sandbox blocks the IO is unknown. Run hands-on audio tests unsandboxed.

## Plan amendments (Task 12)

- **T12-a:** the aggregate is built with `kAudioAggregateDeviceTapAutoStartKey = false` (F1).
- **T12-b:** ask for `kAudioDevicePropertyBufferFrameSize = 256` on the aggregate (5.3 ms, about one 5 ms packet per callback). Log the value read back. 512 also works, at about 5 ms more latency (S4).
- **T12-c (replaces the S3 detection):** `TapError::Denied` can't come from an error code. Detect it as: the tap has delivered only exact zeros for 3 s **while some other process is playing**, i.e. any process object in `kAudioHardwarePropertyProcessObjectList`, other than our own, has `kAudioProcessPropertyIsRunningOutput` = 1. Then report `Capture(Denied)`. Report `Capture(Allowed)` on the first non-zero sample. Until then, report `Unknown`. The output device's `IsRunningSomewhere` must not be used, because our own aggregate keeps it at 1.
- **T12-d:** create and start the tap on the `cj-audio` thread only. `AudioDeviceStart` can block for as long as the user leaves the TCC prompt open (F2).
- Everything else in Task 12 stands: the global stereo tap excluding our process, muted and private; the private aggregate on the default output; `mix_gain` (S2); and rebuilding on `DefaultOutputChanged` (to be proven by the hands-on test).
