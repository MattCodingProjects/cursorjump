#!/usr/bin/env bash
# Installs CursorJump's audio driver, a Core Audio HAL plug-in. It adds the
# "CursorJump Microphone" input, which carries the PC's microphone to Mac
# apps, and a hidden "CursorJump Mic Feed" output that the CursorJump app
# plays the PC's mic into.
#
#   ./scripts/mac/install-audio-driver.sh              # build, sign, install
#   ./scripts/mac/install-audio-driver.sh --uninstall  # remove it
#   ./scripts/mac/install-audio-driver.sh --dry-run [--uninstall]
#                                  # print every command, run none of them
#
# Run it as yourself, not with sudo: it uses sudo (and asks for your
# password) only to copy the driver into /Library/Audio/Plug-Ins/HAL (or
# remove it) and to restart coreaudiod, which loads drivers only as it
# starts. Restarting coreaudiod briefly interrupts all sound on the Mac.
# Signs with the "CursorJump Dev" identity like bundle.sh; CJ_SIGN_IDENTITY
# overrides it (- signs ad hoc). Running it again reinstalls; every step is
# safe to repeat.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
name=CursorJumpAudio.driver
built="$repo/target/audio-driver/$name"
hal=/Library/Audio/Plug-Ins/HAL
installed="$hal/$name"
mic_name="CursorJump Microphone"

usage() {
    echo "usage: $0 [--dry-run] [--uninstall]" >&2
    exit 2
}
mode=install
dry_run=0
for arg in "$@"; do
    case "$arg" in
        --uninstall) mode=uninstall ;;
        --dry-run) dry_run=1 ;;
        *) usage ;;
    esac
done

if [[ "$(id -u)" -eq 0 ]]; then
    echo "Run this as yourself, without sudo: it asks for your password when it needs it." >&2
    exit 1
fi

# Runs a command, or with --dry-run only prints it.
run() {
    if [[ "$dry_run" -eq 1 ]]; then
        printf 'would run:'
        printf ' %q' "$@"
        printf '\n'
    else
        "$@"
    fi
}

# launchd starts coreaudiod again as soon as it exits; wait for the new one.
restart_coreaudiod() {
    local old new
    old="$(pgrep -x coreaudiod || true)"
    run sudo killall coreaudiod || true
    [[ "$dry_run" -eq 1 ]] && return 0
    for _ in $(seq 1 50); do
        new="$(pgrep -x coreaudiod || true)"
        [[ -n "$new" && "$new" != "$old" ]] && return 0
        sleep 0.2
    done
    echo "warning: coreaudiod has not come back yet; sound returns when it does." >&2
}

if [[ "$mode" == uninstall ]]; then
    if [[ ! -e "$installed" ]]; then
        echo "The audio driver is not installed ($installed does not exist); nothing to do."
        [[ "$dry_run" -eq 1 ]] || exit 0
        echo "(dry run: showing what an uninstall does when it is installed)"
    fi
    cat <<MSG
About to, with sudo (it may ask for your password):
  - remove $installed
  - restart coreaudiod (killall coreaudiod), which briefly interrupts sound
MSG
    run sudo rm -rf "$installed"
    restart_coreaudiod
    if [[ "$dry_run" -eq 1 ]]; then
        echo "Dry run: nothing was changed."
    else
        echo "Uninstalled the audio driver: \"$mic_name\" is gone."
    fi
    exit 0
fi

echo "Building the audio driver..."
run "$here/build-audio-driver.sh"

cat <<MSG

About to, with sudo (it may ask for your password):
  - install $installed (root:wheel), replacing any copy already there
  - restart coreaudiod (killall coreaudiod), which briefly interrupts sound
MSG
# An existing HAL directory is left exactly as it is.
if [[ ! -d "$hal" ]]; then
    run sudo install -d -o root -g wheel -m 0755 "$hal"
fi
run sudo rm -rf "$installed"
run sudo ditto "$built" "$installed"
run sudo chown -R root:wheel "$installed"
restart_coreaudiod

if [[ "$dry_run" -eq 1 ]]; then
    echo "Dry run: nothing was changed."
    exit 0
fi

# Give coreaudiod a moment to load the driver, then look for the mic.
found=0
for _ in $(seq 1 10); do
    if system_profiler SPAudioDataType 2>/dev/null | grep -qF "$mic_name"; then
        found=1
        break
    fi
    sleep 1
done
if [[ "$found" -eq 0 ]]; then
    echo "\"$mic_name\" did not show up in system_profiler SPAudioDataType." >&2
    echo "Check Audio MIDI Setup, and the coreaudiod log: log show --last 5m --predicate 'process == \"coreaudiod\"'" >&2
    exit 1
fi

cat <<MSG

The audio driver is installed: Mac apps can now pick "$mic_name" as
their microphone. It is silent until CursorJump feeds it the PC's mic.

Driver:    $installed
Uninstall: $0 --uninstall
MSG
