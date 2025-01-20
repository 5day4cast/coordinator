use super::core::NostrClientCore;
use super::SignerType;
use nostr_sdk::{serde_json, JsonUtil, PublicKey, ToBech32, UnsignedEvent};
use std::str::FromStr;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
#[derive(Clone)]
pub struct NostrClientWrapper {
    #[wasm_bindgen(skip)]
    inner: NostrClientCore,
}

#[wasm_bindgen]
impl NostrClientWrapper {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: NostrClientCore::new(),
        }
    }

    pub(crate) fn get_core(&self) -> &NostrClientCore {
        &self.inner
    }

    #[wasm_bindgen]
    pub async fn initialize(
        &mut self,
        signer_type: SignerType,
        private_key: Option<String>,
    ) -> Result<(), JsValue> {
        self.inner
            .initialize(signer_type, private_key)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "getPrivateKey")]
    pub fn get_private_key(&self) -> Result<Option<String>, JsValue> {
        let maybe_secret_key = self
            .inner
            .get_private_key()
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        match maybe_secret_key {
            Some(secret_key) => secret_key
                .to_bech32()
                .map(Some)
                .map_err(|e| JsValue::from_str(&e.to_string())),
            None => Ok(None),
        }
    }

    #[wasm_bindgen(js_name = "getPublicKey")]
    pub async fn get_public_key(&self) -> Result<String, JsValue> {
        let public_key = self
            .inner
            .get_public_key()
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        public_key
            .to_bech32()
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "getRelays")]
    pub async fn get_relays(&self) -> Result<JsValue, JsValue> {
        let relays = self.inner.get_relays().await;
        let relay_urls: Vec<String> = relays.keys().map(|url| url.to_string()).collect();

        serde_wasm_bindgen::to_value(&relay_urls)
            .map_err(|e| JsValue::from_str(&format!("Serialization error: {}", e)))
    }

    #[wasm_bindgen(js_name = "signEvent")]
    pub async fn sign_event(&self, event_json: &str) -> Result<String, JsValue> {
        let unsigned: UnsignedEvent = serde_json::from_str(event_json)
            .map_err(|e| JsValue::from_str(&format!("Invalid event JSON: {}", e)))?;

        self.inner
            .sign_event(unsigned)
            .await
            .map(|e| e.as_json())
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "getAuthHeader")]
    pub async fn get_auth_header(
        &self,
        url: String,
        method: String,
        body: JsValue,
    ) -> Result<String, JsValue> {
        if url.is_empty() {
            return Err(JsValue::from_str("URL cannot be empty"));
        }
        if method.is_empty() {
            return Err(JsValue::from_str("Method cannot be empty"));
        }

        let body_value: Option<serde_json::Value> = if body.is_null() || body.is_undefined() {
            None
        } else {
            Some(
                serde_wasm_bindgen::from_value(body)
                    .map_err(|e| JsValue::from_str(&format!("Invalid body format: {}", e)))?,
            )
        };

        self.inner
            .create_auth_header(&method, &url, body_value.as_ref())
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(getter)]
    pub fn nip04(&self) -> Nip04Methods {
        Nip04Methods {
            client: self.clone(),
        }
    }

    #[wasm_bindgen(getter)]
    pub fn nip44(&self) -> Nip44Methods {
        Nip44Methods {
            client: self.clone(),
        }
    }
}

#[wasm_bindgen]
pub struct Nip04Methods {
    client: NostrClientWrapper,
}

#[wasm_bindgen]
impl Nip04Methods {
    #[wasm_bindgen]
    pub async fn encrypt(&self, public_key: &str, content: &str) -> Result<String, JsValue> {
        let pk = PublicKey::from_str(public_key)
            .map_err(|e| JsValue::from_str(&format!("Invalid public key: {}", e)))?;

        self.client
            .inner
            .nip04_encrypt(&pk, content)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen]
    pub async fn decrypt(
        &self,
        public_key: &str,
        encrypted_content: &str,
    ) -> Result<String, JsValue> {
        let pk = PublicKey::from_str(public_key)
            .map_err(|e| JsValue::from_str(&format!("Invalid public key: {}", e)))?;

        self.client
            .inner
            .nip04_decrypt(&pk, encrypted_content)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }
}

#[wasm_bindgen]
pub struct Nip44Methods {
    client: NostrClientWrapper,
}

#[wasm_bindgen]
impl Nip44Methods {
    #[wasm_bindgen]
    pub async fn encrypt(&self, public_key: &str, content: &str) -> Result<String, JsValue> {
        let pk = PublicKey::from_str(public_key)
            .map_err(|e| JsValue::from_str(&format!("Invalid public key: {}", e)))?;

        self.client
            .inner
            .nip44_encrypt(&pk, content)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen]
    pub async fn decrypt(
        &self,
        public_key: &str,
        encrypted_content: &str,
    ) -> Result<String, JsValue> {
        let pk = PublicKey::from_str(public_key)
            .map_err(|e| JsValue::from_str(&format!("Invalid public key: {}", e)))?;

        self.client
            .inner
            .nip44_decrypt(&pk, encrypted_content)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }
}
