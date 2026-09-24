use std::sync::Arc;

use dlctix::secp::Point;
use log::{debug, error, warn};
use nostr::ToBech32;

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
        leaderboard::{Metric, PickProgress, FIRST_READ_WAIT},
        Competition, SearchBy,
    },
    infra::oracle::ValueOptions,
    startup::AppState,
    templates::{
        fragments::{
            competition_row::competition_row,
            entry_form::{entry_form, ForecastValue, StationForecast, WeatherContext},
            leaderboard::{leaderboard, leaderboard_row, EntryScore, LeaderboardInfo},
        },
        layouts::base::{base, PageConfig},
        pages::{
            competitions::{competitions_page, CompetitionView},
            entries::{entries_page, no_entries},
            payouts::{payouts_page, PayoutView},
        },
        shared_map::StationMarker,
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
            wasm_version: &state.wasm_version,
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
        wasm_version: &state.wasm_version,
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
                (competition_row(comp))
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
    let entries = state
        .coordinator
        .get_user_entry_views(pubkey.to_hex())
        .await
        .unwrap_or_default();
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
    let lightning_address = match pubkey.to_bech32() {
        Ok(npub) => state
            .users_info
            .login(npub)
            .await
            .ok()
            .and_then(|user| user.lightning_address),
        Err(_) => None,
    };
    let content = payouts_page(&payouts, lightning_address.as_deref());
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
            // Fetch forecasts and station locations in parallel
            let (forecasts, mut markers) = tokio::join!(
                fetch_forecasts(&state, comp),
                fetch_station_markers(&state, competition_id)
            );

            // Enrich markers with weather data from forecasts
            for marker in markers.iter_mut() {
                if let Some(forecast) = forecasts.iter().find(|f| f.station_id == marker.station_id)
                {
                    use crate::templates::shared_map::StationWeather;
                    marker.weather = Some(StationWeather {
                        forecast_high: forecast.temp_high.as_ref().map(|v| v.value),
                        forecast_low: forecast.temp_low.as_ref().map(|v| v.value),
                        actual_high: forecast
                            .weather_context
                            .as_ref()
                            .and_then(|ctx| ctx.today_actual_high),
                        actual_low: forecast
                            .weather_context
                            .as_ref()
                            .and_then(|ctx| ctx.today_actual_low),
                        wind_speed: forecast.wind_speed.as_ref().map(|v| v.value),
                    });
                }
            }

            entry_form(comp, &forecasts, &markers)
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
    let (info, scores) = match state.coordinator.get_competition(competition_id).await {
        Ok(comp) => {
            let status = determine_competition_status(&comp);
            let scores = leaderboard_scores(&state, &comp).await;
            let info = LeaderboardInfo {
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
            };
            (info, scores)
        }
        Err(_) => (
            LeaderboardInfo {
                competition_id: competition_id.to_string(),
                start_time: String::new(),
                end_time: String::new(),
                status: "Unknown".to_string(),
            },
            vec![],
        ),
    };

    let content = leaderboard(&info, &scores);
    render_fragment(&headers, &state, "Leaderboard - Fantasy Weather", content)
}

/// Leaderboard rows fragment (for auto-refresh)
pub async fn leaderboard_rows_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Html<String> {
    let scores = match state.coordinator.get_competition(competition_id).await {
        Ok(competition) => leaderboard_scores(&state, &competition).await,
        Err(_) => vec![],
    };
    Html(
        html! {
            @for score in &scores {
                (leaderboard_row(score))
            }
        }
        .into_string(),
    )
}

