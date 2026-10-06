use super::publisher::{publish_due, retry_delay, MAX_ATTEMPTS};
use super::*;
use crate::domain::{CompetitionStore, RecoveryOutboxEvent};
use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
use bitcoin::{
    absolute::LockTime,
    secp256k1::{Secp256k1, SecretKey as BitcoinSecretKey},
    Amount, FeeRate, XOnlyPublicKey,
};
use coordinator_ark_escrow::EscrowTerms;
use dlctix::{
    hashlock,
    musig2::{self, CompactSignature},
    secp::Scalar,
    MarketMaker, PayoutWeights, Player, SignedContract, SigningSession, TicketedDLC,
};
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc,
    },
};

fn secret(byte: u8) -> SecretKey {
    let mut bytes = [0u8; 32];
    bytes[31] = byte;
    SecretKey::from_slice(&bytes).unwrap()
}

fn recovery(relays: Vec<String>) -> Recovery {
    Recovery::new(
        secret(1),
        Network::Signet,
        relays,
        "https://arkd.example".into(),
    )
}

fn scalar(index: u64) -> Scalar {
    Scalar::from_slice(&hashlock::sha256(&index.to_be_bytes())).unwrap()
}

fn x_only(byte: u8) -> XOnlyPublicKey {
    BitcoinSecretKey::from_slice(&[byte; 32])
        .unwrap()
        .x_only_public_key(&Secp256k1::new())
        .0
}

fn sample_record(recovery: &Recovery, user: &PublicKey) -> EntryRecord {
    EntryRecord {
        v: 1,
        record_type: "entry".into(),
        network: "signet".into(),
        coordinator_pubkey: recovery.public_key().to_hex(),
        user_pubkey: user.to_hex(),
        competition_id: Uuid::now_v7(),
        entry_id: Uuid::now_v7(),
        entry_pubkey: hex::encode(scalar(1).base_point_mul().serialize()),
        status: RecordStatus::Paid,
        escrow: None,
        ticket: Some(TicketRecord {
            hash: "aa".repeat(32),
            preimage: Some("bb".repeat(32)),
        }),
        contract: None,
        updated_at: 1_700_000_000,
    }
}

fn tags(event: &Event) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect()
}

fn outbox(
    d_tag: String,
    kind: &'static str,
    user: Option<&PublicKey>,
    competition_id: Option<Uuid>,
    event: &Event,
) -> RecoveryOutboxEvent {
    RecoveryOutboxEvent {
        content_sha256: content_digest(&event.content),
        d_tag,
        kind,
        user_pubkey: user.map(PublicKey::to_hex),
        competition_id,
        event_json: serde_json::to_string(event).unwrap(),
        created_at: event.created_at.as_secs() as i64,
    }
}

async fn store() -> (CompetitionStore, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let database = DBConnection::new(
        directory.path().to_str().unwrap(),
        "competitions",
        DatabasePoolConfig::default(),
        DatabaseType::Competitions,
    )
    .await
    .unwrap();
    (CompetitionStore::new(database), directory)
}

// The vector in the shared recovery spec: coordinator key 1, player key 2.
#[test]
fn the_blind_tag_matches_the_spec_vector() {
    let coordinator = Keys::new(secret(1)).public_key();
    let player = Keys::new(secret(2)).public_key();
    assert_eq!(
        coordinator.to_hex(),
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
    );
    assert_eq!(
        player.to_hex(),
        "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5"
    );
    let expected = "f5b81b1079306318eceb3a9a65e9e0144bd0757b1f3304e1b5d5d549328b1bc9";
    assert_eq!(blind_tag(&coordinator, &player), expected);
    assert_eq!(recovery(vec![]).blind(&player), expected);
    assert_ne!(blind_tag(&player, &coordinator), expected);
}

