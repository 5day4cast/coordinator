use std::{collections::HashMap, sync::Arc, time::Duration};

use nostr_sdk::ToBech32;
use server::domain::{CompetitionState, TicketStatus};

use crate::{
    bitcoin_integration_test::TestContext,
    helpers::{
        create_test_nostr_client, create_test_wallet, generate_request_create_event,
        generate_test_entry, TestParticipant,
    },
};

#[tokio::test]
#[ignore = "requires local regtest environment and lightning nodes"]
async fn test_three_person_competition_flow_with_real_lightning() -> Result<()> {
    let players = 3;
    let ranks = 1;
    let locations = 1;

    // Setup coordinator's lightning node connection
    let coordinator_ln_client = LndClient::new(
        "http://localhost:10001", // Coordinator's LND REST API endpoint
        include_str!("../../test_data/coordinator_tls.cert").to_string(),
        include_str!("../../test_data/coordinator_macaroon.hex").to_string(),
    )?;

    // Test users' lightning node connections
    let user_ln_clients = vec![
        LndClient::new(
            "http://localhost:10002", // User 1's LND REST API endpoint
            include_str!("../../test_data/user1_tls.cert").to_string(),
            include_str!("../../test_data/user1_macaroon.hex").to_string(),
        )?,
        LndClient::new(
            "http://localhost:10003", // User 2's LND REST API endpoint
            include_str!("../../test_data/user2_tls.cert").to_string(),
            include_str!("../../test_data/user2_macaroon.hex").to_string(),
        )?,
        LndClient::new(
            "http://localhost:10004", // User 3's LND REST API endpoint
            include_str!("../../test_data/user3_tls.cert").to_string(),
            include_str!("../../test_data/user3_macaroon.hex").to_string(),
        )?,
    ];

    // Create test context with real coordinator lightning node
    let context = TestContext::new_with_ln(players, Arc::new(coordinator_ln_client)).await?;
    let coordinator = context.create_coordinator().await?;

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

        let ticket_response = coordinator
            .request_ticket(nostr_bech32.clone(), competition.id)
            .await?;

        // Actually pay the invoice using the user's lightning node
        user_ln_client
            .send_payment(
                ticket_response.payment_request.clone(),
                300,  // timeout in seconds
                1000, // fee limit in sats
            )
            .await?;

        // Wait for payment confirmation
        let mut payment_confirmed = false;
        for _ in 0..30 {
            // Try for 30 seconds
            let invoice_status = coordinator
                .get_ticket_status(
                    nostr_bech32.clone(),
                    competition.id,
                    ticket_response.ticket_id,
                )
                .await?;

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

        participants.push(TestParticipant::new(wallet, nostr_bech32));
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
        )
        .await?;

        participants_entries.insert(participant.nostr_pubkey.clone(), entry.clone());
        participant_entry_indices.insert(participant.nostr_pubkey.clone(), i as u32);

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
    assert_eq!(competition.get_state(), CompetitionState::Broadcasted);
    assert!(competition.signed_contract.is_some());
    assert!(competition.signed_at.is_some());

    Ok(())
}
