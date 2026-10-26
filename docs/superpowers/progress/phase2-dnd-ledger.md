# SDD ledger — plan: docs/superpowers/plans/2026-10-25-file-drag-and-drop.md

Spec: docs/superpowers/specs/2026-10-25-file-drag-and-drop-design.md (approved by user 2026-10-25). Plan approved; user approved exception E1 ("go for it") and asked for thoroughness over cost (helper + reviewer per task).
Branch: phase2 (from main 7aba250). Durable copy of this ledger: docs/superpowers/progress/phase2-dnd-ledger.md (committed periodically; the Mac session seeds from it).

## Pre-flight scan

| Rows | Produces → Consumes | Finding |
|---|---|---|
| T2 ↔ T3 | T2 Enter/Leave `drag` fields, DragId → T3 events/actions | consistent |
| T2 ↔ T6 | FilesHeader{name,kind,total_bytes,entries}, FileEntry, MAX_PATH_DEPTH → FilesIn validation | consistent |
| T2/T3/T6 ↔ mac/*.rs call sites | Enter/Leave/MacEvent/spawn_clipboard_tasks call sites in mac code | mac code can't compile on the PC → R2 |
| T4 ↔ T7 | Pacer::delay_for/on_rtt → SharedPacer in transfer | consistent |
| T5 ↔ T9/T10/T14 | PillView/PillIcon/PillBoard::new(from,local) → renderer, hosts | consistent (PC board new(Mac,Pc), Mac new(Pc,Mac)) |
| T6 ↔ T7/T8 | FilesOut/FilesIn/open_files/NetEv::Files → engine, DndLink::on_files | consistent |
| T7 ↔ T8/T11/T15 | send_item/receive_item/Dest/FreeSpace → DndLink; Dest::Exact for Mac promises | consistent |
| T8 ↔ T11/T13/T17 | Source/DndEv/incoming_outcome → catcher sources, wiring | consistent |
| T11 ↔ T12 | DndCmd/WinDndEv variants listed in T11 for T12 | dead variants fail clippy -D warnings → R3 |
| T11 ↔ T13 | input_thread: T11 adds catcher zone/offer; T13 adds Park on EnterMac | consistent (both touch input_thread.rs, sequential) |
| T13 ↔ T17 | sweep_temp in common/dnd_link.rs (T13) reused by Mac | T13 file list omits common/dnd_link.rs → R4 |
| T13/T17 pill Start point | T13 starts pill at StartIncomingDrag with cursor pos; T17 at Ended{dropped} | spec says "screen where the drop happened"; start point is the edge, not the drop → R5 |
| T1 ↔ T11/T12 (A4–A6), T1b ↔ T14–T16 (A1–A3, A7) | spike verdicts → mechanisms | T1b can only run on the Mac → R1 |
| T9/T10/T11/T12 hands-on steps | "with the user" smoke runs / preview | subagents can't talk to the user → R6 |
| T1 (self) | example files need windows Ole/Com features that T11 lists | R7 |
| T2 (self) | variant index test: DragOffer=11 … DragFinished=15 matches 5 appended variants | ok |
| T3 (self) | tests name the exact action orders the rules state | ok |
| T4 (self) | 10×256 KiB at 20 MB/s = 131 ms | ok |
| T5 (self) | done +1 s hold + 300 ms fade = removed at +1.3 s | ok |
| T6 (self) | old recv_clipboard tests adapted; spawn_stream_tasks replaces spawn_clipboard_tasks | ok |
| T7 (self) | tests cover every rule; symlink test Windows variant may skip | ok |
| T8 (self) | incoming_outcome defined + tested here | ok |
| T9 (self) | 0.62×255≈158, 0.80×255=204 | ok |
| T10, T12, T14–T16 (self) | files list matches steps | ok |
| T13 (self) | step 2 test lives in common/dnd_link.rs | see R4 |
| T17 (self) | uses incoming_outcome + sweep_temp from common | ok |
| T18 (self) | deletes spike examples | ok |

Ruling R1: Task 1 runs as 1a (PC) now; 1b runs at the start of the Mac session, before Task 14. Tasks 2–10 don't depend on the spike; Tasks 11–12 wait for the 1a verdicts — why: 1b needs the Mac — cost if wrong: a Mac-side surprise may force a late change to the shared protocol (Task 2/8), redone then.
Ruling R2: Tasks that touch macOS call sites (2, 3, 6, and any other) must get the macOS build green before reporting DONE: try a local `cargo check --target aarch64-apple-darwin` with stub C compilers, else push and watch the CI macOS job (`gh run watch`) — why: mac code can't be built natively on the PC and CI is the only real check — cost if wrong: a red CI found later by the Mac session.
Ruling R3: Task 11 defines only the DndCmd/WinDndEv variants it uses; Task 12 adds StartIncoming/CancelIncoming/IncomingEnded/Fetch — why: unused variants fail `clippy -D warnings` — cost if wrong: none (names are fixed by the plan).
Ruling R4: Task 13's file list includes app/cursorjump/src/common/dnd_link.rs (for sweep_temp) — why: its step 2 test lives there — cost if wrong: none.
Ruling R5: The receiving side starts the pill at the first Fetch of the drag (the drop moment; Mac: first Fetch or Ended{dropped:true}, whichever comes first), using the cursor position at that moment for the monitor — why: spec says "bottom centre of the screen where the drop happened"; the plan's StartIncomingDrag point is the edge — cost if wrong: pill on the wrong monitor on a multi-monitor drop; easy to move.
Ruling R6: Hands-on steps (spike runs, pill preview, smoke tests) are left as #[ignore] tests/examples by implementers; the controller runs them with the user after the task review — why: subagents can't talk to the user — cost if wrong: a look/feel issue found after review means one extra fix round.
Ruling R7: The spike may add the windows features Task 11 lists (Ole, Com, Com_StructuredStorage, Storage_FileSystem, SystemServices, implement) to app/cursorjump/Cargo.toml; Task 11 keeps them — why: the spike needs them and they're the same list — cost if wrong: none.
Ruling R8: Models — implementers opus (most tasks need cross-file judgment; user asked for thoroughness over cost), reviewers opus, re-reviews sonnet for small fix diffs and opus for large ones, fix rounds 4–5 and the final review on fable — why: user said "burn as many tokens as possible" — cost if wrong: only spend.

## Tasks
Task 1a: dispatched implementer (opus), BASE 8b33f56
Task 1a: implementer BLOCKED — an automated safety check stopped it from writing the spike's input-hook (swallow the button) and SendInput (fake LEFTUP) parts; it did not work around it. Nothing was written.
Ruling R9: Exception E1 removed from the plan. A PC drag riding on the Mac now ends by letting the user's REAL left-button release pass through the existing hook to Windows over the full-screen catcher (accept → Drop, where virtual files are spooled); a forced return leaves the catcher in reject mode until the real release. No input is ever injected on the PC. Spike reworked to need no hooks and no SendInput (source starts DoDragDrop with no button held = A5's state; drops on the next raw-input left-up). Plan amended (Global Constraints, A5/A6, Task 1, Task 3 FinishSourceDrag removed, Task 11 Drop/Cancel/pass-through, Task 13, Task 18) — why: the safety block, and it's strictly safer for anti-cheat than E1 — cost if wrong: after a hotkey/disconnect return mid-drag, the drag stays visible on the PC (reject cursor) until the user lets go.
Task 1a: step 1 done (764c550, CI green); hands-on steps 2/5 with the user next
Task 1a: exp 1 done — Explorer gives CF_HDROP + FILECONTENTS (alpha-1 strip gets DragEnter; inside-Drop read fine); Discord desktop gives only a link (".url" virtual file + URL formats), user says "web link expected". Notes in spike-notes.md.
Ruling R10: A drag whose only file is an http(s) ".url" virtual file (Discord, web images inside links) is sent as the linked file: the sending side downloads the URL's target and offers it under its real name; if the download fails, the drag carries nothing and the pill says "Couldn't fetch the image" — why: spec goal 5 names drags out of Discord and web pages, and a .url shortcut is useless on the Mac — cost if wrong: one HTTP client dependency (Task 11/15 scope); easy to drop.
Task 1a: exp 2/3 PASS (park accept catches mid-screen release; park reject ends cleanly with "no" cursor). Exp 5: A5 as written FAILS — DoDragDrop started with no press on our own window gets no mouse input (no DragOver/feedback/drag image; non-fg stalls entirely; fg drops only on a click). With the left button pressed and held on the spike's OWN window before DoDragDrop: icon follows, 230 QCD/s, drop on real release, file landed. Desktop tolerated a 60 s CF_HDROP wait with no freeze.
Ruling R11: A5 replaced. While the cursor is on the Mac, the PC cursor is parked over the CursorJump catcher window (WS_EX_NOACTIVATE, topmost), and the hook lets the user's REAL left-button down and up pass through to Windows only when WindowFromPoint(park point) is that catcher window (else they are swallowed as today and a Mac->PC drag falls back to drop-on-release with no hover). So the catcher's thread holds the implicit capture; when a Mac drag comes back to the PC, that thread calls DoDragDrop with the button truly down — the normal OLE source state. Still no injection, still "swallow less", hooks still only while on the Mac; never while gaming (crossing is walled then) — why: spike exp 5d/5e; the user asked for the icon to follow ("the drag looks normal", spec goal 6) — cost if wrong: a Mac click reaches our own invisible window (harmless); if the WindowFromPoint guard is wrong, a stray click could reach a PC app — guarded and must be unit-tested. Affects Tasks 11–13 (DndCmd::Park places the catcher under the park point; hook pass-through rule; source DoDragDrop on the catcher thread). Plan amendment to follow after the spike.
Task 1: 1a complete (dnd-spike.md); 1b pending on the Mac before Task 14
Task 2: dispatched implementer (opus, impl-task2), BASE b493be5
Task 2: implementer DONE 382d035 (111/111 cj-core, mac clippy clean via stub CC)
Task 2: dispatched reviewer (opus, review-task2) on b493be5..382d035
Task 2: review APPROVED (spec ✅, no Critical/Important). Fix round 1 for the minors (exact .mp4 assertion, no-extension cut test, Normal-components assertion, stale mac/control.rs:183 doc) sent to impl-task2.
Ruling R12: Add the superscript device names COM¹ COM² COM³ LPT¹ LPT² LPT³ to sanitize_component's reserved list — why: Windows reserves them too, so a Mac file with such a name could not be created on the PC (spec goal 2 "lands exactly where it was dropped") — cost if wrong: none (an extra "_" on very rare names).
Task 2: fix round 1 DONE 46201bc (113/113)
Task 2: re-review (sonnet) all findings addressed
Task 2: complete (382d035, 46201bc)
Task 3: dispatched implementer (opus, impl-task3), BASE 46201bc
Task 3: implementer DONE 606bf9a (135/135). Concerns: MacLeave{Some(foreign)} while Outgoing; MacLeave{None} while Outgoing leaves stale riding; forced return while Incoming does nothing; Task 17 note — no-Accessibility bounce path should skip BeginDrag (carry to Task 17 dispatch).
Task 3: review APPROVED (spec ✅). Ordering ⚠️ answered: DragDropped/DragFinished and Leave are all Control messages on the single ordered control stream.
Ruling R13: MacLeave while Mac always homes an Outgoing drag first (SourceDragHome{x}, riding cleared), then starts a foreign incoming drag if any — why: keeps "Outgoing ⇒ where_ == Mac" and never leaves the catcher in riding mode (reviewer concern 2/1) — cost if wrong: a harmless extra SourceDragHome for an already-ended drag (Task 13 must make SourceDragHome of an ended drag only move the catcher to the edge strip).
Ruling R14: PcEvent::MacLeave gains `game: bool`; a Mac drag coming home while a game holds the PC mouse is refused (PcAction::RefuseIncomingDrag{drag} → DragCancel{Cancelled}), the cursor still returns — why: spec anti-goal "a drag can never carry the cursor across while a game holds the mouse", applied to Mac→PC — cost if wrong: a Mac drag released over a PC game does nothing (desired).
Task 17 notes (carry into dispatch): (a) no-Accessibility bounce path must feed the real drag, never None, and suppress BeginDrag/CancelDrag side effects for the whole bounce round trip with its own "no drag session" flag (bounce() uses feed, not feed_quietly, mac/control.rs:321-330); (b) Mac injector must count the PC-held left button as held from BeginDrag on, so PushOut sees buttons_held after a Mac-side Esc.
Task 13 note: SourceDragHome for an already-ended drag only moves the catcher back to the edge strip.
Task 3: fix round 1 sent to impl-task3 (R13, R14, Enter-overwrite cancel, forced-return-while-Incoming test).
Task 3: fix round 1 DONE 47f0261 (140/140)
Task 3: re-review (sonnet) all findings addressed
Task 13 note: MacLeave game field must be the real game probe (app.rs hardcodes false now)
Task 3: complete (606bf9a, 47f0261)
Task 4: dispatched implementer (opus, impl-task4), BASE 47f0261
Task 4: implementer DONE 2e6c333 (149 cj-core). Task 7 notes: idle on_rtt ramps a shared Pacer to 1 GB/s (100 MB burst) — one Pacer per transfer or feed RTT only while data is in flight; RTT signal only meaningful if pings share the congestion path with file streams.
Task 4: review NEEDS FIXES (Important: rate ramps while the sender isn't using it → up to 100 MB burst when demand returns).
Ruling R15: Pacer ramps only if delay_for actually held the sender back since the last RTT sample (`limited` flag) — why: spec anti-goal "input path keeps strict priority"; a quiet RTT while under-sending proves nothing (RFC 7661) — cost if wrong: slower ramp-up on a link with headroom.
Ruling R16: One quiet sample's ramp credit is capped at 2 × 100 ms (≤ +8 MB/s) — why: lost pongs under load must not earn a giant step — cost if wrong: slightly slower ramp after ping loss.
Ruling R17: The ramp clock resets on every high RTT sample (cut or not) — accepted reading of "since the last increase" — why: standard AIMD; prevents a quiet pong right after a cut refunding congested time — cost if wrong: ≤4 MB/s less ramp per cut.
Task 4: fix round 1 sent to impl-task4 (R15, R16, saturating cost, doc readings).
Task 4: fix round 1 DONE c9e30e0 (153 cj-core)
Ruling R10a: a failed link download (R10) is reported as FailReason::Unreadable{name} ("Copy failed — couldn't read “name” on the PC"), not a new reason — why: avoids a protocol/FailReason change; the text is accurate enough — cost if wrong: slightly less specific wording.
Task 4: re-review round 1 — R15/R16/docs fixed; new Minor: spike-clock test lost its uncut-spike check → round 2 sent. Parked: Instant-overflow None arm untested (unreachable in practice).
Task 4: fix round 2 DONE fc5ccb1 (154, mutation-checked)
Task 4: re-review round 2 all addressed
Task 4: complete (2e6c333, c9e30e0, fc5ccb1)
Task 5: dispatched implementer (opus, impl-task5), BASE fc5ccb1
Task 5: implementer DONE 2d3a996 (167 cj-core). Concerns: Cell anchor makes PillBoard !Sync; '60 min left' at 3599.5 s; unknown total has no bar/indeterminate flag; hosts must always call finish.
Task 5: review NEEDS FIXES (Important: set_held(false) clears every Finished anchor → already-faded pills replay ✓+fade on each game-lock release).
Ruling R18: PillBoard::set_held takes `now` (set_held(held, now)); hold prunes expired pills, release anchors surviving Finished pills at now; the Cell goes, views/next_frame are pure — why: reviewer Important 1 + spec "held back until the game no longer does" covers only pills that end while held — cost if wrong: none (no consumer yet; plan line 372 amended).
Host-task note (T10/T13/T14/T17): every exit path of an incoming drag, including disconnect/session loss, must call PillBoard::finish(Err(..)), or the pill stays forever. If the renderer wants an indeterminate bar for unknown totals, add an explicit PillView field in T9 (don't infer from fraction None).
Task 5: fix round 1 sent to impl-task5 (R18 + regression test, coalesced ETA samples, done.min(total) in sub, total==0 no bar, title_for(&[]) → "file").
Task 5: fix round 1 DONE 23ac600 (172 cj-core)
Task 5: re-review all findings addressed (Minors parked: mid-fade pill replays fully after a hold; ETA stays optimistic during a <5 s stall because the rate uses the last sample not now)
Task 5: complete (2d3a996, 23ac600)
Task 6: dispatched implementer (opus, impl-task6), BASE 7a77ff9
Task 6: implementer DONE 508fe04 (cj-net 14+29, session_tasks 3). Concerns: inline header read in acceptor can stall later streams (no timeout); header.kind vs entries unchecked; no sender-side stop-code helper. Added STOP_INVALID=3.
Task 6: review APPROVED (spec ✅, no Critical/Important). Fix round 1 sent: FilesIn Drop → STOP_CANCELLED, header.kind consistency check, stronger cancel/drop tests, cj-net `files::stop_code(&anyhow::Error) -> Option<u32>` helper (apps never depend on quinn), window-unchanged comment. Parked: inline header read has no timeout (delay not deadlock; add timeout(2s) if smoke runs show it).
Ruling R19: Connection/stream flow-control windows stay unchanged (48 MB conn, quinn default ~1.25 MB per stream) — why: ~125 MB/s per stream at 10 ms RTT is enough on a LAN, and more in-flight buffering hurts the no-stutter anti-goal — cost if wrong: lower throughput on a fast LAN; revisit if measured short in T18.
Task 6: fix round 1 DONE dbd4b38 (cj-net 14+34)
Task 6: re-review all findings addressed
Task 6: complete (508fe04, dbd4b38)
Ruling R20: receive_item never overwrites: if the final path already exists it cleans the .cjpart and fails with Unwritable; callers (dnd_link/OS glue) pick unique destinations (per-drag incoming dir on the PC; Finder's promise URL on the Mac) — why: spec "name collisions: follow what the receiving OS does", which the OS does at its own drop step, not in our engine — cost if wrong: a rare collision fails instead of renaming.
Ruling R21: Stream stop codes map: receiver sees Reset(STOP_CANCELLED) → Cancelled (the real reason arrives via control DragCancel/DragFinished, Task 8); sender sees Stopped(STOP_CANCELLED) → Cancelled, Stopped(STOP_INVALID) → logged at error + Cancelled; other stream errors → Disconnected — why: R19 helper + brief's mapping — cost if wrong: wording only.
Ruling R22: Received files are written by our own code, so on the Mac they carry no quarantine flag (treated like a local copy, per the spec's recommendation); files are flushed and closed before the rename, no fsync — why: spec "treat them like a local copy"; fsync on 5 GB is slow and crash-safety is beyond "no half-copied files" (temp+rename covers it) — cost if wrong: a power cut mid-rename could leave a truncated final file (very rare).
Task 7: dispatched implementer (opus, impl-task7), BASE dbd4b38
Task 7: implementer DONE fe867f5 (18 transfer tests). Concerns: sender Ok ≠ delivered (Task 8 must carry real outcome); sanitize collisions inside a folder fail the item; Windows paths > MAX_PATH fail (not longPathAware); protocol break → Disconnected.
Task 7: review NEEDS FIXES (Important: dropped/panicking receive future leaves .cjpart behind — spec anti-goal "half-copied files left behind").
Ruling R23: receive_item holds an armed PartGuard that removes the part on drop (std::fs, sync); the top-level part is FILE_ATTRIBUTE_HIDDEN on Windows; hard-kill leftovers are swept at startup by sweep_temp (Task 8/13) for CursorJump's own incoming dirs — why: anti-goal; Windows doesn't hide dot-files — cost if wrong: a leftover hidden part after a hard kill outside our dirs (Mac Exact promise dest).
Task 8 notes: (a) the sender's Ok means "sent", not "landed" — the real outcome must come back over control (DragFinished); (b) pick unique InDir destinations so R20 never fires late after a full transfer.
Polish backlog: uniquify sanitize collisions inside a received folder (today: fail-closed Unwritable). Task 14: run `cargo test -p cursorjump common::transfer -- --nocapture` and confirm no "skipped:" lines for the unix permission test.
Task 7: fix round 1 sent to impl-task7 (PartGuard + abort test, hidden part, verbatim Win32 paths + >260 test, Exact uses p, spawn_blocking free space, cancel during file I/O, no error-path flush, HRESULT facility check, cfg(macos), FNV-1a, warn on protocol break).
Task 7: fix round 1 DONE e23e020 (22 transfer tests); receive_item space is now &'static dyn FreeSpace (spawn_blocking)
Task 7: re-review round 1 — all 10 fixed; new: Important A (&'static dyn FreeSpace vs plan's Arc<dyn FreeSpace> in DndLink) and Minor B (macOS spawn_blocking rename races the drop guard for folders) → round 2 sent.
Task 8 notes: (c) serialize or refuse concurrent items with the same sanitized name into the same Dest (shared part name); (d) on disconnect cancel receives by token, never abort tasks/drop a JoinSet (keeps cleanup on the async path). Task 13 note: measure a 300-tiny-file drop for cursor smoothness (sync creates on the 2-worker link runtime).
Task 7: fix round 2 DONE 9d4b62a (Arc<dyn FreeSpace>; sync macOS rename; C skipped — APFS refuses non-UTF-8 names)
Task 7: re-review round 2 all addressed; controller verified disarm follows rename Ok with no await between (transfer.rs:537-538 → 362-365)
Task 7: complete (fe867f5, e23e020, 9d4b62a)
Task 8: dispatched implementer (opus, impl-task8), BASE 9d4b62a
Task 8: implementer DONE 42167e2 (dnd_link 11/11). Concerns: REASON_WAIT 2 s for origin's DragCancel after a reset; dup-dest check not case-folded; out-of-range FileFetch warned+ignored.
Task 8: review NEEDS FIXES (Important: case-variant destinations share one .cjpart → possible corrupt file reported Ok; Pending source resolving to Err(Cancelled|Disconnected) strands the drag; progress cap broken by forced item-end reports). REASON_WAIT 2 s accepted (in practice one control-message latency).
Ruling R24: Destination clash detection folds case (Unicode lowercase + NFC) on every platform — why: NTFS/APFS are case-insensitive and part names collide — cost if wrong: a rare second fetch of a case-variant name fails Unwritable.
Wiring notes (T11–T17): DndLink's offer/fetch/finish/cancel are not cancel-safe (partial control frame if dropped mid-write) — never put them in a select! that can drop them; Tally counts every offered item, so totals stay short / None (Dir) if items are never fetched — the PillBoard wiring must tolerate it.
Task 8: fix round 1 sent to impl-task8.
Task 8: fix round 1 DONE_WITH_CONCERNS 3688ca6 (dnd_link 15/15).
Ruling R25: Progress cap is strict (throttle + trailing flush at last+100 ms, no force at item end) instead of "force when nothing in flight" — why: Windows reads FILECONTENTS serially, so the force rule still gave ~N events; strict cap meets note 6 — cost if wrong: the final Progress can trail its Received by ≤100 ms (PillBoard ignores progress after finish).
Parked: case folding is to_lowercase, not full Unicode case folding (ß vs SS not caught).
Task 8: fix round 2 DONE 36ea0e0 (16/16) — controller reviewed the small diff directly: held progress emitted in end_incoming after removal (flush then finds nothing), test fails without it. OK.
Task 8: complete (42167e2, 3688ca6, 36ea0e0)
Ruling R26: PillView gains `running: bool` (cj-core pill.rs, set while the copy is in progress); the renderer draws the indeterminate track only when running && fraction is None — why: Task 9's "indeterminate thin track if None while running" can't be derived (a Cancelled pill also has fraction None, check 0) — cost if wrong: none (one field).
Task 9: dispatched implementer (opus, impl-task9), BASE 36ea0e0
Task 9: implementer DONE c081c6a (pill_render 7 + ignored preview; ~93/140 µs). Concerns: shadow softened to fit SHADOW_PAD 12; layout slightly tighter; vector ✓/✕/icons; no shaping (emoji/CJK → .notdef); ✕ replaces ✓ on hover of a done pill. Preview viewed by controller: matches the mockup closely.
Task 9: review NEEDS FIXES (Important: hover ✕ drawn on done/cancelled pills, where it has no action and hides the ✓). Fix round 1 sent (close_shown predicate, glyph fallback + NFC, NaN-safe sheen, R26 render test, polish).
Ruling R27: SHADOW_PAD stays 12 with the eased shadow — why: layered/transparent windows hit-test every alpha>0 pixel; a 30 px halo would swallow clicks and cover neighbouring pills' ✕ — cost if wrong: a slightly softer shadow than the CSS mockup.
Task 10 notes: alpha>0 pixels of the pixmap hit-test; neighbouring pill windows overlap by 16 px at pad 12 (upper pill's pad covers 4 px of the lower body) — z-order pills or hit-test only the body rect; use pill_render::close_shown for clicks.
Task 9: fix round 1 DONE 950f235 (pill_render 10 + 1 ignored)
Task 9: re-review all addressed
Task 9: complete (c081c6a, 950f235) — preview shown to user; any look adjustments come as a later change
Task 10: dispatched implementer (opus, impl-task10), BASE 950f235
