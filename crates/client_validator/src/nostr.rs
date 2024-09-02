use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use log::error;
use log::info;
use nostr_sdk::prelude::*;
#[cfg(target_arch = "wasm32")]
use nostr_sdk::serde_json;
#[cfg(target_arch = "wasm32")]
use nostr_sdk::JsonUtil;
use nostr_sdk::{
    hashes::{sha256::Hash as Sha256Hash, Hash},
    nips::nip04,
    signer::SignerBackend,
    Client, Event, Keys, NostrSigner, PublicKey, SignerError, UnsignedEvent,
};
use std::collections::HashMap;
use std::fmt::{self};
use std::str::FromStr;

#[cfg(target_arch = "wasm32")]
use nostr_sdk::nips::nip07::Nip07Signer;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
use web_sys::{Request, RequestInit, RequestMode};

pub enum CustomSigner {
    Keys(Keys),
    #[cfg(target_arch = "wasm32")]
    BrowserSigner(Nip07Signer),
}

impl fmt::Debug for CustomSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CustomSigner::Keys(keys) => f.debug_tuple("Keys").field(keys).finish(),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => {
                f.debug_tuple("Nip07Signer").field(signer).finish()
            }
        }
    }
}

impl Clone for CustomSigner {
    fn clone(&self) -> Self {
        match self {
            CustomSigner::Keys(keys) => CustomSigner::Keys(keys.clone()),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => CustomSigner::BrowserSigner(signer.clone()),
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl NostrSigner for CustomSigner {
    fn backend(&self) -> SignerBackend {
        match self {
            CustomSigner::Keys(_) => SignerBackend::Keys,
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(_) => SignerBackend::BrowserExtension,
        }
    }

    async fn get_public_key(&self) -> Result<PublicKey, SignerError> {
        match self {
            CustomSigner::Keys(keys) => Ok(keys.public_key()),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.get_public_key().await,
        }
    }

    async fn sign_event(&self, unsigned: UnsignedEvent) -> Result<Event, SignerError> {
        match self {
            CustomSigner::Keys(keys) => unsigned.sign_with_keys(keys).map_err(SignerError::backend),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.sign_event(unsigned).await,
        }
    }

    async fn nip44_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, SignerError> {
        match self {
            CustomSigner::Keys(keys) => {
                use nostr_sdk::nips::nip44::{self, Version};
                nip44::encrypt(keys.secret_key(), public_key, content, Version::default())
                    .map_err(SignerError::backend)
            }
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.nip44_encrypt(public_key, content).await,
        }
    }

    async fn nip44_decrypt(
        &self,
        public_key: &PublicKey,
        payload: &str,
    ) -> Result<String, SignerError> {
        match self {
            CustomSigner::Keys(keys) => {
                use nostr_sdk::nips::nip44;
                nip44::decrypt(keys.secret_key(), public_key, payload).map_err(SignerError::backend)
            }
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.nip44_decrypt(public_key, payload).await,
        }
    }

    async fn nip04_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, SignerError> {
        match self {
            CustomSigner::Keys(keys) => {
                nip04::encrypt(keys.secret_key(), public_key, content).map_err(SignerError::backend)
            }
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.nip04_encrypt(public_key, content).await,
        }
    }

    async fn nip04_decrypt(
        &self,
        public_key: &PublicKey,
        encrypted_content: &str,
    ) -> Result<String, SignerError> {
        match self {
            CustomSigner::Keys(keys) => {
                nip04::decrypt(keys.secret_key(), public_key, encrypted_content)
                    .map_err(SignerError::backend)
            }
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => {
                signer.nip04_decrypt(public_key, encrypted_content).await
            }
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub enum SignerType {
    PrivateKey,
    #[cfg(target_arch = "wasm32")]
    NIP07,
}

impl fmt::Debug for SignerType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignerType::PrivateKey => f.debug_tuple("PrivateKey").finish(),
            #[cfg(target_arch = "wasm32")]
            SignerType::NIP07 => f.debug_tuple("NIP07").finish(),
        }
    }
}

impl Clone for SignerType {
    fn clone(&self) -> Self {
        match self {
            SignerType::PrivateKey => SignerType::PrivateKey,
            #[cfg(target_arch = "wasm32")]
            SignerType::NIP07 => SignerType::NIP07,
        }
    }
}

#[derive(Debug)]

pub struct NoSignerError(String);

impl fmt::Display for NoSignerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NoSignerError {}

#[derive(Debug)]
pub enum InitializationError {
    NoPrivateKey(String),
    KeyParsing(nostr_sdk::key::Error),
    RelayConnection(nostr_sdk::client::Error),
    #[cfg(target_arch = "wasm32")]
    BrowserSigner(nostr_sdk::nips::nip07::Error),
}

impl fmt::Display for InitializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPrivateKey(msg) => write!(f, "{}", msg),
            Self::KeyParsing(e) => write!(f, "Key parsing error: {}", e),
            Self::RelayConnection(e) => write!(f, "Relay connection error: {}", e),
            #[cfg(target_arch = "wasm32")]
            Self::BrowserSigner(e) => write!(f, "Browser signer error: {}", e),
        }
    }
}

