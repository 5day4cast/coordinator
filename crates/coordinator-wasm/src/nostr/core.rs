use super::{CustomSigner, NostrError, SignerType};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr_sdk::{
    hashes::{sha256::Hash as Sha256Hash, Hash},
    prelude::*,
    Client, Event, Keys, PublicKey, SecretKey, UnsignedEvent,
};
use std::{collections::HashMap, str::FromStr};

#[derive(Clone)]
pub struct NostrClientCore {
    inner: Option<Client>,
    pub signer: Option<CustomSigner>,
}

impl NostrClientCore {
    pub fn new() -> Self {
        Self {
            inner: None,
            signer: None,
        }
    }

    pub async fn initialize(
        &mut self,
        signer_type: SignerType,
        private_key: Option<String>,
    ) -> Result<(), NostrError> {
        let signer = match signer_type {
            SignerType::PrivateKey => {
                let keys = {
                    if let Some(key) = private_key {
                        Keys::parse(&key)?
                    } else {
                        Keys::generate()
                    }
                };
                CustomSigner::Keys(keys)
            }
            #[cfg(target_arch = "wasm32")]
            SignerType::NIP07 => {
                let browser_signer = Nip07Signer::new()?;
                CustomSigner::BrowserSigner(browser_signer)
            }
        };

        let client = Client::new(signer.clone());

        //TODO: make these relays configurable from the client
        self.add_relay(&client, "wss://relay.damus.io").await?;
        self.add_relay(&client, "wss://relay.nostr.band").await?;
        self.add_relay(&client, "wss://relay.primal.net").await?;

        client.connect().await;
        self.signer = Some(signer);
        self.inner = Some(client);

        Ok(())
    }

    pub async fn add_relay(&mut self, client: &Client, url: &str) -> Result<bool, NostrError> {
        Ok(client.add_relay(url).await?)
    }

    pub fn get_private_key(&self) -> Result<Option<&SecretKey>, NostrError> {
        match &self.signer {
            Some(CustomSigner::Keys(keys)) => Ok(Some(keys.secret_key())),
            #[cfg(target_arch = "wasm32")]
            Some(CustomSigner::BrowserSigner(_)) => Ok(None),
            None => Err(NostrError::NoSigner("No signer initialized".into())),
        }
    }

    pub async fn get_public_key(&self) -> Result<PublicKey, NostrError> {
        match &self.signer {
            Some(signer) => Ok(signer.get_public_key().await?),
            None => Err(NostrError::NoSigner("No signer initialized".into())),
        }
    }

    pub async fn get_relays(&self) -> HashMap<RelayUrl, Relay> {
        if let Some(ref client) = self.inner {
            client.relays().await
        } else {
            HashMap::new()
        }
    }

    pub async fn sign_event(&self, unsigned: UnsignedEvent) -> Result<Event, NostrError> {
        match &self.signer {
            Some(signer) => Ok(signer.sign_event(unsigned).await?),
            None => Err(NostrError::NoSigner("No signer initialized".into())),
        }
    }

    pub async fn nip04_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, NostrError> {
        match &self.signer {
            Some(signer) => Ok(signer.nip04_encrypt(public_key, content).await?),
            None => Err(NostrError::NoSigner("No signer initialized".into())),
        }
    }

    pub async fn nip04_decrypt(
        &self,
        public_key: &PublicKey,
        encrypted_content: &str,
    ) -> Result<String, NostrError> {
        match &self.signer {
            Some(signer) => Ok(signer.nip04_decrypt(public_key, encrypted_content).await?),
            None => Err(NostrError::NoSigner("No signer initialized".into())),
        }
    }

    pub async fn nip44_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, NostrError> {
        match &self.signer {
            Some(signer) => Ok(signer.nip44_encrypt(public_key, content).await?),
            None => Err(NostrError::NoSigner("No signer initialized".into())),
        }
    }

    pub async fn nip44_decrypt(
        &self,
        public_key: &PublicKey,
        encrypted_content: &str,
    ) -> Result<String, NostrError> {
        match &self.signer {
            Some(signer) => Ok(signer.nip44_decrypt(public_key, encrypted_content).await?),
            None => Err(NostrError::NoSigner("No signer initialized".into())),
        }
    }

    pub async fn create_auth_header<T: serde::Serialize>(
        &self,
        method: &str,
        url: &str,
        body: Option<&T>,
    ) -> Result<String, NostrError> {
        let http_method = HttpMethod::from_str(method)
            .map_err(|e| NostrError::NoSigner(format!("Invalid HTTP method: {}", e)))?;
        let http_url =
            Url::from_str(url).map_err(|e| NostrError::NoSigner(format!("Invalid URL: {}", e)))?;

        let mut http_data = HttpData::new(http_url, http_method);

        if let Some(content) = body {
            let content_str = serde_json::to_string(content)
                .map_err(|e| NostrError::NoSigner(format!("Serialization error: {}", e)))?;
            let hash = Sha256Hash::hash(content_str.as_bytes());
            http_data = http_data.payload(hash);
        }

        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| NostrError::NoSigner("No signer initialized".into()))?;

        let event = EventBuilder::http_auth(http_data).sign(signer).await?;

        Ok(format!("Nostr {}", BASE64.encode(event.as_json())))
    }
}
