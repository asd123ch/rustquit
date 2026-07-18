//! Keep-alive: relaunches selected apps when they terminate, while treating
//! related external background helpers as satisfying Keep.
//!
//! Terminations are detected by observing `NSWorkspace.runningApplications`
//! via key-value observing and diffing against the last snapshot. The
//! `NSWorkspaceDidTerminateApplicationNotification` would be simpler, but
//! macOS does not post it for menu bar apps (`LSUIElement`) — which is
//! exactly what most kept apps are.
//!
//! Deliberately independent of the quit engine: it keeps working while the
//! Accessibility permission is missing or the engine is restarting. The
//! engine never auto-quits an app on the keep-alive list, so RustQuit
//! cannot fight itself.
//!
//! Loop protection (optional): after LOOP_LIMIT automatic restarts within
//! LOOP_WINDOW, keep-alive for that app pauses for PAUSE ("until the next
//! day") — an app that crashes right back at launch, or one the user is
//! actively trying to get rid of, stops being resurrected.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{
    NSKeyValueObservingOptions, NSObject, NSObjectNSKeyValueObserverRegistration, NSObjectProtocol,
    NSString, NSTimer, ns_string,
};

use crate::config::ConfigHandle;
use crate::engine::recent::{RecentQuitsHandle, RecentReason};
use crate::procexit::{ExitKind, ProcExitHandle};

/// Restarts within this window count toward the loop-protection limit.
const LOOP_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Automatic restarts allowed per window before keep-alive pauses.
const LOOP_LIMIT: usize = 2;
/// How long a loop-protected app stays paused.
const PAUSE: Duration = Duration::from_secs(24 * 60 * 60);
/// An app that RustQuit itself quit this recently is never restarted
/// (cannot happen while the app is keep-alive-listed, but the lists may
/// have changed in between — defense in depth).
const RECENT_QUIT_GRACE: Duration = Duration::from_secs(60);

pub struct KeepAlive {
    config: ConfigHandle,
    recent_quits: RecentQuitsHandle,
    /// Exit-status source; `None` if the kqueue could not be created
    /// (keep-alive then only relaunches menu bar apps).
    proc_exit: Option<ProcExitHandle>,
    /// Timestamps of automatic restarts, per lowercase bundle ID.
    restarts: RefCell<HashMap<String, Vec<Instant>>>,
    paused_until: RefCell<HashMap<String, Instant>>,
}

/// Keeps the KVO registration alive; must live until exit.
pub struct KeepAliveGuard {
    keep_alive: Rc<KeepAlive>,
    observer: Retained<RunningAppsObserver>,
}

impl KeepAliveGuard {
    pub fn handle(&self) -> Rc<KeepAlive> {
        self.keep_alive.clone()
    }
}

impl Drop for KeepAliveGuard {
    fn drop(&mut self) {
        unsafe {
            NSWorkspace::sharedWorkspace()
                .removeObserver_forKeyPath(&self.observer, ns_string!("runningApplications"));
        }
    }
}

/// What keep-alive remembers about a running app.
#[derive(Clone)]
struct KnownApp {
    bundle_id: String,
    name: String,
    /// Non-regular activation policy at the last snapshot = the app lives
    /// in the menu bar (or fully in the background), not in the Dock.
    menu_bar: bool,
}

pub struct ObserverIvars {
    keep_alive: std::rc::Weak<KeepAlive>,
    /// Last snapshot of running apps; the KVO callback diffs against it
    /// to find terminations (and their pre-death activation policy).
    known: RefCell<HashMap<libc::pid_t, KnownApp>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RQRunningAppsObserver"]
    #[ivars = ObserverIvars]
    pub struct RunningAppsObserver;

    unsafe impl NSObjectProtocol for RunningAppsObserver {}

    impl RunningAppsObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            _key_path: Option<&NSString>,
            _object: Option<&AnyObject>,
            _change: Option<&AnyObject>,
            _context: *mut c_void,
        ) {
            crate::engine::catch_callback_panic("keep-alive running-apps observer", || {
                self.diff_running_apps();
            });
        }
    }
);

