use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::mpsc::TryRecvError;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSWorkspace};
use objc2_foundation::{NSString, NSTimer, NSURL, ns_string};

use crate::config::ConfigHandle;
use crate::network::PendingRequest;
pub use crate::release_data::UpdateStatus;
use crate::release_data::{MAX_RESPONSE_BYTES, RELEASE_API, parse_release};

impl UpdateStatus {
    fn menu_title(&self) -> Option<String> {
        match self {
            Self::Available { version, .. } => Some(format!("Update to RustQuit {version}...")),
            _ => None,
        }
    }
}

pub struct UpdateMonitor {
    config: ConfigHandle,
    status: RefCell<UpdateStatus>,
    pending: RefCell<Option<PendingRequest>>,
    show_result: Cell<bool>,
    daily_timer: RefCell<Option<objc2::rc::Retained<NSTimer>>>,
    poll_timer: RefCell<Option<objc2::rc::Retained<NSTimer>>>,
}

impl UpdateMonitor {
    pub fn start(config: ConfigHandle) -> Rc<Self> {
        let monitor = Rc::new(Self {
            config,
            status: RefCell::new(UpdateStatus::Idle),
            pending: RefCell::new(None),
            show_result: Cell::new(false),
            daily_timer: RefCell::new(None),
            poll_timer: RefCell::new(None),
        });
        let weak = Rc::downgrade(&monitor);
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            crate::engine::catch_callback_panic("automatic update check", || {
                if let Some(monitor) = weak.upgrade() {
                    monitor.check_automatically();
                }
            });
        });
        monitor.daily_timer.replace(Some(unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(86400.0, true, &block)
        }));
        monitor.check_automatically();
        monitor
    }

    fn check_automatically(self: &Rc<Self>) {
        if self.config.data.borrow().check_for_updates {
            self.check_now(false);
        }
    }

    pub fn menu_title(&self) -> Option<String> {
        if !self.config.data.borrow().check_for_updates {
            return None;
        }
        self.status.borrow().menu_title()
    }

    pub fn open_available_release(&self) {
        if let UpdateStatus::Available { url, .. } = &*self.status.borrow()
            && let Some(url) = NSURL::URLWithString(&NSString::from_str(url))
        {
            NSWorkspace::sharedWorkspace().openURL(&url);
        }
    }

    pub fn check_now(self: &Rc<Self>, show_result: bool) {
        self.show_result.set(self.show_result.get() || show_result);
        if self.pending.borrow().is_some() {
            return;
        }
        self.status.replace(UpdateStatus::Checking);
        match crate::network::get(RELEASE_API, MAX_RESPONSE_BYTES) {
            Ok(request) => {
                self.pending.replace(Some(request));
            }
            Err(_) => {
                self.finish(UpdateStatus::Unavailable);
                return;
            }
        }
        let weak = Rc::downgrade(self);
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            crate::engine::catch_callback_panic("update request poll", || {
                if let Some(monitor) = weak.upgrade() {
                    monitor.poll();
                }
            });
        });
        self.poll_timer.replace(Some(unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.25, true, &block)
        }));
    }

    fn poll(&self) {
        let result = self.pending.borrow().as_ref().map(PendingRequest::try_recv);
        let status = match result {
            Some(Ok(Ok(response))) => parse_release(
                response.status,
                &response.final_url,
                &response.body,
                env!("CARGO_PKG_VERSION"),
            )
            .unwrap_or(UpdateStatus::Unavailable),
            Some(Ok(Err(_))) | Some(Err(TryRecvError::Disconnected)) => UpdateStatus::Unavailable,
            Some(Err(TryRecvError::Empty)) | None => return,
        };
        self.pending.take();
        if let Some(timer) = self.poll_timer.take() {
            timer.invalidate();
        }
        self.finish(status);
    }

    fn finish(&self, status: UpdateStatus) {
        if status == UpdateStatus::Unavailable {
            tracing::warn!("GitHub update check unavailable");
        }
        self.status.replace(status.clone());
        if !self.show_result.replace(false) {
            return;
        }
        let mtm = MainThreadMarker::new().expect("update results run on the main thread");
        let alert = NSAlert::new(mtm);
        alert.setMessageText(ns_string!("RustQuit Updates"));
        match &status {
            UpdateStatus::Available { version, .. } => {
                alert.setInformativeText(&NSString::from_str(&format!(
                    "RustQuit {version} is available. With Homebrew: brew upgrade --cask rustquit"
                )));
                alert.addButtonWithTitle(ns_string!("View Release"));
                alert.addButtonWithTitle(ns_string!("Later"));
            }
            UpdateStatus::UpToDate => {
                alert.setInformativeText(ns_string!(
                    "You are running the latest available version."
                ));
                alert.addButtonWithTitle(ns_string!("OK"));
            }
            _ => {
                alert.setInformativeText(ns_string!(
                    "Could not check for updates. Please try again later."
                ));
                alert.addButtonWithTitle(ns_string!("OK"));
            }
        }
        if alert.runModal() == NSAlertFirstButtonReturn
            && let UpdateStatus::Available { url, .. } = &status
            && let Some(url) = NSURL::URLWithString(&NSString::from_str(url))
        {
            NSWorkspace::sharedWorkspace().openURL(&url);
        }
    }
}

