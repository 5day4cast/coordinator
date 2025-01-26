use super::{
    AddEntry, CompetitionError, CompetitionState, CompetitionStore, FundedContract, SearchBy,
    Ticket, UserEntry,
};
use crate::{
    bitcoin_client::Bitcoin,
    domain::{Competition, CreateEvent, Error},
    oracle_client::{Event, Oracle},
    Ln, OracleError,
};
use anyhow::anyhow;
use bdk_wallet::{
    bitcoin::{
        absolute::LockTime, hex::DisplayHex, transaction::Version, Amount, FeeRate, OutPoint, Psbt,
        ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
    },
    LocalOutput, SignOptions,
};
use dlctix::{
    convert_xonly_key, hashlock,
    musig2::{
        secp256k1::{
            Keypair as Musig2Kepair, PublicKey, Scalar as Musig2Scalar, Secp256k1 as Musig2Secp256,
            SecretKey as Musig2SecretKey,
        },
        AggNonce, PartialSignature, PubNonce,
    },
    secp::Point,
    ContractParameters, NonceSharingRound, Outcome, PayoutWeights, Player, PlayerIndex, SigMap,
    SigningSession, TicketedDLC,
};
use futures::TryFutureExt;
use itertools::Itertools;
use log::{debug, error, info};
use rand::SeedableRng;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
    competition_store: Arc<CompetitionStore>,
    bitcoin: Arc<dyn Bitcoin>,
    ln: Arc<dyn Ln>,
    private_key: Musig2SecretKey,
    public_key: PublicKey,
    relative_locktime_block_delta: u16,
}

impl Coordinator {
    pub async fn new(
        oracle_client: Arc<dyn Oracle>,
        competition_store: CompetitionStore,
        bitcoin: Arc<dyn Bitcoin>,
        ln: Arc<dyn Ln>,
        relative_locktime_block_delta: u16,
    ) -> Result<Self, anyhow::Error> {
        let private_key = bitcoin.get_derived_private_key().await?;
        let secp = Musig2Secp256::new();
        let public_key = private_key.public_key(&secp);

        let coordinator = Self {
            oracle_client,
            competition_store: Arc::new(competition_store),
            bitcoin,
            ln,
            private_key,
            public_key,
            relative_locktime_block_delta,
        };
        coordinator.validate_coordinator_metadata().await?;
        Ok(coordinator)
    }

    pub fn public_key(&self) -> String {
        let key = self.public_key.x_only_public_key().0.serialize();
        hex::encode(key)
    }

    pub fn keypair(&self) -> Musig2Kepair {
        let secp = Musig2Secp256::new();
        self.private_key.keypair(&secp)
    }

    pub async fn ping(&self) -> Result<(), Error> {
        self.competition_store.ping().await.map_err(Error::DbError)
    }

    /*
    competition events
    1) users creating entries and pays a hodl invoice (have them on the hook to complete the signing competition pending)
    2) create competition contract (have all users entering in) -- loop checking for user entries matching total or time window exhausted and competition is cancelled (cancel hodl invoice if contract is never made)
    3) wait for all entry nonces then generate aggregate nonces for competition musig session (cancel if not completed within window and close hodl invoices)
    4) wait for all entry partial signatures for competition musig (cancel if not completed within window and close hodl invoices)
        - sign contract with all signatures
        - broadcast funding transaction
        - complete hodl invoices so users has their ticket
        -- at this time, until payouts, users can ask for a ticket
    5) competition running (in time window for when observation runs)
    6) competition window closed (observation window done, waiting for data to be finalized) -- loop checking for oracle attestation
    7) competition results finalized/signed by oracle
        - here payouts can occur

        3 code paths based on competition status, need to skip code path if already been down it
    */

