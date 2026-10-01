//! Payout verification for participant-authorized ticketed DLC sellback.
//! Recipient origin comes from enclave TLS or an entry-key invoice authorization.
use crate::authorization::{authorization_digest, PayoutPolicy};
pub use dlctix;
#[cfg(test)]
use dlctix::bitcoin::hashes::Hash as _;
use dlctix::{
    bitcoin::{Amount, FeeRate, Network, OutPoint},
    secp::{MaybePoint, Scalar},
    ContractParameters, ContractSignatures, EventLockingConditions, MarketMaker, Outcome,
    PayoutWeights, TicketedDLC,
};
pub use lightning_invoice;
use lightning_invoice::Bolt11Invoice;
#[cfg(test)]
use lightning_invoice::Currency;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use uuid::Uuid;

pub use crate::escrow_lightning::MAX_INVOICE_BYTES;
#[cfg(test)]
use std::time::Duration;
pub const MAX_CONTRACT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractCommitment {
    pub contract_parameters: ContractParameters,
    pub funding_outpoint: OutPoint,
}

/// Economics accepted while the participant is online. Other player keys and
/// the future funding outpoint are bound before signing; economics cannot vary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractAuthorization {
    pub competition_id: Uuid,
    pub entry_id: Uuid,
    pub network: Network,
    pub player_index: usize,
    pub player_count: usize,
    pub ticket_hash: [u8; 32],
    pub payout_hash: [u8; 32],
    pub market_maker: MarketMaker,
    pub event: EventLockingConditions,
    pub outcome_payouts: BTreeMap<Outcome, PayoutWeights>,
    pub funding_value: Amount,
    pub relative_locktime_block_delta: u16,
    pub max_fee_rate: FeeRate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayoutError {
    ContractMismatch(String),
    UnknownOutcome,
    NotAPlayer,
    NotAWinner,
    InvalidInvoice(String),
    InvalidPolicy(String),
    PreimageMismatch,
    /// No attestation was given and the contract has not reached its expiry.
    NotExpired,
}
impl fmt::Display for PayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PayoutError {}

fn payout_weight_total(weights: &PayoutWeights, player_count: usize) -> Result<u64, PayoutError> {
    weights
        .iter()
        .try_fold(0u64, |total, (index, weight)| {
            if *index >= player_count || *weight == 0 {
                None
            } else {
                total.checked_add(*weight)
            }
        })
        .filter(|total| *total > 0)
        .ok_or_else(|| {
            PayoutError::InvalidPolicy(
                "Payouts need valid player slots and positive weights with a nonoverflowing total"
                    .into(),
            )
        })
}

impl ContractAuthorization {
    pub fn from_policy(policy: &PayoutPolicy) -> Result<Self, PayoutError> {
        if policy.contract_terms.len() > MAX_CONTRACT_BYTES {
            return Err(PayoutError::InvalidPolicy(
                "Contract authorization too large".into(),
            ));
        }
        if !policy.release_entry_key_after_payment
            || (policy.automatic_lightning_address.is_none() && !policy.allow_invoice_fallback)
        {
            return Err(PayoutError::InvalidPolicy(
                "A payout method and explicit sellback-key consent are required".into(),
            ));
        }
        let terms: Self = serde_json::from_str(&policy.contract_terms)
            .map_err(|e| PayoutError::InvalidPolicy(e.to_string()))?;
        if terms.player_count == 0
            || terms.player_index >= terms.player_count
            || terms.funding_value.to_sat() == 0
            || terms.max_fee_rate == FeeRate::ZERO
        {
            return Err(PayoutError::InvalidPolicy(
                "Invalid player count, slot, amount or fee ceiling".into(),
            ));
        }
        if terms.network == Network::Testnet4 {
            return Err(PayoutError::InvalidPolicy(
                "BOLT11 has no Testnet4 currency".into(),
            ));
        }
        if terms.outcome_payouts.is_empty() {
            return Err(PayoutError::InvalidPolicy(
                "At least one payout outcome is required".into(),
            ));
        }
        for weights in terms.outcome_payouts.values() {
            payout_weight_total(weights, terms.player_count)?;
        }
        Ok(terms)
    }

