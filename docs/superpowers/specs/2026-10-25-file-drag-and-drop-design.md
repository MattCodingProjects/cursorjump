# CursorJump — File drag-and-drop design

Date: 2026-10-25
Status: Draft for review

Builds on the phase 1 design (`2026-10-18-cursorjump-design.md`). Everything there still holds, especially the anti-cheat rules, the gaming rule and "seamless or binned". This is the phase 2 feature that design deferred. Audio sharing is a separate design (`2026-10-25-audio-sharing-design.md`) and gets built after this one.

## Intent

Phase 1 is finished, and the user is very happy with it. The PC's mouse and keyboard already glide onto the Mac, and copy/paste works both ways. The user now wants the two machines to feel like **one unified device** for files too. Grab a file on one screen, drag it across the edge, and drop it anywhere on the other machine. It lands exactly where it was dropped, as if it had always been on that machine.

The user's own examples: images, general files and potentially large ones, dragged **to and from WhatsApp, Google (web pages in the browser), and desktop to desktop**.

**The named targets (decided with the user):**
- the **WhatsApp desktop app**;
- **Gmail**, in the browser;
- **Discord**;
- folders, Downloads and the desktop, on both machines.

## Goals (decided with the user)

1. **Both directions.** PC to Mac, and Mac to PC.
2. **Drops land straight in the target.** Dropping on a WhatsApp chat attaches the file to that chat. Dropping on a Gmail message or a Discord channel attaches it there. Dropping on a folder, Downloads or the desktop puts the file there. There is no "it lands in a folder first and you drag it in again".
3. **Any size.** Files go directly over the existing encrypted LAN connection, with no cloud and no size cap. A short wait after letting go is fine.
4. **Several files or a whole folder** can be dragged at once and behave the same way. A folder arrives as a folder with its contents.
5. **Drags out of apps work, not just drags of files already on disk.** Dragging a photo out of a WhatsApp chat, or an image out of a web page or a Discord channel, onto the other machine must work. Those sources often hand over the file's contents rather than a path on disk.
6. **The drag looks normal.** While dragging across the edge, the file's icon follows the cursor onto the other screen, just like a local drag.
7. **Progress pill.** Anything that takes more than a moment shows a progress pill (spec below).
8. **Clean failure.** If a copy fails partway (Wi-Fi drops, the Mac sleeps, the PC disconnects), the pill shows a clear failed state and no half-copied file is left behind at the destination.

## The progress pill (decided with the user)

Chosen from visual mockups: a minimal, almost black-and-white "glass pill". The final mockup is kept at `.superpowers/brainstorm/556-1792960514/content/transfer-pill-mono.html` (the "Dark glass" option). That folder is git-ignored and exists only on the PC, so the spec below is the source of truth.

