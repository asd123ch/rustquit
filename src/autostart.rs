//! Launch at login via SMAppService (macOS 13+).
//! Only meaningful when RustQuit runs as an .app bundle.

use objc2_service_management::{SMAppService, SMAppServiceStatus};

fn status() -> SMAppServiceStatus {
    let service = unsafe { SMAppService::mainAppService() };
    unsafe { service.status() }
}

pub fn is_enabled() -> bool {
    status() == SMAppServiceStatus::Enabled
}

pub fn requires_approval() -> bool {
    status() == SMAppServiceStatus::RequiresApproval
}

pub fn open_settings() {
    unsafe { SMAppService::openSystemSettingsLoginItems() };
}

pub fn set_enabled(enabled: bool) -> bool {
    let service = unsafe { SMAppService::mainAppService() };
    let result = unsafe {
        if enabled {
            service.registerAndReturnError()
        } else {
            service.unregisterAndReturnError()
        }
    };
    match result {
        Ok(()) => {
            tracing::info!(enabled, "login item updated");
            true
        }
        Err(err) => {
            tracing::warn!(%err, enabled, "cannot toggle login item (is the app running as an .app bundle?)");
            false
        }
    }
}
