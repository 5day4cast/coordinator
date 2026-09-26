//! A run's money trail: every hop its money took, with the full id to look each one up by, and a
//! ledger checking that what went in came out.

use serde::Serialize;
use uuid::Uuid;

use super::format::{msat_as_sats, sats};
use crate::trail::{
    contracted, payout_states, EntryPayment, EntryTrace, Money, PayeeCheck, PayoutState, Trail,
};

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
    /// The Arkade server, whose indexer knows each escrow output.
    pub arkd: Option<String>,
}

impl Links {
    pub fn tx(&self, txid: &str) -> String {
        format!("{}/tx/{txid}", self.explorer.trim_end_matches('/'))
    }

    pub fn coordinator(&self) -> &str {
        self.coordinator.trim_end_matches('/')
    }

    /// The command that asks ark-swapd about a swap.
    pub fn swap_lookup(&self, id: Uuid) -> String {
        format!(
            "curl -s -H \"Authorization: Bearer $ARK_SWAPD_TOKEN\" {}/v1/swaps/{id}",
            self.ark_swap
                .as_deref()
                .map_or("$ARK_SWAPD_URL", |url| url.trim_end_matches('/'))
        )
    }

    /// The command that asks the Arkade server's indexer about an output: whether it was spent
    /// or swept, and when it expires.
    pub fn vtxo_lookup(&self, outpoint: &str) -> String {
        format!(
            "curl -s '{}/v1/indexer/vtxos?outpoints={outpoint}'",
            self.arkd
                .as_deref()
                .map_or("$ARKD_URL", |url| url.trim_end_matches('/'))
        )
    }
}

/// A refund the refund scenario saw settle, from its step.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ScenarioRefund {
    pub paid_sats: u64,
    pub ark_txid: Option<String>,
}

/// Where a hop, or a stage of the money's flow, got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Done,
    Active,
    Failed,
    /// Not reached.
    Waiting,
}

impl Status {
    pub fn class(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Active => "active",
            Status::Failed => "failed",
            Status::Waiting => "waiting",
        }
    }

    /// Where several things together got to: failed if any did, done once all are.
    pub fn of_all(statuses: impl IntoIterator<Item = Status>) -> Status {
        let statuses: Vec<Status> = statuses.into_iter().collect();
        if statuses.contains(&Status::Failed) {
            Status::Failed
        } else if !statuses.is_empty() && statuses.iter().all(|s| *s == Status::Done) {
            Status::Done
        } else if statuses.iter().any(|s| *s != Status::Waiting) {
            Status::Active
        } else {
            Status::Waiting
        }
    }
}

impl From<PayoutState> for Status {
    fn from(state: PayoutState) -> Self {
        match state {
            PayoutState::Paid => Status::Done,
            PayoutState::OwedNothing => Status::Waiting,
            PayoutState::SentUnconfirmed | PayoutState::Owed => Status::Active,
            PayoutState::OtherNode | PayoutState::NeverSent => Status::Failed,
        }
    }
}

/// A command that looks something up, and the node to run it on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lookup {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    pub command: String,
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
    /// Commands that look it up.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lookups: Vec<Lookup>,
    /// A page that shows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// What it is, when that needs saying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
            lookups: Vec::new(),
            link: None,
            note: None,
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

    fn lookup(mut self, on: Option<&str>, command: String) -> Self {
        self.lookups.push(Lookup {
            on: on.map(str::to_string),
            command,
        });
        self
    }

    fn link(mut self, url: String) -> Self {
        self.link = Some(url);
        self
    }

    fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    fn preimage(mut self, preimage: Option<String>) -> Self {
        self.preimage = preimage.filter(|preimage| !preimage.is_empty());
        self
    }
}

/// What a run's money trail is drawn from, all from synth's own database.
pub struct Run<'a> {
    pub competition_id: Option<Uuid>,
    pub entries: &'a [EntryTrace],
    pub scenario_refunds: &'a [(String, ScenarioRefund)],
    pub trail: Option<&'a Trail>,
    pub links: &'a Links,
    /// Whether the run paid its entries from a node, from its configuration. None when the
    /// configuration does not say.
    pub paid_by_node: Option<bool>,
}

impl Run<'_> {
    /// The entries that paid: by their steps, or found paid on the payer afterwards.
    fn paid_entries(&self) -> Vec<&EntryTrace> {
        match self.trail {
            Some(trail) => trail.paid_entries(self.entries),
            None => self.entries.iter().filter(|entry| entry.paid).collect(),
        }
    }

    /// How an entry was paid, as its step recorded it or the payer said afterwards.
    fn payment_of<'e>(&'e self, entry: &'e EntryTrace) -> Option<&'e EntryPayment> {
        entry
            .payment
            .as_ref()
            .or_else(|| self.trail?.late_payment(entry))
    }
}

