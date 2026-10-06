//! The logged-in player's entries, with their money: what each entry cost, what came back, and
//! the totals over all of them.

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::domain::{
    leaderboard::Phase, EntryPayment, LedgerEntry, LedgerTotals, PayoutState, Refund, RefundKind,
    RefundState, Returned, UnpaidTicket,
};
use crate::templates::{
    components::tip,
    format::{self, sats, thousands, tx_url, Explorers, TimeStyle},
    fragments::{entry_form::edit_picks_url, picks::detail_url},
    pages::competitions::{phase_badge, CompetitionView, Queue},
};

/// Rows shown at first, and added by each "Show older entries".
pub const PAGE_SIZE: usize = 25;

/// An entry with its competition, when the competition could be loaded, and what came back.
pub struct EntryRow<'a> {
    pub entry: &'a LedgerEntry,
    pub competition: Option<&'a CompetitionView>,
    pub returned: &'a Returned,
}

/// An unpaid ticket the player holds, in a competition still taking entries.
pub struct UnpaidRow<'a> {
    pub ticket: &'a UnpaidTicket,
    pub competition: &'a CompetitionView,
}

/// Entries page content (requires auth): the player's unpaid entries, the totals over all
/// `count` entries, and the first page of them. Each row's on-chain references link to
/// `explorers`.
pub fn entries_page(
    rows: &[EntryRow],
    totals: &LedgerTotals,
    count: usize,
    open: Option<&CompetitionView>,
    explorers: Explorers,
    unpaid: &[UnpaidRow],
) -> Markup {
    html! {
        div id="allEntries" class="account-page" {
            h1 class="title is-4" { "Your entries" }
            (unpaid_entries(unpaid, OffsetDateTime::now_utc()))
            @if rows.is_empty() {
                (no_entries(open))
            } @else {
                (ledger_summary(totals))
                div class="table-container" {
                    table id="entriesDataTable" class="table is-fullwidth is-hoverable entries-table" {
                        thead {
                            tr {
                                th { "Competition" }
                                th { "Status" }
                                th { "Entry fee" }
                                th { "Returned" }
                                th {}
                            }
                        }
                        tbody {
                            (older_entries(rows, 0, count, explorers))
                        }
                    }
                }
                p class="help" { "Select an entry to see its picks and how each one scored." }
            }
        }
    }
}

/// Entries started but not paid for: each links to its competition's entry form, whose Pay pays
/// the entry's invoice with the picks made there. Nothing when there are none.
pub fn unpaid_entries(unpaid: &[UnpaidRow], now: OffsetDateTime) -> Markup {
    html! {
        @if !unpaid.is_empty() {
            div id="unpaidEntries" class="notification is-warning unpaid-entries" {
                p { strong { "Unpaid entries" } }
                ul {
                    @for row in unpaid {
                        li {
                            (format::window(row.competition.start, row.competition.end))
                            @if let Some(expires) = row.ticket.invoice_expires_at {
                                " · invoice expires in " (format::duration(expires - now))
                            }
                            " "
                            a href=(row.competition.url()) hx-get=(row.competition.url())
                              hx-target="#main-content" hx-push-url="true" { "Make picks and pay" }
                        }
                    }
                }
            }
        }
    }
}

/// The rows from `from` on, and the button for the ones after them, which it replaces with
/// the next page.
pub fn older_entries(rows: &[EntryRow], from: usize, count: usize, explorers: Explorers) -> Markup {
    let shown = from + rows.len();
    let next = format!("/entries?from={shown}");
    html! {
        @for row in rows {
            (entry_row(row, explorers))
        }
        @if shown < count {
            tr id="olderEntries" {
                td colspan="5" class="has-text-centered" {
                    button type="button" class="button is-small is-light" hx-get=(next)
                      hx-target="#olderEntries" hx-swap="outerHTML" {
                        "Show older entries (" (count - shown) " more)"
                    }
                }
            }
        }
    }
}

