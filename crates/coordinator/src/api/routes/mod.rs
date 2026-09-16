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
            Error::PaymentFailed(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Error::InvalidSignature(_) => (StatusCode::FORBIDDEN, self.to_string()),
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
