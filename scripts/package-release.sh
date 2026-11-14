#!/usr/bin/env bash
# Builds, signs and notarizes the Apple Silicon release DMG, then generates the
# matching Homebrew cask. It never uploads files.
set -euo pipefail

cd "$(dirname "$0")/.."

NOTARIZE=1
case "${1:-}" in
  --no-notarize) NOTARIZE=0; shift ;;
  --help)
    echo "Usage: scripts/package-release.sh [--no-notarize]"
    echo "Set APPLE_SIGNING_IDENTITY to a Developer ID identity."
    echo "For notarization, also set RUSTQUIT_NOTARIZATION_ENV."
    exit 0
    ;;
esac
if [ "$#" -ne 0 ]; then
  echo "package-release: unknown argument: $1" >&2
  exit 1
fi
: "${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY to the Developer ID identity}"
IDENTITY="$APPLE_SIGNING_IDENTITY"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP="dist/RustQuit.app"
DMG="dist/RustQuit-$VERSION.dmg"

case "$VERSION" in
  '' | *[!0-9.]* | .* | *.)
    echo "package-release: invalid Cargo package version: $VERSION" >&2
    exit 1
    ;;
esac

if ! rustup target list --installed | grep -qx 'aarch64-apple-darwin'; then
  echo "package-release: missing Rust target aarch64-apple-darwin" >&2
  echo "Run: rustup target add aarch64-apple-darwin" >&2
  exit 1
fi
if [ "$NOTARIZE" -eq 1 ]; then
  : "${RUSTQUIT_NOTARIZATION_ENV:?Set RUSTQUIT_NOTARIZATION_ENV to the notarization environment file}"
  if [ ! -f "$RUSTQUIT_NOTARIZATION_ENV" ]; then
    echo "package-release: notarization environment file not found" >&2
    exit 1
  fi
  # shellcheck source=/dev/null
  . "$RUSTQUIT_NOTARIZATION_ENV"
  : "${APPLE_API_KEY:?APPLE_API_KEY is not configured}"
  : "${APPLE_API_ISSUER:?APPLE_API_ISSUER is not configured}"
  : "${APPLE_API_KEY_PATH:?APPLE_API_KEY_PATH is not configured}"
  if [ ! -f "$APPLE_API_KEY_PATH" ]; then
    echo "package-release: APPLE_API_KEY_PATH points to a missing file" >&2
    exit 1
  fi
fi
if ! security find-identity -v -p codesigning | grep -Fq "$IDENTITY"; then
  echo "package-release: Developer ID signing identity is unavailable: $IDENTITY" >&2
  exit 1
fi

temporary=$(mktemp -d "${TMPDIR:-/tmp}/rustquit-release.XXXXXX")
cleanup() {
  rm -rf "$temporary"
}
trap cleanup EXIT

# Remap dependency and panic locations as well as this checkout's source paths.
if [ -z "${CARGO_ENCODED_RUSTFLAGS+x}" ]; then
  CARGO_ENCODED_RUSTFLAGS=""
  if [ -n "${RUSTFLAGS:-}" ]; then
    read -r -a rustflags <<< "$RUSTFLAGS"
    for flag in "${rustflags[@]}"; do
      CARGO_ENCODED_RUSTFLAGS+="${CARGO_ENCODED_RUSTFLAGS:+$'\037'}$flag"
    done
  fi
fi
for mapping in \
  "$HOME=/build/user" \
  "${CARGO_HOME:-$HOME/.cargo}=/build/cargo" \
  "$(rustc --print sysroot)=/build/toolchain" \
  "$(pwd -P)=/build/rustquit"; do
  CARGO_ENCODED_RUSTFLAGS+="${CARGO_ENCODED_RUSTFLAGS:+$'\037'}--remap-path-prefix=$mapping"
done
export CARGO_ENCODED_RUSTFLAGS

echo "package-release: building RustQuit $VERSION for Apple Silicon"
cargo build --release --locked --target aarch64-apple-darwin

if [ -d dist ]; then
  mkdir -p .release-backups
  mv dist ".release-backups/$(date -u +%Y%m%dT%H%M%SZ)-$$"
fi
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/aarch64-apple-darwin/release/rustquit "$APP/Contents/MacOS/rustquit"
if LC_ALL=C grep -aEq '/(Users|home)/' "$APP/Contents/MacOS/rustquit"; then
  echo "package-release: executable contains a personal build path; refusing to sign" >&2
  exit 1
fi
cp assets/Info.plist "$APP/Contents/Info.plist"
cp LICENSE "$APP/Contents/Resources/LICENSE"
cp assets/RustQuit.icns "$APP/Contents/Resources/RustQuit.icns"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" \
  -c "Set :CFBundleVersion $VERSION" "$APP/Contents/Info.plist"

codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
if [ "$(lipo -archs "$APP/Contents/MacOS/rustquit")" != arm64 ]; then
  echo "package-release: executable must contain only arm64" >&2
  exit 1
fi

if [ "$NOTARIZE" -eq 1 ]; then
  ditto -c -k --keepParent "$APP" "$temporary/RustQuit.zip"
  echo "package-release: notarizing and stapling the app"
  xcrun notarytool submit "$temporary/RustQuit.zip" \
    --key "$APPLE_API_KEY_PATH" \
    --key-id "$APPLE_API_KEY" \
    --issuer "$APPLE_API_ISSUER" \
    --wait
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  spctl --assess --type execute --verbose=2 "$APP"
fi

mkdir -p "$temporary/dmg"
cp -R "$APP" "$temporary/dmg/RustQuit.app"
ln -s /Applications "$temporary/dmg/Applications"
hdiutil create -quiet -volname RustQuit -srcfolder "$temporary/dmg" \
  -ov -format UDZO "$DMG"
codesign --force --timestamp --sign "$IDENTITY" "$DMG"
codesign --verify --strict --verbose=2 "$DMG"
hdiutil verify "$DMG"

if [ "$NOTARIZE" -eq 1 ]; then
  echo "package-release: notarizing and stapling the DMG"
  xcrun notarytool submit "$DMG" \
    --key "$APPLE_API_KEY_PATH" \
    --key-id "$APPLE_API_KEY" \
    --issuer "$APPLE_API_ISSUER" \
    --wait
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
fi

SHA256=$(shasum -a 256 "$DMG" | awk '{print $1}')
if [ "$NOTARIZE" -eq 1 ]; then
  sed -e "s/@VERSION@/$VERSION/g" -e "s/@SHA256@/$SHA256/g" \
    packaging/rustquit.rb.in >dist/rustquit.rb
  echo "  Homebrew cask: dist/rustquit.rb"
else
  echo "package-release: local test build; notarization and release cask generation skipped"
fi

echo "package-release: complete"
echo "  DMG: $DMG"
echo "  SHA-256: $SHA256"