impl std::error::Error for InitializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::KeyParsing(e) => Some(e),
            Self::RelayConnection(e) => Some(e),
            #[cfg(target_arch = "wasm32")]
            Self::BrowserSigner(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl From<InitializationError> for wasm_bindgen::JsValue {
    fn from(error: InitializationError) -> Self {
        wasm_bindgen::JsValue::from_str(&error.to_string())
    }
}
#[derive(Clone)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct NostrClientWrapper {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(skip))]
    inner: Option<Client>,
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(skip))]
    pub signer: Option<CustomSigner>,
}

impl NostrClientWrapper {
    async fn initialize_internal(
        &mut self,
        signer_type: SignerType,
        private_key: Option<String>,
    ) -> Result<(), InitializationError> {
        let signer = match signer_type {
            SignerType::PrivateKey => {
                let keys = {
                    if let Some(key) = private_key {
                        Keys::parse(&key).map_err(InitializationError::KeyParsing)?
                    } else {
                        Keys::generate()
                    }
                };
                CustomSigner::Keys(keys)
            }
            #[cfg(target_arch = "wasm32")]
            SignerType::NIP07 => {
                let browser_signer =
                    Nip07Signer::new().map_err(InitializationError::BrowserSigner)?;
                CustomSigner::BrowserSigner(browser_signer)
            }
        };

        let client = Client::new(signer.clone());

        //TODO: make these relay configurable by the user
        self.add_relay(&client, "wss://relay.damus.io").await?;
        self.add_relay(&client, "wss://relay.nostr.band").await?;
        self.add_relay(&client, "wss://relay.primal.net").await?;

        client.connect().await;
        self.signer = Some(signer);
        self.inner = Some(client);

        Ok(())
    }

    async fn add_relay(&mut self, client: &Client, url: &str) -> Result<bool, InitializationError> {
        client
            .add_relay(url)
            .await
            .map_err(InitializationError::RelayConnection)
    }

    fn get_private_key_internal(&self) -> Result<Option<&SecretKey>, NoSignerError> {
        match &self.signer {
            Some(CustomSigner::Keys(keys)) => Ok(Some(keys.secret_key())),
            #[cfg(target_arch = "wasm32")]
            Some(CustomSigner::BrowserSigner(_)) => Ok(None),
            None => Err(NoSignerError(String::from("No signer initialized"))),
        }
    }

    async fn get_public_key_internal(&self) -> Result<PublicKey, SignerError> {
        match &self.signer {
            Some(signer) => signer.get_public_key().await,
            None => Err(SignerError::backend(NoSignerError(String::from(
                "No signer initialized",
            )))),
        }
    }

    async fn get_relays_internal(&self) -> HashMap<RelayUrl, Relay> {
        if let Some(ref client) = self.inner {
            client.relays().await
        } else {
            HashMap::new()
        }
    }

    async fn sign_event_internal(&self, unsigned: UnsignedEvent) -> Result<Event, SignerError> {
        match &self.signer {
            Some(signer) => signer.sign_event(unsigned).await,
            None => Err(SignerError::backend(NoSignerError(String::from(
                "No signer initialized",
            )))),
        }
    }

