use super::core::{TaprootWalletCore, TaprootWalletCoreBuilder};
use crate::NostrClientWrapper;
use dlctix::{bitcoin::OutPoint, musig2::AggNonce, ContractParameters, SigMap};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct TaprootWallet {
    inner: TaprootWalletCore,
}
#[wasm_bindgen]
#[derive(Clone)]
pub struct TaprootWalletBuilder {
    inner: TaprootWalletCoreBuilder,
}

#[wasm_bindgen]
impl TaprootWalletBuilder {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: TaprootWalletCoreBuilder::new(),
        }
    }

    #[wasm_bindgen]
    pub fn network(self, network: String) -> TaprootWalletBuilder {
        self.inner.clone().network(network);
        self
    }

    #[wasm_bindgen]
    #[wasm_bindgen]
    pub fn nostr_client(self, client: &NostrClientWrapper) -> TaprootWalletBuilder {
        let mut builder = TaprootWalletBuilder::new();
        builder.inner = self.inner.nostr_client(client.get_core());
        builder
    }

    #[wasm_bindgen]
    pub fn encrypted_key(self, key: String) -> TaprootWalletBuilder {
        self.inner.clone().encrypted_key(key);
        self
    }

    #[wasm_bindgen]
    pub async fn build(self) -> Result<TaprootWallet, JsValue> {
        let core = self
            .inner
            .build()
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(TaprootWallet { inner: core })
    }
}

#[wasm_bindgen]
impl TaprootWallet {
    #[wasm_bindgen(js_name = "getPublicData")]
    pub fn get_public_data(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&self.inner.get_public_data())
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "getEncryptedMasterKey")]
    pub async fn get_encrypted_master_key(&self, nostr_pubkey: &str) -> Result<JsValue, JsValue> {
        let encrypted = self
            .inner
            .get_encrypted_master_key(nostr_pubkey)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&encrypted).map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "decryptKey")]
    pub async fn decrypt_key(
        &self,
        encrypted_key: &str,
        nostr_pubkey: &str,
    ) -> Result<String, JsValue> {
        self.inner
            .decrypt_key(encrypted_key, nostr_pubkey)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "encryptDlcKey")]
    pub async fn encrypt_dlc_key(
        &self,
        entry_index: u32,
        nostr_pubkey: &str,
    ) -> Result<String, JsValue> {
        self.inner
            .encrypt_dlc_key(entry_index, nostr_pubkey)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "getDlcAddress")]
    pub fn get_dlc_address(&mut self, entry_index: u32) -> Result<String, JsValue> {
        self.inner
            .get_dlc_address(entry_index)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "addEntryIndex")]
    pub fn add_entry_index(&mut self, entry_index: u32) -> Result<String, JsValue> {
        self.inner
            .add_entry_index(entry_index)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "addContract")]
    pub fn add_contract(
        &mut self,
        entry_index: u32,
        params: JsValue,
        funding_outpoint: JsValue,
    ) -> Result<(), JsValue> {
        let params: ContractParameters = serde_wasm_bindgen::from_value(params)
            .map_err(|e| JsValue::from_str(&format!("Params deserialization error: {}", e)))?;

        let funding_outpoint: OutPoint = serde_wasm_bindgen::from_value(funding_outpoint)
            .map_err(|e| JsValue::from_str(&format!("Outpoint deserialization error: {}", e)))?;

        self.inner
            .add_contract(entry_index, params, funding_outpoint)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "generatePublicNonces")]
    pub fn generate_public_nonces(&self, entry_index: u32) -> Result<JsValue, JsValue> {
        let nonces = self
            .inner
            .generate_public_nonces(entry_index)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&nonces)
            .map_err(|e| JsValue::from_str(&format!("Nonce serialization error: {}", e)))
    }

    #[wasm_bindgen(js_name = "signAggregateNonces")]
    pub fn sign_aggregate_nonces(
        &self,
        aggregate_nonces: JsValue,
        entry_index: u32,
    ) -> Result<JsValue, JsValue> {
        let agg_nonces: SigMap<AggNonce> = serde_wasm_bindgen::from_value(aggregate_nonces)
            .map_err(|e| {
                JsValue::from_str(&format!("Aggregate nonces deserialization error: {}", e))
            })?;

        let partial_sigs = self
            .inner
            .sign_aggregate_nonces(agg_nonces, entry_index)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        serde_wasm_bindgen::to_value(&partial_sigs).map_err(|e| {
            JsValue::from_str(&format!("Partial signatures serialization error: {}", e))
        })
    }
}
