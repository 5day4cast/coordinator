use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use maud::html;
use uuid::Uuid;

use crate::{
    api::extractors::{AuthError, NostrAuth},
    domain::SearchBy,
    startup::AppState,
    templates::{
        fragments::{
            entry_form::{entry_form, ForecastValue, StationForecast},
            leaderboard::{leaderboard, leaderboard_row, EntryScore},
        },
        layouts::base::{base, PageConfig},
        pages::{
            competitions::{competitions_page, CompetitionView},
            entries::{entries_page, no_entries, EntryView},
            payouts::{payouts_page, PayoutView},
        },
    },
};

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
pub async fn competitions_fragment(State(state): State<Arc<AppState>>) -> Html<String> {
    let competitions = fetch_competitions(&state).await;
    Html(competitions_page(&competitions).into_string())
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
    HtmlNostrAuth(NostrAuth { pubkey, .. }): HtmlNostrAuth,
) -> Html<String> {
    let entries = fetch_user_entries(&state, &pubkey.to_hex()).await;
    let markup = if entries.is_empty() {
        no_entries()
    } else {
        entries_page(&entries)
    };
    Html(markup.into_string())
}

/// Payouts page fragment (requires auth, returns HTML error on auth failure)
pub async fn payouts_fragment(
    State(state): State<Arc<AppState>>,
    HtmlNostrAuth(NostrAuth { pubkey, .. }): HtmlNostrAuth,
) -> Html<String> {
    let payouts = fetch_eligible_payouts(&state, &pubkey.to_hex()).await;
    Html(payouts_page(&payouts).into_string())
}

/// Entry form for a competition
pub async fn entry_form_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Html<String> {
    // Get competition details
    let competitions = fetch_competitions(&state).await;
    let competition = competitions
        .iter()
        .find(|c| c.id == competition_id.to_string());

    let markup = match competition {
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
    Html(markup.into_string())
}

/// Leaderboard for a competition
pub async fn leaderboard_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Html<String> {
    let scores = fetch_leaderboard_scores(&state, competition_id).await;
    Html(leaderboard(&competition_id.to_string(), &scores).into_string())
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
    State(_state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
) -> Html<String> {
    // TODO: Fetch actual entry details
    Html(
        html! {
            div {
                h4 class="title is-4" { "Entry Details" }
                p { "Entry ID: " (entry_id) }
                // TODO: Add actual entry observation data
            }
        }
        .into_string(),
    )
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
                }
            })
            .collect(),
        Err(_) => vec![],
    }
}

fn determine_competition_status(competition: &crate::domain::Competition) -> String {
    use time::OffsetDateTime;

    let now = OffsetDateTime::now_utc();
    let start = competition.event_submission.start_observation_date;
    let end = competition.event_submission.end_observation_date;
    let signing = competition.event_submission.signing_date;

    if competition.attestation.is_some() {
        "Completed".to_string()
    } else if now < start {
        "Registration".to_string()
    } else if now < end {
        "Live".to_string()
    } else if now < signing {
        "Setup".to_string()
    } else {
        "Signing".to_string()
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
    // Get user entries that are signed and have attestation but not paid out
    let entries = match state
        .coordinator
        .get_entries(pubkey.to_string(), SearchBy { event_ids: None })
        .await
    {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let competitions = match state.coordinator.get_competitions().await {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut payouts = Vec::new();

    for entry in entries {
        // Must be signed but not paid out
        if entry.signed_at.is_none() || entry.paid_at.is_some() {
            continue;
        }

        // Find competition and check if it has attestation
        if let Some(competition) = competitions.iter().find(|c| c.id == entry.event_id) {
            if competition.attestation.is_some() {
                // Calculate payout amount based on outcome
                // For now, just show that it's eligible
                let payout_amount = competition.event_submission.entry_fee as u64; // Placeholder

                payouts.push(PayoutView {
                    competition_id: competition.id.to_string(),
                    entry_id: entry.id.to_string(),
                    status: "Eligible".to_string(),
                    payout_amount,
                });
            }
        }
    }

    payouts
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

    // TODO: Fetch actual forecasts from oracle
    // For now, return stub data for each location
    locations
        .into_iter()
        .map(|station_id| StationForecast {
            station_id: station_id.clone(),
            station_name: get_station_name(&station_id),
            wind_speed: Some(ForecastValue {
                value: 12.5,
                unit: "mph".to_string(),
            }),
            temp_high: Some(ForecastValue {
                value: 75.0,
                unit: "°F".to_string(),
            }),
            temp_low: Some(ForecastValue {
                value: 58.0,
                unit: "°F".to_string(),
            }),
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

async fn fetch_leaderboard_scores(state: &AppState, competition_id: Uuid) -> Vec<EntryScore> {
    // TODO: Implement actual score calculation
    // This will be ported from leader_board.js calculateScores()
    // For now, return placeholder data

    match state.coordinator.get_competition(competition_id).await {
        Ok(_competition) => {
            // Get all entries for this competition
            // TODO: Calculate actual scores based on oracle observations
            vec![]
        }
        Err(_) => vec![],
    }
}
