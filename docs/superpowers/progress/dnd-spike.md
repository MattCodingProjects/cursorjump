# Drag-and-drop spike findings (Task 1a, PC)

Date: 2026-10-26. Runs used `app/cursorjump/examples/dnd_spike_win.rs` (764c550). The user ran each experiment by hand on the PC, and the logs are in `%TEMP%\cursorjump-spike\`. Task 1b (Mac) is still to come, and runs at the start of the Mac session before Task 14.

## Verdicts

| # | Assumption | Verdict |
|---|---|---|
| A4 | Windows targets accept our `IDataObject`, in some format order | **Holds, with CF_HDROP alone.** Desktop (Explorer), the WhatsApp desktop app and Gmail all accepted a 5 MB `CF_HDROP` drop, and all of them survived a 60 s block in the after-drop `GetData`. Discord was not tried (the user had no time), and is assumed to match WhatsApp, which is also Chromium-based. Check it at acceptance. Virtual files (FILEGROUPDESCRIPTORW/FILECONTENTS) as a *source* were not needed and were not tested. |
| A5 | `DoDragDrop` runs while Windows thinks the left button is up, ending on our raw-input-driven `QueryContinueDrag` | **Fails as written.** See "A5 in detail". It is replaced by Ruling R11 (the button press lands on our own window). |
| A6 | A 1/255-alpha topmost strip gets `DragEnter`; parked full-screen, the real release ends the drag with `Drop` (accept) or without (reject); FILECONTENTS can be read inside `Drop` | **Holds.** See "Catcher results". |

## Catcher results (A6)

- **Explorer:** the strip got `DragEnter`. Explorer offers `CF_HDROP` with the real path, plus FILEGROUPDESCRIPTORW and FILECONTENTS as an `IStream`. Reading FILECONTENTS inside `Drop` took 1–11 ms to the first byte, at 24–130 MB/s.
- **Discord desktop:** no `CF_HDROP`. It offers one FILEGROUPDESCRIPTORW item named `image.png.url`, with no size, and FILECONTENTS as an HGLOBAL. That is a 218-byte `.url` shortcut, alongside UniformResourceLocatorW, text/x-moz-url and HTML. In other words, it hands over **a link, not the image bytes**, and the user said that was expected. Ruling R10 covers it: the sender downloads the link's target and sends that.
- **WhatsApp desktop and browser drag-*outs*:** not tried. Check them at acceptance.
- **`--park` (accept):** after `DragEnter` the strip grew to the full virtual screen. A release mid-screen at (1507, 629) gave `Drop`, the strip shrank back, and FILECONTENTS was read. Nothing dimmed or flickered. The source's drag image ("holding an attachment") stayed visible over the parked alpha-1 window **without** `IDropTargetHelper`.
- **`--park --reject`:** `DragEnter` returned NONE. The user saw a red "no" cursor all the way back. The release gave `DragLeave` with no `Drop`, and the drag ended cleanly with nothing moved.

## A5 in detail (spike source → Windows targets)

| Run | Start state | Result |
|---|---|---|
| 5a | F8, no button held, background process | `EnumFormatEtc` once, then nothing. There was no `QueryContinueDrag` and no `GiveFeedback`, and two real clicks did not drop. The modal loop stalls. |
| 5b/5c | F8 with `--fg` (our small window takes the foreground first) | Nothing while moving: 1 `QueryContinueDrag` in 7.7 s, no `DragOver`, and no icon follows (seen by the user). A click wakes it: the down gives hover, and the real up drops 11–185 ms later through the Raw Input flag. The file landed. A 60 s wait on the desktop: no freeze, and the file landed. |
| 5d | Left button held on the **desktop**, then F8 | It still gets no mouse input. Even the real Windows up did not wake the loop, so effect=NONE. |
| 5e | Left button pressed and held on **our own window**, then F8 | **Normal drag.** The icon follows the cursor, with about 230 `QueryContinueDrag` calls per second and continuous `GiveFeedback`. The drop happens on the real release, effect=COPY, and the file landed. |

Cause: OLE's drag loop only gets mouse input through our thread's capture. Capture without a press over one of our windows only works while the pointer is over our window. So the user's real left-down must have gone to a CursorJump window before `DoDragDrop` starts.

## Ruling R11 (replaces A5)

While the cursor is on the Mac, the PC cursor is parked over the CursorJump catcher window, which is `WS_EX_NOACTIVATE` and topmost. The low-level hook lets the user's **real** left-button down and up pass through to Windows only when `WindowFromPoint(park point)` is that catcher window. Otherwise they are swallowed as today, and a Mac→PC drag falls back to dropping on release with no hover feedback.

The catcher's thread then holds the implicit capture. When a Mac drag comes back to the PC, that thread calls `DoDragDrop` while the button is truly down, which is the normal OLE source state.

Nothing is injected. The hook only swallows less, and is still only installed while the cursor is on the Mac. None of this happens while gaming, because crossing is walled then.

- Affects Task 11 (`DndCmd::Park` puts the catcher under the park point) and the hook pass-through rule, with a unit test that a press is never passed when the window under the park point isn't ours.
- Affects Task 12: the source calls `DoDragDrop` on the catcher's thread.
- Affects Task 13: the wiring.
- Still to check in the smoke test: that a press passed to a `WS_EX_NOACTIVATE` window still gives the capture without taking the foreground. The spike's window was activated by the press.

## Format order and target behaviour (for Task 12)

- Offer **`CF_HDROP` plus `CursorJump.Self`** only. All tested targets took `CF_HDROP`.
- Chromium targets (WhatsApp, Gmail in the browser) call `GetData(CF_HDROP)` about **every 60 ms while hovering**, and also ask for `chromium/x-file-system-files` and `Chromium Web Custom MIME Data Format`. So the hover-time `CF_HDROP` must answer **at once** with a path. Create the destination path (an empty placeholder in the incoming directory) when the drag enters, and block only in the after-drop `GetData` until the file is complete.
- After the drop, `DoDragDrop` returns COPY in about 17 ms, while the target's cross-process `GetData` is still blocked on us. The data object must stay alive and keep serving until that call returns. Do not tear it down when `DoDragDrop` returns.
- During the wait, **WhatsApp and Gmail froze**, with the browser window not responding, for the whole wait. Then both recovered and attached the file. The desktop did not freeze. This matches the spec's "waiting (a)".
- The default browser is Chromium-based: the Gmail drop showed the same Chromium formats.
- The drag image (`--image`) was not working in the spike, because our `SetData` refused DragContext with `E_NOTIMPL`. Task 12's `IDataObject` must accept arbitrary `SetData` (store it and return it) so that `IDragSourceHelper` can attach an image.

---

# Task 1b (Mac) findings

Date: 2026-10-26, on the MacBook (macOS Tahoe 26). Runs used `app/cursorjump/examples/dnd_spike_mac.rs`. Every drag was driven by posted CGEvents made like `inject.rs` (R45), with the user pointing at the target or source during a countdown. Logs are in `$TMPDIR/cursorjump-spike/`. The default browser is Google Chrome. Discord is not installed on the Mac, so the user chose to skip it there. No new permission prompt appeared: the terminal's Accessibility grant was enough to post events.

## Verdicts

| # | Assumption | Verdict |
|---|---|---|
| A1 | Mac targets accept `NSFilePromiseProvider` drops | **Holds for Finder, the Desktop, a Finder folder (Downloads behaves the same) and WhatsApp. Fails for Chrome (Gmail).** Finder asked for the write 15 ms after the release. WhatsApp asked 35 ms after the release, into its own container tmp, then showed its own "transferring" popup through a 30 s wait and attached the photo. Chrome refused: the operation was None and no drop zone appeared. |
| A2 | A lazy `public.file-url` item can block about 60 s at drop time | **Fails.** The data provider is called once, about 75 ms after the drag starts (the system fills the drag pasteboard up front), and is never asked again. Gmail attached the empty placeholder and said "This file is 0 bytes". **What works instead:** hold the posted LeftMouseUp (the button stays down, with a ±1 pt jiggle every 500 ms) until the real bytes have been renamed over the placeholder, then post the up. Gmail then attached the full 4.8 MB photo after an 11 s hold. Chrome keeps the path, not the hover-time inode. |
| A3 | An `NSDraggingSession` starts from a posted LeftMouseDown on our panel, follows posted drags, drops on a posted up, and Esc cancels it | **Holds.** An 8×8 pt borderless non-activating panel at alpha 0.01, with `acceptsFirstMouse:` YES and a 2 pt inset from the left edge, received the posted down. `beginDraggingSessionWithItems:event:source:` inside `mouseDown:` followed every posted drag (180+ moves) with another app frontmost, and dropped on the posted up. A posted Esc key (kVK_Escape, frontmost app Finder) ended the session with None 22 ms later, and nothing was dropped. |
| A7 | A near-transparent edge panel gets drags, and a posted up over it runs `performDragOperation` | **Holds for drags the source starts at once (Finder): `draggingEntered` came before the release, and perform came 4.6 ms after the posted up.** WhatsApp and Chrome start their drags late (see F1). Their drags reached the strip only right after the posted up, but perform still ran and the files arrived. WhatsApp gave a `public.file-url` in its container tmp (Ready). Chrome gave a file promise whose `fileNames` is **nil** before the drop: the generated binding panics, so read it with a nullable `msg_send`. `receivePromisedFiles` delivered `images.jpg` in about 275 ms. Real trackpad drags from Chrome reach the strip normally before the release. |

## Mixed items: promise plus file URL in one dragging item

An `NSFilePromiseProvider` subclass that also writes `public.file-url` (in `writableTypesForPasteboard:` and `pasteboardPropertyListForType:`):
- **Finder** and **WhatsApp** still used the **promise**: `writePromiseToURL` was called, and the URL was ignored.
- **Gmail** used the **URL**. With the release held until the bytes were complete, it attached the photo.

So one item kind serves every tested target.

## F1: Chrome doesn't follow posted drags (open; investigation continues)

Chrome starts a renderer-initiated drag late, after an IPC round trip. It calls `beginDraggingSession` with a synthesized LeftMouseDragged event, which has event number 0 and the current event's timestamp. Run 19's drag-pasteboard probe shows Chrome set up its session about 240 ms into the posted drag. The system did not let that session follow our later posted drags: it came alive only at the posted up and dropped at once where the cursor was.

- Changes that did not help: pressure 1.0; a unique per-press `kCGMouseEventNumber` (Deskflow's recipe); posting at the session tap.
- The button state the system reports (`pressedMouseButtons`, HID and combined) is "down" throughout.
- A late session that is never ended leaves Chrome's tab refusing clicks until a reload. After the bad run, image drags in Chrome with the PC mouse (the installed phase 1 injector) didn't start at all, even after relaunching Chrome. The MacBook trackpad works, and Safari works with the PC mouse.
- Research notes (Chromium's `startDragWithDropData`, the Deskflow and Chrome Remote Desktop event-number fixes) are summarised in the ledger under F1.

To be investigated autonomously while the user is away (a Chrome test page, posted drags, field comparisons). Until then, Task 16 handles late drags at push-out (R48).
