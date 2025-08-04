use anyhow::Result;
use bdk_wallet::bitcoin::Psbt;
use dlctix::{
    attestation_secret,
    bitcoin::{Amount, PublicKey},
};
use log::{debug, info};
use nostr_sdk::ToBech32;
use reqwest_middleware::ClientWithMiddleware;
use server::{
    build_reqwest_client, create_folder,
    db::{DBConnection, DatabasePoolConfig},
    domain::{CompetitionState, InvoiceWatcher, PayoutInfo, TicketStatus},
    setup_logger, Bitcoin, BitcoinClient, CompetitionStore, Coordinator, FinalSignatures, Ln,
    LnClient, LnSettings, Oracle, OracleEvent, REQUIRED_CONFIRMATIONS_FOR_TIME,
};
use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex, Once},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::helpers::{
    create_test_nostr_client, create_test_wallet, generate_oracle_event,
    generate_request_create_event, generate_test_entry, get_keys, get_winning_bytes,
    MockOracleClient, TestParticipant,
};

static INIT_LOGGER: Once = Once::new();

pub fn setup_static_logger() {
    INIT_LOGGER.call_once(|| {
        let filter_targets = vec![
            "hyper".to_string(),
            "hyper_util".to_string(),
            "tungstenite".to_string(),
            "nostr_relay_pool".to_string(),
        ];
        setup_logger(Some("debug".to_string()), filter_targets)
            .expect("Failed to initialize logger");
    });
}

pub struct TestContext {
    pub competition_store: CompetitionStore,
    pub bitcoin_client: Arc<dyn Bitcoin>,
    pub coord_ln: Arc<dyn Ln>,
    pub oracle_client: Arc<dyn Oracle>,
}

