//! Where an entry's money is, what can be done about it now, and when the next step opens.

use std::fmt;

use bitcoin::absolute::LockTime;
use bitcoin::{OutPoint, Txid};
use dlctix::secp::MaybeScalar;
use dlctix::Outcome;
use serde::Serialize;
use uuid::Uuid;

use crate::chain::ChainView;
use crate::contract::EntryContract;
use crate::escrow::Escrow;
use crate::spec::EntryRecord;

/// Something the player can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Broadcast the outcome transaction the attestation unlocks.
    BroadcastOutcome,
    /// Broadcast the expiry transaction (equal shares), the event having expired unattested.
    BroadcastExpiry,
    /// Broadcast the split transaction with this player's ticket preimage.
    BroadcastSplit,
    /// Move this player's split output to their own address.
    ClaimWin,
    /// Take the escrow back through Arkade (refund leaf, with the server).
    RefundEscrow,
    /// Take back an escrow whose VTXO expired, in an Arkade recovery batch.
    RecoverExpiredEscrow,
    /// Put the escrow on chain without the Arkade server, then spend it alone.
    Unroll,
}

impl Action {
    /// The CLI command that does this.
    pub fn command(self) -> &'static str {
        match self {
            Action::BroadcastOutcome
            | Action::BroadcastExpiry
            | Action::BroadcastSplit
            | Action::ClaimWin => "claim",
            Action::RefundEscrow | Action::RecoverExpiredEscrow => "refund-escrow",
            Action::Unroll => "unroll",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Action::BroadcastOutcome => "broadcast the outcome transaction",
            Action::BroadcastExpiry => "broadcast the expiry transaction",
            Action::BroadcastSplit => "broadcast the split transaction",
            Action::ClaimWin => "claim this player's split output",
            Action::RefundEscrow => "refund the escrow through Arkade",
            Action::RecoverExpiredEscrow => "recover the expired escrow in an Arkade batch",
            Action::Unroll => "unroll the escrow on chain",
        })
    }
}

/// When an action opens: a block height, a time, or both unknown until something confirms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Opens {
    pub action: Action,
    pub at_height: Option<u32>,
    /// Unix seconds.
    pub at_time: Option<u64>,
    /// Blocks still to be mined before `at_height` opens, once the tip is known.
    pub blocks_left: Option<u32>,
    /// About when `at_height` opens, unix seconds, once the block interval is known.
    pub eta: Option<u64>,
}

impl Opens {
    /// Opens with block `height`.
    fn at_height(action: Action, height: u32, chain: &ChainView, now: u64) -> Self {
        let blocks_left = chain.blocks_until(height);
        Self {
            action,
            at_height: Some(height),
            at_time: None,
            blocks_left,
            eta: blocks_left
                .and_then(|blocks| chain.time_for(blocks))
                .map(|seconds| now + seconds),
        }
    }

    /// Opens once the chain's median time passes `time`.
    fn at_time(action: Action, time: u64) -> Self {
        Self {
            action,
            at_height: None,
            at_time: Some(time),
            blocks_left: None,
            eta: None,
        }
    }

    /// Opens some blocks after a transaction that has not confirmed yet does.
    fn after_confirmation(action: Action) -> Self {
        Self {
            action,
            at_height: None,
            at_time: None,
            blocks_left: None,
            eta: None,
        }
    }
}

