use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu, NSMenuDelegate,
    NSMenuItem, NSStatusBar, NSStatusBarButton, NSStatusItem, NSVariableStatusItemLength,
    NSWorkspace,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString, ns_string};

use crate::config::ConfigHandle;
use crate::engine::recent::{RecentQuitsHandle, age_label};
use crate::settings_window::SettingsController;

pub struct TrayIvars {
    config: ConfigHandle,
    settings: Retained<SettingsController>,
    recent_quits: RecentQuitsHandle,
    enabled_item: RefCell<Option<Retained<NSMenuItem>>>,
    keep_alive_item: RefCell<Option<Retained<NSMenuItem>>>,
    status_button: RefCell<Option<Retained<NSStatusBarButton>>>,
    recent_menu: RefCell<Option<Retained<NSMenu>>>,
    permission_warning_item: RefCell<Option<Retained<NSMenuItem>>>,
    permission_open_item: RefCell<Option<Retained<NSMenuItem>>>,
    permission_separator: RefCell<Option<Retained<NSMenuItem>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RQTrayTarget"]
    #[ivars = TrayIvars]
    pub struct TrayTarget;

    unsafe impl NSObjectProtocol for TrayTarget {}

    unsafe impl NSMenuDelegate for TrayTarget {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            // The main menu picks up config changes made in the settings
            // window; the recent submenu is rebuilt from scratch.
            self.sync_from_config();
            self.rebuild_recent_menu(menu);
        }
    }

    impl TrayTarget {
        #[unsafe(method(onToggleEnabled:))]
        fn on_toggle_enabled(&self, _sender: Option<&AnyObject>) {
            let ivars = self.ivars();
            let wanted = !ivars.config.data.borrow().enabled;
            if let Err(err) = ivars.config.update(|config| config.enabled = wanted) {
                tracing::error!(%err, "cannot persist enabled state");
            }
            self.sync_from_config();
            tracing::info!(
                enabled = ivars.config.data.borrow().enabled,
                "auto-quit toggled"
            );
        }

        #[unsafe(method(onToggleKeepAlive:))]
        fn on_toggle_keep_alive(&self, _sender: Option<&AnyObject>) {
            let ivars = self.ivars();
            let wanted = !ivars.config.data.borrow().keep_alive_enabled;
            if let Err(err) = ivars
                .config
                .update(|config| config.keep_alive_enabled = wanted)
            {
                tracing::error!(%err, "cannot persist keep-alive state");
            }
            self.sync_from_config();
            tracing::info!(
                enabled = ivars.config.data.borrow().keep_alive_enabled,
                "keep running toggled"
            );
        }

        #[unsafe(method(onOpenSettings:))]
        fn on_open_settings(&self, _sender: Option<&AnyObject>) {
            self.ivars().settings.show();
        }

        #[unsafe(method(onOpenAccessibilitySettings:))]
        fn on_open_accessibility_settings(&self, _sender: Option<&AnyObject>) {
            crate::permissions::open_accessibility_settings();
        }

        #[unsafe(method(onReopenApp:))]
        fn on_reopen_app(&self, sender: Option<&NSMenuItem>) {
            let Some(sender) = sender else { return };
            let Some(object) = sender.representedObject() else {
                return;
            };
            let Ok(bundle_id) = object.downcast::<NSString>() else {
                return;
            };
            let workspace = NSWorkspace::sharedWorkspace();
            let url = workspace.URLForApplicationWithBundleIdentifier(&bundle_id);
            match url {
                Some(url) => {
                    workspace.openURL(&url);
                    tracing::info!("reopening a recently quit app");
                }
                None => {
                    tracing::warn!("recently quit app not found");
                }
            }
        }

        #[unsafe(method(onQuit:))]
        fn on_quit(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    }
);

impl TrayTarget {
    fn new(
        mtm: MainThreadMarker,
        config: ConfigHandle,
        settings: Retained<SettingsController>,
        recent_quits: RecentQuitsHandle,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TrayIvars {
            config,
            settings,
            recent_quits,
            enabled_item: RefCell::new(None),
            keep_alive_item: RefCell::new(None),
            status_button: RefCell::new(None),
            recent_menu: RefCell::new(None),
            permission_warning_item: RefCell::new(None),
            permission_open_item: RefCell::new(None),
            permission_separator: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Sets the toggle checkmarks and the icon dimming to the current
    /// config; called on every menu open and after settings changes.
    pub fn sync_from_config(&self) {
        let ivars = self.ivars();
        let (enabled, keep_alive) = {
            let config = ivars.config.data.borrow();
            (config.enabled, config.keep_alive_enabled)
        };
        let state = |on: bool| {
            if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            }
        };
        if let Some(item) = ivars.enabled_item.borrow().as_ref() {
            item.setState(state(enabled));
        }
        if let Some(item) = ivars.keep_alive_item.borrow().as_ref() {
            item.setState(state(keep_alive));
        }
        // Dim the menu bar icon while auto-quit is off.
        if let Some(button) = ivars.status_button.borrow().as_ref() {
            button.setAppearsDisabled(!enabled);
        }
    }

    /// Shows or hides the "permission missing" menu section.
    pub fn set_permission_ok(&self, ok: bool) {
        let ivars = self.ivars();
        for slot in [
            &ivars.permission_warning_item,
            &ivars.permission_open_item,
            &ivars.permission_separator,
        ] {
            if let Some(item) = slot.borrow().as_ref() {
                item.setHidden(ok);
            }
        }
    }

    /// Fills the "Recently Quit" submenu each time it opens.
    fn rebuild_recent_menu(&self, menu: &NSMenu) {
        let ivars = self.ivars();
        let is_recent_menu = ivars
            .recent_menu
            .borrow()
            .as_ref()
            .is_some_and(|m| **m == *menu);
        if !is_recent_menu {
            return;
        }
        let mtm = MainThreadMarker::from(self);
        menu.removeAllItems();
        let entries = ivars.recent_quits.snapshot();
        if entries.is_empty() {
            let empty = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    ns_string!("No apps quit yet"),
                    None,
                    ns_string!(""),
                )
            };
            empty.setEnabled(false);
            menu.addItem(&empty);
            return;
        }
        let target_obj: &AnyObject = self;
        for entry in entries {
            let title = format!("{} — {}", entry.name, age_label(entry.when));
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(&title),
                    Some(sel!(onReopenApp:)),
                    ns_string!(""),
                )
            };
            unsafe {
                item.setTarget(Some(target_obj));
                item.setRepresentedObject(Some(&NSString::from_str(&entry.bundle_id)));
            }
            menu.addItem(&item);
        }
    }
}

