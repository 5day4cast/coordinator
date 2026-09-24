//! A run's money trail: every hop its money took, with the full id to look each one up by, and a
//! ledger checking that what went in came out.

use serde::Serialize;
use uuid::Uuid;

use super::format::msat_as_sats;
use crate::trail::{EntryTrace, Money, Trail};

/// Where each kind of id can be looked up.
#[derive(Debug, Clone)]
pub struct Links {
    /// The mempool.space-style explorer, such as `https://mutinynet.com`.
    pub explorer: String,
    /// The oracle's public site.
    pub oracle: String,
    /// The coordinator's public site.
    pub coordinator: String,
    /// ark-swapd's API, if synth reaches it.
    pub ark_swap: Option<String>,
}

impl Links {
    fn tx(&self, txid: &str) -> String {
        format!("{}/tx/{txid}", self.explorer.trim_end_matches('/'))
    }
}

/// A refund the refund scenario saw settle, from its step.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ScenarioRefund {
    pub paid_sats: u64,
    pub ark_txid: Option<String>,
}

/// Where a hop got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Done,
    Waiting,
    Failed,
}

impl Status {
    pub fn class(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Waiting => "waiting",
            Status::Failed => "failed",
        }
    }
}

/// One hop of the money, or one id worth having.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub step: String,
    pub status: Status,
    pub from: String,
    pub to: String,
    pub amount_sats: Option<u64>,
    /// In sats, to the millisat; None when synth cannot see it.
    pub fee_sats: Option<String>,
    /// The full id to look it up by.
    pub id: String,
    /// What the payee revealed on being paid, for a Lightning payment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preimage: Option<String>,
    /// Where to look it up: a command, or a page.
    pub lookup: String,
    /// The page, when the lookup is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl Row {
    fn new(step: impl Into<String>, status: Status, id: impl Into<String>) -> Self {
        Self {
            step: step.into(),
            status,
            from: String::new(),
            to: String::new(),
            amount_sats: None,
            fee_sats: None,
            id: id.into(),
            preimage: None,
            lookup: String::new(),
            link: None,
        }
    }

    fn between(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.from = from.into();
        self.to = to.into();
        self
    }

    fn amount(mut self, sats: Option<u64>) -> Self {
        self.amount_sats = sats;
        self
    }

    fn fee_msat(mut self, msat: Option<u64>) -> Self {
        self.fee_sats = msat.map(msat_as_sats);
        self
    }

    fn lookup(mut self, lookup: impl Into<String>) -> Self {
        self.lookup = lookup.into();
        self
    }

    fn link(mut self, url: String) -> Self {
        self.lookup = url.clone();
        self.link = Some(url);
        self
    }
}

/// Every hop of a run's money, in the order it moved.
pub fn rows(
    competition_id: Option<Uuid>,
    entries: &[EntryTrace],
    scenario_refunds: &[(String, ScenarioRefund)],
    trail: Option<&Trail>,
    links: &Links,
) -> Vec<Row> {
    let mut rows = Vec::new();
    if let Some(id) = competition_id {
        rows.push(
            Row::new("competition", Status::Done, id.to_string())
                .between("synth", "coordinator")
                .link(format!(
                    "{}/competitions/{id}/leaderboard",
                    links.coordinator.trim_end_matches('/')
                )),
        );
        rows.push(
            Row::new("oracle event", Status::Done, id.to_string())
                .between("coordinator", "oracle")
                .link(format!(
                    "{}/events/{id}",
                    links.oracle.trim_end_matches('/')
                )),
        );
    }

    for entry in entries {
        rows.extend(entry_rows(entry, competition_id, trail, links));
    }

    if let Some(trail) = trail {
        rows.extend(contract_rows(entries, trail, links));
        rows.extend(payout_rows(trail));
        for refund in &trail.refunds {
            let done = refund.state == "settled";
            rows.push(
                Row::new(
                    format!("refund · {}", refund.user),
                    if done { Status::Done } else { Status::Waiting },
                    refund
                        .ark_txid
                        .clone()
                        .unwrap_or_else(|| refund.ticket_id.to_string()),
                )
                .between("escrow", format!("{}'s Lightning Address", refund.user))
                .amount(Some(refund.paid_sats))
                .lookup(format!(
                    "refund {} of ticket {}",
                    refund.state, refund.ticket_id
                )),
            );
        }
    }
    let traced_refunds = trail.map_or(0, |trail| trail.refunds.len());
    if traced_refunds == 0 {
        for (user, refund) in scenario_refunds {
            rows.push(
                Row::new(
                    format!("refund · {user}"),
                    Status::Done,
                    refund.ark_txid.clone().unwrap_or_default(),
                )
                .between("escrow", format!("{user}'s Lightning Address"))
                .amount(Some(refund.paid_sats))
                .lookup("the Arkade transaction that paid the refund swap"),
            );
        }
    }
    rows
}

