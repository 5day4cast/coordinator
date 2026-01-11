use axum::{
    extract::State,
    http::StatusCode,
    response::{ErrorResponse, IntoResponse},
    Json,
};
use log::{debug, error};
use nostr_sdk::ToBech32;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    api::extractors::{AuthError, NostrAuth},
    domain,
    startup::AppState,
};

pub async fn login(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ErrorResponse> {
    let pubkey = pubkey.to_bech32().expect("public bech32 format");
    debug!("login with pubkey: {}", pubkey);

    match state.users_info.login(pubkey).await {
        Ok(user_info) => Ok((StatusCode::CREATED, Json(user_info))),
        Err(domain::Error::NotFound(e)) => {
            error!("Failed to login: {}", e);
            Err(ErrorResponse::from(AuthError::InvalidLogin))
        }
        Err(e) => {
            error!("Failed to login: {}", e);
            Err(ErrorResponse::from(e))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterPayload {
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
}

pub async fn register(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<RegisterPayload>,
) -> Result<impl IntoResponse, ErrorResponse> {
    let pubkey = pubkey.to_bech32().expect("public bech32 format");

    debug!("registering user: {}", pubkey);
    match state.users_info.register(pubkey, body).await {
        Ok(user_info) => Ok((StatusCode::CREATED, Json(user_info))),
        Err(e) => {
            error!("failed to register: {}", e);
            Err(ErrorResponse::from(e))
        }
    }
}
