# SDD ledger — plan: docs/superpowers/plans/2026-11-15-audio-sharing.md

Spec: docs/superpowers/specs/2026-10-25-audio-sharing-design.md (reachable). Branch `audio` from phase2 1445742; plan commit eec49e7.
This PC session runs PC Tasks 2–9 in order 2,3,4,5,6,7,8,9. Tasks 1, 10–14 are [Mac]; 15 is with the user on both.

## Pre-flight scan
| Pair / task | Produces vs consumes | Finding |
|---|---|---|
| T2→T3 | AudioStream/AudioStatus/AudioWant (serde) → Datagram::Audio, Control variants | consistent |
| T2→T4 | AudioStatus/DeviceState/AudioWant → MacAudio rules | consistent |
| T2,T3→T6 | Packetizer, encode/decode_pcm, AudioReceiver, send_audio → audio_pipe | consistent |
| T6→T7 | AudioRx::pull/reset → MacSoundOut | consistent |
| T6→T8 | AudioOut::sender → PcMicIn | consistent |
| T7,T8→T9 | MacSoundOut start/stop/reopen, PcMicIn start/reopen, DeviceWatch, pc_status → PcAudio | consistent |
| T4,T14 | both touch ui/settings.rs (T4 merge_edits list only; T14 checkbox) | consistent, sequential |
| T9 | Live::start gains AudioTx; T6 tests use their own reader | consistent |
| T10→T11,T13 | UIDs com.cursorjump.audio.mic/.feed | consistent |
| T11→T12,T14 | AudioWatch DefaultOutputChanged → SoundTap::rebuild via cj-audio thread | consistent |
| T12→T14 | TapError::Denied → Capture::Denied | consistent |
| T1→T12 | S1–S4 findings may amend T12 | by design |
| T2 self | pcm_clamps test expects 2.0→32767/32768 and -2.0→-1.0 | implies scale 32768 → Ruling P1 |
| T2 self | PI gains fixed (2e-6, 1e-8) vs drift test must pass | gains may not converge → Ruling P2 |
| T3 self | backlog test "1..=5 true" with 4 KiB cap and ~970 B datagrams | 5 expected; consistent |
| T5 self | consistent |
| T6 self | consistent |
| T7/T8 self | hands-on #[ignore] tests need the user's ears | → Ruling P3 |
| T9 self | `with_factories` test seam named but unspecified | implementer's design; ok |
| T13 self | only test is #[ignore] hands-on (hardware) | acceptable for device code; no plan-mandated defect |