#[test]
fn a_player_decrypts_their_records_with_the_coordinator_key() {
    let recovery = recovery(vec![]);
    let player = Keys::new(secret(2));
    let record = sample_record(&recovery, &player.public_key());
    let blind = recovery.blind(&player.public_key());

    let event = recovery
        .entry_event(&player.public_key(), &record, 1_700_000_000)
        .unwrap();
    event.verify().unwrap();
    assert_eq!(event.kind, Kind::Custom(30078));
    assert_eq!(event.pubkey, recovery.public_key());
    assert_eq!(event.created_at.as_secs(), 1_700_000_000);
    let entry_tags = tags(&event);
    assert!(entry_tags.contains(&vec![
        "d".to_string(),
        format!("{blind}:entry:{}", record.entry_id)
    ]));
    assert!(entry_tags.contains(&vec!["b".to_string(), blind.clone()]));
    // Nothing on the event names the player.
    assert!(entry_tags.iter().all(|tag| tag[0] != "p"));
    assert!(!serde_json::to_string(&event)
        .unwrap()
        .contains(&player.public_key().to_hex()));

    let plaintext =
        nip44::decrypt(player.secret_key(), &recovery.public_key(), &event.content).unwrap();
    assert_eq!(
        serde_json::from_str::<EntryRecord>(&plaintext).unwrap(),
        record
    );
    let stranger = Keys::new(secret(3));
    assert!(nip44::decrypt(
        stranger.secret_key(),
        &recovery.public_key(),
        &event.content
    )
    .is_err());

    let wallet = recovery.wallet_record("nip44-blob");
    let event = recovery
        .wallet_event(&player.public_key(), &wallet, 5)
        .unwrap();
    event.verify().unwrap();
    assert!(tags(&event).contains(&vec!["d".to_string(), format!("{blind}:wallet")]));
    let plaintext =
        nip44::decrypt(player.secret_key(), &recovery.public_key(), &event.content).unwrap();
    let decrypted: WalletRecord = serde_json::from_str(&plaintext).unwrap();
    assert_eq!(decrypted, wallet);
    assert_eq!(decrypted.network, "signet");
    assert_eq!(decrypted.wallet_blob, "nip44-blob");
}

#[test]
fn a_records_digest_ignores_when_it_was_written() {
    let recovery = recovery(vec![]);
    let player = Keys::new(secret(2)).public_key();
    let record = sample_record(&recovery, &player);
    let later = EntryRecord {
        updated_at: record.updated_at + 60,
        ..record.clone()
    };
    assert_eq!(record.digest().unwrap(), later.digest().unwrap());
    let advanced = EntryRecord {
        status: RecordStatus::InContract,
        ..record.clone()
    };
    assert_ne!(record.digest().unwrap(), advanced.digest().unwrap());
    assert_eq!(next_created_at(100, None), 100);
    assert_eq!(next_created_at(100, Some(100)), 101);
    assert_eq!(next_created_at(100, Some(40)), 100);
}

/// A signed contract between the market maker and three players. Outcome 0 pays player 0;
/// outcome 1 pays players 1 and 2; the expiry pays all three.
fn signed_contract() -> (SignedContract, [Scalar; 3], [Scalar; 2]) {
    let market_maker = Scalar::from_slice(&[7; 32]).unwrap();
    let players = [1, 3, 5].map(|key| Scalar::from_slice(&[key; 32]).unwrap());
    let attestations = [20, 21].map(|key| Scalar::from_slice(&[key; 32]).unwrap());
    let params = ContractParameters {
        market_maker: MarketMaker {
            pubkey: market_maker.base_point_mul(),
        },
        players: players
            .iter()
            .enumerate()
            .map(|(index, key)| Player {
                pubkey: key.base_point_mul(),
                ticket_hash: hashlock::sha256(&[index as u8 + 10; 32]),
                payout_hash: hashlock::sha256(&[index as u8 + 20; 32]),
            })
            .collect(),
        event: EventLockingConditions {
            locking_points: attestations
                .iter()
                .map(|key| key.base_point_mul().into())
                .collect(),
            expiry: Some(1_900_000_000),
        },
        outcome_payouts: [
            (Outcome::Attestation(0), PayoutWeights::from([(0, 1)])),
            (
                Outcome::Attestation(1),
                PayoutWeights::from([(1, 1), (2, 1)]),
            ),
            (
                Outcome::Expiry,
                PayoutWeights::from([(0, 1), (1, 1), (2, 1)]),
            ),
        ]
        .into(),
        fee_rate: FeeRate::from_sat_per_vb_u32(1),
        funding_value: Amount::from_sat(100_000),
        relative_locktime_block_delta: 72,
    };
    let dlc = TicketedDLC::new(params, OutPoint::null()).unwrap();
    let mut rng = ChaCha20Rng::from_seed([42; 32]);
    let mut sessions: BTreeMap<_, _> = std::iter::once(market_maker)
        .chain(players)
        .map(|key| {
            (
                key.base_point_mul(),
                SigningSession::new(dlc.clone(), &mut rng, key).unwrap(),
            )
        })
        .collect();
    let nonces = sessions
        .iter()
        .map(|(key, session)| (*key, session.our_public_nonces().clone()))
        .collect();
    let coordinator = sessions
        .remove(&market_maker.base_point_mul())
        .unwrap()
        .aggregate_nonces_and_compute_partial_signatures(nonces)
        .unwrap();
    let signatures = sessions
        .into_iter()
        .map(|(key, session)| {
            let contributor = session
                .compute_partial_signatures(coordinator.aggregated_nonces().clone())
                .unwrap();
            (key, contributor.our_partial_signatures().clone())
        })
        .collect();
    (
        coordinator.aggregate_all_signatures(signatures).unwrap(),
        players,
        attestations,
    )
}

