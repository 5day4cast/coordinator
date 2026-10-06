mod coordinator;
mod home;
mod pages;
mod system;

use axum::{
    response::{IntoResponse, Response},
    Json,
};
use hyper::StatusCode;
use serde_json::json;
use std::borrow::Borrow;

use crate::{api::extractors::AuthError, domain::Error, infra::db::DatabaseWriteError};

pub use coordinator::*;
pub use pages::*;
pub use system::*;

/// Preserve typed failures until Axum builds the HTTP response.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Domain(#[from] Error),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("{0}")]
    Status(StatusCode),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Domain(error) => error.into_response(),
            Self::Auth(error) => error.into_response(),
            Self::Status(status) => status.into_response(),
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, error_message) = match self.borrow() {
            Error::NoAvailableTickets => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::CompetitionFull => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::Conflict(_) => (StatusCode::CONFLICT, self.to_string()),
            Error::PaymentFailed(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Error::InvalidSignature(_) => (StatusCode::FORBIDDEN, self.to_string()),
            Error::FeeEstimateUnavailable => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            Error::EntriesPaused => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            Error::ArkadeUnavailable => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            Error::SwapsUnavailable => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            Error::SettleOnly => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            // Retryable: the oracle is restarting or has not fitted its lines yet. Its message
            // may name internal addresses, so it is logged rather than returned.
            Error::OracleFailed(error) => {
                log::warn!("Oracle request failed: {error}");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    String::from("the oracle is unavailable right now; try again in a moment"),
                )
            }
            Error::DatabaseWrite(error) => {
                log::error!("Database write failed: {error}");
                match error {
                    DatabaseWriteError::QueueFull | DatabaseWriteError::ChannelClosed => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        String::from("database write was not accepted"),
                    ),
                    DatabaseWriteError::ResultChannelClosed => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        String::from(
                            "database write outcome is unknown; do not retry automatically",
                        ),
                    ),
                    DatabaseWriteError::Sqlx(_) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        String::from("internal server error"),
                    ),
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn client_errors_keep_their_message_and_internal_ones_do_not() {
        for (error, status, message) in [
            (
                Error::NotFound("entry 1".into()),
                StatusCode::NOT_FOUND,
                "item not found: entry 1",
            ),
            (
                Error::BadRequest("bad".into()),
                StatusCode::BAD_REQUEST,
                "bad",
            ),
            (
                Error::Conflict("taken".into()),
                StatusCode::CONFLICT,
                "taken",
            ),
            (
                Error::InvalidSignature("nope".into()),
                StatusCode::FORBIDDEN,
                "invalid signature for request",
            ),
            (
                Error::PaymentFailed("routing".into()),
                StatusCode::BAD_REQUEST,
                "Payout payment failed: routing",
            ),
            (
                Error::EntriesPaused,
                StatusCode::SERVICE_UNAVAILABLE,
                "Entries are paused while Bitcoin network fees are high",
            ),
            (
                Error::ArkadeUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "Entries are paused while the Arkade network recovers; try again in a little while",
            ),
            (
                Error::SettleOnly,
                StatusCode::SERVICE_UNAVAILABLE,
                "Entries are paused",
            ),
            (
                Error::FeeEstimateUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "The Bitcoin network fee estimate is unavailable right now, so no ticket was issued; try again in a moment",
            ),
            (
                Error::SwapsUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "Lightning payments are unavailable for a moment, so no invoice was made; try again in a moment",
            ),
            (
                Error::OracleFailed(crate::infra::oracle::Error::BadRequest(
                    "no line has been fitted yet".into(),
                )),
                StatusCode::SERVICE_UNAVAILABLE,
                "the oracle is unavailable right now; try again in a moment",
            ),
            (
                Error::Bitcoin(anyhow::anyhow!("private node detail")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error",
            ),
            (
                Error::Thread("worker".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error",
            ),
        ] {
            let response = error.into_response();
            assert_eq!(response.status(), status);
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body, json!({ "error": message }));
        }
    }

    #[tokio::test]
    async fn write_failures_distinguish_rejection_from_unknown_outcome() {
        for (error, status, message) in [
            (
                DatabaseWriteError::QueueFull,
                StatusCode::SERVICE_UNAVAILABLE,
                "database write was not accepted",
            ),
            (
                DatabaseWriteError::ChannelClosed,
                StatusCode::SERVICE_UNAVAILABLE,
                "database write was not accepted",
            ),
            (
                DatabaseWriteError::ResultChannelClosed,
                StatusCode::INTERNAL_SERVER_ERROR,
                "database write outcome is unknown; do not retry automatically",
            ),
            (
                DatabaseWriteError::Sqlx(sqlx::Error::Protocol("private database detail".into())),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error",
            ),
        ] {
            let response = Error::DatabaseWrite(error).into_response();
            assert_eq!(response.status(), status);
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body, json!({ "error": message }));
        }
    }
}
