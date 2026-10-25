# CursorJump File Drag-and-Drop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drag files, several files or a folder across the screen edge in either direction (PC to Mac and Mac to PC), and drop them straight into the target on the other machine: WhatsApp, Gmail, Discord, a folder, Downloads or the desktop. Big copies show a dark glass progress pill.

**Architecture:**
- **Catchers.** The machine where the drag starts (the *origin*) holds a tiny invisible "catcher" window at the screen edge that leads to the other machine. When a file drag reaches it, the catcher reads what is being dragged. When the cursor crosses, the origin sends a `DragOffer` (names, kinds, sizes), and the crossing message carries the drag's id.
- **Synthetic drag on the receiving side.** The *receiver* starts its own OS drag from a CursorJump window under the cursor: an `NSDraggingSession` on the Mac, or `DoDragDrop` on the PC. The user's continued motion moves it, and the user's release drops it on the real target.
- **Bytes on demand.** When the target asks for the file, the receiver sends `FileFetch`, and the origin streams the item on a low-priority QUIC stream, paced so the cursor never feels it. The receiver writes to a temporary name and renames it only on success, and shows the pill.

**Tech Stack:**
- Existing: Rust (edition 2024), tokio, quinn 0.11, postcard, `windows` 0.62, objc2 / objc2-app-kit.
- New:
  - `tokio-util` (`CancellationToken`)
  - `unicode-normalization`
  - `tiny-skia` and `ab_glyph`, for the pill renderer
  - the bundled Inter font (OFL)
  - `dispatch2`, for Mac main-thread work
  - extra `windows` features (Ole, Com, Com_StructuredStorage, Storage_FileSystem, plus the `implement` macro)
  - extra objc2-app-kit / objc2-foundation features (drag, panel, file promise, visual effect, workspace and URL classes)

  Each task adds what it uses. Versions are pinned by `Cargo.lock`.

**Spec:** `docs/superpowers/specs/2026-10-25-file-drag-and-drop-design.md`. It builds on `docs/superpowers/specs/2026-10-18-cursorjump-design.md`, whose rules all still hold.

**Execution notes:**
- **SDD workspace.** The SDD contracts in `docs/superpowers/progress/contracts/` apply. Use a new ledger `docs/superpowers/progress/phase2-dnd-ledger.md`, written by both machines: pull before writing, and append only.
- **Machines.** Tasks marked **[Mac]** run in Claude Code on the MacBook. Everything else runs on the PC. Windows code type-checks on the Mac with `cargo clippy --target x86_64-pc-windows-msvc` (see `docs/HANDOFF.md`). CI runs both for real.
- **Live testing.** Never run the real app from a test or smoke run while the other machine's app is live.

## Global Constraints

- **Anti-cheat (phase 1, binding):** no global hooks while the cursor is on the PC; no DLL injection, overlays or reading other processes' memory; and no `SendInput`, with exactly one exception, **E1** below. The gaming wall still blocks every crossing while a game holds the mouse (clip, hidden cursor, D3D exclusive full screen).
- **Exception E1 (spec: "write the exception down explicitly in the plan, so the user can see it"):** the PC may call `SendInput` with one `MOUSEEVENTF_LEFTUP`, only to end a file drag that started on the PC and is now riding on the Mac. The cursor is then parked over CursorJump's own full-screen catcher, the hooks are installed, and the gaming rule is not "wall". Nothing else is ever injected on the PC. Every use is logged at `info`.
- **CursorJump's own windows on the PC (spec):** "The phase 1 'no overlays' rule means injected in-game overlays." CursorJump may show ordinary top-level windows for the catcher and the pill. "Neither may appear while the gaming rule says 'wall'." A pill for a Mac-to-PC copy that finishes or fails while a game holds the mouse "is held back until the game no longer does".
- **Transfer (spec):**
  - "Any size. Files go directly over the existing encrypted LAN connection, with no cloud and no size cap."
  - "File data goes over streams, never over the input path, and must not delay input datagrams."
  - "Any cursor lag or stutter during a drag or during a transfer" is an anti-goal.
- **Waiting (spec):** for targets that only accept a finished file, "the drop waits until the file has fully arrived, at any size". There's "no size limit above which files go to Downloads instead".
- **Named targets (spec):**
  - the WhatsApp desktop app;
  - Gmail in each machine's default browser;
  - Discord (assumed to be the desktop app);
  - folders, Downloads and the desktop, on both machines.

  Drags *out of* WhatsApp, Discord and web pages must work too.
- **Clean failure (spec):** "no half-copied file is left behind at the destination". If there isn't room, "fail cleanly before or as soon as that's known".
- **No stuck drags (spec):** no cursor or mouse button stays "held" on either machine after a drag ends, is cancelled or fails.
- **Protocol:** `PROTOCOL_VERSION` becomes 3. Reinstall both machines together.
- **The pill (spec, exact):**
  - **Where:** "on the machine receiving the file, floating at the bottom centre of the screen where the drop happened". It never takes focus.
  - **Glass:** "smoky dark glass … about 62 % opaque near-black, blurred background, a 1 px faint white border and a soft shadow", with white text, monochrome.
  - **Layout:** icon square (or count) · name, semibold, ellipsized · percentage on the right. Under that, a 3 px white bar on a faint track with a slow sheen. Under the bar, dimmed: "From PC · 2.1 of 5.0 GB · 1 min left" (or "From Mac").
  - **Several files:** one pill per drag ("3 files"). Concurrent drags stack vertically.
  - **Done:** a ✓ pops in, then the pill fades after about a second.
  - **Failed:** the pill stays until dismissed and says what happened in plain words.
  - **Tiny files:** no pill if the copy finishes almost instantly.
  - **Cancel:** a ✕ appears on hover, and cancelling cleans up like a failure.
  - **Platforms:** the same design on Windows and macOS.
- **Out of scope (spec):** dragging text or links, and copy/paste of files. The transfer engine must not rule copy/paste out later.

## Review Focus

1. **Changing your mind mid-drag.** The user drags back across the edge with the button still held, or presses Esc. Expected: nothing is dropped anywhere unintended, and no button stays held.
   - A PC-origin drag that comes back simply continues as a normal drag on the PC.
   - A Mac-origin drag that is on the PC can't be pushed back: the edge is a wall while it's active.

   Pinned by `outgoing_drag_coming_home_continues_on_pc` and `incoming_drag_makes_the_edge_a_wall` (Task 3), and `incoming_drag_push_out_cancels_before_leaving` (Task 3).
2. **Connection lost or the Mac sleeping mid-drag or mid-copy.** Expected: the cursor returns, OS drags are cancelled rather than dropped, partial files are removed, and the pill says why. Pinned by `disconnect_during_outgoing_drag_cancels_before_return` (Task 3), `receive_aborted_mid_file_leaves_nothing` (Task 7) and `disconnect_fails_pending_fetches` (Task 8).
3. **Names that are illegal or odd on the other OS:** `a:b.txt`, `CON.txt`, `trailing.`, a decomposed (NFD) `é`, a 300-character name, `..`. Expected: they arrive under a safe name and never land outside the destination. Pinned by `sanitize_component_cases` and `safe_rel_path_rejects_escape` (Task 2), and `hostile_entry_path_is_refused` (Task 6).
4. **Big or odd folders:** thousands of small files, empty subfolders, zero-byte files, symlinks, a file that grows while copying. Expected: everything arrives and empty folders are kept, symlinks are skipped and logged, and a changed file fails cleanly. Pinned by `folder_tree_roundtrip_keeps_empty_dirs_and_zero_byte_files`, `symlinks_are_skipped` and `file_that_changes_size_fails_with_changed` (Task 7).
5. **Destination problems:** not enough space, or a read-only or protected folder. Expected: the copy fails before writing (space) or cleanly (permissions), with a plain message. Pinned by `insufficient_space_fails_before_writing` and `unwritable_destination_fails_cleanly` (Task 7), and `failed_pill_texts` (Task 5).

---

## File Structure

```
crates/cj-core/src/
  dnd.rs          DragId, DragItem, ItemKind, FilesHeader, FileEntry, FailReason, DragOutcome, name sanitizing   (new)
  pacer.rs        Pacer: RTT-driven send-rate limiter for file data                                               (new)
  pill.rs         PillBoard: pill lifecycle, text, animation parameters (pure, clock-injected)                    (new)
  proto.rs        v3: drag fields on Enter/Leave, five new Control variants
  pc_state.rs     drag rules (crossing with a file drag, coming home, cancel on force return)
  mac_state.rs    drag rules (begin, cancel, push out with a drag)
crates/cj-net/src/
  files.rs        FilesOut / FilesIn: framing of a file item on one uni stream                                    (new)
  session.rs      stream tags, accept_stream, open_files, priorities, limits
app/cursorjump/
  assets/fonts/   Inter-Regular.ttf, Inter-SemiBold.ttf, OFL.txt                                                  (new)
  examples/       dnd_spike_win.rs, dnd_spike_mac.rs (throwaway, deleted in Task 18)                              (new)
  src/common/
    transfer.rs   send_item / receive_item engine, temp-then-rename, space check, pacing                          (new)
    dnd_link.rs   per-session drag coordination: offers, fetches, outcomes, progress                              (new)
    pill_render.rs tiny-skia renderer for one PillView                                                            (new)
    session_tasks.rs  one stream acceptor routing clipboard and file streams
  src/win/
    dnd/mod.rs    cj-dnd STA thread, DndCmd/DndEv, finish_os_drag (E1)                                            (new)
    dnd/catcher.rs catcher window + IDropTarget + offer extraction + virtual-file spooling                        (new)
    dnd/source.rs DoDragDrop for drags arriving from the Mac: IDataObject, IDropSource                            (new)
    pill.rs       layered pill windows (cj-pill thread)                                                           (new)
    input_thread.rs  catcher zone, offer on EdgePush, raw left-button state
    app.rs        wiring
  src/mac/
    dnd/mod.rs    main-thread dispatch, DndCmd/DndEv                                                              (new)
    dnd/source.rs DragPanel + NSDraggingSession for drags arriving from the PC                                    (new)
    dnd/catcher.rs CatcherPanel + NSDraggingDestination for drags leaving the Mac                                 (new)
    pill.rs       NSPanel + NSVisualEffectView pills                                                              (new)
    control.rs, inject.rs, app.rs   wiring
docs/superpowers/progress/dnd-spike.md   spike findings (Task 1)                                                  (new)
```

