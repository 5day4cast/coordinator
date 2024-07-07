use std::sync::Arc;

use crate::index_handler;
use axum::{routing::get, Router};
use hyper::Method;
use tower_http::{
    cors::{Any, CorsLayer},
    services::{ServeDir, ServeFile},
};

#[derive(Clone)]
pub struct AppState {
    pub ui_dir: String,
    pub remote_url: String,
    pub oracle_url: String,
}

pub fn app(remote_url: String, oracle_url: String, ui_dir: String) -> Router {
    let cors = CorsLayer::new()
        // allow `GET` and `POST` when accessing the resource
        .allow_methods([Method::GET, Method::POST])
        // allow requests from any origin
        .allow_origin(Any);

    // The ui folder needs to be generated and have this relative path from where the binary is being run
    let serve_dir = ServeDir::new(ui_dir.clone()).not_found_service(ServeFile::new(ui_dir.clone()));
    let app_state = AppState {
        ui_dir,
        remote_url,
        oracle_url
    };
    Router::new()
        .route("/", get(index_handler))
        .with_state(Arc::new(app_state))
        .nest_service("/ui", serve_dir.clone())
        .fallback_service(serve_dir)
        .layer(cors)
}
