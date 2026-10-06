//! `/recover`: the recovery page. It needs nothing from the coordinator but this shell and the
//! WASM module, so the same page works hosted anywhere once the coordinator is gone.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{Html, IntoResponse, Response},
};

use crate::{
    startup::AppState,
    templates::pages::{recover_page, RecoverConfig},
};

pub async fn recover_page_handler(State(state): State<Arc<AppState>>) -> Response {
    let network = state.bitcoin.get_network().to_string();
    let page = recover_page(&RecoverConfig {
        network: &network,
        oracle_base: &state.oracle_url,
        wasm_version: &state.wasm_version,
    });
    let mut response = Html(page.into_string()).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}
