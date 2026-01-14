use std::sync::Arc;

use axum::{extract::State, response::Html, Form};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    infra::bitcoin::{Bitcoin, SendOptions},
    startup::AppState,
    templates::{
        admin::{
            dashboard::{
                admin_dashboard, competition_error, competition_success, CompetitionDefaults,
                Station,
            },
            wallet::{
                fee_estimates_rows, send_error, send_success, wallet_balance_section,
                wallet_outputs_rows, wallet_page, WalletBalance, WalletOutput,
            },
        },
        layouts::admin::{admin_base, AdminPageConfig},
    },
};

/// Admin dashboard page (competition tab)
pub async fn admin_page_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let config = AdminPageConfig {
        title: "5day4cast Admin",
        api_base: &state.private_url,
        oracle_base: &state.oracle_url,
        esplora_url: &state.esplora_url,
    };

    // Fetch stations from oracle
    let stations = fetch_stations(&state.oracle_url).await.unwrap_or_default();
    let defaults = CompetitionDefaults::default();

    let content = admin_dashboard(&stations, &defaults);
    Html(admin_base(&config, content).into_string())
}

/// Admin competition tab fragment (for HTMX tab switching)
pub async fn admin_competition_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let stations = fetch_stations(&state.oracle_url).await.unwrap_or_default();
    let defaults = CompetitionDefaults::default();
    Html(admin_dashboard(&stations, &defaults).into_string())
}

/// Admin wallet page fragment (for HTMX tab switching)
pub async fn admin_wallet_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let balance = fetch_balance(&state).await.unwrap_or(WalletBalance {
        confirmed: 0,
        unconfirmed: 0,
    });
    let address = fetch_address(&state).await.unwrap_or_default();

    Html(wallet_page(&state.esplora_url, &balance, &address).into_string())
}

/// Wallet balance fragment (for HTMX refresh)
pub async fn admin_wallet_balance_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let balance = fetch_balance(&state).await.unwrap_or(WalletBalance {
        confirmed: 0,
        unconfirmed: 0,
    });
    Html(wallet_balance_section(&balance).into_string())
}

/// Wallet address fragment (returns just the new address text)
pub async fn admin_wallet_address_fragment(State(state): State<Arc<AppState>>) -> String {
    fetch_address(&state).await.unwrap_or_default()
}

/// Fee estimates table rows fragment
pub async fn admin_fee_estimates_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let estimates = state
        .bitcoin
        .get_estimated_fee_rates()
        .await
        .unwrap_or_default();
    Html(fee_estimates_rows(&estimates).into_string())
}

/// Wallet outputs table rows fragment
pub async fn admin_wallet_outputs_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let outputs = fetch_outputs(&state).await.unwrap_or_default();
    Html(wallet_outputs_rows(&outputs).into_string())
}

/// Form data for creating a competition
#[derive(Debug, Deserialize)]
pub struct CreateCompetitionForm {
    pub id: Uuid,
    pub signing_date: String,
    pub start_observation_date: String,
    pub end_observation_date: String,
    pub number_of_values_per_entry: usize,
    pub total_allowed_entries: usize,
    pub entry_fee: usize,
    pub coordinator_fee_percentage: usize,
    pub number_of_places_win: usize,
    #[serde(default)]
    pub locations: Vec<String>,
}

/// Handle competition creation from HTMX form
pub async fn admin_create_competition_handler(
    State(state): State<Arc<AppState>>,
    Form(form): Form<CreateCompetitionForm>,
) -> Html<String> {
    // Parse dates
    let signing_date = match OffsetDateTime::parse(
        &form.signing_date,
        &time::format_description::well_known::Rfc3339,
    ) {
        Ok(dt) => dt,
        Err(e) => {
            return Html(competition_error(&format!("Invalid signing date: {}", e)).into_string())
        }
    };

    let start_observation_date = match OffsetDateTime::parse(
        &form.start_observation_date,
        &time::format_description::well_known::Rfc3339,
    ) {
        Ok(dt) => dt,
        Err(e) => {
            return Html(competition_error(&format!("Invalid start date: {}", e)).into_string())
        }
    };

    let end_observation_date = match OffsetDateTime::parse(
        &form.end_observation_date,
        &time::format_description::well_known::Rfc3339,
    ) {
        Ok(dt) => dt,
        Err(e) => {
            return Html(competition_error(&format!("Invalid end date: {}", e)).into_string())
        }
    };

    // Calculate total pool
    let total_competition_pool = form.entry_fee * form.total_allowed_entries;

    // Create the competition via the coordinator
    let create_event = crate::domain::CreateEvent {
        id: form.id,
        signing_date,
        start_observation_date,
        end_observation_date,
        locations: form.locations,
        number_of_values_per_entry: form.number_of_values_per_entry,
        number_of_places_win: form.number_of_places_win,
        total_allowed_entries: form.total_allowed_entries,
        entry_fee: form.entry_fee,
        coordinator_fee_percentage: form.coordinator_fee_percentage,
        total_competition_pool,
    };

    match state.coordinator.create_competition(create_event).await {
        Ok(competition) => Html(competition_success(&competition.id).into_string()),
        Err(e) => Html(competition_error(&e.to_string()).into_string()),
    }
}

/// Form data for sending bitcoin
#[derive(Debug, Deserialize)]
pub struct SendBitcoinForm {
    pub address_to: String,
    pub amount: Option<u64>,
    pub max_fee: Option<u64>,
}

/// Handle send bitcoin from HTMX form
pub async fn admin_send_bitcoin_handler(
    State(state): State<Arc<AppState>>,
    Form(form): Form<SendBitcoinForm>,
) -> Html<String> {
    let send_options = SendOptions {
        address_to: form.address_to,
        address_from: None,
        amount: form.amount,
        max_fee: form.max_fee,
    };

    match state.bitcoin.send_to_address(send_options, vec![]).await {
        Ok(txid) => Html(send_success(&txid.to_string()).into_string()),
        Err(e) => Html(send_error(&e.to_string()).into_string()),
    }
}

// Helper functions

async fn fetch_stations(oracle_url: &str) -> Result<Vec<Station>, anyhow::Error> {
    let client = reqwest_middleware::reqwest::Client::new();
    let response = client
        .get(format!("{}/stations", oracle_url))
        .send()
        .await?;

    if response.status().is_success() {
        let stations: Vec<Station> = response.json().await?;
        Ok(stations)
    } else {
        Ok(vec![])
    }
}

async fn fetch_balance(state: &AppState) -> Result<WalletBalance, anyhow::Error> {
    let balance = state.bitcoin.get_balance().await?;
    Ok(WalletBalance {
        confirmed: balance.confirmed.to_sat(),
        unconfirmed: balance.untrusted_pending.to_sat() + balance.trusted_pending.to_sat(),
    })
}

async fn fetch_address(state: &AppState) -> Result<String, anyhow::Error> {
    let address = state.bitcoin.get_next_address().await?;
    Ok(address.to_string())
}

async fn fetch_outputs(state: &AppState) -> Result<Vec<WalletOutput>, anyhow::Error> {
    let outputs = state.bitcoin.get_outputs().await?;
    Ok(outputs
        .into_iter()
        .map(|o| WalletOutput {
            outpoint: o.outpoint.to_string(),
            txout: crate::templates::admin::wallet::TxOut {
                value: o.txout.value.to_sat(),
                script_pubkey: Some(o.txout.script_pubkey.to_string()),
            },
            is_spent: o.is_spent,
        })
        .collect())
}
