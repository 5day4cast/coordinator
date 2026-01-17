//! Mock Bitcoin client for E2E testing
//!
//! Provides a minimal Bitcoin trait implementation that doesn't require
//! any real Bitcoin infrastructure. Used for Playwright E2E tests.
#![allow(deprecated)] // SignOptions is deprecated but no replacement API exists yet in bdk_wallet 2.3

use async_trait::async_trait;
use bdk_wallet::{
    bitcoin::{
        Address, Amount, FeeRate, Network, OutPoint, Psbt, PublicKey, ScriptBuf, Transaction, Txid,
    },
    AddressInfo, Balance, KeychainKind, LocalOutput, SignOptions,
};
use dlctix::secp::Scalar;
use log::info;
use std::{
    collections::HashMap,
    str::FromStr,
    sync::atomic::{AtomicU32, Ordering},
};
use time::OffsetDateTime;

use super::bitcoin::{Bitcoin, ForeignUtxo, SendOptions};

/// Mock Bitcoin client for E2E testing
pub struct MockBitcoinClient {
    network: Network,
    block_height: AtomicU32,
    address_counter: AtomicU32,
}

impl MockBitcoinClient {
    pub fn new(network: Network) -> Self {
        info!("Creating MockBitcoinClient for network: {:?}", network);
        Self {
            network,
            block_height: AtomicU32::new(100), // Start at block 100
            address_counter: AtomicU32::new(0),
        }
    }

    /// Simulate mining a block
    #[allow(dead_code)]
    pub fn mine_block(&self) {
        self.block_height.fetch_add(1, Ordering::SeqCst);
    }

    /// Set the current block height
    #[allow(dead_code)]
    pub fn set_block_height(&self, height: u32) {
        self.block_height.store(height, Ordering::SeqCst);
    }
}

#[async_trait]
impl Bitcoin for MockBitcoinClient {
    fn get_network(&self) -> Network {
        self.network
    }

    async fn sign_psbt_with_escrow_support(
        &self,
        _psbt: &mut Psbt,
        _options: SignOptions,
    ) -> Result<bool, anyhow::Error> {
        // Mock: pretend signing succeeded
        Ok(true)
    }

    async fn finalize_psbt_with_escrow_support(
        &self,
        _psbt: &mut Psbt,
    ) -> Result<bool, anyhow::Error> {
        // Mock: pretend finalization succeeded
        Ok(true)
    }

    async fn build_psbt(
        &self,
        _script_pubkey: ScriptBuf,
        _amount: Amount,
        _fee_rate: FeeRate,
        _selected_utxos: Vec<OutPoint>,
        _foreign_utxos: Vec<ForeignUtxo>,
    ) -> Result<Psbt, anyhow::Error> {
        // Return a minimal empty PSBT for testing
        // In real E2E tests, we won't actually need to build transactions
        Err(anyhow::anyhow!(
            "MockBitcoinClient: build_psbt not implemented for E2E tests"
        ))
    }

    async fn get_spendable_utxo(&self, _amount_sats: u64) -> Result<LocalOutput, anyhow::Error> {
        Err(anyhow::anyhow!(
            "MockBitcoinClient: no UTXOs available in mock mode"
        ))
    }

    async fn get_current_height(&self) -> Result<u32, anyhow::Error> {
        Ok(self.block_height.load(Ordering::SeqCst))
    }

    async fn get_confirmed_blockchain_time(&self, _blocks: usize) -> Result<u64, anyhow::Error> {
        // Return current timestamp minus some blocks worth of time
        let now = OffsetDateTime::now_utc().unix_timestamp() as u64;
        Ok(now - 600) // 10 minutes ago
    }

    async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error> {
        // Return mock fee rates
        let mut rates = HashMap::new();
        rates.insert(1, 20.0); // 1 block: 20 sat/vB
        rates.insert(3, 15.0); // 3 blocks: 15 sat/vB
        rates.insert(6, 10.0); // 6 blocks: 10 sat/vB
        rates.insert(12, 5.0); // 12 blocks: 5 sat/vB
        Ok(rates)
    }

    async fn get_tx_confirmation_height(&self, _txid: &Txid) -> Result<Option<u32>, anyhow::Error> {
        // Mock: return current height - 3 (confirmed 3 blocks ago)
        let height = self.block_height.load(Ordering::SeqCst);
        Ok(Some(height.saturating_sub(3)))
    }

    async fn broadcast(&self, _transaction: &Transaction) -> Result<(), anyhow::Error> {
        // Mock: pretend broadcast succeeded
        info!("MockBitcoinClient: broadcast transaction (mock - not actually sent)");
        Ok(())
    }

    async fn get_next_address(&self) -> Result<AddressInfo, anyhow::Error> {
        let index = self.address_counter.fetch_add(1, Ordering::SeqCst);

        // Use a well-known regtest address format
        // bcrt1q prefix for regtest bech32 addresses
        let address_str = match self.network {
            Network::Regtest => {
                // Use a valid bech32 regtest address
                "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080"
            }
            Network::Testnet | Network::Testnet4 | Network::Signet => {
                "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"
            }
            _ => "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
        };

        let address = Address::from_str(address_str)
            .map_err(|e| anyhow::anyhow!("Failed to parse mock address: {}", e))?
            .require_network(self.network)
            .map_err(|e| anyhow::anyhow!("Address network mismatch: {}", e))?;

        Ok(AddressInfo {
            index,
            address,
            keychain: KeychainKind::External,
        })
    }

    async fn get_public_key(&self) -> Result<PublicKey, anyhow::Error> {
        // Return a deterministic test public key
        // This is the generator point G (not for production use)
        let pubkey_hex = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let pubkey_bytes = hex::decode(pubkey_hex)?;
        let pubkey = PublicKey::from_slice(&pubkey_bytes)?;
        Ok(pubkey)
    }

    async fn get_derived_private_key(&self) -> Result<Scalar, anyhow::Error> {
        // Return a deterministic test scalar using from_hex like the real impl
        // This is for testing only - uses a known test value
        let test_key_hex = "0000000000000000000000000000000000000000000000000000000000000001";
        Scalar::from_hex(test_key_hex)
            .map_err(|e| anyhow::anyhow!("Failed to create test scalar: {:?}", e))
    }

    async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error> {
        Err(anyhow::anyhow!(
            "MockBitcoinClient: transaction {} not found in mock mode",
            txid
        ))
    }

    async fn sign_psbt(
        &self,
        _psbt: &mut Psbt,
        _sign_options: SignOptions,
    ) -> Result<bool, anyhow::Error> {
        // Mock: pretend signing succeeded
        Ok(true)
    }

    async fn list_utxos(&self) -> Vec<LocalOutput> {
        // Mock: no UTXOs
        Vec::new()
    }

    async fn sync(&self) -> Result<(), anyhow::Error> {
        // Mock: nothing to sync
        info!("MockBitcoinClient: sync (mock - no actual sync performed)");
        Ok(())
    }

    async fn get_balance(&self) -> Result<Balance, anyhow::Error> {
        // Mock: return zero balance
        Ok(Balance::default())
    }

    async fn get_outputs(&self) -> Result<Vec<LocalOutput>, anyhow::Error> {
        // Mock: no outputs
        Ok(Vec::new())
    }

    async fn send_to_address(
        &self,
        _send_options: SendOptions,
        _selected_utxos: Vec<OutPoint>,
    ) -> Result<Txid, anyhow::Error> {
        // Mock: cannot actually send in mock mode
        Err(anyhow::anyhow!(
            "MockBitcoinClient: send_to_address not available in mock mode"
        ))
    }
}
