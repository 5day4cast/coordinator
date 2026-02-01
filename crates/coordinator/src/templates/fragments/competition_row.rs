use maud::{html, Markup};

use crate::templates::pages::competitions::CompetitionView;

/// Single competition row for the table
pub fn competition_row(comp: &CompetitionView) -> Markup {
    html! {
        tr data-competition-id=(comp.id) {
            td data-label="Status" {
                span class=(status_class(&comp.status)) {
                    (comp.status)
                }
            }
            td data-label="Start" {
                span class="utc-time" data-utc=(comp.start_time) { (comp.start_time) }
            }
            td data-label="End" {
                span class="utc-time" data-utc=(comp.end_time) { (comp.end_time) }
            }
            td data-label="Signing" {
                span class="utc-time" data-utc=(comp.signing_time) { (comp.signing_time) }
            }
            td data-label="Fee" { (comp.entry_fee) }
            td data-label="Pool" { (comp.total_pool) }
            td data-label="Entries" { (comp.total_entries) }
            td data-label="Winners" { (comp.num_winners) }
            td data-label="" {
                @if comp.can_enter {
                    button class="button is-primary is-small"
                           data-competition-id=(comp.id)
                           hx-get=(format!("/competitions/{}/entry-form", comp.id))
                           hx-target="#main-content"
                           hx-push-url="true" {
                        "Enter"
                    }
                } @else {
                    button class="button is-info is-small"
                           hx-get=(format!("/competitions/{}/leaderboard", comp.id))
                           hx-target="#main-content"
                           hx-push-url="true" {
                        "View"
                    }
                }
            }
        }
    }
}

fn status_class(status: &str) -> &'static str {
    match status.to_lowercase().as_str() {
        "registration" => "tag is-success",
        "setup" | "live" => "tag is-warning",
        "signing" => "tag is-warning",
        "awaiting results" => "tag is-info is-light",
        "completed" => "tag is-info",
        "failed" | "cancelled" => "tag is-danger",
        _ => "tag",
    }
}
