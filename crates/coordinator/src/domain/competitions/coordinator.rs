use super::{
    states::CompetitionStatus, store::ReservedTicket, AddEntry, CompetitionError, CompetitionState,
    CompetitionStore, FundedContract, KeymeldSigningInfo, PayoutInfo, SearchBy, Ticket,
    TicketStatus, UserEntry, UserEntryView,
};
use crate::{
    api::routes::FinalSignatures,
    domain::{Competition, CreateEvent, EntryStatus, Error},
    infra::{
        bitcoin::{fee_rate_for_target, Bitcoin, ForeignUtxo, REQUIRED_CONFIRMATIONS_FOR_TIME},
        db::DatabaseWriteError,
        escrow::{create_escrow_descriptor, generate_escrow_tx, get_escrow_outpoint},
        keymeld::{
            DlcKeygenSession, DlcSubsetInfo, Keymeld, ParticipantRegistrationData,
            StoredDlcKeygenSession, SubsetDefinition,
        },
        lightning::{extract_amount_from_invoice, Ln},
        oracle::{AddEventEntries, AddEventEntry, Error as OracleError, Event, Oracle},
    },
};
use anyhow::anyhow;
use bitcoin::{
    absolute::LockTime,
    consensus::encode::deserialize,
    hashes::{sha256, Hash, HashEngine},
    transaction::Version,
    Amount, FeeRate, OutPoint, Psbt, PublicKey as BitcoinPublicKey, ScriptBuf, Transaction, TxIn,
    TxOut, Txid,
};
use coordinator_core::RegistrationAssignment;
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
    ContractParameters, ContractSignatures, NonceSharingRound, Outcome, PayoutWeights, Player,
    PlayerIndex, SigMap, SigningSession, TicketedDLC, WinCondition,
};
use futures::TryFutureExt;
use itertools::Itertools;
use keymeld_sdk::prelude::UserId;
use keymeld_sdk::types::{RegistrationContext, SignedRoster};
use log::{debug, error, info, warn};
use nostr_sdk::{nips::nip44, Keys, PublicKey as NostrPublicKey, SecretKey};
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
};
use time::OffsetDateTime;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn confirmation_depth(tip: u32, inclusion_height: u32) -> u32 {
    tip.checked_sub(inclusion_height)
        .and_then(|blocks| blocks.checked_add(1))
        .unwrap_or(0)
}

impl Competition {
    pub(crate) fn funding_reservation_deadline(
        &self,
        now: OffsetDateTime,
    ) -> Result<u64, anyhow::Error> {
        let deadline = self
            .event_submission
            .signing_date
            .unix_timestamp()
            .max(self.event_submission.end_observation_date.unix_timestamp())
            .max(
                self.event_announcement
                    .as_ref()
                    .and_then(|event| event.expiry)
                    .map(i64::from)
                    .unwrap_or(0),
            )
            .max(now.unix_timestamp())
            .checked_add(24 * 60 * 60)
            .ok_or_else(|| anyhow!("Funding reservation deadline overflow"))?;
        u64::try_from(deadline)
            .map_err(|error| anyhow!("Invalid funding reservation deadline: {error}"))
    }
}

/// Response for ticket request containing both payment info and safety mechanisms
#[derive(Debug, Serialize)]
pub struct TicketResponse {
    pub ticket_id: uuid::Uuid,
    pub payment_request: String, // Lightning HODL invoice to pay for entry
    pub escrow_tx: Option<String>, // escrow transaction the coordinator broadcasts prior to settling the HODL invoice
    pub payment_hash: String,      // Hex-encoded payment hash for verification
    pub amount_sats: u64,
    /// The user's keymeld user_id (same as ticket_id) - used for keymeld registration
    pub keymeld_user_id: uuid::Uuid,
    /// Keymeld gateway URL for client registration
    pub keymeld_gateway_url: Option<String>,
    /// Keymeld session ID - users need this to derive their auth_pubkey for server-side registration
    pub keymeld_session_id: Option<String>,
    /// Keymeld enclave public key (hex-encoded) for encrypting the user's ephemeral private key
    /// Users encrypt their ephemeral private key to this key for server-side keymeld registration
    pub keymeld_enclave_public_key: Option<String>,
    pub keymeld_registration: Option<RegistrationAssignment>,
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

