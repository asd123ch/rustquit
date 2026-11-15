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

pub use crate::config_data::{Config, FilterMode, is_app_listed};
use crate::config_data::{MAX_CONFIG_BYTES, normalize_settings, parse_config};
use objc2_foundation::NSFileManager;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
                if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES as u64 =>
            {
                tracing::warn!("configuration is not a safe regular file; using defaults");
                Config::default()
            }
            Ok(_) => match std::fs::read_to_string(&path) {
                Ok(text) => match parse_config(&text, unix_timestamp()) {
                    Ok(config) => config,
                    Err(_) => {
                        tracing::warn!("configuration invalid; using safe defaults");
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
            // Expiry timers still need their entries, including after sleep.
            normalize_settings(&mut config);
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

    /// Sets or clears a persistent Keep suppression deadline.
    pub fn set_keep_alive_ignored_until(
        &self,
        bundle_id: &str,
        until: Option<u64>,
    ) -> io::Result<()> {
        let key = bundle_id.to_ascii_lowercase();
        self.update(|config| match until {
            Some(until) => {
                config.keep_alive_ignored_until.insert(key, until);
            }
            None => {
                config.keep_alive_ignored_until.remove(&key);
            }
        })
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
    home_dir()
        .map(|home| home.join("Library/Application Support/rustquit"))
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("rustquit-uid-{}", unsafe { libc::geteuid() }))
        })
}

pub fn home_dir() -> Option<PathBuf> {
    let file_manager = NSFileManager::defaultManager();
    file_manager
        .homeDirectoryForCurrentUser()
        .path()
        .map(|path| PathBuf::from(path.to_string()))
}

pub fn create_private_dir(path: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn config_path() -> PathBuf {
    app_support_dir().join("config.toml")
}

fn set_listed(list: &mut Vec<String>, bundle_id: &str, listed: bool) {
    let present = is_app_listed(list, bundle_id);
    if listed && !present {
        list.push(bundle_id.to_string());
    } else if !listed && present {
        list.retain(|b| !b.eq_ignore_ascii_case(bundle_id));
    }
}

pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_data::normalize;

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
        normalize(&mut config, unix_timestamp());
        assert_eq!(config.quit_delay_secs, 2.0);
        assert_eq!(config.apps, vec!["com.Example.App"]);
        assert_eq!(config.keep_alive_delay_secs, 1.0);
        assert_eq!(config.keep_alive_apps, vec!["com.Keep.Me"]);

        // An app on both lists stays keep-alive only.
        config.apps = vec!["com.keep.me".into(), "com.Other.App".into()];
        normalize(&mut config, unix_timestamp());
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
    fn saving_one_expired_ignore_preserves_other_pending_expiries() {
        let path = temp_config_path("ignore-expiry");
        let store = ConfigStore::load_from(path.clone());
        store.data.borrow_mut().keep_alive_ignored_until.extend([
            ("com.example.first".into(), 1),
            ("com.example.second".into(), 2),
        ]);

        store
            .set_keep_alive_ignored_until("com.example.first", None)
            .unwrap();

        assert_eq!(
            store
                .data
                .borrow()
                .keep_alive_ignored_until
                .get("com.example.second"),
            Some(&2)
        );
        store.update(|config| config.enabled = false).unwrap();
        assert_eq!(store.data.borrow().keep_alive_ignored_until.len(), 1);
        // A fresh process has no pending timers; startup reconciliation owns recovery.
        assert!(
            ConfigStore::load_from(path.clone())
                .data
                .borrow()
                .keep_alive_ignored_until
                .is_empty()
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
                config.check_for_updates = false;
            })
            .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let reloaded = ConfigStore::load_from(path);
        assert_eq!(reloaded.data.borrow().mode, FilterMode::Blacklist);
        assert_eq!(reloaded.data.borrow().apps, vec!["com.example.Test"]);
        assert!(!reloaded.data.borrow().check_for_updates);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_config_does_not_log_input() {
        let path = temp_config_path("invalid-private");
        let root = path.parent().unwrap();
        std::fs::create_dir_all(root).unwrap();
        let log_path = root.join("diagnostic.log");

        for input in [
            "apps = [\"synthetic-private-marker\"\n",
            "mode = \"synthetic-private-marker\"\n",
        ] {
            std::fs::write(&path, input).unwrap();
            let log = File::create(&log_path).unwrap();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || log.try_clone().unwrap())
                .finish();
            let store = tracing::subscriber::with_default(subscriber, || {
                ConfigStore::load_from(path.clone())
            });

            assert_eq!(*store.data.borrow(), Config::default());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), input);
            let output = std::fs::read_to_string(&log_path).unwrap();
            assert!(output.contains("configuration invalid; using safe defaults"));
            assert!(!output.contains("synthetic-private-marker"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_configs_keep_their_app_lists() {
        let config: Config =
            toml::from_str("apps = ['com.example.Quit']\nkeep_alive_apps = ['com.example.Keep']\n")
                .unwrap();
        assert!(config.check_for_updates);
        assert_eq!(config.apps, ["com.example.Quit"]);
        assert_eq!(config.keep_alive_apps, ["com.example.Keep"]);
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