impl TestContext {
    pub async fn new_with_ln(
        client: ClientWithMiddleware,
        winners: Option<Vec<usize>>,
        contract_expiration: u32,
    ) -> Result<Self> {
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

        let migrations_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("migrations")
            .join("competitions");

        let test_db_name = format!("test_competition_db_{}", Uuid::now_v7());
        let db_connection = DBConnection::with_migrations(
            test_data_folder,
            &test_db_name,
            DatabasePoolConfig::testing(),
            migrations_path.to_str().unwrap(),
        )
        .await?;
        let competition_store = CompetitionStore::new(db_connection.clone());

        let coordinator_ln = LnSettings {
            base_url: String::from("https://localhost:9095"),
            macaroon_file_path: String::from("./test_data/coord_ln/admin.macaroon"),
            tls_cert_path: Some(String::from("./test_data/coord_ln/tls.cert")),
            invoice_watch_interval: 10,
        };

        let ln = LnClient::new(client, coordinator_ln).await?;
        ln.ping().await?;
        let oracle_keys = get_keys(String::from("./test_data/fake_oracle_private_key.pem"));
        let mut oracle_mock = MockOracleClient::new();
        let stored_oracle_event = Arc::new(Mutex::new(None));
        let stored_oracle_event_clone = stored_oracle_event.clone();
        let stored_oracle_event_clone_2 = stored_oracle_event_clone.clone();

        oracle_mock.expect_create_event().returning(move |event| {
            let oracle_event = generate_oracle_event(
                oracle_keys.0.clone(),
                event.id,
                event.total_allowed_entries,
                event.number_of_places_win,
                contract_expiration,
            );
            debug!(
                "Created event with locking points: {:?}",
                oracle_event.event_announcement.locking_points
            );
            *stored_oracle_event.lock().unwrap() = Some(oracle_event.clone());

            Ok(oracle_event)
        });

        oracle_mock
            .expect_submit_entries()
            .times(1)
            .returning(|_| Ok(()));

        let oracle_privkey = oracle_keys.1.clone();
        oracle_mock.expect_get_event().returning(move |_| {
            let stored_event = stored_oracle_event_clone_2.lock().unwrap();
            let oracle_event = stored_event.as_ref().unwrap().clone();
            let attestation = if let Some(winners) = winners.clone() {
                let winning_bytes = get_winning_bytes(winners);
                Some(attestation_secret(
                    oracle_privkey.clone(),
                    oracle_event.nonce,
                    &winning_bytes,
                ))
            } else {
                None
            };

            Ok(OracleEvent {
                id: oracle_event.id,
                nonce: oracle_event.nonce,
                event_announcement: oracle_event.event_announcement.clone(),
                attestation,
            })
        });

        Ok(Self {
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
            4,
            1,
            String::from("testing_coordinator"),
        )
        .await?;
        debug!("coordinator created");
        Ok(Arc::new(coordinator))
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
        invoice_watch_interval: 10,
    };
    let alice_ln = LnClient::new(client.clone(), alice).await?;
    alice_ln.ping().await?;
    let bob = LnSettings {
        base_url: String::from("https://localhost:9098"),
        macaroon_file_path: String::from("./test_data/bob_ln/admin.macaroon"),
        tls_cert_path: Some(String::from("./test_data/bob_ln/tls.cert")),
        invoice_watch_interval: 10,
    };
    let bob_ln = LnClient::new(client.clone(), bob).await?;
    bob_ln.ping().await?;
    let user_ln_clients = vec![alice_ln, bob_ln];

    // Create test context with real coordinator lightning node
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    let expiry_time = current_time + 540; // 9 minutes from now
    let winning_entry_index: u32 = 0;
    let context = TestContext::new_with_ln(
        client,
        Some(vec![winning_entry_index as usize]),
        expiry_time,
    )
    .await?;

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
    for (i, user_ln_client) in user_ln_clients.iter().enumerate() {
        let nostr_client = create_test_nostr_client().await;
        let wallet = create_test_wallet(&nostr_client).await;
        let btc_pubkey = wallet.get_dlc_public_key(i as u32).await.unwrap();
        let btc_pubkey = PublicKey::from_str(&btc_pubkey).unwrap();
        let nostr_pubkey = nostr_client.get_public_key().await?;
        let nostr_bech32 = nostr_pubkey.to_bech32()?;
        debug!("requesting ticket");

        let ticket_response = coordinator
            .request_ticket(nostr_bech32.clone(), competition.id, btc_pubkey)
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
            if invoice_status == TicketStatus::Settled {
                payment_confirmed = true;
                break;
            }

            sleep(Duration::from_secs(1)).await;
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
            user_ln_client.clone(),
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
        sleep(Duration::from_millis(100)).await;
    }

    sleep(Duration::from_secs(1)).await;
    // 1) Wait for all escrow funds to confirm
    // 2) Create oracle event announcement
    // 3) Add all entries to oracle event
    // 4) Create funding transaction and dlc contract
    // Wait for the competition state to change to EscrowFundsConfirmed
    let mut escrow_confirmed = false;
    for _ in 0..30 {
        coordinator.competition_handler().await?;
        let competition = coordinator.get_competition(create_event.id).await?;
        debug!("Competition state: {:?}", competition.get_state());
        if competition.escrow_funds_confirmed_at.is_some() {
            escrow_confirmed = true;
            assert!(competition.escrow_funds_confirmed_at.is_some());
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }
    assert!(
        escrow_confirmed,
        "Competition state did not change to EscrowFundsConfirmed within 30 seconds"
    );

    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("competition: {:?}", competition);
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

        let partial_signatures = participant
            .wallet
            .sign_aggregate_nonces(agg_nonces.clone(), entry_index)?;

        let psbt = Psbt::from_str(&competition.funding_psbt_base64.clone().unwrap()).unwrap();

        let funding_psbt = participant.wallet.sign_funding_psbt(psbt, entry_index)?;

        coordinator
            .submit_final_signatures(
                participant.nostr_pubkey.clone(),
                competition.id,
                entry.id,
                FinalSignatures {
                    partial_signatures,
                    funding_psbt_base64: funding_psbt.to_string(),
                },
            )
            .await?;
    }

    // 1) Sign dlc contract
    // 2) Broadcast funding transaction
    coordinator.competition_handler().await?;

    let competition = coordinator.get_competition(create_event.id).await?;
    assert!(competition.signed_contract.is_some());
    assert!(competition.signed_at.is_some());
    assert_eq!(
        competition.get_state(),
        CompetitionState::FundingBroadcasted
    );
    assert!(competition.funding_broadcasted_at.is_some());

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
        sleep(Duration::from_secs(1)).await;
    }
    assert!(
        confirmed,
        "Funding transaction was not confirmed within 30 seconds"
    );