fn competition_row(
    id: Uuid,
    params: &ContractParameters,
    signed_contract: &serde_json::Value,
) -> RecoveryCompetitionRow {
    RecoveryCompetitionRow {
        id,
        event_submission: serde_json::to_vec(&json!({ "id": id })).unwrap(),
        event_announcement: Some(serde_json::to_vec(&params.event).unwrap()),
        contract_parameters: Some(serde_json::to_vec(params).unwrap()),
        signed_contract: Some(serde_json::to_vec(signed_contract).unwrap()),
        ..Default::default()
    }
}

#[test]
fn an_entry_record_follows_the_entry() {
    let recovery = recovery(vec![]);
    let (contract, players, attestations) = signed_contract();
    let params = contract.params().clone();
    let player = Keys::new(secret(2));
    let entry_id = Uuid::now_v7();
    let escrow = EntryEscrow::new(EscrowTerms {
        player: dlctix::convert_point(players[1].base_point_mul()),
        coordinator: x_only(30),
        server: x_only(31),
        refund_locktime: LockTime::from_time(1_800_000_000).unwrap(),
        exit_delay: RelativeTimelock::Seconds(512 * 10),
        unilateral_refund_delay: RelativeTimelock::Seconds(512 * 20),
    })
    .unwrap();
    let ticket = RecoveryTicketRow {
        ticket_id: Uuid::now_v7(),
        ticket_hash: hex::encode(params.players[1].ticket_hash),
        ticket_preimage: hex::encode([11u8; 32]),
        reserved_at: Some(1_799_990_000),
        reserved_by: Some(player.public_key().to_hex()),
        paid: true,
        settled: false,
        entry_id: Some(entry_id),
        entry_user: Some(player.public_key().to_hex()),
        entry_pubkey: Some(hex::encode(players[1].base_point_mul().serialize())),
        escrow_tap_tree: Some(hex::encode(escrow.vtxo_script().encode_tap_tree())),
        vtxo_outpoint: Some(format!("{}:0", "ab".repeat(32))),
        vtxo_sats: Some(10_500),
        ..Default::default()
    };
    let competition_id = Uuid::now_v7();
    let unsigned = RecoveryCompetitionRow {
        id: competition_id,
        event_submission: serde_json::to_vec(&json!({ "id": competition_id })).unwrap(),
        ..Default::default()
    };

    // Escrowed: the escrow's terms, and no ticket preimage before the payment settles.
    let records = recovery.entry_records(&unsigned, std::slice::from_ref(&ticket));
    assert_eq!(records.len(), 1);
    let (user, record) = &records[0];
    assert_eq!(*user, player.public_key());
    assert_eq!(record.entry_id, entry_id);
    assert_eq!(record.status, RecordStatus::Escrowed);
    assert_eq!(record.coordinator_pubkey, recovery.public_key().to_hex());
    assert_eq!(record.ticket.as_ref().unwrap().preimage, None);
    assert!(record.contract.is_none());
    let escrow_record = record.escrow.as_ref().unwrap();
    assert_eq!(escrow_record.kind, "ark");
    assert_eq!(escrow_record.arkd_url, "https://arkd.example");
    assert_eq!(escrow_record.server_pubkey, x_only(31).to_string());
    assert_eq!(escrow_record.refund_locktime, 1_800_000_000);
    assert_eq!(escrow_record.exit_delay_secs, 5_120);
    assert_eq!(escrow_record.unilateral_refund_delay_secs, 10_240);
    assert_eq!(escrow_record.amount_sat, Some(10_500));
    assert_eq!(escrow_record.created_at, 1_799_990_000);
    let leaves: Vec<String> = escrow
        .vtxo_script()
        .scripts()
        .iter()
        .map(|script| hex::encode(script.as_bytes()))
        .collect();
    assert_eq!(escrow_record.tap_tree, leaves);

    // Paid: the swap settled, so the player holds the preimage.
    let paid = RecoveryTicketRow {
        settled: true,
        ..ticket.clone()
    };
    let (_, record) = &recovery.entry_records(&unsigned, std::slice::from_ref(&paid))[0];
    assert_eq!(record.status, RecordStatus::Paid);
    assert_eq!(
        record.ticket.as_ref().unwrap().preimage.as_deref(),
        Some(hex::encode([11u8; 32]).as_str())
    );

    // In the contract: the player's slot and their pruned signatures, as dlctix prunes them.
    let signed = serde_json::to_value(&contract).unwrap();
    let mut competition = competition_row(competition_id, &params, &signed);
    let (_, record) = &recovery.entry_records(&competition, std::slice::from_ref(&paid))[0];
    assert_eq!(record.status, RecordStatus::InContract);
    let contract_record = record.contract.as_ref().unwrap();
    assert_eq!(contract_record.player_index, 1);
    assert_eq!(
        contract_record.funding_outpoint,
        OutPoint::null().to_string()
    );
    assert_eq!(contract_record.relative_locktime_block_delta, 72);
    assert_eq!(
        contract_record.contract_parameters_sha256,
        hex::encode(Sha256::digest(serde_json::to_vec(&params).unwrap()))
    );
    assert_eq!(
        contract_record.pruned_signatures,
        contract.pruned_signatures(players[1].base_point_mul())
    );
    for key in players {
        assert_eq!(
            pruned_signatures(&params, contract.all_signatures(), key.base_point_mul()),
            contract.pruned_signatures(key.base_point_mul())
        );
    }

    // The oracle attests outcome 0, which pays only player 0.
    competition.attestation =
        Some(serde_json::to_vec(&MaybeScalar::from(attestations[0])).unwrap());
    let (_, record) = &recovery.entry_records(&competition, std::slice::from_ref(&paid))[0];
    assert_eq!(record.status, RecordStatus::Lost);
    competition.attestation =
        Some(serde_json::to_vec(&MaybeScalar::from(attestations[1])).unwrap());
    let (_, record) = &recovery.entry_records(&competition, std::slice::from_ref(&paid))[0];
    assert_eq!(record.status, RecordStatus::Won);

    let settled = RecoveryTicketRow {
        paid_out: true,
        ..paid.clone()
    };
    let (_, record) = &recovery.entry_records(&competition, std::slice::from_ref(&settled))[0];
    assert_eq!(record.status, RecordStatus::Settled);
    let refunded = RecoveryTicketRow {
        refund_state: Some("settled".into()),
        ..ticket.clone()
    };
    let (_, record) = &recovery.entry_records(&unsigned, std::slice::from_ref(&refunded))[0];
    assert_eq!(record.status, RecordStatus::Refunded);

    // The competition event carries the contract as stored, with the attestation.
    let contents = recovery
        .competition_contents(&competition, Some("02aa"))
        .unwrap()
        .unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].0, format!("competition:{competition_id}"));
    let content: serde_json::Value = serde_json::from_str(&contents[0].1).unwrap();
    assert_eq!(content["type"], "competition");
    assert_eq!(content["network"], "signet");
    assert_eq!(content["signed_contract"], signed);
    assert_eq!(
        content["contract_parameters"],
        serde_json::to_value(&params).unwrap()
    );
    assert_eq!(content["oracle"]["pubkey"], "02aa");
    assert_eq!(content["oracle"]["expiry"], 1_900_000_000);
    assert_eq!(content["oracle"]["event_id"], json!(competition_id));
    assert_eq!(
        content["attestation"],
        hex::encode(MaybeScalar::from(attestations[1]).serialize())
    );
    let event = recovery
        .competition_event(
            competition_id,
            contents[0].0.clone(),
            contents[0].1.clone(),
            9,
        )
        .unwrap();
    event.verify().unwrap();
    assert!(tags(&event).contains(&vec!["c".to_string(), competition_id.to_string()]));
    assert!(tags(&event).iter().all(|tag| tag[0] != "b"));
}

