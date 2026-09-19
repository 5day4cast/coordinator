use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr::{
    hashes::{sha256::Hash as Sha256Hash, Hash},
    prelude::*,
};
use std::str::FromStr;

/// Create a NIP-98 HTTP Auth header for authenticated coordinator API requests.
/// `body` must be the exact bytes sent; the coordinator checks its hash.
pub async fn create_auth_header(
    keys: &Keys,
    method: &str,
    url: &str,
    body: Option<&[u8]>,
) -> Result<String> {
    let http_method = HttpMethod::from_str(&method.to_uppercase())
        .map_err(|e| anyhow::anyhow!("Invalid HTTP method: {}", e))?;
    let http_url = Url::from_str(url).map_err(|e| anyhow::anyhow!("Invalid URL: {}", e))?;

    let mut http_data = HttpData::new(http_url, http_method);

    if let Some(body) = body {
        http_data = http_data.payload(Sha256Hash::hash(body));
    }

    // A new signature alone does not change the event ID used by replay guards.
    let event = EventBuilder::http_auth(http_data)
        .tag(Tag::custom(
            TagKind::Custom("request-id".into()),
            [hex::encode(rand::random::<[u8; 16]>())],
        ))
        .sign_with_keys(keys)?;

    Ok(format!("Nostr {}", BASE64.encode(event.as_json())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repeated_requests_have_unique_event_ids() {
        let keys = Keys::generate();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..16 {
            let header = create_auth_header(&keys, "GET", "http://localhost/entries", None)
                .await
                .unwrap();
            let bytes = BASE64
                .decode(header.strip_prefix("Nostr ").unwrap())
                .unwrap();
            let event: Event = serde_json::from_slice(&bytes).unwrap();
            event.verify().unwrap();
            assert!(ids.insert(event.id));
        }
    }
}
