use axum::{
    extract::State,
    http::StatusCode,
    response::{ErrorResponse, IntoResponse},
    Json,
};
use bdk_wallet::{Balance, LocalOutput};
use log::{debug, error};
use serde::Serialize;
use std::{collections::HashMap, sync::Arc};

use crate::{domain::Error, AppState, SendOptions};

#[derive(Debug, Serialize)]
pub struct AddressResponse {
    address: String,
}

#[derive(Debug, Serialize)]
pub struct OutputsResponse {
    outputs: Vec<LocalOutput>,
}

#[derive(Debug, Serialize)]
pub struct SendResponse {
    txid: String,
}

#[derive(Debug, Serialize)]
pub struct FeeEstimatesResponse {
    fee_estimates: HashMap<u16, f64>,
}

pub async fn get_balance(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Balance>, ErrorResponse> {
    debug!("Getting wallet balance");

    match state.bitcoin.get_balance().await {
        Ok(balance) => Ok(Json(balance)),
        Err(e) => {
            error!("Failed to get balance: {}", e);
            Err(ErrorResponse::from(Error::Bitcoin(e)))
        }
    }
}

pub async fn get_next_address(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AddressResponse>, ErrorResponse> {
    debug!("Getting next unused address");

    match state.bitcoin.get_next_address().await {
        Ok(address) => Ok(Json(AddressResponse { address })),
        Err(e) => {
            error!("Failed to get next address: {}", e);
            Err(ErrorResponse::from(Error::Bitcoin(e)))
        }
    }
}

pub async fn get_outputs(
    State(state): State<Arc<AppState>>,
) -> Result<Json<OutputsResponse>, ErrorResponse> {
    debug!("Getting wallet outputs");

    match state.bitcoin.get_outputs().await {
        Ok(outputs) => Ok(Json(OutputsResponse { outputs })),
        Err(e) => {
            error!("Failed to get outputs: {}", e);
            Err(ErrorResponse::from(Error::Bitcoin(e)))
        }
    }
}

pub async fn get_estimated_fee_rates(
    State(state): State<Arc<AppState>>,
) -> Result<Json<FeeEstimatesResponse>, ErrorResponse> {
    debug!("Getting wallet outputs");

    match state.bitcoin.get_estimated_fee_rates().await {
        Ok(fee_estimates) => Ok(Json(FeeEstimatesResponse { fee_estimates })),
        Err(e) => {
            error!("Failed to get estimated fee rates: {}", e);
            Err(ErrorResponse::from(Error::Bitcoin(e)))
        }
    }
}

pub async fn send_to_address(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SendOptions>,
) -> Result<impl IntoResponse, ErrorResponse> {
    debug!("Sending to address: {}", request.address_to);

    match state.bitcoin.send_to_address(request).await {
        Ok(txid) => Ok((
            StatusCode::CREATED,
            Json(SendResponse {
                txid: txid.to_string(),
            }),
        )),
        Err(e) => {
            error!("Failed to send transaction: {}", e);
            Err(ErrorResponse::from(Error::Bitcoin(e)))
        }
    }
}
