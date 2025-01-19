use axum::{
    extract::{Path, Query, State},
    response::ErrorResponse,
    Json,
};
use log::error;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    domain::{AddEntry, Competition, CreateEvent},
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

pub async fn get_competition(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<Competition>, ErrorResponse> {
    state
        .coordinator
        .get_competition(competition_id)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error getting competition: {:?}", e);
            e.into()
        })
}
