use maud::{html, Markup};

use crate::templates::pages::competitions::CompetitionView;

/// Single competition row for the table
pub fn competition_row(comp: &CompetitionView) -> Markup {
    html! {
        tr data-competition-id=(comp.id) {
            td {
                span class=(status_class(&comp.status)) {
                    (comp.status)
                }
            }
            td {
                span class="utc-time" data-utc=(comp.start_time) { (comp.start_time) }
            }
            td {
                span class="utc-time" data-utc=(comp.end_time) { (comp.end_time) }
            }
            td {
                span class="utc-time" data-utc=(comp.signing_time) { (comp.signing_time) }
            }
            td { (comp.entry_fee) }
            td { (comp.total_pool) }
            td { (comp.total_entries) }
            td { (comp.num_winners) }
            td {
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
