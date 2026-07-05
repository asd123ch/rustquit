//! Window counting for the recount worker.
//!
//! The primary count walks AXWindows but does not blindly trust it: apps
//! with close-to-background behaviour (Discord and other Electron apps)
//! order their window out instead of destroying it, and such a hidden
//! window stays in AXWindows forever. `effective_window_count` filters
//! those out; `cg_window_count` is the independent second opinion an app
//! must also pass (both zero) before it is quit.

use std::collections::HashSet;

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFType};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowAlpha,
    kCGWindowIsOnscreen, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
};

use super::{ax, spaces};

/// Result of a recount walk over the app's AX windows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WindowCount {
    /// Standard windows that still matter to the user: visible, minimized,
    /// or parked on another Space.
    pub effective: usize,
    /// Standard windows the app ordered out itself (close-to-background
    /// behaviour); they exist in AXWindows but are invisible everywhere.
    pub ordered_out: usize,
}

/// Counts the process's standard windows, excluding ordered-out ones.
///
/// Per window, cheapest check first; every failure falls back to counting
/// the window, so degraded AX/CG/SkyLight data can only keep an app alive,
/// never quit one wrongly:
/// 1. minimized (AX)                        -> counts
/// 2. onscreen per the CG window list       -> counts
/// 3. offscreen, on an inactive Space only  -> counts (visible after switch)
/// 4. offscreen, yet on a current Space     -> ordered out, does NOT count
pub fn effective_window_count(pid: libc::pid_t) -> ax::AxResult<WindowCount> {
    let app_element = ax::application_element(pid);
    let windows = ax::windows(&app_element)?;
    // The CG list is one snapshot for all windows; fetch it lazily so apps
    // whose windows are all minimized never pay for it.
    let mut onscreen_ids: Option<Option<HashSet<u32>>> = None;
    let mut count = WindowCount {
        effective: 0,
        ordered_out: 0,
    };
    for window in &windows {
        if !matches!(
            ax::subrole(window).as_deref(),
            Ok(ax::SUBROLE_STANDARD_WINDOW)
        ) {
            continue;
        }
        if ax::is_minimized(window) {
            count.effective += 1;
            continue;
        }
        let Some(window_id) = ax::window_id(window) else {
            count.effective += 1;
            continue;
        };
        let onscreen = onscreen_ids
            .get_or_insert_with(|| onscreen_window_ids(pid))
            .as_ref();
        let Some(onscreen) = onscreen else {
            count.effective += 1;
            continue;
        };
        if onscreen.contains(&window_id) {
            count.effective += 1;
            continue;
        }
        match spaces::on_current_space(window_id) {
            Some(true) => {
                tracing::debug!(pid, window_id, "ignoring hidden (ordered-out) window");
                count.ordered_out += 1;
            }
            Some(false) | None => count.effective += 1,
        }
    }
    Ok(count)
}

/// CGWindowIDs of the pid's visible normal windows (layer 0, onscreen,
/// alpha > 0). `None` if the window list is unavailable.
fn onscreen_window_ids(pid: libc::pid_t) -> Option<HashSet<u32>> {
    let option = CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements;
    let list = CGWindowListCopyWindowInfo(option, kCGNullWindowID)?;
    let mut ids = HashSet::new();
    for i in 0..list.count() {
        let ptr = unsafe { list.value_at_index(i) };
        let Some(ptr) = std::ptr::NonNull::new(ptr.cast_mut()) else {
            continue;
        };
        let dict = unsafe { ptr.cast::<CFDictionary>().as_ref() };
        if dict_i64(dict, unsafe { kCGWindowOwnerPID }) == Some(pid as i64)
            && dict_i64(dict, unsafe { kCGWindowLayer }) == Some(0)
            && dict_bool(dict, unsafe { kCGWindowIsOnscreen })
            && dict_f64(dict, unsafe { kCGWindowAlpha }).is_some_and(|a| a > 0.0)
        {
            if let Some(number) = dict_i64(dict, unsafe { kCGWindowNumber }) {
                if let Ok(number) = u32::try_from(number) {
                    ids.insert(number);
                }
            }
        }
    }
    Some(ids)
}

/// Number of visible normal windows of the pid according to the window
/// server: layer == 0, onscreen, alpha > 0.
///
/// The onscreen requirement is load-bearing, not an optimization. Apps keep
/// permanent phantom layer-0 windows with alpha 1 (per-display tab bars,
/// hidden panels; TextEdit alone owns ~18 of them even with no real window
/// open). Counting off-screen windows would therefore veto every quit and
/// disable the app. Dropping them is still safe: minimized windows are
/// covered by the AX count (they stay in AXWindows), and a full-screen
/// window on another, inactive Space reports onscreen == true (verified
/// empirically) and keeps vetoing correctly.
pub fn cg_window_count(pid: libc::pid_t) -> Option<usize> {
    let option = CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements;
    let list = CGWindowListCopyWindowInfo(option, kCGNullWindowID)?;
    let mut count = 0;
    for i in 0..list.count() {
        let ptr = unsafe { list.value_at_index(i) };
        let Some(ptr) = std::ptr::NonNull::new(ptr.cast_mut()) else {
            continue;
        };
        let dict = unsafe { ptr.cast::<CFDictionary>().as_ref() };
        if dict_i64(dict, unsafe { kCGWindowOwnerPID }) == Some(pid as i64)
            && dict_i64(dict, unsafe { kCGWindowLayer }) == Some(0)
            && dict_bool(dict, unsafe { kCGWindowIsOnscreen })
            && dict_f64(dict, unsafe { kCGWindowAlpha }).is_some_and(|a| a > 0.0)
        {
            count += 1;
        }
    }
    Some(count)
}

fn dict_bool(dict: &CFDictionary, key: &objc2_core_foundation::CFString) -> bool {
    let value = unsafe { dict.value(key as *const _ as *const std::ffi::c_void) };
    let Some(ptr) = std::ptr::NonNull::new(value.cast_mut()) else {
        return false;
    };
    let value = unsafe { CFRetained::retain(ptr.cast::<CFType>()) };
    value
        .downcast::<objc2_core_foundation::CFBoolean>()
        .is_ok_and(|boolean| boolean.as_bool())
}

fn dict_i64(dict: &CFDictionary, key: &objc2_core_foundation::CFString) -> Option<i64> {
    let value = unsafe { dict.value(key as *const _ as *const std::ffi::c_void) };
    let ptr = std::ptr::NonNull::new(value.cast_mut())?;
    let value = unsafe { CFRetained::retain(ptr.cast::<CFType>()) };
    let number = value.downcast::<CFNumber>().ok()?;
    number.as_i64()
}

fn dict_f64(dict: &CFDictionary, key: &objc2_core_foundation::CFString) -> Option<f64> {
    let value = unsafe { dict.value(key as *const _ as *const std::ffi::c_void) };
    let ptr = std::ptr::NonNull::new(value.cast_mut())?;
    let value = unsafe { CFRetained::retain(ptr.cast::<CFType>()) };
    let number = value.downcast::<CFNumber>().ok()?;
    number.as_f64()
}