    async fn nip04_encrypt_internal(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, SignerError> {
        match &self.signer {
            Some(signer) => signer.nip04_encrypt(public_key, content).await,
            None => Err(SignerError::backend(NoSignerError(String::from(
                "No signer initialized",
            )))),
        }
    }

    async fn nip04_decrypt_internal(
        &self,
        public_key: &PublicKey,
        encrypted_content: &str,
    ) -> Result<String, SignerError> {
        match &self.signer {
            Some(signer) => signer.nip04_decrypt(public_key, encrypted_content).await,
            None => Err(SignerError::backend(NoSignerError(String::from(
                "No signer initialized",
            )))),
        }
    }

    async fn nip44_encrypt_internal(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, SignerError> {
        match &self.signer {
            Some(signer) => signer.nip44_encrypt(public_key, content).await,
            None => Err(SignerError::backend(NoSignerError(String::from(
                "No signer initialized",
            )))),
        }
    }

    async fn nip44_decrypt_internal(
        &self,
        public_key: &PublicKey,
        encrypted_content: &str,
    ) -> Result<String, SignerError> {
        match &self.signer {
            Some(signer) => signer.nip44_decrypt(public_key, encrypted_content).await,
            None => Err(SignerError::backend(NoSignerError(String::from(
                "No signer initialized",
            )))),
        }
    }

    async fn create_auth_header_internal(
        &self,
        method: &str,
        url: &str,
        body_string: Option<String>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        info!("in auth header");
        let http_method = HttpMethod::from_str(method)?;
        let http_url = Url::from_str(url)?;

        let mut http_data = HttpData::new(http_url, http_method);

        if let Some(content) = body_string {
            let hash = Sha256Hash::hash(content.as_bytes());
            http_data = http_data.payload(hash);
        }

        let event = EventBuilder::http_auth(http_data)
            .sign(self.signer.as_ref().ok_or("No signer initialized")?)
            .await?;

        Ok(format!("Nostr {}", BASE64.encode(event.as_json())))
    }

    pub fn new() -> Self {
        Self {
            inner: None,
            signer: None,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn get_relays(&self) -> HashMap<RelayUrl, Relay> {
        self.get_relays_internal().await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn create_auth_header<T: serde::Serialize>(
        &self,
        method: &str,
        url: &str,
        body: Option<&T>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let body_string = match body {
            Some(body) => Some(serde_json::to_string(body)?),
            None => None,
        };

        self.create_auth_header_internal(method, url, body_string)
            .await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn initialize(
        &mut self,
        signer_type: SignerType,
        private_key: Option<String>,
    ) -> Result<(), InitializationError> {
        self.initialize_internal(signer_type, private_key).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn get_public_key(&self) -> Result<PublicKey, SignerError> {
        self.get_public_key_internal().await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn get_private_key(&self) -> Result<Option<&SecretKey>, NoSignerError> {
        self.get_private_key_internal()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn sign_event(&self, unsigned: UnsignedEvent) -> Result<Event, SignerError> {
        self.sign_event_internal(unsigned).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn nip04_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, SignerError> {
        self.nip04_encrypt_internal(public_key, content).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn nip04_decrypt(
        &self,
        public_key: &PublicKey,
        encrypted_content: &str,
    ) -> Result<String, SignerError> {
        self.nip04_decrypt_internal(public_key, encrypted_content)
            .await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn nip44_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, SignerError> {
        self.nip44_encrypt_internal(public_key, content).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn nip44_decrypt(
        &self,
        public_key: &PublicKey,
        encrypted_content: &str,
    ) -> Result<String, SignerError> {
        self.nip44_decrypt_internal(public_key, encrypted_content)
            .await
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl NostrClientWrapper {
    #[wasm_bindgen(constructor)]
    pub fn new_wasm() -> Self {
        Self::new()
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

    #[wasm_bindgen(js_name = "getPrivateKey")]
    /// Only works when user is using their raw key and not an extension
    pub fn get_private_key(&self) -> Result<Option<String>, JsValue> {
        let Some(private_key) = self
            .get_private_key_internal()
            .map_err(|e| JsValue::from_str(&e.to_string()))?
        else {
            return Ok(None);
        };

        private_key
            .to_bech32()
            .map(Some)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "getPublicKey")]
    pub async fn get_public_key(&self) -> Result<String, JsValue> {
        let public_key = self
            .get_public_key_internal()
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        public_key
            .to_bech32()
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "getRelays")]
    pub async fn get_relays(&self) -> Result<JsValue, JsValue> {
        let relays = self.get_relays_internal().await;
        let relay_urls: Vec<RelayUrl> = relays.keys().cloned().collect();
        serde_wasm_bindgen::to_value(&relay_urls)
            .map_err(|e| JsValue::from_str(&format!("Serialization error: {}", e)))
    }

    #[wasm_bindgen]
    pub async fn initialize(
        &mut self,
        signer_type: SignerType,
        private_key: Option<String>,
    ) -> Result<(), JsValue> {
        self.initialize_internal(signer_type, private_key)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "signEvent")]
    pub async fn sign_event(&self, event_json: &str) -> Result<String, JsValue> {
        let unsigned: UnsignedEvent = serde_json::from_str(event_json)
            .map_err(|e| JsValue::from_str(&format!("Invalid event JSON: {}", e)))?;

        self.sign_event_internal(unsigned)
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
        let body_string: Option<String> = if body.is_null() || body.is_undefined() {
            None
        } else {
            match serde_wasm_bindgen::from_value::<serde_json::Value>(body) {
                Ok(serde_value) => serde_json::to_string(&serde_value).ok(),
                Err(e) => {
                    error!("Failed to convert body into string: {}", e);
                    None
                }
            }
        };

        self.create_auth_header_internal(&method, &url, body_string)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }
}
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub struct Nip04Methods {
    client: NostrClientWrapper,
}
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl Nip04Methods {
    #[wasm_bindgen]
    pub async fn encrypt(&self, public_key: &str, content: &str) -> Result<String, JsValue> {
        let pk = PublicKey::from_str(public_key)
            .map_err(|e| JsValue::from_str(&format!("Invalid public key: {}", e)))?;

        self.client
            .nip04_encrypt_internal(&pk, content)
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
            .nip04_decrypt_internal(&pk, encrypted_content)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub struct Nip44Methods {
    client: NostrClientWrapper,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl Nip44Methods {
    #[wasm_bindgen]
    pub async fn encrypt(&self, public_key: &str, content: &str) -> Result<String, JsValue> {
        let pk = PublicKey::from_str(public_key)
            .map_err(|e| JsValue::from_str(&format!("Invalid public key: {}", e)))?;

        self.client
            .nip44_encrypt_internal(&pk, content)
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
            .nip44_decrypt_internal(&pk, encrypted_content)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }
}
