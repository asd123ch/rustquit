# Parser fuzzing

Two libFuzzer targets compile the app's `src/config_data.rs` and
`src/release_data.rs` modules. They do not link AppKit, load user settings,
contact a server, or start or stop applications.

| Target | Checks |
| --- | --- |
| `config` | TOML decoding, bounded delays, bundle IDs, Keep precedence, suppression deadlines, normalization and serialization round trips. |
| `update_release` | JSON decoding, versions, trusted release URLs, draft/prerelease handling, redirects and HTTP errors. |

Run fuzzing locally after relevant parser or dependency changes. It uses native
macOS tools, without Docker or scheduled jobs. Seeds in `seeds/` are synthetic;
never add real settings, logs, credentials or other private data.

## Deterministic checks

The stable compiler runs these checks locally and in the regular CI workflow:

```sh
cargo test --locked --manifest-path fuzz/Cargo.toml --lib
cargo clippy --locked --manifest-path fuzz/Cargo.toml --lib --tests -- -D warnings
```

The separate lockfile keeps fuzz-only dependencies out of the app. When parser
dependencies change, update both manifests and lockfiles together.

## Local fuzzing

Install the tools under the ignored `target/` directory. This leaves the normal
Rust toolchain and global PATH unchanged:

```sh
cargo install cargo-fuzz --version 0.13.2 --locked --root "$PWD/target/fuzz-tools"
RUSTUP_HOME="$PWD/target/fuzz-tools/rustup" rustup toolchain install nightly --profile minimal --component rust-src --no-self-update
```

Run both targets with AddressSanitizer, debug assertions and overflow checks:

```sh
RUSTUP_HOME="$PWD/target/fuzz-tools/rustup" \
  CARGO_FUZZ="$PWD/target/fuzz-tools/bin/cargo-fuzz" \
  bash scripts/fuzz-smoke.sh 30
```

Each target runs on one worker for 30 seconds, plus build time. The optional
duration accepts 1 to 600 seconds per target. Inputs are capped at 64 KiB, each
input at 10 seconds, and memory at 2 GiB. Deterministic tests check the production
1 MiB limits separately.

Generated inputs and diagnostic artifacts stay in ignored `corpus/`, `artifacts/`
and `target/` directories. Crashes and sanitizer failures exit nonzero. Review
and minimize findings before sharing them. To replay a configuration crash:

```sh
RUSTUP_HOME="$PWD/target/fuzz-tools/rustup" RUSTUP_TOOLCHAIN=nightly \
  target/fuzz-tools/bin/cargo-fuzz run config fuzz/artifacts/config/CRASH_FILE
```

Use `update_release` for a release-parser artifact.

## Native AddressSanitizer tests

Use the same isolated nightly toolchain to check the app's unit tests:

```sh
RUSTUP_HOME="$PWD/target/fuzz-tools/rustup" bash scripts/test-asan.sh
```

The script does not launch RustQuit or alter its configuration. Network tests
exercise Objective-C delegates with synthetic objects without starting requests.
Window tests cover query failures, minimized and inactive-Space windows, stale
quit decisions and repeated desktop changes. Native tests exercise AX and CG
value decoding, Spaces responses, workspace notifications and late callbacks.

Live observer registration, window transitions, permission revocation and
full-screen/multi-display switching still need interactive verification.
AddressSanitizer instruments Rust code and dependencies, not Apple's prebuilt
libraries. A clean run is not proof that every macOS event is handled correctly.
