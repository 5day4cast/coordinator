//! An entry's DLC: the contract every player signed, checked against the derived entry key, and
//! the transactions that enforce it on chain.
//!
//! In order: the outcome transaction (the oracle's attestation adapts its signature) or, past the
//! event's expiry, the expiry transaction; then the split transaction, unlocked by this player's
//! ticket preimage; then, `delta` blocks after the split confirms, the win transaction, which this
//! player signs alone and which pays wherever they choose. The market maker can reclaim an outcome
//! or split output `2 * delta` blocks after it confirms, so each step has a deadline.

use bitcoin::absolute::LockTime;
use bitcoin::sighash::Prevouts;
use bitcoin::transaction::{predict_weight, Version};
use bitcoin::{Amount, FeeRate, OutPoint, ScriptBuf, Transaction, TxOut, Txid};
use dlctix::hashlock::{self, Preimage};
use dlctix::secp::{MaybeScalar, Point};
use dlctix::{ContractSignatures, Outcome, SignedContract, TicketedDLC, WinCondition};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::spec::{CompetitionRecord, EntryRecord};
use crate::{EntryKey, Error, Result};

/// An entry's verified contract.
pub struct EntryContract {
    entry_id: Uuid,
    signed: SignedContract,
    player_index: usize,
    /// Notes on records that disagree with the contract without changing what can be claimed.
    pub warnings: Vec<String>,
}

impl EntryContract {
    /// Rebuild the entry's contract from its competition and check it before anything is signed
    /// with it: `key` must be a player with this wallet's payout hash, and the signatures must
    /// cover every outcome and split this player can claim.
    pub fn new(
        entry: &EntryRecord,
        competition: &CompetitionRecord,
        key: &EntryKey,
    ) -> Result<Self> {
        let fail = |reason: String| Error::Contract {
            entry: entry.entry_id,
            reason,
        };
        if !key.matches(&entry.entry_pubkey) {
            return Err(Error::ForeignEntry(entry.entry_id));
        }
        let (params, funding_outpoint) = competition
            .contract()
            .ok_or_else(|| fail("the competition has no contract yet".into()))?;
        let mut warnings = Vec::new();
        if let Some(record) = &entry.contract {
            if record.funding_outpoint.parse::<OutPoint>().ok() != Some(funding_outpoint) {
                return Err(fail(format!(
                    "the entry record funds {} but the contract {funding_outpoint}",
                    record.funding_outpoint
                )));
            }
            let digest = hex::encode(Sha256::digest(
                serde_json::to_vec(params).map_err(|e| fail(e.to_string()))?,
            ));
            if !record.contract_parameters_sha256.is_empty()
                && !record
                    .contract_parameters_sha256
                    .eq_ignore_ascii_case(&digest)
            {
                warnings.push(
                    "the contract's terms hash differently from the entry record; the contract's \
                     own signatures were checked instead"
                        .into(),
                );
            }
        }
        let point = key.point();
        let player_index = params
            .players
            .iter()
            .position(|player| player.pubkey == point)
            .ok_or_else(|| fail("this entry's key is not a player in the contract".into()))?;
        if params.players[player_index].payout_hash != key.payout_hash() {
            return Err(fail(
                "the contract's payout hash for this player is not this wallet's".into(),
            ));
        }
        if let Some(record) = &entry.contract {
            if record.player_index != player_index {
                warnings.push(format!(
                    "the entry record says player {}, the contract has this key as player {player_index}",
                    record.player_index
                ));
            }
        }

        let dlc = TicketedDLC::new(params.clone(), funding_outpoint)
            .map_err(|e| fail(format!("invalid contract: {e}")))?;
        let candidates: Vec<&ContractSignatures> = competition
            .signed_contract
            .as_ref()
            .map(|signed| &signed.signatures)
            .into_iter()
            .chain(
                entry
                    .contract
                    .as_ref()
                    .and_then(|c| c.pruned_signatures.as_ref()),
            )
            .collect();
        let signatures = candidates
            .into_iter()
            .find(|signatures| dlc.verify_signatures(point, signatures).is_ok())
            .ok_or_else(|| {
                fail("no published signatures cover this player's outcomes and splits".into())
            })?
            .clone();
        Ok(Self {
            entry_id: entry.entry_id,
            signed: dlc.into_signed_contract_unchecked(signatures),
            player_index,
            warnings,
        })
    }

    pub fn signed(&self) -> &SignedContract {
        &self.signed
    }

    pub fn player_index(&self) -> usize {
        self.player_index
    }

    pub fn delta(&self) -> u16 {
        self.signed.params().relative_locktime_block_delta
    }

    pub fn funding_outpoint(&self) -> OutPoint {
        self.signed.dlc().funding_outpoint()
    }

    pub fn funding_value(&self) -> Amount {
        self.signed.params().funding_value
    }

    /// The event's expiry as a consensus locktime, if it has one.
    pub fn expiry(&self) -> Option<LockTime> {
        self.signed
            .params()
            .event
            .expiry
            .map(LockTime::from_consensus)
    }

    /// The outcome `attestation` unlocks, if it opens one of the event's locking points.
    pub fn attested_outcome(&self, attestation: MaybeScalar) -> Option<Outcome> {
        let point = attestation.base_point_mul();
        self.signed
            .params()
            .event
            .locking_points
            .iter()
            .position(|locking| *locking == point)
            .map(Outcome::Attestation)
    }

