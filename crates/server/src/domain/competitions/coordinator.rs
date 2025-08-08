use super::{
    AddEntry, CompetitionError, CompetitionState, CompetitionStore, FundedContract, PayoutInfo,
    SearchBy, Ticket, TicketStatus, UserEntry,
};
use crate::{
    bitcoin_client::Bitcoin,
    domain::{Competition, CreateEvent, EntryStatus, Error},
    escrow::create_escrow_descriptor,
    generate_escrow_tx, get_escrow_outpoint,
    oracle_client::{Event, Oracle},
    AddEventEntries, AddEventEntry, FinalSignatures, ForeignUtxo, Ln, OracleError,
    REQUIRED_CONFIRMATIONS_FOR_TIME,
};
use anyhow::anyhow;
use bdk_wallet::{
    bitcoin::{
        absolute::LockTime,
        consensus::encode::deserialize,
        hashes::{sha256, Hash},
        transaction::Version,
        Amount, FeeRate, OutPoint, Psbt, PublicKey as BitcoinPublicKey, ScriptBuf, Transaction,
        TxIn, TxOut,
    },
    SignOptions,
};
use dlctix::{
    bitcoin::{
        consensus,
        hex::DisplayHex as _,
        key::TweakedPublicKey,
        psbt::Input,
        sighash::Prevouts,
        transaction::{predict_weight, InputWeightPrediction},
        PublicKey as BdkPublicKey,
    },
    convert_xonly_key,
    musig2::{AggNonce, PartialSignature, PubNonce},
    secp::{Point, Scalar},
    ContractParameters, NonceSharingRound, Outcome, PayoutWeights, Player, PlayerIndex, SigMap,
    SigningSession, TicketedDLC, WinCondition,
};
use futures::TryFutureExt;
use itertools::Itertools;
use log::{debug, error, info};
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use serde::Serialize;
use std::time::Duration;
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
};
use std::{io::Write, sync::Arc};
use time::OffsetDateTime;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Response for ticket request containing both payment info and safety mechanisms
#[derive(Debug, Serialize)]
pub struct TicketResponse {
    pub ticket_id: uuid::Uuid,
    pub payment_request: String, // Lightning HODL invoice to pay for entry
    pub escrow_tx: Option<String>, // escrow transaction the coordinator broadcasts prior to settling the HODL invoice
    pub payment_hash: String,      // Hex-encoded payment hash for verification
    pub amount_sats: u64,
}

pub struct CompetitionWatcher {
    coordinator: Arc<Coordinator>,
    sync_interval: Duration,
    cancel_token: CancellationToken,
}

impl CompetitionWatcher {
    pub fn new(
        coordinator: Arc<Coordinator>,
        cancel_token: CancellationToken,
        sync_interval: Duration,
    ) -> Self {
        Self {
            coordinator,
            sync_interval,
            cancel_token,
        }
    }

    pub async fn watch(&self) -> Result<(), anyhow::Error> {
        info!("Starting Competition sync watcher");

        loop {
            if self.cancel_token.is_cancelled() {
                info!("Competition sync watcher received cancellation");
                break;
            }

            match self.coordinator.competition_handler().await {
                Ok(_) => {
                    info!("Competition sync completed successfully");
                }
                Err(e) => {
                    error!("Competition sync error: {}", e);
                }
            }

            tokio::select! {
                _ = sleep(self.sync_interval) => continue,
                _ = self.cancel_token.cancelled() => {
                    info!("Competition sync watcher cancelled during sleep");
                    break;
                }
            }
        }

        Ok(())
    }
}

pub struct Coordinator {
    oracle_client: Arc<dyn Oracle>,
    pub competition_store: Arc<CompetitionStore>,
    pub bitcoin: Arc<dyn Bitcoin>,
    ln: Arc<dyn Ln>,
    private_key: Scalar,
    public_key: Point,
    relative_locktime_block_delta: u32,
    required_confirmations: u32,
    name: String,
}

impl Coordinator {
    pub async fn new(
        oracle_client: Arc<dyn Oracle>,
        competition_store: CompetitionStore,
        bitcoin: Arc<dyn Bitcoin>,
        ln: Arc<dyn Ln>,
        relative_locktime_block_delta: u32,
        required_confirmations: u32,
        name: String,
    ) -> Result<Self, anyhow::Error> {
        let private_key = bitcoin.get_derived_private_key().await?;
        let public_key = private_key.base_point_mul();

        let coordinator = Self {
            oracle_client,
            competition_store: Arc::new(competition_store),
            bitcoin,
            ln,
            private_key,
            public_key,
            relative_locktime_block_delta,
            required_confirmations,
            name,
        };
        coordinator.validate_coordinator_metadata().await?;
        Ok(coordinator)
    }

    pub fn public_key(&self) -> String {
        let (xonly, _) = self.public_key.into();
        hex::encode(xonly.serialize())
    }

    pub async fn ping(&self) -> Result<(), Error> {
        self.competition_store.ping().await.map_err(Error::DbError)
    }

    fn get_immediate_transition_states() -> Vec<CompetitionState> {
        vec![
            CompetitionState::EscrowFundsConfirmed,
            CompetitionState::EventCreated,
            CompetitionState::EntriesSubmitted,
            CompetitionState::SigningComplete,
            CompetitionState::FundingConfirmed,
            CompetitionState::FundingSettled,
        ]
    }

