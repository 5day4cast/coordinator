use super::{
    AddEntry, CompetitionError, CompetitionState, CompetitionStore, SearchBy, Ticket, UserEntry,
};
use crate::{
    bitcoin_client::Bitcoin,
    domain::{Competition, CreateEvent, Error},
    get_key,
    oracle_client::{Event, Oracle},
    Ln, OracleError,
};
use anyhow::anyhow;
use dlctix::{
    bitcoin::{
        absolute::LockTime,
        hex::DisplayHex,
        sighash::{Prevouts, SighashCache},
        transaction, Amount, FeeRate, OutPoint, ScriptBuf, TapSighashType, Transaction, TxIn,
        TxOut,
    },
    convert_xonly_key, hashlock,
    musig2::{
        self,
        secp256k1::{Keypair, PublicKey, Scalar, Secp256k1, SecretKey},
        AggNonce, CompactSignature, PartialSignature, PubNonce,
    },
    secp::Point,
    ContractParameters, NonceSharingRound, Outcome, PayoutWeights, Player, PlayerIndex, SigMap,
    SigningSession, TicketedDLC,
};
use futures::TryFutureExt;
use itertools::Itertools;
use log::{debug, error, info};
use rand::SeedableRng;
use std::sync::Arc;
use std::time::Duration;
use std::{collections::BTreeMap, str::FromStr};
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
    private_key: SecretKey,
    public_key: PublicKey,
    relative_locktime_block_delta: u16,
}