    // Run handler to process confirmation
    coordinator.competition_handler().await?;

    // 1) verify funding confirmed state
    // 2) add funding settled
    // 3) attest the competition state
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::Attested);
    assert!(competition.funding_confirmed_at.is_some());
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

    // Run handler to process attestation
    coordinator.competition_handler().await?;

    // Verify attestation state
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(
        competition.get_state(),
        CompetitionState::OutcomeBroadcasted
    );
    assert!(competition.attestation.is_some());

    let participant = ordered_participants[winning_entry_index as usize].clone();

    // Create a real invoice with the expected payout amount (should be entry fee)
    let payout_amount = competition.event_submission.entry_fee as u64;
    let ln_invoice = participant
        .ln_client
        .create_invoice(
            payout_amount,
            3600, // 1 hour expiry
        )
        .await?;
    debug!(
        "participant ticket: {:?} participant: {:?}",
        participant.ticket_id, participant.nostr_pubkey,
    );
    let entry = &participants_entries[&participant.nostr_pubkey];
    let payout_info = PayoutInfo {
        ticket_id: participant.ticket_id,
        payout_preimage: participant
            .get_payout_preimage(&entry.payout_preimage_encrypted)
            .await
            .unwrap(),
        ephemeral_private_key: participant
            .get_dlc_private_key(participant_entry_indices[&participant.nostr_pubkey])
            .await
            .unwrap(),
        ln_invoice,
    };

    coordinator
        .submit_ticket_payout(
            participant.nostr_pubkey.clone(),
            competition.id,
            participants_entries
                .get(&participant.nostr_pubkey)
                .unwrap()
                .id,
            payout_info,
        )
        .await?;

    // Run handler to process outcome transaction
    coordinator.competition_handler().await?;

    // Verify outcome transaction broadcast
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(
        competition.get_state(),
        CompetitionState::OutcomeBroadcasted
    );
    assert!(competition.outcome_transaction.is_some());
    assert!(competition.outcome_broadcasted_at.is_some());

    // Wait for outcome transaction confirmation and get its height
    let outcome_txid = competition.outcome_transaction.unwrap().compute_txid();
    let mut outcome_height = None;
    for _ in 0..30 {
        if let Ok(Some(height)) = context
            .bitcoin_client
            .get_tx_confirmation_height(&outcome_txid)
            .await
        {
            if height >= 1 {
                outcome_height = Some(height);
                break;
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
    let outcome_height = outcome_height.expect("Outcome transaction not confirmed");

    // Wait for required blocks to pass for first delta transaction
    let required_blocks = competition
        .signed_contract
        .as_ref()
        .unwrap()
        .params()
        .relative_locktime_block_delta as u32;

    // First delta needs required_blocks blocks
    let first_delta_target = outcome_height + required_blocks;
    let mut current_height;
    for _ in 0..60 {
        current_height = context.bitcoin_client.get_current_height().await?;
        if current_height >= first_delta_target {
            debug!(
                "Required blocks passed for first delta. Current height: {}, Target height: {}",
                current_height, first_delta_target
            );
            break;
        }
        debug!(
            "Waiting for blocks for first delta. Current height: {}, Target height: {}",
            current_height, first_delta_target
        );
        sleep(Duration::from_secs(1)).await;
    }

    // Run handler to process delta transactions
    coordinator.competition_handler().await?;

    // Verify delta transactions broadcast
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::DeltaBroadcasted);
    assert!(competition.delta_broadcasted_at.is_some());

    // Wait for second delta (total of 2 * required_blocks since outcome)
    let second_delta_target = outcome_height + (2 * required_blocks);
    for _ in 0..60 {
        current_height = context.bitcoin_client.get_current_height().await?;
        if current_height >= second_delta_target {
            debug!(
                "Required blocks passed for second delta. Current height: {}, Target height: {}",
                current_height, second_delta_target
            );
            break;
        }
        debug!(
            "Waiting for blocks for second delta. Current height: {}, Target height: {}",
            current_height, second_delta_target
        );
        sleep(Duration::from_secs(1)).await;
    }

    // Verify we reached the target height
    current_height = context.bitcoin_client.get_current_height().await?;
    assert!(
        current_height >= second_delta_target,
        "Failed to reach target block height for delta2 transactions. Current: {}, Target: {}",
        current_height,
        second_delta_target
    );

    // Run handler to process final closing transactions
    coordinator.competition_handler().await?;

    // Verify completion
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::Completed);
    assert!(competition.completed_at.is_some());

    cancel_token.cancel();
    Ok(())
}

