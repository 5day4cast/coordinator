use bdk_wallet::{
    bitcoin::{
        bip32::{ChainCode, ChildNumber, DerivationPath, Xpriv},
        secp256k1::{Keypair, Secp256k1, SecretKey, XOnlyPublicKey},
        taproot::TapTweakHash,
        Address, Network, NetworkKind as BDKNetworkKind,
    },
    chain::Merge,
    descriptor::calc_checksum,
    ChangeSet, KeychainKind, Wallet,
};
use blake2::{Blake2b512, Digest};
use dlctix::{
    bitcoin::OutPoint, musig2::AggNonce, secp::Scalar, ContractParameters, SigMap, SigningSession,
    TicketedDLC,
};
use nostr_sdk::{FromBech32, NostrSigner};
use rand::{rngs::StdRng, thread_rng, RngCore, SeedableRng};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{collections::HashMap, str::FromStr};
use wasm_bindgen::prelude::*;

use crate::NostrClientWrapper;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum NetworkKind {
    Main,
    Test,
}

impl From<BDKNetworkKind> for NetworkKind {
    fn from(n: BDKNetworkKind) -> Self {
        match n {
            BDKNetworkKind::Main => NetworkKind::Main,
            BDKNetworkKind::Test => NetworkKind::Test,
        }
    }
}

impl From<Network> for NetworkKind {
    fn from(n: Network) -> Self {
        match n {
            Network::Bitcoin => NetworkKind::Main,
            Network::Testnet | Network::Testnet4 | Network::Signet | Network::Regtest => {
                NetworkKind::Test
            }
            _ => NetworkKind::Test,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct KeyPair {
    pub descriptor: String,
    pub change_descriptor: String,
    pub network: NetworkKind,
}

#[derive(Serialize, Deserialize)]
pub struct EncryptedKey {
    pub encrypted_bitcoin_private_key: String,
    pub network: String,
}

#[wasm_bindgen]
#[derive(Clone)]
pub struct TaprootWalletBuilder {
    network: Option<String>,
    nostr_client: Option<NostrClientWrapper>,
    encrypted_key: Option<String>,
}

#[wasm_bindgen]
impl TaprootWalletBuilder {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            network: None,
            nostr_client: None,
            encrypted_key: None,
        }
    }

    #[wasm_bindgen]
    pub fn network(mut self, network: String) -> TaprootWalletBuilder {
        self.network = Some(network);
        self
    }

    #[wasm_bindgen]
    #[cfg(target_arch = "wasm32")]
    pub fn nostr_client(mut self, client: &NostrClientWrapper) -> TaprootWalletBuilder {
        self.nostr_client = Some(client.clone());
        self
    }

    #[wasm_bindgen]
    pub fn encrypted_key(mut self, key: String) -> TaprootWalletBuilder {
        self.encrypted_key = Some(key);
        self
    }

    #[wasm_bindgen]
    pub async fn build(self) -> Result<TaprootWallet, JsValue> {
        let network = self
            .network
            .ok_or_else(|| JsValue::from_str("Network is required"))?;
        let nostr_client = self
            .nostr_client
            .ok_or_else(|| JsValue::from_str("NostrClient is required"))?;

        let network = Network::from_str(&network).map_err(|e| JsValue::from_str(&e.to_string()))?;

        let wallet = match self.encrypted_key {
            Some(encrypted) => {
                let mut wallet =
                    TaprootWallet::load_from_encrypted(&nostr_client, &encrypted, network).await?;

                if wallet.public_data.network != NetworkKind::from(network) {
                    return Err(JsValue::from_str("Network mismatch"));
                }

                wallet.nostr_client = nostr_client;
                wallet
            }
            None => {
                let mut wallet = TaprootWallet::create_new(network)?;
                wallet.nostr_client = nostr_client;
                wallet
            }
        };

        Ok(wallet)
    }
}

#[wasm_bindgen]
pub struct TaprootWallet {
    extended_key: SecretString,
    public_data: KeyPair,
    nostr_client: NostrClientWrapper,
    bdk_wallet: Wallet,
    network: Network,
    #[wasm_bindgen(skip)]
    change_set: ChangeSet,
    #[wasm_bindgen(skip)]
    dlc_contracts: HashMap<u32, DlcEntry>,
}

pub struct DlcEntry {
    pub child_key: SecretString,
    pub payout_preimage: SecretString,
    pub contract: Option<TicketedDLC>,
    pub funding_outpoint: Option<OutPoint>,
    pub params: Option<ContractParameters>,
    pub ticket_preimage: Option<SecretString>,
}

#[wasm_bindgen]
impl TaprootWallet {
    fn create_new(network: Network) -> Result<Self, JsValue> {
        let mut entropy = [0u8; 32];
        thread_rng().fill_bytes(&mut entropy);
        let secret_key = SecretKey::from_slice(&entropy)
            .map_err(|e| JsValue::from_str(&format!("Invalid secret key: {}", e)))?;

        let mut hasher = Blake2b512::new();
        hasher.update(&secret_key[..]);
        let hash = hasher.finalize();
        let mut chain_code = [0u8; 32];
        chain_code.copy_from_slice(&hash[0..32]);

        let xpriv = Xpriv {
            network: network.into(),
            depth: 0,
            parent_fingerprint: Default::default(),
            chain_code: ChainCode::from(&chain_code),
            child_number: ChildNumber::from_normal_idx(0)
                .map_err(|e| JsValue::from_str(&format!("Invalid child number: {}", e)))?,
            private_key: secret_key,
        };

        Self::from_xpriv(xpriv, network)
    }

