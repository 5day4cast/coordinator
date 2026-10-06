#[path = "list_response.rs"]
mod list_response;
use axum::http::StatusCode;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use axum::{http::HeaderMap, response::Response};
use bitcoin::PublicKey;
use dlctix::{
    musig2::{AggNonce, PartialSignature, PubNonce},
    SigMap,
};
use list_response::ListQuery;
use log::{debug, error, info, warn};
use nostr::ToBech32;
use serde::Deserialize;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    api::{
        extractors::{AuthedJson, NostrAuth},
        routes::ApiError,
    },
    domain::{
        AddEntry, Competition, CreateEvent, CreateQueuedCompetition, Error as DomainError,
        FundedContract, PayoutClaimInfo, PayoutClaimReceipt, PayoutInfo, SearchBy, TicketRefund,
        TicketRegistration, TicketResponse, TicketStatus, UserEntry,
    },
    infra::lnurl::LightningAddress,
    startup::AppState,
};

// Private route not exposed publically so NostrAuth is not needed
pub async fn create_competition(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateEvent>,
) -> Result<Json<Competition>, ApiError> {
    let competition = state
        .coordinator
        .create_competition(body)
        .await
        .map_err(|e| {
            log_failure("creating competition", &e);
            ApiError::from(e)
        })?;
    // Its entry form reads forecasts from the cache; fill it before anyone opens it.
    state.leaderboards.warm(&competition);
    Ok(Json(competition))
}

/// Create a queued competition: players enter without a seat count, and it forms pools when
/// registration closes. Private, like `create_competition`.
pub async fn create_queued_competition(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateQueuedCompetition>,
) -> Result<Json<Competition>, ApiError> {
    let competition = state
        .coordinator
        .create_queued_competition(body)
        .await
        .map_err(|e| {
            log_failure("creating queued competition", &e);
            ApiError::from(e)
        })?;
    state.leaderboards.warm(&competition);
    Ok(Json(competition))
}

/// Request to settle a ticket using the escrow preimage
#[derive(Debug, Deserialize)]
pub struct SettleEscrowRequest {
    pub ticket_id: Uuid,   // The ID of the ticket to settle
    pub preimage: String,  // The preimage that unlocks both the HODL invoice and escrow
    pub escrow_tx: String, // The escrow transaction hex for verification
}

/// Request to obtain a ticket, including the user's Bitcoin public key
/// needed for the escrow transaction refund path
#[derive(Debug, Deserialize)]
pub struct TicketRequest {
    pub btc_pubkey: String, // Bitcoin public key for escrow refund path
    #[serde(default)]
    pub payout: Option<coordinator_core::PayoutRegistrationRequest>,
}

/// Request a competition ticket to enter the DLC
///
/// This endpoint:
/// 1. Generates a HODL invoice for the user to pay
/// 2. Creates an escrow transaction with dual-purpose
/// 3. Returns both to the user
///
/// The refund transaction:
/// 1. Is fully signed by the coordinator and ready to broadcast
/// 2. Spends from coordinator UTXOs directly to the user's address
/// 3. Becomes invalid when the DLC funding transaction is broadcast
///    - This happens because the funding transaction spends the same UTXOs
///    - Creates an elegant invalidation mechanism with no additional signatures needed
///    - Provides security if the coordinator disappears or DLC never forms
///
/// The same preimage is used for multiple purposes:
/// - The HODL invoice (revealed to user when coordinator settles the invoice)
/// - The ticket secret (to claim winnings if user wins the DLC)
/// - The escrow transaction refund path (to claim refund if needed)
///
/// Retries: `payout.entry_id` is the request's idempotency key for the player on this
/// competition. Sending the same request again (same entry id, entry key and payout choices)
/// while its ticket is unpaid answers `200` with the same ticket, invoice and payout policy,
/// even while the first request is still being answered. A request for another entry while one
/// is reserved answers `409 Conflict` and releases the reservation; request the ticket again.
/// After the ticket is entered, or its reservation lapses unpaid, a request is a new one.
pub async fn request_competition_ticket(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: request,
    }: AuthedJson<TicketRequest>,
) -> Result<Json<TicketResponse>, ApiError> {
    let btc_pubkey = PublicKey::from_str(&request.btc_pubkey).map_err(|e| {
        info!("Invalid Bitcoin public key in a ticket request: {e}");
        ApiError::Status(StatusCode::BAD_REQUEST)
    })?;

    state
        .coordinator
        .request_ticket_with_payout(pubkey.to_hex(), competition_id, btc_pubkey, request.payout)
        .await
        .map(Json)
        .map_err(|e| {
            if e.is_refusal() {
                warn!("ticket request for {competition_id} refused: {e}");
            } else {
                error!(
                    "error requesting ticket for {competition_id}: {}",
                    e.detail()
                );
            }
            e.into()
        })
}

