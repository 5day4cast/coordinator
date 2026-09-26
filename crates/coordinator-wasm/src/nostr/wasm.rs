use super::{core::NostrClientCore, stretch, LoginKeys, NostrError, SignerType};
use ::nostr::{JsonUtil, ToBech32};
use wasm_bindgen::prelude::*;
use zeroize::{Zeroize, Zeroizing};

#[wasm_bindgen]
#[derive(Default)]
pub struct NostrClientWrapper {
    inner: NostrClientCore,
}

#[wasm_bindgen]
impl NostrClientWrapper {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn core(&self) -> &NostrClientCore {
        &self.inner
    }

    #[wasm_bindgen(js_name = "isSignerReady")]
    pub fn is_signer_ready(&self) -> bool {
        self.inner.signer.is_some()
    }

    /// Use the NIP-07 extension, a recovery `nsec`, or (with neither) a new key.
    pub fn initialize(
        &mut self,
        signer_type: SignerType,
        private_key: Option<String>,
    ) -> Result<(), JsValue> {
        Ok(self
            .inner
            .initialize(signer_type, private_key.map(Zeroizing::new))?)
    }

    /// Unlock the password-sealed key returned by username login.
    #[wasm_bindgen(js_name = "unlockWithLogin")]
    pub fn unlock_with_login(
        &mut self,
        login: &LoginCredentials,
        sealed_key: &str,
    ) -> Result<(), JsValue> {
        Ok(self.inner.unlock(&login.inner, sealed_key)?)
    }

    /// Seal the local key for password login.
    #[wasm_bindgen(js_name = "sealForLogin")]
    pub fn seal_for_login(&self, login: &LoginCredentials) -> Result<String, JsValue> {
        Ok(self.inner.seal(&login.inner)?)
    }

    /// The local key as `nsec`, for the one-time recovery-key display.
    #[wasm_bindgen(js_name = "recoveryKey")]
    pub fn recovery_key(&self) -> Result<String, JsValue> {
        Ok(self.inner.nsec()?.to_string())
    }

    #[wasm_bindgen(js_name = "getPublicKey")]
    pub async fn get_public_key(&self) -> Result<String, JsValue> {
        let public_key = self.inner.public_key().await?;
        Ok(public_key
            .to_bech32()
            .unwrap_or_else(|never| match never {}))
    }

    /// NIP-98 header. Pass the exact request body string (or null) that will be sent.
    #[wasm_bindgen(js_name = "getAuthHeader")]
    pub async fn get_auth_header(
        &self,
        url: &str,
        method: &str,
        body: Option<String>,
    ) -> Result<String, JsValue> {
        Ok(self
            .inner
            .auth_header(method, url, body.as_deref().map(str::as_bytes))
            .await?)
    }

    /// Signed event JSON answering a password-reset challenge.
    #[wasm_bindgen(js_name = "signChallenge")]
    pub async fn sign_challenge(&self, challenge: &str) -> Result<String, JsValue> {
        Ok(self.inner.sign_challenge(challenge).await?.as_json())
    }
}

/// Keys derived from a username and password; the vault key never leaves WASM.
#[wasm_bindgen]
pub struct LoginCredentials {
    inner: LoginKeys,
}

#[wasm_bindgen]
impl LoginCredentials {
    /// Slow on purpose (scrypt, ~1-2 s), and blocks the thread it runs on.
    /// Pages stretch the password in a worker and call `fromStretched`.
    pub fn derive(username: &str, password: &str) -> Result<LoginCredentials, JsValue> {
        let inner = LoginKeys::derive(username, password).map_err(NostrError::from)?;
        Ok(Self { inner })
    }

    /// The credentials from `stretchLoginPassword`'s 32 bytes, which are
    /// zeroed in place: the caller's array holds nothing afterwards.
    #[wasm_bindgen(js_name = "fromStretched")]
    pub fn from_stretched(stretched: &mut [u8]) -> Result<LoginCredentials, JsValue> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        let result = if stretched.len() == 32 {
            bytes.copy_from_slice(stretched);
            Ok(Self {
                inner: LoginKeys::from_stretched(&*bytes),
            })
        } else {
            Err(JsValue::from_str("stretched login key must be 32 bytes"))
        };
        stretched.zeroize();
        result
    }

    /// Credential sent to the server in place of the password.
    #[wasm_bindgen(getter, js_name = "authKey")]
    pub fn auth_key(&self) -> String {
        self.inner.auth_key_hex()
    }
}

/// scrypt(password) salted with the username, for `LoginCredentials.fromStretched`.
/// The log-in worker runs this so the page stays responsive; its output
/// unlocks the account as the password does, so zero it once passed on.
#[wasm_bindgen(js_name = "stretchLoginPassword")]
pub fn stretch_login_password(username: &str, password: &str) -> Result<Vec<u8>, JsValue> {
    let stretched = stretch(username, password).map_err(NostrError::from)?;
    Ok(stretched.to_vec())
}