/// Over all the player's entries: what they paid, what came back, and the difference. What is
/// still on its way is said apart, so the net isn't read as final.
pub fn ledger_summary(totals: &LedgerTotals) -> Markup {
    let net = totals.net_sats();
    let net_class = match net {
        0 => "ledger-net",
        net if net > 0 => "ledger-net is-up",
        _ => "ledger-net is-down",
    };
    html! {
        div id="ledgerSummary" class="ledger-summary" {
            p class="ledger-line" {
                span { "Paid " strong { (sats(totals.paid_sats)) } " across " (plural(totals.entries as u64, "entry", "entries")) }
                " · "
                span {
                    "Received " strong { (sats(totals.received_sats())) }
                    " (won " (sats(totals.won_sats)) ", refunded " (sats(totals.refunded_sats)) ")"
                }
                " · "
                span { "Net " strong class=(net_class) { (signed_sats(net)) } }
            }
            @if totals.pending_sats() > 0 {
                p class="ledger-pending" {
                    "Not counted yet: "
                    @if totals.pending_payouts > 0 {
                        (sats(totals.pending_payout_sats)) " in "
                        (plural(totals.pending_payouts as u64, "payout", "payouts")) " not settled yet"
                    }
                    @if totals.pending_payouts > 0 && totals.pending_refunds > 0 { " · " }
                    @if totals.pending_refunds > 0 {
                        (sats(totals.pending_refund_sats)) " in "
                        (plural(totals.pending_refunds as u64, "refund", "refunds"))
                        @match totals.locked_until {
                            Some(at) if totals.pending_refunds == 1 => {
                                ", locked until " (format::time(at, TimeStyle::DateTime))
                            }
                            Some(at) => { ", the next opening " (format::time(at, TimeStyle::DateTime)) }
                            None => { " on the way" }
                        }
                    }
                }
            }
        }
    }
}

fn plural(count: u64, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        thousands(count),
        if count == 1 { one } else { many }
    )
}

/// `+1,200 sats`, `−1,200 sats`, `0 sats`.
fn signed_sats(value: i64) -> String {
    let sign = match value {
        0 => "",
        value if value > 0 => "+",
        _ => "\u{2212}",
    };
    format!("{sign}{}", sats(value.unsigned_abs()))
}

/// A click anywhere on the row opens the entry's picks. The copy buttons stop
/// their own click, as do the fee's "?" and the details (see page.js); the Picks
/// button's click reaches the row, and the Leaderboard link consumes its click
/// so the row does not see it.
fn entry_row(row: &EntryRow, explorers: Explorers) -> Markup {
    let picks = detail_url(&row.entry.entry_id);
    let leaderboard = format!("/competitions/{}/leaderboard", row.entry.competition_id);
    html! {
        tr class="is-clickable" hx-get=(picks) hx-target="#entryValues" hx-swap="innerHTML"
           "hx-status:500"="swap:innerHTML" {
            td data-label="Competition" {
                @match row.competition {
                    Some(competition) => {
                        (format::window(competition.start, competition.end))
                        // Which pool of a queued competition the entry plays in.
                        @match &competition.queue {
                            Queue::Pool(pool) => { span class="cell-note" { " · " (pool.label()) } }
                            Queue::Queued(queue) if queue.pools.is_empty() => {
                                span class="cell-note" { " · pools form at the start" }
                            }
                            _ => {}
                        }
                    }
                    None => { (row.entry.start_time) }
                }
            }
            td data-label="Status" {
                @if let Some(competition) = row.competition {
                    (phase_badge(competition))
                }
            }
            td data-label="Entry fee" class="ledger-paid" { (paid(row.entry.payment.as_ref())) }
            td data-label="Returned" class="ledger-returned" { (returned(row)) }
            td class="has-text-right entry-links" {
                (details(row, explorers))
                button type="button" class="button is-small is-text picks-button" { "Picks" }
                @if row.competition.is_some_and(picks_editable) {
                    button type="button" class="button is-small is-text"
                      hx-get=(edit_picks_url(&row.entry.entry_id)) hx-trigger="click consume"
                      hx-target="#entryValues" hx-swap="innerHTML" { "Edit picks" }
                }
                a href=(leaderboard) hx-get=(leaderboard) hx-trigger="click consume"
                  hx-target="#main-content" hx-push-url="true" { "Leaderboard" }
            }
        }
    }
}

/// Whether an entry in `competition` may still change its picks, as far as the page can tell:
/// a queued competition before entries close. The edit screen itself has the final say
/// (`Competition::picks_lock`).
fn picks_editable(competition: &CompetitionView) -> bool {
    competition.phase == Phase::Upcoming
        && matches!(&competition.queue, Queue::Queued(queue) if queue.pools.is_empty())
}