/// Entry detail fragment (for modal). Picks are public once the observation window opens.
pub async fn entry_detail_fragment(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
) -> Html<String> {
    let entry = state
        .coordinator
        .get_entry_by_id(entry_id)
        .await
        .ok()
        .flatten();
    let competition = match &entry {
        Some(entry) => state.coordinator.get_competition(entry.event_id).await.ok(),
        None => None,
    };
    let board = match &competition {
        Some(competition) => state
            .leaderboards
            .get(competition, FIRST_READ_WAIT)
            .await
            .inspect_err(|e| warn!("Leaderboard for entry {entry_id}: {e}"))
            .ok(),
        None => None,
    };
    let row = board
        .as_ref()
        .and_then(|board| board.rows.iter().find(|row| row.entry_id == entry_id));
    let window_open = competition.as_ref().is_some_and(|competition| {
        time::OffsetDateTime::now_utc() >= competition.event_submission.start_observation_date
    });
    let has_weather = board
        .as_ref()
        .is_some_and(|board| board.weather_fetched_at.is_some());
    let provisional = board.as_ref().is_none_or(|board| board.provisional);

    Html(
        html! {
            div {
                // Compact header with entry ID and total score
                div class="entry-detail-header" {
                    div {
                        h5 class="title is-5 mb-1" { "Picks & Score" }
                        span class="entry-id" { (format!("#{}", &entry_id.to_string()[..8])) }
                    }
                    @if let Some(row) = row.filter(|row| has_weather && !row.picks.is_empty()) {
                        span class="entry-detail-score" { "🏆 " (row.standing.score) " pts" }
                    }
                }

                @match row {
                    None => p class="has-text-grey" { "Entry details not available" },
                    Some(row) if row.picks.is_empty() => {
                        @if window_open {
                            p class="has-text-grey" { "No picks recorded" }
                        } @else {
                            p class="has-text-grey" { "Picks are shown once the observation window opens" }
                        }
                    },
                    Some(row) => {
                        @if !has_weather {
                            p class="entry-pending-msg mb-3" { "Weather data unavailable" }
                        } @else if provisional {
                            p class="entry-pending-msg mb-3" { "Provisional: the points if the window ended now" }
                        }
                        @for pick in &row.picks {
                            (pick_card(pick, has_weather))
                        }
                    },
                }
            }
        }
        .into_string(),
    )
}

/// Render a compact pick card
fn pick_card(pick: &PickProgress, show_points: bool) -> Markup {
    let (icon, label, unit) = match pick.metric {
        Metric::TempHigh => ("🟡", "High", "°F"),
        Metric::TempLow => ("🔵", "Low", "°F"),
        Metric::WindSpeed => ("💨", "Wind", " kt"),
    };
    html! {
        div class="entry-pick-card" {
            span class="entry-pick-icon" { (icon) }
            div class="entry-pick-info" {
                div class="entry-pick-label" { (label) " · " (pick.station_id) }
                @if pick.forecast.is_some() || pick.observed.is_some() {
                    div class="entry-pick-values" {
                        span class="entry-pick-forecast" {
                            "Forecasted: "
                            strong { (format_value(pick.forecast, unit)) }
                        }
                        @if pick.observed.is_some() {
                            span class="entry-pick-observed" {
                                "Observed: "
                                strong { (format_value(pick.observed, unit)) }
                            }
                        }
                    }
                }
            }
            div class="entry-pick-result" {
                span class=(pick_choice_class(&pick.pick)) { (format_pick_short(&pick.pick)) }
                @if show_points {
                    div class=(points_class(pick.points)) { (pick.points) " pts" }
                }
            }
        }
    }
}

/// Format a numeric value with unit for display
fn format_value(value: Option<f64>, unit: &str) -> String {
    match value {
        Some(v) => format!("{:.1}{}", v, unit),
        None => "-".to_string(),
    }
}

/// Format a pick as short text
fn format_pick_short(pick: &ValueOptions) -> &'static str {
    match pick {
        ValueOptions::Over => "Over ↑",
        ValueOptions::Par => "Par →",
        ValueOptions::Under => "Under ↓",
    }
}

/// Get CSS class for pick choice display
fn pick_choice_class(pick: &ValueOptions) -> &'static str {
    match pick {
        ValueOptions::Over => "entry-pick-choice pick-over",
        ValueOptions::Par => "entry-pick-choice pick-par",
        ValueOptions::Under => "entry-pick-choice pick-under",
    }
}

