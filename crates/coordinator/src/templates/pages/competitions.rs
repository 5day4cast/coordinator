use maud::{html, Markup};

use crate::templates::fragments::competition_row;

/// View data for a competition
#[derive(Debug, Clone)]
pub struct CompetitionView {
    pub id: String,
    pub start_time: String,
    pub end_time: String,
    pub signing_time: String,
    pub status: String,
    pub entry_fee: u64,
    pub total_pool: u64,
    pub total_entries: u64,
    pub num_winners: u64,
    pub can_enter: bool,
}

/// Competitions page content
pub fn competitions_page(competitions: &[CompetitionView]) -> Markup {
    html! {
        div id="allCompetitions" class="container" {
            div class="box" {
                div class="table-container" {
                    table id="competitionsDataTable"
                          class="table is-fullwidth is-striped is-hoverable" {
                        thead {
                            tr {
                                th { "ID" }
                                th { "Start Time" }
                                th { "End Time" }
                                th { "Signing Time" }
                                th { "Status" }
                                th { "Entry fee (sats)" }
                                th { "Total Prize Pool (sats)" }
                                th { "Total Entries" }
                                th { "Places 'In The Money'" }
                                th { "Enter/View" }
                            }
                        }
                        tbody hx-get="/competitions/rows"
                              hx-trigger="every 30s"
                              hx-swap="innerHTML" {
                            @for comp in competitions {
                                (competition_row(comp))
                            }
                        }
                    }
                }
            }
        }
    }
}
