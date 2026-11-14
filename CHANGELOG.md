# Changelog

## 0.5.0 - 2026-11-14

Changes since v0.4.1.

### Added

- Optional daily update checks through GitHub, with a manual check in Settings.
  The menu bar shows an update action only when a newer release is available.
- Release packaging and a Homebrew cask template for Apple Silicon Macs
  running macOS 13 or newer.
- A security policy, contribution guide and automated security checks.

### Fixed

- Invalid configuration files no longer expose their contents in error logs.
- Keep no longer reopens settings windows when an embedded menu bar app is
  still running.
- Recently Quit no longer treats one process exiting as the whole app closing
  when another process with the same bundle identifier remains active.
- Improved last-window detection for apps with missing window-close events
  or leftover offscreen helper surfaces. Windows on other Spaces remain
  protected, and uncertain window state still prevents quitting.

### Changed

- Updated dependencies and release builds for macOS 27.
- Simplified installation and usage documentation, with source-build steps
  in a collapsible section.
