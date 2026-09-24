use axum::http::StatusCode;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use bitcoin::PublicKey;
use dlctix::{
    musig2::{AggNonce, PartialSignature, PubNonce},
    SigMap,
};
use log::{debug, error};
use nostr::ToBech32;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    api::{
        extractors::{AuthedJson, NostrAuth},
        routes::ApiError,
    },
    domain::{
        AddEntry, Competition, CreateEvent, Error as DomainError, FundedContract, PayoutClaimInfo,
        PayoutClaimReceipt, PayoutInfo, SearchBy, TicketRefund, TicketResponse, TicketStatus,
        UserEntry,
    },
    infra::lnurl::LightningAddress,
    startup::AppState,
    templates::qr,
};

// Private route not exposed publically so NostrAuth is not needed
pub async fn create_competition(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateEvent>,
) -> Result<Json<Competition>, ApiError> {
    let competition = state
        .coordinator
        .create_competition(body)
        .await
        .map_err(|e| {
            error!("error creating competition: {:?}", e);
            ApiError::from(e)
        })?;
    // Its entry form reads forecasts from the cache; fill it before anyone opens it.
    state.leaderboards.warm(&competition);
    Ok(Json(competition))
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
    #[serde(default)]
    pub payout: Option<coordinator_core::PayoutRegistrationRequest>,
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
/// A ticket with its invoice's QR code, which the entry form shows as an
/// image instead of loading a QR script.
#[derive(Debug, Serialize)]
pub struct TicketWithQr {
    #[serde(flatten)]
    pub ticket: TicketResponse,
    /// `data:image/svg+xml` URL of the invoice's QR code.
    pub payment_request_qr: Option<String>,
}

pub async fn request_competition_ticket(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: request,
    }: AuthedJson<TicketRequest>,
) -> Result<Json<TicketWithQr>, ApiError> {
    let btc_pubkey = PublicKey::from_str(&request.btc_pubkey).map_err(|e| {
        error!("Invalid Bitcoin public key: {:?}", e);
        ApiError::Status(StatusCode::BAD_REQUEST)
    })?;

    let ticket = state
        .coordinator
        .request_ticket_with_payout(pubkey.to_hex(), competition_id, btc_pubkey, request.payout)
        .await
        .map_err(|e| {
            error!("error requesting ticket: {:?}", e);
            ApiError::from(e)
        })?;
    let payment_request_qr = qr::lightning_invoice_svg(&ticket.payment_request)
        .map(|svg| qr::svg_data_url(&svg));
    Ok(Json(TicketWithQr {
        ticket,
        payment_request_qr,
    }))
}

pub async fn get_ticket_status(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, ticket_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<TicketStatus>, ApiError> {
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

/// Where a player's refund has got to, when their competition never kicked off.
pub async fn get_ticket_refund(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, ticket_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Option<TicketRefund>>, ApiError> {
    state
        .coordinator
        .get_ticket_refund(pubkey.to_hex(), competition_id, ticket_id)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error getting ticket refund: {:?}", e);
            e.into()
        })
}

/* Two steps
1) submit entry with ticket_id for the hold invoice
2) pay the hold invoice (server watching invoice state to become accepted)
3) server marks ticket as paid -> include in competition
*/
pub async fn add_event_entry(
    State(state): State<Arc<AppState>>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<AddEntry>,
) -> Result<Json<UserEntry>, ApiError> {
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
) -> Result<Json<Vec<UserEntry>>, ApiError> {
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
) -> Result<Json<Vec<Competition>>, ApiError> {
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
) -> Result<Json<Competition>, ApiError> {
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
) -> Result<Json<FundedContract>, ApiError> {
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
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: public_nonces,
    }: AuthedJson<SigMap<PubNonce>>,
) -> Result<StatusCode, ApiError> {
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
) -> Result<Json<SigMap<AggNonce>>, ApiError> {
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
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: final_signatures,
    }: AuthedJson<FinalSignatures>,
) -> Result<StatusCode, ApiError> {
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
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: payout_info,
    }: AuthedJson<PayoutInfo>,
) -> Result<StatusCode, ApiError> {
    let pubkey = pubkey.to_hex();
    debug!("submitted payout by: {} for entry {}", pubkey, entry_id);

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

/// One-click payout to the Lightning Address on the account.
pub async fn claim_ticket_payout(
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body: claim,
    }: AuthedJson<PayoutClaimInfo>,
) -> Result<Json<PayoutClaimReceipt>, ApiError> {
    let npub = pubkey.to_bech32().expect("public bech32 format");
    let user = state.users_info.login(npub).await?;
    let address = user
        .lightning_address
        .as_deref()
        .map(LightningAddress::parse)
        .transpose()
        .map_err(|e| ApiError::from(DomainError::BadRequest(e.to_string())))?
        .ok_or_else(|| {
            ApiError::from(DomainError::BadRequest(
                "Add a Lightning Address on the payouts page first".into(),
            ))
        })?;
    let pubkey = pubkey.to_hex();
    debug!("payout claim by: {} for entry {}", pubkey, entry_id);

    state
        .coordinator
        .claim_ticket_payout(pubkey, competition_id, entry_id, claim, &address)
        .await
        .map(Json)
        .map_err(|e| {
            error!("error claiming payout: {:?}", e);
            e.into()
        })
}

pub async fn get_payout_authorization(
    NostrAuth { pubkey, .. }: NostrAuth,
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<crate::domain::PayoutAuthorizationInfo>, ApiError> {
    state
        .coordinator
        .payout_authorization_info(&pubkey.to_hex(), competition_id, entry_id)
        .await
        .map(Json)
        .map_err(Into::into)
}

pub async fn submit_invoice_fallback(
    State(state): State<Arc<AppState>>,
    Path((competition_id, entry_id)): Path<(Uuid, Uuid)>,
    AuthedJson {
        auth: NostrAuth { pubkey, .. },
        body,
    }: AuthedJson<crate::domain::InvoiceFallbackRequest>,
) -> Result<Json<Uuid>, ApiError> {
    state
        .coordinator
        .submit_invoice_fallback(&pubkey.to_hex(), competition_id, entry_id, body)
        .await
        .map(Json)
        .map_err(Into::into)
}

pub async fn get_payout_terms(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<Uuid>,
) -> Result<Json<crate::domain::PayoutTermsQuote>, ApiError> {
    state
        .coordinator
        .payout_terms_quote(competition_id)
        .await
        .map(Json)
        .map_err(Into::into)
}
