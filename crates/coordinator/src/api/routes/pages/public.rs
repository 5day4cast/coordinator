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

use super::oracle_view;
use crate::{
    api::extractors::NostrAuth,
    domain::{
        leaderboard::{self as scores, CompetitionWeather},
        Competition, SearchBy,
    },
    startup::AppState,
    templates::{
        format::short_npub,
        fragments::{
            entry_form::{
                entry_form, forecast_choices, payout_line, PayoutDestination, StationForecast,
            },
            leaderboard::{
                leaderboard, leaderboard_scores, LeaderboardRow, LeaderboardView, Provisional,
            },
            picks::{picks_detail, Metric, PickProgress, PickState, PickView, Readings},
        },
        layouts::base::{base, PageConfig},
        pages::{
            competitions::{competitions_page, CompetitionView, ListOptions},
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
fn page(
    headers: &HeaderMap,
    state: &AppState,
    title: &str,
    content: Markup,
    caching: Caching,
) -> Response {
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
    headers.insert(
        header::VARY,
        HeaderValue::from_static("HX-Request, HX-History-Restore-Request, HX-Target"),
    );
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
            show_cancelled: query
                .cancelled
                .is_some_and(|value| value == "1" || value == "true"),
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
    page(
        &headers,
        &state,
        "Fantasy Weather: call the weather, win the pot",
        content,
        Caching::Public,
    )
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
    page(
        &headers,
        &state,
        title,
        entries_page(&rows, open),
        Caching::Private,
    )
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
fn signed_out(
    headers: &HeaderMap,
    state: &AppState,
    title: &str,
    path: &str,
    what: &str,
) -> Response {
    let mut response = page(
        headers,
        state,
        title,
        sign_in_required(path, what),
        Caching::Private,
    );
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
    let mut response = page(
        headers,
        state,
        "Not found - Fantasy Weather",
        content,
        Caching::Public,
    );
    if !is_fragment(headers) {
        *response.status_mut() = StatusCode::NOT_FOUND;
    }
    response
}

/// Each station's forecasts as the oracle recorded them for this competition.
fn station_forecasts(
    locations: &[String],
    weather: Option<&CompetitionWeather>,
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
                    let forecast = weather
                        .and_then(|weather| weather.forecast(station_id, score_metric(*metric)));
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
        return leaderboard_response(&state, &headers, &view);
    }
    let locations = &competition.event_submission.locations;
    let (weather, stations, terms, destination) = tokio::join!(
        state.leaderboards.weather(&competition),
        state.leaderboards.stations(),
        state.coordinator.payout_terms_quote(competition_id),
        payout_destination(&state, auth.as_ref()),
    );
    let terms = terms
        .inspect_err(|error| warn!("payout terms for {competition_id}: {error}"))
        .ok();
    let stations = station_map(
        stations
            .as_ref()
            .map(|stations| stations.value.as_slice())
            .unwrap_or_default(),
    );
    let content = entry_form(
        &view,
        &station_forecasts(
            locations,
            weather.as_ref().map(|weather| &weather.value),
            &stations,
        ),
        &station_pins(locations, &stations),
        terms.as_ref(),
        &destination,
    );
    page(
        &headers,
        &state,
        "Enter - Fantasy Weather",
        content,
        Caching::Private,
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct RetryQuery {
    attempt: Option<u8>,
}

/// Public forecasts can finish loading after a cold cache without asking the wallet to sign.
pub async fn entry_forecasts_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    Query(query): Query<RetryQuery>,
) -> Response {
    let Ok(competition) = state.coordinator.get_competition(competition_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content = if CompetitionView::new(&competition, now()).can_enter {
        let (weather, stations) = tokio::join!(
            state.leaderboards.weather(&competition),
            state.leaderboards.stations()
        );
        let stations = station_map(
            stations
                .as_ref()
                .map(|stations| stations.value.as_slice())
                .unwrap_or_default(),
        );
        let locations = &competition.event_submission.locations;
        forecast_choices(
            &competition_id.to_string(),
            &station_forecasts(
                locations,
                weather.as_ref().map(|weather| &weather.value),
                &stations,
            ),
            &station_pins(locations, &stations),
            query.attempt.unwrap_or(0),
        )
    } else {
        html! { p class="notice" { "Entries have closed. " a href=(format!("/competitions/{competition_id}/leaderboard")) { "View the leaderboard" } } }
    };
    let mut response = Html(content.into_string()).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
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
        payout_line(
            &competition_id.to_string(),
            terms.ok().as_ref(),
            &destination,
        )
        .into_string(),
    )
    .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

/// Leaderboard for a competition. It needs only the competition, so it
/// renders at once; the scores load into it from the rows fragment.
pub async fn leaderboard_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let Ok(competition) = state.coordinator.get_competition(competition_id).await else {
        return not_found(&headers, &state, "Competition");
    };
    let view = CompetitionView::new(&competition, now());
    leaderboard_response(&state, &headers, &view)
}

fn leaderboard_response(state: &AppState, headers: &HeaderMap, view: &CompetitionView) -> Response {
    let content = leaderboard(view, now());
    page(
        headers,
        state,
        "Leaderboard - Fantasy Weather",
        content,
        Caching::Public,
    )
}

/// The leaderboard's scores: loaded after the page, then every minute while
/// the window is open.
pub async fn leaderboard_rows_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    Query(query): Query<RetryQuery>,
) -> Response {
    let Ok(competition) = state.coordinator.get_competition(competition_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let now = now();
    let board = state.leaderboards.get(competition_id).await.ok();
    if board.as_ref().is_none_or(|board| {
        board.weather_fetched_at.is_none() && board.phase != scores::Phase::Upcoming
    }) {
        return pending_scores(
            "leaderboardScores",
            &format!("/competitions/{competition_id}/leaderboard/rows"),
            query.attempt.unwrap_or(0),
        );
    }
    let board = board.as_ref().map(leaderboard_view).unwrap_or_default();
    let view = CompetitionView::new(&competition, now);
    let mut response = Html(leaderboard_scores(&view, &board, now).into_string()).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Initial cache fills retry briefly, including finished events. Once data arrives,
/// only open windows poll. Failed fetches end in a visible manual retry.
fn pending_scores_content(id: &str, url: &str, attempt: u8) -> Markup {
    let retry = attempt < 4;
    let next = format!("{url}?attempt={}", attempt.saturating_add(1));
    html! {
        div id=(id) hx-get=[retry.then_some(&next)] hx-trigger=[retry.then_some("load delay:1s")]
            hx-target="this" hx-swap="outerHTML" hx-disinherit="*" {
            p class="notice" role="status" {
                @if retry { span class="spinner" aria-hidden="true" {} " Loading observations and scores…" }
                @else {
                    "Observations are temporarily unavailable. "
                    button type="button" class="button is-small" hx-get=(url) hx-target=(format!("#{id}")) hx-swap="outerHTML" { "Retry" }
                }
            }
        }
    }
}

fn pending_scores(id: &str, url: &str, attempt: u8) -> Response {
    let mut response = Html(pending_scores_content(id, url, attempt).into_string()).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Adapt the cached domain result; rendering never performs its own scoring or oracle fetches.
fn leaderboard_view(board: &scores::Leaderboard) -> LeaderboardView {
    LeaderboardView {
        rows: board
            .rows
            .iter()
            .map(|row| {
                let npub = nostr::PublicKey::from_hex(&row.player.pubkey)
                    .ok()
                    .map(|key| key.to_bech32().unwrap_or_else(|never| match never {}));
                let name = row.player.name.trim();
                let fallback = name.is_empty()
                    || (name.len() <= 16 && name.chars().all(|c| c.is_ascii_hexdigit()));
                LeaderboardRow {
                    rank: row.standing.rank,
                    entry_id: row.entry_id.to_string(),
                    player: if fallback {
                        npub.as_deref()
                            .map(short_npub)
                            .unwrap_or_else(|| "Player".into())
                    } else {
                        name.into()
                    },
                    owner: npub.as_deref().map(owner_tag).unwrap_or_default(),
                    score: i32::try_from(row.standing.score).unwrap_or(i32::MAX),
                }
            })
            .collect(),
        provisional: (board.provisional && board.phase != scores::Phase::Upcoming).then_some(
            Provisional {
                updated_at: board.observed_until.or(board.weather_fetched_at),
            },
        ),
    }
}

/// Public, like the leaderboard; ownership is highlighted locally without a signature.
pub fn owner_tag(npub: &str) -> String {
    hex::encode(Sha256::digest(npub.as_bytes()))[..16].to_owned()
}

fn station_map(
    stations: &[crate::templates::admin::dashboard::Station],
) -> std::collections::HashMap<String, crate::templates::admin::dashboard::Station> {
    stations
        .iter()
        .map(|station| (station.station_id.clone(), station.clone()))
        .collect()
}

fn score_metric(metric: Metric) -> scores::Metric {
    match metric {
        Metric::TempHigh => scores::Metric::TempHigh,
        Metric::TempLow => scores::Metric::TempLow,
        Metric::WindSpeed => scores::Metric::WindSpeed,
    }
}

/// Picks are public after entries close. The domain seam withholds them beforehand.
pub async fn entry_detail_fragment(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
    Query(query): Query<RetryQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(entry) = state
        .coordinator
        .get_entry_by_id(entry_id)
        .await
        .ok()
        .flatten()
    else {
        return not_found(&headers, &state, "Entry");
    };
    let (board, stations) = tokio::join!(
        state.leaderboards.get(entry.event_id),
        state.leaderboards.stations(),
    );
    let Ok(board) = board else {
        return not_found(&headers, &state, "Competition");
    };
    if board.weather_fetched_at.is_none() && board.phase != scores::Phase::Upcoming {
        let content = pending_scores_content(
            "entryPickScores",
            &format!("/entries/{entry_id}/detail"),
            query.attempt.unwrap_or(0),
        );
        return page(
            &headers,
            &state,
            "Picks - Fantasy Weather",
            content,
            Caching::Public,
        );
    }
    let readings = match board.phase {
        scores::Phase::Upcoming => Readings::NotStarted,
        scores::Phase::Live => Readings::Live {
            updated_at: board.observed_until.or(board.weather_fetched_at),
        },
        scores::Phase::Closed => Readings::Closed,
        scores::Phase::Scored => Readings::Final,
    };
    let stations = station_map(
        stations
            .as_ref()
            .map(|stations| stations.value.as_slice())
            .unwrap_or_default(),
    );
    let picks = board
        .rows
        .iter()
        .find(|row| row.entry_id == entry_id)
        .map(|row| {
            row.picks
                .iter()
                .map(|pick| pick_view(pick, &stations, board.phase == scores::Phase::Live))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    page(
        &headers,
        &state,
        "Picks - Fantasy Weather",
        picks_detail(&entry_id.to_string(), &picks, readings, now()),
        Caching::Public,
    )
}

fn pick_view(
    pick: &scores::PickProgress,
    stations: &std::collections::HashMap<String, crate::templates::admin::dashboard::Station>,
    live: bool,
) -> PickView {
    let metric = match pick.metric {
        scores::Metric::TempHigh => Metric::TempHigh,
        scores::Metric::TempLow => Metric::TempLow,
        scores::Metric::WindSpeed => Metric::WindSpeed,
    };
    PickView {
        station_id: pick.station_id.clone(),
        station_name: oracle_view::station_name(stations, &pick.station_id),
        metric,
        pick: pick.pick.clone(),
        forecast: pick.forecast,
        score: u32::try_from(pick.points).unwrap_or(u32::MAX),
        hit: pick.hit,
        observed: if live { None } else { pick.observed },
        progress: live.then_some(PickProgress {
            observed_so_far: pick.observed,
            hours_covered: pick.hours_covered.floor() as u32,
            hours_total: pick.hours_total.ceil() as u32,
            state: match pick.state {
                scores::PickState::Pending => PickState::Pending,
                scores::PickState::LockedIn => PickState::LockedIn,
                scores::PickState::OnTrack => PickState::OnTrack,
                scores::PickState::OffTrack => PickState::OffTrack,
                scores::PickState::Out => PickState::Out,
                scores::PickState::Final => PickState::Final,
            },
        }),
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