            // Run each sync on its own task: a panic while handling one
            // competition must not take the watcher down for all of them.
            let coordinator = self.coordinator.clone();
            match tokio::spawn(async move { coordinator.competition_handler().await }).await {
                Ok(Ok(_)) => info!("Competition sync completed successfully"),
                Ok(Err(e)) => error!("Competition sync error: {}", e),
                Err(e) => error!("Competition sync task failed: {}", e),
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
    keymeld: Arc<dyn Keymeld>,
    keymeld_gateway_url: Option<String>,
    private_key: Scalar,
    public_key: Point,
    relative_locktime_block_delta: u32,
    required_confirmations: u32,
    name: String,
    escrow_enabled: bool,
    invoice_settlement_confirmations: u32,
}

impl Coordinator {
    #[expect(
        clippy::too_many_arguments,
        reason = "wires every runtime dependency once at startup"
    )]
    pub async fn new(
        oracle_client: Arc<dyn Oracle>,
        competition_store: CompetitionStore,
        bitcoin: Arc<dyn Bitcoin>,
        ln: Arc<dyn Ln>,
        keymeld: Arc<dyn Keymeld>,
        keymeld_gateway_url: Option<String>,
        relative_locktime_block_delta: u32,
        required_confirmations: u32,
        name: String,
        escrow_enabled: bool,
        invoice_settlement_confirmations: u32,
    ) -> Result<Self, anyhow::Error> {
        let private_key = bitcoin.get_derived_private_key().await?;
        let public_key = private_key.base_point_mul();

        let coordinator = Self {
            oracle_client,
            competition_store: Arc::new(competition_store),
            bitcoin,
            ln,
            keymeld,
            keymeld_gateway_url,
            private_key,
            public_key,
            relative_locktime_block_delta,
            required_confirmations,
            name,
            escrow_enabled,
            invoice_settlement_confirmations,
        };
        coordinator.validate_coordinator_metadata().await?;
        Ok(coordinator)
    }

    /// Check if escrow transactions are enabled
    pub fn is_escrow_enabled(&self) -> bool {
        self.escrow_enabled
    }

    /// Check if Keymeld signing is enabled
    pub fn is_keymeld_enabled(&self) -> bool {
        self.keymeld.is_enabled()
    }

    /// Settle all hold invoices for a competition.
    /// This releases the held funds to the coordinator.
    pub async fn settle_competition_invoices(
        &self,
        competition_id: Uuid,
    ) -> Result<(), anyhow::Error> {
        if self.is_keymeld_enabled() {
            let competition = self
                .competition_store
                .get_competition(competition_id)
                .await?;
            let stored = self
                .competition_store
                .get_keymeld_session(competition_id)
                .await?
                .ok_or_else(|| anyhow!("Competition has no authorized Keymeld session"))?;
            let session = self.restore_keymeld_session(&stored)?;
            self.verify_keymeld_competition(&competition, &session)
                .await?;
        }
        let tickets = self.competition_store.get_tickets(competition_id).await?;
        for ticket in tickets.values() {
            if ticket.paid_at.is_none() || ticket.settled_at.is_some() {
                continue;
            }
            self.ln
                .settle_hold_invoice(ticket.encrypted_preimage.clone())
                .await?;
            self.competition_store
                .mark_ticket_settled(ticket.id)
                .await?;
        }
        Ok(())
    }

    /// Check if all participants have registered with keymeld for a competition
    /// Returns true if all expected participants have registered, false otherwise
    pub async fn check_keymeld_registrations(&self, competition_id: Uuid) -> Result<bool, Error> {
        let stored_session = self
            .competition_store
            .get_keymeld_session(competition_id)
            .await
            .map_err(Error::from)?
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "Keymeld session not found for competition {}",
                    competition_id
                ))
            })?;

        let session = self.restore_keymeld_session(&stored_session)?;

        let status = self
            .keymeld
            .get_keygen_status(&session)
            .await
            .map_err(|e| Error::BadRequest(format!("Failed to get keymeld status: {}", e)))?;

        info!(
            "Keymeld session {} status: {}",
            status.session_id, status.status
        );

        Ok(status.is_completed)
    }

    /// Complete keymeld keygen after all participants have registered
    /// Waits for keygen to complete and updates the stored session with aggregate key
    pub async fn complete_keymeld_keygen(
        &self,
        competition: &mut Competition,
    ) -> Result<(), Error> {
        let stored_session = self
            .competition_store
            .get_keymeld_session(competition.id)
            .await
            .map_err(Error::from)?
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "Keymeld session not found for competition {}",
                    competition.id
                ))
            })?;

        let session = self.restore_keymeld_session(&stored_session)?;

        let roster = self
            .verify_keymeld_competition(competition, &session)
            .await?;
        let aggregate_key = roster.roster.aggregate_public_key;

        info!(
            "Keymeld keygen session {} completed for competition {}",
            session.session_id, competition.id
        );

        // Update stored session with aggregate key
        let updated_session = DlcKeygenSession {
            aggregate_key,
            ..session
        };

        self.store_keymeld_session(competition.id, updated_session)
            .await?;

        // Mark keygen as completed on the competition
        competition.keymeld_keygen_completed_at = Some(time::OffsetDateTime::now_utc());

        Ok(())
    }

    /// Encrypt shared and independent authority credentials before persisting the pinned session.
    pub async fn store_keymeld_session(
        &self,
        competition_id: Uuid,
        session: DlcKeygenSession,
    ) -> Result<(), Error> {
        let keys = self.keymeld_storage_keys()?;
        let stored_session = StoredDlcKeygenSession::from_session(&session, &keys)
            .map_err(|error| Error::Bitcoin(anyhow!(error)))?;
        self.competition_store
            .store_keymeld_session(competition_id, &stored_session)
            .await
            .map_err(Error::from)?;
        Ok(())
    }

    fn keymeld_storage_keys(&self) -> Result<Keys, Error> {
        SecretKey::from_slice(&self.private_key.serialize())
            .map(Keys::new)
            .map_err(|error| Error::Bitcoin(anyhow!(error)))
    }

    fn restore_keymeld_session(
        &self,
        stored: &StoredDlcKeygenSession,
    ) -> Result<DlcKeygenSession, Error> {
        stored
            .to_session(&self.keymeld_storage_keys()?)
            .map_err(|error| Error::Bitcoin(anyhow!(error)))
    }

    /// Verify the complete accepted roster before signatures, funding, or invoice settlement.
    async fn verify_keymeld_competition(
        &self,
        competition: &Competition,
        session: &DlcKeygenSession,
    ) -> Result<SignedRoster, Error> {
        let mut entries = self
            .competition_store
            .get_competition_entries(competition.id, vec![EntryStatus::Paid])
            .await
            .map_err(Error::from)?;
        entries.sort_by_key(|entry| entry.ticket_id);
        let roster = self
            .keymeld
            .wait_for_keygen_completion(session)
            .await
            .map_err(|error| Error::Bitcoin(anyhow!(error)))?;
        let coordinator_id = session
            .authorization_manifest
            .manifest
            .coordinator_user_id
            .clone();
        let mut expected_keys = BTreeMap::new();
        let mut expected_contexts = BTreeMap::new();
        expected_keys.insert(coordinator_id.clone(), self.public_key.serialize().to_vec());
        for entry in &entries {
            let user = UserId::from(entry.ticket_id);
            expected_keys.insert(
                user.clone(),
                hex::decode(&entry.ephemeral_pubkey)
                    .map_err(|error| Error::Bitcoin(anyhow!(error)))?,
            );
            expected_contexts.insert(
                user,
                entry.keymeld_registration_context.clone().ok_or_else(|| {
                    Error::BadRequest(
                        "Accepted entry is missing authorized registration context".into(),
                    )
                })?,
            );
        }
        verify_accepted_keymeld_roster(
            session,
            &roster,
            &expected_keys,
            &expected_contexts,
            coordinator_id,
            competition.event_submission.number_of_places_win,
        )
        .map_err(Error::Bitcoin)?;
        Ok(roster)
    }

    pub fn public_key(&self) -> String {
        let (xonly, _) = self.public_key.into();
        hex::encode(xonly.serialize())
    }

    pub async fn ping(&self) -> Result<(), Error> {
        self.competition_store.ping().await.map_err(Error::from)
    }

    pub async fn quick_check(&self) -> Result<(), Error> {
        self.competition_store
            .quick_check()
            .await
            .map_err(Error::from)
    }

    pub async fn competition_handler(&self) -> Result<(), anyhow::Error> {
        let competitions: Vec<Competition> = self.competition_store.get_competitions(true).await?;

        for mut competition in competitions {
            let mut processed_states = 0;
            const MAX_CONSECUTIVE_STATES: usize = 10;

            if competition.skip_competition() {
                // Auto-expire failed competitions after 1 hour so they stop
                // polluting every tick's log output and DB query results.
                const FAILED_EXPIRY_HOURS: i64 = 1;
                if competition.is_failed() && competition.cancelled_at.is_none() {
                    if let Some(failed_at) = competition.failed_at {
                        let age = OffsetDateTime::now_utc() - failed_at;
                        if age.whole_hours() >= FAILED_EXPIRY_HOURS {
                            competition.cancelled_at = Some(OffsetDateTime::now_utc());
                            if let Err(e) = self
                                .competition_store
                                .update_competitions(vec![competition.clone()])
                                .await
                            {
                                return Err(anyhow!(
                                    "Failed to cancel expired-failed competition {}: {}",
                                    competition.id,
                                    e
                                ));
                            } else {
                                info!(
                                    "Auto-cancelled failed competition {} (failed {}h ago)",
                                    competition.id,
                                    age.whole_hours()
                                );
                            }
                            continue;
                        }
                    }
                }
                debug!(
                    "Skipping competition {} in state {}",
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
                    return Err(anyhow!(
                        "Failed to save competition {} after cancellation: {}",
                        competition.id,
                        e
                    ));
                }
                info!("Cancelled expired competition {}", competition.id);
                continue;
            }

            if let Err(error) = self.renew_funding_reservation(&competition).await {
                error!(
                    "Cannot reserve funding inputs for competition {}: {}",
                    competition.id, error
                );
                continue;
            }

            loop {
                let status: CompetitionStatus = competition.clone().into();
                let current_state_name = status.state_name();

                let new_status = self.process_status(status).await;
                let new_state_name = new_status.state_name();
                let is_immediate = new_status.is_immediate_transition();

                let updated_competition = new_status.into_competition();

                info!(
                    "Competition {} transitioned {} -> {}",
                    competition.id, current_state_name, new_state_name
                );

                if new_state_name != current_state_name {
                    processed_states += 1;

                    if is_immediate && processed_states < MAX_CONSECUTIVE_STATES {
                        if let Err(e) = self
                            .competition_store
                            .update_competitions(vec![updated_competition.clone()])
                            .await
                        {
                            return Err(anyhow!(
                                "Failed to save competition {} in state {}: {}",
                                competition.id,
                                new_state_name,
                                e
                            ));
                        }
                        competition = updated_competition;
                        continue;
                    }
                }

                if let Err(e) = self
                    .competition_store
                    .update_competitions(vec![updated_competition])
                    .await
                {
                    return Err(anyhow!(
                        "Failed to save competition {} in state {}: {}",
                        competition.id,
                        new_state_name,
                        e
                    ));
                }
                break;
            }
        }

        Ok(())
    }

    pub async fn process_status(&self, status: CompetitionStatus) -> CompetitionStatus {
        use super::states::*;

        let competition_id = status.competition_id();
        let state_name = status.state_name();
        info!(
            "Processing competition {} in state {}",
            competition_id, state_name
        );

        match status {
            CompetitionStatus::Created(state) => {
                debug!(
                    "Competition {}, waiting for entries: {}/{}",
                    state.competition_id,
                    state.competition.total_entries,
                    state.competition.event_submission.total_allowed_entries
                );
                // Check if we have entries and should transition
                if state.competition.total_entries > 0 {
                    // Transition to CollectingEntries
                    state.first_entry_added()
                } else {
                    CompetitionStatus::Created(state)
                }
            }

            CompetitionStatus::CollectingEntries(state) => {
                if state.has_all_entries() {
                    if self.escrow_enabled {
                        state.into_awaiting_escrow()
                    } else {
                        // Skip escrow - go directly to EscrowConfirmed
                        state.into_escrow_confirmed()
                    }
                } else {
                    CompetitionStatus::CollectingEntries(state)
                }
            }

            CompetitionStatus::AwaitingEscrow(mut state) => {
                match self
                    .check_escrow_confirmations(state.competition_mut())
                    .await
                {
                    Ok(_) => {
                        if state.competition().escrow_funds_confirmed_at.is_some() {
                            state.escrow_confirmed()
                        } else {
                            CompetitionStatus::AwaitingEscrow(state)
                        }
                    }
                    Err(e) => {
                        error!(
                            "Competition {} failed to check escrow: {}",
                            competition_id, e
                        );
                        state
                            .competition_mut()
                            .errors
                            .push(CompetitionError::FailedEscrowConfirmation(e.to_string()));
                        if state.competition().should_abort() {
                            CompetitionStatus::AwaitingEscrow(state)
                                .fail(CompetitionError::FailedEscrowConfirmation(e.to_string()))
                        } else {
                            CompetitionStatus::AwaitingEscrow(state)
                        }
                    }
                }
            }

            CompetitionStatus::EscrowConfirmed(mut state) => {
                match self.submit_event_to_oracle(state.competition_mut()).await {
                    Ok(_) => {
                        if state.competition().event_created_at.is_some() {
                            CompetitionStatus::EventCreated(
                                super::states::EventCreated::from_competition(
                                    state.into_competition(),
                                ),
                            )
                        } else {
                            CompetitionStatus::EscrowConfirmed(state)
                        }
                    }
                    Err(e) => {
                        error!(
                            "Competition {} failed to create oracle event: {}",
                            competition_id, e
                        );
                        CompetitionStatus::EscrowConfirmed(state)
                            .fail(CompetitionError::FailedCreateEvent(e.to_string()))
                    }
                }
            }

            CompetitionStatus::EventCreated(mut state) => {
                match self.submit_entries_to_oracle(state.competition_mut()).await {
                    Ok(_) => state.entries_submitted(),
                    Err(e) => {
                        error!(
                            "Competition {} failed to submit entries: {}",
                            competition_id, e
                        );
                        CompetitionStatus::EventCreated(state)
                            .fail(CompetitionError::FailedSubmitEntries(e.to_string()))
                    }
                }
            }

            CompetitionStatus::EntriesSubmitted(mut state) => {
                match self.create_funding_psbt(state.competition_mut()).await {
                    Ok(_) => {
                        let comp = state.competition();
                        if let (Some(params), Some(outpoint), Some(psbt)) = (
                            comp.contract_parameters.clone(),
                            comp.funding_outpoint,
                            comp.funding_psbt_base64.clone(),
                        ) {
                            state.contract_created(params, outpoint, psbt)
                        } else {
                            CompetitionStatus::EntriesSubmitted(state)
                        }
                    }
                    Err(e) => {
                        error!(
                            "Competition {} failed to create contract: {}",
                            competition_id, e
                        );
                        CompetitionStatus::EntriesSubmitted(state)
                            .fail(CompetitionError::FailedCreateTransaction(e.to_string()))
                    }
                }
            }

            CompetitionStatus::ContractCreated(mut state) => {
                if self.is_keymeld_enabled() {
                    // Keymeld flow: Poll for registrations, then wait for completion
                    match self
                        .check_keymeld_registrations(state.competition().id)
                        .await
                    {
                        Ok(true) => {
                            // All participants registered, wait for keygen to complete
                            info!(
                                "All keymeld registrations complete for competition {}, waiting for keygen completion",
                                state.competition().id
                            );
                            match self.complete_keymeld_keygen(state.competition_mut()).await {
                                Ok(_) => {
                                    // The outer lifecycle loop commits this state before
                                    // another tick can start unattended signing.
                                    CompetitionStatus::AwaitingSignatures(
                                        AwaitingSignatures::from_competition(
                                            state.into_competition(),
                                        ),
                                    )
                                }
                                Err(e) => {
                                    error!(
                                        "Competition {} keymeld keygen completion failed: {}",
                                        competition_id, e
                                    );
                                    CompetitionStatus::ContractCreated(state)
                                        .fail(CompetitionError::FailedBroadcast(e.to_string()))
                                }
                            }
                        }
                        Ok(false) => {
                            // Still waiting for registrations
                            debug!(
                                "Competition {} waiting for keymeld registrations",
                                state.competition().id
                            );
                            CompetitionStatus::ContractCreated(state)
                        }
                        Err(e) => {
                            error!(
                                "Competition {} failed to check keymeld registrations: {}",
                                competition_id, e
                            );
                            CompetitionStatus::ContractCreated(state)
                        }
                    }
                } else if state.has_nonces() {
                    // Legacy flow: proceed when we have nonces
                    CompetitionStatus::AwaitingSignatures(AwaitingSignatures::from_competition(
                        state.into_competition(),
                    ))
                } else {
                    CompetitionStatus::ContractCreated(state)
                }
            }

            CompetitionStatus::AwaitingSignatures(mut state) => {
                if self.is_keymeld_enabled() {
                    match self.sign_dlc_contract(state.competition_mut()).await {
                        Ok(_) => {
                            if let Some(signed) = state.competition().signed_contract.clone() {
                                state.signing_complete(signed)
                            } else {
                                CompetitionStatus::AwaitingSignatures(state)
                            }
                        }
                        Err(e) => {
                            error!(
                                "Competition {} keymeld signing failed: {}",
                                competition_id, e
                            );
                            CompetitionStatus::AwaitingSignatures(state)
                                .fail(CompetitionError::FailedBroadcast(e.to_string()))
                        }
                    }
                } else {
                    // Legacy MuSig2 flow
                    if !state.has_all_nonces() {
                        return CompetitionStatus::AwaitingSignatures(state);
                    }

                    match self
                        .generate_aggregate_nonces_and_coord_partial_signatures(
                            state.competition_mut(),
                        )
                        .await
                    {
                        Ok(_) => {}
                        Err(e) => {
                            error!(
                                "Competition {} nonce aggregation failed: {}",
                                competition_id, e
                            );
                            return CompetitionStatus::AwaitingSignatures(state)
                                .fail(CompetitionError::FailedNonceAggregation(e.to_string()));
                        }
                    }

                    if !state.has_all_signatures() {
                        return CompetitionStatus::AwaitingSignatures(state);
                    }

                    match self.sign_dlc_contract(state.competition_mut()).await {
                        Ok(_) => {
                            if let Some(signed) = state.competition().signed_contract.clone() {
                                state.signing_complete(signed)
                            } else {
                                CompetitionStatus::AwaitingSignatures(state)
                            }
                        }
                        Err(e) => {
                            error!("Competition {} signing failed: {}", competition_id, e);
                            CompetitionStatus::AwaitingSignatures(state)
                                .fail(CompetitionError::FailedBroadcast(e.to_string()))
                        }
                    }
                }
            }

            CompetitionStatus::SigningComplete(mut state) => {
                match self
                    .sign_and_broadcast_funding_tx(state.competition_mut())
                    .await
                {
                    Ok(_) => {
                        if let Some(tx) = state.competition().funding_transaction.clone() {
                            state.funding_broadcasted(tx)
                        } else {
                            CompetitionStatus::SigningComplete(state)
                        }
                    }
                    Err(e) => {
                        error!(
                            "Competition {} funding broadcast failed: {}",
                            competition_id, e
                        );
                        CompetitionStatus::SigningComplete(state)
                            .fail(CompetitionError::FailedBroadcast(e.to_string()))
                    }
                }
            }

            CompetitionStatus::FundingBroadcasted(mut state) => {
                // Settle hold invoices based on configured confirmation requirement
                // If invoice_settlement_confirmations is 0, settle immediately at broadcast
                // Otherwise, wait for the required confirmations before settling
                let should_settle = if self.invoice_settlement_confirmations == 0 {
                    // Settle immediately at broadcast time
                    state.competition().invoices_settled_at.is_none()
                } else {
                    // Check if we have enough confirmations to settle
                    if let Some(funding_tx) = &state.competition().funding_transaction {
                        let txid = funding_tx.compute_txid();
                        match self.transaction_confirmations(&txid).await {
                            Ok(Some(confirmations)) => {
                                confirmations >= self.invoice_settlement_confirmations
                                    && state.competition().invoices_settled_at.is_none()
                            }
                            _ => false,
                        }
                    } else {
                        false
                    }
                };

                if should_settle {
                    info!(
                        "Settling hold invoices for competition {} (required confirmations: {})",
                        competition_id, self.invoice_settlement_confirmations
                    );
                    if let Err(e) = self.settle_competition_invoices(competition_id).await {
                        error!(
                            "Competition {} failed to settle invoices: {}",
                            competition_id, e
                        );
                    } else {
                        state.competition_mut().invoices_settled_at =
                            Some(OffsetDateTime::now_utc());
                    }
                }

                // Continue checking for funding confirmation
                match self
                    .check_funding_confirmation(state.competition_mut())
                    .await
                {
                    Ok(_) => {
                        if state.competition().funding_confirmed_at.is_some() {
                            state.funding_confirmed()
                        } else {
                            CompetitionStatus::FundingBroadcasted(state)
                        }
                    }
                    Err(e) => {
                        error!(
                            "Competition {} funding confirmation failed: {}",
                            competition_id, e
                        );
                        state
                            .competition_mut()
                            .errors
                            .push(CompetitionError::FailedFundingConfirmation(e.to_string()));
                        if state.competition().should_abort() {
                            CompetitionStatus::FundingBroadcasted(state)
                                .fail(CompetitionError::FailedFundingConfirmation(e.to_string()))
                        } else {
                            CompetitionStatus::FundingBroadcasted(state)
                        }
                    }
                }
            }

            CompetitionStatus::FundingConfirmed(mut state) => {
                state.competition_mut().funding_settled_at = Some(OffsetDateTime::now_utc());
                info!(
                    "Competition {} funding confirmed, invoices settled",
                    competition_id
                );
                state.funding_settled()
            }

            CompetitionStatus::FundingSettled(state) => state.await_attestation(),

            CompetitionStatus::AwaitingAttestation(mut state) => {
                match self.check_oracle_attestation(state.competition_mut()).await {
                    Ok(_) => {
                        if let Some(attestation) = state.competition().attestation {
                            state.attested(attestation)
                        } else {
                            CompetitionStatus::AwaitingAttestation(state)
                        }
                    }
                    Err(e) => {
                        if e.downcast_ref::<OracleError>()
                            .is_some_and(|oe| oe.is_transient())
                        {
                            warn!(
                                "Competition {} attestation check hit transient error, will retry: {}",
                                competition_id, e
                            );
                        } else {
                            error!(
                                "Competition {} attestation check failed: {}",
                                competition_id, e
                            );
                            state
                                .competition_mut()
                                .errors
                                .push(CompetitionError::FailedCheckingAttestation(e.to_string()));
                            if state.competition().should_abort() {
                                return CompetitionStatus::AwaitingAttestation(state).fail(
                                    CompetitionError::FailedCheckingAttestation(e.to_string()),
                                );
                            }
                        }
                        CompetitionStatus::AwaitingAttestation(state)
                    }
                }
            }

            CompetitionStatus::Attested(mut state) => {
                match self
                    .publish_outcome_transaction(state.competition_mut())
                    .await
                {
                    Ok(_) => {
                        if let Some(tx) = state.competition().outcome_transaction.clone() {
                            state.outcome_broadcasted(tx)
                        } else {
                            CompetitionStatus::Attested(state)
                        }
                    }
                    Err(e) => {
                        error!(
                            "Competition {} outcome broadcast failed: {}",
                            competition_id, e
                        );
                        CompetitionStatus::Attested(state)
                            .fail(CompetitionError::FailedBroadcast(e.to_string()))
                    }
                }
            }

            CompetitionStatus::ExpiryBroadcasted(state) => state.completed(),

            CompetitionStatus::OutcomeBroadcasted(mut state) => {
                match self
                    .publish_delta_transactions(state.competition_mut())
                    .await
                {
                    Ok(_) => {
                        if state.competition().delta_broadcasted_at.is_some() {
                            state.delta_broadcasted()
                        } else {
                            CompetitionStatus::OutcomeBroadcasted(state)
                        }
                    }
                    Err(e) => {
                        error!(
                            "Competition {} delta broadcast failed: {}",
                            competition_id, e
                        );
                        CompetitionStatus::OutcomeBroadcasted(state)
                            .fail(CompetitionError::FailedBroadcast(e.to_string()))
                    }
                }
            }

            CompetitionStatus::DeltaBroadcasted(mut state) => {
                match self
                    .publish_delta2_transactions(state.competition_mut())
                    .await
                {
                    Ok(_) => {
                        if state.competition().completed_at.is_some() {
                            state.completed()
                        } else {
                            CompetitionStatus::DeltaBroadcasted(state)
                        }
                    }
                    Err(e) => {
                        error!(
                            "Competition {} delta2 broadcast failed: {}",
                            competition_id, e
                        );
                        CompetitionStatus::DeltaBroadcasted(state)
                            .fail(CompetitionError::FailedBroadcast(e.to_string()))
                    }
                }
            }

            CompetitionStatus::Completed(_)
            | CompetitionStatus::Failed(_)
            | CompetitionStatus::Cancelled(_) => status,
        }
    }

    async fn transaction_confirmations(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error> {
        let Some(height) = self.bitcoin.get_tx_confirmation_height(txid).await? else {
            return Ok(None);
        };
        let tip = self.bitcoin.get_current_height().await?;
        Ok(Some(confirmation_depth(tip, height)))
    }

    async fn renew_funding_reservation(
        &self,
        competition: &Competition,
    ) -> Result<(), anyhow::Error> {
        if competition.funding_broadcasted_at.is_none() {
            if let Some(encoded) = &competition.funding_psbt_base64 {
                let psbt = Psbt::from_str(encoded)?;
                self.bitcoin
                    .reserve_psbt_inputs_until(
                        &psbt,
                        competition.funding_reservation_deadline(OffsetDateTime::now_utc())?,
                    )
                    .await?;
            }
        }
        Ok(())
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
                match self.transaction_confirmations(&txid).await? {
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
        // Sort entries by ticket_id to ensure player indices match the ticket order
        // used when creating keymeld subset definitions at competition creation time
        entries.sort_by_key(|entry| entry.ticket_id);
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

        let outcome_payouts = generate_payouts(competition, &mut entries, &players)?;
        debug!("Generated outcome payouts:");
        for (outcome, weights) in &outcome_payouts {
            debug!("Outcome {:?}: weights={:?}", outcome, weights);
        }

        let contract_amount_sats = competition.event_submission.total_competition_pool;
        let fee_rates = self.bitcoin.get_estimated_fee_rates().await?;
        info!("Fee rates: {:?}", fee_rates);

        // TODO (@tee8z): make this configurable from the admin screen
        let fee_rate = fee_rate_for_target(&fee_rates, 1)?;

        let contract_params =
            competition
                .contract_parameters
                .clone()
                .unwrap_or(ContractParameters {
                    market_maker: dlctix::MarketMaker {
                        pubkey: self.public_key,
                    },
                    players,
                    event: event_announcement.clone(),
                    outcome_payouts,
                    fee_rate,
                    funding_value: Amount::from_sat(contract_amount_sats as u64),
                    relative_locktime_block_delta: competition
                        .event_submission
                        .relative_locktime_block_delta
                        .unwrap_or(self.relative_locktime_block_delta as u16),
                });
        competition.contract_parameters = Some(contract_params.clone());

        let funding_output = contract_params.funding_output()?;

        let funding_script = funding_output.script_pubkey.clone();
        debug!("Contract params have been built");
        let coordinator_pubkey = self.bitcoin.get_public_key().await?;
        let entries_lookup = entries
            .iter()
            .map(|user_entry| (user_entry.id, user_entry))
            .collect::<HashMap<_, _>>();

        // When escrow is disabled, the coordinator funds the contract directly from its wallet
        // When escrow is enabled, we use the escrow transactions from each ticket
        let escrow_inputs: Vec<ForeignUtxo> = if self.escrow_enabled {
            tickets
                .values()
                .map(|ticket| {
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
                        .ok_or_else(|| {
                            anyhow!("Escrow output not found at index {}", outpoint.vout)
                        })?
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
                    let payment_hash_from_ticket = parse_hash32(&ticket.hash)?;
                    let preimage = hex::decode(&ticket.encrypted_preimage)
                        .map_err(|e| anyhow!("Failed to decode preimage: {}", e))?;
                    let payment_hash_from_preimage = sha256::Hash::hash(&preimage).to_byte_array();

                    debug!(
                        "Payment hash from ticket.hash: {}",
                        hex::encode(payment_hash_from_ticket)
                    );
                    debug!(
                        "Payment hash from preimage: {}",
                        hex::encode(payment_hash_from_preimage)
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
                .collect::<Result<Vec<_>, anyhow::Error>>()?
        } else {
            // Escrow disabled - coordinator wallet funds the contract directly
            debug!("Escrow disabled - using coordinator wallet UTXOs for funding");
            vec![]
        };

        debug!("Contract amount: {}", contract_amount_sats);
        debug!(
            "Escrow inputs: {} (escrow_enabled={})",
            escrow_inputs.len(),
            self.escrow_enabled
        );

        let psbt = match &competition.funding_psbt_base64 {
            Some(encoded) => Psbt::from_str(encoded)?,
            None => {
                self.bitcoin
                    .build_psbt(
                        funding_script.clone(),
                        Amount::from_sat(contract_amount_sats as u64),
                        contract_params.fee_rate,
                        vec![],
                        escrow_inputs,
                    )
                    .await?
            }
        };
        if let Err(error) = self
            .bitcoin
            .reserve_psbt_inputs_until(
                &psbt,
                competition.funding_reservation_deadline(OffsetDateTime::now_utc())?,
            )
            .await
        {
            if competition.funding_psbt_base64.is_none() {
                if let Err(release_error) = self.bitcoin.release_psbt_inputs(&psbt).await {
                    warn!("Failed to release new funding inputs: {}", release_error);
                }
            }
            return Err(error);
        }

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
        let ticketed_dlc = TicketedDLC::new(contract_params.clone(), funding_outpoint)?;
        debug!("Built ticketed dlc");

        if self.is_keymeld_enabled() {
            // Keymeld flow: Retrieve keygen session (created at competition creation) and register participants
            info!(
                "Using Keymeld for MuSig2 signing for competition {}",
                competition.id
            );

            // Retrieve the keygen session created at competition creation time
            let stored_session = self
                .competition_store
                .get_keymeld_session(competition.id)
                .await
                .map_err(|e| anyhow!("Failed to get keymeld session: {}", e))?
                .ok_or_else(|| {
                    anyhow!(
                        "No keymeld session found for competition {} - was keymeld enabled at competition creation?",
                        competition.id
                    )
                })?;

            let keygen_session = self.restore_keymeld_session(&stored_session)?;

            info!(
                "Retrieved keymeld keygen session {} for competition {}, registering participants",
                keygen_session.session_id, competition.id
            );

            // Use ticket_id as keymeld user_id for each player
            let player_user_ids: Vec<UserId> = entries
                .iter()
                .map(|entry| UserId::from(entry.ticket_id))
                .collect();

            for (entry, user_id) in entries.iter().zip(player_user_ids.iter()) {
                let registration_data = ParticipantRegistrationData {
                    encrypted_private_key: entry.encrypted_keymeld_private_key.clone().ok_or_else(
                        || {
                            anyhow!(
                                "Entry {} is missing its authorized registration envelope",
                                entry.id
                            )
                        },
                    )?,
                    public_key: entry.ephemeral_pubkey.clone(),
                    auth_pubkey: entry.keymeld_auth_pubkey.clone().ok_or_else(|| {
                        anyhow!("Entry {} is missing its authentication key", entry.id)
                    })?,
                    context: entry.keymeld_registration_context.clone().ok_or_else(|| {
                        anyhow!("Entry {} is missing its registration context", entry.id)
                    })?,
                };
                self.keymeld
                    .register_participant(&keygen_session, user_id.clone(), &registration_data)
                    .await
                    .map_err(|error| {
                        anyhow!("Participant {user_id} registration failed: {error}")
                    })?;
            }

            // For keymeld, we don't use local nonces - set a placeholder to indicate keymeld mode
            // The actual aggregate key is stored in the keygen session after completion
            if competition.public_nonces.is_none() {
                // Create empty SigMap to indicate keymeld mode (no local nonces)
                competition.public_nonces = Some(SigMap {
                    by_outcome: BTreeMap::new(),
                    by_win_condition: BTreeMap::new(),
                });
            }
        } else {
            // Traditional MuSig2 flow: Generate local nonces
            let signing_session = {
                let mut rng = create_deterministic_rng(
                    &funding_outpoint,
                    self.private_key,
                    &contract_params,
                )?;
                SigningSession::<NonceSharingRound>::new(ticketed_dlc, &mut rng, self.private_key)?
            };
            debug!("Started musig nonce sharing round");
            if competition.public_nonces.is_none() {
                competition.public_nonces = Some(signing_session.our_public_nonces().to_owned());
            }
        }

        competition.contracted_at = Some(OffsetDateTime::now_utc());
        competition.errors = vec![];

        Ok(competition)
    }

    /// Aggregate nonces from all participants and generate coordinator's partial signatures
    ///
    /// When keymeld is enabled, this step is skipped - keymeld handles nonce aggregation
    /// and partial signature generation internally during the signing session.
    pub async fn generate_aggregate_nonces_and_coord_partial_signatures<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        // When keymeld is enabled, skip this step - keymeld handles nonce aggregation internally
        if self.is_keymeld_enabled() {
            info!(
                "Keymeld mode: skipping nonce aggregation for competition {} (handled by keymeld)",
                competition.id
            );
            // Set placeholder values to allow state transition
            if competition.aggregated_nonces.is_none() {
                competition.aggregated_nonces = Some(SigMap {
                    by_outcome: BTreeMap::new(),
                    by_win_condition: BTreeMap::new(),
                });
            }
            if competition.partial_signatures.is_none() {
                competition.partial_signatures = Some(SigMap {
                    by_outcome: BTreeMap::new(),
                    by_win_condition: BTreeMap::new(),
                });
            }
            competition.errors = vec![];
            return Ok(competition);
        }

        // Traditional MuSig2 flow
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
            let mut rng =
                create_deterministic_rng(funding_outpoint, self.private_key, contract_parameters)?;
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

    /// Sign the DLC contract by aggregating all partial signatures
    ///
    /// When keymeld is enabled, signing is coordinated via the keymeld service which
    /// handles nonce aggregation and signature generation internally.
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

        let ticketed_dlc =
            TicketedDLC::new(contract_parameters.to_owned(), funding_outpoint.to_owned())?;

        if self.is_keymeld_enabled() {
            // Keymeld flow: Use sign_dlc_batch to get all signatures
            info!(
                "Using Keymeld for DLC signing for competition {}",
                competition.id
            );

            // Retrieve stored keygen session and decrypt the session secret
            let stored_session = self
                .competition_store
                .get_keymeld_session(competition.id)
                .await
                .map_err(|e| anyhow!("Failed to get keymeld session: {}", e))?
                .ok_or_else(|| {
                    anyhow!(
                        "No keymeld session found for competition {}",
                        competition.id
                    )
                })?;

            let keygen_session = self.restore_keymeld_session(&stored_session)?;

            // Get signing data from ticketed DLC
            let signing_data = ticketed_dlc.signing_data()?;

            // Get entries to build player_user_ids for subset definitions
            // Sort by ticket_id to match the order used in keymeld subset definitions
            let mut entries = self
                .competition_store
                .get_competition_entries(competition.id, vec![EntryStatus::Paid])
                .await?;
            entries.sort_by_key(|entry| entry.ticket_id);
            let player_user_ids: Vec<UserId> = entries
                .iter()
                .map(|entry| UserId::from(entry.ticket_id))
                .collect();

            self.verify_keymeld_competition(competition, &keygen_session)
                .await?;

            // Call keymeld to perform batch signing
            let dlc_signatures = self
                .keymeld
                .sign_dlc_batch(
                    &keygen_session,
                    &signing_data,
                    contract_parameters,
                    player_user_ids,
                )
                .await
                .map_err(|e| anyhow!("Keymeld signing failed: {}", e))?;

            info!(
                "Keymeld signing completed for competition {} with {} outcome signatures and {} split signatures",
                competition.id,
                dlc_signatures.outcome_signatures.len(),
                dlc_signatures.split_signatures.len()
            );

            let contract_signatures = ContractSignatures {
                expiry_tx_signature: dlc_signatures.expiry_signature,
                outcome_tx_signatures: dlc_signatures.outcome_signatures,
                split_tx_signatures: dlc_signatures.split_signatures,
            };

            // The market maker's verification covers every signature in the
            // contract; a bad set would lock the funding output until every
            // player cooperates, so the contract is only accepted once verified.
            let market_maker = ticketed_dlc.params().market_maker.pubkey;
            let signed_contract = ticketed_dlc
                .into_signed_contract(market_maker, contract_signatures)
                .map_err(|e| anyhow!("keymeld produced an invalid contract signature set: {e}"))?;

            if competition.signed_contract.is_none() {
                competition.signed_contract = Some(signed_contract);
                competition.signed_at = Some(OffsetDateTime::now_utc());
            }
        } else {
            // Traditional MuSig2 flow
            let Some(coordinator_partial_sigantures) = competition.partial_signatures.clone()
            else {
                return Err(anyhow!(
                    "coordinator partial signatures do not exist, failed signing competition dlc contract {}",
                    competition.id
                ));
            };

            let Some(coordinator_nonces) = competition.public_nonces.as_ref() else {
                return Err(anyhow!("coordinator nonces missing"));
            };

            let signing_session = {
                let mut rng = create_deterministic_rng(
                    funding_outpoint,
                    self.private_key,
                    contract_parameters,
                )?;
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
            let mut partial_sigs_by_sender: BTreeMap<Point, SigMap<PartialSignature>> =
                BTreeMap::new();

            for (sender_pubkey, final_signature) in &final_signatures_by_sender {
                match coordinator_session
                    .verify_partial_signatures(*sender_pubkey, &final_signature.partial_signatures)
                {
                    Ok(_) => {
                        partial_sigs_by_sender
                            .insert(*sender_pubkey, final_signature.partial_signatures.clone());

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
        }

        competition.errors = vec![];
        Ok(competition)
    }

    async fn sign_and_broadcast_funding_tx<'a>(
        &self,
        competition: &'a mut Competition,
    ) -> Result<&'a mut Competition, anyhow::Error> {
        if self.is_keymeld_enabled() {
            let stored = self
                .competition_store
                .get_keymeld_session(competition.id)
                .await?
                .ok_or_else(|| anyhow!("Competition has no authorized Keymeld session"))?;
            let session = self.restore_keymeld_session(&stored)?;
            self.verify_keymeld_competition(competition, &session)
                .await?;
        }
        let Some(funding_psbt_base64) = competition.funding_psbt_base64.clone() else {
            return Err(anyhow!(
                        "Unsigned funding psbt doesn't exists, failed publishing competition {} funding transaction",
                        competition.id
                    ));
        };

        self.renew_funding_reservation(competition).await?;
        let mut funding_psbt = Psbt::from_str(&funding_psbt_base64)?;

        // When escrow is enabled, users sign their own funding PSBTs which need to be merged.
        // When escrow is disabled (hold invoice flow), the coordinator funds the DLC directly.
        if self.is_escrow_enabled() {
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
        } else {
            debug!(
                "Escrow disabled: coordinator signs funding PSBT directly (no user PSBTs to merge)"
            );
        }

        let funding_transaction = signed_funding_tx(self.bitcoin.clone(), funding_psbt).await?;

        debug!(
            "Broadcasting funding transaction: {:?}",
            funding_transaction
        );

        self.bitcoin.broadcast(&funding_transaction).await?;
        info!(
            "Competition {} funding tx broadcast: txid={}",
            competition.id,
            funding_transaction.compute_txid()
        );

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
        match self.transaction_confirmations(&txid).await? {
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

        let event = self.oracle_client.get_event(&competition.id).await?;
        let Some(attestation) = event.attestation else {
            info!(
                "No oracle attestation found for competition {} yet, skipping add",
                competition.id
            );
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
        if competition.outcome_broadcasted_at.is_some() && competition.outcome_transaction.is_some()
        {
            info!(
                "Outcome transaction already published for competition {}, skipping",
                competition.id
            );
            return Ok(competition);
        }

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
            info!(
                "Competition {} outcome tx broadcast: txid={}",
                competition.id,
                outcome_tx.compute_txid()
            );
            competition.outcome_broadcasted_at = Some(OffsetDateTime::now_utc());
            competition.errors = vec![];

            return Ok(competition);
        }

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
                    info!(
                        "Competition {} expiry tx broadcast: txid={}",
                        competition.id,
                        expiry_tx.compute_txid()
                    );
                    competition.expiry_broadcasted_at = Some(OffsetDateTime::now_utc())
                };

                return Ok(competition);
            }
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

        let blocks_since_outcome = current_height
            .checked_sub(outcome_height)
            .ok_or_else(|| anyhow!("LND chain tip is behind the outcome confirmation height"))?;
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
        let fee_rate = fee_rate_for_target(&fee_rates, 1)?;

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
                        if let Some(player) = signed_contract.params().players.get(player_index) {
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

        info!(
            "Competition {} delta check: outcome={:?}, winners={}, paid_winners={}, entries={}",
            competition.id,
            outcome,
            winners.len(),
            paid_winners.len(),
            entries.len()
        );
        for (player_index, entry) in &paid_winners {
            info!(
                "Competition {} paid_winner: player_index={}, entry_id={}, paid_out_at={:?}, ephemeral_privatekey={}, sellback_broadcasted_at={:?}",
                competition.id,
                player_index,
                entry.id,
                entry.paid_out_at,
                entry.ephemeral_privatekey.is_some(),
                entry.sellback_broadcasted_at
            );
        }

        if paid_winners.len() != winners.len() {
            info!(
                "Competition {} not all winners paid: paid_winners={}, winners={}, blocks_since_outcome={}, required_2x_delta={}",
                competition.id,
                paid_winners.len(),
                winners.len(),
                blocks_since_outcome,
                2 * required_delta
            );
            // Log which winners are missing
            for &player_index in winners.keys() {
                let found_entry = entries.iter().find(|entry| {
                    let Ok(pubkey) = Point::from_hex(&entry.ephemeral_pubkey) else {
                        return false;
                    };
                    signed_contract
                        .params()
                        .players
                        .get(player_index)
                        .map(|player| player.pubkey == pubkey)
                        .unwrap_or(false)
                });
                if let Some(entry) = found_entry {
                    let is_paid_winner = paid_winners.iter().any(|(idx, _)| *idx == player_index);
                    info!(
                        "Competition {} winner player_index={}: entry_id={}, paid_out_at={:?}, ephemeral_privatekey={}, sellback_broadcasted_at={:?}, in_paid_winners={}",
                        competition.id,
                        player_index,
                        entry.id,
                        entry.paid_out_at,
                        entry.ephemeral_privatekey.is_some(),
                        entry.sellback_broadcasted_at,
                        is_paid_winner
                    );
                } else {
                    info!(
                        "Competition {} winner player_index={}: NO MATCHING ENTRY FOUND",
                        competition.id, player_index
                    );
                }
            }
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
            info!(
                "Competition {} taking UNIFIED CLOSE path: all {} winners paid",
                competition.id,
                paid_winners.len()
            );
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

            info!(
                "Competition {} unified close: winner_seckeys_count={}, close_tx_prevout_value={}",
                competition.id,
                winner_seckeys.len(),
                close_tx_prevout.value
            );

            let input_index = close_tx_input.previous_output.vout as usize;

            signed_contract.sign_outcome_close_tx_input(
                &outcome,
                &mut close_tx,
                input_index,
                &Prevouts::All(&[close_tx_prevout]),
                self.private_key,
                &winner_seckeys,
            )?;

            if competition.delta_broadcasted_at.is_none() {
                info!(
                    "Competition {} broadcasting unified close tx",
                    competition.id
                );
                self.bitcoin.broadcast(&close_tx).await?;
                info!(
                    "Competition {} unified close tx broadcast: txid={}",
                    competition.id,
                    close_tx.compute_txid()
                );
                competition.delta_broadcasted_at = Some(OffsetDateTime::now_utc());
            } else {
                info!(
                    "Competition {} unified close already broadcast, skipping",
                    competition.id
                );
            }

            // Mark all entries as closed
            let now = OffsetDateTime::now_utc();
            for (_, entry) in paid_winners {
                self.competition_store
                    .mark_entry_sellback_broadcast(entry.id, now)
                    .await?;
            }
        } else {
            info!(
                "Competition {} taking SPLIT TX path: paid_winners={}, winners={}",
                competition.id,
                paid_winners.len(),
                winners.len()
            );
            // Not all winners have been paid via lightning.
            // We need the split TX so each winner has their own output to
            // claim from (on-chain via split-win, or off-chain via lightning
            // leading to split-close/split-sellback). Delta2 will later do
            // split-reclaim for any winners still unpaid after 2*delta blocks.

            // Broadcast the split TX if not already done.
            // Use a paid winner's ticket preimage when available - easier to
            // reason about which preimage was used. Fall back to any winner's
            // preimage if no one has been paid yet.
            if competition.delta_broadcasted_at.is_none() {
                let (split_player_index, split_entry) = if !paid_winners.is_empty() {
                    info!(
                        "Competition {} using paid winner for split TX preimage",
                        competition.id
                    );
                    let &(idx, entry) = &paid_winners[0];
                    (idx, entry)
                } else {
                    info!(
                        "Competition {} no paid winners, finding any winner entry for split TX preimage",
                        competition.id
                    );
                    // No paid winners yet - find any winner's entry for the preimage
                    winners
                        .keys()
                        .find_map(|&player_index| {
                            entries
                                .iter()
                                .find(|entry| {
                                    let Ok(pubkey) = Point::from_hex(&entry.ephemeral_pubkey)
                                    else {
                                        return false;
                                    };
                                    signed_contract
                                        .params()
                                        .players
                                        .get(player_index)
                                        .map(|player| player.pubkey == pubkey)
                                        .unwrap_or(false)
                                })
                                .map(|entry| (player_index, entry))
                        })
                        .ok_or_else(|| {
                            anyhow!(
                                "Competition {} has no winner entries to build split TX",
                                competition.id
                            )
                        })?
                };

                info!(
                    "Competition {} building split TX: split_player_index={}, split_entry_id={}, ticket_id={}",
                    competition.id,
                    split_player_index,
                    split_entry.id,
                    split_entry.ticket_id
                );

                let ticket = self
                    .competition_store
                    .get_ticket(split_entry.ticket_id)
                    .await
                    .map_err(|e| anyhow!("Failed to get ticket for split TX: {}", e))?;

                let ticket_preimage =
                    dlctix::hashlock::preimage_from_hex(&ticket.encrypted_preimage)
                        .map_err(|e| anyhow!("Failed to decode ticket preimage: {}", e))?;

                let win_cond = WinCondition {
                    outcome,
                    player_index: split_player_index,
                };

                info!(
                    "Competition {} signing split TX: outcome={:?}, player_index={}",
                    competition.id, win_cond.outcome, win_cond.player_index
                );

                let split_tx = signed_contract
                    .signed_split_tx(&win_cond, ticket_preimage)
                    .map_err(|e| anyhow!("Failed to build signed split TX: {}", e))?;

                self.bitcoin.broadcast(&split_tx).await?;
                info!(
                    "Competition {} split tx broadcast: txid={}",
                    competition.id,
                    split_tx.compute_txid()
                );
                competition.delta_broadcasted_at = Some(OffsetDateTime::now_utc());
            } else {
                info!(
                    "Competition {} split TX already broadcast, processing individual closes",
                    competition.id
                );
            }

            // Handle individual cooperative closes for paid winners
            for (player_index, entry) in paid_winners {
                // Skip if already processed
                if entry.sellback_broadcasted_at.is_some() {
                    info!(
                        "Competition {} skipping already-closed entry {} for player {}",
                        competition.id, entry.id, player_index
                    );
                    continue;
                }

                info!(
                    "Competition {} broadcasting split-close for player {}, entry {}",
                    competition.id, player_index, entry.id
                );

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

                let input_index = close_tx_input.previous_output.vout as usize;

                signed_contract.sign_split_close_tx_input(
                    &win_condition,
                    &mut close_tx,
                    input_index,
                    &Prevouts::All(&[close_tx_prevout]),
                    self.private_key,
                    winner_seckey,
                )?;

                self.bitcoin.broadcast(&close_tx).await?;
                info!(
                    "Competition {} split-close tx broadcast for player {}: txid={}",
                    competition.id,
                    player_index,
                    close_tx.compute_txid()
                );

                // Mark entry as closed
                self.competition_store
                    .mark_entry_sellback_broadcast(entry.id, OffsetDateTime::now_utc())
                    .await?;
            }
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

        let blocks_since_outcome = current_height
            .checked_sub(outcome_height)
            .ok_or_else(|| anyhow!("LND chain tip is behind the outcome confirmation height"))?;
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
        let fee_rate = fee_rate_for_target(&fee_rates, 1)?;

        // The split TX was broadcast during delta, so each winner has their
        // own output. Use split-reclaim for unpaid winners who haven't been
        // closed or reclaimed yet.
        for &player_index in winners.keys() {
            if let Some(entry) = entries.iter().find(|entry| {
                let Ok(pubkey) = Point::from_hex(&entry.ephemeral_pubkey) else {
                    return false;
                };
                if let Some(player) = signed_contract.params().players.get(player_index) {
                    player.pubkey == pubkey
                } else {
                    false
                }
            }) {
                // Skip if already processed, paid out, or already closed via delta
                if entry.reclaimed_broadcasted_at.is_some()
                    || entry.paid_out_at.is_some()
                    || entry.sellback_broadcasted_at.is_some()
                {
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

                let input_index = reclaim_tx_input.previous_output.vout as usize;

                signed_contract.sign_split_reclaim_tx_input(
                    &win_condition,
                    &mut reclaim_tx,
                    input_index,
                    &Prevouts::All(&[reclaim_tx_prevout]),
                    self.private_key,
                )?;

                self.bitcoin.broadcast(&reclaim_tx).await?;
                info!(
                    "Competition {} split-reclaim tx broadcast for player {}: txid={}",
                    competition.id,
                    player_index,
                    reclaim_tx.compute_txid()
                );

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
        let dlc_pubkey = self.public_key;
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
        let dlc_pubkey = self.public_key;
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

        // First insert the competition into the database
        let competition = self
            .competition_store
            .add_competition_with_tickets(competition, tickets.clone())
            .map_err(|e| {
                error!(
                    "competition added to oracle, but failed to be saved with tickets: competition_id {} {:?}",
                    create_event.id, e
                );
                Error::from(e)
            })
            .await?;

        // If keymeld is enabled, create the keygen session now with all ticket_ids
        // This allows users to derive their auth_pubkey before submitting their entry
        // NOTE: This must happen AFTER add_competition_with_tickets since store_keymeld_session
        // does an UPDATE on the competitions table
        if self.is_keymeld_enabled() {
            let player_user_ids: Vec<UserId> = tickets
                .iter()
                .map(|ticket| UserId::from(ticket.id))
                .collect();

            // Pre-compute subset definitions for all possible outcomes
            // This is required because keymeld needs to know all subsets at keygen time
            let subset_info = compute_dlc_subset_definitions(
                self.keymeld.coordinator_user_id(),
                &player_user_ids,
                competition.event_submission.number_of_places_win,
            );

            info!(
                "Creating keymeld keygen session for competition {} with {} potential participants and {} subset definitions",
                competition.id,
                player_user_ids.len(),
                subset_info.definitions.len()
            );

            let keygen_session = self
                .keymeld
                .init_keygen_session(competition.id, player_user_ids, subset_info)
                .await
                .map_err(|error| {
                    Error::Bitcoin(anyhow!("Keymeld session creation failed: {error}"))
                })?;
            self.store_keymeld_session(competition.id, keygen_session)
                .await?;
        }

        Ok(competition)
    }

    pub async fn get_competitions(&self) -> Result<Vec<Competition>, Error> {
        self.competition_store
            .get_competitions(false)
            .map_err(|e| {
                error!("failed to get competitions: {:?}", e);
                Error::from(e)
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
        if !matches!(competition.get_state(), CompetitionState::Created) {
            return Err(Error::BadRequest(
                "Competition is no longer accepting entries".into(),
            ));
        }
        if competition.total_entries as usize >= competition.event_submission.total_allowed_entries
        {
            return Err(Error::CompetitionFull);
        }
        debug!("got competition: {:?}", competition);

        // Get ticket
        let ReservedTicket {
            ticket,
            superseded_payment_hash,
        } = self
            .competition_store
            .get_and_reserve_ticket(competition_id, &pubkey)
            .await
            .map_err(|e| match e {
                DatabaseWriteError::Sqlx(sqlx::Error::RowNotFound) => Error::NoAvailableTickets,
                e => Error::from(e),
            })?;
        if let Some(old_hash) = superseded_payment_hash {
            self.cancel_superseded_invoice(ticket.id, old_hash).await;
        }
        match self
            .create_ticket_response(ticket.clone(), btc_pubkey, competition)
            .await
        {
            Ok(response) => Ok(response),
            Err(e) => {
                if let Err(clear_err) = self
                    .competition_store
                    .clear_ticket_reservation(&ticket)
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

    /// A stale reservation was taken over and the ticket already carries a
    /// fresh hash, so nothing the previous holder still has can pay for it.
    /// Their invoice is cancelled to refund any late payment promptly; if LND
    /// cannot be reached, LND itself cancels a held HTLC before it times out.
    async fn cancel_superseded_invoice(&self, ticket_id: Uuid, old_hash: String) {
        if let Err(e) = self.ln.cancel_hold_invoice(old_hash).await {
            warn!(
                "Failed to cancel the superseded invoice of ticket {}: {}",
                ticket_id, e
            );
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

        // Generate escrow transaction only if escrow is enabled
        let escrow_tx_hex = if self.escrow_enabled {
            let escrow_tx = generate_escrow_tx(
                self.bitcoin.clone(),
                ticket.id,
                btc_pubkey,
                payment_hash,
                competition.event_submission.entry_fee as u64,
                competition
                    .funding_reservation_deadline(OffsetDateTime::now_utc())
                    .map_err(Error::Bitcoin)?,
            )
            .await
            .map_err(|e| {
                error!("Failed to generate escrow transaction: {}", e);
                Error::BadRequest("Failed to generate refund transaction".to_string())
            })?;

            debug!("escrow_tx: {:?}", escrow_tx);

            let escrow_hex = hex::encode(dlctix::bitcoin::consensus::encode::serialize(&escrow_tx));

            // Store the escrow transaction in the database
            let stored = self
                .competition_store
                .update_ticket_escrow(&ticket, btc_pubkey.to_string(), escrow_hex.clone())
                .await
                .map_err(|e| {
                    error!("Failed to update ticket with escrow transaction: {}", e);
                    Error::from(e)
                })?;

            if !stored {
                return Err(Error::BadRequest(
                    "Ticket reservation changed; request a new ticket".into(),
                ));
            }

            debug!(
                "Created ticket {} with escrow tx {}",
                ticket.id,
                escrow_tx.compute_txid()
            );

            Some(escrow_hex)
        } else {
            debug!(
                "Created ticket {} without escrow (escrow disabled)",
                ticket.id
            );
            None
        };

        let fee_multiplier = competition.event_submission.coordinator_fee_percentage as f64 / 100.0;
        let coordinator_fee =
            (competition.event_submission.entry_fee as f64 * fee_multiplier).round() as u64;

        let full_fee = (competition.event_submission.entry_fee as u64) + coordinator_fee;

        // Check if ticket already has a payment request (reuse existing invoice if not expired)
        // Invoice needs to stay active through:
        // 1. Registration period (until start_observation_date when entries close)
        // 2. Keymeld signing (~5 min buffer)
        // 3. Funding transaction broadcast (~5 min buffer)
        let now = time::OffsetDateTime::now_utc();
        let time_until_entries_close =
            (competition.event_submission.start_observation_date - now).whole_seconds();
        let buffer_seconds = 600i64; // 10 minutes buffer for signing + broadcast
        let invoice_expiry_seconds = (time_until_entries_close + buffer_seconds).max(900); // minimum 15 min
        let payment_request = if let Some(existing_payment_request) = &ticket.payment_request {
            // Expired unpaid invoices are rotated atomically by the ticket
            // store. LND does not permit reusing a cancelled payment hash.
            existing_payment_request.clone()
        } else {
            // Create new HODL invoice
            // Note: escrow_tx_hex is empty string when escrow disabled
            let invoice = self
                .ln
                .add_hold_invoice(
                    full_fee,
                    invoice_expiry_seconds as u64,
                    hex::encode(payment_hash),
                    ticket.competition_id,
                    escrow_tx_hex.clone().unwrap_or_default(),
                )
                .await
                .map_err(|e| {
                    error!("Failed to create HODL invoice: {}", e);
                    Error::BadRequest("Failed to create invoice".to_string())
                })?;

            let expires_at =
                time::OffsetDateTime::now_utc() + time::Duration::seconds(invoice_expiry_seconds);

            // Update ticket with payment request and expiry
            let stored = self
                .competition_store
                .update_ticket_payment_request(&ticket, &invoice.payment_request, expires_at)
                .await
                .map_err(|e| {
                    error!("Failed to update ticket with payment request: {}", e);
                    Error::from(e)
                })?;

            if !stored {
                self.cancel_superseded_invoice(ticket.id, ticket.hash.clone())
                    .await;
                return Err(Error::BadRequest(
                    "Ticket reservation changed; request a new ticket".into(),
                ));
            }
            invoice.payment_request
        };

        let keymeld_registration = if self.is_keymeld_enabled() {
            let stored = self
                .competition_store
                .get_keymeld_session(competition.id)
                .await
                .map_err(Error::from)?
                .ok_or_else(|| {
                    Error::BadRequest("Competition has no authorized Keymeld session".into())
                })?;
            let session = self.restore_keymeld_session(&stored)?;
            Some(
                self.keymeld
                    .get_registration_assignment(&session, UserId::from(ticket.id))
                    .await
                    .map_err(|error| Error::Bitcoin(anyhow!(error)))?,
            )
        } else {
            None
        };
        let keymeld_session_id = keymeld_registration
            .as_ref()
            .map(|assignment| assignment.session_id.clone());
        let keymeld_enclave_public_key = keymeld_registration
            .as_ref()
            .map(|assignment| assignment.enclave_public_key.clone());

        Ok(TicketResponse {
            ticket_id: ticket.id,
            payment_request,
            escrow_tx: escrow_tx_hex,
            payment_hash: hex::encode(payment_hash),
            amount_sats: full_fee,
            // ticket_id is used as the keymeld user_id for consistency
            keymeld_user_id: ticket.id,
            keymeld_gateway_url: self.keymeld_gateway_url.clone(),
            keymeld_session_id,
            keymeld_enclave_public_key,
            keymeld_registration,
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
                    e => Error::from(e),
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
                Error::from(e)
            })
            .await
    }

    /// Delete a competition by ID. Only allowed if no entries have been paid.
    pub async fn delete_competition(&self, competition_id: Uuid) -> Result<(), Error> {
        // First check if competition exists and has no paid entries
        let competition = self.get_competition(competition_id).await?;

        if competition.total_paid_entries > 0 {
            return Err(Error::BadRequest(format!(
                "Cannot delete competition {} - it has {} paid entries",
                competition_id, competition.total_paid_entries
            )));
        }

        self.competition_store
            .delete_competition(competition_id)
            .await
            .map_err(|e| {
                error!(
                    "failed to delete competition: competition_id {} {:?}",
                    competition_id, e
                );
                Error::from(e)
            })?;

        info!("Deleted competition: {}", competition_id);
        Ok(())
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
                DatabaseWriteError::Sqlx(sqlx::Error::RowNotFound) => {
                    Error::BadRequest("Invalid ticket or competition".into())
                }
                e => Error::from(e),
            })?;

        Ok(())
    }

    pub async fn add_entry(&self, pubkey: String, mut entry: AddEntry) -> Result<UserEntry, Error> {
        let competition = self
            .competition_store
            .get_competition(entry.event_id)
            .await
            .map_err(|e| {
                error!("error {:?}", e);
                match e {
                    sqlx::Error::RowNotFound => Error::BadRequest("Competition not found".into()),
                    e => Error::from(e),
                }
            })?;

        if !matches!(competition.get_state(), CompetitionState::Created) {
            return Err(Error::BadRequest(
                "Competition is no longer accepting entries".into(),
            ));
        }
        validate_entry_keys(&mut entry)?;
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
                    e => Error::from(e),
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

        if ticket.competition_id != entry.event_id {
            return Err(Error::BadRequest(
                "Ticket belongs to a different competition".into(),
            ));
        }

        if let Some(btc_pubkey) = &ticket.ephemeral_pubkey {
            if btc_pubkey != &entry.ephemeral_pubkey {
                return Err(Error::BadRequest(format!(
                    "Entry public key {} must match ticket escrow public key {}",
                    entry.ephemeral_pubkey, btc_pubkey
                )));
            }
        }

        if self.is_keymeld_enabled() {
            let stored = self
                .competition_store
                .get_keymeld_session(entry.event_id)
                .await
                .map_err(Error::from)?
                .ok_or_else(|| {
                    Error::BadRequest("Competition has no authorized Keymeld session".into())
                })?;
            let session = self.restore_keymeld_session(&stored)?;
            let data = ParticipantRegistrationData {
                encrypted_private_key: entry.encrypted_keymeld_private_key.clone().ok_or_else(
                    || Error::BadRequest("Missing authorized Keymeld registration envelope".into()),
                )?,
                public_key: entry.ephemeral_pubkey.clone(),
                auth_pubkey: entry.keymeld_auth_pubkey.clone().ok_or_else(|| {
                    Error::BadRequest("Missing Keymeld authentication key".into())
                })?,
                context: entry.keymeld_registration_context.clone().ok_or_else(|| {
                    Error::BadRequest("Missing Keymeld registration context".into())
                })?,
            };
            session
                .validate_registration(&UserId::from(ticket.id), &data)
                .map_err(|_| Error::BadRequest("Invalid Keymeld registration context".into()))?;
        }

        let user_entry = self
            .competition_store
            .add_entry(entry.clone().into_user_entry(pubkey), ticket.id)
            .await
            .map_err(|e| match e {
                DatabaseWriteError::Sqlx(sqlx::Error::RowNotFound) => {
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
                    Error::from(e)
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
            .map_err(Error::from)
            .await
    }

    /// Get lightweight entry views for the entries list page.
    /// Single query that joins entries with competitions for dates and payout status.
    pub async fn get_user_entry_views(&self, pubkey: String) -> Result<Vec<UserEntryView>, Error> {
        self.competition_store
            .get_user_entry_views(pubkey)
            .map_err(Error::from)
            .await
    }

    /// Get a single entry by ID (public, for leaderboard entry details)
    pub async fn get_entry_by_id(&self, entry_id: Uuid) -> Result<Option<UserEntry>, Error> {
        self.competition_store
            .get_entry_by_id(entry_id)
            .map_err(Error::from)
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
                pubkey.clone(),
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

        // Get keymeld signing info if enabled
        let keymeld = if self.is_keymeld_enabled() {
            self.get_keymeld_signing_info(competition_id, &pubkey, &entries[0])
                .await
                .ok()
        } else {
            None
        };

        Ok(FundedContract {
            contract_params: contract,
            funding_outpoint,
            funding_psbt_base64,
            keymeld,
        })
    }

    /// Get keymeld signing info for a user's entry
    /// Only returns info if the user's ticket has been paid (HODL invoice accepted)
    /// Decrypts the stored session secret and re-encrypts it to the user's nostr pubkey
    async fn get_keymeld_signing_info(
        &self,
        competition_id: Uuid,
        user_pubkey: &str,
        entry: &UserEntry,
    ) -> Result<KeymeldSigningInfo, Error> {
        // Check that the user's ticket has been paid before revealing keymeld info
        let ticket = self
            .competition_store
            .get_ticket(entry.ticket_id)
            .await
            .map_err(Error::from)?;

        if ticket.paid_at.is_none() {
            return Err(Error::BadRequest(
                "Keymeld info not available until payment is received".to_string(),
            ));
        }

        let stored_session = self
            .competition_store
            .get_keymeld_session(competition_id)
            .await
            .map_err(Error::from)?
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "Keymeld session not found for competition {}",
                    competition_id
                ))
            })?;

        let gateway_url = self
            .keymeld_gateway_url
            .clone()
            .ok_or_else(|| Error::BadRequest("Keymeld gateway URL not configured".to_string()))?;

        // Decrypt the session secret from storage
        let session = self.restore_keymeld_session(&stored_session)?;
        let session_secret = session.session_secret;

        // Re-encrypt to the user's pubkey
        let nostr_pubkey = NostrPublicKey::from_hex(user_pubkey)
            .map_err(|e| Error::BadRequest(format!("Invalid user pubkey: {}", e)))?;

        let coordinator_secret_key = SecretKey::from_slice(&self.private_key.serialize())
            .map_err(|e| Error::BadRequest(format!("Failed to create secret key: {}", e)))?;

        let encrypted_session_secret = nip44::encrypt(
            &coordinator_secret_key,
            &nostr_pubkey,
            hex::encode(session_secret),
            nip44::Version::V2,
        )
        .map_err(|e| Error::BadRequest(format!("NIP-44 encryption failed: {}", e)))?;

        Ok(KeymeldSigningInfo {
            enabled: true,
            gateway_url,
            session_id: stored_session.session_id,
            encrypted_session_secret,
            authorization_manifest: stored_session.authorization_manifest,
            // Use ticket_id as user_id for consistency with keygen session creation
            user_id: entry.ticket_id.to_string(),
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

        let Some(coordinator_nonces) = competition.public_nonces.as_ref() else {
            return Err(Error::BadRequest(
                "Coordinator nonces not yet available".to_string(),
            ));
        };

        if public_nonces.is_mirror(coordinator_nonces) {
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

        let stored = self
            .competition_store
            .add_public_nonces(entry_id, public_nonces)
            .await
            .map_err(|e| {
                error!(
                    "failed save entry public nonces: entry_id {}, event_id {} {:?}",
                    entry_id, competition_id, e
                );
                Error::from(e)
            })?;
        if !stored {
            return Err(Error::BadRequest(
                "Public nonces already submitted for this entry".to_string(),
            ));
        }

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

        let stored = self
            .competition_store
            .add_final_signatures(entry_id, final_signatures)
            .await
            .map_err(|e| {
                error!(
                    "failed save entry signatures: entry_id {}, event_id {} {:?}",
                    entry_id, competition_id, e
                );
                Error::from(e)
            })?;
        if !stored {
            return Err(Error::BadRequest(
                "Signatures already submitted for this entry".to_string(),
            ));
        }

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

        let invoice_amount_sats = extract_amount_from_invoice(&payout_info.ln_invoice)
            .map_err(|e| Error::BadRequest(format!("Invalid lightning invoice: {}", e)))?;

        if let Some(invoice_amount_sats) = invoice_amount_sats {
            if invoice_amount_sats != payout_amount_sats {
                return Err(Error::BadRequest(format!(
                    "Invoice amount {} sats does not match expected payout {} sats",
                    invoice_amount_sats, payout_amount_sats
                )));
            }
        }

        // The pending payout is recorded before anything is sent, and the
        // database allows one live payout per entry, so a concurrent or
        // repeated request cannot pay twice. The PayoutWatcher marks it paid
        // once the payment settles.
        let payout_id = self
            .competition_store
            .store_payout_info_pending(
                entry_id,
                payout_info.payout_preimage,
                payout_info.ephemeral_private_key,
                payout_info.ln_invoice.clone(),
                payout_amount_sats,
            )
            .await
            .map_err(|e| match e {
                DatabaseWriteError::Sqlx(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                    Error::BadRequest("A payout for this entry is already in progress".into())
                }
                e => Error::from(e),
            })?;
        info!("Payout {} initiated for entry {}", payout_id, entry_id);

        // The payout watcher is the sole sender. Persisting this outbox item
        // before acknowledging the request also survives shutdown before the
        // first RPC. Ambiguous RPC errors never release the entry's payout lock.
        Ok(())
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
            ticket_hash: parse_hash32(&ticket.hash)?,
            // players generate this preimage and only share after winning with the coordinator
            payout_hash: parse_hash32(payout_hash)?,
        };
        players.push(player);
    }

    Ok(players)
}
/// A 32-byte hash from hex. Entry and ticket hashes are validated on the way
/// in; a bad row must still fail the competition rather than panic the watcher.
fn parse_hash32(hex_str: &str) -> Result<[u8; 32], anyhow::Error> {
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(hex_str).map_err(|e| anyhow!("invalid hash hex: {}", e))?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| anyhow!("hash must be 32 bytes, got {}", bytes.len()))
}

/// Entry keys are used verbatim when the contract is built; reject anything
/// the contract builder could not parse so one entry cannot poison a
/// competition.
fn validate_entry_keys(entry: &mut AddEntry) -> Result<(), Error> {
    let pubkey = Point::from_hex(&entry.ephemeral_pubkey).map_err(|_| {
        Error::BadRequest("ephemeral_pubkey must be a compressed secp256k1 point in hex".into())
    })?;
    let payout_hash = parse_hash32(&entry.payout_hash)
        .map_err(|_| Error::BadRequest("payout_hash must be 32 bytes in hex".into()))?;
    // The database's per-competition uniqueness constraints compare strings.
    // Store a single encoding so case/prefix aliases cannot bypass them.
    entry.ephemeral_pubkey = hex::encode(pubkey.serialize());
    entry.payout_hash = hex::encode(payout_hash);
    Ok(())
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
    entries: &mut [UserEntry],
    players: &[Player],
) -> Result<BTreeMap<Outcome, PayoutWeights>, anyhow::Error> {
    debug!("Generating payouts for {} players", players.len());

    // Sort entries by ticket_id for consistent indexing
    // This ensures player indices match the ticket order used when creating
    // keymeld subset definitions at competition creation time
    entries.sort_by_key(|entry| entry.ticket_id);
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

/// Pre-compute DLC subset definitions for keygen session creation.
///
/// This generates subset definitions based on the competition parameters (number of entries
/// and number of winners). Each outcome has a subset containing the coordinator plus all
/// winning players for that outcome.
///
/// The player_user_ids must be ordered to match the player indices (0, 1, 2, ...).
/// At competition creation time, tickets are generated in order, so ticket_ids map
/// directly to player indices.
///
/// Returns DlcSubsetInfo containing:
/// - definitions: SubsetDefinition list for keygen session creation
/// - outcome_subset_ids: Mapping from outcome index to subset UUID for signing
pub fn compute_dlc_subset_definitions(
    coordinator_user_id: UserId,
    player_user_ids: &[UserId],
    number_of_places_win: usize,
) -> DlcSubsetInfo {
    let num_players = player_user_ids.len();
    let possible_rankings = generate_ranking_permutations(num_players, number_of_places_win);

    let mut definitions = Vec::new();
    let mut outcome_subset_ids = BTreeMap::new();

    for (outcome_index, winner_indices) in possible_rankings.iter().enumerate() {
        let subset_id = Uuid::now_v7();
        outcome_subset_ids.insert(outcome_index, subset_id);

        // Subset includes: coordinator + all winners for this outcome
        let mut participants = vec![coordinator_user_id.clone()];
        for &winner_idx in winner_indices {
            if winner_idx < player_user_ids.len() {
                participants.push(player_user_ids[winner_idx].clone());
            }
        }

        debug!(
            "Outcome {}: subset {} with {} participants (winners: {:?})",
            outcome_index,
            subset_id,
            participants.len(),
            winner_indices
        );

        definitions.push(SubsetDefinition::with_id(subset_id, participants));
    }

    info!(
        "Pre-computed {} subset definitions for {} players with {} winners per outcome",
        definitions.len(),
        num_players,
        number_of_places_win
    );

    DlcSubsetInfo {
        definitions,
        outcome_subset_ids,
    }
}

fn verify_accepted_keymeld_roster(
    session: &DlcKeygenSession,
    signed: &SignedRoster,
    expected_keys: &BTreeMap<UserId, Vec<u8>>,
    expected_contexts: &BTreeMap<UserId, RegistrationContext>,
    coordinator_id: UserId,
    number_of_places_win: usize,
) -> Result<(), anyhow::Error> {
    let roster = &signed.roster;
    let recipients = &session.recipient_authorization;
    let enclave = recipients
        .user_enclave_assignments
        .get(&coordinator_id)
        .ok_or_else(|| anyhow!("Coordinator enclave is absent from pinned recipients"))?;
    let key = recipients
        .recipient_public_keys
        .get(enclave)
        .ok_or_else(|| anyhow!("Coordinator enclave key is absent from pinned recipients"))?;
    signed.verify(key)?;
    signed.verify_registrations(&session.authorization_manifest)?;
    roster.verify_aggregates()?;
    if roster.participants != *expected_keys
        || !matches!(roster.taproot_tweak, keymeld_sdk::TaprootTweak::None)
    {
        return Err(anyhow!(
            "Keymeld roster does not match accepted participant keys or funding tweak"
        ));
    }
    for (user, authorization) in &roster.registrations {
        if recipients.user_enclave_assignments.get(user) != Some(&authorization.context.enclave_id)
            || authorization.context.require_signing_approval
            || (user != &coordinator_id
                && expected_contexts.get(user) != Some(&authorization.context))
        {
            return Err(anyhow!("Keymeld roster registration differs from accepted entry or delegated signing policy"));
        }
    }
    let players = expected_keys
        .keys()
        .filter(|user| *user != &coordinator_id)
        .cloned()
        .collect::<Vec<_>>();
    let rankings = generate_ranking_permutations(players.len(), number_of_places_win);
    if rankings.len() != session.outcome_subset_ids.len()
        || roster.subset_definitions.len() != session.outcome_subset_ids.len()
    {
        return Err(anyhow!("Keymeld subsets do not match competition outcomes"));
    }
    for (outcome, winners) in rankings.into_iter().enumerate() {
        let subset_id = session
            .outcome_subset_ids
            .get(&outcome)
            .ok_or_else(|| anyhow!("Missing Keymeld outcome subset"))?;
        let subset = roster
            .subset_definitions
            .iter()
            .find(|subset| &subset.subset_id == subset_id)
            .ok_or_else(|| anyhow!("Missing authorized outcome subset"))?;
        let mut participants = vec![coordinator_id.clone()];
        participants.extend(winners.into_iter().map(|winner| players[winner].clone()));
        if subset.participants != participants {
            return Err(anyhow!(
                "Keymeld subset participants differ from competition payout recipients"
            ));
        }
    }
    if !session.aggregate_key.is_empty() && session.aggregate_key != roster.aggregate_public_key {
        return Err(anyhow!(
            "Keymeld aggregate key differs from the completed session"
        ));
    }
    Ok(())
}

fn find_player_indices(
    players: &[Player],
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
    entries: &[UserEntry],
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
        .sign_psbt_with_escrow_support(&mut funding_tx)
        .await?;

    debug!(
        "Coordinator signing complete. Fully signed: {}",
        fully_signed
    );

    // Log signature state after signing
    for (i, input) in funding_tx.inputs.iter().enumerate() {
        if input.witness_script.is_some() {
            debug!("Input {} signature count: {}", i, input.partial_sigs.len());
            for pk in input.partial_sigs.keys() {
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
        } else if input.partial_sigs.is_empty() && input.tap_key_sig.is_none() {
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

/// Seed for the market maker's MuSig2 nonces.
///
/// Secret nonces are never stored: each signing step re-derives them and checks
/// they reproduce the published public nonces, so they must be deterministic.
/// The seed binds the key, the funding outpoint and the full contract
/// parameters, so a changed contract never reuses a nonce. The remaining rule
/// is that a nonce set must only ever sign one set of aggregate nonces; the
/// write-once `public_nonces`/`partial_signatures` fields and the one-shot
/// nonce submission per entry enforce that.
fn create_deterministic_rng(
    funding_outpoint: &OutPoint,
    private_key: Scalar,
    params: &ContractParameters,
) -> Result<ChaCha20Rng, anyhow::Error> {
    let params_digest = sha256::Hash::hash(&serde_json::to_vec(params)?);
    let tag = sha256::Hash::hash(b"coordinator/musig-nonce/v1");

    let mut engine = sha256::Hash::engine();
    engine.input(tag.as_byte_array());
    engine.input(tag.as_byte_array());
    engine.input(&private_key.serialize());
    engine.input(funding_outpoint.txid.as_byte_array());
    engine.input(&funding_outpoint.vout.to_le_bytes());
    engine.input(params_digest.as_byte_array());

    Ok(ChaCha20Rng::from_seed(
        sha256::Hash::from_engine(engine).to_byte_array(),
    ))
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

#[cfg(test)]
mod funding_lifecycle_tests {
    use super::*;

    #[test]
    fn confirmation_requirements_count_blocks_since_inclusion() {
        assert_eq!(confirmation_depth(900_000, 900_000), 1);
        assert_eq!(confirmation_depth(900_004, 900_000), 5);
        assert_eq!(confirmation_depth(900_005, 900_000), 6);
        assert_eq!(confirmation_depth(899_999, 900_000), 0);
    }

    #[test]
    fn funding_reservations_cover_future_competitions_and_extend_overdue_sessions() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let signing_date = now + time::Duration::days(30);
        let competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date,
            start_observation_date: now + time::Duration::days(28),
            end_observation_date: now + time::Duration::days(29),
            locations: vec![],
            number_of_values_per_entry: 1,
            number_of_places_win: 1,
            total_allowed_entries: 1,
            entry_fee: 1_000,
            coordinator_fee_percentage: 10,
            total_competition_pool: 900,
            relative_locktime_block_delta: None,
        });
        assert_eq!(
            competition.funding_reservation_deadline(now).unwrap(),
            (signing_date + time::Duration::days(1)).unix_timestamp() as u64,
        );
        let restarted_at = signing_date + time::Duration::days(2);
        assert_eq!(
            competition
                .funding_reservation_deadline(restarted_at)
                .unwrap(),
            (restarted_at + time::Duration::days(1)).unix_timestamp() as u64,
        );
    }
}

#[cfg(test)]
mod keymeld_authorization_tests {
    use super::*;
    use keymeld_core::{authorization::EnclaveRecipientAuthorization, KeyAggContext, PublicKey};
    use keymeld_sdk::{
        types::{ParticipantRoster, RegistrationAuthorization, SessionAuthorizationManifest},
        AuthorizationCredentials, EnclaveId, SessionCredentials, SessionId, UserCredentials,
    };

    struct Fixture {
        session: DlcKeygenSession,
        roster: SignedRoster,
        keys: BTreeMap<UserId, Vec<u8>>,
        contexts: BTreeMap<UserId, RegistrationContext>,
        coordinator: UserId,
        player: UserId,
    }

    fn fixture() -> Fixture {
        let coordinator = UserId::new_v7();
        let player = UserId::new_v7();
        let session_id = SessionId::new_v7();
        let creator = AuthorizationCredentials::from_secret(&[9; 32]).unwrap();
        let authority = AuthorizationCredentials::from_secret(&[2; 32]).unwrap();
        let credentials = SessionCredentials::from_session_secret(&[3; 32]).unwrap();
        let registrations = BTreeMap::from([
            (
                coordinator.clone(),
                AuthorizationCredentials::from_secret(&[4; 32]).unwrap(),
            ),
            (
                player.clone(),
                AuthorizationCredentials::from_secret(&[5; 32]).unwrap(),
            ),
        ]);
        let participant_keys = BTreeMap::from([
            (
                coordinator.clone(),
                UserCredentials::from_private_key(&[1; 32]).unwrap(),
            ),
            (
                player.clone(),
                UserCredentials::from_private_key(&[7; 32]).unwrap(),
            ),
        ]);
        let keys: BTreeMap<_, _> = participant_keys
            .iter()
            .map(|(id, key)| (id.clone(), key.public_key_bytes()))
            .collect();
        let subsets =
            compute_dlc_subset_definitions(coordinator.clone(), std::slice::from_ref(&player), 1);
        let manifest = keymeld_sdk::SignedSessionManifest::sign(
            SessionAuthorizationManifest {
                keygen_session_id: session_id.clone(),
                coordinator_user_id: coordinator.clone(),
                creator_pubkey: creator.public_key_bytes(),
                signing_pubkey: authority.public_key_bytes(),
                session_public_key: credentials.public_key_bytes(),
                participant_verifiers: registrations
                    .iter()
                    .map(|(id, credential)| (id.clone(), credential.public_key_bytes()))
                    .collect(),
                timeout_secs: 300,
                max_signing_sessions: None,
                encrypted_taproot_tweak: "unused-in-roster-test".into(),
                subset_definitions: subsets
                    .definitions
                    .iter()
                    .map(|subset| keymeld_core::protocol::SubsetDefinition {
                        subset_id: subset.subset_id,
                        participants: subset.participants.clone(),
                    })
                    .collect(),
            },
            &creator.export_secret(),
        )
        .unwrap();
        let enclave = EnclaveId::new(1);
        let enclave_key = AuthorizationCredentials::from_secret(&[8; 32])
            .unwrap()
            .public_key_bytes();
        let recipients = EnclaveRecipientAuthorization::sign(
            &manifest,
            BTreeMap::from([(coordinator.clone(), enclave), (player.clone(), enclave)]),
            BTreeMap::from([(enclave, enclave_key.clone())]),
            &creator.export_secret(),
        )
        .unwrap();
        let contexts: BTreeMap<_, _> = participant_keys
            .iter()
            .map(|(id, key)| {
                (
                    id.clone(),
                    RegistrationContext {
                        keygen_session_id: session_id.clone(),
                        user_id: id.clone(),
                        manifest_hash: manifest.digest().unwrap(),
                        enclave_id: enclave,
                        enclave_key_epoch: 1,
                        public_key: key.public_key_bytes(),
                        auth_pubkey: key
                            .derive_session_auth_pubkey(&session_id.to_string())
                            .unwrap(),
                        require_signing_approval: false,
                    },
                )
            })
            .collect();
        let authorized = contexts
            .iter()
            .map(|(id, context)| {
                let envelope = participant_keys[id]
                    .prepare_registration(context.clone(), &hex::encode(&enclave_key))
                    .unwrap();
                (
                    id.clone(),
                    RegistrationAuthorization::sign(
                        &registrations[id].export_secret(),
                        context.clone(),
                        &envelope,
                    )
                    .unwrap(),
                )
            })
            .collect();
        let mut public_keys = keys
            .values()
            .map(|key| PublicKey::from_slice(key).unwrap())
            .collect::<Vec<_>>();
        public_keys.sort_by_key(|key| key.serialize());
        let aggregate: PublicKey = KeyAggContext::new(public_keys).unwrap().aggregated_pubkey();
        let aggregate = aggregate.serialize().to_vec();
        let roster = SignedRoster::sign(
            ParticipantRoster {
                keygen_session_id: session_id.clone(),
                manifest_hash: manifest.digest().unwrap(),
                participants: keys.clone(),
                registrations: authorized,
                aggregate_public_key: aggregate.clone(),
                subset_aggregate_keys: subsets
                    .definitions
                    .iter()
                    .map(|subset| (subset.subset_id, aggregate.clone()))
                    .collect(),
                subset_definitions: manifest.manifest.subset_definitions.clone(),
                taproot_tweak: keymeld_sdk::TaprootTweak::None,
            },
            &[8; 32],
        )
        .unwrap();
        Fixture {
            session: DlcKeygenSession {
                session_id,
                session_secret: [3; 32],
                authorization_manifest: manifest,
                recipient_authorization: recipients,
                signing_authority: authority,
                registration_authorities: registrations,
                aggregate_key: aggregate,
                outcome_subset_ids: subsets.outcome_subset_ids,
            },
            roster,
            keys,
            contexts,
            coordinator,
            player,
        }
    }

    #[test]
    fn funding_roster_accepts_only_the_paid_entry_keys_and_contexts() {
        let mut fixture = fixture();
        verify_accepted_keymeld_roster(
            &fixture.session,
            &fixture.roster,
            &fixture.keys,
            &fixture.contexts,
            fixture.coordinator.clone(),
            1,
        )
        .unwrap();
        fixture.keys.insert(
            fixture.player.clone(),
            UserCredentials::from_private_key(&[10; 32])
                .unwrap()
                .public_key_bytes(),
        );
        assert!(verify_accepted_keymeld_roster(
            &fixture.session,
            &fixture.roster,
            &fixture.keys,
            &fixture.contexts,
            fixture.coordinator.clone(),
            1
        )
        .is_err());
        fixture.keys = fixture.roster.roster.participants.clone();
        fixture
            .contexts
            .get_mut(&fixture.player)
            .unwrap()
            .enclave_key_epoch += 1;
        assert!(verify_accepted_keymeld_roster(
            &fixture.session,
            &fixture.roster,
            &fixture.keys,
            &fixture.contexts,
            fixture.coordinator,
            1
        )
        .is_err());
    }

    #[test]
    fn funding_roster_rejects_substituted_aggregates_and_outcome_assignments() {
        let mut fixture = fixture();
        fixture.roster.roster.aggregate_public_key = UserCredentials::from_private_key(&[10; 32])
            .unwrap()
            .public_key_bytes();
        fixture.roster = SignedRoster::sign(fixture.roster.roster, &[8; 32]).unwrap();
        assert!(verify_accepted_keymeld_roster(
            &fixture.session,
            &fixture.roster,
            &fixture.keys,
            &fixture.contexts,
            fixture.coordinator,
            1
        )
        .is_err());
        let mut fixture = self::fixture();
        fixture.session.outcome_subset_ids.insert(0, Uuid::now_v7());
        assert!(verify_accepted_keymeld_roster(
            &fixture.session,
            &fixture.roster,
            &fixture.keys,
            &fixture.contexts,
            fixture.coordinator,
            1
        )
        .is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dlctix::{attestation_locking_point, hashlock, MarketMaker};
    use rand::RngCore;

    #[test]
    fn entry_key_validation_normalizes_aliases_before_uniqueness_checks() {
        let key = Scalar::from_slice(&[1; 32]).unwrap().base_point_mul();
        let mut entry = AddEntry {
            id: Uuid::now_v7(),
            ticket_id: Uuid::now_v7(),
            event_id: Uuid::now_v7(),
            ephemeral_pubkey: hex::encode(key.serialize()).to_uppercase(),
            payout_hash: format!("0x{}", hex::encode([0xab; 32]).to_uppercase()),
            expected_observations: vec![],
            encrypted_keymeld_private_key: None,
            keymeld_auth_pubkey: None,
            keymeld_registration_context: None,
        };
        validate_entry_keys(&mut entry).unwrap();
        assert_eq!(entry.ephemeral_pubkey, hex::encode(key.serialize()));
        assert_eq!(entry.payout_hash, hex::encode([0xab; 32]));
        entry.payout_hash = "00".into();
        assert!(validate_entry_keys(&mut entry).is_err());
    }

    fn params() -> ContractParameters {
        let mut rng = rand::rng();
        let player = |rng: &mut rand::rngs::ThreadRng| Player {
            pubkey: Scalar::random(rng).base_point_mul(),
            ticket_hash: hashlock::sha256(&hashlock::preimage_random(rng)),
            payout_hash: hashlock::sha256(&hashlock::preimage_random(rng)),
        };
        let oracle = Scalar::random(&mut rng).base_point_mul();
        let nonce = Scalar::random(&mut rng).base_point_mul();
        ContractParameters {
            market_maker: MarketMaker {
                pubkey: Scalar::random(&mut rng).base_point_mul(),
            },
            players: vec![player(&mut rng), player(&mut rng)],
            event: dlctix::EventLockingConditions {
                locking_points: vec![
                    attestation_locking_point(oracle, nonce, b"a"),
                    attestation_locking_point(oracle, nonce, b"b"),
                ],
                expiry: None,
            },
            outcome_payouts: BTreeMap::from([
                (Outcome::Attestation(0), PayoutWeights::from([(0, 1)])),
                (Outcome::Attestation(1), PayoutWeights::from([(1, 1)])),
            ]),
            fee_rate: dlctix::bitcoin::FeeRate::from_sat_per_vb_u32(1),
            funding_value: Amount::from_sat(100_000),
            relative_locktime_block_delta: 72,
        }
    }

    fn first_draw(outpoint: &OutPoint, key: Scalar, params: &ContractParameters) -> u64 {
        create_deterministic_rng(outpoint, key, params)
            .unwrap()
            .next_u64()
    }

    #[test]
    fn nonce_seed_is_stable_for_the_same_contract() {
        let key = Scalar::random(&mut rand::rng());
        let outpoint = OutPoint::null();
        let params = params();

        assert_eq!(
            first_draw(&outpoint, key, &params),
            first_draw(&outpoint, key, &params)
        );
    }

    #[test]
    fn nonce_seed_changes_with_contract_params() {
        let key = Scalar::random(&mut rand::rng());
        let outpoint = OutPoint::null();
        let params = params();
        let mut changed = params.clone();
        changed.fee_rate = dlctix::bitcoin::FeeRate::from_sat_per_vb_u32(2);

        assert_ne!(
            first_draw(&outpoint, key, &params),
            first_draw(&outpoint, key, &changed)
        );
    }
}
