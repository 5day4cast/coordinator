//! Reading events from Nostr relays: one `REQ` per relay, every stored event up to `EOSE`.

use std::time::Duration;

use futures::future::join_all;
use nostr::Event;
use serde_json::{json, Value};

use super::ws::WebSocket;

/// Relays tried when neither the recovery file nor `--relays` names any.
pub const DEFAULT_RELAYS: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://relay.nostr.band",
];

const RELAY_TIMEOUT: Duration = Duration::from_secs(20);

/// Every event matching `filters` on any of `relays`, without duplicates, and the relays that
/// failed. Signatures are not checked here; the caller checks what it uses.
pub async fn fetch(relays: &[String], filters: &[Value]) -> (Vec<Event>, Vec<String>) {
    let results = join_all(relays.iter().map(|relay| async move {
        let fetched = tokio::time::timeout(RELAY_TIMEOUT, fetch_one(relay, filters)).await;
        match fetched {
            Ok(result) => result.map_err(|e| format!("{relay}: {e}")),
            Err(_) => Err(format!("{relay}: timed out")),
        }
    }))
    .await;
    let mut events: Vec<Event> = Vec::new();
    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok(found) => {
                for event in found {
                    if !events.iter().any(|known| known.id == event.id) {
                        events.push(event);
                    }
                }
            }
            Err(e) => failures.push(e),
        }
    }
    (events, failures)
}

async fn fetch_one(relay: &str, filters: &[Value]) -> Result<Vec<Event>, String> {
    let mut socket = WebSocket::connect(relay).await?;
    let subscription = format!("recover-{}", hex::encode(rand::random::<[u8; 6]>()));
    let mut request = vec![json!("REQ"), json!(subscription)];
    request.extend(filters.iter().cloned());
    socket.send_text(&Value::Array(request).to_string()).await?;
    let mut events = Vec::new();
    while let Some(text) = socket.next_text().await? {
        let Ok(Value::Array(message)) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match message.as_slice() {
            [kind, id, event] if kind == "EVENT" && *id == subscription => {
                if let Ok(event) = serde_json::from_value::<Event>(event.clone()) {
                    events.push(event);
                }
            }
            [kind, id, ..] if kind == "EOSE" && *id == subscription => break,
            [kind, id, reason, ..] if kind == "CLOSED" && *id == subscription => {
                return Err(format!("closed the subscription: {reason}"));
            }
            _ => {}
        }
    }
    let _ = socket
        .send_text(&json!(["CLOSE", subscription]).to_string())
        .await;
    socket.close().await;
    Ok(events)
}
