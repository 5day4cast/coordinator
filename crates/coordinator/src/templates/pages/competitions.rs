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
    pub number_of_values_per_entry: usize,
}

/// Competitions page content
pub fn competitions_page(competitions: &[CompetitionView]) -> Markup {
    html! {
        div id="allCompetitions" class="container" {
            div class="box" {
                div class="table-container" {
                    table id="competitionsDataTable"
                          class="table is-fullwidth is-striped is-hoverable competitions-table" {
                        thead {
                            tr {
                                th { "Status" }
                                th { "Start" }
                                th { "End" }
                                th { "Signing" }
                                th { "Fee" }
                                th { "Pool" }
                                th { "Entries" }
                                th { "Winners" }
                                th { "" }
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

        // JavaScript to convert UTC times to local timezone
        script {
            (maud::PreEscaped(r#"
            (function() {
                function formatLocalTimes() {
                    document.querySelectorAll('.utc-time').forEach(function(el) {
                        const utc = el.dataset.utc;
                        if (utc) {
                            const date = new Date(utc);
                            el.textContent = date.toLocaleString(undefined, {
                                month: 'short',
                                day: 'numeric',
                                hour: 'numeric',
                                minute: '2-digit'
                            });
                            el.title = date.toLocaleString();
                        }
                    });
                }
                // Run immediately for this fragment
                formatLocalTimes();
                // Also run after HTMX swaps (for auto-refresh of rows)
                document.body.addEventListener('htmx:afterSwap', formatLocalTimes);
            })();
            "#))
        }
    }
}
