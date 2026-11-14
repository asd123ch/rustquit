#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$#" -ne 0 ]; then
  echo "Usage: bash scripts/sign-release.sh" >&2
  exit 1
fi

VERSION=$(cargo read-manifest --offline | ruby -rjson -e 'puts JSON.parse(STDIN.read).fetch("version")')
DMG="dist/RustQuit-$VERSION.dmg"
KEY=${RELEASE_SIGNING_KEY:-$(git config --get user.signingkey || true)}
if [ -z "$KEY" ] || [ ! -f "$KEY" ]; then
  echo "sign-release: configure user.signingkey or RELEASE_SIGNING_KEY" >&2
  exit 1
fi
if [ ! -f "$DMG" ] || [ -e "$DMG.sig" ]; then
  echo "sign-release: expected a DMG without an existing detached signature" >&2
  exit 1
fi

ssh-keygen -Y sign -f "$KEY" -n file "$DMG"
ssh-keygen -Y verify -f packaging/release-signers -I asd123ch -n file \
  -s "$DMG.sig" < "$DMG"
