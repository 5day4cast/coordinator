use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    Extension, Json,
};
use axum_extra::extract::Form;
use log::{error, info};
use maud::Markup;
use serde::Deserialize;
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::{
    api::admin_auth::AdminCsrf,
    infra::{
        admin_weather::{Filters, Window},
        bitcoin::SendOptions,
    },
    startup::AppState,
    templates::{
        admin::{
            dashboard::{competition_error, competition_success},
            wallet::{
                fee_estimates_rows, send_error, send_success, wallet_balance_section,
                wallet_outputs_rows, wallet_page, WalletBalance, WalletOutput,
            },
        },
        layouts::admin::{admin_base, AdminPageConfig},
    },
};

/// Helper to render a fragment or wrap it in the admin base layout for direct navigation.
pub(super) fn render_admin_fragment(
    headers: &HeaderMap,
    state: &AppState,
    csrf: &AdminCsrf,
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
            explorer_url: &state.explorer_url,
            network: &state.network,
            csrf_token: csrf.0.as_deref(),
        };
        Html(admin_base(&config, content).into_string())
    }
}

/// Weather discovery is rendered on the server. The filters are ordinary GET parameters.
pub async fn admin_page_handler(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Query(filters): Query<Filters>,
    headers: HeaderMap,
) -> Html<String> {
    admin_discovery(&state, &csrf, &headers, &filters).await
}

pub async fn admin_competition_fragment(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    Query(filters): Query<Filters>,
    headers: HeaderMap,
) -> Html<String> {
    admin_discovery(&state, &csrf, &headers, &filters).await
}

async fn admin_discovery(
    state: &Arc<AppState>,
    csrf: &AdminCsrf,
    headers: &HeaderMap,
    filters: &Filters,
) -> Html<String> {
    let content = match filters.window(OffsetDateTime::now_utc()) {
        Ok(window) => {
            let data = state.admin_weather.read(window.clone()).await;
            crate::templates::admin::discovery::discovery(filters, &window, &data, &state.network)
        }
        Err(error) => {
            maud::html! { main.admin-workspace { h1 { "Check the discovery filters" } p { (error) } a href="/admin/competition" { "Start again" } } }
        }
    };
    render_admin_fragment(headers, state, csrf, "Weather discovery", content)
}

