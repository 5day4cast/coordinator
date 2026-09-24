//! Where a run's money went, followed past the run's own steps.
//!
//! A lifecycle run ends once its competition awaits the oracle, but its money keeps moving for
//! hours: the oracle attests, the outcome goes on-chain, the coordinator pays the winners over
//! Lightning. Refunds of a cancelled competition come later still. The [`tracker`] keeps looking
//! at each run's competition until its money settles one way or another, and saves what it found
//! as the run's [`Trail`], which the run's page is drawn from.
//!
//! Money that stops somewhere it should not stay is [`Money::Stuck`]. The tracker keeps watching
//! it, and when it moves again the trail keeps the record of where it was held and for how long.

pub mod tracker;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::client::competitions::CompetitionResponse;
use crate::crypto::keys::SynthUser;
use crate::scenarios::StepResult;
use crate::settlement::Settlement;

/// Where an entry's money went, recorded by the scenario as it happens so a failed or interrupted
/// entry still shows how far its payment got. Saved as the details of the entry's step: first
/// before the entry pays, then once the step ends.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntryTrace {
    pub user: String,
    pub nostr_pubkey: String,
    #[serde(default)]
    pub entry_id: Option<Uuid>,
    #[serde(default)]
    pub ticket_id: Option<Uuid>,
    #[serde(default)]
    pub amount_sats: Option<u64>,
    /// Identifies the payment on the paying node, the invoice's node, and in ark-swapd.
    #[serde(default)]
    pub payment_hash: Option<String>,
    /// The invoice the ticket asked to be paid: ark-swapd's, for an Arkade escrow.
    #[serde(default)]
    pub invoice: Option<String>,
    /// Where the player's payouts and refunds go, as the entry registered it.
    #[serde(default)]
    pub lightning_address: Option<String>,
    /// The entry's Arkade escrow terms, from the payout policy the player signed.
    #[serde(default)]
    pub escrow: Option<EscrowTerms>,
    /// How the paying node paid it. None when the coordinator's test endpoint settled it, or for
    /// entries recorded before synth kept it.
    #[serde(default)]
    pub payment: Option<EntryPayment>,
    /// The coordinator's test endpoint settled the invoice; no Lightning payment was made.
    #[serde(default)]
    pub settled_by_test_endpoint: bool,
    #[serde(default)]
    pub paid: bool,
    #[serde(default)]
    pub entry_submitted: bool,
}

impl EntryTrace {
    pub fn new(user: &SynthUser) -> Self {
        Self {
            user: user.name.clone(),
            nostr_pubkey: user.nostr_pubkey_hex(),
            ..Self::default()
        }
    }

    /// The step, carrying this trace as its details.
    pub fn attach(&self, mut step: StepResult) -> StepResult {
        step.details = serde_json::to_value(self).ok();
        step
    }

    /// Whether the entry reached the point of paying without recording how it ended: a payment
    /// may have gone out that the step never saw.
    pub fn may_have_paid(&self) -> bool {
        !self.paid && !self.settled_by_test_endpoint && self.payment_hash.is_some()
    }
}

/// When an entry's Arkade escrow lets its player take it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscrowTerms {
    /// UNIX seconds. From then the refund leaf opens: the player, with the Arkade server's
    /// co-signature, can spend the escrow. The coordinator refunds through it.
    pub refund_at: i64,
    /// Seconds. The player alone can spend the escrow this long after unrolling it on-chain.
    pub solo_delay_secs: u32,
}

impl EscrowTerms {
    /// The terms fixed by an escrow's PSBT `TapTree` field, hex, as the payout policy carries it.
    pub fn from_tap_tree(tap_tree_hex: &str) -> Option<Self> {
        use coordinator_ark_escrow::{EntryEscrow, RelativeTimelock, VtxoScript};
        let bytes = hex::decode(tap_tree_hex).ok()?;
        let escrow =
            EntryEscrow::from_vtxo_script(&VtxoScript::decode_tap_tree(&bytes).ok()?).ok()?;
        let terms = escrow.terms();
        let RelativeTimelock::Seconds(solo_delay_secs) = terms.unilateral_refund_delay else {
            return None;
        };
        Some(Self {
            refund_at: i64::from(terms.refund_locktime.to_consensus_u32()),
            solo_delay_secs,
        })
    }
}

/// How the paying node paid an entry's invoice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryPayment {
    /// The paying node, by the name it gives itself.
    pub payer_alias: String,
    pub payer_pubkey: String,
    /// What the invoice's node revealed on settling, proving the payment.
    pub preimage: String,
    /// What the routing nodes took, on top of the amount.
    pub fee_msat: u64,
    /// The channels the payment went through, the payer's first.
    #[serde(default)]
    pub route: Vec<RouteHop>,
}

