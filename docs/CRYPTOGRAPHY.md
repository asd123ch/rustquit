# Cryptographic dependencies

Reviewed on 2026-11-14. This records implementation
evidence, not a certification or a guarantee about future server settings.

## Update checks

`src/network.rs` uses the macOS URL Loading System through `NSURLSession`.
It supplies no custom trust handler or TLS override. `assets/Info.plist` contains
no App Transport Security exceptions. The release build targets macOS 13 or
newer and links a modern Apple SDK. Apple documents that ATS applies by default
to this configuration and requires HTTPS, TLS 1.2 or newer, authenticated
certificates and forward secrecy. See [Apple's ATS requirements](https://developer.apple.com/documentation/security/preventing-insecure-network-connections).

Apple documents minimum RSA certificate keys of 2048 bits or ECC keys of
256 bits, SHA-256 or stronger certificate signatures, AES-128 or AES-256 and
ECDHE key exchange. These are platform requirements, not measurements of every
connection. RustQuit does not add weaker protocol or cipher exceptions.

The only update endpoint is GitHub's HTTPS release API. Redirects are rejected,
response size and time are bounded, and the parser accepts only release links
within the expected repository. The ephemeral session disables shared cookies,
credentials and disk caching. No passwords, license keys or custom cryptographic
secrets are stored by the app. Update checks can be disabled.

## Downloads and release signing

Apple Developer ID signatures, Hardened Runtime and notarization protect public
release artifacts. These depend on Apple's tools and services. Notarization
does not establish reproducible builds or prove the absence of vulnerabilities.

`scripts/sign-release.sh` adds an OpenSSH detached signature over the complete
DMG, using the `file` namespace. `packaging/release-signers` contains only the
trusted public key. Signing material stays outside the repository. Verification
commands are in the [security policy](../.github/SECURITY.md).
The committed public key was checked with `ssh-keygen` on 2026-11-14 and is
RSA-3072. Verification of the exact final Developer ID certificate chain remains
part of the release review; the TLS policy does not establish its key strength.

Homebrew downloads over HTTPS and checks the cask's SHA-256 digest. RustQuit
itself never downloads or runs an updater. Before a public release, verify the
exact final DMG, its detached signature, notarization ticket and cask hash.
