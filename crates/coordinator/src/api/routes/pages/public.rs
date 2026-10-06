//! Public pages. Each handler returns just its content to htmx requests and
//! the whole page, layout included, to direct visits, reloads and Back.
//!
//! Weather comes from the leaderboard cache (`domain::leaderboard`), never
//! from the oracle directly. Pages and fragments wait for a fetch in flight
//! only briefly (`FIRST_READ_WAIT`), so each answers within the site's 400 ms
//! budget; without the weather yet, they say it is still loading and ask again
//! (`fragments::loading`).

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    extract::{FromRequestParts, Path, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
};
use log::{debug, error, warn};
use maud::{html, Markup};
use nostr::{PublicKey, ToBech32};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use super::oracle_view::{
    station_forecasts, station_name, station_pins, stations_by_id, stations_named,
};
use crate::{
    api::extractors::NostrAuth,
    domain::{
        leaderboard::{Leaderboard, Phase, FIRST_READ_WAIT},
        Competition, Error, LedgerTotals, RefundProgress, Returned,
    },
    infra::refresh_cache::Cached,
    startup::AppState,
    templates::{
        components::menu_toggle,
        format::short_npub,
        fragments::{
            entry_form::{
                entry_form, forecast_choices, forecasts_url, payout_line, ticket_status, Forecasts,
                NetworkFee, PayoutDestination, TicketProgress,
            },
            leaderboard::{
                leaderboard, leaderboard_scores, queue_pools, rows_url, LeaderboardRow,
                LeaderboardView,
            },
            loading::{placeholder, Pending, MAX_ASKS},
            picks::{detail_url, own_picks_detail, picks_detail, withheld_picks_detail, PickView},
        },
        layouts::base::{base, PageConfig},
        pages::{
            competitions::{competitions_page, shown_ids, CompetitionView, ListOptions, Tab},
            entries::{entries_page, older_entries, sign_in_required, EntryRow, PAGE_SIZE},
            help::help_page,
            payouts::payouts_page,
        },
    },
};

/// The `HX-Target` htmx 4 sends for a navigation of the page's main content.
const MAIN_CONTENT: &str = "main#main-content";

/// Whether to answer with content only: htmx swaps it into the open page.
/// History restores (Back) need the whole page.
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

impl Caching {
    fn header(self) -> HeaderValue {
        HeaderValue::from_static(match self {
            Caching::Public => "no-cache",
            Caching::Private => "private, no-store",
        })
    }
}

/// A fragment that is only ever loaded into a page: rows, forecasts,
/// picks' refreshes, a ticket's status.
fn fragment(content: Markup, caching: Caching) -> Response {
    let mut response = Html(content.into_string()).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, caching.header());
    response
}

