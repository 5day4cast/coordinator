//! The HTTP API. Every route except `/health` needs `Authorization: Bearer <token>`.
//!
//! | Route | Use |
//! | --- | --- |
//! | `POST /v1/swaps` | `{ "escrow_address", "amount_sat", "preimage"? }` → the swap and its invoice. Returns the open swap if the escrow already has one. |
//! | `GET /v1/swaps/{id}` | A swap's state. |
//! | `GET /v1/wallet` | The Ark wallet's addresses and balance. |
//! | `POST /v1/wallet/board` | Move confirmed boarding coins into VTXOs in the next batch. |

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::swap::Swapper;

#[derive(Clone)]
struct AppState {
    swapper: Arc<Swapper>,
    token: Arc<String>,
    /// This instance, as a holder of the worker lease.
    holder: Arc<String>,
}

pub fn router(swapper: Arc<Swapper>, token: String, holder: String) -> Router {
    let state = AppState {
        swapper,
        token: Arc::new(token),
        holder: Arc::new(holder),
    };
    let authenticated = Router::new()
        .route("/v1/swaps", post(create_swap))
        .route("/v1/swaps/{id}", get(get_swap))
        .route("/v1/wallet", get(wallet))
        .route("/v1/wallet/board", post(board))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(authenticated)
        .with_state(state)
}

async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if constant_time_eq(presented.as_bytes(), state.token.as_bytes()) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSwap {
    escrow_address: String,
    amount_sat: u64,
    /// Hex. The invoice pays to its SHA256; the service makes one if absent.
    #[serde(default)]
    preimage: Option<String>,
}

async fn create_swap(State(state): State<AppState>, Json(request): Json<CreateSwap>) -> Response {
    let preimage = match request.preimage.as_deref().map(hex::decode).transpose() {
        Ok(Some(bytes)) => match <[u8; 32]>::try_from(bytes) {
            Ok(preimage) => Some(preimage),
            Err(_) => {
                return failure(
                    StatusCode::BAD_REQUEST,
                    anyhow::anyhow!("the preimage must be 32 bytes"),
                )
            }
        },
        Ok(None) => None,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error.into()),
    };
    match state
        .swapper
        .create(&request.escrow_address, request.amount_sat, preimage)
        .await
    {
        Ok(swap) => (StatusCode::CREATED, Json(swap)).into_response(),
        Err(error) => failure(StatusCode::BAD_REQUEST, error),
    }
}

async fn get_swap(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    match state.swapper.store.get(id).await {
        Ok(Some(swap)) => Json(swap).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn wallet(State(state): State<AppState>) -> Response {
    match state.swapper.wallet.view().await {
        Ok(view) => Json(view).into_response(),
        Err(error) => failure(StatusCode::BAD_GATEWAY, error),
    }
}

async fn board(State(state): State<AppState>) -> Response {
    // Only the lease holder moves the wallet's coins, so two instances never spend the same one.
    match state.swapper.store.holds_lease(&state.holder).await {
        Ok(true) => {}
        Ok(false) => {
            return failure(
                StatusCode::CONFLICT,
                anyhow::anyhow!("another ark-swapd instance runs the wallet; retry there"),
            )
        }
        Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
    match state.swapper.wallet.board().await {
        Ok(txid) => {
            Json(json!({ "commitment_txid": txid.map(|txid| txid.to_string()) })).into_response()
        }
        Err(error) => failure(StatusCode::BAD_GATEWAY, error),
    }
}

fn failure(status: StatusCode, error: anyhow::Error) -> Response {
    log::warn!("{status}: {error:#}");
    (status, Json(json!({ "error": format!("{error:#}") }))).into_response()
}
