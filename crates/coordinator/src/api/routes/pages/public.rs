use std::sync::Arc;

use dlctix::secp::Point;
use log::{debug, error};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use maud::{html, Markup};
use uuid::Uuid;

use crate::{
    api::extractors::{AuthError, NostrAuth},
    domain::{
        scoring::{calculate_option_score, Forecast, Observation},
        SearchBy,
    },
    infra::oracle::ValueOptions,
    startup::AppState,
    templates::{
        fragments::{
            entry_form::{entry_form, ForecastValue, StationForecast, WeatherContext},
            leaderboard::{leaderboard, leaderboard_row, EntryScore, LeaderboardInfo},
        },
        layouts::base::{base, PageConfig},
        pages::{
            competitions::{competitions_page, CompetitionView},
            entries::{entries_page, no_entries, EntryView},
            payouts::{payouts_page, PayoutView},
        },
    },
};

/// Helper to render a fragment or wrap it in the base layout for direct navigation.
/// Returns just the fragment for HTMX requests, or a full page for direct URL access.
fn render_fragment(
    headers: &HeaderMap,
    state: &AppState,
    title: &str,
    content: Markup,
) -> Html<String> {
    let is_htmx = headers.get("HX-Request").is_some();

    if is_htmx {
        Html(content.into_string())
    } else {
        let config = PageConfig {
            title,
            api_base: &state.remote_url,
            oracle_base: &state.oracle_url,
            network: &state.bitcoin.get_network().to_string(),
        };
        Html(base(&config, content).into_string())
    }
}

/// HTML error response for HTMX routes
pub struct HtmlAuthError(pub AuthError);

impl IntoResponse for HtmlAuthError {
    fn into_response(self) -> Response {
        let (status, message) = match &self.0 {
            AuthError::NoAuthHeader => (
                StatusCode::UNAUTHORIZED,
                "Please log in to access this page.",
            ),
            AuthError::ExpiredTimestamp => (
                StatusCode::UNAUTHORIZED,
                "Your session has expired. Please log in again.",
            ),
            AuthError::InvalidSignature(_) => (
                StatusCode::FORBIDDEN,
                "Authentication failed. Please log in again.",
            ),
            _ => (
                StatusCode::UNAUTHORIZED,
                "Authentication required. Please log in.",
            ),
        };

        let body = html! {
            div class="container" {
                div class="notification is-warning" {
                    p { (message) }
                    button class="button is-primary mt-3"
                           onclick="document.getElementById('loginModal').classList.add('is-active')" {
                        "Log In"
                    }
                }
            }
        };

        (status, Html(body.into_string())).into_response()
    }
}

impl From<AuthError> for HtmlAuthError {
    fn from(err: AuthError) -> Self {
        HtmlAuthError(err)
    }
}

/// Nostr auth extractor that returns HTML errors for HTMX routes
pub struct HtmlNostrAuth(pub NostrAuth);

impl<S> axum::extract::FromRequestParts<S> for HtmlNostrAuth
where
    S: Send + Sync,
{
    type Rejection = HtmlAuthError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        NostrAuth::from_request_parts(parts, state)
            .await
            .map(HtmlNostrAuth)
            .map_err(HtmlAuthError)
    }
}

/// Public home page - competitions list
pub async fn public_page_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let config = PageConfig {
        title: "Fantasy Weather",
        api_base: &state.remote_url,
        oracle_base: &state.oracle_url,
        network: &state.bitcoin.get_network().to_string(),
    };

    let competitions = fetch_competitions(&state).await;
    let content = competitions_page(&competitions);
    Html(base(&config, content).into_string())
}

/// Competitions page fragment (for HTMX navigation)
pub async fn competitions_fragment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Html<String> {
    let competitions = fetch_competitions(&state).await;
    let content = competitions_page(&competitions);
    render_fragment(&headers, &state, "Competitions - Fantasy Weather", content)
}

/// Competition rows fragment (for HTMX auto-refresh)
pub async fn competitions_rows_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let competitions = fetch_competitions(&state).await;
    Html(
        html! {
            @for comp in &competitions {
                (crate::templates::fragments::competition_row::competition_row(comp))
            }
        }
        .into_string(),
    )
}

