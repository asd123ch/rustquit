//! NSWorkspace feed: delivers app launch/terminate/deactivate to the engine.

use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification,
    NSWorkspaceApplicationKey, NSWorkspaceDidDeactivateApplicationNotification,
    NSWorkspaceDidLaunchApplicationNotification, NSWorkspaceDidTerminateApplicationNotification,
};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSOperationQueue};

use super::Engine;

pub(crate) fn running_app_from(
    notification: &NSNotification,
) -> Option<Retained<NSRunningApplication>> {
    let user_info = notification.userInfo()?;
    let value = user_info.objectForKey(unsafe { NSWorkspaceApplicationKey })?;
    value.downcast::<NSRunningApplication>().ok()
}

pub type ObserverToken = Retained<ProtocolObject<dyn NSObjectProtocol>>;

/// Subscribes to launch/terminate/deactivate; the returned tokens must
/// stay alive as long as the engine runs.
pub fn subscribe(engine: &Rc<Engine>) -> Vec<ObserverToken> {
    let workspace = NSWorkspace::sharedWorkspace();
    let center = workspace.notificationCenter();
    let queue = NSOperationQueue::mainQueue();

    let launch_engine = Rc::downgrade(engine);
    let launch_block = RcBlock::new(move |notification: NonNull<NSNotification>| {
        super::catch_callback_panic("workspace launch notification", || {
            let Some(engine) = launch_engine.upgrade() else {
                return;
            };
            if let Some(app) = running_app_from(unsafe { notification.as_ref() }) {
                engine.on_app_launched(app);
            }
        });
    });

    let terminate_engine = Rc::downgrade(engine);
    let terminate_block = RcBlock::new(move |notification: NonNull<NSNotification>| {
        super::catch_callback_panic("workspace termination notification", || {
            let Some(engine) = terminate_engine.upgrade() else {
                return;
            };
            if let Some(app) = running_app_from(unsafe { notification.as_ref() }) {
                engine.on_app_terminated(app.processIdentifier());
            }
        });
    });

    // Self-healing trigger: some apps (Firefox, iTerm2, some Electron apps)
    // fail to deliver AX destroy notifications. Deactivation events come
    // from AppKit rather than the target's AX stream, so a recount whenever
    // the user switches away catches many missed closes.
    let deactivate_engine = Rc::downgrade(engine);
    let deactivate_block = RcBlock::new(move |notification: NonNull<NSNotification>| {
        super::catch_callback_panic("workspace deactivation notification", || {
            let Some(engine) = deactivate_engine.upgrade() else {
                return;
            };
            if let Some(app) = running_app_from(unsafe { notification.as_ref() }) {
                engine.on_window_state_changed(app.processIdentifier());
            }
        });
    });

    let launch_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidLaunchApplicationNotification),
            None,
            Some(&queue),
            &launch_block,
        )
    };
    let terminate_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidTerminateApplicationNotification),
            None,
            Some(&queue),
            &terminate_block,
        )
    };
    let deactivate_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidDeactivateApplicationNotification),
            None,
            Some(&queue),
            &deactivate_block,
        )
    };

    // Space switches make AX window lists briefly unreliable; the engine
    // records the timestamp and defers pending quits accordingly.
    let space_engine = Rc::downgrade(engine);
    let space_block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
        super::catch_callback_panic("workspace Space notification", || {
            if let Some(engine) = space_engine.upgrade() {
                engine.on_space_changed();
            }
        });
    });
    let space_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceActiveSpaceDidChangeNotification),
            None,
            Some(&queue),
            &space_block,
        )
    };
    vec![launch_token, terminate_token, deactivate_token, space_token]
}

pub fn unsubscribe(tokens: &[ObserverToken]) {
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    for token in tokens {
        unsafe { center.removeObserver(token.as_ref()) };
    }
}

/// All currently running "real" apps (Dock apps, activationPolicy == Regular).
pub fn regular_running_apps() -> Vec<Retained<NSRunningApplication>> {
    let workspace = NSWorkspace::sharedWorkspace();
    let apps = workspace.runningApplications();
    apps.iter()
        .filter(|app| {
            app.activationPolicy() == objc2_app_kit::NSApplicationActivationPolicy::Regular
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSDictionary, NSString};

    #[test]
    fn workspace_notifications_without_app_data_are_ignored() {
        let name = NSString::from_str("synthetic.notification");
        let notification =
            unsafe { NSNotification::notificationWithName_object_userInfo(&name, None, None) };
        assert!(running_app_from(&notification).is_none());
        let wrong_value = NSString::from_str("not an application");
        let dictionary = NSDictionary::<NSString, AnyObject>::from_slices(
            &[unsafe { NSWorkspaceApplicationKey }],
            &[wrong_value.as_ref()],
        );
        // Erasing generic types preserves the Foundation dictionary and its objects.
        let dictionary: Retained<NSDictionary> = unsafe { Retained::cast_unchecked(dictionary) };
        let notification = unsafe {
            NSNotification::notificationWithName_object_userInfo(&name, None, Some(&dictionary))
        };
        assert!(running_app_from(&notification).is_none());
    }

    #[test]
    fn workspace_app_is_retained_after_notification_is_dropped() {
        let app = NSRunningApplication::currentApplication();
        let pid = app.processIdentifier();
        let dictionary = NSDictionary::<NSString, AnyObject>::from_slices(
            &[unsafe { NSWorkspaceApplicationKey }],
            &[app.as_ref()],
        );
        let dictionary: Retained<NSDictionary> = unsafe { Retained::cast_unchecked(dictionary) };
        let notification = unsafe {
            NSNotification::notificationWithName_object_userInfo(
                &NSString::from_str("synthetic.notification"),
                None,
                Some(&dictionary),
            )
        };
        let decoded = running_app_from(&notification).unwrap();
        assert_eq!(Retained::as_ptr(&decoded), Retained::as_ptr(&app));
        drop(notification);
        drop(dictionary);
        drop(app);
        assert_eq!(decoded.processIdentifier(), pid);
    }
}