#[tokio::test]
#[ignore = "requires local regtest environment and lightning nodes"]
async fn test_two_person_competition_flow_nobody_wins_with_real_lightning() -> Result<()> {
    let players = 2;
    let ranks = 1;
    let locations = 1;
    let client = build_reqwest_client();
    let alice = LnSettings {
        base_url: String::from("https://localhost:9092"),
        macaroon_file_path: String::from("./test_data/alice_ln/admin.macaroon"),
        tls_cert_path: Some(String::from("./test_data/alice_ln/tls.cert")),
        invoice_watch_interval: 10,
    };
    let alice_ln = LnClient::new(client.clone(), alice).await?;
    alice_ln.ping().await?;
    let bob = LnSettings {
        base_url: String::from("https://localhost:9098"),
        macaroon_file_path: String::from("./test_data/bob_ln/admin.macaroon"),
        tls_cert_path: Some(String::from("./test_data/bob_ln/tls.cert")),
        invoice_watch_interval: 10,
    };
    let bob_ln = LnClient::new(client.clone(), bob).await?;
    bob_ln.ping().await?;
    let user_ln_clients = vec![alice_ln, bob_ln];

    // Create test context with real coordinator lightning node
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    let expiry_time = current_time + 540; // 9 minutes from now
    let context =
        TestContext::new_with_ln(client, Some((0..players).collect()), expiry_time).await?;

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
    for (i, user_ln_client) in user_ln_clients.iter().enumerate() {
        let nostr_client = create_test_nostr_client().await;
        let wallet = create_test_wallet(&nostr_client).await;
        let btc_pubkey = wallet.get_dlc_public_key(i as u32).await.unwrap();
        let btc_pubkey = PublicKey::from_str(&btc_pubkey).unwrap();
        let nostr_pubkey = nostr_client.get_public_key().await?;
        let nostr_bech32 = nostr_pubkey.to_bech32()?;
        debug!("requesting ticket");

        let ticket_response = coordinator
            .request_ticket(nostr_bech32.clone(), competition.id, btc_pubkey)
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
            if invoice_status == TicketStatus::Settled {
                payment_confirmed = true;
                break;
            }

            sleep(Duration::from_secs(1)).await;
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
            user_ln_client.clone(),
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
        sleep(Duration::from_millis(100)).await;
    }
    sleep(Duration::from_secs(1)).await;
    // 1) Wait for all escrow funds to confirm
    // 2) Create oracle event announcement
    // 3) Add all entries to oracle event
    // 4) Create funding transaction and dlc contract
    // Wait for the competition state to change to EscrowFundsConfirmed
    let mut escrow_confirmed = false;
    for _ in 0..30 {
        coordinator.competition_handler().await?;
        let competition = coordinator.get_competition(create_event.id).await?;
        debug!("Competition state: {:?}", competition.get_state());
        if competition.escrow_funds_confirmed_at.is_some() {
            escrow_confirmed = true;
            assert!(competition.escrow_funds_confirmed_at.is_some());
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }
    assert!(
        escrow_confirmed,
        "Competition state did not change to EscrowFundsConfirmed within 30 seconds"
    );

    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("competition: {:?}", competition);
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

        let partial_signatures = participant
            .wallet
            .sign_aggregate_nonces(agg_nonces.clone(), entry_index)?;

        let psbt = Psbt::from_str(&competition.funding_psbt_base64.clone().unwrap()).unwrap();

        let funding_psbt = participant.wallet.sign_funding_psbt(psbt, entry_index)?;

        coordinator
            .submit_final_signatures(
                participant.nostr_pubkey.clone(),
                competition.id,
                entry.id,
                FinalSignatures {
                    partial_signatures,
                    funding_psbt_base64: funding_psbt.to_string(),
                },
            )
            .await?;
    }

    // 1) Sign dlc contract
    // 2) Broadcast funding transaction
    coordinator.competition_handler().await?;

    let competition = coordinator.get_competition(create_event.id).await?;
    assert!(competition.signed_contract.is_some());
    assert!(competition.signed_at.is_some());
    assert_eq!(
        competition.get_state(),
        CompetitionState::FundingBroadcasted
    );
    assert!(competition.funding_broadcasted_at.is_some());

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
        sleep(Duration::from_secs(1)).await;
    }
    assert!(
        confirmed,
        "Funding transaction was not confirmed within 30 seconds"
    );

    // Run handler to process confirmation
    coordinator.competition_handler().await?;

    // 1) verify funding confirmed state
    // 2) add funding settled
    // 3) attest the competition state
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::Attested);
    assert!(competition.funding_confirmed_at.is_some());
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

    // Run handler to process attestation
    coordinator.competition_handler().await?;

    // Verify attestation state
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(
        competition.get_state(),
        CompetitionState::OutcomeBroadcasted
    );
    assert!(competition.attestation.is_some());

    // All players should be able to claim a refund from this competition via lightning
    for participant in ordered_participants.iter() {
        // Create a real invoice with the expected payout amount
        let payout_amount = competition.event_submission.total_competition_pool as u64; // Or calculate based on winner's share
        let ln_invoice = participant
            .ln_client
            .create_invoice(
                payout_amount,
                3600, // 1 hour expiry (default lnd)
            )
            .await?;
        let entry = &participants_entries[&participant.nostr_pubkey];
        let payout_info = PayoutInfo {
            ticket_id: participant.ticket_id,
            payout_preimage: participant
                .get_payout_preimage(&entry.payout_preimage_encrypted)
                .await
                .unwrap(),
            ephemeral_private_key: participant
                .get_dlc_private_key(participant_entry_indices[&participant.nostr_pubkey])
                .await
                .unwrap(),
            ln_invoice,
        };

        coordinator
            .submit_ticket_payout(
                participant.nostr_pubkey.clone(),
                competition.id,
                participants_entries
                    .get(&participant.nostr_pubkey)
                    .unwrap()
                    .id,
                payout_info,
            )
            .await?;
    }

    // Run handler to process outcome transaction
    coordinator.competition_handler().await?;

    // Verify outcome transaction broadcast
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(
        competition.get_state(),
        CompetitionState::OutcomeBroadcasted
    );
    assert!(competition.outcome_transaction.is_some());
    assert!(competition.outcome_broadcasted_at.is_some());

    // Wait for outcome transaction confirmation and get its height
    let outcome_txid = competition.outcome_transaction.unwrap().compute_txid();
    let mut outcome_height = None;
    for _ in 0..30 {
        if let Ok(Some(height)) = context
            .bitcoin_client
            .get_tx_confirmation_height(&outcome_txid)
            .await
        {
            if height >= 1 {
                outcome_height = Some(height);
                break;
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
    let outcome_height = outcome_height.expect("Outcome transaction not confirmed");

    // Wait for required blocks to pass for first delta transaction
    let required_blocks = competition
        .signed_contract
        .as_ref()
        .unwrap()
        .params()
        .relative_locktime_block_delta as u32;

    // First delta needs required_blocks blocks
    let first_delta_target = outcome_height + required_blocks;
    let mut current_height;
    for _ in 0..60 {
        current_height = context.bitcoin_client.get_current_height().await?;
        if current_height >= first_delta_target {
            debug!(
                "Required blocks passed for first delta. Current height: {}, Target height: {}",
                current_height, first_delta_target
            );
            break;
        }
        debug!(
            "Waiting for blocks for first delta. Current height: {}, Target height: {}",
            current_height, first_delta_target
        );
        sleep(Duration::from_secs(1)).await;
    }

    // Run handler to process delta transactions
    coordinator.competition_handler().await?;

    // Verify delta transactions broadcast
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::DeltaBroadcasted);
    assert!(competition.delta_broadcasted_at.is_some());

    // Wait for second delta (total of 2 * required_blocks since outcome)
    let second_delta_target = outcome_height + (2 * required_blocks);
    for _ in 0..60 {
        current_height = context.bitcoin_client.get_current_height().await?;
        if current_height >= second_delta_target {
            debug!(
                "Required blocks passed for second delta. Current height: {}, Target height: {}",
                current_height, second_delta_target
            );
            break;
        }
        debug!(
            "Waiting for blocks for second delta. Current height: {}, Target height: {}",
            current_height, second_delta_target
        );
        sleep(Duration::from_secs(1)).await;
    }

    // Verify we reached the target height
    current_height = context.bitcoin_client.get_current_height().await?;
    assert!(
        current_height >= second_delta_target,
        "Failed to reach target block height for delta2 transactions. Current: {}, Target: {}",
        current_height,
        second_delta_target
    );

    // Run handler to process final closing transactions
    coordinator.competition_handler().await?;

    // Verify completion
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::Completed);
    assert!(competition.completed_at.is_some());

    cancel_token.cancel();
    Ok(())
}

