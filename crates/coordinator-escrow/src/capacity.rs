//! Conservative admission bounds before an event accepts entries or funds.
//! These account for the full policy roster, the largest player's signing scope, and the
//! predecessor receipts a renewed preparation carries. The synthetic values are size models,
//! never registration or signing inputs.
use crate::payout::dlctix::{
    bitcoin::{Amount, FeeRate, Network, OutPoint},
    secp::{Point, Scalar},
    ContractParameters, EventLockingConditions, MarketMaker, Outcome, Player,
};
use crate::{
    ark::ArkFunding,
    authorization::{ArkEscrowPolicy, PayoutPolicy},
    generic,
    oracle_statement::{
        LineTerms, ObservationTerms, Outcomes, RankingOutcomes, ScoringRules, SignedStatement,
        Statement, Terms,
    },
    payout::{ContractAuthorization, ContractCommitment, MAX_INVOICE_BYTES},
    pools::PoolRules,
    queued::{QueuedEntryTerms, QueuedTerms},
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
/// signing item per winning place, so the place count dominates the batch size: a
/// competition pays one place to up to 25 players, or two places to up to 20.
pub const MAX_COMPETITION_PLAYERS: usize = 25;
pub const MAX_COMPETITION_WINNING_PLACES: usize = 2;
pub const MAX_TWO_PLACE_PLAYERS: usize = 20;

/// Whether `players` competing for `winning_places` is a shape the game offers: 1 to 25
/// players over one place, or at most 20 over two. [`validate_competition_capacity`] also
/// checks that the shape fits Keymeld's signing batch and payload limits.
pub fn supported_shape(players: usize, winning_places: usize) -> bool {
    (1..=MAX_COMPETITION_PLAYERS).contains(&players)
        && (1..=MAX_COMPETITION_WINNING_PLACES).contains(&winning_places)
        && winning_places <= players
        && (winning_places == 1 || players <= MAX_TWO_PLACE_PLAYERS)
}

/// The largest observation terms a queued competition may carry. Every entry's consent and each
/// pool's oracle statement repeat them, so [`validate_competition_capacity`] charges them at these
/// bounds, and [`crate::queued::QueuedTerms::validate`] refuses larger terms before anyone pays.
/// An event covers at most 50 stations, each scored on three metrics against one line each.
pub const MAX_QUEUED_TARGETS: usize = 50;
pub const MAX_QUEUED_SCORING_FIELDS: usize = 3;
pub const MAX_QUEUED_LINES: usize = MAX_QUEUED_TARGETS * MAX_QUEUED_SCORING_FIELDS;
/// The longest source, station or metric name, in bytes. Names hold no character JSON escapes,
/// so each costs at most this many bytes wherever it is encoded.
pub const MAX_QUEUED_NAME_BYTES: usize = 32;
/// Longer than an Arkade escrow's tap tree and checkpoint exit script in hex.
const MODELED_TAP_TREE_HEX_CHARS: usize = 4096;
const MODELED_EXIT_SCRIPT_HEX_CHARS: usize = 512;
/// An Arkade-funded pool's signing and settlement requests name its batch's commitment
/// transaction. Bitcoin relays no transaction over 400,000 weight units, so an unsigned one
/// serializes to at most 100,000 bytes.
const MODELED_COMMITMENT_TX_HEX_CHARS: usize = 2 * 100_000;

/// Keymeld's sealed-state context: it deflates a state's JSON, then encrypts it.
const SEALED_STATE: &str = "escrow_state_v2";
/// Deflate never adds more than a five-byte header per stored block of up to this many bytes.
const DEFLATE_STORED_BLOCK_BYTES: usize = 65_535;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompetitionCapacity {
    /// The whole contract's items, which the signing session's batch carries.
    pub signing_items: usize,
    /// The items one player's enclave permits: the ones the player signs.
    pub participant_signing_items: usize,
    /// This and the next two are requests encrypted, as Keymeld's escrow command carries them.
    pub bind_request_bytes: usize,
    pub signing_request_bytes: usize,
    pub settlement_request_bytes: usize,
    /// The largest state's JSON before it is sealed. Keymeld refuses to seal more than one
    /// payload.
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
/// A sealed receipt's bytes, from the JSON of the state it seals. Keymeld refuses a state whose
/// JSON exceeds one payload, then deflates and encrypts it. How far a state compresses depends
/// on its keys and digests, so this charges deflate's worst case: stored blocks, at five bytes
/// each over the JSON.
fn sealed(name: &str, json: usize) -> Result<usize, KeyMeldError> {
    let json = check(name, json)?;
    let deflated = json + 5 * (json / DEFLATE_STORED_BLOCK_BYTES + 1);
    check(name, encrypted_size(deflated, SEALED_STATE)?)
}
/// The signing items one player's enclave permits, as `(full, split)`. The verifier permits
/// exactly the contract's messages that the player signs. Every outcome transaction, and the
/// splits of the refund-all and expiry outcomes, are signed by every player and the market
/// maker: those are `full`. A ranked outcome's splits are signed by the market maker and that
/// outcome's winners only, so a player signs the `split` items of the outcomes they place in.
fn participant_items(
    players: usize,
    winning_places: usize,
    permutations: usize,
) -> Option<(usize, usize)> {
    let full = permutations
        .checked_add(2)?
        .checked_add(players.checked_mul(2)?)?;
    // A player places in k·P(n-1, k-1) of the ranked outcomes, each with k splits.
    let placed = (1..winning_places).try_fold(winning_places, |count, index| {
        count.checked_mul(players - index)
    })?;
    Some((full, placed.checked_mul(winning_places)?))
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
fn modeled_name() -> String {
    "x".repeat(MAX_QUEUED_NAME_BYTES)
}
/// A double with the longest JSON encoding: 17 significant digits and a three-digit negative
/// exponent.
const MODELED_LINE_BOUND: f64 = -f64::MIN_POSITIVE;
/// The largest observation terms a queued competition may carry, each field at its longest
/// encoding.
fn modeled_observation() -> ObservationTerms {
    ObservationTerms {
        source: modeled_name(),
        start_observation_date: i64::MIN,
        end_observation_date: i64::MIN,
        targets: vec![modeled_name(); MAX_QUEUED_TARGETS],
        scoring_fields: vec![modeled_name(); MAX_QUEUED_SCORING_FIELDS],
        number_of_values_per_entry: u32::MAX,
        scoring_rules: ScoringRules::Lines,
        lines: (0..MAX_QUEUED_LINES)
            .map(|_| LineTerms {
                target: modeled_name(),
                metric: modeled_name(),
                lower: MODELED_LINE_BOUND,
                upper: MODELED_LINE_BOUND,
                window_hours: u32::MAX,
            })
            .collect(),
    }
}
/// The oracle's signed statement of a pool's event, which a queued competition's pool binds.
fn modeled_statement(players: usize, point: Point) -> SignedStatement {
    SignedStatement {
        statement: Statement {
            event_id: Uuid::max(),
            signing_date: i64::MIN,
            expiry: u32::MAX,
            nonce_point: point,
            outcomes: Outcomes::Ranking(RankingOutcomes {
                number_of_places_win: u32::MAX,
                entry_ids: vec![Uuid::max(); players],
            }),
            terms: Terms::Observation(modeled_observation()),
        },
        signature: "f".repeat(128),
    }
}
/// A queued entry's consent, which names the competition's terms instead of a contract.
fn modeled_queued_entry(market_maker: MarketMaker) -> Result<String, KeyMeldError> {
    let entry = QueuedEntryTerms {
        terms: QueuedTerms {
            competition_id: Uuid::max(),
            network: Network::Regtest,
            market_maker,
            oracle_pubkey: "f".repeat(64),
            signing_date: i64::MIN,
            expiry: u32::MAX,
            observation: modeled_observation(),
            number_of_places_win: u32::MAX,
            multi_place_min_players: Some(u32::MAX),
            pool_rules: PoolRules::new(10, MAX_COMPETITION_PLAYERS)
                .map_err(|e| invalid(e.to_string()))?,
            stake_sats: u64::MAX,
            relative_locktime_block_delta: u16::MAX,
            max_fee_rate: FeeRate::from_sat_per_kwu(u64::MAX),
        },
        entry_id: Uuid::max(),
        ticket_hash: [255; 32],
        payout_hash: [255; 32],
    };
    serde_json::to_string(&entry).map_err(|e| KeyMeldError::SerializationError(e.to_string()))
}

/// Bound a Coordinator ranking event independently of its future participant
/// keys, invoices, and oracle points. Values use the largest accepted address,
/// invoice, numeric fields and encoded hash/signature bytes. A player's signing scope
/// holds the items that player signs, each with its real signer count, and every item
/// carries a subset and an adaptor point whether or not the real one does.
pub fn validate_competition_capacity(
    players: usize,
    winning_places: usize,
) -> Result<CompetitionCapacity, KeyMeldError> {
    if !supported_shape(players, winning_places) {
        return Err(invalid(format!(
            "Confidential competitions support 1-{MAX_COMPETITION_PLAYERS} players over one \
             winning place, or up to {MAX_TWO_PLACE_PLAYERS} players over two winning places"
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
    let (full_items, split_items) = participant_items(players, winning_places, permutations)
        .ok_or_else(|| invalid("Competition signing count overflow"))?;
    let participant_signing_items = full_items + split_items;
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
            (players - count..players).map(|slot| (slot, 100)).collect(),
        );
    }
    payouts.insert(
        Outcome::Expiry,
        (0..players).map(|slot| (slot, 100)).collect(),
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
    // Generated weights are at most 100. Every modeled recipient gets three
    // digits, including the refund and expiry recipients whose real weight is 1.
    let contract_terms = serde_json::to_string(&terms)
        .map_err(|e| KeyMeldError::SerializationError(e.to_string()))?;
    let app_policy = PayoutPolicy {
        queued_entry: None,
        automatic_lightning_address: Some("x".repeat(320)),
        allow_invoice_fallback: false,
        release_entry_key_after_payment: true,
        contract_terms,
        // An Arkade escrow adds its terms and two permissions to every policy.
        ark_escrow: Some(ArkEscrowPolicy {
            escrow_tap_tree: "f".repeat(MODELED_TAP_TREE_HEX_CHARS),
            max_fee_sats: u64::MAX,
            max_refund_fee_sats: u64::MAX,
            checkpoint_exit_script: "f".repeat(MODELED_EXIT_SCRIPT_HEX_CHARS),
        }),
    };
    // A queued entry's policy names its competition's terms instead of a contract.
    let queued_policy = Payload::encode(&PayoutPolicy {
        contract_terms: String::new(),
        queued_entry: Some(modeled_queued_entry(params.market_maker.clone())?),
        ..app_policy.clone()
    })?;
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
    // Model whichever kind of consent is larger.
    if let Some(verifier) = policy.verifier.as_mut() {
        if queued_policy.as_bytes().len() > verifier.policy_data.as_bytes().len() {
            verifier.policy_data = queued_policy;
        }
    }
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
    // A queued competition's pool also binds the oracle's statement of its event.
    let statement = modeled_statement(players, point);
    let binding_data = Payload::encode(&generic::ContractBinding {
        statement: Some(statement.clone()),
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
        &json!({"contract":contract,"contract_digest":"f".repeat(64),"policy_digest":vec![255u8;32],"manifest_digest":vec![255u8;32],"participant_public_keys":keys,"statement":statement}),
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
    let binding_receipt = sealed("binding receipt", bytes(&binding)? + METADATA_RESERVE)?;
    let signers: Vec<_> = keys
        .iter()
        .map(|(user_id, public_key)| ScopeSigner {
            user_id: user_id.clone(),
            public_key: public_key.clone(),
        })
        .collect();
    // The market maker and one ranked outcome's winners.
    let winners = &signers[..winning_places + 1];
    let item = |index: usize, signers: &[ScopeSigner]| SigningItem {
        item_id: Uuid::from_u128(index as u128 + 1),
        message_digest: [255; 32],
        subset_id: Some(id),
        signers: signers.to_vec(),
        tweak: KeyTweak::None,
        adaptor: AdaptorContext::Single {
            adaptor_id: id,
            point: key.clone(),
        },
    };
    let scope = SigningScope {
        session_tweak: KeyTweak::None,
        batch: (0..full_items)
            .map(|index| item(index, &signers))
            .chain((full_items..participant_signing_items).map(|index| item(index, winners)))
            .collect(),
    };
    let sign_action = worst(&Action::Sign {
        scope: scope.clone(),
    })?;
    let ark_funding = ArkFunding {
        commitment_tx: "f".repeat(MODELED_COMMITMENT_TX_HEX_CHARS),
        vout: u32::MAX,
    };
    // The Coordinator first sends the compact scope, and the full one only to a verifier that
    // predates it, so the full one bounds both.
    let sign_parameters = worst(&generic::ActionParameters::SignContract {
        scope,
        ark_funding: Some(ark_funding.clone()),
    })?;
    // The verifier keeps the commitment in the prepared state.
    let signing_application_state = Payload::encode(&json!({
        "kind": "contract_signing",
        "ark_funding": ark_funding,
    }))?;
    let signing_state = bytes(&binding)?
        + bytes(&sign_action)?
        + bytes(&signing_application_state)?
        + METADATA_RESERVE;
    let signing_receipt = sealed("signing receipt", signing_state)?;
    // An invoice occurs in both authenticated application_state and public output.
    // Both are bounded Payloads; reserve full allowed invoice plus fixed fields.
    let settlement_state = payload(MAX_INVOICE_BYTES + 2048)?;
    let settlement_state = bytes(&binding)? + 2 * bytes(&settlement_state)? + METADATA_RESERVE;
    let settlement_receipt = sealed("settlement receipt", settlement_state)?;
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
    let parameters = payload(
        2 * (signing_items * 256 + 1024) + MAX_INVOICE_BYTES + bytes(&ark_funding)? + 4096,
    )?;
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
    // An executed state adds its fixed-size output to the prepared one.
    let largest_receipt_bytes = check(
        "execution receipt",
        signing_state.max(settlement_state) + METADATA_RESERVE,
    )?;
    Ok(CompetitionCapacity {
        signing_items,
        participant_signing_items,
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
    fn queued_terms_are_modeled_at_the_largest_the_enclave_accepts() {
        crate::queued::check_observation_size(&modeled_observation()).unwrap();
        let longest = serde_json::to_string(&MODELED_LINE_BOUND).unwrap().len();
        assert_eq!(longest, 24);
        for value in [f64::MIN, f64::MAX, -f64::EPSILON, -1.2345678901234567e-300] {
            assert!(serde_json::to_string(&value).unwrap().len() <= longest);
        }
    }
    #[test]
    fn small_events_reserve_both_invoice_receipts_and_signing_retry() {
        for players in [2, 3, 7] {
            let capacity = validate_competition_capacity(players, 1).unwrap();
            assert_eq!(capacity.signing_items, 4 * players + 2);
            // Every item but the other players' win splits.
            assert_eq!(capacity.participant_signing_items, 3 * players + 3);
            // A pool's bind request carries its oracle statement at the largest terms, so it
            // may outgrow the settlement request; each still fits one payload.
            assert!(capacity.bind_request_bytes <= escrow::MAX_PAYLOAD_BYTES);
            assert!(capacity.settlement_request_bytes <= escrow::MAX_PAYLOAD_BYTES);
            assert!(capacity.signing_request_bytes > 0);
            assert!(capacity.largest_receipt_bytes < escrow::MAX_PAYLOAD_BYTES);
        }
    }
}

#[cfg(test)]
mod supported_sizes {
    //! The admitted envelope is capped explicitly rather than discovered from
    //! byte limits: one place up to 25 players, two places up to 20. Both must fit
    //! Keymeld's signing batch and one payload per request.
    use super::{
        supported_shape, validate_competition_capacity, MAX_COMPETITION_PLAYERS,
        MAX_COMPETITION_WINNING_PLACES, MAX_TWO_PLACE_PLAYERS,
    };
    use keymeld_core::escrow::{MAX_BATCH_ITEMS, MAX_PAYLOAD_BYTES};

    fn fits_payloads(capacity: &super::CompetitionCapacity) {
        for size in [
            capacity.bind_request_bytes,
            capacity.signing_request_bytes,
            capacity.settlement_request_bytes,
            capacity.largest_receipt_bytes,
        ] {
            assert!(size <= MAX_PAYLOAD_BYTES);
        }
    }

    #[test]
    fn the_capped_one_place_shape_is_admitted_with_headroom() {
        let capacity = validate_competition_capacity(MAX_COMPETITION_PLAYERS, 1)
            .expect("25 players over one place must be admitted");
        println!("25 players over one place: {capacity:?}");
        assert!(
            capacity.signing_items < MAX_BATCH_ITEMS,
            "item count should not be the binding limit at the cap"
        );
        fits_payloads(&capacity);
    }

    #[test]
    fn twenty_players_over_two_places_fit_one_batch_and_one_payload_per_request() {
        // P(20, 2) ranked outcomes with three signatures each, plus refund-all and expiry.
        let items = 20 * 19 * 3 + 2 * 20 + 2;
        assert_eq!(items, 1_182);
        assert!(items <= MAX_BATCH_ITEMS);
        let capacity = validate_competition_capacity(MAX_TWO_PLACE_PLAYERS, 2)
            .expect("20 players over two places must be admitted");
        assert_eq!(capacity.signing_items, items);
        // A player signs the 422 items every signer signs, and the two splits of each of the
        // 38 outcomes they place in.
        assert_eq!(capacity.participant_signing_items, 422 + 76);
        fits_payloads(&capacity);
        println!("20 players over two places: {capacity:?}");
    }

    #[test]
    fn shapes_beyond_the_cap_are_refused() {
        assert!(supported_shape(MAX_COMPETITION_PLAYERS, 1));
        assert!(supported_shape(MAX_TWO_PLACE_PLAYERS, 2));
        assert!(validate_competition_capacity(MAX_COMPETITION_PLAYERS + 1, 1).is_err());
        assert!(validate_competition_capacity(0, 1).is_err());
        assert!(validate_competition_capacity(4, 0).is_err());
        // Two places only up to 20 players, whatever Keymeld's batch allows.
        for players in [MAX_TWO_PLACE_PLAYERS + 1, MAX_COMPETITION_PLAYERS] {
            assert!(
                !supported_shape(players, 2),
                "{players} players over two places"
            );
            assert!(validate_competition_capacity(players, 2).is_err());
        }
        // A third place is not offered.
        for players in 3..=MAX_COMPETITION_PLAYERS {
            assert!(
                validate_competition_capacity(players, MAX_COMPETITION_WINNING_PLACES + 1).is_err(),
                "{players} players over {} places must be refused",
                MAX_COMPETITION_WINNING_PLACES + 1
            );
        }
    }
}