    pub fn verify_preimage(&self, preimage: &[u8]) -> Result<(), PayoutError> {
        if preimage.len() != 32 || sha256(preimage) != self.payout_hash {
            return Err(PayoutError::PreimageMismatch);
        }
        Ok(())
    }

    pub fn verify_contract(
        &self,
        commitment: &ContractCommitment,
        player_pubkey: &[u8],
    ) -> Result<(), PayoutError> {
        let p = &commitment.contract_parameters;
        p.validate()
            .map_err(|e| PayoutError::ContractMismatch(e.to_string()))?;
        if p.market_maker != self.market_maker
            || p.event != self.event
            || p.outcome_payouts != self.outcome_payouts
            || p.funding_value != self.funding_value
            || p.relative_locktime_block_delta != self.relative_locktime_block_delta
            || p.players.len() != self.player_count
            || p.fee_rate > self.max_fee_rate
        {
            return Err(PayoutError::ContractMismatch(
                "Contract differs from authorized economics".into(),
            ));
        }
        let player = p
            .players
            .get(self.player_index)
            .ok_or(PayoutError::NotAPlayer)?;
        if player.pubkey.serialize().as_slice() != player_pubkey
            || player.ticket_hash != self.ticket_hash
            || player.payout_hash != self.payout_hash
        {
            return Err(PayoutError::ContractMismatch(
                "Registered player slot, ticket or payout hash differs".into(),
            ));
        }
        Ok(())
    }
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
pub fn invoice_digest(invoice: &str) -> String {
    hex::encode(sha256(invoice.as_bytes()))
}
pub fn contract_digest(contract: &ContractCommitment) -> Result<String, PayoutError> {
    authorization_digest("payout-contract-v1", contract)
        .map(hex::encode)
        .map_err(|e| PayoutError::ContractMismatch(e.to_string()))
}

/// Verify the complete signature set using the market maker's perspective.
/// A player's perspective can omit losing outcomes and is insufficient here.
pub fn verify_completed_contract(
    contract: &ContractCommitment,
    signatures: &ContractSignatures,
) -> Result<(), PayoutError> {
    let dlc = TicketedDLC::new(
        contract.contract_parameters.clone(),
        contract.funding_outpoint,
    )
    .map_err(|e| PayoutError::ContractMismatch(e.to_string()))?;
    dlc.verify_signatures(contract.contract_parameters.market_maker.pubkey, signatures)
        .map_err(|e| PayoutError::ContractMismatch(e.to_string()))
}

/// The most completed contracts [`VerifiedContracts`] remembers.
pub const MAX_VERIFIED_CONTRACTS: usize = 64;

/// Completed contracts this process has already verified, so that the forfeits of one kickoff,
/// each carrying the same contract and signatures, verify them once.
///
/// Only a success is remembered, keyed by the contract's digest and the digest of the exact
/// signatures verified, so a changed signature is verified again and a failure never becomes a
/// success. Both are digests of public data. The oldest entry is dropped beyond
/// [`MAX_VERIFIED_CONTRACTS`].
#[derive(Default)]
pub struct VerifiedContracts(std::sync::Mutex<std::collections::VecDeque<([u8; 32], [u8; 32])>>);
impl VerifiedContracts {
    /// [`verify_completed_contract`], once for each contract and signature set.
    pub fn verify(
        &self,
        contract: &ContractCommitment,
        signatures: &ContractSignatures,
    ) -> Result<(), PayoutError> {
        let key = (
            authorization_digest("payout-contract-v1", contract)
                .map_err(|e| PayoutError::ContractMismatch(e.to_string()))?,
            authorization_digest("payout-contract-signatures-v1", signatures)
                .map_err(|e| PayoutError::ContractMismatch(e.to_string()))?,
        );
        if self.recall(&key) {
            return Ok(());
        }
        verify_completed_contract(contract, signatures)?;
        self.remember(key);
        Ok(())
    }
    /// Whether `key` was verified, making it the most recently used if so.
    fn recall(&self, key: &([u8; 32], [u8; 32])) -> bool {
        let Ok(mut verified) = self.0.lock() else {
            return false;
        };
        let Some(index) = verified.iter().position(|entry| entry == key) else {
            return false;
        };
        let Some(entry) = verified.remove(index) else {
            return false;
        };
        verified.push_back(entry);
        true
    }
    fn remember(&self, key: ([u8; 32], [u8; 32])) {
        let Ok(mut verified) = self.0.lock() else {
            return;
        };
        if !verified.contains(&key) {
            if verified.len() >= MAX_VERIFIED_CONTRACTS {
                verified.pop_front();
            }
            verified.push_back(key);
        }
    }
}

fn lightning_network(
    network: Network,
) -> Result<crate::escrow_lightning::LightningNetwork, PayoutError> {
    use crate::escrow_lightning::LightningNetwork as L;
    match network {
        Network::Bitcoin => Ok(L::Bitcoin),
        Network::Testnet => Ok(L::Testnet),
        Network::Signet => Ok(L::Signet),
        Network::Regtest => Ok(L::Regtest),
        _ => Err(PayoutError::InvalidInvoice(
            "Unsupported invoice network".into(),
        )),
    }
}
fn lightning_error(error: crate::escrow_lightning::LightningEvidenceError) -> PayoutError {
    match error {
        crate::escrow_lightning::LightningEvidenceError::InvalidInvoice(message) => {
            PayoutError::InvalidInvoice(message)
        }
        crate::escrow_lightning::LightningEvidenceError::PreimageMismatch => {
            PayoutError::PreimageMismatch
        }
    }
}
fn invoice_amount_msat(sats: u64) -> Result<u64, PayoutError> {
    sats.checked_mul(1000)
        .filter(|n| *n > 0)
        .ok_or_else(|| PayoutError::InvalidInvoice("Invalid amount".into()))
}
pub fn validate_invoice(
    invoice: &str,
    owed_sats: u64,
    network: Network,
    now_secs: u64,
) -> Result<Bolt11Invoice, PayoutError> {
    crate::escrow_lightning::validate_invoice(
        invoice,
        invoice_amount_msat(owed_sats)?,
        lightning_network(network)?,
        now_secs,
    )
    .map_err(lightning_error)
}
/// Validate an authenticated prepared invoice during reconciliation, including after expiry.
pub fn validate_prepared_invoice(
    invoice: &str,
    owed_sats: u64,
    network: Network,
) -> Result<Bolt11Invoice, PayoutError> {
    crate::escrow_lightning::validate_prepared_invoice(
        invoice,
        invoice_amount_msat(owed_sats)?,
        lightning_network(network)?,
    )
    .map_err(lightning_error)
}
pub fn verify_payment_preimage(invoice: &str, preimage: &[u8; 32]) -> Result<(), PayoutError> {
    crate::escrow_lightning::verify_payment_preimage(invoice, preimage).map_err(lightning_error)
}

/// The complete MuSig context for one contract signature. DLC keys are untweaked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningRequirement {
    pub signers: Vec<[u8; 33]>,
    pub adaptor_point: Option<[u8; 33]>,
}
pub fn signing_requirements(
    commitment: &ContractCommitment,
) -> Result<BTreeMap<[u8; 32], SigningRequirement>, PayoutError> {
    let dlc = TicketedDLC::new(
        commitment.contract_parameters.clone(),
        commitment.funding_outpoint,
    )
    .map_err(|e| PayoutError::ContractMismatch(e.to_string()))?;
    let data = dlc
        .signing_data()
        .map_err(|e| PayoutError::ContractMismatch(e.to_string()))?;
    let mut result = BTreeMap::new();
    for (outcome, sighash) in &data.outcome_sighashes {
        let adaptor_point = match outcome {
            Outcome::Attestation(index) => Some(data.adaptor_points[index].serialize()),
            Outcome::Expiry => None,
        };
        result.insert(
            *sighash,
            SigningRequirement {
                signers: data.funding_signers.iter().map(|p| p.serialize()).collect(),
                adaptor_point,
            },
        );
    }
    for (win, sighash) in &data.split_sighashes {
        result.insert(
            *sighash,
            SigningRequirement {
                signers: data.split_signers[&win.outcome]
                    .iter()
                    .map(|p| p.serialize())
                    .collect(),
                adaptor_point: None,
            },
        );
    }
    Ok(result)
}