/// A channel a payment went through, and the node it led to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteHop {
    pub chan_id: String,
    pub pubkey: String,
    #[serde(default)]
    pub alias: Option<String>,
}

/// What the paying node says of an entry payment whose step never saw it finish: the run was
/// interrupted, or the step gave up while the payment was in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatePayment {
    pub user: String,
    pub payment_hash: String,
    /// The payment, if the node made it. None when the node has no record of paying it, which
    /// settles that nothing went out.
    #[serde(default)]
    pub payment: Option<EntryPayment>,
}

/// ark-swapd's swap of an entry's Lightning payment into the entry's Arkade escrow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapSeen {
    pub user: String,
    pub payment_hash: String,
    pub id: Uuid,
    pub state: String,
    /// What ark-swapd paid into the escrow.
    pub amount_sat: u64,
    pub escrow_address: String,
    /// The escrow's Arkade output, `txid:vout`.
    #[serde(default)]
    pub escrow_vtxo: Option<String>,
    #[serde(default)]
    pub ark_txid: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// The entry invoice's state on the node that issued it, when synth reaches that node:
    /// SETTLED means the player was charged.
    #[serde(default)]
    pub invoice_state: Option<String>,
    /// The escrow output as the Arkade server's indexer reports it.
    #[serde(default)]
    pub vtxo: Option<VtxoSeen>,
}

impl SwapSeen {
    /// Whether ark-swapd is done with it, so there is nothing more to look up. ark-swapd records a
    /// settled swap's escrow output after settling it, so a settled swap is final only once it has.
    pub fn is_final(&self) -> bool {
        match self.state.as_str() {
            "settled" => self.escrow_vtxo.is_some(),
            "expired" | "failed" | "unsettled" => true,
            _ => false,
        }
    }

    /// ark-swapd took the player's payment, or holds it, but records no escrow output it paid.
    pub fn funded_without_vtxo(&self) -> bool {
        self.escrow_vtxo.is_none() && matches!(self.state.as_str(), "escrow_paid" | "settled")
    }
}

/// An Arkade output, as the Arkade server's indexer reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VtxoSeen {
    pub amount_sat: u64,
    /// UNIX seconds. After this the server may sweep it.
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub spent: bool,
    #[serde(default)]
    pub swept: bool,
    /// The Arkade transaction that spent it.
    #[serde(default)]
    pub spent_by: Option<String>,
    /// The commitment transaction that settled it into a batch.
    #[serde(default)]
    pub settled_by: Option<String>,
}

/// An on-chain transaction, as the explorer reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainTx {
    pub txid: String,
    /// The output that matters here, for a funding transaction.
    #[serde(default)]
    pub vout: Option<u32>,
    #[serde(default)]
    pub fee_sat: Option<u64>,
    /// What all its outputs hold.
    #[serde(default)]
    pub value_sat: Option<u64>,
    #[serde(default)]
    pub confirmed: bool,
}

/// Whether a payout's invoice paid the node the player's Lightning Address is served by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayeeCheck {
    /// No payee is configured, so synth cannot tell who was paid.
    #[default]
    Unchecked,
    /// The invoice pays the configured payee's node.
    Matches,
    /// The payee's node reports the invoice settled.
    SettledOnPayee,
    /// The invoice pays some other node.
    OtherNode,
}

/// One player's payout: what the contract owes them, what the coordinator says it sent, and what
/// the nodes synth can reach saw of the payment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayoutSeen {
    pub user: String,
    pub entry_id: Uuid,
    /// Their entry key, which is how the contract names them.
    pub pubkey: String,
    /// Their share of the pot under the deciding outcome, in percent.
    pub weight: u64,
    pub owed_sats: u64,
    /// When the coordinator sent the payout, or finished it; it does not say which.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub sent_at: Option<OffsetDateTime>,
    /// The invoice the payout paid.
    #[serde(default)]
    pub invoice: Option<String>,
    #[serde(default)]
    pub amount_sats: Option<u64>,
    #[serde(default)]
    pub payment_hash: Option<String>,
    /// The node the invoice pays.
    #[serde(default)]
    pub payee_pubkey: Option<String>,
    #[serde(default)]
    pub payee_alias: Option<String>,
    /// The Lightning Address the entry registered for payouts.
    #[serde(default)]
    pub lightning_address: Option<String>,
    /// Found on a node synth can reach: the node that paid it, or the one it paid.
    #[serde(default)]
    pub preimage: Option<String>,
    #[serde(default)]
    pub fee_msat: Option<u64>,
    /// The node synth saw pay it, if it can reach that node.
    #[serde(default)]
    pub paid_by: Option<String>,
    #[serde(default)]
    pub payee: PayeeCheck,
}