/// Entries page fragment (requires auth, returns HTML error on auth failure)
pub async fn entries_fragment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    HtmlNostrAuth(NostrAuth { pubkey, .. }): HtmlNostrAuth,
) -> Html<String> {
    let entries = fetch_user_entries(&state, &pubkey.to_hex()).await;
    let content = if entries.is_empty() {
        no_entries()
    } else {
        entries_page(&entries)
    };
    render_fragment(&headers, &state, "My Entries - Fantasy Weather", content)
}

/// Payouts page fragment (requires auth, returns HTML error on auth failure)
pub async fn payouts_fragment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    HtmlNostrAuth(NostrAuth { pubkey, .. }): HtmlNostrAuth,
) -> Html<String> {
    let payouts = fetch_eligible_payouts(&state, &pubkey.to_hex()).await;
    let content = payouts_page(&payouts);
    render_fragment(&headers, &state, "Payouts - Fantasy Weather", content)
}

/// Entry form for a competition
/// Returns full page if accessed directly, or fragment if via HTMX
pub async fn entry_form_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    headers: HeaderMap,
) -> Html<String> {
    // Get competition details
    let competitions = fetch_competitions(&state).await;
    let competition = competitions
        .iter()
        .find(|c| c.id == competition_id.to_string());

    let content = match competition {
        Some(comp) => {
            // Fetch forecasts from oracle
            let forecasts = fetch_forecasts(&state, comp).await;
            entry_form(comp, &forecasts)
        }
        None => {
            html! {
                div class="notification is-danger" {
                    "Competition not found"
                }
            }
        }
    };

    render_fragment(&headers, &state, "Submit Entry - Fantasy Weather", content)
}

/// Leaderboard for a competition
/// Returns full page if accessed directly, or fragment if via HTMX
pub async fn leaderboard_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    headers: HeaderMap,
) -> Html<String> {
    let scores = fetch_leaderboard_scores(&state, competition_id).await;

    // Fetch competition details for observation period
    let info = match state.coordinator.get_competition(competition_id).await {
        Ok(comp) => {
            let status = determine_competition_status(&comp);
            LeaderboardInfo {
                competition_id: competition_id.to_string(),
                start_time: comp
                    .event_submission
                    .start_observation_date
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                end_time: comp
                    .event_submission
                    .end_observation_date
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                status,
            }
        }
        Err(_) => LeaderboardInfo {
            competition_id: competition_id.to_string(),
            start_time: String::new(),
            end_time: String::new(),
            status: "Unknown".to_string(),
        },
    };

    let content = leaderboard(&info, &scores);
    render_fragment(&headers, &state, "Leaderboard - Fantasy Weather", content)
}

/// Leaderboard rows fragment (for auto-refresh)
pub async fn leaderboard_rows_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Html<String> {
    let scores = fetch_leaderboard_scores(&state, competition_id).await;
    Html(
        html! {
            @for score in &scores {
                (leaderboard_row(score))
            }
        }
        .into_string(),
    )
}

