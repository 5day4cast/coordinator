use anyhow::Result;
use dlctix::{
    bitcoin::OutPoint,
    musig2::secp256k1::{PublicKey, SecretKey},
};
use env_logger;
use log::{debug, error};
use maplit::hashmap;
use server::{
    create_folder,
    domain::{CompetitionState, CompetitionStore, DBConnection},
    Bitcoin, Coordinator, Ln, Oracle,
};
use std::{
    fs,
    path::Path,
    sync::{Arc, Once},
};
use uuid::Uuid;

use crate::helpers::{
    create_test_participants, generate_competition_entries, generate_oracle_event,
    generate_request_create_event, get_oracle_keys, MockBitcoinClient, MockLnClient,
    MockOracleClient,
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
    pub async fn new() -> Result<Self> {
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
            .times(10)
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

#[tokio::test]
#[ignore = "Should be used to debug issues of dlc creation, very large and slow, shouldn't run in CI"]
async fn test_ten_person_competition_flow() -> Result<()> {
    let context = TestContext::new().await?;
    let coordinator = context.create_coordinator().await?;

    // Create competition
    let create_event = generate_request_create_event(3); // 3 locations
    debug!("create event request completed");
    let competition = coordinator.create_competition(create_event.clone()).await?;
    debug!("create competition completed");
    assert_eq!(competition.get_state(), CompetitionState::Created);

    // Create 10 participants
    let mut participants = create_test_participants(10).await?;
    debug!("10 participants created");

    // Generate entries for all participants
    let participants_entries = generate_competition_entries(
        create_event.id,
        &mut participants,
        &create_event.locations,
        1, // one entry per participant
    )
    .await?;
    debug!("Generated 1 entry for each participants");

    // Submit entries for all participants
    for (participants_key, entry) in participants_entries.clone() {
        let user_entry = coordinator.add_entry(participants_key, entry).await?;
        // Verify entry was added
        assert!(user_entry.id != Uuid::nil());
    }
    debug!("Add 1 entry for each participants");

    // Let the competition handler run to create transactions
    coordinator
        .competition_handler()
        .await
        .map_err(|e| error!("{}", e))
        .unwrap();
    debug!("Created DLC contract and starting musig2 process");

    // Get updated competition
    let mut competition = coordinator.get_competition(create_event.id).await?;
    debug!("competition: {:?}", competition.errors);
    assert_eq!(competition.get_state(), CompetitionState::EntriesCollected);
    assert!(competition.contract_parameters.is_some());
    assert!(competition.funding_transaction.is_some());

    // For each participant, submit nonces
    for participant in participants.iter_mut() {
        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");
        // Add contract to wallet
        participant.wallet.add_contract(
            1, // Use 1 as the entry index since each participant has one entry
            competition.contract_parameters.clone().unwrap(),
            competition.funding_transaction.unwrap(),
        )?;
        debug!(
            "Add contract to participant wallet: {}",
            participant.nostr_pubkey
        );

        // Generate and submit nonces
        let nonces = participant.wallet.generate_public_nonces(1)?; // Use 1 as entry index
        debug!("Generated public nonces for participant",);

        coordinator
            .submit_public_nonces(
                participant.nostr_pubkey.clone(),
                competition.id,
                entry.id, // Use the stored entry ID
                nonces,
            )
            .await?;
        debug!("Submitted public nonces for participant",);
    }

    // Let competition handler run to generate aggregate nonces
    coordinator
        .competition_handler()
        .await
        .map_err(|e| error!("{}", e))
        .unwrap();

    // Get updated competition
    competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(
        competition.get_state(),
        CompetitionState::AggregateNoncesGenerated
    );
    assert!(competition.aggregated_nonces.is_some());

    // For each participant, submit partial signatures
    let agg_nonces = competition.aggregated_nonces.clone().unwrap();
    for participant in participants.iter() {
        let entry = participants_entries
            .get(&participant.nostr_pubkey)
            .expect("Entry should exist for participant");

        // Generate and submit partial signatures
        let partial_sigs = participant
            .wallet
            .sign_aggregate_nonces(agg_nonces.clone(), 1)?; // Use 1 as entry index
        debug!("Generated partial sigs for participant");

        coordinator
            .submit_partial_signatures(
                participant.nostr_pubkey.clone(),
                competition.id,
                entry.id, // Use the stored entry ID
                partial_sigs,
            )
            .await?;
        debug!("Submitted partial sigs for participant");
    }

    // Let competition handler run to publish transactions
    coordinator
        .competition_handler()
        .await
        .map_err(|e| error!("{}", e))
        .unwrap();

    // Get final competition state
    competition = coordinator.get_competition(create_event.id).await?;
    debug!("competition.errors: {:?}", competition.errors);

    assert_eq!(competition.get_state(), CompetitionState::Broadcasted);
    assert!(competition.signed_contract.is_some());
    assert!(competition.signed_at.is_some());

    Ok(())
}
