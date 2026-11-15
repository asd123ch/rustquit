#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

duration=${1:-30}
case "$duration" in
  ''|*[!0-9]*) echo "Usage: bash scripts/fuzz-smoke.sh [seconds-per-target: 1..600]" >&2; exit 1 ;;
esac
if [ "$duration" -lt 1 ] || [ "$duration" -gt 600 ] || [ "$#" -gt 1 ]; then
  echo "fuzz-smoke: duration must be 1..600 seconds per target" >&2
  exit 1
fi

export RUSTUP_TOOLCHAIN=nightly
cargo fetch --locked --manifest-path fuzz/Cargo.toml
export CARGO_NET_OFFLINE=true
fuzzer=${CARGO_FUZZ:-cargo-fuzz}
for target in config update_release; do
  corpus="fuzz/corpus/$target"
  artifacts="fuzz/artifacts/$target"
  mkdir -p "$corpus" "$artifacts"
  echo "Fuzzing $target for $duration seconds; log: $artifacts/run.log"
  if ! "$fuzzer" run "$target" "$corpus" "fuzz/seeds/$target" -- \
    -max_total_time="$duration" -timeout=10 -rss_limit_mb=2048 -max_len=65536 \
    -artifact_prefix="$artifacts/" -print_final_stats=1 > "$artifacts/run.log" 2>&1; then
    tail -n 60 "$artifacts/run.log" >&2
    exit 1
  fi
  tail -n 7 "$artifacts/run.log"
done