- **Where:** on the machine receiving the file, floating at the bottom centre of the screen where the drop happened. It sits above other windows but never takes focus or blocks clicks underneath it beyond its own outline.
- **Look:** a smoky dark glass pill (about 62 % opaque near-black, blurred background, a 1 px faint white border and a soft shadow), with white text. It's monochrome: no colour accents.
- **Contents, left to right:** a small rounded square icon for the file type (or a count, for example "3", for several files); the file name in semibold, truncated with an ellipsis; the percentage on the right. Under that is a thin (about 3 px) white progress bar on a faint track, with a slow moving sheen. Under the bar, in small dimmed text: "From PC · 2.1 of 5.0 GB · 1 min left" (or "From Mac").
- **Several files:** one pill for the whole drag ("3 files"), not one per file. Separate drags in progress at once stack vertically.
- **Done:** a ✓ pops in, then the pill fades out after about a second.
- **Failed:** the pill stays until dismissed and says what happened in plain words, for example "Copy failed — Mac went to sleep".
- **Tiny files:** if a drag finishes almost instantly, no pill appears at all. The threshold is the builder's call, roughly "would a pill flash and vanish?".
- *(Claude's call)* **Cancel:** a small ✕ appears on the pill on hover. Clicking it cancels the copy and cleans up like a failure.
- The same pill design is used on both Windows and macOS.

## Non-goals

- Dragging non-file content such as selected text or links. Those already travel by copy/paste. *(Claude's call)*
- Copying files with Ctrl+C and pasting them on the other machine. It's not requested. The transfer engine should not rule it out later. *(Claude's call)*
- Syncing, shared folders, or sending files without a drag.
- Any machine other than these two.

## Anti-goals (would make it a failure even if it technically works)

- **Any cursor lag or stutter during a drag or during a transfer.** A 5 GB copy in progress must not make the Mac cursor feel any different. The input path keeps strict priority over file data.
- Drops that only work into Finder/Explorer but not into apps (WhatsApp, Gmail, Discord). The user named those apps specifically.
- A stuck drag: the cursor or a mouse button stays "held" on either machine after the drag ends, is cancelled or fails.
- Half-copied or corrupt files left behind.
- Anything that weakens the phase 1 anti-cheat guarantees on the PC (see Constraints).

## Constraints

- **Anti-cheat (from phase 1, still binding).** No global input hooks while the cursor is on the PC, and no DLL injection, overlays or reading other processes' memory. The PC side never injects input (`SendInput`), except `SetCursorPos` on return. The user plays Valorant and Roblox with kernel anti-cheat, and an account ban is the worst possible outcome. If drag-and-drop on the PC truly cannot work without some injection (for example, to finish or cancel a drag that Explorer started), the builder must:
  - keep it to the moments a file drag is actually crossing the edge;
  - never do it while a game is focused or the gaming rule says "wall";
  - write the exception down explicitly in the plan, so the user can see it.
- **CursorJump's own windows on the PC** *(Claude's call)*. The phase 1 "no overlays" rule means injected in-game overlays. CursorJump may show its own ordinary top-level windows, as any app does, for two things: an invisible drop-target window at the edge to catch a drag leaving the PC, and the progress pill. Neither may appear while the gaming rule says "wall". A pill for a Mac-to-PC copy that finishes or fails while a game holds the mouse is held back until the game no longer does.
- **Crossing with a button held.** In phase 1, crossing while a mouse button is held is a deferred check (`phase2-followups.md`). A file drag is exactly that case, so this feature must make it solid. The gaming rule still applies: a drag can never carry the cursor across while a game holds the mouse.
- **Transport.** Use the existing QUIC session (`crates/cj-net/src/session.rs`), which already carries input datagrams, a control stream, and one unidirectional stream per clipboard item. File data goes over streams, never over the input path, and must not delay input datagrams. The current stream limit (`MAX_UNI_STREAMS = 4`) and connection flow-control window may need raising for large files.
- **Protocol changes:** bump `PROTOCOL_VERSION` and reinstall both machines together, as in phase 1.
- **Mac.** Must work on macOS Tahoe with the existing signed app ("CursorJump Dev") and its existing permissions where possible. If a new permission is needed, the "Mac not ready" wall from phase 1 should explain it.
- **Disk.** If the destination doesn't have room for the file, fail cleanly before or as soon as that's known, with a plain message in the pill.

## How it gets made (Claude's calls)

- Built into the existing Rust app and crates on both machines. No new apps or services.
- **Spike first.** The riskiest part is whether each named drop target accepts the drop the way the feature delivers it. Some apps only accept a file that's already fully on disk, and others accept "the file is on its way" (macOS file promises, Windows virtual files). Before the full build, a throwaway spike should drop a test file from a fake remote source into each target on each OS, and record which mechanism each one accepts. The spike's findings decide the mechanism per target. The targets are:
  - the WhatsApp desktop app;
  - Gmail in the browser;
  - Discord;
  - Finder and the Mac desktop;
  - Explorer, Downloads and the Windows desktop.

  The user didn't name a browser, so the spike uses each machine's default browser. Check it at the start of the spike, and ask the user if it's unclear. Discord is assumed to be the desktop app; confirm that the same way. *(Claude's call)*
- **Waiting for big files (decided with the user).** For targets that only accept a finished file on disk, the drop waits until the file has fully arrived, at any size. The target app may look busy or "Not responding" meanwhile, and the pill shows progress the whole time. There's no size limit above which files go to Downloads instead. A wait is preferred over a different landing place.

## Left to the builder

- How a drag leaving the PC is detected and its file list or contents obtained without breaking the anti-cheat rules, and how a drag leaving the Mac is detected.
- The drag mechanisms on each OS (for example `NSDraggingSession` with file promises or URLs on the Mac; OLE `IDataObject` with `CF_HDROP` or virtual file streams on Windows), chosen per the spike.
- Name collisions at the destination: follow what the receiving OS normally does for a local drag.
- Whether received files on the Mac carry the quarantine flag. Recommended: treat them like a local copy, but decide and note it.
- Transfer framing, chunking, integrity checks, and temp-file-then-rename to guarantee "no half-copied files".
- The "tiny file" threshold, pill animation timings, and exact sizes.
- Test strategy. Acceptance should include, in both directions:
  - a 5 GB file with the cursor feeling unchanged during the copy;
  - several files, and a folder;
  - a drop into WhatsApp, into Gmail, into Discord, into Downloads and onto the desktop;
  - a drag out of WhatsApp, out of Discord and out of a web page;
  - a Wi-Fi drop mid-copy;
  - cancel;
  - Valorant or Roblox running in the background with no change in behaviour.
