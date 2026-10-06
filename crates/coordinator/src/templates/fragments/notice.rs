//! Notices shown across the public pages.

use maud::{html, Markup};

use crate::domain::SETTLE_ONLY_PAUSED;

/// The banner the competitions and entry pages show while the coordinator takes no new entries
/// (settle-only mode). Players see only that entries are paused.
pub fn entries_paused_banner() -> Markup {
    html! {
        div id="entriesPausedBanner" class="notification is-warning" role="status" {
            (SETTLE_ONLY_PAUSED) "."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_says_only_that_entries_are_paused() {
        let html = entries_paused_banner().into_string();
        assert!(html.contains("Entries are paused."));
        for operator_word in ["settle", "restore", "backup", "maintenance", "refund"] {
            assert!(
                !html.to_lowercase().contains(operator_word),
                "{operator_word}"
            );
        }
    }
}
