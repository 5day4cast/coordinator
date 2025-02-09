use anyhow::Result;
use dlctix::{
    bitcoin::Amount,
    musig2::secp256k1::{PublicKey, SecretKey},
};
use log::{debug, info};
use nostr_sdk::ToBech32;
use reqwest_middleware::ClientWithMiddleware;
use server::{
    build_reqwest_client, create_folder,
    domain::{CompetitionState, DBConnection, InvoiceWatcher, TicketStatus},
    Bitcoin, BitcoinClient, CompetitionStore, Coordinator, Ln, LnClient, LnSettings, Oracle,
};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{Arc, Once},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::helpers::{
    create_test_nostr_client, create_test_wallet, generate_oracle_event,
    generate_request_create_event, generate_test_entry, get_keys, MockOracleClient,
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
    pub coord_ln: Arc<dyn Ln>,
    pub oracle_client: Arc<dyn Oracle>,
}

impl TestContext {
    pub async fn new_with_ln(entries: usize, client: ClientWithMiddleware) -> Result<Self> {
        setup_static_logger();
        let test_data_folder = "./test_data";
        create_folder(test_data_folder);

        // Setup real Bitcoin client
        let bitcoin_settings = server::BitcoinSettings {
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

        let coordinator_ln = LnSettings {
            base_url: String::from("https://localhost:9095"),
            macaroon_file_path: String::from("./test_data/coord_ln/admin.macaroon"),
            tls_cert_path: Some(String::from("./test_data/coord_ln/tls.cert")),
        };

        let ln = LnClient::new(client, coordinator_ln).await?;
        ln.ping().await?;
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
            coord_ln: Arc::new(ln),
            oracle_client: Arc::new(oracle_mock),
        })
    }

    pub async fn create_coordinator(&self) -> Result<Arc<Coordinator>> {
        let coordinator = Coordinator::new(
            self.oracle_client.clone(),
            self.competition_store.clone(),
            self.bitcoin_client.clone(),
            self.coord_ln.clone(),
            144,
            1,
        )
        .await?;
        debug!("coordinator created");
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
#[ignore = "requires local regtest environment and lightning nodes"]
async fn test_two_person_competition_flow_with_real_lightning() -> Result<()> {
    let players = 2;
    let ranks = 1;
    let locations = 1;
    let client = build_reqwest_client();
    let alice = LnSettings {
        base_url: String::from("https://localhost:9092"),
        macaroon_file_path: String::from("./test_data/alice_ln/admin.macaroon"),
        tls_cert_path: Some(String::from("./test_data/alice_ln/tls.cert")),
    };
    let alice_ln = LnClient::new(client.clone(), alice).await?;
    alice_ln.ping().await?;
    let bob = LnSettings {
        base_url: String::from("https://localhost:9098"),
        macaroon_file_path: String::from("./test_data/bob_ln/admin.macaroon"),
        tls_cert_path: Some(String::from("./test_data/bob_ln/tls.cert")),
    };
    let bob_ln = LnClient::new(client.clone(), bob).await?;
    bob_ln.ping().await?;
    let user_ln_clients = vec![alice_ln, bob_ln];

    // Create test context with real coordinator lightning node
    let context = TestContext::new_with_ln(players, client).await?;

    let coordinator = context.create_coordinator().await?;
    let cancel_token = CancellationToken::new();
    let cancel_token_clone = cancel_token.clone();

    let invoice_watcher = InvoiceWatcher::new(
        coordinator.clone(),
        context.coord_ln.clone(),
        cancel_token_clone,
        Duration::from_secs(2),
    );

    tokio::spawn(async move { invoice_watcher.watch().await });

    // Create competition
    let create_event = generate_request_create_event(locations, players, ranks);

    let competition = coordinator.create_competition(create_event.clone()).await?;
    assert_eq!(competition.get_state(), CompetitionState::Created);

    let mut participants = Vec::new();

    // Create participants and process their entries with real lightning payments
    for (i, user_ln_client) in user_ln_clients.into_iter().enumerate() {
        let nostr_client = create_test_nostr_client().await;
        let wallet = create_test_wallet(&nostr_client).await;
        let nostr_pubkey = nostr_client.get_public_key().await?;
        let nostr_bech32 = nostr_pubkey.to_bech32()?;
        debug!("requesting ticket");
        let ticket_response = coordinator
            .request_ticket(nostr_bech32.clone(), competition.id)
            .await?;
        debug!("ticket: {:?}", ticket_response);
        // Actually pay the invoice using the user's lightning node
        user_ln_client
            .send_payment(
                ticket_response.payment_request.clone(),
                300,  // timeout in seconds
                1000, // fee limit in sats
            )
            .await?;
        debug!("sent payment");
        // Wait for payment confirmation
        let mut payment_confirmed = false;
        for _ in 0..30 {
            // Try for 30 seconds
            debug!("checking invoice status");
            let invoice_status = coordinator
                .get_ticket_status(
                    nostr_bech32.clone(),
                    competition.id,
                    ticket_response.ticket_id,
                )
                .await?;
            debug!("invoice_status: {:?}", invoice_status);
            if invoice_status == TicketStatus::Paid {
                payment_confirmed = true;
                break;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        assert!(
            payment_confirmed,
            "Payment was not confirmed for participant {}",
            i
        );

        participants.push(TestParticipant::new(
            wallet,
            nostr_bech32,
            ticket_response.ticket_id,
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

        debug!("Submitting entry for participant {}", i);
        coordinator
            .add_entry(participant.nostr_pubkey.clone(), entry)
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

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

    // Broadcast funding transaction
    coordinator.competition_handler().await?;

    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(
        competition.get_state(),
        CompetitionState::FundingBroadcasted
    );
    assert!(competition.signed_contract.is_some());
    assert!(competition.signed_at.is_some());

    // Wait for automatic block generation on mutinynet to confirm transaction
    let funding_txid = competition
        .funding_transaction
        .as_ref()
        .unwrap()
        .compute_txid();

    // Wait for transaction to be confirmed (should take at most 30 seconds if using mutinynet)
    let mut confirmed = false;
    for _ in 0..30 {
        if let Ok(Some(confirmations)) = context
            .bitcoin_client
            .get_tx_confirmation_height(&funding_txid)
            .await
        {
            debug!("Transaction has {} confirmations", confirmations);
            if confirmations >= 1 {
                confirmed = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        confirmed,
        "Funding transaction was not confirmed within 30 seconds"
    );

    // Run handler to process confirmation
    coordinator.competition_handler().await?;

    // Verify funding confirmed state
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::FundingConfirmed);
    assert!(competition.funding_confirmed_at.is_some());

    // Run to process invoice settlement
    coordinator.competition_handler().await?;

    // Verify final state with settled invoices
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::FundingSettled);
    assert!(competition.funding_settled_at.is_some());

    // Verify all tickets are properly settled
    let tickets = context
        .competition_store
        .get_tickets(competition.id)
        .await?;
    for ticket in tickets.values() {
        assert!(
            ticket.settled_at.is_some(),
            "Ticket {} was not settled",
            ticket.id
        );
    }

    cancel_token.cancel();
    Ok(())
}
