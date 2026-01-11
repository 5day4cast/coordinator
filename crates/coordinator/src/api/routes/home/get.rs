use std::sync::Arc;

use axum::{extract::State, response::Html};
use log::info;
use tokio::fs;

use crate::startup::AppState;

//TODO: add pulling down wasm that holds the cryptograph needed for signing the dlc musig
pub async fn index_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(
        public_index(
            &state.remote_url,
            &state.oracle_url,
            &state.ui_dir,
            &state.bitcoin.network.to_string(),
        )
        .await,
    )
}

pub async fn public_index(
    remote_url: &str,
    oracle_url: &str,
    ui_dir: &str,
    bitcoin_network: &str,
) -> String {
    let mut file_content = fs::read_to_string(&format!("{}/index.html", ui_dir))
        .await
        .expect("Unable to read index.html");
    info!("remote_url: {}", remote_url);
    info!("oracle_url: {}", oracle_url);
    info!("bitcoin_network: {}", bitcoin_network);
    file_content = file_content.replace("{SERVER_ADDRESS}", remote_url);
    file_content = file_content.replace("{ORACLE_BASE}", oracle_url);
    file_content.replace("{NETWORK}", bitcoin_network)
}

pub async fn admin_index_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(
        admin_index(
            &state.private_url,
            &state.oracle_url,
            &state.admin_ui_dir,
            &state.esplora_url,
        )
        .await,
    )
}

pub async fn admin_index(
    remote_url: &str,
    oracle_url: &str,
    admin_ui_dir: &str,
    esplora_url: &str,
) -> String {
    let mut file_content = fs::read_to_string(&format!("{}/index.html", admin_ui_dir))
        .await
        .expect("Unable to read index.html");
    info!("remote_url: {}", remote_url);
    info!("oracle_url: {}", oracle_url);
    info!("esplora_url: {}", esplora_url);
    file_content = file_content.replace("{SERVER_ADDRESS}", remote_url);
    file_content = file_content.replace("{ORACLE_BASE}", oracle_url);
    file_content.replace("{ESPLORA_URL}", esplora_url)
}