/// The all-in entry fee, its parts in a tip, and when it was paid.
fn paid(payment: Option<&EntryPayment>) -> Markup {
    let Some(payment) = payment else {
        return html! { span class="cell-note" { "Not paid" } };
    };
    let mut parts = vec![format!("{} to the pot", sats(payment.entry_fee_sats))];
    if payment.service_fee_sats > 0 {
        parts.push(format!("{} service fee", sats(payment.service_fee_sats)));
    }
    if payment.network_fee_sats > 0 {
        parts.push(format!("{} network fee", sats(payment.network_fee_sats)));
    }
    html! {
        (sats(payment.total_sats)) (tip(&parts.join(" + ")))
        @if let Some(at) = payment.paid_at {
            span class="cell-note ledger-when" { "paid " (format::time(at, TimeStyle::DateTime)) }
        }
    }
}

/// What came back for the entry, and where it stands.
fn returned(row: &EntryRow) -> Markup {
    match row.returned {
        Returned::InPlay => nothing("in play"),
        Returned::AwaitingResult => nothing("awaiting results"),
        // After an expiry every entry is owed its share of the pot, paid yet or not.
        Returned::NoPayout
            if row
                .competition
                .is_some_and(|competition| competition.phase == Phase::Expired) =>
        {
            nothing("pot share due; see Payouts")
        }
        Returned::NoPayout => nothing("no payout"),
        Returned::NoRefund => nothing("no refund recorded"),
        Returned::RefundWrittenOff => nothing("refund stopped; contact support"),
        Returned::Payout(payout) => {
            // Everyone's share of a pot that went back, rather than a win.
            let shared = row.competition.is_some_and(|competition| {
                competition.pot_refunded || competition.phase == Phase::Expired
            });
            let label = if shared { "Pot share" } else { "Won" };
            html! {
                span class=[matches!(payout.state, PayoutState::Settled(_)).then_some("ledger-settled")] {
                    (label) " " (sats(payout.sats))
                }
                @match payout.state {
                    PayoutState::Settled(at) => { (when("settled", at)) }
                    PayoutState::Pending => { (when("sent, not settled yet", None)) }
                    PayoutState::Failed => { (when("payout failed; see Payouts", None)) }
                }
            }
        }
        Returned::Refund(refund) => refund_cell(refund),
    }
}

fn refund_cell(refund: &Refund) -> Markup {
    html! {
        @match refund.state {
            RefundState::Refunded(at) => {
                span class="ledger-settled" { "Refunded " (sats(refund.sats)) }
                @match refund.kind {
                    RefundKind::Lightning => { (when("payment released", at)) }
                    RefundKind::Escrow => { (when("refunded", at)) }
                }
            }
            RefundState::Refunding => {
                "Refund " (sats(refund.sats))
                @match refund.kind {
                    RefundKind::Lightning => { (when("releasing your payment…", None)) }
                    RefundKind::Escrow => { (when("refunding…", None)) }
                }
            }
            RefundState::Locked(at) => {
                "Refund " (sats(refund.sats))
                (when("locked until", Some(at)))
            }
        }
    }
}

/// The note under an amount: where it stands, and since when.
fn when(label: &str, at: Option<OffsetDateTime>) -> Markup {
    html! {
        span class="cell-note ledger-when" {
            (label)
            @if let Some(at) = at { " " (format::time(at, TimeStyle::DateTime)) }
        }
    }
}

/// Nothing back, and why.
fn nothing(note: &str) -> Markup {
    html! { "—" (when(note, None)) }
}

