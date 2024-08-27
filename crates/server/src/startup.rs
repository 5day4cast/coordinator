use std::sync::Arc;

use crate::{
    add_event_entry, create_event, get_entries, index_handler, CompetitionData, Coordinator,
    OracleClient, Settings,
};
use axum::{
    body::Body,
    extract::Request,
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use hyper::{
    header::{ACCEPT, CONTENT_TYPE},
    Method,
};
use log::info;
use reqwest_middleware::{reqwest::Client, ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use tower_http::{
    cors::{Any, CorsLayer},
    services::{ServeDir, ServeFile},
};

#[derive(Clone)]
pub struct AppState {
    pub ui_dir: String,
    pub remote_url: String,
    pub oracle_url: String,
    pub oracle_client: Arc<OracleClient>,
    pub coordinator: Arc<Coordinator>,
}

pub async fn app(config: Settings) -> Result<Router, anyhow::Error> {
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([ACCEPT, CONTENT_TYPE])
        .allow_origin(Any);

    // The ui folder needs to be generated and have this relative path from where the binary is being run
    let serve_dir = ServeDir::new(config.ui_dir.clone())
        .not_found_service(ServeFile::new(config.ui_dir.clone()));
    // Unwrapping here as we don't want to start up if something went wrong setting up the competition data
    let competition_data = CompetitionData::new(&config.competition_db).unwrap();
    let reqwest_client = build_reqwest_client();
    let oracle_client = OracleClient::new(&config.oracle_url, reqwest_client);
    let coordinator = Coordinator::new(
        oracle_client.clone(),
        competition_data,
        &config.private_key_file,
    )
    .await?;
    let mut oracle_url: String = config.oracle_url.to_string();
    // removes the ending "/"
    oracle_url.pop();
    info!("oracle: {}", oracle_url);
    let app_state = AppState {
        ui_dir: config.ui_dir,
        remote_url: config.remote_url,
        oracle_url,
        oracle_client: Arc::new(oracle_client),
        coordinator: Arc::new(coordinator),
    };
    Ok(Router::new()
        .route("/", get(index_handler))
        .route("/competitions", post(create_event))
        .route("/entries", post(add_event_entry))
        .route("/entries", get(get_entries))
        .layer(middleware::from_fn(log_request))
        .with_state(Arc::new(app_state))
        .nest_service("/ui", serve_dir.clone())
        .fallback_service(serve_dir)
        .layer(cors))
}

async fn log_request(request: Request<Body>, next: Next) -> impl IntoResponse {
    let now = time::OffsetDateTime::now_utc();
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_default();
    info!(target: "http_request","new request, {} {}", request.method().as_str(), path);

    let response = next.run(request).await;
    let response_time = time::OffsetDateTime::now_utc() - now;
    info!(target: "http_response", "response, code: {}, time: {}", response.status().as_str(), response_time);

    response
}

pub fn build_reqwest_client() -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    ClientBuilder::new(Client::new())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build()
}