    pub async fn competition_handler(&self) -> Result<(), anyhow::Error> {
        let competitions: Vec<Competition> = self.competition_store.get_competitions(true).await?;
        let mut updated_competitions: Vec<Competition> = vec![];
        info!("running competition handler");
        for mut competition in competitions {
            //TODO: change to propert retry logic
            competition.failed_at = None;
            competition.errors = vec![];
            if competition.skip_competition() {
                info!(
                    "skipping process competition {} {:?}",
                    competition.id,
                    competition.get_state()
                );
                continue;
            }

            // Check for timeouts first
            if competition.is_expired() {
                competition.cancelled_at = Some(OffsetDateTime::now_utc());
                updated_competitions.push(competition.clone());
                info!(
                    "skipping process competition {} {:?}",
                    competition.id,
                    competition.get_state()
                );
                continue;
            }
            info!(
                "competition {} {:?} {:?}",
                competition.id,
                competition.get_state(),
                competition
            );
            match competition.get_state() {
                CompetitionState::Created => {
                    debug!(
                        "competition {}, waiting for more entries: {}/{}",
                        competition.id,
                        competition.total_entries,
                        competition.total_allowed_entries
                    );
                    continue;
                }
                CompetitionState::EntriesCollected => {
                    match self.create_transactions(competition.clone()).await {
                        Ok(updated) => updated_competitions.push(updated),
                        Err(e) => {
                            error!(
                                "competition {} failed to generate contract params: {}",
                                competition.id, e
                            );
                            competition
                                .errors
                                .push(CompetitionError::FailedCreateTransaction(e.to_string()));
                            competition.failed_at = Some(OffsetDateTime::now_utc());
                            updated_competitions.push(competition);
                        }
                    }
                }
                CompetitionState::NoncesCollected => {
                    match self
                        .generate_aggregate_nonces_and_coord_partial_signatures(competition.clone())
                        .await
                    {
                        Ok(updated) => updated_competitions.push(updated),
                        Err(e) => {
                            error!(
                                "competition {} failed to generate aggregate nonce: {}",
                                competition.id, e
                            );
                            competition
                                .errors
                                .push(CompetitionError::FailedNonceAggregation(e.to_string()));
                            competition.failed_at = Some(OffsetDateTime::now_utc());
                            updated_competitions.push(competition);
                        }
                    }
                }
                CompetitionState::PartialSignaturesCollected => {
                    match self.publish_transactions(competition.clone()).await {
                        Ok(updated) => updated_competitions.push(updated),
                        Err(e) => {
                            error!("competition {} failed to broadcast: {}", competition.id, e);
                            competition
                                .errors
                                .push(CompetitionError::FailedBroadcast(e.to_string()));
                            competition.failed_at = Some(OffsetDateTime::now_utc());
                            updated_competitions.push(competition);
                        }
                    }
                }
                _ => continue,
            }
        }

        if !updated_competitions.is_empty() {
            info!("updating contract");

            self.competition_store
                .update_competitions(updated_competitions)
                .await?;
        }

        Ok(())
    }