impl RunningAppsObserver {
    fn new(
        mtm: MainThreadMarker,
        keep_alive: std::rc::Weak<KeepAlive>,
        initial: HashMap<libc::pid_t, KnownApp>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ObserverIvars {
            keep_alive,
            known: RefCell::new(initial),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Compares the current running apps to the last snapshot: reports
    /// every disappeared app and watches every appeared keep-listed one.
    fn diff_running_apps(&self) {
        let current = snapshot_running_apps();
        let (terminated, appeared) = {
            let known = self.ivars().known.borrow();
            let terminated: Vec<(libc::pid_t, KnownApp)> = known
                .iter()
                .filter(|(pid, _)| !current.contains_key(pid))
                .map(|(pid, app)| (*pid, app.clone()))
                .collect();
            let appeared: Vec<(libc::pid_t, String)> = current
                .iter()
                .filter(|(pid, _)| !known.contains_key(pid))
                .map(|(pid, app)| (*pid, app.bundle_id.clone()))
                .collect();
            (terminated, appeared)
        };
        self.ivars().known.replace(current);
        if let Some(keep_alive) = self.ivars().keep_alive.upgrade() {
            for (pid, bundle_id) in appeared {
                keep_alive.watch_if_listed(pid, &bundle_id);
            }
            for (pid, app) in terminated {
                keep_alive.on_terminated(app.bundle_id, app.name, pid, app.menu_bar);
            }
        }
    }
}

fn snapshot_running_apps() -> HashMap<libc::pid_t, KnownApp> {
    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter_map(|app| {
            let bundle_id = app.bundleIdentifier()?;
            Some((
                app.processIdentifier(),
                KnownApp {
                    bundle_id: bundle_id.to_string(),
                    name: app
                        .localizedName()
                        .map(|name| name.to_string())
                        .unwrap_or_else(|| bundle_id.to_string()),
                    menu_bar: app.activationPolicy() != NSApplicationActivationPolicy::Regular,
                },
            ))
        })
        .collect()
}

/// Seconds after startup before missing keep-alive apps are launched;
/// gives login items a head start so nothing is started twice.
const RECONCILE_DELAY_SECS: f64 = 5.0;

pub fn setup(config: ConfigHandle, recent_quits: RecentQuitsHandle) -> KeepAliveGuard {
    let mtm = MainThreadMarker::new().expect("keep-alive setup runs on the main thread");
    let proc_exit = match crate::procexit::ProcExitWatcher::spawn() {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::error!(%err, "cannot watch process exits; crash detection disabled");
            None
        }
    };
    let keep_alive = Rc::new(KeepAlive {
        config,
        recent_quits,
        proc_exit,
        restarts: RefCell::new(HashMap::new()),
        paused_until: RefCell::new(HashMap::new()),
    });

    // Persistent ignores survive a RustQuit restart. Recreate their expiry
    // timers so a missing kept app is restored when its 24 hours are over.
    let ignored: Vec<(String, u64)> = keep_alive
        .config
        .data
        .borrow()
        .keep_alive_ignored_until
        .iter()
        .map(|(bundle_id, until)| (bundle_id.clone(), *until))
        .collect();
    for (bundle_id, until) in ignored {
        keep_alive.schedule_ignore_expiry(bundle_id, until);
    }

    // KVO on runningApplications instead of the DidTerminate notification:
    // the notification is not posted for LSUIElement (menu bar) apps.
    let initial = snapshot_running_apps();
    for (pid, app) in &initial {
        keep_alive.watch_if_listed(*pid, &app.bundle_id);
    }
    let observer = RunningAppsObserver::new(mtm, Rc::downgrade(&keep_alive), initial);
    unsafe {
        NSWorkspace::sharedWorkspace().addObserver_forKeyPath_options_context(
            &observer,
            ns_string!("runningApplications"),
            NSKeyValueObservingOptions::empty(),
            std::ptr::null_mut(),
        );
    }

    // Bring missing keep-alive apps up shortly after startup.
    let weak = Rc::downgrade(&keep_alive);
    let reconcile_block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        crate::engine::catch_callback_panic("keep-alive reconcile timer", || {
            if let Some(keep_alive) = weak.upgrade() {
                keep_alive.reconcile_missing_apps();
            }
        });
    });
    let _ = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(
            RECONCILE_DELAY_SECS,
            false,
            &reconcile_block,
        )
    };

    KeepAliveGuard {
        keep_alive,
        observer,
    }
}

impl KeepAlive {
    /// Registers a keep-listed running app with the exit watcher, so its
    /// eventual exit status (crash vs. deliberate quit) is captured.
    fn watch_if_listed(&self, pid: libc::pid_t, bundle_id: &str) {
        let listed = is_listed(&self.config.data.borrow().keep_alive_apps, bundle_id);
        if listed {
            if let Some(proc_exit) = &self.proc_exit {
                proc_exit.watch(pid);
            }
        }
    }