Ruling P1: encode_pcm = round(clamp(x,-1,1)·32768) saturated to i16; decode = v/32768 — the only scale that satisfies pcm_clamps_out_of_range and the roundtrip test — cost if wrong: a 1-LSB level change.
Ruling P2: the PI gains are starting values; the implementer may tune them (and the controller's update rate) if the drift test needs it, recording the final values in the report — spec pins no gains — cost if wrong: re-tuning later.
Ruling P3: Tasks 7/8 implementers write and compile the #[ignore] hands-on tests but do not run them; the controller runs all PC hands-on tests with the user in one sitting after Task 9 — continuous execution can't wait on ears — cost if wrong: a late-found device bug reopens 7/8.
Task 2: dispatched (BASE eec49e7, implementer opus)
Task 2: implementer DONE_WITH_CONCERNS (1675250). Gains tuned per P2: Kp 2e-4, Ki 5e-8. rubato 5.1.0 (no default features); root Cargo.toml gets [profile.dev.package.rubato] opt-level=3 for test speed. PcMic fill error up to ~13 ms (no skips/underruns).
Ruling P4: capture sides (Task 8 PC mic, Task 12 Mac tap) must keep sending packets through silence (WASAPI SILENT-flagged buffers → zeros; tap IOProc runs continuously) — the jitter buffer treats a sender pause as an underrun and raises its target — cost if wrong: latency creeps to 80 ms after quiet spells.
Task 2: review 1 — Needs fixes. Important: (I1) KP 2e-4 turns per-packet sawtooth/jitter into ±1000 ppm pitch modulation; (I2) pull frees packet Vecs on the realtime thread.
Task 2: minor (deferred): reset() rebuilds sinc tables (use in-place resets); drift test covers MacSound only (PcMic ~13 ms); AudioStats lives in receiver.rs but jitter.rs builds it (layering); root Cargo.toml rubato opt-level override outside brief (kept — Ruling: test speed, harmless); untested edges: non-whole-frame pull, zero-length pull, straggler after re-anchor; a skip stopping on a None slot counts as lost.
Ruling P5: AudioReceiver::push becomes push(&mut self, seq: u32, samples: &[f32]), copying into preallocated fixed packet slots, so neither push nor pull allocates or frees; Task 6 carries packets through its channel as fixed [f32; 480] arrays by value — the plan's Vec<f32> signature would free on the RT thread — cost if wrong: an interface tweak in Task 6.
Ruling P6: "new sequence" threshold ahead is ≥ 1 s of packets (200 MacSound / 100 PcMic) instead of 1000 packets, so the slot ring stays fixed at 1 s — anything a second ahead is a new session in practice — cost if wrong: a reset on an absurd reorder.
Task 2: fix round 1/5 (2 addressed, 0 open; commits 1675250..dd59e4c)
Task 2: complete (commits eec49e7..dd59e4c, review clean)
Task 3: dispatched (BASE dd59e4c, implementer sonnet)
Task 3: minor (deferred): backlog test bound 1..=5 has no margin (exactly 5 fit; widen to 1..=6 or derive from constants); send_audio doc silent on datagrams-unsupported.
Task 3: complete (commits dd59e4c..924a6e9, review clean)
Task 4: dispatched (BASE 924a6e9, implementer sonnet)
Ruling P7: subagent commits may carry their own model's Co-Authored-By trailer (04833be says Sonnet 5.5) — accurate attribution; not rewriting pushed history — cost: cosmetic.
Task 4: review 1 — Approved, with one plan-mandated Important: NOTE_NO_PC_OUTPUT shows on every connect before AudioStatus arrives (pc == None counts as Missing).
Ruling P8: while connected with no PC status yet, `note` is None (routing still treats both as not Ok) — the spec wants the menu to say why in plain words, and a false "PC has no sound output" flash contradicts that — cost if wrong: no note for the few ms before the status arrives.
Task 4: minor (deferred): connected() twice wipes pc (guard if already connected — folded into the fix round as cheap); set_pc/set_mic_listening accepted while disconnected (fine); test gaps (beyond-grace want/note, reconnect want.sound, exact t+5 connect, double disconnect, sharing toggled in grace, Capture::Unknown); config test literal newline style.
Task 4: fix round 1/5 (2 addressed, 0 open; commits 04833be..301bd73). Note: P8 also hides NOTE_CAPTURE_DENIED until the PC reports — accepted (status follows the connect within ms).
Task 4: complete (commits 924a6e9..301bd73, review clean)
Task 5: dispatched (BASE 301bd73, implementer haiku)
Task 5: minor (deferred): a volume key held at the crossing seeds held/held_at_entry and its up never clears them (benign, cleared on exit); could filter is_volume_key in keys_down_now/seed_entry_keys.
Task 5: complete (commits 301bd73..580a1fd, review clean)
Task 6: dispatched (BASE 580a1fd, implementer sonnet)
Task 6: minor (deferred): #[path] include re-runs the module's tests (incl. 4 s sine) in the no-alloc binary; no_session_drops_quietly asserts nothing beyond no-panic; AudioTx ignores the Datagram's stream field (callers must route by stream — Task 9 / Task 14); debug log per queue-full drop; common/mod.rs expect(dead_code) for the pipe must go once devices use it.
Task 6: complete (commits 580a1fd..7776298, review clean)
Task 7: dispatched (BASE 7776298, implementer opus)
Ruling P9: MacSoundOut::reopen() reopens only when the default render endpoint id changed (always on AUDCLNT_E_DEVICE_INVALIDATED) — unrelated device notifications must not cause audible gaps — cost if wrong: a format change on the same endpoint would need invalidation to trigger.
Task 7: minor (deferred, FINAL-REVIEW PRIORITY): render loop runs is_default() (CoCreateInstance+GetDefaultAudioEndpoint+GetId) before refilling — fill first, keep the enumerator in Output (render.rs:255-270).
Task 7: minor (deferred): every open failure logged once as "no output" at info — split real failures to warn; pc_status warns on every call if the enumerator fails; hands-on test Arc::try_unwrap race; wide spaces in hands-on println strings; weak asserts in pc_status/device_watch tests; HANDS-ON CHECK: whether the device-wide mic switch lives under HKLM ConsentStore (HKCU only checked).
Task 7: complete (commits 7776298..ccfb559, review clean)
Task 8: dispatched (BASE ccfb559, implementer sonnet)
Task 8: minor (deferred): from_raw_parts on possibly-null data when frames==0 (guard it); a failure mid-capture retries once then waits for reopen() (unlike render's retry-forever); no on_state if the thread fails to spawn.
Ruling P10: Task 9's PcAudio re-pokes the mic (PcMicIn::reopen) every 2 s while want.mic is on and the mic state isn't Ok, besides on device-change events — DeviceWatch doesn't see privacy un-blocks or format-change invalidations, so the mic could otherwise stay Missing/Blocked until an unrelated device change — cost: a failed Initialize every 2 s while blocked (no indicator).
Task 8: complete (commits ccfb559..5116cd2, review clean)
Task 9: dispatched (BASE 5116cd2, implementer opus)
Task 9: implementer DONE w/ concerns (fb8abca): audio_pipe dead-code expect narrowed to non-Windows (Task 14 must drop it); 2 s tick also re-reads pc_status() while the mic isn't Ok (privacy switch fires no event) — accepted as part of P10; capture override also cleared by device change while no mic runs — accepted.
Task 9: review 1 — Needs fixes. Important: (I1) a capture-thread failure report sticks for the session: the Mac drops want.mic on non-Ok, so the P10 poke has no mic, the probe is unchanged, and the override never clears (audio.rs:266-270; test at :550 asserts it).
Task 9: minor (deferred, FINAL-REVIEW PRIORITY): MacSoundOut::stop / PcMicIn drop join device threads on the async loop that also drives Motion (up to ~100 ms+, worse on BT profile switches) — move joins off-loop.
Task 9: minor (deferred): pc_status() does COM on the loop every 2 s on a mic-less PC and per AudioDevices burst (tick needs only the registry; debounce device events); Ev::MicState has no generation (one wrong status round trip possible); no test of AudioOut set(Some)/set(None) via connect(&Session); redundant rx.reset before start.
Task 9: fix round 1/5 (1 addressed, 0 open; commits fb8abca..c044765). Out-of-scope: a persistently failing mic open cycles ask/fail every 2 s — Mac-side backoff could be added (Task 14 / final review).
Task 9: complete (commits 5116cd2..c044765, review clean)
PC tasks 2–9 complete. Next: P3 hands-on (Task 7 render x2, Task 8 capture) with the user; then [Mac] Tasks 1, 10–14 on the MacBook.
CI: audio_datagrams_cross (Task 3 test) is flaky on both OSes — 10 back-to-back send_audio calls overrun the 4 KiB backlog before the driver drains; CI red since 04833be. Routed to the Task 3 implementer as a post-completion fix (BASE c044765).
Flaky-test fix 132cf9e: re-review clean.
Hands-on (PC, 2026-11-15 22:55Z, user): render_plays_a_tone_through_the_default_output PASS (396 packets, 0 lost; 1 underrun at end of feed, expected). render_follows_a_default_device_change PASS at 22:57Z: user has a single output, so tested by unplugging/replugging the wired headphones — reopened in ~50 ms each time, 0 underruns, 6 notifications. capture_records_the_default_mic: first run FAILED (talk/silent RMS ratio 2.8×, talking −51 dBFS; user had missed the prompts) — rerun pending.
Hands-on mic test: windows were too short for a person (1.5 s) — test reworked in bf5a1b6 (3-2-1 countdowns, 3 s silent, 4 s talk, dBFS output). Rerun 23:01Z PASS: silent −59.3 dBFS, talking −34.9 dBFS, 24.4 dB apart, on the user's main mic "Microphone (USB PnP Audio Device)" (Windows default). All PC hands-on tests passed.
Note: bf5a1b6 (hands-on mic test windows) had no scoped re-review — test-only, #[ignore], validated by the user's passing run; final review to glance at it.
Mac session up (2026-11-15 23:10Z, macOS 26.5.1, arm64): `audio` at 4d75b3f pulled into ~/Projects/cursorjump; cj-core + cj-net tests pass (211+36), workspace clippy -D warnings clean on the Mac. objc2-core-audio 0.3.2 binds CATapDescription and AudioHardwareCreateProcessTap (no extern_class! needed). Starting Task 1 (spike) with the user.
Task 1 (Mac spike, with the user, 2026-11-16 00:10–00:25Z): S1 holds on the built-in speakers (YouTube muted; user confirmed silent then back); the output-switch half is untested, because there was no second output (Task 12 hands-on covers it). S2 holds (tap is pre-volume: the same peak at 100/50/0 % and muted). S3 half holds: denial gives no error, only exact zeros, and the plan's IsRunningSomewhere test can't separate that from silence (our aggregate keeps the output running). S4 holds (48 kHz float32 interleaved stereo; 512 frames by default, 256 and 128 settable). Also: tap auto-start withholds the IOProc during silence (F1); the first-time TCC prompt blocks AudioDeviceStart (6 s). Task 12 amended (T12-a..d: auto-start off, 256-frame buffer, Denied = 3 s of zeros while another process IsRunningOutput, tap only on cj-audio). Findings: docs/superpowers/progress/audio-spike.md.
Task 1: complete. Next: Task 10 (HAL plug-in).
Task 10: dispatched (BASE 8b043b9, implementer opus)
Task 10: implementer DONE w/ concerns (7995ede): ring_test 5/5 (also ASan/UBSan), driver builds --no-sign with -Werror, CFPlugIn harness loads it; Step 4 (install into coreaudiod, Audio MIDI Setup, QuickTime) not run — needs the user's sudo; the host may compute IsRunningSomewhere itself (Task 11 must check its listener fires); install exits 1 if the mic isn't listed within ~10 s.
Task 10: review 1 dispatched (opus).
Task 10: review 1 — Approved (no Critical/Important). Minors 1–2 (install: killall failure swallowed; system_profiler|grep -q pipefail false failure) fixed by the controller in 099cd22. Deferred minors: ring data[] formally racy under C11 (use relaxed __atomic on floats); stream IsActive set sends no PropertiesChanged; plist comment names CJ_FACTORY_UUID (macro is _STRING); -Werror vs CI SDK drift; dispatch_queue_create unchecked; no concurrent writer/reader ring test.
Task 10: Step 4 (install with the user) next.
Task 10 Step 4 (user, 2026-11-16 00:43–00:47Z): install.sh signed with CursorJump Dev from the user's Terminal; coreaudiod loaded it (S5 holds, no ad hoc fallback needed); system_profiler lists "CursorJump Microphone" (1 ch, 48 kHz, Virtual), feed hidden; QuickTime recorded from it: silent, no error; --uninstall then reinstall: loaded again.
Task 10: complete (commits 8b043b9..099cd22, review clean)
Task 11: dispatched (BASE a63cbe1, implementer opus)
Task 11: implementer DONE w/ concerns (1c9c5a1): 13 fake-device tests; mic_listening_follows_a_recording_client PASS 4/4 (coreaudiod does forward the driver's IsRunningSomewhere — Task 10's concern resolved); InputDevices gained builtin_input_uid() for the built-in fallback; engage_and_release_on_this_mac not run.
Pre-existing failure found on the Mac: tests/audio_pipe_no_alloc.rs (MacSound 1 heap call). Cause: macOS std Mutex boxes its pthread mutex on first lock, so AudioRx::publish's first try_lock allocated on the RT thread. Fixed by the controller in add47f6 (lock once in audio_pipe()); CI now runs that test on macOS too (it was compile-only there).
Task 11: review 1 — Approved (no Critical/Important). Fix round 1/5 dispatched for minors 1 (failed restore also tries the built-in mic, keeps the file), 3 (engage refuses to switch if the current default can't be read), 4 (store the mic ID only once its listener is added), 6 (hands-on test restores via a drop guard). Ruling P11: the warn on a failed mic-listener add stays (rare, otherwise silent) — cost: one possible log line on a callback path. Deferred: minor 2 (no built-in mic and saved device gone), 7 (crash file + engage/release without recover: test gap).
Task 11: fix round 1/5 (4 addressed, 0 open; commits 1c9c5a1..d1b99e5); re-review clean.
Task 11 hands-on (user, 2026-11-16): engage_and_release_on_this_mac PASS — BuiltInMicrophoneDevice → CursorJump Microphone for 3 s → back; the user saw both switches in System Settings › Sound › Input. core_audio_reads_this_macs_inputs PASS.
Task 11: complete (commits 2fd16e5..d1b99e5, review clean)
Task 12: dispatched (BASE d1b99e5, implementer opus)
Task 12: implementer DONE w/ concerns (627a023): 12 unit tests; hands-on the_tap_captures… ran: −75 dBFS mean at 19 % volume (gain 0.017), 1802 packets, Capture Allowed (user-hears-nothing not yet confirmed); output-switch test not run (no 2nd device). Concerns: an output device with inputs (headset) adds input buffers (tap assumed last, unverified); start takes an on_capture callback; T12-c can give a false Denied when an app keeps output running on silence.
Hands-on (user): a refused tap still mutes (F6 in audio-spike.md).
Ruling P12: T12-c's heuristic is replaced. At tap start (and rebuild), SoundTap self-probes: a second private, unmuted tap of only our own process (initStereoMixdownOfProcesses([me])) on its own aggregate, while our process plays ±1e-4 (−80 dBFS, inaudible) noise to the default output for up to 500 ms; any non-zero sample → Allowed, exact zeros for the whole window → Denied; probe torn down after. Capture is a process-wide TCC grant, so the probe answers for the global tap too — a false Denied drops the tap for the session (F6) — cost: about 500 ms and some extra code at start.
Task 12: review 1 dispatched (BASE 8ef6ad1).
Task 12 hands-on (user, 2026-11-16 00:28Z): the_tap_captures_and_the_speakers_stay_quiet PASS — 1802 packets over 10 s at 40 % volume (gain 0.068), buffer 256 frames held, Capture Allowed; the user confirmed the speakers stayed silent (one boop before the tap started).
Task 12: review 1 — Needs fixes. Important: (I1) the IOProc assumes the tap is the aggregate's last input buffer; output devices with inputs (AirPods, headsets) would add mic streams — risk of sending the Mac mic / forcing HFP; (I2) a sample-rate change on the same device goes unnoticed (stale resample ratio); (I3) a failed second open in rebuild leaves the muted tap alive with no reader (Mac silent).
Task 12: fix round 1/5 dispatched: I1 (IOProcStreamUsage: tap input only), I2 (NominalSampleRate listener; also M7), I3 (drop the tap when rebuild fails), P12 self-probe (start(out) -> Result<SoundTap, TapError> restored; Denied is synchronous, no global tap on Denied; on_capture/capture() removed), M2 (leak IOProc state if teardown fails), M3 (raw-pointer buffer slice), M5 (log a sender-thread panic). Deferred: M1, M4 (moot), M6.
Task 12: fix round 1/5 (7 addressed, 0 open; commits 627a023..fd7c92c); re-review clean. The probe said Allowed after 75 ms from Terminal. Gate kept open: the AirPods/headset hands-on must confirm the aggregate's stream order (device inputs, then the tap).
Correction to P12: rebuild does NOT re-probe (the TCC grant is process-wide); only start probes.
Task 12: fix round 2/5 dispatched for the re-review's new Minors: N1 (the probe window starts at the first noise callback; 15 s cap → Failed — otherwise every first launch is a false Denied, because the TCC prompt blocks AudioDeviceStart), N2 (no tap buffer ≠ listened), N3 (rate listener before the first read), N4 (realtime layout guard in both IOProcs: unexpected buffers → silence plus a counter; makes a wrong stream order fail safe).
Task 12: fix round 2/5 (4 addressed, 0 open; commits fd7c92c..6695a1e); re-review clean. Deferred Minors: the N3 clobber window (a rate change between read and store); the 'sent silence' warning wording. Hands-on gates left for acceptance: the AirPods/headset stream order (I1/N4 fail closed); a first-launch Allow with TCC reset, using the bundled app (Task 15 item 14).
Task 13: dispatched (BASE 6695a1e, implementer opus)
Task 12 hands-on rerun on 6695a1e (user, 2026-11-16 00:51Z): probe Allowed in 68 ms; the_tap_captures_and_the_speakers_stay_quiet PASS (1802 packets/10 s, buffer 256); the user confirmed silent.
Task 12: complete (commits 8ef6ad1..6695a1e, review clean)
Task 13: implementer DONE w/ concerns (d1e9749): hands-on tone test PASS 4/4 (999.001 Hz, −0.000 dB, 0 lost/underruns); the pitch sits at the receiver's −1000 ppm clamp while it converges (accepted: 1.7 cents, inaudible).
Task 13: review 1 — Needs fixes. Important: stop/Drop freed and returned the Box<AudioRx> even when AudioDeviceStop and DestroyIOProcID both failed (possible use-after-free / aliased &mut).
Ruling P13: MicFeed::stop -> Option<AudioRx>; the rx comes back only if the IOProc never started, or Stop or Destroy returned 0; otherwise it is leaked with a warn, and Task 14 makes a fresh PcMic pipe and swaps its AudioTx — cost: Task 14's tx() must be swappable.
Task 13: fix round 1/5 dispatched (Important 1, Minors 1 and 3; deferred: Minor 2 pitch margin, Minor 4 unwind into the HAL).
Task 14: dispatched (BASE 8de7cad, implementer opus), in parallel with the Task 13 fix round (P13's stop -> Option<AudioRx> is fixed by ruling)
Task 13: fix round 1/5 (3 addressed, 0 open; commits d1e9749..dbf0aac); re-review clean. Hands-on: 999.001 Hz, −0.000 dB, 0 underruns, 0 skips.
Task 13: complete (commits 61f4d2a..dbf0aac, review clean)
Task 14: implementer DONE w/ concerns (0fa9abb): 361+6 tests, clippy clean, aarch64 release build OK; tx() returns MicTx (a swappable AudioTx); mac/app.rs at 1250 lines (audio glue in mac/app/audio.rs).
Ruling P14: the mic route engages only once MicFeed has started, and releases if the feed fails or stops — a default input that is our mic with no feed is silent (implementer concern 3) — cost: none.
Ruling P15: MacAudio::set_sharing(true) after false resets capture to Unknown, so toggling the switch re-probes after the user grants capture; NOTE_CAPTURE_DENIED gains ', then turn Mac sound and mic through PC off and on' — without it a refusal sticks until restart (concern 4) — cost: a tiny cj-core change (Task 4 code).
Task 14: review 1 dispatched (BASE 259e72f).
Task 14: review 1 — Needs fixes. Important: Quit can strand the input on CursorJump Microphone — reconcile engages, then blocks cj-audio in SoundTap::start (≥500 ms probe, a TCC prompt, up to a 15 s stall, back-to-back retries); Quit's 1 s wait times out and the process exits engaged; switch-off and grace releases are also delayed.
Task 14: fix round 1/5 dispatched: (a) SoundTap::start on a helper thread that posts back into the worker select; a late result after Apply(off)/Quit is dropped; (b) the shutdown runs MicRoute::recover() if the Quit reply times out or the thread is dead; P14; P15; Minor 1 (Timeout vs Disconnected); Minor 2 (Want on every new session test). Deferred: Minor 3 (tray insert index), Minor 4 (file size).
Task 14: fix round 1/5 (6 addressed: I1, P14, P15, M1, M2; 0 open; commits 0fa9abb..63be3a9); re-review clean (MovedTap Send wrapper checked field by field: sound). Fix round 2/5 dispatched: a racy test (a_blocked_tap_start…) and the tap.rs doc on MovedTap's invariant — no behaviour change, so Step 4 (reinstall both machines) goes ahead on 63be3a9.
Task 14: fix round 2/5 (2 addressed, 0 open; commit cd3a321: test determinism and doc only; checked by the controller, 20/20 runs). Code complete; Step 4 (reinstall both, YouTube end-to-end) with the user.
Task 14 Step 4 (user, 2026-11-16 01:38Z): reinstalled both machines on protocol v4 (Mac 63be3a9+; PC from audio). First-run TCC prompt: the probe waited for it (28 s), then Allowed. YouTube on the Mac plays in the PC's headphones — the user: 'working flawlessly'. Cursor latency unchanged (median 3.1–3.4 ms, 0 spikes). PC mic stream: 4399 packets, 0 lost, 0 underruns, buffer 39 ms (target 30).
Finding: MicListening went true 60 ms after engage and stayed on. Recorders: corespeechd (com.apple.CoreSpeech, the Siri voice trigger follows the default input), the Sound settings meter, and CursorJump itself (the tap aggregate's input). So the PC mic stays open permanently — against the spec.
Ruling P16: MicListening = some process other than us and system background listeners (deny-list: CoreSpeech) is running input on CursorJump Microphone, computed from the process list (IsRunningInput + Devices in input scope); the device's IsRunningSomewhere stays as the trigger — cost: Siri can't hear through the PC mic; extra listeners per process. Dispatched to the Task 11 implementer.
P16: implemented (f90d026). Findings: on macOS 26.5.1 a process's IsRunningInput change never notifies, but its input-scope kAudioProcessPropertyDevices does → Ruling P16a: listen to both on each process (accepted). corespeechd's input devices list the built-in device, not our mic, so Siri wasn't the cause; most likely the open Sound settings pane's input meter kept the mic open (the controller had asked the user to open it). The deny-list stays as a guard. P16 review dispatched.
