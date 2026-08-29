//! Native settings window: mode, delay, launch at login, and the app list
//! with checkboxes. The window is built once and then only shown/hidden.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver};

use objc2::rc::{Retained, Weak, autoreleasepool};
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSControlStateValueOff, NSControlStateValueOn,
    NSControlTextEditingDelegate, NSImageView, NSLayoutConstraint, NSModalResponseOK, NSOpenPanel,
    NSPopUpButton, NSScrollView, NSStackView, NSTableColumn, NSTableView, NSTableViewDataSource,
    NSTableViewDelegate, NSTextField, NSUserInterfaceLayoutOrientation, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSBundle, NSInteger, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect,
    NSSize, NSString, ns_string,
};

use crate::config::{ConfigHandle, FilterMode};
use crate::engine;

const DELAY_CHOICES: &[(f64, &str)] = &[
    (0.05, "0.05 seconds"),
    (0.5, "0.5 seconds"),
    (1.0, "1 second"),
    (2.0, "2 seconds"),
    (3.0, "3 seconds"),
    (5.0, "5 seconds"),
    (10.0, "10 seconds"),
];

/// Index of the 2-second default in DELAY_CHOICES.
const DEFAULT_DELAY_INDEX: usize = 3;

const RESTART_DELAY_CHOICES: &[(f64, &str)] = &[
    (5.0, "5 seconds"),
    (10.0, "10 seconds"),
    (30.0, "30 seconds"),
    (60.0, "1 minute"),
];

/// Index of the 10-second default in RESTART_DELAY_CHOICES.
const DEFAULT_RESTART_DELAY_INDEX: usize = 1;

/// Extra vertical gap between settings sections.
const SECTION_GAP: f64 = 22.0;

const GITHUB_URL: &str = "https://github.com/asd123ch/rustquit";

#[derive(Clone)]
struct AppEntry {
    bundle_id: String,
    name: String,
    icon_path: Option<String>,
}

pub struct SettingsIvars {
    config: ConfigHandle,
    entries: RefCell<Vec<AppEntry>>,
    all_entries: RefCell<Vec<AppEntry>>,
    search_field: RefCell<Option<Retained<objc2_app_kit::NSSearchField>>>,
    only_selected_checkbox: RefCell<Option<Retained<NSButton>>>,
    scan_rx: RefCell<Option<Receiver<Vec<AppEntry>>>>,
    scan_timer: RefCell<Option<Retained<objc2_foundation::NSTimer>>>,
    window: RefCell<Option<Retained<NSWindow>>>,
    table: RefCell<Option<Retained<NSTableView>>>,
    /// First list column; its title flips between "Quit" (whitelist) and
    /// "Protect" (blacklist) because the checkbox meaning flips with it.
    quit_column: RefCell<Option<Retained<NSTableColumn>>>,
    apps_hint: RefCell<Option<Retained<NSTextField>>>,
    whitelist_radio: RefCell<Option<Retained<NSButton>>>,
    blacklist_radio: RefCell<Option<Retained<NSButton>>>,
    delay_popup: RefCell<Option<Retained<NSPopUpButton>>>,
    autostart_checkbox: RefCell<Option<Retained<NSButton>>>,
    enabled_checkbox: RefCell<Option<Retained<NSButton>>>,
    keep_alive_checkbox: RefCell<Option<Retained<NSButton>>>,
    auto_start_missing_checkbox: RefCell<Option<Retained<NSButton>>>,
    restart_delay_popup: RefCell<Option<Retained<NSPopUpButton>>>,
    loop_protection_checkbox: RefCell<Option<Retained<NSButton>>>,
    /// Menu bar target, informed after toggles so the icon and the menu
    /// checkmarks stay in sync with the settings window.
    tray: RefCell<Option<objc2::rc::Weak<crate::tray::TrayTarget>>>,
    /// Checking a Keep box launches the app right away when it is not
    /// running, so launch failures surface while it is being configured.
    keep_alive: RefCell<Option<std::rc::Rc<crate::keepalive::KeepAlive>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RQSettingsController"]
    #[ivars = SettingsIvars]
    pub struct SettingsController;

    unsafe impl NSObjectProtocol for SettingsController {}