fn entry_rows(
    entry: &EntryTrace,
    competition_id: Option<Uuid>,
    trail: Option<&Trail>,
    links: &Links,
) -> Vec<Row> {
    let who = &entry.user;
    let mut rows = Vec::new();
    let payee = entry
        .payment
        .as_ref()
        .and_then(|payment| payment.route.last())
        .map(|hop| hop.alias.clone().unwrap_or_else(|| hop.pubkey.clone()))
        .unwrap_or_else(|| "ark-swapd's node".to_string());
    let payer = entry
        .payment
        .as_ref()
        .map(|payment| payment.payer_alias.clone())
        .filter(|alias| !alias.is_empty())
        .unwrap_or_else(|| "the payer".to_string());

    if let Some(invoice) = &entry.invoice {
        rows.push(
            Row::new(
                format!("entry invoice · {who}"),
                Status::Done,
                invoice.clone(),
            )
            .between("coordinator", who.clone())
            .amount(entry.amount_sats)
            .lookup(format!("lncli decodepayreq {invoice}")),
        );
    }
    if let Some(hash) = &entry.payment_hash {
        let mut row = Row::new(
            format!("entry payment · {who}"),
            if entry.paid {
                Status::Done
            } else {
                Status::Failed
            },
            hash.clone(),
        )
        .between(payer.clone(), payee.clone())
        .amount(entry.amount_sats)
        .fee_msat(entry.payment.as_ref().map(|payment| payment.fee_msat));
        row.preimage = entry
            .payment
            .as_ref()
            .map(|payment| payment.preimage.clone())
            .filter(|preimage| !preimage.is_empty());
        row.lookup = match &entry.payment {
            Some(_) => format!(
                "lncli trackpayment {hash} (on {payer}); lncli lookupinvoice {hash} (on {payee})"
            ),
            None => "settled by the coordinator's test endpoint".to_string(),
        };
        rows.push(row);
    }
    if let Some(payment) = entry.payment.as_ref().filter(|p| !p.route.is_empty()) {
        let mut from = payment.payer_alias.clone();
        let hops: Vec<String> = payment
            .route
            .iter()
            .map(|hop| {
                let to = hop.alias.clone().unwrap_or_else(|| hop.pubkey.clone());
                let hop = format!("{} ({from} → {to})", hop.chan_id);
                from = to;
                hop
            })
            .collect();
        rows.push(
            Row::new(
                format!("entry route · {who}"),
                Status::Done,
                hops.join(", "),
            )
            .between(payer.clone(), payee.clone())
            .lookup(format!(
                "lncli getchaninfo --chan_id {}",
                payment.route[0].chan_id
            )),
        );
    }

    let swap = trail.and_then(|trail| {
        trail
            .swaps
            .iter()
            .find(|swap| Some(&swap.payment_hash) == entry.payment_hash.as_ref())
    });
    if let Some(swap) = swap {
        let status = match swap.state.as_str() {
            "settled" | "escrow_paid" => Status::Done,
            "expired" | "failed" | "unsettled" => Status::Failed,
            _ => Status::Waiting,
        };
        let swap_lookup = match &links.ark_swap {
            Some(url) => format!(
                "curl -H \"Authorization: Bearer $ARK_SWAPD_TOKEN\" {url}/v1/swaps/{}",
                swap.id
            ),
            None => format!("ark-swapd swap {}", swap.id),
        };
        rows.push(
            Row::new(format!("escrow swap · {who}"), status, swap.id.to_string())
                .between("ark-swapd", swap.escrow_address.clone())
                .amount(Some(swap.amount_sat))
                .fee_msat(
                    entry
                        .amount_sats
                        .map(|paid| paid.saturating_sub(swap.amount_sat) * 1000),
                )
                .lookup(swap_lookup),
        );
        if let Some(vtxo) = &swap.escrow_vtxo {
            rows.push(
                Row::new(format!("escrow output · {who}"), status, vtxo.clone())
                    .between("ark-swapd", format!("{who}'s escrow"))
                    .amount(Some(swap.amount_sat))
                    .lookup(match &swap.ark_txid {
                        Some(txid) => format!("Arkade transaction {txid}"),
                        None => "the escrow's Arkade output".to_string(),
                    }),
            );
        }
    }

    if let (Some(ticket), Some(competition)) = (entry.ticket_id, competition_id) {
        rows.push(
            Row::new(
                format!("ticket · {who}"),
                if entry.paid {
                    Status::Done
                } else {
                    Status::Waiting
                },
                ticket.to_string(),
            )
            .between(who.clone(), "coordinator")
            .amount(entry.amount_sats)
            .lookup(format!(
                "{}/api/v1/competitions/{competition}/tickets/{ticket}/status (signed by the player)",
                links.coordinator.trim_end_matches('/')
            )),
        );
    }
    if let Some(entry_id) = entry.entry_id {
        rows.push(
            Row::new(
                format!("entry · {who}"),
                if entry.entry_submitted {
                    Status::Done
                } else if entry.paid {
                    Status::Failed
                } else {
                    Status::Waiting
                },
                entry_id.to_string(),
            )
            .between(who.clone(), "coordinator")
            .lookup(format!(
                "{}/api/v1/entries?event_id={} (signed by the player)",
                links.coordinator.trim_end_matches('/'),
                competition_id.map(|id| id.to_string()).unwrap_or_default()
            )),
        );
    }
    rows
}