    pub async fn competition_handler(&self) -> Result<(), anyhow::Error> {
        let immediate_states = Self::get_immediate_transition_states();
        let competitions: Vec<Competition> =
            self.competition_store.get_competitions(true, true).await?;

        for mut competition in competitions {
            let mut processed_states = 0;
            const MAX_CONSECUTIVE_STATES: usize = 10;
            if competition.skip_competition() {
                info!(
                    "Skipping process competition {} {:?}",
                    competition.id,
                    competition.get_state()
                );
                continue;
            }

            if competition.is_expired() && competition.cancelled_at.is_none() {
                competition.cancelled_at = Some(OffsetDateTime::now_utc());
                if let Err(e) = self
                    .competition_store
                    .update_competitions(vec![competition.clone()])
                    .await
                {
                    error!(
                        "Failed to save competition {} after processing cancelled_at: {}",
                        competition.id, e
                    );
                }
                info!(
                    "Skipping process competition {} {:?}",
                    competition.id,
                    competition.get_state()
                );
                continue;
            }

            loop {
                let current_state = competition.get_state();
                match self.process_competition_state(competition.clone()).await {
                    Ok(updated_competiton) => {
                        info!(
                            "Updated competition {} new state {} previous state: {}",
                            competition.id,
                            updated_competiton.get_state(),
                            current_state
                        );
                        if updated_competiton.get_state() != current_state {
                            processed_states += 1;

                            if immediate_states.contains(&updated_competiton.get_state())
                                && processed_states < MAX_CONSECUTIVE_STATES
                            {
                                if let Err(e) = self
                                    .competition_store
                                    .update_competitions(vec![updated_competiton.clone()])
                                    .await
                                {
                                    error!(
                                        "Failed to save competition {} after processing state {}: {}",
                                        competition.id, updated_competiton.get_state(), e
                                    );
                                }
                                competition = updated_competiton;
                                continue;
                            }
                        }
                        if let Err(e) = self
                            .competition_store
                            .update_competitions(vec![updated_competiton.clone()])
                            .await
                        {
                            error!(
                                "Failed to save competition {} after processing state {}: {}",
                                competition.id,
                                competition.get_state(),
                                e
                            );
                        }
                        break;
                    }
                    Err(err) => {
                        error!("Failed to process competition: {}: {}", competition.id, err);
                        if let Err(e) = self
                            .competition_store
                            .update_competitions(vec![competition.clone()])
                            .await
                        {
                            error!(
                                "Failed to save competition {} after processing error: {}",
                                competition.id, e
                            );
                        }
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn process_competition_state(
        &self,
        mut competition: Competition,
    ) -> Result<Competition, Error> {
        info!(
            "Competition {} {:?} {:?}",
            competition.id,
            competition.get_state(),
            competition
        );
        match competition.get_state() {
            CompetitionState::Created => {
                debug!(
                    "Competition {}, waiting for more entries: {}/{}",
                    competition.id,
                    competition.total_entries,
                    competition.event_submission.total_allowed_entries
                );
                return Ok(competition);
            }
            CompetitionState::EntriesCollected => {
                match self.check_escrow_confirmations(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "competition {} failed to check escrow funds: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedEscrowConfirmation(e.to_string()));
                        if competition.should_abort() {
                            competition.failed_at = Some(OffsetDateTime::now_utc());
                        }
                    }
                }
            }
            CompetitionState::EscrowFundsConfirmed => {
                match self.submit_event_to_oracle(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Competition {} failed to submit entries to oracle: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedSubmitEntries(e.to_string()));
                        competition.failed_at = Some(OffsetDateTime::now_utc());
                    }
                }
            }
            CompetitionState::EventCreated => {
                match self.submit_entries_to_oracle(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Competition {} failed to submit entries to oracle: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedSubmitEntries(e.to_string()));
                        competition.failed_at = Some(OffsetDateTime::now_utc());
                    }
                };
            }
            CompetitionState::EntriesSubmitted => {
                match self.create_funding_psbt(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Competition {} failed to generate contract params: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedCreateTransaction(e.to_string()));
                        competition.failed_at = Some(OffsetDateTime::now_utc());
                    }
                }
            }
            CompetitionState::NoncesCollected => {
                match self
                    .generate_aggregate_nonces_and_coord_partial_signatures(&mut competition)
                    .await
                {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Competition {} failed to generate aggregate nonce: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedNonceAggregation(e.to_string()));
                        competition.failed_at = Some(OffsetDateTime::now_utc());
                    }
                }
            }
            CompetitionState::PartialSignaturesCollected => {
                match self.sign_dlc_contract(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Competition {} failed to sign dlc contract: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedBroadcast(e.to_string()));
                        competition.failed_at = Some(OffsetDateTime::now_utc());
                    }
                }
            }
            CompetitionState::SigningComplete => {
                match self.sign_and_broadcast_funding_tx(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Competition {} failed to broadcast funding txs: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedBroadcast(e.to_string()));
                        competition.failed_at = Some(OffsetDateTime::now_utc());
                    }
                }
            }
            CompetitionState::FundingBroadcasted => {
                match self.check_funding_confirmation(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Competition {} failed to check confirm: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedFundingConfirmation(e.to_string()));
                        if competition.should_abort() {
                            competition.failed_at = Some(OffsetDateTime::now_utc());
                        }
                    }
                }
            }
            CompetitionState::FundingConfirmed => {
                // Invoices are already settled immediately upon payment
                // Just mark the funding as settled and move to next state
                competition.funding_settled_at = Some(OffsetDateTime::now_utc());
                info!(
                    "Competition {} funding confirmed, all invoices already settled",
                    competition.id
                );
            }
            CompetitionState::FundingSettled => {
                match self.check_oracle_attestation(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Failed to check oracle attestation for competition {}: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedCheckingAttestation(e.to_string()));
                        if competition.should_abort() {
                            competition.failed_at = Some(OffsetDateTime::now_utc());
                        }
                    }
                }
            }
            CompetitionState::Attested => {
                // An outcome transaction spends the funding outpoint, and locks it into
                // a 2nd stage multisig contract between the outcome winners and the market maker.
                // If any player knows the attestation to outcome they can
                // unlock that outcome TX and publish it.
                match self.publish_outcome_transaction(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Failed to broadcast outcome transactions for competition {}: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedBroadcast(e.to_string()));
                        competition.failed_at = Some(OffsetDateTime::now_utc());
                    }
                }
            }
            CompetitionState::OutcomeBroadcasted => {
                match self.publish_delta_transactions(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "failed to broadcast delta transactions for competition {}: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedBroadcast(e.to_string()));
                        competition.failed_at = Some(OffsetDateTime::now_utc());
                    }
                }
            }
            CompetitionState::DeltaBroadcasted => {
                match self.publish_delta2_transactions(&mut competition).await {
                    Ok(updated) => competition = updated.to_owned(),
                    Err(e) => {
                        error!(
                            "Failed to broadcast delta2 transactions for competition {}: {}",
                            competition.id, e
                        );
                        competition
                            .errors
                            .push(CompetitionError::FailedBroadcast(e.to_string()));
                        competition.failed_at = Some(OffsetDateTime::now_utc());
                    }
                }
            }
            _ => (),
        }
        info!(
            "Completed processing competition {} , state {}",
            competition.id,
            competition.get_state()
        );
        Ok(competition)
    }

    async fn check_escrow_confirmations<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        let tickets = self.competition_store.get_tickets(competition.id).await?;
        debug!("Checking escrow confirmations: {:?}", tickets);

        let mut all_confirmed = true;
        let mut pending_txids = Vec::new();

        for (_, ticket) in tickets {
            if let Some(escrow_tx_hex) = &ticket.escrow_transaction {
                let bytes = hex::decode(escrow_tx_hex)
                    .map_err(|e| anyhow!("Failed to decode escrow transaction: {}", e))?;
                let escrow_tx: Transaction = deserialize(&bytes)
                    .map_err(|e| anyhow!("Failed to deserialize escrow transaction: {}", e))?;

                let txid = escrow_tx.compute_txid();

                // Check if transaction has required confirmations
                match self.bitcoin.get_tx_confirmation_height(&txid).await? {
                    Some(confirmations) if confirmations >= self.required_confirmations => {
                        debug!(
                            "Escrow transaction {} has {} confirmations for ticket {}",
                            txid, confirmations, ticket.id
                        );
                    }
                    Some(confirmations) => {
                        debug!(
                                "Escrow transaction {} has {} of {} required confirmations for ticket {}",
                                txid, confirmations, self.required_confirmations, ticket.id
                            );
                        all_confirmed = false;
                        pending_txids.push(txid);
                    }
                    None => {
                        debug!(
                            "Escrow transaction {} not found on-chain for ticket {}",
                            txid, ticket.id
                        );
                        all_confirmed = false;
                        pending_txids.push(txid);
                    }
                }
            }
        }

        if all_confirmed {
            competition.escrow_funds_confirmed_at = Some(OffsetDateTime::now_utc());
            debug!("All escrow funds confirmed");
        } else {
            debug!(
                "Competition {} waiting for {} escrow transactions to confirm",
                competition.id,
                pending_txids.len()
            );
        }
        competition.errors = vec![];

        Ok(competition)
    }

    async fn submit_event_to_oracle<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        if competition.event_created_at.is_none() {
            let event: Event = match self
                .oracle_client
                .create_event(competition.event_submission.clone())
                .await
            {
                Ok(event) => Ok(event),
                Err(OracleError::NotFound(e)) => Err(Error::NotFound(e)),
                Err(OracleError::BadRequest(e)) => Err(Error::BadRequest(e)),
                Err(e) => Err(Error::OracleFailed(e)),
            }?;
            debug!(
                "Created competition's {} oracle event: {:?}",
                competition.id, event
            );

            competition.event_announcement = Some(event.event_announcement);
            competition.event_created_at = Some(OffsetDateTime::now_utc());
            competition.errors = vec![];
        }
        Ok(competition)
    }

    async fn submit_entries_to_oracle<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        let entries = self
            .competition_store
            .get_competition_entries(competition.id, vec![EntryStatus::Paid])
            .await
            .map_err(|e| {
                anyhow!(
                    "Failed to get entries for competition {}: {}",
                    competition.id,
                    e
                )
            })?;

        if entries.is_empty() {
            return Err(anyhow!(
                "No paid entries found for competition {}",
                competition.id
            ));
        }

        let mut oracle_entries: Vec<AddEventEntry> = Vec::new();
        for entry in entries {
            oracle_entries.push(entry.entry_submission);
        }

        if oracle_entries.is_empty() {
            return Err(anyhow!(
                "No valid entry submissions found for competition {}",
                competition.id,
            ));
        }

        if oracle_entries.len() != competition.event_submission.total_allowed_entries {
            return Err(anyhow!(
                "Entry submissions {} do not match the total allowed entries {} for competition {}",
                oracle_entries.len(),
                competition.event_submission.total_allowed_entries,
                competition.id,
            ));
        }

        info!(
            "Submitting {} entries to oracle for competition {}",
            oracle_entries.len(),
            competition.id,
        );

        let event_entries = AddEventEntries {
            event_id: competition.id,
            entries: oracle_entries,
        };

        if competition.entries_submitted_at.is_none() {
            self.oracle_client
                .submit_entries(event_entries)
                .await
                .map_err(|e| anyhow!("Failed to submit entries to oracle: {:?}", e))?;

            competition.entries_submitted_at = Some(OffsetDateTime::now_utc());
        }
        info!(
            "Successfully submitted entries to oracle for competition {}",
            competition.id
        );
        competition.errors = vec![];

        Ok(competition)
    }

    pub async fn create_funding_psbt<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        debug!("Creating transactions for competition: {}", competition.id);

        let Some(event_announcement) = competition.event_announcement.clone() else {
            return Err(anyhow!(
                "Event announcement not found for competition {}",
                competition.id
            ));
        };
        if competition.public_nonces.is_some() {
            return Ok(competition);
        }
        let mut entries = self
            .competition_store
            .get_competition_entries(competition.id, vec![EntryStatus::Paid])
            .await?;
        debug!("Competition entries {:?}", entries);
        let tickets = self.competition_store.get_tickets(competition.id).await?;

        let players = generate_players(&entries, &tickets)?;

        debug!("Generated players:");
        for (i, player) in players.iter().enumerate() {
            debug!(
                "Player {}: pubkey={:?}, ticket_hash={:?}, payout_hash={:?}",
                i, player.pubkey, player.ticket_hash, player.payout_hash
            );
        }

        let outcome_payouts = generate_payouts(&competition, &mut entries, &players)?;
        debug!("Generated outcome payouts:");
        for (outcome, weights) in &outcome_payouts {
            debug!("Outcome {:?}: weights={:?}", outcome, weights);
        }

        let contract_amount_sats = competition.event_submission.total_competition_pool;
        let fee_rates = self.bitcoin.get_estimated_fee_rates().await?;
        info!("Fee rates: {:?}", fee_rates);

        // TODO (@tee8z): make this configurable from the admin screen
        let rate_confirm_within_2_blocks = if fee_rates.is_empty() {
            1 as u64
        } else {
            fee_rates[&1_u16].ceil() as u64
        };

        let fee_rate = FeeRate::from_sat_per_vb_unchecked(rate_confirm_within_2_blocks);

        let contract_params = ContractParameters {
            market_maker: dlctix::MarketMaker {
                pubkey: Point::from(self.public_key),
            },
            players,
            event: event_announcement.clone(),
            outcome_payouts,
            fee_rate,
            funding_value: Amount::from_sat(contract_amount_sats as u64),
            relative_locktime_block_delta: self.relative_locktime_block_delta as u16,
        };
        competition.contract_parameters = Some(contract_params.clone());

        let funding_output = contract_params.funding_output().unwrap();

        let funding_script = funding_output.script_pubkey.clone();
        debug!("Contract params have been built");
        let coordinator_pubkey = self.bitcoin.get_public_key().await?;
        let entries_lookup = entries
            .iter()
            .map(|user_entry| (user_entry.id, user_entry))
            .collect::<HashMap<_, _>>();

        let escrow_inputs: Vec<ForeignUtxo> = tickets
            .iter()
            .map(|(_, ticket)| {
                let hex_data = ticket
                    .escrow_transaction
                    .clone()
                    .ok_or_else(|| anyhow!("Missing escrow transaction"))?;

                let bytes = hex::decode(&hex_data)
                    .map_err(|e| anyhow!("Failed to decode escrow transaction: {}", e))?;

                let transaction: Transaction = deserialize(&bytes)
                    .map_err(|e| anyhow!("Failed to deserialize escrow transaction: {}", e))?;
                debug!("Escrow transaction: {:?}", transaction);
                let outpoint = get_escrow_outpoint(
                    &transaction,
                    Amount::from_sat(competition.event_submission.entry_fee as u64),
                )?;

                let escrow_output = transaction
                    .output
                    .get(outpoint.vout as usize)
                    .ok_or_else(|| anyhow!("Escrow output not found at index {}", outpoint.vout))?
                    .clone();

                let user_pubkey = &entries_lookup
                    .get(&ticket.entry_id.unwrap())
                    .ok_or_else(|| anyhow!("Missing entry for ticket {}", ticket.id))?
                    .ephemeral_pubkey;

                let btc_pubkey = ticket
                    .ephemeral_pubkey
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing btc_pubkey for ticket {}", ticket.id))?;

                if btc_pubkey != user_pubkey {
                    return Err(anyhow!(
                        "Entry public key {} must match ticket escrow public key {}",
                        user_pubkey,
                        btc_pubkey
                    ));
                }

                let user_pubkey = BdkPublicKey::from_str(user_pubkey)
                    .map_err(|e| anyhow!("Failed to parse user public key: {}", e))?;
                let payment_hash_from_ticket = string_to_byte_array(&ticket.hash);
                let preimage = hex::decode(&ticket.encrypted_preimage)
                    .map_err(|e| anyhow!("Failed to decode preimage: {}", e))?;
                let payment_hash_from_preimage = sha256::Hash::hash(&preimage).to_byte_array();

                debug!(
                    "Payment hash from ticket.hash: {}",
                    hex::encode(&payment_hash_from_ticket)
                );
                debug!(
                    "Payment hash from preimage: {}",
                    hex::encode(&payment_hash_from_preimage)
                );

                if payment_hash_from_ticket != payment_hash_from_preimage {
                    return Err(anyhow!("Payment hash mismatch for ticket {}", ticket.id));
                }

                // Use the hash that was used when creating the escrow
                let payment_hash = payment_hash_from_preimage;

                let escrow_descriptor =
                    create_escrow_descriptor(&coordinator_pubkey, &user_pubkey, &payment_hash)?;

                let witness_script = escrow_descriptor.explicit_script().map_err(|e| {
                    anyhow!("Failed to extract witness script from descriptor: {}", e)
                })?;

                Ok(ForeignUtxo {
                    outpoint,
                    psbt: Input {
                        witness_utxo: Some(escrow_output.clone()),
                        non_witness_utxo: Some(transaction),
                        witness_script: Some(witness_script),
                        ..Default::default()
                    },
                    satisfaction_weight: escrow_descriptor
                        .max_weight_to_satisfy()
                        .map_err(|e| anyhow!("Failed to get satisfactory weight: {}", e))?,
                })
            })
            .collect::<Result<Vec<_>, anyhow::Error>>()?;

        debug!("Contract amount: {}", contract_amount_sats);
        debug!("Escrow inputs: {:?}", escrow_inputs);

        let psbt = self
            .bitcoin
            .build_psbt(
                funding_script.clone(),
                Amount::from_sat(contract_amount_sats as u64),
                fee_rate,
                vec![],
                escrow_inputs,
            )
            .await?;

        let funding_txid = psbt.unsigned_tx.compute_txid();
        debug!("unsigned funding txid: {:?}", funding_txid);

        let funding_output_index = psbt
            .unsigned_tx
            .output
            .iter()
            .position(|output| {
                output.script_pubkey == funding_script
                    && output.value == Amount::from_sat(contract_amount_sats as u64)
            })
            .ok_or_else(|| anyhow!("Funding output not found in PSBT"))?;

        let funding_outpoint = OutPoint {
            txid: funding_txid,
            vout: funding_output_index as u32,
        };

        if competition.funding_psbt_base64.is_none() {
            competition.funding_psbt_base64 = Some(psbt.to_string());
            competition.funding_outpoint = Some(funding_outpoint);
        }

        // Note: word of warning, the ticketed_dlc may be very large in memory depending on the contract params
        let ticketed_dlc = TicketedDLC::new(contract_params, funding_outpoint)?;
        debug!("Built ticketed dlc");

        let signing_session = {
            let mut rng = create_deterministic_rng(&funding_outpoint, self.private_key);
            SigningSession::<NonceSharingRound>::new(ticketed_dlc, &mut rng, self.private_key)?
        };
        debug!("Started musig nonce sharing round");
        if competition.public_nonces.is_none() {
            competition.public_nonces = Some(signing_session.our_public_nonces().to_owned());
        }
        competition.errors = vec![];

        Ok(competition)
    }

    pub async fn generate_aggregate_nonces_and_coord_partial_signatures<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        let Some(contract_parameters) = &competition.contract_parameters else {
            return Err(anyhow!("contract parameters don't exists, failed publishing competition {} funding transaction", competition.id));
        };
        let Some(funding_outpoint) = &competition.funding_outpoint else {
            return Err(anyhow!("funding outpoint doesn't exists, failed publishing competition {} funding transaction", competition.id));
        };
        let Some(our_nonces) = competition.public_nonces.as_ref() else {
            return Err(anyhow!("coordinator nonces missing"));
        };

        let ticketed_dlc =
            TicketedDLC::new(contract_parameters.to_owned(), funding_outpoint.to_owned())?;

        let signing_session = {
            let mut rng = create_deterministic_rng(&funding_outpoint, self.private_key);
            SigningSession::<NonceSharingRound>::new(ticketed_dlc, &mut rng, self.private_key)?
        };

        // Verify our stored nonces match what would be generated
        if signing_session.our_public_nonces() != our_nonces {
            return Err(anyhow!("coordinator nonce mismatch"));
        }
        debug!("our_nonces: {:?}", our_nonces);

        let received_nonces = self.get_received_nonces(competition.id).await?;
        debug!("received_nonces: {:?}", received_nonces);
        let coordinator_sessions =
            signing_session.aggregate_nonces_and_compute_partial_signatures(received_nonces)?;
        debug!(
            "Received_nonces aggregated nonces 1: {:?}",
            coordinator_sessions.aggregated_nonces()
        );
        if competition.aggregated_nonces.is_none() {
            competition.aggregated_nonces =
                Some(coordinator_sessions.aggregated_nonces().to_owned());
        }
        if competition.partial_signatures.is_none() {
            competition.partial_signatures =
                Some(coordinator_sessions.our_partial_signatures().to_owned());
        }
        competition.errors = vec![];

        Ok(competition)
    }

    pub async fn sign_dlc_contract<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        let Some(contract_parameters) = &competition.contract_parameters else {
            return Err(anyhow!(
                "contract parameters don't exists, failed signing competition dlc contract {}",
                competition.id
            ));
        };
        let Some(funding_outpoint) = &competition.funding_outpoint else {
            return Err(anyhow!(
                "funding outpoint doesn't exists, failed signing competition dlc contract {}",
                competition.id
            ));
        };

        let Some(coordinator_partial_sigantures) = competition.partial_signatures.clone() else {
            return Err(
                anyhow!("coordinator partial signatures do not exist, failed signing competition dlc contract {}", competition.id),
            );
        };

        let Some(coordinator_nonces) = competition.public_nonces.as_ref() else {
            return Err(anyhow!("coordinator nonces missing"));
        };

        let ticketed_dlc =
            TicketedDLC::new(contract_parameters.to_owned(), funding_outpoint.to_owned())?;

        let signing_session = {
            let mut rng = create_deterministic_rng(&funding_outpoint, self.private_key);
            SigningSession::<NonceSharingRound>::new(ticketed_dlc, &mut rng, self.private_key)?
        };

        if signing_session.our_public_nonces() != coordinator_nonces {
            return Err(anyhow!("coordinator nonce mismatch"));
        }

        let received_nonces = self.get_received_nonces(competition.id).await?;
        debug!("Received all aggregate nonces from entries");

        let coordinator_session =
            signing_session.aggregate_nonces_and_compute_partial_signatures(received_nonces)?;
        debug!("Built coordinator session before publishing");

        if *coordinator_session.our_partial_signatures() != coordinator_partial_sigantures {
            return Err(anyhow!("coordinator partial signatures mismatch"));
        }

        let final_signatures_by_sender: BTreeMap<Point, FinalSignatures> =
            self.get_final_sigs_by_sender(competition.id).await?;
        let mut partial_sigs_by_sender: BTreeMap<Point, SigMap<PartialSignature>> = BTreeMap::new();

        for (sender_pubkey, final_signature) in &final_signatures_by_sender {
            match coordinator_session
                .verify_partial_signatures(*sender_pubkey, &final_signature.partial_signatures)
            {
                Ok(_) => {
                    partial_sigs_by_sender.insert(
                        sender_pubkey.clone(),
                        final_signature.partial_signatures.clone(),
                    );

                    debug!(
                        "✓ Signature verification succeeded for player {}",
                        sender_pubkey
                    )
                }
                Err(e) => {
                    error!(
                        "✗ Signature verification failed for player {}: {}",
                        sender_pubkey, e
                    );
                    return Err(anyhow!(
                        "Signature verification failed for {:?}: {}",
                        sender_pubkey,
                        e
                    ));
                }
            }
        }

        debug!("Verified all partial signatures");

        let signed_contract =
            coordinator_session.aggregate_all_signatures(partial_sigs_by_sender)?;

        debug!("Signed dlc contract");
        if competition.signed_contract.is_none() {
            competition.signed_contract = Some(signed_contract);
            competition.signed_at = Some(OffsetDateTime::now_utc());
        }
        competition.errors = vec![];

        Ok(competition)
    }

    async fn sign_and_broadcast_funding_tx<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        let Some(funding_psbt_base64) = competition.funding_psbt_base64.clone() else {
            return Err(anyhow!(
                        "Unsigned funding psbt doesn't exists, failed publishing competition {} funding transaction",
                        competition.id
                    ));
        };

        let mut funding_psbt = Psbt::from_str(&funding_psbt_base64)?;

        let final_signatures_by_sender: BTreeMap<Point, FinalSignatures> =
            self.get_final_sigs_by_sender(competition.id).await?;

        if (final_signatures_by_sender.len() as u64) < competition.total_entries {
            return Err(anyhow!(
                "Not enough final signatures for competition {}",
                competition.id
            ));
        }

        debug!("Merging all funding psbts");

        for (sender_pubkey, final_signature) in &final_signatures_by_sender {
            let sender_funding_psbt = Psbt::from_str(&final_signature.funding_psbt_base64)?;
            match funding_psbt.combine(sender_funding_psbt) {
                Ok(_) => {
                    debug!(
                        "✓ Funding PSBT combination succeeded for player {}",
                        sender_pubkey
                    );
                }
                Err(err) => {
                    error!(
                        "✗ Funding PSBT combination failed for player {}: {}",
                        sender_pubkey, err
                    );
                }
            }
        }

        debug!("Combined all psbts");

        let funding_transaction = signed_funding_tx(self.bitcoin.clone(), funding_psbt).await?;

        debug!(
            "Broadcasting funding transaction: {:?}",
            funding_transaction
        );

        self.bitcoin.broadcast(&funding_transaction).await?;

        if competition.funding_broadcasted_at.is_none() {
            competition.funding_broadcasted_at = Some(OffsetDateTime::now_utc());
            competition.funding_transaction = Some(funding_transaction);
        }

        Ok(competition)
    }

    async fn check_funding_confirmation<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        let funding_tx = competition.funding_transaction.as_ref().ok_or_else(|| {
            anyhow!(
                "No funding transaction found for competition {}",
                competition.id
            )
        })?;

        let txid = funding_tx.compute_txid();
        match self.bitcoin.get_tx_confirmation_height(&txid).await? {
            Some(confirmations) if confirmations >= self.required_confirmations => {
                info!(
                    "Funding transaction {} confirmed with {} confirmations for competition {}",
                    txid, confirmations, competition.id
                );
                if competition.funding_confirmed_at.is_none() {
                    competition.funding_confirmed_at = Some(OffsetDateTime::now_utc());
                }
            }
            Some(confirmations) => {
                debug!(
                    "Funding transaction {} has {} of {} required confirmations for competition {}",
                    txid, confirmations, self.required_confirmations, competition.id
                );
            }
            None => {
                debug!(
                    "Funding transaction {} not yet confirmed for competition {}",
                    txid, competition.id
                );
            }
        }
        competition.errors = vec![];

        Ok(competition)
    }

    pub async fn check_oracle_attestation<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        if competition.attestation.is_some() {
            return Ok(competition);
        }

        let Some(signed_contract) = competition.signed_contract.as_ref() else {
            return Err(anyhow!(
                "No signed contract found for competition {}",
                competition.id
            ));
        };

        if let Some(expiry) = signed_contract.dlc().params().event.expiry {
            let current_time = self
                .bitcoin
                .get_confirmed_blockchain_time(REQUIRED_CONFIRMATIONS_FOR_TIME)
                .await?;

            if current_time > expiry as u64 {
                // Get the expiry transaction
                let Some(expiry_tx) = signed_contract.expiry_tx() else {
                    return Err(anyhow!(
                        "No expiry transaction found for competition {}",
                        competition.id
                    ));
                };

                debug!(
                    "Broadcasting expiry transaction, current time {} expiry_tx lock time {} : {:?}",
                    current_time, expiry_tx.lock_time, expiry_tx
                );

                if competition.expiry_broadcasted_at.is_none() {
                    debug!("expiry_tx: {:?}", expiry_tx);
                    self.bitcoin.broadcast(&expiry_tx).await?;
                    competition.expiry_broadcasted_at = Some(OffsetDateTime::now_utc())
                };

                return Ok(competition);
            }
        }

        let event = self.oracle_client.get_event(&competition.id).await?;
        let Some(attestation) = event.attestation else {
            info!(
                "No oracle attestation found for competition {} yet, skipping add",
                competition.id
            );
            return Ok(competition);
        };
        debug!("attestation above verification: {:?}", attestation);

        match competition.verify_event_attestation(&attestation) {
            Ok(outcome) => {
                info!(
                    "Oracle attestation verified for competition {}: {}",
                    competition.id, outcome
                );
            }
            Err(e) => {
                error!(
                    "Oracle attestation verification failed for competition {}: {}",
                    competition.id, e
                );
                return Err(anyhow!("Oracle attestation verification failed: {}", e));
            }
        }
        if competition.attestation.is_none() {
            competition.attestation = Some(attestation);
        }

        info!(
            "Oracle attestation added for competition {}",
            competition.id
        );
        competition.errors = vec![];

        Ok(competition)
    }

    pub async fn publish_outcome_transaction<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        let Some(attestation) = competition.attestation else {
            return Err(anyhow!(
                "No attestation found for competition {}",
                competition.id
            ));
        };
        debug!(
            "attestation at publish_outcome_transaction: {:?}",
            attestation
        );
        let Some(signed_contract) = competition.signed_contract.as_ref() else {
            return Err(anyhow!(
                "No signed contract found for competition {}",
                competition.id
            ));
        };

        let Some(event_announcement) = competition.event_announcement.as_ref() else {
            return Err(anyhow!(
                "No event announcement found for competition {}",
                competition.id
            ));
        };

        if let Some(expiry) = event_announcement.expiry {
            let current_time = self
                .bitcoin
                .get_confirmed_blockchain_time(REQUIRED_CONFIRMATIONS_FOR_TIME)
                .await?;

            if current_time > expiry as u64 {
                // Get the expiry transaction
                let Some(expiry_tx) = signed_contract.expiry_tx() else {
                    return Err(anyhow!(
                        "No expiry transaction found for competition {}",
                        competition.id
                    ));
                };

                debug!(
                    "Broadcasting expiry transaction: {:?}",
                    expiry_tx.compute_txid()
                );

                if competition.expiry_broadcasted_at.is_none() {
                    debug!("expiry_tx: {:?}", expiry_tx);
                    self.bitcoin.broadcast(&expiry_tx).await?;
                    competition.expiry_broadcasted_at = Some(OffsetDateTime::now_utc())
                };

                return Ok(competition);
            }
        }

        let outcome = competition.get_current_outcome()?;

        let outcome_index = match outcome {
            Outcome::Attestation(i) => i,
            Outcome::Expiry => return Err(anyhow!("Unexpected expiry outcome")),
        };

        if !event_announcement.is_valid_outcome(&outcome) {
            return Err(anyhow!("Invalid outcome for this contract"));
        }

        let outcome_tx = signed_contract.signed_outcome_tx(outcome_index, attestation)?;

        let tx_hex = consensus::encode::serialize_hex(&outcome_tx);
        debug!("Raw transaction hex: {}", tx_hex);
        debug!("Transaction ID: {}", outcome_tx.compute_txid());
        competition.outcome_transaction = Some(outcome_tx.clone());
        if competition.outcome_broadcasted_at.is_none() {
            self.bitcoin.broadcast(&outcome_tx).await?;
            competition.outcome_broadcasted_at = Some(OffsetDateTime::now_utc());
        }
        competition.errors = vec![];

        Ok(competition)
    }

    pub async fn publish_delta_transactions<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        let Some(signed_contract) = competition.signed_contract.as_ref() else {
            return Err(anyhow!(
                "No signed contract found for competition {}",
                competition.id
            ));
        };

        if competition.outcome_broadcasted_at.is_none() {
            return Err(anyhow!(
                "No outcome broadcast timestamp found for competition {}",
                competition.id
            ));
        };
        let Some(outcome_transaction) = competition.outcome_transaction.as_ref() else {
            return Err(anyhow!(
                "No outcome transaction found for competition {}",
                competition.id
            ));
        };
        let current_height = self.bitcoin.get_current_height().await?;
        let Some(outcome_height) = self
            .bitcoin
            .get_tx_confirmation_height(&outcome_transaction.compute_txid())
            .await?
        else {
            info!(
                "Outcome transaction not confirmed yet for competition {}",
                competition.id
            );
            return Ok(competition);
        };

        let blocks_since_outcome = current_height - outcome_height;
        let required_delta = signed_contract.params().relative_locktime_block_delta as u32;

        if blocks_since_outcome < required_delta {
            info!(
                "Not enough blocks since outcome tx for competition {}. Need {} more blocks",
                competition.id,
                required_delta - blocks_since_outcome
            );
            return Ok(competition);
        };

        // Get outcome and winner information
        let outcome = competition.get_current_outcome()?;
        let winners = signed_contract
            .params()
            .outcome_payouts
            .get(&outcome)
            .ok_or_else(|| anyhow!("No payout mapping found for outcome"))?;

        let entries = self
            .competition_store
            .get_competition_entries(competition.id, vec![EntryStatus::Paid])
            .await?;

        // Get fee rate for transactions
        let fee_rates = self.bitcoin.get_estimated_fee_rates().await?;
        let rate_confirm_within_2_blocks = if fee_rates.is_empty() {
            1 as u64
        } else {
            fee_rates[&1_u16].ceil() as u64
        };
        let fee_rate = FeeRate::from_sat_per_vb_unchecked(rate_confirm_within_2_blocks);

        // Check if we can do a unified close
        let paid_winners: Vec<(PlayerIndex, &UserEntry)> = winners
            .iter()
            .filter_map(|(&player_index, _)| {
                entries
                    .iter()
                    .find(|entry| {
                        let Ok(pubkey) = Point::from_hex(&entry.ephemeral_pubkey) else {
                            return false;
                        };
                        if let Some(player) =
                            signed_contract.params().players.get(player_index.clone())
                        {
                            player.pubkey == pubkey
                        } else {
                            false
                        }
                    })
                    .map(move |entry| (player_index, entry))
            })
            .filter(|(_, entry)| {
                entry.paid_out_at.is_some()
                    && entry.ephemeral_privatekey.is_some()
                    && entry.sellback_broadcasted_at.is_none()
            })
            .collect();

        if paid_winners.len() != winners.len() {
            // Technically we are good to broadcast the first delta transaction
            // once blocks_since_outcome < required_delta, we add this wait to
            // give users more time to be paid out via lightning
            if blocks_since_outcome < (2 * required_delta) {
                info!(
                    "Not enough blocks since outcome tx. Need {} more blocks",
                    (2 * required_delta) - blocks_since_outcome
                );
                return Ok(competition);
            }
        }

        if paid_winners.len() == winners.len() {
            // All winners have paid out and none have had sellback broadcast - do unified close
            let (close_tx_input, close_tx_prevout) =
                signed_contract.outcome_close_tx_input_and_prevout(&outcome)?;

            let mut close_tx = simple_sweep_tx(
                signed_contract.params().market_maker.pubkey,
                close_tx_input.clone(),
                signed_contract.close_tx_input_weight(),
                close_tx_prevout.value,
                fee_rate,
            );

            let winner_seckeys: BTreeMap<Point, Scalar> = paid_winners
                .iter()
                .filter_map(|(_, entry)| {
                    let seckey = Scalar::from_hex(entry.ephemeral_privatekey.as_ref()?).ok()?;
                    let pubkey = Point::from_hex(&entry.ephemeral_pubkey).ok()?;
                    Some((pubkey, seckey))
                })
                .collect();

            let input_index = close_tx_input.previous_output.vout.clone() as usize;

            signed_contract.sign_outcome_close_tx_input(
                &outcome,
                &mut close_tx,
                input_index,
                &Prevouts::All(&[close_tx_prevout]),
                self.private_key,
                &winner_seckeys,
            )?;

            debug!("Broadcasting unified close transaction");
            if competition.delta_broadcasted_at.is_none() {
                self.bitcoin.broadcast(&close_tx).await?;
                competition.delta_broadcasted_at = Some(OffsetDateTime::now_utc());
            }

            // Mark all entries as closed
            let now = OffsetDateTime::now_utc();
            for (_, entry) in paid_winners {
                self.competition_store
                    .mark_entry_sellback_broadcast(entry.id, now)
                    .await?;
            }
        } else {
            // Handle individual cooperative closes for paid winners
            for (player_index, entry) in paid_winners {
                // Skip if already processed
                if entry.sellback_broadcasted_at.is_some() {
                    continue;
                }

                let win_condition = WinCondition {
                    outcome,
                    player_index,
                };

                let (close_tx_input, close_tx_prevout) =
                    signed_contract.split_close_tx_input_and_prevout(&win_condition)?;

                let mut close_tx = simple_sweep_tx(
                    signed_contract.params().market_maker.pubkey,
                    close_tx_input.clone(),
                    signed_contract.close_tx_input_weight(),
                    close_tx_prevout.value,
                    fee_rate,
                );

                let winner_seckey = Scalar::from_hex(entry.ephemeral_privatekey.as_ref().unwrap())
                    .map_err(|e| anyhow!("Invalid winner secret key: {}", e))?;

                let input_index = close_tx_input.previous_output.vout.clone() as usize;

                signed_contract.sign_split_close_tx_input(
                    &win_condition,
                    &mut close_tx,
                    input_index,
                    &Prevouts::All(&[close_tx_prevout]),
                    self.private_key,
                    winner_seckey,
                )?;

                debug!(
                    "Broadcasting individual close transaction for player {}",
                    player_index
                );

                if competition.delta_broadcasted_at.is_none() {
                    self.bitcoin.broadcast(&close_tx).await?;
                }

                // Mark entry as closed
                self.competition_store
                    .mark_entry_sellback_broadcast(entry.id, OffsetDateTime::now_utc())
                    .await?;
            }
            competition.delta_broadcasted_at = Some(OffsetDateTime::now_utc());
        }
        competition.errors = vec![];

        Ok(competition)
    }

    pub async fn publish_delta2_transactions<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        let Some(signed_contract) = competition.signed_contract.as_ref() else {
            return Err(anyhow!(
                "No signed contract found for competition {}",
                competition.id
            ));
        };

        let Some(outcome_transaction) = competition.outcome_transaction.as_ref() else {
            return Err(anyhow!(
                "No outcome transaction found for competition {}",
                competition.id
            ));
        };

        if competition.outcome_broadcasted_at.is_none() {
            return Err(anyhow!(
                "No outcome broadcast timestamp found for competition {}",
                competition.id
            ));
        };

        let current_height = self.bitcoin.get_current_height().await?;
        let Some(outcome_height) = self
            .bitcoin
            .get_tx_confirmation_height(&outcome_transaction.compute_txid())
            .await?
        else {
            info!(
                "Outcome transaction not confirmed yet for competition {}",
                competition.id
            );
            return Ok(competition);
        };

        let blocks_since_outcome = current_height - outcome_height;
        let required_delta = signed_contract.params().relative_locktime_block_delta as u32;

        if blocks_since_outcome < (2 * required_delta) {
            info!(
                "Not enough blocks since outcome tx. Need {} more blocks",
                (2 * required_delta) - blocks_since_outcome
            );
            return Ok(competition);
        }

        // Get outcome and winner information
        let outcome = competition.get_current_outcome()?;
        let winners = signed_contract
            .params()
            .outcome_payouts
            .get(&outcome)
            .ok_or_else(|| anyhow!("No payout mapping found for outcome"))?;

        let entries = self
            .competition_store
            .get_competition_entries(competition.id, vec![EntryStatus::Paid])
            .await?;

        // Get fee rate for transactions
        let fee_rates = self.bitcoin.get_estimated_fee_rates().await?;
        let rate_confirm_within_2_blocks = if fee_rates.is_empty() {
            1 as u64
        } else {
            fee_rates[&1_u16].ceil() as u64
        };
        let fee_rate = FeeRate::from_sat_per_vb_unchecked(rate_confirm_within_2_blocks);

        // Get unpaid winners that haven't been reclaimed
        for (&player_index, _) in winners {
            if let Some(entry) = entries.iter().find(|entry| {
                let Ok(pubkey) = Point::from_hex(&entry.ephemeral_pubkey) else {
                    return false;
                };
                if let Some(player) = signed_contract.params().players.get(player_index.clone()) {
                    player.pubkey == pubkey
                } else {
                    false
                }
            }) {
                // Skip if already processed or if paid out
                if entry.reclaimed_broadcasted_at.is_some() || entry.paid_out_at.is_some() {
                    continue;
                }

                let win_condition = WinCondition {
                    outcome,
                    player_index,
                };

                let (reclaim_tx_input, reclaim_tx_prevout) =
                    signed_contract.split_reclaim_tx_input_and_prevout(&win_condition)?;

                let mut reclaim_tx = simple_sweep_tx(
                    signed_contract.params().market_maker.pubkey,
                    reclaim_tx_input.clone(),
                    signed_contract.split_reclaim_tx_input_weight(),
                    reclaim_tx_prevout.value,
                    fee_rate,
                );

                let input_index = reclaim_tx_input.previous_output.vout.clone() as usize;

                signed_contract.sign_split_reclaim_tx_input(
                    &win_condition,
                    &mut reclaim_tx,
                    input_index,
                    &Prevouts::All(&[reclaim_tx_prevout]),
                    self.private_key,
                )?;

                debug!(
                    "Broadcasting reclaim transaction for player {}",
                    player_index
                );

                if competition.completed_at.is_none() {
                    self.bitcoin.broadcast(&reclaim_tx).await?;
                }
                // Mark entry as reclaimed
                self.competition_store
                    .mark_entry_reclaim_broadcast(entry.id, OffsetDateTime::now_utc())
                    .await?;
            }
        }
        competition.completed_at = Some(OffsetDateTime::now_utc());
        competition.errors = vec![];

        Ok(competition)
    }

    //Nonces from every entry into competition
    pub async fn get_received_nonces(
        &self,
        competition_id: Uuid,
    ) -> Result<BTreeMap<Point, SigMap<PubNonce>>, anyhow::Error> {
        // Get all entries for this competition
        let entries = self
            .competition_store
            .get_competition_entries(competition_id, vec![EntryStatus::Paid])
            .await?;

        let mut nonces_map: BTreeMap<Point, SigMap<PubNonce>> = BTreeMap::new();
        let entry_count = entries.len();

        for entry in entries {
            // Skip entries that haven't submitted nonces
            let Some(public_nonces) = entry.public_nonces else {
                continue;
            };

            // Convert ephemeral pubkey string to Point
            let pubkey = Point::from_hex(&entry.ephemeral_pubkey)
                .map_err(|e| anyhow!("Invalid ephemeral pubkey for entry {}: {}", entry.id, e))?;

            nonces_map.insert(pubkey, public_nonces);
        }

        // Verify we have nonces from all entries
        if nonces_map.len() != entry_count {
            return Err(anyhow!(
                "Missing nonces from some entries. Expected {}, got {}",
                entry_count,
                nonces_map.len()
            ));
        }

        Ok(nonces_map)
    }

    //Signed transaction from every entry into competition
    pub async fn get_final_sigs_by_sender(
        &self,
        competition_id: Uuid,
    ) -> Result<BTreeMap<Point, FinalSignatures>, anyhow::Error> {
        // Get all entries for this competition
        let entries = self
            .competition_store
            .get_competition_entries(competition_id, vec![EntryStatus::Paid])
            .await?;

        let entry_count = entries.len();
        let mut sigs_map: BTreeMap<Point, FinalSignatures> = BTreeMap::new();

        for entry in entries {
            // Skip entries that haven't submitted partial signatures
            let Some(partial_signatures) = entry.partial_signatures else {
                continue;
            };

            // Skip entries that haven't submitted signed funding psbt
            let Some(funding_psbt_base64) = entry.funding_psbt_base64 else {
                continue;
            };

            // Verify entry has been signed (has a signed_at timestamp)
            if entry.signed_at.is_none() {
                continue;
            }

            // Convert ephemeral pubkey string to Point
            let pubkey = Point::from_hex(&entry.ephemeral_pubkey)
                .map_err(|e| anyhow!("Invalid ephemeral pubkey for entry {}: {}", entry.id, e))?;

            sigs_map.insert(
                pubkey,
                FinalSignatures {
                    partial_signatures,
                    funding_psbt_base64,
                },
            );
        }

        // Verify we have signatures from all entries
        if sigs_map.len() != entry_count {
            return Err(anyhow!(
                "Missing final signatures from some entries. Expected {}, got {}",
                entry_count,
                sigs_map.len()
            ));
        }

        Ok(sigs_map)
    }

    pub async fn validate_coordinator_metadata(&self) -> Result<(), anyhow::Error> {
        let stored_public_key = match self.competition_store.get_stored_public_key().await {
            Ok(key) => key,
            Err(sqlx::Error::RowNotFound) => {
                self.add_metadata().await?;
                return Ok(());
            }
            Err(e) => return Err(anyhow!("error getting stored public key: {}", e)),
        };
        let dlc_pubkey = Point::from(self.public_key);
        let (xonly, _) = dlc_pubkey.into();
        let bitcoin_key = convert_xonly_key(xonly);

        if stored_public_key != bitcoin_key {
            return Err(anyhow!(
                "stored_pubkey: {:?} pem_pubkey: {:?}",
                stored_public_key,
                self.public_key()
            ));
        }
        Ok(())
    }

    async fn add_metadata(&self) -> Result<(), anyhow::Error> {
        let dlc_pubkey = Point::from(self.public_key);
        let (xonly, _) = dlc_pubkey.into();
        let bitcoin_key = convert_xonly_key(xonly);

        self.competition_store
            .add_coordinator_metadata(self.name.clone(), bitcoin_key)
            .await
            .map_err(|e| anyhow!("failed to add coordinator metadata: {}", e))
    }

    pub async fn create_competition(
        &self,
        create_event: CreateEvent,
    ) -> Result<Competition, Error> {
        let competition = Competition::new(&create_event);

        if competition.event_submission.number_of_places_win > 5 {
            return Err(Error::BadRequest(format!(
                "Number of winners exceeds maximum allowed 5 {}",
                competition.event_submission.number_of_places_win
            )));
        }

        debug!("created competition");
        let tickets = competition
            .generate_competition_tickets(create_event.total_allowed_entries)
            .await?;
        debug!("tickets: {:?}", tickets);
        let competition = self
            .competition_store
            .add_competition_with_tickets(competition, tickets)
            .map_err(|e| {
                error!(
                    "competition added to oracle, but failed to be saved with tickets: competition_id {} {:?}",
                    create_event.id, e
                );
                Error::DbError(e)
            })
            .await?;

        Ok(competition)
    }

    pub async fn get_competitions(&self) -> Result<Vec<Competition>, Error> {
        self.competition_store
            .get_competitions(false, false)
            .map_err(|e| {
                error!("failed to get competitions: {:?}", e);
                Error::DbError(e)
            })
            .await
    }

    pub async fn request_ticket(
        &self,
        pubkey: String,
        competition_id: Uuid,
        btc_pubkey: BitcoinPublicKey,
    ) -> Result<TicketResponse, Error> {
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;
        if competition.total_entries as usize >= competition.event_submission.total_allowed_entries
        {
            return Err(Error::CompetitionFull);
        }
        debug!("got competition: {:?}", competition);

        // Get ticket
        let ticket = self
            .competition_store
            .get_and_reserve_ticket(competition_id, &pubkey)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => Error::NoAvailableTickets,
                e => Error::DbError(e),
            })?;
        match self
            .create_ticket_response(ticket.clone(), btc_pubkey, competition)
            .await
        {
            Ok(response) => Ok(response),
            Err(e) => {
                if let Err(clear_err) = self
                    .competition_store
                    .clear_ticket_reservation(ticket.id)
                    .await
                {
                    error!(
                        "Failed to clear ticket reservation for ticket {}: {}",
                        ticket.id, clear_err
                    );
                }
                Err(e)
            }
        }
    }

    async fn create_ticket_response(
        &self,
        ticket: Ticket,
        btc_pubkey: BitcoinPublicKey,
        competition: Competition,
    ) -> Result<TicketResponse, Error> {
        // Decode preimage from encrypted_preimage
        let preimage = hex::decode(&ticket.encrypted_preimage)
            .map_err(|_| Error::BadRequest("Invalid preimage".into()))?;

        // Calculate payment hash from preimage
        let payment_hash = sha256::Hash::hash(&preimage).to_byte_array();

        //Note: we are currently NOT refunding the coordinator fee, we may want to change that
        // Generate escrow transaction BEFORE creating the invoice
        let escrow_tx = generate_escrow_tx(
            self.bitcoin.clone(),
            ticket.id,
            btc_pubkey,
            payment_hash,
            competition.event_submission.entry_fee as u64,
        )
        .await
        .map_err(|e| {
            error!("Failed to generate escrow transaction: {}", e);
            Error::BadRequest(format!("Failed to generate refund transaction: {}", e))
        })?;

        debug!("escrow_tx: {:?}", escrow_tx);

        let escrow_tx_hex = hex::encode(dlctix::bitcoin::consensus::encode::serialize(&escrow_tx));

        // Store the refund transaction in the database
        self.competition_store
            .update_ticket_escrow(ticket.id, btc_pubkey.to_string(), escrow_tx_hex.clone())
            .await
            .map_err(|e| {
                error!("Failed to update ticket with refund transaction: {}", e);
                Error::DbError(e)
            })?;

        let fee_multiplier = competition.event_submission.coordinator_fee_percentage as f64 / 100.0;
        let coordinator_fee =
            (competition.event_submission.entry_fee as f64 * fee_multiplier).round() as u64;

        let full_fee = (competition.event_submission.entry_fee as u64) + coordinator_fee;

        // Now create the HODL invoice with short timeout
        let invoice = self
            .ln
            .add_hold_invoice(
                full_fee,
                300, // 5 minute timeout
                hex::encode(payment_hash),
                ticket.competition_id,
                escrow_tx_hex.clone(),
            )
            .await
            .map_err(|e| {
                error!("Failed to create HODL invoice: {}", e);
                Error::BadRequest(format!("Failed to create invoice: {}", e))
            })?;

        // Update ticket with payment request
        self.competition_store
            .update_ticket_payment_request(ticket.id, &invoice.payment_request)
            .await
            .map_err(|e| {
                error!("Failed to update ticket with payment request: {}", e);
                Error::DbError(e)
            })?;

        debug!(
            "Created ticket {} with refund tx {} and invoice",
            ticket.id,
            escrow_tx.compute_txid()
        );

        Ok(TicketResponse {
            ticket_id: ticket.id,
            payment_request: invoice.payment_request,
            escrow_tx: Some(escrow_tx_hex),
            payment_hash: hex::encode(payment_hash),
            amount_sats: full_fee,
        })
    }

    pub async fn get_ticket_status(
        &self,
        user_pubkey: String,
        competition_id: Uuid,
        ticket_id: Uuid,
    ) -> Result<TicketStatus, Error> {
        let ticket = self
            .competition_store
            .get_ticket(ticket_id)
            .await
            .map_err(|e| {
                debug!("error: {:?}", e);
                match e {
                    sqlx::Error::RowNotFound => Error::NotFound("Ticket not found".into()),
                    e => Error::DbError(e),
                }
            })?;

        // Verify this ticket belongs to this competition
        if ticket.competition_id != competition_id {
            return Err(Error::BadRequest(
                "Ticket does not belong to this competition".into(),
            ));
        }

        // Verify this ticket was reserved by this user
        if ticket.reserved_by.as_deref() != Some(&user_pubkey) {
            return Err(Error::BadRequest("Ticket not reserved by this user".into()));
        }

        Ok(ticket.get_status())
    }

    pub async fn get_competition(&self, competition_id: Uuid) -> Result<Competition, Error> {
        self.competition_store
            .get_competition(competition_id)
            .map_err(|e| {
                error!(
                    "failed to get competition: competition_id {} {:?}",
                    competition_id, e
                );
                Error::DbError(e)
            })
            .await
    }

    pub async fn handle_invoice_accepted(
        &self,
        competition_id: Uuid,
        ticket_hash: &str,
    ) -> Result<(), Error> {
        self.competition_store
            .mark_ticket_paid(ticket_hash, competition_id)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => {
                    Error::BadRequest("Invalid ticket or competition".into())
                }
                e => Error::DbError(e),
            })?;

        Ok(())
    }

    pub async fn add_entry(&self, pubkey: String, entry: AddEntry) -> Result<UserEntry, Error> {
        let competition = self
            .competition_store
            .get_competition(entry.event_id)
            .await
            .map_err(|e| {
                error!("error {:?}", e);
                match e {
                    sqlx::Error::RowNotFound => Error::BadRequest("Competition not found".into()),
                    e => Error::DbError(e),
                }
            })?;

        validate_entry(entry.clone().into(), competition).await?;

        debug!("entry: {:?}", entry);
        let ticket = self
            .competition_store
            .get_ticket(entry.ticket_id)
            .await
            .map_err(|e| {
                error!("error {:?}", e);
                match e {
                    sqlx::Error::RowNotFound => Error::BadRequest("Ticket not found".into()),
                    e => Error::DbError(e),
                }
            })?;

        if ticket.reserved_by.as_deref() != Some(&pubkey) {
            return Err(Error::BadRequest("Ticket not reserved by this user".into()));
        }

        if ticket.paid_at.is_none() {
            return Err(Error::BadRequest("Ticket has not been paid".into()));
        }

        if ticket.entry_id.is_some() {
            return Err(Error::BadRequest("Ticket has already been used".into()));
        }

        if let Some(btc_pubkey) = &ticket.ephemeral_pubkey {
            if btc_pubkey != &entry.ephemeral_pubkey {
                return Err(Error::BadRequest(format!(
                    "Entry public key {} must match ticket escrow public key {}",
                    entry.ephemeral_pubkey, btc_pubkey
                )));
            }
        }

        let user_entry = self
            .competition_store
            .add_entry(entry.clone().into_user_entry(pubkey), ticket.id)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => {
                    Error::BadRequest(
                        "Failed to claim ticket - may have expired or been claimed by another entry"
                            .into(),
                    )
                }
                e => {
                    error!(
                        "entry added to oracle, but failed to be saved: entry_id {}, event_id {} {:?}",
                        entry.id, entry.event_id, e
                    );
                    Error::DbError(e)
                }
            })?;

        Ok(user_entry)
    }

    pub async fn get_entries(
        &self,
        pubkey: String,
        filter: SearchBy,
    ) -> Result<Vec<UserEntry>, Error> {
        self.competition_store
            .get_user_entries(pubkey, filter)
            .map_err(Error::DbError)
            .await
    }

    pub async fn get_contract_parameters(
        &self,
        pubkey: String,
        competition_id: Uuid,
    ) -> Result<FundedContract, Error> {
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;

        // Verify user has an entry in this competition
        let entries = self
            .competition_store
            .get_user_entries(
                pubkey,
                SearchBy {
                    event_ids: Some(vec![competition_id]),
                },
            )
            .await?;

        if entries.is_empty() {
            return Err(Error::NotFound(format!(
                "No entry found for user in competition {}",
                competition_id
            )));
        }

        let contract = competition.contract_parameters.ok_or_else(|| {
            Error::NotFound(format!(
                "Contract parameters not yet available for competition {}",
                competition_id
            ))
        })?;

        let funding_outpoint = competition.funding_outpoint.ok_or_else(|| {
            Error::NotFound(format!(
                "Funding transaction is not yet available for competition {}",
                competition_id
            ))
        })?;

        let funding_psbt_base64 = competition.funding_psbt_base64.ok_or_else(|| {
            Error::NotFound(format!(
                "Funding psbt is not yet available for competition {}",
                competition_id
            ))
        })?;

        Ok(FundedContract {
            contract_params: contract,
            funding_outpoint,
            funding_psbt_base64,
        })
    }

    pub async fn submit_public_nonces(
        &self,
        pubkey: String,
        competition_id: Uuid,
        entry_id: Uuid,
        public_nonces: SigMap<PubNonce>,
    ) -> Result<(), Error> {
        debug!(
            "Submitting nonces for entry {}: {:?}",
            entry_id, public_nonces
        );
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;

        if competition.contract_parameters.is_none() {
            return Err(Error::BadRequest(
                "Contract parameters not yet available".to_string(),
            ));
        }

        if public_nonces.is_mirror(&competition.public_nonces.unwrap()) {
            return Err(Error::BadRequest(format!(
                "public nonces do not match competitions: {}",
                entry_id
            )));
        }

        let entries = self
            .competition_store
            .get_user_entries(
                pubkey,
                SearchBy {
                    event_ids: Some(vec![competition_id]),
                },
            )
            .await?;

        let entry = entries
            .iter()
            .find(|e| e.id == entry_id)
            .ok_or_else(|| Error::NotFound(format!("Entry {} not found", entry_id)))?;

        if entry.public_nonces.is_some() {
            return Err(Error::BadRequest(
                "Public nonces already submitted for this entry".to_string(),
            ));
        }

        self.competition_store
            .add_public_nonces(entry_id, public_nonces)
            .await
            .map_err(|e| {
                error!(
                    "failed save entry public nonces: entry_id {}, event_id {} {:?}",
                    entry_id, competition_id, e
                );
                Error::DbError(e)
            })?;

        Ok(())
    }

    pub async fn get_aggregate_nonces(
        &self,
        pubkey: String,
        competition_id: Uuid,
    ) -> Result<SigMap<AggNonce>, Error> {
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;

        let entries = self
            .competition_store
            .get_user_entries(
                pubkey,
                SearchBy {
                    event_ids: Some(vec![competition_id]),
                },
            )
            .await?;

        if entries.is_empty() {
            return Err(Error::NotFound(format!(
                "No entry found for user in competition {}",
                competition_id
            )));
        }

        competition.aggregated_nonces.ok_or_else(|| {
            Error::NotFound(format!(
                "Aggregate nonces not yet available for competition {}",
                competition_id
            ))
        })
    }

    pub async fn submit_final_signatures(
        &self,
        pubkey: String,
        competition_id: Uuid,
        entry_id: Uuid,
        final_signatures: FinalSignatures,
    ) -> Result<(), Error> {
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;

        if competition.contract_parameters.is_none() {
            return Err(Error::BadRequest(
                "Contract parameters not yet available".to_string(),
            ));
        }

        let Some(comp_partial_signatures) = competition.partial_signatures else {
            return Err(Error::BadRequest(
                "Contract partial_signatures not yet available".to_string(),
            ));
        };

        if final_signatures
            .partial_signatures
            .is_mirror(&comp_partial_signatures)
        {
            return Err(Error::BadRequest(format!(
                "partial signatures do not match competitions: {}",
                entry_id
            )));
        }
        debug!("adding signatures on entry {} for {}", entry_id, pubkey);
        let entries = self
            .competition_store
            .get_user_entries(
                pubkey,
                SearchBy {
                    event_ids: Some(vec![competition_id]),
                },
            )
            .await?;

        entries
            .iter()
            .find(|e| e.id == entry_id)
            .ok_or_else(|| Error::NotFound(format!("Entry {} not found", entry_id)))?;

        self.competition_store
            .add_final_signatures(entry_id, final_signatures)
            .await
            .map_err(|e| {
                error!(
                    "failed save entry signatures: entry_id {}, event_id {} {:?}",
                    entry_id, competition_id, e
                );
                Error::DbError(e)
            })?;

        Ok(())
    }

    pub async fn submit_ticket_payout(
        &self,
        pubkey: String,
        competition_id: Uuid,
        entry_id: Uuid,
        payout_info: PayoutInfo,
    ) -> Result<(), Error> {
        if payout_info.ln_invoice.is_empty() {
            return Err(Error::BadRequest("Invalid lightning invoice".into()));
        }

        // Get the competition and verify it's in a valid state for payouts
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;

        if !competition.is_attested() {
            return Err(Error::BadRequest(
                "Competition results not yet attested".into(),
            ));
        }

        if let Some(ref event_announcement) = competition.event_announcement {
            debug!("Locking points: {:?}", event_announcement.locking_points);
        }

        if competition.is_delta_broadcasted()
            || competition.is_expiry_broadcasted()
            || competition.is_completed()
        {
            return Err(Error::BadRequest(
                "Funds already received to user's on-chain key".into(),
            ));
        }

        // Get the entry and verify ownership
        let entries = self
            .competition_store
            .get_user_entries(
                pubkey.clone(),
                SearchBy {
                    event_ids: Some(vec![competition_id]),
                },
            )
            .await?;

        let entry = entries
            .iter()
            .find(|e| e.id == entry_id)
            .ok_or_else(|| Error::NotFound(format!("Entry {} not found", entry_id)))?;

        // Verify the ticket matches
        if entry.ticket_id != payout_info.ticket_id {
            return Err(Error::BadRequest("Invalid ticket for this entry".into()));
        }

        // Get competition tickets to verify against stored data
        let tickets = self.competition_store.get_tickets(competition_id).await?;
        _ = tickets
            .get(&entry.id)
            .ok_or_else(|| Error::NotFound("Ticket not found".into()))?;

        // Verify the entry hasn't already been paid out or pending a payout
        if let Some(paid_out_at) = entry.paid_out_at {
            return Err(Error::BadRequest(format!(
                "Entry {} already paid out at {}",
                entry.id, paid_out_at
            )));
        }

        // Get the current outcome
        let outcome = competition.get_current_outcome()?;
        debug!("Current outcome: {:?}", outcome);

        // Get the signed contract to verify winner
        let signed_contract = competition
            .signed_contract
            .as_ref()
            .ok_or_else(|| Error::BadRequest("Competition contract not yet signed".into()))?;

        // Verify this entry is a winner
        let winner_weights = signed_contract
            .params()
            .outcome_payouts
            .get(&outcome)
            .ok_or_else(|| Error::BadRequest("No payout mapping for outcome".into()))?;

        let ephemeral_pubkey = Point::from_hex(&entry.ephemeral_pubkey)
            .map_err(|e| Error::BadRequest(format!("Invalid ephemeral pubkey: {}", e)))?;

        debug!("Ephemeral pubkey: {:?}", ephemeral_pubkey);
        debug!("Winner weights: {:?}", winner_weights);
        debug!("Players: {:?}", signed_contract.params().players);

        let is_winner = winner_weights.iter().any(|(player_index, _)| {
            if let Some(player) = signed_contract.params().players.get(*player_index) {
                player.pubkey == ephemeral_pubkey
            } else {
                false
            }
        });

        if !is_winner {
            return Err(Error::BadRequest("Entry is not a winner".into()));
        }

        // Verify the private key matches the public key
        let provided_private_key = Scalar::from_hex(&payout_info.ephemeral_private_key)
            .map_err(|e| Error::BadRequest(format!("Invalid private key: {}", e)))?;

        let derived_public_key = provided_private_key.base_point_mul();
        if derived_public_key != ephemeral_pubkey {
            return Err(Error::BadRequest(
                "Invalid private key for this entry".into(),
            ));
        }

        // Verify the payout preimage matches the hash
        let provided_preimage =
            dlctix::hashlock::preimage_from_hex(&payout_info.payout_preimage)
                .map_err(|e| Error::BadRequest(format!("Invalid payout preimage: {}", e)))?;

        let derived_hash = dlctix::hashlock::sha256(&provided_preimage);
        if derived_hash.to_hex_string(dlctix::bitcoin::hex::Case::Lower) != entry.payout_hash {
            return Err(Error::BadRequest(
                "Invalid payout preimage for this entry".into(),
            ));
        }

        if payout_info.ln_invoice.is_empty() {
            return Err(Error::BadRequest("Invalid lightning invoice".into()));
        }

        // Calculate the payout amount based on winner's weight
        let total_pool_sats = signed_contract.params().funding_value.to_sat();
        let winner_weight = winner_weights
            .iter()
            .find_map(|(player_index, weight)| {
                if let Some(player) = signed_contract.params().players.get(*player_index) {
                    if player.pubkey == ephemeral_pubkey {
                        Some(*weight)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .ok_or_else(|| Error::BadRequest("Unable to determine winner weight".into()))?;

        let payout_amount_sats = (total_pool_sats * winner_weight) / 100;

        debug!(
            "Total pool: {} sats, Winner weight: {}%, Payout amount: {} sats",
            total_pool_sats, winner_weight, payout_amount_sats
        );

        let invoice_amount_sats =
            crate::ln_client::extract_amount_from_invoice(&payout_info.ln_invoice)
                .map_err(|e| Error::BadRequest(format!("Invalid lightning invoice: {}", e)))?;

        if let Some(invoice_amount_sats) = invoice_amount_sats {
            if invoice_amount_sats != payout_amount_sats {
                return Err(Error::BadRequest(format!(
                    "Invoice amount {} sats does not match expected payout {} sats",
                    invoice_amount_sats, payout_amount_sats
                )));
            }
        }

        match self
            .ln
            .send_payment(
                payout_info.ln_invoice.clone(),
                payout_amount_sats,
                60,   // TODO(@tee8z): make this timeout configurable, 60 second timeout
                1000, // TODO(@tee8z): make this fee configurable, 1000 sat fee limit
            )
            .await
        {
            Ok(_) => {
                // Store payout info but DON'T mark as paid out yet
                // The PayoutWatcher will monitor the payment and mark as paid when it settles
                self.competition_store
                    .store_payout_info_pending(
                        entry_id,
                        payout_info.payout_preimage,
                        payout_info.ephemeral_private_key,
                        payout_info.ln_invoice,
                        payout_amount_sats,
                    )
                    .await
                    .map_err(Error::DbError)
                    .inspect(|pay_out_id| info!("Payout initiated with ID: {}", pay_out_id))
                    .map(|_| ())
            }
            Err(e) => {
                return Err(Error::PaymentFailed(format!(
                    "Failed to initiate lightning payment: {}",
                    e
                )));
            }
        }
    }
}

fn generate_players(
    entries: &Vec<UserEntry>,
    tickets: &HashMap<Uuid, Ticket>,
) -> Result<Vec<Player>, anyhow::Error> {
    let mut players = vec![];
    for entry in entries {
        let Some(ticket) = tickets.get(&entry.id) else {
            error!("ticket missing for entry {} , skipping", entry.id);
            continue;
        };

        let payout_hash = entry.payout_hash.as_str();
        debug!("player_pubkey: {}", entry.ephemeral_pubkey);

        let pubkey = Point::from_hex(&entry.ephemeral_pubkey)
            .map_err(|e| anyhow!("failed to get point: {}", e))?;
        let player = Player {
            pubkey,
            // coordinator generates this preimage and only shares after player has paid for a ticket
            ticket_hash: string_to_byte_array(&ticket.hash),
            // players generate this preimage and only share after winning with the coordinator
            payout_hash: string_to_byte_array(payout_hash),
        };
        players.push(player);
    }

    Ok(players)
}
fn string_to_byte_array(hex_str: &str) -> [u8; 32] {
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(hex_str).expect("valid hex string");
    bytes.try_into().expect("32 bytes")
}

fn get_percentage_weights(num_winners: usize) -> Vec<u64> {
    match num_winners {
        1 => vec![100],
        2 => vec![60, 40],
        3 => vec![45, 35, 20],
        4 => vec![42, 30, 18, 10],
        5 => vec![40, 27, 16, 9, 8],
        _ => vec![100], // fallback to winner takes all
    }
}

fn generate_payouts(
    competition: &Competition,
    entries: &mut Vec<UserEntry>,
    players: &Vec<Player>,
) -> Result<BTreeMap<Outcome, PayoutWeights>, anyhow::Error> {
    debug!("Generating payouts for {} players", players.len());

    // Sort entries by ID for consistent indexing
    entries.sort_by_key(|entry| entry.id);
    let mut payouts: BTreeMap<Outcome, PayoutWeights> = BTreeMap::new();

    let possible_rankings = generate_ranking_permutations(
        entries.len(),
        competition.event_submission.number_of_places_win,
    );
    debug!("Generated {} possible rankings", possible_rankings.len());
    for (outcome_index, winner_indices) in possible_rankings.iter().enumerate() {
        debug!(
            "Processing outcome {} with winner indices: {:?}",
            outcome_index, winner_indices
        );

        // Special handling for "all players" outcome
        if winner_indices.len() == entries.len() {
            debug!("Processing special 'all players' outcome for equal refunds");

            // Create equal weights for all players (everyone gets their entry fee back)
            let mut equal_weights: BTreeMap<PlayerIndex, u64> = BTreeMap::new();
            let weight_per_player = 100 / players.len() as u64;
            let remainder = 100 % players.len() as u64;

            for i in 0..players.len() {
                // Distribute remainder to maintain total of 100
                let player_weight = if (i as u64) < remainder {
                    weight_per_player + 1
                } else {
                    weight_per_player
                };
                equal_weights.insert(i, player_weight);
                debug!(
                    "Assigning equal weight {} to player index {} for refund outcome",
                    player_weight, i
                );
            }

            debug!(
                "Final weights for refund outcome {}: {:?}",
                outcome_index, equal_weights
            );

            payouts.insert(Outcome::Attestation(outcome_index), equal_weights);
            continue;
        }

        // Normal outcome processing
        let entry_pubkeys = find_winning_entries_pubkeys(entries, winner_indices.to_owned());
        debug!("Winner pubkeys: {:?}", entry_pubkeys);

        let player_indices = find_player_indices(players, entry_pubkeys)?;
        debug!("Mapped to player indices: {:?}", player_indices);

        if player_indices.len() != competition.event_submission.number_of_places_win {
            return Err(anyhow!(
                "Incorrect number of winners for outcome {}",
                outcome_index
            ));
        }

        let percentage_weights =
            get_percentage_weights(competition.event_submission.number_of_places_win);
        let mut payout_weights: BTreeMap<PlayerIndex, u64> = BTreeMap::new();

        for (rank, &player_index) in player_indices.iter().enumerate() {
            let weight = percentage_weights[rank];
            debug!(
                "Assigning weight {} to player index {}",
                weight, player_index
            );
            payout_weights.insert(player_index, weight);
        }

        debug!(
            "Final weights for outcome {}: {:?}",
            outcome_index, payout_weights
        );

        // Verify total weight is 100
        let total_weight: u64 = payout_weights.values().sum();
        if total_weight != 100 {
            return Err(anyhow!(
                "Total weight for outcome {} should be 100, got {}",
                outcome_index,
                total_weight
            ));
        }

        payouts.insert(Outcome::Attestation(outcome_index), payout_weights);
    }

    // Add expiry outcome with equal distribution
    let mut expiry_weights = BTreeMap::new();
    let player_count = players.len() as u64;
    let base_weight = 100 / player_count;
    let remainder = 100 % player_count;

    for i in 0..players.len() {
        // Distribute remainder one point at a time to early indices
        let weight = if (i as u64) < remainder {
            base_weight + 1
        } else {
            base_weight
        };
        expiry_weights.insert(i, weight);
    }
    payouts.insert(Outcome::Expiry, expiry_weights);

    debug!("Generated {} total outcomes", payouts.len());
    Ok(payouts)
}

pub fn generate_ranking_permutations(num_players: usize, rankings: usize) -> Vec<Vec<usize>> {
    let mut permutations = (0..num_players)
        .permutations(rankings)
        .collect::<Vec<Vec<usize>>>();

    // Always add the special "refund all" outcome
    permutations.push((0..num_players).collect());

    permutations
}

fn find_player_indices(
    players: &Vec<Player>,
    entry_ticket_pubkeys: Vec<String>,
) -> Result<Vec<usize>, anyhow::Error> {
    entry_ticket_pubkeys
        .into_iter()
        .map(|pubkey| {
            let pubkey_point = Point::from_hex(&pubkey)?;
            players
                .iter()
                .position(|player| player.pubkey == pubkey_point)
                .ok_or_else(|| anyhow!("No player found for pubkey: {}", pubkey))
        })
        .collect()
}

fn find_winning_entries_pubkeys(
    entries: &Vec<UserEntry>,
    winning_entry_indices: Vec<usize>,
) -> Vec<String> {
    winning_entry_indices
        .into_iter()
        .map(|idx| entries[idx].ephemeral_pubkey.clone())
        .collect()
}

async fn signed_funding_tx(
    bitcoin_client: Arc<dyn Bitcoin>,
    mut funding_tx: Psbt,
) -> Result<Transaction, anyhow::Error> {
    debug!("Funding Psbt before coordinator signing: {:?}", funding_tx);

    // Sign the PSBT (including escrow inputs)
    let fully_signed = bitcoin_client
        .sign_psbt_with_escrow_support(
            &mut funding_tx,
            SignOptions {
                trust_witness_utxo: true,
                sign_with_tap_internal_key: true,
                allow_all_sighashes: true,
                allow_grinding: true,
                ..Default::default()
            },
        )
        .await?;

    debug!(
        "Coordinator signing complete. Fully signed: {}",
        fully_signed
    );

    // Log signature state after signing
    for (i, input) in funding_tx.inputs.iter().enumerate() {
        if input.witness_script.is_some() {
            debug!("Input {} signature count: {}", i, input.partial_sigs.len());
            for (pk, _) in &input.partial_sigs {
                debug!("  - Signed by: {}", pk);
            }
        }
    }

    // Verify we have all required signatures
    let mut validation_errors = Vec::new();
    for (i, input) in funding_tx.inputs.iter().enumerate() {
        // Skip already finalized inputs
        if input.final_script_witness.is_some() {
            debug!("Input {} is already finalized", i);
            continue;
        }

        // Check escrow inputs
        if input.witness_script.is_some() {
            // For 2-of-2 multisig escrow, we need exactly 2 signatures
            if input.partial_sigs.len() < 2 {
                validation_errors.push(format!(
                    "Input {}: Escrow input has {} signatures, needs 2",
                    i,
                    input.partial_sigs.len()
                ));
            }
        } else if input.partial_sigs.is_empty() {
            validation_errors.push(format!("Input {}: No signatures present", i));
        }
    }

    if !validation_errors.is_empty() {
        return Err(anyhow!(
            "PSBT not ready for finalization. Issues: {}",
            validation_errors.join(", ")
        ));
    }

    // Now finalize the PSBT
    let finalized = bitcoin_client
        .finalize_psbt_with_escrow_support(&mut funding_tx)
        .await?;

    if !finalized {
        return Err(anyhow!("Failed to finalize PSBT after signing"));
    }

    // Extract the final transaction
    match funding_tx.clone().extract_tx() {
        Ok(final_tx) => {
            debug!(
                "Successfully extracted and finalized funding transaction: {}",
                final_tx.compute_txid()
            );
            Ok(final_tx)
        }
        Err(e) => {
            // Detailed debugging if extraction fails
            debug!("Failed to extract transaction: {:?}", e);

            for (i, input) in funding_tx.inputs.iter().enumerate() {
                if input.final_script_witness.is_none() && input.witness_script.is_some() {
                    debug!("Input {} not finalized:", i);
                    debug!("  - Signatures: {}", input.partial_sigs.len());
                    if let Some(ws) = &input.witness_script {
                        debug!("  - Witness script: {}", ws.to_hex_string());
                    }
                }
            }

            Err(anyhow!("Failed to extract transaction: {}", e))
        }
    }
}

fn p2tr_script_pubkey(pubkey: Point) -> ScriptBuf {
    let (xonly, _) = pubkey.into();
    let tweaked = TweakedPublicKey::dangerous_assume_tweaked(convert_xonly_key(xonly));
    ScriptBuf::new_p2tr_tweaked(tweaked)
}

fn simple_sweep_tx(
    destination_pubkey: Point,
    input: TxIn,
    input_weight: InputWeightPrediction,
    prevout_value: Amount,
    fee_rate: FeeRate,
) -> Transaction {
    let script_pubkey = p2tr_script_pubkey(destination_pubkey);
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![input],
        output: vec![TxOut {
            value: {
                let tx_weight = predict_weight([input_weight], [script_pubkey.len()]);
                let fee = tx_weight * fee_rate;
                prevout_value - fee
            },
            script_pubkey,
        }],
    }
}

fn create_deterministic_rng(funding_outpoint: &OutPoint, private_key: Scalar) -> ChaCha20Rng {
    let mut hasher = sha256::Hash::engine();

    hasher.write_all(&funding_outpoint.txid[..]).unwrap();
    hasher
        .write_all(&funding_outpoint.vout.to_le_bytes())
        .unwrap();
    hasher.write_all(&private_key.serialize()).unwrap();

    let hash = sha256::Hash::from_engine(hasher);
    let seed: [u8; 32] = hash.to_byte_array();
    ChaCha20Rng::from_seed(seed)
}

async fn validate_entry(entry: AddEventEntry, competition: Competition) -> Result<(), Error> {
    if entry.id.get_version_num() != 7 {
        return Err(Error::BadRequest(format!(
            "Client needs to provide a valid Uuidv7 for entry id {}",
            entry.id
        )));
    }

    let mut choice_count = 0;
    for weather_choice in &entry.expected_observations {
        if weather_choice.temp_high.is_some() {
            choice_count += 1;
        }
        if weather_choice.temp_low.is_some() {
            choice_count += 1;
        }
        if weather_choice.wind_speed.is_some() {
            choice_count += 1;
        }

        if choice_count > competition.event_submission.number_of_values_per_entry {
            return Err(Error::BadRequest(format!(
                "entry_id {0} not valid, too many value choices, max allowed {1} but got {2}",
                entry.id, competition.event_submission.number_of_values_per_entry, choice_count
            )));
        }
    }

    let locations_choose: Vec<String> = entry
        .expected_observations
        .clone()
        .iter()
        .map(|weather_vals| weather_vals.stations.clone())
        .collect();
    let all_valid_locations = locations_choose
        .iter()
        .all(|choose| competition.event_submission.locations.contains(choose));
    if !all_valid_locations {
        return Err(Error::BadRequest(format!(
            "entry_id {0} not valid, choose locations not in the even",
            entry.id
        )));
    }
    Ok(())
}