impl PayoutSeen {
    /// The entries API's `paid_out_at` includes an initiated payment. Only a matching preimage
    /// observed on a Lightning node proves that the invoice settled, and only an invoice to the
    /// player's own node pays the player.
    pub fn is_confirmed(&self) -> bool {
        use sha2::{Digest, Sha256};
        let (Some(preimage), Some(hash)) = (&self.preimage, &self.payment_hash) else {
            return false;
        };
        let (Ok(preimage), Ok(hash)) = (hex::decode(preimage), hex::decode(hash)) else {
            return false;
        };
        preimage.len() == 32
            && hash.len() == 32
            && Sha256::digest(&preimage).as_slice() == hash.as_slice()
            && self
                .amount_sats
                .is_some_and(|amount| amount >= self.owed_sats)
            && self.payee != PayeeCheck::OtherNode
    }
}

/// Where one player's payout got to. Every view of a payout (the verdict, the hops, the flow)
/// reads it from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutState {
    /// The deciding outcome pays them nothing.
    OwedNothing,
    /// Settled, for at least their share, and not to another node as far as synth can check.
    Paid,
    /// The invoice pays a node other than the player's.
    OtherNode,
    /// The coordinator says it sent it, but no node synth reaches confirms it.
    SentUnconfirmed,
    /// Owed, and not sent yet.
    Owed,
    /// Owed, and the competition ended without sending it.
    NeverSent,
}

impl PayoutState {
    pub fn is_owed(self) -> bool {
        self != PayoutState::OwedNothing
    }
}

/// Each payout's state, in order. A settled invoice can account for only one payout, so a second
/// payout naming the same payment is not paid by it. `ended` is whether the competition failed or
/// was cancelled, so an unsent payout never will be.
pub fn payout_states(payouts: &[PayoutSeen], ended: bool) -> Vec<PayoutState> {
    let mut hashes = std::collections::HashSet::new();
    payouts
        .iter()
        .map(|payout| {
            if payout.owed_sats == 0 {
                PayoutState::OwedNothing
            } else if payout.payee == PayeeCheck::OtherNode {
                PayoutState::OtherNode
            } else if payout.is_confirmed()
                && hashes.insert(
                    payout
                        .payment_hash
                        .as_deref()
                        .unwrap_or_default()
                        .to_ascii_lowercase(),
                )
            {
                PayoutState::Paid
            } else if payout.sent_at.is_some() {
                PayoutState::SentUnconfirmed
            } else if ended {
                PayoutState::NeverSent
            } else {
                PayoutState::Owed
            }
        })
        .collect()
}

/// A refund of a ticket whose competition ended before its contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundSeen {
    pub user: String,
    pub ticket_id: Uuid,
    /// minted, submitting, submitted, paid, or settled.
    pub state: String,
    pub paid_sats: u64,
    /// The Arkade transaction that moved the escrow into the refund's swap.
    #[serde(default)]
    pub ark_txid: Option<String>,
    /// The invoice the refund pays, to the player's Lightning Address, and its hash. Coordinators
    /// that do not report them leave them out.
    #[serde(default)]
    pub invoice: Option<String>,
    #[serde(default)]
    pub payment_hash: Option<String>,
    /// Found on the node that paid it.
    #[serde(default)]
    pub preimage: Option<String>,
    #[serde(default)]
    pub fee_msat: Option<u64>,
    #[serde(default)]
    pub paid_by: Option<String>,
}

impl RefundSeen {
    pub fn is_settled(&self) -> bool {
        self.state == "settled"
    }
}

/// Where a run's money stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Money {
    /// Still moving; synth keeps looking.
    Following,
    /// Every winner was paid their share.
    PaidOut,
    /// Every paid entry got its money back.
    Refunded,
    /// Nobody paid anything, so there is nothing to follow.
    NothingPaid,
    /// It stopped with money owed, and is not moving. Synth keeps watching for it to move.
    Stuck {
        reason: String,
        /// When the money stopped: when the competition ended, or when synth found it stuck.
        #[serde(default = "OffsetDateTime::now_utc", with = "time::serde::rfc3339")]
        since: OffsetDateTime,
    },
    /// Synth stopped following it before it could verify where the money went. Does not fail
    /// the run: it says what synth could not see, not that money was lost.
    #[serde(alias = "timed_out")]
    Unverified { reason: String },
}