/// Where an entry's money is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Location {
    /// Nothing of this entry is on chain or in an escrow.
    NoFunds,
    /// The Arkade escrow VTXO.
    Escrow {
        outpoint: OutPoint,
        amount_sat: u64,
    },
    /// The escrow was spent: into the competition's funding at kickoff, or refunded.
    EscrowSpent {
        by: Option<Txid>,
    },
    /// The contract's funding output, unspent.
    Funding {
        outpoint: OutPoint,
        amount_sat: u64,
        confirmed: bool,
    },
    /// An outcome transaction's output, before the split.
    Outcome {
        txid: Txid,
        outcome: String,
        confirmed_height: Option<u32>,
    },
    /// This player's split output.
    Split {
        outpoint: OutPoint,
        amount_sat: u64,
        confirmed_height: Option<u32>,
    },
    /// This player's split output was spent.
    Spent {
        outpoint: OutPoint,
        by: Txid,
        confirmed_height: Option<u32>,
    },
    /// The outcome that happened pays this player nothing.
    Lost {
        outcome: String,
    },
    /// Something other than this contract's transactions spent the money.
    SpentElsewhere {
        outpoint: OutPoint,
        by: Txid,
    },
    Unknown {
        reason: String,
    },
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::NoFunds => write!(
                f,
                "nothing on chain: no escrow and no contract was recorded for this entry"
            ),
            Location::Escrow {
                outpoint,
                amount_sat,
            } => write!(f, "{amount_sat} sats in the Arkade escrow VTXO {outpoint}"),
            Location::EscrowSpent { by: Some(by) } => write!(f, "the escrow was spent by {by}"),
            Location::EscrowSpent { by: None } => write!(f, "the escrow was spent"),
            Location::Funding {
                outpoint,
                amount_sat,
                confirmed,
            } => write!(
                f,
                "the contract's {amount_sat} sat funding output {outpoint}{}",
                if *confirmed { "" } else { " (unconfirmed)" }
            ),
            Location::Outcome {
                txid,
                outcome,
                confirmed_height,
            } => write!(
                f,
                "the {outcome} outcome transaction {txid}'s output, {}",
                confirmed(*confirmed_height)
            ),
            Location::Split {
                outpoint,
                amount_sat,
                confirmed_height,
            } => write!(
                f,
                "{amount_sat} sats in this player's split output {outpoint}, {}",
                confirmed(*confirmed_height)
            ),
            Location::Spent {
                outpoint,
                by,
                confirmed_height,
            } => write!(
                f,
                "this player's split output {outpoint} was spent by {by}, {}: this player's \
                 claim if they sent it, otherwise the market maker's reclaim",
                confirmed(*confirmed_height)
            ),
            Location::Lost { outcome } => {
                write!(f, "the {outcome} outcome pays this player nothing")
            }
            Location::SpentElsewhere { outpoint, by } => write!(
                f,
                "{outpoint} was spent by {by}, which is not one of this contract's transactions"
            ),
            Location::Unknown { reason } => write!(f, "unknown: {reason}"),
        }
    }
}

fn confirmed(height: Option<u32>) -> String {
    match height {
        Some(height) => format!("confirmed in block {height}"),
        None => "unconfirmed".into(),
    }
}

/// One entry's state.
#[derive(Debug, Clone, Serialize)]
pub struct EntryReport {
    pub entry_id: Uuid,
    pub competition_id: Uuid,
    /// The status the coordinator last recorded.
    pub status: String,
    pub location: Location,
    /// What can be done now, in order.
    pub now: Vec<Action>,
    pub next: Option<Opens>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

impl fmt::Display for EntryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Entry {} (competition {}, recorded {})",
            self.entry_id, self.competition_id, self.status
        )?;
        writeln!(f, "  Money: {}", self.location)?;
        if self.now.is_empty() {
            writeln!(f, "  Now: nothing to do")?;
        }
        for action in &self.now {
            writeln!(
                f,
                "  Now: {action} (coordinator-recover {})",
                action.command()
            )?;
        }
        if let Some(next) = &self.next {
            write!(f, "  Next: {}", next.action)?;
            match (next.at_height, next.at_time) {
                (Some(height), _) => match (next.blocks_left, next.eta) {
                    (Some(0), _) => writeln!(f, " from block {height}, the next block")?,
                    (Some(blocks), Some(eta)) => writeln!(
                        f,
                        " from block {height} (in {blocks} blocks, around {})",
                        utc(eta)
                    )?,
                    (Some(blocks), None) => {
                        writeln!(f, " from block {height} (in {blocks} blocks)")?
                    }
                    (None, _) => writeln!(f, " from block {height}")?,
                },
                (None, Some(time)) => writeln!(f, " from {} (unix {time})", utc(time))?,
                (None, None) => writeln!(f, " once the previous step confirms")?,
            }
        }
        for warning in &self.warnings {
            writeln!(f, "  Warning: {warning}")?;
        }
        for note in &self.notes {
            writeln!(f, "  Note: {note}")?;
        }
        Ok(())
    }
}

