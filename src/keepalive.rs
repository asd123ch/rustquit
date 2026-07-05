//! Keep-alive: relaunches selected apps when they terminate (crash, updater
//! quit, accidental ⌘Q).
//!
//! Deliberately independent of the quit engine: it registers its own
//! workspace observer and keeps working while the Accessibility permission
//! is missing or the engine is restarting. The engine never auto-quits an
//! app on the keep-alive list, so RustQuit cannot fight itself.
//!
//! Loop protection (optional): after LOOP_LIMIT automatic restarts within
//! LOOP_WINDOW, keep-alive for that app pauses for PAUSE ("until the next
//! day") — an app that crashes right back at launch, or one the user is
//! actively trying to get rid of, stops being resurrected.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceDidTerminateApplicationNotification,
};
use objc2_foundation::{NSNotification, NSOperationQueue, NSString, NSTimer};

use crate::config::ConfigHandle;
use crate::engine::recent::RecentQuitsHandle;
use crate::engine::workspace::ObserverToken;

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
    /// Timestamps of automatic restarts, per lowercase bundle ID.
    restarts: RefCell<HashMap<String, Vec<Instant>>>,
    paused_until: RefCell<HashMap<String, Instant>>,
}

/// Keeps the observer registration alive; must live until exit.
pub struct KeepAliveGuard {
    _keep_alive: Rc<KeepAlive>,
    _token: ObserverToken,
}

pub fn setup(config: ConfigHandle, recent_quits: RecentQuitsHandle) -> KeepAliveGuard {
    let keep_alive = Rc::new(KeepAlive {
        config,
        recent_quits,
        restarts: RefCell::new(HashMap::new()),
        paused_until: RefCell::new(HashMap::new()),
    });

    let weak = Rc::downgrade(&keep_alive);
    let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
        crate::engine::catch_callback_panic("keep-alive termination notification", || {
            let Some(keep_alive) = weak.upgrade() else {
                return;
            };
            let app = crate::engine::workspace::running_app_from(unsafe { notification.as_ref() });
            let Some(bundle_id) = app.and_then(|app| app.bundleIdentifier()) else {
                return;
            };
            keep_alive.on_terminated(bundle_id.to_string());
        });
    });
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidTerminateApplicationNotification),
            None,
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    };

    KeepAliveGuard {
        _keep_alive: keep_alive,
        _token: token,
    }
}

impl KeepAlive {
    fn on_terminated(self: &Rc<Self>, bundle_id: String) {
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
            "keep-alive: app terminated, restart scheduled"
        );
        let weak = Rc::downgrade(self);
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            crate::engine::catch_callback_panic("keep-alive restart timer", || {
                if let Some(keep_alive) = weak.upgrade() {
                    keep_alive.restart(&bundle_id);
                }
            });
        });
        let _ =
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(delay, false, &block) };
    }

    /// Runs after the delay; everything is re-checked at fire time.
    fn restart(&self, bundle_id: &str) {
        let loop_protection = {
            let config = self.config.data.borrow();
            if !config.keep_alive_enabled || !is_listed(&config.keep_alive_apps, bundle_id) {
                return;
            }
            config.keep_alive_loop_protection
        };
        if self.is_paused(bundle_id) {
            return;
        }

        // Updaters usually relaunch the app themselves; never start a
        // second instance.
        let ns_bundle_id = NSString::from_str(bundle_id);
        let running = NSRunningApplication::runningApplicationsWithBundleIdentifier(&ns_bundle_id);
        if !running.is_empty() {
            tracing::debug!(bundle_id, "keep-alive: app is already running again");
            return;
        }

        // Never resurrect an app RustQuit itself just quit.
        let self_quit = self.recent_quits.snapshot().iter().any(|quit| {
            quit.bundle_id.eq_ignore_ascii_case(bundle_id)
                && quit.when.elapsed() < RECENT_QUIT_GRACE
        });
        if self_quit {
            tracing::debug!(bundle_id, "keep-alive: app was auto-quit, not restarting");
            return;
        }

        let key = bundle_id.to_ascii_lowercase();
        if loop_protection {
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
                    "keep-alive paused for a day after repeated restarts"
                );
                return;
            }
        }

        let workspace = NSWorkspace::sharedWorkspace();
        let Some(url) = workspace.URLForApplicationWithBundleIdentifier(&ns_bundle_id) else {
            tracing::warn!(bundle_id, "keep-alive: app not found, cannot restart");
            return;
        };
        if workspace.openURL(&url) {
            tracing::info!(bundle_id, "keep-alive: app restarted");
            self.restarts
                .borrow_mut()
                .entry(key)
                .or_default()
                .push(Instant::now());
        } else {
            tracing::warn!(bundle_id, "keep-alive: restart request failed");
        }
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