    pub async fn create_transactions(
        &self,
        mut competition: Competition,
    ) -> Result<Competition, anyhow::Error> {
        debug!("Creating transactions for competition: {}", competition.id);
        let mut entries = self
            .competition_store
            .get_competition_entries(competition.id)
            .await?;
        debug!("Competition entries {:?}", entries);

        let players = generate_players(&entries)?;
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

        let contract_amount_sats = competition.total_competition_pool;
        let fee_rates = self.bitcoin.get_estimated_fee_rates().await?;
        info!("fee rates: {:?}", fee_rates);
        // TODO: make this configurable from the admin screen
        let rate_confirm_within_2_blocks = if fee_rates.is_empty() {
            1 as u64
        } else {
            fee_rates[&1_u16].ceil() as u64
        };

        let fee_rate = FeeRate::from_sat_per_vb_unchecked(rate_confirm_within_2_blocks);
        let estimated_tx_size = 200; // Approximate size for taproot tx with 1 input, 2 outputs
        let estimated_fee = fee_rate
            .fee_vb(estimated_tx_size)
            .ok_or_else(|| anyhow!("Failed to calculate estimated fee"))?;
        debug!("Estimated fee: {}", estimated_fee);

        // Get UTXO first to ensure we have enough funds
        let utxo: LocalOutput = self
            .bitcoin
            .get_spendable_utxo(contract_amount_sats + estimated_fee.to_sat())
            .await?;

        let contract_params = ContractParameters {
            market_maker: dlctix::MarketMaker {
                pubkey: Point::from(self.public_key),
            },
            players,
            event: competition.event_announcement.clone(),
            outcome_payouts,
            fee_rate,
            funding_value: Amount::from_sat(contract_amount_sats),
            relative_locktime_block_delta: self.relative_locktime_block_delta,
        };
        competition.contract_parameters = Some(contract_params.clone());
        debug!("contract params have been built");

        let change_amount = utxo
            .txout
            .value
            .to_sat()
            .checked_sub(contract_amount_sats)
            .and_then(|amt| amt.checked_sub(estimated_fee.to_sat()))
            .ok_or_else(|| anyhow!("UTXO value insufficient for contract amount plus fees"))?;

        let utxo: LocalOutput = self
            .bitcoin
            .get_spendable_utxo(contract_amount_sats)
            .await?;
        debug!("Spendable utxo found");
        debug!("UTXO value: {}", utxo.txout.value);

        debug!("Contract amount needed: {}", contract_amount_sats);

        let mut outputs = vec![contract_params.funding_output().unwrap()];
        // Dust threshold
        if change_amount > 1000 {
            let change_address = self.bitcoin.get_next_address().await?;
            outputs.push(TxOut {
                value: Amount::from_sat(change_amount),
                script_pubkey: change_address.script_pubkey(),
            });
        }
        let funding_tx = signed_funding_tx(self.bitcoin.clone(), outputs, utxo).await?;

        debug!("signed funding tx: {:?}", funding_tx);

        //At this point, store the funding_outpoint with the event in the DB
        let funding_outpoint = OutPoint {
            txid: funding_tx.compute_txid(),
            vout: 0,
        };

        competition.funding_transaction = Some(funding_tx);
        competition.funding_outpoint = Some(funding_outpoint);
        // at this point time to perform musig with the players
        // this needs to be done in an async manner and a background thread waiting for users to sign
        // some thought will need to be done on the best way to do this for the least amount of user interaction
        // even after musig is signed, users then need to additionally pay the coordinator to get the secret allowing them to claim their winnings (maybe after competition completes?)
        // Note: word of warning, the ticketed_dlc may be very large in memory depending on the contract params

        let ticketed_dlc = TicketedDLC::new(contract_params, funding_outpoint)?;
        debug!("built ticketed dlc");

        let seed = self.private_key.secret_bytes();
        let mut rng = rand::rngs::StdRng::from_seed(seed);

        let signing_session = SigningSession::<NonceSharingRound>::new(
            ticketed_dlc,
            &mut rng,
            Musig2Scalar::from(self.private_key),
        )?;
        debug!("started musig nonce sharing round");

        competition.public_nonces = Some(signing_session.our_public_nonces().to_owned());
        Ok(competition)
    }

    pub async fn generate_aggregate_nonces_and_coord_partial_signatures(
        &self,
        mut competition: Competition,
    ) -> Result<Competition, anyhow::Error> {
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

        let seed = self.private_key.secret_bytes();
        let mut rng = rand::rngs::StdRng::from_seed(seed);

        let signing_session = SigningSession::<NonceSharingRound>::new(
            ticketed_dlc,
            &mut rng,
            Musig2Scalar::from(self.private_key),
        )?;

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
        competition.aggregated_nonces = Some(coordinator_sessions.aggregated_nonces().to_owned());
        competition.partial_signatures =
            Some(coordinator_sessions.our_partial_signatures().to_owned());

        Ok(competition)
    }