/// Admin wallet page (full page for direct navigation, fragment for HTMX)
pub async fn admin_wallet_fragment(
    State(state): State<Arc<AppState>>,
    Extension(csrf): Extension<AdminCsrf>,
    headers: HeaderMap,
) -> Html<String> {
    let overview = state.coordinator.admin_wallet_overview().await;
    let content = wallet_page(&state.network, &overview);
    render_admin_fragment(&headers, &state, &csrf, "Node & wallets", content)
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
    pub history_days: Option<u32>,
    pub id: Uuid,
    pub signing_date: String,
    pub start_observation_date: String,
    pub end_observation_date: String,
    pub total_allowed_entries: usize,
    pub entry_fee: usize,
    /// A percentage with up to two decimals, such as 2.5.
    pub coordinator_fee_percentage: String,
    pub number_of_places_win: usize,
    #[serde(default)]
    pub locations: Vec<String>,
    #[serde(default)]
    pub relative_locktime_block_delta: Option<u16>,
    /// `lines` or `fixed`; lines when absent.
    #[serde(default)]
    pub scoring_rules: Option<String>,
    /// Set to create a queued competition: no seat count, pools formed when registration closes.
    #[serde(default)]
    pub queued: Option<String>,
    /// A queued competition's smallest pool.
    #[serde(default)]
    pub min_players: Option<usize>,
    /// A queued competition's largest pool.
    #[serde(default)]
    pub max_pool_size: Option<usize>,
    /// A queued competition's entry cap.
    #[serde(default)]
    pub max_entries: Option<u32>,
    /// How many entries one player may make; one if unset.
    #[serde(default)]
    pub max_entries_per_player: Option<u32>,
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
        Ok(dt) => dt.to_offset(UtcOffset::UTC),
        Err(e) => {
            return Html(competition_error(&format!("Invalid signing date: {}", e)).into_string())
        }
    };

    let start_observation_date = match OffsetDateTime::parse(
        &form.start_observation_date,
        &time::format_description::well_known::Rfc3339,
    ) {
        Ok(dt) => dt.to_offset(UtcOffset::UTC),
        Err(e) => {
            return Html(competition_error(&format!("Invalid start date: {}", e)).into_string())
        }
    };

    let end_observation_date = match OffsetDateTime::parse(
        &form.end_observation_date,
        &time::format_description::well_known::Rfc3339,
    ) {
        Ok(dt) => dt.to_offset(UtcOffset::UTC),
        Err(e) => {
            return Html(competition_error(&format!("Invalid end date: {}", e)).into_string())
        }
    };

    // Validate at least 1 location is selected
    if form.locations.is_empty() {
        return Html(competition_error("At least 1 location must be selected").into_string());
    }

    let Some(shape) = crate::domain::WindowShape::of(start_observation_date, end_observation_date)
    else {
        return Html(competition_error(crate::domain::WindowShape::RULE).into_string());
    };
    let number_of_values_per_entry = form.locations.len() * shape.metrics().len();

    if form.locations.len() > 50 {
        return Html(competition_error("Select no more than 50 stations").into_string());
    }
    {
        if start_observation_date <= OffsetDateTime::now_utc()
            || end_observation_date <= start_observation_date
        {
            return Html(
                competition_error("Choose a future, ordered observation window").into_string(),
            );
        }
        let window = Window {
            history_days: form.history_days.unwrap_or(3),
            start: start_observation_date,
            end: end_observation_date,
        };
        match state.admin_weather.eligible(&window).await {
            Ok(stations) if form.locations.iter().all(|id| stations.iter().any(|s| &s.station.station_id == id)) => {},
            Ok(_) => return Html(competition_error("A selected station is no longer eligible or lacks forecast coverage through this window. Refresh discovery and review the selection.").into_string()),
            Err(_) => return Html(competition_error("The oracle could not verify current station eligibility. Retry after discovery refreshes.").into_string()),
        }
    }

    let max_entries_per_player = form
        .max_entries_per_player
        .unwrap_or(crate::domain::ONE_ENTRY_PER_PLAYER);

    let coordinator_fee =
        match crate::domain::CoordinatorFee::parse_percent(&form.coordinator_fee_percentage) {
            Ok(fee) => fee,
            Err(error) => return Html(competition_error(&error.to_string()).into_string()),
        };

    if form
        .queued
        .as_deref()
        .is_some_and(|queued| !queued.is_empty() && queued != "false")
    {
        // Pools score lines and pay one winner; the seat count comes from demand.
        let request = crate::domain::CreateQueuedCompetition {
            id: form.id,
            signing_date,
            start_observation_date,
            end_observation_date,
            locations: form.locations,
            number_of_values_per_entry,
            entry_fee: form.entry_fee,
            coordinator_fee,
            relative_locktime_block_delta: form.relative_locktime_block_delta,
            min_players: form
                .min_players
                .unwrap_or(crate::domain::DEFAULT_MIN_PLAYERS),
            max_pool_size: form
                .max_pool_size
                .unwrap_or(coordinator_escrow::pools::MAX_POOL_PLAYERS),
            max_entries: form.max_entries,
            max_entries_per_player,
        };
        return match state.coordinator.create_queued_competition(request).await {
            Ok(competition) => {
                state.leaderboards.warm(&competition);
                Html(competition_success(&competition.id).into_string())
            }
            Err(e) => Html(competition_error(&e.to_string()).into_string()),
        };
    }

    let scoring_rules = match form
        .scoring_rules
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        None => crate::infra::oracle::ScoringRules::Lines,
        Some(text) => match crate::infra::oracle::ScoringRules::parse(text) {
            Some(rules) => rules,
            None => return Html(competition_error("Scoring must be lines or fixed").into_string()),
        },
    };

    // Calculate total pool
    let Some(total_competition_pool) = form.entry_fee.checked_mul(form.total_allowed_entries)
    else {
        return Html(
            competition_error("Total competition pool exceeds the supported amount").into_string(),
        );
    };

    // Create the competition via the coordinator
    let create_event = crate::domain::CreateEvent {
        id: form.id,
        signing_date,
        start_observation_date,
        end_observation_date,
        locations: form.locations,
        number_of_values_per_entry,
        number_of_places_win: form.number_of_places_win,
        total_allowed_entries: form.total_allowed_entries,
        entry_fee: form.entry_fee,
        coordinator_fee,
        total_competition_pool,
        relative_locktime_block_delta: form.relative_locktime_block_delta,
        unlisted: false,
        scoring_rules: Some(scoring_rules),
        scoring_fields: None,
        max_entries_per_player,
    };

    match state.coordinator.create_competition(create_event).await {
        Ok(competition) => {
            // Its entry form reads forecasts from the cache; fill it before anyone opens it.
            state.leaderboards.warm(&competition);
            Html(competition_success(&competition.id).into_string())
        }
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

async fn fetch_balance(state: &AppState) -> Result<WalletBalance, anyhow::Error> {
    let balance = state.bitcoin.get_balance().await?;
    Ok(WalletBalance {
        confirmed: balance.confirmed.to_sat(),
        unconfirmed: balance.unconfirmed.to_sat(),
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
            is_spent: false,
        })
        .collect())
}

/// Form data for deleting a competition
#[derive(Debug, Deserialize)]
pub struct DeleteCompetitionForm {
    pub competition_id: String,
}

/// Handle competition deletion from admin panel
pub async fn admin_delete_competition_handler(
    State(state): State<Arc<AppState>>,
    Form(form): Form<DeleteCompetitionForm>,
) -> Html<String> {
    // Parse UUID
    let competition_id = match uuid::Uuid::parse_str(&form.competition_id) {
        Ok(id) => id,
        Err(e) => {
            return Html(
                crate::templates::admin::dashboard::competition_error(&format!(
                    "Invalid competition ID: {}",
                    e
                ))
                .into_string(),
            )
        }
    };

    match state.coordinator.delete_competition(competition_id).await {
        Ok(()) => Html(
            crate::templates::admin::dashboard::competition_success_message(&format!(
                "Competition {} deleted successfully",
                competition_id
            ))
            .into_string(),
        ),
        Err(e) => Html(
            crate::templates::admin::dashboard::competition_error(&e.to_string()).into_string(),
        ),
    }
}

/// Test-only: Settle a ticket's HODL invoice without real Lightning payment.
/// Used by the synthetic testing tool. Marks the ticket as both paid and settled.
/// The admin router never registers this route on mainnet.
pub async fn admin_settle_test_invoice_handler(
    State(state): State<Arc<AppState>>,
    Path(ticket_id): Path<Uuid>,
) -> impl IntoResponse {
    match state
        .coordinator
        .competition_store
        .test_settle_ticket(ticket_id)
        .await
    {
        Ok(true) => {
            info!("Test-settled ticket {}", ticket_id);
            (StatusCode::OK, Json(serde_json::json!({ "settled": true })))
        }
        Ok(false) => {
            error!("Ticket {} not found or not in settleable state", ticket_id);
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Ticket not found or not reserved" })),
            )
        }
        Err(e) => {
            error!("Failed to test-settle ticket {}: {}", ticket_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        }
    }
}
