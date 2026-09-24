//! Public pages. Each handler returns just its content to htmx requests and
//! the whole page, layout included, to direct visits and reloads.

use std::sync::Arc;

use dlctix::secp::Point;
use log::{debug, error, warn};
use nostr::ToBech32;

use axum::{
    extract::{FromRequestParts, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
};
use maud::{html, Markup};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use super::oracle_view::{self, OracleEventView};
use crate::{
    api::extractors::NostrAuth,
    domain::{
        scoring::{calculate_option_score, Forecast, Observation},
        Competition, SearchBy,
    },
    startup::AppState,
    templates::{
        format::short_npub,
        fragments::{
            entry_form::{entry_form, payout_line, PayoutDestination, StationForecast},
            leaderboard::{leaderboard, leaderboard_rows, shared_ranks, EntryScore, LeaderboardRow},
            picks::{picks_detail, Metric, PickView, Readings},
        },
        layouts::base::{base, PageConfig},
        pages::{
            competitions::{competitions_page, CompetitionView, ListOptions, Phase},
            entries::{entries_page, sign_in_required, EntryRow},
            payouts::{payouts_page, PayoutView},
        },
        shared_map::{lat_lon_to_svg, StationPin},
    },
};

/// Whether to answer with content only: htmx swaps it into the open page.
/// History restores (Back after the cache expired) need the whole page.
fn is_fragment(headers: &HeaderMap) -> bool {
    headers.contains_key("HX-Request") && !headers.contains_key("HX-History-Restore-Request")
}

/// Who may keep a copy of a response.
#[derive(Clone, Copy)]
enum Caching {
    /// Anyone's view of public data: revalidate before reuse.
    Public,
    /// One account's data: never store it.
    Private,
}

/// Content for htmx, or the whole page for a direct visit.
fn page(headers: &HeaderMap, state: &AppState, title: &str, content: Markup, caching: Caching) -> Response {
    let body = if is_fragment(headers) {
        // htmx takes the document title from a <title> in the response; only
        // navigations of the main content should change it.
        let navigation = headers
            .get("HX-Target")
            .is_some_and(|target| target == "main-content");
        html! {
            @if navigation { title { (title) } }
            (content)
        }
        .into_string()
    } else {
        let network = state.bitcoin.get_network().to_string();
        let config = PageConfig {
            title,
            api_base: &state.remote_url,
            oracle_base: &state.oracle_url,
            network: &network,
            wasm_version: &state.wasm_version,
        };
        base(&config, content).into_string()
    };
    let mut response = Html(body).into_response();
    let headers = response.headers_mut();
    // The same address answers with a fragment or a page; caches must not mix them.
    headers.insert(header::VARY, HeaderValue::from_static("HX-Request"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(match caching {
            Caching::Public => "no-cache",
            Caching::Private => "private, no-store",
        }),
    );
    response
}

/// A NIP-98 signature when the request carries a valid one. Pages that work
/// for everyone use it to personalize; an invalid header counts as absent.
pub struct MaybeAuth(pub Option<NostrAuth>);

impl<S> FromRequestParts<S> for MaybeAuth
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        if !parts.headers.contains_key(header::AUTHORIZATION) {
            return Ok(MaybeAuth(None));
        }
        Ok(MaybeAuth(
            NostrAuth::from_request_parts(parts, state)
                .await
                .inspect_err(|error| debug!("ignoring page authorization: {error}"))
                .ok(),
        ))
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

async fn competition_views(state: &AppState, now: OffsetDateTime) -> Vec<CompetitionView> {
    match state.coordinator.get_competitions().await {
        Ok(competitions) => competitions
            .iter()
            .map(|competition| CompetitionView::new(competition, now))
            .collect(),
        Err(error) => {
            error!("failed to load competitions: {error}");
            vec![]
        }
    }
}

/// `?page=1&cancelled=1` on the competitions list.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    page: Option<usize>,
    cancelled: Option<String>,
}

impl From<ListQuery> for ListOptions {
    fn from(query: ListQuery) -> Self {
        ListOptions {
            page: query.page.unwrap_or(0),
            show_cancelled: query.cancelled.is_some_and(|value| value == "1" || value == "true"),
        }
    }
}

/// Public home page - competitions list
pub async fn public_page_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    competitions_fragment(State(state), Query(query), headers).await
}

