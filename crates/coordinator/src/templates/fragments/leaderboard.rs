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
    components::{tip, tip_start},
    format::{self, ordinal, sats, TimeStyle},
    fragments::picks::{detail_url, LIVE_REFRESH},
    pages::competitions::{phase_badge, CompetitionView, PoolLink, Queue, QueueView, Refunds},
};
use uuid::Uuid;

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
    /// The oracle could not verify the window's weather data.
    pub unverified: bool,
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
    let queue = competition.queue.queued();
    // A queue's entries move to its pools, whose pages show their scores.
    let split = queue.is_some_and(|queue| !queue.pools.is_empty());
    html! {
        div id="competitionLeaderboard" class="leaderboard" {
            a class="back-link" href="/competitions" hx-get="/competitions"
              hx-target="#main-content" hx-push-url="true" { "← All competitions" }

            div class="leaderboard-heading" {
                h1 class="title is-4" { "Leaderboard" }
                (phase_badge(competition))
            }
            @if let Queue::Pool(pool) = &competition.queue {
                p class="pool-of" {
                    (pool.label()) " · "
                    a href=(pool.url()) hx-get=(pool.url()) hx-target="#main-content" hx-push-url="true" {
                        "All pools"
                    }
                }
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
                div { dt { "Pot" } dd { (competition.pot()) } }
                @if let Some(queue) = queue {
                    div {
                        dt { "Paid places" (tip(&format!("{}.", queue.pool_note()))) }
                        dd {
                            @if let Some(split) = competition.prize_split() {
                                (split)
                                @if let Some(rule) = competition.prize_rule() {
                                    span class="cell-note prize-note" { (rule) }
                                }
                            } @else {
                                "Each pool's winner"
                            }
                        }
                    }
                } @else if competition.pot_refunded || competition.phase == Phase::Expired {
                    div class="refund-fact" {
                        dt { "Each entry's share" }
                        dd { (refund_allocation(competition)) }
                    }
                } @else if competition.has_ranked_prizes() {
                    div {
                        dt { "Paid places" }
                        dd {
                            @if prizes.len() == 1 {
                                "Winner takes all"
                            } @else {
                                @for (place, (_, amount)) in prizes.iter().enumerate() {
                                    span class="prize" { (ordinal(place + 1)) " " (sats(*amount)) }
                                }
                            }
                        }
                    }
                }
                div { dt { "Entries" } dd { (competition.entries()) } }
            }
            @if let Some(queue) = queue.filter(|_| split) {
                (queue_pools(&competition.id, queue, None))
            }

            @if !split && competition.result_is_late(now) {
                p class="notice" {
                    "Results are late. "
                    @match competition.expiry {
                        Some(expiry) => {
                            "If none arrive by " (format::zoned_time(expiry, TimeStyle::DateTime))
                            ", the pot is shared back among the entries."
                        }
                        None => { "If none arrive, the pot is shared back among the entries." }
                    }
                }
            }

            @if competition.pot_refunded || competition.phase == Phase::Expired {
                p class="notice" {
                    "The competition's terms set these shares; payment may still be pending."
                    @if competition.ticket_price > competition.entry_fee {
                        " The service fee included in the entry fee isn't part of the pot, so it \
                         isn't shared back."
                    }
                }
            }

            @if competition.can_enter {
                a class="button is-primary mb-4"
                  href=(competition.url()) hx-get=(competition.url())
                  hx-target="#main-content" hx-push-url="true" { "Enter this competition" }
            }
            @if competition.did_not_fill() {
                p class="notice" {
                    @if queue.is_some() {
                        "Too few players entered to make a pool, so this competition "
                    } @else {
                        "Not enough entries arrived before the window started, so this competition "
                    }
                    "doesn't run and nothing is scored. "
                    (refund_note(competition, now))
                }
            } @else if !split && !ran(competition.phase) {
                p class="notice" {
                    "This competition did not run, so nothing is scored."
                    @if competition.owes_refunds() {
                        " " (refund_note(competition, now))
                    }
                }
            }

            @if split {
                // Each pool's page has its scores.
            } @else if competition.total_entries == 0 {
                // Fees paid for entries that never arrived are counted in the refund note.
                @if competition.entered() == 0 {
                    p class="empty-state" { "No entries yet." }
                }
            } @else {
                div id="leaderboardScores" class="leaderboard-scores"
                    hx-get=(rows_url(&competition.id)) hx-trigger="load" hx-swap="outerHTML"
                    "hx-status:500"="swap:outerHTML" {
                    (scores_table(
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

            @if let Some(funding) = &competition.funding {
                p class="help contract-funding" {
                    "Contract funding: " (format::chain_id(&funding.outpoint, funding.url.clone()))
                }
            }
        }
    }
}

/// The address a queue's pools load from, with the signed-in player's marked.
pub fn pools_url(competition_id: &str) -> String {
    format!("/competitions/{competition_id}/pools")
}

/// A queue's pools, each a competition with its own leaderboard. The page shows them to everyone
/// (`mine` is `None`), then loads them again from [`pools_url`], signed when the player is logged
/// in, with `mine` naming the pools that hold the player's entries; logging in or out reloads them.
pub fn queue_pools(competition_id: &str, queue: &QueueView, mine: Option<&[Uuid]>) -> Markup {
    let trigger = match mine {
        None => "load, fw:login from:body, fw:logout from:body",
        Some(_) => "fw:login from:body, fw:logout from:body",
    };
    let own = |pool: &PoolLink| {
        mine.is_some_and(|mine| Uuid::parse_str(&pool.id).is_ok_and(|id| mine.contains(&id)))
    };
    html! {
        section id="queuePools" class="queue-pools"
            hx-get=(pools_url(competition_id)) hx-trigger=(trigger) hx-swap="outerHTML" {
            h2 class="title is-6" { "Pools" }
            ul {
                @for (position, pool) in queue.pools.iter().enumerate() {
                    li class=[own(pool).then_some("is-own")] {
                        a href=(pool.url()) hx-get=(pool.url()) hx-target="#main-content" hx-push-url="true" {
                            (pool.label(position))
                        }
                        @if let Some(size) = pool.size {
                            " · " (size) " players"
                        }
                        @if own(pool) {
                            span class="you-badge" { "Your pool" }
                        }
                    }
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
    // The rows are in payout order, so the oracle's result pays the first ones.
    let paid = match board.phase {
        Phase::Scored if competition.has_ranked_prizes() && !competition.pot_refunded => {
            usize::try_from(competition.paid_places).unwrap_or(usize::MAX)
        }
        _ => 0,
    };
    let url = rows_url(&competition.id);
    html! {
        div id="leaderboardScores" class="leaderboard-scores"
            hx-get=[live.then_some(&url)] hx-trigger=[live.then_some(LIVE_REFRESH)]
            hx-swap=[live.then_some("outerHTML")] "hx-status:500"=[live.then_some("swap:outerHTML")] {
            @match board.phase {
                Phase::Live => {
                    p class="provisional-note" {
                        @match board.updated_at {
                            Some(at) => { "Updated " (format::ago(at, now)) }
                            None => { "Scores so far" }
                        }
                        (tip("Scored as if the window ended now; scores can change until the result is final."))
                    }
                }
                Phase::AwaitingResult => {
                    p class="provisional-note" {
                        "Window closed"
                        (tip_start("Scores so far; the oracle's own reading decides the final result."))
                    }
                }
                Phase::Expired if board.unverified => {
                    p class="notice" {
                        "The oracle couldn't verify the weather data for this window, so the pot is "
                        "shared back among the entries."
                    }
                }
                Phase::Expired => {
                    p class="notice" {
                        "The oracle never signed a result in time, so under the competition's terms "
                        "the pot is shared back among the entries."
                    }
                }
                Phase::Scored if competition.pot_refunded => {
                    p class="notice" {
                        strong { "No-score outcome. " }
                        @if !board.any_readings {
                            "No readings were recorded inside the window, so no entry scored any points. "
                        } @else {
                            "No entry scored any points. "
                        }
                        "Each entry receives a share of the pot under this competition's terms."
                        @if competition.refund_shares.as_ref().is_some_and(|shares| {
                            shares.first().is_some_and(|first| shares.iter().any(|amount| amount != first))
                        }) {
                            " This older competition used unequal pot-return shares."
                        }
                    }
                }
                Phase::Scored if !board.any_readings && !board.rows.is_empty() => {
                    p class="notice" {
                        "No readings are available for this window. The oracle's result decides the "
                        "payouts; this page couldn't work out each entry's share."
                    }
                }
                _ => {}
            }
            (scores_table(leaderboard_rows(
                &board.rows,
                ran(board.phase) && competition.has_ranked_prizes(),
                paid,
            )))
        }
    }
}

fn scores_table(rows: Markup) -> Markup {
    html! {
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
                tbody id="leaderboardRows" { (rows) }
            }
        }
    }
}

/// Show the signed allocation, never reconstruct refunds from the planned winner prizes.
fn refund_allocation(competition: &CompetitionView) -> Markup {
    let Some(shares) = competition
        .refund_shares
        .as_ref()
        .filter(|shares| !shares.is_empty())
    else {
        return html! { "Amounts unavailable" };
    };
    let mut amounts = std::collections::BTreeMap::<u64, usize>::new();
    for amount in shares {
        *amounts.entry(*amount).or_default() += 1;
    }
    html! {
        @if amounts.len() == 1 {
            (sats(shares[0])) " per entry"
        } @else {
            @for (amount, count) in amounts.iter().rev() {
                span class="refund-share" {
                    (count) " " (if *count == 1 { "entry" } else { "entries" }) " × " (sats(*amount))
                }
            }
        }
    }
}

/// What happened to the entry fees of a competition that didn't fill. Escrowed fees can't
/// move until their escrows' refund locktime, so until then it says when they will. Fees whose
/// refund an operator wrote off, or whose Lightning payment was settled, are no longer counted
/// as owed, but are said to be, with where to ask about them.
fn refund_note(competition: &CompetitionView, now: OffsetDateTime) -> Markup {
    let progress = competition.refunds;
    let returned = progress.refunded + progress.released;
    let kept = progress.paid().saturating_sub(returned);
    html! {
        @match competition.refunds(now) {
            Refunds::Nothing => { "No entry fees were paid." }
            Refunds::Partly if progress.written_off > 0 => {
                "Refunds are finished: " (returned) " of " (progress.paid()) " paid entry fees \
                 were returned; the operator closed the other " (kept) ", which could not be \
                 refunded automatically. If one of them is yours, contact us."
            }
            Refunds::Partly => {
                "Refunds are finished: " (returned) " of " (progress.paid()) " paid entry fees \
                 were returned; the other " (kept) " could not be refunded automatically. If one \
                 of them is yours, contact us."
            }
            Refunds::Done => { "Every entry fee has been returned." }
            Refunds::Locked(at) => {
                "Entry fees go back to the refund destination shown when entering. Refunds open "
                (format::time(at, TimeStyle::DateTime)) ", when the escrows holding them unlock."
            }
            Refunds::Pending if progress.escrowed > 0 => {
                "Refunding… Entry fees go back to the refund destination shown when entering: "
                (progress.refunded) " of " (progress.escrowed) " paid entry fees returned so far."
            }
            Refunds::Pending if progress.held > 0 && competition.phase != Phase::Unfilled => {
                "Refunding… Held entry fees go back to their payers: " (progress.released) " of "
                (progress.held) " released so far."
            }
            Refunds::Pending => {
                "Entry fees go back to the refund destination shown when entering once it is cancelled."
            }
        }
    }
}

/// The rows of the leaderboard's table. Entries of a competition that didn't run are listed
/// without a rank or score. `paid` is how many of the first rows, in payout order, the final
/// result pays: none until it is in.
pub fn leaderboard_rows(rows: &[LeaderboardRow], scored: bool, paid: usize) -> Markup {
    // Until someone scores, everyone would share first place: no ranks yet.
    let ranked = scored && rows.iter().any(|row| row.score != 0);
    html! {
        @for (position, row) in rows.iter().enumerate() {
            (leaderboard_row(row, scored, ranked, position < paid))
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
fn leaderboard_row(row: &LeaderboardRow, scored: bool, ranked: bool, paid: bool) -> Markup {
    html! {
        tr class="is-clickable" data-owner=(row.owner)
           hx-get=(detail_url(&row.entry_id)) hx-target="#entryValues" hx-swap="innerHTML"
           "hx-status:500"="swap:innerHTML" {
            td data-label="Rank" {
                @if ranked { (row.rank) } @else { "—" }
                @if paid { " " span class="paid-badge" { "Paid" } }
            }
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
    fn a_funded_contract_names_its_funding_output() {
        use crate::templates::pages::competitions::FundingView;
        let mut competition = view("c1", Phase::Live, -5);
        assert!(!leaderboard(&competition, NOW)
            .into_string()
            .contains("Contract funding"));

        let outpoint = format!("{}:1", "f".repeat(64));
        competition.funding = Some(FundingView {
            outpoint: outpoint.clone(),
            url: Some(format!("https://mempool.example/tx/{}", "f".repeat(64))),
        });
        let html = leaderboard(&competition, NOW).into_string();
        assert!(html.contains("Contract funding: "));
        assert!(html.contains(&format!(r#"data-copy="{outpoint}""#)));
        assert!(html.contains(&format!(
            r#"href="https://mempool.example/tx/{}""#,
            "f".repeat(64)
        )));

        // Without an explorer, the outpoint is there to copy.
        competition.funding.as_mut().unwrap().url = None;
        let html = leaderboard(&competition, NOW).into_string();
        assert!(html.contains(&format!(r#"data-copy="{outpoint}""#)));
        assert!(!html.contains("explorer-link"));
    }

    #[test]
    fn the_page_shows_the_pot_and_prizes_and_loads_its_rows() {
        let mut competition = view("c1", Phase::Scored, -60);
        competition.paid_places = 2;
        let html = leaderboard(&competition, NOW).into_string();
        assert!(html.contains("15,000 sats"));
        assert!(html.contains("1st 10,500 sats"));
        assert!(html.contains("2nd 4,500 sats"));
        assert!(!html.contains("(70%)"));
        assert!(html.contains(r#"hx-get="/competitions/c1/leaderboard/rows""#));
        assert!(
            html.contains(r#"hx-trigger="load""#),
            "a finished board loads once"
        );
        assert!(html.contains("Loading scores"));
        // The tie rule and what a row does are on the help page, not under the table.
        assert!(!html.contains("earlier entry is paid") && !html.contains("Select an entry"));
        let one = leaderboard(&view("c1", Phase::Scored, -60), NOW).into_string();
        assert!(one.contains("Winner takes all") && !one.contains("1st"));
    }

    /// Past its signing time without a result, the page says so in one line, with when the
    /// pot goes back if none comes. Until then, and once the result is in, it says nothing.
    #[test]
    fn a_late_result_says_when_the_pot_is_shared_back() {
        // Ended ten minutes after its start; signing was due five minutes after that.
        let mut late = view("c1", Phase::AwaitingResult, -60);
        late.expiry = Some(NOW + time::Duration::hours(23));
        let html = leaderboard(&late, NOW).into_string();
        assert!(
            html.contains("Results are late. If none arrive by <time"),
            "{html}"
        );
        assert!(html.contains(r#"datetime="2026-09-25T11:00:00Z""#));
        assert!(html.contains("the pot is shared back among the entries."));
        for jargon in ["oracle", "attest", "expir", "contract", "refund"] {
            let notice = &html[html.find("Results are late").unwrap()..];
            let notice = &notice[..notice.find("</p>").unwrap()];
            assert!(
                !notice.to_lowercase().contains(jargon),
                "{jargon}: {notice}"
            );
        }

        late.expiry = None;
        let html = leaderboard(&late, NOW).into_string();
        assert!(html.contains("Results are late. If none arrive, the pot is shared back"));

        let due_soon = view("c1", Phase::AwaitingResult, -14);
        assert!(!leaderboard(&due_soon, NOW)
            .into_string()
            .contains("Results are late"));
        let scored = view("c1", Phase::Scored, -60);
        assert!(!leaderboard(&scored, NOW)
            .into_string()
            .contains("Results are late"));
    }

    #[test]
    fn a_one_pool_queue_shows_how_its_prizes_split() {
        let html = leaderboard(
            &crate::templates::pages::competitions::tests::twenty_seats(4),
            NOW,
        )
        .into_string();
        assert!(html.contains("1st 70% · 2nd 30%"), "{html}");
        assert!(html.contains("Under 10 players: winner takes all"));
        assert!(html.contains("20 seats · 16 left"));
        assert!(html.contains(r#"data-tip="Up to 20 players, all in one pool.""#));
    }

    #[test]
    fn a_queue_shows_how_many_entered_and_links_its_pools_once_split() {
        use crate::templates::pages::competitions::{
            tests::{queued, POOL},
            PoolLink,
        };
        let queue = queued("q", 40);
        let waiting = leaderboard(&queue, NOW).into_string();
        assert!(waiting.contains("40 entered"));
        assert!(waiting.contains("100,000 sats per pool"));
        assert!(waiting
            .contains(r#"data-tip="Players are split into pools of up to 25 at the start.""#));
        assert!(
            waiting.contains("Each pool&#39;s winner") || waiting.contains("Each pool's winner")
        );
        assert!(!waiting.contains(" of 3"));
        assert!(waiting.contains("/competitions/q/leaderboard/rows"));

        let mut split = queue;
        split.phase = Phase::Unfilled;
        split.can_enter = false;
        if let Queue::Queued(queue) = &mut split.queue {
            queue.pools = vec![PoolLink {
                id: POOL.into(),
                index: Some(0),
                size: Some(20),
            }];
        }
        let html = leaderboard(&split, NOW).into_string();
        assert!(html.contains(&format!(r#"href="/competitions/{POOL}/leaderboard""#)));
        assert!(html.contains("Pool 1</a> · 20 players"), "{html}");
        // Loaded again, signed when logged in, to mark the player's pool.
        assert!(html.contains(r#"hx-get="/competitions/q/pools""#));
        assert!(html.contains(r#"hx-trigger="load, fw:login from:body, fw:logout from:body""#));
        assert!(!html.contains("Your pool"));
        assert!(
            !html.contains("leaderboard/rows"),
            "the scores are the pools'"
        );
        assert!(!html.contains("did not run") && !html.contains("Too few"));
    }

    #[test]
    fn the_players_pool_is_marked_and_the_marked_list_does_not_reload_itself() {
        use crate::templates::pages::competitions::tests::POOL;
        const OTHER: &str = "01a0c226-0000-7000-8000-000000000002";
        let queue = QueueView {
            min_players: Some(2),
            max_players: 25,
            entries: Some(40),
            max_entries: None,
            held: None,
            pools: [POOL, OTHER]
                .into_iter()
                .enumerate()
                .map(|(index, id)| PoolLink {
                    id: id.into(),
                    index: Some(index as u64),
                    size: Some(20),
                })
                .collect(),
        };
        let mine = [Uuid::parse_str(OTHER).unwrap()];
        let html = queue_pools("q", &queue, Some(&mine)).into_string();
        assert_eq!(html.matches("Your pool").count(), 1);
        assert!(
            html.contains(r#"<li class="is-own"><a href="/competitions/01a0c226-0000-7000-8000-000000000002/leaderboard""#),
            "{html}"
        );
        assert!(html.contains(r#"hx-trigger="fw:login from:body, fw:logout from:body""#));
        assert!(!html.contains("load,"));
        let none = queue_pools("q", &queue, Some(&[])).into_string();
        assert!(!none.contains("Your pool") && !none.contains("is-own"));
    }

    #[test]
    fn a_pool_links_back_to_its_competition() {
        use crate::templates::pages::competitions::PoolOf;
        let mut pool = view("p", Phase::Live, -5);
        pool.queue = Queue::Pool(PoolOf {
            parent_id: "q".into(),
            index: Some(0),
        });
        let html = leaderboard(&pool, NOW).into_string();
        assert!(html.contains("Pool 1 · <a"));
        assert!(html.contains(">All pools</a>"));
        assert!(html.contains(r#"href="/competitions/q/leaderboard""#));
        assert!(html.contains("1 of 3"));
        assert!(html.contains("/competitions/p/leaderboard/rows"));
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
        let html = leaderboard_rows(&rows, true, 0).into_string();
        assert!(html.contains("…0000aaaa") && html.contains("…0000bbbb"));
        assert!(html.contains(r#"data-copy="01a0d0f5-0000-7000-8000-00000000aaaa""#));
        assert!(html.contains(r#"data-owner="owner-alice""#));
        assert!(html.contains("npub1abcd…wxyz"));
        assert!(!html.contains("No scores yet"));
        assert!(leaderboard_rows(&[], true, 0)
            .into_string()
            .contains("No scores yet"));
    }

    #[test]
    fn rows_open_the_picks_without_script_filters() {
        let html = leaderboard_rows(&[row("e1", "bob", 0, 1)], true, 0).into_string();
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
            unverified: false,
        };
        let live = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(live.contains("Updated <time"));
        assert!(live.contains("12 min ago"));
        assert!(!live.contains("Provisional") && !live.contains("Score so far"));
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
        assert!(awaiting.contains("Window closed"));
        let scored = LeaderboardView {
            phase: Phase::Scored,
            ..board
        };
        let done = leaderboard_scores(&closed, &scored, NOW).into_string();
        assert!(!done.contains("hx-trigger"));
        assert!(!done.contains("Updated") && !done.contains("Window closed"));
        assert!(done.contains("30 pts"));
    }

    /// Rows are in payout order: with one paid place and a tie for first, the earlier entry
    /// (listed first) is the one paid, and only the final result says so.
    #[test]
    fn the_final_result_marks_the_paid_rows() {
        let competition = view("c1", Phase::Scored, -60);
        let tied = LeaderboardView {
            rows: vec![
                row("e1", "bob", 30, 1),
                row("e2", "amy", 30, 1),
                row("e3", "cat", 20, 3),
            ],
            phase: Phase::Scored,
            updated_at: None,
            any_readings: true,
            unverified: false,
        };
        let html = leaderboard_scores(&competition, &tied, NOW).into_string();
        assert_eq!(html.matches("paid-badge").count(), 1);
        assert!(
            html.contains(r#"<td data-label="Rank">1 <span class="paid-badge">Paid</span></td>"#)
        );
        let live = LeaderboardView {
            phase: Phase::Live,
            ..tied
        };
        let provisional = leaderboard_scores(&competition, &live, NOW).into_string();
        assert!(!provisional.contains("paid-badge"));
    }

    /// With every score at 0 everyone would share first place, so no rank shows until
    /// someone scores.
    #[test]
    fn no_ranks_show_until_someone_scores() {
        let competition = view("c1", Phase::Live, -5);
        let mut board = LeaderboardView {
            rows: vec![row("e1", "amy", 0, 1), row("e2", "bob", 0, 1)],
            phase: Phase::Live,
            updated_at: None,
            any_readings: false,
            unverified: false,
        };
        let html = leaderboard_scores(&competition, &board, NOW).into_string();
        assert_eq!(html.matches(r#"<td data-label="Rank">—</td>"#).count(), 2);
        assert_eq!(html.matches("0 pts").count(), 2);
        board.rows = vec![row("e2", "bob", 10, 1), row("e1", "amy", 0, 2)];
        let html = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(html.contains(r#"<td data-label="Rank">1</td>"#));
        assert!(html.contains(r#"<td data-label="Rank">2</td>"#));
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
            unverified: false,
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
            written_off: 0,
            opens_at: Some(NOW - time::Duration::minutes(5)),
            ..Default::default()
        };
        let page = leaderboard(&competition, NOW).into_string();
        assert!(page.contains("nothing is scored"));
        assert!(page.contains("Refunding… "));
        assert!(page.contains("1 of 2 paid entry fees returned so far"));

        // Until the escrows unlock, it says when refunds open rather than "0 of 2".
        let mut locked = competition.clone();
        locked.refunds.refunded = 0;
        locked.refunds.opens_at = Some(NOW + time::Duration::hours(15));
        let page = leaderboard(&locked, NOW).into_string();
        assert!(
            page.contains(r#"Refunds open <time datetime="2026-09-25T03:00:00Z""#),
            "{page}"
        );
        assert!(!page.contains("returned so far") && !page.contains("Refunding"));

        let board = LeaderboardView {
            rows: vec![row("e1", "bob", 30, 1), row("e2", "amy", 30, 1)],
            phase: Phase::Unfilled,
            updated_at: None,
            any_readings: true,
            unverified: false,
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

    /// An escrow whose refund an operator wrote off is no longer owed: once the rest are
    /// returned, refunds are finished, and the page says how many were not.
    #[test]
    fn written_off_refunds_leave_the_rest_done() {
        let mut competition = view("c1", Phase::Unfilled, -60);
        competition.total_entries = 0;
        competition.refunds = crate::domain::RefundProgress {
            escrowed: 2,
            refunded: 1,
            written_off: 1,
            opens_at: Some(NOW - time::Duration::minutes(5)),
            ..Default::default()
        };
        let page = leaderboard(&competition, NOW).into_string();
        assert!(
            page.contains("1 of 2 paid entry fees returned so far"),
            "{page}"
        );

        competition.refunds.refunded = 2;
        let page = leaderboard(&competition, NOW).into_string();
        assert!(!page.contains("Refunding"), "{page}");
        assert!(
            page.contains(
                "Refunds are finished: 2 of 3 paid entry fees were returned; the operator \
                 closed the other 1, which could not be refunded automatically. If one of them \
                 is yours, contact us."
            ),
            "{page}"
        );

        // Every paid fee written off: nothing is owed, and it was not a competition nobody paid.
        competition.refunds = crate::domain::RefundProgress {
            escrowed: 0,
            refunded: 0,
            written_off: 1,
            opens_at: None,
            ..Default::default()
        };
        let page = leaderboard(&competition, NOW).into_string();
        assert!(page.contains("Refunds are finished: 0 of 1"), "{page}");
        assert!(!page.contains("No entry fees were paid."));
        // Its entries are the rows; the paid fee keeps "No entries yet" away.
        assert!(page.contains("<dd>0 of 3</dd>"), "{page}");
        assert!(!page.contains("No entries yet."), "{page}");
    }

    /// A competition cancelled with entry fees held by Lightning, not escrowed, says where they
    /// stand: released, being released, or settled so they couldn't be.
    #[test]
    fn a_cancelled_competition_paid_by_lightning_explains_its_refunds() {
        let mut competition = view("c1", Phase::Cancelled, -60);
        competition.total_entries = competition.total_allowed_entries;
        competition.refunds = crate::domain::RefundProgress {
            held: 2,
            released: 1,
            ..Default::default()
        };
        let page = leaderboard(&competition, NOW).into_string();
        assert!(page.contains("Refunding… Held entry fees go back to their payers: 1 of 2"));

        competition.refunds.released = 2;
        let page = leaderboard(&competition, NOW).into_string();
        assert!(
            page.contains("Every entry fee has been returned."),
            "{page}"
        );

        competition.refunds.released = 1;
        competition.refunds.settled = 1;
        let page = leaderboard(&competition, NOW).into_string();
        assert!(
            page.contains(
                "Refunds are finished: 1 of 2 paid entry fees were returned; the other 1 could \
                 not be refunded automatically. If one of them is yours, contact us."
            ),
            "{page}"
        );
    }

    /// A full competition cancelled before it ran, as by a failed kickoff check, still says
    /// where its escrowed entry fees stand; an operator's cancellation with nothing escrowed
    /// does not.
    #[test]
    fn a_cancelled_competition_with_escrows_explains_its_refunds() {
        let mut competition = view("c1", Phase::Cancelled, -60);
        competition.total_entries = competition.total_allowed_entries;
        let page = leaderboard(&competition, NOW).into_string();
        assert!(page.contains("did not run, so nothing is scored."));
        assert!(!page.contains("Refund") && !page.contains("returned"));

        competition.refunds = crate::domain::RefundProgress {
            escrowed: 3,
            refunded: 1,
            written_off: 0,
            opens_at: Some(NOW - time::Duration::minutes(5)),
            ..Default::default()
        };
        let page = leaderboard(&competition, NOW).into_string();
        assert!(page.contains("did not run, so nothing is scored. Refunding… "));
        assert!(page.contains("1 of 3 paid entry fees returned so far"));
        assert!(phase_badge(&competition)
            .into_string()
            .contains(">Cancelled</span>"));
    }

    /// A no-score outcome explains why each entry has an allocation without
    /// claiming that the allocation has already been paid.
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
            unverified: false,
        };
        let html = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(html.contains("No-score outcome."));
        assert!(html.contains("No readings were recorded inside the window"));
        assert!(html.contains("Each entry receives a share of the pot"));
        assert!(!html.contains("equal shares"));
        assert!(!html.contains("network fees"));
        assert!(!html.contains("earlier entry is paid"));
        assert!(!html.contains("0 pts"), "{html}");
        assert_eq!(html.matches("not scored").count(), 3);
        let badge = phase_badge(&competition).into_string();
        assert!(badge.contains(">Finished</span>"));
        assert!(badge.contains("the pot is shared back"));

        board.any_readings = true;
        let with_readings = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(with_readings.contains("No entry scored any points."));
        assert!(!with_readings.contains("No readings"));
        assert!(!with_readings.contains("No station reported"));

        competition.pot_refunded = false;
        board.any_readings = false;
        let html = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(
            html.contains("couldn&#39;t work out each entry&#39;s share")
                || html.contains("couldn't work out each entry's share")
        );
        assert!(html.contains("0 pts"));
        assert!(phase_badge(&competition)
            .into_string()
            .contains(">Finished</span>"));

        board.any_readings = true;
        assert!(!leaderboard_scores(&competition, &board, NOW)
            .into_string()
            .contains("No readings"));
    }

    #[test]
    fn pot_return_pages_show_signed_amounts_and_explain_nonreturnable_ticket_charges() {
        let mut competition = view("c1", Phase::Scored, -60);
        competition.entry_fee = 1_000;
        competition.ticket_price = 1_100;
        competition.pot_refunded = true;
        competition.refund_shares = Some(vec![1_020, 990, 990]);
        let historical = leaderboard(&competition, NOW).into_string();
        assert!(
            historical.contains("Each entry&#39;s share")
                || historical.contains("Each entry's share")
        );
        assert!(historical.contains("1 entry × 1,020 sats"));
        assert!(historical.contains("2 entries × 990 sats"));
        assert!(historical.contains("payment may still be pending"));
        assert!(historical.contains("The service fee included in the entry fee isn"));
        assert!(!historical.contains("1,100 sats"), "{historical}");
        assert!(!historical.contains("coordinator fee") && !historical.contains("signed contract"));
        assert!(!historical.contains("Pot returned"));
        assert!(!historical.contains("Paid places"));
        assert!(!historical.contains("1st"));
        assert!(!historical.contains("earlier entry is paid"));

        let board = LeaderboardView {
            rows: vec![row("e1", "amy", 0, 1)],
            phase: Phase::Scored,
            updated_at: None,
            any_readings: false,
            unverified: false,
        };
        assert!(leaderboard_scores(&competition, &board, NOW)
            .into_string()
            .contains("older competition used unequal pot-return shares"));
        competition.refund_shares = Some(vec![1_000; 3]);
        assert!(leaderboard(&competition, NOW)
            .into_string()
            .contains("1,000 sats per entry"));
        assert!(!leaderboard_scores(&competition, &board, NOW)
            .into_string()
            .contains("unequal pot-return shares"));

        competition.refund_shares = None;
        assert!(leaderboard(&competition, NOW)
            .into_string()
            .contains("Amounts unavailable"));
    }

    #[test]
    fn expiry_shows_contract_return_terms_without_promising_a_completed_refund() {
        let mut competition = view("expired", Phase::Expired, -60);
        competition.entry_fee = 1_000;
        competition.ticket_price = 1_000;
        competition.refund_shares = Some(vec![1_000; 3]);
        let html = leaderboard(&competition, NOW).into_string();
        assert!(html.contains(">Finished</span>"));
        assert!(html.contains("Each entry&#39;s share") || html.contains("Each entry's share"));
        assert!(html.contains("1,000 sats per entry"));
        assert!(html.contains("payment may still be pending"));
        assert!(!html.contains("service fee"));
        assert!(!html.contains("Paid places"));
        assert!(!html.contains("Pot returned"));

        let board = LeaderboardView {
            rows: vec![row("e1", "amy", 0, 1)],
            phase: Phase::Expired,
            updated_at: None,
            any_readings: false,
            unverified: false,
        };
        let scores = leaderboard_scores(&competition, &board, NOW).into_string();
        assert!(scores.contains("never signed a result in time"));
        assert!(!scores.contains("attestation") && !scores.contains("signed expiry terms"));
        assert!(!scores.contains("No-score outcome"));
        assert!(!scores.contains("0 pts"));
        // The oracle couldn't verify the window's data: said plainly, with no detail.
        let unverified = LeaderboardView {
            unverified: true,
            ..board
        };
        let scores = leaderboard_scores(&competition, &unverified, NOW).into_string();
        assert!(
            scores.contains("The oracle couldn&#39;t verify the weather data for this window")
                || scores.contains("The oracle couldn't verify the weather data for this window")
        );
        assert!(scores.contains("so the pot is shared back among the entries."));
        assert!(!scores.contains("never signed a result in time"));
        assert_eq!(scores.matches(r#"class="notice""#).count(), 1);

        // Only an expired competition says so.
        let awaiting = LeaderboardView {
            phase: Phase::AwaitingResult,
            ..unverified
        };
        let scores = leaderboard_scores(&competition, &awaiting, NOW).into_string();
        assert!(!scores.contains("couldn"));
    }
}
