# Security policy

## Supported versions

Security updates are provided for the latest published RustQuit release. Older
versions are unsupported once a newer release is available.

| Version | Supported |
| --- | --- |
| Latest published release | Yes |
| Older releases | No |

## Reporting a vulnerability

Report suspected vulnerabilities through
[GitHub Private Vulnerability Reporting](https://github.com/asd123ch/rustquit/security/advisories/new).
Do not disclose the issue in a public issue, discussion or pull request before
a fix is available.

Include the affected RustQuit and macOS versions, the expected and observed
behaviour, reproduction steps and the potential impact. Screenshots and logs
are useful when they do not contain credentials, personal data or other secrets.

You can expect an initial response within 14 calendar days. Confirmed issues
are assessed, fixed and disclosed according to their severity and practical
impact. Please allow time for a corrected release before publishing details.

## Scope

The policy covers the source in this repository and official RustQuit release
artifacts. Modified source builds and third-party packages are outside its
scope, but reports that also affect an official release are welcome.

## Security properties

RustQuit requires macOS Accessibility access to observe application windows.
It has no privileged helper, IPC server, telemetry or analytics. Network
requests do not use shared cookies, credentials or cached responses.

Optional update checks use GitHub's HTTPS release API with bounded responses,
timeouts and no redirects. RustQuit validates release versions and pins links
to this repository's release pages. It never downloads or executes updates
automatically. Auto-Quit and Keep work without network access.

The app is not App-Sandboxed because it must inspect Accessibility elements
owned by other applications. Official releases use Apple notarization and the
Hardened Runtime without exception entitlements. Configuration and optional
diagnostic logs are stored locally with user-only permissions.

RustQuit requests normal application termination and never force-quits another
app. It revalidates each termination decision immediately before sending the
request.

## Verifying downloads

Official DMGs have an Apple Developer ID signature and a stapled notarization
ticket. macOS checks these when opening the downloaded app.

Releases also include an OpenSSH detached signature, `RustQuit-VERSION.dmg.sig`.
Download it alongside the DMG. From a trusted checkout of this repository,
verify both files with:

```sh
ssh-keygen -Y verify -f packaging/release-signers -I asd123ch -n file \
  -s /path/to/RustQuit-VERSION.dmg.sig < /path/to/RustQuit-VERSION.dmg
```

Replace `VERSION` and the paths with the downloaded release. The signer file
contains only the public release key. Do not substitute an untrusted key when
verification fails. This signature authenticates the download; it does not
prove that the binary was reproducibly built from the source.
