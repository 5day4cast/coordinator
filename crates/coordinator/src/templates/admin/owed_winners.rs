//! Winners owed because their Lightning payout window closed unpaid, on the operator pages:
//! a notice on the operations page and, on a competition's page, each winner with the actions
//! that approve the sweep of their output and record paying them. See
//! `docs/ops/owed-winners.md`.

use maud::{html, Markup};

use crate::domain::OwedWinner;
use crate::templates::format::sats;

/// The operations page's notice while any winner is owed: how many, how much, and where.
pub fn owed_winners_notice(owed: &[OwedWinner]) -> Markup {
    let still_owed: Vec<&OwedWinner> = owed.iter().filter(|winner| winner.is_owed()).collect();
    let total: u64 = still_owed.iter().map(|winner| winner.amount_sats).sum();
    let mut competitions: Vec<_> = still_owed
        .iter()
        .map(|winner| winner.competition_id)
        .collect();
    competitions.sort();
    competitions.dedup();
    html! {
        @if !still_owed.is_empty() {
            div.notice role="alert" id="owed-winners-notice" {
                strong { "Winners owed: " (still_owed.len()) ", " (sats(total)) }
                p { "Their Lightning payout window closed unpaid. Pay each one, then record it on the competition's page; approve a sweep of a winner's output to take it back." }
                ul {
                    @for competition in &competitions {
                        li { a href=(format!("/admin/operations/{competition}#owed-winners")) { code { (competition) } } }
                    }
                }
            }
        }
    }
}

/// A competition's owed winners, each with what can be done about them.
pub fn owed_winners_section(owed: &[OwedWinner]) -> Markup {
    html! {
        @if !owed.is_empty() {
            section id="owed-winners" {
                h2 { "Winners owed" }
                p { "Their Lightning payout window closed with them unpaid. The coordinator sweeps a winner's output to its own key only once you approve, and the winner stays owed until you record paying them, which approves the sweep too. Sweep before paying so the winner cannot also claim on chain." }
                div.scroll { table.ops-table {
                    thead { tr { th { "Entry" } th { "Owed" } th { "Since (UTC)" } th { "Output" } th { "Payment" } th { "Actions" } } }
                    tbody { @for winner in owed { (owed_winner_row(winner)) } }
                } }
            }
        }
    }
}

fn owed_winner_row(winner: &OwedWinner) -> Markup {
    let entry = winner.entry_id;
    let result = format!("owed-{entry}-result");
    html! {
        tr id=(format!("owed-{entry}")) {
            td { code { (entry) } }
            td { (sats(winner.amount_sats)) }
            td { (winner.owed_since) }
            td { (winner.output_status()) }
            td {
                @if let Some(at) = &winner.settled_at {
                    "Paid, recorded " (at)
                    @if let Some(note) = &winner.settled_note { p.note { (note) } }
                } @else if winner.claimed_on_chain_at.is_some() {
                    "Claimed on chain"
                } @else {
                    strong.attention { "Still owed" }
                }
            }
            td {
                @if winner.claimed_on_chain_at.is_none() && winner.sweep_approved_at.is_none() && winner.swept_at.is_none() {
                    form hx-post="/admin/api/owed-winners/approve-sweep" hx-target=(format!("#{result}")) hx-swap="innerHTML" hx-confirm="Sweep this winner's output to the coordinator's key once its delay has passed? The winner stays owed until you record paying them." {
                        input type="hidden" name="entry_id" value=(entry);
                        button type="submit" { "Approve sweep" }
                    }
                }
                @if winner.is_owed() {
                    form hx-post="/admin/api/owed-winners/settle" hx-target=(format!("#{result}")) hx-swap="innerHTML" hx-confirm="Record that this winner was paid? This also approves sweeping their output." {
                        input type="hidden" name="entry_id" value=(entry);
                        label { "How they were paid" input name="note" required maxlength="500" placeholder="Payment hash or note"; }
                        button type="submit" { "Record paid" }
                    }
                }
                div id=(result) role="status" {}
            }
        }
    }
}

/// What an action on an owed winner did.
pub fn owed_winner_result(message: &str, winner: &OwedWinner) -> Markup {
    html! {
        div class="notification is-success" {
            (message) " " (winner.output_status()) "."
        }
    }
}

/// Why an action on an owed winner failed.
pub fn owed_winner_error(message: &str) -> Markup {
    html! {
        div class="notification is-danger" { (message) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn owed() -> OwedWinner {
        OwedWinner {
            entry_id: Uuid::now_v7(),
            competition_id: Uuid::now_v7(),
            amount_sats: 12_345,
            owed_since: "2026-10-06T23:00:00Z".into(),
            sweepable_at_height: Some(1_200),
            sweep_approved_at: None,
            swept_at: None,
            claimed_on_chain_at: None,
            claim_txid: None,
            settled_at: None,
            settled_note: None,
        }
    }

    /// A held winner offers both actions; one swept and paid offers none, and the notice counts
    /// only those still owed.
    #[test]
    fn owed_winners_show_their_state_and_the_actions_left() {
        let held = owed();
        let html = owed_winners_section(std::slice::from_ref(&held)).into_string();
        assert!(html.contains("12,345 sats"));
        assert!(html.contains("Held: sweeping it needs an operator"));
        assert!(html.contains("from block 1200"));
        assert!(html.contains(r#"hx-post="/admin/api/owed-winners/approve-sweep""#));
        assert!(html.contains(r#"hx-post="/admin/api/owed-winners/settle""#));
        assert!(html.contains(&format!(r#"value="{}""#, held.entry_id)));
        assert!(html.contains("Still owed"));
        assert!(!html.contains("onclick"));

        let mut paid = owed();
        paid.sweep_approved_at = Some("2026-10-07T01:00:00Z".into());
        paid.swept_at = Some("2026-10-07T02:00:00Z".into());
        paid.settled_at = Some("2026-10-07T03:00:00Z".into());
        paid.settled_note = Some("paid hash abc".into());
        let html = owed_winners_section(std::slice::from_ref(&paid)).into_string();
        assert!(html.contains("Swept to the coordinator"));
        assert!(html.contains("paid hash abc"));
        assert!(!html.contains("owed-winners/approve-sweep"));
        assert!(!html.contains("owed-winners/settle"));

        let notice = owed_winners_notice(&[held.clone(), paid]).into_string();
        assert!(notice.contains("Winners owed: 1, 12,345 sats"));
        assert!(notice.contains(&format!("/admin/operations/{}", held.competition_id)));
        assert!(owed_winners_notice(&[]).into_string().is_empty());
        assert!(owed_winners_section(&[]).into_string().is_empty());
    }
}
