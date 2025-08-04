use anyhow::anyhow;
use async_trait::async_trait;
use bdk_wallet::{
    bitcoin::{Amount, FeeRate, Network, OutPoint, Psbt, ScriptBuf, Txid},
    LocalOutput, SignOptions,
};
use client_validator::{
    NostrClientCore, SignerType, TaprootWalletCore, TaprootWalletCoreBuilder, WalletError,
};
use dlctix::{
    attestation_locking_point,
    musig2::secp256k1::{PublicKey, Secp256k1, SecretKey},
    secp::Scalar,
    EventLockingConditions,
};
use log::{debug, info};
use mockall::mock;
use server::{
    domain::{generate_ranking_permutations, AddEntry, CreateEvent},
    get_key, AddEventEntries, Bitcoin, ForeignUtxo, Ln, LnClient, Oracle, OracleError as Error,
    OracleEvent, ValueOptions, WeatherChoices,
};
use std::collections::HashMap;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

mock! {
    #[derive(Send, Sync)]
    pub OracleClient { }

    #[async_trait]
    impl Oracle for OracleClient {
        async fn create_event(&self, event: CreateEvent) -> Result<OracleEvent, Error>;
        async fn submit_entries(&self, event_entries: AddEventEntries) -> Result<(), Error>;
        async fn get_event(&self, event_id: &Uuid) -> Result<OracleEvent, Error>;
    }
}

