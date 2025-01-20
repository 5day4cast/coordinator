use axum::{
    extract::{Path, Query, State},
    response::ErrorResponse,
    Json,
};
use dlctix::{
    musig2::{AggNonce, PartialSignature, PubNonce},
    SigMap,
};
use hyper::StatusCode;
use log::{debug, error};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    domain::{AddEntry, Competition, CreateEvent, FundedContract},
    nostr_extractor::NostrAuth,
    AppState, SearchBy, UserEntry,
};

// Private route not exposed publically so NostrAuth is not needed
pub async fn create_event(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateEvent>,
) -> Result<Json<Competition>, ErrorResponse> {
    state
        .coordinator
        .create_competition(body)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error creating competition: {:?}", e);
            e.into()
        })
}

pub async fn add_event_entry(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddEntry>,
) -> Result<Json<UserEntry>, ErrorResponse> {
    let pubkey = pubkey.to_hex();
    state
        .coordinator
        .add_entry(pubkey, body)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error adding entry: {:?}", e);
            e.into()
        })
}

pub async fn get_entries(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Query(filter): Query<SearchBy>,
) -> Result<Json<Vec<UserEntry>>, ErrorResponse> {
    let pubkey = pubkey.to_hex();

    state
        .coordinator
        .get_entries(pubkey, filter)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error getting entries: {:?}", e);
            e.into()
        })
}

//TODO: add the ability to filter competition list
pub async fn get_competitions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Competition>>, ErrorResponse> {
    let competitions = state.coordinator.get_competitions().await.map_err(|e| {
        error!("error getting competitions: {:?}", e);
        e
    })?;
    let competitions = competitions
        .into_iter()
        .map(|mut comp| {
            if !comp.is_funding_tx_broadcasted() {
                comp.funding_transaction = None;
            }
            comp
        })
        .collect::<Vec<_>>();

    Ok(Json(competitions))
}

pub async fn get_competition(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<Competition>, ErrorResponse> {
    let mut competition = state
        .coordinator
        .get_competition(competition_id)
        .await
        .map_err(|e| {
            error!("error getting competition: {:?}", e);
            e
        })?;

    if !competition.is_funding_tx_broadcasted() {
        competition.funding_transaction = None;
    }

    Ok(Json(competition))
}

pub async fn get_contract_parameters(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<FundedContract>, ErrorResponse> {
    let pubkey = pubkey.to_hex();
    state
        .coordinator
        .get_contract_parameters(pubkey, competition_id)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error getting contract parameters: {:?}", e);
            e.into()
        })
}

pub async fn submit_public_nonces(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    Json(public_nonces): Json<SigMap<PubNonce>>,
) -> Result<StatusCode, ErrorResponse> {
    let pubkey = pubkey.to_hex();
    debug!("submitted nonce by: {} {:?}", pubkey, public_nonces);

    state
        .coordinator
        .submit_public_nonces(pubkey, competition_id, entry_id, public_nonces)
        .await
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            error!("error submitting public nonces: {:?}", e);
            e.into()
        })
}

pub async fn get_aggregate_nonces(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<SigMap<AggNonce>>, ErrorResponse> {
    let pubkey = pubkey.to_hex();
    state
        .coordinator
        .get_aggregate_nonces(pubkey, competition_id)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error getting aggregate nonces: {:?}", e);
            e.into()
        })
}

pub async fn submit_partial_signatures(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    Json(partial_sigs): Json<SigMap<PartialSignature>>,
) -> Result<StatusCode, ErrorResponse> {
    let pubkey = pubkey.to_hex();
    debug!("submitted partial_sigs by: {} {:?}", pubkey, partial_sigs);

    state
        .coordinator
        .submit_partial_signatures(pubkey, competition_id, entry_id, partial_sigs)
        .await
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            error!("error submitting partial signatures: {:?}", e);
            e.into()
        })
}
