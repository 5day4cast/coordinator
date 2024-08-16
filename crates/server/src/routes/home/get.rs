use std::sync::Arc;

use axum::{extract::State, response::Html};
use tokio::fs;

use crate::AppState;

//TODO: add pulling down wasm that holds the cryptograph needed for signing transactions
pub async fn index_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(index(&state.remote_url, &state.oracle_url, &state.ui_dir).await)
}

pub async fn index(remote_url: &str, oracle_url: &str, ui_dir: &str) -> String {
    let mut file_content = fs::read_to_string(&format!("{}/index.html", ui_dir))
        .await
        .expect("Unable to read index.html");

    file_content = file_content.replace("{SERVER_ADDRESS}", remote_url);
    file_content.replace("{ORACLE_BASE}", oracle_url)
}
