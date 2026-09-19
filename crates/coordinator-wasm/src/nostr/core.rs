use super::{login::LoginKeys, CustomSigner, NostrError, SignerType};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr_sdk::{
    hashes::{sha256::Hash as Sha256Hash, Hash},
    prelude::*,
    Event, EventBuilder, Keys, Kind, PublicKey, SecretKey,
};
use std::str::FromStr;
use zeroize::Zeroizing;

/// The user's Nostr identity: signs NIP-98 HTTP auth and encrypts wallet
/// backups to itself. It makes no relay connections.
#[derive(Clone, Default)]
pub struct NostrClientCore {
    pub signer: Option<CustomSigner>,
}

impl NostrClientCore {
    pub fn initialize(
        &mut self,
        signer_type: SignerType,
        private_key: Option<Zeroizing<String>>,
    ) -> Result<(), NostrError> {
        self.signer = Some(match signer_type {
            SignerType::PrivateKey => CustomSigner::Keys(match private_key {
                Some(key) => Keys::parse(key.as_str())?,
                None => Keys::generate(),
            }),
            #[cfg(target_arch = "wasm32")]
            SignerType::NIP07 => CustomSigner::BrowserSigner(Nip07Signer::new()?),
        });
        Ok(())
    }

    /// Unlock a password-sealed key (see [`LoginKeys`]).
    pub fn unlock(&mut self, login: &LoginKeys, sealed_key: &str) -> Result<(), NostrError> {
        self.signer = Some(CustomSigner::Keys(login.open(sealed_key)?));
        Ok(())
    }

    /// Seal the local key for password login. Extension signers have no local key.
    pub fn seal(&self, login: &LoginKeys) -> Result<String, NostrError> {
        Ok(login.seal(self.local_secret_key()?)?)
    }

    fn local_secret_key(&self) -> Result<&SecretKey, NostrError> {
        match &self.signer {
            Some(CustomSigner::Keys(keys)) => Ok(keys.secret_key()),
            #[cfg(target_arch = "wasm32")]
            Some(CustomSigner::BrowserSigner(_)) => Err(NostrError::NoLocalKey),
            None => Err(NostrError::NoSigner),
        }
    }

    /// The local key as `nsec`, shown once at registration as the recovery key.
    pub fn nsec(&self) -> Result<Zeroizing<String>, NostrError> {
        Ok(Zeroizing::new(self.local_secret_key()?.to_bech32()?))
    }

    fn signer(&self) -> Result<&CustomSigner, NostrError> {
        self.signer.as_ref().ok_or(NostrError::NoSigner)
    }

    pub async fn public_key(&self) -> Result<PublicKey, NostrError> {
        Ok(self.signer()?.get_public_key().await?)
    }

    /// NIP-98 `Authorization` header. `body` must be the exact bytes sent, so
    /// the server can check the payload hash.
    pub async fn auth_header(
        &self,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
    ) -> Result<String, NostrError> {
        let method = HttpMethod::from_str(&method.to_uppercase())
            .map_err(|_| NostrError::InvalidRequest("HTTP method"))?;
        let url = Url::from_str(url).map_err(|_| NostrError::InvalidRequest("URL"))?;

        let mut http_data = HttpData::new(url, method);
        if let Some(body) = body {
            http_data = http_data.payload(Sha256Hash::hash(body));
        }
        let event = EventBuilder::http_auth(http_data)
            .sign(self.signer()?)
            .await?;
        Ok(format!("Nostr {}", BASE64.encode(event.as_json())))
    }

    /// Sign a password-reset challenge, proving control of the account key.
    pub async fn sign_challenge(&self, challenge: &str) -> Result<Event, NostrError> {
        Ok(EventBuilder::new(Kind::HttpAuth, challenge)
            .sign(self.signer()?)
            .await?)
    }
}