/// Keeps the status item, menu, and target alive; must live until exit.
pub struct Tray {
    _status_item: Retained<NSStatusItem>,
    target: Retained<TrayTarget>,
}

impl Tray {
    pub fn setup(
        mtm: MainThreadMarker,
        config: ConfigHandle,
        settings: Retained<SettingsController>,
        recent_quits: RecentQuitsHandle,
    ) -> Tray {
        let target = TrayTarget::new(mtm, config, settings, recent_quits);

        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        if let Some(button) = status_item.button(mtm) {
            // Glyph-only version of the app logo, embedded at compile time.
            // As a template image, macOS recolors it for light/dark menu bars.
            let data =
                objc2_foundation::NSData::with_bytes(include_bytes!("../assets/menubar-icon.png"));
            let image = NSImage::initWithData(NSImage::alloc(), &data);
            match image {
                Some(image) => {
                    image.setSize(objc2_foundation::NSSize::new(16.0, 16.0));
                    image.setTemplate(true);
                    button.setImage(Some(&image));
                }
                None => button.setTitle(ns_string!("RQ")),
            }
            // sync_from_config below applies the dimmed state.
            target.ivars().status_button.replace(Some(button));
        }

        let menu = NSMenu::new(mtm);
        let target_obj: &AnyObject = &target;

        // Permission section: only visible while the Accessibility
        // permission is missing (set_permission_ok hides it).
        let warning_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("⚠️ Accessibility permission missing"),
                None,
                ns_string!(""),
            )
        };
        warning_item.setEnabled(false);
        menu.addItem(&warning_item);
        target
            .ivars()
            .permission_warning_item
            .replace(Some(warning_item));

        let open_settings_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Open System Settings…"),
                Some(sel!(onOpenAccessibilitySettings:)),
                ns_string!(""),
            )
        };
        unsafe { open_settings_item.setTarget(Some(target_obj)) };
        menu.addItem(&open_settings_item);
        target
            .ivars()
            .permission_open_item
            .replace(Some(open_settings_item));

        let permission_separator = NSMenuItem::separatorItem(mtm);
        menu.addItem(&permission_separator);
        target
            .ivars()
            .permission_separator
            .replace(Some(permission_separator));

        let enabled_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Auto-Quit"),
                Some(sel!(onToggleEnabled:)),
                ns_string!(""),
            )
        };
        unsafe { enabled_item.setTarget(Some(target_obj)) };
        menu.addItem(&enabled_item);
        target.ivars().enabled_item.replace(Some(enabled_item));

        let keep_alive_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Keep Running"),
                Some(sel!(onToggleKeepAlive:)),
                ns_string!(""),
            )
        };
        unsafe { keep_alive_item.setTarget(Some(target_obj)) };
        menu.addItem(&keep_alive_item);
        target
            .ivars()
            .keep_alive_item
            .replace(Some(keep_alive_item));

        // "Recently Quit" submenu, rebuilt on every open via NSMenuDelegate.
        let recent_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Recently Quit"),
                None,
                ns_string!(""),
            )
        };
        let recent_menu = NSMenu::new(mtm);
        recent_menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        recent_item.setSubmenu(Some(&recent_menu));
        menu.addItem(&recent_item);
        target.ivars().recent_menu.replace(Some(recent_menu));

        let settings_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Settings…"),
                Some(sel!(onOpenSettings:)),
                ns_string!(","),
            )
        };
        unsafe { settings_item.setTarget(Some(target_obj)) };
        menu.addItem(&settings_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Quit RustQuit"),
                Some(sel!(onQuit:)),
                ns_string!("q"),
            )
        };
        unsafe { quit_item.setTarget(Some(target_obj)) };
        menu.addItem(&quit_item);

        // The delegate refreshes the toggle states on every open, so
        // changes made in the settings window show up immediately.
        menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        target.sync_from_config();

        status_item.setMenu(Some(&menu));

        Tray {
            _status_item: status_item,
            target,
        }
    }

    pub fn target(&self) -> &TrayTarget {
        &self.target
    }

    pub fn target_retained(&self) -> Retained<TrayTarget> {
        self.target.clone()
    }
}