impl Drop for UpdateMonitor {
    fn drop(&mut self) {
        for timer in [&self.daily_timer, &self.poll_timer] {
            if let Some(timer) = timer.take() {
                timer.invalidate();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::Response;
    use crate::release_data::{InvalidRelease, RELEASES_URL};

    fn parse_response(response: Response, current: &str) -> Result<UpdateStatus, InvalidRelease> {
        parse_release(
            response.status,
            &response.final_url,
            &response.body,
            current,
        )
    }

    #[test]
    fn menu_entry_exists_only_for_an_available_update() {
        for status in [
            UpdateStatus::Idle,
            UpdateStatus::Checking,
            UpdateStatus::UpToDate,
            UpdateStatus::Unavailable,
        ] {
            assert_eq!(status.menu_title(), None);
        }
        let available = UpdateStatus::Available {
            version: "0.5.0".to_owned(),
            url: format!("{RELEASES_URL}/tag/v0.5.0"),
        };
        assert_eq!(
            available.menu_title().as_deref(),
            Some("Update to RustQuit 0.5.0...")
        );
    }

    fn response(version: &str) -> Response {
        Response {
            status: 200,
            final_url: RELEASE_API.to_owned(),
            body: serde_json::to_vec(&serde_json::json!({
                "tag_name": format!("v{version}"),
                "html_url": format!("{RELEASES_URL}/tag/v{version}"),
                "draft": false,
                "prerelease": false,
            }))
            .unwrap(),
        }
    }

    #[test]
    fn offers_only_newer_versions() {
        assert!(matches!(
            parse_response(response("0.4.10"), "0.4.8"),
            Ok(UpdateStatus::Available { .. })
        ));
        for version in ["0.4.8", "0.4.1"] {
            assert_eq!(
                parse_response(response(version), "0.4.8"),
                Ok(UpdateStatus::UpToDate)
            );
        }
    }

    #[test]
    fn rejects_untrusted_and_unpublished_releases() {
        for (field, value) in [
            (
                "html_url",
                serde_json::json!("https://example.com/download"),
            ),
            ("draft", serde_json::json!(true)),
            ("prerelease", serde_json::json!(true)),
        ] {
            let mut response = response("0.4.9");
            let mut body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            body[field] = value;
            response.body = serde_json::to_vec(&body).unwrap();
            assert!(parse_response(response, "0.4.8").is_err());
        }
        for version in ["0.5.0-beta.1", "0.5.0+build", "invalid"] {
            assert!(parse_response(response(version), "0.4.8").is_err());
        }
    }

    #[test]
    fn handles_missing_releases_and_network_failures() {
        let mut missing = response("0.4.9");
        missing.status = 404;
        assert_eq!(parse_response(missing, "0.4.8"), Ok(UpdateStatus::UpToDate));
        for status in [301, 403, 429, 500] {
            let mut failed = response("0.4.9");
            failed.status = status;
            assert!(parse_response(failed, "0.4.8").is_err());
        }
        let mut redirected = response("0.4.9");
        redirected.final_url = "https://example.com".to_owned();
        assert!(parse_response(redirected, "0.4.8").is_err());
        let mut malformed = response("0.4.9");
        malformed.body = b"not json".to_vec();
        assert!(parse_response(malformed, "0.4.8").is_err());
    }
}