/// Competitions page, grouped, with paging and the cancelled toggle.
pub async fn competitions_fragment(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let now = now();
    let competitions = competition_views(&state, now).await;
    let content = competitions_page(&competitions, query.into(), now);
    page(&headers, &state, "Fantasy Weather: call the weather, win the pot", content, Caching::Public)
}

/// Entries page (requires auth; signed-out visitors get the log-in prompt)
pub async fn entries_fragment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    MaybeAuth(auth): MaybeAuth,
) -> Response {
    let title = "Your entries - Fantasy Weather";
    let Some(NostrAuth { pubkey, .. }) = auth else {
        return signed_out(&headers, &state, title, "/entries", "your entries");
    };
    let now = now();
    let (entries, competitions) = tokio::join!(
        state.coordinator.get_user_entry_views(pubkey.to_hex()),
        competition_views(&state, now)
    );
    let entries = entries.unwrap_or_default();
    let rows: Vec<EntryRow> = entries
        .iter()
        .map(|entry| EntryRow {
            entry,
            competition: competitions
                .iter()
                .find(|competition| competition.id == entry.competition_id),
        })
        .collect();
    let open = competitions
        .iter()
        .filter(|competition| competition.can_enter)
        .min_by_key(|competition| competition.start);
    page(&headers, &state, title, entries_page(&rows, open), Caching::Private)
}

/// Payouts page (requires auth; signed-out visitors get the log-in prompt)
pub async fn payouts_fragment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    MaybeAuth(auth): MaybeAuth,
) -> Response {
    let title = "Payouts - Fantasy Weather";
    let Some(NostrAuth { pubkey, .. }) = auth else {
        return signed_out(&headers, &state, title, "/payouts", "your payouts");
    };
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
    page(&headers, &state, title, content, Caching::Private)
}

/// An account page opened without a signature: a prompt that loads the page
/// once the visitor logs in. htmx requests get 401, which it does not swap.
fn signed_out(headers: &HeaderMap, state: &AppState, title: &str, path: &str, what: &str) -> Response {
    let mut response = page(headers, state, title, sign_in_required(path, what), Caching::Private);
    if is_fragment(headers) {
        *response.status_mut() = StatusCode::UNAUTHORIZED;
    }
    response
}

/// Where the signed-in player's winnings go, from their account.
async fn payout_destination(state: &AppState, auth: Option<&NostrAuth>) -> PayoutDestination {
    let Some(auth) = auth else {
        return PayoutDestination::LoggedOut;
    };
    let Ok(npub) = auth.pubkey.to_bech32();
    match state.users_info.login(npub).await {
        Ok(user) => match user.lightning_address {
            Some(address) if !address.is_empty() => PayoutDestination::Address(address),
            _ => PayoutDestination::NoAddress,
        },
        Err(error) => {
            warn!("profile lookup for the entry form failed: {error}");
            PayoutDestination::LoggedOut
        }
    }
}

fn not_found(headers: &HeaderMap, state: &AppState, what: &str) -> Response {
    let content = html! {
        div class="notification is-warning" {
            (what) " not found. "
            a href="/competitions" hx-get="/competitions" hx-target="#main-content" hx-push-url="true" {
                "See all competitions"
            }
        }
    };
    let mut response = page(headers, state, "Not found - Fantasy Weather", content, Caching::Public);
    if !is_fragment(headers) {
        *response.status_mut() = StatusCode::NOT_FOUND;
    }
    response
}

/// Each station's forecasts as the oracle recorded them for this competition.
fn station_forecasts(
    locations: &[String],
    event: &OracleEventView,
    stations: &std::collections::HashMap<String, crate::templates::admin::dashboard::Station>,
) -> Vec<StationForecast> {
    locations
        .iter()
        .map(|station_id| StationForecast {
            station_id: station_id.clone(),
            station_name: oracle_view::station_name(stations, station_id),
            forecasts: Metric::ALL
                .iter()
                .map(|metric| {
                    let forecast = event
                        .reading(station_id, metric.id())
                        .and_then(|reading| reading.baseline);
                    (*metric, forecast)
                })
                .collect(),
        })
        .collect()
}

