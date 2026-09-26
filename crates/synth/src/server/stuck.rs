//! Money a run holds and cannot move, laid out for debugging: every id copyable, what synth could
//! look up itself, the read-only command for what it could not, and which component should move
//! the money next and why it has not.
//!
//! Synth only reads. Nothing here moves or refunds anything.

use maud::{html, Markup};
use time::OffsetDateTime;

use super::format::{self, copyable};
use super::money::{Links, Run};
use crate::client::competitions::CompetitionResponse;
use crate::trail::{payout_states, EntryTrace, Money, PayoutState, SwapSeen, Trail};

/// The stuck block of a run's page: while its money is stuck, and afterwards as the record of
/// where it was held. None for a run whose money never stuck.
pub fn block(run: &Run, now: OffsetDateTime) -> Option<Markup> {
    let trail = run.trail?;
    let held = trail.held.as_ref()?;
    let still = matches!(trail.money, Money::Stuck { .. }) && held.until.is_none();
    let competition = trail.competition.as_ref();
    Some(html! {
        section.stuck {
            h2 {
                @if still { span class="badge stuck" { "Stuck" } " " } @else { span class="badge was_stuck" { "Was stuck" } " " }
                (format::sats(held.sats)) " sats held"
            }
            p {
                (held.reason)
                br;
                "Held since " (format::time(held.since, now))
                @if let (Some(until), Some(then)) = (held.until, &held.then) {
                    " until " (format::time(until, now)) ", " (format::span_between(held.since, until))
                    ". Then " (then) "."
                } @else {
                    ", " (format::span_between(held.since, now)) " so far."
                    @if let Some(at) = held.nearest_expiry.and_then(unix) {
                        " Nearest expiry or refund opening: " (format::time(at, now)) "."
                    }
                    " Synth only watches it; it never moves or refunds money."
                }
            }
            @if let Some(competition) = competition {
                (competition_facts(competition, run.links, now))
            }
            @if competition.is_some_and(crate::trail::contracted) {
                (contract_facts(trail, run.links))
            } @else {
                @for entry in trail.paid_entries(run.entries) {
                    @if !trail.refund_of(entry).is_some_and(|refund| refund.is_settled()) {
                        (escrow_facts(trail, entry, run.links, now))
                    }
                }
            }
        }
    })
}

fn unix(at: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(at).ok()
}

/// A labelled value, with its copy button and a note.
fn fact(label: &str, value: Option<&str>, note: Option<Markup>) -> Markup {
    html! {
        tr {
            th { (label) }
            td {
                @match value {
                    Some(value) => (copyable(value)),
                    None => span.note { "unknown" },
                }
                @if let Some(note) = note { br; span.note { (note) } }
            }
        }
    }
}

/// A read-only command for what synth cannot look up itself, and where to run it.
fn command(label: &str, on: &str, command: &str) -> Markup {
    html! {
        tr {
            th { (label) }
            td { span.note { "run on " (on) ":" } br; (copyable(command)) }
        }
    }
}

fn competition_facts(
    competition: &CompetitionResponse,
    links: &Links,
    now: OffsetDateTime,
) -> Markup {
    let id = competition.id.to_string();
    let state = competition
        .state
        .clone()
        .unwrap_or_else(|| competition.inferred_status().to_string());
    html! {
        h3 { "Competition" }
        div.scroll { table.facts {
            (fact("Competition", Some(&id), Some(html! {
                a href=(format!("{}/api/v1/competitions/{id}", links.coordinator())) rel="noreferrer" { "as the coordinator reports it" }
            })))
            (fact("State", Some(&state), None))
            @if let Some(at) = competition.failed_at {
                tr { th { "Failed" } td { (format::time(at, now)) } }
            }
            @if let Some(at) = competition.cancelled_at {
                tr { th { "Cancelled" } td { (format::time(at, now)) } }
            }
            @if !competition.errors.is_empty() {
                tr { th { "Errors" } td.error { (serde_json::to_string(&competition.errors).unwrap_or_default()) } }
            }
        } }
    }
}

