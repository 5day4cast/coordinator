use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr_sdk::{
    hashes::{sha256::Hash as Sha256Hash, Hash},
    prelude::*,
};
use std::str::FromStr;

/// Create a NIP-98 HTTP Auth header for authenticated coordinator API requests.
pub async fn create_auth_header<T: serde::Serialize>(
    keys: &Keys,
    method: &str,
    url: &str,
    body: Option<&T>,
) -> Result<String> {
    let http_method = HttpMethod::from_str(&method.to_uppercase())
        .map_err(|e| anyhow::anyhow!("Invalid HTTP method: {}", e))?;
    let http_url =
        Url::from_str(url).map_err(|e| anyhow::anyhow!("Invalid URL: {}", e))?;

    let mut http_data = HttpData::new(http_url, http_method);

    if let Some(content) = body {
        let content_str = serde_json::to_string(content)?;
        let hash = Sha256Hash::hash(content_str.as_bytes());
        http_data = http_data.payload(hash);
    }

    let event = EventBuilder::http_auth(http_data).sign_with_keys(keys)?;

    Ok(format!("Nostr {}", BASE64.encode(event.as_json())))
}