/// `time` as an ISO 8601 UTC date and time.
pub fn utc(time: u64) -> String {
    let days = time / 86_400;
    let seconds = time % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3_600,
        seconds % 3_600 / 60
    )
}

/// What Arkade says about an escrow VTXO, when the CLI could ask it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EscrowState {
    /// Unspent, and spendable offchain until `expires_at` (unix seconds).
    Live {
        expires_at: i64,
    },
    /// Expired and swept: only a recovery batch returns it.
    Swept,
    /// Already unrolled on chain.
    Unrolled,
    Spent {
        by: Option<Txid>,
    },
    /// Arkade did not answer; `reason` says why.
    Unreachable {
        reason: String,
    },
}

/// Where a contract's money has got to on chain, for this player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stage {
    /// More chain data is needed; see [`ChainView::missing`].
    Pending,
    FundingNotOnChain,
    FundingUnspent {
        confirmed: bool,
    },
    FundingSpentElsewhere {
        by: Txid,
    },
    NotPaid {
        outcome: Outcome,
    },
    OutcomeUnspent {
        outcome: Outcome,
        txid: Txid,
        height: Option<u32>,
    },
    OutcomeSpentElsewhere {
        txid: Txid,
        by: Txid,
    },
    SplitUnspent {
        outcome: Outcome,
        outpoint: OutPoint,
        value: u64,
        height: Option<u32>,
    },
    SplitSpent {
        outpoint: OutPoint,
        by: Txid,
        height: Option<u32>,
    },
}

/// Follow the contract's money on chain as far as `chain` knows.
pub(crate) fn stage(contract: &EntryContract, chain: &ChainView) -> Stage {
    let funding = contract.funding_outpoint();
    let Some(spend) = chain.outspend(funding) else {
        return Stage::Pending;
    };
    let Some(outcome_txid) = spend.spent_by else {
        return match chain.tx(funding.txid) {
            None => Stage::Pending,
            Some(None) => Stage::FundingNotOnChain,
            Some(Some(status)) => Stage::FundingUnspent {
                confirmed: status.confirmed_height.is_some(),
            },
        };
    };
    let Some(outcome) = contract.outcome_of(outcome_txid) else {
        return Stage::FundingSpentElsewhere { by: outcome_txid };
    };
    if contract.win_condition(outcome).is_none() {
        return Stage::NotPaid { outcome };
    }
    let outcome_output = OutPoint::new(outcome_txid, 0);
    let Some(split) = chain.outspend(outcome_output) else {
        return Stage::Pending;
    };
    let Some(split_txid) = split.spent_by else {
        return Stage::OutcomeUnspent {
            outcome,
            txid: outcome_txid,
            height: spend.confirmed_height,
        };
    };
    if contract.split_txid(outcome) != Some(split_txid) {
        return Stage::OutcomeSpentElsewhere {
            txid: outcome_txid,
            by: split_txid,
        };
    }
    let Some((outpoint, value)) = contract.win_output(outcome) else {
        return Stage::NotPaid { outcome };
    };
    let Some(win) = chain.outspend(outpoint) else {
        return Stage::Pending;
    };
    match win.spent_by {
        None => Stage::SplitUnspent {
            outcome,
            outpoint,
            value: value.to_sat(),
            height: split.confirmed_height,
        },
        Some(by) => Stage::SplitSpent {
            outpoint,
            by,
            height: win.confirmed_height,
        },
    }
}

/// Whether the expiry transaction is final now, by the chain's tip and median time past.
pub(crate) fn expired(expiry: LockTime, chain: &ChainView) -> bool {
    match expiry {
        LockTime::Blocks(height) => chain
            .tip_height
            .is_some_and(|tip| tip + 1 > height.to_consensus_u32()),
        LockTime::Seconds(time) => chain
            .median_time_past
            .is_some_and(|mtp| mtp > u64::from(time.to_consensus_u32())),
    }
}