    fn from_xpriv(xpriv: Xpriv, network: Network) -> Result<Self, JsValue> {
        let extended_key = SecretString::from(xpriv.to_string());
        let network_kind: NetworkKind = xpriv.network.into();

        let external_base = format!("tr({}/0/*)", xpriv);
        let internal_base = format!("tr({}/1/*)", xpriv);

        // Calculate and append checksums
        let external_checksum = calc_checksum(&external_base).map_err(|e| {
            JsValue::from_str(&format!(
                "Failed to calc external descriptor for bitcoin wallet : {}",
                e
            ))
        })?;
        let internal_checksum = calc_checksum(&internal_base).map_err(|e| {
            JsValue::from_str(&format!(
                "Failed to calc internal descriptor for bitcoin wallet : {}",
                e
            ))
        })?;

        let external_descriptor = format!("{}#{}", external_base, external_checksum);
        let internal_descriptor = format!("{}#{}", internal_base, internal_checksum);
        let public_data = KeyPair {
            descriptor: external_descriptor.clone(),
            change_descriptor: internal_descriptor.clone(),
            network: network_kind,
        };

        //TODO: get this stored remotely and reloaded when the user comes back to the site & logs in
        // we will want the ChangeSet to be a json object and encypted by the user's nostr private key before storing remotely
        let change_set = ChangeSet::default();

        // Load will always come back as None for now since ChangeSet is empty, this wont be helpful until we have saved state that we load
        let bdk_wallet = Wallet::load()
            .descriptor(KeychainKind::External, Some(external_descriptor.clone()))
            .descriptor(KeychainKind::Internal, Some(internal_descriptor.clone()))
            .extract_keys()
            .check_network(network)
            .load_wallet_no_persist(change_set.clone()) //TODO: we may need to change this for 'existing' wallets
            .map_err(|e| JsValue::from_str(&format!("Failed to load bitcoin wallet : {}", e)))?;

        let wallet = if let Some(loaded_wallet) = bdk_wallet {
            loaded_wallet
        } else {
            Wallet::create(external_descriptor, internal_descriptor)
                .network(network)
                .create_wallet_no_persist()
                .map_err(|e| {
                    JsValue::from_str(&format!("Failed to create new bitcoin wallet: {}", e))
                })?
        };

        Ok(Self {
            extended_key,
            public_data,
            nostr_client: NostrClientWrapper::new(),
            bdk_wallet: wallet,
            network,
            change_set,
            dlc_contracts: HashMap::new(),
        })
    }

    async fn load_from_encrypted(
        nostr_client: &NostrClientWrapper,
        encrypted: &str,
        network: Network,
    ) -> Result<Self, JsValue> {
        let signer = nostr_client
            .signer
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Nostr signer not initialized"))?;

        let pubkey = signer
            .get_public_key()
            .await
            .map_err(|e| JsValue::from_str(&format!("Failed to get public key: {}", e)))?;

        let xpriv_str = signer
            .nip44_decrypt(&pubkey, encrypted)
            .await
            .map_err(|e| JsValue::from_str(&format!("Decryption error: {}", e)))?;

        let xpriv = Xpriv::from_str(&xpriv_str)
            .map_err(|e| JsValue::from_str(&format!("Invalid xpriv: {}", e)))?;

        Self::from_xpriv(xpriv, network)
    }

    #[wasm_bindgen(js_name = "getPublicData")]
    pub fn get_public_data(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&self.public_data)
            .map_err(|e| JsValue::from_str(&format!("Serialization error: {}", e)))
    }

    fn update_changes(&mut self) {
        if let Some(new_changes) = self.bdk_wallet.staged() {
            self.change_set.merge(new_changes.to_owned());
            //TODO: serialize and backup self.new_changes here
        }
    }

