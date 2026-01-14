use maud::{html, Markup};

use crate::templates::pages::competitions::CompetitionView;

/// Single competition row for the table
pub fn competition_row(comp: &CompetitionView) -> Markup {
    html! {
        tr {
            td { (comp.id) }
            td { (comp.start_time) }
            td { (comp.end_time) }
            td { (comp.signing_time) }
            td {
                span class=(status_class(&comp.status)) {
                    (comp.status)
                }
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
                        "Create Entry"
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
        "completed" => "tag is-info",
        "failed" | "cancelled" => "tag is-danger",
        _ => "tag",
    }
}