/// Report on an entry with a verified contract. `now` is unix seconds, for estimates.
pub(crate) fn contract_report(
    report: &mut EntryReport,
    contract: &EntryContract,
    attestation: Option<MaybeScalar>,
    has_preimage: bool,
    chain: &ChainView,
    now: u64,
) {
    let delta = contract.delta();
    let reclaim = u32::from(delta) * 2;
    let tip = chain.tip_height;
    let deadline = |what: &str, height: u32| {
        let opens = height + reclaim;
        match tip {
            Some(tip) if tip + 1 >= opens => format!(
                "the market maker's reclaim of {what} has been open since block {opens}; \
                 it can take this money at any time"
            ),
            Some(tip) => format!(
                "the market maker can reclaim {what} from block {opens} ({} blocks from now, 2·delta after it confirmed)",
                opens - tip - 1
            ),
            None => format!("the market maker can reclaim {what} from block {opens}"),
        }
    };
    let needs_preimage = |report: &mut EntryReport| {
        if !has_preimage {
            report.warnings.push(
                "the split transaction needs this player's ticket preimage, which the records do \
                 not hold yet: pass it with --ticket-preimage"
                    .into(),
            );
        }
    };
    report.warnings.extend(contract.warnings.iter().cloned());
    match stage(contract, chain) {
        Stage::Pending => {
            report.location = Location::Unknown {
                reason: "waiting for chain data".into(),
            }
        }
        Stage::FundingNotOnChain => {
            report.location = Location::Unknown {
                reason: format!(
                    "the funding transaction {} is not on chain",
                    contract.funding_outpoint().txid
                ),
            }
        }
        Stage::FundingSpentElsewhere { by } => {
            report.location = Location::SpentElsewhere {
                outpoint: contract.funding_outpoint(),
                by,
            }
        }
        Stage::FundingUnspent { confirmed } => {
            report.location = Location::Funding {
                outpoint: contract.funding_outpoint(),
                amount_sat: contract.funding_value().to_sat(),
                confirmed,
            };
            let attested = attestation.and_then(|a| contract.attested_outcome(a));
            let expiry = contract.expiry();
            // The split spends the outcome output with a `delta` relative locktime: it opens
            // `delta` blocks after the outcome or expiry transaction confirms.
            let split_after = |report: &mut EntryReport, what: &str| {
                report.next = Some(Opens::after_confirmation(Action::BroadcastSplit));
                report.notes.push(format!(
                    "the split opens {delta} blocks after the {what} transaction confirms"
                ));
            };
            match (attested, expiry) {
                (Some(outcome), _) if contract.win_condition(outcome).is_some() => {
                    report.now = vec![Action::BroadcastOutcome];
                    split_after(report, "outcome");
                    needs_preimage(report);
                }
                (Some(outcome), _) => {
                    report.location = Location::Lost {
                        outcome: outcome.to_string(),
                    }
                }
                (None, Some(expiry)) if expired(expiry, chain) => {
                    if contract.win_condition(Outcome::Expiry).is_some() {
                        report.now = vec![Action::BroadcastExpiry];
                        split_after(report, "expiry");
                        needs_preimage(report);
                    }
                }
                (None, Some(expiry)) => {
                    report.next = Some(match expiry {
                        LockTime::Blocks(height) => Opens::at_height(
                            Action::BroadcastExpiry,
                            height.to_consensus_u32() + 1,
                            chain,
                            now,
                        ),
                        LockTime::Seconds(time) => Opens::at_time(
                            Action::BroadcastExpiry,
                            u64::from(time.to_consensus_u32()) + 1,
                        ),
                    });
                    report.notes.push(
                        "the outcome transaction can be broadcast as soon as the oracle attests"
                            .into(),
                    );
                }
                (None, None) => report.notes.push(
                    "the contract has no expiry: it waits for the oracle's attestation".into(),
                ),
            }
            if !confirmed {
                report
                    .notes
                    .push("the funding transaction is not confirmed yet".into());
            }
        }
        Stage::NotPaid { outcome } => {
            report.location = Location::Lost {
                outcome: outcome.to_string(),
            }
        }
        Stage::OutcomeUnspent {
            outcome,
            txid,
            height,
        } => {
            report.location = Location::Outcome {
                txid,
                outcome: outcome.to_string(),
                confirmed_height: height,
            };
            match height {
                Some(height) if chain.matured(height, delta) => {
                    report.now = vec![Action::BroadcastSplit]
                }
                Some(height) => {
                    report.next = Some(Opens::at_height(
                        Action::BroadcastSplit,
                        height + u32::from(delta),
                        chain,
                        now,
                    ))
                }
                None => {
                    report.next = Some(Opens::after_confirmation(Action::BroadcastSplit));
                    report.notes.push(format!(
                        "the outcome transaction is not confirmed yet; the split opens {delta} \
                         blocks after it confirms"
                    ));
                }
            }
            needs_preimage(report);
            if let Some(height) = height {
                report.warnings.push(deadline("the outcome output", height));
            }
        }
        Stage::OutcomeSpentElsewhere { txid, by } => {
            report.location = Location::SpentElsewhere {
                outpoint: OutPoint::new(txid, 0),
                by,
            }
        }
        Stage::SplitUnspent {
            outpoint,
            value,
            height,
            ..
        } => {
            report.location = Location::Split {
                outpoint,
                amount_sat: value,
                confirmed_height: height,
            };
            match height {
                Some(height) if chain.matured(height, delta) => {
                    report.now = vec![Action::ClaimWin];
                }
                Some(height) => {
                    report.next = Some(Opens::at_height(
                        Action::ClaimWin,
                        height + u32::from(delta),
                        chain,
                        now,
                    ))
                }
                None => {
                    report.next = Some(Opens::after_confirmation(Action::ClaimWin));
                    report.notes.push(format!(
                        "the claim opens {delta} blocks after the split transaction confirms"
                    ));
                }
            }
            if let Some(height) = height {
                report.warnings.push(deadline("this split output", height));
            }
        }
        Stage::SplitSpent {
            outpoint,
            by,
            height,
        } => {
            report.location = Location::Spent {
                outpoint,
                by,
                confirmed_height: height,
            }
        }
    }
}

