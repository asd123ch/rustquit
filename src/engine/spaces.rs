//! Window-server Space queries via the private SkyLight framework.
//!
//! Needed to tell a *hidden* window apart from a window on another Space.
//! Some apps (Discord and other Electron apps with close-to-background
//! behaviour) do not destroy their window when the close button is clicked —
//! they order it out. Such a window stays in AXWindows with subrole
//! AXStandardWindow, so a plain AX count keeps the app alive forever.
//!
//! In the CG window list both cases look identical (onscreen == false).
//! The distinguishing signal, verified empirically: a hidden window still
//! sits on a *currently visible* Space, while a window on another Space is
//! only a member of that inactive Space.
//!
//! The symbols are resolved at runtime with dlopen/dlsym and everything
//! degrades gracefully: if SkyLight or a symbol is unavailable, callers get
//! `None` and fall back to the conservative answer (window counts as alive,
//! the app is not quit).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2_core_foundation::{CFArray, CFNumber, CFRetained, CFType};

/// `SLSCopySpacesForWindows` selection mask for Spaces that are currently
/// visible on any display ("current" Spaces).
const SPACE_MASK_CURRENT: i32 = 5;
const SPACE_MASK_ALL: i32 = 7;

type MainConnectionIdFn = unsafe extern "C" fn() -> i32;
type CopySpacesForWindowsFn =
    unsafe extern "C" fn(cid: i32, mask: i32, window_ids: *const c_void) -> *mut CFArray;

// The window server serializes requests per connection.
struct SkyLight {
    connection: i32,
    copy_spaces_for_windows: CopySpacesForWindowsFn,
}

fn skylight() -> Option<&'static SkyLight> {
    static INSTANCE: OnceLock<Option<SkyLight>> = OnceLock::new();
    INSTANCE.get_or_init(load).as_ref()
}

fn symbol(handle: *mut c_void, name: &[u8]) -> Option<*mut c_void> {
    debug_assert!(name.ends_with(b"\0"));
    NonNull::new(unsafe { libc::dlsym(handle, name.as_ptr().cast()) }).map(NonNull::as_ptr)
}

fn load() -> Option<SkyLight> {
    const PATH: &[u8] = b"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight\0";
    let handle = unsafe { libc::dlopen(PATH.as_ptr().cast(), libc::RTLD_LAZY) };
    if handle.is_null() {
        tracing::warn!("SkyLight unavailable; hidden-window detection disabled");
        return None;
    }
    let (Some(main_connection), Some(copy_spaces)) = (
        symbol(handle, b"SLSMainConnectionID\0"),
        symbol(handle, b"SLSCopySpacesForWindows\0"),
    ) else {
        tracing::warn!("SkyLight symbols missing; hidden-window detection disabled");
        return None;
    };
    let main_connection =
        unsafe { std::mem::transmute::<*mut c_void, MainConnectionIdFn>(main_connection) };
    Some(SkyLight {
        connection: unsafe { main_connection() },
        copy_spaces_for_windows: unsafe {
            std::mem::transmute::<*mut c_void, CopySpacesForWindowsFn>(copy_spaces)
        },
    })
}

/// Whether the window is a member of a Space that is currently visible on
/// some display. `None` if SkyLight is unavailable or the query failed.
///
/// A window that is *not* onscreen yet still on a current Space has been
/// ordered out by its app (hidden). `Some(false)` alone cannot distinguish
/// inactive-Space windows from unassigned surfaces; use `on_any_space` for that.
pub fn on_current_space(window_id: u32) -> Option<bool> {
    has_space(window_id, SPACE_MASK_CURRENT)
}

/// Distinguishes unassigned helper surfaces from windows on inactive Spaces.
/// A failed query is unknown, never evidence that a window is unassigned.
pub fn on_any_space(window_id: u32) -> Option<bool> {
    has_space(window_id, SPACE_MASK_ALL)
}

fn has_space(window_id: u32, mask: i32) -> Option<bool> {
    query_space(skylight(), window_id, mask)
}

fn query_space(sky: Option<&SkyLight>, window_id: u32, mask: i32) -> Option<bool> {
    if window_id == 0 {
        return None;
    }
    let sky = sky?;
    let ids = CFArray::from_retained_objects(&[CFNumber::new_i64(i64::from(window_id))]);
    let spaces = unsafe {
        (sky.copy_spaces_for_windows)(
            sky.connection,
            mask,
            CFRetained::as_ptr(&ids).as_ptr().cast(),
        )
    };
    let spaces = unsafe { CFRetained::<CFType>::from_raw(NonNull::new(spaces)?.cast()) };
    let spaces = spaces.downcast::<CFArray>().ok()?;
    Some(spaces.count() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CFString;
    use std::cell::Cell;

    thread_local! {
        static REQUEST: Cell<(i32, i32, i64)> = const { Cell::new((0, 0, 0)) };
    }

    unsafe extern "C" fn spaces(cid: i32, mask: i32, ids: *const c_void) -> *mut CFArray {
        let ids = unsafe { &*ids.cast::<CFArray>() };
        let id = unsafe { &*ids.value_at_index(0).cast::<CFNumber>() };
        REQUEST.set((cid, mask, id.as_i64().unwrap()));
        let values = if mask == SPACE_MASK_ALL {
            vec![CFNumber::new_i64(7)]
        } else {
            vec![]
        };
        CFRetained::into_raw(CFArray::from_retained_objects(&values))
            .as_ptr()
            .cast()
    }

    unsafe extern "C" fn failed(_: i32, _: i32, _: *const c_void) -> *mut CFArray {
        std::ptr::null_mut()
    }

    unsafe extern "C" fn wrong_type(_: i32, _: i32, _: *const c_void) -> *mut CFArray {
        CFRetained::into_raw(CFString::from_str("invalid response"))
            .as_ptr()
            .cast()
    }

    #[test]
    fn current_and_all_spaces_queries_preserve_mask_and_window_id() {
        let sky = SkyLight {
            connection: 19,
            copy_spaces_for_windows: spaces,
        };
        assert_eq!(query_space(Some(&sky), 42, SPACE_MASK_CURRENT), Some(false));
        assert_eq!(REQUEST.get(), (19, 5, 42));
        assert_eq!(
            query_space(Some(&sky), u32::MAX, SPACE_MASK_ALL),
            Some(true)
        );
        assert_eq!(REQUEST.get(), (19, 7, i64::from(u32::MAX)));
    }

    #[test]
    fn missing_bridge_invalid_id_and_failed_queries_remain_unknown() {
        assert_eq!(query_space(None, 42, SPACE_MASK_ALL), None);
        for callback in [failed as CopySpacesForWindowsFn, wrong_type] {
            let sky = SkyLight {
                connection: 19,
                copy_spaces_for_windows: callback,
            };
            assert_eq!(query_space(Some(&sky), 42, SPACE_MASK_ALL), None);
        }
        let sky = SkyLight {
            connection: 19,
            copy_spaces_for_windows: spaces,
        };
        REQUEST.set((0, 0, 0));
        assert_eq!(query_space(Some(&sky), 0, SPACE_MASK_CURRENT), None);
        assert_eq!(REQUEST.get(), (0, 0, 0));
    }
}
