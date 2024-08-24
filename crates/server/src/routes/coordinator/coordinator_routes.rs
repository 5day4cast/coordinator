use std::{borrow::Borrow, sync::Arc};

use axum::{
    extract::{Query, State},
    response::{ErrorResponse, IntoResponse, Response},
    Json,
};
use hyper::StatusCode;
use log::error;
use serde_json::json;

use crate::{
    domain::{AddEntry, Error},
    AppState, SearchBy, UserEntry,
};

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
