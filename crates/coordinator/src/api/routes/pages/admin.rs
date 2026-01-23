use std::sync::Arc;

use axum::{extract::State, http::HeaderMap, response::Html};
use axum_extra::extract::Form;
use log::error;
use maud::Markup;
use serde::Deserialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

use crate::{
    infra::bitcoin::SendOptions,
    startup::AppState,
    templates::{
        admin::{
            dashboard::{
                admin_dashboard, competition_error, competition_success, CompetitionDefaults,
                Forecast, Observation, Station, StationWithWeather,
            },
            is_allowed_station,
            wallet::{
                fee_estimates_rows, send_error, send_success, wallet_balance_section,
                wallet_outputs_rows, wallet_page, WalletBalance, WalletOutput,
            },
        },
        layouts::admin::{admin_base, AdminPageConfig},
    },
};

/// Helper to render a fragment or wrap it in the admin base layout for direct navigation.
fn render_admin_fragment(
    headers: &HeaderMap,
    state: &AppState,
    title: &str,
    content: Markup,
) -> Html<String> {
    let is_htmx = headers.get("HX-Request").is_some();

    if is_htmx {
        Html(content.into_string())
    } else {
        let config = AdminPageConfig {
            title,
            api_base: &state.private_url,
            oracle_base: &state.oracle_url,
            esplora_url: &state.esplora_url,
        };
        Html(admin_base(&config, content).into_string())
    }
}

/// Admin dashboard page (competition tab)
pub async fn admin_page_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let config = AdminPageConfig {
        title: "5day4cast Admin",
        api_base: &state.private_url,
        oracle_base: &state.oracle_url,
        esplora_url: &state.esplora_url,
    };

    // Fetch stations from oracle and filter to top 200 cities
    let stations: Vec<Station> = fetch_stations(&state.oracle_url)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s| is_allowed_station(&s.station_id))
        .collect();

    // Fetch weather data (forecasts and observations) for all stations
    let station_ids: Vec<&str> = stations.iter().map(|s| s.station_id.as_str()).collect();
    let (forecasts, observations) = tokio::join!(
        fetch_forecasts(&state.oracle_url, &station_ids),
        fetch_observations(&state.oracle_url, &station_ids)
    );

    // Merge stations with weather data
    let stations_with_weather = merge_stations_with_weather(
        stations,
        forecasts.unwrap_or_default(),
        observations.unwrap_or_default(),
    );

    let defaults = CompetitionDefaults::default();

    let content = admin_dashboard(&stations_with_weather, &defaults);
    Html(admin_base(&config, content).into_string())
}

/// Admin competition tab fragment (for HTMX tab switching)
pub async fn admin_competition_fragment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Html<String> {
    // Fetch stations from oracle and filter to top 200 cities
    let stations: Vec<Station> = fetch_stations(&state.oracle_url)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s| is_allowed_station(&s.station_id))
        .collect();

    // Fetch weather data (forecasts and observations) for all stations
    let station_ids: Vec<&str> = stations.iter().map(|s| s.station_id.as_str()).collect();
    let (forecasts, observations) = tokio::join!(
        fetch_forecasts(&state.oracle_url, &station_ids),
        fetch_observations(&state.oracle_url, &station_ids)
    );

    // Merge stations with weather data
    let stations_with_weather = merge_stations_with_weather(
        stations,
        forecasts.unwrap_or_default(),
        observations.unwrap_or_default(),
    );

    let defaults = CompetitionDefaults::default();
    let content = admin_dashboard(&stations_with_weather, &defaults);
    render_admin_fragment(&headers, &state, "5day4cast Admin - Competition", content)
}

/// Admin wallet page (full page for direct navigation, fragment for HTMX)
pub async fn admin_wallet_fragment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Html<String> {
    let balance = fetch_balance(&state)
        .await
        .inspect_err(|e| error!("Failed to fetch wallet balance: {e}"))
        .unwrap_or(WalletBalance {
            confirmed: 0,
            unconfirmed: 0,
        });
    let address = fetch_address(&state)
        .await
        .inspect_err(|e| error!("Failed to fetch wallet address: {e}"))
        .unwrap_or_default();

    let content = wallet_page(&state.esplora_url, &balance, &address);
    render_admin_fragment(&headers, &state, "5day4cast Admin - Wallet", content)
}

/// Wallet balance fragment (for HTMX refresh)
pub async fn admin_wallet_balance_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let balance = fetch_balance(&state)
        .await
        .inspect_err(|e| error!("Failed to fetch wallet balance: {e}"))
        .unwrap_or(WalletBalance {
            confirmed: 0,
            unconfirmed: 0,
        });
    Html(wallet_balance_section(&balance).into_string())
}

