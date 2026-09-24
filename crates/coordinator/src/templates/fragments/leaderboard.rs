//! A competition's leaderboard: pot, prizes, and every entry's score.
//!
//! The page itself needs only the competition, so it renders at once; the
//! rows need the oracle's scores and load into it afterwards (and every 30
//! seconds while results can still change). Rows arrive as
//! [`LeaderboardRow`]s, best first; this module only decides how they are
//! shown (shared ranks for ties, player names, the viewer's own entries).

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::domain::scoring::ScoredEntry;
use crate::templates::{
    format::{self, ordinal, sats},
    pages::competitions::{phase_badge, CompetitionView, Phase},
};

/// Entry score for the leaderboard (simplified view)
#[derive(Debug, Clone)]
pub struct EntryScore {
    pub rank: usize,
    pub entry_id: String,
    pub username: String,
    pub score: i32,
}

impl EntryScore {
    pub fn from_scored_entry(entry: &ScoredEntry, rank: usize) -> Self {
        Self {
            rank,
            entry_id: entry.entry_id.clone(),
            username: String::new(),
            score: entry.raw_score,
        }
    }
}

/// One leaderboard row as shown.
#[derive(Debug, Clone)]
pub struct LeaderboardRow {
    /// Shared by tied scores: 20, 10, 10, 0 rank 1, 2, 2, 4.
    pub rank: usize,
    pub entry_id: String,
    pub player: String,
    /// Hash of the entrant's npub; the browser marks rows matching its own.
    pub owner: String,
    pub score: i32,
}

/// Ranks for scores listed best first: equal scores share the better rank.
pub fn shared_ranks(scores: &[i32]) -> Vec<usize> {
    scores
        .iter()
        .map(|score| 1 + scores.iter().filter(|other| *other > score).count())
        .collect()
}

/// The address the leaderboard loads its rows from.
pub fn rows_url(competition_id: &str) -> String {
    format!("/competitions/{competition_id}/leaderboard/rows")
}

/// Leaderboard page content; the rows load separately (see [`leaderboard_rows`]).
pub fn leaderboard(competition: &CompetitionView, now: OffsetDateTime) -> Markup {
    let prizes = competition.prizes();
    // Scores can change until the result is final; poll until then.
    let trigger = if matches!(competition.phase, Phase::Live | Phase::AwaitingResults) {
        "load, every 30s"
    } else {
        "load"
    };
    html! {
        div id="competitionLeaderboard" class="leaderboard" {
            a class="back-link" href="/competitions" hx-get="/competitions"
              hx-target="#main-content" hx-push-url="true" { "← All competitions" }

            div class="leaderboard-heading" {
                h1 class="title is-4" { "Leaderboard" }
                (phase_badge(competition))
            }
            p class="leaderboard-window" {
                (format::window(competition.start, competition.end))
                @match competition.phase {
                    Phase::Open => { " · starts in " (format::duration(competition.start - now)) }
                    Phase::Live => { " · results in " (format::duration(competition.end - now)) }
                    _ => {}
                }
            }

            dl class="leaderboard-facts" {
                div { dt { "Pot" } dd { (sats(competition.total_pool)) } }
                div {
                    dt { "Paid places" }
                    dd {
                        @for (place, (percent, amount)) in prizes.iter().enumerate() {
                            span class="prize" { (ordinal(place + 1)) " " (sats(*amount)) " (" (percent) "%)" }
                        }
                    }
                }
                div { dt { "Entries" } dd { (competition.total_entries) " of " (competition.total_allowed_entries) } }
                div { dt { "Entry" } dd { (sats(competition.ticket_price)) } }
            }

            @if competition.can_enter {
                a class="button is-primary mb-4"
                  href=(competition.url()) hx-get=(competition.url())
                  hx-target="#main-content" hx-push-url="true" { "Enter this competition" }
            }
            @if competition.did_not_fill() {
                p class="notice" {
                    "Not enough entries arrived before the window started, so this competition was cancelled "
                    "and every entry fee was returned."
                }
            }

            @if competition.total_entries == 0 {
                p class="empty-state" { "No entries yet." }
            } @else {
                div class="table-container" {
                    table id="competitionLeaderboardData" class="table is-fullwidth is-hoverable leaderboard-table" {
                        thead {
                            tr {
                                th { "Rank" }
                                th { "Player" }
                                th { "Entry" }
                                th class="has-text-right" { "Score" }
                                th {}
                            }
                        }
                        tbody id="leaderboardRows" hx-get=(rows_url(&competition.id))
                              hx-trigger=(trigger) hx-swap="innerHTML" {
                            tr class="rows-loading" {
                                td colspan="5" {
                                    span class="spinner" aria-hidden="true" {}
                                    "Loading scores…"
                                }
                            }
                        }
                    }
                }
                p class="help" {
                    "Select an entry to see its picks. Tied scores share a rank"
                    @if !prizes.is_empty() {
                        "; if a tie spans the last paid place, the oracle's tie-break decides who is paid"
                    }
                    "."
                }
            }
        }
    }
}