/// Refusals a client can act on, such as an unknown ticket or one not reserved by this
/// player, are expected answers and logged at info, so errors mean something is broken.
fn log_failure(action: &str, e: &DomainError) {
    if e.is_refusal() {
        info!("{action} refused: {e}");
    } else {
        error!("error {action}: {}", e.detail());
    }
}

pub async fn get_ticket_status(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, ticket_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<TicketStatus>, ApiError> {
    state
        .coordinator
        .get_ticket_status(pubkey.to_hex(), competition_id, ticket_id)
        .await
        .map(Json)
        .map_err(|e| {
            log_failure("getting ticket status", &e);
            e.into()
        })
}

/// Keep the Keymeld registration the player's browser sealed for their ticket, sent before it
/// shows the ticket's invoice, so that the ticket can be refunded even if it is never used for an
/// entry. It is deleted if the reservation is released unpaid.
pub async fn register_ticket(
    State(state): State<Arc<AppState>>,
    Path((competition_id, ticket_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<TicketRegistration>,
) -> Result<StatusCode, ApiError> {
    state
        .coordinator
        .register_ticket(pubkey.to_hex(), competition_id, ticket_id, body)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|e| {
            log_failure("registering ticket", &e);
            e.into()
        })
}

/// Where a player's refund has got to, when their competition never kicked off.
pub async fn get_ticket_refund(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, ticket_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Option<TicketRefund>>, ApiError> {
    state
        .coordinator
        .get_ticket_refund(pubkey.to_hex(), competition_id, ticket_id)
        .await
        .map(Json)
        .map_err(|e| {
            log_failure("getting ticket refund", &e);
            e.into()
        })
}

/* Two steps
1) submit entry with ticket_id for the hold invoice
2) pay the hold invoice (server watching invoice state to become accepted)
3) server marks ticket as paid -> include in competition
*/
pub async fn add_event_entry(
    State(state): State<Arc<AppState>>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<AddEntry>,
) -> Result<Json<UserEntry>, ApiError> {
    let pubkey = pubkey.to_hex();
    state
        .coordinator
        .add_entry(pubkey, body)
        .await
        .map(Json)
        .map_err(|e| {
            if e.is_refusal() {
                warn!("entry refused: {e}");
            } else {
                error!("error adding entry: {}", e.detail());
            }
            e.into()
        })
}

/// `GET /api/v1/entries?event_id=<id>` narrows the list to one competition. The query
/// cannot carry `SearchBy::event_ids`, a list, so clients asked with `event_id` and were
/// sent every entry the player has ever made.
// Keep URL scalars in one struct. Serde flatten buffers query values as strings,
// which prevents the nested usize and bool fields from using URL deserialization.
pub type EntriesQuery = ListQuery;

pub async fn get_entries(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Query(query): Query<EntriesQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let pubkey = pubkey.to_hex();
    let mut page = query.page()?;
    if query.event_id.is_some() {
        page.history = true;
    }
    let events: Vec<_> = query.event_id.into_iter().collect();
    let mut ids = state
        .coordinator
        .competition_store
        .entry_page_ids(&pubkey, &events, &page)
        .await
        .map_err(DomainError::from)?;
    let more = ids.len() > page.limit;
    ids.truncate(page.limit);
    let next = more.then(|| ids.last().copied()).flatten();
    let rows = state
        .coordinator
        .competition_store
        .get_user_entries_selected(pubkey, SearchBy { event_ids: None }, Some(&ids))
        .await
        .map_err(DomainError::from)?;
    list_response::response(&headers, &rows, next, true)
}

pub async fn get_competitions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let page = query.page()?;
    let mut ids = state
        .coordinator
        .competition_store
        .competition_page_ids(&page)
        .await
        .map_err(DomainError::from)?;
    let more = ids.len() > page.limit;
    ids.truncate(page.limit);
    let next = more.then(|| ids.last().copied()).flatten();
    let mut competitions = state.coordinator.get_competitions_page(&ids).await?;
    for competition in &mut competitions {
        if !competition.is_funding_broadcasted() {
            competition.funding_transaction = None;
        }
    }
    state
        .coordinator
        .attach_min_players_now(&mut competitions)
        .await;
    list_response::response(&headers, &competitions, next, false)
}

pub async fn get_competition(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<Competition>, ApiError> {
    let mut competition = state
        .coordinator
        .get_competition(competition_id)
        .await
        .inspect_err(|e| {
            log_failure("getting competition", e);
        })?;

    if !competition.is_funding_broadcasted() {
        competition.funding_transaction = None;
    }
    state
        .coordinator
        .attach_min_players_now(std::slice::from_mut(&mut competition))
        .await;

    Ok(Json(competition))
}

pub async fn get_contract_parameters(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<FundedContract>, ApiError> {
    let pubkey = pubkey.to_hex();
    state
        .coordinator
        .get_contract_parameters(pubkey, competition_id)
        .await
        .map(Json)
        .map_err(|e| {
            log_failure("getting contract parameters", &e);
            e.into()
        })
}

pub async fn submit_public_nonces(
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: public_nonces,
    }: AuthedJson<SigMap<PubNonce>>,
) -> Result<StatusCode, ApiError> {
    let pubkey = pubkey.to_hex();
    debug!("submitted nonce by: {} {:?}", pubkey, public_nonces);

    state
        .coordinator
        .submit_public_nonces(pubkey, competition_id, entry_id, public_nonces)
        .await
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            log_failure("submitting public nonces", &e);
            e.into()
        })
}

pub async fn get_aggregate_nonces(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<SigMap<AggNonce>>, ApiError> {
    let pubkey = pubkey.to_hex();
    state
        .coordinator
        .get_aggregate_nonces(pubkey, competition_id)
        .await
        .map(Json)
        .map_err(|e| {
            log_failure("getting aggregate nonces", &e);
            e.into()
        })
}

#[derive(Debug, Clone, Deserialize)]
pub struct FinalSignatures {
    pub funding_psbt_base64: String,
    pub partial_signatures: SigMap<PartialSignature>,
}

pub async fn submit_final_signatures(
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: final_signatures,
    }: AuthedJson<FinalSignatures>,
) -> Result<StatusCode, ApiError> {
    let pubkey = pubkey.to_hex();
    debug!(
        "submitted final signatures by: {} {:?}",
        pubkey, final_signatures
    );

    state
        .coordinator
        .submit_final_signatures(pubkey, competition_id, entry_id, final_signatures)
        .await
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            log_failure("submitting partial signatures", &e);
            e.into()
        })
}

