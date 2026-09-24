//! Where a run's money went, followed past the run's own steps.
//!
//! A lifecycle run ends once its competition awaits the oracle, but its money keeps moving for
//! hours: the oracle attests, the outcome goes on-chain, the coordinator pays the winners over
//! Lightning. Refunds of a cancelled competition come later still. The [`tracker`] keeps looking
//! at each run's competition until its money settles one way or another, and saves what it found
//! as the run's [`Trail`], which the run's page is drawn from.

pub mod tracker;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::client::competitions::CompetitionResponse;
use crate::crypto::keys::SynthUser;
use crate::scenarios::StepResult;
use crate::settlement::Settlement;

/// Where an entry's money went, recorded by the scenario as it happens so a failed entry still
/// shows how far its payment got. Saved as the details of the entry's step.
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
    /// How the paying node paid it. None when the coordinator's test endpoint settled it.
    #[serde(default)]
    pub payment: Option<EntryPayment>,
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
}

impl SwapSeen {
    /// Whether ark-swapd is done with it, so there is nothing more to look up.
    pub fn is_final(&self) -> bool {
        matches!(
            self.state.as_str(),
            "settled" | "expired" | "failed" | "unsettled"
        )
    }
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
    #[serde(default)]
    pub confirmed: bool,
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
    /// The Lightning Address the invoice came from, if the player registered one.
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
}

impl PayoutSeen {
    /// The entries API's `paid_out_at` includes an initiated payment. Only a matching
    /// preimage observed on a Lightning node proves that the invoice settled.
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
    }
}

/// A settled Lightning invoice can account for only one outgoing payment.
pub fn confirmed_payouts(payouts: &[PayoutSeen]) -> Vec<&PayoutSeen> {
    let mut hashes = std::collections::HashSet::new();
    payouts
        .iter()
        .filter(|payout| payout.is_confirmed())
        .filter(|payout| {
            hashes.insert(payout.payment_hash.as_deref().unwrap().to_ascii_lowercase())
        })
        .collect()
}

/// A refund of a ticket whose competition was cancelled before its contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundSeen {
    pub user: String,
    pub ticket_id: Uuid,
    /// minted, submitting, submitted, paid, or settled.
    pub state: String,
    pub paid_sats: u64,
    #[serde(default)]
    pub ark_txid: Option<String>,
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
    /// It stopped with money owed.
    Stuck { reason: String },
    /// Synth stopped looking before it settled.
    TimedOut { reason: String },
}

impl Money {
    pub fn is_settled(&self) -> bool {
        !matches!(self, Money::Following)
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
            Money::TimedOut { .. } => "timed_out",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Money::Stuck { reason } | Money::TimedOut { reason } => Some(reason),
            _ => None,
        }
    }
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
    #[serde(default)]
    pub payouts: Vec<PayoutSeen>,
    #[serde(default)]
    pub refunds: Vec<RefundSeen>,
    #[serde(default)]
    pub funding_tx: Option<ChainTx>,
    #[serde(default)]
    pub outcome_tx: Option<ChainTx>,
    pub money: Money,
    /// What could not be looked up, and why.
    #[serde(default)]
    pub gaps: Vec<String>,
}

/// Where the money stands, from what the competition says and what synth found.
///
/// `paid_entries` counts the entries the run paid for; `give_up` is whether synth has followed
/// the run as long as it will.
pub fn judge(
    competition: &CompetitionResponse,
    payouts: &[PayoutSeen],
    refunds: &[RefundSeen],
    paid_entries: usize,
    give_up: bool,
) -> Money {
    let owed: Vec<&PayoutSeen> = payouts.iter().filter(|p| p.owed_sats > 0).collect();
    let sent = confirmed_payouts(payouts)
        .iter()
        .filter(|p| p.owed_sats > 0)
        .count();
    let payouts_words = || format!("{sent} of {} payouts confirmed", owed.len());
    if paid_entries == 0 {
        return Money::NothingPaid;
    }
    if competition.completed_at.is_some() {
        // Missing entry lookups or an unavailable settlement must not make an empty
        // set of payouts count as success. Retry incomplete evidence until timeout.
        if payouts.len() < paid_entries || owed.is_empty() {
            return if give_up {
                Money::TimedOut {
                    reason: "payout records are incomplete; settlement could not be verified"
                        .into(),
                }
            } else {
                Money::Following
            };
        }
        return if sent == owed.len() {
            Money::PaidOut
        } else if give_up {
            Money::TimedOut {
                reason: format!("the competition completed with {}", payouts_words()),
            }
        } else {
            Money::Following
        };
    }
    let ended = competition.failed_at.or(competition.cancelled_at);
    let contracted = competition.outcome_broadcasted_at.is_some()
        || competition.funding_broadcasted_at.is_some();
    match ended {
        Some(_)
            if contracted
                && payouts.len() >= paid_entries
                && !owed.is_empty()
                && sent == owed.len() =>
        {
            Money::PaidOut
        }
        Some(at) if contracted => Money::Stuck {
            reason: format!(
                "the competition {} at {} UTC after {}, with {}",
                if competition.failed_at.is_some() {
                    "failed"
                } else {
                    "was cancelled"
                },
                clock(at),
                if competition.outcome_broadcasted_at.is_some() {
                    "its outcome went on-chain"
                } else {
                    "its funding went out"
                },
                payouts_words()
            ),
        },
        Some(_) => {
            let settled = refunds.iter().filter(|r| r.state == "settled").count();
            if settled >= paid_entries {
                Money::Refunded
            } else if give_up {
                Money::Stuck {
                    reason: format!(
                        "the competition was cancelled and {settled} of {paid_entries} refunds settled"
                    ),
                }
            } else {
                Money::Following
            }
        }
        None if give_up => Money::TimedOut {
            reason: format!(
                "synth stopped following it at {}, with {}",
                competition
                    .state
                    .as_deref()
                    .unwrap_or(competition.inferred_status()),
                payouts_words()
            ),
        },
        None => Money::Following,
    }
}

