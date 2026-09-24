use std::sync::Arc;

use dlctix::secp::Point;
use log::{debug, error};
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
        leaderboard::{CompetitionWeather, Leaderboard},
        scoring::{calculate_option_score, Forecast, Observation},
        Competition, SearchBy, UserEntry,
    },
    infra::oracle::ValueOptions,
    startup::AppState,
    templates::{
        admin::dashboard::Station,
        fragments::{
            competition_row::competition_row,
            entry_form::{entry_form, ForecastValue, StationForecast},
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
    let content = match state.coordinator.get_competition(competition_id).await {
        Ok(competition) => {
            // Both come from caches the leaderboard refresher keeps warm.
            let (weather, stations) = tokio::join!(
                state.leaderboards.weather(&competition),
                state.leaderboards.stations()
            );
            let stations = stations
                .as_ref()
                .map(|stations| stations.value.as_slice())
                .unwrap_or_default();
            let forecasts = station_forecasts(
                &competition,
                weather.as_ref().map(|weather| &weather.value),
                stations,
            );
            let mut markers = station_markers(&competition, stations);

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

            entry_form(&competition_view(&competition), &forecasts, &markers)
        }
        Err(_) => {
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
    let (board, competition) = tokio::join!(
        state.leaderboards.get(competition_id),
        state.coordinator.get_competition(competition_id)
    );
    let scores = entry_scores(board.ok().as_ref());

    // Fetch competition details for observation period
    let info = match competition {
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
    let board = state.leaderboards.get(competition_id).await.ok();
    let scores = entry_scores(board.as_ref());
    Html(
        html! {
            @for score in &scores {
                (leaderboard_row(score))
            }
        }
        .into_string(),
    )
}

/// Leaderboard rows as the current leaderboard template shows them: standing, player, score.
fn entry_scores(board: Option<&Leaderboard>) -> Vec<EntryScore> {
    board
        .map(|board| {
            board
                .rows
                .iter()
                .map(|row| EntryScore {
                    rank: row.standing.rank,
                    entry_id: row.entry_id.to_string(),
                    username: row.player.name.clone(),
                    score: i32::try_from(row.standing.score).unwrap_or(i32::MAX),
                })
                .collect()
        })
        .unwrap_or_default()
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
                // Compact header with entry ID and total score
                div class="entry-detail-header" {
                    div {
                        h5 class="title is-5 mb-1" { "Picks & Score" }
                        span class="entry-id" { (format!("#{}", &entry_id.to_string()[..8])) }
                    }
                    @if entry.is_some() {
                        @if let Some(ref data) = weather_data {
                            @if let Some(total_score) = data.total_score {
                                span class="entry-detail-score" { "🏆 " (total_score) " pts" }
                            }
                        }
                    }
                }

                @if let Some(entry) = entry {
                    @if entry.entry_submission.expected_observations.is_empty() {
                        p class="has-text-grey" { "No picks recorded" }
                    } @else if let Some(ref data) = weather_data {
                        @if data.total_score.is_none() {
                            p class="entry-pending-msg mb-3" {
                                "Scores available after observations are recorded"
                            }
                        }
                        @for obs in &entry.entry_submission.expected_observations {
                            @let forecast = data.forecasts.get(&obs.stations);
                            @let observation = data.observations.get(&obs.stations);
                            // Temp High card
                            @if let Some(pick) = &obs.temp_high {
                                @let forecast_val = forecast.and_then(|f| f.temp_high);
                                @let obs_val = observation.and_then(|o| o.temp_high);
                                (pick_card(&PickCardData { icon: "🟡", label: "High", station_id: &obs.stations, forecast_val, obs_val, unit: "°F", pick, show_points: data.has_observations }))
                            }
                            // Temp Low card
                            @if let Some(pick) = &obs.temp_low {
                                @let forecast_val = forecast.and_then(|f| f.temp_low);
                                @let obs_val = observation.and_then(|o| o.temp_low);
                                (pick_card(&PickCardData { icon: "🔵", label: "Low", station_id: &obs.stations, forecast_val, obs_val, unit: "°F", pick, show_points: data.has_observations }))
                            }
                            // Wind Speed card
                            @if let Some(pick) = &obs.wind_speed {
                                @let forecast_val = forecast.and_then(|f| f.wind_speed);
                                @let obs_val = observation.and_then(|o| o.wind_speed);
                                (pick_card(&PickCardData { icon: "💨", label: "Wind", station_id: &obs.stations, forecast_val, obs_val, unit: " mph", pick, show_points: data.has_observations }))
                            }
                        }
                    } @else {
                        // No weather data - show picks only
                        p class="entry-pending-msg mb-3" { "Weather data unavailable" }
                        @for obs in &entry.entry_submission.expected_observations {
                            @if let Some(pick) = &obs.temp_high {
                                (pick_card(&PickCardData { icon: "🟡", label: "High", station_id: &obs.stations, forecast_val: None, obs_val: None, unit: "°F", pick, show_points: false }))
                            }
                            @if let Some(pick) = &obs.temp_low {
                                (pick_card(&PickCardData { icon: "🔵", label: "Low", station_id: &obs.stations, forecast_val: None, obs_val: None, unit: "°F", pick, show_points: false }))
                            }
                            @if let Some(pick) = &obs.wind_speed {
                                (pick_card(&PickCardData { icon: "💨", label: "Wind", station_id: &obs.stations, forecast_val: None, obs_val: None, unit: " mph", pick, show_points: false }))
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

/// Data for rendering a pick card
struct PickCardData<'a> {
    icon: &'a str,
    label: &'a str,
    station_id: &'a str,
    forecast_val: Option<f64>,
    obs_val: Option<f64>,
    unit: &'a str,
    pick: &'a ValueOptions,
    show_points: bool,
}

/// Render a compact pick card
fn pick_card(data: &PickCardData) -> Markup {
    let points = if data.show_points {
        Some(calculate_option_score(
            data.forecast_val,
            data.obs_val,
            data.pick,
        ))
    } else {
        None
    };

    html! {
        div class="entry-pick-card" {
            span class="entry-pick-icon" { (data.icon) }
            div class="entry-pick-info" {
                div class="entry-pick-label" { (data.label) " · " (data.station_id) }
                @if data.forecast_val.is_some() || data.obs_val.is_some() {
                    div class="entry-pick-values" {
                        span class="entry-pick-forecast" {
                            "Forecasted: "
                            strong { (format_value(data.forecast_val, data.unit)) }
                        }
                        @if data.obs_val.is_some() {
                            span class="entry-pick-observed" {
                                "Observed: "
                                strong { (format_value(data.obs_val, data.unit)) }
                            }
                        }
                    }
                }
            }
            div class="entry-pick-result" {
                span class=(pick_choice_class(data.pick)) { (format_pick_short(data.pick)) }
                @if let Some(pts) = points {
                    div class=(points_class(pts)) { (pts) " pts" }
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
fn points_class(points: i32) -> &'static str {
    if points >= 20 {
        "entry-pick-points points-high"
    } else if points > 0 {
        "entry-pick-points points-mid"
    } else {
        "entry-pick-points points-zero"
    }
}

/// Entry weather data for display
struct EntryWeatherData {
    forecasts: std::collections::HashMap<String, Forecast>,
    observations: std::collections::HashMap<String, Observation>,
    total_score: Option<i32>,
    has_observations: bool,
}

/// Forecast and observations for an entry's picks, from the leaderboard's cached weather.
///
/// Values are given as the oracle compares them (temperatures in whole degrees, wind in exact
/// knots), so each card's points are the points the oracle awards.
async fn fetch_entry_weather_data(state: &AppState, entry: &UserEntry) -> Option<EntryWeatherData> {
    use crate::domain::leaderboard::{progress, Metric};
    use std::collections::HashMap;

    let competition = state
        .coordinator
        .get_competition(entry.event_id)
        .await
        .ok()?;
    let choices = &entry.entry_submission.expected_observations;
    if choices.is_empty() {
        return None;
    }
    let weather = state.leaderboards.weather(&competition).await?;
    let weather = &weather.value;
    let started =
        time::OffsetDateTime::now_utc() >= competition.event_submission.start_observation_date;
    let as_scored = |metric: Metric, value: Option<f64>| match metric.par_rule() {
        progress::ParRule::Rounded => value.map(f64::round),
        progress::ParRule::Exact => value,
    };

    let mut forecast_map: HashMap<String, Forecast> = HashMap::new();
    let mut observation_map: HashMap<String, Observation> = HashMap::new();
    for choice in choices {
        let station = choice.stations.as_str();
        let forecast = |metric| as_scored(metric, weather.forecast(station, metric));
        forecast_map.insert(
            station.to_owned(),
            Forecast {
                station_id: station.to_owned(),
                temp_high: forecast(Metric::TempHigh),
                temp_low: forecast(Metric::TempLow),
                wind_speed: forecast(Metric::WindSpeed),
            },
        );
        let observed = |metric| as_scored(metric, weather.observed(station, metric));
        let observation = Observation {
            station_id: station.to_owned(),
            temp_high: observed(Metric::TempHigh),
            temp_low: observed(Metric::TempLow),
            wind_speed: observed(Metric::WindSpeed),
        };
        let any = observation.temp_high.is_some()
            || observation.temp_low.is_some()
            || observation.wind_speed.is_some();
        if started && any {
            observation_map.insert(station.to_owned(), observation);
        }
    }

    let has_observations = !observation_map.is_empty();
    // The oracle's score once it has attested; until then, what the picks would score now.
    let total_score = has_observations.then(|| {
        if weather.event.is_final() {
            let score = weather.event.scores.get(&entry.entry_submission.id);
            return score.copied().unwrap_or(0) as i32;
        }
        choices
            .iter()
            .flat_map(|choice| {
                [
                    (Metric::TempHigh, &choice.temp_high),
                    (Metric::TempLow, &choice.temp_low),
                    (Metric::WindSpeed, &choice.wind_speed),
                ]
                .into_iter()
                .filter_map(|(metric, pick)| {
                    let pick = pick.as_ref()?;
                    Some(progress::points(
                        pick,
                        metric,
                        weather.forecast(&choice.stations, metric),
                        weather.observed(&choice.stations, metric),
                    ))
                })
            })
            .sum::<u64>() as i32
    });

    Some(EntryWeatherData {
        forecasts: forecast_map,
        observations: observation_map,
        total_score,
        has_observations,
    })
}

// Helper functions

async fn fetch_competitions(state: &AppState) -> Vec<CompetitionView> {
    match state.coordinator.get_competitions().await {
        Ok(competitions) => competitions.iter().map(competition_view).collect(),
        Err(_) => vec![],
    }
}

fn competition_view(c: &Competition) -> CompetitionView {
    let status = determine_competition_status(c);
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

/// Map markers for a competition's stations.
fn station_markers(competition: &Competition, stations: &[Station]) -> Vec<StationMarker> {
    let locations: std::collections::HashSet<&str> = competition
        .event_submission
        .locations
        .iter()
        .map(String::as_str)
        .collect();
    stations
        .iter()
        .filter(|s| locations.contains(s.station_id.as_str()))
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

/// Each station's forecasts as the oracle will compare them, for the entry form. Until the
/// window opens a forecast can still change; one the oracle has not published is left out.
fn station_forecasts(
    competition: &Competition,
    weather: Option<&CompetitionWeather>,
    stations: &[Station],
) -> Vec<StationForecast> {
    use crate::domain::leaderboard::Metric;
    competition
        .event_submission
        .locations
        .iter()
        .map(|station_id| {
            let forecast = |metric: Metric| {
                let value = weather?.forecast(station_id, metric)?;
                Some(ForecastValue {
                    value,
                    unit: metric.unit().to_string(),
                })
            };
            StationForecast {
                station_id: station_id.clone(),
                station_name: stations
                    .iter()
                    .find(|s| &s.station_id == station_id)
                    .map(|s| s.station_name.clone())
                    .unwrap_or_else(|| get_station_name(station_id)),
                wind_speed: forecast(Metric::WindSpeed),
                temp_high: forecast(Metric::TempHigh),
                temp_low: forecast(Metric::TempLow),
                weather_context: None,
            }
        })
        .collect()
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
