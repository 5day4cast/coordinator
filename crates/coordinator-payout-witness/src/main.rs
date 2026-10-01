//! Run on a trusted, independently administered host behind a fixed HTTPS origin.
use anyhow::{bail, Context, Result};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use coordinator_escrow::payout_witness::{
    Authenticated, AuthenticationKey, Receipt, Request, WitnessError, MAX_REQUEST_BYTES,
};
use coordinator_payout_witness::{initialize, initialize_empty, Ledger};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::Semaphore;

struct Service {
    ledger: Ledger,
    key: AuthenticationKey,
    admission: Semaphore,
    payment_hashes: AtomicU64,
    released_entries: AtomicU64,
    capacity_failures: AtomicU64,
    unavailable: AtomicU64,
}

async fn reserve(
    State(service): State<Arc<Service>>,
    Json(request): Json<Authenticated<Request>>,
) -> Response {
    let Ok(_permit) = service.admission.try_acquire() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    if service.key.verify_request(&request).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let result = service
        .ledger
        .reserve(request.payload.ledger_id, &request.payload.reservation)
        .await;
    match &result {
        Ok(occupancy) => {
            service
                .payment_hashes
                .fetch_max(occupancy.payment_hashes, Ordering::Relaxed);
            service
                .released_entries
                .fetch_max(occupancy.released_entries, Ordering::Relaxed);
        }
        Err(WitnessError::Capacity) => {
            service.capacity_failures.fetch_add(1, Ordering::Relaxed);
        }
        Err(WitnessError::Unavailable) => {
            service.unavailable.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    match service.key.sign_receipt(Receipt {
        request: request.payload,
        result,
    }) {
        Ok(receipt) => Json(receipt).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn metrics(State(service): State<Arc<Service>>) -> String {
    format!(
        "# TYPE payout_witness_payment_hashes gauge\npayout_witness_payment_hashes {}\n\
         # TYPE payout_witness_released_entries gauge\npayout_witness_released_entries {}\n\
         # TYPE payout_witness_capacity_failures_total counter\npayout_witness_capacity_failures_total {}\n\
         # TYPE payout_witness_unavailable_total counter\npayout_witness_unavailable_total {}\n",
        service.payment_hashes.load(Ordering::Relaxed),
        service.released_entries.load(Ordering::Relaxed),
        service.capacity_failures.load(Ordering::Relaxed),
        service.unavailable.load(Ordering::Relaxed),
    )
}

fn read_key_file(path: &str) -> Result<AuthenticationKey> {
    let bytes = zeroize::Zeroizing::new(std::fs::read_to_string(path)?);
    AuthenticationKey::from_hex(&bytes)
}

async fn serve(database: &Path) -> Result<()> {
    let ledger = Ledger::open(database).await?;
    let occupancy = ledger.occupancy().await?;
    let key_path = std::env::var("PAYOUT_WITNESS_KEY_FILE")?;
    let key = read_key_file(&key_path)?;
    let state = Arc::new(Service {
        ledger,
        key,
        admission: Semaphore::new(32),
        payment_hashes: AtomicU64::new(occupancy.payment_hashes),
        released_entries: AtomicU64::new(occupancy.released_entries),
        capacity_failures: AtomicU64::new(0),
        unavailable: AtomicU64::new(0),
    });
    let router = Router::new()
        .route("/v1/reservations", post(reserve))
        .route("/metrics", get(metrics))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state.clone());
    let address =
        std::env::var("PAYOUT_WITNESS_LISTEN").unwrap_or_else(|_| "127.0.0.1:8182".into());
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    let service =
        Arc::try_unwrap(state).map_err(|_| anyhow::anyhow!("Witness handlers still running"))?;
    service.ledger.close().await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [operation, database, inventory] if operation == "initialize-from" => {
            let occupancy = initialize(Path::new(database), Path::new(inventory)).await
                .context("Witness initialization failed; do not serve an incomplete inventory")?;
            println!("Imported {} payment hashes and {} released entries", occupancy.payment_hashes, occupancy.released_entries);
            Ok(())
        }
        [operation, database, acknowledgment]
            if operation == "initialize-empty" && acknowledgment == "--acknowledge-fresh-epoch" =>
        {
            let ledger_id = initialize_empty(Path::new(database), true)
                .await
                .context("Fresh-epoch initialization failed; no ledger identity may be reused")?;
            println!("PAYOUT_WITNESS_LEDGER_ID={ledger_id}");
            Ok(())
        }
        [operation, database] if operation == "serve" => serve(Path::new(database)).await,
        _ => bail!("Usage: coordinator-payout-witness initialize-from DATABASE COMPLETE_INVENTORY | initialize-empty DATABASE --acknowledge-fresh-epoch | serve DATABASE"),
    }
}
