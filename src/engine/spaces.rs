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

use objc2_core_foundation::{CFArray, CFNumber, CFRetained};

/// `SLSCopySpacesForWindows` selection mask for Spaces that are currently
/// visible on any display ("current" Spaces).
const SPACE_MASK_CURRENT: i32 = 5;

type MainConnectionIdFn = unsafe extern "C" fn() -> i32;
type CopySpacesForWindowsFn =
    unsafe extern "C" fn(cid: i32, mask: i32, window_ids: *const c_void) -> *mut CFArray;

struct SkyLight {
    connection: i32,
    copy_spaces_for_windows: CopySpacesForWindowsFn,
}

// The connection id is a plain token and the function pointer is stateless;
// the window server serializes requests per connection.
unsafe impl Send for SkyLight {}
unsafe impl Sync for SkyLight {}

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
/// ordered out by its app (hidden); a window whose only Spaces are inactive
/// returns `Some(false)` here and must be treated as alive.
pub fn on_current_space(window_id: u32) -> Option<bool> {
    let sky = skylight()?;
    let ids = CFArray::from_retained_objects(&[CFNumber::new_i64(i64::from(window_id))]);
    let spaces = unsafe {
        (sky.copy_spaces_for_windows)(
            sky.connection,
            SPACE_MASK_CURRENT,
            CFRetained::as_ptr(&ids).as_ptr().cast(),
        )
    };
    let spaces = unsafe { CFRetained::from_raw(NonNull::new(spaces)?) };
    Some(spaces.count() > 0)
}