/// The references support asks for: the entry, its payment, and its payout or refund; and
/// where its money is on chain: the escrow VTXO while the entry fee is held in it, and the
/// transaction that funded the contract.
fn details(row: &EntryRow, explorers: Explorers) -> Markup {
    let refund = match row.returned {
        Returned::Refund(refund) => Some(refund),
        _ => None,
    };
    // The escrow is the entry's own: the page is only ever its owner's.
    let escrow = row
        .entry
        .escrow
        .as_ref()
        .filter(|escrow| !escrow.spent_into_pool);
    let vtxo = escrow.and_then(|escrow| {
        let outpoint = escrow.vtxo.as_deref()?;
        let (txid, _) = outpoint.split_once(':')?;
        Some((outpoint, tx_url(explorers.ark, txid), escrow.sats))
    });
    let ark_refund = escrow
        .and_then(|escrow| escrow.refund.as_ref())
        .and_then(|refund| refund.ark_txid.as_deref());
    let funding = row.entry.funding.as_ref();
    let on_chain = vtxo.is_some() || ark_refund.is_some() || funding.is_some();
    let label = if on_chain {
        "Payment and on-chain details"
    } else {
        "Payment details"
    };
    html! {
        details class="ledger-details" {
            summary title=(label) aria-label=(label) { "?" }
            dl {
                dt { "Entry" } dd { (format::copyable_id(&row.entry.entry_id)) }
                dt { "Payment hash" } dd { (format::copyable_id(&row.entry.payment_hash)) }
                @if let Some(payout) = &row.entry.payout {
                    dt { "Payout" } dd { (format::copyable_id(&payout.id)) }
                }
                @if let Some(id) = refund.and_then(|refund| refund.id.as_ref()) {
                    dt { "Refund" } dd { (format::copyable_id(id)) }
                }
                @if let Some(hash) = refund
                    .filter(|refund| refund.kind == RefundKind::Escrow)
                    .and_then(|refund| refund.payment_hash.as_ref())
                {
                    dt { "Refund payment hash" } dd { (format::copyable_id(hash)) }
                }
            }
            @if on_chain {
                p class="ledger-chain" { "On-chain details" }
                dl {
                    @if let Some((outpoint, url, amount)) = vtxo {
                        dt { "Escrow VTXO" }
                        dd {
                            (format::chain_id(outpoint, url))
                            span class="ledger-amount" { (sats(amount)) }
                        }
                    }
                    @if let Some(txid) = ark_refund {
                        dt { "Refund transaction" }
                        dd { (format::chain_id(txid, tx_url(explorers.ark, txid))) }
                    }
                    @if let Some(funding) = funding {
                        dt { "Contract funding" }
                        dd {
                            (format::chain_id(&funding.outpoint(), tx_url(explorers.chain, &funding.txid)))
                        }
                    }
                }
            }
        }
    }
}

/// Empty entries message, pointing at a competition that takes entries now.
pub fn no_entries(open: Option<&CompetitionView>) -> Markup {
    html! {
        div class="empty-state-box" {
            p { "You haven't entered a competition yet." }
            @if let Some(competition) = open {
                a class="button is-primary mt-3"
                  href=(competition.url()) hx-get=(competition.url())
                  hx-target="#main-content" hx-push-url="true" {
                    "Enter the next competition (starts in "
                    (format::duration(competition.start - OffsetDateTime::now_utc()))
                    ")"
                }
            } @else {
                a href="/competitions" hx-get="/competitions" hx-target="#main-content" hx-push-url="true" {
                    "See upcoming competitions"
                }
            }
        }
    }
}