/// Every sighash the contract's outcome and split transactions need.
pub fn contract_sighashes(
    commitment: &ContractCommitment,
) -> Result<BTreeSet<[u8; 32]>, PayoutError> {
    let dlc = TicketedDLC::new(
        commitment.contract_parameters.clone(),
        commitment.funding_outpoint,
    )
    .map_err(|e| PayoutError::ContractMismatch(e.to_string()))?;
    let signing_data = dlc
        .signing_data()
        .map_err(|e| PayoutError::ContractMismatch(e.to_string()))?;
    Ok(signing_data
        .outcome_sighashes
        .values()
        .chain(signing_data.split_sighashes.values())
        .copied()
        .collect())
}

/// The claimed contract is the one that was signed iff the signed messages
/// are exactly its sighashes. Any other message in the batch, or any missing
/// sighash, means the market maker signed something else.
pub fn verify_contract_binding(
    commitment: &ContractCommitment,
    signed_messages: &[[u8; 32]],
) -> Result<(), PayoutError> {
    let expected = contract_sighashes(commitment)?;
    let signed: BTreeSet<[u8; 32]> = signed_messages.iter().copied().collect();
    if signed_messages.len() != signed.len() {
        return Err(PayoutError::ContractMismatch(
            "signed batch contains duplicate messages".into(),
        ));
    }
    if expected != signed {
        return Err(PayoutError::ContractMismatch(format!(
            "contract needs {} signatures, batch signed {} matching messages",
            expected.len(),
            expected.intersection(&signed).count()
        )));
    }
    Ok(())
}