impl Coordinator {
    pub async fn new(
        oracle_client: Arc<dyn Oracle>,
        competition_store: CompetitionStore,
        bitcoin: Arc<dyn Bitcoin>,
        ln: Arc<dyn Ln>,
        private_key_file_path: &str,
        relative_locktime_block_delta: u16,
    ) -> Result<Self, anyhow::Error> {
        let secret_key: SecretKey = get_key(private_key_file_path)?;
        let secp = Secp256k1::new();
        let public_key = secret_key.public_key(&secp);
        let coordinator = Self {
            oracle_client,
            competition_store: Arc::new(competition_store),
            bitcoin,
            ln,
            private_key: secret_key,
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

    pub fn keypair(&self) -> Keypair {
        let secp = Secp256k1::new();
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
        let competitions: Vec<Competition> = self.competition_store.get_competitions().await?;
        let mut updated_competitions: Vec<Competition> = vec![];

        for mut competition in competitions {
            if competition.skip_competition() {
                continue;
            }

            // Check for timeouts first
            if competition.is_expired() {
                competition.cancelled_at = Some(OffsetDateTime::now_utc());
                updated_competitions.push(competition);
                continue;
            }

            // Process based on current state
            match competition.get_state() {
                CompetitionState::Created => {
                    // Wait for more entries
                    continue;
                }
                CompetitionState::EntriesCollected => {
                    // Create contract
                    match self.create_transactions(competition.clone()).await {
                        Ok(updated) => updated_competitions.push(updated),
                        Err(e) => {
                            competition
                                .errors
                                .push(CompetitionError::FailedCreateTransaction(e.to_string()));
                            competition.failed_at = Some(OffsetDateTime::now_utc());
                            updated_competitions.push(competition);
                        }
                    }
                }
                CompetitionState::NoncesCollected => {
                    // Generate aggregate nonces
                    match self.generate_aggregate_nonces(competition.clone()).await {
                        Ok(updated) => updated_competitions.push(updated),
                        Err(e) => {
                            competition
                                .errors
                                .push(CompetitionError::FailedNonceAggregation(e.to_string()));
                            competition.failed_at = Some(OffsetDateTime::now_utc());
                            updated_competitions.push(competition);
                        }
                    }
                }
                CompetitionState::PartialSignaturesCollected => {
                    // Publish transactions
                    match self.publish_transactions(competition.clone()).await {
                        Ok(updated) => updated_competitions.push(updated),
                        Err(e) => {
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
            self.competition_store
                .update_competitions(updated_competitions)
                .await?;
        }

        Ok(())
    }

    async fn generate_aggregate_nonces(
        &self,
        mut competition: Competition,
    ) -> Result<Competition, anyhow::Error> {
        let Some(contract_parameters) = &competition.contract_parameters else {
            return Err(anyhow!("contract parameters don't exist"));
        };

        let Some(funding_transaction) = &competition.funding_transaction else {
            return Err(anyhow!("funding transaction doesn't exist"));
        };

        let ticketed_dlc = TicketedDLC::new(
            contract_parameters.to_owned(),
            funding_transaction.to_owned(),
        )?;

        let mut rng = rand::rngs::StdRng::from_entropy();
        let signing_session = SigningSession::<NonceSharingRound>::new(
            ticketed_dlc,
            &mut rng,
            Scalar::from(self.private_key),
        )?;

        let received_nonces = self.get_received_nonces(competition.id).await?;
        let aggregate =
            signing_session.aggregate_nonces_and_compute_partial_signatures(received_nonces)?;
        competition.partial_signatures = Some(aggregate.our_partial_signatures().to_owned());
        competition.aggregated_nonces = Some(aggregate.aggregated_nonces().to_owned());

        Ok(competition)
    }

    pub async fn create_transactions(
        &self,
        mut competition: Competition,
    ) -> Result<Competition, anyhow::Error> {
        debug!("creating dlc contract for competition: {}", competition.id);
        let mut entries = self
            .competition_store
            .get_competition_entries(competition.id)
            .await?;
        debug!("competition has {} entries", entries.len());
        let players =
            generate_players(&entries).map_err(|e| anyhow!("Failed to generate players: {}", e))?;
        debug!("generated players for each entry");

        let outcome_payouts = generate_payouts(&competition, &mut entries, &players)
            .map_err(|e| anyhow!("Failed to generate payouts: {}", e))?;
        debug!("generated outcome payouts");

        let contract_amount_sats = competition.total_competition_pool;
        let fee_rates = self.bitcoin.get_estimated_fee_rates().await?;

        // TODO: make this configurable from the admin screen
        let rate_confirm_within_2_blocks = fee_rates[&1_u16].ceil() as u64;

        let contract_params = ContractParameters {
            market_maker: dlctix::MarketMaker {
                pubkey: Point::from(self.public_key),
            },
            players,
            event: competition.event_announcement.clone(),
            outcome_payouts,
            fee_rate: FeeRate::from_sat_per_vb_unchecked(rate_confirm_within_2_blocks),
            funding_value: Amount::from_sat(contract_amount_sats),
            relative_locktime_block_delta: self.relative_locktime_block_delta,
        };
        competition.contract_parameters = Some(contract_params.clone());
        debug!("contract params have been built");

        let wallet_outpoint: OutPoint = self
            .bitcoin
            .get_spendable_utxo(contract_amount_sats)
            .await?;
        debug!("spendable utxo found");

        // Prepare a funding transaction (value should be total value of competition money pool)
        let tx_out = TxOut {
            value: Amount::from_sat(contract_amount_sats),
            script_pubkey: ScriptBuf::new(),
        };

        let funding_tx = signed_funding_tx(
            Scalar::from(self.private_key),
            contract_params.funding_output().unwrap(),
            wallet_outpoint,
            &tx_out,
        );
        debug!("signed funding tx");

        //At this point, store the funding_outpoint with the event in the DB
        let funding_outpoint = OutPoint {
            txid: funding_tx.compute_txid(),
            vout: 0,
        };

        competition.funding_transaction = Some(funding_outpoint);

        // at this point time to perform musig with the players
        // this needs to be done in an async manner and a background thread waiting for users to sign
        // some thought will need to be done on the best way to do this for the least amount of user interaction
        // even after musig is signed, users then need to additionally pay the coordinator to get the secret allowing them to claim their winnings (maybe after competition completes?)
        // Note: word of warning, the ticketed_dlc may be very large in memory depending on the contract params

        let ticketed_dlc = TicketedDLC::new(contract_params, funding_outpoint)?;
        debug!("built ticketed dlc");

        let mut rng = rand::rngs::StdRng::from_entropy();

        let signing_session = SigningSession::<NonceSharingRound>::new(
            ticketed_dlc,
            &mut rng,
            Scalar::from(self.private_key),
        )?;
        debug!("started musig nonce sharing round");

        competition.public_nonces = Some(signing_session.our_public_nonces().to_owned());
        Ok(competition)
    }

    pub async fn generate_partial_signatures(
        &self,
        mut competition: Competition,
    ) -> Result<Competition, anyhow::Error> {
        let Some(contract_parameters) = &competition.contract_parameters else {
            return Err(anyhow!("contract parameters don't exists, failed publishing competition {} funding transaction", competition.id));
        };
        let Some(funding_transaction) = &competition.funding_transaction else {
            return Err(anyhow!("funding transaction doesn't exists, failed publishing competition {} funding transaction", competition.id));
        };
        let ticketed_dlc = TicketedDLC::new(
            contract_parameters.to_owned(),
            funding_transaction.to_owned(),
        )?;
        let mut rng = rand::rngs::StdRng::from_entropy();

        let signing_session = SigningSession::<NonceSharingRound>::new(
            ticketed_dlc,
            &mut rng,
            Scalar::from(self.private_key),
        )?;

        let received_nonces = self.get_received_nonces(competition.id).await?;

        let coordinator_sessions =
            signing_session.aggregate_nonces_and_compute_partial_signatures(received_nonces)?;

        competition.aggregated_nonces = Some(coordinator_sessions.aggregated_nonces().to_owned());

        Ok(competition)
    }

    pub async fn publish_transactions(
        &self,
        mut competition: Competition,
    ) -> Result<Competition, anyhow::Error> {
        let Some(contract_parameters) = &competition.contract_parameters else {
            return Err(anyhow!("contract parameters don't exists, failed publishing competition {} funding transaction", competition.id));
        };
        let Some(funding_transaction) = &competition.funding_transaction else {
            return Err(anyhow!("funding transaction doesn't exists, failed publishing competition {} funding transaction", competition.id));
        };
        let Some(_) = competition.partial_signatures.clone() else {
            return Err(anyhow!("coordinator partial signatures do not exist, failed publishing competition {} funding transaction", competition.id));
        };

        let ticketed_dlc = TicketedDLC::new(
            contract_parameters.to_owned(),
            funding_transaction.to_owned(),
        )?;
        let mut rng = rand::rngs::StdRng::from_entropy();
        debug!("built ticketed dlc before publishing");

        let signing_session = SigningSession::<NonceSharingRound>::new(
            ticketed_dlc,
            &mut rng,
            Scalar::from(self.private_key),
        )?;

        let received_nonces = self.get_received_nonces(competition.id).await?;
        debug!("got received nonces");

        let coordinator_session =
            signing_session.aggregate_nonces_and_compute_partial_signatures(received_nonces)?;
        debug!("built coordinator session before publishing");

        let partial_sigs_by_sender: BTreeMap<Point, SigMap<PartialSignature>> =
            self.get_partial_sigs_by_sender(competition.id).await?;

        debug!("got partial sigs by sender");
        //Bug here, things aren't mapping up with the setup outcome/payouts
        for (&sender_pubkey, partial_sigs) in &partial_sigs_by_sender {
            coordinator_session.verify_partial_signatures(sender_pubkey, partial_sigs)?;
        }
        debug!("verified all paritial signatures");

        let signed_contract =
            coordinator_session.aggregate_all_signatures(partial_sigs_by_sender)?;
        debug!("signed dlc contract");

        competition.signed_contract = Some(signed_contract);
        competition.signed_at = Some(OffsetDateTime::now_utc());

        self.bitcoin
            .broadcast(funding_transaction.to_string())
            .await?;

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
        if stored_public_key != convert_xonly_key(self.public_key.x_only_public_key().0) {
            return Err(anyhow!(
                "stored_pubkey: {:?} pem_pubkey: {:?}",
                stored_public_key,
                self.public_key()
            ));
        }
        Ok(())
    }

    async fn add_metadata(&self) -> Result<(), anyhow::Error> {
        self.competition_store
            .add_coordinator_metadata(convert_xonly_key(self.public_key.x_only_public_key().0))
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
    ) -> Result<ContractParameters, Error> {
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

        competition.contract_parameters.ok_or_else(|| {
            Error::NotFound(format!(
                "Contract parameters not yet available for competition {}",
                competition_id
            ))
        })
    }

    pub async fn submit_public_nonces(
        &self,
        pubkey: String,
        competition_id: Uuid,
        entry_id: Uuid,
        public_nonces: SigMap<PubNonce>,
    ) -> Result<(), Error> {
        let competition = self
            .competition_store
            .get_competition(competition_id)
            .await?;

        // Verify the competition has contract parameters
        if competition.contract_parameters.is_none() {
            return Err(Error::BadRequest(
                "Contract parameters not yet available".to_string(),
            ));
        }

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

        let entry = entries
            .iter()
            .find(|e| e.id == entry_id)
            .ok_or_else(|| Error::NotFound(format!("Entry {} not found", entry_id)))?;

        // Verify entry hasn't already submitted nonces
        if entry.public_nonces.is_some() {
            return Err(Error::BadRequest(
                "Public nonces already submitted for this entry".to_string(),
            ));
        }

        // Save the public nonces
        self.competition_store
            .add_public_nonces(entry_id, public_nonces)
            .await?;

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
            .await?;

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
        let player = Player {
            pubkey: Point::from_hex(&entry.ephemeral_pubkey)?,
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

fn generate_payouts(
    competition: &Competition,
    entries: &mut Vec<UserEntry>,
    players: &Vec<Player>,
) -> Result<BTreeMap<Outcome, PayoutWeights>, anyhow::Error> {
    // Sort entries by ID (UUID v7) for consistent indexing
    entries.sort_by_key(|entry| entry.id);
    let mut payouts: BTreeMap<Outcome, PayoutWeights> = BTreeMap::new();

    let allowed_scored_ranks = 3; // Will be configurable in future

    // Generate all possible ranking permutations
    let possible_rankings = generate_ranking_permutations(entries.len(), allowed_scored_ranks);

    // For each possible ranking combination, create a payout mapping
    // The index in possible_rankings matches the oracle's attestation index
    for (outcome_index, winner_indices) in possible_rankings.iter().enumerate() {
        let entry_pubkeys = find_winning_entries_pubkeys(entries, winner_indices.to_owned());
        let player_indices = find_player_indices(players, entry_pubkeys)?;

        let mut payout_weights: BTreeMap<PlayerIndex, u64> = BTreeMap::new();
        for (rank, &player_index) in player_indices.iter().enumerate() {
            let weight = allowed_scored_ranks - rank;
            payout_weights.insert(player_index, weight as u64);
        }

        payouts.insert(Outcome::Attestation(outcome_index), payout_weights);
    }

    // Add expiry outcome if configured
    if competition.event_announcement.expiry.is_some() {
        let mut refund_payout: BTreeMap<PlayerIndex, u64> = BTreeMap::new();
        for (player_index, _) in players.iter().enumerate() {
            refund_payout.insert(player_index, 1);
        }
        payouts.insert(Outcome::Expiry, refund_payout);
    }

    Ok(payouts)
}

pub fn generate_ranking_permutations(num_players: usize, rankings: usize) -> Vec<Vec<usize>> {
    (0..num_players).permutations(rankings).collect()
}

fn find_player_indices(
    players: &Vec<Player>,
    entry_ticket_pubkeys: Vec<String>,
) -> Result<Vec<usize>, anyhow::Error> {
    let mut player_indecies = vec![];
    for entry_ticket_pubkey in entry_ticket_pubkeys {
        let pubkey_point = Point::from_hex(&entry_ticket_pubkey)?;

        if let Some(index) = players
            .iter()
            .position(|player| player.pubkey == pubkey_point)
        {
            player_indecies.push(index);
        }
    }
    Ok(player_indecies)
}

fn find_winning_entries_pubkeys(
    entries: &Vec<UserEntry>,
    winning_entry_indices: Vec<usize>,
) -> Vec<String> {
    entries
        .iter()
        .enumerate()
        .filter(|(i, _)| winning_entry_indices.contains(i))
        .map(|(_, entry)| entry.ephemeral_pubkey.clone())
        .collect()
}

fn signed_funding_tx(
    market_maker_seckey: Scalar,
    funding_output: TxOut,
    mm_utxo_outpoint: OutPoint,
    mm_utxo_prevout: &TxOut,
) -> Transaction {
    let mut funding_tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: mm_utxo_outpoint,
            ..TxIn::default()
        }],
        output: vec![funding_output],
    };

    let funding_tx_sighash = SighashCache::new(&funding_tx)
        .taproot_key_spend_signature_hash(
            0,
            &Prevouts::All(&[mm_utxo_prevout]),
            TapSighashType::Default,
        )
        .unwrap();

    let signature: CompactSignature =
        musig2::deterministic::sign_solo(market_maker_seckey, funding_tx_sighash);

    funding_tx.input[0].witness.push(signature.serialize());
    funding_tx
}