**Spike-dependent assumptions.** Task 1 checks these, and the later tasks are written against them. If the spike disproves one, record the fallback in `dnd-spike.md`, amend the named task in this plan before running it, and note the amendment in the ledger.

| Id | Assumption | Default used by | Fallback if false |
|---|---|---|---|
| A1 | Mac targets accept `NSFilePromiseProvider` drops | Task 15 | A2 for that target |
| A2 | A lazy `public.file-url` pasteboard item can block about 60 s at drop time | Task 15 | file-url after prefetch, pill shown during prefetch |
| A3 | An `NSDraggingSession` starts from a CGEvent-posted LeftMouseDown on our panel, follows posted drags, drops on posted up, and Esc cancels it | Task 15 | stop and rethink with the user |
| A4 | Windows targets accept our `IDataObject`, in some format order | Task 12 | the per-target order found |
| A5 | `DoDragDrop` runs while the button is physically held but logically up, ending on our raw-input-driven `QueryContinueDrag` | Task 12 | Exception E2: one `MOUSEEVENTF_LEFTDOWN` before `DoDragDrop`. Ask the user before adding it. |
| A6 | A 1/255-alpha layered topmost window gets `DragEnter` from Explorer, WhatsApp, browser and Discord drags, and E1 ends the source's drag with or without a drop | Task 11 | stop and rethink with the user |
| A7 | A near-transparent Mac panel at the edge gets drags from Finder, WhatsApp, browser and Discord, and a posted LeftMouseUp over it triggers `performDragOperation` | Task 16 | stop and rethink with the user |

---

### Task 1: Spike — prove the drag mechanics on both machines (throwaway)

Throwaway code, with no tests and no review gate beyond the user watching it work. Split: **1a on the PC, 1b [Mac]**. It runs with the user, who does the dragging.

**Files:**
- Create `app/cursorjump/examples/dnd_spike_win.rs` and `app/cursorjump/examples/dnd_spike_mac.rs`. Each is `#![cfg]`-gated, with an empty `main` on the other OS.
- Create `docs/superpowers/progress/dnd-spike.md`.

- [ ] **Step 1 (1a, PC):** Write `dnd_spike_win.rs`. It needs:
  - **Catcher (A6):** a catcher window on the main monitor's right edge (2 px strip) that logs `DragEnter` formats and extracts CF_HDROP paths or FILEDESCRIPTORW/FILECONTENTS. A `--park` mode grows it to the full virtual screen and, after 3 s, ends the drag with one `SendInput` LEFTUP. Accept/reject is chosen by a flag.
  - **Source (A4, A5):** a source mode where you hold the left button and press a hotkey. The program swallows the button with a temporary LL hook to reproduce the "logically up" state, then starts `DoDragDrop` with:
    - (i) a lazy CF_HDROP whose `GetData` sleeps 60 s, then returns a 1 GB temp file;
    - (ii) FILEDESCRIPTORW/FILECONTENTS serving the same bytes from an `IStream`.

    `QueryContinueDrag` follows raw-input left-button state.
- [ ] **Step 2 (1a):** With the user, drag from Explorer, WhatsApp, Gmail (default browser) and Discord onto the catcher. Then drop the spike source into WhatsApp, Gmail, Discord, an Explorer folder, Downloads and the desktop, in both formats. Note:
  - whether the target freezes;
  - whether a 60 s wait survives;
  - whether a drag cursor or drag image is visible at the park point.
- [ ] **Step 3 (1b, Mac):** Write `dnd_spike_mac.rs`. It needs:
  - **Catcher (A7):** a 2 pt near-transparent edge panel that logs dragged types, and `performDragOperation` on a posted LeftMouseUp. It also receives file promises into a temp dir and logs whether `fileNames` is known before the drop.
  - **Source (A1–A3):** a small panel that, on a key, posts LeftMouseDown on itself and starts an `NSDraggingSession` with:
    - (i) `NSFilePromiseProvider`;
    - (ii) a lazy `public.file-url` item that blocks 60 s.

    Arrow keys post LeftMouseDragged, Return posts LeftMouseUp, and Esc is posted as a key.
- [ ] **Step 4 (1b):** With the user, run the same matrix against the WhatsApp desktop app, Gmail (default browser), Discord, a Finder folder, Downloads and the desktop. Also drag out of Finder, WhatsApp, a web page and Discord onto the catcher.
- [ ] **Step 5:** Write `dnd-spike.md`:
  - a per-target table of what each one accepts (promise / lazy URL / lazy HDROP / virtual file) and whether a 60 s wait survives;
  - each machine's default browser;
  - any new macOS permission prompt seen;
  - the verdict on A1–A7;
  - the Windows `IDataObject` format order to use;
  - any plan amendments.
- [ ] **Step 6:** Commit `docs: drag-and-drop spike findings`, with the example files.

---

### Task 2: Drag data model, name sanitizing, protocol v3 (`cj-core::dnd`, `cj-core::proto`)

**Files:**
- Create: `crates/cj-core/src/dnd.rs`
- Modify: `crates/cj-core/src/proto.rs`, `crates/cj-core/src/lib.rs`, `crates/cj-core/Cargo.toml` (add `unicode-normalization`)
- Modify every `Control::Enter` / `Control::Leave` construction or match in `app/` and `crates/` to the new fields (`drag: None`), so that the workspace builds.

**Interfaces — Produces:**
```rust
// dnd.rs
pub type DragId = u64;                                   // random, chosen by the origin; never 0
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind { File, Dir }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DragItem { pub name: String, pub kind: ItemKind, pub size: Option<u64> } // name "" = not known until fetched
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FilesHeader { pub drag: DragId, pub item: u32, pub name: String, pub kind: ItemKind, pub total_bytes: u64, pub entries: u64 }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FileEntry { pub path: Vec<String>, pub kind: ItemKind, pub size: u64 } // path relative to the item root; [] = the root itself
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum FailReason { Disconnected, Sleep, NoSpace, Unreadable { name: String }, Unwritable, Changed { name: String }, Cancelled }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum DragOutcome { Done, NotDropped, Failed(FailReason) }
pub fn sanitize_component(name: &str) -> String;
pub fn safe_rel_path(path: &[String]) -> Option<std::path::PathBuf>;
pub const MAX_PATH_DEPTH: usize = 64;

// proto.rs (v3)
pub const PROTOCOL_VERSION: u16 = 3;
Control::Enter { edge: Side, frac: f32, drag: Option<DragId> }
Control::Leave { frac: f32, drag: Option<DragId> }
// appended after MacReady, in this order:
Control::DragOffer { drag: DragId, items: Vec<DragItem> }       // origin → receiver, just before the crossing that carries it
Control::DragCancel { drag: DragId, reason: FailReason }        // either side
Control::DragDropped { drag: DragId }                           // receiver → origin: released on a target that accepted
Control::FileFetch { drag: DragId, item: u32 }                  // receiver → origin
Control::DragFinished { drag: DragId, outcome: DragOutcome }    // receiver → origin: final result; origin frees its temp files
```

**Sanitizing rules (`sanitize_component`), applied in this order:**
1. NFC-normalize.
2. Replace `< > : " / \ | ? *` and U+0000–U+001F with `_`.
3. Strip trailing dots and spaces.
4. Prefix `_` to a Windows reserved device name: `CON PRN AUX NUL COM1–COM9 LPT1–LPT9`, case-insensitive, with or without an extension.
5. Turn an empty result, `.` or `..` into `_`.
6. Truncate to at most 255 UTF-8 bytes **and** 255 UTF-16 units, on a char boundary, keeping the extension (the last `.xyz` of up to 16 bytes).

`safe_rel_path` sanitizes each component. It returns `None` if the path is longer than `MAX_PATH_DEPTH` or any input component is `.`, `..` or empty. It never yields an absolute path.

- [ ] **Step 1: Write the failing tests** in `dnd.rs` and `proto.rs`:
  - `sanitize_component_cases`: `"a:b.txt"→"a_b.txt"`, `"CON.txt"→"_CON.txt"`, `"lpt3"→"_lpt3"`, `"trailing. "→"trailing"`, `"e\u{301}"→"é"` (NFC, `"\u{e9}"`), `""→"_"`, `".."→"_"`, a 300-char `"x…x.mp4"` → ends with `.mp4`, ≤255 bytes and ≤255 UTF-16 units, and `"emoji🍎.png"` unchanged.
  - `safe_rel_path_rejects_escape`: `["..","x"]`, `["a",""]`, `["."]` and 65 components → `None`; `["Photos","a:b.jpg"]` → `Photos/a_b.jpg` (relative).
  - `control_roundtrip_all_variants` extended with each new variant, plus Enter/Leave with `Some(7)`.
  - `control_variant_indices_are_stable`: `ReleaseAll` → 9 and `MacReady{ready:true}` → `[10,1]` stay; `DragOffer` → index 11 and `DragFinished` → index 15.