/// One held escrow: from the player's payment to the output holding it, and what should move it.
fn escrow_facts(trail: &Trail, entry: &EntryTrace, links: &Links, now: OffsetDateTime) -> Markup {
    let swap = trail.swap_of(entry);
    let refund = trail.refund_of(entry);
    let ticket = entry.ticket_id.map(|id| id.to_string());
    let (component, why) = next_for_escrow(trail, entry, swap, now);
    html! {
        h3 { (entry.user) "'s escrow" }
        div.scroll { table.facts {
            (fact("Ticket", ticket.as_deref(), None))
            (fact("Swap", swap.map(|swap| swap.id.to_string()).as_deref(),
                swap.map(|swap| html! { "state " strong { (swap.state) } @if let Some(error) = &swap.error { ": " (error) } })))
            (fact("Payment hash", entry.payment_hash.as_deref(), Some(match swap.and_then(|swap| swap.invoice_state.as_deref()) {
                Some("SETTLED") => html! { "invoice " strong { "SETTLED" } ": the player was charged" },
                Some(state) => html! { "invoice " strong { (state) } },
                None => html! { "invoice state not looked up: synth does not reach ark-swapd's node" },
            })))
            @if swap.and_then(|swap| swap.invoice_state.as_deref()).is_none() {
                @if let Some(hash) = &entry.payment_hash {
                    (command("Invoice state", "ark-swapd's LND node", &format!("lncli lookupinvoice {hash}")))
                }
            }
            (fact("Arkade transaction", swap.and_then(|swap| swap.ark_txid.as_deref()), None))
            (fact("Escrow address", swap.map(|swap| swap.escrow_address.as_str()), None))
            (fact("Escrow output", swap.and_then(|swap| swap.escrow_vtxo.as_deref()),
                swap.map(|swap| html! { (format::sats(swap.amount_sat)) " sats"
                    @if swap.funded_without_vtxo() { ", but ark-swapd records no output" } })))
            @match swap.and_then(|swap| Some((swap.escrow_vtxo.as_deref()?, swap.vtxo.as_ref()))) {
                Some((_, Some(vtxo))) => tr {
                    th { "Output status" }
                    td {
                        @if vtxo.swept { strong { "swept" } " by the Arkade server" }
                        @else if vtxo.spent { strong { "spent" } @if let Some(by) = &vtxo.spent_by { " by " (copyable(by)) } }
                        @else { strong { "unspent" } }
                        @if let Some(at) = vtxo.expires_at.and_then(unix) { br; span.note { "expires " (format::time(at, now)) } }
                    }
                },
                Some((outpoint, None)) => (command("Output status and expiry", "any machine that reaches the Arkade server", &links.vtxo_lookup(outpoint))),
                None => {},
            }
            @match entry.escrow {
                Some(terms) => tr {
                    th { "Refund leaf" }
                    td {
                        @match unix(terms.refund_at) {
                            Some(at) => { "opens " (format::time(at, now)) }
                            None => { "opens at " (terms.refund_at) }
                        }
                        br;
                        span.note {
                            "From then the player can take it back with the Arkade server's signature, which is how the coordinator refunds it. "
                            "Alone, the player can after unrolling it on-chain and waiting "
                            (format::duration_ms(i64::from(terms.solo_delay_secs) * 1000)) "."
                        }
                    }
                },
                None => tr { th { "Refund leaf" } td.note { "not known: this entry was recorded before synth kept its escrow terms" } },
            }
            @if let Some(refund) = refund {
                (fact("Refund", Some(&refund.state), refund.ark_txid.as_ref().map(|txid| html! { "Arkade transaction " (copyable(txid)) })))
            }
            @if let Some(ticket) = &ticket {
                (command("The coordinator's records", "the coordinator's host, read-only",
                    &format!("sqlite3 -readonly \"$COORDINATOR_DB\" \"SELECT t.paid_at, t.settled_at, t.escrow_transaction, e.vtxo_outpoint, e.funded_at, r.state, r.error FROM tickets t LEFT JOIN ticket_ark_escrows e ON e.ticket_id = t.id LEFT JOIN ticket_ark_refunds r ON r.ticket_id = t.id WHERE t.id = '{ticket}'\"")))
            }
            tr.next { th { "Moves it next" } td { strong { (component) } br; (why) } }
        } }
    }
}

