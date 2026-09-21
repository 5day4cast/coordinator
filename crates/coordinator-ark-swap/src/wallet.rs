//! The service's own Ark wallet: the liquidity that pays escrows.
//!
//! Top it up by sending VTXOs to its Ark address, for example from `mutinynet.arkade.money`.
//! Or send on-chain coins to its boarding address and call `POST /v1/wallet/board`.

use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ark_bdk_wallet::Wallet;
use ark_client::{
    Blockchain, Client, Error, InMemorySwapStorage, OfflineClient, OfflineClientConfig,
    SpendStatus, TxStatus,
};
use ark_core::send::SendReceiver;
use ark_core::server::GetVtxosRequest;
use ark_core::{ArkAddress, ExplorerUtxo};
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Address, Amount, OutPoint, Transaction, Txid};
use coordinator_ark::ArkServer;
use serde::Serialize;

use crate::config::Config;

type ArkClient = Client<Esplora, Wallet, InMemorySwapStorage>;

pub struct ArkWallet {
    client: ArkClient,
    server: ArkServer,
    /// One send at a time, so concurrent swaps never select the same VTXOs.
    sending: tokio::sync::Mutex<()>,
}

#[derive(Debug, Serialize)]
pub struct WalletView {
    pub ark_address: String,
    pub boarding_address: String,
    pub confirmed_sat: u64,
    pub pre_confirmed_sat: u64,
    pub recoverable_sat: u64,
}

impl ArkWallet {
    pub async fn open(config: &Config) -> anyhow::Result<Self> {
        // Connecting through coordinator-ark installs the rustls provider before ark-client needs it.
        let server = ArkServer::connect(config.ark_server_url.clone()).await?;
        anyhow::ensure!(
            server.info().network == config.network,
            "the Arkade server runs {}, not {}",
            server.info().network,
            config.network
        );
        let keypair = load_or_create_key(&config.data_dir.join("wallet.key"))?;
        let blockchain = Arc::new(Esplora::new(&config.esplora_url)?);
        let wallet = Arc::new(Wallet::new(keypair, config.network, &config.esplora_url)?);
        let client = OfflineClient::with_keypair(
            OfflineClientConfig {
                ark_server_url: config.ark_server_url.clone(),
                // Boarding waits for a batch, and batches come once a session.
                timeout: Duration::from_secs(2 * server.info().session_duration + 30),
                ..Default::default()
            },
            keypair,
            blockchain,
            wallet,
            Arc::new(InMemorySwapStorage::new()),
        )
        .connect()
        .await
        .map_err(|error| anyhow::anyhow!("connect the Ark wallet: {error}"))?;
        Ok(Self {
            client,
            server,
            sending: tokio::sync::Mutex::new(()),
        })
    }

    /// Parse an escrow address and check it belongs to this wallet's Arkade server.
    pub fn escrow_address(&self, address: &str) -> anyhow::Result<ArkAddress> {
        let address = ArkAddress::decode(address).context("not an Ark address")?;
        anyhow::ensure!(
            address.server() == self.server.rules().signer,
            "the address is for another Arkade server"
        );
        anyhow::ensure!(
            address.encode().starts_with(self.server.hrp()),
            "the address is for another network"
        );
        Ok(address)
    }

    pub fn dust(&self) -> Amount {
        self.server.info().dust
    }

    /// Pay `amount` to `address` in an Arkade transaction, returning its txid.
    pub async fn pay(&self, address: ArkAddress, amount: Amount) -> anyhow::Result<Txid> {
        let _one_at_a_time = self.sending.lock().await;
        self.client
            .send(vec![SendReceiver::bitcoin(address, amount)])
            .await
            .map_err(|error| anyhow::anyhow!("pay the escrow: {error}"))
    }

    /// An unspent VTXO at `address` worth `amount`, created at or after `since` (UNIX seconds).
    ///
    /// After a crash between paying and recording it, this finds the payment instead of paying twice.
    pub async fn paid_vtxo(
        &self,
        address: ArkAddress,
        amount: Amount,
        since: i64,
    ) -> anyhow::Result<Option<OutPoint>> {
        let response = self
            .server
            .client()
            .list_vtxos(GetVtxosRequest::new_for_addresses(std::iter::once(address)))
            .await?;
        Ok(response
            .vtxos
            .into_iter()
            .find(|vtxo| vtxo.amount == amount && vtxo.created_at >= since && !vtxo.is_spent)
            .map(|vtxo| vtxo.outpoint))
    }

