use super::{DlcEntryData, WalletError};
use crate::NostrClientCore;
use bdk_wallet::{
    bitcoin::{
        bip32::{ChainCode, ChildNumber, DerivationPath},
        ecdsa,
        hashes::{sha256, Hash},
        secp256k1::{Message, Secp256k1 as BdkSecp256k1, SecretKey},
        sighash::{EcdsaSighashType, SighashCache},
        Network, NetworkKind as BDKNetworkKind, PublicKey,
    },
    descriptor::calc_checksum,
    ChangeSet, KeychainKind, Wallet,
};
use blake2::{Blake2b512, Digest};
use dlctix::{
    bitcoin::{bip32::Xpriv, OutPoint, Psbt},
    musig2::{AggNonce, PartialSignature},
    secp::Scalar,
    ContractParameters, NonceSharingRound, SigMap, SigningSession, TicketedDLC,
};
use log::debug;
use nostr_sdk::{FromBech32, NostrSigner};
use rand::{thread_rng, RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{collections::HashMap, io::Write, str::FromStr};

use super::DlcEntry;

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

#[derive(Clone)]
pub struct TaprootWalletCoreBuilder {
    network: Option<String>,
    nostr_client: Option<NostrClientCore>,
    encrypted_key: Option<String>,
}

impl TaprootWalletCoreBuilder {
    pub fn new() -> Self {
        Self {
            network: None,
            nostr_client: None,
            encrypted_key: None,
        }
    }

    pub fn network(mut self, network: String) -> TaprootWalletCoreBuilder {
        self.network = Some(network);
        self
    }

    pub fn nostr_client(self, client: &NostrClientCore) -> Self {
        Self {
            network: self.network,
            nostr_client: Some(client.clone()),
            encrypted_key: self.encrypted_key,
        }
    }

    pub fn encrypted_key(mut self, key: String) -> TaprootWalletCoreBuilder {
        self.encrypted_key = Some(key);
        self
    }

    pub async fn build(self) -> Result<TaprootWalletCore, WalletError> {
        let network = self
            .network
            .ok_or_else(|| WalletError::NetworkError("Network is required".into()))?;

        let nostr_client = self
            .nostr_client
            .ok_or_else(|| WalletError::SignerError("NostrClient is required".into()))?;

        let network =
            Network::from_str(&network).map_err(|e| WalletError::NetworkError(e.to_string()))?;

        match self.encrypted_key {
            Some(encrypted) => {
                TaprootWalletCore::load_from_encrypted(&nostr_client, &encrypted, network).await
            }
            None => Ok(TaprootWalletCore::create_new(&nostr_client, network)?),
        }
    }
}

pub struct TaprootWalletCore {
    extended_key: SecretString,
    public_data: KeyPair,
    nostr_client: NostrClientCore,
    network: Network,
    dlc_contracts: HashMap<u32, DlcEntry>,
    //TODO: add functionality for payouts via bdk_wallet
    bdk_wallet: Wallet,
    change_set: ChangeSet,
}

impl TaprootWalletCore {
    pub fn create_new(
        nostr_client: &NostrClientCore,
        network: Network,
    ) -> Result<Self, WalletError> {
        let mut entropy = [0u8; 32];
        thread_rng().fill_bytes(&mut entropy);
        let secret_key =
            SecretKey::from_slice(&entropy).map_err(|e| WalletError::KeyError(e.to_string()))?;

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
                .map_err(|e| WalletError::KeyError(e.to_string()))?,
            private_key: secret_key,
        };

        Self::from_xpriv(nostr_client, xpriv, network)
    }

    fn from_xpriv(
        nostr_client: &NostrClientCore,
        xpriv: Xpriv,
        network: Network,
    ) -> Result<Self, WalletError> {
        let extended_key = SecretString::from(xpriv.to_string());
        let network_kind: NetworkKind = xpriv.network.into();

        let external_base = format!("tr({}/0/*)", xpriv);
        let internal_base = format!("tr({}/1/*)", xpriv);

        // Calculate and append checksums
        let external_checksum =
            calc_checksum(&external_base).map_err(|e| WalletError::CheckSum(e.to_string()))?;
        let internal_checksum =
            calc_checksum(&internal_base).map_err(|e| WalletError::CheckSum(e.to_string()))?;

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
            .map_err(|e| WalletError::LoadWallet(e.to_string()))?;

        let wallet = if let Some(loaded_wallet) = bdk_wallet {
            loaded_wallet
        } else {
            Wallet::create(external_descriptor, internal_descriptor)
                .network(network)
                .create_wallet_no_persist()
                .map_err(|e| WalletError::LoadWallet(e.to_string()))?
        };

        Ok(Self {
            extended_key,
            public_data,
            nostr_client: nostr_client.to_owned(),
            bdk_wallet: wallet,
            network,
            change_set,
            dlc_contracts: HashMap::new(),
        })
    }

    pub async fn load_from_encrypted(
        nostr_client: &NostrClientCore,
        encrypted: &str,
        network: Network,
    ) -> Result<Self, WalletError> {
        let signer = nostr_client
            .signer
            .as_ref()
            .ok_or_else(|| WalletError::NostrNotInitialize)?;

        let pubkey = signer
            .get_public_key()
            .await
            .map_err(|e| WalletError::PublicKeyError(e.to_string()))?;

        let xpriv_str = signer
            .nip44_decrypt(&pubkey, encrypted)
            .await
            .map_err(|e| WalletError::PublicKeyError(e.to_string()))?;

        let xpriv =
            Xpriv::from_str(&xpriv_str).map_err(|e| WalletError::PublicKeyError(e.to_string()))?;

        Self::from_xpriv(nostr_client, xpriv, network)
    }

    pub fn get_public_data(&self) -> KeyPair {
        self.public_data.clone()
    }

    fn update_changes(&mut self) {
        todo!();
        //TODO: serialize and backup self.new_changes here
        /*if let Some(new_changes) = self.bdk_wallet.staged() {
            self.change_set.merge(new_changes.to_owned());

        }*/
    }

    pub async fn get_encrypted_master_key(
        &self,
        nostr_pubkey: &str,
    ) -> Result<EncryptedKey, WalletError> {
        let encrypted = self.encrypt_key(&self.extended_key, nostr_pubkey).await?;

        Ok(EncryptedKey {
            encrypted_bitcoin_private_key: encrypted,
            network: self.network.to_string(),
        })
    }

    pub async fn encrypt_key(
        &self,
        key_to_encrypt: &SecretString,
        nostr_pubkey: &str,
    ) -> Result<String, WalletError> {
        let pubkey = nostr_sdk::PublicKey::from_bech32(nostr_pubkey)
            .map_err(|e| WalletError::InvalidPublicKey(e.to_string()))?;

        let signer = self
            .nostr_client
            .signer
            .as_ref()
            .ok_or_else(|| WalletError::NostrNotInitialize)?;

        signer
            .nip44_encrypt(&pubkey, key_to_encrypt.expose_secret())
            .await
            .map_err(|e| WalletError::EncryptionError(e.to_string()))
    }

    pub async fn decrypt_key(
        &self,
        encrypted_key: &str,
        nostr_pubkey: &str,
    ) -> Result<String, WalletError> {
        let pubkey = nostr_sdk::PublicKey::from_bech32(nostr_pubkey)
            .map_err(|e| WalletError::InvalidPublicKey(e.to_string()))?;

        let signer = self
            .nostr_client
            .signer
            .as_ref()
            .ok_or_else(|| WalletError::NostrNotInitialize)?;

        signer
            .nip44_decrypt(&pubkey, encrypted_key)
            .await
            .map_err(|e| WalletError::DecryptionError(e.to_string()))
    }

    pub async fn get_encrypted_dlc_private_key(
        &self,
        entry_index: u32,
        nostr_pubkey: &str,
    ) -> Result<String, WalletError> {
        let child_xpriv = self.derive_dlc_key(entry_index)?;
        let secret_bytes = child_xpriv.private_key.secret_bytes();
        let child_key = hex::encode(secret_bytes);
        self.encrypt_key(&SecretString::from(child_key), nostr_pubkey)
            .await
    }

    pub async fn get_encrypted_dlc_payout_preimage(
        &self,
        entry_index: u32,
        nostr_pubkey: &str,
    ) -> Result<String, WalletError> {
        if let Some(entry) = self.get_dlc_entry(entry_index) {
            self.encrypt_key(&SecretString::from(entry.payout_preimage), nostr_pubkey)
                .await
        } else {
            Err(WalletError::DlcEntryNotFound(entry_index))
        }
    }

    pub async fn get_dlc_public_key(&self, entry_index: u32) -> Result<String, WalletError> {
        let child_xpriv = self.derive_dlc_key(entry_index)?;

        let secret_bytes = child_xpriv.private_key.secret_bytes();
        let secret_scalar = Scalar::from_hex(&hex::encode(secret_bytes)).map_err(|e| {
            WalletError::KeyError(format!("Failed to convert key to scalar: {}", e))
        })?;

        let point = secret_scalar.base_point_mul();
        Ok(point.to_string())
    }

    pub fn derive_dlc_key(&self, entry_index: u32) -> Result<Xpriv, WalletError> {
        // BIP86 path for Taproot: m/86'/0'/contract_index'
        let path = format!("m/86'/0'/{}'/0/0", entry_index);
        debug!("Deriving key with path: {}", path);

        let path = DerivationPath::from_str(&path)
            .map_err(|e| WalletError::DlcKeyError(format!("Invalid derivation path: {}", e)))?;

        let secp = BdkSecp256k1::new();
        let master_xpriv =
            dlctix::bitcoin::bip32::Xpriv::from_str(self.extended_key.expose_secret())
                .map_err(|e| WalletError::DlcKeyError(format!("Invalid master key: {}", e)))?;
        debug!(
            "Master key fingerprint: {}",
            master_xpriv.fingerprint(&BdkSecp256k1::new())
        );

        let child_xpriv = master_xpriv
            .derive_priv(&secp, &path)
            .map_err(|e| WalletError::DlcKeyError(format!("Key derivation error: {}", e)))?;

        Ok(child_xpriv)
    }

    pub fn add_entry_index(&mut self, entry_index: u32) -> Result<String, WalletError> {
        let child_xpriv = self.derive_dlc_key(entry_index)?;
        let payout_preimage =
            self.generate_preimage_from_secret(child_xpriv.private_key.secret_bytes());

        //TODO: this should be a lightning invoice hash
        let payout_hash = self.generate_preimage_from_secret(payout_preimage);

        let payout_preimage = SecretString::from(hex::encode(payout_preimage));
        let payout_hash = hex::encode(payout_hash);

        self.dlc_contracts.insert(
            entry_index,
            DlcEntry {
                contract: None,
                data: DlcEntryData {
                    payout_preimage,
                    params: None,
                    funding_outpoint: None,
                    ticket_preimage: None,
                },
            },
        );

        Ok(payout_hash)
    }

    fn generate_preimage_from_secret(&self, secret_bytes: [u8; 32]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(&secret_bytes);
        hasher.finalize().into()
    }

    pub fn get_dlc_entry(&self, entry_index: u32) -> Option<DlcEntryData> {
        let Some(entry) = self.dlc_contracts.get(&entry_index) else {
            return None;
        };
        Some(entry.data.clone())
    }

    pub fn add_contract(
        &mut self,
        entry_index: u32,
        params: ContractParameters,
        funding_outpoint: OutPoint,
    ) -> Result<(), WalletError> {
        let contract = TicketedDLC::new(params.clone(), funding_outpoint)
            .map_err(|e| WalletError::ContractError(e.to_string()))?;

        let Some(dlc_data) = self.dlc_contracts.get_mut(&entry_index) else {
            return Err(WalletError::NoContract(entry_index));
        };

        dlc_data.contract = Some(contract);
        dlc_data.data.funding_outpoint = Some(funding_outpoint);
        dlc_data.data.params = Some(params);
        Ok(())
    }

    fn reconstruct_contract(&self, entry_index: u32) -> Result<TicketedDLC, WalletError> {
        let Some(entry) = self.dlc_contracts.get(&entry_index) else {
            return Err(WalletError::NoContract(entry_index));
        };

        let Some(ref params) = entry.data.params else {
            return Err(WalletError::ContractError("No parameters found".into()));
        };

        let Some(funding_outpoint) = entry.data.funding_outpoint else {
            return Err(WalletError::ContractError(
                "No funding outpoint found".into(),
            ));
        };

        // Reconstruct the contract from parameters
        TicketedDLC::new(params.clone(), funding_outpoint)
            .map_err(|e| WalletError::ContractError(e.to_string()))
    }

    pub fn generate_public_nonces(
        &mut self,
        entry_index: u32,
    ) -> Result<dlctix::SigMap<dlctix::musig2::PubNonce>, WalletError> {
        let contract = self.reconstruct_contract(entry_index)?;

        let funding_outpoint = contract.funding_outpoint();

        let child_xpriv = self.derive_dlc_key(entry_index)?;
        let secret_bytes = child_xpriv.private_key.secret_bytes();
        let secret_scalar = Scalar::from_hex(&hex::encode(secret_bytes)).map_err(|e| {
            WalletError::KeyError(format!("Failed to convert key to scalar: {}", e))
        })?;

        let mut rng = self.create_deterministic_rng(&funding_outpoint, secret_scalar);

        let signing_session =
            SigningSession::<NonceSharingRound>::new(contract.to_owned(), &mut rng, secret_scalar)
                .map_err(|e| WalletError::DlcError(e.to_string()))?;

        Ok(signing_session.our_public_nonces().to_owned())
    }

    pub fn sign_aggregate_nonces(
        &self,
        aggregate_nonces: SigMap<AggNonce>,
        entry_index: u32,
    ) -> Result<SigMap<PartialSignature>, WalletError> {
        let contract = self.reconstruct_contract(entry_index)?;

        // Get funding outpoint for deterministic RNG
        let funding_outpoint = contract.funding_outpoint();

        let child_xpriv = self.derive_dlc_key(entry_index)?;

        let secret_bytes = child_xpriv.private_key.secret_bytes();
        let secret_scalar = Scalar::from_hex(&hex::encode(secret_bytes)).map_err(|e| {
            WalletError::KeyError(format!("Failed to convert key to scalar: {}", e))
        })?;

        let pubkey = secret_scalar.base_point_mul();
        debug!("Client signing");

        contract
            .params()
            .players
            .iter()
            .position(|p| p.pubkey == pubkey)
            .ok_or_else(|| WalletError::DlcError("Player not found in contract".into()))?;

        debug!("Contract parameters: {:?}", contract.params());
        debug!("Received aggregate nonces: {:?}", aggregate_nonces);

        let mut rng = self.create_deterministic_rng(&funding_outpoint, secret_scalar);

        let initial_session =
            SigningSession::<NonceSharingRound>::new(contract.to_owned(), &mut rng, secret_scalar)
                .map_err(|e| WalletError::DlcError(e.to_string()))?;

        debug!("Generated initial nonces");

        // Use all aggregate nonces for signing
        let partial_sigs = initial_session
            .compute_partial_signatures(aggregate_nonces)
            .map_err(|e| WalletError::DlcError(format!("Signature computation failed: {}", e)))?;

        debug!("Generated partial signatures");

        Ok(partial_sigs.our_partial_signatures().to_owned())
    }

    pub fn sign_funding_psbt(&self, mut psbt: Psbt, entry_index: u32) -> Result<Psbt, WalletError> {
        // Derive the child key specific to this DLC entry
        let child_xpriv = self.derive_dlc_key(entry_index)?;
        let child_privkey = child_xpriv.to_priv();
        let secp = BdkSecp256k1::new();
        let child_pubkey = PublicKey::from_private_key(&secp, &child_privkey);

        debug!(
            "Signing funding PSBT for entry {} with derived pubkey: {}",
            entry_index, child_pubkey
        );

        let mut signed_count = 0;

        // Clone the transaction for sighash computation
        let tx = psbt.unsigned_tx.clone();

        // Process each input in the PSBT
        for (input_index, input) in psbt.inputs.iter_mut().enumerate() {
            // Check if this is an escrow input by looking for witness_script
            let witness_script = match &input.witness_script {
                Some(ws) => ws,
                None => {
                    debug!(
                        "Input {} is not an escrow input (no witness_script), skipping",
                        input_index
                    );
                    continue;
                }
            };

            // Ensure we have the witness UTXO for signing
            let witness_utxo = input.witness_utxo.as_ref().ok_or_else(|| {
                WalletError::KeyError(format!(
                    "Missing witness_utxo for escrow input {}",
                    input_index
                ))
            })?;

            // Check if we already have a signature from this key
            if input.partial_sigs.contains_key(&child_pubkey) {
                debug!("Input {} already has our signature, skipping", input_index);
                continue;
            }

            // For miniscript escrow, verify our key is actually needed
            // Simple verification: check if our public key bytes appear in the witness script
            let script_bytes = witness_script.as_bytes();
            let pubkey_bytes = child_pubkey.to_bytes();

            let key_found = script_bytes
                .windows(pubkey_bytes.len())
                .any(|window| window == pubkey_bytes);

            if !key_found {
                debug!(
                    "Our pubkey {} not found in witness script for input {}, skipping",
                    child_pubkey, input_index
                );
                continue;
            }

            debug!(
                "Found our key in escrow input {}, creating signature",
                input_index
            );

            // Get the sighash type from PSBT or default to ALL
            let sighash_type = match input.sighash_type {
                Some(psbt_type) => {
                    EcdsaSighashType::from_standard(psbt_type.to_u32()).map_err(|_| {
                        WalletError::KeyError("Invalid sighash type in PSBT".to_string())
                    })?
                }
                None => EcdsaSighashType::All,
            };

            // Create a sighash cache for the transaction
            let mut sighash_cache = SighashCache::new(&tx);

            // Compute the signature hash
            let sighash = sighash_cache
                .p2wsh_signature_hash(
                    input_index,
                    witness_script,
                    witness_utxo.value,
                    sighash_type,
                )
                .map_err(|e| {
                    WalletError::KeyError(format!(
                        "Failed to compute sighash for input {}: {}",
                        input_index, e
                    ))
                })?;

            // Create the ECDSA signature
            let message = Message::from_digest(sighash.to_byte_array());
            let signature = secp.sign_ecdsa(&message, &child_privkey.inner);

            // Create ecdsa::Signature with the signature and sighash type
            let ecdsa_sig = ecdsa::Signature {
                signature,
                sighash_type,
            };

            input.partial_sigs.insert(child_pubkey, ecdsa_sig);
            signed_count += 1;

            debug!(
                "Added signature for escrow input {} (witness script: {} bytes)",
                input_index,
                witness_script.len()
            );
        }

        match signed_count {
            0 => debug!(
                "No escrow inputs were signed - our key may not be required for these inputs"
            ),
            n => debug!(
                "Successfully signed {} escrow input(s) for entry {}",
                n, entry_index
            ),
        }

        Ok(psbt)
    }

    fn create_deterministic_rng(
        &self,
        funding_outpoint: &OutPoint,
        private_key: Scalar,
    ) -> ChaCha20Rng {
        let mut hasher = sha256::Hash::engine();

        hasher.write_all(&funding_outpoint.txid[..]).unwrap();
        hasher
            .write_all(&funding_outpoint.vout.to_le_bytes())
            .unwrap();
        hasher.write_all(&private_key.serialize()).unwrap();

        let hash = sha256::Hash::from_engine(hasher);
        let seed: [u8; 32] = hash.to_byte_array();
        ChaCha20Rng::from_seed(seed)
    }
}