/// Every hop of a run's money, in the order it moved.
pub fn rows(run: &Run) -> Vec<Row> {
    let links = run.links;
    let mut rows = Vec::new();
    if let Some(id) = run.competition_id {
        rows.push(
            Row::new("competition", Status::Done, id.to_string())
                .between("synth", "coordinator")
                .link(format!(
                    "{}/competitions/{id}/leaderboard",
                    links.coordinator()
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
    for entry in run.entries {
        rows.extend(entry_rows(run, entry));
    }
    if let Some(trail) = run.trail {
        rows.extend(contract_rows(trail, links));
        rows.extend(payout_rows(trail));
        rows.extend(refund_rows(trail));
    }
    if run.trail.is_none_or(|trail| trail.refunds.is_empty()) {
        for (user, refund) in run.scenario_refunds {
            let mut row = Row::new(
                format!("refund · {user}"),
                Status::Done,
                refund.ark_txid.clone().unwrap_or_default(),
            )
            .between("escrow", format!("{user}'s Lightning Address"))
            .amount(Some(refund.paid_sats))
            .note("the Arkade transaction that paid the refund's swap");
            if let Some(txid) = &refund.ark_txid {
                row = row.lookup(None, format!("ark transaction {txid}"));
            }
            rows.push(row);
        }
    }
    rows
}

fn entry_rows(run: &Run, entry: &EntryTrace) -> Vec<Row> {
    let links = run.links;
    let who = &entry.user;
    let trail = run.trail;
    let payment = run.payment_of(entry);
    let paid = entry.paid || trail.is_some_and(|trail| trail.late_payment(entry).is_some());
    let mut rows = Vec::new();
    let payee = payment
        .and_then(|payment| payment.route.last())
        .map(|hop| hop.alias.clone().unwrap_or_else(|| hop.pubkey.clone()))
        .unwrap_or_else(|| "ark-swapd's node".to_string());
    let payer = payment
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
            .lookup(None, format!("lncli decodepayreq {invoice}")),
        );
    }
    if let Some(hash) = &entry.payment_hash {
        let status = if paid {
            Status::Done
        } else if trail.is_some_and(|trail| {
            trail
                .late_payments
                .iter()
                .any(|late| &late.payment_hash == hash)
        }) {
            Status::Failed
        } else {
            Status::Active
        };
        let mut row = Row::new(format!("entry payment · {who}"), status, hash.clone())
            .between(payer.clone(), payee.clone())
            .amount(entry.amount_sats)
            .fee_msat(payment.map(|payment| payment.fee_msat))
            .preimage(payment.map(|payment| payment.preimage.clone()));
        row = match (payment, entry.settled_by_test_endpoint, run.paid_by_node) {
            (Some(_), _, _) => row
                .lookup(Some(payer.as_str()), format!("lncli trackpayment {hash}"))
                .lookup(Some(payee.as_str()), format!("lncli lookupinvoice {hash}")),
            (None, true, _) | (None, false, Some(false)) => {
                row.note("settled by the coordinator's test endpoint: no Lightning payment")
            }
            (None, false, Some(true)) => row
                .note(if entry.paid {
                    "paid from the payer's node; this run predates synth keeping payment details"
                } else {
                    "the step ended before its payment did; the payer has not said how it ended"
                })
                .lookup(Some("the payer"), format!("lncli trackpayment {hash}")),
            (None, false, None) => row.note("synth did not record how this entry was paid"),
        };
        rows.push(row);
    }
    if let Some(payment) = payment.filter(|payment| !payment.route.is_empty()) {
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
            .lookup(
                Some(payer.as_str()),
                format!("lncli getchaninfo --chan_id {}", payment.route[0].chan_id),
            ),
        );
    }

    if let Some(swap) = trail.and_then(|trail| trail.swap_of(entry)) {
        let status = match swap.state.as_str() {
            "settled" | "escrow_paid" if swap.escrow_vtxo.is_some() => Status::Done,
            "settled" | "escrow_paid" => Status::Active,
            "expired" | "failed" | "unsettled" => Status::Failed,
            _ => Status::Active,
        };
        let mut row = Row::new(format!("escrow swap · {who}"), status, swap.id.to_string())
            .between("ark-swapd", swap.escrow_address.clone())
            .amount(Some(swap.amount_sat))
            .lookup(None, links.swap_lookup(swap.id));
        if let Some(paid) = entry.amount_sats.filter(|paid| *paid >= swap.amount_sat) {
            row = row.fee_msat(Some((paid - swap.amount_sat) * 1000));
        }
        if swap.funded_without_vtxo() {
            row = row.note(format!(
                "ark-swapd says {}, but records no escrow output",
                swap.state
            ));
        } else if let Some(error) = &swap.error {
            row = row.note(error.clone());
        }
        rows.push(row);
        if let Some(outpoint) = &swap.escrow_vtxo {
            let mut row = Row::new(format!("escrow output · {who}"), status, outpoint.clone())
                .between("ark-swapd", format!("{who}'s escrow"))
                .amount(Some(swap.amount_sat))
                .lookup(None, links.vtxo_lookup(outpoint));
            if let Some(vtxo) = &swap.vtxo {
                row = row.note(match (vtxo.swept, vtxo.spent, &vtxo.spent_by) {
                    (true, _, _) => "swept by the Arkade server".to_string(),
                    (false, true, Some(by)) => format!("spent by {by}"),
                    (false, true, None) => "spent".to_string(),
                    (false, false, _) => "unspent".to_string(),
                });
            }
            rows.push(row);
        }
    }

    if let (Some(ticket), Some(competition)) = (entry.ticket_id, run.competition_id) {
        rows.push(
            Row::new(
                format!("ticket · {who}"),
                if paid { Status::Done } else { Status::Active },
                ticket.to_string(),
            )
            .between(who.clone(), "coordinator")
            .amount(entry.amount_sats)
            .link(format!(
                "{}/api/v1/competitions/{competition}/tickets/{ticket}/status",
                links.coordinator()
            ))
            .note("signed by the player"),
        );
    }
    if let Some(entry_id) = entry.entry_id {
        rows.push(
            Row::new(
                format!("entry · {who}"),
                if entry.entry_submitted {
                    Status::Done
                } else if paid {
                    Status::Failed
                } else {
                    Status::Active
                },
                entry_id.to_string(),
            )
            .between(who.clone(), "coordinator")
            .link(format!(
                "{}/api/v1/entries?event_id={}",
                links.coordinator(),
                run.competition_id
                    .map(|id| id.to_string())
                    .unwrap_or_default()
            ))
            .note("signed by the player"),
        );
    }
    rows
}

fn chain_status(confirmed: bool) -> Status {
    if confirmed {
        Status::Done
    } else {
        Status::Active
    }
}

fn contract_rows(trail: &Trail, links: &Links) -> Vec<Row> {
    let mut rows = Vec::new();
    let pot = trail.settlement.as_ref().map(|s| s.pot_sats);
    if let Some(funding) = &trail.funding_tx {
        let id = match funding.vout {
            Some(vout) => format!("{}:{vout}", funding.txid),
            None => funding.txid.clone(),
        };
        rows.push(
            Row::new("contract funding", chain_status(funding.confirmed), id)
                .between("players' escrows (Arkade)", "the contract, on-chain")
                .amount(pot)
                .fee_msat(funding.fee_sat.map(|fee| fee * 1000))
                .link(links.tx(&funding.txid))
                .note("the fee is the whole Arkade batch's, shared by every output in it"),
        );
    }
    if let Some(outcome) = &trail.outcome_tx {
        rows.push(
            Row::new(
                "outcome",
                chain_status(outcome.confirmed),
                outcome.txid.clone(),
            )
            .between("the contract", "the winners' outputs")
            .amount(pot)
            .fee_msat(outcome.fee_sat.map(|fee| fee * 1000))
            .link(links.tx(&outcome.txid)),
        );
    }
    for closing in &trail.closing_txs {
        rows.push(
            Row::new(
                "delta or expiry",
                chain_status(closing.confirmed),
                closing.txid.clone(),
            )
            .between("the contract's outputs", "whoever its outputs pay")
            .amount(closing.value_sat)
            .fee_msat(closing.fee_sat.map(|fee| fee * 1000))
            .link(links.tx(&closing.txid))
            .note("where the pot went once the payout window closed"),
        );
    }
    rows
}

fn payout_rows(trail: &Trail) -> Vec<Row> {
    let states = payout_states(&trail.payouts, trail.ended());
    let mut rows = Vec::new();
    for (payout, state) in trail.payouts.iter().zip(states) {
        if payout.owed_sats == 0 && payout.invoice.is_none() {
            continue;
        }
        let status = Status::from(state);
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
                .note(match state {
                    PayoutState::SentUnconfirmed => "sent, but the coordinator reports no invoice",
                    PayoutState::NeverSent => "owed, and never sent: entry id shown",
                    _ => "owed, not sent yet: entry id shown",
                }),
            );
            continue;
        };
        let payee = payout.payee_alias.as_deref().unwrap_or("the payee's node");
        let checked = match payout.payee {
            PayeeCheck::Unchecked => "who it paid is not checked: set trail.payee".to_string(),
            PayeeCheck::Matches => format!("pays {payee}, the Lightning Address's node"),
            PayeeCheck::SettledOnPayee => format!("settled on {payee}"),
            PayeeCheck::OtherNode => format!(
                "pays {}, not the Lightning Address's node",
                payout.payee_pubkey.as_deref().unwrap_or("another node")
            ),
        };
        rows.push(
            Row::new(format!("payout · {}", payout.user), status, hash.clone())
                .between(from.clone(), to)
                .amount(payout.amount_sats.or(Some(payout.owed_sats)))
                .fee_msat(payout.fee_msat)
                .preimage(payout.preimage.clone())
                .lookup(Some(from.as_str()), format!("lncli trackpayment {hash}"))
                .lookup(Some(payee), format!("lncli lookupinvoice {hash}"))
                .note(checked),
        );
        if let Some(invoice) = &payout.invoice {
            rows.push(
                Row::new(
                    format!("payout invoice · {}", payout.user),
                    status,
                    invoice.clone(),
                )
                .between(payout.user.clone(), "coordinator")
                .amount(payout.amount_sats)
                .lookup(None, format!("lncli decodepayreq {invoice}")),
            );
        }
    }
    rows
}

