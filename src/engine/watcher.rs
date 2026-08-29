//! One watcher per observed app: owns the AXObserver, registers
//! WindowCreated app-wide and UIElementDestroyed per window.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::rc::Weak;

use objc2::rc::Retained;
use objc2_app_kit::NSRunningApplication;
use objc2_application_services::{AXObserver, AXUIElement};
use objc2_core_foundation::{CFRetained, CFString};

use super::Engine;
use super::ax::{self, AxResult};

/// Passed to the C callbacks as refcon. Lives in a Box with a stable
/// address and is freed only after the observer (field order!).
pub struct CallbackContext {
    pub pid: libc::pid_t,
    pub engine: Weak<Engine>,
}

/// C callback for all AX notifications. No panic may unwind into C.
unsafe extern "C-unwind" fn ax_callback(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    refcon: *mut c_void,
) {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if refcon.is_null() {
            return;
        }
        let ctx = unsafe { &*(refcon as *const CallbackContext) };
        let Some(engine) = ctx.engine.upgrade() else {
            return;
        };
        let name = unsafe { notification.as_ref() }.to_string();
        let element = unsafe { element.as_ref() };
        engine.on_ax_event(ctx.pid, &name, element);
    }));
    if result.is_err() {
        tracing::error!("caught panic in AX callback");
    }
}

pub struct Watcher {
    pub app: Retained<NSRunningApplication>,
    pub element: CFRetained<AXUIElement>,
    /// Field order is drop order: invalidate the observer first (no more
    /// callbacks), only then may the context be freed.
    observer: ax::Observer,
    _context: Box<CallbackContext>,
    watched_windows: RefCell<Vec<CFRetained<AXUIElement>>>,
    /// The app has shown at least one standard window this session —
    /// only then may it ever be quit automatically.
    pub had_window: Cell<bool>,
    /// Invalidates delayed quit checks when something happens in between.
    pub generation: Cell<u64>,
}

impl Watcher {
    pub fn new(
        app: Retained<NSRunningApplication>,
        engine: Weak<Engine>,
        generation: u64,
    ) -> AxResult<(Watcher, bool)> {
        let pid = app.processIdentifier();
        let element = ax::application_element(pid);
        ax::nudge_electron(&element);

        let observer = ax::Observer::new(pid, Some(ax_callback))?;
        let context = Box::new(CallbackContext { pid, engine });
        let refcon = &*context as *const CallbackContext as *mut c_void;

        observer.add_notification(&element, ax::NOTIF_WINDOW_CREATED, refcon)?;

        let watcher = Watcher {
            app,
            element,
            observer,
            _context: context,
            watched_windows: RefCell::new(Vec::new()),
            had_window: Cell::new(false),
            generation: Cell::new(generation),
        };

        // Register windows that are already open. The app-level observer is
        // active first, so a window created during this scan cannot be lost.
        let windows_ready = match watcher.refresh_windows() {
            Ok(()) => true,
            Err(err) => {
                tracing::debug!(pid, ?err, "cannot read windows at watcher start");
                false
            }
        };
        Ok((watcher, windows_ready))
    }

    pub fn refcon(&self) -> *mut c_void {
        &*self._context as *const CallbackContext as *mut c_void
    }

    /// Registers the destroy notification for a (new) window.
    pub fn watch_window(&self, window: CFRetained<AXUIElement>) {
        if self.watched_windows.borrow().iter().any(|w| **w == *window) {
            return;
        }
        // Remember the standard window even if this app does not support a
        // per-window destroy notification. The AppKit deactivation fallback
        // can still recount it safely later.
        if matches!(
            ax::subrole(&window).as_deref(),
            Ok(ax::SUBROLE_STANDARD_WINDOW)
        ) {
            self.had_window.set(true);
        }
        match self
            .observer
            .add_notification(&window, ax::NOTIF_ELEMENT_DESTROYED, self.refcon())
        {
            Ok(()) => {
                self.watched_windows.borrow_mut().push(window);
            }
            Err(err) => {
                tracing::debug!(
                    pid = self.app.processIdentifier(),
                    ?err,
                    "cannot register destroy notification"
                );
            }
        }
    }

    /// Synchronizes windows that were already present before the observer
    /// became ready. Transient AX startup failures are retried by Engine.
    pub fn refresh_windows(&self) -> AxResult<()> {
        for window in ax::windows(&self.element)? {
            self.watch_window(window);
        }
        Ok(())
    }

    /// Drops a destroyed window from the list.
    pub fn forget_window(&self, element: &AXUIElement) {
        self.watched_windows
            .borrow_mut()
            .retain(|w| **w != *element);
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        for window in self.watched_windows.borrow().iter() {
            self.observer
                .remove_notification(window, ax::NOTIF_ELEMENT_DESTROYED);
        }
        self.observer
            .remove_notification(&self.element, ax::NOTIF_WINDOW_CREATED);
    }
}