/// Get CSS class for points display
fn points_class(points: u64) -> &'static str {
    if points >= 20 {
        "entry-pick-points points-high"
    } else if points > 0 {
        "entry-pick-points points-mid"
    } else {
        "entry-pick-points points-zero"
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
                    ticket_price: c.calculate_invoice_amount(),
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

fn determine_competition_status(competition: &Competition) -> String {
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

        let policy = state
            .coordinator
            .competition_store
            .entry_payout_policy(entry.id)
            .await
            .ok()
            .flatten()
            .and_then(|json| {
                serde_json::from_str::<coordinator_escrow::authorization::PayoutPolicy>(&json).ok()
            });
        if entry.paid_out_at.is_some() && policy.is_none() {
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
                    let window_closed = state
                        .coordinator
                        .competition_store
                        .payout_window_is_closed(competition.id)
                        .await
                        .unwrap_or(true)
                        || competition.delta_broadcasted_at.is_some()
                        || competition.expiry_broadcasted_at.is_some()
                        || competition.completed_at.is_some()
                        || competition.cancelled_at.is_some();
                    payouts.push(PayoutView {
                        competition_id: competition.id.to_string(),
                        entry_id: entry.id.to_string(),
                        status: state
                            .coordinator
                            .competition_store
                            .payout_job_status(entry.id)
                            .await
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| {
                                if window_closed {
                                    "On-chain settlement".into()
                                } else if policy
                                    .as_ref()
                                    .is_some_and(|p| p.automatic_lightning_address.is_some())
                                {
                                    "Queued automatically".into()
                                } else {
                                    "Awaiting invoice".into()
                                }
                            }),
                        payout_amount,
                        automatic_lightning_address: policy
                            .as_ref()
                            .and_then(|p| p.automatic_lightning_address.clone()),
                        allow_invoice_fallback: policy
                            .as_ref()
                            .is_none_or(|p| p.allow_invoice_fallback)
                            && !window_closed
                            && entry.paid_out_at.is_none(),
                        escrow_enabled: policy.is_some(),
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
fn calculate_entry_payout(competition: &Competition, ephemeral_pubkey_hex: &str) -> Option<u64> {
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

/// Fetch station locations from the Oracle and build map markers for a competition's stations
async fn fetch_station_markers(state: &AppState, competition_id: Uuid) -> Vec<StationMarker> {
    let locations = match state.coordinator.get_competition(competition_id).await {
        Ok(comp) => comp.event_submission.locations,
        Err(_) => return vec![],
    };

    if locations.is_empty() {
        return vec![];
    }

    let stations = state.leaderboards.stations(FIRST_READ_WAIT).await;
    let Some(all_stations) = stations.value() else {
        warn!("No stations from the oracle for the map yet");
        return vec![];
    };

    let location_set: std::collections::HashSet<&str> =
        locations.iter().map(|s| s.as_str()).collect();

    all_stations
        .iter()
        .filter(|s| location_set.contains(s.station_id.as_str()))
        .filter_map(|s| {
            StationMarker::new(
                s.station_id.clone(),
                s.station_name.clone(),
                s.state.clone(),
                s.latitude,
                s.longitude,
            )
        })
        .collect()
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

/// The leaderboard's rows, from the cached leaderboard: the page never waits on the oracle.
async fn leaderboard_scores(state: &AppState, competition: &Competition) -> Vec<EntryScore> {
    match state.leaderboards.get(competition, FIRST_READ_WAIT).await {
        Ok(board) => board
            .rows
            .iter()
            .map(|row| EntryScore {
                rank: row.standing.rank,
                entry_id: row.entry_id.to_string(),
                username: row.player.name.clone(),
                score: i32::try_from(row.standing.score).unwrap_or(i32::MAX),
            })
            .collect(),
        Err(e) => {
            error!("Leaderboard for competition {}: {e}", competition.id);
            vec![]
        }
    }
}