fn contract_rows(entries: &[EntryTrace], trail: &Trail, links: &Links) -> Vec<Row> {
    let mut rows = Vec::new();
    let pot = trail.settlement.as_ref().map(|s| s.pot_sats);
    let paid_in = paid_in(entries);
    if let Some(funding) = &trail.funding_tx {
        let id = match funding.vout {
            Some(vout) => format!("{}:{vout}", funding.txid),
            None => funding.txid.clone(),
        };
        rows.push(
            Row::new(
                "contract funding",
                if funding.confirmed {
                    Status::Done
                } else {
                    Status::Waiting
                },
                id,
            )
            .between("players' escrows (Arkade)", "the contract, on-chain")
            .amount(pot)
            .fee_msat(funding.fee_sat.map(|fee| fee * 1000))
            .link(links.tx(&funding.txid)),
        );
    }
    if let Some(pot) = pot {
        rows.push(
            Row::new("coordinator fee", Status::Done, String::new())
                .between("players", "coordinator")
                .amount(Some(paid_in.saturating_sub(pot)))
                .lookup("what the players paid beyond the pot"),
        );
    }
    if let Some(outcome) = &trail.outcome_tx {
        rows.push(
            Row::new(
                "outcome",
                if outcome.confirmed {
                    Status::Done
                } else {
                    Status::Waiting
                },
                outcome.txid.clone(),
            )
            .between("the contract", "the winners' outputs")
            .amount(pot)
            .fee_msat(outcome.fee_sat.map(|fee| fee * 1000))
            .link(links.tx(&outcome.txid)),
        );
    }
    rows
}

