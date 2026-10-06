//! Recovery records: the key and relays they are published with, and a player's recovery file.
//! See docs/RECOVERY.md.

use axum::{
    extract::State,
    http::{
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE},
        HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    Json,
};
use log::error;
use nostr::ToBech32;
use std::sync::Arc;
use time::OffsetDateTime;

use crate::{
    api::{extractors::NostrAuth, routes::ApiError},
    domain::{self, recovery::kit_file_name},
    startup::AppState,
};

/// The recovery key, network and relays, so a recovery tool can find the records. Not found
/// while recovery records are off.
pub async fn get_recovery_info(State(state): State<Arc<AppState>>) -> Response {
    match &state.recovery {
        Some(recovery) => Json(recovery.info()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The signed-in player's recovery file: their wallet backup and entry records, encrypted to
/// them, and the contracts of the competitions they entered. Only the player's own records.
pub async fn get_recovery_kit(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
) -> Result<Response, ApiError> {
    let Some(recovery) = &state.recovery else {
        return Err(ApiError::Status(StatusCode::NOT_FOUND));
    };
    let npub = pubkey.to_bech32().unwrap_or_else(|never| match never {});
    let user_pubkey = pubkey.to_hex();
    let (user, events) = tokio::join!(
        state.users_info.login(npub.clone()),
        state
            .coordinator
            .competition_store
            .recovery_kit_events(&user_pubkey),
    );
    let wallet_blob = user.ok().map(|user| user.encrypted_bitcoin_private_key);
    let events = events.map_err(domain::Error::from)?;
    let now = OffsetDateTime::now_utc().unix_timestamp().max(0) as u64;
    let kit = recovery
        .kit(&pubkey, wallet_blob.as_deref(), &events, now)
        .map_err(|e| {
            error!("Cannot build a recovery file: {e:#}");
            ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
    let body = serde_json::to_vec_pretty(&kit).map_err(domain::Error::from)?;
    let disposition =
        HeaderValue::try_from(format!("attachment; filename=\"{}\"", kit_file_name(&npub)))
            .unwrap_or(HeaderValue::from_static("attachment"));
    Ok((
        [
            (CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (CONTENT_DISPOSITION, disposition),
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response())
}
