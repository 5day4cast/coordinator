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

/// Competition info for the leaderboard header
#[derive(Debug, Clone)]
pub struct LeaderboardInfo {
    pub competition_id: String,
    pub start_time: String,
    pub end_time: String,
    pub status: String,
}

/// Leaderboard content fragment
pub fn leaderboard(info: &LeaderboardInfo, scores: &[EntryScore]) -> Markup {
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

                // Observation period info
                div class="notification is-info is-light mb-4" {
                    div class="columns is-mobile" {
                        div class="column" {
                            p {
                                strong { "Observation Period" }
                            }
                            p class="is-size-7" {
                                span class="has-text-weight-semibold" { "Start: " }
                                span class="utc-time" data-utc=(info.start_time) { (info.start_time) }
                            }
                            p class="is-size-7" {
                                span class="has-text-weight-semibold" { "End: " }
                                span class="utc-time" data-utc=(info.end_time) { (info.end_time) }
                            }
                        }
                        div class="column is-narrow" {
                            span class=(format!("tag {}", status_class(&info.status))) {
                                (info.status)
                            }
                        }
                    }
                }
                // Script to convert UTC times to local - runs on fragment load
                script {
                    (maud::PreEscaped(r#"
                    (function() {
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
                    })();
                    "#))
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
                        tbody hx-get=(format!("/competitions/{}/leaderboard/rows", info.competition_id))
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

fn status_class(status: &str) -> &'static str {
    match status {
        "Registration" => "is-success",
        "Live" => "is-warning",
        "Completed" => "is-info",
        _ => "is-light",
    }
}

/// Single leaderboard row
pub fn leaderboard_row(score: &EntryScore) -> Markup {
    html! {
        tr hx-get=(format!("/entries/{}/detail", score.entry_id))
           hx-target="#entryValues"
           hx-swap="innerHTML"
           onclick="document.getElementById('entryScore').classList.add('is-active')"
           style="cursor: pointer;" {
            td { (score.rank) }
            td title=(score.entry_id) { (&score.entry_id[..8]) }
            td { (score.score) }
        }
    }
}
