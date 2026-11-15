use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_APPS: usize = 4096;
const MIN_QUIT_DELAY_SECS: f64 = 0.05;
const MAX_QUIT_DELAY_SECS: f64 = 10.0;
const MIN_RESTART_DELAY_SECS: f64 = 1.0;
const MAX_RESTART_DELAY_SECS: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterMode {
    /// Quit all apps except the listed ones.
    Blacklist,
    /// Quit only the listed apps.
    Whitelist,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mode: FilterMode,
    pub quit_delay_secs: f64,
    pub enabled: bool,
    pub check_for_updates: bool,
    /// Bundle IDs; their meaning depends on the mode.
    pub apps: Vec<String>,
    /// Master toggle for relaunching the keep-alive apps.
    pub keep_alive_enabled: bool,
    /// Restore missing kept apps when RustQuit starts, unless a related
    /// background helper already keeps the app suite running.
    #[serde(alias = "keep_alive_launch_at_startup")]
    pub keep_alive_auto_start_missing: bool,
    /// Wait after a keep-alive app terminates before relaunching it, so
    /// self-relaunching updaters win the race.
    pub keep_alive_delay_secs: f64,
    /// After repeated automatic restarts within an hour, pause keep-alive
    /// for the app until the next day (guards against crash loops).
    pub keep_alive_loop_protection: bool,
    /// Bundle IDs to keep running. Listed apps are never auto-quit, even
    /// while the keep-alive master toggle is off.
    pub keep_alive_apps: Vec<String>,
    /// Lowercase bundle ID -> Unix timestamp through which automatic Keep
    /// launches are suppressed. Entries are created from the tray menu.
    pub keep_alive_ignored_until: BTreeMap<String, u64>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            // Deliberately conservative default: whitelist + empty list
            // does nothing until the user adds apps.
            mode: FilterMode::Whitelist,
            quit_delay_secs: 2.0,
            enabled: true,
            check_for_updates: true,
            apps: Vec::new(),
            keep_alive_enabled: true,
            keep_alive_auto_start_missing: true,
            keep_alive_delay_secs: 10.0,
            keep_alive_loop_protection: true,
            keep_alive_apps: Vec::new(),
            keep_alive_ignored_until: BTreeMap::new(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidConfig;

pub fn parse_config(text: &str, now: u64) -> Result<Config, InvalidConfig> {
    if text.len() > MAX_CONFIG_BYTES {
        return Err(InvalidConfig);
    }
    let mut config = toml::from_str(text).map_err(|_| InvalidConfig)?;
    normalize(&mut config, now);
    Ok(config)
}

pub fn normalize(config: &mut Config, now: u64) {
    config
        .keep_alive_ignored_until
        .retain(|_, until| *until > now);
    normalize_settings(config);
}

pub fn normalize_settings(config: &mut Config) {
    if !config.quit_delay_secs.is_finite() {
        config.quit_delay_secs = Config::default().quit_delay_secs;
    }
    config.quit_delay_secs = config
        .quit_delay_secs
        .clamp(MIN_QUIT_DELAY_SECS, MAX_QUIT_DELAY_SECS);

    if !config.keep_alive_delay_secs.is_finite() {
        config.keep_alive_delay_secs = Config::default().keep_alive_delay_secs;
    }
    config.keep_alive_delay_secs = config
        .keep_alive_delay_secs
        .clamp(MIN_RESTART_DELAY_SECS, MAX_RESTART_DELAY_SECS);

    normalize_app_list(&mut config.apps);
    normalize_app_list(&mut config.keep_alive_apps);
    normalize_ignored_apps(&mut config.keep_alive_ignored_until);

    // An app cannot be on both lists; keep-alive wins (matches the engine,
    // which never auto-quits a keep-alive app). The settings window greys
    // out the other checkbox for the same reason.
    config
        .apps
        .retain(|bundle_id| !is_app_listed(&config.keep_alive_apps, bundle_id));
}

pub fn is_app_listed(apps: &[String], bundle_id: &str) -> bool {
    apps.iter().any(|id| id.eq_ignore_ascii_case(bundle_id))
}

fn normalize_ignored_apps(ignored: &mut BTreeMap<String, u64>) {
    let previous = std::mem::take(ignored);
    for (bundle_id, until) in previous {
        if ignored.len() >= MAX_APPS {
            break;
        }
        if valid_bundle_id(&bundle_id) {
            ignored
                .entry(bundle_id.to_ascii_lowercase())
                .and_modify(|stored| *stored = (*stored).max(until))
                .or_insert(until);
        }
    }
}

fn normalize_app_list(list: &mut Vec<String>) {
    list.retain(|bundle_id| valid_bundle_id(bundle_id));
    list.sort_by_key(|bundle_id| bundle_id.to_ascii_lowercase());
    list.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    list.truncate(MAX_APPS);
}

fn valid_bundle_id(bundle_id: &str) -> bool {
    !bundle_id.is_empty()
        && bundle_id.len() <= 255
        && bundle_id.contains('.')
        && !bundle_id.starts_with('.')
        && !bundle_id.ends_with('.')
        && bundle_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}