    pub async fn view(&self) -> anyhow::Result<WalletView> {
        let balance = self
            .client
            .offchain_balance()
            .await
            .map_err(|error| anyhow::anyhow!("read the balance: {error}"))?;
        let (ark_address, _) = self
            .client
            .get_offchain_address()
            .await
            .map_err(|error| anyhow::anyhow!("read the Ark address: {error}"))?;
        let boarding_address = self
            .client
            .get_boarding_address()
            .await
            .map_err(|error| anyhow::anyhow!("read the boarding address: {error}"))?;
        Ok(WalletView {
            ark_address: ark_address.encode(),
            boarding_address: boarding_address.to_string(),
            confirmed_sat: balance.confirmed().to_sat(),
            pre_confirmed_sat: balance.pre_confirmed().to_sat(),
            recoverable_sat: balance.recoverable().to_sat(),
        })
    }

    /// Move confirmed boarding outputs, and VTXOs near expiry, into fresh VTXOs in the next batch.
    pub async fn board(&self) -> anyhow::Result<Option<Txid>> {
        let _one_at_a_time = self.sending.lock().await;
        let mut rng = <rand08::rngs::StdRng as rand08::SeedableRng>::from_entropy();
        self.client
            .settle(&mut rng)
            .await
            .map_err(|error| anyhow::anyhow!("board: {error}"))
    }
}

/// The wallet's secret key, created on first start with owner-only permissions.
fn load_or_create_key(path: &Path) -> anyhow::Result<Keypair> {
    let secp = Secp256k1::new();
    if path.exists() {
        let hex_key = std::fs::read_to_string(path)?;
        let secret = SecretKey::from_slice(&hex::decode(hex_key.trim())?)?;
        return Ok(Keypair::from_secret_key(&secp, &secret));
    }
    let secret = SecretKey::new(&mut bitcoin::secp256k1::rand::thread_rng());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    writeln!(file, "{}", hex::encode(secret.secret_bytes()))?;
    Ok(Keypair::from_secret_key(&secp, &secret))
}

/// Chain data for ark-client, from an Esplora server.
///
/// Adapted from `ark-client-sample` in arkade-os/rust-sdk (MIT).
pub struct Esplora {
    client: esplora_client::AsyncClient,
}

impl Esplora {
    fn new(url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            client: esplora_client::Builder::new(url).build_async()?,
        })
    }
}

impl Blockchain for Esplora {
    async fn find_outpoints(&self, address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
        let tip = self.client.get_height().await.map_err(Error::consumer)?;
        let script_pubkey = address.script_pubkey();
        let txs = self
            .client
            .scripthash_txs(&script_pubkey, None)
            .await
            .map_err(Error::consumer)?;
        let spent: HashSet<OutPoint> = txs
            .iter()
            .flat_map(|tx| tx.vin.iter())
            .filter(|input| {
                input
                    .prevout
                    .as_ref()
                    .is_some_and(|prevout| prevout.scriptpubkey == script_pubkey)
            })
            .map(|input| OutPoint::new(input.txid, input.vout))
            .collect();
        Ok(txs
            .iter()
            .flat_map(|tx| {
                tx.vout
                    .iter()
                    .enumerate()
                    .filter(|(_, output)| output.scriptpubkey == script_pubkey)
                    .map(|(vout, output)| {
                        let outpoint = OutPoint::new(tx.txid, vout as u32);
                        let confirmations = tx
                            .status
                            .block_height
                            .and_then(|height| tip.checked_sub(height))
                            .map_or(0, |depth| depth + 1);
                        ExplorerUtxo {
                            outpoint,
                            amount: Amount::from_sat(output.value),
                            confirmation_blocktime: tx.status.block_time,
                            confirmations: u64::from(confirmations),
                            is_spent: spent.contains(&outpoint),
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    async fn find_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
        self.client.get_tx(txid).await.map_err(Error::consumer)
    }

    async fn get_tx_status(&self, txid: &Txid) -> Result<TxStatus, Error> {
        let info = self
            .client
            .get_tx_info(txid)
            .await
            .map_err(Error::consumer)?;
        Ok(TxStatus {
            confirmed_at: info.and_then(|info| info.status.block_time.map(|time| time as i64)),
        })
    }

    async fn get_output_status(&self, txid: &Txid, vout: u32) -> Result<SpendStatus, Error> {
        let status = self
            .client
            .get_output_status(txid, u64::from(vout))
            .await
            .map_err(Error::consumer)?;
        Ok(SpendStatus {
            spend_txid: status.and_then(|status| status.txid),
        })
    }

    async fn broadcast(&self, tx: &Transaction) -> Result<(), Error> {
        self.client.broadcast(tx).await.map_err(Error::consumer)
    }

    async fn get_fee_rate(&self) -> Result<f64, Error> {
        Ok(1.0)
    }

    async fn broadcast_package(&self, txs: &[&Transaction]) -> Result<(), Error> {
        for tx in txs {
            self.broadcast(tx).await?;
        }
        Ok(())
    }
}