    unsafe impl NSTableViewDataSource for SettingsController {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.ivars().entries.borrow().len() as NSInteger
        }
    }

    unsafe impl NSControlTextEditingDelegate for SettingsController {}

    unsafe impl NSWindowDelegate for SettingsController {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            self.finish_refresh();
            self.ivars().entries.borrow_mut().clear();
            self.ivars().all_entries.borrow_mut().clear();
            if let Some(field) = self.ivars().search_field.borrow().as_ref() {
                field.setStringValue(ns_string!(""));
            }
            if let Some(checkbox) = self.ivars().only_selected_checkbox.borrow().as_ref() {
                checkbox.setState(NSControlStateValueOff);
            }
            if let Some(table) = self.ivars().table.borrow().as_ref() {
                table.reloadData();
            }
        }
    }

    unsafe impl NSTableViewDelegate for SettingsController {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for_row(
            &self,
            _table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            self.make_cell_view(column, row)
        }
    }

    impl SettingsController {
        #[unsafe(method(onToggleApp:))]
        fn on_toggle_app(&self, sender: Option<&NSButton>) {
            let Some(sender) = sender else { return };
            let row = sender.tag() as usize;
            let bundle_id = {
                let entries = self.ivars().entries.borrow();
                let Some(entry) = entries.get(row) else { return };
                entry.bundle_id.clone()
            };
            let listed = sender.state() == NSControlStateValueOn;
            if let Err(err) = self.ivars().config.set_app_listed(&bundle_id, listed) {
                sender.setState(if listed {
                    NSControlStateValueOff
                } else {
                    NSControlStateValueOn
                });
                tracing::error!(%err, "cannot persist app list");
                return;
            }
            self.refresh_after_toggle();
        }

        #[unsafe(method(onToggleKeepApp:))]
        fn on_toggle_keep_app(&self, sender: Option<&NSButton>) {
            let Some(sender) = sender else { return };
            let row = sender.tag() as usize;
            let bundle_id = {
                let entries = self.ivars().entries.borrow();
                let Some(entry) = entries.get(row) else { return };
                entry.bundle_id.clone()
            };
            let listed = sender.state() == NSControlStateValueOn;
            if let Err(err) = self.ivars().config.set_keep_alive_listed(&bundle_id, listed) {
                sender.setState(if listed {
                    NSControlStateValueOff
                } else {
                    NSControlStateValueOn
                });
                tracing::error!(%err, "cannot persist keep-alive list");
                return;
            }
            if listed {
                // A kept app should be running; launching it now surfaces
                // failures at configuration time and lifts loop protection.
                if let Some(keep_alive) = self.ivars().keep_alive.borrow().as_ref() {
                    keep_alive.keep_checked(&bundle_id);
                }
            }
            self.refresh_after_toggle();
        }

        #[unsafe(method(onEnabledChanged:))]
        fn on_enabled_changed(&self, sender: Option<&NSButton>) {
            let Some(sender) = sender else { return };
            let wanted = sender.state() == NSControlStateValueOn;
            if let Err(err) = self.ivars().config.update(|config| config.enabled = wanted) {
                tracing::error!(%err, "cannot persist enabled state");
                self.sync_controls();
            }
            self.notify_tray();
        }

        #[unsafe(method(onKeepAliveEnabledChanged:))]
        fn on_keep_alive_enabled_changed(&self, sender: Option<&NSButton>) {
            let Some(sender) = sender else { return };
            let wanted = sender.state() == NSControlStateValueOn;
            if let Err(err) = self
                .ivars()
                .config
                .update(|config| config.keep_alive_enabled = wanted)
            {
                tracing::error!(%err, "cannot persist keep-alive state");
                self.sync_controls();
            }
            self.notify_tray();
        }

        #[unsafe(method(onAutoStartMissingKeptAppsChanged:))]
        fn on_auto_start_missing_kept_apps_changed(&self, sender: Option<&NSButton>) {
            let Some(sender) = sender else { return };
            let wanted = sender.state() == NSControlStateValueOn;
            match self
                .ivars()
                .config
                .update(|config| config.keep_alive_auto_start_missing = wanted)
            {
                Ok(()) => {
                    if wanted {
                        if let Some(keep_alive) = self.ivars().keep_alive.borrow().as_ref() {
                            keep_alive.reconcile_missing_apps();
                        }
                    }
                }
                Err(err) => {
                    tracing::error!(%err, "cannot persist automatic Keep launch state");
                    self.sync_controls();
                }
            }
        }

        #[unsafe(method(onLoopProtectionChanged:))]
        fn on_loop_protection_changed(&self, sender: Option<&NSButton>) {
            let Some(sender) = sender else { return };
            let wanted = sender.state() == NSControlStateValueOn;
            if let Err(err) = self
                .ivars()
                .config
                .update(|config| config.keep_alive_loop_protection = wanted)
            {
                tracing::error!(%err, "cannot persist loop protection state");
                self.sync_controls();
            }
        }

        #[unsafe(method(onRestartDelayChanged:))]
        fn on_restart_delay_changed(&self, sender: Option<&NSPopUpButton>) {
            let Some(sender) = sender else { return };
            let index = sender.indexOfSelectedItem();
            if let Some((secs, _)) = RESTART_DELAY_CHOICES.get(index as usize) {
                if let Err(err) = self
                    .ivars()
                    .config
                    .update(|config| config.keep_alive_delay_secs = *secs)
                {
                    tracing::error!(%err, "cannot persist restart delay");
                    self.sync_controls();
                }
            }
        }

        #[unsafe(method(onModeChanged:))]
        fn on_mode_changed(&self, sender: Option<&NSButton>) {
            let Some(sender) = sender else { return };
            let mode = if sender.tag() == 1 {
                FilterMode::Blacklist
            } else {
                FilterMode::Whitelist
            };
            if let Err(err) = self.ivars().config.update(|config| config.mode = mode) {
                tracing::error!(%err, "cannot persist mode");
            }
            // Right or wrong, reflect the stored state — and relabel the
            // Quit/Protect column for the new mode.
            self.sync_controls();
        }

        #[unsafe(method(onDelayChanged:))]
        fn on_delay_changed(&self, sender: Option<&NSPopUpButton>) {
            let Some(sender) = sender else { return };
            let index = sender.indexOfSelectedItem();
            if let Some((secs, _)) = DELAY_CHOICES.get(index as usize) {
                if let Err(err) = self
                    .ivars()
                    .config
                    .update(|config| config.quit_delay_secs = *secs)
                {
                    tracing::error!(%err, "cannot persist quit delay");
                    self.sync_controls();
                }
            }
        }

        #[unsafe(method(onAutostartChanged:))]
        fn on_autostart_changed(&self, sender: Option<&NSButton>) {
            let Some(sender) = sender else { return };
            let wanted = sender.state() == NSControlStateValueOn;
            if !crate::autostart::set_enabled(wanted) {
                // Revert when the system rejects the change.
                sender.setState(if crate::autostart::is_enabled() {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
            }
        }

        #[unsafe(method(onAddApp:))]
        fn on_add_app(&self, _sender: Option<&AnyObject>) {
            self.add_app_via_panel();
        }

        #[unsafe(method(onSearchChanged:))]
        fn on_search_changed(&self, _sender: Option<&AnyObject>) {
            self.apply_filter();
        }

        #[unsafe(method(onOnlySelectedChanged:))]
        fn on_only_selected_changed(&self, _sender: Option<&AnyObject>) {
            self.apply_filter();
        }

        #[unsafe(method(onOpenGitHub:))]
        fn on_open_github(&self, _sender: Option<&AnyObject>) {
            let url = objc2_foundation::NSURL::URLWithString(ns_string!(GITHUB_URL));
            if let Some(url) = url {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }
    }
);

impl SettingsController {
    pub fn new(mtm: MainThreadMarker, config: ConfigHandle) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SettingsIvars {
            config,
            entries: RefCell::new(Vec::new()),
            all_entries: RefCell::new(Vec::new()),
            search_field: RefCell::new(None),
            only_selected_checkbox: RefCell::new(None),
            scan_rx: RefCell::new(None),
            scan_timer: RefCell::new(None),
            window: RefCell::new(None),
            table: RefCell::new(None),
            quit_column: RefCell::new(None),
            apps_hint: RefCell::new(None),
            whitelist_radio: RefCell::new(None),
            blacklist_radio: RefCell::new(None),
            delay_popup: RefCell::new(None),
            autostart_checkbox: RefCell::new(None),
            enabled_checkbox: RefCell::new(None),
            keep_alive_checkbox: RefCell::new(None),
            auto_start_missing_checkbox: RefCell::new(None),
            restart_delay_popup: RefCell::new(None),
            loop_protection_checkbox: RefCell::new(None),
            tray: RefCell::new(None),
            keep_alive: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    pub fn set_keep_alive(&self, keep_alive: std::rc::Rc<crate::keepalive::KeepAlive>) {
        self.ivars().keep_alive.replace(Some(keep_alive));
    }

    /// Wires the menu bar target so toggles made here update the menu
    /// checkmarks and the icon dimming immediately.
    pub fn set_tray(&self, tray: Retained<crate::tray::TrayTarget>) {
        self.ivars().tray.replace(Some(objc2::rc::Weak::new(&tray)));
    }

    fn notify_tray(&self) {
        if let Some(tray) = self.ivars().tray.borrow().as_ref().and_then(|t| t.load()) {
            tray.sync_from_config();
        }
    }

    /// After a Quit/Keep checkbox toggle: with the selected-only filter
    /// active, an unchecked app must drop out of the list right away;
    /// otherwise the row is redrawn so the sibling checkbox greys out.
    fn refresh_after_toggle(&self) {
        let filter_active = self
            .ivars()
            .only_selected_checkbox
            .borrow()
            .as_ref()
            .is_some_and(|checkbox| checkbox.state() == NSControlStateValueOn);
        if filter_active {
            self.apply_filter();
        } else if let Some(table) = self.ivars().table.borrow().as_ref() {
            table.reloadData();
        }
    }

    fn make_cell_view(
        &self,
        column: Option<&NSTableColumn>,
        row: NSInteger,
    ) -> Option<Retained<NSView>> {
        let mtm = MainThreadMarker::from(self);
        let entries = self.ivars().entries.borrow();
        let entry = entries.get(row as usize)?;
        let identifier = column
            .map(|c| c.identifier().to_string())
            .unwrap_or_default();

        let config = self.ivars().config.data.borrow();
        let in_list = |list: &[String]| {
            list.iter()
                .any(|b| b.eq_ignore_ascii_case(&entry.bundle_id))
        };
        let checkbox_cell = |listed: bool, enabled: bool, action: Sel| {
            let checkbox = unsafe {
                NSButton::checkboxWithTitle_target_action(
                    ns_string!(""),
                    Some(self as &AnyObject),
                    Some(action),
                    mtm,
                )
            };
            checkbox.setState(if listed {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            checkbox.setEnabled(enabled);
            checkbox.setTag(row);
            // NSButton -> NSControl -> NSView
            Retained::into_super(Retained::into_super(checkbox))
        };

        // Quit and Keep are mutually exclusive per app: whichever side is
        // checked greys the other out (the engine would ignore the quit
        // entry anyway — keep-alive apps are never auto-quit).
        let quit_listed = in_list(&config.apps);
        let keep_listed = in_list(&config.keep_alive_apps);
        match identifier.as_str() {
            "quit" => Some(checkbox_cell(quit_listed, !keep_listed, sel!(onToggleApp:))),
            "keep" => Some(checkbox_cell(
                keep_listed,
                !quit_listed,
                sel!(onToggleKeepApp:),
            )),
            _ => {
                let row_stack = NSStackView::new(mtm);
                row_stack.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
                row_stack.setSpacing(6.0);

                if let Some(path) = &entry.icon_path {
                    let icon =
                        NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(path));
                    let image_view = NSImageView::imageViewWithImage(&icon, mtm);
                    NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&[
                        image_view.widthAnchor().constraintEqualToConstant(16.0),
                        image_view.heightAnchor().constraintEqualToConstant(16.0),
                    ]));
                    row_stack.addArrangedSubview(&image_view);
                }

                let label = NSTextField::labelWithString(&NSString::from_str(&entry.name), mtm);
                row_stack.addArrangedSubview(&label);

                // The bundle ID stays available as a tooltip, not as
                // visible noise.
                row_stack.setToolTip(Some(&NSString::from_str(&entry.bundle_id)));

                Some(Retained::into_super(row_stack))
            }
        }
    }

    fn add_app_via_panel(&self) {
        let mtm = MainThreadMarker::from(self);
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(false);
        panel.setAllowsMultipleSelection(false);
        panel.setDirectoryURL(Some(&objc2_foundation::NSURL::fileURLWithPath(ns_string!(
            "/Applications"
        ))));
        panel.setPrompt(Some(ns_string!("Add")));
        let response = panel.runModal();
        if response != NSModalResponseOK {
            return;
        }
        let Some(url) = panel.URL() else {
            return;
        };
        let Some(bundle) = NSBundle::bundleWithURL(&url) else {
            tracing::warn!("selection is not an app bundle");
            return;
        };
        let Some(bundle_id) = bundle.bundleIdentifier() else {
            tracing::warn!("app bundle without a bundle ID");
            return;
        };
        let bundle_id = bundle_id.to_string();
        let path = url.path().map(|p| p.to_string()).unwrap_or_default();
        if crate::protected::is_protected_bundle_id(&bundle_id)
            || crate::protected::is_protected_path(&path)
        {
            tracing::warn!("refused to add protected system app");
            show_protected_warning(mtm);
            return;
        }
        match self.ivars().config.set_app_listed(&bundle_id, true) {
            Ok(()) => self.refresh_entries(),
            Err(err) => tracing::error!(%err, "cannot persist app selection"),
        }
    }

    /// Starts a background scan of standard application locations.
    fn refresh_entries(&self) {
        if self.ivars().scan_rx.borrow().is_some() {
            return;
        }

        let (tx, rx) = mpsc::channel();
        self.ivars().scan_rx.replace(Some(rx));
        let spawn = std::thread::Builder::new()
            .name("rustquit-app-scan".to_string())
            .spawn(move || {
                let entries = autoreleasepool(|_| scan_standard_apps());
                let _ = tx.send(entries);
            });
        if let Err(err) = spawn {
            self.ivars().scan_rx.borrow_mut().take();
            tracing::error!(%err, "cannot start application scan");
            return;
        }

        let controller = Weak::new(self);
        let block = block2::RcBlock::new(
            move |_timer: std::ptr::NonNull<objc2_foundation::NSTimer>| {
                crate::engine::catch_callback_panic("application scan timer", || {
                    if let Some(controller) = controller.load() {
                        controller.poll_refresh();
                    }
                });
            },
        );
        let timer = unsafe {
            objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                0.05, true, &block,
            )
        };
        self.ivars().scan_timer.replace(Some(timer));
    }

    fn poll_refresh(&self) {
        let result = {
            let receiver = self.ivars().scan_rx.borrow();
            receiver.as_ref().map(Receiver::try_recv)
        };
        match result {
            Some(Ok(entries)) => {
                self.finish_refresh();
                self.complete_refresh(entries);
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => self.finish_refresh(),
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
        }
    }

    fn finish_refresh(&self) {
        self.ivars().scan_rx.borrow_mut().take();
        if let Some(timer) = self.ivars().scan_timer.borrow_mut().take() {
            timer.invalidate();
        }
    }

    /// Adds running apps and configured leftovers on the main thread, then
    /// publishes the completed list to the table.
    fn complete_refresh(&self, mut entries: Vec<AppEntry>) {
        let mut seen: std::collections::HashSet<String> = entries
            .iter()
            .map(|entry| entry.bundle_id.to_ascii_lowercase())
            .collect();

        // Running apps that are not in the standard folders.
        for app in engine::workspace::regular_running_apps() {
            let Some(bundle_id) = app.bundleIdentifier() else {
                continue;
            };
            let bundle_id = bundle_id.to_string();
            let seen_key = bundle_id.to_ascii_lowercase();
            if crate::protected::is_protected_bundle_id(&bundle_id) || seen.contains(&seen_key) {
                continue;
            }
            seen.insert(seen_key);
            let icon_path = app
                .bundleURL()
                .and_then(|url| url.path().map(|path| path.to_string()));
            entries.push(AppEntry {
                name: app
                    .localizedName()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| bundle_id.clone()),
                icon_path,
                bundle_id,
            });
        }

        // Configured apps not found anywhere (e.g. uninstalled): resolve
        // via Launch Services, otherwise fall back to the bare bundle ID.
        let config = self.ivars().config.data.borrow();
        let configured: Vec<String> = config
            .apps
            .iter()
            .chain(config.keep_alive_apps.iter())
            .cloned()
            .collect();
        drop(config);
        for bundle_id in configured.iter() {
            let seen_key = bundle_id.to_ascii_lowercase();
            if seen.contains(&seen_key) || crate::protected::is_protected_bundle_id(bundle_id) {
                continue;
            }
            let workspace = NSWorkspace::sharedWorkspace();
            let resolved = workspace
                .URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle_id))
                .and_then(|url| url.path().map(|p| p.to_string()))
                .and_then(|path| app_entry_from_path(std::path::Path::new(&path)));
            entries.push(resolved.unwrap_or_else(|| AppEntry {
                name: bundle_id.clone(),
                icon_path: None,
                bundle_id: bundle_id.clone(),
            }));
            seen.insert(seen_key);
        }

        entries.sort_by_key(|entry| entry.name.to_lowercase());
        self.ivars().all_entries.replace(entries);
        self.apply_filter();
    }

    /// Publishes the entries matching the search field (and, when enabled,
    /// the selected-only toggle) to the table.
    fn apply_filter(&self) {
        let query = self
            .ivars()
            .search_field
            .borrow()
            .as_ref()
            .map(|field| field.stringValue().to_string().to_lowercase())
            .unwrap_or_default();
        let only_selected = self
            .ivars()
            .only_selected_checkbox
            .borrow()
            .as_ref()
            .is_some_and(|checkbox| checkbox.state() == NSControlStateValueOn);
        let config = self.ivars().config.data.borrow();
        let all = self.ivars().all_entries.borrow();
        let filtered: Vec<AppEntry> = all
            .iter()
            .filter(|entry| {
                let selected = |list: &[String]| {
                    list.iter()
                        .any(|b| b.eq_ignore_ascii_case(&entry.bundle_id))
                };
                if only_selected && !selected(&config.apps) && !selected(&config.keep_alive_apps) {
                    return false;
                }
                query.is_empty()
                    || entry.name.to_lowercase().contains(&query)
                    || entry.bundle_id.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        drop(all);
        drop(config);
        self.ivars().entries.replace(filtered);
        if let Some(table) = self.ivars().table.borrow().as_ref() {
            table.reloadData();
        }
    }

    /// Shows the window (builds it on first call).
    pub fn show(&self) {
        let mtm = MainThreadMarker::from(self);
        if self.ivars().window.borrow().is_none() {
            self.build_window(mtm);
        }
        if self.ivars().entries.borrow().is_empty() {
            self.refresh_entries();
        }
        self.sync_controls();
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.makeKeyAndOrderFront(None);
            #[allow(deprecated)]
            NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        }
    }

    /// Sets radios/popup/checkbox to the current config state.
    fn sync_controls(&self) {
        let ivars = self.ivars();
        let config = ivars.config.data.borrow();
        let (on, off) = (NSControlStateValueOn, NSControlStateValueOff);
        if let Some(radio) = ivars.whitelist_radio.borrow().as_ref() {
            radio.setState(if config.mode == FilterMode::Whitelist {
                on
            } else {
                off
            });
        }
        if let Some(radio) = ivars.blacklist_radio.borrow().as_ref() {
            radio.setState(if config.mode == FilterMode::Blacklist {
                on
            } else {
                off
            });
        }
        if let Some(popup) = ivars.delay_popup.borrow().as_ref() {
            let index = DELAY_CHOICES
                .iter()
                .position(|(secs, _)| (secs - config.quit_delay_secs).abs() < 0.01)
                .unwrap_or(DEFAULT_DELAY_INDEX);
            popup.selectItemAtIndex(index as NSInteger);
        }
        if let Some(checkbox) = ivars.autostart_checkbox.borrow().as_ref() {
            checkbox.setState(if crate::autostart::is_enabled() {
                on
            } else {
                off
            });
        }
        if let Some(checkbox) = ivars.enabled_checkbox.borrow().as_ref() {
            checkbox.setState(if config.enabled { on } else { off });
        }
        if let Some(checkbox) = ivars.keep_alive_checkbox.borrow().as_ref() {
            checkbox.setState(if config.keep_alive_enabled { on } else { off });
        }
        if let Some(checkbox) = ivars.auto_start_missing_checkbox.borrow().as_ref() {
            checkbox.setState(if config.keep_alive_auto_start_missing {
                on
            } else {
                off
            });
        }
        if let Some(checkbox) = ivars.loop_protection_checkbox.borrow().as_ref() {
            checkbox.setState(if config.keep_alive_loop_protection {
                on
            } else {
                off
            });
        }
        if let Some(popup) = ivars.restart_delay_popup.borrow().as_ref() {
            let index = RESTART_DELAY_CHOICES
                .iter()
                .position(|(secs, _)| (secs - config.keep_alive_delay_secs).abs() < 0.01)
                .unwrap_or(DEFAULT_RESTART_DELAY_INDEX);
            popup.selectItemAtIndex(index as NSInteger);
        }

        // In blacklist mode the checkbox means the opposite ("spare this
        // app"), so the column title and the explanation flip with it.
        let (column_title, hint) = match config.mode {
            FilterMode::Whitelist => (
                "Quit",
                "Quit: close the app once its last window is closed. \
                 Keep: relaunch the app whenever it quits. \
                 An app can only use one of the two.",
            ),
            FilterMode::Blacklist => (
                "Protect",
                "Protect: never quit this app; all unchecked apps are closed \
                 once their last window closes. \
                 Keep: additionally relaunch the app whenever it quits. \
                 An app can only use one of the two.",
            ),
        };
        if let Some(column) = ivars.quit_column.borrow().as_ref() {
            column.setTitle(&NSString::from_str(column_title));
        }
        // setTitle alone does not redraw the header until some other event
        // does; force it so the mode switch is reflected immediately.
        if let Some(table) = ivars.table.borrow().as_ref() {
            if let Some(header) = table.headerView() {
                header.setNeedsDisplay(true);
            }
        }
        if let Some(label) = ivars.apps_hint.borrow().as_ref() {
            label.setStringValue(&NSString::from_str(hint));
        }
    }

    fn build_window(&self, mtm: MainThreadMarker) {
        let self_obj: &AnyObject = self;
        let stack = NSStackView::new(mtm);
        stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        stack.setAlignment(objc2_app_kit::NSLayoutAttribute::Leading);
        stack.setSpacing(8.0);
        stack.setTranslatesAutoresizingMaskIntoConstraints(false);

        let checkbox = |title: &str, action: Sel| unsafe {
            NSButton::checkboxWithTitle_target_action(
                &NSString::from_str(title),
                Some(self_obj),
                Some(action),
                mtm,
            )
        };
        let delay_popup = |choices: &[(f64, &str)], action: Sel| {
            let popup = NSPopUpButton::initWithFrame_pullsDown(
                NSPopUpButton::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(160.0, 25.0)),
                false,
            );
            for (_, title) in choices {
                popup.addItemWithTitle(&NSString::from_str(title));
            }
            unsafe {
                popup.setTarget(Some(self_obj));
                popup.setAction(Some(action));
            }
            popup
        };
        let labeled_row = |label: &str, control: &NSView| {
            let row = NSStackView::new(mtm);
            row.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
            row.setSpacing(8.0);
            row.addArrangedSubview(&NSTextField::labelWithString(
                &NSString::from_str(label),
                mtm,
            ));
            row.addArrangedSubview(control);
            row
        };
        let end_section = |view: &NSView| {
            stack.setCustomSpacing_afterView(SECTION_GAP, view);
        };

        // — General —
        stack.addArrangedSubview(&bold_label("General", mtm));
        let autostart = checkbox("Launch at login", sel!(onAutostartChanged:));
        stack.addArrangedSubview(&autostart);
        end_section(&autostart);
        self.ivars().autostart_checkbox.replace(Some(autostart));

        // — Auto-Quit —
        stack.addArrangedSubview(&bold_label("Auto-Quit", mtm));
        let enabled = checkbox(
            "Quit apps when their last window closes",
            sel!(onEnabledChanged:),
        );
        stack.addArrangedSubview(&enabled);
        self.ivars().enabled_checkbox.replace(Some(enabled));

        let whitelist_radio = radio(
            "Quit only the selected apps (whitelist)",
            0,
            self_obj,
            sel!(onModeChanged:),
            mtm,
        );
        let blacklist_radio = radio(
            "Quit all apps except the selected ones (blacklist)",
            1,
            self_obj,
            sel!(onModeChanged:),
            mtm,
        );
        stack.addArrangedSubview(&whitelist_radio);
        stack.addArrangedSubview(&blacklist_radio);
        self.ivars().whitelist_radio.replace(Some(whitelist_radio));
        self.ivars().blacklist_radio.replace(Some(blacklist_radio));

        let popup = delay_popup(DELAY_CHOICES, sel!(onDelayChanged:));
        let delay_row = labeled_row("Quit delay:", &popup);
        stack.addArrangedSubview(&delay_row);
        end_section(&delay_row);
        self.ivars().delay_popup.replace(Some(popup));

        // — Keep Running —
        stack.addArrangedSubview(&bold_label("Keep Running", mtm));
        let keep_alive = checkbox(
            "Relaunch the selected apps when they quit",
            sel!(onKeepAliveEnabledChanged:),
        );
        stack.addArrangedSubview(&keep_alive);
        self.ivars().keep_alive_checkbox.replace(Some(keep_alive));

        let auto_start_missing = checkbox(
            "Automatically start missing kept apps",
            sel!(onAutoStartMissingKeptAppsChanged:),
        );
        stack.addArrangedSubview(&auto_start_missing);
        self.ivars()
            .auto_start_missing_checkbox
            .replace(Some(auto_start_missing));

        let restart_popup = delay_popup(RESTART_DELAY_CHOICES, sel!(onRestartDelayChanged:));
        let restart_row = labeled_row("Restart delay:", &restart_popup);
        stack.addArrangedSubview(&restart_row);
        self.ivars()
            .restart_delay_popup
            .replace(Some(restart_popup));

        let loop_protection = checkbox("Restart-loop protection", sel!(onLoopProtectionChanged:));
        stack.addArrangedSubview(&loop_protection);
        self.ivars()
            .loop_protection_checkbox
            .replace(Some(loop_protection));
        let loop_hint = hint_label(
            "After two automatic restarts within an hour (e.g. an app that \
             crashes right after launching, or one you quit on purpose), the \
             app is left alone until the next day.",
            mtm,
        );
        stack.addArrangedSubview(&loop_hint);
        end_section(&loop_hint);

        // — Apps —
        stack.addArrangedSubview(&bold_label("Apps", mtm));
        // Text and column title are set by sync_controls (mode-dependent).
        let apps_hint = hint_label("", mtm);
        stack.addArrangedSubview(&apps_hint);
        self.ivars().apps_hint.replace(Some(apps_hint));

        let only_selected = checkbox("Show only selected apps", sel!(onOnlySelectedChanged:));
        stack.addArrangedSubview(&only_selected);
        self.ivars()
            .only_selected_checkbox
            .replace(Some(only_selected));

        let search = objc2_app_kit::NSSearchField::new(mtm);
        unsafe {
            search.setSendsWholeSearchString(false);
            search.setSendsSearchStringImmediately(true);
            search.setTarget(Some(self_obj));
            search.setAction(Some(sel!(onSearchChanged:)));
        }
        search.setTranslatesAutoresizingMaskIntoConstraints(false);
        stack.addArrangedSubview(&search);
        self.ivars().search_field.replace(Some(search.clone()));

        let table = NSTableView::new(mtm);
        for (identifier, title, width) in [
            ("quit", "Quit", 52.0), // wide enough for the "Protect" title
            ("keep", "Keep", 40.0),
            ("app", "App", 360.0),
        ] {
            let column = NSTableColumn::initWithIdentifier(
                NSTableColumn::alloc(mtm),
                &NSString::from_str(identifier),
            );
            column.setTitle(&NSString::from_str(title));
            column.setWidth(width);
            table.addTableColumn(&column);
            if identifier == "quit" {
                self.ivars().quit_column.replace(Some(column));
            }
        }
        table.setRowHeight(22.0);
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(self)));
            table.setDelegate(Some(ProtocolObject::from_ref(self)));
        }

        let scroll = NSScrollView::new(mtm);
        scroll.setDocumentView(Some(&table));
        scroll.setHasVerticalScroller(true);
        scroll.setTranslatesAutoresizingMaskIntoConstraints(false);
        stack.addArrangedSubview(&scroll);
        self.ivars().table.replace(Some(table));

        let add_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                ns_string!("Add App…"),
                Some(self_obj),
                Some(sel!(onAddApp:)),
                mtm,
            )
        };
        stack.addArrangedSubview(&add_button);

        // Footer: version and project link.
        let footer = NSStackView::new(mtm);
        footer.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        footer.setSpacing(6.0);
        let version_label = NSTextField::labelWithString(
            &NSString::from_str(&format!("RustQuit v{}", env!("CARGO_PKG_VERSION"))),
            mtm,
        );
        version_label.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(11.0)));
        version_label.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
        footer.addArrangedSubview(&version_label);
        let link_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                ns_string!("github.com/asd123ch/rustquit"),
                Some(self_obj),
                Some(sel!(onOpenGitHub:)),
                mtm,
            )
        };
        link_button.setBordered(false);
        link_button.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(11.0)));
        link_button.setContentTintColor(Some(&objc2_app_kit::NSColor::linkColor()));
        footer.addArrangedSubview(&link_button);
        stack.addArrangedSubview(&footer);

        // Window
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(530.0, 780.0));
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(ns_string!("RustQuit Settings"));
        unsafe { window.setReleasedWhenClosed(false) };
        window.setDelegate(Some(ProtocolObject::from_ref(self)));

        let content = window.contentView().expect("window has a contentView");
        content.addSubview(&stack);
        NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&[
            stack
                .topAnchor()
                .constraintEqualToAnchor_constant(&content.topAnchor(), 20.0),
            stack
                .leadingAnchor()
                .constraintEqualToAnchor_constant(&content.leadingAnchor(), 20.0),
            stack
                .trailingAnchor()
                .constraintEqualToAnchor_constant(&content.trailingAnchor(), -20.0),
            stack
                .bottomAnchor()
                .constraintEqualToAnchor_constant(&content.bottomAnchor(), -20.0),
            scroll
                .heightAnchor()
                .constraintGreaterThanOrEqualToConstant(240.0),
            scroll
                .widthAnchor()
                .constraintEqualToAnchor(&stack.widthAnchor()),
            search
                .widthAnchor()
                .constraintEqualToAnchor(&stack.widthAnchor()),
        ]));

        window.center();
        self.ivars().window.replace(Some(window));
    }
}

