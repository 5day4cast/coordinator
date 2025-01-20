use anyhow::anyhow;
use async_trait::async_trait;
use client_validator::{NostrClientCore, SignerType, TaprootWalletCore, TaprootWalletCoreBuilder};
use dlctix::{
    attestation_locking_point,
    bitcoin::OutPoint,
    musig2::secp256k1::{PublicKey, Secp256k1, SecretKey},
    secp::Scalar,
    EventLockingConditions,
};
use log::info;
use mockall::mock;
use server::{
    domain::{generate_ranking_permutations, AddEntry, CoordinatorInfo, CreateEvent},
    get_key, AddEventEntry, Bitcoin, Ln, Oracle, OracleError as Error, OracleEvent, ValueOptions,
    WeatherChoices,
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
        async fn submit_entry(&self, entry: AddEventEntry) -> Result<(), Error>;
    }
}

mock! {
    #[derive(Send, Sync)]
    pub BitcoinClient { }

    #[async_trait]
    impl Bitcoin for BitcoinClient {
        async fn get_spendable_utxo(&self, amount: u64) -> Result<OutPoint, anyhow::Error>;
        async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error>;
        async fn broadcast(&self, tx: String) -> Result<(), anyhow::Error>;
    }
}

mock! {
    #[derive(Send, Sync)]
    pub LnClient { }

    #[async_trait]
    impl Ln for LnClient {
        async fn ping(&self) -> Result<(), anyhow::Error>;
        async fn add_hold_invoice(
            &self,
            value: u64,
            expiry_time_secs: u64,
            ticket_hash: String,
            entry_id: Uuid,
            entry_index: u64,
            competition_id: Uuid,
        ) -> Result<server::InvoiceAddResponse, anyhow::Error>;
        async fn cancel_hold_invoice(&self, ticket_hash: String) -> Result<(), anyhow::Error>;
        async fn settle_hold_invoice(&self, ticket_preimage: String) -> Result<(), anyhow::Error>;
        async fn send_payment(
            &self,
            payout_payment_request: String,
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

pub struct TestParticipant {
    pub wallet: client_validator::TaprootWalletCore,
    pub nostr_pubkey: String,
}

impl TestParticipant {
    pub fn new(wallet: client_validator::TaprootWalletCore, nostr_pubkey: String) -> Self {
        Self {
            wallet,
            nostr_pubkey,
        }
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
    let observation_date = now + Duration::days(1);
    let signing_date = observation_date + Duration::hours(2); // Signing happens after observation

    CreateEvent {
        id: Uuid::now_v7(),
        signing_date,
        observation_date,
        locations: (0..num_locations).map(|i| format!("LOC_{}", i)).collect(),
        number_of_values_per_entry: num_locations * 3, // 3 values per location
        total_allowed_entries,
        entry_fee: 1000,
        total_competition_pool: 10000,
        coordinator: Some(CoordinatorInfo {
            pubkey: "test_coordinator_pubkey".to_string(),
            signature: "test_coordinator_signature".to_string(),
        }),
        number_of_places_win,
    }
}

pub async fn generate_test_entry(
    competition_id: Uuid,
    wallet: &mut TaprootWalletCore,
    nostr_pubkey: &str,
    station_ids: &Vec<String>,
    entry_index: u32,
) -> Result<AddEntry, anyhow::Error> {
    let entry_id = Uuid::now_v7();

    // Use the provided wallet and entry index
    let ephemeral_privatekey_encrypted = wallet
        .get_encrypted_dlc_private_key(entry_index, nostr_pubkey)
        .await?;

    let ephemeral_pubkey = wallet.get_dlc_public_key(entry_index).await?;
    info!("entry ephemeral_pubkey: {}", ephemeral_pubkey);
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
) -> OracleEvent {
    let possible_user_outcomes: Vec<Vec<usize>> =
        generate_ranking_permutations(total_allowed_entries, number_of_places_win);

    let outcome_messages: Vec<Vec<u8>> = generate_outcome_messages(possible_user_outcomes);

    let mut rng = rand::thread_rng();
    let nonce = Scalar::random(&mut rng);
    let nonce_point = nonce.base_point_mul();
    // Manually set expiry to 7 days after the signature should have been provided so users can get their funds back
    let expiry = OffsetDateTime::now_utc()
        .saturating_add(Duration::DAY * 7)
        .unix_timestamp() as u32;

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
    }
}

pub fn get_oracle_keys(private_key_file_path: String) -> (PublicKey, SecretKey) {
    let secret_key: SecretKey = get_key(&private_key_file_path).expect("Failed to find key");
    let secp = Secp256k1::new();
    let public_key = secret_key.public_key(&secp);
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
