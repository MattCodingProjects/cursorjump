#!/usr/bin/env bash
# Creates the self-signed code-signing identity "CursorJump Dev" in the
# login keychain. bundle.sh signs with it, so macOS sees every rebuild as
# the same app and keeps its Accessibility grant.
#
#   ./scripts/mac/make-signing-cert.sh
#
# Safe to run again: it does nothing when the identity already exists.
set -euo pipefail

name="CursorJump Dev"
keychain="$HOME/Library/Keychains/login.keychain-db"

has_identity() {
    local identities
    identities="$(security find-identity -v -p codesigning)"
    grep -qF "\"$name\"" <<<"$identities"
}

if has_identity; then
    echo "The code-signing identity \"$name\" already exists. Nothing to do."
    exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if security find-certificate -c "$name" "$keychain" >/dev/null 2>&1; then
    # An earlier run imported the certificate but did not finish trusting it.
    echo "Found the \"$name\" certificate in the login keychain; trusting it."
    security find-certificate -c "$name" -p "$keychain" >"$work/cert.pem"
else
    echo "Creating the self-signed code-signing certificate \"$name\"..."
    cat >"$work/cert.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $name
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
subjectKeyIdentifier = hash
CNF
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -config "$work/cert.cnf" -keyout "$work/key.pem" -out "$work/cert.pem"

    # SHA-1/3DES PKCS#12 is what `security import` reads. Naming the
    # algorithms works with both LibreSSL (/usr/bin/openssl) and OpenSSL 3,
    # whose newer defaults `security` may reject.
    pass="$(openssl rand -hex 16)"
    openssl pkcs12 -export -name "$name" \
        -inkey "$work/key.pem" -in "$work/cert.pem" \
        -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
        -passout "pass:$pass" -out "$work/cert.p12"
    security import "$work/cert.p12" -f pkcs12 -k "$keychain" -P "$pass" -T /usr/bin/codesign
fi

echo "macOS will now ask for your login password to trust \"$name\" for code signing."
security add-trusted-cert -p codeSign -k "$keychain" "$work/cert.pem"

if ! has_identity; then
    echo "The \"$name\" certificate is in the login keychain but is not a usable signing identity." >&2
    echo "Delete it in Keychain Access, then run this script again." >&2
    exit 1
fi
echo "Created the code-signing identity \"$name\"."