/// Report on an entry that has an escrow and no usable contract.
pub(crate) fn escrow_report(
    report: &mut EntryReport,
    escrow: &Escrow,
    state: Option<&EscrowState>,
    now: u64,
) {
    report.location = Location::Escrow {
        outpoint: escrow.outpoint,
        amount_sat: escrow.amount.to_sat(),
    };
    let refund_at = escrow.refund_at();
    let open = now >= refund_at;
    let refund = match state {
        Some(EscrowState::Spent { by }) => {
            report.location = Location::EscrowSpent { by: *by };
            report.notes.push(
                "if the competition kicked off, the escrow funded its contract; its record \
                 should follow once the contract is published"
                    .into(),
            );
            return;
        }
        Some(EscrowState::Unrolled) => {
            report.now = vec![Action::Unroll];
            report
                .notes
                .push("the escrow is on chain; unroll finishes by spending it alone".into());
            return;
        }
        Some(EscrowState::Swept) => Action::RecoverExpiredEscrow,
        Some(EscrowState::Live { expires_at }) => {
            if *expires_at > 0 {
                report.warnings.push(format!(
                    "the escrow VTXO expires {}; after that only an Arkade recovery batch returns it",
                    utc(*expires_at as u64)
                ));
            }
            Action::RefundEscrow
        }
        Some(EscrowState::Unreachable { reason }) => {
            report.notes.push(format!(
                "Arkade did not answer ({reason}); without it, unroll takes the escrow on chain"
            ));
            Action::RefundEscrow
        }
        None => {
            report.notes.push(
                "the escrow's state on Arkade was not checked here; the CLI checks it".into(),
            );
            Action::RefundEscrow
        }
    };
    if open {
        report.now = vec![refund];
    } else {
        report.next = Some(Opens::at_time(refund, refund_at));
    }
}

/// A report with only the record's facts filled in.
pub(crate) fn blank_report(entry: &EntryRecord) -> EntryReport {
    EntryReport {
        entry_id: entry.entry_id,
        competition_id: entry.competition_id,
        status: entry.status.clone(),
        location: Location::NoFunds,
        now: Vec::new(),
        next: None,
        warnings: Vec::new(),
        notes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_700_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00 UTC");
    }
}
