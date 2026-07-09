//! Engine: manages one watcher per running app and reacts to AX events.
//! Central rule: notifications are only triggers — for every decision the
//! window count is queried fresh, so cached counts cannot go stale.

pub mod ax;
mod counter;
mod quitter;
pub mod recent;
mod spaces;
mod watcher;
pub(crate) mod workspace;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_application_services::AXUIElement;
use objc2_core_foundation::CFRetained;
use objc2_foundation::NSTimer;

use watcher::Watcher;
pub use workspace::regular_running_apps as regular_running_apps_public;

use crate::config::{Config, ConfigHandle, FilterMode};
use recent::RecentQuitsHandle;

/// AX events around a Space switch are noisy (windows can briefly
/// disappear from AXWindows); pending quits are deferred this long after
/// the active Space changed.
const SPACE_CHANGE_GRACE_SECS: f64 = 3.0;
const PENDING_TERMINATION_TIMEOUT_SECS: f64 = 30.0;
const RECOUNT_QUEUE_CAPACITY: usize = 256;

/// Apps that hide themselves when their last window is closed
/// (close-to-background behaviour). From the outside, that state is
/// indistinguishable from the user pressing Cmd+H — the "restore windows on
/// unhide" memory lives inside the app process only. For the apps listed
/// here, hidden + an ordered-out standard window is treated as "the user
/// closed the last window" instead of "the user hid the app".
const CLOSE_TO_BACKGROUND_BUNDLE_IDS: &[&str] = &["com.hnc.discord"];

#[derive(Clone, Copy, Debug)]
enum RecountPhase {
    WindowDestroyed,
    AppDeactivated,
    FinalCheck,
}

struct RecountRequest {
    pid: libc::pid_t,
    generation: u64,
    phase: RecountPhase,
}

struct RecountResult {
    request: RecountRequest,
    count: ax::AxResult<counter::WindowCount>,
    /// Visible normal windows per the window server, computed on the
    /// worker (the systemwide CGWindowList enumeration must not run on
    /// the main thread). `None` if the window list was unavailable.
    /// Vetoes the quit on a final check and feeds the "windowless before
    /// termination" signal for keep-alive.
    cg_count: Option<usize>,
}

struct PendingTermination {
    bundle_id: String,
    name: String,
    generation: u64,
    requested_at: Instant,
}

/// Pure filter decision; used by should_quit and unit-tested on its own.
/// Cmd+H must never lead to a quit (Quitty-inspired guard) — except for
/// close-to-background apps whose last recount excluded an ordered-out
/// standard window: there the hide was the app's own reaction to its last
/// window being closed, which is exactly the case RustQuit exists for.
fn hidden_blocks_quit(watcher: &Watcher) -> bool {
    if !watcher.app.isHidden() {
        return false;
    }
    if watcher.last_ordered_out.get() == 0 {
        return true;
    }
    let bundle_id = watcher.app.bundleIdentifier().map(|b| b.to_string());
    !bundle_id.as_deref().is_some_and(|id| {
        CLOSE_TO_BACKGROUND_BUNDLE_IDS
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(id))
    })
}

fn decide_quit(config: &Config, bundle_id: Option<&str>) -> bool {
    if !config.enabled {
        return false;
    }
    let Some(bundle_id) = bundle_id else {
        // Never touch apps without a bundle ID (bare binaries).
        return false;
    };
    if crate::protected::is_protected_bundle_id(bundle_id) {
        return false;
    }
    // Apps marked "keep running" are never auto-quit — even while the
    // keep-alive master toggle is off, quitting them is clearly against
    // the user's intent (and would make RustQuit fight itself).
    if config
        .keep_alive_apps
        .iter()
        .any(|b| b.eq_ignore_ascii_case(bundle_id))
    {
        return false;
    }
    let listed = config
        .apps
        .iter()
        .any(|b| b.eq_ignore_ascii_case(bundle_id));
    match config.mode {
        FilterMode::Whitelist => listed,
        FilterMode::Blacklist => !listed,
    }
}

