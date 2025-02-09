use anyhow::Result;
use dlctix::{
    bitcoin::Amount,
    musig2::secp256k1::{PublicKey, SecretKey},
};
use env_logger;
use log::{debug, info};
use nostr_sdk::ToBech32;
use server::{
    create_folder,
    domain::{CompetitionState, CompetitionStore, DBConnection},
    Bitcoin, BitcoinClient, BitcoinSettings, Coordinator, Ln, Oracle,
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
    generate_request_create_event, generate_test_entry, get_keys, MockLnClient, MockOracleClient,
    TestParticipant,
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
    pub bitcoin_client: Arc<dyn Bitcoin>,
    pub ln_mock: Arc<dyn Ln>,
    pub oracle_client: Arc<dyn Oracle>,
}

impl TestContext {
    pub async fn new(entries: usize) -> Result<Self> {
        setup_static_logger();
        let test_data_folder = "./test_data";
        create_folder(test_data_folder);

        // Setup real Bitcoin client
        let bitcoin_settings = BitcoinSettings {
            network: dlctix::bitcoin::Network::Regtest,
            esplora_url: "http://localhost:9102".to_string(),
            seed_path: "./test_data/fake_coordinator_private_key.pem".to_string(),
            storage_file: "./test_data/bitcoin.db".to_string(),
            refresh_blocks_secs: 15,
        };

        let bitcoin_client = BitcoinClient::new(&bitcoin_settings).await?;

        // Wait for initial sync
        bitcoin_client.sync().await?;

        // Verify we have sufficient balance for the test
        let balance = bitcoin_client.get_balance().await?;
        info!("Initial balance: {} sats", balance.total());
        assert!(
            balance.total() > Amount::from_sat(1_000_000),
            "Insufficient funds for test"
        );

        let test_db_name = format!("test_competition_db_{}", Uuid::now_v7());
        let db_connection = DBConnection::new(test_data_folder, &test_db_name)?;
        let competition_store = CompetitionStore::new(db_connection.clone())?;

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
            bitcoin_client: Arc::new(bitcoin_client),
            ln_mock: Arc::new(ln_mock),
            oracle_client: Arc::new(oracle_mock),
        })
    }

    pub async fn create_coordinator(&self) -> Result<Arc<Coordinator>> {
        let coordinator = Coordinator::new(
            self.oracle_client.clone(),
            self.competition_store.clone(),
            self.bitcoin_client.clone(),
            self.ln_mock.clone(),
            144,
            1,
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

/// Requires an esplora instance pointed at a bitcoind instance that has funds in it's wallet ready to be spent
/// Do not run this test in ci, should be run locally for development only

#[tokio::test]
#[ignore = "requires local regtest environment"]
async fn test_two_person_competition_flow_with_real_bitcoin() -> Result<()> {
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
        debug!("participant {} added: {}", i, nostr_pubkey.to_bech32()?);
        participants.push(TestParticipant::new(
            wallet,
            nostr_pubkey.to_bech32()?,
            Uuid::now_v7(),
        ));
    }

    // Generate and submit entries
    let mut participant_entry_indices = HashMap::new();
    let mut participants_entries = HashMap::new();
    for (i, participant) in participants.iter_mut().enumerate() {
        let entry = generate_test_entry(
            create_event.id,
            &mut participant.wallet,
            &participant.nostr_pubkey,
            &create_event.locations,
            i as u32,
            participant.ticket_id,
        )
        .await?;
        participants_entries.insert(participant.nostr_pubkey.clone(), entry.clone());
        participant_entry_indices.insert(participant.nostr_pubkey.clone(), i as u32);

        debug!(
            "Submitting entry for participant {} with pubkey: {}",
            i, participant.nostr_pubkey
        );
        coordinator
            .add_entry(participant.nostr_pubkey.clone(), entry)
            .await?;
    }

    // Create transactions
    coordinator.competition_handler().await?;
    let competition = coordinator.get_competition(create_event.id).await?;
    let contract_params = competition.contract_parameters.as_ref().unwrap();

    // Map participants to contract indices
    let mut pubkey_to_contract_idx = HashMap::new();
    for participant in &participants {
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
    }

    // Sort participants by contract index
    let mut ordered_participants = participants;
    ordered_participants.sort_by_key(|p| pubkey_to_contract_idx[&p.nostr_pubkey]);

    // Process nonces
    for participant in &mut ordered_participants {
        let entry_index = participant_entry_indices[&participant.nostr_pubkey];
        participant.wallet.add_contract(
            entry_index,
            competition.contract_parameters.clone().unwrap(),
            competition.funding_outpoint.unwrap(),
        )?;

        let nonces = participant.wallet.generate_public_nonces(entry_index)?;
        let entry = participants_entries.get(&participant.nostr_pubkey).unwrap();

        coordinator
            .submit_public_nonces(
                participant.nostr_pubkey.clone(),
                competition.id,
                entry.id,
                nonces,
            )
            .await?;
    }

    // Generate aggregate nonces
    coordinator.competition_handler().await?;
    let competition = coordinator.get_competition(create_event.id).await?;
    let agg_nonces = competition.aggregated_nonces.clone().unwrap();

    // Submit signatures
    for participant in &ordered_participants {
        let entry_index = participant_entry_indices[&participant.nostr_pubkey];
        let entry = participants_entries.get(&participant.nostr_pubkey).unwrap();

        let partial_sigs = participant
            .wallet
            .sign_aggregate_nonces(agg_nonces.clone(), entry_index)?;

        coordinator
            .submit_partial_signatures(
                participant.nostr_pubkey.clone(),
                competition.id,
                entry.id,
                partial_sigs,
            )
            .await?;
    }

    // Final handler run to broadcast
    coordinator.competition_handler().await?;

    // Verify final state
    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("Final competition state: {:?}", competition.get_state());
    debug!("Competition errors: {:?}", competition.errors);

    assert_eq!(
        competition.get_state(),
        CompetitionState::FundingBroadcasted
    );
    assert!(competition.signed_contract.is_some());
    assert!(competition.signed_at.is_some());

    Ok(())
}
