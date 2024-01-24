use std::sync::Arc;

use axum::{extract::State, response::Html};
use tokio::fs;

use crate::AppState;

pub async fn index_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(index(&state.remote_url, &state.ui_dir).await)
}

pub async fn index(remote_url: &str, ui_dir: &str) -> String {
    let file_content = fs::read_to_string(&format!("{}/index.html", ui_dir))
        .await
        .expect("Unable to read index.html");

    file_content.replace("{SERVER_ADDRESS}", remote_url)
}