fn payout_rows(trail: &Trail) -> Vec<Row> {
    let ended = trail
        .competition
        .as_ref()
        .is_some_and(|c| c.failed_at.is_some() || c.cancelled_at.is_some());
    let mut rows = Vec::new();
    for payout in trail
        .payouts
        .iter()
        .filter(|p| p.owed_sats > 0 || p.invoice.is_some())
    {
        let status = match (payout.is_confirmed(), ended) {
            (true, _) => Status::Done,
            (false, true) => Status::Failed,
            (false, false) => Status::Waiting,
        };
        let from = payout
            .paid_by
            .clone()
            .unwrap_or_else(|| "the coordinator's node".to_string());
        let to = match (&payout.lightning_address, &payout.payee_alias) {
            (Some(address), Some(node)) => format!("{address} ({node})"),
            (Some(address), None) => address.clone(),
            (None, Some(node)) => node.clone(),
            (None, None) => format!("{}'s invoice", payout.user),
        };
        let Some(hash) = &payout.payment_hash else {
            rows.push(
                Row::new(
                    format!("payout · {}", payout.user),
                    status,
                    payout.entry_id.to_string(),
                )
                .between(from, to)
                .amount(Some(payout.owed_sats))
                .lookup(if payout.sent_at.is_some() {
                    "sent, but the coordinator reports no invoice"
                } else {
                    "owed, not sent yet: entry id shown"
                }),
            );
            continue;
        };
        let mut row = Row::new(format!("payout · {}", payout.user), status, hash.clone())
            .between(from.clone(), to)
            .amount(payout.amount_sats.or(Some(payout.owed_sats)))
            .fee_msat(payout.fee_msat)
            .lookup(format!(
                "lncli trackpayment {hash} (on {from}); lncli lookupinvoice {hash} (on {})",
                payout.payee_alias.as_deref().unwrap_or("the payee's node")
            ));
        row.preimage = payout.preimage.clone();
        rows.push(row);
        if let Some(invoice) = &payout.invoice {
            rows.push(
                Row::new(
                    format!("payout invoice · {}", payout.user),
                    status,
                    invoice.clone(),
                )
                .between(payout.user.clone(), "coordinator")
                .amount(payout.amount_sats)
                .lookup(format!("lncli decodepayreq {invoice}")),
            );
        }
    }
    rows
}

fn paid_in(entries: &[EntryTrace]) -> u64 {
    entries
        .iter()
        .filter(|entry| entry.paid)
        .filter_map(|entry| entry.amount_sats)
        .sum()
}

/// Whether what went in came out. Amounts are in sats.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Ledger {
    pub entries_paid: usize,
    /// What the players paid, all told.
    pub paid_in: u64,
    /// What the contract holds: None before there is one.
    pub pot: Option<u64>,
    /// What the players paid beyond the pot.
    pub coordinator_fee: Option<u64>,
    /// What the pot owes the winners under the deciding outcome.
    pub owed: u64,
    pub paid_out: u64,
    pub refunded: u64,
    /// Owed, and not sent yet.
    pub unpaid: u64,
    /// In the pot but owed to nobody: what the shares' rounding leaves.
    pub left_in_pot: u64,
    /// What the payer's node paid in routing fees on top of the entries, in millisats.
    pub entry_routing_fee_msat: Option<u64>,
    /// What the players paid beyond what ark-swapd put in their escrows.
    pub swap_fees: Option<u64>,
    /// On-chain fees: the funding transaction's, which the Arkade server's wallet pays, and the
    /// outcome's, which comes out of the contract.
    pub funding_fee: Option<u64>,
    pub outcome_fee: Option<u64>,
    /// What the coordinator's node paid to route the payouts, in millisats; None while any is
    /// not visible to synth.
    pub payout_routing_fee_msat: Option<u64>,
    /// What is not accounted for, once the money settled: owed and unpaid, or paid in and
    /// neither in the pot, the fee, nor refunded. Zero when it balances.
    pub remainder: i64,
    /// Why it does not balance, in words, for each thing that is off.
    pub flags: Vec<String>,
}

