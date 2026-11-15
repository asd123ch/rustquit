# Changelog

## 0.5.1 - 2026-11-15

### Fixed

- Retry window-type checks after temporary Accessibility failures and apply
  the short messaging timeout to window objects as well as applications.
- Restore Keep after restart-loop pauses and process every expired manual
  ignore, including when several deadlines pass during sleep.
- Restore missing kept apps when Keep is enabled again and when a related
  background helper exits, without reopening settings while a helper is running.
- Cancel window recounts when Accessibility cannot identify a window's subrole,
  instead of treating that window as closed.
- Reject malformed Accessibility window arrays and malformed Space-query responses.
- Replace monitor-specific icon color profiles with standard sRGB profiles.

### Changed

- Expand tests for window counting, Spaces transitions, stale quit decisions
  and native callback handling.
- Update the TOML dependency to 1.2.1.
- Add on-demand local parser fuzzing and document configuration and security verification.
- Simplify app-list checks and native menu setup without changing app behaviour.
- Remove redundant manual thread-safety implementations.

### Removed

- Remove the Scorecard badge and workflow.

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
