//! The logged-in player's entries.

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::domain::UserEntryView;
use crate::templates::{
    format,
    fragments::picks::detail_url,
    pages::competitions::{phase_badge, CompetitionView},
};

/// An entry with its competition, when the competition could be loaded.
pub struct EntryRow<'a> {
    pub entry: &'a UserEntryView,
    pub competition: Option<&'a CompetitionView>,
}

/// Entries page content (requires auth)
pub fn entries_page(rows: &[EntryRow], open: Option<&CompetitionView>) -> Markup {
    html! {
        div id="allEntries" class="account-page" {
            h1 class="title is-4" { "Your entries" }
            @if rows.is_empty() {
                (no_entries(open))
            } @else {
                div class="table-container" {
                    table id="entriesDataTable" class="table is-fullwidth is-hoverable entries-table" {
                        thead {
                            tr {
                                th { "Competition" }
                                th { "Status" }
                                th { "Entry" }
                                th { "Payment" }
                                th {}
                            }
                        }
                        tbody {
                            @for row in rows {
                                (entry_row(row))
                            }
                        }
                    }
                }
                p class="help" { "Select an entry to see its picks and how each one scored." }
            }
        }
    }
}

/// A click anywhere on the row opens the entry's picks. The copy button stops
/// its own click (see page.js), the Picks button's click reaches the row, and
/// the Leaderboard link consumes its click so the row does not see it.
fn entry_row(row: &EntryRow) -> Markup {
    let picks = detail_url(&row.entry.entry_id);
    let leaderboard = format!("/competitions/{}/leaderboard", row.entry.competition_id);
    html! {
        tr class="is-clickable" hx-get=(picks) hx-target="#entryValues" hx-swap="innerHTML" {
            td data-label="Competition" {
                @match row.competition {
                    Some(competition) => { (format::window(competition.start, competition.end)) }
                    None => { (row.entry.start_time) }
                }
            }
            td data-label="Status" {
                @if let Some(competition) = row.competition { (phase_badge(competition)) }
            }
            td data-label="Entry" { (format::copyable_id(&row.entry.entry_id)) }
            td data-label="Payment" { (row.entry.status) }
            td class="has-text-right entry-links" {
                button type="button" class="button is-small is-text picks-button" { "Picks" }
                a href=(leaderboard) hx-get=(leaderboard) hx-trigger="click consume"
                  hx-target="#main-content" hx-push-url="true" { "Leaderboard" }
            }
        }
    }
}

/// Empty entries message, pointing at a competition that takes entries now.
pub fn no_entries(open: Option<&CompetitionView>) -> Markup {
    html! {
        div class="empty-state-box" {
            p { "You haven't entered a competition yet." }
            @if let Some(competition) = open {
                a class="button is-primary mt-3"
                  href=(competition.url()) hx-get=(competition.url())
                  hx-target="#main-content" hx-push-url="true" {
                    "Enter the next competition (starts in "
                    (format::duration(competition.start - OffsetDateTime::now_utc()))
                    ")"
                }
            } @else {
                a href="/competitions" hx-get="/competitions" hx-target="#main-content" hx-push-url="true" {
                    "See upcoming competitions"
                }
            }
        }
    }
}

/// What a signed-out visitor sees at an account page's address. The key lives
/// only in this tab's memory, so a reload always lands here; logging in loads
/// the page in place. So does Back while logged in (`fw:reload`, see
/// htmx_auth.js), since htmx asks the server again without a signature.
pub fn sign_in_required(path: &str, what: &str) -> Markup {
    html! {
        div class="account-page sign-in-required" data-signed-reload
            hx-get=(path) hx-trigger="fw:login from:body, fw:reload" hx-target="this" hx-swap="outerHTML"
            hx-disinherit="*" {
            h1 class="title is-4" { "Log in to see " (what) }
            p {
                "You're signed out. For safety your key is kept only in this tab's memory, "
                "so reloading the page or opening a new tab signs you out."
            }
            div class="buttons mt-4" {
                button type="button" class="button is-primary" data-open-modal="loginModal" { "Log in" }
                button type="button" class="button is-light" data-open-modal="registerModal" { "Sign up" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::pages::competitions::{
        tests::{view, NOW},
        Phase,
    };

    fn entry() -> UserEntryView {
        UserEntryView {
            entry_id: "01a0d0f5-52e1-7141-b4e1-8dbd7169fc2e".into(),
            competition_id: "c1".into(),
            start_time: String::new(),
            end_time: String::new(),
            status: "Entry Paid".into(),
        }
    }

    #[test]
    fn entries_link_to_picks_and_the_leaderboard() {
        let entry = entry();
        let competition = view("c1", Phase::Live, -5);
        let html = entries_page(
            &[EntryRow {
                entry: &entry,
                competition: Some(&competition),
            }],
            None,
        )
        .into_string();
        assert!(html.contains(r#"hx-get="/entries/01a0d0f5-52e1-7141-b4e1-8dbd7169fc2e/detail""#));
        assert!(html.contains(r#"href="/competitions/c1/leaderboard""#));
        assert!(html.contains(r#"hx-trigger="click consume""#));
        assert!(!html.contains("closest"), "trigger filters need eval, which the CSP forbids");
        assert!(html.contains("…7169fc2e"));
        assert!(html.contains("badge-live"));
    }

    #[test]
    fn no_entries_links_to_an_open_competition() {
        let mut open = view("next", Phase::Open, 90);
        open.start = NOW.max(OffsetDateTime::now_utc()) + time::Duration::minutes(90);
        let html = entries_page(&[], Some(&open)).into_string();
        assert!(html.contains(r#"href="/competitions/next/entry-form""#));
        assert!(html.contains("Enter the next competition"));
    }

    #[test]
    fn signed_out_visitors_are_told_why_and_can_log_in() {
        let html = sign_in_required("/entries", "your entries").into_string();
        assert!(html.contains("reloading the page or opening a new tab signs you out"));
        assert!(html.contains(r#"data-open-modal="loginModal""#));
        assert!(html.contains(r#"hx-trigger="fw:login from:body, fw:reload""#));
    }
}
