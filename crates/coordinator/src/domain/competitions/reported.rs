//! Warn about a lasting condition once, rather than on every pass that finds it again.
//!
//! Runners and sweeps revisit the same competitions, tickets, and swaps every few seconds. A
//! condition that persists would otherwise log the same warning each time.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use uuid::Uuid;

/// What was last reported about each subject, per topic.
#[derive(Default)]
pub struct Reported {
    last: Mutex<HashMap<(&'static str, Uuid), String>>,
}

impl Reported {
    /// Whether `what` is news about `subject` under `topic`: nothing was reported yet, or the
    /// last report said something else.
    pub fn is_new(&self, topic: &'static str, subject: Uuid, what: &str) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        match last.get(&(topic, subject)) {
            Some(previous) if previous == what => false,
            _ => {
                last.insert((topic, subject), what.to_owned());
                true
            }
        }
    }

    /// Forget `subject` under `topic` once its condition clears, so a recurrence is reported.
    pub fn clear(&self, topic: &'static str, subject: Uuid) {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(topic, subject));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_condition_is_reported_once() {
        let reported = Reported::default();
        let subject = Uuid::now_v7();
        assert!(reported.is_new("swap", subject, "no VTXO"));
        assert!(!reported.is_new("swap", subject, "no VTXO"));
        assert!(
            reported.is_new("refund", subject, "no VTXO"),
            "topics are separate"
        );
        assert!(
            reported.is_new("swap", subject, "wrong amount"),
            "a different condition is news"
        );
        reported.clear("swap", subject);
        assert!(
            reported.is_new("swap", subject, "wrong amount"),
            "a condition that cleared and came back is news"
        );
    }
}
