//! System processes that must never be auto-quit.
//!
//! Union of the hard-coded protection lists of SwiftQuit (Finder,
//! Spotlight, Notification Center) and Quitty (plus SystemUIServer, Dock,
//! Control Center, WindowManager, TextInputMenuAgent, System Events),
//! extended with loginwindow and RustQuit itself. Enforced in three
//! places: the quit decision, the settings app list (not shown at all),
//! and the "Add App…" panel (warning popup).

const PROTECTED_BUNDLE_IDS: &[&str] = &[
    // Core system UI — quitting these breaks the desktop.
    "com.apple.finder",
    "com.apple.spotlight",
    "com.apple.notificationcenterui",
    "com.apple.systemuiserver",
    "com.apple.dock",
    "com.apple.controlcenter",
    "com.apple.windowmanager",
    "com.apple.textinputmenuagent",
    "com.apple.systemevents",
    "com.apple.backboardd",
    "com.apple.loginwindow",
    "com.apple.coreservices.uiagent",
    // Wizards and tools where an interruption could do real damage.
    "com.apple.migrateassistant",
    "com.apple.bootcampassistant",
    "com.apple.diskutility",
    // Accessibility tools — never pull those away from someone relying on them.
    "com.apple.voiceoverutility",
    "com.apple.magnifier",
    // Phone: audio calls keep running without any window; auto-quitting
    // the app would drop an active call.
    "com.apple.mobilephone",
    // Launcher stubs that never own real windows (listing them is noise).
    "com.apple.exposelauncher",
    // RustQuit itself.
    "ch.patrick.rustquit",
];

/// Bundle IDs are matched case-insensitively (Apple is not consistent).
pub fn is_protected_bundle_id(bundle_id: &str) -> bool {
    let lower = bundle_id.to_lowercase();
    // The ".launcher" suffix marks stub apps that only trampoline into a
    // system service (Siri, Time Machine, Screenshot, Apps, …).
    lower.ends_with(".launcher") || PROTECTED_BUNDLE_IDS.contains(&lower.as_str())
}

/// Defense in depth: everything living under /System/Library/ (e.g.
/// CoreServices) is system infrastructure, regardless of its bundle ID.
pub fn is_protected_path(path: &str) -> bool {
    let path = std::path::Path::new(path);
    protected_system_path(path)
        || std::fs::canonicalize(path)
            .ok()
            .is_some_and(|resolved| protected_system_path(&resolved))
}

fn protected_system_path(path: &std::path::Path) -> bool {
    path.starts_with("/System/Library")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_system_apps_are_protected() {
        assert!(is_protected_bundle_id("com.apple.finder"));
        assert!(is_protected_bundle_id("com.apple.Spotlight"));
        assert!(is_protected_bundle_id("com.apple.WindowManager"));
        assert!(is_protected_bundle_id("ch.patrick.rustquit"));
    }

    #[test]
    fn normal_apps_are_not_protected() {
        assert!(!is_protected_bundle_id("com.apple.TextEdit"));
        assert!(!is_protected_bundle_id("com.apple.Safari"));
        assert!(!is_protected_bundle_id("org.mozilla.firefox"));
        assert!(!is_protected_bundle_id("com.apple.Terminal"));
        assert!(!is_protected_bundle_id("com.apple.systempreferences"));
    }

    #[test]
    fn wizards_stubs_and_accessibility_tools_are_protected() {
        assert!(is_protected_bundle_id("com.apple.MigrateAssistant"));
        assert!(is_protected_bundle_id("com.apple.bootcampassistant"));
        assert!(is_protected_bundle_id("com.apple.DiskUtility"));
        assert!(is_protected_bundle_id("com.apple.VoiceOverUtility"));
        assert!(is_protected_bundle_id("com.apple.exposelauncher"));
        // Suffix rule for launcher stubs.
        assert!(is_protected_bundle_id("com.apple.siri.launcher"));
        assert!(is_protected_bundle_id("com.apple.backup.launcher"));
        assert!(is_protected_bundle_id("com.apple.screenshot.launcher"));
    }

    #[test]
    fn system_library_paths_are_protected() {
        assert!(is_protected_path("/System/Library/CoreServices/Finder.app"));
        assert!(!is_protected_path("/System/Applications/TextEdit.app"));
        assert!(!is_protected_path("/Applications/Safari.app"));
    }

    #[test]
    fn symlinks_into_system_library_are_protected() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("rustquit-protected-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let link = root.join("SystemLibrary");
        symlink("/System/Library", &link).unwrap();
        assert!(is_protected_path(link.to_str().unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }
}
