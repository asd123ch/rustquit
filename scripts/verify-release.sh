#!/usr/bin/env bash
# Read-only checks for the current version's app and DMG in dist/.
set -euo pipefail
cd "$(dirname "$0")/.."

NOTARIZED=1
case "${1:-}" in
  --local) NOTARIZED=0; shift ;;
  --help)
    echo "Usage: bash scripts/verify-release.sh [--local]"
    echo "--local skips notarization, Gatekeeper and release cask checks."
    exit 0
    ;;
esac
if [ "$#" -ne 0 ]; then
  echo "verify-release: unknown argument: $1" >&2
  exit 1
fi

VERSION=$(cargo read-manifest --offline | ruby -rjson -e 'puts JSON.parse(STDIN.read).fetch("version")')
APP="dist/RustQuit.app"
DMG="dist/RustQuit-$VERSION.dmg"
fail() { echo "verify-release: $*" >&2; exit 1; }

plutil -lint "$APP/Contents/Info.plist"
for key in CFBundleVersion CFBundleShortVersionString; do
  [ "$(/usr/libexec/PlistBuddy -c "Print :$key" "$APP/Contents/Info.plist")" = "$VERSION" ] || fail "version mismatch: $key"
done
[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Contents/Info.plist")" = "ch.asd123.rustquit" ] || fail "unexpected bundle identifier"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign --verify --strict --verbose=2 "$DMG"
signature=$(codesign -dvv "$APP" 2>&1)
grep -Fq 'Authority=Developer ID Application:' <<< "$signature" || fail "Developer ID signature missing"
grep -Eq 'flags=.*\(runtime\)' <<< "$signature" || fail "Hardened Runtime missing"
[ "$(lipo -archs "$APP/Contents/MacOS/rustquit")" = arm64 ] || fail "executable must contain only arm64"
hdiutil verify "$DMG"

temporary=$(mktemp -d "${TMPDIR:-/tmp}/rustquit-verify.XXXXXX")
mounted=0
cleanup() {
  if [ "$mounted" -eq 1 ]; then
    if ! hdiutil detach "$temporary/mount" -quiet; then
      echo "verify-release: detach $temporary/mount manually; temporary files retained" >&2
      return 1
    fi
  fi
  rm -rf "$temporary"
}
trap cleanup EXIT
hdiutil attach "$DMG" -readonly -nobrowse -mountpoint "$temporary/mount" -quiet
mounted=1
diff -qr "$APP" "$temporary/mount/RustQuit.app"
[ "$(readlink "$temporary/mount/Applications")" = /Applications ] || fail "Applications link missing"

if [ "$NOTARIZED" -eq 1 ]; then
  xcrun stapler validate "$APP"
  xcrun stapler validate "$DMG"
  spctl --assess --type execute --verbose=2 "$APP"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
  ssh-keygen -Y verify -f packaging/release-signers -I asd123ch -n file \
    -s "$DMG.sig" < "$DMG"
  SHA256=$(shasum -a 256 "$DMG" | awk '{print $1}')
  sed -e "s/@VERSION@/$VERSION/g" -e "s/@SHA256@/$SHA256/g" \
    packaging/rustquit.rb.in > "$temporary/rustquit.rb"
  cmp "$temporary/rustquit.rb" dist/rustquit.rb
  echo "verify-release: notarized package, detached signature and matching cask verified"
else
  echo "verify-release: local package verified; NOT approved for public distribution"
fi
shasum -a 256 "$DMG"