/// Which component should move a held escrow next, and why it has not, from what synth saw.
fn next_for_escrow(
    trail: &Trail,
    entry: &EntryTrace,
    swap: Option<&SwapSeen>,
    now: OffsetDateTime,
) -> (&'static str, String) {
    let Some(swap) = swap else {
        return (
            "ark-swapd",
            "it reports no swap for this payment, so no escrow was funded from it".into(),
        );
    };
    let charged = swap.invoice_state.as_deref() == Some("SETTLED");
    match swap.state.as_str() {
        "awaiting_payment" | "paying_escrow" => {
            return (
                "ark-swapd",
                format!(
                    "its swap is still {}: it has not paid the escrow",
                    swap.state
                ),
            )
        }
        "failed" | "expired" | "unsettled" => {
            return (
                "ark-swapd",
                format!(
                    "its swap ended {}{}{}",
                    swap.state,
                    swap.error
                        .as_deref()
                        .map(|error| format!(" ({error})"))
                        .unwrap_or_default(),
                    if charged {
                        ", yet the entry invoice settled: it holds the player's payment"
                    } else {
                        ""
                    }
                ),
            )
        }
        _ => {}
    }
    if swap.funded_without_vtxo() {
        return (
            "ark-swapd",
            format!(
                "it says the swap is {}, but records no escrow output, so nothing knows which \
                 output to spend or refund. Find the output with the Arkade indexer by the \
                 escrow address.",
                swap.state
            ),
        );
    }
    if let Some(vtxo) = &swap.vtxo {
        if vtxo.swept {
            return (
                "the Arkade server",
                "it swept the escrow output after the output expired; the escrow no longer \
                 holds the money"
                    .into(),
            );
        }
        if vtxo.spent {
            return (
                "nobody",
                format!(
                    "the escrow output was spent{}; follow that transaction",
                    vtxo.spent_by
                        .as_deref()
                        .map(|by| format!(" by {by}"))
                        .unwrap_or_default()
                ),
            );
        }
    }
    if let Some(refund) = trail.refund_of(entry) {
        return (
            "the coordinator",
            format!(
                "its refund of this escrow is {}; it has not settled",
                refund.state
            ),
        );
    }
    match entry.escrow.and_then(|terms| unix(terms.refund_at)) {
        Some(opens) if opens > now => (
            "the coordinator",
            format!(
                "it refunds the escrow once the refund leaf opens, {}",
                format::when(opens, now)
            ),
        ),
        _ => (
            "the coordinator",
            "its cleanup should refund a dead competition's escrows, but it reports no refund \
             for this ticket. On master, cleanup only picks competitions with paid tickets that \
             are not settled (or with an on-chain escrow transaction), and an Arkade ticket is \
             settled once ark-swapd settles its invoice, so this competition is never picked. \
             The coordinator's records above show the ticket's settled_at."
                .into(),
        ),
    }
}

