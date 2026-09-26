//! A competition's leaderboard: pot, prizes, and every entry's score.
//!
//! The page itself needs only the competition, so it renders at once; the
//! scores need the oracle and load into it afterwards, then every minute while
//! the window is open. They arrive as a [`LeaderboardView`] from the
//! leaderboard seam, best first; this module only decides how they are shown
//! (shared ranks for ties, player names, provisional scores, the viewer's own
//! entries).

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::domain::leaderboard::Phase;
use crate::templates::{
    format::{self, ordinal, sats},
    fragments::picks::{detail_url, LIVE_REFRESH},
    pages::competitions::{phase_badge, CompetitionView, Refunds},
};

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

/// The address the leaderboard loads its rows from.
pub fn rows_url(competition_id: &str) -> String {
    format!("/competitions/{competition_id}/leaderboard/rows")
}

/// Everything the leaderboard's scores show, from the domain's leaderboard.
#[derive(Debug, Clone)]
pub struct LeaderboardView {
    /// Best first; tied scores share a rank.
    pub rows: Vec<LeaderboardRow>,
    /// The leaderboard's phase: `Scored` as soon as the oracle's result is final.
    pub phase: Phase,
    /// When the readings behind the scores last changed, if known.
    pub updated_at: Option<OffsetDateTime>,
    /// Any pick has a reading from the window.
    pub any_readings: bool,
}

/// Whether a competition in `phase` ran, so its entries have scores. One that didn't fill, or
/// was cancelled or failed before its contract, scores nothing.
pub fn ran(phase: Phase) -> bool {
    !matches!(phase, Phase::Unfilled | Phase::Cancelled | Phase::Failed)
}

/// Leaderboard page content. The scores load separately (see
/// [`leaderboard_scores`]), so the page shows at once.
pub fn leaderboard(competition: &CompetitionView, now: OffsetDateTime) -> Markup {
    let prizes = competition.prizes();
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
                    Phase::Upcoming => { " · starts in " (format::duration(competition.start - now)) }
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
                    "Not enough entries arrived before the window started, so this competition "
                    "doesn't run and nothing is scored. "
                    (refund_note(competition))
                }
            } @else if !ran(competition.phase) {
                p class="notice" { "This competition did not run, so nothing is scored." }
            }

            @if competition.total_entries == 0 {
                p class="empty-state" { "No entries yet." }
            } @else {
                div id="leaderboardScores" class="leaderboard-scores"
                    hx-get=(rows_url(&competition.id)) hx-trigger="load" hx-swap="outerHTML"
                    "hx-status:500"="swap:outerHTML" {
                    (scores_table(
                        competition,
                        false,
                        html! {
                            tr class="rows-loading" {
                                td colspan="5" {
                                    span class="spinner" aria-hidden="true" {}
                                    "Loading scores…"
                                }
                            }
                        },
                    ))
                }
            }
        }
    }
}

/// The leaderboard's scores, loaded after the page. While the window is open
/// they reload every minute; the reload after it closes, or after the
/// competition is cancelled, carries no trigger, so the polling stops there.
pub fn leaderboard_scores(
    competition: &CompetitionView,
    board: &LeaderboardView,
    now: OffsetDateTime,
) -> Markup {
    let live = board.phase == Phase::Live;
    let provisional = matches!(board.phase, Phase::Live | Phase::AwaitingResult);
    let url = rows_url(&competition.id);
    html! {
        div id="leaderboardScores" class="leaderboard-scores"
            hx-get=[live.then_some(&url)] hx-trigger=[live.then_some(LIVE_REFRESH)]
            hx-swap=[live.then_some("outerHTML")] "hx-status:500"=[live.then_some("swap:outerHTML")] {
            @match board.phase {
                Phase::Live => {
                    p class="provisional-note" {
                        strong { "Provisional:" } " if the window ended now"
                        @if let Some(at) = board.updated_at {
                            " · updated " (format::ago(at, now))
                        }
                    }
                }
                Phase::AwaitingResult => {
                    p class="provisional-note" {
                        strong { "Window closed:" } " awaiting the oracle's result. "
                        "Scores are what the readings so far give."
                    }
                }
                Phase::Expired => {
                    p class="notice" { "The oracle never published a result, so every entry was refunded." }
                }
                Phase::Scored if competition.pot_refunded => {
                    p class="notice" {
                        "No station reported inside the window, so nothing was scored and every "
                        "entry was refunded: the pot went back to all " (competition.total_entries)
                        " entries in equal shares, less network fees."
                    }
                }
                Phase::Scored if !board.any_readings && !board.rows.is_empty() => {
                    p class="notice" {
                        "No readings were recorded at these stations during the window, so every "
                        "entry scored 0 and tied. The oracle's tie-break decided who was paid."
                    }
                }
                _ => {}
            }
            (scores_table(
                competition,
                provisional,
                leaderboard_rows(&board.rows, ran(board.phase) && !competition.pot_refunded),
            ))
        }
    }
}

