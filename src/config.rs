//! TOML config at ~/Library/Application Support/rustquit/config.toml.
//! A corrupt or missing file never crashes the app — it just yields defaults.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_APPS: usize = 4096;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MIN_QUIT_DELAY_SECS: f64 = 0.05;
const MAX_QUIT_DELAY_SECS: f64 = 10.0;
const MIN_RESTART_DELAY_SECS: f64 = 1.0;
const MAX_RESTART_DELAY_SECS: f64 = 60.0;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    /// Bundle IDs; their meaning depends on the mode.
    pub apps: Vec<String>,
    /// Master toggle for relaunching the keep-alive apps.
    pub keep_alive_enabled: bool,
    /// Wait after a keep-alive app terminates before relaunching it, so
    /// self-relaunching updaters win the race.
    pub keep_alive_delay_secs: f64,
    /// After repeated automatic restarts within an hour, pause keep-alive
    /// for the app until the next day (guards against crash loops).
    pub keep_alive_loop_protection: bool,
    /// Bundle IDs to keep running. Listed apps are never auto-quit, even
    /// while the keep-alive master toggle is off.
    pub keep_alive_apps: Vec<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            // Deliberately conservative default: whitelist + empty list
            // does nothing until the user adds apps.
            mode: FilterMode::Whitelist,
            quit_delay_secs: 2.0,
            enabled: true,
            apps: Vec::new(),
            keep_alive_enabled: true,
            keep_alive_delay_secs: 10.0,
            keep_alive_loop_protection: true,
            keep_alive_apps: Vec::new(),
        }
    }
}

pub struct ConfigStore {
    path: PathBuf,
    pub data: RefCell<Config>,
}

pub type ConfigHandle = Rc<ConfigStore>;

impl ConfigStore {
    pub fn load() -> ConfigHandle {
        Self::load_from(config_path())
    }

    fn load_from(path: PathBuf) -> ConfigHandle {
        let metadata = std::fs::symlink_metadata(&path);
        let data = match metadata.as_ref() {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tracing::info!("no configuration found; using defaults");
                Config::default()
            }
            Err(err) => {
                tracing::warn!(%err, "configuration unreadable; using safe defaults");
                Config::default()
            }
            Ok(metadata)
                if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES =>
            {
                tracing::warn!("configuration is not a safe regular file; using defaults");
                Config::default()
            }
            Ok(_) => match std::fs::read_to_string(&path) {
                Ok(text) => match toml::from_str::<Config>(&text) {
                    Ok(mut config) => {
                        normalize(&mut config);
                        config
                    }
                    Err(err) => {
                        tracing::warn!(%err, "configuration invalid; using safe defaults");
                        Config::default()
                    }
                },
                Err(err) => {
                    tracing::warn!(%err, "configuration unreadable; using safe defaults");
                    Config::default()
                }
            },
        };
        if metadata.is_ok_and(|metadata| metadata.file_type().is_file()) {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Rc::new(ConfigStore {
            path,
            data: RefCell::new(data),
        })
    }

    pub fn update(&self, change: impl FnOnce(&mut Config)) -> io::Result<()> {
        let previous = self.data.borrow().clone();
        {
            let mut config = self.data.borrow_mut();
            change(&mut config);
            normalize(&mut config);
        }
        if let Err(err) = self.save_current() {
            self.data.replace(previous);
            return Err(err);
        }
        Ok(())
    }

    /// Adds a bundle ID to the quit list or removes it, then saves.
    pub fn set_app_listed(&self, bundle_id: &str, listed: bool) -> io::Result<()> {
        self.update(|config| set_listed(&mut config.apps, bundle_id, listed))
    }

    /// Adds a bundle ID to the keep-alive list or removes it, then saves.
    pub fn set_keep_alive_listed(&self, bundle_id: &str, listed: bool) -> io::Result<()> {
        self.update(|config| set_listed(&mut config.keep_alive_apps, bundle_id, listed))
    }

    fn save_current(&self) -> io::Result<()> {
        let text = toml::to_string_pretty(&*self.data.borrow()).map_err(io::Error::other)?;
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("configuration path has no parent"))?;
        create_private_dir(dir)?;