/// The leaderboard's rows, loaded after the page and refreshed while live.
pub fn leaderboard_rows(rows: &[LeaderboardRow]) -> Markup {
    html! {
        @for row in rows {
            (leaderboard_row(row))
        }
        @if rows.is_empty() {
            tr {
                td colspan="5" class="empty-state" {
                    "No scores yet: they appear once the oracle has the entries."
                }
            }
        }
    }
}

fn leaderboard_row(row: &LeaderboardRow) -> Markup {
    let picks = format!("/entries/{}/detail", row.entry_id);
    html! {
        // Links inside the row do their own thing; a click elsewhere opens the picks.
        tr class="is-clickable" data-owner=(row.owner)
           hx-get=(picks) hx-target="#entryValues" hx-swap="innerHTML"
           hx-trigger="click[!target.closest('a, button')]" {
            td data-label="Rank" { (row.rank) }
            td data-label="Player" {
                (row.player)
                span class="you-badge" { "You" }
            }
            td data-label="Entry" { (format::copyable_id(&row.entry_id)) }
            td data-label="Score" class="has-text-right" { (row.score) " pts" }
            td class="has-text-right" {
                a href=(picks) hx-get=(picks) hx-target="#entryValues" hx-swap="innerHTML" { "Picks" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::pages::competitions::tests::{view, NOW};

    fn row(entry_id: &str, player: &str, score: i32, rank: usize) -> LeaderboardRow {
        LeaderboardRow {
            rank,
            entry_id: entry_id.to_owned(),
            player: player.to_owned(),
            owner: format!("owner-{player}"),
            score,
        }
    }

    #[test]
    fn tied_scores_share_a_rank() {
        assert_eq!(shared_ranks(&[0, 0, 0]), vec![1, 1, 1]);
        assert_eq!(shared_ranks(&[20, 10, 10, 0]), vec![1, 2, 2, 4]);
        assert_eq!(shared_ranks(&[]), Vec::<usize>::new());
    }

    #[test]
    fn the_page_shows_the_pot_and_prizes_and_loads_its_rows() {
        let mut competition = view("c1", Phase::Finished, -60);
        competition.paid_places = 2;
        let html = leaderboard(&competition, NOW).into_string();
        assert!(html.contains("15,000 sats"));
        assert!(html.contains("1st 10,500 sats (70%)"));
        assert!(html.contains("2nd 4,500 sats (30%)"));
        assert!(html.contains(r#"hx-get="/competitions/c1/leaderboard/rows""#));
        assert!(html.contains(r#"hx-trigger="load""#), "a finished board loads once");
        assert!(html.contains("Loading scores"));
        assert!(html.contains("tie-break"));
    }

    #[test]
    fn rows_show_distinct_ids_players_and_owners() {
        let rows = vec![
            row("01a0d0f5-0000-7000-8000-00000000aaaa", "alice", 20, 1),
            row("01a0d0f5-0000-7000-8000-00000000bbbb", "npub1abcd…wxyz", 20, 1),
            row("01a0d0f5-0000-7000-8000-00000000cccc", "carol", 20, 1),
        ];
        let html = leaderboard_rows(&rows).into_string();
        assert!(html.contains("…0000aaaa") && html.contains("…0000bbbb"));
        assert!(html.contains(r#"data-copy="01a0d0f5-0000-7000-8000-00000000aaaa""#));
        assert!(html.contains(r#"data-owner="owner-alice""#));
        assert!(html.contains("npub1abcd…wxyz"));
        assert!(!html.contains("No scores yet"));
        assert!(leaderboard_rows(&[]).into_string().contains("No scores yet"));
    }

    #[test]
    fn a_live_board_refreshes_its_rows() {
        let html = leaderboard(&view("c1", Phase::Live, -5), NOW).into_string();
        assert!(html.contains(r#"hx-trigger="load, every 30s""#));
        assert!(html.contains("results in 5 min"));
    }

    #[test]
    fn an_unfilled_competition_explains_its_refunds() {
        let mut competition = view("c1", Phase::Cancelled, -60);
        competition.total_entries = 0;
        let html = leaderboard(&competition, NOW).into_string();
        assert!(html.contains("every entry fee was returned"));
        assert!(html.contains("No entries yet."));
        assert!(!html.contains("leaderboard/rows"), "nothing to load without entries");
    }
}
