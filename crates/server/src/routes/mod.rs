mod coordinator;
mod home;
mod system;

use axum::{
    response::{IntoResponse, Response},
    Json,
};
use hyper::StatusCode;
use serde_json::json;
use std::borrow::Borrow;

use crate::domain::Error;

pub use coordinator::*;
pub use home::*;
pub use system::*;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, error_message) = match self.borrow() {
            Error::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::InvalidSignature(_) => (StatusCode::FORBIDDEN, self.to_string()),
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
