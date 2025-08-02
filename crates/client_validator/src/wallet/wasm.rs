use super::core::{TaprootWalletCore, TaprootWalletCoreBuilder};
use crate::NostrClientWrapper;
use bdk_wallet::bitcoin::Psbt;
use dlctix::{
    bitcoin::OutPoint,
    musig2::AggNonce,
    secp::{MaybeScalar, Scalar},
    ContractParameters, EventLockingConditions, SigMap,
};
use log::{debug, info};
use std::str::FromStr;
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
    pub fn network(mut self, network: String) -> TaprootWalletBuilder {
        self.inner = self.inner.network(network);
        self
    }

    #[wasm_bindgen]
    pub fn nostr_client(mut self, client: &NostrClientWrapper) -> TaprootWalletBuilder {
        self.inner = self.inner.nostr_client(client.get_core());
        self
    }

    #[wasm_bindgen]
    pub fn encrypted_key(mut self, key: String) -> TaprootWalletBuilder {
        self.inner = self.inner.encrypted_key(key);
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

    #[wasm_bindgen(js_name = "getEncryptedDlcPrivateKey")]
    pub async fn get_encrypted_dlc_private_key(
        &self,
        entry_index: u32,
        nostr_pubkey: &str,
    ) -> Result<String, JsValue> {
        self.inner
            .get_encrypted_dlc_private_key(entry_index, nostr_pubkey)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "getEncryptedDlcPayoutPreimage")]
    pub async fn get_encrypted_dlc_payout_preimage(
        &self,
        entry_index: u32,
        nostr_pubkey: &str,
    ) -> Result<String, JsValue> {
        self.inner
            .get_encrypted_dlc_payout_preimage(entry_index, nostr_pubkey)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "getDlcPublicKey")]
    pub async fn get_dlc_public_key(&self, entry_index: u32) -> Result<String, JsValue> {
        self.inner
            .get_dlc_public_key(entry_index)
            .await
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
        info!("params: {:?}", params);
        let params: ContractParameters = serde_wasm_bindgen::from_value(params)
            .map_err(|e| JsValue::from_str(&format!("Params deserialization error: {}", e)))?;

        let funding_outpoint: OutPoint = serde_wasm_bindgen::from_value(funding_outpoint)
            .map_err(|e| JsValue::from_str(&format!("Outpoint deserialization error: {}", e)))?;

        self.inner
            .add_contract(entry_index, params, funding_outpoint)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    #[wasm_bindgen(js_name = "generatePublicNonces")]
    pub fn generate_public_nonces(&mut self, entry_index: u32) -> Result<JsValue, JsValue> {
        let nonces = self
            .inner
            .generate_public_nonces(entry_index)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        debug!("Generated nonces: {:?}", nonces);

        if nonces.by_outcome.is_empty() && nonces.by_win_condition.is_empty() {
            return Err(JsValue::from_str("No nonces generated"));
        }

        serde_wasm_bindgen::to_value(&nonces)
            .map_err(|e| JsValue::from_str(&format!("Nonce serialization error: {}", e)))
    }

    #[wasm_bindgen(js_name = "signAggregateNonces")]
    pub fn sign_aggregate_nonces(
        &self,
        aggregate_nonces: JsValue,
        entry_index: u32,
    ) -> Result<JsValue, JsValue> {
        debug!("Received aggregate nonces JsValue: {:?}", aggregate_nonces);

        let agg_nonces: SigMap<AggNonce> = serde_wasm_bindgen::from_value(aggregate_nonces)
            .map_err(|e| {
                JsValue::from_str(&format!("Aggregate nonces deserialization error: {}", e))
            })?;

        debug!("Deserialized aggregate nonces: {:?}", agg_nonces);

        let partial_sigs = self
            .inner
            .sign_aggregate_nonces(agg_nonces, entry_index)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        debug!("Generated partial signatures: {:?}", partial_sigs);

        serde_wasm_bindgen::to_value(&partial_sigs).map_err(|e| {
            JsValue::from_str(&format!("Partial signatures serialization error: {}", e))
        })
    }

    #[wasm_bindgen(js_name = "signFundingPsbt")]
    pub fn sign_funding_psbt(
        &self,
        funding_psbt_base64: String,
        entry_index: u32,
    ) -> Result<String, JsValue> {
        debug!("Received funding psbt: {:?}", funding_psbt_base64);

        let psbt = Psbt::from_str(&funding_psbt_base64)
            .map_err(|e| JsValue::from_str(&format!("Invalid PSBT base64: {}", e)))?;

        debug!("Deserialized funding psbt: {:?}", psbt);

        let signed_psbt = self
            .inner
            .sign_funding_psbt(psbt, entry_index)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        debug!("Generated signed funding psbt: {:?}", signed_psbt);
        let psbt_base64 = signed_psbt.to_string();
        debug!("Generated signed funding psbt base64: {:?}", psbt_base64);

        Ok(psbt_base64)
    }

    #[wasm_bindgen(js_name = "getCurrentOutcome")]
    pub fn get_current_outcome(
        &self,
        attestation_hex: &str,
        event_announcement: JsValue,
    ) -> Result<String, JsValue> {
        let attestation = Scalar::from_hex(attestation_hex)
            .map_err(|e| JsValue::from_str(&format!("Invalid attestation hex: {}", e)))?;
        let maybe_attestation = MaybeScalar::Valid(attestation);

        let event_announcement: EventLockingConditions =
            serde_wasm_bindgen::from_value(event_announcement).map_err(|e| {
                JsValue::from_str(&format!("Event announcement deserialization error: {}", e))
            })?;

        let outcome = self
            .inner
            .get_current_outcome(maybe_attestation, event_announcement)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;

        Ok(outcome.to_string())
    }
}