fn refund_rows(trail: &Trail) -> Vec<Row> {
    let mut rows = Vec::new();
    for refund in &trail.refunds {
        let status = if refund.is_settled() {
            Status::Done
        } else {
            Status::Active
        };
        let to = format!("{}'s Lightning Address", refund.user);
        let mut row = Row::new(
            format!("refund · {}", refund.user),
            status,
            refund
                .ark_txid
                .clone()
                .unwrap_or_else(|| refund.ticket_id.to_string()),
        )
        .between("escrow", to.clone())
        .amount(Some(refund.paid_sats))
        .note(format!(
            "refund {}{}",
            refund.state,
            if refund.ark_txid.is_some() {
                ": the Arkade transaction that moved the escrow into its swap"
            } else {
                ": ticket id shown"
            }
        ));
        if let Some(txid) = &refund.ark_txid {
            row = row.lookup(None, format!("ark transaction {txid}"));
        }
        rows.push(row);
        if let Some(hash) = &refund.payment_hash {
            let from = refund
                .paid_by
                .clone()
                .unwrap_or_else(|| "the coordinator's node".to_string());
            let mut row = Row::new(
                format!("refund payment · {}", refund.user),
                if refund.preimage.is_some() {
                    Status::Done
                } else {
                    status
                },
                hash.clone(),
            )
            .between(from.clone(), to)
            .amount(Some(refund.paid_sats))
            .fee_msat(refund.fee_msat)
            .preimage(refund.preimage.clone())
            .lookup(Some(from.as_str()), format!("lncli trackpayment {hash}"));
            if let Some(invoice) = &refund.invoice {
                row = row.lookup(None, format!("lncli decodepayreq {invoice}"));
            }
            rows.push(row);
        }
    }
    rows
}

