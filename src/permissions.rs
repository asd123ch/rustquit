use objc2_app_kit::NSWorkspace;
use objc2_application_services::{AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt};
use objc2_core_foundation::{CFBoolean, CFDictionary};
use objc2_foundation::{NSURL, ns_string};

/// Checks the Accessibility permission without triggering a dialog.
pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrustedWithOptions(None) }
}

/// Checks the permission and shows the system dialog on first call,
/// which deep-links to System Settings → Privacy & Security → Accessibility.
pub fn request_trust_with_prompt() -> bool {
    let key = unsafe { kAXTrustedCheckOptionPrompt };
    let dict = CFDictionary::from_slices(&[key], &[CFBoolean::new(true)]);
    unsafe { AXIsProcessTrustedWithOptions(Some(dict.as_ref())) }
}

/// Opens the Accessibility pane of System Settings.
pub fn open_accessibility_settings() {
    let url = NSURL::URLWithString(ns_string!(
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
    ));
    if let Some(url) = url {
        let workspace = NSWorkspace::sharedWorkspace();
        workspace.openURL(&url);
    }
}
