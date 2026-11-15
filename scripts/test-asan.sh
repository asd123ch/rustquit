#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$#" -ne 0 ]; then
  echo "Usage: bash scripts/test-asan.sh" >&2
  exit 1
fi
export RUSTUP_TOOLCHAIN=nightly
export RUSTFLAGS='-Zsanitizer=address -Cdebug-assertions=yes -Coverflow-checks=yes'
cargo test --locked --all-targets --target aarch64-apple-darwin \
  --target-dir "$PWD/target/asan"
