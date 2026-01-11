use async_trait::async_trait;
use nostr_sdk::{
    nips::nip04,
    signer::{SignerBackend, SignerError},
    Event, Keys, PublicKey, UnsignedEvent,
};
use std::fmt;

#[cfg(target_arch = "wasm32")]
use nostr_sdk::nips::nip07::Nip07Signer;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
#[derive(Clone, Debug)]
pub enum SignerType {
    PrivateKey,
    #[cfg(target_arch = "wasm32")]
    NIP07,
}

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
impl nostr_sdk::NostrSigner for CustomSigner {
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
        content: &str,
    ) -> Result<String, SignerError> {
        match self {
            CustomSigner::Keys(keys) => {
                use nostr_sdk::nips::nip44;
                nip44::decrypt(keys.secret_key(), public_key, content).map_err(SignerError::backend)
            }
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.nip44_decrypt(public_key, content).await,
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