/// Wallet address fragment (returns just the new address text)
pub async fn admin_wallet_address_fragment(State(state): State<Arc<AppState>>) -> String {
    fetch_address(&state)
        .await
        .inspect_err(|e| error!("Failed to fetch wallet address: {e}"))
        .unwrap_or_default()
}

/// Fee estimates table rows fragment
pub async fn admin_fee_estimates_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let estimates = state
        .bitcoin
        .get_estimated_fee_rates()
        .await
        .inspect_err(|e| error!("Failed to fetch fee estimates: {e}"))
        .unwrap_or_default();
    Html(fee_estimates_rows(&estimates).into_string())
}

/// Wallet outputs table rows fragment
pub async fn admin_wallet_outputs_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let outputs = fetch_outputs(&state)
        .await
        .inspect_err(|e| error!("Failed to fetch wallet outputs: {e}"))
        .unwrap_or_default();
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

async fn fetch_forecasts(
    oracle_url: &str,
    station_ids: &[&str],
) -> Result<Vec<Forecast>, anyhow::Error> {
    if station_ids.is_empty() {
        return Ok(vec![]);
    }

    let client = reqwest_middleware::reqwest::Client::new();

    // Fetch forecasts for today and tomorrow (end date is exclusive, so +2 days)
    let today = time::OffsetDateTime::now_utc();
    let end_date = today + time::Duration::days(2);

    let start = today.format(&Rfc3339).unwrap_or_default();
    let end = end_date.format(&Rfc3339).unwrap_or_default();

    let station_ids_param = station_ids.join(",");

    let response = client
        .get(format!(
            "{}/stations/forecasts?station_ids={}&start={}&end={}",
            oracle_url, station_ids_param, start, end
        ))
        .send()
        .await?;

    if response.status().is_success() {
        let forecasts: Vec<Forecast> = response.json().await?;
        Ok(forecasts)
    } else {
        Ok(vec![])
    }
}

async fn fetch_observations(
    oracle_url: &str,
    station_ids: &[&str],
) -> Result<Vec<Observation>, anyhow::Error> {
    if station_ids.is_empty() {
        return Ok(vec![]);
    }

    let client = reqwest_middleware::reqwest::Client::new();

    // Fetch observations for today only
    let today = time::OffsetDateTime::now_utc();
    let tomorrow = today + time::Duration::days(1);

    let start = today.format(&Rfc3339).unwrap_or_default();
    let end = tomorrow.format(&Rfc3339).unwrap_or_default();

    let station_ids_param = station_ids.join(",");

    let response = client
        .get(format!(
            "{}/stations/observations?station_ids={}&start={}&end={}",
            oracle_url, station_ids_param, start, end
        ))
        .send()
        .await?;

    if response.status().is_success() {
        let observations: Vec<Observation> = response.json().await?;
        Ok(observations)
    } else {
        Ok(vec![])
    }
}

fn merge_stations_with_weather(
    stations: Vec<Station>,
    forecasts: Vec<Forecast>,
    observations: Vec<Observation>,
) -> Vec<StationWithWeather> {
    use std::collections::HashMap;

    // Get today and tomorrow date strings
    let today = time::OffsetDateTime::now_utc();
    let tomorrow = today + time::Duration::days(1);

    let today_str = today.date().to_string();
    let tomorrow_str = tomorrow.date().to_string();

    // Index forecasts by station_id and date
    let mut forecast_map: HashMap<(&str, &str), &Forecast> = HashMap::new();
    for forecast in &forecasts {
        // The date field format is "YYYY-MM-DD" or "YYYY-MM-DDTHH:MM:SS..."
        let date_part = forecast.date.split('T').next().unwrap_or(&forecast.date);
        forecast_map.insert((forecast.station_id.as_str(), date_part), forecast);
    }

    // Index observations by station_id (we only fetch today's)
    let mut observation_map: HashMap<&str, &Observation> = HashMap::new();
    for observation in &observations {
        observation_map.insert(observation.station_id.as_str(), observation);
    }

    stations
        .into_iter()
        .map(|station| {
            let today_forecast =
                forecast_map.get(&(station.station_id.as_str(), today_str.as_str()));
            let tomorrow_forecast =
                forecast_map.get(&(station.station_id.as_str(), tomorrow_str.as_str()));
            let today_observation = observation_map.get(station.station_id.as_str());

            StationWithWeather {
                today_actual_high: today_observation.map(|o| o.temp_high),
                today_actual_low: today_observation.map(|o| o.temp_low),
                today_forecast_high: today_forecast.map(|f| f.temp_high),
                today_forecast_low: today_forecast.map(|f| f.temp_low),
                tomorrow_forecast_high: tomorrow_forecast.map(|f| f.temp_high),
                tomorrow_forecast_low: tomorrow_forecast.map(|f| f.temp_low),
                station,
            }
        })
        .collect()
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
