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