fn scores_table(competition: &CompetitionView, provisional: bool, rows: Markup) -> Markup {
    html! {
        div class="table-container" {
            table id="competitionLeaderboardData" class="table is-fullwidth is-hoverable leaderboard-table" {
                thead {
                    tr {
                        th { "Rank" }
                        th { "Player" }
                        th { "Entry" }
                        th class="has-text-right" { @if provisional { "Score so far" } @else { "Score" } }
                        th {}
                    }
                }
                tbody id="leaderboardRows" { (rows) }
            }
        }
        p class="help" {
            "Select an entry to see its picks. Tied scores share a rank"
            @if !competition.prizes().is_empty() {
                "; if a tie spans the last paid place, the oracle's tie-break decides who is paid"
            }
            "."
        }
    }
}

/// What happened to the entry fees of a competition that didn't fill.
fn refund_note(competition: &CompetitionView) -> Markup {
    let progress = competition.refunds;
    html! {
        @match competition.refunds() {
            Refunds::Nothing => { "No entry fees were paid." }
            Refunds::Done => { "Every entry fee has been returned." }
            Refunds::Pending if progress.escrowed > 0 => {
                "Entry fees go back to the refund destination shown when entering: "
                (progress.refunded) " of " (progress.escrowed) " returned so far."
            }
            Refunds::Pending => {
                "Entry fees go back to the refund destination shown when entering once it is cancelled."
            }
        }
    }
}