/// The outcome whose locking point the oracle's attestation opens.
pub fn attested_outcome(
    params: &ContractParameters,
    attestation: &[u8; 32],
) -> Result<Outcome, PayoutError> {
    let attestation = Scalar::from_slice(attestation).map_err(|_| PayoutError::UnknownOutcome)?;
    let locking_point = MaybePoint::Valid(attestation.base_point_mul());
    params
        .event
        .locking_points
        .iter()
        .position(|point| *point == locking_point)
        .map(Outcome::Attestation)
        .ok_or(PayoutError::UnknownOutcome)
}

/// The outcome a contract settles on: the attested one, or, with no attestation, the
/// expiry outcome once `now` (UNIX seconds) has reached the contract's expiry. Anyone can
/// broadcast the expiry transaction from then on, so its refund shares are what the
/// contract pays unless an attestation arrives first.
pub fn settled_outcome(
    params: &ContractParameters,
    attestation: Option<&[u8; 32]>,
    now: u64,
) -> Result<Outcome, PayoutError> {
    if let Some(attestation) = attestation {
        return attested_outcome(params, attestation);
    }
    match params.event.expiry {
        // Below this a locktime is a block height, which a clock cannot check.
        Some(expiry) if expiry >= 500_000_000 && now >= u64::from(expiry) => Ok(Outcome::Expiry),
        _ => Err(PayoutError::NotExpired),
    }
}

/// What the market maker owes the player with `player_pubkey` (compressed
/// secp256k1) under `outcome`: their weight's share of the funding value.
/// Weights are relative ratios; each share is rounded down to whole sats.
pub fn owed_sats(
    params: &ContractParameters,
    outcome: &Outcome,
    player_pubkey: &[u8],
) -> Result<u64, PayoutError> {
    let index = params
        .players
        .iter()
        .position(|player| player.pubkey.serialize().as_slice() == player_pubkey)
        .ok_or(PayoutError::NotAPlayer)?;
    let weights = params
        .outcome_payouts
        .get(outcome)
        .ok_or(PayoutError::NotAWinner)?;
    let total = payout_weight_total(weights, params.players.len())?;
    let weight = weights
        .get(&index)
        .copied()
        .ok_or(PayoutError::NotAWinner)?;
    let amount =
        (u128::from(params.funding_value.to_sat()) * u128::from(weight)) / u128::from(total);
    u64::try_from(amount)
        .ok()
        .filter(|amount| *amount > 0)
        .ok_or(PayoutError::NotAWinner)
}
#[cfg(test)]
#[path = "payout_tests.rs"]
pub(crate) mod tests;
