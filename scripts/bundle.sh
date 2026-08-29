#!/bin/sh
# Builds RustQuit.app into dist/ and signs it.
#
# Signing identity: prefers the self-signed "RustQuit Dev" certificate
# (run scripts/make-signing-identity.sh once to create it), otherwise
# ad-hoc. Ad-hoc means: after every rebuild the Accessibility permission
# must be re-granted in System Settings.
set -eu

cd "$(dirname "$0")/.."

cargo build --release --locked

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

APP=dist/RustQuit.app
rm -rf dist
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/rustquit "$APP/Contents/MacOS/rustquit"
cp assets/Info.plist "$APP/Contents/Info.plist"
cp LICENSE "$APP/Contents/Resources/LICENSE"
cp assets/RustQuit.icns "$APP/Contents/Resources/RustQuit.icns"

# Single source of truth for the version is Cargo.toml.
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" \
                        -c "Set :CFBundleVersion $VERSION" "$APP/Contents/Info.plist"

IDENTITY="-"
KC="$HOME/Library/Keychains/rustquit-signing.keychain-db"
PW_FILE="$HOME/Library/Application Support/rustquit/signing-keychain-password"
KEYCHAIN_UNLOCKED=0
lock_signing_keychain() {
    if [ "$KEYCHAIN_UNLOCKED" -eq 1 ]; then
        security lock-keychain "$KC" >/dev/null 2>&1 || true
    fi
}
trap lock_signing_keychain EXIT

if [ -f "$KC" ] && [ -f "$PW_FILE" ] &&
   security find-identity -p codesigning "$KC" 2>/dev/null | grep -q "RustQuit Dev"; then
    # Dedicated signing keychain (created by scripts/make-signing-identity.sh).
    security unlock-keychain -p "$(cat "$PW_FILE")" "$KC"
    KEYCHAIN_UNLOCKED=1
    codesign --force --options runtime --timestamp=none \
             --sign "RustQuit Dev" --keychain "$KC" "$APP"
    IDENTITY="RustQuit Dev"
elif security find-identity -v -p codesigning 2>/dev/null | grep -q "RustQuit Dev"; then
    # Identity in the default keychain search list (e.g. login keychain).
    codesign --force --options runtime --timestamp=none --sign "RustQuit Dev" "$APP"
    IDENTITY="RustQuit Dev"
else
    echo "Note: certificate 'RustQuit Dev' not found — signing ad-hoc."
    echo "      Run scripts/make-signing-identity.sh once to fix this."
    codesign --force --options runtime --timestamp=none --sign - "$APP"
fi

codesign --verify --deep --strict "$APP"

echo "Done: $APP v$VERSION (signature: $IDENTITY)"
echo "Install: cp -R $APP /Applications/"