/// What a signed-out visitor sees at an account page's address. The page is
/// rendered unsigned, so a reload lands here too; a remembered login or a new
/// one loads the page in place, signed (see htmx_auth.js).
pub fn sign_in_required(path: &str, what: &str) -> Markup {
    html! {
        div class="account-page sign-in-required"
            hx-get=(path) hx-trigger="fw:login from:body" hx-target="this" hx-swap="outerHTML" {
            h1 class="title is-4" { "Log in to see " (what) }
            p { "You're signed out. Logging in keeps you logged in on this browser until you log out." }
            div class="buttons mt-4" {
                button type="button" class="button is-primary" data-open-modal="loginModal" { "Log in" }
                button type="button" class="button is-light" data-open-modal="registerModal" { "Sign up" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ArkRefundState, EscrowRefund, LedgerEscrow, LedgerPayout};
    use crate::templates::pages::competitions::tests::{view, NOW};

    const ENTRY: &str = "01a0d0f5-52e1-7141-b4e1-8dbd7169fc2e";

    fn at(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }

    fn entry() -> LedgerEntry {
        LedgerEntry {
            entry_id: ENTRY.into(),
            competition_id: "c1".into(),
            start_time: String::new(),
            payment_hash: "a".repeat(56) + "0badf00d",
            payment: Some(EntryPayment {
                entry_fee_sats: 5_000,
                service_fee_sats: 250,
                network_fee_sats: 50,
                total_sats: 5_300,
                // Sep 1 2026, 10:00 UTC
                paid_at: Some(at(1_788_256_800)),
            }),
            lightning_settled: true,
            lightning_released_at: None,
            escrow: None,
            payout: None,
            funding: None,
        }
    }

    /// The page with one row for `entry` in `competition`.
    fn one_row(entry: &LedgerEntry, competition: &CompetitionView) -> String {
        one_row_with(entry, competition, Explorers::default())
    }

    fn one_row_with(
        entry: &LedgerEntry,
        competition: &CompetitionView,
        explorers: Explorers,
    ) -> String {
        let returned = entry.returned(Some(competition.phase), NOW);
        let mut totals = LedgerTotals::default();
        totals.add(entry, &returned);
        entries_page(
            &[EntryRow {
                entry,
                competition: Some(competition),
                returned: &returned,
            }],
            &totals,
            1,
            None,
            explorers,
            &[],
        )
        .into_string()
    }

    #[test]
    fn entries_link_to_picks_and_the_leaderboard() {
        let html = one_row(&entry(), &view("c1", Phase::Live, -5));
        assert!(html.contains(&format!(r#"hx-get="/entries/{ENTRY}/detail""#)));
        assert!(html.contains(r#"href="/competitions/c1/leaderboard""#));
        assert!(html.contains(r#"hx-trigger="click consume""#));
        assert!(
            !html.contains("closest"),
            "trigger filters need eval, which the CSP forbids"
        );
        assert!(html.contains("…7169fc2e"));
        assert!(html.contains("badge-live"));
    }

    #[test]
    fn an_entry_in_a_pool_says_which() {
        use crate::templates::pages::competitions::{tests::queued, PoolOf};
        let mut pool = view("c1", Phase::Live, -5);
        pool.queue = Queue::Pool(PoolOf {
            parent_id: "q".into(),
            index: Some(2),
        });
        let html = one_row(&entry(), &pool);
        assert!(html.contains(" · Pool 3"));
        assert!(html.contains(r#"href="/competitions/c1/leaderboard""#));

        let html = one_row(&entry(), &queued("c1", 4));
        assert!(html.contains("pools form at the start"));
    }

    #[test]
    fn a_row_shows_the_all_in_entry_fee_and_when_it_was_paid() {
        let html = one_row(&entry(), &view("c1", Phase::Live, -5));
        // One number; its parts only in the tip.
        assert!(html.contains(
            r#"<td data-label="Entry fee" class="ledger-paid">5,300 sats<span class="tip""#
        ));
        assert!(html.contains(
            r#"data-tip="5,000 sats to the pot + 250 sats service fee + 50 sats network fee""#
        ));
        assert!(html.contains(r#"paid <time datetime="2026-09-01T10:00:00Z""#));
        assert!(html.contains("—<span class=\"cell-note ledger-when\">in play</span>"));

        // A ticket priced before network fees has no such part.
        let mut older = entry();
        older.payment.as_mut().unwrap().network_fee_sats = 0;
        older.payment.as_mut().unwrap().total_sats = 5_250;
        let html = one_row(&older, &view("c1", Phase::Live, -5));
        assert!(html.contains(r#"data-tip="5,000 sats to the pot + 250 sats service fee""#));
    }

    /// The Returned cell of `entry` in a competition in `phase`.
    fn returned_cell(entry: &LedgerEntry, phase: Phase) -> String {
        let html = one_row(entry, &view("c1", phase, -5));
        let start = html.find(r#"data-label="Returned""#).unwrap();
        let end = start + html[start..].find("</td>").unwrap();
        html[start..end].to_owned()
    }

    #[test]
    fn each_row_says_what_came_back_and_where_it_stands() {
        assert!(returned_cell(&entry(), Phase::AwaitingResult).contains("awaiting results"));
        assert!(returned_cell(&entry(), Phase::Scored).contains("no payout"));
        // An expired contract owes every entry its share, before any payout is recorded.
        let expired = returned_cell(&entry(), Phase::Expired);
        assert!(expired.contains("pot share due; see Payouts") && !expired.contains("no payout"));
        assert!(returned_cell(&entry(), Phase::Cancelled).contains("no refund recorded"));

        let paid_out = |state| LedgerEntry {
            payout: Some(LedgerPayout {
                id: "payout-1".into(),
                sats: 12_000,
                initiated_at: Some(at(1_788_429_600)),
                state,
            }),
            ..entry()
        };
        let settled = returned_cell(
            &paid_out(PayoutState::Settled(Some(at(1_788_436_800)))),
            Phase::Scored,
        );
        assert!(settled.contains(r#"<span class="ledger-settled">Won 12,000 sats</span>"#));
        assert!(settled.contains(r#"settled <time datetime="2026-09-03T12:00:00Z""#));
        let pending = returned_cell(&paid_out(PayoutState::Pending), Phase::Scored);
        assert!(pending.contains("<span>Won 12,000 sats</span>"));
        assert!(pending.contains("sent, not settled yet"));
        assert!(returned_cell(&paid_out(PayoutState::Failed), Phase::Scored)
            .contains("payout failed; see Payouts"));
        // The expiry's pot, shared back, is no win.
        assert!(
            returned_cell(&paid_out(PayoutState::Pending), Phase::Expired)
                .contains("Pot share 12,000 sats")
        );

        // A hold invoice cancelled, or still held for a competition that didn't run.
        let released = LedgerEntry {
            lightning_settled: false,
            lightning_released_at: Some(at(1_788_341_400)),
            ..entry()
        };
        let cell = returned_cell(&released, Phase::Unfilled);
        assert!(cell.contains(r#"<span class="ledger-settled">Refunded 5,300 sats</span>"#));
        assert!(cell.contains(r#"payment released <time datetime="2026-09-02T09:30:00Z""#));
        let held = LedgerEntry {
            lightning_settled: false,
            ..entry()
        };
        assert!(returned_cell(&held, Phase::Unfilled).contains(
            "Refund 5,300 sats<span class=\"cell-note ledger-when\">releasing your payment…"
        ));

        // An escrow: locked until its locktime, then refunding, then refunded less its fee.
        let opens = NOW + time::Duration::hours(6);
        let mut escrowed = LedgerEntry {
            escrow: Some(LedgerEscrow {
                sats: 5_300,
                opens_at: Some(opens),
                spent_into_pool: false,
                written_off: false,
                refund: None,
                vtxo: None,
            }),
            ..entry()
        };
        let cell = returned_cell(&escrowed, Phase::Cancelled);
        assert!(cell.contains("Refund 5,300 sats"));
        assert!(cell.contains("locked until <time"));
        escrowed.escrow.as_mut().unwrap().refund = Some(EscrowRefund {
            id: "refund-1".into(),
            payment_hash: "b".repeat(56) + "5eedf00d",
            fee_sats: 20,
            state: ArkRefundState::Submitted,
            updated_at: None,
            ark_txid: None,
        });
        assert!(returned_cell(&escrowed, Phase::Cancelled).contains("refunding…"));
        escrowed
            .escrow
            .as_mut()
            .unwrap()
            .refund
            .as_mut()
            .unwrap()
            .state = ArkRefundState::Settled;
        let cell = returned_cell(&escrowed, Phase::Cancelled);
        assert!(cell.contains("Refunded 5,280 sats"));
        let mut written_off = escrowed.clone();
        written_off.escrow.as_mut().unwrap().refund = None;
        written_off.escrow.as_mut().unwrap().written_off = true;
        assert!(returned_cell(&written_off, Phase::Cancelled)
            .contains("refund stopped; contact support"));

        // Its references are behind the row's "?".
        let html = one_row(&escrowed, &view("c1", Phase::Cancelled, -5));
        let details = &html[html.find("<details").unwrap()..html.find("</details>").unwrap()];
        for reference in ["…7169fc2e", "…0badf00d", "…refund-1", "…5eedf00d"] {
            assert!(details.contains(reference), "{reference}");
        }
        assert!(details.contains(r#"data-copy="refund-1""#));
    }

    /// The row's "?" details.
    fn details_of(html: &str) -> &str {
        &html[html.find("<details").unwrap()..html.find("</details>").unwrap()]
    }

    #[test]
    fn the_details_show_where_the_money_is_on_chain() {
        let vtxo = format!("{}:0", "e".repeat(64));
        let mut escrowed = LedgerEntry {
            escrow: Some(LedgerEscrow {
                sats: 5_300,
                opens_at: None,
                spent_into_pool: false,
                written_off: false,
                refund: None,
                vtxo: Some(vtxo.clone()),
            }),
            ..entry()
        };
        let explorers = Explorers {
            chain: "https://mempool.example/",
            ark: "https://ark.example",
        };
        let upcoming = view("c1", Phase::Upcoming, 30);

        // Nothing on chain yet: only the payment references.
        let html = one_row_with(&entry(), &upcoming, explorers);
        assert!(!details_of(&html).contains("On-chain details"));
        assert!(html.contains(r#"title="Payment details""#));

        // In escrow: its VTXO and amount, linked to the Arkade explorer.
        let html = one_row_with(&escrowed, &upcoming, explorers);
        let details = details_of(&html);
        assert!(details.contains("On-chain details"));
        assert!(details.contains("Escrow VTXO"));
        assert!(details.contains(&format!(r#"data-copy="{vtxo}""#)));
        assert!(details.contains(&format!(
            r#"href="https://ark.example/tx/{}""#,
            "e".repeat(64)
        )));
        assert!(details.contains("5,300 sats"));
        assert!(!details.contains("Contract funding"));

        // Without an Arkade explorer, the VTXO is there to copy, with no link.
        let html = one_row_with(&escrowed, &upcoming, Explorers::default());
        let details = details_of(&html);
        assert!(details.contains(&format!(r#"data-copy="{vtxo}""#)));
        assert!(!details.contains("explorer-link"));

        // Funded: the escrow went into the contract, whose funding output links to the chain's
        // explorer.
        escrowed.escrow.as_mut().unwrap().spent_into_pool = true;
        escrowed.funding = Some(crate::domain::ContractFunding {
            txid: "f".repeat(64),
            vout: 2,
        });
        let html = one_row_with(&escrowed, &view("c1", Phase::Live, -5), explorers);
        let details = details_of(&html);
        assert!(!details.contains("Escrow VTXO"));
        assert!(details.contains("Contract funding"));
        assert!(details.contains(&format!(r#"data-copy="{}:2""#, "f".repeat(64))));
        assert!(details.contains(&format!(
            r#"href="https://mempool.example/tx/{}""#,
            "f".repeat(64)
        )));
        assert!(!details.contains("15,900"));

        // Refunded: the Arkade transaction that moved the escrow out.
        let mut refunded = LedgerEntry {
            escrow: Some(LedgerEscrow {
                refund: Some(EscrowRefund {
                    id: "refund-1".into(),
                    payment_hash: "b".repeat(64),
                    fee_sats: 20,
                    state: ArkRefundState::Settled,
                    updated_at: None,
                    ark_txid: Some("a".repeat(64)),
                }),
                ..escrowed.escrow.clone().unwrap()
            }),
            ..entry()
        };
        refunded.escrow.as_mut().unwrap().spent_into_pool = false;
        let html = one_row_with(&refunded, &view("c1", Phase::Cancelled, -5), explorers);
        let details = details_of(&html);
        assert!(details.contains("Refund transaction"));
        assert!(details.contains(&format!(
            r#"href="https://ark.example/tx/{}""#,
            "a".repeat(64)
        )));
    }

    fn summary(totals: LedgerTotals) -> String {
        ledger_summary(&totals).into_string()
    }

    #[test]
    fn the_summary_nets_what_settled_and_calls_out_what_is_pending() {
        let totals = LedgerTotals {
            entries: 5,
            paid_sats: 26_500,
            won_sats: 12_000,
            refunded_sats: 10_580,
            pending_payouts: 1,
            pending_payout_sats: 7_000,
            pending_refunds: 1,
            pending_refund_sats: 5_300,
            locked_until: Some(at(1_788_436_800)),
        };
        let html = summary(totals);
        assert!(html.contains("Paid <strong>26,500 sats</strong> across 5 entries"));
        assert!(html.contains(
            "Received <strong>22,580 sats</strong> (won 12,000 sats, refunded 10,580 sats)"
        ));
        assert!(html.contains(r#"Net <strong class="ledger-net is-down">−3,920 sats</strong>"#));
        assert!(html.contains(
            "Not counted yet: 7,000 sats in 1 payout not settled yet · 5,300 sats in 1 refund, \
             locked until <time"
        ));

        // Up overall, with two refunds on their way and nothing locked.
        let html = summary(LedgerTotals {
            won_sats: 30_000,
            pending_payouts: 0,
            pending_payout_sats: 0,
            pending_refunds: 2,
            pending_refund_sats: 10_600,
            locked_until: None,
            ..totals
        });
        assert!(html.contains(r#"<strong class="ledger-net is-up">+14,080 sats</strong>"#));
        assert!(html.contains("Not counted yet: 10,600 sats in 2 refunds on the way"));
        assert!(!html.contains("payout"));

        // Nothing pending, nothing to call out.
        let html = summary(LedgerTotals {
            entries: 1,
            paid_sats: 5_300,
            ..LedgerTotals::default()
        });
        assert!(html.contains("across 1 entry"));
        assert!(html.contains(r#"<strong class="ledger-net is-down">−5,300 sats</strong>"#));
        assert!(!html.contains("Not counted yet"));
    }

    #[test]
    fn older_entries_load_a_page_at_a_time() {
        let entry = entry();
        let competition = view("c1", Phase::Scored, -60);
        let returned = entry.returned(Some(competition.phase), NOW);
        let rows: Vec<EntryRow> = (0..PAGE_SIZE)
            .map(|_| EntryRow {
                entry: &entry,
                competition: Some(&competition),
                returned: &returned,
            })
            .collect();
        let html = entries_page(
            &rows,
            &LedgerTotals::default(),
            60,
            None,
            Explorers::default(),
            &[],
        )
        .into_string();
        assert!(html.contains(r##"hx-get="/entries?from=25" hx-target="#olderEntries""##));
        assert!(html.contains("Show older entries (35 more)"));

        let more = older_entries(&rows, 25, 60, Explorers::default()).into_string();
        assert_eq!(
            more.matches("<tr class=\"is-clickable\"").count(),
            PAGE_SIZE
        );
        assert!(more.contains(r#"hx-get="/entries?from=50""#));
        let last = older_entries(&rows[..10], 50, 60, Explorers::default()).into_string();
        assert!(
            !last.contains("olderEntries"),
            "the last page has no button"
        );
    }

    #[test]
    fn no_entries_links_to_an_open_competition() {
        let mut open = view("next", Phase::Upcoming, 90);
        open.start = NOW.max(OffsetDateTime::now_utc()) + time::Duration::minutes(90);
        let html = entries_page(
            &[],
            &LedgerTotals::default(),
            0,
            Some(&open),
            Explorers::default(),
            &[],
        )
        .into_string();
        assert!(html.contains(r#"href="/competitions/next/entry-form""#));
        assert!(html.contains("Enter the next competition"));
        assert!(!html.contains("ledgerSummary"));
    }

    #[test]
    fn entries_in_a_queue_taking_entries_can_edit_their_picks() {
        use crate::templates::pages::competitions::tests::queued;
        let edit = format!(
            r##"hx-get="/entries/{ENTRY}/edit" hx-trigger="click consume" hx-target="#entryValues""##
        );
        let open = one_row(&entry(), &queued("c1", 3));
        assert!(open.contains(&edit));
        assert!(open.contains("Edit picks"));
        // Not once entries close, nor in a single competition, whose picks are fixed once entered.
        let mut started = queued("c1", 3);
        started.phase = Phase::Live;
        assert!(!one_row(&entry(), &started).contains("Edit picks"));
        assert!(!one_row(&entry(), &view("c1", Phase::Upcoming, 60)).contains("Edit picks"));
    }

    #[test]
    fn unpaid_entries_link_to_their_entry_form_with_the_invoice_expiry() {
        let competition = view("c1", Phase::Upcoming, 90);
        let ticket = UnpaidTicket {
            ticket_id: uuid::Uuid::from_u128(7),
            competition_id: uuid::Uuid::from_u128(1),
            invoice_expires_at: Some(NOW + time::Duration::minutes(42)),
        };
        let rows = [UnpaidRow {
            ticket: &ticket,
            competition: &competition,
        }];
        let html = unpaid_entries(&rows, NOW).into_string();
        assert!(html.contains("Unpaid entries"));
        assert!(html.contains("invoice expires in 42 min"));
        assert!(html.contains(r#"href="/competitions/c1/entry-form""#));
        assert!(html.contains("Make picks and pay"));
        assert!(unpaid_entries(&[], NOW).into_string().is_empty());

        // Shown even before any entry is paid.
        let page = entries_page(
            &[],
            &LedgerTotals::default(),
            0,
            None,
            Explorers::default(),
            &rows,
        )
        .into_string();
        assert!(page.contains(r#"id="unpaidEntries""#));
        assert!(page.contains("You haven't entered a competition yet."));
    }

    #[test]
    fn signed_out_visitors_are_told_why_and_can_log_in() {
        let html = sign_in_required("/entries", "your entries").into_string();
        assert!(html.contains("You're signed out"));
        assert!(html.contains(r#"data-open-modal="loginModal""#));
        assert!(html.contains(r#"hx-trigger="fw:login from:body""#));
    }
}