pub async fn submit_ticket_payout(
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: payout_info,
    }: AuthedJson<PayoutInfo>,
) -> Result<StatusCode, ApiError> {
    let pubkey = pubkey.to_hex();
    debug!("submitted payout by: {} for entry {}", pubkey, entry_id);

    state
        .coordinator
        .submit_ticket_payout(pubkey, competition_id, entry_id, payout_info)
        .await
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            log_failure("submitting payout information", &e);
            e.into()
        })
}

/// One-click payout to the Lightning Address on the account.
pub async fn claim_ticket_payout(
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: claim,
    }: AuthedJson<PayoutClaimInfo>,
) -> Result<Json<PayoutClaimReceipt>, ApiError> {
    let npub = pubkey.to_bech32().expect("public bech32 format");
    let user = state.users_info.login(npub).await?;
    let address = user
        .lightning_address
        .as_deref()
        .map(LightningAddress::parse)
        .transpose()
        .map_err(|e| ApiError::from(DomainError::BadRequest(e.to_string())))?
        .ok_or_else(|| {
            ApiError::from(DomainError::BadRequest(
                "Add a Lightning Address on the payouts page first".into(),
            ))
        })?;
    let pubkey = pubkey.to_hex();
    debug!("payout claim by: {} for entry {}", pubkey, entry_id);

    state
        .coordinator
        .claim_ticket_payout(pubkey, competition_id, entry_id, claim, &address)
        .await
        .map(Json)
        .map_err(|e| {
            log_failure("claiming payout", &e);
            e.into()
        })
}

