# CursorJump — Design

Date: 2026-10-18
Status: Draft for review

## Intent

The user owns a Windows gaming PC (wired LAN, two monitors, their main mouse and keyboard) and a MacBook Pro M4 Pro on macOS Tahoe (Wi-Fi), which they prefer for work. Because the PC desk setup is more comfortable, the Mac has gone unused, even though their work is now split across both machines.

CursorJump makes the MacBook behave like a **third monitor of the PC**: the PC's mouse and keyboard glide onto the Mac screen and control it, and copy/paste works across both. Later, files can be dragged across the screen edge.

It is a personal, from-scratch recreation of the *kind* of tool CursorHop is. It must not use, decompile, or copy CursorHop's code. It is built only from public OS APIs and open protocols. It is for personal use only and will not be sold.

## The bar for success

**"If it isn't seamless, it gets binned instantly."** It must feel like a genuine third screen:

- No perceptible lag or stutter on the Mac cursor; motion as smooth as on the PC monitors.
- Mouse speed and scrolling on the Mac feel the same as on the PC (no floatiness or sluggishness).
- Keys and shortcuts just work: no stuck keys, no doubled keys, Ctrl-shortcuts act as Cmd-shortcuts.
- It looks after itself: auto-starts, auto-reconnects, never strands the cursor.
- Gaming on the PC is never disturbed.
- File transfers may take a moment; nothing else may.

## Goals

### Phase 1: mouse, keyboard, and clipboard (build first)

1. **Mouse handoff.** Pushing the cursor off the right edge of the PC's main (center) monitor moves it onto the Mac.
2. **Keyboard follows the cursor.** While the cursor is on the Mac, all PC keyboard input goes to the Mac and none reaches Windows.
3. **Copy/paste both ways.** Plain text, formatted text (rich text/HTML), and images.
4. **Encrypted, paired connection.** All traffic is encrypted. A one-time pairing confirms a short code on both screens so that no other device on the network can connect.
5. **Zero-config discovery.** The machines find each other automatically on the local network. No IP addresses.
6. **Set and forget.** Starts at login on both machines, connects by itself, and reconnects quietly after sleep or Wi-Fi blips.
7. **Small UI.** A Windows tray icon and a macOS menu-bar icon showing status and a live latency readout. Each gives access to a small settings window.

### Phase 2: file drag-and-drop (after phase 1 is solid)

8. **Drag files across the edge**, both directions (PC to Mac and Mac to PC), of any size, directly over the LAN with no cloud and no size cap. A short delay on drop is acceptable. Phase 1 must be architected so this slots in without rework, with a bulk-data channel separate from the input path.

## Non-goals

- Moving running programs or windows between machines.
- Controlling the PC from the Mac's own keyboard and trackpad. The Mac's built-in inputs keep working normally, **on the Mac only**, as a backup.
- Linux, or more than these two machines.
- A full drag-the-screens layout editor (v1 uses a simple side and offset setting).
- Selling, distributing, licensing, or monetizing it.

## Anti-goals (would make it a failure even if it technically works)

- Any added latency, jitter, or stutter in cursor movement.
- Mac mouse feel that differs noticeably from the PC.
- Stuck modifier keys, or keystrokes leaking to Windows while on the Mac.
- The cursor jumping to the Mac in the middle of a game.
- Any input lag or interference in PC games while the cursor is on the PC.
- Needing manual reconnection or fiddling during normal daily use.

## Layout and edge behaviour (decided with the user)

Physical desk: **[Vertical/portrait monitor] [Main landscape monitor] [MacBook]**, left to right.

- **The two PC monitors are untouched.** Windows keeps handling them exactly as it does now.
- **Entry:** the **entire right edge of the main monitor** leads to the Mac, mapped **proportionally**. Leaving a third of the way down the PC edge lands a third of the way down the Mac's left edge. (The user chose this over true-size or physical mapping because the laptop moves around on the desk.)
- **Return:** the **entire left edge of the Mac** leads back to the main monitor's right edge, proportionally.
- The Mac's other three edges are walls.
- **Settings:** which side the Mac is on (default right; any side of any PC monitor is allowed), a vertical offset/fine-tune slider, a pointer-speed multiplier, and the Ctrl/Cmd swap toggle.

## Gaming rule (decided with the user)

"Behave exactly like a third monitor would." The Mac edge acts as a **wall** whenever any of the following is true:

- A game (or any app) has **clipped/locked the cursor** (`GetClipCursor` returns less than the full virtual screen). This is the same mechanism that already stops the cursor reaching the second monitor in-game.
- The **cursor is hidden** (`GetCursorInfo` reports not showing). Mouse-look games hide the cursor and recenter it, and desktop use never hides it at the moment of crossing.
- An **exclusive full-screen D3D app** is running (`SHQueryUserNotificationState` == `QUNS_RUNNING_D3D_FULL_SCREEN`).

After Alt+Tab to the desktop, the lock or hidden cursor goes away and crossing works again.

## Anti-cheat safety (decided)

The user plays competitive games with kernel anti-cheat, including **Valorant (Riot Vanguard)** and **Roblox (Hyperion)**, and possibly others. An account ban is the worst possible outcome, so the PC side must be invisible to games:

- **No global input hooks while the cursor is on the PC.** Edge detection uses only passive, non-intrusive means, such as cursor-position polling and Raw Input, which ordinary apps use all the time. Low-level hooks are installed only while the cursor is on the Mac, to stop input from reaching Windows, and removed the moment it returns. Because the gaming rule makes the edge a wall in-game, hooks are never installed while a game has the mouse.
- **The PC side never injects input into Windows.** No `SendInput`. The only exception is positioning the cursor when it returns from the Mac (`SetCursorPos`), and that never happens while a game is focused.
- **No DLL injection, overlays, or reading other processes' memory.** Only the documented window and cursor queries in the gaming rule.
- Acceptance testing includes playing Valorant and Roblox with CursorJump running, and checking that crossing is blocked in-game and works after Alt+Tab.