#[test]
fn a_ticket_before_its_entry_takes_the_entry_id_from_its_payout_policy() {
    let recovery = recovery(vec![]);
    let player = Keys::new(secret(2));
    let entry_id = Uuid::now_v7();
    let terms = json!({ "entry_id": entry_id, "payout_hash": "00" }).to_string();
    let ticket = RecoveryTicketRow {
        ticket_id: Uuid::now_v7(),
        ticket_hash: "cd".repeat(32),
        ticket_preimage: "ef".repeat(32),
        reserved_by: Some(player.public_key().to_hex()),
        policy_entry_pubkey: Some(hex::encode(scalar(4).base_point_mul().serialize())),
        policy_json: Some(json!({ "contract_terms": "", "queued_entry": terms }).to_string()),
        ..Default::default()
    };
    let competition = RecoveryCompetitionRow {
        id: Uuid::now_v7(),
        event_submission: b"{}".to_vec(),
        ..Default::default()
    };
    let records = recovery.entry_records(&competition, std::slice::from_ref(&ticket));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].1.entry_id, entry_id);
    assert_eq!(records[0].1.status, RecordStatus::Ticketed);

    // No player or no entry id: no record.
    let anonymous = RecoveryTicketRow {
        reserved_by: None,
        ..ticket.clone()
    };
    let unnamed = RecoveryTicketRow {
        policy_json: None,
        ..ticket
    };
    assert!(recovery
        .entry_records(&competition, &[anonymous, unnamed])
        .is_empty());
}