    /// The outcome whose transaction is `txid`.
    pub fn outcome_of(&self, txid: Txid) -> Option<Outcome> {
        self.signed
            .dlc()
            .unsigned_outcome_txs()
            .iter()
            .find(|(_, tx)| tx.compute_txid() == txid)
            .map(|(outcome, _)| *outcome)
    }

    pub fn outcome_txid(&self, outcome: Outcome) -> Option<Txid> {
        self.signed
            .dlc()
            .unsigned_outcome_txs()
            .get(&outcome)
            .map(Transaction::compute_txid)
    }

    pub fn split_txid(&self, outcome: Outcome) -> Option<Txid> {
        self.signed
            .unsigned_split_tx(&outcome)
            .map(Transaction::compute_txid)
    }

    /// This player's win condition under `outcome`, if they are paid in it.
    pub fn win_condition(&self, outcome: Outcome) -> Option<WinCondition> {
        self.signed
            .params()
            .outcome_payouts
            .get(&outcome)
            .filter(|payouts| payouts.contains_key(&self.player_index))
            .map(|_| WinCondition {
                outcome,
                player_index: self.player_index,
            })
    }

    /// This player's split output under `outcome`, and its value.
    pub fn win_output(&self, outcome: Outcome) -> Option<(OutPoint, Amount)> {
        let win = self.win_condition(outcome)?;
        let (input, prevout) = self.signed.split_win_tx_input_and_prevout(&win).ok()?;
        Some((input.previous_output, prevout.value))
    }

    /// The signed outcome transaction for `attestation`.
    pub fn outcome_tx(&self, attestation: MaybeScalar) -> Result<Transaction> {
        let Some(Outcome::Attestation(index)) = self.attested_outcome(attestation) else {
            return Err(self.fail("the attestation opens none of the event's outcomes"));
        };
        self.signed
            .signed_outcome_tx(index, attestation)
            .map_err(|e| self.fail(&format!("cannot sign the outcome transaction: {e}")))
    }

    /// The signed expiry transaction. Valid only once the event's expiry has passed.
    pub fn expiry_tx(&self) -> Result<Transaction> {
        self.signed
            .expiry_tx()
            .ok_or_else(|| self.fail("the contract has no signed expiry transaction"))
    }

    /// The split transaction for `outcome`, unlocked with this player's ticket preimage.
    pub fn split_tx(&self, outcome: Outcome, ticket_preimage: Preimage) -> Result<Transaction> {
        let win = self
            .win_condition(outcome)
            .ok_or_else(|| self.fail("this player is not paid in that outcome"))?;
        self.check_ticket(ticket_preimage)?;
        self.signed
            .signed_split_tx(&win, ticket_preimage)
            .map_err(|e| self.fail(&format!("cannot sign the split transaction: {e}")))
    }

    /// The transaction moving this player's split output to `destination`, signed with the entry
    /// key. Valid once the split has `delta` confirmations.
    pub fn win_tx(
        &self,
        outcome: Outcome,
        ticket_preimage: Preimage,
        key: &EntryKey,
        destination: ScriptBuf,
        fee_rate: FeeRate,
    ) -> Result<Transaction> {
        let win = self
            .win_condition(outcome)
            .ok_or_else(|| self.fail("this player is not paid in that outcome"))?;
        if key.point() != self.signed.params().players[self.player_index].pubkey {
            return Err(Error::ForeignEntry(self.entry_id));
        }
        self.check_ticket(ticket_preimage)?;
        let (input, prevout) = self
            .signed
            .split_win_tx_input_and_prevout(&win)
            .map_err(|e| self.fail(&e.to_string()))?;
        let prevout = prevout.clone();
        let weight = predict_weight(
            [self.signed.split_win_tx_input_weight()],
            [destination.len()],
        );
        let fee = fee_rate
            .checked_mul_by_weight(weight)
            .ok_or_else(|| self.fail("fee overflow"))?;
        let value = prevout
            .value
            .checked_sub(fee)
            .filter(|value| *value >= destination.minimal_non_dust())
            .ok_or_else(|| {
                self.fail(&format!(
                    "a {fee} fee leaves nothing of the {} output",
                    prevout.value
                ))
            })?;
        let mut tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![input],
            output: vec![TxOut {
                value,
                script_pubkey: destination,
            }],
        };
        self.signed
            .sign_split_win_tx_input(
                &win,
                &mut tx,
                0,
                &Prevouts::All(&[prevout]),
                ticket_preimage,
                key.scalar(),
            )
            .map_err(|e| self.fail(&format!("cannot sign the win transaction: {e}")))?;
        Ok(tx)
    }

    /// Whether `preimage` is this player's ticket preimage.
    pub fn check_ticket(&self, preimage: Preimage) -> Result<()> {
        if hashlock::sha256(&preimage)
            == self.signed.params().players[self.player_index].ticket_hash
        {
            Ok(())
        } else {
            Err(self.fail("the ticket preimage does not match this player's ticket hash"))
        }
    }

    pub fn player_point(&self) -> Point {
        self.signed.params().players[self.player_index].pubkey
    }

    fn fail(&self, reason: &str) -> Error {
        Error::Contract {
            entry: self.entry_id,
            reason: reason.to_owned(),
        }
    }
}

/// A 32-byte preimage from hex.
pub fn parse_preimage(hex_preimage: &str) -> Result<Preimage> {
    let mut preimage = [0u8; 32];
    hex::decode_to_slice(hex_preimage.trim(), &mut preimage)
        .map_err(|_| Error::Invalid("ticket preimage: expected 32 bytes of hex".into()))?;
    Ok(preimage)
}