/// The rows of the leaderboard's table. Entries of a competition that didn't run are listed
/// without a rank or score.
pub fn leaderboard_rows(rows: &[LeaderboardRow], scored: bool) -> Markup {
    html! {
        @for row in rows {
            (leaderboard_row(row, scored))
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

/// A click anywhere on the row opens the entry's picks. The copy button
/// stops its own click (see page.js); the Picks button's click reaches the row.
fn leaderboard_row(row: &LeaderboardRow, scored: bool) -> Markup {
    html! {
        tr class="is-clickable" data-owner=(row.owner)
           hx-get=(detail_url(&row.entry_id)) hx-target="#entryValues" hx-swap="innerHTML"
           "hx-status:500"="swap:innerHTML" {
            td data-label="Rank" { @if scored { (row.rank) } @else { "—" } }
            td data-label="Player" {
                (row.player)
                span class="you-badge" { "You" }
            }
            td data-label="Entry" { (format::copyable_id(&row.entry_id)) }
            td data-label="Score" class="has-text-right" {
                @if scored { (row.score) " pts" } @else { "not scored" }
            }
            td class="has-text-right" {
                button type="button" class="button is-small is-text picks-button" { "Picks" }
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
    fn the_page_shows_the_pot_and_prizes_and_loads_its_rows() {
        let mut competition = view("c1", Phase::Scored, -60);
        competition.paid_places = 2;
        let html = leaderboard(&competition, NOW).into_string();
        assert!(html.contains("15,000 sats"));
        assert!(html.contains("1st 10,500 sats (70%)"));
        assert!(html.contains("2nd 4,500 sats (30%)"));
        assert!(html.contains(r#"hx-get="/competitions/c1/leaderboard/rows""#));
        assert!(
            html.contains(r#"hx-trigger="load""#),
            "a finished board loads once"
        );
        assert!(html.contains("Loading scores"));
        assert!(html.contains("tie-break"));
    }

    #[test]
    fn rows_show_distinct_ids_players_and_owners() {
        let rows = vec![
            row("01a0d0f5-0000-7000-8000-00000000aaaa", "alice", 20, 1),
            row(
                "01a0d0f5-0000-7000-8000-00000000bbbb",
                "npub1abcd…wxyz",
                20,
                1,
            ),
            row("01a0d0f5-0000-7000-8000-00000000cccc", "carol", 20, 1),
        ];
        let html = leaderboard_rows(&rows, true).into_string();
        assert!(html.contains("…0000aaaa") && html.contains("…0000bbbb"));
        assert!(html.contains(r#"data-copy="01a0d0f5-0000-7000-8000-00000000aaaa""#));
        assert!(html.contains(r#"data-owner="owner-alice""#));
        assert!(html.contains("npub1abcd…wxyz"));
        assert!(!html.contains("No scores yet"));
        assert!(leaderboard_rows(&[], true)
            .into_string()
            .contains("No scores yet"));
    }

    #[test]
    fn rows_open_the_picks_without_script_filters() {
        let html = leaderboard_rows(&[row("e1", "bob", 0, 1)], true).into_string();
        assert!(html.contains(r#"hx-get="/entries/e1/detail""#));
        assert!(
            !html.contains("hx-trigger"),
            "filters need eval, which the CSP forbids"
        );
        assert!(html.contains(
            r#"<button type="button" class="button is-small is-text picks-button">Picks</button>"#
        ));
    }

    #[test]
    fn a_live_board_shows_provisional_scores_and_refreshes_until_the_window_closes() {
        let competition = view("c1", Phase::Live, -5);
        let page = leaderboard(&competition, NOW).into_string();
        assert!(page.contains("results in 5 min"));
        assert!(page.contains(r#"hx-trigger="load""#));

        let board = LeaderboardView {
            rows: vec![row("e1", "bob", 30, 1)],
            phase: Phase::Live,
            updated_at: Some(NOW - time::Duration::minutes(12)),
            any_readings: true,
        };
        let live = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(live.contains("<strong>Provisional:</strong> if the window ended now · updated"));
        assert!(live.contains("12 min ago"));
        assert!(live.contains("Score so far"));
        assert!(live.contains(r#"hx-trigger="every 60s""#));
        assert!(live.contains(r#"hx-swap="outerHTML""#));

        // Closed, then scored: no more polling, and "Final" only once scored.
        let closed = view("c1", Phase::AwaitingResult, -30);
        let waiting = LeaderboardView {
            phase: Phase::AwaitingResult,
            ..board.clone()
        };
        let awaiting = leaderboard_scores(&closed, &waiting, NOW).into_string();
        assert!(
            !awaiting.contains("hx-trigger"),
            "polling stops once the window closes"
        );
        assert!(awaiting.contains("<strong>Window closed:</strong> awaiting the oracle"));
        assert!(awaiting.contains("Score so far"));
        let scored = LeaderboardView {
            phase: Phase::Scored,
            ..board
        };
        let done = leaderboard_scores(&closed, &scored, NOW).into_string();
        assert!(!done.contains("hx-trigger"));
        assert!(!done.contains("Provisional") && !done.contains("Window closed"));
        assert!(done.contains("30 pts"));
    }

    /// A competition cancelled mid-window stops polling.
    #[test]
    fn a_cancelled_board_does_not_poll() {
        let competition = view("c1", Phase::Cancelled, -5);
        let board = LeaderboardView {
            rows: vec![row("e1", "bob", 0, 1)],
            phase: Phase::Cancelled,
            updated_at: None,
            any_readings: false,
        };
        assert!(!leaderboard_scores(&competition, &board, NOW)
            .into_string()
            .contains("hx-trigger"));
    }

    #[test]
    fn an_unfilled_competition_explains_its_refunds() {
        let mut competition = view("c1", Phase::Cancelled, -60);
        competition.total_entries = 0;
        let html = leaderboard(&competition, NOW).into_string();
        assert!(html.contains("No entry fees were paid."));
        assert!(html.contains("No entries yet."));
        assert!(
            !html.contains("leaderboard/rows"),
            "nothing to load without entries"
        );
    }

    /// A competition that didn't run lists its entries without ranks or scores, and says why
    /// and where the entry fees stand.
    #[test]
    fn a_competition_that_did_not_run_scores_nothing() {
        let mut competition = view("c1", Phase::Unfilled, -60);
        competition.total_entries = 2;
        competition.refunds = crate::domain::RefundProgress {
            escrowed: 2,
            refunded: 1,
        };
        let page = leaderboard(&competition, NOW).into_string();
        assert!(page.contains("nothing is scored"));
        assert!(page.contains("1 of 2 returned so far"));

        let board = LeaderboardView {
            rows: vec![row("e1", "bob", 30, 1), row("e2", "amy", 30, 1)],
            phase: Phase::Unfilled,
            updated_at: None,
            any_readings: true,
        };
        let scores = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(!scores.contains("30 pts"), "{scores}");
        assert_eq!(scores.matches("not scored").count(), 2);

        competition.refunds.refunded = 2;
        assert!(leaderboard(&competition, NOW)
            .into_string()
            .contains("Every entry fee has been returned."));
        competition.total_entries = 0;
        competition.refunds = Default::default();
        assert!(leaderboard(&competition, NOW)
            .into_string()
            .contains("No entry fees were paid."));
    }

    /// A finished competition with no readings says so, and how the pot went: back to every
    /// entry when the oracle attested the refund outcome, with no ranks or scores.
    #[test]
    fn a_finished_competition_without_readings_explains_its_result() {
        let mut competition = view("c1", Phase::Scored, -60);
        competition.total_entries = 3;
        competition.pot_refunded = true;
        let mut board = LeaderboardView {
            rows: vec![
                row("e1", "amy", 0, 1),
                row("e2", "bob", 0, 1),
                row("e3", "cat", 0, 1),
            ],
            phase: Phase::Scored,
            updated_at: None,
            any_readings: false,
        };
        let html = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(html.contains("No station reported inside the window"));
        assert!(html.contains("the pot went back to all 3 entries in equal shares"));
        assert!(!html.contains("0 pts"), "{html}");
        assert_eq!(html.matches("not scored").count(), 3);
        assert!(phase_badge(&competition)
            .into_string()
            .contains(">Refunded</span>"));

        competition.pot_refunded = false;
        let html = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(html.contains("tie-break decided who was paid"));
        assert!(html.contains("0 pts"));
        assert!(phase_badge(&competition)
            .into_string()
            .contains(">Finished</span>"));

        board.any_readings = true;
        assert!(!leaderboard_scores(&competition, &board, NOW)
            .into_string()
            .contains("No readings"));
    }
}
