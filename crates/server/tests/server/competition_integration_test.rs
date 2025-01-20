use anyhow::Result;
use dlctix::{
    bitcoin::OutPoint,
    musig2::secp256k1::{PublicKey, SecretKey},
    secp::Point,
    Outcome,
};
use env_logger;
use log::{debug, error, info};
use maplit::hashmap;
use nostr_sdk::ToBech32;
use server::{
    create_folder,
    domain::{CompetitionState, CompetitionStore, DBConnection},
    Bitcoin, Coordinator, Ln, Oracle,
};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{Arc, Once},
};
use uuid::Uuid;

use crate::helpers::{
    create_test_nostr_client, create_test_participants, create_test_wallet, generate_oracle_event,
    generate_request_create_event, generate_test_entry, get_oracle_keys, MockBitcoinClient,
    MockLnClient, MockOracleClient, TestParticipant,
};

static INIT_LOGGER: Once = Once::new();

pub fn setup_static_logger() {
    INIT_LOGGER.call_once(|| {
        env_logger::init();
    });
}

pub struct TestContext {
    pub oracle_keys: (PublicKey, SecretKey),
    pub test_database: String,
    pub competition_store: CompetitionStore,
    pub bitcoin_mock: Arc<dyn Bitcoin>,
    pub ln_mock: Arc<dyn Ln>,
    pub oracle_client: Arc<dyn Oracle>,
}

impl TestContext {
    pub async fn new(entries: usize) -> Result<Self> {
        setup_static_logger();
        let test_data_folder = "./test_data";
        create_folder(test_data_folder);
        let test_db_name = format!("test_competition_db_{}", Uuid::now_v7());
        let db_connection =
            DBConnection::new(test_data_folder, &test_db_name).expect("Failed to create test DB");

        let competition_store = CompetitionStore::new(db_connection.clone())
            .expect("Failed to create competition store");

        let mut bitcoin_mock = MockBitcoinClient::new();
        bitcoin_mock
            .expect_get_spendable_utxo()
            .returning(|_| Ok(OutPoint::default()));
        bitcoin_mock
            .expect_get_estimated_fee_rates()
            .returning(|| Ok(hashmap! { 1 => 1.0, 2 => 2.0 }));
        bitcoin_mock.expect_broadcast().returning(|_| Ok(()));

        //TODO: add to the expected as we add calls to the ln client in the code
        let ln_mock = MockLnClient::new();
        let oracle_keys = get_oracle_keys(String::from("./test_data/fake_oracle_private_key.pem"));
        let mut oracle_mock = MockOracleClient::new();
        oracle_mock.expect_create_event().returning(move |event| {
            Ok(generate_oracle_event(
                oracle_keys.0.clone(),
                event.id,
                event.total_allowed_entries,
                event.number_of_places_win,
            ))
        });
        oracle_mock
            .expect_submit_entry()
            .times(entries)
            .returning(|_| Ok(()));

        Ok(Self {
            oracle_keys,
            test_database: db_connection.connection_path,
            competition_store,
            bitcoin_mock: Arc::new(bitcoin_mock),
            ln_mock: Arc::new(ln_mock),
            oracle_client: Arc::new(oracle_mock),
        })
    }

