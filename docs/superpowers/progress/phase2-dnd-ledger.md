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