## Keyboard mapping

- Keys are translated by physical key (scancode to macOS virtual keycode), not by character. The Mac's own keyboard layout setting governs characters.
- The PC keyboard is **US (ANSI)**, per the user, who said "I believe", so confirm it during setup. The Mac should use the US input source. The mapping table and key tests target US ANSI.
- Default modifier mapping *(builder's call, adjustable)*: PC **Ctrl → Cmd**, PC **Win → Control**, **Alt → Option**. Ctrl+C/V/Z/Tab etc. therefore behave like Cmd+C/V/Z/Tab.
- **On every crossing (in either direction) and on disconnect, release all held keys and mouse buttons** on the side being left, so nothing sticks.
- System combinations that Windows reserves (Ctrl+Alt+Del, Win+L) cannot be captured and will act on the PC. This is an accepted limitation.

## Daily life (decided with the user)

- Auto-start at login on both machines; auto-discover; auto-connect.
- First connection only: both screens show the same short code and the user confirms it on each. Keys are then remembered permanently.
- If the Mac sleeps, closes its lid, or drops off the network while the cursor is on it, the **cursor snaps back to the PC immediately**, within well under a second, detected by heartbeat timeout.
- **Mac asleep or locked:** pushing the cursor onto the Mac when its display is asleep wakes it, like a real monitor. While connected, the Mac app keeps the system awake (the display may still sleep) so that it can respond. If the whole Mac is truly asleep (lid closed or system sleep), the edge is a wall. **The PC keyboard and mouse must work at the Mac lock screen and login window**, so the user can type their Mac password from the PC. That requires a small privileged helper on the Mac (e.g. a LaunchDaemon) in addition to the menu-bar app. The builder designs that helper securely: it accepts input only from the paired and authenticated session.
- An emergency hotkey on the PC also forces the cursor home *(builder picks the combo; it must be unlikely to collide with games)*.
- A pause toggle in the tray disables crossing.

## Constraints

- **Hardware/OS:** Windows 10/11 PC on wired gigabit LAN. MacBook Pro M4 Pro (Apple silicon) on macOS Tahoe (26), on Wi-Fi. **Both machines are personally owned and unmanaged** (no MDM, corporate VPN or endpoint security), so self-signed builds, local-network access and user-granted permissions are all available.
- **Discovery fallback (Claude's call):** mDNS first. If the router filters multicast, fall back to UDP broadcast on the local subnet, still with no IP typing. A manually entered address is a hidden last resort only.
- **Network:** the Mac is on Wi-Fi. Phase 1 must feel smooth on Wi-Fi. The design must actively counter Wi-Fi jitter, notably macOS AWDL background scanning, which causes periodic latency spikes. The latency readout exists so the user can tell whether a USB-C Ethernet adapter is worth buying. That purchase is a fallback, not a requirement.
- **Mac permissions:** the Mac side needs Accessibility (and any input or event-posting permissions Tahoe requires). macOS ties these grants to the app's code signature. The dev build must use a stable signing identity (e.g. a self-signed certificate) so the permissions survive rebuilds.
- **Windows privilege:** runs as a normal user process by default. When an elevated (admin) window or UAC prompt has focus, crossing may not work. That's acceptable for v1, but the app must fail safe and must never strand the cursor or swallow input.
- **Hook discipline:** any Windows low-level input hook must return near-instantly and do no work on the hook thread, because games run while it is installed. If Windows silently drops the hook (LowLevelHooksTimeout), the app must detect and recover.
- **Clean-room:** no CursorHop binaries, code, or decompilation. Public docs and open-source libraries only, with license-compatible dependencies.

## How it gets made (Claude's calls, not user decisions)

- **Language:** Rust, one codebase, two roles. The Windows build is the "server" (owns physical input). The macOS build is the "client" (receives and injects).
- **Repository:** private GitHub repo. Claude Code on Windows builds and tests the Windows side. The user also runs Claude Code on the Mac, which pulls, builds, and tests the macOS side there. Mac-specific behaviour can only be verified on the Mac.
- **Suggested building blocks** *(builder may substitute)*: Win32 low-level hooks plus Raw Input on Windows; CoreGraphics event posting (`CGEventPost`) on macOS; the Noise protocol (`snow`) for encryption and pairing; mDNS (`mdns-sd`) for discovery; native clipboard APIs (Windows clipboard listener, `NSPasteboard`); and a lightweight tray/menu-bar UI.

## Left to the builder

- Transport details: UDP vs. QUIC vs. TCP for the input path, packet format, event coalescing rate, and the separate reliable channel for clipboard and files.
- Exactly how pointer movement is captured on the PC while on the Mac and how acceleration is matched so it feels identical (the requirement is "same feel"; the method is open), plus how high-resolution and smooth scrolling are reproduced on the Mac.
- How clipboard changes are detected and transferred (eager vs. on-demand), and size caps for clipboard images.
- Settings window design, tray and menu-bar visuals, the emergency hotkey, and the default pointer-speed calibration.
- Auto-start mechanism on each OS.
- How AWDL and Wi-Fi jitter are mitigated.
- Phase 2 file drag-and-drop mechanics (OLE drag detection on Windows, file-promise drags on macOS, transfer channel). Phase 2 gets its own design pass once phase 1 is solid.
- Test strategy, including a way to measure end-to-end cursor latency and jitter.