/// Whether what went in came out. Amounts are in sats.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Ledger {
    pub entries_paid: usize,
    /// What the players paid, all told.
    pub paid_in: u64,
    /// What ark-swapd put in the players' escrows; None unless every paid entry's swap is known.
    pub escrowed: Option<u64>,
    /// What the players paid beyond what reached their escrows: ark-swapd's fees. Negative when
    /// an escrow got more than its player paid.
    pub swap_fees: Option<i64>,
    /// What the contract holds: None before there is one.
    pub pot: Option<u64>,
    /// What the players paid beyond the pot.
    pub coordinator_fee: Option<u64>,
    /// What the pot owes the winners under the deciding outcome.
    pub owed: u64,
    /// Confirmed paid to the winners.
    pub paid_out: u64,
    pub confirmed_payouts: usize,
    /// Owed, and not confirmed paid.
    pub unpaid: u64,
    /// In the pot but owed to nobody: what rounding the shares down leaves.
    pub rounding: u64,
    pub refunded: u64,
    /// What the settled refunds' swaps and payments kept: their escrows less what the players got.
    pub refund_fees: Option<u64>,
    /// What the payer's node paid in routing fees on top of the entries, in millisats.
    pub entry_routing_fee_msat: Option<u64>,
    /// The funding transaction's fee: the whole Arkade batch's, shared by every output in it.
    pub funding_batch_fee: Option<u64>,
    /// The outcome transaction's fee, which comes out of the contract.
    pub outcome_fee: Option<u64>,
    /// What the delta or expiry transactions paid in fees.
    pub closing_fees: Option<u64>,
    /// What the coordinator's node paid to route the confirmed payouts, in millisats.
    pub payout_routing_fee_msat: Option<u64>,
    /// Once the money stopped moving, what is not accounted for: positive when money owed or
    /// held is missing, negative when more left than was owed. Zero while it moves.
    pub remainder: i64,
    /// Why it does not balance, in words, for each thing that is off.
    pub flags: Vec<String>,
}

