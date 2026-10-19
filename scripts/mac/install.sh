#!/usr/bin/env bash
# Builds and signs CursorJump, installs it to /Applications, turns on start
# at login and starts it.
#
#   ./scripts/mac/install.sh
#
# Run scripts/mac/make-signing-cert.sh once first. Settings and the device
# identity stay in ~/Library/Application Support/CursorJump and survive
# reinstalling.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
dest=/Applications/CursorJump.app
exe="$dest/Contents/MacOS/cursorjump"
label=com.cursorjump.agent

"$here/bundle.sh"

# Stop an installed copy that is running (whether launchd or the user started it).
if launchctl print "gui/$UID/$label" >/dev/null 2>&1 || pgrep -f "$exe" >/dev/null 2>&1; then
    echo "Stopping the running CursorJump..."
fi
launchctl bootout "gui/$UID/$label" >/dev/null 2>&1 || true
pkill -f "$exe" >/dev/null 2>&1 || true
for _ in 1 2 3 4 5 6 7 8 9 10; do
    pgrep -f "$exe" >/dev/null 2>&1 || break
    sleep 0.5
done

# Replace the whole bundle so no stale file (or a file a process still maps) remains.
rm -rf "$dest"
ditto "$repo/target/CursorJump.app" "$dest"
echo "Installed $dest"

# Writes the LaunchAgent and loads it; RunAtLoad starts CursorJump.
"$exe" --enable-autostart

cat <<MSG

CursorJump is installed, starts at login, and is running: look for its icon
in the menu bar.

macOS asks for two permissions; allow both:
  - Local Network, so CursorJump can find and reach your PC.
  - Accessibility (System Settings > Privacy & Security > Accessibility),
    so the PC's keyboard and mouse can control this Mac.

Settings and pairing: ~/Library/Application Support/CursorJump/
Start at login:       ~/Library/LaunchAgents/$label.plist
Logs:                 ~/Library/Application Support/CursorJump/cursorjump.log
MSG
