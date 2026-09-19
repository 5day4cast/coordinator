use super::{DlcWalletCore, WalletError};
use crate::nostr::NostrClientWrapper;
use coordinator_core::RegistrationAssignment;
use dlctix::{
    bitcoin::{Network, OutPoint, Psbt},
    musig2::AggNonce,
    secp::{MaybeScalar, Scalar},
    ContractParameters, EventLockingConditions, SigMap,
};
use serde::{de::DeserializeOwned, Serialize};
use std::str::FromStr;
use uuid::Uuid;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct DlcWallet {
    inner: DlcWalletCore,
}

#[wasm_bindgen]
impl DlcWallet {
    /// Create a wallet with a fresh seed.
    pub fn create(nostr_client: &NostrClientWrapper, network: &str) -> Result<DlcWallet, JsValue> {
        Ok(Self {
            inner: DlcWalletCore::create(nostr_client.core(), parse_network(network)?),
        })
    }

    /// Restore a wallet from its encrypted backup.
    pub async fn load(
        nostr_client: &NostrClientWrapper,
        network: &str,
        encrypted_backup: &str,
    ) -> Result<DlcWallet, JsValue> {
        let inner = DlcWalletCore::load(
            nostr_client.core(),
            encrypted_backup,
            parse_network(network)?,
        )
        .await?;
        Ok(Self { inner })
    }

    /// `{ encrypted_bitcoin_private_key, network }`, encrypted to the user's own Nostr key.
    #[wasm_bindgen(js_name = "encryptedBackup")]
    pub async fn encrypted_backup(&self) -> Result<JsValue, JsValue> {
        to_js(&self.inner.encrypted_backup().await?)
    }

    /// `{ ephemeral_pubkey, payout_hash }` for a new entry.
    #[wasm_bindgen(js_name = "entryRegistration")]
    pub fn entry_registration(&self, entry_id: &str) -> Result<JsValue, JsValue> {
        to_js(&self.inner.entry_registration(parse_entry_id(entry_id)?)?)
    }

    /// `{ encrypted_private_key, auth_pubkey, context }` for the ticket's keymeld slot.
    /// `assignment_json` is the ticket response's `keymeld_registration`.
    #[wasm_bindgen(js_name = "keymeldRegistration")]
    pub async fn keymeld_registration(
        &self,
        entry_id: &str,
        assignment_json: &str,
    ) -> Result<JsValue, JsValue> {
        let assignment: RegistrationAssignment = serde_json::from_str(assignment_json)
            .map_err(|e| JsValue::from_str(&format!("Invalid keymeld assignment: {e}")))?;
        let prepared = self
            .inner
            .keymeld_registration(parse_entry_id(entry_id)?, &assignment)
            .await?;
        // JSON-compatible so byte arrays in the context survive the round trip.
        prepared
            .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// `{ ephemeral_private_key, payout_preimage }` for an entry this wallet created.
    #[wasm_bindgen(js_name = "payoutRelease")]
    pub fn payout_release(
        &self,
        entry_id: &str,
        expected_pubkey: &str,
    ) -> Result<JsValue, JsValue> {
        to_js(
            &self
                .inner
                .payout_release(parse_entry_id(entry_id)?, expected_pubkey)?,
        )
    }

    #[wasm_bindgen(js_name = "addContract")]
    pub fn add_contract(
        &mut self,
        entry_id: &str,
        params: JsValue,
        funding_outpoint: JsValue,
    ) -> Result<(), JsValue> {
        let params: ContractParameters = from_js(params, "contract parameters")?;
        let funding_outpoint: OutPoint = from_js(funding_outpoint, "funding outpoint")?;
        Ok(self
            .inner
            .add_contract(parse_entry_id(entry_id)?, params, funding_outpoint)?)
    }

    #[wasm_bindgen(js_name = "generatePublicNonces")]
    pub fn generate_public_nonces(&mut self, entry_id: &str) -> Result<JsValue, JsValue> {
        to_js(
            &self
                .inner
                .generate_public_nonces(parse_entry_id(entry_id)?)?,
        )
    }

    #[wasm_bindgen(js_name = "signAggregateNonces")]
    pub fn sign_aggregate_nonces(
        &mut self,
        entry_id: &str,
        aggregate_nonces: JsValue,
    ) -> Result<JsValue, JsValue> {
        let aggregate_nonces: SigMap<AggNonce> = from_js(aggregate_nonces, "aggregate nonces")?;
        to_js(
            &self
                .inner
                .sign_aggregate_nonces(aggregate_nonces, parse_entry_id(entry_id)?)?,
        )
    }

    #[wasm_bindgen(js_name = "signFundingPsbt")]
    pub fn sign_funding_psbt(&self, entry_id: &str, psbt_base64: &str) -> Result<String, JsValue> {
        let psbt = Psbt::from_str(psbt_base64)
            .map_err(|e| JsValue::from_str(&format!("Invalid PSBT: {e}")))?;
        Ok(self
            .inner
            .sign_funding_psbt(psbt, parse_entry_id(entry_id)?)?
            .to_string())
    }

    /// `{ source: "pinned", pcrs }` or `{ source: "coordinator" }`: where this
    /// build's keymeld enclave trust comes from, for display to the user.
    #[wasm_bindgen(js_name = "keymeldTrust")]
    pub fn keymeld_trust() -> Result<JsValue, JsValue> {
        to_js(&super::enclave_trust()?)
    }

    /// The outcome key (as used in `outcome_payouts`) selected by an attestation.
    #[wasm_bindgen(js_name = "currentOutcome")]
    pub fn current_outcome(
        attestation_hex: &str,
        event_announcement: JsValue,
    ) -> Result<String, JsValue> {
        let attestation = Scalar::from_hex(attestation_hex)
            .map_err(|e| JsValue::from_str(&format!("Invalid attestation: {e}")))?;
        let event: EventLockingConditions = from_js(event_announcement, "event announcement")?;
        Ok(DlcWalletCore::current_outcome(MaybeScalar::Valid(attestation), &event)?.to_string())
    }
}

fn parse_network(network: &str) -> Result<Network, WalletError> {
    Network::from_str(network).map_err(|_| WalletError::Network(network.to_owned()))
}

fn parse_entry_id(entry_id: &str) -> Result<Uuid, WalletError> {
    Uuid::parse_str(entry_id).map_err(|_| WalletError::InvalidEntryId(entry_id.to_owned()))
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(value).map_err(|e| JsValue::from_str(&e.to_string()))
}

fn from_js<T: DeserializeOwned>(value: JsValue, what: &str) -> Result<T, JsValue> {
    serde_wasm_bindgen::from_value(value)
        .map_err(|e| JsValue::from_str(&format!("Invalid {what}: {e}")))
}