pub fn ledger(run: &Run) -> Ledger {
    let paid = run.paid_entries();
    let paid_in: u64 = paid.iter().filter_map(|entry| entry.amount_sats).sum();
    let mut ledger = Ledger {
        entries_paid: paid.len(),
        paid_in,
        entry_routing_fee_msat: paid
            .iter()
            .map(|entry| run.payment_of(entry).map(|payment| payment.fee_msat))
            .sum(),
        ..Ledger::default()
    };
    if paid.iter().any(|entry| entry.amount_sats.is_none()) {
        ledger
            .flags
            .push("an entry was paid, but its amount was not recorded".into());
    }
    let refunded_traced: u64 = run
        .trail
        .map(|trail| {
            trail
                .refunds
                .iter()
                .filter(|refund| refund.is_settled())
                .map(|refund| refund.paid_sats)
                .sum()
        })
        .unwrap_or(0);
    let refunded_by_scenario: u64 = run.scenario_refunds.iter().map(|(_, r)| r.paid_sats).sum();
    ledger.refunded = refunded_traced.max(refunded_by_scenario);

    let Some(trail) = run.trail else {
        return ledger;
    };

    // What reached the escrows, and whether any escrow got more than its player paid.
    let swapped: Vec<(&EntryTrace, Option<u64>)> = paid
        .iter()
        .map(|entry| (*entry, trail.swap_of(entry).map(|swap| swap.amount_sat)))
        .collect();
    if !swapped.is_empty() && swapped.iter().all(|(_, amount)| amount.is_some()) {
        let escrowed: u64 = swapped.iter().filter_map(|(_, amount)| *amount).sum();
        ledger.escrowed = Some(escrowed);
        ledger.swap_fees = Some(paid_in as i64 - escrowed as i64);
    }
    for (entry, amount) in &swapped {
        if let (Some(paid), Some(escrowed)) = (entry.amount_sats, amount) {
            if *escrowed > paid {
                ledger.flags.push(format!(
                    "ark-swapd put {} sats more in {}'s escrow than {} paid",
                    sats(escrowed - paid),
                    entry.user,
                    entry.user
                ));
            }
        }
    }

    let pot = trail.settlement.as_ref().map(|s| s.pot_sats);
    ledger.pot = pot;
    ledger.coordinator_fee = pot.map(|pot| paid_in.saturating_sub(pot));
    ledger.owed = trail.settlement.as_ref().map_or_else(
        || trail.payouts.iter().map(|p| p.owed_sats).sum(),
        |settlement| settlement.shares.iter().map(|share| share.owed_sats).sum(),
    );
    let states = payout_states(&trail.payouts, trail.ended());
    let confirmed: Vec<_> = trail
        .payouts
        .iter()
        .zip(&states)
        .filter(|(_, state)| **state == PayoutState::Paid)
        .map(|(payout, _)| payout)
        .collect();
    if confirmed.len() < trail.payouts.iter().filter(|p| p.is_confirmed()).count() {
        ledger
            .flags
            .push("several entries name the same settled payment; it is counted once".into());
    }
    ledger.confirmed_payouts = confirmed.len();
    ledger.paid_out = confirmed
        .iter()
        .map(|p| p.amount_sats.unwrap_or(p.owed_sats))
        .sum();
    ledger.unpaid = ledger.owed.saturating_sub(ledger.paid_out);
    if trail
        .settlement
        .as_ref()
        .is_some_and(|s| s.decided.is_some())
    {
        ledger.rounding = pot.unwrap_or(0).saturating_sub(ledger.owed);
    }
    ledger.refund_fees = {
        let kept: Option<Vec<u64>> = trail
            .refunds
            .iter()
            .filter(|refund| refund.is_settled())
            .map(|refund| {
                let entry = run
                    .entries
                    .iter()
                    .find(|entry| entry.ticket_id == Some(refund.ticket_id))?;
                let escrowed = trail.swap_of(entry)?.amount_sat;
                Some(escrowed.saturating_sub(refund.paid_sats))
            })
            .collect();
        kept.filter(|kept| !kept.is_empty())
            .map(|kept| kept.iter().sum())
    };
    ledger.funding_batch_fee = trail.funding_tx.as_ref().and_then(|tx| tx.fee_sat);
    ledger.outcome_fee = trail.outcome_tx.as_ref().and_then(|tx| tx.fee_sat);
    ledger.closing_fees = (!trail.closing_txs.is_empty())
        .then(|| trail.closing_txs.iter().map(|tx| tx.fee_sat).sum())
        .flatten();
    ledger.payout_routing_fee_msat = (!confirmed.is_empty())
        .then(|| confirmed.iter().map(|p| p.fee_msat).sum())
        .flatten();

    // Real checks: the pot is what the entries buy, and the escrows could fund it.
    if let Some(pot) = pot {
        let entry_fee = trail
            .competition
            .as_ref()
            .and_then(|c| c.event_submission.get("entry_fee")?.as_u64());
        if let Some(entry_fee) = entry_fee {
            let expected = entry_fee * ledger.entries_paid as u64;
            if pot != expected {
                ledger.flags.push(format!(
                    "the pot holds {} sats, but {} entries of {} make {}",
                    sats(pot),
                    ledger.entries_paid,
                    sats(entry_fee),
                    sats(expected)
                ));
            }
        }
        if let Some(escrowed) = ledger.escrowed.filter(|escrowed| pot > *escrowed) {
            ledger.flags.push(format!(
                "the pot holds {} sats, more than the {} the escrows held",
                sats(pot),
                sats(escrowed)
            ));
        }
        if pot > paid_in {
            ledger.flags.push(format!(
                "the pot holds {} sats, more than the {} the players paid",
                sats(pot),
                sats(paid_in)
            ));
        }
    }
    if ledger.paid_out > ledger.owed && ledger.owed > 0 {
        ledger.flags.push(format!(
            "{} sats were paid out, more than the {} owed",
            sats(ledger.paid_out),
            sats(ledger.owed)
        ));
    }

    // Once the money stopped, what is missing.
    let stopped = trail.money.is_final() || matches!(trail.money, Money::Stuck { .. });
    let funded = trail.competition.as_ref().is_some_and(contracted);
    if stopped && trail.money != Money::NothingPaid {
        if funded
            || trail
                .competition
                .as_ref()
                .is_some_and(|c| c.completed_at.is_some())
        {
            ledger.remainder = ledger.owed as i64 - ledger.paid_out as i64;
            match ledger.remainder {
                0 => {}
                missing if missing > 0 => ledger.flags.push(format!(
                    "{} sats owed to the winners have no confirmed payout",
                    sats(missing.unsigned_abs())
                )),
                extra => ledger.flags.push(format!(
                    "{} sats more were paid out than the winners were owed",
                    sats(extra.unsigned_abs())
                )),
            }
        } else if trail.ended() {
            let held: u64 = paid
                .iter()
                .filter(|entry| {
                    !trail
                        .refund_of(entry)
                        .is_some_and(|refund| refund.is_settled())
                })
                .map(|entry| {
                    trail
                        .swap_of(entry)
                        .map(|swap| swap.amount_sat)
                        .or(entry.amount_sats)
                        .unwrap_or(0)
                })
                .sum();
            let scenario_covered = run.scenario_refunds.len() >= paid.len();
            ledger.remainder = if scenario_covered { 0 } else { held as i64 };
            if ledger.remainder > 0 {
                ledger.flags.push(format!(
                    "{} sats in the escrows were never refunded",
                    sats(held)
                ));
            }
        }
    }
    ledger
}

