use axum::{extract::State, response::ErrorResponse};
use hyper::StatusCode;
use log::{debug, error};
use std::sync::Arc;

use crate::{domain::Error, AppState};

pub async fn health(State(state): State<Arc<AppState>>) -> Result<StatusCode, ErrorResponse> {
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
    Ok(StatusCode::OK)
}