        let suffix = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let tmp = self.path.with_extension(format!(
            "toml.tmp.{}.{}.{}",
            std::process::id(),
            timestamp,
            suffix
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
            File::open(dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

pub fn app_support_dir() -> PathBuf {
    dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Library/Application Support")))
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("rustquit-uid-{}", unsafe { libc::geteuid() }))
        })
        .join("rustquit")
}

pub fn create_private_dir(path: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn config_path() -> PathBuf {
    app_support_dir().join("config.toml")
}

fn set_listed(list: &mut Vec<String>, bundle_id: &str, listed: bool) {
    let present = list.iter().any(|b| b.eq_ignore_ascii_case(bundle_id));
    if listed && !present {
        list.push(bundle_id.to_string());
    } else if !listed && present {
        list.retain(|b| !b.eq_ignore_ascii_case(bundle_id));
    }
}

fn normalize(config: &mut Config) {
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

    // An app cannot be on both lists; keep-alive wins (matches the engine,
    // which never auto-quits a keep-alive app). The settings window greys
    // out the other checkbox for the same reason.
    config.apps.retain(|bundle_id| {
        !config
            .keep_alive_apps
            .iter()
            .any(|keep| keep.eq_ignore_ascii_case(bundle_id))
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("rustquit-test-{}-{name}", std::process::id()))
            .join("config.toml")
    }

    #[test]
    fn normalization_is_safe_and_case_insensitive() {
        let mut config = Config {
            mode: FilterMode::Blacklist,
            quit_delay_secs: f64::INFINITY,
            enabled: true,
            apps: vec![
                "com.Example.App".into(),
                "com.example.app".into(),
                "../invalid".into(),
            ],
            keep_alive_delay_secs: 0.0,
            keep_alive_apps: vec!["com.Keep.Me".into(), "no-dot".into()],
            ..Config::default()
        };
        normalize(&mut config);
        assert_eq!(config.quit_delay_secs, 2.0);
        assert_eq!(config.apps, vec!["com.Example.App"]);
        assert_eq!(config.keep_alive_delay_secs, 1.0);
        assert_eq!(config.keep_alive_apps, vec!["com.Keep.Me"]);

        // An app on both lists stays keep-alive only.
        config.apps = vec!["com.keep.me".into(), "com.Other.App".into()];
        normalize(&mut config);
        assert_eq!(config.apps, vec!["com.Other.App"]);
        assert_eq!(config.keep_alive_apps, vec!["com.Keep.Me"]);
    }

    #[test]
    fn update_rolls_back_when_save_fails() {
        let path = PathBuf::from("/dev/null/config.toml");
        let store = ConfigStore::load_from(path);
        let previous = store.data.borrow().clone();
        assert!(store.update(|config| config.enabled = false).is_err());
        assert_eq!(*store.data.borrow(), previous);
    }

    #[test]
    fn saved_config_is_private_and_reloadable() {
        let path = temp_config_path("private");
        let root = path.parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&root);
        let store = ConfigStore::load_from(path.clone());
        store
            .update(|config| {
                config.mode = FilterMode::Blacklist;
                config.apps.push("com.example.Test".into());
            })
            .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let reloaded = ConfigStore::load_from(path);
        assert_eq!(reloaded.data.borrow().mode, FilterMode::Blacklist);
        assert_eq!(reloaded.data.borrow().apps, vec!["com.example.Test"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symlink_config_is_ignored() {
        use std::os::unix::fs::symlink;

        let path = temp_config_path("symlink");
        let root = path.parent().unwrap().to_path_buf();
        let target = root.join("target.toml");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            &target,
            "mode = \"blacklist\"\nquit_delay_secs = 2\nenabled = false\napps = []\n",
        )
        .unwrap();
        symlink(&target, &path).unwrap();

        let store = ConfigStore::load_from(path);
        assert_eq!(*store.data.borrow(), Config::default());

        std::fs::remove_dir_all(root).unwrap();
    }
}