fn scan_standard_apps() -> Vec<AppEntry> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut app_dirs = vec![
        std::path::PathBuf::from("/Applications"),
        std::path::PathBuf::from("/System/Applications"),
        std::path::PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(home) = crate::config::home_dir() {
        app_dirs.push(home.join("Applications"));
    }
    for dir in app_dirs {
        let mut paths = Vec::new();
        collect_app_paths(&dir, 2, &mut paths);
        for path in paths {
            if let Some(entry) = app_entry_from_path(&path) {
                if seen.insert(entry.bundle_id.to_ascii_lowercase()) {
                    entries.push(entry);
                }
            }
        }
    }
    entries
}

/// Modal warning shown when the user tries to add a protected system app.
fn show_protected_warning(mtm: MainThreadMarker) {
    let alert = objc2_app_kit::NSAlert::new(mtm);
    alert.setAlertStyle(objc2_app_kit::NSAlertStyle::Warning);
    alert.setMessageText(ns_string!("This app cannot be added"));
    alert.setInformativeText(ns_string!(
        "This is a macOS system component. Quitting it automatically \
         could make your Mac unusable, so RustQuit never manages it."
    ));
    alert.addButtonWithTitle(ns_string!("OK"));
    alert.runModal();
}

/// Builds an AppEntry (bundle ID, localized name, icon) from an .app path.
fn app_entry_from_path(path: &std::path::Path) -> Option<AppEntry> {
    let path_str = path.to_str()?;
    let ns_path = NSString::from_str(path_str);
    let bundle = NSBundle::bundleWithPath(&ns_path)?;
    let bundle_id = bundle.bundleIdentifier()?.to_string();
    if crate::protected::is_protected_bundle_id(&bundle_id)
        || crate::protected::is_protected_path(path_str)
    {
        return None;
    }
    let file_manager = objc2_foundation::NSFileManager::defaultManager();
    let name = file_manager.displayNameAtPath(&ns_path).to_string();
    let name = name.strip_suffix(".app").unwrap_or(&name).to_string();
    Some(AppEntry {
        bundle_id,
        name,
        icon_path: Some(path_str.to_string()),
    })
}

fn collect_app_paths(dir: &std::path::Path, depth: usize, paths: &mut Vec<std::path::PathBuf>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for item in read_dir.flatten() {
        let path = item.path();
        let hidden = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| name.starts_with('.'));
        if hidden {
            continue;
        }
        if path.extension().is_some_and(|extension| extension == "app") {
            paths.push(path);
        } else if depth > 0 && path.is_dir() && !path.is_symlink() {
            collect_app_paths(&path, depth - 1, paths);
        }
    }
}

fn bold_label(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFont(Some(&objc2_app_kit::NSFont::boldSystemFontOfSize(13.0)));
    label
}

/// Small, gray, wrapping explanation text under a control.
fn hint_label(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    label.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(11.0)));
    label.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
    label.setSelectable(false);
    label.setPreferredMaxLayoutWidth(490.0);
    label
}

fn radio(
    title: &str,
    tag: NSInteger,
    target: &AnyObject,
    action: Sel,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let button = unsafe {
        NSButton::radioButtonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action),
            mtm,
        )
    };
    button.setTag(tag);
    button
}
