use std::{borrow::Borrow, sync::Arc};

use axum::{
    extract::{Query, State},
    response::{ErrorResponse, IntoResponse, Response},
    Json,
};
use dlctix::musig2::secp256k1;
use hyper::StatusCode;
use log::error;
use serde_json::json;

use crate::{
    domain::{AddEntry, CoordinatorInfo, CreateEvent, CreateEventMessage, Error},
    AppState, SearchBy, UserEntry,
};

pub async fn create_event(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateEventMessage>,
) -> Result<StatusCode, ErrorResponse> {
    let secp = secp256k1::Secp256k1::new();
    let signature = secp.sign_schnorr(&body.message().unwrap(), &state.coordinator.keypair());

    let event = CreateEvent::new(
        body,
        CoordinatorInfo {
            pubkey: state.coordinator.public_key(),
            signature: signature.to_string(),
        },
    );
    state.oracle_client.create_event(event).await.map_err(|e| {
        error!("error creating event: {:?}", e);
        Error::OracleFailed(e)
    })?;
    Ok(StatusCode::CREATED)
}

pub async fn add_event_entry(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddEntry>,
) -> Result<Json<UserEntry>, ErrorResponse> {
    state
        .coordinator
        .add_entry(body)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error adding entry: {:?}", e);
            e.into()
        })
}

pub async fn get_entries(
    State(state): State<Arc<AppState>>,
    Query(filter): Query<SearchBy>,
) -> Result<Json<Vec<UserEntry>>, ErrorResponse> {
    state
        .coordinator
        .get_entries(filter)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error getting entries: {:?}", e);
            e.into()
        })
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, error_message) = match self.borrow() {
            Error::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::InvalidSignature(_) => (StatusCode::FORBIDDEN, self.to_string()),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::from("internal server error"),
            ),
        };
        let body = Json(json!({
            "error": error_message,
        }));
        (status, body).into_response()
    }
}