fn station_pins(
    locations: &[String],
    stations: &std::collections::HashMap<String, crate::templates::admin::dashboard::Station>,
) -> Vec<StationPin> {
    locations
        .iter()
        .filter_map(|station_id| {
            let station = stations.get(station_id)?;
            let (svg_x, svg_y) = lat_lon_to_svg(station.latitude, station.longitude)?;
            let label = if station.iata_id.trim().is_empty() {
                station_id.trim_start_matches('K').to_owned()
            } else {
                station.iata_id.trim().to_owned()
            };
            Some(StationPin {
                station_id: station_id.clone(),
                label,
                name: oracle_view::station_name(stations, station_id)
                    .unwrap_or_else(|| station_id.clone()),
                svg_x,
                svg_y,
            })
        })
        .collect()
}

/// Entry form for a competition
pub async fn entry_form_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    headers: HeaderMap,
    MaybeAuth(auth): MaybeAuth,
) -> Response {
    let Ok(competition) = state.coordinator.get_competition(competition_id).await else {
        return not_found(&headers, &state, "Competition");
    };
    let view = CompetitionView::new(&competition, now());
    if !view.can_enter {
        // Entries are closed; the leaderboard is what there is to see.
        return leaderboard_response(&state, &headers, &competition, &view).await;
    }
    let locations = &competition.event_submission.locations;
    let (event, stations, terms, destination) = tokio::join!(
        oracle_view::event(&state.oracle_url, competition_id),
        oracle_view::stations(&state.oracle_url),
        state.coordinator.payout_terms_quote(competition_id),
        payout_destination(&state, auth.as_ref()),
    );
    let terms = terms
        .inspect_err(|error| warn!("payout terms for {competition_id}: {error}"))
        .ok();
    let content = entry_form(
        &view,
        &station_forecasts(locations, &event, &stations),
        &station_pins(locations, &stations),
        terms.as_ref(),
        &destination,
    );
    page(&headers, &state, "Enter - Fantasy Weather", content, Caching::Private)
}

/// The entry form's payout line, reloaded when the player logs in or out.
pub async fn entry_payout_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    MaybeAuth(auth): MaybeAuth,
) -> Response {
    let (terms, destination) = tokio::join!(
        state.coordinator.payout_terms_quote(competition_id),
        payout_destination(&state, auth.as_ref()),
    );
    let mut response = Html(
        payout_line(&competition_id.to_string(), terms.ok().as_ref(), &destination).into_string(),
    )
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

/// Leaderboard for a competition
pub async fn leaderboard_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let Ok(competition) = state.coordinator.get_competition(competition_id).await else {
        return not_found(&headers, &state, "Competition");
    };
    let view = CompetitionView::new(&competition, now());
    leaderboard_response(&state, &headers, &competition, &view).await
}

async fn leaderboard_response(
    state: &AppState,
    headers: &HeaderMap,
    competition: &Competition,
    view: &CompetitionView,
) -> Response {
    let rows = leaderboard_view_rows(state, competition).await;
    let content = leaderboard(view, &rows, now());
    page(headers, state, "Leaderboard - Fantasy Weather", content, Caching::Public)
}

