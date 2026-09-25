mod common;
pub mod escrow_refund;
pub mod full_lifecycle;
pub mod types;

pub use escrow_refund::run_escrow_refund;
pub use full_lifecycle::run_full_lifecycle;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::CoordinatorClient;
    use axum::{extract::State, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};

    async fn created(
        State(posted): State<Arc<Mutex<Vec<serde_json::Value>>>>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let id = body["id"].clone();
        posted.lock().unwrap().push(body);
        Json(serde_json::json!({
            "id": id, "created_at": "2026-09-25T00:00:00Z", "event_submission": {}
        }))
    }

    /// Every competition a scenario makes asks the oracle to keep it off its public list.
    #[tokio::test]
    async fn scenario_competitions_are_unlisted() {
        let posted = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/api/v1/competitions", post(created))
            .with_state(posted.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = CoordinatorClient::new(&url, None);
        let config = ScenarioConfig::default();

        full_lifecycle::create_competition(&client, &config)
            .await
            .unwrap();
        escrow_refund::create_competition(&client, &config)
            .await
            .unwrap();
        server.abort();
        let posted = posted.lock().unwrap();
        assert_eq!(posted.len(), 2);
        assert!(posted
            .iter()
            .all(|body| body["unlisted"] == serde_json::json!(true)));
    }
}