    pub async fn create_coordinator(&self) -> Result<Arc<Coordinator>> {
        let test_cert_path = String::from("./test_data/fake_coordinator_private_key.pem");
        let coordinator = Coordinator::new(
            self.oracle_client.clone(),
            self.competition_store.clone(),
            self.bitcoin_mock.clone(),
            self.ln_mock.clone(),
            &test_cert_path,
            144,
        )
        .await?;

        Ok(Arc::new(coordinator))
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        let test_database = self.test_database.clone();
        let path = Path::new(&test_database);
        if let Err(e) = fs::remove_file(path) {
            eprintln!("Failed to cleanup test database: {}", e);
        }
        assert!(!path.exists(), "Test database was not cleaned up properly");
    }
}
/*
#[tokio::test]
async fn test_key_derivation_matches() -> Result<()> {
    let context = TestContext::new(1).await?;
    let coordinator = context.create_coordinator().await?;

    // Create competition
    let create_event = generate_request_create_event(1, 1, 1); // Just 1 location for simplicity
    coordinator.create_competition(create_event.clone()).await?;

    // Create a single test participant
    let mut participants = create_test_participants(1).await?;

    // Generate entry for participant
    // Create test participants and entries
    let mut participants = Vec::new();
    for i in 0..1 {
        let nostr_client = create_test_nostr_client().await;
        let wallet = create_test_wallet(&nostr_client).await;
        let nostr_pubkey = nostr_client.get_public_key().await?;
        let nostr_bech_32 = nostr_pubkey.to_bech32().unwrap();
        debug!("participant {} added: {}", i, nostr_bech_32);
        participants.push(TestParticipant::new(
            wallet,
            nostr_pubkey.to_bech32().unwrap(),
        ));
    }

    let participant = participants.get_mut(0).unwrap();
    // Get the entry we just created
    let entry = participants
        .get(&participant.nostr_pubkey)
        .expect("Entry should exist for participant");

    // Get the pubkey stored in the entry
    let stored_pubkey = entry.ephemeral_pubkey.clone();

    // Add entry to coordinator
    coordinator
        .add_entry(participant.nostr_pubkey.clone(), entry.clone())
        .await?;

    // Let the competition handler run to create transactions
    coordinator.competition_handler().await?;

    // Get updated competition
    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("competiton {:?}", competition);
    assert!(competition.contract_parameters.is_some());
    assert!(competition.funding_transaction.is_some());

    // Add contract to wallet
    participant.wallet.add_contract(
        1, // Using 1 as the entry index since we only have one entry
        competition.contract_parameters.clone().unwrap(),
        competition.funding_transaction.unwrap(),
    )?;

    // Generate nonces - this will use the derived key
    let nonces = participant.wallet.generate_public_nonces(1)?;

    // Get the pubkey directly from wallet for comparison
    let derived_pubkey = participant.wallet.get_dlc_public_key(1).await?;

    // Compare the stored pubkey with derived pubkey
    assert_eq!(
        stored_pubkey, derived_pubkey,
        "Stored pubkey {} does not match derived pubkey {}",
        stored_pubkey, derived_pubkey
    );

    // Add more detailed debugging output
    debug!(
        "Stored pubkey: {}\nDerived pubkey: {}\nNonces generated successfully: {:?}",
        stored_pubkey, derived_pubkey, nonces
    );

    Ok(())
}
#[tokio::test]
async fn test_player_indices_and_payouts_match() -> Result<()> {
    let context = TestContext::new(3).await?;
    let coordinator = context.create_coordinator().await?;

    // Create competition
    let create_event = generate_request_create_event(1, 3, 2); // Just 1 location for simplicity
    coordinator.create_competition(create_event.clone()).await?;

    // Create test participants and entries
    let mut participants = Vec::new();
    for i in 0..3 {
        let nostr_client = create_test_nostr_client().await;
        let wallet = create_test_wallet(&nostr_client).await;
        let nostr_pubkey = nostr_client.get_public_key().await?;
        let nostr_bech_32 = nostr_pubkey.to_bech32().unwrap();
        debug!("participant {} added: {}", i, nostr_bech_32);
        participants.push(TestParticipant::new(
            wallet,
            nostr_pubkey.to_bech32().unwrap(),
        ));
    }

    // Submit entries and store pubkeys in order
    let mut submitted_pubkeys = Vec::new();
    for (participant_key, entry) in participants_entries.iter() {
        coordinator
            .add_entry(participant_key.clone(), entry.clone())
            .await?;
        submitted_pubkeys.push(entry.ephemeral_pubkey.clone());
    }

    coordinator.competition_handler().await?;
    let competition = coordinator.get_competition(create_event.id).await?;
    let contract_params = competition.contract_parameters.unwrap();

    // Check each attestation outcome
    let mut seen_outcomes = Vec::new();
    for (outcome, weights) in contract_params.outcome_payouts.iter() {
        match outcome {
            Outcome::Attestation(index) => {
                debug!("Outcome Attestation({}) weights: {:?}", index, weights);

                // Create a representation of this outcome that includes both players and their weights
                let mut outcome_representation: Vec<(usize, u64)> = weights
                    .iter()
                    .map(|(&player, &weight)| (player, weight))
                    .collect();
                outcome_representation.sort_by_key(|&(player, _)| player);

                // Verify we haven't seen this exact outcome before
                assert!(
                    !seen_outcomes.contains(&outcome_representation),
                    "Duplicate outcome found: {:?}",
                    outcome_representation
                );
                seen_outcomes.push(outcome_representation);

                // Ensure weights sum to 100 (60% + 40% for 2 places)
                let total_weight: u64 = weights.values().sum();
                assert_eq!(
                    total_weight, 100,
                    "Total weight for outcome {} should be 100 (percentage-based), got {}",
                    index, total_weight
                );

                // Verify we have exactly 2 winners
                assert_eq!(
                    weights.len(),
                    2,
                    "Should have exactly 2 winners for each outcome"
                );

                // Verify one weight is 60 and one is 40
                let mut weight_values: Vec<_> = weights.values().copied().collect();
                weight_values.sort_unstable_by(|a, b| b.cmp(a));
                assert_eq!(
                    weight_values,
                    vec![60, 40],
                    "Weights should be 60 for first place and 40 for second place"
                );
            }
            Outcome::Expiry => {
                let player_count = weights.len() as u64;
                let base_weight = 100 / player_count;
                let remainder = 100 % player_count;

                // Check each weight is either base_weight or base_weight + 1
                let mut remainder_count = 0;
                for weight in weights.values() {
                    assert!(
                        *weight == base_weight || *weight == base_weight + 1,
                        "Expiry weights should be either {} or {}",
                        base_weight,
                        base_weight + 1
                    );
                    if *weight == base_weight + 1 {
                        remainder_count += 1;
                    }
                }

                // Verify we distributed the remainder correctly
                assert_eq!(
                    remainder_count, remainder as usize,
                    "Number of increased weights should match remainder"
                );

                // Verify total is exactly 100
                let total_weight: u64 = weights.values().sum();
                assert_eq!(
                    total_weight, 100,
                    "Total weight for expiry outcome should be 100%"
                );
            }
        }
    }

    Ok(())
}
*/
const TEST_ENTRY_INDEX: u32 = 0;

