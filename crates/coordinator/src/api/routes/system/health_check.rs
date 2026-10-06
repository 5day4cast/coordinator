use axum::{extract::State, Json};
use log::{debug, error};
use serde::Serialize;
use std::sync::Arc;

use crate::{api::routes::ApiError, domain::Error, startup::AppState};

/// What `/api/v1/health_check` reports once the service, its threads and the database are up.
#[derive(Debug, Serialize)]
pub struct Health {
    pub status: &'static str,
    /// The coordinator settles what it owes and takes no new money.
    pub settle_only: bool,
}

pub async fn health(State(state): State<Arc<AppState>>) -> Result<Json<Health>, ApiError> {
    // Ping the database
    state.coordinator.ping().await.map_err(|e| {
        error!("{}", e);
        e
    })?;

    // Verify the background threads are still running
    for (thread_name, thread) in state.background_threads.clone().iter() {
        if thread.is_finished() {
            let err = Error::Thread(format!(
                "thread {} has died, we need to restart the service",
                thread_name
            ));
            error!("{}", err);
            return Err(err.into());
        }
    }

    debug!("service, background threads, and db are up");
    Ok(Json(Health {
        status: "ok",
        settle_only: state.coordinator.settle_only(),
    }))
}
