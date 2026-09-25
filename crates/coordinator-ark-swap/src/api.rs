//! The HTTP API. Every route except `/health` needs `Authorization: Bearer <token>`.
//!
//! | Route | Use |
//! | --- | --- |
//! | `POST /v1/swaps` | `{ "escrow_address", "amount_sat", "preimage"? }` → the swap and its invoice. Returns the open swap if the escrow already has one. |
//! | `GET /v1/swaps/{id}` | A swap's state. |
//! | `GET /v1/swaps?payment_hash=<hex>` | The swap whose invoice pays to that hash, for tracing a payment to its escrow. |
//! | `GET /v1/swaps?without_escrow_vtxo=true` | Swaps in `escrow_paid`, `settled` or `unsettled` that record no escrow VTXO, oldest first. Read-only, for finding money by hand. |
//! | `POST /v1/refunds` | `{ "payment_hash", "amount_sat", "player_key", "deadline" }` → the swap an unused escrow's refund pays. Returns the swap already minted for that invoice. |
//! | `POST /v1/refunds/{id}/paid` | `{ "preimage" }` → records the payment and claims the swap. |
//! | `GET /v1/refunds/{id}` | A refund's state. |
//! | `GET /v1/wallet` | The Ark wallet's addresses and balance. |
//! | `POST /v1/wallet/board` | Move confirmed boarding coins into VTXOs in the next batch. |

use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
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
        .route("/v1/swaps", post(create_swap).get(find_swap))
        .route("/v1/swaps/{id}", get(get_swap))
        .route("/v1/refunds", post(mint_refund))
        .route("/v1/refunds/{id}/paid", post(refund_paid))
        .route("/v1/refunds/{id}", get(get_refund))
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindSwap {
    /// Hex. The hash of the invoice the swap issued.
    #[serde(default)]
    payment_hash: Option<String>,
    /// List the swaps that paid an escrow without recording its output.
    #[serde(default)]
    without_escrow_vtxo: bool,
}

async fn find_swap(State(state): State<AppState>, Query(query): Query<FindSwap>) -> Response {
    let payment_hash = match (query.payment_hash, query.without_escrow_vtxo) {
        (Some(hash), false) => hash,
        (None, true) => {
            return match state.swapper.store.without_escrow_vtxo().await {
                Ok(swaps) => Json(swaps).into_response(),
                Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
            };
        }
        _ => {
            return failure(
                StatusCode::BAD_REQUEST,
                anyhow::anyhow!("give either payment_hash or without_escrow_vtxo=true"),
            )
        }
    };
    let payment_hash = match bytes32(&payment_hash) {
        Ok(hash) => hex::encode(hash),
        Err(error) => return failure(StatusCode::BAD_REQUEST, error),
    };
    match state.swapper.store.for_payment_hash(&payment_hash).await {
        Ok(Some(swap)) => Json(swap).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn get_swap(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    match state.swapper.store.get(id).await {
        Ok(Some(swap)) => Json(swap).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MintRefund {
    /// Hex. The invoice the swap service pays, which the swap's claim leaf commits to.
    payment_hash: String,
    amount_sat: u64,
    /// Hex x-only. The player's entry key, which reclaims the swap after the deadline.
    player_key: String,
    /// UNIX seconds. After this the player may take the swap back.
    deadline: u32,
}

async fn mint_refund(State(state): State<AppState>, Json(request): Json<MintRefund>) -> Response {
    let payment_hash = match bytes32(&request.payment_hash) {
        Ok(hash) => hash,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error),
    };
    let player = match request.player_key.parse() {
        Ok(key) => key,
        Err(error) => {
            return failure(
                StatusCode::BAD_REQUEST,
                anyhow::anyhow!("the player key must be an x-only public key: {error}"),
            )
        }
    };
    match state
        .swapper
        .mint_refund(payment_hash, request.amount_sat, player, request.deadline)
        .await
    {
        Ok(refund) => (StatusCode::CREATED, Json(refund)).into_response(),
        Err(error) => failure(StatusCode::BAD_REQUEST, error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefundPaid {
    /// Hex. What paying the player's invoice revealed, and what claims the swap.
    preimage: String,
}

async fn refund_paid(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(request): Json<RefundPaid>,
) -> Response {
    // Only the lease holder claims, so two instances never spend the same swap.
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
    let preimage = match bytes32(&request.preimage) {
        Ok(preimage) => preimage,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error),
    };
    match state.swapper.refund_paid(id, preimage).await {
        Ok(refund) => Json(refund).into_response(),
        Err(error) => failure(StatusCode::BAD_REQUEST, error),
    }
}

async fn get_refund(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    match state.swapper.store.refund(id).await {
        Ok(Some(refund)) => Json(refund).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

fn bytes32(value: &str) -> anyhow::Result<[u8; 32]> {
    <[u8; 32]>::try_from(hex::decode(value)?)
        .map_err(|_| anyhow::anyhow!("expected 32 bytes, hex encoded"))
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