// A pool of 20 players paying two places has 381 attested outcomes and 800 win conditions.
// Its contract event is far over a relay's usual limit, so it is compressed and split; each
// player's record keeps only their own signatures.
#[test]
fn a_twenty_player_two_place_contract_fits_in_events() {
    let recovery = recovery(vec![]);
    let players = 20;
    let outcomes = coordinator_escrow::oracle_statement::ranking_outcomes(players, 2);
    assert_eq!(outcomes.len(), 381);
    let keys: Vec<Scalar> = (0..players as u64).map(scalar).collect();
    let attestations: Vec<Scalar> = (0..outcomes.len() as u64)
        .map(|index| scalar(10_000 + index))
        .collect();
    let equal: PayoutWeights = (0..players).map(|player| (player, 1)).collect();
    let params = ContractParameters {
        market_maker: MarketMaker {
            pubkey: scalar(5_000).base_point_mul(),
        },
        players: keys
            .iter()
            .enumerate()
            .map(|(index, key)| Player {
                pubkey: key.base_point_mul(),
                ticket_hash: hashlock::sha256(&[index as u8, 1]),
                payout_hash: hashlock::sha256(&[index as u8, 2]),
            })
            .collect(),
        event: EventLockingConditions {
            locking_points: attestations
                .iter()
                .map(|key| key.base_point_mul().into())
                .collect(),
            expiry: Some(1_900_000_000),
        },
        outcome_payouts: outcomes
            .iter()
            .enumerate()
            .map(|(index, winners)| {
                let weights = if winners.len() == players {
                    equal.clone()
                } else {
                    winners
                        .iter()
                        .zip([60, 40])
                        .map(|(player, weight)| (*player, weight))
                        .collect()
                };
                (Outcome::Attestation(index), weights)
            })
            .chain(std::iter::once((Outcome::Expiry, equal.clone())))
            .collect(),
        fee_rate: FeeRate::from_sat_per_vb_u32(2),
        funding_value: Amount::from_sat(200_000),
        relative_locktime_block_delta: 432,
    };
    // Distinct signatures, so compression gains no more than it would on real ones.
    let signer = scalar(6_000);
    let signatures = ContractSignatures {
        expiry_tx_signature: Some(musig2::deterministic::sign_solo::<CompactSignature>(
            signer, b"expiry",
        )),
        outcome_tx_signatures: params
            .event
            .locking_points
            .iter()
            .enumerate()
            .map(|(index, point)| {
                (
                    index,
                    musig2::deterministic::adaptor::sign_solo(signer, index.to_be_bytes(), *point),
                )
            })
            .collect(),
        split_tx_signatures: params
            .all_win_conditions()
            .into_iter()
            .map(|condition| {
                (
                    condition,
                    musig2::deterministic::sign_solo::<CompactSignature>(
                        signer,
                        condition.to_string(),
                    ),
                )
            })
            .collect(),
    };
    assert_eq!(signatures.split_tx_signatures.len(), 800);
    let funding = OutPoint::from_str(&format!("{}:0", "07".repeat(32))).unwrap();
    let signed = json!({
        "signatures": signatures,
        "dlc": { "params": params, "funding_outpoint": funding },
    });
    let competition_id = Uuid::now_v7();
    let competition = competition_row(competition_id, &params, &signed);

    let contents = recovery
        .competition_contents(&competition, Some("02aa"))
        .unwrap()
        .unwrap();
    let joined =
        join_competition_contents(&contents.iter().map(|(_, c)| c.clone()).collect::<Vec<_>>())
            .unwrap();
    let sizes: Vec<usize> = contents.iter().map(|(_, content)| content.len()).collect();
    assert!(
        joined.len() > MAX_CONTENT_BYTES,
        "content {} bytes",
        joined.len()
    );
    assert!(contents.len() > 1, "{} bytes in {sizes:?}", joined.len());
    for (d_tag, content) in &contents {
        assert!(
            content.len() <= MAX_CONTENT_BYTES,
            "{d_tag}: {sizes:?}, {} bytes before compression",
            joined.len()
        );
    }
    assert_eq!(contents[0].0, format!("competition:{competition_id}"));
    for (index, (d_tag, _)) in contents.iter().enumerate().skip(1) {
        assert_eq!(*d_tag, format!("competition:{competition_id}:part:{index}"));
    }
    let content: serde_json::Value = serde_json::from_str(&joined).unwrap();
    assert_eq!(content["competition_id"], json!(competition_id));
    assert_eq!(content["signed_contract"], signed);
    assert_eq!(content["funding_outpoint"], funding.to_string());
    for (d_tag, content) in contents {
        recovery
            .competition_event(competition_id, d_tag, content, 9)
            .unwrap()
            .verify()
            .unwrap();
    }

    // Player 7 wins in 19 rankings as first and 19 as second, in the refund outcome and at
    // expiry.
    let player = Keys::new(secret(2));
    let ticket = RecoveryTicketRow {
        ticket_id: Uuid::now_v7(),
        ticket_hash: hex::encode(params.players[7].ticket_hash),
        ticket_preimage: "ab".repeat(32),
        settled: true,
        entry_id: Some(Uuid::now_v7()),
        entry_user: Some(player.public_key().to_hex()),
        entry_pubkey: Some(hex::encode(keys[7].base_point_mul().serialize())),
        ..Default::default()
    };
    let records = recovery.entry_records(&competition, std::slice::from_ref(&ticket));
    let (user, record) = &records[0];
    let contract_record = record.contract.as_ref().unwrap();
    assert_eq!(contract_record.player_index, 7);
    assert_eq!(contract_record.funding_outpoint, funding.to_string());
    let pruned = contract_record.pruned_signatures.as_ref().unwrap();
    assert_eq!(pruned.outcome_tx_signatures.len(), 39);
    assert_eq!(pruned.split_tx_signatures.len(), 40);
    assert!(pruned.expiry_tx_signature.is_some());
    let record_bytes = serde_json::to_string(record).unwrap().len();
    assert!(
        record_bytes < MAX_ENTRY_RECORD_BYTES,
        "entry record {record_bytes} bytes"
    );
    let event = recovery.entry_event(user, record, 9).unwrap();
    let plaintext =
        nip44::decrypt(player.secret_key(), &recovery.public_key(), &event.content).unwrap();
    let decrypted: EntryRecord = serde_json::from_str(&plaintext).unwrap();
    let decrypted = decrypted.contract.unwrap();
    assert_eq!(decrypted.pruned_signatures.as_ref(), Some(pruned));
}