- [ ] **Step 2:** Run `cargo test -p cj-core dnd proto`. Expected: FAIL (does not compile).
- [ ] **Step 3:** Implement. Update the doc comment on `PROTOCOL_VERSION`: "3: drag-and-drop".
- [ ] **Step 4:** Run `cargo test -p cj-core` and `cargo clippy --workspace --all-targets -- -D warnings`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): drag data model, name sanitizing, protocol v3`.

---

### Task 3: Drag rules in the state machines (`cj-core::{pc_state, mac_state}`)

**Files:**
- Modify: `crates/cj-core/src/pc_state.rs`, `crates/cj-core/src/mac_state.rs`
- Modify the call sites in `app/` with `offer: None` / `drag: None`, so that the workspace builds (wiring comes in Tasks 13 and 17).

**Interfaces — Produces:**
```rust
// pc_state.rs
PcEvent::EdgePush { frac, game, buttons_held, offer: Option<DragId> } // offer: catcher holds a file drag AND only the left button is held
PcEvent::MacLeave { frac, drag: Option<DragId> }
PcEvent::DragDropped { drag: DragId }          // Mac: our outgoing drag was released on a target
PcEvent::DragGone { drag: DragId }             // either drag ended without needing more from the machine (NotDropped, Finished, DoDragDrop returned)
PcAction::EnterMac { mac_edge, frac, drag: Option<DragId> }
PcAction::FinishSourceDrag { drag }            // dnd: spool virtual items, then E1 with accept
PcAction::CancelSourceDrag { drag }            // dnd: E1 with reject; net: DragCancel{Cancelled}
PcAction::SourceDragHome { drag }              // dnd: catcher back to edge strip; the OS drag continues on the PC
PcAction::StartIncomingDrag { drag }           // dnd: DoDragDrop with the Mac's offer
PcAction::CancelIncomingDrag { drag }          // dnd: QueryContinueDrag → cancel
impl PcMachine { pub fn riding(&self) -> Option<Riding>; }
#[derive(Debug, Clone, Copy, PartialEq)] pub enum Riding { Outgoing(DragId), Incoming(DragId) }

// mac_state.rs
MacEvent::Enter { edge, frac, drag: Option<DragId> }
MacEvent::PushOut { frac, buttons_held, offer: Option<DragId> } // offer: the Mac catcher holds a file drag
MacEvent::DragEnded { drag: DragId }
MacAction::BeginDrag { drag }                  // after StartControl
MacAction::CancelDrag { drag }                 // post Esc; always before ReleaseAllInjected
MacAction::SendLeave { frac, drag: Option<DragId> }
impl MacMachine { pub fn incoming(&self) -> Option<DragId>; }
```

**Rules (PcMachine):**
- **Crossing.** `EdgePush` crosses if the phase 1 conditions hold and (`!buttons_held` or `offer.is_some()`). `game_locked` still blocks. With an offer: `riding = Outgoing(id)` and `EnterMac{drag: Some(id)}`. While `riding == Incoming(_)`, `EdgePush` is ignored.
- **Mac leave.**
  - `MacLeave{drag: Some(id)}` where `id` is the outgoing drag: `[ReturnToPc, SourceDragHome{id}]`, riding cleared.
  - Any other `Some(id)`: `[ReturnToPc, StartIncomingDrag{id}]`, `riding = Incoming(id)`.
- **Drops.** `DragDropped{id}` while `Outgoing(id)`: `[FinishSourceDrag{id}]`, riding cleared (`where_` stays Mac).
- **Drag gone.** `DragGone{id}` clears a matching riding, with no actions.
- **Forced return while Outgoing** (hotkey, hook lost, pause, `MacReady(false)`): `[SendReleaseAll, CancelSourceDrag, ReturnToPc]`.
- **Disconnected.**
  - While Outgoing: `[CancelSourceDrag, ReturnToPc, Reconnect]`.
  - While Incoming: `[CancelIncomingDrag, Reconnect]`.

**Rules (MacMachine):**
- **Enter.** `Enter{drag: Some(id)}` appends `BeginDrag{id}` and sets `incoming`.
- **Push out:**
  - if `incoming == Some(id)`: `[CancelDrag{id}, ReleaseAllInjected, SendLeave{frac, drag: Some(id)}, StopControl]`, incoming cleared;
  - else if `offer == Some(o)`: `[ReleaseAllInjected, SendLeave{frac, drag: Some(o)}, StopControl]` (the release drops onto the catcher);
  - else if `buttons_held`: a wall, as today;
  - else as today, with `drag: None`.
- **Drag ended.** `DragEnded{id}` clears a matching `incoming`.
- **Release all / disconnect.** `ReleaseAll` or `Disconnected` while `incoming`: `CancelDrag` goes first, then the phase 1 actions.

- [ ] **Step 1: Write the failing tests:**
  - **pc_state:**
    - `file_drag_crosses_with_left_held`
    - `buttons_held_without_offer_is_still_a_wall`
    - `file_drag_still_blocked_by_game_lock`
    - `outgoing_drag_coming_home_continues_on_pc` (asserts `[ReturnToPc{..}, SourceDragHome{id}]`)
    - `mac_leave_with_foreign_drag_starts_incoming`
    - `incoming_drag_makes_the_edge_a_wall`
    - `dropped_outgoing_finishes_source_drag_and_stays_on_mac`
    - `hotkey_during_outgoing_drag_cancels_before_return` (exact order)
    - `disconnect_during_outgoing_drag_cancels_before_return` (`[CancelSourceDrag, ReturnToPc, Reconnect]`)
    - `disconnect_during_incoming_drag_cancels_it`
  - **mac_state:**
    - `enter_with_drag_begins_drag_after_start_control`
    - `incoming_drag_push_out_cancels_before_leaving` (exact order)
    - `push_out_with_offer_drops_on_catcher_then_leaves`
    - `push_out_with_buttons_and_no_drag_is_a_wall`
    - `release_all_during_incoming_cancels_first`
    - `drag_ended_clears_incoming`
- [ ] **Step 2:** Run `cargo test -p cj-core pc_state mac_state`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cj-core`, then clippy on the workspace. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): drag rules in the PC and Mac state machines`.

---

### Task 4: Send pacing (`cj-core::pacer`)

**Files:** Create `crates/cj-core/src/pacer.rs`. Modify `lib.rs`.

**Interfaces — Produces:**
```rust
pub struct Pacer;
impl Pacer {
    pub fn new(now: Instant) -> Pacer;
    pub fn on_rtt(&mut self, rtt: Duration, now: Instant);      // fed from the app's pongs
    pub fn delay_for(&mut self, bytes: usize, now: Instant) -> Duration; // token bucket: Duration::ZERO = send now; reserves the bytes
    pub fn rate(&self) -> u64;                                   // bytes/s, for logs
}
pub const START_RATE: u64 = 20_000_000;   // 20 MB/s
pub const FLOOR_RATE: u64 = 2_000_000;
pub const CEIL_RATE: u64 = 1_000_000_000;
pub const RTT_HEADROOM: Duration = Duration::from_millis(4);
```

**Rules:**
- The baseline is the minimum RTT seen in the last 10 s.
- When an RTT sample is above baseline + `RTT_HEADROOM`, multiply the rate by 0.7, at most once per 200 ms.
- Otherwise, add 4 MB/s per 100 ms elapsed since the last increase.
- Clamp the rate to `[FLOOR_RATE, CEIL_RATE]`.
- The bucket depth is 100 ms of the current rate.

- [ ] **Step 1: Write the failing tests:**
  - `starts_at_start_rate`;
  - `rtt_spike_cuts_rate_once_per_200ms` (two spikes 50 ms apart cut once, to 14 MB/s);
  - `quiet_rtt_ramps_up_and_clamps`;
  - `never_below_floor`;
  - `delay_for_spreads_bytes_at_rate` (at 20 MB/s, 10 × 256 KiB after the bucket empties gives a delay of about 131 ms ±5 ms in total);
  - `baseline_forgets_after_10s`.
- [ ] **Step 2:** Run `cargo test -p cj-core pacer`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cj-core`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): RTT-driven pacer for file data`.

---

### Task 5: Pill model and text (`cj-core::pill`)

**Files:** Create `crates/cj-core/src/pill.rs`. Modify `lib.rs`.

**Interfaces — Produces:**
```rust
#[derive(Debug, Clone, Copy, PartialEq)] pub enum Peer { Pc, Mac }       // "PC" / "Mac" in text
#[derive(Debug, Clone, PartialEq)] pub enum PillIcon { Video, Image, Folder, Document, Count(u32) }
pub fn icon_for(items: &[DragItem]) -> PillIcon;   // >1 item → Count; Dir → Folder; by extension (mp4 mov mkv avi webm → Video; jpg jpeg png gif heic webp bmp tiff → Image) else Document
pub fn title_for(items: &[DragItem]) -> String;    // 1 item → its name ("file" if unknown); n>1 → "n files", or "n items" if any Dir
#[derive(Debug, Clone, PartialEq)]
pub struct PillView {
    pub drag: DragId, pub icon: PillIcon, pub name: String,
    pub right: String,        // "42%" while running with a known total; "" otherwise; ✓ is drawn from check_scale
    pub sub: String, pub fraction: Option<f32>, pub sheen_phase: f32,
    pub check_scale: f32,     // 0 until done; pops 0.6→1.0 over CHECK_POP
    pub opacity: f32, pub failed: bool,
}
pub struct PillBoard;
impl PillBoard {
    pub fn new(from: Peer, local: Peer) -> PillBoard;
    pub fn start(&mut self, drag: DragId, items: &[DragItem], now: Instant);
    pub fn progress(&mut self, drag: DragId, done: u64, total: Option<u64>, now: Instant);
    pub fn finish(&mut self, drag: DragId, result: Result<(), FailReason>, now: Instant);
    pub fn dismiss(&mut self, drag: DragId);       // ✕ on a failed pill
    pub fn set_held(&mut self, held: bool);         // PC game lock: views() empty while held; nothing expires while held
    pub fn views(&self, now: Instant) -> Vec<PillView>;  // bottom-most first
    pub fn next_frame(&self, now: Instant) -> Option<Instant>; // None = nothing visible or pending
}
pub const SHOW_AFTER: Duration = Duration::from_millis(500); // "tiny files": not shown if finished sooner
pub const DONE_HOLD: Duration = Duration::from_secs(1);
pub const FADE: Duration = Duration::from_millis(300);
pub const CHECK_POP: Duration = Duration::from_millis(150);
pub const SHEEN_PERIOD: Duration = Duration::from_millis(1600);
pub const ETA_AFTER: Duration = Duration::from_secs(2);
pub fn format_sub(from: Peer, done: u64, total: Option<u64>, eta: Option<Duration>) -> String;
pub fn fail_text(reason: &FailReason, from: Peer, local: Peer) -> String;
```

**Exact copy:**
- **Running:** `"From {from} · {done} of {total} {unit}"`, plus `" · {eta}"` once `ETA_AFTER` has passed and a rate is known. If the total is unknown: `"From {from} · {done} {unit}"`.
- **Units**, chosen from the total (or from `done` if unknown):
  - below 1 000 000 bytes: KB, integer, rounded up;
  - below 1 000 000 000: MB, integer, rounded;
  - otherwise: GB, one decimal.

  Example: `"From PC · 2.1 of 5.0 GB · 1 min left"`.
- **ETA**, from the rate over the last 5 s:
  - under 5 s: `"a few seconds left"`;
  - under 60 s: `"{n} s left"`, with n rounded up to a multiple of 5;
  - under an hour: `"{ceil(min)} min left"`;
  - otherwise: `"{h} h {m} min left"`.
- **Done:** sub `"From {from} · {total} {unit}"`, ✓ shown, `right` = "".
- **Cancelled:** sub `"Cancelled"`, then the pill fades like Done.
- **Failed:** the bar is hidden, the pill persists, and the sub is `fail_text`:
  - Disconnected → `"Copy failed — {from} disconnected"`
  - Sleep → `"Copy failed — {from} went to sleep"`
  - NoSpace → `"Copy failed — not enough space on this {local}"`
  - Unreadable{name} → `"Copy failed — couldn't read “{name}” on the {from}"`
  - Unwritable → `"Copy failed — couldn't save here"`
  - Changed{name} → `"Copy failed — “{name}” changed while copying"`

  Separators are U+00B7 and U+2014, and the quotes are U+201C/U+201D.
- **Percent:** floor of done/total. It shows 100 % only once done.

- [ ] **Step 1: Write the failing tests:**
  - `sub_text_examples` (2.1 of 5.0 GB · 1 min left; 512 of 800 MB; 37 KB; unknown total);
  - `eta_text_buckets`;
  - `failed_pill_texts` (every reason, exact strings);
  - `tiny_copy_never_shows` (finish at +400 ms: no view at +500 ms);
  - `slow_copy_shows_after_500ms`;
  - `done_pops_check_holds_then_fades` (check_scale 0.6 at done, 1.0 at +150 ms, opacity 1.0 at +1 s, 0 and removed at +1.3 s);
  - `failed_persists_until_dismissed`;
  - `concurrent_drags_stack_in_start_order`;
  - `held_board_shows_nothing_and_expires_nothing`;
  - `title_and_icon_rules`.
- [ ] **Step 2:** Run `cargo test -p cj-core pill`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cj-core`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): progress pill model and copy`.

---

### Task 6: File streams on the session (`cj-net::{files, session}`)

**Files:**
- Create: `crates/cj-net/src/files.rs`
- Modify: `crates/cj-net/src/session.rs`, `crates/cj-net/src/lib.rs`, `crates/cj-net/tests/loopback.rs`, `app/cursorjump/src/common/session_tasks.rs`

**Interfaces — Produces:**
```rust
// session.rs
pub enum IncomingStream { Clipboard(ClipboardIn), Files(FilesIn) }
impl Session {
    pub async fn accept_stream(&self) -> anyhow::Result<IncomingStream>; // single consumer; replaces recv_clipboard
    pub async fn send_clipboard(&self, d: &ClipboardData) -> anyhow::Result<()>; // now writes tag 0 first
    pub async fn open_files(&self, h: &FilesHeader) -> anyhow::Result<FilesOut>; // tag 1, priority FILES_PRIORITY
}
pub struct ClipboardIn; impl ClipboardIn { pub async fn read(self, max_bytes: u64) -> anyhow::Result<ClipboardData>; } // old recv_clipboard limit semantics
// files.rs
pub struct FilesOut; impl FilesOut {
    pub async fn entry(&mut self, e: &FileEntry) -> anyhow::Result<()>;   // u32-LE-length-prefixed postcard
    pub async fn write(&mut self, chunk: &[u8]) -> anyhow::Result<()>;    // bytes of the current File entry
    pub fn finish(self) -> anyhow::Result<()>;
    pub fn cancel(self);                                                   // reset(STOP_CANCELLED)
}
pub struct FilesIn; impl FilesIn {
    pub fn header(&self) -> &FilesHeader;
    pub async fn next_entry(&mut self) -> anyhow::Result<Option<FileEntry>>; // Err if the current entry's bytes are not fully read
    pub async fn read_chunk(&mut self, buf: &mut [u8]) -> anyhow::Result<usize>; // 0 = end of current entry
    pub fn cancel(self);                                                       // stop(STOP_CANCELLED)
}
const STREAM_TAG_CLIPBOARD: u8 = 0; const STREAM_TAG_FILES: u8 = 1;
const CONTROL_PRIORITY: i32 = 100; const FILES_PRIORITY: i32 = -100;  // clipboard stays 0
const MAX_UNI_STREAMS: u32 = 16;
const STOP_CANCELLED: u32 = 2;
const MAX_FRAME: usize = 64 * 1024;  // header / entry frame limit
```

**Receiver validation:**
- Errors on:
  - a frame over `MAX_FRAME`;
  - more entries than `header.entries`;
  - cumulative entry sizes over `header.total_bytes`;
  - a `Dir` entry with a nonzero size;
  - a path longer than `MAX_PATH_DEPTH`;
  - an unknown tag. The stream is stopped, and the session stays up.
- `FilesIn` never touches the disk.
- Set the control stream's priority at session start.

`session_tasks.rs`: replace the clipboard reader with one acceptor task that reads `accept_stream()`. It routes `Clipboard` to the existing `on_item` path and `Files` to a new callback. Signature: `spawn_stream_tasks(session, clip_max, on_clip: impl Fn(ClipboardData)->bool, on_files: impl Fn(FilesIn)->bool) -> (ClipboardOut, [JoinHandle<()>; 2])`, which replaces `spawn_clipboard_tasks`. Add `NetEv::Files(FilesIn)` for the apps to use later. Update both apps' call sites to pass `|_| true` for now.

- [ ] **Step 1: Write the failing tests** in `tests/loopback.rs`:
  - `files_stream_roundtrip_20mb`: one item, three entries (dir, 20 MB file, 0-byte file). SHA-256 matches, and `next_entry` returns `None` at the end.
  - `clipboard_still_works_alongside_files` (a 1 MB clipboard item arrives while a 20 MB file stream is in flight);
  - `files_cancel_resets_receiver` (a cancel mid-entry makes `read_chunk` Err);
  - `hostile_entry_path_is_refused` (an entry with 65 components → Err, and the session is still up for a following clipboard item);
  - `entry_bytes_over_header_total_are_refused`;
  - the existing `clipboard_transfer_and_limit`, adapted to `accept_stream` + `ClipboardIn::read`.
- [ ] **Step 2:** Run `cargo test -p cj-net --test loopback`. Expected: FAIL.
- [ ] **Step 3:** Implement. Update `session_tasks.rs` and its test (`clipboard_items_cross_and_the_latest_wins` must still pass).
- [ ] **Step 4:** Run `cargo test -p cj-net` and `cargo test -p cursorjump common::session_tasks`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(net): tagged uni streams and paced-ready file streams`.