pub struct Engine {
    watchers: RefCell<HashMap<libc::pid_t, Watcher>>,
    workspace_tokens: RefCell<Vec<workspace::ObserverToken>>,
    pending_terminations: RefCell<HashMap<libc::pid_t, PendingTermination>>,
    recount_tx: SyncSender<RecountRequest>,
    recount_rx: RefCell<Receiver<RecountResult>>,
    recount_timer: RefCell<Option<Retained<NSTimer>>>,
    /// Requests sent to the worker whose results have not been drained yet.
    /// The drain timer only runs while this is non-zero, so an idle RustQuit
    /// causes no periodic wake-ups at all.
    outstanding_recounts: Cell<usize>,
    generation_nonce: Cell<u64>,
    own_pid: libc::pid_t,
    config: ConfigHandle,
    recent_quits: RecentQuitsHandle,
    last_space_change: Cell<Option<Instant>>,
}

impl Engine {
    /// Starts the engine: registers running apps and subscribes to
    /// launch/terminate/deactivate. Must only be called once the
    /// Accessibility permission has been granted.
    pub fn start(config: ConfigHandle, recent_quits: RecentQuitsHandle) -> io::Result<Rc<Engine>> {
        let (recount_tx, worker_rx) = mpsc::sync_channel::<RecountRequest>(RECOUNT_QUEUE_CAPACITY);
        let (worker_tx, recount_rx) = mpsc::sync_channel::<RecountResult>(RECOUNT_QUEUE_CAPACITY);
        thread::Builder::new()
            .name("rustquit-ax-recount".to_string())
            .spawn(move || {
                while let Ok(request) = worker_rx.recv() {
                    let count = counter::effective_window_count(request.pid);
                    let cg_count = counter::cg_window_count(request.pid);
                    if worker_tx
                        .send(RecountResult {
                            request,
                            count,
                            cg_count,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })?;

        let engine = Rc::new(Engine {
            watchers: RefCell::new(HashMap::new()),
            workspace_tokens: RefCell::new(Vec::new()),
            pending_terminations: RefCell::new(HashMap::new()),
            recount_tx,
            recount_rx: RefCell::new(recount_rx),
            recount_timer: RefCell::new(None),
            outstanding_recounts: Cell::new(0),
            generation_nonce: Cell::new(0),
            own_pid: std::process::id() as libc::pid_t,
            config,
            recent_quits,
            last_space_change: Cell::new(None),
        });
        for app in workspace::regular_running_apps() {
            engine.add_watcher(app);
        }
        let tokens = workspace::subscribe(&engine);
        engine.workspace_tokens.replace(tokens);
        tracing::info!(
            apps = engine.watchers.borrow().len(),
            "engine started, watching running apps"
        );
        Ok(engine)
    }

    /// Starts the drain timer if it is not already running. It stops itself
    /// once every outstanding result has been drained.
    fn ensure_recount_timer(self: &Rc<Self>) {
        if self.recount_timer.borrow().is_some() {
            return;
        }
        let engine = Rc::downgrade(self);
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            catch_callback_panic("recount timer", || {
                if let Some(engine) = engine.upgrade() {
                    engine.drain_recount_results();
                }
            });
        });
        let timer =
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.05, true, &block) };
        self.recount_timer.replace(Some(timer));
    }

    fn stop_recount_timer(&self) {
        if let Some(timer) = self.recount_timer.borrow_mut().take() {
            timer.invalidate();
        }
    }

    fn add_watcher(self: &Rc<Self>, app: Retained<NSRunningApplication>) {
        self.add_watcher_inner(app, true);
    }

    fn add_watcher_inner(self: &Rc<Self>, app: Retained<NSRunningApplication>, may_retry: bool) {
        let pid = app.processIdentifier();
        if pid == self.own_pid || self.watchers.borrow().contains_key(&pid) {
            return;
        }
        let generation = self.next_generation();
        match Watcher::new(app.clone(), Rc::downgrade(self), generation) {
            Ok(watcher) => {
                tracing::debug!(pid, "watcher created");
                self.watchers.borrow_mut().insert(pid, watcher);
            }
            Err(err) => {
                // Affects only this one app; e.g. its AX tree is not ready
                // right after launch → retry exactly once after 2 s.
                tracing::debug!(pid, ?err, may_retry, "cannot create watcher");
                if may_retry {
                    let engine = Rc::downgrade(self);
                    let block = block2::RcBlock::new(
                        move |_timer: std::ptr::NonNull<objc2_foundation::NSTimer>| {
                            catch_callback_panic("watcher retry timer", || {
                                if let Some(engine) = engine.upgrade() {
                                    if !app.isTerminated() {
                                        engine.add_watcher_inner(app.clone(), false);
                                    }
                                }
                            });
                        },
                    );
                    let _ = unsafe {
                        objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                            2.0, false, &block,
                        )
                    };
                }
            }
        }
    }

    pub(crate) fn on_app_launched(self: &Rc<Self>, app: Retained<NSRunningApplication>) {
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            return;
        }
        self.add_watcher(app);
    }

    pub(crate) fn on_app_terminated(&self, pid: libc::pid_t) {
        if self.watchers.borrow_mut().remove(&pid).is_some() {
            tracing::debug!(pid, "app terminated, watcher removed");
        }
        if let Some(pending) = self.pending_terminations.borrow_mut().remove(&pid) {
            if pending.requested_at.elapsed().as_secs_f64() <= PENDING_TERMINATION_TIMEOUT_SECS {
                self.recent_quits.push(pending.bundle_id, pending.name);
            }
        }
    }

    /// Rescue path for apps whose AX destroy events never arrive
    /// (Firefox, iTerm2, some Electron apps): when the user switches away
    /// from an app, recount its windows and quit it if none are left.
    pub(crate) fn on_app_deactivated(self: &Rc<Self>, pid: libc::pid_t) {
        let watchers = self.watchers.borrow();
        let Some(watcher) = watchers.get(&pid) else {
            return;
        };
        if !watcher.had_window.get() || !self.should_quit(watcher) {
            return;
        }
        self.request_recount(pid, watcher.generation.get(), RecountPhase::AppDeactivated);
    }

    pub(crate) fn on_space_changed(&self) {
        self.last_space_change.set(Some(Instant::now()));
    }

    fn space_change_recent(&self) -> bool {
        self.last_space_change
            .get()
            .is_some_and(|t| t.elapsed().as_secs_f64() < SPACE_CHANGE_GRACE_SECS)
    }

    /// Called by the C callback (via CallbackContext) on the main thread.
    pub(crate) fn on_ax_event(
        self: &Rc<Self>,
        pid: libc::pid_t,
        notification: &str,
        element: &AXUIElement,
    ) {
        let watchers = self.watchers.borrow();
        let Some(watcher) = watchers.get(&pid) else {
            return;
        };
        match notification {
            ax::NOTIF_WINDOW_CREATED => {
                self.bump_generation(watcher);
                self.pending_terminations.borrow_mut().remove(&pid);
                let retained = unsafe { CFRetained::retain(NonNull::from(element)) };
                watcher.watch_window(retained);
                tracing::debug!(pid, "window created");
            }
            ax::NOTIF_ELEMENT_DESTROYED => {
                watcher.forget_window(element);
                let generation = self.bump_generation(watcher);
                self.request_recount(pid, generation, RecountPhase::WindowDestroyed);
            }
            _ => {}
        }
    }

    /// Decides from the config whether the app may be auto-quit.
    fn should_quit(&self, watcher: &Watcher) -> bool {
        // Re-check the activation policy at decision time, not just when the
        // watcher was created. Menu bar apps that flip to Regular only while
        // a window is open (many Tauri/Electron apps do this to get a proper
        // Dock presence and window focus) switch back to Accessory as the
        // window closes. Quitting them would kill an app the user expects to
        // keep living in the menu bar, so never quit an app that is not a
        // regular Dock app right now.
        if watcher.app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            return false;
        }
        // Defense in depth: never touch anything under /System/Library/.
        let path = watcher
            .app
            .bundleURL()
            .and_then(|url| url.path().map(|p| p.to_string()));
        if path
            .as_deref()
            .is_some_and(crate::protected::is_protected_path)
        {
            return false;
        }
        let bundle_id = watcher.app.bundleIdentifier().map(|b| b.to_string());
        decide_quit(&self.config.data.borrow(), bundle_id.as_deref())
    }

    /// Second half of the quit path, runs after the delay has elapsed.
    pub(crate) fn finish_quit(self: &Rc<Self>, pid: libc::pid_t, generation: u64) {
        let watchers = self.watchers.borrow();
        let Some(watcher) = watchers.get(&pid) else {
            return;
        };
        if watcher.generation.get() != generation {
            tracing::debug!(pid, "quit cancelled: new window event during the delay");
            return;
        }
        if !self.should_quit(watcher) {
            tracing::debug!(pid, "quit cancelled: configuration changed");
            return;
        }
        if watcher.app.isTerminated() || !watcher.app.isFinishedLaunching() {
            return;
        }
        if hidden_blocks_quit(watcher) {
            tracing::debug!(pid, "quit cancelled: app is hidden");
            return;
        }
        // AX events around Space switches are unreliable; defer, re-check later.
        if self.space_change_recent() {
            tracing::debug!(pid, "quit deferred: active Space changed recently");
            quitter::schedule(
                Rc::downgrade(self),
                pid,
                generation,
                SPACE_CHANGE_GRACE_SECS,
            );
            return;
        }
        if self.pending_terminations.borrow().contains_key(&pid) {
            return;
        }
        drop(watchers);
        self.request_recount(pid, generation, RecountPhase::FinalCheck);
    }

    fn request_recount(self: &Rc<Self>, pid: libc::pid_t, generation: u64, phase: RecountPhase) {
        match self.recount_tx.try_send(RecountRequest {
            pid,
            generation,
            phase,
        }) {
            Ok(()) => {
                self.outstanding_recounts
                    .set(self.outstanding_recounts.get() + 1);
                self.ensure_recount_timer();
            }
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::debug!(pid, "AX recount queue full; dropping trigger");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                tracing::error!("AX recount worker unavailable");
            }
        }
    }

    fn drain_recount_results(self: &Rc<Self>) {
        loop {
            let result = match self.recount_rx.borrow().try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    tracing::error!("AX recount worker disconnected");
                    self.outstanding_recounts.set(0);
                    break;
                }
            };
            self.outstanding_recounts
                .set(self.outstanding_recounts.get().saturating_sub(1));
            self.handle_recount_result(result);
        }
        if self.outstanding_recounts.get() == 0 {
            self.stop_recount_timer();
        }
    }

    fn handle_recount_result(self: &Rc<Self>, result: RecountResult) {
        let RecountResult {
            request,
            count,
            cg_count,
        } = result;
        let watchers = self.watchers.borrow();
        let Some(watcher) = watchers.get(&request.pid) else {
            return;
        };
        if watcher.generation.get() != request.generation {
            return;
        }
        let count = match count {
            Ok(count) => count,
            Err(err) => {
                tracing::debug!(pid = request.pid, ?err, "recount failed");
                return;
            }
        };
        tracing::debug!(
            pid = request.pid,
            phase = ?request.phase,
            effective = count.effective,
            ordered_out = count.ordered_out,
            ?cg_count,
            "recount result"
        );
        // Remembered for the hidden-app guard: a close-to-background app
        // whose last recount saw an ordered-out standard window closed that
        // window; it did not get hidden by the user.
        watcher.last_ordered_out.set(count.ordered_out);
        if count.effective != 0 {
            return;
        }

        match request.phase {
            RecountPhase::WindowDestroyed | RecountPhase::AppDeactivated => {
                if !watcher.had_window.get() || !self.should_quit(watcher) {
                    return;
                }
                let delay = self.config.data.borrow().quit_delay_secs.max(0.05);
                quitter::schedule(Rc::downgrade(self), request.pid, request.generation, delay);
            }
            RecountPhase::FinalCheck => {
                drop(watchers);
                self.finish_quit_after_recount(request.pid, request.generation, cg_count);
            }
        }
    }

    fn finish_quit_after_recount(
        self: &Rc<Self>,
        pid: libc::pid_t,
        generation: u64,
        cg_count: Option<usize>,
    ) {
        let watchers = self.watchers.borrow();
        let Some(watcher) = watchers.get(&pid) else {
            return;
        };
        if watcher.generation.get() != generation
            || !self.should_quit(watcher)
            || watcher.app.isTerminated()
            || !watcher.app.isFinishedLaunching()
            || hidden_blocks_quit(watcher)
            || self.pending_terminations.borrow().contains_key(&pid)
        {
            return;
        }
        if self.space_change_recent() {
            quitter::schedule(
                Rc::downgrade(self),
                pid,
                generation,
                SPACE_CHANGE_GRACE_SECS,
            );
            return;
        }
        match cg_count {
            Some(0) => {}
            Some(n) => {
                // E.g. a full-screen window on another Space: don't quit.
                tracing::debug!(pid, n, "quit cancelled: CG count reports windows");
                return;
            }
            None => {
                tracing::debug!(pid, "quit cancelled: CG count failed");
                return;
            }
        }
        let pending_generation = self.bump_generation(watcher);
        let name = watcher
            .app
            .localizedName()
            .map(|n| n.to_string())
            .unwrap_or_default();
        tracing::info!(pid, "last window closed; requesting graceful termination");
        let ok = watcher.app.terminate();
        if !ok {
            tracing::warn!(pid, "termination request rejected");
            return;
        }
        if let Some(bundle_id) = watcher.app.bundleIdentifier() {
            self.pending_terminations.borrow_mut().insert(
                pid,
                PendingTermination {
                    bundle_id: bundle_id.to_string(),
                    name,
                    generation: pending_generation,
                    requested_at: Instant::now(),
                },
            );
            quitter::schedule_pending_expiry(
                Rc::downgrade(self),
                pid,
                pending_generation,
                PENDING_TERMINATION_TIMEOUT_SECS,
            );
        }
    }

    pub(crate) fn expire_pending_termination(&self, pid: libc::pid_t, generation: u64) {
        let mut pending = self.pending_terminations.borrow_mut();
        if pending
            .get(&pid)
            .is_some_and(|p| p.generation == generation)
        {
            pending.remove(&pid);
        }
    }

    fn next_generation(&self) -> u64 {
        let mut next = self.generation_nonce.get().wrapping_add(1);
        if next == 0 {
            next = 1;
        }
        self.generation_nonce.set(next);
        next
    }

    fn bump_generation(&self, watcher: &Watcher) -> u64 {
        let generation = self.next_generation();
        watcher.generation.set(generation);
        generation
    }
}

