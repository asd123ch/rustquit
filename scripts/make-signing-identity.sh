#!/usr/bin/env bash
# Create a STABLE self-signed code-signing identity "RustQuit Dev" in a
# DEDICATED keychain — fully non-interactive: no login-keychain password or
# codesign access popups. Idempotent: re-running keeps the existing identity.
#
# Why this matters: macOS ties the Accessibility (TCC) permission to the code
# signing identity. An ad-hoc signature changes on every rebuild, so the
# permission would have to be re-granted after each build. A stable identity
# makes the grant permanent.
#
# Why a dedicated keychain: signing from the login keychain triggers a macOS
# password dialog for codesign. A dedicated keychain with a random password
# sidesteps that without granting arbitrary applications access to the key.
set -euo pipefail
umask 077

IDENTITY="RustQuit Dev"
KC="$HOME/Library/Keychains/rustquit-signing.keychain-db"
PW_DIR="$HOME/Library/Application Support/rustquit"
PW_FILE="$PW_DIR/signing-keychain-password"
WORKDIR="$(mktemp -d)"
cleanup() {
  security lock-keychain "$KC" >/dev/null 2>&1 || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

# Already set up in the dedicated keychain? Unlock and stop.
if [ -f "$KC" ] && [ -f "$PW_FILE" ] &&
   security find-identity -p codesigning "$KC" 2>/dev/null | grep -qF "$IDENTITY"; then
  echo "Already set up: \"$IDENTITY\" in $KC — nothing to do."
  echo "Build with:  scripts/bundle.sh"
  exit 0
fi

# An identity of the same name already lives in the default keychain search
# list (e.g. created manually via Keychain Access)? Keep using that one —
# replacing it would invalidate an existing Accessibility grant.
if security find-identity -p codesigning 2>/dev/null | grep -qF "$IDENTITY"; then
  echo "\"$IDENTITY\" already exists in your default keychain — keeping it."
  echo "bundle.sh will sign with it as before."
  exit 0
fi

echo "Creating self-signed code-signing identity \"$IDENTITY\" in a dedicated keychain..."
KCPW="$(openssl rand -hex 32)"

# 1. Self-signed leaf usable as its own code-signing anchor.
cat > "$WORKDIR/codesign.cnf" <<'EOF'
[ req ]
distinguished_name = dn
x509_extensions    = v3
prompt             = no

[ dn ]
CN = RustQuit Dev

[ v3 ]
basicConstraints     = critical, CA:true
keyUsage             = critical, digitalSignature
extendedKeyUsage     = critical, codeSigning
subjectKeyIdentifier = hash
EOF
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "$WORKDIR/key.pem" -out "$WORKDIR/cert.pem" \
  -days 3650 -config "$WORKDIR/codesign.cnf" >/dev/null 2>&1

# 2. PKCS#12 bundle. OpenSSL 3.x defaults to a MAC the Security framework
#    can't verify, so force the legacy SHA1/3DES PBE + a non-empty password.
P12PW="$(openssl rand -hex 32)"
openssl pkcs12 -export \
  -inkey "$WORKDIR/key.pem" -in "$WORKDIR/cert.pem" -name "$IDENTITY" \
  -out "$WORKDIR/identity.p12" -passout "pass:${P12PW}" \
  -macalg sha1 -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES >/dev/null 2>&1

# 3. Fresh dedicated keychain with a random password.
security delete-keychain "$KC" 2>/dev/null || true
security create-keychain -p "$KCPW" "$KC"
security set-keychain-settings -lut 300 "$KC"
security unlock-keychain -p "$KCPW" "$KC"

# 4. Import the key as non-extractable and authorize Apple's signed tooling.
#    bundle.sh locks the dedicated keychain immediately after each build.
security import "$WORKDIR/identity.p12" -k "$KC" -P "$P12PW" -x \
  -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k "$KCPW" "$KC" >/dev/null 2>&1

# 5. Verify and persist the keychain password for bundle.sh.
if security find-identity -p codesigning "$KC" | grep -qF "$IDENTITY"; then
  mkdir -p "$PW_DIR"
  printf '%s\n' "$KCPW" > "$PW_FILE"
  echo "Done. Build with:  scripts/bundle.sh"
else
  echo "ERROR: identity was not created." >&2
  exit 1
fi
