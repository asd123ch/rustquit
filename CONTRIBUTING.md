# Contributing to RustQuit

Bug reports, focused fixes and well-scoped improvements are welcome. Use
[GitHub Issues](https://github.com/asd123ch/rustquit/issues) to discuss a
substantial behaviour or interface change before implementing it.

Security reports follow the private process in
[.github/SECURITY.md](.github/SECURITY.md).

## Development requirements

Development requires an Apple Silicon Mac with macOS 13 or newer, Xcode
Command Line Tools and Rust 1.88 or newer. Cargo uses the committed lockfile
for reproducible dependency resolution.

Run these checks before opening a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
cargo audit
```

The continuous integration workflow runs the same checks on macOS. It also
checks the release shell scripts for syntax errors.

## Pull requests

1. Create a focused branch and keep unrelated changes out of the pull request.
2. Explain the user-visible behaviour and security or privacy impact.
3. Add or update automated tests for new functionality and bug fixes when a
   practical test boundary exists.
4. Update user documentation when behaviour, configuration or requirements
   change.
5. Confirm that no credentials, private keys, customer data, internal paths or
   private release instructions are included.
6. Open the pull request against `main` and address review and CI findings.

Code, documentation and commit messages are written in English. Rust code must
be formatted with rustfmt and pass Clippy without warnings. Generated binaries
and release artifacts do not belong in the source repository.

By contributing, you agree that your contribution is licensed under the
project's [GNU Affero General Public License v3.0](LICENSE).
