//! Window counting for the recount worker.
//!
//! The primary count walks AXWindows but does not blindly trust it: apps
//! with close-to-background behaviour (Discord and other Electron apps)
//! order their window out instead of destroying it, and such a hidden
//! window stays in AXWindows forever. `effective_window_count` filters
//! those out. `cg_window_count` is the independent second opinion before a
//! quit; its strictness follows the strength of the triggering AX signal.

use std::collections::HashSet;

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFType};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowAlpha,
    kCGWindowIsOnscreen, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID,
};

use super::{ax, spaces};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CgWindowPolicy {
    /// Protect every plausible visible or inactive-Space window.
    Conservative,
    /// AX confirmed that a window was destroyed. Only surfaces whose
    /// visibility or Space membership confirms a real window may contradict
    /// that strong signal.
    Confirmed,
}

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
    count_windows(&NativeWindowQueries(pid))
}

trait WindowQueries {
    type Window;
    fn windows(&self) -> ax::AxResult<Vec<Self::Window>>;
    fn subrole(&self, window: &Self::Window) -> ax::AxResult<String>;
    fn minimized(&self, window: &Self::Window) -> bool;
    fn window_id(&self, window: &Self::Window) -> Option<u32>;
    fn onscreen_ids(&self) -> Option<HashSet<u32>>;
    fn on_current_space(&self, id: u32) -> Option<bool>;
}

struct NativeWindowQueries(libc::pid_t);

impl WindowQueries for NativeWindowQueries {
    type Window = CFRetained<objc2_application_services::AXUIElement>;

    fn windows(&self) -> ax::AxResult<Vec<Self::Window>> {
        ax::windows(&ax::application_element(self.0))
    }

    fn subrole(&self, window: &Self::Window) -> ax::AxResult<String> {
        ax::subrole(window)
    }

    fn minimized(&self, window: &Self::Window) -> bool {
        ax::is_minimized(window)
    }

    fn window_id(&self, window: &Self::Window) -> Option<u32> {
        ax::window_id(window)
    }

    fn onscreen_ids(&self) -> Option<HashSet<u32>> {
        onscreen_window_ids(self.0)
    }

    fn on_current_space(&self, id: u32) -> Option<bool> {
        spaces::on_current_space(id)
    }
}

