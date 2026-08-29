#![deny(unsafe_op_in_unsafe_fn)]

mod autostart;
mod config;
mod engine;
mod instance;
mod keepalive;
mod permissions;
mod procexit;
mod protected;
mod settings_window;
mod tray;

use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSTimer;

fn main() {
    // Persistent diagnostics are opt-in via RUSTQUIT_DIAGNOSTICS.
    let _log_guard = init_logging();

    let mtm = MainThreadMarker::new().expect("main() runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let _instance_guard = match instance::InstanceGuard::acquire() {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            tracing::warn!("RustQuit is already running; exiting this instance");
            return;
        }
        Err(err) => {
            tracing::error!(%err, "cannot acquire the single-instance lock");
            return;
        }
    };

    let config = config::ConfigStore::load();
    let settings = settings_window::SettingsController::new(mtm, config.clone());
    let recent_quits = engine::recent::RecentQuits::new_handle();

    // Relaunches keep-alive apps; independent of the engine and its
    // Accessibility permission. Must stay alive until run() ends.
    let keep_alive = keepalive::setup(config.clone(), recent_quits.clone());

    // Must stay alive until run() ends (owns the status item, menu, target).
    let tray = tray::Tray::setup(
        mtm,
        config.clone(),
        settings.clone(),
        recent_quits.clone(),
        keep_alive.handle(),
    );
    settings.set_tray(tray.target_retained());
    settings.set_keep_alive(keep_alive.handle());

    // Debug helper: open the settings window right away.
    if std::env::var_os("RUSTQUIT_SHOW_SETTINGS").is_some() {
        settings.show();
    }

    // Keeps the engine alive once it starts (after the permission is granted).
    let engine_slot: Rc<RefCell<Option<Rc<engine::Engine>>>> = Rc::new(RefCell::new(None));

    if permissions::request_trust_with_prompt() {
        tracing::info!("Accessibility permission present");
        tray.target().set_accessibility_permission_ok(true);
        match engine::Engine::start(config.clone(), recent_quits.clone()) {
            Ok(engine) => {
                engine_slot.borrow_mut().replace(engine);
            }
            Err(err) => tracing::error!(%err, "cannot start engine"),
        }
    } else {
        tracing::warn!("Accessibility permission missing; waiting for it to be granted");
        tray.target().set_accessibility_permission_ok(false);
    }

    // Reconcile permission changes for the whole lifetime. Revocation drops
    // all observers immediately; granting it creates a fresh engine.
    {
        let target = tray.target_retained();
        let slot = engine_slot.clone();
        let config_for_timer = config.clone();
        let recent_for_timer = recent_quits.clone();
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            engine::catch_callback_panic("permission watchdog", || {
                let trusted = permissions::is_trusted();
                target.set_accessibility_permission_ok(trusted);
                let running = slot.borrow().is_some();
                match (trusted, running) {
                    (true, false) => {
                        match engine::Engine::start(
                            config_for_timer.clone(),
                            recent_for_timer.clone(),
                        ) {
                            Ok(engine) => {
                                tracing::info!("Accessibility permission granted; starting engine");
                                slot.borrow_mut().replace(engine);
                            }
                            Err(err) => tracing::error!(%err, "cannot start engine"),
                        }
                    }
                    (false, true) => {
                        tracing::warn!("Accessibility permission revoked; stopping engine");
                        slot.borrow_mut().take();
                    }
                    _ => {}
                }
            });
        });
        let _watchdog =
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(2.0, true, &block) };
    }

    tracing::info!("RustQuit started");
    app.run();
}

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let diagnostics = std::env::var_os("RUSTQUIT_DIAGNOSTICS").is_some();
    let filter = if diagnostics {
        LevelFilter::DEBUG
    } else {
        LevelFilter::WARN
    };
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let log_dir = crate::config::home_dir().map(|home| home.join("Library/Logs/rustquit"));
    if let Some(dir) = log_dir.as_deref() {
        if dir.exists() {
            secure_logs(dir);
        }
    }

    let file_layer = diagnostics
        .then(|| {
            let dir = log_dir?;
            if crate::config::create_private_dir(&dir).is_err() {
                return None;
            }
            let appender = tracing_appender::rolling::RollingFileAppender::builder()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("rustquit.log")
                .max_log_files(7)
                .build(&dir)
                .ok()?;
            secure_logs(&dir);
            Some(tracing_appender::non_blocking(appender))
        })
        .flatten();

    match file_layer {
        Some((writer, guard)) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(writer)
                        .with_ansi(false),
                )
                .init();
            Some(guard)
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .init();
            None
        }
    }
}

fn secure_logs(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let _ = crate::config::create_private_dir(dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().filter(|entry| {
        entry.file_type().is_ok_and(|kind| kind.is_file())
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("rustquit.log"))
    }) {
        let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600));
    }
}