impl Money {
    /// Whether synth has stopped looking for news about it. Stuck money is watched until it
    /// moves, so it is not final.
    pub fn is_final(&self) -> bool {
        !matches!(self, Money::Following | Money::Stuck { .. })
    }

    /// Whether the money ended where it should.
    pub fn is_good(&self) -> bool {
        matches!(self, Money::PaidOut | Money::Refunded | Money::NothingPaid)
    }

    /// The name the database and the dashboard use.
    pub fn label(&self) -> &'static str {
        match self {
            Money::Following => "following",
            Money::PaidOut => "paid_out",
            Money::Refunded => "refunded",
            Money::NothingPaid => "nothing_paid",
            Money::Stuck { .. } => "stuck",
            Money::Unverified { .. } => "unverified",
        }
    }

    /// How people read it.
    pub fn words(&self) -> &'static str {
        label_words(self.label())
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Money::Stuck { reason, .. } | Money::Unverified { reason } => Some(reason),
            _ => None,
        }
    }
}

/// How people read a money label, as the database stores it.
pub fn label_words(label: &str) -> &'static str {
    match label {
        "following" => "following",
        "paid_out" => "paid out",
        "refunded" => "refunded",
        "nothing_paid" => "nothing paid",
        "stuck" => "stuck",
        // Labelled "timed_out" before synth told unverified money from stuck money.
        "unverified" | "timed_out" => "unverified",
        _ => "unknown",
    }
}

/// Money that was held somewhere it should not stay, and for how long. Kept once the money moves
/// on, so the trail still says where it sat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    #[serde(with = "time::serde::rfc3339")]
    pub since: OffsetDateTime,
    /// When synth first found it stuck, which is when it starts counting how long to watch it.
    #[serde(default = "OffsetDateTime::now_utc", with = "time::serde::rfc3339")]
    pub found: OffsetDateTime,
    /// Why it was stuck, as synth first said it.
    pub reason: String,
    /// What was held, in sats.
    pub sats: u64,
    /// UNIX seconds: the soonest an escrow holding it expires or its refund leaf opens, if known.
    #[serde(default)]
    pub nearest_expiry: Option<i64>,
    /// When synth saw it move on, and how.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub until: Option<OffsetDateTime>,
    #[serde(default)]
    pub then: Option<String>,
}

/// What the tracker last found about a run's money.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trail {
    #[serde(with = "time::serde::rfc3339")]
    pub refreshed_at: OffsetDateTime,
    pub competition_id: Uuid,
    /// The competition as the coordinator last reported it, without its contract and
    /// signatures. None if the coordinator never answered for it.
    #[serde(default)]
    pub competition: Option<CompetitionResponse>,
    #[serde(default)]
    pub settlement: Option<Settlement>,
    #[serde(default)]
    pub swaps: Vec<SwapSeen>,
    /// What the paying node says of entry payments their steps lost track of.
    #[serde(default)]
    pub late_payments: Vec<LatePayment>,
    #[serde(default)]
    pub payouts: Vec<PayoutSeen>,
    #[serde(default)]
    pub refunds: Vec<RefundSeen>,
    #[serde(default)]
    pub funding_tx: Option<ChainTx>,
    #[serde(default)]
    pub outcome_tx: Option<ChainTx>,
    /// What spent the contract's outputs after the outcome, or its funding at expiry: where a
    /// pot that was not paid out went.
    #[serde(default)]
    pub closing_txs: Vec<ChainTx>,
    pub money: Money,
    /// Money that was stuck, kept after it moves on.
    #[serde(default)]
    pub held: Option<Held>,
    /// What could not be looked up, and why.
    #[serde(default)]
    pub gaps: Vec<String>,
}

impl Trail {
    /// The entries that paid: those whose steps say so, and those found paid afterwards.
    pub fn paid_entries<'a>(&self, entries: &'a [EntryTrace]) -> Vec<&'a EntryTrace> {
        entries
            .iter()
            .filter(|entry| entry.paid || self.late_payment(entry).is_some())
            .collect()
    }

    /// The payment the paying node made for an entry whose step did not see it finish.
    pub fn late_payment(&self, entry: &EntryTrace) -> Option<&EntryPayment> {
        let hash = entry.payment_hash.as_ref()?;
        self.late_payments
            .iter()
            .find(|late| &late.payment_hash == hash)?
            .payment
            .as_ref()
    }

    pub fn swap_of(&self, entry: &EntryTrace) -> Option<&SwapSeen> {
        let hash = entry.payment_hash.as_ref()?;
        self.swaps.iter().find(|swap| &swap.payment_hash == hash)
    }

    pub fn refund_of(&self, entry: &EntryTrace) -> Option<&RefundSeen> {
        let ticket = entry.ticket_id?;
        self.refunds
            .iter()
            .find(|refund| refund.ticket_id == ticket)
    }

    /// Whether the competition failed or was cancelled.
    pub fn ended(&self) -> bool {
        self.competition.as_ref().is_some_and(ended)
    }
}

