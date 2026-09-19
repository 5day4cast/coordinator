use ::nostr::{
    signer::{SignerBackend, SignerError},
    util::BoxedFuture,
    Event, Keys, PublicKey, UnsignedEvent,
};
use std::fmt;

#[cfg(target_arch = "wasm32")]
use nostr_browser_signer::BrowserSigner;
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
    BrowserSigner(BrowserSigner),
}

impl fmt::Debug for CustomSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CustomSigner::Keys(keys) => f.debug_tuple("Keys").field(keys).finish(),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => {
                f.debug_tuple("BrowserSigner").field(signer).finish()
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

impl ::nostr::NostrSigner for CustomSigner {
    fn backend(&self) -> SignerBackend<'_> {
        match self {
            CustomSigner::Keys(_) => SignerBackend::Keys,
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(_) => SignerBackend::BrowserExtension,
        }
    }

    fn get_public_key(&self) -> BoxedFuture<'_, Result<PublicKey, SignerError>> {
        match self {
            CustomSigner::Keys(keys) => keys.get_public_key(),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => Box::pin(async move {
                let public_key = signer.get_public_key().await?;
                // PublicKey parsing is byte-only in nostr 0.44; preserve the
                // previous curve validation at the extension boundary.
                public_key.xonly().map_err(SignerError::backend)?;
                Ok(public_key)
            }),
        }
    }

    fn sign_event(&self, unsigned: UnsignedEvent) -> BoxedFuture<'_, Result<Event, SignerError>> {
        match self {
            CustomSigner::Keys(keys) => keys.sign_event(unsigned),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.sign_event(unsigned),
        }
    }

    fn nip44_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        match self {
            CustomSigner::Keys(keys) => keys.nip44_encrypt(public_key, content),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.nip44_encrypt(public_key, content),
        }
    }

    fn nip44_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        match self {
            CustomSigner::Keys(keys) => keys.nip44_decrypt(public_key, content),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.nip44_decrypt(public_key, content),
        }
    }

    fn nip04_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        match self {
            CustomSigner::Keys(keys) => keys.nip04_encrypt(public_key, content),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => signer.nip04_encrypt(public_key, content),
        }
    }

    fn nip04_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        encrypted_content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        match self {
            CustomSigner::Keys(keys) => keys.nip04_decrypt(public_key, encrypted_content),
            #[cfg(target_arch = "wasm32")]
            CustomSigner::BrowserSigner(signer) => {
                signer.nip04_decrypt(public_key, encrypted_content)
            }
        }
    }
}
