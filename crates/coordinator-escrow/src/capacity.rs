//! Conservative admission bounds before an event accepts entries or funds.
//! These account for the full policy roster, signing scope, and two predecessor
//! receipts used by a renewed key-release preparation. The synthetic values are
//! size models, never registration or signing inputs.
use crate::payout::dlctix::{
    bitcoin::{Amount, FeeRate, Network, OutPoint},
    secp::Scalar,
    ContractParameters, EventLockingConditions, MarketMaker, Outcome, Player,
};
use crate::{
    authorization::PayoutPolicy,
    generic,
    payout::{ContractAuthorization, ContractCommitment, MAX_INVOICE_BYTES},
    KeyMeldError, SessionId, UserId,
};
use keymeld_core::{
    crypto::EncryptedData,
    escrow::{
        self,
        protocol::{BindEscrowRequest, Payload, PrepareEscrowRequest},
        Action, ActionAttempt, AdaptorContext, ApplicationContext, EscrowContext, KeyTweak,
        PublicKeyBytes, Recipient, ScopeSigner, SignedEscrowPolicy, SigningItem, SigningScope,
    },
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Admission caps for the confidential payout path. Each ranked outcome costs a
/// signing item per winning place, so the place count dominates the batch size.
pub const MAX_COMPETITION_PLAYERS: usize = 25;
pub const MAX_COMPETITION_WINNING_PLACES: usize = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompetitionCapacity {
    pub signing_items: usize,
    pub bind_request_bytes: usize,
    pub signing_request_bytes: usize,
    pub settlement_request_bytes: usize,
    pub largest_receipt_bytes: usize,
}
fn invalid(message: impl Into<String>) -> KeyMeldError {
    KeyMeldError::ValidationError(message.into())
}
fn bytes(value: &impl Serialize) -> Result<usize, KeyMeldError> {
    serde_json::to_vec(value)
        .map(|value| value.len())
        .map_err(|e| KeyMeldError::SerializationError(e.to_string()))
}
fn check(name: &str, size: usize) -> Result<usize, KeyMeldError> {
    if size > escrow::MAX_PAYLOAD_BYTES {
        return Err(invalid(format!(
            "Competition exceeds confidential {name} capacity ({size} > {} bytes)",
            escrow::MAX_PAYLOAD_BYTES
        )));
    }
    Ok(size)
}
fn payload(size: usize) -> Result<Payload, KeyMeldError> {
    Payload::new(vec![255; check("payload", size)?])
}
fn encrypted_size(size: usize, purpose: &str) -> Result<usize, KeyMeldError> {
    // AES-GCM tag plus the protocol's existing compact context/nonce framing.
    Ok(size
        + EncryptedData {
            ciphertext: vec![0; 16],
            nonce: vec![0; 12],
            context: purpose.into(),
        }
        .to_bytes()?
        .len())
}
fn worst_bytes(value: &mut Value) {
    match value {
        Value::Array(values) if values.iter().all(|v| v.as_u64().is_some_and(|n| n <= 255)) => {
            for value in values {
                *value = json!(255);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(worst_bytes),
        Value::Object(values) => values.values_mut().for_each(worst_bytes),
        _ => {}
    }
}
fn worst(value: &impl Serialize) -> Result<Value, KeyMeldError> {
    let mut value =
        serde_json::to_value(value).map_err(|e| KeyMeldError::SerializationError(e.to_string()))?;
    worst_bytes(&mut value);
    Ok(value)
}

/// Bound a Coordinator ranking event independently of its future participant
/// keys, invoices, and oracle points. Values use the largest accepted address,
/// invoice, numeric fields and encoded hash/signature bytes. Scope sizing uses
/// every participant for every item, which deliberately overestimates subsets.
pub fn validate_competition_capacity(
    players: usize,
    winning_places: usize,
) -> Result<CompetitionCapacity, KeyMeldError> {
    if players == 0
        || players > MAX_COMPETITION_PLAYERS
        || winning_places == 0
        || winning_places > MAX_COMPETITION_WINNING_PLACES
        || winning_places > players
    {
        return Err(invalid(format!(
            "Confidential competitions support 1-{MAX_COMPETITION_PLAYERS} players \
             over 1-{MAX_COMPETITION_WINNING_PLACES} winning places"
        )));
    }
    let permutations = (0..winning_places)
        .try_fold(1usize, |count, index| count.checked_mul(players - index))
        .ok_or_else(|| invalid("Competition outcome count overflow"))?;
    // Ranked outcomes, refund-all attestation, and separate expiry; each has its
    // outcome signature and one split signature per payout recipient.
    let signing_items = permutations
        .checked_mul(winning_places + 1)
        .and_then(|n| n.checked_add(2 * players + 2))
        .ok_or_else(|| invalid("Competition signing count overflow"))?;
    if signing_items > escrow::MAX_BATCH_ITEMS {
        return Err(invalid(format!(
            "Competition needs {signing_items} signing items; confidential limit is {}",
            escrow::MAX_BATCH_ITEMS
        )));
    }
    let point = Scalar::from_slice(&[18; 32])
        .expect("fixed valid sizing scalar")
        .base_point_mul();
    let key = PublicKeyBytes::new(&point.serialize())?;
    let user = UserId::from(Uuid::from_u128(1));
    let id = Uuid::from_u128(1);
    let mut payouts = BTreeMap::new();
    for index in 0..=permutations {
        let count = if index == permutations {
            players
        } else {
            winning_places
        };
        payouts.insert(
            Outcome::Attestation(index),
            (players - count..players)
                .enumerate()
                .map(|(rank, slot)| (slot, if rank == 0 { 100 } else { 0 }))
                .collect(),
        );
    }
    payouts.insert(
        Outcome::Expiry,
        (0..players)
            .map(|slot| (slot, if slot == 0 { 100 } else { 0 }))
            .collect(),
    );
    let params = ContractParameters {
        market_maker: MarketMaker { pubkey: point },
        players: (0..players)
            .map(|_| Player {
                pubkey: point,
                ticket_hash: [255; 32],
                payout_hash: [255; 32],
            })
            .collect(),
        event: EventLockingConditions {
            locking_points: vec![point.into(); permutations + 1],
            expiry: Some(u32::MAX),
        },
        outcome_payouts: payouts,
        fee_rate: FeeRate::from_sat_per_kwu(u64::MAX),
        funding_value: Amount::from_sat(u64::MAX),
        relative_locktime_block_delta: u16::MAX,
    };
    let terms = ContractAuthorization {
        competition_id: id,
        entry_id: id,
        network: Network::Regtest,
        player_index: players - 1,
        player_count: players,
        ticket_hash: [255; 32],
        payout_hash: [255; 32],
        market_maker: params.market_maker.clone(),
        event: params.event.clone(),
        outcome_payouts: params.outcome_payouts.clone(),
        funding_value: params.funding_value,
        relative_locktime_block_delta: u16::MAX,
        max_fee_rate: params.fee_rate,
    };
    let mut contract_terms = serde_json::to_string(&terms)
        .map_err(|e| KeyMeldError::SerializationError(e.to_string()))?;
    // Valid synthetic weights use 0/100. Reserve two additional digits for each
    // weight so arbitrary accepted percentage distributions cannot be larger.
    contract_terms.push_str(&" ".repeat(2 * (permutations * winning_places + 2 * players)));
    let app_policy = PayoutPolicy {
        automatic_lightning_address: Some("x".repeat(320)),
        allow_invoice_fallback: false,
        release_entry_key_after_payment: true,
        contract_terms,
        ark_escrow: None,
    };
    let context = EscrowContext {
        keygen_session_id: SessionId::from(id),
        user_id: user.clone(),
        escrow_id: id,
        manifest_digest: [255; 32],
        application: ApplicationContext::commit(
            generic::VERIFIER_ID.into(),
            generic::VERIFIER_VERSION,
            &[],
        )?,
    };
    let mut policy = generic::participant_policy(
        context,
        key.clone(),
        app_policy,
        Recipient {
            encryption_public_key: key.clone(),
        },
    )?;
    policy.context.application.commitment = [255; 32];
    let signed = SignedEscrowPolicy {
        policy,
        signature: vec![255; 64],
    };
    let policies: BTreeMap<_, _> = (1..=players)
        .map(|index| (UserId::from(Uuid::from_u128(index as u128)), signed.clone()))
        .collect();
    let contract = ContractCommitment {
        contract_parameters: params,
        funding_outpoint: OutPoint::null(),
    };
    let binding_data = Payload::encode(&generic::ContractBinding {
        contract: contract.clone(),
    })?;
    let request = BindEscrowRequest {
        schema_version: escrow::SCHEMA_VERSION,
        policy: signed.clone(),
        application_context: signed.policy.verifier.as_ref().unwrap().policy_data.clone(),
        participant_policies: policies,
        binding_data,
    };
    let bind_request_bytes = check(
        "binding request",
        encrypted_size(bytes(&worst(&request)?)?, "escrow-request-v1")?,
    )?;
    let keys: BTreeMap<_, _> = (1..=players + 1)
        .map(|index| (UserId::from(Uuid::from_u128(index as u128)), key.clone()))
        .collect();
    let app_bound = worst(
        &json!({"contract":contract,"contract_digest":"f".repeat(64),"policy_digest":vec![255u8;32],"manifest_digest":vec![255u8;32],"participant_public_keys":keys}),
    )?;
    let digests: BTreeMap<_, _> = (1..=players)
        .map(|index| (UserId::from(Uuid::from_u128(index as u128)), [255; 32]))
        .collect();
    let binding = worst(
        &json!({"context":signed.policy.context,"policy_digest":vec![255u8;32],"enclave_id":u32::MAX,"participant_policy_digests":digests,"application_state":payload(bytes(&app_bound)?)?}),
    )?;
    // Extra metadata budget covers envelope tags and future fixed context fields.
    // No user-controlled unbounded value is charged to this reserve.
    const METADATA_RESERVE: usize = 2048;
    let binding_receipt = check(
        "binding receipt",
        encrypted_size(bytes(&binding)? + METADATA_RESERVE, "escrow_state_v1")?,
    )?;
    let signers: Vec<_> = keys
        .iter()
        .map(|(user_id, public_key)| ScopeSigner {
            user_id: user_id.clone(),
            public_key: public_key.clone(),
        })
        .collect();
    let scope = SigningScope {
        session_tweak: KeyTweak::None,
        batch: (0..signing_items)
            .map(|index| SigningItem {
                item_id: Uuid::from_u128(index as u128 + 1),
                message_digest: [255; 32],
                subset_id: Some(id),
                signers: signers.clone(),
                tweak: KeyTweak::None,
                adaptor: AdaptorContext::Single {
                    adaptor_id: id,
                    point: key.clone(),
                },
            })
            .collect(),
    };
    let sign_action = worst(&Action::Sign {
        scope: scope.clone(),
    })?;
    let sign_parameters = worst(&generic::ActionParameters::SignContract {
        scope,
        ark_funding: None,
    })?;
    let signing_receipt = check(
        "signing receipt",
        encrypted_size(
            bytes(&binding)? + bytes(&sign_action)? + METADATA_RESERVE,
            "escrow_state_v1",
        )?,
    )?;
    // An invoice occurs in both authenticated application_state and public output.
    // Both are bounded Payloads; reserve full allowed invoice plus fixed fields.
    let settlement_state = payload(MAX_INVOICE_BYTES + 2048)?;
    let settlement_receipt = check(
        "settlement receipt",
        encrypted_size(
            bytes(&binding)? + 2 * bytes(&settlement_state)? + METADATA_RESERVE,
            "escrow_state_v1",
        )?,
    )?;
    let attempt = ActionAttempt {
        attempt_id: id,
        signing_session_id: None,
    };
    let sign_request = PrepareEscrowRequest {
        schema_version: escrow::SCHEMA_VERSION,
        binding_receipt: payload(binding_receipt)?,
        action_id: generic::SIGN_CONTRACT.into(),
        attempt: ActionAttempt {
            attempt_id: id,
            signing_session_id: Some(SessionId::from(id)),
        },
        action: None,
        action_parameters: Payload::encode(&sign_parameters)?,
        prior_preparation_receipts: vec![payload(signing_receipt)?],
    };
    let signing_request_bytes = check(
        "signing retry request",
        encrypted_size(bytes(&sign_request)?, "escrow-request-v1")?,
    )?;
    // At most one <=65-byte signature per DLC item plus bounded map keys/tags.
    // Double 256 bytes per item also covers quoting JSON signatures inside JSON.
    let parameters = payload(2 * (signing_items * 256 + 1024) + MAX_INVOICE_BYTES + 4096)?;
    let settlement_request = PrepareEscrowRequest {
        schema_version: escrow::SCHEMA_VERSION,
        binding_receipt: payload(binding_receipt)?,
        action_id: generic::RELEASE_ENTRY_KEY.into(),
        attempt,
        action: None,
        action_parameters: parameters,
        prior_preparation_receipts: vec![
            payload(settlement_receipt)?,
            payload(settlement_receipt)?,
        ],
    };
    let settlement_request_bytes = check(
        "invoice renewal request",
        encrypted_size(bytes(&settlement_request)?, "escrow-request-v1")?,
    )?;
    let largest_receipt_bytes = check(
        "execution receipt",
        signing_receipt.max(settlement_receipt) + METADATA_RESERVE,
    )?;
    Ok(CompetitionCapacity {
        signing_items,
        bind_request_bytes,
        signing_request_bytes,
        settlement_request_bytes,
        largest_receipt_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_large_ranking_and_invalid_shapes_are_rejected_before_funding() {
        for (players, places) in [(0, 1), (101, 1), (4, 0), (4, 5), (10, 3)] {
            assert!(validate_competition_capacity(players, places).is_err());
        }
    }
    #[test]
    fn minimal_event_reserves_both_invoice_receipts_and_signing_retry() {
        let capacity = validate_competition_capacity(2, 1).unwrap();
        assert_eq!(capacity.signing_items, 10);
        assert!(capacity.settlement_request_bytes > capacity.bind_request_bytes);
        assert!(capacity.signing_request_bytes > 0);
        assert!(capacity.largest_receipt_bytes < escrow::MAX_PAYLOAD_BYTES);
    }
}

#[cfg(test)]
mod supported_sizes {
    //! The admitted envelope is capped explicitly rather than discovered from
    //! byte limits. Signing items stay well below MAX_BATCH_ITEMS at the cap, so
    //! the payload size, not the item count, is what a larger cap would strain.
    use super::{
        validate_competition_capacity, MAX_COMPETITION_PLAYERS, MAX_COMPETITION_WINNING_PLACES,
    };

    #[test]
    fn the_capped_shape_is_admitted_with_headroom() {
        let capacity =
            validate_competition_capacity(MAX_COMPETITION_PLAYERS, MAX_COMPETITION_WINNING_PLACES)
                .expect("the advertised maximum competition must be admitted");
        assert!(
            capacity.signing_items < keymeld_core::escrow::MAX_BATCH_ITEMS,
            "item count should not be the binding limit at the cap"
        );
        for size in [
            capacity.bind_request_bytes,
            capacity.signing_request_bytes,
            capacity.settlement_request_bytes,
            capacity.largest_receipt_bytes,
        ] {
            assert!(size <= keymeld_core::escrow::MAX_PAYLOAD_BYTES);
        }
    }

    #[test]
    fn shapes_beyond_the_cap_are_refused() {
        assert!(validate_competition_capacity(MAX_COMPETITION_PLAYERS + 1, 1).is_err());
        assert!(validate_competition_capacity(0, 1).is_err());
        assert!(validate_competition_capacity(4, 0).is_err());
        // Extra winning places multiply ranked outcomes and are not supported yet.
        for players in 2..=MAX_COMPETITION_PLAYERS {
            assert!(
                validate_competition_capacity(players, MAX_COMPETITION_WINNING_PLACES + 1).is_err(),
                "{players} players over {} places must be refused",
                MAX_COMPETITION_WINNING_PLACES + 1
            );
        }
    }
}