#[tokio::test]
async fn test_four_person_competition_flow() -> Result<()> {
    let context = TestContext::new(4).await?;
    let coordinator = context.create_coordinator().await?;

    // Create competition
    let create_event = generate_request_create_event(1, 4, 2);
    info!("create event request completed");
    let competition = coordinator.create_competition(create_event.clone()).await?;
    info!("create competition completed");
    assert_eq!(competition.get_state(), CompetitionState::Created);

    // Create 4 participants
    let mut participants = Vec::new();
    for i in 0..4 {
        let nostr_client = create_test_nostr_client().await;
        let wallet = create_test_wallet(&nostr_client).await;
        let nostr_pubkey = nostr_client.get_public_key().await?;
        let nostr_bech_32 = nostr_pubkey.to_bech32().unwrap();
        debug!("participant {} added: {}", i, nostr_bech_32);
        participants.push(TestParticipant::new(
            wallet,
            nostr_pubkey.to_bech32().unwrap(),
        ));
    }
    info!("4 participants created");

    // Generate entries for all participants
    let mut participants_entries = HashMap::new();
    for participant in &mut participants {
        let entry = generate_test_entry(
            create_event.id,
            &mut participant.wallet,
            &participant.nostr_pubkey,
            &create_event.locations,
            TEST_ENTRY_INDEX, // Fixed entry index
        )
        .await?;
        participants_entries.insert(participant.nostr_pubkey.clone(), entry);
    }
    info!("Generated 1 entry for each participants");

    // Submit entries and store the mapping of pubkey to contract index
    let mut pubkey_to_contract_idx = HashMap::new();
    for (i, participant) in participants.iter().enumerate() {
        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        debug!(
            "Submitting entry {} with pubkey: {}",
            i, entry.ephemeral_pubkey
        );
        let user_entry = coordinator
            .add_entry(participant.nostr_pubkey.clone(), entry.clone())
            .await?;
        debug!("Entry {} added with id: {}", i, user_entry.id);
    }

    // Let competition handler run to create transactions
    debug!("Running competition handler to create transactions");
    coordinator.competition_handler().await?;

    // Get competition with contract parameters
    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("Contract parameters: {:?}", competition.contract_parameters);
    let contract_params = competition.contract_parameters.as_ref().unwrap();

    // Map participants to their contract indices
    for participant in &participants {
        let derived_pubkey = participant
            .wallet
            .get_dlc_public_key(TEST_ENTRY_INDEX)
            .await?;
        let contract_idx = contract_params
            .players
            .iter()
            .position(|p| p.pubkey.to_string() == derived_pubkey)
            .expect("Participant should be in contract");
        pubkey_to_contract_idx.insert(participant.nostr_pubkey.clone(), contract_idx);
        debug!(
            "Participant with pubkey {} is at contract index {}",
            participant.nostr_pubkey, contract_idx
        );
    }

    debug!(
        "Competition state after contract creation: {:?}",
        competition.get_state()
    );

    // Process participants in contract index order
    let mut ordered_participants = participants;
    ordered_participants.sort_by_key(|p| pubkey_to_contract_idx[&p.nostr_pubkey]);

    for participant in &mut ordered_participants {
        let contract_idx = pubkey_to_contract_idx[&participant.nostr_pubkey];
        debug!(
            "Processing participant {} with pubkey {}",
            contract_idx, participant.nostr_pubkey
        );

        // Add contract to wallet
        participant.wallet.add_contract(
            TEST_ENTRY_INDEX, // Fixed entry index
            competition.contract_parameters.clone().unwrap(),
            competition.funding_transaction.unwrap(),
        )?;
        debug!("Added contract to participant {} wallet", contract_idx);

        // Generate and submit nonces
        let nonces = participant
            .wallet
            .generate_public_nonces(TEST_ENTRY_INDEX)?;
        debug!("Generated nonces for participant {}", contract_idx);

        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        coordinator
            .submit_public_nonces(
                participant.nostr_pubkey.clone(),
                competition.id,
                entry.id,
                nonces,
            )
            .await?;
        debug!("Submitted nonces for participant {}", contract_idx);
    }

    // Run handler to generate aggregate nonces
    debug!("Running handler to generate aggregate nonces");
    coordinator.competition_handler().await?;

    // Get competition with aggregate nonces
    let competition = coordinator.get_competition(create_event.id).await?;
    debug!(
        "Competition state after nonce aggregation: {:?}",
        competition.get_state()
    );

    // Submit partial signatures in the same order
    let agg_nonces = competition.aggregated_nonces.clone().unwrap();
    for participant in &ordered_participants {
        let contract_idx = pubkey_to_contract_idx[&participant.nostr_pubkey];
        debug!(
            "Generating partial signatures for participant {}",
            contract_idx
        );

        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        let partial_sigs = participant
            .wallet
            .sign_aggregate_nonces(agg_nonces.clone(), TEST_ENTRY_INDEX)?;
        debug!(
            "Generated partial signatures for participant {}",
            contract_idx
        );

        coordinator
            .submit_partial_signatures(
                participant.nostr_pubkey.clone(),
                competition.id,
                entry.id,
                partial_sigs,
            )
            .await?;
        debug!(
            "Submitted partial signatures for participant {}",
            contract_idx
        );
    }

    // Final competition handler run
    debug!("Running final competition handler");
    coordinator.competition_handler().await?;

    // Get final competition state
    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("Final competition state: {:?}", competition.get_state());
    debug!("Competition errors: {:?}", competition.errors);

    assert_eq!(competition.get_state(), CompetitionState::Broadcasted);
    assert!(competition.signed_contract.is_some());
    assert!(competition.signed_at.is_some());

    Ok(())
}