#[test]
fn small_competition_contents_are_published_as_they_are() {
    let id = Uuid::now_v7();
    let content = json!({ "v": 1, "type": "competition", "competition_id": id }).to_string();
    let contents = split_competition_content(id, &content).unwrap();
    assert_eq!(
        contents,
        vec![(format!("competition:{id}"), content.clone())]
    );
    assert_eq!(
        join_competition_contents(&[content.clone()]).unwrap(),
        content
    );
}

#[tokio::test]
async fn the_outbox_retries_until_every_relay_takes_an_event() {
    let (store, _directory) = store().await;
    let steady_seen = Arc::new(AtomicUsize::new(0));
    let seen = steady_seen.clone();
    let steady = super::relay::test_relay::spawn(Arc::new(move |_: &serde_json::Value| {
        seen.fetch_add(1, SeqCst);
        Some((true, String::new()))
    }))
    .await;
    let flaky_seen = Arc::new(AtomicUsize::new(0));
    let seen = flaky_seen.clone();
    let flaky = super::relay::test_relay::spawn(Arc::new(move |_: &serde_json::Value| {
        let attempt = seen.fetch_add(1, SeqCst);
        Some((attempt > 0, "rate-limited: slow down".to_string()))
    }))
    .await;
    let relays = vec![steady.clone(), flaky.clone()];
    let recovery = recovery(relays.clone());
    let player = Keys::new(secret(2)).public_key();
    let event = recovery
        .wallet_event(&player, &recovery.wallet_record("blob"), 100)
        .unwrap();
    let d_tag = recovery.wallet_d_tag(&player);
    let now = 1_000;
    store
        .put_recovery_events(
            vec![outbox(d_tag.clone(), "wallet", Some(&player), None, &event)],
            now,
            None,
        )
        .await
        .unwrap();
    assert_eq!(store.recovery_outbox_depth().await.unwrap(), 1);

    // One relay took it and the other refused, so it waits for its retry.
    publish_due(&store, &relays, now).await.unwrap();
    assert_eq!(store.recovery_outbox_depth().await.unwrap(), 1);
    assert!(store.due_recovery_events(now, 10).await.unwrap().is_empty());
    let retry_at = now + retry_delay(1);
    let due = store.due_recovery_events(retry_at, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].attempts, 1);
    assert_eq!(due[0].accepted_relays, vec![steady.clone()]);

    // The retry goes only to the relay that refused.
    publish_due(&store, &relays, retry_at).await.unwrap();
    assert_eq!(store.recovery_outbox_depth().await.unwrap(), 0);
    assert_eq!(steady_seen.load(SeqCst), 1);
    assert_eq!(flaky_seen.load(SeqCst), 2);

    // A new version is due again for every relay. An attempt on the old version leaves it due.
    let newer = recovery
        .wallet_event(&player, &recovery.wallet_record("blob 2"), 101)
        .unwrap();
    store
        .put_recovery_events(
            vec![outbox(d_tag.clone(), "wallet", Some(&player), None, &newer)],
            retry_at,
            None,
        )
        .await
        .unwrap();
    store
        .record_recovery_attempts(vec![crate::domain::RecoveryAttempt {
            d_tag: d_tag.clone(),
            content_sha256: content_digest(&event.content),
            accepted_relays: relays.clone(),
            published_at: Some(retry_at),
            next_attempt_at: retry_at,
            error: None,
        }])
        .await
        .unwrap();
    let due = store.due_recovery_events(retry_at, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].attempts, 0);
    assert!(due[0].accepted_relays.is_empty());
    assert!(due[0].event_json.contains(&newer.id.to_hex()));
}