---

### Task 7: Transfer engine (`app/cursorjump/src/common/transfer.rs`)

**Files:**
- Create: `app/cursorjump/src/common/transfer.rs`
- Modify: `app/cursorjump/src/common/mod.rs`, `app/cursorjump/Cargo.toml` (`tokio-util`)

**Interfaces:**
- Consumes: `FilesOut`/`FilesIn`/`Session::open_files` (Task 6), `Pacer` (Task 4), `cj_core::dnd::*` (Task 2).
- Produces:
```rust
pub type SharedPacer = Arc<std::sync::Mutex<Pacer>>;
pub async fn send_item(s: &Session, drag: DragId, item: u32, root: &Path, pacer: &SharedPacer, cancel: &CancellationToken) -> Result<(), FailReason>;
pub enum Dest { Exact(PathBuf), InDir(PathBuf) }   // Exact: write exactly this path (Mac promise URL); InDir: dir/<sanitized header.name>
pub trait FreeSpace: Send + Sync { fn free_bytes(&self, dir: &Path) -> std::io::Result<u64>; }
pub struct OsFreeSpace;                              // GetDiskFreeSpaceExW / statvfs
pub async fn receive_item(files: FilesIn, dest: Dest, space: &dyn FreeSpace, progress: impl FnMut(u64) + Send, cancel: &CancellationToken) -> Result<PathBuf, FailReason>;
pub const CHUNK: usize = 256 * 1024;
pub const SPACE_MARGIN: u64 = 64 * 1024 * 1024;
```

**Rules:**
- **Walking the source:** `send_item` walks `root` without following symlinks. Symlinks are skipped and logged at `info`. Empty dirs are sent as `Dir` entries. Before streaming, it sums the sizes for `FilesHeader.total_bytes` and the entry count.
- **Changed files:** if a file's byte count read differs from its `stat` size, the stream is cancelled with `Changed{name}`. An open or read error gives `Unreadable{name}`.
- **Pacing:** before each chunk, sleep `pacer.delay_for(n)`.
- **Space check:** `receive_item` checks `space.free_bytes(parent) >= total_bytes + SPACE_MARGIN` before creating anything, and returns `NoSpace` otherwise.
- **Temp names:** it writes under `parent/.{name}.cjpart`, a file or a directory tree, using `safe_rel_path` for every entry. It renames to the final path only after the last entry. For `InDir`, the final name is the sanitized header name. For `Exact`, it's the given path.
- **On any error or cancel**, it removes the `.cjpart` and maps the error:
  - a stream error → `Disconnected`, unless the token was cancelled (`Cancelled`);
  - `PermissionDenied` or `ReadOnlyFilesystem` → `Unwritable`;
  - `StorageFull` → `NoSpace`.
- **Progress:** `progress` receives cumulative bytes, at most every 100 ms and at the end.

