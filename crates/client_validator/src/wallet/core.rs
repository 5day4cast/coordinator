use super::{DlcEntryData, WalletError};
use crate::NostrClientCore;
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
    bitcoin::OutPoint,
    musig2::{AggNonce, PartialSignature},
    secp::Scalar,
    ContractParameters, SigMap, SigningSession, TicketedDLC,
};
use nostr_sdk::{FromBech32, NostrSigner};
use rand::{rngs::StdRng, thread_rng, RngCore, SeedableRng};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{collections::HashMap, str::FromStr};

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
            None => Ok(TaprootWalletCore::create_new(network)?),
        }
    }
}

pub struct TaprootWalletCore {
    extended_key: SecretString,
    public_data: KeyPair,
    nostr_client: NostrClientCore,
    bdk_wallet: Wallet,
    network: Network,
    change_set: ChangeSet,
    dlc_contracts: HashMap<u32, DlcEntry>,
}

impl TaprootWalletCore {
    pub fn create_new(network: Network) -> Result<Self, WalletError> {
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

        Self::from_xpriv(xpriv, network)
    }

    fn from_xpriv(xpriv: Xpriv, network: Network) -> Result<Self, WalletError> {
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
            nostr_client: NostrClientCore::new(),
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

        Self::from_xpriv(xpriv, network)
    }

    pub fn get_public_data(&self) -> KeyPair {
        self.public_data.clone()
    }

    fn update_changes(&mut self) {
        if let Some(new_changes) = self.bdk_wallet.staged() {
            self.change_set.merge(new_changes.to_owned());
            //TODO: serialize and backup self.new_changes here
        }
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

    pub async fn encrypt_dlc_key(
        &self,
        entry_index: u32,
        nostr_pubkey: &str,
    ) -> Result<String, WalletError> {
        let child_key = self.derive_dlc_key(entry_index)?;
        self.encrypt_key(&child_key, nostr_pubkey).await
    }

    pub fn get_dlc_address(&mut self, entry_index: u32) -> Result<String, WalletError> {
        let child_key = self.derive_dlc_key(entry_index)?;
        let xpriv = Xpriv::from_str(&child_key.expose_secret())
            .map_err(|e| WalletError::DlcKeyError(format!("Invalid child key: {}", e)))?;

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
            .map_err(|e| WalletError::DlcKeyError(format!("Tweak error: {}", e)))?;

        // Create address using tweaked key
        let address = Address::p2tr(&secp, tweaked_key, None, self.network);

        // Make sure BDK reveals up to our DLC index
        let _ = self
            .bdk_wallet
            .reveal_addresses_to(KeychainKind::Internal, entry_index);

        self.update_changes();
        Ok(address.to_string())
    }

    fn derive_dlc_key(&self, entry_index: u32) -> Result<SecretString, WalletError> {
        // BIP86 path for Taproot: m/86'/0'/contract_index'
        let path = format!("m/86'/0'/{}'/0/0", entry_index);
        let path = DerivationPath::from_str(&path)
            .map_err(|e| WalletError::DlcKeyError(format!("Invalid derivation path: {}", e)))?;

        // Create secp context for signing using DLC's Secp256k1
        let secp = Secp256k1::new();

        // Convert BDK master key to DLC master key format
        let master_xpriv =
            dlctix::bitcoin::bip32::Xpriv::from_str(self.extended_key.expose_secret())
                .map_err(|e| WalletError::DlcKeyError(format!("Invalid master key: {}", e)))?;

        // Derive the child key using DLC's derive_priv
        let child_xpriv = master_xpriv
            .derive_priv(&secp, &path)
            .map_err(|e| WalletError::DlcKeyError(format!("Key derivation error: {}", e)))?;

        Ok(SecretString::from(child_xpriv.to_string()))
    }

    pub fn add_entry_index(&mut self, entry_index: u32) -> Result<String, WalletError> {
        let child_key = self.derive_dlc_key(entry_index)?;
        let payout_preimage = self.generate_preimage_from_secret(&child_key);
        let payout_preimage = SecretString::from(hex::encode(payout_preimage));

        let payout_hash = self.generate_preimage_from_secret(&payout_preimage);
        let payout_hash = hex::encode(payout_hash);

        self.dlc_contracts.insert(
            entry_index,
            DlcEntry {
                contract: None,
                data: DlcEntryData {
                    child_key,
                    payout_preimage,
                    params: None,
                    funding_outpoint: None,
                    ticket_preimage: None,
                },
            },
        );

        Ok(payout_hash)
    }

    fn generate_preimage_from_secret(&self, child_key: &SecretString) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(child_key.expose_secret().as_bytes());
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

    pub fn generate_public_nonces(
        &self,
        entry_index: u32,
    ) -> Result<dlctix::SigMap<dlctix::musig2::PubNonce>, WalletError> {
        let Some(dlc_data) = self.dlc_contracts.get(&entry_index) else {
            return Err(WalletError::NoContract(entry_index));
        };

        let Some(ref contract) = dlc_data.contract else {
            return Err(WalletError::ContractError("No contract found".into()));
        };

        let signing_key = Scalar::from_str(dlc_data.data.child_key.expose_secret())
            .map_err(|e| WalletError::KeyError(e.to_string()))?;

        let mut rng = StdRng::from_entropy();

        let signing_session = SigningSession::new(contract.to_owned(), &mut rng, signing_key)
            .map_err(|e| WalletError::DlcError(e.to_string()))?;

        Ok(signing_session.our_public_nonces().to_owned())
    }

    pub fn sign_aggregate_nonces(
        &self,
        aggregate_nonces: SigMap<AggNonce>,
        entry_index: u32,
    ) -> Result<SigMap<PartialSignature>, WalletError> {
        let Some(dlc_data) = self.dlc_contracts.get(&entry_index) else {
            return Err(WalletError::NoContract(entry_index));
        };

        let Some(ref contract) = dlc_data.contract else {
            return Err(WalletError::ContractError("No contract found".into()));
        };

        let signing_key = Scalar::from_str(dlc_data.data.child_key.expose_secret())
            .map_err(|e| WalletError::KeyError(e.to_string()))?;

        let mut rng = StdRng::from_entropy();

        let signing_session = SigningSession::new(contract.to_owned(), &mut rng, signing_key)
            .map_err(|e| WalletError::DlcError(e.to_string()))?;

        let partial_sigs = signing_session
            .compute_partial_signatures(aggregate_nonces)
            .map_err(|e| WalletError::DlcError(e.to_string()))?;

        Ok(partial_sigs.our_partial_signatures().to_owned())
    }
}