/// The contract holding the pot, the transactions that moved it, and the winners it owes.
fn contract_facts(trail: &Trail, links: &Links) -> Markup {
    let states = payout_states(&trail.payouts, trail.ended());
    let never_sent = states
        .iter()
        .filter(|state| **state == PayoutState::NeverSent || **state == PayoutState::Owed)
        .count();
    let other_node = states
        .iter()
        .filter(|state| **state == PayoutState::OtherNode)
        .count();
    let decided = trail
        .settlement
        .as_ref()
        .is_some_and(|settlement| settlement.decided.is_some());
    let funding = trail.funding_tx.as_ref().map(|funding| match funding.vout {
        Some(vout) => format!("{}:{vout}", funding.txid),
        None => funding.txid.clone(),
    });
    let (component, why) = if other_node > 0 {
        (
            "the coordinator",
            format!("it paid {other_node} invoice(s) to a node other than the player's"),
        )
    } else if !decided {
        (
            "the oracle, then the coordinator",
            "no outcome decided the payouts before the competition ended; the contract's expiry \
             path is what returns the pot"
                .to_string(),
        )
    } else if never_sent > 0 {
        (
            "the coordinator",
            format!(
                "it pays each winner over Lightning before the payout window closes, and sent \
                 {never_sent} of them nothing{}",
                if trail.closing_txs.is_empty() {
                    String::new()
                } else {
                    "; then the delta or expiry transaction below moved the pot on-chain, to \
                     whoever its outputs pay"
                        .to_string()
                }
            ),
        )
    } else {
        (
            "the coordinator",
            "its payouts were sent, but synth could not confirm them".to_string(),
        )
    };
    html! {
        h3 { "The contract" }
        div.scroll { table.facts {
            (fact("Funding output", funding.as_deref(), trail.funding_tx.as_ref().map(|funding| html! {
                a href=(links.tx(&funding.txid)) rel="noreferrer" { "on the explorer" }
            })))
            (fact("Outcome", trail.outcome_tx.as_ref().map(|tx| tx.txid.as_str()), trail.outcome_tx.as_ref().map(|tx| html! {
                a href=(links.tx(&tx.txid)) rel="noreferrer" { "on the explorer" }
            })))
            @for closing in &trail.closing_txs {
                (fact("Delta or expiry", Some(&closing.txid), Some(html! {
                    @if let Some(value) = closing.value_sat { (format::sats(value)) " sats out · " }
                    a href=(links.tx(&closing.txid)) rel="noreferrer" { "on the explorer" }
                })))
            }
            @for (payout, state) in trail.payouts.iter().zip(&states).filter(|(_, state)| state.is_owed()) {
                tr {
                    th { (payout.user) " is owed" }
                    td {
                        (format::sats(payout.owed_sats)) " sats"
                        br;
                        span.note {
                            (match state {
                                PayoutState::Paid => "paid",
                                PayoutState::SentUnconfirmed => "sent, not confirmed",
                                PayoutState::OtherNode => "paid to another node",
                                PayoutState::Owed | PayoutState::NeverSent => "never sent",
                                PayoutState::OwedNothing => "owed nothing",
                            })
                            @if let Some(hash) = &payout.payment_hash { " · payment hash " (copyable(hash)) }
                        }
                    }
                }
            }
            tr.next { th { "Moves it next" } td { strong { (component) } br; (why) } }
        } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trail::{EscrowTerms, Held, VtxoSeen};
    use uuid::Uuid;

    fn cancelled_trail(swap: SwapSeen, entry: &EntryTrace) -> Trail {
        let competition: CompetitionResponse = serde_json::from_value(serde_json::json!({
            "id": Uuid::now_v7(),
            "created_at": "2026-09-24T01:08:35Z",
            "event_submission": {},
            "cancelled_at": "2026-09-24T03:00:00Z",
        }))
        .unwrap();
        let since = time::macros::datetime!(2026-09-24 03:00:00 UTC);
        let _ = entry;
        Trail {
            refreshed_at: OffsetDateTime::now_utc(),
            competition_id: competition.id,
            competition: Some(competition),
            settlement: None,
            swaps: vec![swap],
            late_payments: Vec::new(),
            payouts: Vec::new(),
            refunds: Vec::new(),
            funding_tx: None,
            outcome_tx: None,
            closing_txs: Vec::new(),
            money: Money::Stuck {
                reason: "the competition was cancelled".into(),
                since,
            },
            held: Some(Held {
                since,
                found: since,
                reason: "the competition was cancelled".into(),
                sats: 1090,
                nearest_expiry: None,
                until: None,
                then: None,
            }),
            gaps: Vec::new(),
        }
    }

    fn entry() -> EntryTrace {
        EntryTrace {
            user: "alice".into(),
            ticket_id: Some(Uuid::now_v7()),
            payment_hash: Some("ab".repeat(32)),
            amount_sats: Some(1100),
            escrow: Some(EscrowTerms {
                refund_at: 1_790_000_000,
                solo_delay_secs: 86_528,
            }),
            paid: true,
            ..EntryTrace::default()
        }
    }

    fn swap(state: &str, vtxo: Option<&str>) -> SwapSeen {
        SwapSeen {
            user: "alice".into(),
            payment_hash: "ab".repeat(32),
            id: Uuid::now_v7(),
            state: state.into(),
            amount_sat: 1090,
            escrow_address: "tark1escrow".into(),
            escrow_vtxo: vtxo.map(str::to_string),
            ark_txid: Some("cd".repeat(32)),
            error: None,
            invoice_state: Some("SETTLED".into()),
            vtxo: None,
        }
    }

    fn links() -> Links {
        Links {
            explorer: "https://mutinynet.com".into(),
            oracle: "https://4casttruth.win".into(),
            coordinator: "https://5day4cast.com".into(),
            ark_swap: None,
            arkd: None,
        }
    }

    fn render(trail: &Trail, entries: &[EntryTrace]) -> String {
        block(
            &Run {
                competition_id: Some(trail.competition_id),
                entries,
                scenario_refunds: &[],
                trail: Some(trail),
                links: &links(),
                paid_by_node: Some(true),
            },
            time::macros::datetime!(2026-09-25 00:00:00 UTC),
        )
        .unwrap()
        .into_string()
    }

    /// Swap 01a0cc69 reported funding without an escrow output: ark-swapd is who must find it.
    #[test]
    fn a_swap_funded_without_an_output_points_at_ark_swapd() {
        let entry = entry();
        let trail = cancelled_trail(swap("settled", None), &entry);
        let (component, why) = next_for_escrow(
            &trail,
            &entry,
            trail.swaps.first(),
            OffsetDateTime::now_utc(),
        );
        assert_eq!(component, "ark-swapd");
        assert!(why.contains("records no escrow output"), "{why}");
        let page = render(&trail, std::slice::from_ref(&entry));
        assert!(page.contains("the player was charged"));
        assert!(page.contains("1,090 sats held"));
    }

    /// A cancelled competition's funded escrow, its refund leaf open, and no refund: coordinator
    /// cleanup never picked it. The output's state comes from a command when synth has no indexer.
    #[test]
    fn a_funded_escrow_nobody_refunded_points_at_coordinator_cleanup() {
        let entry = entry();
        let trail = cancelled_trail(swap("settled", Some("ee:0")), &entry);
        let (component, why) = next_for_escrow(
            &trail,
            &entry,
            trail.swaps.first(),
            OffsetDateTime::now_utc(),
        );
        assert_eq!(component, "the coordinator");
        assert!(why.contains("never picked"), "{why}");
        let page = render(&trail, std::slice::from_ref(&entry));
        assert!(page.contains("$ARKD_URL/v1/indexer/vtxos?outpoints=ee:0"));
        assert!(page.contains("sqlite3 -readonly"));
        assert!(page.contains("Refund leaf"));

        let before = time::macros::datetime!(2026-09-01 00:00:00 UTC);
        let (_, why) = next_for_escrow(&trail, &entry, trail.swaps.first(), before);
        assert!(why.contains("once the refund leaf opens"), "{why}");
    }

    #[test]
    fn a_spent_or_swept_output_says_the_money_moved() {
        let entry = entry();
        let mut swept = swap("settled", Some("ee:0"));
        swept.vtxo = Some(VtxoSeen {
            amount_sat: 1090,
            swept: true,
            ..VtxoSeen::default()
        });
        let trail = cancelled_trail(swept, &entry);
        let (component, _) = next_for_escrow(
            &trail,
            &entry,
            trail.swaps.first(),
            OffsetDateTime::now_utc(),
        );
        assert_eq!(component, "the Arkade server");
    }

    /// Once the money moves, the block stays as the record of where it was held.
    #[test]
    fn money_that_moved_keeps_its_record() {
        let entry = entry();
        let mut trail = cancelled_trail(swap("settled", Some("ee:0")), &entry);
        trail.money = Money::Refunded;
        let held = trail.held.as_mut().unwrap();
        held.until = Some(time::macros::datetime!(2026-09-24 09:00:00 UTC));
        held.then = Some("refunded by Arkade transaction ff".into());
        let page = render(&trail, std::slice::from_ref(&entry));
        assert!(page.contains("Was stuck"));
        assert!(page.contains("6 h"), "{page}");
        assert!(page.contains("Then refunded by Arkade transaction ff."));
    }
}