/// Whether a competition failed or was cancelled.
pub fn ended(competition: &CompetitionResponse) -> bool {
    competition.failed_at.is_some() || competition.cancelled_at.is_some()
}

/// Whether a competition's contract was funded, so its money left the escrows.
pub fn contracted(competition: &CompetitionResponse) -> bool {
    competition.outcome_broadcasted_at.is_some() || competition.funding_broadcasted_at.is_some()
}

/// What the verdict is drawn from.
#[derive(Clone, Copy)]
pub struct Evidence<'a> {
    /// The run's steps are still going.
    pub running: bool,
    /// None if the coordinator never answered for the competition.
    pub competition: Option<&'a CompetitionResponse>,
    /// Whether an outcome (the oracle's, or the contract's expiry) decided the payouts.
    pub decided: bool,
    pub payouts: &'a [PayoutSeen],
    pub refunds: &'a [RefundSeen],
    /// Entries that paid, by their steps or found paid on the paying node afterwards.
    pub paid_entries: usize,
    /// Entries that reached paying and whose payment synth could not settle either way.
    pub unverified_entries: usize,
    /// Synth has followed the run as long as it follows unsettled money.
    pub give_up: bool,
    pub now: OffsetDateTime,
}

/// Where the money stands, from what the competition says and what synth found.
pub fn judge(evidence: &Evidence) -> Money {
    let Evidence {
        running,
        competition,
        decided,
        payouts,
        refunds,
        paid_entries,
        unverified_entries,
        give_up,
        now,
    } = *evidence;
    // A run still going decides nothing yet, even before anyone has paid.
    if running {
        return Money::Following;
    }
    if unverified_entries > 0 {
        return if give_up {
            Money::Unverified {
                reason: format!(
                    "{unverified_entries} entry payment(s) could not be verified either way"
                ),
            }
        } else {
            Money::Following
        };
    }
    if paid_entries == 0 {
        return Money::NothingPaid;
    }
    let Some(competition) = competition else {
        return if give_up {
            Money::Unverified {
                reason: "the coordinator never answered for the competition".into(),
            }
        } else {
            Money::Following
        };
    };
    let is_ended = ended(competition);
    let died = |at: OffsetDateTime| {
        format!(
            "the competition {} at {} UTC",
            if competition.failed_at.is_some() {
                "failed"
            } else {
                "was cancelled"
            },
            clock(at)
        )
    };

    if competition.completed_at.is_some() || (is_ended && contracted(competition)) {
        let states = payout_states(payouts, is_ended);
        let count = |wanted: PayoutState| states.iter().filter(|state| **state == wanted).count();
        let owed = states.iter().filter(|state| state.is_owed()).count();
        let paid = count(PayoutState::Paid);
        let unsent = count(PayoutState::Owed) + count(PayoutState::NeverSent);
        let in_flight = count(PayoutState::SentUnconfirmed);
        let other_node = count(PayoutState::OtherNode);
        let confirmed = format!("{paid} of {owed} payouts confirmed");
        let complete = payouts.len() >= paid_entries && owed > 0;
        if complete && paid == owed {
            return Money::PaidOut;
        }
        let stopped = competition
            .failed_at
            .or(competition.cancelled_at)
            .or(competition.completed_at)
            .unwrap_or(now);
        if other_node > 0 {
            return Money::Stuck {
                reason: format!(
                    "{other_node} payout(s) paid a node other than the player's, with {confirmed}"
                ),
                since: stopped,
            };
        }
        // Nothing in flight can still settle, so money owed and unsent is stuck now.
        if is_ended && decided && complete && in_flight == 0 && unsent > 0 {
            let after = if competition.outcome_broadcasted_at.is_some() {
                "its outcome went on-chain"
            } else {
                "its funding went out"
            };
            return Money::Stuck {
                reason: if paid == 0 {
                    format!("{} after {after}, and no payout was sent", died(stopped))
                } else {
                    format!(
                        "{} after {after}, with {unsent} owed payout(s) never sent",
                        died(stopped)
                    )
                },
                since: stopped,
            };
        }
        if !give_up {
            return Money::Following;
        }
        if is_ended && !decided {
            return Money::Stuck {
                reason: format!(
                    "{} after its funding went out, before an outcome decided the payouts",
                    died(stopped)
                ),
                since: stopped,
            };
        }
        if !complete {
            return Money::Unverified {
                reason: "payout records are incomplete; settlement could not be verified".into(),
            };
        }
        return if unsent > 0 {
            Money::Stuck {
                reason: format!(
                    "the competition completed with {unsent} owed payout(s) never sent, and \
                     {confirmed}"
                ),
                since: stopped,
            }
        } else {
            Money::Unverified {
                reason: format!(
                    "every owed payout was sent, but {confirmed}; the payee's node may be out \
                     of synth's reach"
                ),
            }
        };
    }

    if let Some(at) = competition.cancelled_at.or(competition.failed_at) {
        let settled = refunds.iter().filter(|refund| refund.is_settled()).count();
        if settled >= paid_entries {
            return Money::Refunded;
        }
        return if give_up {
            Money::Stuck {
                reason: format!(
                    "{} before its contract, and {settled} of {paid_entries} escrows were refunded",
                    died(at)
                ),
                since: at,
            }
        } else {
            Money::Following
        };
    }
    if give_up {
        let states = payout_states(payouts, false);
        let owed = states.iter().filter(|state| state.is_owed()).count();
        let paid = states.iter().filter(|s| **s == PayoutState::Paid).count();
        Money::Unverified {
            reason: format!(
                "synth stopped following it at {}, with {paid} of {owed} payouts confirmed",
                competition
                    .state
                    .as_deref()
                    .unwrap_or(competition.inferred_status()),
            ),
        }
    } else {
        Money::Following
    }
}

