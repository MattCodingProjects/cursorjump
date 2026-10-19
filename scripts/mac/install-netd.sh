#!/usr/bin/env bash
# Installs cursorjump-netd, the root helper behind CursorJump's "Reduce
# Wi-Fi hiccups" setting: while the PC is connected, it holds AWDL (the
# interface AirDrop uses) down, because its channel hopping causes latency
# spikes on Wi-Fi.
#
#   ./scripts/mac/install-netd.sh              # build, sign, install, start
#   ./scripts/mac/install-netd.sh --uninstall  # stop, remove, AWDL back up
#
# Run it as yourself, not with sudo: it uses sudo (and asks for your
# password) only for the steps that need root, and lets only your user talk
# to the helper. Signs with the "CursorJump Dev" identity like bundle.sh;
# CJ_SIGN_IDENTITY overrides it (- signs ad hoc).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
label=com.cursorjump.netd
helper="/Library/PrivilegedHelperTools/$label"
plist="/Library/LaunchDaemons/$label.plist"
socket=/var/run/cursorjump-netd.sock
log=/var/log/cursorjump-netd.log
target=aarch64-apple-darwin
identity="${CJ_SIGN_IDENTITY:-CursorJump Dev}"

usage() {
    echo "usage: $0 [--uninstall]" >&2
    exit 2
}
mode=install
[[ $# -le 1 ]] || usage
case "${1-}" in
    "") ;;
    --uninstall) mode=uninstall ;;
    *) usage ;;
esac

uid="$(id -u)"
if [[ "$uid" -eq 0 ]]; then
    echo "Run this as yourself, without sudo: it asks for your password when it needs it." >&2
    exit 1
fi

# Stops the daemon if it is loaded; it brings AWDL back up as it stops.
stop_daemon() {
    sudo launchctl bootout "system/$label" >/dev/null 2>&1 || true
    for _ in $(seq 1 25); do
        sudo launchctl print "system/$label" >/dev/null 2>&1 || return 0
        sleep 0.2
    done
    echo "warning: $label is still loaded after bootout" >&2
}

if [[ "$mode" == uninstall ]]; then
    cat <<MSG
About to, with sudo (it may ask for your password):
  - stop the $label LaunchDaemon (launchctl bootout system/$label)
  - remove $plist and $helper
  - bring awdl0 back up (ifconfig awdl0 up)
MSG
    stop_daemon
    sudo rm -f "$plist" "$helper" "$socket"
    sudo ifconfig awdl0 up
    echo "Uninstalled the Wi-Fi helper. Its log stays at $log."
    exit 0
fi

if [[ "$identity" != "-" ]]; then
    identities="$(security find-identity -v -p codesigning)"
    if ! grep -qF "\"$identity\"" <<<"$identities"; then
        echo "No code-signing identity \"$identity\" found." >&2
        echo "Run scripts/mac/make-signing-cert.sh first (or set CJ_SIGN_IDENTITY=- to sign ad hoc)." >&2
        exit 1
    fi
fi

cd "$repo"
echo "Building cursorjump-netd (release, $target)..."
cargo build --release -p cursorjump-netd --target "$target"
built="$repo/target/$target/release/cursorjump-netd"
codesign --force --options runtime --timestamp=none --identifier "$label" --sign "$identity" "$built"
codesign --verify --strict "$built"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
sed "s/__ALLOWED_UID__/$uid/g" "$here/$label.plist" >"$work/$label.plist"
plutil -lint -s "$work/$label.plist"

cat <<MSG

About to, with sudo (it may ask for your password):
  - stop $label if it is running
  - install $helper (root:wheel 0755)
  - install $plist (root:wheel 0644), letting only uid $uid ($(id -un)) use the helper
  - start it: launchctl bootstrap system $plist
MSG
stop_daemon
sudo install -d -o root -g wheel -m 0755 /Library/PrivilegedHelperTools
sudo install -S -o root -g wheel -m 0755 "$built" "$helper"
sudo install -S -o root -g wheel -m 0644 "$work/$label.plist" "$plist"
sudo launchctl bootstrap system "$plist"

for _ in $(seq 1 25); do
    [[ -S "$socket" ]] && break
    sleep 0.2
done
if [[ ! -S "$socket" ]]; then
    echo "The helper was started, but $socket did not appear; see $log" >&2
    exit 1
fi

cat <<MSG

The Wi-Fi helper is installed and running, and starts with the Mac.
Turn on "Reduce Wi-Fi hiccups" in CursorJump's settings: AirDrop then pauses
while the PC is connected.

Helper:    $helper
Daemon:    $plist
Log:       $log
Uninstall: $0 --uninstall
MSG