#[tokio::test]
async fn an_event_one_relay_never_takes_stops_after_its_attempts() {
    let (store, _directory) = store().await;
    let steady = super::relay::test_relay::spawn(Arc::new(|_: &serde_json::Value| {
        Some((true, String::new()))
    }))
    .await;
    // Nothing listens here.
    let unreachable = "ws://127.0.0.1:1".to_string();
    let relays = vec![steady, unreachable];
    let recovery = recovery(relays.clone());
    let player = Keys::new(secret(2)).public_key();
    let event = recovery
        .wallet_event(&player, &recovery.wallet_record("blob"), 100)
        .unwrap();
    store
        .put_recovery_events(
            vec![outbox(
                recovery.wallet_d_tag(&player),
                "wallet",
                Some(&player),
                None,
                &event,
            )],
            0,
            None,
        )
        .await
        .unwrap();
    let mut now = 0;
    for attempt in 1..=MAX_ATTEMPTS {
        assert_eq!(store.recovery_outbox_depth().await.unwrap(), 1, "{attempt}");
        publish_due(&store, &relays, now).await.unwrap();
        now += retry_delay(attempt);
    }
    assert_eq!(store.recovery_outbox_depth().await.unwrap(), 0);
}

#[tokio::test]
async fn a_recovery_file_holds_only_its_players_records() {
    let (store, _directory) = store().await;
    let recovery = recovery(vec!["wss://relay.example".into()]);
    let alice = Keys::new(secret(2));
    let bob = Keys::new(secret(3));
    let alice_competition = Uuid::now_v7();
    let bob_competition = Uuid::now_v7();
    let mut events = Vec::new();
    for (keys, competition) in [(&alice, alice_competition), (&bob, bob_competition)] {
        let user = keys.public_key();
        let record = EntryRecord {
            competition_id: competition,
            ..sample_record(&recovery, &user)
        };
        let entry = recovery.entry_event(&user, &record, 10).unwrap();
        events.push(outbox(
            recovery.entry_d_tag(&user, record.entry_id),
            "entry",
            Some(&user),
            Some(competition),
            &entry,
        ));
        let wallet = recovery
            .wallet_event(&user, &recovery.wallet_record("blob"), 10)
            .unwrap();
        events.push(outbox(
            recovery.wallet_d_tag(&user),
            "wallet",
            Some(&user),
            None,
            &wallet,
        ));
        let d_tag = format!("competition:{competition}");
        let content = json!({ "v": 1, "type": "competition", "competition_id": competition });
        let contract = recovery
            .competition_event(competition, d_tag.clone(), content.to_string(), 10)
            .unwrap();
        events.push(outbox(
            d_tag,
            "competition",
            None,
            Some(competition),
            &contract,
        ));
    }
    store
        .put_recovery_events(events, 10, Some(10))
        .await
        .unwrap();

    let rows = store
        .recovery_kit_events(&alice.public_key().to_hex())
        .await
        .unwrap();
    let kit = recovery
        .kit(&alice.public_key(), Some("alice blob"), &rows, 20)
        .unwrap();
    assert_eq!(kit.kit_type, "coordinator-recovery-kit");
    assert_eq!(kit.user_pubkey, alice.public_key().to_hex());
    assert_eq!(kit.coordinator_pubkey, recovery.public_key().to_hex());
    assert_eq!(kit.relays, vec!["wss://relay.example".to_string()]);
    assert_eq!(kit.entries.len(), 1);
    let record: EntryRecord = serde_json::from_str(
        &nip44::decrypt(alice.secret_key(), &recovery.public_key(), &kit.entries[0]).unwrap(),
    )
    .unwrap();
    assert_eq!(record.user_pubkey, alice.public_key().to_hex());
    assert_eq!(record.competition_id, alice_competition);
    assert!(nip44::decrypt(bob.secret_key(), &recovery.public_key(), &kit.entries[0]).is_err());
    assert_eq!(
        kit.competitions,
        vec![json!({ "v": 1, "type": "competition", "competition_id": alice_competition })]
    );
    let wallet: WalletRecord = serde_json::from_str(
        &nip44::decrypt(
            alice.secret_key(),
            &recovery.public_key(),
            kit.wallet.as_ref().unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(wallet.wallet_blob, "alice blob");
    let file = serde_json::to_string(&kit).unwrap();
    assert!(!file.contains(&bob_competition.to_string()));
    assert!(!file.contains(&bob.public_key().to_hex()));
    assert_eq!(
        kit_file_name("npub1qqqqqqqqqqqqqqqqqqqqqqq"),
        "coordinator-recovery-npub1qqqqqqqqqqq.json"
    );
}