/// The trail as tab-separated lines, a header first, for a spreadsheet or a terminal.
pub fn tsv(rows: &[Row]) -> String {
    let clean = |field: &str| field.replace(['\t', '\n', '\r'], " ");
    let mut out =
        String::from("step\tstatus\tfrom\tto\tamount_sats\tfee_sats\tid\tpreimage\tlookup\tnote\n");
    for row in rows {
        let lookup = match &row.link {
            Some(link) => link.clone(),
            None => row
                .lookups
                .iter()
                .map(|lookup| match &lookup.on {
                    Some(on) => format!("on {on}: {}", lookup.command),
                    None => lookup.command.clone(),
                })
                .collect::<Vec<_>>()
                .join("; "),
        };
        let fields = [
            clean(&row.step),
            row.status.class().to_string(),
            clean(&row.from),
            clean(&row.to),
            row.amount_sats.map(|s| s.to_string()).unwrap_or_default(),
            row.fee_sats.clone().unwrap_or_default(),
            clean(&row.id),
            row.preimage.clone().unwrap_or_default(),
            clean(&lookup),
            clean(row.note.as_deref().unwrap_or_default()),
        ];
        out.push_str(&fields.join("\t"));
        out.push('\n');
    }
    out
}

/// The whole trail as JSON: where the money stands, where it was held, the ledger, and every hop.
pub fn json(run_id: &str, run: &Run, ledger: &Ledger, rows: &[Row]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "run_id": run_id,
        "competition_id": run.competition_id,
        "money": run.trail.map(|trail| &trail.money),
        "held": run.trail.and_then(|trail| trail.held.as_ref()),
        "ledger": ledger,
        "hops": rows,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settlement::{Decided, Settlement, Share};
    use crate::trail::{ChainTx, EntryPayment, PayoutSeen, RefundSeen, RouteHop, SwapSeen};
    use sha2::{Digest, Sha256};
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
            lightning_address: Some("freya@lnurl.example".into()),
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
            ..EntryTrace::default()
        }
    }

    /// Three players paid 1,100 sats each; ark-swapd kept 10 of each and put 1,090 in each
    /// escrow; the contract holds a 3,000 pot split 1,020 / 990 / 990.
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
                    amount_sat: 1090,
                    escrow_address: "tark1escrow".into(),
                    escrow_vtxo: Some("ee:0".into()),
                    ark_txid: Some("ee".into()),
                    error: None,
                    invoice_state: None,
                    vtxo: None,
                })
                .collect(),
            late_payments: Vec::new(),
            payouts: owed
                .iter()
                .enumerate()
                .map(|(index, owed_sats)| PayoutSeen {
                    user: "alice".into(),
                    owed_sats: *owed_sats,
                    sent_at: sent.then(OffsetDateTime::now_utc),
                    amount_sats: sent.then_some(*owed_sats),
                    payment_hash: sent.then(|| hex::encode(Sha256::digest([index as u8 + 1; 32]))),
                    preimage: sent.then(|| hex::encode([index as u8 + 1; 32])),
                    fee_msat: sent.then_some(2000),
                    ..PayoutSeen::default()
                })
                .collect(),
            refunds: Vec::new(),
            funding_tx: Some(ChainTx {
                txid: "8fc3".into(),
                vout: Some(0),
                fee_sat: Some(199),
                value_sat: None,
                confirmed: true,
            }),
            outcome_tx: Some(ChainTx {
                txid: "5011".into(),
                vout: None,
                fee_sat: Some(222),
                value_sat: None,
                confirmed: true,
            }),
            closing_txs: Vec::new(),
            money,
            held: None,
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

    fn links() -> Links {
        Links {
            explorer: "https://mutinynet.com".into(),
            oracle: "https://4casttruth.win".into(),
            coordinator: "https://5day4cast.com".into(),
            ark_swap: Some("http://10.90.9.1:9737".into()),
            arkd: None,
        }
    }

    fn ledger_of(entries: &[EntryTrace], trail: &Trail) -> Ledger {
        ledger(&Run {
            competition_id: Some(trail.competition_id),
            entries,
            scenario_refunds: &[],
            trail: Some(trail),
            links: &links(),
            paid_by_node: Some(true),
        })
    }

    /// Every figure comes from a different record: the entries' payments, ark-swapd's swaps, the
    /// contract, and the payouts. Once every share is sent, nothing is missing or flagged.
    #[test]
    fn a_paid_out_competition_balances() {
        let ledger = ledger_of(&entries(), &trail(Money::PaidOut, true));
        assert_eq!(ledger.paid_in, 3300);
        assert_eq!(ledger.escrowed, Some(3270));
        assert_eq!(ledger.swap_fees, Some(30));
        assert_eq!(ledger.pot, Some(3000));
        assert_eq!(ledger.coordinator_fee, Some(300));
        assert_eq!(
            (ledger.owed, ledger.paid_out, ledger.unpaid),
            (3000, 3000, 0)
        );
        assert_eq!(ledger.confirmed_payouts, 3);
        assert_eq!(ledger.entry_routing_fee_msat, Some(3003));
        assert_eq!(
            (ledger.funding_batch_fee, ledger.outcome_fee),
            (Some(199), Some(222))
        );
        assert_eq!(ledger.payout_routing_fee_msat, Some(6000));
        assert_eq!(ledger.remainder, 0);
        assert!(ledger.flags.is_empty(), "{:?}", ledger.flags);
    }

    /// Escrows that could not have funded the pot, and one that got more than its player paid,
    /// are flagged: numbers that disagree between records, not an identity.
    #[test]
    fn records_that_disagree_are_flagged() {
        let mut short = trail(Money::PaidOut, true);
        for swap in &mut short.swaps {
            swap.amount_sat = 900;
        }
        short.swaps[0].amount_sat = 1150;
        let ledger = ledger_of(&entries(), &short);
        assert_eq!(ledger.escrowed, Some(2950));
        assert_eq!(
            ledger.flags,
            [
                "ark-swapd put 50 sats more in alice's escrow than alice paid",
                "the pot holds 3,000 sats, more than the 2,950 the escrows held",
            ]
        );
        assert_eq!(ledger.swap_fees, Some(350));
    }

    /// The shares' rounding is shown once, as what the pot owes nobody, and is not missing money.
    #[test]
    fn rounding_is_shown_once_and_not_flagged() {
        let mut rounded = trail(Money::PaidOut, true);
        let owed = [1000, 999, 999];
        for (share, owed) in rounded
            .settlement
            .as_mut()
            .unwrap()
            .shares
            .iter_mut()
            .zip(owed)
        {
            share.owed_sats = owed;
        }
        for (payout, owed) in rounded.payouts.iter_mut().zip(owed) {
            payout.owed_sats = owed;
            payout.amount_sats = Some(owed);
        }
        let ledger = ledger_of(&entries(), &rounded);
        assert_eq!(ledger.rounding, 2);
        assert_eq!(ledger.remainder, 0);
        assert!(ledger.flags.is_empty(), "{:?}", ledger.flags);
    }

    /// Run 01a0d0f5: the pot was owed, never sent, and the money is stuck.
    #[test]
    fn unpaid_winners_are_the_remainder_once_the_money_stops() {
        let stuck = Money::Stuck {
            reason: "no payout was sent".into(),
            since: OffsetDateTime::now_utc(),
        };
        let ledger = ledger_of(&entries(), &trail(stuck, false));
        assert_eq!(ledger.unpaid, 3000);
        assert_eq!(ledger.remainder, 3000);
        assert_eq!(
            ledger.flags,
            ["3,000 sats owed to the winners have no confirmed payout"]
        );
        assert_eq!(ledger.payout_routing_fee_msat, None);

        let following = ledger_of(&entries(), &trail(Money::Following, false));
        assert_eq!(
            following.remainder, 0,
            "not judged while it is still moving"
        );
    }

    #[test]
    fn paying_out_more_than_owed_says_so() {
        let mut over = trail(Money::PaidOut, true);
        over.payouts[0].amount_sats = Some(1100);
        let ledger = ledger_of(&entries(), &over);
        assert_eq!(ledger.remainder, -80);
        assert!(ledger
            .flags
            .contains(&"80 sats more were paid out than the winners were owed".to_string()));
    }

    #[test]
    fn absent_payout_records_do_not_erase_the_pots_obligations() {
        let mut missing = trail(
            Money::Unverified {
                reason: "unreachable".into(),
            },
            false,
        );
        missing.payouts.clear();
        let ledger = ledger_of(&entries(), &missing);
        assert_eq!(ledger.owed, 3000);
        assert_eq!(ledger.unpaid, 3000);
        assert_eq!(ledger.remainder, 3000);
    }

    #[test]
    fn a_reused_payment_is_not_counted_twice_in_the_ledger() {
        let mut duplicate = trail(Money::PaidOut, true);
        duplicate.payouts[2].payment_hash = duplicate.payouts[1].payment_hash.clone();
        duplicate.payouts[2].preimage = duplicate.payouts[1].preimage.clone();
        let ledger = ledger_of(&entries(), &duplicate);
        assert_eq!(ledger.paid_out, 2010);
        assert_eq!(ledger.remainder, 990);
        assert_eq!(ledger.payout_routing_fee_msat, Some(4000));
        assert!(ledger
            .flags
            .iter()
            .any(|flag| flag.contains("counted once")));
    }

    /// A competition cancelled before its contract owes each escrow back; the ones not refunded
    /// are what is missing, and what the refunds' swaps kept is a fee, not a loss.
    #[test]
    fn escrows_never_refunded_are_the_remainder_of_a_cancelled_competition() {
        let entries = entries();
        let mut cancelled = trail(
            Money::Stuck {
                reason: "not refunded".into(),
                since: OffsetDateTime::now_utc(),
            },
            false,
        );
        cancelled.competition = Some(
            serde_json::from_value(serde_json::json!({
                "id": cancelled.competition_id,
                "created_at": "2026-09-24T01:08:35Z",
                "event_submission": { "entry_fee": 1000 },
                "cancelled_at": "2026-09-24T03:00:00Z",
            }))
            .unwrap(),
        );
        cancelled.settlement = None;
        cancelled.payouts.clear();
        cancelled.refunds.push(RefundSeen {
            user: "alice".into(),
            ticket_id: entries[0].ticket_id.unwrap(),
            state: "settled".into(),
            paid_sats: 1080,
            ark_txid: Some("aa".into()),
            invoice: None,
            payment_hash: None,
            preimage: None,
            fee_msat: None,
            paid_by: None,
        });
        let ledger = ledger_of(&entries, &cancelled);
        assert_eq!(ledger.refunded, 1080);
        assert_eq!(ledger.refund_fees, Some(10));
        assert_eq!(ledger.remainder, 2180);
        assert_eq!(
            ledger.flags,
            ["2,180 sats in the escrows were never refunded"]
        );
    }

    #[test]
    fn every_hop_carries_its_full_id_and_where_to_look_it_up() {
        let links = links();
        let trail = trail(Money::PaidOut, true);
        let entries = entries();
        let rows = rows(&Run {
            competition_id: Some(trail.competition_id),
            entries: &entries,
            scenario_refunds: &[],
            trail: Some(&trail),
            links: &links,
            paid_by_node: Some(true),
        });
        let find = |step: &str| rows.iter().find(|row| row.step == step).unwrap();

        let payment = find("entry payment · alice");
        assert_eq!(payment.id, "a");
        assert_eq!(payment.preimage.as_deref(), Some("a-preimage"));
        assert_eq!(payment.fee_sats.as_deref(), Some("1.001"));
        assert_eq!(
            (payment.from.as_str(), payment.to.as_str()),
            ("thor", "swapd")
        );
        // Each command pastes as it is; the node it runs on is said beside it.
        assert_eq!(
            payment.lookups,
            [
                Lookup {
                    on: Some("thor".into()),
                    command: "lncli trackpayment a".into()
                },
                Lookup {
                    on: Some("swapd".into()),
                    command: "lncli lookupinvoice a".into()
                },
            ]
        );

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
        assert!(find("escrow swap · alice").lookups[0].command.starts_with(
            "curl -s -H \"Authorization: Bearer $ARK_SWAPD_TOKEN\" http://10.90.9.1:9737/v1/swaps/"
        ));
        assert_eq!(find("escrow swap · alice").fee_sats.as_deref(), Some("10"));
        let output = find("escrow output · alice");
        assert_eq!(output.id, "ee:0");
        assert_eq!(
            output.lookups[0].command,
            "curl -s '$ARKD_URL/v1/indexer/vtxos?outpoints=ee:0'"
        );

        let tsv = tsv(&rows);
        let mut lines = tsv.lines();
        assert_eq!(
            lines.next(),
            Some("step\tstatus\tfrom\tto\tamount_sats\tfee_sats\tid\tpreimage\tlookup\tnote")
        );
        assert!(lines.all(|line| line.split('\t').count() == 10));
    }

    /// An entry paid before synth kept payment details is not mistaken for a test settlement.
    #[test]
    fn an_older_lightning_entry_is_not_labelled_a_test_settlement() {
        let mut older = entry("alice", "a");
        older.payment = None;
        let links = links();
        let row = |paid_by_node| {
            rows(&Run {
                competition_id: None,
                entries: std::slice::from_ref(&older),
                scenario_refunds: &[],
                trail: None,
                links: &links,
                paid_by_node,
            })
            .into_iter()
            .find(|row| row.step == "entry payment · alice")
            .unwrap()
        };
        assert!(row(Some(true))
            .note
            .unwrap()
            .starts_with("paid from the payer's node"));
        assert!(row(Some(false)).note.unwrap().contains("test endpoint"));
        let mut settled = older.clone();
        settled.settled_by_test_endpoint = true;
        assert!(rows(&Run {
            competition_id: None,
            entries: std::slice::from_ref(&settled),
            scenario_refunds: &[],
            trail: None,
            links: &links,
            paid_by_node: None,
        })[1]
            .note
            .as_deref()
            .unwrap()
            .contains("test endpoint"));
    }
}
