# Configuration reference

RustQuit stores settings in `~/Library/Application Support/rustquit/config.toml`.
Prefer the Settings window. For manual edits, quit RustQuit first, edit the file,
then reopen the app. The file is not watched for changes while RustQuit runs.
Settings changes replace the file atomically with user-only permissions.

## Fields

Missing fields use these defaults. An invalid type or invalid TOML makes the
whole file fall back to defaults. Unknown fields are ignored and disappear on
the next save. Files larger than 1 MiB, symlinks and non-regular files are not loaded.

| Field | Default | Meaning and accepted values |
| --- | --- | --- |
| `mode` | `"whitelist"` | `"whitelist"` quits only `apps`. `"blacklist"` protects `apps` and may quit other apps. |
| `apps` | `[]` | Bundle identifiers selected for the current mode. |
| `enabled` | `true` | Enable Auto-Quit. This does not disable Keep. |
| `quit_delay_secs` | `2.0` | Seconds before rechecking and requesting a quit. Clamped to 0.05 through 10. Non-finite values use the default. |
| `check_for_updates` | `true` | Automatic checks at launch and daily. Manual checks remain available in Settings. |
| `keep_alive_enabled` | `true` | Enable Keep launches. Listed Keep apps remain protected from Auto-Quit when this is off. |
| `keep_alive_apps` | `[]` | Bundle identifiers to keep running. Keep takes precedence if an app appears in both lists. |
| `keep_alive_auto_start_missing` | `true` | Restore missing Keep apps and relaunch them after normal exits. With this off, regular apps restart only after a detected crash; menu bar apps retain their Keep behaviour. A running related background helper can suppress a settings-frontend launch. |
| `keep_alive_delay_secs` | `10.0` | Seconds to wait after termination before attempting a restart. Clamped to 1 through 60. Non-finite values use the default. |
| `keep_alive_loop_protection` | `true` | After two automatic restarts within an hour, suppress the next restart for 24 hours. This loop-protection pause is in memory; a manual ignore is persisted separately. |
| `keep_alive_ignored_until` | `{}` | Table of bundle identifiers to Unix timestamps in seconds. Suppresses automatic Keep launches until the deadline, including after restarting RustQuit. Each expiry timer removes its own entry; expired entries are also discarded on startup. |

The older name `keep_alive_launch_at_startup` is accepted as an alias for
`keep_alive_auto_start_missing`. Use only one of these names in a file.

App lists are case-insensitive, deduplicated, sorted and limited to 4096 entries.
Identifiers must be 1 through 255 ASCII characters, contain a dot, and not start
or end with a dot. Letters, numbers, dots, hyphens and underscores are accepted.
Invalid identifiers are discarded. Ignore-table keys are stored in lowercase;
case variants use the later deadline. The table also has a 4096-entry limit.

An empty default whitelist quits nothing. System protections, pending-window
checks and missing permissions still apply even when an app is listed for Quit.
An explicit reopen from Recently Quit is a user action, not an automatic Keep
launch. It does not clear a separately requested 24-hour ignore.

## Example

This uses synthetic app identifiers and leaves automatic quitting disabled:

```toml
mode = "whitelist"
apps = ["com.example.Editor"]
enabled = false
quit_delay_secs = 2.0
check_for_updates = true
keep_alive_enabled = true
keep_alive_apps = ["com.example.MenuHelper"]
keep_alive_auto_start_missing = true
keep_alive_delay_secs = 10.0
keep_alive_loop_protection = true

[keep_alive_ignored_until]
```

Use the Recently Quit menu to create a 24-hour ignore or resume Keep early.
Do not copy another user's app list or publish your configuration for debugging.

## Other inputs and outputs

- Launch at login is managed by macOS Service Management, not this TOML file.
- Accessibility permission is managed in macOS System Settings.
- Recent-quit history and loop-protection counters are in memory.
- `RUSTQUIT_DIAGNOSTICS=1` enables local diagnostic files. They can contain
  process and bundle identifiers; review them before sharing. See the README
  for their location and retention. Invalid TOML contents are not logged.
- Update checks send an HTTPS request to GitHub and expose the app version in
  the User-Agent. A successful check only offers a release-page link. The app
  does not download, install or execute an update.
- The menu and Settings window are the user interface. RustQuit exposes no
  command-line configuration API, plugin API or IPC service.
