#!/usr/bin/env bash
# Builds the CursorJump audio driver (a Core Audio HAL plug-in with the
# "CursorJump Microphone" and the hidden "CursorJump Mic Feed") into
# target/audio-driver/CursorJumpAudio.driver.
#
#   ./scripts/mac/build-audio-driver.sh            # build and sign
#   ./scripts/mac/build-audio-driver.sh --no-sign  # build only (CI)
#
# Signs with the "CursorJump Dev" identity from make-signing-cert.sh;
# CJ_SIGN_IDENTITY overrides it (- signs ad hoc). Installing is
# install-audio-driver.sh's job.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
src="$repo/app/cursorjump-audio-driver"
bundle="$repo/target/audio-driver/CursorJumpAudio.driver"
identity="${CJ_SIGN_IDENTITY:-CursorJump Dev}"
factory=CursorJumpAudio_Create

usage() {
    echo "usage: $0 [--no-sign]" >&2
    exit 2
}
sign=1
[[ $# -le 1 ]] || usage
case "${1-}" in
    "") ;;
    --no-sign) sign=0 ;;
    *) usage ;;
esac

if [[ "$sign" -eq 1 && "$identity" != "-" ]]; then
    identities="$(security find-identity -v -p codesigning)"
    if ! grep -qF "\"$identity\"" <<<"$identities"; then
        echo "No code-signing identity \"$identity\" found." >&2
        echo "Run scripts/mac/make-signing-cert.sh first (or set CJ_SIGN_IDENTITY=- to sign ad hoc)." >&2
        exit 1
    fi
fi

echo "Building $bundle..."
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS"
clang -bundle -arch arm64 -O2 -std=c11 -Wall -Wextra -Werror -fvisibility=hidden \
    -framework CoreAudio -framework CoreFoundation \
    "$src/src/plugin.c" "$src/src/ring.c" \
    -o "$bundle/Contents/MacOS/CursorJumpAudio"
cp "$src/Info.plist" "$bundle/Contents/Info.plist"
plutil -lint -s "$bundle/Contents/Info.plist"

# coreaudiod finds the factory by the name in Info.plist: make sure it is exported.
if ! nm -gU "$bundle/Contents/MacOS/CursorJumpAudio" | grep -q " _$factory\$"; then
    echo "The driver does not export $factory, which Info.plist names." >&2
    exit 1
fi

if [[ "$sign" -eq 0 ]]; then
    echo "Built $bundle (not signed)."
    exit 0
fi
codesign --force --timestamp=none --sign "$identity" "$bundle"
codesign --verify --strict "$bundle"
echo "Built and signed $bundle (identity \"$identity\")."