    fn on_terminated(
        self: &Rc<Self>,
        bundle_id: String,
        name: String,
        pid: libc::pid_t,
        menu_bar: bool,
    ) {
        let (enabled, listed, delay) = {
            let config = self.config.data.borrow();
            (
                config.keep_alive_enabled,
                is_listed(&config.keep_alive_apps, &bundle_id),
                config.keep_alive_delay_secs,
            )
        };
        if !listed {
            return;
        }
        self.recent_quits
            .push_keep_termination(bundle_id.clone(), name);
        if !enabled {
            return;
        }
        tracing::info!(
            bundle_id,
            delay,
            "keep-alive: app terminated, restart check scheduled"
        );
        let weak = Rc::downgrade(self);
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            crate::engine::catch_callback_panic("keep-alive restart timer", || {
                if let Some(keep_alive) = weak.upgrade() {
                    keep_alive.restart(&bundle_id, pid, menu_bar);
                }
            });
        });
        let _ =
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(delay, false, &block) };
    }

    /// Runs after the delay; everything is re-checked at fire time. This
    /// is the only path that counts toward loop protection — startup
    /// reconciliation and the user checking a Keep box do not.
    fn restart(&self, bundle_id: &str, pid: libc::pid_t, menu_bar: bool) {
        // Never resurrect an app RustQuit itself just quit.
        let self_quit = self.recent_quits.snapshot().iter().any(|quit| {
            quit.reason == RecentReason::AutoQuit
                && quit.bundle_id.eq_ignore_ascii_case(bundle_id)
                && quit.when.elapsed() < RECENT_QUIT_GRACE
        });
        if self_quit {
            tracing::debug!(bundle_id, "keep-alive: app was auto-quit, not restarting");
            return;
        }
        let exit_kind = self
            .proc_exit
            .as_ref()
            .and_then(|proc_exit| proc_exit.take_exit_kind(pid));
        let auto_start_missing = self.config.data.borrow().keep_alive_auto_start_missing;
        // Without the always-running option, regular apps are only restored
        // after crashes. Menu bar apps retain their original Keep behavior.
        if !menu_bar && !auto_start_missing {
            match exit_kind {
                Some(ExitKind::Crashed) => {
                    tracing::info!(bundle_id, "keep-alive: app crashed, relaunching");
                }
                Some(ExitKind::Clean) => {
                    tracing::info!(
                        bundle_id,
                        "keep-alive: app quit on its own, not relaunching"
                    );
                    return;
                }
                None => {
                    tracing::info!(
                        bundle_id,
                        "keep-alive: exit status unknown, treating as a normal quit"
                    );
                    return;
                }
            }
        }
        if self.is_paused(bundle_id) {
            return;
        }

        let key = bundle_id.to_ascii_lowercase();
        if self.config.data.borrow().keep_alive_loop_protection {
            let recent = {
                let mut restarts = self.restarts.borrow_mut();
                let list = restarts.entry(key.clone()).or_default();
                list.retain(|t| t.elapsed() < LOOP_WINDOW);
                list.len()
            };
            if recent >= LOOP_LIMIT {
                self.paused_until
                    .borrow_mut()
                    .insert(key, Instant::now() + PAUSE);
                tracing::warn!(
                    bundle_id,
                    "keep-alive paused for a day after repeated restarts \
                     (re-checking the Keep box lifts the pause)"
                );
                return;
            }
        }
        if self.launch_if_needed(bundle_id) {
            self.restarts
                .borrow_mut()
                .entry(key)
                .or_default()
                .push(Instant::now());
        }
    }

    /// The user just checked the Keep box: that explicit intent overrides
    /// loop protection, but respects a separately requested 24-hour ignore.
    /// Otherwise the launch happens right away, so failures surface while
    /// Keep is being configured rather than at the first crash.
    pub fn keep_checked(&self, bundle_id: &str) {
        let key = bundle_id.to_ascii_lowercase();
        self.paused_until.borrow_mut().remove(&key);
        self.restarts.borrow_mut().remove(&key);
        // An already-running instance needs its exit watched from now on.
        self.watch_running_instances(bundle_id);
        self.launch_if_needed(bundle_id);
    }

    /// Restores kept apps that are not currently running when the
    /// always-running option is active.
    pub fn reconcile_missing_apps(&self) {
        if !self.config.data.borrow().keep_alive_auto_start_missing {
            tracing::debug!("automatic launch of missing kept apps disabled");
            return;
        }
        self.reconcile();
    }

    /// Launches every non-ignored keep-alive app that is not running.
    pub fn reconcile(&self) {
        let listed: Vec<String> = self.config.data.borrow().keep_alive_apps.clone();
        for bundle_id in listed {
            if !self.is_paused(&bundle_id) {
                self.launch_if_needed(&bundle_id);
            }
        }
    }

    /// Launches the app hidden unless it is already running. Returns
    /// whether a launch was requested.
    ///
    /// Hidden is the whole trick for close-to-background suites: the
    /// process comes back (menu bar icons are unaffected by the hidden
    /// state), but no settings or main window pops up. A regular app
    /// relaunched this way sits in the Dock; one click shows its window.
    fn launch_if_needed(&self, bundle_id: &str) -> bool {
        {
            let config = self.config.data.borrow();
            if !config.keep_alive_enabled || !is_listed(&config.keep_alive_apps, bundle_id) {
                return false;
            }
        }
        if self.ignore_remaining(bundle_id).is_some() {
            tracing::debug!(bundle_id, "keep-alive: app ignored temporarily");
            return false;
        }

        // Updaters usually relaunch the app themselves; never start a
        // second instance.
        let ns_bundle_id = NSString::from_str(bundle_id);
        let running = NSRunningApplication::runningApplicationsWithBundleIdentifier(&ns_bundle_id);
        if !running.is_empty() {
            tracing::debug!(bundle_id, "keep-alive: app is already running");
            return false;
        }

        let workspace = NSWorkspace::sharedWorkspace();
        let Some(url) = workspace.URLForApplicationWithBundleIdentifier(&ns_bundle_id) else {
            tracing::warn!(bundle_id, "keep-alive: app not found, cannot restart");
            return false;
        };
        let target_path = url.path().map(|path| path.to_string()).unwrap_or_default();
        if background_suite_running(bundle_id, &target_path) {
            tracing::debug!(
                bundle_id,
                "keep-alive: related background process is already running"
            );
            return false;
        }
        // Launch hidden and without stealing focus; errors surface in the
        // completion handler.
        let configuration = NSWorkspaceOpenConfiguration::configuration();
        configuration.setActivates(false);
        configuration.setHides(true);
        configuration.setAddsToRecentItems(false);
        let for_log = bundle_id.to_string();
        let completion = RcBlock::new(
            move |app: *mut NSRunningApplication, error: *mut objc2_foundation::NSError| {
                if app.is_null() {
                    let reason = unsafe { error.as_ref() }
                        .map(|e| e.localizedDescription().to_string())
                        .unwrap_or_else(|| "unknown error".to_string());
                    tracing::warn!(bundle_id = %for_log, %reason, "keep-alive: launch failed");
                } else {
                    tracing::info!(bundle_id = %for_log, "keep-alive: app launched hidden");
                }
            },
        );
        workspace.openApplicationAtURL_configuration_completionHandler(
            &url,
            &configuration,
            Some(&completion),
        );
        true
    }

    fn is_paused(&self, bundle_id: &str) -> bool {
        if self.ignore_remaining(bundle_id).is_some() {
            return true;
        }
        let key = bundle_id.to_ascii_lowercase();
        let mut paused = self.paused_until.borrow_mut();
        match paused.get(&key) {
            Some(until) if Instant::now() < *until => true,
            Some(_) => {
                paused.remove(&key);
                false
            }
            None => false,
        }
    }

    /// Suppresses every automatic Keep launch for 24 hours. Persisting the
    /// Unix deadline means restarting RustQuit cannot bypass the user's pause.
    pub fn ignore_for_24_hours(self: &Rc<Self>, bundle_id: &str) -> std::io::Result<()> {
        let until = crate::config::unix_timestamp().saturating_add(PAUSE.as_secs());
        self.config
            .set_keep_alive_ignored_until(bundle_id, Some(until))?;
        self.restarts
            .borrow_mut()
            .remove(&bundle_id.to_ascii_lowercase());
        self.schedule_ignore_expiry(bundle_id.to_string(), until);
        tracing::info!(bundle_id, "keep-alive ignored for 24 hours");
        Ok(())
    }

    /// Removes a manual ignore and loop-protection pause, then immediately
    /// restores the Keep invariant for the app.
    pub fn resume_now(&self, bundle_id: &str) -> std::io::Result<()> {
        self.config.set_keep_alive_ignored_until(bundle_id, None)?;
        let key = bundle_id.to_ascii_lowercase();
        self.paused_until.borrow_mut().remove(&key);
        self.restarts.borrow_mut().remove(&key);
        self.watch_running_instances(bundle_id);
        self.launch_if_needed(bundle_id);
        tracing::info!(bundle_id, "keep-alive resumed by user");
        Ok(())
    }

    /// Remaining manual-ignore duration, or `None` when launch is allowed.
    pub fn ignore_remaining(&self, bundle_id: &str) -> Option<Duration> {
        let key = bundle_id.to_ascii_lowercase();
        let until = self
            .config
            .data
            .borrow()
            .keep_alive_ignored_until
            .get(&key)
            .copied()?;
        let now = crate::config::unix_timestamp();
        (until > now).then(|| Duration::from_secs(until - now))
    }

    fn schedule_ignore_expiry(self: &Rc<Self>, bundle_id: String, until: u64) {
        let delay = until.saturating_sub(crate::config::unix_timestamp()).max(1) as f64;
        let weak = Rc::downgrade(self);
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            crate::engine::catch_callback_panic("keep-alive ignore timer", || {
                if let Some(keep_alive) = weak.upgrade() {
                    keep_alive.on_ignore_expired(&bundle_id, until);
                }
            });
        });
        let _ =
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(delay, false, &block) };
    }

    fn on_ignore_expired(self: &Rc<Self>, bundle_id: &str, expected_until: u64) {
        let stored_until = self
            .config
            .data
            .borrow()
            .keep_alive_ignored_until
            .get(&bundle_id.to_ascii_lowercase())
            .copied();
        let Some(stored_until) = stored_until else {
            return;
        };
        let now = crate::config::unix_timestamp();
        if stored_until > now {
            // The wall clock moved backwards. A newly extended ignore already
            // has its own timer, so only recreate the original one here.
            if stored_until == expected_until {
                self.schedule_ignore_expiry(bundle_id.to_string(), stored_until);
            }
            return;
        }

        if let Err(err) = self.config.set_keep_alive_ignored_until(bundle_id, None) {
            tracing::warn!(%err, bundle_id, "cannot remove expired keep-alive ignore");
        }
        self.watch_running_instances(bundle_id);
        self.launch_if_needed(bundle_id);
        tracing::info!(bundle_id, "keep-alive 24-hour ignore expired");
    }

    fn watch_running_instances(&self, bundle_id: &str) {
        let running = NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &NSString::from_str(bundle_id),
        );
        for app in running.iter() {
            if let Some(proc_exit) = &self.proc_exit {
                proc_exit.watch(app.processIdentifier());
            }
        }
    }
}