    #[wasm_bindgen(js_name = "getEncryptedMasterKey")]
    pub async fn get_encrypted_master_key(&self, nostr_pubkey: &str) -> Result<JsValue, JsValue> {
        let encrypted = self.encrypt_key(&self.extended_key, nostr_pubkey).await?;

        let encrypted_key = EncryptedKey {
            encrypted_bitcoin_private_key: encrypted,
            network: self.network.to_string(),
        };

        serde_wasm_bindgen::to_value(&encrypted_key)
            .map_err(|e| JsValue::from_str(&format!("Serialization error: {}", e)))
    }

    async fn encrypt_key(
        &self,
        key_to_encrypt: &SecretString,
        nostr_pubkey: &str,
    ) -> Result<String, JsValue> {
        let pubkey = nostr_sdk::PublicKey::from_bech32(nostr_pubkey)
            .map_err(|e| JsValue::from_str(&format!("Invalid nostr public key: {}", e)))?;

        let signer = self
            .nostr_client
            .signer
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Nostr signer not initialized"))?;

        signer
            .nip44_encrypt(&pubkey, key_to_encrypt.expose_secret())
            .await
            .map_err(|e| JsValue::from_str(&format!("Encryption error: {}", e)))
    }

    #[wasm_bindgen(js_name = "decryptKey")]
    pub async fn decrypt_key(
        &self,
        encrypted_key: &str,
        nostr_pubkey: &str,
    ) -> Result<String, JsValue> {
        let pubkey = nostr_sdk::PublicKey::from_bech32(nostr_pubkey)
            .map_err(|e| JsValue::from_str(&format!("Invalid nostr public key: {}", e)))?;

        let signer = self
            .nostr_client
            .signer
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Nostr signer not initialized"))?;

        signer
            .nip44_decrypt(&pubkey, encrypted_key)
            .await
            .map_err(|e| JsValue::from_str(&format!("Decryption error: {}", e)))
    }

    #[wasm_bindgen(js_name = "encryptDlcKey")]
    pub async fn encrypt_dlc_key(
        &self,
        entry_index: u32,
        nostr_pubkey: &str,
    ) -> Result<String, JsValue> {
        let child_key = self.derive_dlc_key(entry_index)?;
        self.encrypt_key(&child_key, nostr_pubkey).await
    }

    #[wasm_bindgen(js_name = "getDlcAddress")]
    pub fn get_dlc_address(&mut self, entry_index: u32) -> Result<String, JsValue> {
        let child_key = self.derive_dlc_key(entry_index)?;
        let xpriv = Xpriv::from_str(&child_key.expose_secret())
            .map_err(|e| JsValue::from_str(&format!("Invalid child key: {}", e)))?;

        // Create secp context
        let secp = Secp256k1::new();

        // Get the secret key and create keypair
        let secret_key = xpriv.private_key;
        let keypair = Keypair::from_secret_key(&secp, &secret_key);

        // Get x-only public key
        let (internal_key, _parity) = XOnlyPublicKey::from_keypair(&keypair);

        // Generate taproot hash and convert to scalar
        let tap_tweak_hash = TapTweakHash::from_key_and_tweak(internal_key, None);
        let tweak = tap_tweak_hash.to_scalar();

        // Apply tweak - note we get back both the tweaked key and its parity
        let (tweaked_key, _parity) = internal_key
            .add_tweak(&secp, &tweak)
            .map_err(|e| JsValue::from_str(&format!("Tweak error: {}", e)))?;

        // Create address using tweaked key
        let address = Address::p2tr(&secp, tweaked_key, None, self.network);

        // Make sure BDK reveals up to our DLC index
        let _ = self
            .bdk_wallet
            .reveal_addresses_to(KeychainKind::Internal, entry_index);

        self.update_changes();
        Ok(address.to_string())
    }

    fn derive_dlc_key(&self, entry_index: u32) -> Result<SecretString, JsValue> {
        // BIP86 path for Taproot: m/86'/0'/contract_index'
        let path = format!("m/86'/0'/{}'/0/0", entry_index);
        let path = DerivationPath::from_str(&path)
            .map_err(|e| JsValue::from_str(&format!("Invalid derivation path: {}", e)))?;

        // Create secp context for signing using DLC's Secp256k1
        let secp = Secp256k1::new();

        // Convert BDK master key to DLC master key format
        let master_xpriv =
            dlctix::bitcoin::bip32::Xpriv::from_str(self.extended_key.expose_secret())
                .map_err(|e| JsValue::from_str(&format!("Invalid master key: {}", e)))?;

        // Derive the child key using DLC's derive_priv
        let child_xpriv = master_xpriv
            .derive_priv(&secp, &path)
            .map_err(|e| JsValue::from_str(&format!("Key derivation error: {}", e)))?;

        Ok(SecretString::from(child_xpriv.to_string()))
    }