fn clock(at: OffsetDateTime) -> String {
    format!("{:02}:{:02}", at.hour(), at.minute())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Run 01a0d0f5: the outcome went on-chain, no payout was ever sent, and the competition
    /// failed once its delta expired. The run had passed.
    #[test]
    fn a_competition_that_failed_after_its_outcome_left_its_winners_unpaid() {
        let failed = competition(serde_json::json!({
            "funding_broadcasted_at": "2026-09-24T01:11:26Z",
            "outcome_broadcasted_at": "2026-09-24T01:36:37Z",
            "delta_broadcasted_at": "2026-09-24T04:15:12Z",
            "failed_at": "2026-09-24T04:16:13Z",
            "cancelled_at": "2026-09-24T05:16:13Z",
        }));
        let payouts = [payout(1020, false), payout(990, false), payout(990, false)];
        assert_eq!(
            judge(&failed, &payouts, &[], 3, false),
            Money::Stuck {
                reason: "the competition failed at 04:16 UTC after its outcome went on-chain, \
                         with 0 of 3 payouts confirmed"
                    .into()
            }
        );
    }

    #[test]
    fn a_completed_competition_that_paid_every_winner_is_paid_out() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-24T01:02:31Z" }));
        let payouts = [payout(3000, true), payout(0, false)];
        assert_eq!(judge(&completed, &payouts, &[], 2, false), Money::PaidOut);
        let short = [payout(1020, true), payout(990, false)];
        assert!(matches!(
            judge(&completed, &short, &[], 2, true),
            Money::TimedOut { .. }
        ));
    }

    #[test]
    fn missing_records_and_initiated_payments_are_not_paid_out() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-24T01:02:31Z" }));
        assert_eq!(judge(&completed, &[], &[], 2, false), Money::Following);
        assert!(matches!(
            judge(&completed, &[], &[], 2, true),
            Money::TimedOut { .. }
        ));
        let mut winner = payout(3000, true);
        assert_eq!(
            judge(&completed, &[winner.clone()], &[], 2, false),
            Money::Following
        );
        winner.preimage = None;
        assert_eq!(
            judge(&completed, &[winner.clone()], &[], 1, false),
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

    #[test]
    fn one_payment_cannot_settle_two_entry_shares() {
        let completed = competition(serde_json::json!({ "completed_at": "2026-09-24T01:02:31Z" }));
        let first = payout(1000, true);
        let mut duplicate = first.clone();
        duplicate.entry_id = Uuid::now_v7();
        let payouts = [first, duplicate];
        assert_eq!(confirmed_payouts(&payouts).len(), 1);
        assert_eq!(judge(&completed, &payouts, &[], 2, false), Money::Following);
        assert!(matches!(
            judge(&completed, &payouts, &[], 2, true),
            Money::TimedOut { .. }
        ));
    }

    #[test]
    fn a_competition_awaiting_its_oracle_is_followed_until_synth_gives_up() {
        let waiting = competition(serde_json::json!({
            "funding_broadcasted_at": "2026-09-24T01:11:26Z",
            "state": "outcome_broadcasted",
        }));
        let payouts = [payout(3000, false)];
        assert_eq!(judge(&waiting, &payouts, &[], 3, false), Money::Following);
        assert_eq!(
            judge(&waiting, &payouts, &[], 3, true),
            Money::TimedOut {
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
        }
    }

    #[test]
    fn a_cancelled_competition_settles_once_every_paid_entry_is_refunded() {
        let cancelled = competition(serde_json::json!({ "cancelled_at": "2026-09-24T03:00:00Z" }));
        let refunds = [refund("settled"), refund("submitted")];
        assert_eq!(judge(&cancelled, &[], &refunds, 2, false), Money::Following);
        assert!(matches!(
            judge(&cancelled, &[], &refunds, 2, true),
            Money::Stuck { .. }
        ));
        let refunds = [refund("settled"), refund("settled")];
        assert_eq!(judge(&cancelled, &[], &refunds, 2, false), Money::Refunded);
    }

    #[test]
    fn a_run_that_paid_nothing_has_nothing_to_follow() {
        let created = competition(serde_json::json!({}));
        assert_eq!(judge(&created, &[], &[], 0, false), Money::NothingPaid);
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
    }
}