#[tokio::test]
#[ignore = "requires local regtest environment and lightning nodes"]
async fn test_two_person_competition_flow_contract_expires_with_real_lightning() -> Result<()> {
    let players = 2;
    let ranks = 1;
    let locations = 1;
    let client = build_reqwest_client();
    let alice = LnSettings {
        base_url: String::from("https://localhost:9092"),
        macaroon_file_path: String::from("./test_data/alice_ln/admin.macaroon"),
        tls_cert_path: Some(String::from("./test_data/alice_ln/tls.cert")),
        invoice_watch_interval: 10,
    };
    let alice_ln = LnClient::new(client.clone(), alice).await?;
    alice_ln.ping().await?;
    let bob = LnSettings {
        base_url: String::from("https://localhost:9098"),
        macaroon_file_path: String::from("./test_data/bob_ln/admin.macaroon"),
        tls_cert_path: Some(String::from("./test_data/bob_ln/tls.cert")),
        invoice_watch_interval: 10,
    };
    let bob_ln = LnClient::new(client.clone(), bob).await?;
    bob_ln.ping().await?;
    let user_ln_clients = vec![alice_ln, bob_ln];

    // Create test context with real coordinator lightning node
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    let expiry_time = current_time + 20; // 20 seconds from now
    info!("Contract expires in: {:?}", expiry_time);

    let context = TestContext::new_with_ln(client, None, expiry_time).await?;

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
    for (i, user_ln_client) in user_ln_clients.iter().enumerate() {
        let nostr_client = create_test_nostr_client().await;
        let wallet = create_test_wallet(&nostr_client).await;
        let btc_pubkey = wallet.get_dlc_public_key(i as u32).await.unwrap();
        let btc_pubkey = PublicKey::from_str(&btc_pubkey).unwrap();
        let nostr_pubkey = nostr_client.get_public_key().await?;
        let nostr_bech32 = nostr_pubkey.to_bech32()?;
        debug!("requesting ticket");

        let ticket_response = coordinator
            .request_ticket(nostr_bech32.clone(), competition.id, btc_pubkey)
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
            if invoice_status == TicketStatus::Settled {
                payment_confirmed = true;
                break;
            }

            sleep(Duration::from_secs(1)).await;
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
            user_ln_client.clone(),
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
        sleep(Duration::from_millis(100)).await;
    }
    sleep(Duration::from_secs(1)).await;
    // 1) Wait for all escrow funds to confirm
    // 2) Create oracle event announcement
    // 3) Add all entries to oracle event
    // 4) Create funding transaction and dlc contract
    // Wait for the competition state to change to EscrowFundsConfirmed
    let mut escrow_confirmed = false;
    for _ in 0..30 {
        coordinator.competition_handler().await?;
        let competition = coordinator.get_competition(create_event.id).await?;
        debug!("Competition state: {:?}", competition.get_state());
        if competition.escrow_funds_confirmed_at.is_some() {
            escrow_confirmed = true;
            assert!(competition.escrow_funds_confirmed_at.is_some());
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }
    assert!(
        escrow_confirmed,
        "Competition state did not change to EscrowFundsConfirmed within 30 seconds"
    );

    let competition = coordinator.get_competition(create_event.id).await?;
    debug!("competition: {:?}", competition);
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

        let partial_signatures = participant
            .wallet
            .sign_aggregate_nonces(agg_nonces.clone(), entry_index)?;

        let psbt = Psbt::from_str(&competition.funding_psbt_base64.clone().unwrap()).unwrap();

        let funding_psbt = participant.wallet.sign_funding_psbt(psbt, entry_index)?;

        coordinator
            .submit_final_signatures(
                participant.nostr_pubkey.clone(),
                competition.id,
                entry.id,
                FinalSignatures {
                    partial_signatures,
                    funding_psbt_base64: funding_psbt.to_string(),
                },
            )
            .await?;
    }

    // 1) Sign dlc contract
    // 2) Broadcast funding transaction
    coordinator.competition_handler().await?;

    let competition = coordinator.get_competition(create_event.id).await?;
    assert!(competition.signed_contract.is_some());
    assert!(competition.signed_at.is_some());
    assert_eq!(
        competition.get_state(),
        CompetitionState::FundingBroadcasted
    );
    assert!(competition.funding_broadcasted_at.is_some());

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
        sleep(Duration::from_secs(1)).await;
    }
    assert!(
        confirmed,
        "Funding transaction was not confirmed within 30 seconds"
    );

    // Run handler to process confirmation
    coordinator.competition_handler().await?;

    // 1) verify funding confirmed state
    // 2) add funding settled
    // 3) no attestation on the competition
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::FundingSettled);
    assert!(competition.funding_confirmed_at.is_some());
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

    // Get the expiry time from the contract
    let expiry = competition
        .signed_contract
        .unwrap()
        .dlc()
        .params()
        .event
        .expiry
        .unwrap();

    // Wait for blockchain time to advance past expiry
    info!(
        "Waiting for confirmed blockchain time to pass expiration date ({})",
        expiry
    );
    let mut blockchain_time_passed = false;
    for _ in 0..60 {
        match coordinator
            .bitcoin
            .get_confirmed_blockchain_time(REQUIRED_CONFIRMATIONS_FOR_TIME)
            .await
        {
            Ok(current_time) => {
                info!(
                    "Current blockchain time: {}, expiry: {}",
                    current_time, expiry
                );
                if current_time > expiry as u64 {
                    blockchain_time_passed = true;
                    break;
                }
            }
            Err(e) => {
                info!("Error getting blockchain time: {}", e);
            }
        }

        sleep(Duration::from_secs(1)).await;
    }

    assert!(
        blockchain_time_passed,
        "Blockchain time did not advance past expiry"
    );

    // Run handler to process expiry transaction
    coordinator.competition_handler().await?;

    // Verify attestation state
    let competition = coordinator.get_competition(create_event.id).await?;
    assert!(competition.attestation.is_none());

    // Verify expiry transaction broadcast
    let competition = coordinator.get_competition(create_event.id).await?;
    assert_eq!(competition.get_state(), CompetitionState::ExpiryBroadcasted);
    assert!(competition.expiry_broadcasted_at.is_some());

    let expired_tx = competition.signed_contract.unwrap().expiry_tx().unwrap();

    // Wait for expiry transaction confirmation and get its height
    let mut expiry_height = None;
    for _ in 0..30 {
        if let Ok(Some(height)) = context
            .bitcoin_client
            .get_tx_confirmation_height(&expired_tx.compute_txid())
            .await
        {
            if height >= 1 {
                expiry_height = Some(height);
                break;
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
    let expiry_height = expiry_height.expect("Expire transaction not confirmed");
    assert!(expiry_height > 0);

    cancel_token.cancel();
    Ok(())
}