pub fn ledger(
    entries: &[EntryTrace],
    scenario_refunds: &[(String, ScenarioRefund)],
    trail: Option<&Trail>,
) -> Ledger {
    let paid_in = paid_in(entries);
    let entries_paid = entries.iter().filter(|entry| entry.paid).count();
    let mut ledger = Ledger {
        entries_paid,
        paid_in,
        entry_routing_fee_msat: entries
            .iter()
            .filter(|entry| entry.paid)
            .map(|entry| entry.payment.as_ref().map(|payment| payment.fee_msat))
            .sum(),
        ..Ledger::default()
    };
    if entries
        .iter()
        .any(|entry| entry.paid && entry.amount_sats.is_none())
    {
        ledger
            .flags
            .push("an entry was paid, but its amount was not recorded".into());
    }
    let refunded_traced: u64 = trail
        .map(|trail| {
            trail
                .refunds
                .iter()
                .filter(|refund| refund.state == "settled")
                .map(|refund| refund.paid_sats)
                .sum()
        })
        .unwrap_or(0);
    let refunded_by_scenario: u64 = scenario_refunds.iter().map(|(_, r)| r.paid_sats).sum();
    ledger.refunded = refunded_traced.max(refunded_by_scenario);

    let Some(trail) = trail else {
        return ledger;
    };
    let settled = trail.money.is_settled();
    let pot = trail.settlement.as_ref().map(|s| s.pot_sats);
    ledger.pot = pot;
    ledger.coordinator_fee = pot.map(|pot| paid_in.saturating_sub(pot));
    ledger.owed = trail.settlement.as_ref().map_or_else(
        || trail.payouts.iter().map(|p| p.owed_sats).sum(),
        |settlement| settlement.shares.iter().map(|share| share.owed_sats).sum(),
    );
    ledger.paid_out = trail
        .payouts
        .iter()
        .filter(|p| p.is_confirmed())
        .map(|p| p.amount_sats.unwrap_or(p.owed_sats))
        .sum();
    ledger.unpaid = ledger.owed.saturating_sub(ledger.paid_out);
    let decided = trail
        .settlement
        .as_ref()
        .is_some_and(|s| s.decided.is_some());
    if decided {
        ledger.left_in_pot = pot.unwrap_or(0).saturating_sub(ledger.owed);
    }
    let swapped: Vec<u64> = entries
        .iter()
        .filter(|entry| entry.paid)
        .filter_map(|entry| {
            let swap = trail
                .swaps
                .iter()
                .find(|swap| Some(&swap.payment_hash) == entry.payment_hash.as_ref())?;
            Some(entry.amount_sats?.saturating_sub(swap.amount_sat))
        })
        .collect();
    ledger.swap_fees =
        (swapped.len() == entries_paid && entries_paid > 0).then(|| swapped.iter().sum());
    ledger.funding_fee = trail.funding_tx.as_ref().and_then(|tx| tx.fee_sat);
    ledger.outcome_fee = trail.outcome_tx.as_ref().and_then(|tx| tx.fee_sat);
    let sent: Vec<&crate::trail::PayoutSeen> =
        trail.payouts.iter().filter(|p| p.is_confirmed()).collect();
    ledger.payout_routing_fee_msat = sent
        .iter()
        .map(|p| p.fee_msat)
        .sum::<Option<u64>>()
        .filter(|_| !sent.is_empty());

    // Players paid the pot plus the coordinator's fee; check the pot is what the entries buy.
    if let Some(pot) = pot {
        let entry_fee = trail
            .competition
            .as_ref()
            .and_then(|c| c.event_submission.get("entry_fee")?.as_u64());
        if let Some(entry_fee) = entry_fee {
            let expected = entry_fee * entries_paid as u64;
            if pot != expected {
                ledger.flags.push(format!(
                    "the pot holds {pot} sats, but {entries_paid} entries of {entry_fee} make {expected}"
                ));
            }
        }
        if pot > paid_in {
            ledger.flags.push(format!(
                "the pot holds {pot} sats, more than the {paid_in} the players paid"
            ));
        }
    }
    if ledger.left_in_pot > 0 {
        ledger.flags.push(format!(
            "{} sats of the pot are owed to nobody: the shares' rounding",
            ledger.left_in_pot
        ));
    }
    if ledger.paid_out > ledger.owed && ledger.owed > 0 {
        ledger.flags.push(format!(
            "{} sats were paid out, more than the {} owed",
            ledger.paid_out, ledger.owed
        ));
    }

    // What should be gone once the money settled.
    ledger.remainder = match (&trail.money, pot) {
        (Money::Refunded, _) | (_, None) if settled => paid_in as i64 - ledger.refunded as i64,
        (_, Some(pot)) if settled => pot as i64 - ledger.paid_out as i64 - ledger.refunded as i64,
        _ => 0,
    };
    if settled && ledger.remainder != 0 {
        ledger.flags.push(match (&trail.money, pot) {
            (Money::Refunded, _) | (_, None) => {
                format!("{} sats paid in were never refunded", ledger.remainder)
            }
            _ => format!(
                "{} sats of the pot have no confirmed payout or refund",
                ledger.remainder
            ),
        });
    }
    ledger
}