    #[wasm_bindgen(js_name = "addEntryIndex")]
    pub fn add_entry_index(&mut self, entry_index: u32) -> Result<String, JsValue> {
        let child_key = self.derive_dlc_key(entry_index)?;
        let payout_preimage = self.generate_preimage_from_secret(&child_key);
        let payout_preimage = SecretString::new(hex::encode(payout_preimage).into());

        let payout_hash = self.generate_preimage_from_secret(&payout_preimage);
        let payout_hash = hex::encode(payout_hash);

        self.dlc_contracts.insert(
            entry_index,
            DlcEntry {
                child_key: SecretString::from(child_key),
                payout_preimage,
                contract: None,
                params: None,
                funding_outpoint: None,
                ticket_preimage: None,
            },
        );

        Ok(payout_hash)
    }

    fn generate_preimage_from_secret(&self, child_key: &SecretString) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(child_key.expose_secret().as_bytes());
        hasher.finalize().into()
    }

    #[wasm_bindgen(js_name = "addContract")]
    pub fn add_contract(
        &mut self,
        entry_index: u32,
        params: JsValue,
        funding_outpoint: JsValue,
    ) -> Result<(), JsValue> {
        let params: ContractParameters =
            serde_wasm_bindgen::from_value(params.clone()).map_err(|e| {
                JsValue::from_str(&format!("Deserialization contract params error: {}", e))
            })?;

        let funding_outpoint: OutPoint = serde_wasm_bindgen::from_value(funding_outpoint.clone())
            .map_err(|e| {
            JsValue::from_str(&format!("Deserialization funding outpoint error: {}", e))
        })?;

        let contract = TicketedDLC::new(params.clone(), funding_outpoint).map_err(|e| {
            JsValue::from_str(&format!("Failed to build ticketed dlc from input: {}", e))
        })?;
        let Some(dlc_data) = self.dlc_contracts.get_mut(&entry_index) else {
            return Err(JsValue::from_str("No current contract error"));
        };
        dlc_data.contract = Some(contract);
        dlc_data.funding_outpoint = Some(funding_outpoint);
        dlc_data.params = Some(params);
        Ok(())
    }

    #[wasm_bindgen(js_name = "generatePublicNonces")]
    pub fn generate_public_nonces(&self, entry_index: u32) -> Result<JsValue, JsValue> {
        //TODO: handle re-generating the contract if user walks away then do the nonce generating
        let Some(dlc_data) = self.dlc_contracts.get(&entry_index) else {
            return Err(JsValue::from_str("No current contract error"));
        };
        let Some(ref contract) = dlc_data.contract else {
            return Err(JsValue::from_str("No current contract error"));
        };

        let signing_key = Scalar::from_str(dlc_data.child_key.expose_secret())
            .map_err(|e| JsValue::from_str(&format!("Scalar conversion error: {}", e)))?;

        let mut rng = StdRng::from_entropy();

        let signing_session = SigningSession::new(contract.to_owned(), &mut rng, signing_key)
            .map_err(|e| JsValue::from_str(&format!("Failed to create signing session: {}", e)))?;

        let public_nonces = signing_session.our_public_nonces();

        serde_wasm_bindgen::to_value(public_nonces)
            .map_err(|e| JsValue::from_str(&format!("Serialization of public nonces error: {}", e)))
    }

    #[wasm_bindgen(js_name = "signAggregateNonces")]
    pub fn sign_aggregate_nonces(
        &self,
        aggregate_nonces: JsValue,
        entry_index: u32,
    ) -> Result<JsValue, JsValue> {
        //TODO: handle re-generating the contract if user walks away then do the nonce generating
        let aggregated_nonces: SigMap<AggNonce> =
            serde_wasm_bindgen::from_value(aggregate_nonces.clone()).map_err(|e| {
                JsValue::from_str(&format!("Deserialization aggregate nonces error: {}", e))
            })?;

        let Some(dlc_data) = self.dlc_contracts.get(&entry_index) else {
            return Err(JsValue::from_str("No current contract error"));
        };

        let Some(ref contract) = dlc_data.contract else {
            return Err(JsValue::from_str("No current contract error"));
        };

        let signing_key = Scalar::from_str(dlc_data.child_key.expose_secret())
            .map_err(|e| JsValue::from_str(&format!("Scalar conversion error: {}", e)))?;

        let mut rng = StdRng::from_entropy();

        let signing_session = SigningSession::new(contract.to_owned(), &mut rng, signing_key)
            .map_err(|e| JsValue::from_str(&format!("Failed to create signing session: {}", e)))?;

        let partial_sigs = signing_session
            .compute_partial_signatures(aggregated_nonces)
            .map_err(|e| {
                JsValue::from_str(&format!(
                    "Failed to generate partial signatures session: {}",
                    e
                ))
            })?;

        let sigs = partial_sigs.our_partial_signatures();

        serde_wasm_bindgen::to_value(sigs).map_err(|e| {
            JsValue::from_str(&format!("Serialization of partial signatures error: {}", e))
        })
    }
}