mock! {
    #[derive(Send, Sync)]
    pub BitcoinClient { }

    #[async_trait]
    impl Bitcoin for BitcoinClient {
        fn get_network(&self) -> Network;
        async fn sign_psbt_with_escrow_support(
            &self,
            psbt: &mut Psbt,
            options: SignOptions,
        ) -> Result<bool, anyhow::Error>;
        async fn finalize_psbt_with_escrow_support(
            &self,
            psbt: &mut Psbt,
        ) -> Result<bool, anyhow::Error>;
        async fn build_psbt(
            &self,
            script_pubkey: ScriptBuf,
            amount: Amount,
            fee_rate: FeeRate,
            selected_utxos: Vec<OutPoint>,
            foreign_utxos: Vec<ForeignUtxo>,
        ) -> Result<Psbt, anyhow::Error>;
        async fn get_spendable_utxo(&self, amount_sats: u64) -> Result<bdk_wallet::LocalOutput, anyhow::Error>;
        async fn get_current_height(&self) -> Result<u32, anyhow::Error>;
        async fn get_confirmed_blockchain_time(&self, blocks: usize) -> Result<u64, anyhow::Error>;
        async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error>;
        async fn get_tx_confirmation_height(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error>;
        async fn broadcast(&self, transaction: &bdk_wallet::bitcoin::Transaction) -> Result<(), anyhow::Error>;
        async fn get_next_address(&self) -> Result<bdk_wallet::AddressInfo, anyhow::Error>;
        async fn get_public_key(&self) -> Result<bdk_wallet::bitcoin::PublicKey, anyhow::Error>;
        async fn get_derived_private_key(&self) -> Result<dlctix::secp::Scalar, anyhow::Error>;
        async fn get_raw_transaction(&self, txid: &Txid) -> Result<bdk_wallet::bitcoin::Transaction, anyhow::Error>;
        async fn sign_psbt(
            &self,
            psbt: &mut Psbt,
            sign_options: SignOptions,
        ) -> Result<bool, anyhow::Error>;
        async fn list_utxos(&self) -> Vec<LocalOutput>;
        async fn sync(&self) -> Result<(), anyhow::Error>;
    }
}

mock! {
    #[derive(Send, Sync)]
    pub LnClient { }

    #[async_trait]
    impl Ln for LnClient {
        async fn ping(&self) -> Result<(), anyhow::Error>;
        async fn cancel_hold_invoice(&self, ticket_hash: String) -> Result<(), anyhow::Error>;
        async fn settle_hold_invoice(&self, ticket_preimage: String) -> Result<(), anyhow::Error>;
        async fn lookup_invoice(&self, r_hash: &str) -> Result<server::InvoiceLookupResponse, anyhow::Error>;
        async fn add_hold_invoice(
            &self,
            value: u64,
            expiry_time_secs: u64,
            ticket_hash_hex: String,
            competition_id: Uuid,
            hex_refund_tx: String,
        ) -> Result<server::InvoiceAddResponse, anyhow::Error>;
        async fn add_invoice(
            &self,
            value: u64,
            expiry_time_secs: u64,
            memo: String,
            competition_id: Uuid,
        ) -> Result<server::InvoiceAddResponse, anyhow::Error>;
        async fn create_invoice(
            &self,
            value: u64,
            expiry_time_secs: u64,
        ) -> Result<String, anyhow::Error>;
        async fn send_payment(
            &self,
            payout_payment_request: String,
            amount_sats: u64,
            timeout_seconds: u64,
            fee_limit_sat: u64,
        ) -> Result<(), anyhow::Error>;
    }
}

pub async fn create_test_wallet(nostr_client: &NostrClientCore) -> TaprootWalletCore {
    TaprootWalletCoreBuilder::new()
        .network("regtest".to_string())
        .nostr_client(nostr_client)
        .build()
        .await
        .expect("Failed to create test wallet")
}

#[derive(Clone)]
pub struct TestParticipant {
    pub wallet: TaprootWalletCore,
    pub nostr_pubkey: String,
    pub ticket_id: Uuid,
    pub ln_client: LnClient,
}

impl TestParticipant {
    pub fn new(
        wallet: client_validator::TaprootWalletCore,
        nostr_pubkey: String,
        ticket_id: Uuid,
        ln_client: LnClient,
    ) -> Self {
        Self {
            wallet,
            nostr_pubkey,
            ticket_id,
            ln_client,
        }
    }

    pub async fn get_payout_preimage(
        &self,
        encrypted_preimage: &str,
    ) -> Result<String, anyhow::Error> {
        let preimage = self
            .wallet
            .decrypt_key(&encrypted_preimage, &self.nostr_pubkey)
            .await?;
        Ok(preimage)
    }

    pub async fn get_dlc_private_key(&self, entry_index: u32) -> Result<String, WalletError> {
        let key = self
            .wallet
            .get_encrypted_dlc_private_key(entry_index, &self.nostr_pubkey)
            .await?;
        self.wallet.decrypt_key(&key, &self.nostr_pubkey).await
    }
}

pub async fn create_test_nostr_client() -> NostrClientCore {
    let mut client = NostrClientCore::new();
    client
        .initialize(SignerType::PrivateKey, None)
        .await
        .expect("Failed to initialize test nostr client");
    client
}

pub fn generate_request_create_event(
    num_locations: usize,
    total_allowed_entries: usize,
    number_of_places_win: usize,
) -> CreateEvent {
    let now = OffsetDateTime::now_utc();
    let start_observation_date = now + Duration::hours(6);
    let end_observation_date = now + Duration::hours(30);

    let signing_date = end_observation_date + Duration::hours(3); // Signing happens after observation

    CreateEvent {
        id: Uuid::now_v7(),
        signing_date,
        end_observation_date,
        start_observation_date,
        coordinator_fee_percentage: 5,
        locations: (0..num_locations).map(|i| format!("LOC_{}", i)).collect(),
        number_of_values_per_entry: num_locations * 3, // 3 values per location
        total_allowed_entries,
        entry_fee: 1000,
        total_competition_pool: 10000,
        number_of_places_win,
    }
}

pub async fn generate_test_entry(
    competition_id: Uuid,
    wallet: &mut TaprootWalletCore,
    nostr_pubkey: &str,
    station_ids: &Vec<String>,
    entry_index: u32,
    ticket_id: Uuid,
) -> Result<AddEntry, anyhow::Error> {
    let entry_id = Uuid::now_v7();

    // Use the provided wallet and entry index
    let ephemeral_privatekey_encrypted = wallet
        .get_encrypted_dlc_private_key(entry_index, nostr_pubkey)
        .await?;

    let ephemeral_pubkey = wallet.get_dlc_public_key(entry_index).await?;
    info!(
        "entry ephemeral_pubkey: {}, entry index: {}",
        ephemeral_pubkey, entry_index
    );
    // Generate payout hash
    let payout_hash = wallet.add_entry_index(entry_index)?;

    // Get the payout preimage
    let dlc_entry = wallet
        .get_dlc_entry(entry_index)
        .ok_or_else(|| anyhow!("No DLC entry found for index {}", entry_index))?;

    let payout_preimage_encrypted = wallet
        .encrypt_key(&dlc_entry.payout_preimage, nostr_pubkey)
        .await?;

    let mut choices = vec![];
    for station_id in station_ids {
        choices.push(generate_random_weather_choices(station_id));
    }

    Ok(AddEntry {
        id: entry_id,
        ticket_id,
        ephemeral_pubkey,
        ephemeral_privatekey_encrypted,
        payout_hash,
        payout_preimage_encrypted,
        event_id: competition_id,
        expected_observations: choices,
    })
}

// Helper to generate random weather choices
pub fn generate_random_weather_choices(station_id: &String) -> WeatherChoices {
    use rand::Rng;
    let mut rng = rand::thread_rng();

    let mut random_option = || match rng.gen_range(0..3) {
        0 => ValueOptions::Over,
        1 => ValueOptions::Par,
        _ => ValueOptions::Under,
    };

    WeatherChoices {
        stations: station_id.to_owned(),
        wind_speed: Some(random_option()),
        temp_high: Some(random_option()),
        temp_low: Some(random_option()),
    }
}

// This was pulled from the oracle code and should be moved into a shared library otherwise this may get out of date with how the oracle really works
// code located at: https://github.com/tee8z/noaa-oracle/blob/11dc0b696036a3a0a69f24a8f66b6188f87153b5/crates/oracle/src/db/mod.rs#L121
pub fn generate_oracle_event(
    oracle_pubkey: PublicKey,
    event_id: Uuid,
    total_allowed_entries: usize,
    number_of_places_win: usize,
    expiry: u32,
) -> OracleEvent {
    let possible_user_outcomes: Vec<Vec<usize>> =
        generate_ranking_permutations(total_allowed_entries, number_of_places_win);

    let outcome_messages: Vec<Vec<u8>> = generate_outcome_messages(possible_user_outcomes);

    let mut rng = rand::thread_rng();
    let nonce = Scalar::random(&mut rng);
    let nonce_point = nonce.base_point_mul();

    let locking_points = outcome_messages
        .iter()
        .map(|msg| attestation_locking_point(oracle_pubkey, nonce_point, msg))
        .collect();

    OracleEvent {
        id: event_id,
        nonce,
        // The actual announcement the oracle is going to attest the outcome
        event_announcement: EventLockingConditions {
            expiry: Some(expiry),
            locking_points,
        },
        attestation: None,
    }
}

pub fn get_keys(private_key_file_path: String) -> (PublicKey, SecretKey) {
    debug!("private_key_file_path: {:?}", private_key_file_path);

    let secret_key: SecretKey = get_key(&private_key_file_path).expect("Failed to find key");
    debug!("get_keys secret key: {:?}", secret_key);

    let secp = Secp256k1::new();
    let public_key = secret_key.public_key(&secp);
    debug!("get_keys public key: {:?}", public_key);

    (public_key, secret_key)
}

pub fn generate_outcome_messages(possible_user_outcomes: Vec<Vec<usize>>) -> Vec<Vec<u8>> {
    possible_user_outcomes
        .into_iter()
        .map(|inner_vec| {
            inner_vec
                .into_iter()
                .flat_map(|num| num.to_be_bytes())
                .collect::<Vec<u8>>()
        })
        .collect()
}

pub fn get_winning_bytes(winners: Vec<usize>) -> Vec<u8> {
    winners
        .iter()
        .flat_map(|&idx| idx.to_be_bytes())
        .collect::<Vec<u8>>()
}
