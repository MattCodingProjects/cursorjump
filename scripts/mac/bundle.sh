#!/usr/bin/env bash
# Builds CursorJump for Apple silicon and assembles a signed
# target/CursorJump.app.
#
#   ./scripts/mac/bundle.sh
#
# Signs with the "CursorJump Dev" identity from make-signing-cert.sh, so
# the Accessibility grant survives rebuilds. CJ_SIGN_IDENTITY=- signs ad hoc
# instead (fine for testing the bundle; macOS then asks again each build).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
identity="${CJ_SIGN_IDENTITY:-CursorJump Dev}"
target=aarch64-apple-darwin
app="$repo/target/CursorJump.app"

if [[ "$identity" != "-" ]]; then
    identities="$(security find-identity -v -p codesigning)"
    if ! grep -qF "\"$identity\"" <<<"$identities"; then
        echo "No code-signing identity \"$identity\" found." >&2
        echo "Run scripts/mac/make-signing-cert.sh first (or set CJ_SIGN_IDENTITY=- to sign ad hoc)." >&2
        exit 1
    fi
fi

cd "$repo"
echo "Building CursorJump (release, $target)..."
cargo build --release -p cursorjump --target "$target"

# `cargo pkgid` ends in "#0.1.0" or "#cursorjump@0.1.0".
pkgid="$(cargo pkgid -p cursorjump)"
version="${pkgid##*[#@]}"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "target/$target/release/cursorjump" "$app/Contents/MacOS/cursorjump"
sed "s/__VERSION__/$version/g" "$here/Info.plist" >"$app/Contents/Info.plist"
plutil -lint -s "$app/Contents/Info.plist"

codesign --force --options runtime --timestamp=none --sign "$identity" "$app"
codesign --verify --strict "$app"
echo "Built and signed $app (version $version, identity \"$identity\")."