fn count_windows(queries: &impl WindowQueries) -> ax::AxResult<WindowCount> {
    let windows = queries.windows()?;
    // The CG list is one snapshot for all windows; fetch it lazily so apps
    // whose windows are all minimized never pay for it.
    let mut onscreen_ids: Option<Option<HashSet<u32>>> = None;
    let mut count = WindowCount {
        effective: 0,
        ordered_out: 0,
    };
    for window in &windows {
        if queries.subrole(window)? != ax::SUBROLE_STANDARD_WINDOW {
            continue;
        }
        if queries.minimized(window) {
            count.effective += 1;
            continue;
        }
        let Some(window_id) = queries.window_id(window) else {
            count.effective += 1;
            continue;
        };
        let onscreen = onscreen_ids
            .get_or_insert_with(|| queries.onscreen_ids())
            .as_ref();
        let Some(onscreen) = onscreen else {
            count.effective += 1;
            continue;
        };
        if onscreen.contains(&window_id) {
            count.effective += 1;
            continue;
        }
        match queries.on_current_space(window_id) {
            Some(true) => {
                tracing::debug!(window_id, "ignoring hidden (ordered-out) window");
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
            && dict_bool(dict, unsafe { kCGWindowIsOnscreen }).is_some_and(|value| value)
            && dict_f64(dict, unsafe { kCGWindowAlpha }).is_some_and(|a| a > 0.0)
            && let Some(number) = dict_i64(dict, unsafe { kCGWindowNumber })
            && let Ok(number) = u32::try_from(number)
        {
            ids.insert(number);
        }
    }
    Some(ids)
}

/// Number of normal windows that veto a quit according to the selected policy.
/// Candidates have layer == 0 and alpha > 0.
///
/// Apps keep permanent phantom layer-0 windows, so not every offscreen window
/// can veto a quit. SkyLight separates those still attached to a currently
/// visible Space (ordered out / phantom) from real windows parked on another
/// Desktop. After AX explicitly reports a destroyed window, only a surface
/// confirmed as onscreen or on an inactive Space overrides that stronger
/// signal. Query failures always keep the app alive.
pub fn cg_window_count(pid: libc::pid_t, policy: CgWindowPolicy) -> Option<usize> {
    let option = CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements;
    let list = CGWindowListCopyWindowInfo(option, kCGNullWindowID)?;
    let mut count = 0;
    for i in 0..list.count() {
        let ptr = unsafe { list.value_at_index(i) };
        let Some(ptr) = std::ptr::NonNull::new(ptr.cast_mut()) else {
            continue;
        };
        let dict = unsafe { ptr.cast::<CFDictionary>().as_ref() };
        if dict_i64(dict, unsafe { kCGWindowOwnerPID }) != Some(pid as i64)
            || dict_i64(dict, unsafe { kCGWindowLayer }) != Some(0)
            || !dict_f64(dict, unsafe { kCGWindowAlpha }).is_some_and(|a| a > 0.0)
        {
            continue;
        }
        let onscreen = dict_bool(dict, unsafe { kCGWindowIsOnscreen });
        let window_id = dict_i64(dict, unsafe { kCGWindowNumber })
            .and_then(|number| u32::try_from(number).ok())
            .filter(|number| *number != 0);
        let on_current_space = window_id.and_then(spaces::on_current_space);
        let on_any_space = if onscreen != Some(true) && on_current_space == Some(false) {
            window_id.and_then(spaces::on_any_space)
        } else {
            None
        };
        if cg_window_vetoes_quit(onscreen, on_current_space, on_any_space, policy) {
            count += 1;
        }
    }
    Some(count)
}

/// An offscreen window on the current Space is commonly a phantom or an app
/// hiding instead of closing. The conservative policy protects every other
/// state; destroyed-window evidence only accepts a confirmed user window.
fn cg_window_vetoes_quit(
    onscreen: Option<bool>,
    on_current_space: Option<bool>,
    on_any_space: Option<bool>,
    policy: CgWindowPolicy,
) -> bool {
    // A successful empty all-Spaces query identifies an unassigned surface,
    // not a user window on another Desktop. Visible or conflicting data wins.
    if onscreen != Some(true) && on_current_space == Some(false) && on_any_space == Some(false) {
        return false;
    }
    match policy {
        CgWindowPolicy::Conservative => {
            !matches!((onscreen, on_current_space), (Some(false), Some(true)))
        }
        CgWindowPolicy::Confirmed => matches!(
            (onscreen, on_current_space),
            (Some(true), _) | (Some(false), Some(false))
        ),
    }
}

fn dict_bool(dict: &CFDictionary, key: &objc2_core_foundation::CFString) -> Option<bool> {
    let value = unsafe { dict.value(key as *const _ as *const std::ffi::c_void) };
    let ptr = std::ptr::NonNull::new(value.cast_mut())?;
    let value = unsafe { CFRetained::retain(ptr.cast::<CFType>()) };
    value
        .downcast::<objc2_core_foundation::CFBoolean>()
        .ok()
        .map(|boolean| boolean.as_bool())
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

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_application_services::AXError;
    use std::cell::Cell;

    #[derive(Clone)]
    struct Window {
        subrole: ax::AxResult<String>,
        minimized: bool,
        id: Option<u32>,
    }

    struct Queries {
        windows: ax::AxResult<Vec<Window>>,
        onscreen: Option<HashSet<u32>>,
        current: Option<bool>,
        cg_calls: Cell<usize>,
        space_calls: Cell<usize>,
    }

    impl WindowQueries for Queries {
        type Window = Window;
        fn windows(&self) -> ax::AxResult<Vec<Window>> {
            self.windows.clone()
        }
        fn subrole(&self, window: &Window) -> ax::AxResult<String> {
            window.subrole.clone()
        }
        fn minimized(&self, window: &Window) -> bool {
            window.minimized
        }
        fn window_id(&self, window: &Window) -> Option<u32> {
            window.id
        }
        fn onscreen_ids(&self) -> Option<HashSet<u32>> {
            self.cg_calls.set(self.cg_calls.get() + 1);
            self.onscreen.clone()
        }
        fn on_current_space(&self, _id: u32) -> Option<bool> {
            self.space_calls.set(self.space_calls.get() + 1);
            self.current
        }
    }

    fn queries() -> Queries {
        Queries {
            windows: Ok(vec![Window {
                subrole: Ok(ax::SUBROLE_STANDARD_WINDOW.into()),
                minimized: false,
                id: Some(42),
            }]),
            onscreen: Some(HashSet::new()),
            current: Some(true),
            cg_calls: Cell::new(0),
            space_calls: Cell::new(0),
        }
    }

    #[test]
    fn unreadable_subrole_must_not_be_treated_as_no_windows() {
        let mut q = queries();
        q.windows.as_mut().unwrap()[0].subrole = Err(AXError::CannotComplete);
        assert_eq!(count_windows(&q), Err(AXError::CannotComplete));
        assert_eq!(q.cg_calls.get(), 0);
    }

    #[test]
    fn enumeration_errors_propagate_without_consulting_window_server() {
        for error in [
            AXError::CannotComplete,
            AXError::APIDisabled,
            AXError::InvalidUIElement,
            AXError::Failure,
        ] {
            let mut q = queries();
            q.windows = Err(error);
            assert_eq!(count_windows(&q), Err(error));
            assert_eq!(q.cg_calls.get(), 0);
        }
    }

    #[test]
    fn empty_and_nonstandard_windows_do_not_trigger_visibility_queries() {
        let mut q = queries();
        q.windows = Ok(Vec::new());
        assert_eq!(
            count_windows(&q).unwrap(),
            WindowCount {
                effective: 0,
                ordered_out: 0
            }
        );
        q.windows = Ok(vec![Window {
            subrole: Ok("AXDialog".into()),
            minimized: false,
            id: Some(42),
        }]);
        assert_eq!(count_windows(&q).unwrap().effective, 0);
        assert_eq!(q.cg_calls.get(), 0);
    }

    #[test]
    fn minimized_windows_remain_open_without_cg_or_spaces_queries() {
        let mut q = queries();
        q.windows.as_mut().unwrap()[0].minimized = true;
        assert_eq!(
            count_windows(&q).unwrap(),
            WindowCount {
                effective: 1,
                ordered_out: 0
            }
        );
        assert_eq!(q.cg_calls.get(), 0);
        assert_eq!(q.space_calls.get(), 0);
    }

    #[test]
    fn missing_window_id_or_cg_snapshot_keeps_windows_alive() {
        let mut q = queries();
        q.windows.as_mut().unwrap()[0].id = None;
        assert_eq!(count_windows(&q).unwrap().effective, 1);
        assert_eq!(q.cg_calls.get(), 0);
        q.windows.as_mut().unwrap()[0].id = Some(42);
        q.onscreen = None;
        assert_eq!(count_windows(&q).unwrap().effective, 1);
        assert_eq!(q.space_calls.get(), 0);
    }

    #[test]
    fn visible_windows_skip_spaces_and_share_one_cg_snapshot() {
        let mut q = queries();
        let window = q.windows.as_ref().unwrap()[0].clone();
        q.windows.as_mut().unwrap().push(window);
        q.onscreen = Some(HashSet::from([42]));
        assert_eq!(
            count_windows(&q).unwrap(),
            WindowCount {
                effective: 2,
                ordered_out: 0
            }
        );
        assert_eq!(q.cg_calls.get(), 1);
        assert_eq!(q.space_calls.get(), 0);
    }

    #[test]
    fn inactive_and_unknown_spaces_are_not_ordered_out() {
        for current in [Some(false), None] {
            let mut q = queries();
            q.current = current;
            assert_eq!(
                count_windows(&q).unwrap(),
                WindowCount {
                    effective: 1,
                    ordered_out: 0
                }
            );
        }
        assert_eq!(
            count_windows(&queries()).unwrap(),
            WindowCount {
                effective: 0,
                ordered_out: 1
            }
        );
    }

    #[test]
    fn subrole_failure_after_an_ordered_out_window_invalidates_the_snapshot() {
        let mut q = queries();
        q.windows.as_mut().unwrap().push(Window {
            subrole: Err(AXError::InvalidUIElement),
            minimized: false,
            id: Some(43),
        });
        assert_eq!(count_windows(&q), Err(AXError::InvalidUIElement));
    }

    #[test]
    fn visible_and_inactive_windows_veto_both_evidence_policies() {
        for policy in [CgWindowPolicy::Confirmed, CgWindowPolicy::Conservative] {
            for current in [None, Some(false), Some(true)] {
                for any in [None, Some(false), Some(true)] {
                    assert!(cg_window_vetoes_quit(Some(true), current, any, policy));
                }
            }
            for any in [None, Some(true)] {
                assert!(cg_window_vetoes_quit(Some(false), Some(false), any, policy));
            }
        }
    }

    #[test]
    fn cg_dictionary_reads_reject_missing_and_wrong_types() {
        use objc2_core_foundation::{CFBoolean, CFString};
        let key = CFString::from_str("value");
        let missing = CFString::from_str("missing");
        let number = CFNumber::new_i64(42);
        let dict = CFDictionary::<CFType, CFType>::from_slices(&[key.as_ref()], &[number.as_ref()]);
        let dict = dict.as_opaque();
        assert_eq!(dict_i64(dict, &key), Some(42));
        assert_eq!(dict_f64(dict, &key), Some(42.0));
        assert_eq!(dict_bool(dict, &key), None);
        assert_eq!(dict_i64(dict, &missing), None);
        assert_eq!(dict_bool(dict, &missing), None);
        assert_eq!(dict_f64(dict, &missing), None);
        for value in [false, true] {
            let boolean = CFBoolean::new(value);
            let dict =
                CFDictionary::<CFType, CFType>::from_slices(&[key.as_ref()], &[boolean.as_ref()]);
            let dict = dict.as_opaque();
            assert_eq!(dict_bool(dict, &key), Some(value));
            assert_eq!(dict_i64(dict, &key), None);
            assert_eq!(dict_f64(dict, &key), None);
        }
    }

    fn conservative(onscreen: Option<bool>, on_current_space: Option<bool>) -> bool {
        cg_window_vetoes_quit(
            onscreen,
            on_current_space,
            None,
            CgWindowPolicy::Conservative,
        )
    }

    fn confirmed(onscreen: Option<bool>, on_current_space: Option<bool>) -> bool {
        cg_window_vetoes_quit(onscreen, on_current_space, None, CgWindowPolicy::Confirmed)
    }

    #[test]
    fn unassigned_offscreen_surfaces_do_not_veto_windowless_apps() {
        for onscreen in [None, Some(false)] {
            assert!(!cg_window_vetoes_quit(
                onscreen,
                Some(false),
                Some(false),
                CgWindowPolicy::Conservative,
            ));
        }
    }

    #[test]
    fn assigned_or_uncertain_surfaces_still_veto() {
        assert!(cg_window_vetoes_quit(
            None,
            None,
            Some(false),
            CgWindowPolicy::Conservative,
        ));
        for assigned in [Some(true), None] {
            assert!(cg_window_vetoes_quit(
                None,
                Some(false),
                assigned,
                CgWindowPolicy::Conservative,
            ));
        }
        assert!(cg_window_vetoes_quit(
            Some(true),
            Some(false),
            Some(false),
            CgWindowPolicy::Conservative,
        ));
        assert!(cg_window_vetoes_quit(
            None,
            Some(true),
            Some(false),
            CgWindowPolicy::Conservative,
        ));
    }

    #[test]
    fn visible_and_inactive_space_windows_veto_quit() {
        assert!(conservative(Some(true), Some(true)));
        assert!(conservative(Some(true), Some(false)));
        assert!(conservative(Some(false), Some(false)));
    }

    #[test]
    fn uncertain_space_membership_fails_closed() {
        assert!(conservative(Some(false), None));
        assert!(conservative(None, Some(true)));
        assert!(conservative(None, None));
    }

    #[test]
    fn only_ordered_out_current_space_windows_do_not_veto() {
        assert!(!conservative(Some(false), Some(true)));
    }

    #[test]
    fn destroyed_window_evidence_requires_confirmed_cg_vetoes() {
        assert!(confirmed(Some(true), None));
        assert!(confirmed(Some(false), Some(false)));
        assert!(!confirmed(Some(false), Some(true)));
        assert!(!confirmed(None, Some(false)));
    }
}
