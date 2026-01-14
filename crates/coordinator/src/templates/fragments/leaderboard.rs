use maud::{html, Markup};

use crate::domain::scoring::ScoredEntry;

/// Entry score for the leaderboard (simplified view)
#[derive(Debug, Clone)]
pub struct EntryScore {
    pub rank: usize,
    pub entry_id: String,
    pub score: i32,
}

impl EntryScore {
    pub fn from_scored_entry(entry: &ScoredEntry, rank: usize) -> Self {
        Self {
            rank,
            entry_id: entry.entry_id.clone(),
            score: entry.raw_score,
        }
    }
}

/// Leaderboard content fragment
pub fn leaderboard(competition_id: &str, scores: &[EntryScore]) -> Markup {
    html! {
        div id="competitionLeaderboard" class="container" {
            div class="box" {
                div class="level" {
                    div class="level-left" {
                        h4 class="title is-4" { "Leader Board" }
                    }
                    div class="level-right" {
                        button class="button is-light"
                               hx-get="/competitions"
                               hx-target="#main-content"
                               hx-push-url="true" {
                            "Back to Competitions"
                        }
                    }
                }

                div class="table-container" {
                    table id="competitionLeaderboardData"
                          class="table is-fullwidth is-striped is-hoverable" {
                        thead {
                            tr {
                                th { "Rank" }
                                th { "Entry ID" }
                                th { "Score" }
                            }
                        }
                        tbody hx-get=(format!("/competitions/{}/leaderboard/rows", competition_id))
                              hx-trigger="every 30s"
                              hx-swap="innerHTML" {
                            @for score in scores {
                                (leaderboard_row(score))
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Single leaderboard row
pub fn leaderboard_row(score: &EntryScore) -> Markup {
    html! {
        tr hx-get=(format!("/entries/{}/detail", score.entry_id))
           hx-target="#entryScore .modal-content .box"
           hx-swap="innerHTML"
           onclick="document.getElementById('entryScore').classList.add('is-active')"
           style="cursor: pointer;" {
            td { (score.rank) }
            td { (score.entry_id) }
            td { (score.score) }
        }
    }
}
