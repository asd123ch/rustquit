//! Launch at login via SMAppService (macOS 13+).
//! Only meaningful when RustQuit runs as an .app bundle.

use smappservice_rs::{AppService, ServiceStatus, ServiceType};

pub fn is_enabled() -> bool {
    AppService::new(ServiceType::MainApp).status() == ServiceStatus::Enabled
}

pub fn requires_approval() -> bool {
    AppService::new(ServiceType::MainApp).status() == ServiceStatus::RequiresApproval
}

pub fn open_settings() {
    AppService::open_system_settings_login_items();
}

pub fn set_enabled(enabled: bool) -> bool {
    let service = AppService::new(ServiceType::MainApp);
    let result = if enabled {
        service.register()
    } else {
        service.unregister()
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