- [ ] **Step 1: Write the failing tests** (tokio, loopback sessions as in `session_tasks` tests, tempdirs):
  - `single_file_roundtrip_in_dir`
  - `exact_dest_is_respected`
  - `folder_tree_roundtrip_keeps_empty_dirs_and_zero_byte_files` (300 files over 3 levels, one empty dir, one 0-byte file, contents compared)
  - `symlinks_are_skipped` (`#[cfg(unix)]`, plus a Windows variant using a junction if creatable, else skipped)
  - `file_that_changes_size_fails_with_changed` (a sender hook test-only fn appends while reading: wrap the reader via a `cfg(test)` seam)
  - `receive_aborted_mid_file_leaves_nothing` (drop the sender's session after 5 MB → `Disconnected`, and the parent dir is empty)
  - `cancel_cleans_up_and_reports_cancelled`
  - `insufficient_space_fails_before_writing` (a fake `FreeSpace` → `NoSpace`, with nothing created)
  - `unwritable_destination_fails_cleanly` (a read-only dir: `set_readonly` on Windows, `0o555` on unix → `Unwritable`, with nothing left)
  - `progress_is_monotonic_and_ends_at_total`
- [ ] **Step 2:** Run `cargo test -p cursorjump common::transfer`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cursorjump common::transfer`, on the PC now and on the Mac in Task 14. Expected: PASS.
- [ ] **Step 5:** Commit `feat(app): file transfer engine with temp-then-rename and pacing`.

---

### Task 8: Drag coordination per session (`app/cursorjump/src/common/dnd_link.rs`)

**Files:** Create `app/cursorjump/src/common/dnd_link.rs`. Modify `common/mod.rs`.

**Interfaces:**
- Consumes: Task 7 `send_item`/`receive_item`/`Dest`/`FreeSpace`, Task 6 `FilesIn`, Task 5 `PillBoard` inputs (via events), and `Control` drag variants (Task 2).
- Produces:
```rust
pub enum Source { Ready(PathBuf), Pending(tokio::sync::watch::Receiver<Option<Result<PathBuf, FailReason>>>) } // Pending: spooled virtual file / Mac promise
pub enum DndEv {
    Progress { drag: DragId, done: u64, total: Option<u64> },   // receiver side, aggregated over the drag's items
    Received { drag: DragId, item: u32, result: Result<PathBuf, FailReason> },
    Finished { drag: DragId, outcome: DragOutcome },            // either side
}
pub struct DndLink;
impl DndLink {
    pub fn new(session: Session, pacer: SharedPacer, space: Arc<dyn FreeSpace>, events: UnboundedSender<DndEv>) -> DndLink;
    // origin
    pub async fn offer(&mut self, drag: DragId, items: Vec<DragItem>, sources: Vec<Source>) -> anyhow::Result<()>; // sends DragOffer
    pub fn on_fetch(&mut self, drag: DragId, item: u32);            // waits for Pending, spawns send_item
    pub fn on_finished(&mut self, drag: DragId, outcome: DragOutcome); // drops sources; emits Finished
    // receiver
    pub fn on_offer(&mut self, drag: DragId, items: Vec<DragItem>);
    pub fn items(&self, drag: DragId) -> Option<&[DragItem]>;
    pub async fn fetch(&mut self, drag: DragId, item: u32, dest: Dest) -> anyhow::Result<()>; // sends FileFetch; the FilesIn is matched in on_files
    pub fn on_files(&mut self, files: FilesIn);                      // unknown (drag,item) → files.cancel()
    pub async fn finish(&mut self, drag: DragId, outcome: DragOutcome) -> anyhow::Result<()>; // sends DragFinished
    // either
    pub async fn cancel(&mut self, drag: DragId, reason: FailReason) -> anyhow::Result<()>; // sends DragCancel; cancels tokens
    pub fn on_cancel(&mut self, drag: DragId, reason: FailReason);
    pub fn disconnected(&mut self);   // every pending fetch → Received{Err(Disconnected)}, every drag → Finished{Failed(Disconnected)}
}
/// The receiver's final outcome once the OS drag has ended: None while any fetched item is still pending.
pub fn incoming_outcome(dropped: bool, results: &[Option<Result<(), FailReason>>]) -> Option<DragOutcome>;
```

**Rules:**
- A drag is complete on the receiver when every fetched item has a `Received`. The caller decides when to call `finish`, after the OS reports the drop is over.
- Progress totals add the known `DragItem.size` of not-yet-started File items to each started item's `FilesHeader.total_bytes`. The total is `None` while any Dir item hasn't started.
- Progress events are throttled to 10 per second per drag.

- [ ] **Step 1: Write the failing tests** (two `DndLink`s over a loopback session pair, with each side's `Control`s pumped by a small test loop):
  - `offer_fetch_receive_finish_roundtrip`
  - `pending_source_waits_then_sends`
  - `pending_source_failure_reports_unreadable`
  - `cancel_from_receiver_stops_sender_and_cleans_dest`
  - `cancel_from_origin_fails_pending_fetch_with_cancelled`
  - `disconnect_fails_pending_fetches`
  - `unknown_files_stream_is_cancelled`
  - `progress_aggregates_two_items_and_is_throttled`
  - `incoming_outcome_rules` (`None` while any fetched item is pending; `NotDropped`; `Done`; the first `Failed` wins)
- [ ] **Step 2:** Run `cargo test -p cursorjump common::dnd_link`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cursorjump common`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(app): per-session drag coordination`.

---

### Task 9: Pill renderer (`app/cursorjump/src/common/pill_render.rs`)

**Files:**
- Create: `app/cursorjump/src/common/pill_render.rs`, and `app/cursorjump/assets/fonts/{Inter-Regular.ttf, Inter-SemiBold.ttf, OFL.txt}` (from the official Inter release)
- Modify: `common/mod.rs`, `Cargo.toml` (`tiny-skia`, `ab_glyph`)

**Interfaces:**
- Consumes: `PillView`, `PillIcon` (Task 5).
- Produces:
```rust
pub const PILL_W: f32 = 360.0; pub const PILL_H: f32 = 52.0; pub const PILL_GAP: f32 = 8.0; pub const BOTTOM_MARGIN: f32 = 24.0; // logical px
pub enum Backdrop { Blurred, Opaque }   // Mac (blur view behind) / Windows (no blur)
pub fn render(view: &PillView, scale: f32, backdrop: Backdrop, hover: bool) -> tiny_skia::Pixmap; // premultiplied RGBA, size = pill_px(scale) + shadow
pub fn pill_px(scale: f32) -> (u32, u32);
pub const SHADOW_PAD: f32 = 12.0;
pub fn close_rect(scale: f32) -> tiny_skia::Rect;  // ✕ hit area, pixmap coordinates
```

**Visuals:**

| Element | Spec |
|---|---|
| Body | Full pill radius. Fill rgba(18,18,18,0.62) for `Blurred`, 0.80 for `Opaque` (Claude's call: Windows has no blur behind a layered window). 1 px border rgba(255,255,255,0.14). Soft shadow: black at 0.45, 24 px blur, offset y = 6. |
| Padding | 14 px horizontal |
| Icon square | 26 px, radius 7, rgba(255,255,255,0.12). Glyphs drawn as vector shapes in #F5F5F5: play triangle, photo, folder, page. `Count(n)` is the number in SemiBold 13. |
| Name | Inter SemiBold 12, #F5F5F5, ellipsized ("…") to fit |
| Percent | Regular 12 at 60 % opacity |
| ✓ | Drawn at the percent position, scaled by `check_scale` |
| Bar | 3 px, track rgba(255,255,255,0.14). Fill #FFFFFF with a 160 px moving highlight (#D9D9D9→#FFFFFF→#D9D9D9) at `sheen_phase`. No bar if `fraction` is `None` and the pill is failed. An indeterminate thin track if `None` while running. |
| Sub | Regular 10.5 at 55 % opacity, ellipsized |
| ✕ | Drawn only if `hover` or `failed`, at the right, replacing the percent |
| Opacity | The whole pixmap is multiplied by `view.opacity` |

- [ ] **Step 1: Write the failing tests:**
  - `body_alpha_matches_backdrop` (centre-left body pixel alpha ≈ 158 ±3 for `Blurred`, 204 ±3 for `Opaque`);
  - `bar_fill_tracks_fraction` (at 0.42, the white run on the bar row ends at 42 % ±1 px of the bar's width);
  - `long_name_is_ellipsized` (a 200-char name: the measured run stays inside the name box, and the string ends with "…");
  - `close_only_on_hover_or_failed`;
  - `opacity_scales_alpha`;
  - `scale_2_doubles_pixels`.
- [ ] **Step 2:** Run `cargo test -p cursorjump common::pill_render`. Expected: FAIL.
- [ ] **Step 3:** Implement. Fonts are embedded with `include_bytes!`.
- [ ] **Step 4:** Run the tests. Expected: PASS. Write `pill_preview.png` to the scratch dir from a `#[ignore]` test (`preview_png`), and show it to the user next to the mockup `.superpowers/brainstorm/556-1792960514/content/transfer-pill-mono.html`. Adjust until they're happy.
- [ ] **Step 5:** Commit `feat(app): dark glass pill renderer`.

---

### Task 10: Windows pill windows (`app/cursorjump/src/win/pill.rs`)

**Files:** Create `app/cursorjump/src/win/pill.rs`. Modify `win/mod.rs`.

**Interfaces:**
- Consumes: `PillBoard` (Task 5) and `render(.., Backdrop::Opaque, ..)` (Task 9).
- Produces:
```rust
pub enum PillCmd { Start { drag: DragId, items: Vec<DragItem>, at: (i32, i32) }, Progress { drag: DragId, done: u64, total: Option<u64> }, Finish { drag: DragId, result: Result<(), FailReason> }, Held(bool) }
pub fn spawn_pill_thread(on_cancel: impl Fn(DragId) + Send + 'static) -> anyhow::Result<std::sync::mpsc::Sender<PillCmd>>; // thread "cj-pill"
fn stack_origin(work: Rect, scale: f32, index: usize) -> (i32, i32); // pure
```

**Rules:**
- **Thread.** The thread owns a `PillBoard::new(Peer::Mac, Peer::Pc)` and one popup window per visible pill:
  - extended styles `WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`;
  - shown with `SW_SHOWNOACTIVATE`;
  - painted with `UpdateLayeredWindow` (per-pixel alpha, BGRA premultiplied DIB from the pixmap).
- **Position.** The monitor is the one under `at` (`MonitorFromPoint`), using its work area and DPI (`GetDpiForMonitor`). The pills are centred horizontally, `BOTTOM_MARGIN` above the work area's bottom, and stack upward with `PILL_GAP`.
- **Redraw.** Redraws at the board's `next_frame` (about 30 fps cap) with a timer. There's no timer when idle.
- **Hover and clicks.** Hover comes from `WM_MOUSEMOVE` plus `TrackMouseEvent(TME_LEAVE)`. A click in `close_rect` on a running pill calls `on_cancel`. On a failed pill it calls `dismiss`.
- **Held.** `Held(true)` hides every pill window, and the board is held.

- [ ] **Step 1: Write the failing test:** `stack_origin_bottom_centre_and_upward` (a work area of 0,0–1920,1040 at scale 1.0 and 1.5; index 0 and 1).
- [ ] **Step 2:** Run `cargo test -p cursorjump win::pill`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests. Expected: PASS. Then add an `#[ignore]` smoke test `smoke_pill_runs_5s` that starts a fake 5 s transfer. Run it once with the user watching to check the look, the stacking, the ✓ and fade, the failed state and ✕, and that the pill never steals focus from a focused Notepad.
- [ ] **Step 5:** Commit `feat(win): pill windows`.

---

### Task 11: Windows catcher and drags leaving the PC (`win/dnd/{mod,catcher}.rs`, `win/input_thread.rs`)

**Files:**
- Create: `app/cursorjump/src/win/dnd/mod.rs`, `app/cursorjump/src/win/dnd/catcher.rs`
- Modify: `app/cursorjump/src/win/input_thread.rs`, `app/cursorjump/src/win/mod.rs`, `app/cursorjump/Cargo.toml` (windows features: `Win32_System_Ole`, `Win32_System_Com`, `Win32_System_Com_StructuredStorage`, `Win32_Storage_FileSystem`, `Win32_System_SystemServices`; enable `implement`)

**Interfaces:**
- Consumes: `layout::Rect`/edge helpers, `gaming::game_locked`, `probe_game` (existing), `DragItem` (Task 2), `Source` (Task 8).
- Produces:
```rust
// dnd/mod.rs
pub enum DndCmd { CatcherZone(bool), Park(bool), Finish { drag: DragId }, Cancel { drag: DragId }, Home { drag: DragId },
                  StartIncoming { drag: DragId, items: Vec<DragItem> }, CancelIncoming { drag: DragId } /* Task 12 */ }
pub enum WinDndEv { Offer { drag: DragId, items: Vec<DragItem>, sources: Vec<Source> }, OfferGone { drag: DragId },
                    Spooled { drag: DragId } /* virtual items written, OS drag ended */, IncomingEnded { drag: DragId, dropped: bool } /* Task 12 */,
                    Fetch { drag: DragId, item: u32, dir: PathBuf, reply: tokio::sync::oneshot::Sender<Result<PathBuf, FailReason>> } /* Task 12 */ }
pub struct DndShared { pub offer: AtomicU64 /* 0 = none */, pub left_down: AtomicBool /* raw input, physical */ }
pub fn spawn_dnd_thread(cfg: Config, shared: Arc<DndShared>, events: impl Fn(WinDndEv) + Send + 'static) -> anyhow::Result<std::sync::mpsc::Sender<DndCmd>>; // "cj-dnd", OleInitialize (STA)
fn finish_os_drag(accept: bool);  // Exception E1: one SendInput MOUSEEVENTF_LEFTUP; debug_assert park mode; info! log
// catcher.rs
fn strip_rect(monitor: Rect, side: Side) -> Rect;   // 2 px along the Mac edge, full edge length (pure)
fn items_from_paths(paths: &[PathBuf]) -> Vec<DragItem>; // name, kind, size via metadata (pure, fs)
fn items_from_descriptors(names: &[(String, bool, Option<u64>)]) -> (Vec<DragItem>, Vec<Vec<u32>>); // virtual: group "a\b.txt" by first component; returns per-top-level-item descriptor indices
const CATCH_ZONE_PX: i32 = 48;
const SELF_FORMAT: &str = "CursorJump.Self";  // registered clipboard format marking our own data objects
```

**Rules:**
- **Catcher window.** Class `CursorJumpCatcher`, `WS_POPUP`, extended styles `WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`, alpha 1 (`SetLayeredWindowAttributes`). Registered with `RegisterDragDrop`.
- **Showing it.** `CatcherZone(true)` shows the strip. The input thread sends it on raw mouse input while on the PC when all of these hold:
  - only `VK_LBUTTON` is held;
  - the cursor is within `CATCH_ZONE_PX` of the Mac edge;
  - `!game_locked(probe_game())`. Probe only when the first two hold.

  It sends `false` when any of them stops holding. It sends nothing while a game is locked.
- **`DragEnter`:**
  - If the data object has `SELF_FORMAT`, or neither of the formats below: `DROPEFFECT_NONE`.
  - CF_HDROP → `Source::Ready` per path.
  - Otherwise FILEDESCRIPTORW + FILECONTENTS → `Source::Pending` per top-level item.

  Then store the `IDataObject`, set `shared.offer` to a fresh random id, emit `Offer`, and return `DROPEFFECT_COPY`. `DragOver` keeps returning COPY (or NONE in reject mode).
- **`DragLeave`** when not parked: clear the offer and emit `OfferGone`.
- **`Park(true)`** (on `EnterMac{drag: Some}`) grows the window to the virtual screen. `Park(false)` shrinks it back and hides it.
- **`Finish{drag}`:**
  1. For each pending item, read FILECONTENTS (`lindex` = descriptor index; `TYMED_ISTREAM` or `TYMED_HGLOBAL`) into `%LOCALAPPDATA%\CursorJump\outgoing\<drag>\<sanitized path>`, then resolve its watch.
  2. Call `finish_os_drag(true)`; `Drop` returns `DROPEFFECT_COPY`.
  3. `Park(false)`, then emit `Spooled`.
- **`Cancel{drag}`:** reject mode, then `finish_os_drag(false)` (`DragOver` returns NONE, so there's no `Drop`), then `Park(false)`. Pending sources resolve to `Err(Cancelled)`.
- **`Home{drag}`:** `Park(false)` and back to strip mode. The offer is cleared. The OS drag goes on locally.
- **Input thread changes:**
  - **`EdgePush`:** set `offer` to `shared.offer` (nonzero) only when exactly the left button is held.
  - **Raw left button:** on every `WM_INPUT` mouse packet, `RI_MOUSE_LEFT_BUTTON_DOWN`/`UP` update `shared.left_down`.
  - **Parking:** unchanged. The full-screen catcher lies under the park point.
- **Drag cursor at the park point:** if the spike found a drag cursor or drag image visible there, apply the fix recorded in `dnd-spike.md`. If none was found, accept it and log it in the ledger for the user.

- [ ] **Step 1: Write the failing tests:**
  - `strip_rect_on_each_side` (including a negative-x monitor);
  - `items_from_paths_files_and_dirs` (a tempdir);
  - `items_from_descriptors_groups_by_top_component`;
  - `catcher_zone_logic` (pure fn `fn in_catch_zone(edge: &Rect, side, x, y) -> bool`: 47 px in → true, 49 → false);
  - `edge_push_offer_only_with_left_alone` (pure fn `fn offer_for(left: bool, others: bool, offer: u64) -> Option<DragId>`).
- [ ] **Step 2:** Run `cargo test -p cursorjump win::dnd win::input_thread`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cursorjump` and clippy. Expected: PASS. Then run the `#[ignore]` `smoke_catcher_logs_offer` with the user: drag a file from Explorer to the right edge (the app isn't running), and see the `Offer` logged with the right names and sizes.
- [ ] **Step 5:** Commit `feat(win): edge catcher for drags leaving the PC (exception E1)`.

---

### Task 12: Windows drags arriving from the Mac (`win/dnd/source.rs`)

**Files:** Create `app/cursorjump/src/win/dnd/source.rs`. Modify `win/dnd/mod.rs`.

**Interfaces:**
- Consumes: `DndShared.left_down` (Task 11), `DndCmd::{StartIncoming, CancelIncoming}`, `WinDndEv::{Fetch, IncomingEnded}`, `SELF_FORMAT`, and the format order from `dnd-spike.md` (A4).
- Produces:
```rust
fn continue_decision(escape: bool, left_down: bool, cancelled: bool) -> HRESULT; // S_OK / DRAGDROP_S_DROP / DRAGDROP_S_CANCEL (pure)
fn hdrop_bytes(paths: &[PathBuf]) -> Vec<u8>;   // DROPFILES (fWide) + double-NUL list (pure)
```

**Rules:**
- **`StartIncoming`** runs `DoDragDrop` on the cj-dnd thread with:
  - our `IDataObject`, which offers `SELF_FORMAT` plus the A4 formats in the A4 order;
  - our `IDropSource`, whose `QueryContinueDrag` = `continue_decision(fEscapePressed, shared.left_down, cancel_flag)` and `GiveFeedback` = `DRAGDROP_S_USEDEFAULTCURSORS`;
  - allowed effects `DROPEFFECT_COPY` only.
- **Drag image:** `IDragSourceHelper::InitializeFromBitmap` with the shell icon of the first item (`SHGetFileInfoW`, `SHGFI_USEFILEATTRIBUTES | SHGFI_ICON | SHGFI_LARGEICON`).
- **Lazy CF_HDROP `GetData`:** for each item, emit `Fetch{dir: %LOCALAPPDATA%\CursorJump\incoming\<drag>}` and block on the replies, at any wait. Then return the HGLOBAL from `hdrop_bytes` with the received paths. If any reply fails, return `E_FAIL`. Cache the result for repeat `GetData` calls.
- **FILEDESCRIPTORW / FILECONTENTS (when A4 needs it):** FILECONTENTS returns an `IStream` whose `Read` is fed from the item as it arrives (`Fetch` into the incoming dir, then read the growing file). A failed fetch makes `Read` return `E_FAIL`.
- **When `DoDragDrop` returns:** emit `IncomingEnded{dropped: effect != NONE}`.
- **`CancelIncoming`** sets the cancel flag.

- [ ] **Step 1: Write the failing tests:**
  - `continue_decision_table` (escape → cancel; cancelled → cancel; !left_down → drop; else continue);
  - `hdrop_bytes_layout` (header offset 20, fWide = 1, two paths, double NUL).
- [ ] **Step 2:** Run `cargo test -p cursorjump win::dnd::source`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests and clippy. Expected: PASS. Then run the `#[ignore]` `smoke_incoming_drag_from_local_file` with the user: hold the left button, the test starts an incoming drag backed by a local 50 MB file through the real `Fetch` path (a fake fetch copies it slowly over 5 s), and the user drops it into WhatsApp, Gmail, Discord and an Explorer folder.
- [ ] **Step 5:** Commit `feat(win): drags arriving from the Mac`.

---

### Task 13: PC app wiring (`win/app.rs`)

**Files:** Modify `app/cursorjump/src/win/app.rs` and `app/cursorjump/src/win/input_thread.rs` (`EnterMac{drag}` → `DndCmd::Park(true)` before the hooks go on).

**Interfaces — Consumes:** everything from Tasks 3, 4, 5, 8, 10, 11 and 12.

**Rules:**
- **Per live session:** one `DndLink` and one `SharedPacer`, fed from every Pong (`Pacer::on_rtt`). The stream acceptor's `on_files` → `NetEv::Files`.
- **`WinDndEv::Offer` / `OfferGone`:** keep the latest offer (items and sources). The input thread reads the atomic itself.
- **`PcAction::EnterMac{drag: Some(id)}`:** `dnd.offer(id, items, sources)` (sends `DragOffer`), then `Control::Enter{.., drag: Some(id)}` on the same control stream.
- **`FinishSourceDrag` / `CancelSourceDrag` / `SourceDragHome`:** `DndCmd::Finish` / `Cancel` / `Home`. `CancelSourceDrag` also sends `DragCancel{Cancelled}` if the link is up.
- **`StartIncomingDrag{id}`:** `DndCmd::StartIncoming{items: dnd.items(id)}` and `PillCmd::Start{at: cursor_pos}`, so the pill shows on the monitor where the drop happens.
- **`WinDndEv::Fetch`:** `dnd.fetch(id, item, Dest::InDir(dir))`, with a reply on `DndEv::Received`.
- **`IncomingEnded{dropped}`:**
  - If not dropped: `dnd.finish(NotDropped)`.
  - If dropped: `finish` with `incoming_outcome(..)` (Task 8) once it returns `Some`. The pill gets `Finish`.
  - Feed `PcEvent::DragGone`.
- **From the Mac:**
  - `Control::DragDropped{id}` → `PcEvent::DragDropped`.
  - `Control::DragFinished{id, outcome}` → `dnd.on_finished` and `PcEvent::DragGone`.
  - `Control::FileFetch` → `dnd.on_fetch`.
  - `Control::DragCancel` → `dnd.on_cancel`, plus `DndCmd::CancelIncoming` if it's the incoming drag.
  - `Control::DragOffer` → `dnd.on_offer`.
  - `Control::Leave{frac, drag}` → `PcEvent::MacLeave{frac, drag}`.
- **Pill updates:** `DndEv::Progress` → `PillCmd::Progress`. A pill ✕ → `dnd.cancel(id, Cancelled)` and `DndCmd::CancelIncoming`.
- **Game hold-back:** every 1 s while any pill is pending, `PillCmd::Held(game_locked(probe_game()))`.
- **Disconnect:** `dnd.disconnected()` before the machine's `Disconnected`.
- **Startup cleanup:** delete `outgoing\*` older than 1 h and `incoming\*` older than 24 h. Incoming files are kept that long because targets such as WhatsApp read them after the drop (Claude's call). Repeat hourly. Put this in `common/dnd_link.rs` as `pub fn sweep_temp(root: &Path, max_age: Duration, now: SystemTime) -> usize` (returns the number of entries removed), so the Mac reuses it.

- [ ] **Step 1: Write the failing test:** `sweep_temp_removes_only_old_entries` (a tempdir with one entry 2 h old and one fresh, checked against `max_age` = 1 h; mtimes set with `File::set_modified`).
- [ ] **Step 2:** Run `cargo test -p cursorjump common::dnd_link::tests::sweep_temp_removes_only_old_entries`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cursorjump`, clippy and `cargo fmt --check`. Expected: PASS. Push the branch, and check that CI is green on both OSes.
- [ ] **Step 5:** Commit `feat(win): drag-and-drop wiring`.

---

### Task 14 [Mac]: Mac pill panels (`mac/pill.rs`, `mac/dnd/mod.rs`)

**Files:**
- Create: `app/cursorjump/src/mac/pill.rs`, `app/cursorjump/src/mac/dnd/mod.rs` (dispatch helper only)
- Modify: `mac/mod.rs`, `Cargo.toml` (objc2-app-kit features: NSPanel, NSWindow, NSView, NSVisualEffectView, NSImage, NSImageView, NSScreen, NSTrackingArea, NSColor, NSGraphics; `dispatch2`)

**Interfaces:**
- Consumes: `PillBoard` and `render(.., Backdrop::Blurred, ..)`.
- Produces:
```rust
// dnd/mod.rs
pub fn on_main(f: impl FnOnce(MainThreadMarker) + Send + 'static);           // dispatch2 main queue, async
pub fn on_main_sync<R: Send>(timeout: Duration, f: impl FnOnce(MainThreadMarker) -> R + Send) -> Option<R>;
// pill.rs
pub enum PillCmd { Start { drag: DragId, items: Vec<DragItem> }, Progress { drag: DragId, done: u64, total: Option<u64> }, Finish { drag: DragId, result: Result<(), FailReason> } }
pub fn pill_host(on_cancel: impl Fn(DragId) + Send + Sync + 'static) -> impl Fn(PillCmd) + Send + Sync + Clone;  // board lives on the main thread
fn stack_origin_pt(visible: (f64, f64, f64, f64), index: usize) -> (f64, f64);  // pure; AppKit bottom-left origin
```

**Rules:**
- **Board.** The board is `PillBoard::new(Peer::Pc, Peer::Mac)`, living on the main thread.
- **Panel.** One `NSPanel` per visible pill, with:
  - style borderless | nonactivatingPanel, clear background, `hasShadow` false (the renderer draws the shadow);
  - level `NSStatusWindowLevel`;
  - collectionBehavior `canJoinAllSpaces | fullScreenAuxiliary | stationary`.
- **Contents.** An `NSVisualEffectView` (material `HUDWindow`, `darkAqua` appearance, state active, `maskImage` = pill shape) under an `NSImageView` showing the rendered pixmap (CGImage at the backing scale).
- **Position.** The screen is the one under the mouse at `Start`: `visibleFrame`, bottom centre, `BOTTOM_MARGIN`, stacking upward.
- **Hover and clicks.** Hover comes from an `NSTrackingArea`. A click in `close_rect` → cancel (running) or dismiss (failed).
- **Redraw.** A main-queue timer at the board's `next_frame`, with no timer when idle.

- [ ] **Step 1: Write the failing test:** `stack_origin_pt_bottom_centre_and_upward`.
- [ ] **Step 2:** Run `cargo test -p cursorjump mac::pill`. Expected: FAIL.
- [ ] **Step 3:** Implement. Also run `cargo test -p cursorjump common::transfer common::dnd_link common::pill_render` on the Mac. Expected: PASS.
- [ ] **Step 4:** Run the tests. Then run the `#[ignore]` `smoke_pill_runs_5s` with the user watching, over a light and a dark wallpaper and over a full-screen app. Check the blur, the stacking, the ✓ and fade, the failed state and ✕, and that the pill takes no focus.
- [ ] **Step 5:** Commit `feat(mac): pill panels`.

---

### Task 15 [Mac]: Mac drags arriving from the PC (`mac/dnd/source.rs`, `mac/control.rs`, `mac/inject.rs`)

**Files:**
- Create: `app/cursorjump/src/mac/dnd/source.rs`
- Modify: `mac/control.rs`, `mac/inject.rs`, `mac/dnd/mod.rs`, `Cargo.toml` (NSDragging*, NSFilePromiseProvider, NSPasteboardItem, NSWorkspace, NSOperation, NSURL, objc2-uniform-type-identifiers)

**Interfaces:**
- Consumes: `MacAction::{BeginDrag, CancelDrag}` (Task 3), `Injector`, `on_main_sync`, and A1/A2/A3 from `dnd-spike.md`.
- Produces:
```rust
pub enum MacDndEv { Ended { drag: DragId, dropped: bool }, Fetch { drag: DragId, item: u32, dest: Dest, reply: tokio::sync::oneshot::Sender<Result<PathBuf, FailReason>> }, /* Task 16: */ Offer { drag: DragId, items: Vec<DragItem>, sources: Vec<Source> }, OfferGone { drag: DragId } }
pub struct DragSource; impl DragSource {
    pub fn new(events: impl Fn(MacDndEv) + Send + Sync + 'static) -> DragSource;
    pub fn prepare(&self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> bool;  // main thread (sync, ≤100 ms): DragPanel 8×8 pt under `at`, items armed
}
pub const KVK_ESCAPE: u16 = 0x35;
fn utt_for_name(name: &str) -> String; // UTType identifier from the extension, "public.data" fallback (pure-ish)
```

**Rules:**
- **`BeginDrag{id}`** on cj-inject (after WarpTo and StartControl):
  1. `DragSource::prepare(id, items, cursor)`. The items come from the latest `DragOffer`, passed in through the control thread's channel.
  2. Then `injector.button(Left, true)`. The posted LeftMouseDown lands on DragPanel, whose view's `mouseDown:` calls `beginDraggingSessionWithItems:event:source:`.
- **Dragging items.** One `NSDraggingItem` per item:
  - A1 (default): an `NSFilePromiseProvider` with `fileType` = `utt_for_name`, delegate → `fileNameForType` = sanitized name, `writePromiseToURL` → `Fetch{dest: Dest::Exact(url)}`, and the completion handler called with the reply. It uses its own `NSOperationQueue`.
  - Targets the spike marked A2: an `NSPasteboardItem` with a data provider for `public.file-url` that blocks on `Fetch{dest: InDir(~/Library/Caches/CursorJump/incoming/<drag>)}`.
- **Drag image:** `NSWorkspace iconForContentType`.
- **Session ending.** `draggingSession:endedAtPoint:operation:` → `Ended{dropped: operation != none}`. The control thread feeds `MacEvent::DragEnded`.
- **`CancelDrag`:** `injector.key(KVK_ESCAPE, true)` then `false`.
- **Injector:** the Left button held from `BeginDrag` makes motion post `LeftMouseDragged`, as today. Nothing new is needed beyond checking it in the smoke test.

- [ ] **Step 1: Write the failing tests:**
  - `utt_for_name_cases` (mp4 → `public.mpeg-4`, jpg → `public.jpeg`, unknown → `public.data`);
  - a control routing test `begin_drag_prepares_then_presses_left` (fake `Inject` + fake source, asserting the call order);
  - `cancel_drag_posts_escape_before_release`.
- [ ] **Step 2:** Run `cargo test -p cursorjump mac::dnd mac::control`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests and clippy. Expected: PASS. Then run the `#[ignore]` `smoke_drag_from_fake_pc` with the user. It arms a drag backed by a local 50 MB file through a slow fake `Fetch`, presses Left, drives it with posted moves, and the user picks the drop target with the arrow keys: WhatsApp, Gmail, Discord, a Finder folder and the desktop.
- [ ] **Step 5:** Commit `feat(mac): drags arriving from the PC`.

---

### Task 16 [Mac]: Mac catcher and drags leaving the Mac (`mac/dnd/catcher.rs`)

**Files:** Create `app/cursorjump/src/mac/dnd/catcher.rs`. Modify `mac/control.rs` and `mac/dnd/mod.rs` (`NSFilePromiseReceiver`).

**Interfaces:**
- Consumes: the entry edge (`MacMachine::entry_edge`), the injector's virtual cursor and `buttons_held`, `MacEvent::PushOut{offer}`, and A7.
- Produces:
```rust
pub struct Catcher; impl Catcher {
    pub fn new(events: impl Fn(MacDndEv) + Send + Sync + 'static) -> Catcher;
    pub fn zone(&self, show: Option<Side>);   // Some(edge) shows the 2 pt strip on that edge of the main display; None hides (async to main)
    pub fn offer(&self) -> Option<DragId>;    // AtomicU64 read, any thread
}
fn in_catch_zone(bounds: Rectf, edge: Side, pos: (f64, f64)) -> bool;  // 48 pt (pure)
```

**Rules:**
- **Showing the strip.** cj-inject calls `zone(Some(entry))` while controlled, with only Left held and the cursor `in_catch_zone`, and `zone(None)` otherwise. Change detection means a call only happens on transitions.
- **Panel.** Borderless nonactivating `NSPanel`, background alpha 0.01, level `NSStatusWindowLevel`, all Spaces and full screen, `registerForDraggedTypes: [public.file-url] + NSFilePromiseReceiver.readableDraggedTypes`.
- **`draggingEntered:`** builds the items:
  - file URLs → `Source::Ready` (names and sizes from the file system);
  - promise receivers → `Source::Pending` (name from `fileNames` if the spike found it known, else "").

  Then set the offer atomic, emit `Offer`, and return Copy.
- **`draggingExited:`** without a push-out: clear the offer and emit `OfferGone`.
- **`performDragOperation:`** (from `ReleaseAllInjected`'s LeftMouseUp at push-out) returns YES. URLs are already ready. For promises, call `receivePromisedFilesAtDestination: ~/Library/Caches/CursorJump/outgoing/<drag>/` with an operation queue, and each reader callback resolves its watch: Ok(path), or Err(Unreadable{name}).
- **`PushOut`:** `offer` = `catcher.offer()` when only Left is held.

- [ ] **Step 1: Write the failing tests:**
  - `in_catch_zone_each_edge` (47 pt in → true, 49 → false);
  - control test `push_out_carries_catcher_offer_only_with_left_alone`.
- [ ] **Step 2:** Run `cargo test -p cursorjump mac::dnd mac::control`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests and clippy. Expected: PASS. Then run the `#[ignore]` `smoke_catcher_logs_offer` with the user: drag from Finder, WhatsApp, a web page and Discord to the left edge, and check that the offer is logged correctly.
- [ ] **Step 5:** Commit `feat(mac): edge catcher for drags leaving the Mac`.

---

### Task 17 [Mac]: Mac app wiring (`mac/app.rs`)

**Files:** Modify `app/cursorjump/src/mac/app.rs` and `app/cursorjump/src/mac/control.rs`.

**Interfaces — Consumes:** Tasks 3, 4, 5, 8, 14, 15 and 16.

**Rules (mirror of Task 13):**
- **Per session:** one `DndLink` and `SharedPacer` (fed from pongs), and `NetEv::Files` → `on_files`.
- **`Control::DragOffer`** → `dnd.on_offer`. The session reader also passes it straight to cj-inject on the same channel as `Enter`. Input bypasses the app loop, so this is the only way the offer's items are guaranteed to reach cj-inject before the `Enter` that begins the drag.
- **`Control::Enter{drag}`** → `MacEvent::Enter{drag}`, then `PillCmd::Start` when the session reports `Ended{dropped: true}`. The pill shows on the receiving machine, at the drop.
- **`MacDndEv::Fetch`** → `dnd.fetch`, with a reply on `Received`.
- **`Ended`:**
  - not dropped: send `DragFinished{NotDropped}`;
  - dropped: send `DragDropped` at once, then `DragFinished` with `incoming_outcome(..)` (Task 8) once it returns `Some`.
- **Offers:** `MacDndEv::Offer` / `OfferGone` keep the latest offer.
- **On `SendLeave{drag: Some(o)}`:** if `o` is the local offer, `dnd.offer(o, items, sources)` then `Control::Leave{.., drag: Some(o)}`. If `o` is the incoming drag (going home), just `Leave`.
- **From the PC:**
  - `Control::FileFetch` → `on_fetch`.
  - `Control::DragFinished` → `on_finished`.
  - `Control::DragCancel` → `on_cancel`, plus `CancelDrag` if it's the incoming drag.
- **Pill updates:** `DndEv::Progress` → `PillCmd::Progress`. A pill ✕ → `dnd.cancel`.
- **Before sleep:** on `NSWorkspaceWillSleepNotification` (`power.rs`), `dnd.cancel(each active, Sleep)` before the session drops.
- **Cleanup:** `sweep_temp` (Task 13), under `~/Library/Caches/CursorJump/{outgoing,incoming}`.
- **Quarantine (spec: "decide and note it"):** received files get no quarantine flag. CursorJump doesn't set `LSFileQuarantineEnabled`, and doesn't add `com.apple.quarantine`. They're treated like a local copy from the user's own PC. Note this in `HANDOFF.md` (Task 18).
- **New Mac permissions:** if the spike or the smoke tests hit a new permission prompt (none is expected: windows, drag sessions and posted events are covered by Accessibility), add it to the phase 1 "Mac not ready" wall's checks and text.

- [ ] **Step 1: Write the failing test:** `going_home_leave_sends_no_offer` (pure fn `fn leave_needs_offer(drag: Option<DragId>, local_offer: Option<DragId>, incoming: Option<DragId>) -> bool`).
- [ ] **Step 2:** Run `cargo test -p cursorjump mac::app`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p cursorjump`, clippy and fmt. Expected: PASS. Push, and check that CI is green.
- [ ] **Step 5:** Commit `feat(mac): drag-and-drop wiring`.

---

### Task 18: Acceptance, install and handoff (both machines, with the user)

**Files:**
- Modify: `docs/ACCEPTANCE.md` (new "File drag-and-drop" section), `docs/HANDOFF.md`
- Delete: `app/cursorjump/examples/dnd_spike_*.rs`

- [ ] **Step 1:** Install both from the branch, together (protocol v3): `scripts\win\install.ps1` on the PC, `./scripts/mac/install.sh` on the Mac.
- [ ] **Step 2:** Run the spec's acceptance list in both directions with the user, and record each result in `ACCEPTANCE.md`:
  - a 5 GB file with the cursor feeling unchanged. The latency readout's median during the copy should be within 2 ms of idle; if it isn't, tune `RTT_HEADROOM` or the rate constants in `pacer.rs` and re-run;
  - several files, and a folder;
  - drops into WhatsApp, Gmail, Discord, Downloads and the desktop;
  - drags out of WhatsApp, Discord and a web page;
  - a Wi-Fi drop mid-copy (pill "Copy failed — … disconnected", no `.cjpart` left);
  - Mac sleep mid-copy ("went to sleep");
  - cancel with ✕;
  - dragging back across the edge mid-drag, and Esc mid-drag (nothing dropped, nothing stuck);
  - the gaming wall during a drag, with Valorant or Roblox running in the background and no change in behaviour, and a Mac-to-PC copy finishing during a game, with the pill held back until Alt+Tab.
- [ ] **Step 3:** Delete the spike examples. Update `HANDOFF.md`: what shipped, exception E1, the temp folders and their cleanup, the pacer constants and where to tune them, and the next step (the audio sharing plan, `docs/superpowers/specs/2026-10-25-audio-sharing-design.md`).
- [ ] **Step 4:** Run `cargo test --workspace` (on each machine) and clippy. Expected: PASS.
- [ ] **Step 5:** Commit `docs: drag-and-drop acceptance and handoff`. Push.