pub(crate) fn catch_callback_panic(name: &str, callback: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)).is_err() {
        tracing::error!(name, "caught panic in framework callback");
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop_recount_timer();
        workspace::unsubscribe(&self.workspace_tokens.borrow());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: FilterMode, apps: &[&str], enabled: bool) -> Config {
        Config {
            mode,
            enabled,
            apps: apps.iter().map(|s| s.to_string()).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn whitelist_quits_only_listed() {
        let c = config(FilterMode::Whitelist, &["com.apple.TextEdit"], true);
        assert!(decide_quit(&c, Some("com.apple.TextEdit")));
        assert!(!decide_quit(&c, Some("org.mozilla.firefox")));
        assert!(decide_quit(&c, Some("com.apple.textedit")));
    }

    #[test]
    fn blacklist_quits_all_except_listed() {
        let c = config(FilterMode::Blacklist, &["org.mozilla.firefox"], true);
        assert!(!decide_quit(&c, Some("org.mozilla.firefox")));
        assert!(decide_quit(&c, Some("com.apple.TextEdit")));
    }

    #[test]
    fn disabled_never_quits() {
        let c = config(FilterMode::Blacklist, &[], false);
        assert!(!decide_quit(&c, Some("com.apple.TextEdit")));
    }

    #[test]
    fn hard_exclusions_always_win() {
        let c = config(FilterMode::Blacklist, &[], true);
        assert!(!decide_quit(&c, Some("com.apple.finder")));
        assert!(!decide_quit(&c, Some("ch.patrick.rustquit")));
        assert!(!decide_quit(&c, None));
    }

    #[test]
    fn keep_alive_apps_are_never_quit() {
        let mut c = config(FilterMode::Blacklist, &[], true);
        c.keep_alive_apps = vec!["com.hnc.Discord".into()];
        assert!(!decide_quit(&c, Some("com.hnc.discord")));
        // The exclusion holds even while keep-alive itself is toggled off.
        c.keep_alive_enabled = false;
        assert!(!decide_quit(&c, Some("com.hnc.Discord")));
        // Whitelist mode: being quit-listed does not override keep-alive.
        c.mode = FilterMode::Whitelist;
        c.apps = vec!["com.hnc.Discord".into()];
        assert!(!decide_quit(&c, Some("com.hnc.Discord")));
    }
}