/// The trail as tab-separated lines, a header first, for a spreadsheet or a terminal.
pub fn tsv(rows: &[Row]) -> String {
    let clean = |field: &str| field.replace(['\t', '\n', '\r'], " ");
    let mut out =
        String::from("step\tstatus\tfrom\tto\tamount_sats\tfee_sats\tid\tpreimage\tlookup\n");
    for row in rows {
        let fields = [
            clean(&row.step),
            row.status.class().to_string(),
            clean(&row.from),
            clean(&row.to),
            row.amount_sats.map(|s| s.to_string()).unwrap_or_default(),
            row.fee_sats.clone().unwrap_or_default(),
            clean(&row.id),
            row.preimage.clone().unwrap_or_default(),
            clean(&row.lookup),
        ];
        out.push_str(&fields.join("\t"));
        out.push('\n');
    }
    out
}

/// The whole trail as JSON: where the money stands, the ledger, and every hop.
pub fn json(
    run_id: &str,
    competition_id: Option<Uuid>,
    money: Option<&Money>,
    ledger: &Ledger,
    rows: &[Row],
) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "run_id": run_id,
        "competition_id": competition_id,
        "money": money,
        "ledger": ledger,
        "hops": rows,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settlement::{Decided, Settlement, Share};
    use crate::trail::{ChainTx, EntryPayment, PayoutSeen, RouteHop, SwapSeen};
    use time::OffsetDateTime;

    fn entry(user: &str, hash: &str) -> EntryTrace {
        EntryTrace {
            user: user.into(),
            nostr_pubkey: "00".repeat(32),
            entry_id: Some(Uuid::now_v7()),
            ticket_id: Some(Uuid::now_v7()),
            amount_sats: Some(1100),
            payment_hash: Some(hash.into()),
            invoice: Some("lntbs11u1invoice".into()),
            payment: Some(EntryPayment {
                payer_alias: "thor".into(),
                payer_pubkey: "02aa".into(),
                preimage: format!("{hash}-preimage"),
                fee_msat: 1001,
                route: vec![
                    RouteHop {
                        chan_id: "3771505203178766336".into(),
                        pubkey: "02bb".into(),
                        alias: Some("odin".into()),
                    },
                    RouteHop {
                        chan_id: "42".into(),
                        pubkey: "02cc".into(),
                        alias: Some("swapd".into()),
                    },
                ],
            }),
            paid: true,
            entry_submitted: true,
        }
    }

    fn trail(money: Money, sent: bool) -> Trail {
        let competition: crate::client::competitions::CompetitionResponse =
            serde_json::from_value(serde_json::json!({
                "id": Uuid::now_v7(),
                "created_at": "2026-09-24T01:08:35Z",
                "event_submission": { "entry_fee": 1000 },
                "funding_broadcasted_at": "2026-09-24T01:11:26Z",
                "outcome_broadcasted_at": "2026-09-24T01:36:37Z",
            }))
            .unwrap();
        let owed = [1020, 990, 990];
        Trail {
            refreshed_at: OffsetDateTime::now_utc(),
            competition_id: competition.id,
            competition: Some(competition),
            settlement: Some(Settlement {
                pot_sats: 3000,
                decided: Some(Decided::Attested(3)),
                shares: owed
                    .iter()
                    .map(|owed_sats| Share {
                        pubkey: String::new(),
                        weight: 0,
                        owed_sats: *owed_sats,
                    })
                    .collect(),
            }),
            swaps: ["a", "b", "c"]
                .iter()
                .map(|hash| SwapSeen {
                    user: String::new(),
                    payment_hash: hash.to_string(),
                    id: Uuid::now_v7(),
                    state: "settled".into(),
                    amount_sat: 1100,
                    escrow_address: "tark1escrow".into(),
                    escrow_vtxo: Some("ee:0".into()),
                    ark_txid: Some("ee".into()),
                    error: None,
                })
                .collect(),
            payouts: owed
                .iter()
                .map(|owed_sats| PayoutSeen {
                    user: "alice".into(),
                    owed_sats: *owed_sats,
                    sent_at: sent.then(OffsetDateTime::now_utc),
                    amount_sats: sent.then_some(*owed_sats),
                    payment_hash: sent.then(|| {
                        "02d449a31fbb267c8f352e9968a79e3e5fc95c1bbeaa502fd6454ebde5a4bedc".into()
                    }),
                    preimage: sent.then(|| {
                        "1111111111111111111111111111111111111111111111111111111111111111".into()
                    }),
                    fee_msat: sent.then_some(2000),
                    ..PayoutSeen::default()
                })
                .collect(),
            refunds: Vec::new(),
            funding_tx: Some(ChainTx {
                txid: "8fc3".into(),
                vout: Some(0),
                fee_sat: Some(199),
                confirmed: true,
            }),
            outcome_tx: Some(ChainTx {
                txid: "5011".into(),
                vout: None,
                fee_sat: Some(222),
                confirmed: true,
            }),
            money,
            gaps: Vec::new(),
        }
    }

    fn entries() -> Vec<EntryTrace> {
        vec![
            entry("alice", "a"),
            entry("bob", "b"),
            entry("charlie", "c"),
        ]
    }

    /// Players paid 3,300 sats: a 3,000 pot and a 300 fee. Once every share is sent, nothing is
    /// left over.
    #[test]
    fn a_paid_out_competition_balances() {
        let ledger = ledger(&entries(), &[], Some(&trail(Money::PaidOut, true)));
        assert_eq!(ledger.paid_in, 3300);
        assert_eq!(ledger.pot, Some(3000));
        assert_eq!(ledger.coordinator_fee, Some(300));
        assert_eq!(ledger.paid_out, 3000);
        assert_eq!(ledger.unpaid, 0);
        assert_eq!(ledger.entry_routing_fee_msat, Some(3003));
        assert_eq!(ledger.swap_fees, Some(0));
        assert_eq!(
            (ledger.funding_fee, ledger.outcome_fee),
            (Some(199), Some(222))
        );
        assert_eq!(ledger.payout_routing_fee_msat, Some(6000));
        assert_eq!(ledger.remainder, 0);
        assert!(ledger.flags.is_empty(), "{:?}", ledger.flags);
    }

    /// Run 01a0d0f5: the pot was owed, never sent, and the money settled stuck.
    #[test]
    fn unpaid_winners_are_the_remainder_once_it_settles() {
        let stuck = Money::Stuck {
            reason: "0 of 3 payouts sent".into(),
        };
        let ledger = ledger(&entries(), &[], Some(&trail(stuck, false)));
        assert_eq!(ledger.unpaid, 3000);
        assert_eq!(ledger.remainder, 3000);
        assert_eq!(
            ledger.flags,
            ["3000 sats of the pot have no confirmed payout or refund"]
        );
        assert_eq!(ledger.payout_routing_fee_msat, None);

        let following = ledger_following();
        assert_eq!(
            following.remainder, 0,
            "not flagged while it is still moving"
        );
    }

    fn ledger_following() -> Ledger {
        ledger(&entries(), &[], Some(&trail(Money::Following, false)))
    }

    #[test]
    fn absent_payout_records_do_not_erase_the_pots_obligations() {
        let mut missing = trail(
            Money::TimedOut {
                reason: "unreachable".into(),
            },
            false,
        );
        missing.payouts.clear();
        let ledger = ledger(&entries(), &[], Some(&missing));
        assert_eq!(ledger.owed, 3000);
        assert_eq!(ledger.unpaid, 3000);
        assert_eq!(ledger.left_in_pot, 0);
        assert_eq!(ledger.remainder, 3000);
    }

    #[test]
    fn every_hop_carries_its_full_id_and_where_to_look_it_up() {
        let links = Links {
            explorer: "https://mutinynet.com".into(),
            oracle: "https://4casttruth.win".into(),
            coordinator: "https://5day4cast.com".into(),
            ark_swap: Some("http://10.90.9.1:9737".into()),
        };
        let trail = trail(Money::PaidOut, true);
        let rows = rows(
            Some(trail.competition_id),
            &entries(),
            &[],
            Some(&trail),
            &links,
        );
        let find = |step: &str| rows.iter().find(|row| row.step == step).unwrap();

        let payment = find("entry payment · alice");
        assert_eq!(payment.id, "a");
        assert_eq!(payment.preimage.as_deref(), Some("a-preimage"));
        assert_eq!(payment.fee_sats.as_deref(), Some("1.001"));
        assert_eq!(
            (payment.from.as_str(), payment.to.as_str()),
            ("thor", "swapd")
        );
        assert!(payment.lookup.contains("lncli trackpayment a (on thor)"));

        assert_eq!(
            find("entry route · alice").id,
            "3771505203178766336 (thor → odin), 42 (odin → swapd)"
        );
        assert_eq!(
            find("contract funding").link.as_deref(),
            Some("https://mutinynet.com/tx/8fc3")
        );
        assert_eq!(find("contract funding").id, "8fc3:0");
        assert_eq!(
            find("oracle event").link.as_deref(),
            Some(format!("https://4casttruth.win/events/{}", trail.competition_id).as_str())
        );
        assert!(find("escrow swap · alice").lookup.starts_with(
            "curl -H \"Authorization: Bearer $ARK_SWAPD_TOKEN\" http://10.90.9.1:9737/v1/swaps/"
        ));
        assert_eq!(find("escrow output · alice").id, "ee:0");

        let tsv = tsv(&rows);
        let mut lines = tsv.lines();
        assert_eq!(
            lines.next(),
            Some("step\tstatus\tfrom\tto\tamount_sats\tfee_sats\tid\tpreimage\tlookup")
        );
        assert!(lines.all(|line| line.split('\t').count() == 9));
    }
}