pub async fn get_payout_authorization(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<crate::domain::PayoutAuthorizationInfo>, ApiError> {
    state
        .coordinator
        .payout_authorization_info(&pubkey.to_hex(), competition_id, entry_id)
        .await
        .map(Json)
        .map_err(Into::into)
}

pub async fn submit_invoice_fallback(
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<crate::domain::InvoiceFallbackRequest>,
) -> Result<Json<Uuid>, ApiError> {
    state
        .coordinator
        .submit_invoice_fallback(&pubkey.to_hex(), competition_id, entry_id, body)
        .await
        .map(Json)
        .map_err(Into::into)
}

/// The network fee a ticket issued now would carry, for display before a ticket is requested.
/// A ticket's own fee is fixed when it is issued and comes with it. 503 while the fee estimate
/// is unavailable, or in settle-only mode, when no ticket can be issued either.
pub async fn get_network_fee(
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::domain::NetworkFeeQuote>, ApiError> {
    if state.coordinator.settle_only() {
        return Err(crate::domain::Error::SettleOnly.into());
    }
    state
        .coordinator
        .network_fee_quote()
        .await
        .map(Json)
        .map_err(Into::into)
}

pub async fn get_payout_terms(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<crate::domain::PayoutTermsQuote>, ApiError> {
    state
        .coordinator
        .payout_terms_quote(competition_id)
        .await
        .map(Json)
        .map_err(Into::into)
}

#[cfg(test)]
mod entries_query_tests {
    use super::EntriesQuery;
    use axum::{extract::Query, http::Uri};
    use uuid::Uuid;

    #[test]
    fn entries_can_be_narrowed_to_one_competition() {
        let id = Uuid::now_v7();
        let uri: Uri = format!("/api/v1/entries?event_id={id}").parse().unwrap();
        let Query(query) = Query::<EntriesQuery>::try_from_uri(&uri).unwrap();
        assert_eq!(query.event_id, Some(id));
        let Query(all) =
            Query::<EntriesQuery>::try_from_uri(&"/api/v1/entries".parse().unwrap()).unwrap();
        assert_eq!(all.event_id, None);
    }
}

#[cfg(test)]
mod min_players_now_tests {
    use crate::config::KickoffCheckSettings;
    use crate::domain::{Competition, CompetitionKind, CoordinatorFee, CreateEvent, QueueSummary};
    use coordinator_escrow::pools::PoolRules;
    use time::{Duration, OffsetDateTime};
    use uuid::Uuid;

    fn competition(start: OffsetDateTime) -> Competition {
        Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: start + Duration::days(2),
            start_observation_date: start,
            end_observation_date: start + Duration::days(1),
            locations: vec!["KORD".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 25,
            entry_fee: 1_000,
            coordinator_fee: CoordinatorFee::whole_percent(3),
            total_competition_pool: 25_000,
            relative_locktime_block_delta: None,
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
        })
    }

    fn queue(start: OffsetDateTime, min_players: usize) -> Competition {
        let mut queue = competition(start);
        queue.kind = CompetitionKind::Queued;
        queue.queue = Some(QueueSummary {
            pool_rules: PoolRules::new(min_players, 25).unwrap(),
            entries: 1,
            max_entries: 100,
            held: 1,
            stake_sats: 1_000,
            terms_digest: String::new(),
            pools: Vec::new(),
        });
        queue
    }

    #[test]
    fn the_api_gives_the_players_a_competition_needs_now_while_it_takes_entries() {
        let settings = KickoffCheckSettings::default();
        let now = OffsetDateTime::now_utc();
        let open = now + Duration::hours(6);

        // The terms' minimum while fees allow small pools, raised to five above them.
        let single = competition(open);
        assert_eq!(single.min_players_to_start(&settings, 1, now), Some(2));
        assert_eq!(single.min_players_to_start(&settings, 3, now), Some(5));
        let small = queue(open, 3);
        assert_eq!(small.min_players_to_start(&settings, 2, now), Some(3));
        assert_eq!(small.min_players_to_start(&settings, 9, now), Some(5));
        assert_eq!(
            queue(open, 7).min_players_to_start(&settings, 9, now),
            Some(7)
        );
        let off = KickoffCheckSettings {
            enabled: false,
            ..KickoffCheckSettings::default()
        };
        assert_eq!(small.min_players_to_start(&off, 9, now), Some(3));

        // Not once entries close, nor for a pool or a cancelled competition.
        assert_eq!(
            competition(now - Duration::minutes(1)).min_players_to_start(&settings, 1, now),
            None
        );
        let mut pool = competition(open);
        pool.kind = CompetitionKind::Pool;
        assert_eq!(pool.min_players_to_start(&settings, 1, now), None);
        let mut cancelled = queue(open, 2);
        cancelled.cancelled_at = Some(now);
        assert_eq!(cancelled.min_players_to_start(&settings, 1, now), None);

        // Serialised only when filled in.
        let mut shown = small.clone();
        let json = serde_json::to_value(&shown).unwrap();
        assert!(json.get("min_players_now").is_none());
        shown.min_players_now = shown.min_players_to_start(&settings, 9, now);
        let json = serde_json::to_value(&shown).unwrap();
        assert_eq!(json["min_players_now"], 5);
        assert_eq!(json["pool_rules"]["min_players"], 3);
    }
}

#[cfg(test)]
mod entry_query_tests {
    use super::EntriesQuery;
    use axum::{extract::Query, http::Uri};
    use uuid::Uuid;

    #[test]
    fn paginated_entries_parse_the_actual_synth_query() {
        let event = Uuid::now_v7();
        let cursor = Uuid::now_v7();
        let uri: Uri =
            format!("/api/v1/entries?event_id={event}&limit=100&cursor={cursor}&history=true")
                .parse()
                .unwrap();
        let Query(query) = Query::<EntriesQuery>::try_from_uri(&uri).unwrap();
        assert_eq!(query.event_id, Some(event));
        let page = query.page().unwrap();
        assert_eq!(page.limit, 100);
        assert_eq!(page.before, Some(cursor));
        assert!(page.history);
        for value in ["-1", "abc"] {
            let uri: Uri = format!("/api/v1/entries?limit={value}").parse().unwrap();
            assert!(Query::<EntriesQuery>::try_from_uri(&uri).is_err());
        }
        for value in ["0", "101"] {
            let uri: Uri = format!("/api/v1/entries?limit={value}").parse().unwrap();
            assert!(Query::<EntriesQuery>::try_from_uri(&uri)
                .unwrap()
                .page()
                .is_err());
        }
        let uri: Uri = "/api/v1/entries".parse().unwrap();
        assert_eq!(
            Query::<EntriesQuery>::try_from_uri(&uri)
                .unwrap()
                .page()
                .unwrap()
                .limit,
            50
        );
    }
}