/// Entry detail fragment (for modal)
pub async fn entry_detail_fragment(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
) -> Html<String> {
    // Fetch the entry details
    let entry = state
        .coordinator
        .get_entry_by_id(entry_id)
        .await
        .ok()
        .flatten();

    // If we have an entry, fetch forecast and observation data
    let weather_data = if let Some(ref entry) = entry {
        fetch_entry_weather_data(&state, entry).await
    } else {
        None
    };

    Html(
        html! {
            div {
                h4 class="title is-4" { "Entry Details" }
                p class="mb-4" { "Entry ID: " (entry_id) }

                @if let Some(entry) = entry {
                    h5 class="title is-5 mt-4" { "Picks & Score Breakdown" }
                    @if entry.entry_submission.expected_observations.is_empty() {
                        p class="has-text-grey" { "No picks recorded" }
                    } @else if let Some(ref data) = weather_data {
                        // Show total score if competition is complete
                        @if let Some(total_score) = data.total_score {
                            p class="mb-3" {
                                strong { "Total Score: " }
                                span class="tag is-info is-medium" { (total_score) }
                            }
                        } @else {
                            p class="mb-3 has-text-grey-light" {
                                "Final scores will be available after observations are recorded"
                            }
                        }
                        table class="table is-fullwidth is-striped" {
                            thead {
                                tr {
                                    th { "Station" }
                                    th { "Metric" }
                                    th { "Forecast" }
                                    th { "Actual" }
                                    th { "Pick" }
                                    @if data.is_complete {
                                        th { "Points" }
                                    }
                                }
                            }
                            tbody {
                                @for obs in &entry.entry_submission.expected_observations {
                                    @let forecast = data.forecasts.get(&obs.stations);
                                    @let observation = data.observations.get(&obs.stations);
                                    // Temp High row
                                    @if let Some(pick) = &obs.temp_high {
                                        @let forecast_val = forecast.and_then(|f| f.temp_high);
                                        @let obs_val = observation.and_then(|o| o.temp_high);
                                        tr {
                                            td { (get_station_name(&obs.stations)) }
                                            td { "Temp High" }
                                            td { (format_value(forecast_val, "°F")) }
                                            td { (format_value(obs_val, "°F")) }
                                            td { (format_pick(&Some(pick.clone()))) }
                                            @if data.is_complete {
                                                @let points = calculate_option_score(forecast_val, obs_val, pick);
                                                td class=(score_class(points)) { (points) }
                                            }
                                        }
                                    }
                                    // Temp Low row
                                    @if let Some(pick) = &obs.temp_low {
                                        @let forecast_val = forecast.and_then(|f| f.temp_low);
                                        @let obs_val = observation.and_then(|o| o.temp_low);
                                        tr {
                                            td { (get_station_name(&obs.stations)) }
                                            td { "Temp Low" }
                                            td { (format_value(forecast_val, "°F")) }
                                            td { (format_value(obs_val, "°F")) }
                                            td { (format_pick(&Some(pick.clone()))) }
                                            @if data.is_complete {
                                                @let points = calculate_option_score(forecast_val, obs_val, pick);
                                                td class=(score_class(points)) { (points) }
                                            }
                                        }
                                    }
                                    // Wind Speed row
                                    @if let Some(pick) = &obs.wind_speed {
                                        @let forecast_val = forecast.and_then(|f| f.wind_speed);
                                        @let obs_val = observation.and_then(|o| o.wind_speed);
                                        tr {
                                            td { (get_station_name(&obs.stations)) }
                                            td { "Wind Speed" }
                                            td { (format_value(forecast_val, " mph")) }
                                            td { (format_value(obs_val, " mph")) }
                                            td { (format_pick(&Some(pick.clone()))) }
                                            @if data.is_complete {
                                                @let points = calculate_option_score(forecast_val, obs_val, pick);
                                                td class=(score_class(points)) { (points) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } @else {
                        // No weather data available - show picks only
                        p class="mb-3 has-text-grey-light" {
                            "Weather data unavailable"
                        }
                        table class="table is-fullwidth is-striped" {
                            thead {
                                tr {
                                    th { "Station" }
                                    th { "Temp High" }
                                    th { "Temp Low" }
                                    th { "Wind Speed" }
                                }
                            }
                            tbody {
                                @for obs in &entry.entry_submission.expected_observations {
                                    tr {
                                        td { (get_station_name(&obs.stations)) }
                                        td { (format_pick(&obs.temp_high)) }
                                        td { (format_pick(&obs.temp_low)) }
                                        td { (format_pick(&obs.wind_speed)) }
                                    }
                                }
                            }
                        }
                    }
                } @else {
                    p class="has-text-grey" { "Entry details not available" }
                }
            }
        }
        .into_string(),
    )
}

/// Format a pick value for display
fn format_pick(pick: &Option<ValueOptions>) -> String {
    match pick {
        Some(ValueOptions::Over) => "Over ↑".to_string(),
        Some(ValueOptions::Par) => "Par →".to_string(),
        Some(ValueOptions::Under) => "Under ↓".to_string(),
        None => "-".to_string(),
    }
}

/// Format a numeric value with unit for display
fn format_value(value: Option<f64>, unit: &str) -> String {
    match value {
        Some(v) => format!("{:.1}{}", v, unit),
        None => "-".to_string(),
    }
}

/// Get CSS class for score display
fn score_class(points: i32) -> &'static str {
    if points >= 20 {
        "has-text-success has-text-weight-bold"
    } else if points > 0 {
        "has-text-info"
    } else {
        "has-text-grey"
    }
}

/// Entry weather data for display
struct EntryWeatherData {
    forecasts: std::collections::HashMap<String, Forecast>,
    observations: std::collections::HashMap<String, Observation>,
    total_score: Option<i32>,
    is_complete: bool,
}

/// Fetch forecast and observation data for an entry's display
async fn fetch_entry_weather_data(
    state: &AppState,
    entry: &crate::domain::UserEntry,
) -> Option<EntryWeatherData> {
    use std::collections::HashMap;

    // Get the competition to find observation dates
    let competition = state
        .coordinator
        .get_competition(entry.event_id)
        .await
        .ok()?;

    // Get station IDs from the entry's picks
    let station_ids: Vec<&str> = entry
        .entry_submission
        .expected_observations
        .iter()
        .map(|obs| obs.stations.as_str())
        .collect();

    if station_ids.is_empty() {
        return None;
    }

    // Fetch forecasts and observations for the competition period
    let (forecasts, observations) = tokio::join!(
        fetch_entry_forecasts(
            &state.oracle_url,
            &station_ids,
            competition.event_submission.start_observation_date,
            competition.event_submission.end_observation_date
        ),
        fetch_entry_observations(
            &state.oracle_url,
            &station_ids,
            competition.event_submission.start_observation_date,
            competition.event_submission.end_observation_date
        )
    );

    let forecasts = forecasts.ok().unwrap_or_default();
    let observations = observations.ok().unwrap_or_default();

    // Index by station ID
    let mut forecast_map: HashMap<String, Forecast> = HashMap::new();
    for f in forecasts {
        forecast_map.insert(f.station_id.clone(), f);
    }

    let mut observation_map: HashMap<String, Observation> = HashMap::new();
    for o in observations {
        observation_map.insert(o.station_id.clone(), o);
    }

    // Check if observations are complete (past end date and have observation data)
    let now = time::OffsetDateTime::now_utc();
    let is_complete =
        now >= competition.event_submission.end_observation_date && !observation_map.is_empty();

    // Calculate total score only if observations are complete
    let total_score = if is_complete {
        let mut score = 0i32;
        for obs in &entry.entry_submission.expected_observations {
            let forecast = forecast_map.get(&obs.stations);
            let observation = observation_map.get(&obs.stations);

            if let Some(pick) = &obs.temp_high {
                score += calculate_option_score(
                    forecast.and_then(|f| f.temp_high),
                    observation.and_then(|o| o.temp_high),
                    pick,
                );
            }
            if let Some(pick) = &obs.temp_low {
                score += calculate_option_score(
                    forecast.and_then(|f| f.temp_low),
                    observation.and_then(|o| o.temp_low),
                    pick,
                );
            }
            if let Some(pick) = &obs.wind_speed {
                score += calculate_option_score(
                    forecast.and_then(|f| f.wind_speed),
                    observation.and_then(|o| o.wind_speed),
                    pick,
                );
            }
        }
        Some(score)
    } else {
        None
    };

    Some(EntryWeatherData {
        forecasts: forecast_map,
        observations: observation_map,
        total_score,
        is_complete,
    })
}

/// Fetch forecasts for entry score calculation
async fn fetch_entry_forecasts(
    oracle_url: &str,
    station_ids: &[&str],
    start: time::OffsetDateTime,
    end: time::OffsetDateTime,
) -> Result<Vec<Forecast>, anyhow::Error> {
    if station_ids.is_empty() {
        return Ok(vec![]);
    }

    let client = reqwest_middleware::reqwest::Client::new();

    let start_str = start
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let end_str = end
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    let station_ids_param = station_ids.join(",");

    let response = client
        .get(format!(
            "{}/stations/forecasts?station_ids={}&start={}&end={}",
            oracle_url, station_ids_param, start_str, end_str
        ))
        .send()
        .await?;

    if response.status().is_success() {
        // The oracle returns forecasts with temp_high/temp_low as integers
        #[derive(serde::Deserialize)]
        struct RawForecast {
            station_id: String,
            temp_high: i64,
            temp_low: i64,
            #[serde(default)]
            wind_speed: Option<f64>,
        }

        let raw_forecasts: Vec<RawForecast> = response.json().await?;
        Ok(raw_forecasts
            .into_iter()
            .map(|f| Forecast {
                station_id: f.station_id,
                temp_high: Some(f.temp_high as f64),
                temp_low: Some(f.temp_low as f64),
                wind_speed: f.wind_speed,
            })
            .collect())
    } else {
        Ok(vec![])
    }
}

/// Fetch observations for entry score calculation
async fn fetch_entry_observations(
    oracle_url: &str,
    station_ids: &[&str],
    start: time::OffsetDateTime,
    end: time::OffsetDateTime,
) -> Result<Vec<Observation>, anyhow::Error> {
    if station_ids.is_empty() {
        return Ok(vec![]);
    }

    let client = reqwest_middleware::reqwest::Client::new();

    let start_str = start
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let end_str = end
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    let station_ids_param = station_ids.join(",");

    let response = client
        .get(format!(
            "{}/stations/observations?station_ids={}&start={}&end={}",
            oracle_url, station_ids_param, start_str, end_str
        ))
        .send()
        .await?;

    if response.status().is_success() {
        #[derive(serde::Deserialize)]
        struct RawObservation {
            station_id: String,
            temp_high: f64,
            temp_low: f64,
            #[serde(default)]
            wind_speed: Option<f64>,
        }

        let raw_observations: Vec<RawObservation> = response.json().await?;
        Ok(raw_observations
            .into_iter()
            .map(|o| Observation {
                station_id: o.station_id,
                temp_high: Some(o.temp_high),
                temp_low: Some(o.temp_low),
                wind_speed: o.wind_speed,
            })
            .collect())
    } else {
        Ok(vec![])
    }
}

// Helper functions

async fn fetch_competitions(state: &AppState) -> Vec<CompetitionView> {
    match state.coordinator.get_competitions().await {
        Ok(competitions) => competitions
            .into_iter()
            .map(|c| {
                let status = determine_competition_status(&c);
                let can_enter = status == "Registration"
                    && c.total_entries < c.event_submission.total_allowed_entries as u64;

                CompetitionView {
                    id: c.id.to_string(),
                    start_time: c
                        .event_submission
                        .start_observation_date
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                    end_time: c
                        .event_submission
                        .end_observation_date
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                    signing_time: c
                        .event_submission
                        .signing_date
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                    status,
                    entry_fee: c.event_submission.entry_fee as u64,
                    total_pool: c.event_submission.total_competition_pool as u64,
                    total_entries: c.total_entries,
                    num_winners: c.event_submission.number_of_places_win as u64,
                    can_enter,
                    number_of_values_per_entry: c.event_submission.number_of_values_per_entry,
                }
            })
            .collect(),
        Err(_) => vec![],
    }
}

fn determine_competition_status(competition: &crate::domain::Competition) -> String {
    use crate::domain::CompetitionState;
    use time::OffsetDateTime;

    let state = competition.get_state();

    match state {
        CompetitionState::Failed => "Failed".to_string(),
        CompetitionState::Cancelled => "Cancelled".to_string(),
        CompetitionState::Completed => "Completed".to_string(),
        CompetitionState::Attested
        | CompetitionState::OutcomeBroadcasted
        | CompetitionState::DeltaBroadcasted
        | CompetitionState::ExpiryBroadcasted => "Completed".to_string(),
        // For all other states, use time-based labels for user-friendliness
        _ => {
            let now = OffsetDateTime::now_utc();
            let start = competition.event_submission.start_observation_date;
            let end = competition.event_submission.end_observation_date;

            if now < start {
                "Registration".to_string()
            } else if now < end {
                "Live".to_string()
            } else {
                "Awaiting Results".to_string()
            }
        }
    }
}

async fn fetch_user_entries(state: &AppState, pubkey: &str) -> Vec<EntryView> {
    match state
        .coordinator
        .get_entries(pubkey.to_string(), SearchBy { event_ids: None })
        .await
    {
        Ok(entries) => entries
            .into_iter()
            .map(|e| EntryView {
                competition_id: e.event_id.to_string(),
                start_time: String::new(), // Would need to fetch competition for this
                end_time: String::new(),
                status: if e.paid_at.is_some() {
                    "Paid"
                } else if e.signed_at.is_some() {
                    "Signed"
                } else {
                    "Pending"
                }
                .to_string(),
                entry_id: e.id.to_string(),
            })
            .collect(),
        Err(_) => vec![],
    }
}

async fn fetch_eligible_payouts(state: &AppState, pubkey: &str) -> Vec<PayoutView> {
    debug!("Fetching eligible payouts for pubkey: {}", pubkey);

    let entries = match state
        .coordinator
        .get_entries(pubkey.to_string(), SearchBy { event_ids: None })
        .await
    {
        Ok(e) => {
            debug!("Found {} entries for pubkey {}", e.len(), pubkey);
            e
        }
        Err(e) => {
            error!("Failed to fetch entries for payouts: {:?}", e);
            return vec![];
        }
    };

    let competitions = match state.coordinator.get_competitions().await {
        Ok(c) => {
            debug!("Found {} competitions", c.len());
            c
        }
        Err(e) => {
            error!("Failed to fetch competitions for payouts: {:?}", e);
            return vec![];
        }
    };

    let mut payouts = Vec::new();

    for entry in &entries {
        debug!(
            "Checking entry {} (event_id: {}, paid_out_at: {:?})",
            entry.id, entry.event_id, entry.paid_out_at
        );

        if entry.paid_out_at.is_some() {
            debug!("Skipping entry {} - already paid out", entry.id);
            continue;
        }

        if let Some(competition) = competitions.iter().find(|c| c.id == entry.event_id) {
            debug!(
                "Found competition {} for entry {} - attestation: {}, outcome_broadcasted_at: {:?}",
                competition.id,
                entry.id,
                competition.attestation.is_some(),
                competition.outcome_broadcasted_at
            );

            if competition.attestation.is_some() && competition.outcome_broadcasted_at.is_some() {
                if let Some(payout_amount) =
                    calculate_entry_payout(competition, &entry.ephemeral_pubkey)
                {
                    debug!(
                        "Entry {} is eligible for payout of {} sats",
                        entry.id, payout_amount
                    );
                    payouts.push(PayoutView {
                        competition_id: competition.id.to_string(),
                        entry_id: entry.id.to_string(),
                        status: "Eligible".to_string(),
                        payout_amount,
                    });
                } else {
                    debug!("Entry {} is not eligible for payout", entry.id);
                }
            }
        } else {
            debug!(
                "No matching competition found for entry {} (event_id: {})",
                entry.id, entry.event_id
            );
        }
    }

    debug!(
        "Returning {} eligible payouts for pubkey {}",
        payouts.len(),
        pubkey
    );
    payouts
}

/// Calculate the payout amount in sats for an entry based on the competition outcome.
/// Returns None if the entry is not a winner or the calculation cannot be performed.
fn calculate_entry_payout(
    competition: &crate::domain::Competition,
    ephemeral_pubkey_hex: &str,
) -> Option<u64> {
    let contract_params = competition.contract_parameters.as_ref()?;
    let outcome = competition.get_current_outcome().ok()?;
    let outcome_weights = contract_params.outcome_payouts.get(&outcome)?;
    let ephemeral_pubkey = Point::from_hex(ephemeral_pubkey_hex).ok()?;

    let player_weight = outcome_weights.iter().find_map(|(player_index, weight)| {
        let player = contract_params.players.get(*player_index)?;
        if player.pubkey == ephemeral_pubkey {
            Some(*weight)
        } else {
            None
        }
    })?;

    let total_pool_sats = contract_params.funding_value.to_sat();
    Some((total_pool_sats * player_weight) / 100)
}

async fn fetch_forecasts(state: &AppState, competition: &CompetitionView) -> Vec<StationForecast> {
    // Get full competition to access locations
    let competition_id = match Uuid::parse_str(&competition.id) {
        Ok(id) => id,
        Err(_) => return vec![],
    };

    let locations = match state.coordinator.get_competition(competition_id).await {
        Ok(comp) => comp.event_submission.locations,
        Err(_) => return vec![],
    };

    if locations.is_empty() {
        return vec![];
    }

    // Fetch forecasts and observations from oracle
    let station_ids: Vec<&str> = locations.iter().map(|s| s.as_str()).collect();
    let (forecasts, observations) = tokio::join!(
        fetch_oracle_forecasts(&state.oracle_url, &station_ids),
        fetch_oracle_observations(&state.oracle_url, &station_ids)
    );

    let forecasts = forecasts.unwrap_or_default();
    let observations = observations.unwrap_or_default();

    // Build weather context maps
    use std::collections::HashMap;
    let today = time::OffsetDateTime::now_utc();
    let today_str = today.date().to_string();

    // Index forecasts by station_id for today
    let mut forecast_map: HashMap<&str, &OracleForecast> = HashMap::new();
    for forecast in &forecasts {
        let date_part = forecast.date.split('T').next().unwrap_or(&forecast.date);
        if date_part == today_str {
            forecast_map.insert(forecast.station_id.as_str(), forecast);
        }
    }

    // Index observations by station_id
    let mut observation_map: HashMap<&str, &OracleObservation> = HashMap::new();
    for obs in &observations {
        observation_map.insert(obs.station_id.as_str(), obs);
    }

    locations
        .into_iter()
        .map(|station_id| {
            // Build weather context from oracle data
            let weather_context = {
                let forecast = forecast_map.get(station_id.as_str());
                let observation = observation_map.get(station_id.as_str());

                if forecast.is_some() || observation.is_some() {
                    Some(WeatherContext {
                        current_temp: None, // Current temp not available from observation endpoint
                        today_forecast_high: forecast.map(|f| f.temp_high as f64),
                        today_forecast_low: forecast.map(|f| f.temp_low as f64),
                        today_actual_high: observation.map(|o| o.temp_high),
                        today_actual_low: observation.map(|o| o.temp_low),
                    })
                } else {
                    None
                }
            };

            // Use forecast values for the competition picks (these are the "par" values)
            let forecast = forecast_map.get(station_id.as_str());

            StationForecast {
                station_id: station_id.clone(),
                station_name: get_station_name(&station_id),
                wind_speed: Some(ForecastValue {
                    value: 12.5, // Wind speed not in current forecast data
                    unit: "mph".to_string(),
                }),
                temp_high: Some(ForecastValue {
                    value: forecast.map(|f| f.temp_high as f64).unwrap_or(75.0),
                    unit: "°F".to_string(),
                }),
                temp_low: Some(ForecastValue {
                    value: forecast.map(|f| f.temp_low as f64).unwrap_or(58.0),
                    unit: "°F".to_string(),
                }),
                weather_context,
            }
        })
        .collect()
}

/// Oracle forecast response
#[derive(Debug, Clone, serde::Deserialize)]
struct OracleForecast {
    station_id: String,
    date: String,
    temp_high: i64,
    temp_low: i64,
}

/// Oracle observation response
#[derive(Debug, Clone, serde::Deserialize)]
struct OracleObservation {
    station_id: String,
    temp_high: f64,
    temp_low: f64,
}

async fn fetch_oracle_forecasts(
    oracle_url: &str,
    station_ids: &[&str],
) -> Result<Vec<OracleForecast>, anyhow::Error> {
    if station_ids.is_empty() {
        return Ok(vec![]);
    }

    let client = reqwest_middleware::reqwest::Client::new();

    // Fetch forecasts for today and tomorrow
    let today = time::OffsetDateTime::now_utc();
    let end_date = today + time::Duration::days(2);

    let start = today
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let end = end_date
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    let station_ids_param = station_ids.join(",");

    let response = client
        .get(format!(
            "{}/stations/forecasts?station_ids={}&start={}&end={}",
            oracle_url, station_ids_param, start, end
        ))
        .send()
        .await?;

    if response.status().is_success() {
        let forecasts: Vec<OracleForecast> = response.json().await?;
        Ok(forecasts)
    } else {
        Ok(vec![])
    }
}

async fn fetch_oracle_observations(
    oracle_url: &str,
    station_ids: &[&str],
) -> Result<Vec<OracleObservation>, anyhow::Error> {
    if station_ids.is_empty() {
        return Ok(vec![]);
    }

    let client = reqwest_middleware::reqwest::Client::new();

    // Fetch observations for today
    let today = time::OffsetDateTime::now_utc();
    let tomorrow = today + time::Duration::days(1);

    let start = today
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let end = tomorrow
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    let station_ids_param = station_ids.join(",");

    let response = client
        .get(format!(
            "{}/stations/observations?station_ids={}&start={}&end={}",
            oracle_url, station_ids_param, start, end
        ))
        .send()
        .await?;

    if response.status().is_success() {
        let observations: Vec<OracleObservation> = response.json().await?;
        Ok(observations)
    } else {
        Ok(vec![])
    }
}

fn get_station_name(station_id: &str) -> String {
    // Common airport codes to names
    match station_id {
        "KORD" => "Chicago O'Hare International".to_string(),
        "KJFK" => "John F. Kennedy International".to_string(),
        "KLAX" => "Los Angeles International".to_string(),
        "KEWR" => "Newark Liberty International".to_string(),
        "KATL" => "Hartsfield-Jackson Atlanta International".to_string(),
        "KDFW" => "Dallas/Fort Worth International".to_string(),
        "KDEN" => "Denver International".to_string(),
        "KSFO" => "San Francisco International".to_string(),
        _ => format!("Station {}", station_id),
    }
}

/// Oracle event response containing entries with scores
#[derive(Debug, Clone, serde::Deserialize)]
struct OracleEvent {
    entries: Vec<OracleEntry>,
}

/// Oracle entry with score
#[derive(Debug, Clone, serde::Deserialize)]
struct OracleEntry {
    id: Uuid,
    score: Option<i64>,
}

async fn fetch_leaderboard_scores(state: &AppState, competition_id: Uuid) -> Vec<EntryScore> {
    // Fetch event from oracle to get entries with scores
    let oracle_entries = fetch_oracle_event_entries(&state.oracle_url, competition_id).await;

    if oracle_entries.is_empty() {
        return vec![];
    }

    // Convert to EntryScore and sort by score (highest first), then by entry_id for ties
    let mut scores: Vec<EntryScore> = oracle_entries
        .iter()
        .map(|entry| EntryScore {
            rank: 0, // Will be assigned after sorting
            entry_id: entry.id.to_string(),
            score: entry.score.unwrap_or(0) as i32,
        })
        .collect();

    // Sort by score descending, then by entry_id ascending (earlier entries win ties)
    scores.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.entry_id.cmp(&b.entry_id))
    });

    // Assign ranks after sorting
    for (idx, score) in scores.iter_mut().enumerate() {
        score.rank = idx + 1;
    }

    scores
}

async fn fetch_oracle_event_entries(oracle_url: &str, event_id: Uuid) -> Vec<OracleEntry> {
    let client = reqwest_middleware::reqwest::Client::new();

    let response = match client
        .get(format!("{}/oracle/events/{}", oracle_url, event_id))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(_) => return vec![],
    };

    if response.status().is_success() {
        match response.json::<OracleEvent>().await {
            Ok(event) => event.entries,
            Err(_) => vec![],
        }
    } else {
        vec![]
    }
}
