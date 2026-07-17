//! Ring buffer of recently terminated apps, shown in the tray menu with
//! reopen and temporary Keep-suppression actions.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

const MAX_ENTRIES: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RecentReason {
    AutoQuit,
    KeepTermination,
}

#[derive(Clone)]
pub struct RecentQuit {
    pub bundle_id: String,
    pub name: String,
    pub when: Instant,
    pub reason: RecentReason,
}

#[derive(Default)]
pub struct RecentQuits {
    entries: RefCell<VecDeque<RecentQuit>>,
}

pub type RecentQuitsHandle = Rc<RecentQuits>;

impl RecentQuits {
    pub fn new_handle() -> RecentQuitsHandle {
        Rc::new(RecentQuits::default())
    }

    pub fn push_auto_quit(&self, bundle_id: String, name: String) {
        self.push(bundle_id, name, RecentReason::AutoQuit);
    }

    pub fn push_keep_termination(&self, bundle_id: String, name: String) {
        self.push(bundle_id, name, RecentReason::KeepTermination);
    }

    fn push(&self, bundle_id: String, name: String, reason: RecentReason) {
        let mut entries = self.entries.borrow_mut();
        entries.retain(|e| !e.bundle_id.eq_ignore_ascii_case(&bundle_id));
        entries.push_front(RecentQuit {
            bundle_id,
            name,
            when: Instant::now(),
            reason,
        });
        entries.truncate(MAX_ENTRIES);
    }

    pub fn snapshot(&self) -> Vec<RecentQuit> {
        self.entries.borrow().iter().cloned().collect()
    }
}

/// "just now", "5m ago", "2h ago"
pub fn age_label(when: Instant) -> String {
    let secs = when.elapsed().as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn entries_are_deduplicated_and_bounded() {
        let recent = RecentQuits::default();
        for index in 0..10 {
            recent.push_auto_quit(format!("com.example.{index}"), format!("App {index}"));
        }
        assert_eq!(recent.snapshot().len(), MAX_ENTRIES);

        recent.push_keep_termination("com.example.5".into(), "Renamed".into());
        let entries = recent.snapshot();
        assert_eq!(entries[0].bundle_id, "com.example.5");
        assert_eq!(entries[0].name, "Renamed");
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.bundle_id == "com.example.5")
                .count(),
            1
        );
    }

    #[test]
    fn age_labels_use_expected_units() {
        assert_eq!(age_label(Instant::now()), "just now");
        assert_eq!(
            age_label(Instant::now() - Duration::from_secs(5 * 60)),
            "5m ago"
        );
        assert_eq!(
            age_label(Instant::now() - Duration::from_secs(2 * 60 * 60)),
            "2h ago"
        );
    }
}
