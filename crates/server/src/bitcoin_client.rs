use crate::{get_key, BitcoinSettings};
use anyhow::anyhow;
use async_trait::async_trait;
use bdk_esplora::{
    esplora_client::{r#async::DefaultSleeper, AsyncClient, Builder},
    EsploraAsyncExt,
};
use bdk_wallet::{
    bitcoin::{
        address::NetworkChecked,
        bip32::{ChildNumber, Xpriv},
        secp256k1::SecretKey as BdkSecretKey,
        Address, Amount, Network, NetworkKind, Psbt, Transaction, Txid,
    },
    coin_selection::DefaultCoinSelectionAlgorithm,
    descriptor::calc_checksum,
    file_store::Store,
    AddressInfo, Balance, ChangeSet, KeychainKind, LocalOutput, PersistedWallet, SignOptions,
    TxBuilder, Wallet,
};
use dlctix::{bitcoin::bip32::ChainCode, musig2::secp256k1::SecretKey as DlcSecretKey};
use log::{debug, error, info};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::Path,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::{sync::RwLock, time::sleep};
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait Bitcoin: Send + Sync {
    async fn get_spendable_utxo(&self, amount_sats: u64) -> Result<LocalOutput, anyhow::Error>;
    async fn get_current_height(&self) -> Result<u32, anyhow::Error>;
    async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error>;
    async fn get_tx_confirmation_height(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error>;
    async fn broadcast(&self, transaction: &Transaction) -> Result<(), anyhow::Error>;
    async fn get_next_address(&self) -> Result<AddressInfo, anyhow::Error>;
    async fn get_derived_private_key(&self) -> Result<DlcSecretKey, anyhow::Error>;
    async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error>;
    async fn sign_psbt(
        &self,
        psbt: &mut Psbt,
        sign_options: SignOptions,
    ) -> Result<bool, anyhow::Error>;
}

pub struct BitcoinClient {
    pub network: Network,
    seed_path: SecretString,
    wallet: RwLock<PersistedWallet<Store<ChangeSet>>>,
    client: AsyncClient,
    wallet_store: RwLock<Store<bdk_wallet::ChangeSet>>,
}

const DB_MAGIC: &str = "coordinator_wallet_store";

#[derive(Deserialize)]
pub struct SendOptions {
    pub address_to: String,
    pub address_from: Option<String>,
    pub amount: Option<u64>,
    pub max_fee: Option<u64>,
}

impl SendOptions {
    pub fn get_destination_address(
        &self,
        network: Network,
    ) -> Result<Address<NetworkChecked>, anyhow::Error> {
        Address::from_str(&self.address_to)?
            .require_network(network)
            .map_err(|e| anyhow!("Invalid destination address: {}", e))
    }

    pub fn find_source_utxos<'a>(
        &self,
        wallet: &'a bdk_wallet::Wallet,
    ) -> Result<Vec<LocalOutput>, anyhow::Error> {
        match &self.address_from {
            Some(addr) => {
                let source_addr = Address::from_str(addr)?.require_network(wallet.network())?;

                let utxos = wallet
                    .list_unspent()
                    .filter(|utxo| {
                        Address::from_script(&utxo.txout.script_pubkey, wallet.network())
                            .ok()
                            .map_or(false, |addr| {
                                addr == source_addr
                                    && !utxo.is_spent
                                    && utxo.chain_position.is_confirmed()
                            })
                    })
                    .collect::<Vec<_>>();

                if utxos.is_empty() {
                    Err(anyhow!("No spendable UTXOs found for address: {}", addr))
                } else {
                    Ok(utxos)
                }
            }
            None => Ok(vec![]),
        }
    }

    pub fn configure_tx_builder(
        &self,
        builder: &mut TxBuilder<DefaultCoinSelectionAlgorithm>,
        dest_addr: Address<NetworkChecked>,
    ) -> Result<(), anyhow::Error> {
        if let Some(amount_sats) = self.amount {
            builder.add_recipient(dest_addr.script_pubkey(), Amount::from_sat(amount_sats));
        } else {
            builder.drain_wallet().drain_to(dest_addr.script_pubkey());
        }
        Ok(())
    }

    pub fn validate_fee(&self, psbt: &Psbt) -> Result<(), anyhow::Error> {
        if let Some(max_fee_sats) = self.max_fee {
            let fee = psbt.fee().unwrap_or_default();
            if fee > Amount::from_sat(max_fee_sats) {
                return Err(anyhow!(
                    "Transaction fee {} sats exceeds maximum specified fee {} sats",
                    fee,
                    max_fee_sats
                ));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Bitcoin for BitcoinClient {
    async fn get_derived_private_key(&self) -> Result<DlcSecretKey, anyhow::Error> {
        // Get the secret key directly from the path
        let secret_key: BdkSecretKey = get_key(&self.seed_path.expose_secret())?;

        // Convert to DLC secret key format
        let dlc_key = DlcSecretKey::from_slice(&secret_key.secret_bytes())
            .map_err(|e| anyhow!("Invalid secret key bytes: {}", e))?;

        Ok(dlc_key)
    }

    async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error> {
        let Some(transaction) = self.client.get_tx(txid).await? else {
            return Err(anyhow!("Transaction not found: {}", txid));
        };
        Ok(transaction)
    }

    async fn sign_psbt(
        &self,
        psbt: &mut Psbt,
        sign_options: SignOptions,
    ) -> Result<bool, anyhow::Error> {
        let wallet = self.wallet.write().await;
        let finalized = wallet.sign(psbt, sign_options)?;
        Ok(finalized)
    }

    async fn get_tx_confirmation_height(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error> {
        let tx_status = self.client.get_tx_status(txid).await?;
        Ok(tx_status.block_height)
    }

    async fn get_spendable_utxo(&self, amount_sats: u64) -> Result<LocalOutput, anyhow::Error> {
        let amount = Amount::from_sat(amount_sats);
        let current_height = self.get_current_height().await?;
        let utxo = self.wallet.read().await.list_unspent().find(|utxo| {
            // If this is a coinbase-derived UTXO, check maturity
            if let Some(conf_height) = utxo.chain_position.confirmation_height_upper_bound() {
                if utxo.outpoint.vout == 0 {
                    debug!("Coinbase UTXO found at height {}", conf_height);
                    if current_height < conf_height + 100 {
                        debug!(
                            "Coinbase UTXO not mature yet. Current height: {}, Need: {}",
                            current_height,
                            conf_height + 100
                        );
                        return false;
                    }
                }
            }

            // Check if UTXO is confirmed, not spent, bigger than amount, and not locked
            utxo.txout.value >= amount && !utxo.is_spent && utxo.chain_position.is_confirmed()
        });

        if let Some(utxo) = utxo {
            Ok(utxo)
        } else {
            Err(anyhow!("No utxos with the request amount available"))
        }
    }

    async fn get_next_address(&self) -> Result<AddressInfo, anyhow::Error> {
        let (mut wallet, mut store) = tokio::join!(self.wallet.write(), self.wallet_store.write());

        let address = wallet.next_unused_address(KeychainKind::External);
        //Might need to apply update here, not sure
        wallet.persist(&mut store)?;

        Ok(address)
    }

    async fn get_current_height(&self) -> Result<u32, anyhow::Error> {
        self.client
            .get_height()
            .await
            .map_err(|e| anyhow!("Failed to get block height: {}", e))
    }

    /// Get an object where the key is the confirmation target (in number of blocks) and the value is the estimated feerate (in sat/vB).
    /// The available confirmation targets are 1-25, 144, 504 and 1008 blocks.
    /// For example: { "1": 87.882, "2": 87.882, "3": 87.882, "4": 87.882, "5": 81.129, "6": 68.285, ..., "144": 1.027, "504": 1.027, "1008": 1.027 }
    async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error> {
        let fee_estimates = self.client.get_fee_estimates().await?;
        Ok(fee_estimates)
    }

    async fn broadcast(&self, transaction: &Transaction) -> Result<(), anyhow::Error> {
        //TODO: add child-pays-for-parent if fees are too low

        self.client
            .broadcast(transaction)
            .await
            .map_err(|e| anyhow!("error broadcasting: {}", e))
    }
}

impl BitcoinClient {
    pub async fn new(settings: &BitcoinSettings) -> Result<BitcoinClient, anyhow::Error> {
        let path = Path::new(&settings.storage_file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut db = Store::<bdk_wallet::ChangeSet>::open_or_create_new(
            DB_MAGIC.as_bytes(),
            settings.storage_file.to_owned(),
        )
        .map_err(|e| anyhow!("Failed to open or create bitcoin db: {}", e))?;
        info!("Bitcoin db configured");

        let path = Path::new(&settings.seed_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Set up wallet with EC private key
        let (external_desc, internal_desc) =
            setup_wallet_descriptors(&settings.seed_path, settings.network.into())
                .map_err(|e| anyhow!("Failed to load bitcoin private key: {}", e))?;

        let wallet_opt = Wallet::load()
            .descriptor(KeychainKind::External, Some(external_desc.clone()))
            .descriptor(KeychainKind::Internal, Some(internal_desc.clone()))
            .extract_keys()
            .check_network(settings.network)
            .load_wallet(&mut db)
            .map_err(|e| anyhow!("Failed to load bitcoin wallet store: {}", e))?;

        let mut wallet = match wallet_opt {
            Some(wallet) => wallet,
            None => Wallet::create(external_desc, internal_desc)
                .network(settings.network)
                .create_wallet(&mut db)
                .map_err(|e| anyhow!("Failed to create bitcoin wallet from keys: {}", e))?,
        };
        let esplora_api = if settings.network == Network::Bitcoin {
            format!("{}/api", settings.network)
        } else {
            format!("{}/{}/api", settings.esplora_url, settings.network)
        };
        let client = Builder::new(&esplora_api)
            .build_async_with_sleeper::<DefaultSleeper>()
            .map_err(|e| anyhow!("Failed to create esplora client: {}", e))?;

        // Perform initial full scan
        info!("Starting initial full scan...");
        let request = wallet
            .start_full_scan()
            .inspect({
                let mut once = BTreeSet::<KeychainKind>::new();
                move |keychain, spk_i, _| {
                    if once.insert(keychain) {
                        info!("Scanning keychain [{:?}]", keychain);
                    }
                    info!(" {:<3}", spk_i);
                }
            })
            .build();

        let update = client
            .full_scan(request, 20, 5) // stop_gap: 20, parallel_requests: 5
            .await
            .map_err(|e| anyhow!("Failed to complete full blockchain scan: {}", e))?;

        wallet
            .apply_update(update)
            .map_err(|e| anyhow!("Failed to load full blockchain scan into wallet: {}", e))?;

        wallet.persist(&mut db).map_err(|e| {
            anyhow!(
                "Failed to load persist full blockchain scan into wallet: {}",
                e
            )
        })?;

        info!("Initial scan completed");

        Ok(BitcoinClient {
            network: settings.network,
            wallet: RwLock::new(wallet),
            seed_path: SecretString::from(settings.seed_path.clone()),
            client,
            wallet_store: RwLock::new(db),
        })
    }

    pub async fn get_balance(&self) -> Result<Balance, anyhow::Error> {
        let balance = self.wallet.read().await.balance();
        Ok(balance)
    }

    pub async fn get_outputs(&self) -> Result<Vec<LocalOutput>, anyhow::Error> {
        let outputs = self.wallet.read().await.list_output().collect();
        Ok(outputs)
    }

    pub async fn send_to_address(&self, send_options: SendOptions) -> Result<Txid, anyhow::Error> {
        // Get wallet read lock first to check addresses and UTXOs
        let wallet = self.wallet.read().await;

        // Validate destination address and find source UTXOs
        let dest_addr = send_options.get_destination_address(wallet.network())?;
        let source_utxos = send_options.find_source_utxos(&wallet)?;

        // Switch to write lock for transaction building
        drop(wallet);
        let mut wallet = self.wallet.write().await;

        let mut builder = wallet.build_tx();

        for utxo in source_utxos {
            builder.add_utxo(utxo.outpoint)?;
        }

        send_options.configure_tx_builder(&mut builder, dest_addr)?;

        let psbt = builder.finish()?;

        send_options.validate_fee(&psbt)?;

        // Sign and finalize
        let finalized = wallet.sign(&mut psbt.clone(), SignOptions::default())?;
        if !finalized {
            return Err(anyhow!("Failed to sign transaction"));
        }

        // Extract and broadcast
        let tx = psbt.extract_tx()?;
        self.client.broadcast(&tx).await?;

        Ok(tx.compute_txid())
    }

    pub async fn sync(&self) -> Result<(), anyhow::Error> {
        let wallet = self.wallet.read().await;
        let request = wallet
            .start_sync_with_revealed_spks()
            .inspect(|item, progress| {
                let pc = (100 * progress.consumed()) as f32 / progress.total() as f32;
                info!("[ SCANNING {:03.0}% ] {}", pc, item);
            })
            .build();

        let update = self.client.sync(request, 5).await?; // parallel_requests: 5
        drop(wallet);

        let (mut wallet, mut store) = tokio::join!(self.wallet.write(), self.wallet_store.write());
        wallet.apply_update(update)?;
        wallet.persist(&mut store)?;

        info!("Sync completed successfully");

        Ok(())
    }
}

fn setup_wallet_descriptors(
    seed_path: &str,
    network: NetworkKind,
) -> Result<(String, String), anyhow::Error> {
    let xpriv = derive_wallet_key(seed_path, network)?;

    // Use standard tr descriptor
    let external_base = format!("tr({}/0/*)", xpriv);
    let internal_base = format!("tr({}/1/*)", xpriv);

    let external_checksum = calc_checksum(&external_base)?;
    let internal_checksum = calc_checksum(&internal_base)?;

    let external_descriptor = format!("{}#{}", external_base, external_checksum);
    let internal_descriptor = format!("{}#{}", internal_base, internal_checksum);

    Ok((external_descriptor, internal_descriptor))
}

fn derive_wallet_key(seed_path: &str, network: NetworkKind) -> Result<Xpriv, anyhow::Error> {
    // Get the secret key from the provided path
    let secret_key: BdkSecretKey = get_key(seed_path)?;
    let chain_code = ChainCode::from(secret_key.secret_bytes());

    // Create extended private key with network support
    let xpriv = Xpriv {
        network,
        depth: 0,
        parent_fingerprint: Default::default(),
        chain_code: chain_code.into(),
        child_number: ChildNumber::from_normal_idx(0)?,
        private_key: secret_key,
    };

    Ok(xpriv)
}

pub struct BitcoinSyncWatcher {
    bitcoin: Arc<BitcoinClient>,
    cancel_token: CancellationToken,
    sync_interval: Duration,
}

impl BitcoinSyncWatcher {
    pub fn new(
        bitcoin: Arc<BitcoinClient>,
        cancel_token: CancellationToken,
        sync_interval: Duration,
    ) -> Self {
        Self {
            bitcoin,
            cancel_token,
            sync_interval,
        }
    }

    pub async fn watch(&self) -> Result<(), anyhow::Error> {
        info!("Starting Bitcoin sync watcher");

        loop {
            if self.cancel_token.is_cancelled() {
                info!("Bitcoin sync watcher received cancellation");
                break;
            }

            match self.bitcoin.sync().await {
                Ok(_) => {
                    info!("Bitcoin wallet sync completed successfully");
                }
                Err(e) => {
                    error!("Bitcoin wallet sync error: {}", e);
                }
            }

            tokio::select! {
                _ = sleep(self.sync_interval) => continue,
                _ = self.cancel_token.cancelled() => {
                    info!("Bitcoin sync watcher cancelled during sleep");
                    break;
                }
            }
        }

        Ok(())
    }
}
