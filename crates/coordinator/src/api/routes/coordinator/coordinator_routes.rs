use axum::http::StatusCode;
use axum::{
    extract::{Path, Query, State},
    response::ErrorResponse,
    Json,
};
use bdk_wallet::bitcoin::PublicKey;
use dlctix::{
    musig2::{AggNonce, PartialSignature, PubNonce},
    SigMap,
};
use log::{debug, error};
use serde::Deserialize;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    api::extractors::NostrAuth,
    domain::{
        AddEntry, Competition, CreateEvent, FundedContract, KeymeldSessionInfo, PayoutInfo,
        SearchBy, TicketResponse, TicketStatus, UserEntry,
    },
    startup::AppState,
};

// Private route not exposed publically so NostrAuth is not needed
pub async fn create_competition(
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
pub async fn request_competition_ticket(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    Json(request): Json<TicketRequest>,
) -> Result<Json<TicketResponse>, ErrorResponse> {
    let btc_pubkey = PublicKey::from_str(&request.btc_pubkey).map_err(|e| {
        error!("Invalid Bitcoin public key: {:?}", e);
        ErrorResponse::from(StatusCode::BAD_REQUEST)
    })?;

    state
        .coordinator
        .request_ticket(pubkey.to_hex(), competition_id, btc_pubkey)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error requesting ticket: {:?}", e);
            e.into()
        })
}

pub async fn get_ticket_status(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, ticket_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<TicketStatus>, ErrorResponse> {
    state
        .coordinator
        .get_ticket_status(pubkey.to_hex(), competition_id, ticket_id)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error getting ticket status: {:?}", e);
            e.into()
        })
}

/* Two steps
1) submit entry with ticket_id for the hold invoice
2) pay the hold invoice (server watching invoice state to become accepted)
3) server marks ticket as paid -> include in competition
*/
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
            if !comp.is_funding_broadcasted() {
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

    if !competition.is_funding_broadcasted() {
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

#[derive(Debug, Clone, Deserialize)]
pub struct FinalSignatures {
    pub funding_psbt_base64: String,
    pub partial_signatures: SigMap<PartialSignature>,
}

pub async fn submit_final_signatures(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    Json(final_signatures): Json<FinalSignatures>,
) -> Result<StatusCode, ErrorResponse> {
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
            error!("error submitting partial signatures: {:?}", e);
            e.into()
        })
}

pub async fn submit_ticket_payout(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    Json(payout_info): Json<PayoutInfo>,
) -> Result<StatusCode, ErrorResponse> {
    let pubkey = pubkey.to_hex();
    debug!("submitted payout by: {} {:?}", pubkey, payout_info);

    state
        .coordinator
        .submit_ticket_payout(pubkey, competition_id, entry_id, payout_info)
        .await
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            error!("error submitting payout information: {:?}", e);
            e.into()
        })
}

/// Get Keymeld session info for a competition
///
/// Returns information needed for clients to join a Keymeld MuSig2 signing session.
/// This includes:
/// - Whether Keymeld is enabled on this coordinator
/// - The gateway URL to connect to
/// - The session ID (if a keygen session is active)
/// - Encrypted session secret (NIP-44) for the requesting user
pub async fn get_keymeld_session(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<KeymeldSessionInfo>, ErrorResponse> {
    let pubkey_hex = pubkey.to_hex();

    state
        .coordinator
        .get_keymeld_session_info(competition_id, &pubkey_hex)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error getting keymeld session info: {:?}", e);
            e.into()
        })
}