    pub async fn publish_transactions(
        &self,
        mut competition: Competition,
    ) -> Result<Competition, anyhow::Error> {
        let Some(contract_parameters) = &competition.contract_parameters else {
            return Err(anyhow!("contract parameters don't exists, failed publishing competition {} funding transaction", competition.id));
        };
        let Some(funding_outpoint) = &competition.funding_outpoint else {
            return Err(anyhow!("funding outpoint doesn't exists, failed publishing competition {} funding transaction", competition.id));
        };
        let Some(funding_transaction) = &competition.funding_transaction else {
            return Err(anyhow!("funding transaction doesn't exists, failed publishing competition {} funding transaction", competition.id));
        };
        let Some(coordinator_partial_sigantures) = competition.partial_signatures.clone() else {
            return Err(anyhow!("coordinator partial signatures do not exist, failed publishing competition {} funding transaction", competition.id));
        };

        let Some(coordinator_nonces) = competition.public_nonces.as_ref() else {
            return Err(anyhow!("coordinator nonces missing"));
        };

        let ticketed_dlc =
            TicketedDLC::new(contract_parameters.to_owned(), funding_outpoint.to_owned())?;

        let seed = self.private_key.secret_bytes();
        let mut rng = rand::rngs::StdRng::from_seed(seed);
        debug!("Built ticketed dlc before publishing");

        let signing_session = SigningSession::<NonceSharingRound>::new(
            ticketed_dlc,
            &mut rng,
            Musig2Scalar::from(self.private_key),
        )?;

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

        let partial_sigs_by_sender: BTreeMap<Point, SigMap<PartialSignature>> =
            self.get_partial_sigs_by_sender(competition.id).await?;

        for (sender_pubkey, partial_sigs) in &partial_sigs_by_sender {
            match coordinator_session.verify_partial_signatures(*sender_pubkey, &partial_sigs) {
                Ok(_) => debug!(
                    "✓ Signature verification succeeded for player {}",
                    sender_pubkey
                ),
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

        debug!("Verified all paritial signatures");

        let signed_contract =
            coordinator_session.aggregate_all_signatures(partial_sigs_by_sender)?;
        debug!("Signed dlc contract");

        competition.signed_contract = Some(signed_contract);
        competition.signed_at = Some(OffsetDateTime::now_utc());

        debug!(
            "Broadcasting funding transaction: {:?}",
            funding_transaction
        );
        self.bitcoin.broadcast(funding_transaction).await?;

        competition.funding_broadcasted_at = Some(OffsetDateTime::now_utc());

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
            .get_competition_entries(competition_id)
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
    pub async fn get_partial_sigs_by_sender(
        &self,
        competition_id: Uuid,
    ) -> Result<BTreeMap<Point, SigMap<PartialSignature>>, anyhow::Error> {
        // Get all entries for this competition
        let entries = self
            .competition_store
            .get_competition_entries(competition_id)
            .await?;

        let entry_count = entries.len();
        let mut sigs_map: BTreeMap<Point, SigMap<PartialSignature>> = BTreeMap::new();

        for entry in entries {
            // Skip entries that haven't submitted partial signatures
            let Some(partial_signatures) = entry.partial_signatures else {
                continue;
            };

            // Verify entry has been signed (has a signed_at timestamp)
            if entry.signed_at.is_none() {
                continue;
            }

            // Convert ephemeral pubkey string to Point
            let pubkey = Point::from_hex(&entry.ephemeral_pubkey)
                .map_err(|e| anyhow!("Invalid ephemeral pubkey for entry {}: {}", entry.id, e))?;

            sigs_map.insert(pubkey, partial_signatures);
        }

        // Verify we have signatures from all entries
        if sigs_map.len() != entry_count {
            return Err(anyhow!(
                "Missing partial signatures from some entries. Expected {}, got {}",
                entry_count,
                sigs_map.len()
            ));
        }

        Ok(sigs_map)
    }

    pub async fn validate_coordinator_metadata(&self) -> Result<(), anyhow::Error> {
        let stored_public_key = match self.competition_store.get_stored_public_key().await {
            Ok(key) => key,
            Err(duckdb::Error::QueryReturnedNoRows) => {
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
            .add_coordinator_metadata(bitcoin_key)
            .await
            .map_err(|e| anyhow!("failed to add coordinator metadata: {}", e))
    }

    // Becareful with these two operations, there's a possibility here of a
    // competition being added to the oracle but never saved to our local DB (low, but possible)
    pub async fn create_competition(
        &self,
        create_event: CreateEvent,
    ) -> Result<Competition, Error> {
        let oracle_event: Event = match self
            .oracle_client
            .create_event(create_event.clone().into())
            .await
        {
            Ok(event) => Ok(event),
            Err(OracleError::NotFound(e)) => Err(Error::NotFound(e)),
            Err(OracleError::BadRequest(e)) => Err(Error::BadRequest(e)),
            Err(e) => Err(Error::OracleFailed(e)),
        }?;

        let competition = Competition::new(&create_event, &oracle_event);

        let competition = self
            .competition_store
            .add_competition(competition)
            .map_err(|e| {
                error!(
                    "competition added to oracle, but failed to be saved: competition_id {} {:?}",
                    create_event.id, e
                );
                Error::DbError(e)
            })
            .await?;

        Ok(competition)
    }

    pub async fn get_competitions(&self) -> Result<Vec<Competition>, Error> {
        self.competition_store
            .get_competitions(false)
            .map_err(|e| {
                error!("failed to get competitions: {:?}", e);
                Error::DbError(e)
            })
            .await
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

    async fn create_ticket(&self) -> Result<Ticket, Error> {
        let mut rng = rand::thread_rng();
        let ticket_preimage = hashlock::preimage_random(&mut rng);
        let ticket_hash = hashlock::sha256(&ticket_preimage);
        //TODO: encrypt preimage hex with market maker key
        let ticket_preimage_encrypt = ticket_preimage.to_lower_hex_string();
        Ok(Ticket {
            hash: ticket_hash.to_lower_hex_string(),
            preimage: ticket_preimage_encrypt,
        })
    }

    // Be-careful with these two operations, there's a possibility here of an
    // entry being added to the oracle but never saved to our local DB (low, but possible)
    pub async fn add_entry(&self, pubkey: String, entry: AddEntry) -> Result<UserEntry, Error> {
        info!("add_entry: {:?}", entry);
        let ticket = self.create_ticket().await?;
        match self.oracle_client.submit_entry(entry.clone().into()).await {
            Ok(_) => Ok(()),
            Err(OracleError::NotFound(e)) => Err(Error::NotFound(e)),
            Err(OracleError::BadRequest(e)) => Err(Error::BadRequest(e)),
            Err(e) => Err(Error::OracleFailed(e)),
        }?;
        let user_entry = self
            .competition_store
            .add_entry(
                entry.clone().into_user_entry(pubkey),
                ticket.hash,
                ticket.preimage,
            )
            .map_err(|e| {
                error!(
                    "entry added to oracle, but failed to be saved: entry_id {}, event_id {} {:?}",
                    entry.id, entry.event_id, e
                );
                Error::DbError(e)
            })
            .await?;

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

        Ok(FundedContract {
            contract_params: contract,
            funding_outpoint,
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

    pub async fn submit_partial_signatures(
        &self,
        pubkey: String,
        competition_id: Uuid,
        entry_id: Uuid,
        partial_sigs: SigMap<PartialSignature>,
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

        if partial_sigs.is_mirror(&comp_partial_signatures) {
            return Err(Error::BadRequest(format!(
                "parital signatures do not match competitions: {}",
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
            .add_partial_signatures(entry_id, partial_sigs)
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
}

fn generate_players(entries: &Vec<UserEntry>) -> Result<Vec<Player>, anyhow::Error> {
    let mut players = vec![];
    for entry in entries {
        let ticket_hash = entry
            .ticket_hash
            .as_ref()
            .ok_or_else(|| anyhow!("ticket_hash is missing for entry: {}", entry.id))?;

        let payout_hash = entry.payout_hash.as_str();
        debug!("player_pubkey: {}", entry.ephemeral_pubkey);

        let pubkey = Point::from_hex(&entry.ephemeral_pubkey)
            .map_err(|e| anyhow!("failed to get point: {}", e))?;
        let player = Player {
            pubkey,
            // coordinator generates this preimage and only shares after player has paid for a ticket
            ticket_hash: string_to_byte_array(ticket_hash),
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

    let possible_rankings =
        generate_ranking_permutations(entries.len(), competition.number_of_places_win);
    debug!("Generated {} possible rankings", possible_rankings.len());

    for (outcome_index, winner_indices) in possible_rankings.iter().enumerate() {
        debug!(
            "Processing outcome {} with winner indices: {:?}",
            outcome_index, winner_indices
        );

        let entry_pubkeys = find_winning_entries_pubkeys(entries, winner_indices.to_owned());
        debug!("Winner pubkeys: {:?}", entry_pubkeys);

        let player_indices = find_player_indices(players, entry_pubkeys)?;
        debug!("Mapped to player indices: {:?}", player_indices);

        if player_indices.len() != competition.number_of_places_win {
            return Err(anyhow!(
                "Incorrect number of winners for outcome {}",
                outcome_index
            ));
        }

        let percentage_weights = get_percentage_weights(competition.number_of_places_win);
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
    (0..num_players).permutations(rankings).collect()
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
    outputs: Vec<TxOut>,
    utxo: LocalOutput,
) -> Result<Transaction, anyhow::Error> {
    // Create unsigned transaction first
    let funding_tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: utxo.outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::default(),
        }],
        output: outputs,
    };

    // Create PSBT from unsigned transaction
    let mut psbt = Psbt::from_unsigned_tx(funding_tx)?;

    // Add input data
    psbt.inputs[0].witness_utxo = Some(utxo.txout.clone());

    let finalized = bitcoin_client
        .sign_psbt(&mut psbt, SignOptions::default())
        .await?;
    if !finalized {
        return Err(anyhow!("Failed to sign PSBT"));
    }

    let final_tx = psbt.extract_tx()?;

    debug!("Successfully signed funding transaction");
    debug!("Txid: {}", final_tx.compute_txid());

    Ok(final_tx)
}