/// Leaderboard rows fragment (for auto-refresh)
pub async fn leaderboard_rows_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Response {
    let Ok(competition) = state.coordinator.get_competition(competition_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rows = leaderboard_view_rows(&state, &competition).await;
    let mut response = Html(leaderboard_rows(&rows).into_string()).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// The browser's tag for an account: marks "You" rows without publishing npubs.
pub fn owner_tag(npub: &str) -> String {
    hex::encode(Sha256::digest(npub.as_bytes()))[..16].to_owned()
}

/// Scores from `fetch_leaderboard_scores`, with shared ranks and player names.
async fn leaderboard_view_rows(state: &AppState, competition: &Competition) -> Vec<LeaderboardRow> {
    let (scores, entries) = tokio::join!(
        fetch_leaderboard_scores(state, competition.id),
        state
            .coordinator
            .competition_store
            .get_competition_entries(competition.id, vec![])
    );
    let entries = entries.unwrap_or_default();
    let ranks = shared_ranks(&scores.iter().map(|score| score.score).collect::<Vec<_>>());
    scores
        .iter()
        .zip(ranks)
        .map(|(score, rank)| {
            let npub = entries
                .iter()
                .find(|entry| entry.id.to_string() == score.entry_id)
                .and_then(|entry| nostr::PublicKey::from_hex(&entry.pubkey).ok())
                .map(|pubkey| pubkey.to_bech32().unwrap_or_else(|never| match never {}));
            LeaderboardRow {
                rank,
                entry_id: score.entry_id.clone(),
                player: player_name(score, npub.as_deref()),
                owner: npub.as_deref().map(owner_tag).unwrap_or_default(),
                score: score.score,
            }
        })
        .collect()
}

/// The player's username, or a short npub for accounts without one.
fn player_name(score: &EntryScore, npub: Option<&str>) -> String {
    let username = score.username.trim();
    // Without a username the scores carry a hex fragment of the pubkey.
    let is_hex_fragment = username.len() == 8 && username.chars().all(|c| c.is_ascii_hexdigit());
    match (username.is_empty() || is_hex_fragment, npub) {
        (false, _) => username.to_owned(),
        (true, Some(npub)) => short_npub(npub),
        (true, None) => "Player".to_owned(),
    }
}

/// An entry's picks with the oracle's forecasts and readings (for the dialog)
pub async fn entry_detail_fragment(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let entry = state.coordinator.get_entry_by_id(entry_id).await.ok().flatten();
    let Some(entry) = entry else {
        return not_found(&headers, &state, "Entry");
    };
    let (competition, event, stations) = tokio::join!(
        state.coordinator.get_competition(entry.event_id),
        oracle_view::event(&state.oracle_url, entry.event_id),
        oracle_view::stations(&state.oracle_url),
    );
    let readings = match competition.map(|competition| Phase::of(&competition, now())) {
        Ok(Phase::Finished | Phase::Cancelled | Phase::Failed) => Readings::Final,
        _ => Readings::Expected,
    };
    let picks: Vec<PickView> = entry
        .entry_submission
        .expected_observations
        .iter()
        .flat_map(|choice| {
            let station = &choice.stations;
            let station_name = oracle_view::station_name(&stations, station);
            [
                (Metric::TempHigh, &choice.temp_high),
                (Metric::TempLow, &choice.temp_low),
                (Metric::WindSpeed, &choice.wind_speed),
            ]
            .into_iter()
            .filter_map(|(metric, pick)| {
                let pick = pick.as_ref()?;
                let reading = event.reading(station, metric.id());
                Some(PickView {
                    station_id: station.clone(),
                    station_name: station_name.clone(),
                    metric,
                    pick: pick.clone(),
                    forecast: reading.and_then(|reading| reading.baseline),
                    observed: reading.and_then(|reading| reading.observed),
                })
            })
            .collect::<Vec<_>>()
        })
        .collect();
    let content = picks_detail(&entry_id.to_string(), &picks, readings);
    page(&headers, &state, "Picks - Fantasy Weather", content, Caching::Public)
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
    // Fetch event from oracle to get entries with scores (used for sort order via final_score)
    let oracle_entries = fetch_oracle_event_entries(&state.oracle_url, competition_id).await;

    if oracle_entries.is_empty() {
        return vec![];
    }

    // Fetch competition weather data once for raw score calculation
    let competition = state.coordinator.get_competition(competition_id).await.ok();
    let weather = if let Some(ref comp) = competition {
        let station_ids: Vec<&str> = comp
            .event_submission
            .locations
            .iter()
            .map(|s| s.as_str())
            .collect();
        if !station_ids.is_empty() {
            let (forecasts, observations) = tokio::join!(
                fetch_entry_forecasts(
                    &state.oracle_url,
                    &station_ids,
                    comp.event_submission.start_observation_date,
                    comp.event_submission.end_observation_date
                ),
                fetch_entry_observations(
                    &state.oracle_url,
                    &station_ids,
                    comp.event_submission.start_observation_date,
                    comp.event_submission.end_observation_date
                )
            );
            let forecast_map: std::collections::HashMap<String, Forecast> = forecasts
                .ok()
                .unwrap_or_default()
                .into_iter()
                .map(|f| (f.station_id.clone(), f))
                .collect();
            let observation_map: std::collections::HashMap<String, Observation> = observations
                .ok()
                .unwrap_or_default()
                .into_iter()
                .map(|o| (o.station_id.clone(), o))
                .collect();
            Some((forecast_map, observation_map))
        } else {
            None
        }
    } else {
        None
    };

    let local_entries = state
        .coordinator
        .competition_store
        .get_competition_entries(competition_id, vec![])
        .await
        .unwrap_or_default();
    let entries_by_oracle_id: std::collections::HashMap<_, _> = local_entries
        .iter()
        .map(|entry| (entry.entry_submission.id, entry))
        .collect();

    // Build index of oracle scores for sort order (final_score with tiebreaker)
    let oracle_score_map: std::collections::HashMap<Uuid, i64> = oracle_entries
        .iter()
        .map(|e| {
            (
                entries_by_oracle_id
                    .get(&e.id)
                    .map_or(e.id, |entry| entry.id),
                e.score.unwrap_or(0),
            )
        })
        .collect();

    // Convert to EntryScore, computing raw scores from picks + weather data
    let mut scores: Vec<EntryScore> = Vec::with_capacity(oracle_entries.len());

    for oracle_entry in &oracle_entries {
        let mut entry_score = EntryScore {
            rank: 0,
            entry_id: entries_by_oracle_id
                .get(&oracle_entry.id)
                .map_or(oracle_entry.id, |entry| entry.id)
                .to_string(),
            username: String::new(),
            score: 0,
        };

        // Fetch entry details for username and raw score calculation
        if let Some(entry) = entries_by_oracle_id.get(&oracle_entry.id) {
            // Look up username
            if let Ok(pubkey) = nostr::PublicKey::from_hex(&entry.pubkey) {
                let bech32 = pubkey.to_bech32().unwrap_or_else(|never| match never {});
                if let Ok(Some(name)) = state.users_info.get_username_by_pubkey(&bech32).await {
                    entry_score.username = name;
                } else {
                    entry_score.username = entry.pubkey[..8].to_string();
                }
            }

            // Compute raw score from picks + weather data
            if let Some((ref forecast_map, ref observation_map)) = weather {
                let mut raw_score = 0i32;
                for obs in &entry.entry_submission.expected_observations {
                    let forecast = forecast_map.get(&obs.stations);
                    let observation = observation_map.get(&obs.stations);

                    if let Some(pick) = &obs.temp_high {
                        raw_score += calculate_option_score(
                            forecast.and_then(|f| f.temp_high),
                            observation.and_then(|o| o.temp_high),
                            pick,
                        );
                    }
                    if let Some(pick) = &obs.temp_low {
                        raw_score += calculate_option_score(
                            forecast.and_then(|f| f.temp_low),
                            observation.and_then(|o| o.temp_low),
                            pick,
                        );
                    }
                    if let Some(pick) = &obs.wind_speed {
                        raw_score += calculate_option_score(
                            forecast.and_then(|f| f.wind_speed),
                            observation.and_then(|o| o.wind_speed),
                            pick,
                        );
                    }
                }
                entry_score.score = raw_score;
            }
        }

        scores.push(entry_score);
    }

    // Sort by oracle final_score (handles tiebreaking), but display raw_score
    scores.sort_by(|a, b| {
        let a_oracle = oracle_score_map
            .get(&Uuid::parse_str(&a.entry_id).unwrap_or_default())
            .unwrap_or(&0);
        let b_oracle = oracle_score_map
            .get(&Uuid::parse_str(&b.entry_id).unwrap_or_default())
            .unwrap_or(&0);
        b_oracle
            .cmp(a_oracle)
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