fn is_listed(list: &[String], bundle_id: &str) -> bool {
    list.iter().any(|b| b.eq_ignore_ascii_case(bundle_id))
}

/// Settings frontends often exit while a separately installed menu bar
/// helper keeps the product alive. Treat a related accessory app as the
/// selected app already running, without maintaining product-specific IDs.
fn background_suite_running(target_bundle_id: &str, target_path: &str) -> bool {
    let bundle_prefix = format!("{}.", target_bundle_id.to_ascii_lowercase());
    let target_name = Path::new(target_path)
        .file_stem()
        .and_then(|name| name.to_str())
        .map(normalized_app_name)
        .filter(|name| name.len() >= 6);
    let target_path = Path::new(target_path);

    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|app| app.activationPolicy() != NSApplicationActivationPolicy::Regular)
        .any(|app| {
            let Some(helper_path) = app
                .bundleURL()
                .and_then(|url| url.path().map(|path| path.to_string()))
            else {
                return false;
            };
            let helper_path = Path::new(&helper_path);
            if helper_path.starts_with(target_path) {
                return false;
            }

            let related_bundle = app.bundleIdentifier().is_some_and(|bundle_id| {
                bundle_id
                    .to_string()
                    .to_ascii_lowercase()
                    .starts_with(&bundle_prefix)
            });
            if related_bundle {
                return true;
            }

            let Some(target_name) = target_name.as_ref() else {
                return false;
            };
            helper_path.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|name| normalized_app_name(name).starts_with(target_name))
            })
        })
}

fn normalized_app_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}
