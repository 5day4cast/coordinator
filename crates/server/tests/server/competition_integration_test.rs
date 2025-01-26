use anyhow::Result;
use bdk_wallet::{
    bitcoin::secp256k1::Secp256k1,
    chain::{ChainPosition, ConfirmationBlockTime},
    AddressInfo, KeychainKind, LocalOutput,
};
use dlctix::{
    bitcoin::{Address, Amount, KnownHrp, OutPoint, ScriptBuf, TxOut},
    convert_xonly_key,
    musig2::secp256k1::{PublicKey, SecretKey},
    secp::Point,
};
use env_logger;
use log::debug;
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
    create_test_nostr_client, create_test_wallet, generate_oracle_event,
    generate_request_create_event, generate_test_entry, get_keys, MockBitcoinClient, MockLnClient,
    MockOracleClient, TestParticipant,
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
        bitcoin_mock.expect_get_spendable_utxo().returning(|_| {
            Ok(LocalOutput {
                outpoint: OutPoint::default(),
                txout: TxOut {
                    value: Amount::from_sat(10000000),
                    script_pubkey: ScriptBuf::new(),
                },
                keychain: bdk_wallet::KeychainKind::External,
                is_spent: false,
                chain_position: ChainPosition::Confirmed {
                    anchor: ConfirmationBlockTime::default(),
                    transitively: None,
                },
                derivation_index: 0,
            })
        });
        bitcoin_mock
            .expect_get_estimated_fee_rates()
            .returning(|| Ok(hashmap! { 1 => 1.0, 2 => 2.0 }));
        bitcoin_mock.expect_broadcast().returning(|_| Ok(()));
        bitcoin_mock.expect_get_derived_private_key().returning(|| {
            let key = get_keys(String::from("./test_data/fake_coordinator_private_key.pem"));
            Ok(key.1)
        });
        bitcoin_mock.expect_sign_psbt().returning(|_, _| Ok(true));

        bitcoin_mock.expect_get_next_address().returning(|| {
            let (public, _private) =
                get_keys(String::from("./test_data/fake_coordinator_private_key.pem"));
            let (x_only_public_key, _parity) = public.x_only_public_key();

            let secp = Secp256k1::new();
            let address = Address::p2tr(
                &secp,
                convert_xonly_key(x_only_public_key),
                None,
                KnownHrp::Regtest,
            );

            Ok(AddressInfo {
                address,
                index: 0,
                keychain: KeychainKind::External,
            })
        });
        //TODO: add to the expected as we add calls to the ln client in the code
        let ln_mock = MockLnClient::new();
        let oracle_keys = get_keys(String::from("./test_data/fake_oracle_private_key.pem"));
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
        let coordinator = Coordinator::new(
            self.oracle_client.clone(),
            self.competition_store.clone(),
            self.bitcoin_mock.clone(),
            self.ln_mock.clone(),
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

#[tokio::test]
async fn test_dlc_public_key_format() -> Result<()> {
    let nostr_client = create_test_nostr_client().await;
    let wallet = create_test_wallet(&nostr_client).await;

    let pubkey = wallet.get_dlc_public_key(0).await.unwrap();

    // Verify format
    assert!(pubkey.starts_with("02"), "Public key should start with 02");
    assert_eq!(
        pubkey.len(),
        66,
        "Public key should be 33 bytes (66 hex chars including prefix)"
    );

    // Verify it can be parsed by Point
    Point::from_hex(&pubkey).expect("Should parse as Point");

    Ok(())
}

#[tokio::test]
async fn test_two_person_competition_flow() -> Result<()> {
    let players = 2;
    let ranks = 1;
    let locations = 1;
    let context = TestContext::new(players).await?;
    let coordinator = context.create_coordinator().await?;

    // Create competition
    let create_event = generate_request_create_event(locations, players, ranks);
    debug!("create event request completed");
    let competition = coordinator.create_competition(create_event.clone()).await?;
    debug!("create competition completed");
    assert_eq!(competition.get_state(), CompetitionState::Created);

    let mut participants = Vec::new();
    for i in 0..players {
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
    debug!("participants created");

    // Generate entries for all participants
    let mut participant_entry_indices = HashMap::new(); // Add this to store entry indices
    let mut participants_entries = HashMap::new();
    for (i, participant) in participants.iter_mut().enumerate() {
        let entry_index = i as u32;
        let entry = generate_test_entry(
            create_event.id,
            &mut participant.wallet,
            &participant.nostr_pubkey,
            &create_event.locations,
            i as u32, // Fixed entry index
        )
        .await?;
        participants_entries.insert(participant.nostr_pubkey.clone(), entry);
        participant_entry_indices.insert(participant.nostr_pubkey.clone(), entry_index);
    }
    debug!("Generated 1 entry for each participants");

    // Submit entries and store the mapping of pubkey to contract index
    let mut pubkey_to_contract_idx = HashMap::new();
    for participant in &participants {
        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        debug!(
            "Submitting entry {} with pubkey: {}",
            participant_entry_indices[&participant.nostr_pubkey], entry.ephemeral_pubkey
        );
        let user_entry = coordinator
            .add_entry(participant.nostr_pubkey.clone(), entry.clone())
            .await?;
        debug!(
            "Entry {} added with id: {}",
            participant_entry_indices[&participant.nostr_pubkey], user_entry.id
        );
    }

    // Let competition handler run to create transactions
    debug!("Running competition handler to create transactions");
    coordinator.competition_handler().await?;

    // Get competition with contract parameters
    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("competition.error: {:?}", competition);
    debug!("Contract parameters: {:?}", competition.contract_parameters);
    let contract_params = competition.contract_parameters.as_ref().unwrap();

    // Map participants to their contract indices
    for participant in participants.iter() {
        let derived_pubkey = participant
            .wallet
            .get_dlc_public_key(participant_entry_indices[&participant.nostr_pubkey])
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
        let entry_index = participant_entry_indices[&participant.nostr_pubkey];

        debug!(
            "Processing participant {} with pubkey {}",
            contract_idx, participant.nostr_pubkey
        );

        // Add contract to wallet
        participant.wallet.add_contract(
            entry_index,
            competition.contract_parameters.clone().unwrap(),
            competition.funding_outpoint.unwrap(),
        )?;
        debug!("Added contract to participant {} wallet", contract_idx);

        let nonces = participant.wallet.generate_public_nonces(entry_index)?;
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
        let entry_index = participant_entry_indices[&participant.nostr_pubkey];

        debug!(
            "Generating partial signatures for participant {}",
            contract_idx
        );

        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        let partial_sigs = participant
            .wallet
            .sign_aggregate_nonces(agg_nonces.clone(), entry_index)?;
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

#[tokio::test]
async fn test_four_person_competition_flow() -> Result<()> {
    let players = 4;
    let ranks = 1;
    let locations = 3;
    let context = TestContext::new(players).await?;
    let coordinator = context.create_coordinator().await?;

    // Create competition
    let create_event = generate_request_create_event(locations, players, ranks);
    debug!("create event request completed");
    let competition = coordinator.create_competition(create_event.clone()).await?;
    debug!("create competition completed");
    assert_eq!(competition.get_state(), CompetitionState::Created);

    let mut participants = Vec::new();
    for i in 0..players {
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
    debug!("participants created");

    // Generate entries for all participants
    let mut participant_entry_indices = HashMap::new(); // Add this to store entry indices
    let mut participants_entries = HashMap::new();
    for (i, participant) in participants.iter_mut().enumerate() {
        let entry_index = i as u32;
        let entry = generate_test_entry(
            create_event.id,
            &mut participant.wallet,
            &participant.nostr_pubkey,
            &create_event.locations,
            i as u32, // Fixed entry index
        )
        .await?;
        participants_entries.insert(participant.nostr_pubkey.clone(), entry);
        participant_entry_indices.insert(participant.nostr_pubkey.clone(), entry_index);
    }
    debug!("Generated 1 entry for each participants");

    // Submit entries and store the mapping of pubkey to contract index
    let mut pubkey_to_contract_idx = HashMap::new();
    for participant in &participants {
        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        debug!(
            "Submitting entry {} with pubkey: {}",
            participant_entry_indices[&participant.nostr_pubkey], entry.ephemeral_pubkey
        );
        let user_entry = coordinator
            .add_entry(participant.nostr_pubkey.clone(), entry.clone())
            .await?;
        debug!(
            "Entry {} added with id: {}",
            participant_entry_indices[&participant.nostr_pubkey], user_entry.id
        );
    }

    // Let competition handler run to create transactions
    debug!("Running competition handler to create transactions");
    coordinator.competition_handler().await?;

    // Get competition with contract parameters
    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("competition.error: {:?}", competition);
    debug!("Contract parameters: {:?}", competition.contract_parameters);
    let contract_params = competition.contract_parameters.as_ref().unwrap();

    // Map participants to their contract indices
    for participant in participants.iter() {
        let derived_pubkey = participant
            .wallet
            .get_dlc_public_key(participant_entry_indices[&participant.nostr_pubkey])
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
        let entry_index = participant_entry_indices[&participant.nostr_pubkey];

        debug!(
            "Processing participant {} with pubkey {}",
            contract_idx, participant.nostr_pubkey
        );

        // Add contract to wallet
        participant.wallet.add_contract(
            entry_index,
            competition.contract_parameters.clone().unwrap(),
            competition.funding_outpoint.unwrap(),
        )?;
        debug!("Added contract to participant {} wallet", contract_idx);

        let nonces = participant.wallet.generate_public_nonces(entry_index)?;
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
        let entry_index = participant_entry_indices[&participant.nostr_pubkey];

        debug!(
            "Generating partial signatures for participant {}",
            contract_idx
        );

        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        let partial_sigs = participant
            .wallet
            .sign_aggregate_nonces(agg_nonces.clone(), entry_index)?;
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

#[tokio::test]
#[ignore = "slow and large test, should never run in ci, sanity check for local dev"]
async fn test_ten_person_competition_flow() -> Result<()> {
    let players = 10;
    let ranks = 3;
    let locations = 3;
    let context = TestContext::new(players).await?;
    let coordinator = context.create_coordinator().await?;

    // Create competition
    let create_event = generate_request_create_event(locations, players, ranks);
    debug!("create event request completed");
    let competition = coordinator.create_competition(create_event.clone()).await?;
    debug!("create competition completed");
    assert_eq!(competition.get_state(), CompetitionState::Created);

    let mut participants = Vec::new();
    for i in 0..players {
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
    debug!("participants created");

    // Generate entries for all participants
    let mut participant_entry_indices = HashMap::new(); // Add this to store entry indices
    let mut participants_entries = HashMap::new();
    for (i, participant) in participants.iter_mut().enumerate() {
        let entry_index = i as u32;
        let entry = generate_test_entry(
            create_event.id,
            &mut participant.wallet,
            &participant.nostr_pubkey,
            &create_event.locations,
            i as u32, // Fixed entry index
        )
        .await?;
        participants_entries.insert(participant.nostr_pubkey.clone(), entry);
        participant_entry_indices.insert(participant.nostr_pubkey.clone(), entry_index);
    }
    debug!("Generated 1 entry for each participants");

    // Submit entries and store the mapping of pubkey to contract index
    let mut pubkey_to_contract_idx = HashMap::new();
    for participant in &participants {
        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        debug!(
            "Submitting entry {} with pubkey: {}",
            participant_entry_indices[&participant.nostr_pubkey], entry.ephemeral_pubkey
        );
        let user_entry = coordinator
            .add_entry(participant.nostr_pubkey.clone(), entry.clone())
            .await?;
        debug!(
            "Entry {} added with id: {}",
            participant_entry_indices[&participant.nostr_pubkey], user_entry.id
        );
    }

    // Let competition handler run to create transactions
    debug!("Running competition handler to create transactions");
    coordinator.competition_handler().await?;

    // Get competition with contract parameters
    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("competition.error: {:?}", competition);
    debug!("Contract parameters: {:?}", competition.contract_parameters);
    let contract_params = competition.contract_parameters.as_ref().unwrap();

    // Map participants to their contract indices
    for participant in participants.iter() {
        let derived_pubkey = participant
            .wallet
            .get_dlc_public_key(participant_entry_indices[&participant.nostr_pubkey])
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
        let entry_index = participant_entry_indices[&participant.nostr_pubkey];

        debug!(
            "Processing participant {} with pubkey {}",
            contract_idx, participant.nostr_pubkey
        );

        // Add contract to wallet
        participant.wallet.add_contract(
            entry_index,
            competition.contract_parameters.clone().unwrap(),
            competition.funding_outpoint.unwrap(),
        )?;
        debug!("Added contract to participant {} wallet", contract_idx);

        let nonces = participant.wallet.generate_public_nonces(entry_index)?;
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
        let entry_index = participant_entry_indices[&participant.nostr_pubkey];

        debug!(
            "Generating partial signatures for participant {}",
            contract_idx
        );

        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        let partial_sigs = participant
            .wallet
            .sign_aggregate_nonces(agg_nonces.clone(), entry_index)?;
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
