//! Keep-alive: relaunches selected apps when they terminate (crash, updater
//! quit, accidental ⌘Q).
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
use crate::engine::recent::RecentQuitsHandle;
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
                keep_alive.on_terminated(app.bundle_id, pid, app.menu_bar);
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
                keep_alive.reconcile();
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

    fn on_terminated(self: &Rc<Self>, bundle_id: String, pid: libc::pid_t, menu_bar: bool) {
        let delay = {
            let config = self.config.data.borrow();
            if !config.keep_alive_enabled || !is_listed(&config.keep_alive_apps, &bundle_id) {
                return;
            }
            config.keep_alive_delay_secs
        };
        if self.is_paused(&bundle_id) {
            tracing::debug!(bundle_id, "keep-alive paused (loop protection)");
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
            quit.bundle_id.eq_ignore_ascii_case(bundle_id)
                && quit.when.elapsed() < RECENT_QUIT_GRACE
        });
        if self_quit {
            tracing::debug!(bundle_id, "keep-alive: app was auto-quit, not restarting");
            return;
        }
        // Menu bar apps come back after any termination. A regular app is
        // only resurrected when it actually crashed: quitting it — by
        // closing the settings window of a menu bar suite, by ⌘Q, or by
        // an updater — was a decision, and the exit status proves the
        // difference race-free.
        if !menu_bar {
            let exit_kind = self
                .proc_exit
                .as_ref()
                .and_then(|proc_exit| proc_exit.take_exit_kind(pid));
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
    /// an active loop-protection pause, and the launch happens right away —
    /// which also raises the App Management permission prompt up front,
    /// not at the first crash.
    pub fn keep_checked(&self, bundle_id: &str) {
        let key = bundle_id.to_ascii_lowercase();
        self.paused_until.borrow_mut().remove(&key);
        self.restarts.borrow_mut().remove(&key);
        // An already-running instance needs its exit watched from now on.
        let running = NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &NSString::from_str(bundle_id),
        );
        for app in running.iter() {
            if let Some(proc_exit) = &self.proc_exit {
                proc_exit.watch(app.processIdentifier());
            }
        }
        self.launch_if_needed(bundle_id);
    }

    /// Launches every keep-alive app that is not running. Called once
    /// shortly after RustQuit starts: apps that crashed or were quit while
    /// RustQuit was not watching come back, and a fresh install asks for
    /// the App Management permission right away.
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
        // Launch hidden and without stealing focus; errors (e.g. a missing
        // App Management permission) surface in the completion handler.
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
}

fn is_listed(list: &[String], bundle_id: &str) -> bool {
    list.iter().any(|b| b.eq_ignore_ascii_case(bundle_id))
}