fn clock(at: OffsetDateTime) -> String {
    format!("{:02}:{:02}", at.hour(), at.minute())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn competition(json: serde_json::Value) -> CompetitionResponse {
        let mut base = serde_json::json!({
            "id": Uuid::now_v7(),
            "created_at": "2026-09-24T01:08:35Z",
            "event_submission": {},
        });
        base.as_object_mut()
            .unwrap()
            .extend(json.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn payout(owed_sats: u64, sent: bool) -> PayoutSeen {
        PayoutSeen {
            owed_sats,
            sent_at: sent.then(OffsetDateTime::now_utc),
            amount_sats: sent.then_some(owed_sats),
            preimage: sent
                .then(|| "1111111111111111111111111111111111111111111111111111111111111111".into()),
            payment_hash: sent
                .then(|| "02d449a31fbb267c8f352e9968a79e3e5fc95c1bbeaa502fd6454ebde5a4bedc".into()),
            ..PayoutSeen::default()
        }
    }

    fn judge_of(
        competition: &CompetitionResponse,
        payouts: &[PayoutSeen],
        refunds: &[RefundSeen],
        paid_entries: usize,
        give_up: bool,
    ) -> Money {
        judge(&Evidence {
            running: false,
            competition: Some(competition),
            decided: payouts.iter().any(|payout| payout.owed_sats > 0),
            payouts,
            refunds,
            paid_entries,
            unverified_entries: 0,
            give_up,
            now: OffsetDateTime::now_utc(),
        })
    }

    fn failed_after_outcome() -> CompetitionResponse {
        competition(serde_json::json!({
            "funding_broadcasted_at": "2026-09-24T01:11:26Z",
            "outcome_broadcasted_at": "2026-09-24T01:36:37Z",
            "delta_broadcasted_at": "2026-09-24T04:15:12Z",
            "failed_at": "2026-09-24T04:16:13Z",
            "cancelled_at": "2026-09-24T05:16:13Z",
        }))
    }

    /// Run 01a0d0f5: the outcome went on-chain, no payout was ever sent, and the competition
    /// failed once its delta expired. Nothing is in flight, so it is stuck at once.
    #[test]
    fn a_contract_that_died_without_sending_a_payout_is_stuck_at_once() {
        let payouts = [payout(1020, false), payout(990, false), payout(990, false)];
        assert_eq!(
            judge_of(&failed_after_outcome(), &payouts, &[], 3, false),
            Money::Stuck {
                reason: "the competition failed at 04:16 UTC after its outcome went on-chain, \
                         and no payout was sent"
                    .into(),
                since: datetime!(2026-09-24 04:16:13 UTC),
            }
        );
    }

    /// A payout that was sent but is not confirmed yet may still settle: keep following it until
    /// the timeout, then say it could not be verified rather than that it is stuck.
    #[test]
    fn a_payout_in_flight_when_the_competition_died_is_followed_until_the_timeout() {
        let mut in_flight = payout(1020, true);
        in_flight.preimage = None;
        let payouts = [in_flight, payout(990, true), payout(990, true)];
        let failed = failed_after_outcome();
        assert_eq!(judge_of(&failed, &payouts, &[], 3, false), Money::Following);
        assert!(matches!(
            judge_of(&failed, &payouts, &[], 3, true),
            Money::Unverified { .. }
        ));
        let mut unsent = payouts.clone();
        unsent[2] = payout(990, false);
        assert_eq!(judge_of(&failed, &unsent, &[], 3, false), Money::Following);
        assert!(
            matches!(
                judge_of(&failed, &unsent, &[], 3, true),
                Money::Stuck { .. }
            ),
            "a payout never sent is stuck once synth stops waiting for the rest"
        );
    }

    /// A contract that died before any outcome decided its payouts holds the pot on-chain.
    #[test]
    fn a_contract_that_died_before_its_outcome_is_stuck_once_synth_gives_up() {
        let failed = competition(serde_json::json!({
            "funding_broadcasted_at": "2026-09-24T01:11:26Z",
            "failed_at": "2026-09-24T04:16:13Z",
        }));
        let undecided = [payout(0, false), payout(0, false)];
        assert_eq!(
            judge_of(&failed, &undecided, &[], 2, false),
            Money::Following
        );
        assert!(matches!(
            judge_of(&failed, &undecided, &[], 2, true),
            Money::Stuck { reason, .. } if reason.contains("before an outcome")
        ));
    }

    #[test]
    fn a_completed_competition_that_paid_every_winner_is_paid_out() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-24T01:02:31Z" }));
        let payouts = [payout(3000, true), payout(0, false)];
        assert_eq!(
            judge_of(&completed, &payouts, &[], 2, false),
            Money::PaidOut
        );
        let short = [payout(1020, true), payout(990, false)];
        assert_eq!(
            judge_of(&completed, &short, &[], 2, false),
            Money::Following
        );
        assert!(matches!(
            judge_of(&completed, &short, &[], 2, true),
            Money::Stuck { .. }
        ));
    }

    #[test]
    fn missing_records_and_initiated_payments_are_not_paid_out() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-24T01:02:31Z" }));
        assert_eq!(judge_of(&completed, &[], &[], 2, false), Money::Following);
        assert!(matches!(
            judge_of(&completed, &[], &[], 2, true),
            Money::Unverified { .. }
        ));
        let mut winner = payout(3000, true);
        assert_eq!(
            judge_of(&completed, &[winner.clone()], &[], 2, false),
            Money::Following
        );
        winner.preimage = None;
        assert_eq!(
            judge_of(&completed, &[winner.clone()], &[], 1, false),
            Money::Following
        );
        winner.preimage = Some("00".repeat(32));
        assert!(
            !winner.is_confirmed(),
            "a preimage for a different invoice proves nothing"
        );
        let mut winner = payout(3000, true);
        winner.amount_sats = Some(2999);
        assert!(
            !winner.is_confirmed(),
            "an underpayment does not pay the share"
        );
    }

    /// A settled invoice to someone else's node does not pay the player.
    #[test]
    fn a_payout_to_another_node_is_not_paid_out() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-24T01:02:31Z" }));
        let mut elsewhere = payout(3000, true);
        elsewhere.payee = PayeeCheck::OtherNode;
        assert!(!elsewhere.is_confirmed());
        assert_eq!(
            payout_states(std::slice::from_ref(&elsewhere), false),
            [PayoutState::OtherNode]
        );
        assert!(matches!(
            judge_of(&completed, &[elsewhere], &[], 1, false),
            Money::Stuck { .. }
        ));
    }

    #[test]
    fn one_payment_cannot_settle_two_entry_shares() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-24T01:02:31Z" }));
        let first = payout(1000, true);
        let mut duplicate = first.clone();
        duplicate.entry_id = Uuid::now_v7();
        let payouts = [first, duplicate];
        assert_eq!(
            payout_states(&payouts, false),
            [PayoutState::Paid, PayoutState::SentUnconfirmed]
        );
        assert_eq!(
            judge_of(&completed, &payouts, &[], 2, false),
            Money::Following
        );
        assert!(matches!(
            judge_of(&completed, &payouts, &[], 2, true),
            Money::Unverified { .. }
        ));
    }

    #[test]
    fn a_competition_awaiting_its_oracle_is_followed_until_synth_gives_up() {
        let waiting = competition(serde_json::json!({
            "funding_broadcasted_at": "2026-09-24T01:11:26Z",
            "state": "outcome_broadcasted",
        }));
        let payouts = [payout(3000, false)];
        assert_eq!(
            judge_of(&waiting, &payouts, &[], 3, false),
            Money::Following
        );
        assert_eq!(
            judge_of(&waiting, &payouts, &[], 3, true),
            Money::Unverified {
                reason:
                    "synth stopped following it at outcome_broadcasted, with 0 of 1 payouts confirmed"
                        .into()
            }
        );
    }

    fn refund(state: &str) -> RefundSeen {
        RefundSeen {
            user: "alice".into(),
            ticket_id: Uuid::now_v7(),
            state: state.into(),
            paid_sats: 1100,
            ark_txid: None,
            invoice: None,
            payment_hash: None,
            preimage: None,
            fee_msat: None,
            paid_by: None,
        }
    }

    #[test]
    fn a_cancelled_competition_settles_once_every_paid_entry_is_refunded() {
        let cancelled = competition(serde_json::json!({ "cancelled_at": "2026-09-24T03:00:00Z" }));
        let refunds = [refund("settled"), refund("submitted")];
        assert_eq!(
            judge_of(&cancelled, &[], &refunds, 2, false),
            Money::Following
        );
        let stuck = judge_of(&cancelled, &[], &refunds, 2, true);
        assert!(
            matches!(&stuck, Money::Stuck { since, .. } if *since == datetime!(2026-09-24 03:00:00 UTC)),
            "held since the cancellation: {stuck:?}"
        );
        let refunds = [refund("settled"), refund("settled")];
        assert_eq!(
            judge_of(&cancelled, &[], &refunds, 2, false),
            Money::Refunded
        );
    }

    #[test]
    fn a_run_that_paid_nothing_has_nothing_to_follow() {
        let created = competition(serde_json::json!({}));
        assert_eq!(judge_of(&created, &[], &[], 0, false), Money::NothingPaid);
    }

    #[test]
    fn a_running_run_or_an_unverified_entry_decides_nothing_yet() {
        let created = competition(serde_json::json!({}));
        let mut evidence = Evidence {
            running: true,
            competition: Some(&created),
            decided: false,
            payouts: &[],
            refunds: &[],
            paid_entries: 0,
            unverified_entries: 1,
            give_up: true,
            now: OffsetDateTime::now_utc(),
        };
        assert_eq!(judge(&evidence), Money::Following);
        evidence.running = false;
        assert!(matches!(judge(&evidence), Money::Unverified { .. }));
        evidence.give_up = false;
        assert_eq!(judge(&evidence), Money::Following);
        evidence.competition = None;
        evidence.unverified_entries = 0;
        evidence.paid_entries = 1;
        assert_eq!(judge(&evidence), Money::Following);
        evidence.give_up = true;
        assert!(matches!(judge(&evidence), Money::Unverified { .. }));
    }

    /// ark-swapd fills in a settled swap's escrow output after settling it.
    #[test]
    fn a_settled_swap_is_looked_up_until_it_names_its_escrow_output() {
        let mut swap = SwapSeen {
            user: "alice".into(),
            payment_hash: "ab".repeat(32),
            id: Uuid::now_v7(),
            state: "settled".into(),
            amount_sat: 1100,
            escrow_address: "tark1escrow".into(),
            escrow_vtxo: None,
            ark_txid: None,
            error: None,
            invoice_state: None,
            vtxo: None,
        };
        assert!(!swap.is_final());
        assert!(swap.funded_without_vtxo());
        swap.escrow_vtxo = Some("ee:0".into());
        assert!(swap.is_final());
        assert!(!swap.funded_without_vtxo());
    }

    /// Money labelled before synth told unverified money from stuck money still reads.
    #[test]
    fn an_older_timed_out_verdict_reads_as_unverified() {
        let money: Money =
            serde_json::from_value(serde_json::json!({"status": "timed_out", "reason": "x"}))
                .unwrap();
        assert_eq!(money.label(), "unverified");
        assert_eq!(label_words("timed_out"), "unverified");
        let stuck: Money =
            serde_json::from_value(serde_json::json!({"status": "stuck", "reason": "x"})).unwrap();
        assert!(matches!(stuck, Money::Stuck { .. }));
        assert!(!stuck.is_final(), "stuck money is watched until it moves");
    }

    /// Steps saved before entries kept their invoice and payment still read.
    #[test]
    fn an_older_entry_step_still_reads() {
        let trace: EntryTrace = serde_json::from_value(serde_json::json!({
            "user": "alice",
            "nostr_pubkey": "00",
            "entry_id": null,
            "ticket_id": null,
            "amount_sats": 1100,
            "payment_hash": "ab",
            "paid": true,
            "entry_submitted": false,
        }))
        .unwrap();
        assert!(trace.paid && trace.payment.is_none() && trace.invoice.is_none());
        assert!(!trace.settled_by_test_endpoint && trace.escrow.is_none());
    }
}
