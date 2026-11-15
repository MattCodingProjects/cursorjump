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