/// A fragment saying the server failed, which the elements that load
/// fragments swap in (`hx-status:500`).
fn failed(id: &str, url: &str, what: &str) -> Response {
    let mut response = fragment(
        placeholder(id, url, what, Pending::Failed, 0),
        Caching::Public,
    );
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
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
        // navigations of the main content change it, and they close the
        // phone menu (the navbar's toggle, swapped in unchecked).
        let navigation = headers
            .get("HX-Target")
            .is_some_and(|target| target == MAIN_CONTENT);
        html! {
            @if navigation {
                title { (title) }
                (menu_toggle(true))
            }
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
    headers.insert(header::CACHE_CONTROL, caching.header());
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

/// The `npub` form of a key; nostr's bech32 encoding cannot fail.
fn npub(pubkey: &PublicKey) -> String {
    pubkey.to_bech32().unwrap_or_else(|never| match never {})
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Every competition as the lists show it, read without contracts: until [`complete`] is given
/// a view, it has no refunds and doesn't say whether its pot went back to the players.
async fn competition_views(state: &AppState, now: OffsetDateTime) -> Vec<CompetitionView> {
    let (competitions, network_fee) = tokio::join!(
        state.coordinator.list_competitions(),
        state.coordinator.shown_network_fee_quote(),
    );
    // What a ticket issued now adds, so the price shown is what entering costs.
    let network_fee = network_fee.ok().map(|quote| quote.network_fee_sats);
    match competitions {
        Ok(competitions) => competitions
            .iter()
            .map(|competition| {
                let mut view = CompetitionView::new(competition, now);
                view.network_fee = network_fee.filter(|_| view.can_enter);
                view
            })
            .collect(),
        Err(error) => {
            error!("failed to load competitions: {error}");
            vec![]
        }
    }
}

/// Adds what the page shows of the competitions `shown` and the list leaves out: their refunds,
/// and for a scored one whether its pot went back to the players, which takes its contract.
async fn complete(state: &AppState, views: &mut [CompetitionView], shown: &[String]) {
    let ids: Vec<Uuid> = shown
        .iter()
        .filter_map(|id| Uuid::parse_str(id).ok())
        .collect();
    let scored: Vec<Uuid> = views
        .iter()
        .filter(|view| view.phase == Phase::Scored && shown.contains(&view.id))
        .filter_map(|view| Uuid::parse_str(&view.id).ok())
        .collect();
    let (refunds, scored) = tokio::join!(
        refund_status(state, &ids),
        futures::future::join_all(
            scored
                .into_iter()
                .map(|id| state.coordinator.get_competition(id))
        )
    );
    for view in views.iter_mut() {
        if let Some(progress) = Uuid::parse_str(&view.id)
            .ok()
            .and_then(|id| refunds.get(&id))
        {
            view.refunds = *progress;
        }
    }
    for competition in scored {
        match competition {
            Ok(competition) => {
                let id = competition.id.to_string();
                if let Some(view) = views.iter_mut().find(|view| view.id == id) {
                    view.add_contract(&competition);
                }
            }
            Err(error) => error!("failed to load a shown competition: {error}"),
        }
    }
}

/// How far the escrow refunds of `competition_ids` have got; none if that can't be read, so a
/// page still renders.
async fn refund_status(
    state: &AppState,
    competition_ids: &[Uuid],
) -> std::collections::HashMap<Uuid, RefundProgress> {
    state
        .coordinator
        .refund_status(competition_ids)
        .await
        .unwrap_or_else(|error| {
            error!("failed to read refund progress: {error}");
            Default::default()
        })
}

/// One competition as the page shows it, with its refunds.
async fn competition_view(
    state: &AppState,
    competition: &Competition,
    now: OffsetDateTime,
) -> CompetitionView {
    let mut view = CompetitionView::new(competition, now);
    view.refunds = refund_status(state, &[competition.id])
        .await
        .remove(&competition.id)
        .unwrap_or_default();
    view
}

/// `?show=finished&q=portland&page=1&cancelled=1` on the competitions list.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    show: Option<String>,
    q: Option<String>,
    page: Option<usize>,
    cancelled: Option<String>,
}

impl From<ListQuery> for ListOptions {
    fn from(query: ListQuery) -> Self {
        let page = query.page.unwrap_or(0);
        let tab = query
            .show
            .as_deref()
            .and_then(Tab::from_param)
            // A page of finished competitions, as they were addressed before their tab.
            .unwrap_or(if page > 0 {
                Tab::Finished
            } else {
                Tab::Overview
            });
        ListOptions {
            tab,
            page,
            show_cancelled: query
                .cancelled
                .is_some_and(|value| value == "1" || value == "true"),
            // Only the Live and Finished tabs are searched.
            search: query
                .q
                .as_deref()
                .and_then(ListOptions::search_for)
                .filter(|_| tab != Tab::Overview),
            stations: Default::default(),
        }
    }
}

/// `?again=3`: a placeholder's own third request (see `fragments::loading`).
#[derive(Debug, Default, Deserialize)]
pub struct Again {
    again: Option<u8>,
}

impl Again {
    fn asked(&self) -> u8 {
        self.again.unwrap_or(0).min(MAX_ASKS)
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
    let mut options = ListOptions::from(query);
    // A search finds competitions by their stations' names too, when the oracle's stations
    // are cached; without them, by id and station code alone.
    let named = async {
        match options.search.as_deref() {
            Some(search) => {
                let stations = state.leaderboards.stations(FIRST_READ_WAIT).await;
                stations_named(
                    stations.value().map(Vec::as_slice).unwrap_or_default(),
                    search,
                )
            }
            None => Default::default(),
        }
    };
    let (mut competitions, named) = tokio::join!(competition_views(&state, now), named);
    options.stations = named;
    let shown = shown_ids(&competitions, &options);
    complete(&state, &mut competitions, &shown).await;
    let content = competitions_page(&competitions, &options, now);
    page(
        &headers,
        &state,
        "Fantasy Weather: call the weather, win the pot",
        content,
        Caching::Public,
    )
}

/// `?from=25` on the entries page: the rows after the first 25, for "Show older entries".
#[derive(Debug, Default, Deserialize)]
pub struct EntriesPage {
    from: Option<usize>,
}

/// Entries page (requires auth; signed-out visitors get the log-in prompt): the totals of
/// the player's money over every entry, and a page of the entries with theirs. `?from=` asks
/// htmx for the next page's rows alone.
pub async fn entries_fragment(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EntriesPage>,
    headers: HeaderMap,
    MaybeAuth(auth): MaybeAuth,
) -> Response {
    let title = "Your entries - Fantasy Weather";
    let Some(NostrAuth { pubkey, .. }) = auth else {
        return signed_out(&headers, &state, title, "/entries", "your entries");
    };
    let now = now();
    let pubkey = pubkey.to_hex();
    let (ledger, mut competitions) = tokio::join!(
        state.coordinator.player_ledger(&pubkey),
        competition_views(&state, now)
    );
    let ledger = ledger
        .inspect_err(|error| error!("failed to load entries: {error}"))
        .unwrap_or_default();
    let phases: HashMap<&str, Phase> = competitions
        .iter()
        .map(|competition| (competition.id.as_str(), competition.phase))
        .collect();
    let returned: Vec<Returned> = ledger
        .iter()
        .map(|entry| entry.returned(phases.get(entry.competition_id.as_str()).copied(), now))
        .collect();
    let mut totals = LedgerTotals::default();
    for (entry, returned) in ledger.iter().zip(&returned) {
        totals.add(entry, returned);
    }
    let from = query
        .from
        .filter(|_| is_fragment(&headers))
        .unwrap_or(0)
        .min(ledger.len());
    let shown = from..(from + PAGE_SIZE).min(ledger.len());
    let shown_ids: Vec<String> = ledger[shown.clone()]
        .iter()
        .map(|entry| entry.competition_id.clone())
        .collect();
    complete(&state, &mut competitions, &shown_ids).await;
    let views: HashMap<&str, &CompetitionView> = competitions
        .iter()
        .map(|competition| (competition.id.as_str(), competition))
        .collect();
    let rows: Vec<EntryRow> = ledger[shown.clone()]
        .iter()
        .zip(&returned[shown])
        .map(|(entry, returned)| EntryRow {
            entry,
            competition: views.get(entry.competition_id.as_str()).copied(),
            returned,
        })
        .collect();
    if from > 0 {
        return fragment(older_entries(&rows, from, ledger.len()), Caching::Private);
    }
    let open = competitions
        .iter()
        .filter(|competition| competition.can_enter && !competition.unlisted)
        .min_by_key(|competition| competition.start);
    page(
        &headers,
        &state,
        title,
        entries_page(&rows, &totals, ledger.len(), open),
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
    let pubkey_hex = pubkey.to_hex();
    let (payouts, user) = tokio::join!(
        state.coordinator.eligible_payouts(&pubkey_hex),
        state.users_info.login(npub(&pubkey)),
    );
    let payouts = payouts
        .inspect_err(|error| error!("failed to load payouts: {error}"))
        .unwrap_or_default();
    let lightning_address = user.ok().and_then(|user| user.lightning_address);
    let content = payouts_page(&payouts, lightning_address.as_deref());
    page(&headers, &state, title, content, Caching::Private)
}

/// How it works: the rules and scoring the competition pages leave out.
#[derive(Debug, Default, Deserialize)]
pub struct HelpQuery {
    /// `advanced` unfolds "How the tech works", for links into it.
    #[serde(default)]
    open: Option<String>,
}

pub async fn help_fragment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<HelpQuery>,
) -> Response {
    let open_advanced = query.open.as_deref() == Some("advanced");
    page(
        &headers,
        &state,
        "How it works - Fantasy Weather",
        help_page(open_advanced),
        Caching::Public,
    )
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
    match state.users_info.login(npub(&auth.pubkey)).await {
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

/// A page that is not there, with the way back, and status 404.
pub fn not_found(headers: &HeaderMap, state: &AppState, what: &str) -> Response {
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
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// A competition by id: `Ok(None)` when there is none, `Err` when it could
/// not be looked up.
async fn find_competition(state: &AppState, id: Uuid) -> Result<Option<Competition>, Error> {
    match state.coordinator.get_competition(id).await {
        Ok(competition) => Ok(Some(competition)),
        Err(Error::NotFound(_) | Error::DbError(sqlx::Error::RowNotFound)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Why cached weather is missing: still on its way, or the last fetch failed.
fn pending<V>(cached: &Cached<V>) -> Pending {
    if cached.refreshing {
        Pending::Loading
    } else {
        Pending::Unavailable
    }
}

/// The entry form's forecasts, from what the cache has after up to `wait`.
async fn forecasts(state: &AppState, competition: &Competition, wait: Duration) -> Forecasts {
    let (weather, stations) = tokio::join!(
        state.leaderboards.weather(competition, wait),
        state.leaderboards.stations(wait),
    );
    match weather.value() {
        Some(value) if !value.forecasts_unavailable() => {
            let stations = stations_by_id(stations.value().map(Vec::as_slice).unwrap_or_default());
            let locations = &competition.event_submission.locations;
            Forecasts::Ready {
                stations: station_forecasts(
                    locations,
                    &competition.event_submission.metrics(),
                    competition.event_submission.scoring_rules(),
                    value,
                    &stations,
                ),
                pins: station_pins(locations, &stations),
            }
        }
        Some(_) => Forecasts::Pending(Pending::Unavailable),
        None => Forecasts::Pending(pending(&weather)),
    }
}

/// Entry form for a competition
pub async fn entry_form_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    headers: HeaderMap,
    MaybeAuth(auth): MaybeAuth,
) -> Response {
    let competition = match find_competition(&state, competition_id).await {
        Ok(Some(competition)) => competition,
        Ok(None) => return not_found(&headers, &state, "Competition"),
        Err(error) => {
            error!("entry form for {competition_id}: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let view = competition_view(&state, &competition, now()).await;
    if !view.can_enter {
        // Entries are closed; the leaderboard is what there is to see.
        return leaderboard_response(&state, &headers, &view);
    }
    let (forecasts, terms, destination, network_fee) = tokio::join!(
        forecasts(&state, &competition, FIRST_READ_WAIT),
        state.coordinator.payout_terms_quote(competition_id),
        payout_destination(&state, auth.as_ref()),
        state.coordinator.shown_network_fee_quote(),
    );
    let terms = terms
        .inspect_err(|error| warn!("payout terms for {competition_id}: {error}"))
        .ok();
    // Logged where the estimate failed; the form says it is unavailable.
    let arkade = terms.as_ref().is_some_and(|terms| terms.arkade);
    let network_fee = match network_fee {
        Ok(quote) if arkade && quote.arkade_unavailable => {
            NetworkFee::ArkadeUnavailable(quote.network_fee_sats)
        }
        Ok(quote) if quote.pauses(view.entry_fee) => NetworkFee::Paused(quote.network_fee_sats),
        Ok(quote) => NetworkFee::Estimate(quote.network_fee_sats),
        Err(_) => NetworkFee::Unavailable,
    };
    let content = entry_form(&view, &forecasts, terms.as_ref(), &destination, network_fee);
    page(
        &headers,
        &state,
        "Enter - Fantasy Weather",
        content,
        Caching::Private,
    )
}

/// The entry form's forecasts, loaded after the form when the cache had none
/// yet: public, so they load without asking the wallet to sign.
pub async fn entry_forecasts_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    Query(again): Query<Again>,
) -> Response {
    let url = forecasts_url(&competition_id.to_string());
    let competition = match find_competition(&state, competition_id).await {
        Ok(Some(competition)) => competition,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            error!("forecasts for {competition_id}: {error}");
            return failed("entryForecasts", &url, "forecasts");
        }
    };
    let content = if CompetitionView::new(&competition, now()).can_enter {
        let forecasts = forecasts(&state, &competition, FIRST_READ_WAIT).await;
        forecast_choices(&competition_id.to_string(), &forecasts, again.asked())
    } else {
        let leaderboard = format!("/competitions/{competition_id}/leaderboard");
        html! { p class="notice" { "Entries have closed. " a href=(leaderboard) { "View the leaderboard" } } }
    };
    fragment(content, Caching::Public)
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
    fragment(
        payout_line(
            &competition_id.to_string(),
            terms.ok().as_ref(),
            &destination,
        ),
        Caching::Private,
    )
}

/// A ticket's payment status for the payment dialog, signed like the account
/// pages. Once the ticket is paid or has failed, the response tells
/// `entry_form.js` with an `HX-Trigger` event, and the polling stops.
pub async fn ticket_status_fragment(
    State(state): State<Arc<AppState>>,
    Path((competition_id, ticket_id)): Path<(Uuid, Uuid)>,
    MaybeAuth(auth): MaybeAuth,
) -> Response {
    let Some(NostrAuth { pubkey, .. }) = auth else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let status = match state
        .coordinator
        .get_ticket_status(pubkey.to_hex(), competition_id, ticket_id)
        .await
    {
        Ok(status) => status,
        Err(error) => {
            // Not swapped: the dialog keeps its status line and asks again.
            warn!("ticket {ticket_id} status: {error}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let progress = TicketProgress::from(&status);
    let url = format!("/competitions/{competition_id}/tickets/{ticket_id}/status");
    let mut response = fragment(ticket_status(&url, progress), Caching::Private);
    if let Some(event) = progress
        .event(&ticket_id.to_string())
        .and_then(|event| HeaderValue::from_str(&event).ok())
    {
        response
            .headers_mut()
            .insert(HeaderName::from_static("hx-trigger"), event);
    }
    response
}

/// Leaderboard for a competition. It needs only the competition, so it
/// renders at once; the scores load into it from the rows fragment.
pub async fn leaderboard_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let competition = match find_competition(&state, competition_id).await {
        Ok(Some(competition)) => competition,
        Ok(None) => return not_found(&headers, &state, "Competition"),
        Err(error) => {
            error!("leaderboard for {competition_id}: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let view = competition_view(&state, &competition, now()).await;
    leaderboard_response(&state, &headers, &view)
}

/// A queued competition's pools, with the signed-in player's marked: loaded by its leaderboard,
/// signed when the player is logged in (see `htmx_auth.js`).
pub async fn queue_pools_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    MaybeAuth(auth): MaybeAuth,
) -> Response {
    let competition = match find_competition(&state, competition_id).await {
        Ok(Some(competition)) => competition,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            // Not swapped: the leaderboard keeps the pools it showed.
            error!("pools of {competition_id}: {error}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let view = CompetitionView::new(&competition, now());
    let Some(queue) = view.queue.queued() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mine: Vec<Uuid> = match auth {
        Some(NostrAuth { pubkey, .. }) => state
            .coordinator
            .get_user_entry_views(pubkey.to_hex())
            .await
            .inspect_err(|error| warn!("entries for the pools of {competition_id}: {error}"))
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| Uuid::parse_str(&entry.competition_id).ok())
            .collect(),
        None => vec![],
    };
    fragment(queue_pools(&view.id, queue, Some(&mine)), Caching::Private)
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

/// Whether a phase's scores come from the oracle's weather.
fn scored_from_weather(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Live | Phase::AwaitingResult | Phase::Scored | Phase::Expired
    )
}

/// Why a leaderboard's weather is missing, when its scores need it.
fn missing_weather(board: &Leaderboard) -> Option<Pending> {
    (scored_from_weather(board.phase) && board.weather_fetched_at.is_none()).then_some(
        if board.refreshing {
            Pending::Loading
        } else {
            Pending::Unavailable
        },
    )
}

/// The leaderboard's scores: loaded after the page, then every minute while
/// the window is open.
pub async fn leaderboard_rows_fragment(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    Query(again): Query<Again>,
) -> Response {
    const ID: &str = "leaderboardScores";
    const WHAT: &str = "observations and scores";
    let url = rows_url(&competition_id.to_string());
    let competition = match find_competition(&state, competition_id).await {
        Ok(Some(competition)) => competition,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            error!("leaderboard rows for {competition_id}: {error}");
            return failed(ID, &url, WHAT);
        }
    };
    let board = match state.leaderboards.get(&competition, FIRST_READ_WAIT).await {
        Ok(board) => board,
        Err(error) => {
            error!("leaderboard for {competition_id}: {error}");
            return failed(ID, &url, WHAT);
        }
    };
    if let Some(pending) = missing_weather(&board) {
        return fragment(
            placeholder(ID, &url, WHAT, pending, again.asked()),
            Caching::Public,
        );
    }
    let now = now();
    let view = competition_view(&state, &competition, now).await;
    fragment(
        leaderboard_scores(&view, &leaderboard_view(&board), now),
        Caching::Public,
    )
}

/// The domain's leaderboard as the page shows it: shared ranks, player names,
/// and a tag the browser matches to mark the viewer's own rows.
fn leaderboard_view(board: &Leaderboard) -> LeaderboardView {
    LeaderboardView {
        rows: board
            .rows
            .iter()
            .map(|row| {
                let npub = PublicKey::from_hex(&row.player.pubkey)
                    .ok()
                    .map(|key| npub(&key));
                let name = row.player.name.trim();
                let unnamed = name.is_empty()
                    || (name.len() <= 16 && name.chars().all(|c| c.is_ascii_hexdigit()));
                LeaderboardRow {
                    rank: row.standing.rank,
                    entry_id: row.entry_id.to_string(),
                    player: if unnamed {
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
        phase: board.phase,
        updated_at: board.observed_until.or(board.weather_fetched_at),
        any_readings: board
            .rows
            .iter()
            .any(|row| row.picks.iter().any(|pick| pick.observed.is_some())),
        unverified: board.settlement_blocked,
    }
}

/// Public, like the leaderboard; ownership is highlighted locally without a signature.
pub fn owner_tag(npub: &str) -> String {
    hex::encode(Sha256::digest(npub.as_bytes()))[..16].to_owned()
}

/// An entry's picks, public once entries close (the leaderboard withholds
/// them before). While the window is open they refresh every minute.
pub async fn entry_detail_fragment(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
    Query(again): Query<Again>,
    headers: HeaderMap,
) -> Response {
    const ID: &str = "entryPickScores";
    const WHAT: &str = "picks' readings";
    let url = detail_url(&entry_id.to_string());
    let entry = match state.coordinator.get_entry_by_id(entry_id).await {
        Ok(Some(entry)) => entry,
        Ok(None) => return not_found(&headers, &state, "Entry"),
        Err(error) => {
            error!("entry {entry_id}: {error}");
            return failed(ID, &url, WHAT);
        }
    };
    let competition = match find_competition(&state, entry.event_id).await {
        Ok(Some(competition)) => competition,
        Ok(None) => return not_found(&headers, &state, "Competition"),
        Err(error) => {
            error!("competition of entry {entry_id}: {error}");
            return failed(ID, &url, WHAT);
        }
    };
    let (board, stations) = tokio::join!(
        state.leaderboards.get(&competition, FIRST_READ_WAIT),
        state.leaderboards.stations(FIRST_READ_WAIT),
    );
    let board = match board {
        Ok(board) => board,
        Err(error) => {
            error!("leaderboard for entry {entry_id}: {error}");
            return failed(ID, &url, WHAT);
        }
    };
    let content = match missing_weather(&board) {
        Some(pending) => placeholder(ID, &url, WHAT, pending, again.asked()),
        None => {
            let stations = stations_by_id(stations.value().map(Vec::as_slice).unwrap_or_default());
            let picks: Vec<PickView> = board
                .rows
                .iter()
                .find(|row| row.entry_id == entry_id)
                .map(|row| {
                    row.picks
                        .iter()
                        .map(|pick| PickView {
                            pick,
                            station_name: station_name(&stations, &pick.station_id),
                        })
                        .collect()
                })
                .unwrap_or_default();
            picks_detail(
                &entry_id.to_string(),
                &picks,
                board.phase,
                board.observed_until.or(board.weather_fetched_at),
                now(),
            )
        }
    };
    page(
        &headers,
        &state,
        "Picks - Fantasy Weather",
        content,
        Caching::Public,
    )
}

/// A player's own picks before the window opens, signed when they are logged
/// in (see `htmx_auth.js`): the public dialog asks here for what the
/// leaderboard withholds. Anyone else gets the dialog's message again.
pub async fn own_entry_detail_fragment(
    State(state): State<Arc<AppState>>,
    Path(entry_id): Path<Uuid>,
    MaybeAuth(auth): MaybeAuth,
) -> Response {
    let entry = match state.coordinator.get_entry_by_id(entry_id).await {
        Ok(Some(entry)) => entry,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            // Not swapped: the dialog keeps its message.
            error!("entry {entry_id}: {error}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let competition = match find_competition(&state, entry.event_id).await {
        Ok(Some(competition)) => competition,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            error!("competition of entry {entry_id}: {error}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let now = now();
    let id = entry_id.to_string();
    let owner = auth.is_some_and(|auth| auth.pubkey.to_hex() == entry.pubkey);
    if !owner || Phase::of(&competition, now) != Phase::Upcoming {
        return fragment(withheld_picks_detail(&id, now), Caching::Private);
    }
    let (picks, stations) = tokio::join!(
        state
            .leaderboards
            .own_picks(&competition, &entry, FIRST_READ_WAIT),
        state.leaderboards.stations(FIRST_READ_WAIT),
    );
    let stations = stations_by_id(stations.value().map(Vec::as_slice).unwrap_or_default());
    let views: Vec<PickView> = picks
        .iter()
        .map(|pick| PickView {
            pick,
            station_name: station_name(&stations, &pick.station_id),
        })
        .collect();
    fragment(own_picks_detail(&id, &views, now), Caching::Private)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_tags_are_stable_and_short() {
        let tag = owner_tag("npub1example");
        assert_eq!(tag.len(), 16);
        assert_eq!(tag, owner_tag("npub1example"));
        assert_ne!(tag, owner_tag("npub1other"));
    }

    #[test]
    fn only_scores_from_weather_wait_for_it() {
        for phase in [
            Phase::Live,
            Phase::AwaitingResult,
            Phase::Scored,
            Phase::Expired,
        ] {
            assert!(scored_from_weather(phase), "{phase:?}");
        }
        for phase in [
            Phase::Upcoming,
            Phase::Unfilled,
            Phase::Cancelled,
            Phase::Failed,
        ] {
            assert!(!scored_from_weather(phase), "{phase:?}");
        }
    }
}
