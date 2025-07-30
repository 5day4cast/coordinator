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
        ecdsa,
        hashes::{sha256, Hash},
        psbt::Input,
        secp256k1::{Message, Secp256k1, SecretKey as BdkSecretKey},
        sighash::{EcdsaSighashType, SighashCache},
        Address, Amount, Network, NetworkKind, OutPoint, Psbt, PublicKey, ScriptBuf, Transaction,
        Txid, Weight, Witness,
    },
    coin_selection::DefaultCoinSelectionAlgorithm,
    descriptor::calc_checksum,
    file_store::Store,
    AddressInfo, Balance, ChangeSet, KeychainKind, LocalOutput, PersistedWallet, SignOptions,
    TxBuilder, Wallet,
};
use dlctix::{
    bitcoin::{bip32::ChainCode, FeeRate},
    secp::Scalar,
};
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

// Needs to be over half of the last 10 blocks block time passed
// the expiry time for a block to be considered final and able to broadcast
pub const REQUIRED_CONFIRMATIONS_FOR_TIME: usize = 6;

#[async_trait]
pub trait Bitcoin: Send + Sync {
    fn get_network(&self) -> Network;
    async fn sign_psbt_with_escrow_support(
        &self,
        psbt: &mut Psbt,
        options: SignOptions,
    ) -> Result<bool, anyhow::Error>;
    async fn finalize_psbt_with_escrow_support(
        &self,
        psbt: &mut Psbt,
    ) -> Result<bool, anyhow::Error>;
    async fn build_psbt(
        &self,
        script_pubkey: ScriptBuf,
        amount: Amount,
        fee_rate: FeeRate,
        selected_utxos: Vec<OutPoint>,
        foreign_utxos: Vec<ForeignUtxo>,
    ) -> Result<Psbt, anyhow::Error>;
    async fn get_spendable_utxo(&self, amount_sats: u64) -> Result<LocalOutput, anyhow::Error>;
    async fn get_current_height(&self) -> Result<u32, anyhow::Error>;
    async fn get_confirmed_blockchain_time(&self, blocks: usize) -> Result<u64, anyhow::Error>;
    async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error>;
    async fn get_tx_confirmation_height(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error>;
    async fn broadcast(&self, transaction: &Transaction) -> Result<(), anyhow::Error>;
    async fn get_next_address(&self) -> Result<AddressInfo, anyhow::Error>;
    async fn get_public_key(&self) -> Result<bdk_wallet::bitcoin::PublicKey, anyhow::Error>;
    async fn get_derived_private_key(&self) -> Result<Scalar, anyhow::Error>;
    async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error>;
    async fn sign_psbt(
        &self,
        psbt: &mut Psbt,
        sign_options: SignOptions,
    ) -> Result<bool, anyhow::Error>;
    async fn list_utxos(&self) -> Vec<LocalOutput>;
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

#[derive(Debug, Clone)]
pub struct ForeignUtxo {
    pub outpoint: OutPoint,
    pub psbt: Input,
    pub satisfaction_weight: Weight,
}

#[async_trait]
impl Bitcoin for BitcoinClient {
    fn get_network(&self) -> Network {
        self.network
    }

    async fn get_public_key(&self) -> Result<bdk_wallet::bitcoin::PublicKey, anyhow::Error> {
        let secret_key = get_key::<BdkSecretKey>(&self.seed_path.expose_secret())?;
        let private_key = bdk_wallet::bitcoin::PrivateKey {
            compressed: true,
            network: NetworkKind::from(self.network),
            inner: secret_key,
        };
        let secp = bdk_wallet::bitcoin::secp256k1::Secp256k1::new();

        let public_key = bdk_wallet::bitcoin::PublicKey::from_private_key(&secp, &private_key);
        Ok(public_key)
    }

    async fn get_derived_private_key(&self) -> Result<Scalar, anyhow::Error> {
        let secret_key = get_key::<BdkSecretKey>(&self.seed_path.expose_secret())?;
        let dlc_key = Scalar::from_hex(&hex::encode(secret_key.secret_bytes()))
            .map_err(|e| anyhow!("Failed to convert private key to scalar: {}", e))?;

        Ok(dlc_key)
    }

    async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error> {
        let Some(transaction) = self.client.get_tx(txid).await? else {
            return Err(anyhow!("Transaction not found: {}", txid));
        };
        Ok(transaction)
    }

    async fn sign_psbt_with_escrow_support(
        &self,
        psbt: &mut Psbt,
        options: SignOptions,
    ) -> Result<bool, anyhow::Error> {
        // First, let BDK sign what it can (non-escrow inputs)
        let wallet = self.wallet.write().await;
        let bdk_finalized = wallet.sign(psbt, options.clone())?;

        debug!("BDK signing complete. Finalized: {}", bdk_finalized);

        if bdk_finalized {
            return Ok(true);
        }

        // Now manually sign any escrow inputs that BDK couldn't handle
        let escrow_signed = self.sign_escrow_inputs(psbt).await?;

        if escrow_signed > 0 {
            debug!("Manually signed {} escrow inputs", escrow_signed);
        }

        // Return whether the PSBT is fully signed (but not finalized)
        Ok(self.is_psbt_fully_signed(psbt))
    }

    async fn finalize_psbt_with_escrow_support(
        &self,
        psbt: &mut Psbt,
    ) -> Result<bool, anyhow::Error> {
        let wallet = self.wallet.write().await;
        let finalized = wallet.finalize_psbt(
            psbt,
            SignOptions {
                trust_witness_utxo: true,
                allow_all_sighashes: true,
                try_finalize: true,
                sign_with_tap_internal_key: true,
                allow_grinding: true,
                ..Default::default()
            },
        )?;

        if finalized {
            return Ok(true);
        }

        debug!("BDK finalization incomplete, finalizing escrow inputs manually");

        // Manually finalize each escrow input
        for (index, input) in psbt.inputs.iter_mut().enumerate() {
            // Skip already finalized inputs
            if input.final_script_witness.is_some() {
                debug!("Input {} is already finalized", index);
                continue;
            }

            // Only process escrow inputs (those with witness_script)
            if let Some(witness_script) = input.witness_script.clone() {
                debug!("Finalizing escrow input {}", index);

                // For escrow miniscript: wsh(or_d(multi(2,A,B),and_v(v:pk(B),and_v(v:sha256(H),older(144)))))
                // We're using the 2-of-2 multisig path, so we need:
                // - Empty byte vector (for OP_FALSE to select the first branch)
                // - Signature from first key in the multisig
                // - Signature from second key in the multisig
                // - The witness script itself

                let mut witness = Witness::new();

                // Add empty byte vector for OP_FALSE (selects the multisig branch in or_d)
                witness.push(&[]);

                // Extract public keys from the witness script to get the correct order
                // The multisig format is: OP_PUSHNUM_2 <pubkey1> <pubkey2> OP_PUSHNUM_2 OP_CHECKMULTISIG
                let script_bytes = witness_script.as_bytes();
                let mut pubkeys_in_script = Vec::new();

                // Find 33-byte pubkeys in the script (they start after OP_PUSHBYTES_33 = 0x21)
                let mut i = 0;
                while i < script_bytes.len() {
                    if script_bytes[i] == 0x21 && i + 33 < script_bytes.len() {
                        if let Ok(pk) = PublicKey::from_slice(&script_bytes[i + 1..i + 34]) {
                            pubkeys_in_script.push(pk);
                            if pubkeys_in_script.len() == 2 {
                                break; // We found both pubkeys
                            }
                        }
                        i += 34;
                    } else {
                        i += 1;
                    }
                }

                if pubkeys_in_script.len() != 2 {
                    return Err(anyhow!(
                        "Failed to extract 2 public keys from witness script for input {}",
                        index
                    ));
                }

                debug!(
                    "Multisig pubkey order in script: {} then {}",
                    pubkeys_in_script[0], pubkeys_in_script[1]
                );

                // Add signatures in the order they appear in the script
                for pubkey in &pubkeys_in_script {
                    if let Some(sig) = input.partial_sigs.get(pubkey) {
                        witness.push_ecdsa_signature(sig);
                    } else {
                        return Err(anyhow!(
                            "Missing signature for pubkey {} in input {}",
                            pubkey,
                            index
                        ));
                    }
                }

                // Add the witness script
                witness.push(witness_script.as_bytes());

                // Set the final witness
                input.final_script_witness = Some(witness);

                // Clear the partial signatures and witness script as they're now in the final witness
                input.partial_sigs.clear();
                input.witness_script = None;

                debug!(
                    "Finalized escrow input {} with {} witness elements",
                    index,
                    input.final_script_witness.as_ref().unwrap().len()
                );
            } else if input.tap_internal_key.is_some() {
                // This is a taproot input, let BDK handle it
                debug!(
                    "Input {} is taproot, should have been finalized by BDK",
                    index
                );
            }
        }

        // Check if all inputs are now finalized
        let all_finalized = psbt.inputs.iter().enumerate().all(|(idx, input)| {
            let finalized =
                input.final_script_witness.is_some() || input.final_script_sig.is_some();
            if !finalized {
                debug!("Input {} is still not finalized", idx);
            }
            finalized
        });

        Ok(all_finalized)
    }

    async fn build_psbt(
        &self,
        script_pubkey: ScriptBuf,
        amount: Amount,
        fee_rate: FeeRate,
        selected_utxos: Vec<OutPoint>,
        foreign_utxos: Vec<ForeignUtxo>,
    ) -> Result<Psbt, anyhow::Error> {
        self.print_balance_info().await?;

        let mut wallet = self.wallet.write().await;
        let mut tx_builder = wallet.build_tx();

        for utxo in selected_utxos {
            tx_builder
                .add_utxo(utxo)
                .map_err(|e| anyhow!("Failed to add utxo: {}", e))?;
        }

        for foreign_utxo in foreign_utxos {
            tx_builder
                .add_foreign_utxo(
                    foreign_utxo.outpoint,
                    foreign_utxo.psbt,
                    foreign_utxo.satisfaction_weight,
                )
                .map_err(|e| anyhow!("Failed to add foreign utxo: {}", e))?;
        }

        tx_builder
            .add_recipient(script_pubkey, amount)
            .fee_rate(fee_rate);

        // This ensures the txid won't change after signing
        tx_builder.only_witness_utxo();

        let psbt = tx_builder.finish()?;

        Ok(psbt)
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
        debug!("Transaction status: {:?}", tx_status);
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
        wallet.persist(&mut store)?;

        Ok(address)
    }

    async fn get_current_height(&self) -> Result<u32, anyhow::Error> {
        self.client
            .get_height()
            .await
            .map_err(|e| anyhow!("Failed to get block height: {}", e))
    }

    async fn get_confirmed_blockchain_time(&self, blocks: usize) -> Result<u64, anyhow::Error> {
        let mut last_blocks = self
            .client
            .get_blocks(None)
            .await
            .map_err(|e| anyhow!("Failed to get block height: {}", e))?;
        last_blocks.sort_by_key(|block| std::cmp::Reverse(block.time.height));

        if last_blocks.is_empty() {
            return Err(anyhow!("No blocks available to calculate median time"));
        }

        let lastest_time = last_blocks
            .get(blocks - 1)
            .map(|val| val.time.timestamp)
            .unwrap_or_default();

        Ok(lastest_time)
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

    async fn list_utxos(&self) -> Vec<LocalOutput> {
        let wallet = self.wallet.read().await;
        wallet.list_unspent().collect()
    }
}

impl BitcoinClient {
    pub async fn new(settings: &BitcoinSettings) -> Result<BitcoinClient, anyhow::Error> {
        info!("Creating Bitcoin client with settings:");
        info!("  Storage file: {}", settings.storage_file);
        info!("  Seed path: {}", settings.seed_path);
        info!("  Network: {}", settings.network);
        info!("  Esplora URL: {}", settings.esplora_url);

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
        info!("Loaded wallet: {}", wallet_opt.is_some());

        let mut wallet = match wallet_opt {
            Some(wallet) => wallet,
            None => Wallet::create(external_desc, internal_desc)
                .network(settings.network)
                .create_wallet(&mut db)
                .map_err(|e| anyhow!("Failed to create bitcoin wallet from keys: {}", e))?,
        };
        info!(
            "Wallet ready. Has {} UTXOs",
            wallet.list_unspent().collect::<Vec<_>>().len()
        );

        let esplora_api =
            if settings.network == Network::Regtest || settings.network == Network::Testnet {
                format!("{}/{}/api", settings.esplora_url, settings.network)
            } else {
                // For mutinynet and mainnet network is not needed in the path
                format!("{}/api", settings.esplora_url)
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

        let balance = wallet.balance();
        info!("Wallet balance after scan: {} sats", balance.total());
        info!("Confirmed balance: {} sats", balance.confirmed);
        info!("Unconfirmed balance: {} sats", balance.untrusted_pending);

        Ok(BitcoinClient {
            network: settings.network,
            wallet: RwLock::new(wallet),
            seed_path: SecretString::from(settings.seed_path.clone()),
            client,
            wallet_store: RwLock::new(db),
        })
    }

    pub async fn print_balance_info(&self) -> Result<(), anyhow::Error> {
        let balance = self.wallet.read().await.balance();
        info!("Wallet balance: {} sats", balance.total());

        let all_utxos = self.list_utxos().await;
        info!("Available UTXOs: {}", all_utxos.len());

        for utxo in &all_utxos {
            info!(
                "UTXO: {} - Amount: {} sats, Keychain: {:?}",
                utxo.outpoint, utxo.txout.value, utxo.keychain
            );
        }

        Ok(())
    }

    pub async fn get_balance(&self) -> Result<Balance, anyhow::Error> {
        let balance = self.wallet.read().await.balance();
        Ok(balance)
    }

    pub async fn get_outputs(&self) -> Result<Vec<LocalOutput>, anyhow::Error> {
        let outputs = self.wallet.read().await.list_output().collect();
        Ok(outputs)
    }

    pub async fn send_to_address(
        &self,
        send_options: SendOptions,
        selected_utxos: Vec<OutPoint>,
    ) -> Result<Txid, anyhow::Error> {
        // Get wallet read lock first to check addresses and UTXOs
        let wallet = self.wallet.read().await;

        // Validate destination address and find source UTXOs
        let dest_addr = send_options.get_destination_address(wallet.network())?;
        let source_utxos = if selected_utxos.is_empty() {
            send_options.find_source_utxos(&wallet)?
        } else {
            // Convert OutPoint to LocalOutput
            let mut utxos = Vec::new();
            for outpoint in selected_utxos {
                if let Some(utxo) = wallet.get_utxo(outpoint) {
                    utxos.push(utxo);
                } else {
                    return Err(anyhow::anyhow!("UTXO not found: {}", outpoint));
                }
            }
            utxos
        };

        // Switch to write lock for transaction building
        drop(wallet);
        let mut wallet = self.wallet.write().await;

        let mut builder = wallet.build_tx();

        for utxo in source_utxos {
            builder.add_utxo(utxo.outpoint)?;
        }

        send_options.configure_tx_builder(&mut builder, dest_addr)?;

        let mut psbt = builder.finish()?;

        send_options.validate_fee(&psbt)?;

        // Sign and finalize
        info!("PSBT before signing: {}", psbt.to_string());
        let finalized = wallet.sign(&mut psbt, SignOptions::default())?;
        info!("PSBT after signing: {}", psbt.to_string());
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

    async fn sign_escrow_inputs(&self, psbt: &mut Psbt) -> Result<usize, anyhow::Error> {
        // Load the private key from the seed file
        let secret_key = get_key::<BdkSecretKey>(&self.seed_path.expose_secret())?;
        let coordinator_privkey = bdk_wallet::bitcoin::PrivateKey {
            compressed: true,
            network: NetworkKind::from(self.network),
            inner: secret_key,
        };

        // Verify network
        if coordinator_privkey.network != self.network.into() {
            return Err(anyhow!(
                "Private key network {:?} doesn't match wallet network {}",
                coordinator_privkey.network,
                self.network
            ));
        }

        let secp = Secp256k1::new();
        let coordinator_pubkey = PublicKey::from_private_key(&secp, &coordinator_privkey);

        debug!(
            "Checking for escrow inputs to sign with pubkey: {}",
            coordinator_pubkey
        );

        let tx = psbt.unsigned_tx.clone();
        let mut escrow_inputs_signed = 0;

        for (idx, input) in psbt.inputs.iter_mut().enumerate() {
            // Skip if already has our signature
            if input.partial_sigs.contains_key(&coordinator_pubkey) {
                debug!("Input {} already has coordinator signature", idx);
                continue;
            }

            // Check if this is an escrow input by looking for witness_script
            let witness_script = match &input.witness_script {
                Some(ws) => ws,
                None => {
                    debug!("Input {} is not an escrow input (no witness_script)", idx);
                    continue;
                }
            };

            // Verify this witness script contains our pubkey
            let script_bytes = witness_script.as_bytes();
            let pubkey_bytes = coordinator_pubkey.to_bytes();

            if !script_bytes
                .windows(pubkey_bytes.len())
                .any(|window| window == pubkey_bytes)
            {
                debug!(
                    "Input {} witness script doesn't contain coordinator pubkey",
                    idx
                );
                continue;
            }

            // Get witness UTXO for signing
            let witness_utxo = input
                .witness_utxo
                .as_ref()
                .ok_or_else(|| anyhow!("Missing witness_utxo for escrow input {}", idx))?;

            debug!(
                "Signing escrow input {} (value: {} sats)",
                idx, witness_utxo.value
            );
            let script_bytes = witness_utxo.script_pubkey.as_bytes();
            if script_bytes.len() >= 34 && script_bytes[0] == 0x00 && script_bytes[1] == 0x20 {
                // This is a P2WSH script
                let expected_hash = &script_bytes[2..34];

                // Calculate the hash of the witness script
                let actual_hash = sha256::Hash::hash(&witness_script.as_bytes()).to_byte_array();

                debug!("Expected witness hash: {}", hex::encode(expected_hash));
                debug!("Actual witness hash: {}", hex::encode(actual_hash));

                if expected_hash != actual_hash.as_slice() {
                    debug!("HASH MISMATCH for input {}", idx);
                } else {
                    debug!("Hash verification OK for input {}", idx);
                }
            }

            // Create sighash
            let mut cache = SighashCache::new(&tx);
            let sighash = cache.p2wsh_signature_hash(
                idx,
                witness_script,
                witness_utxo.value,
                EcdsaSighashType::All,
            )?;

            // Sign
            let message = Message::from_digest(sighash.to_byte_array());
            let sig = secp.sign_ecdsa(&message, &coordinator_privkey.inner);

            // Add signature to PSBT
            input.partial_sigs.insert(
                coordinator_pubkey,
                ecdsa::Signature {
                    signature: sig,
                    sighash_type: EcdsaSighashType::All,
                },
            );

            escrow_inputs_signed += 1;
            debug!("Added coordinator signature to escrow input {}", idx);
        }

        Ok(escrow_inputs_signed)
    }

    fn is_psbt_fully_signed(&self, psbt: &Psbt) -> bool {
        for (i, input) in psbt.inputs.iter().enumerate() {
            // Skip already finalized inputs
            if input.final_script_witness.is_some() {
                continue;
            }

            // Check escrow inputs
            if let Some(_) = &input.witness_script {
                // For 2-of-2 multisig escrow, we need exactly 2 signatures
                if input.partial_sigs.len() < 2 {
                    debug!(
                        "Input {} needs more signatures ({}/2)",
                        i,
                        input.partial_sigs.len()
                    );
                    return false;
                }
            } else if input.partial_sigs.is_empty() {
                // Non-escrow inputs should have at least one signature
                debug!("Input {} has no signatures", i);
                return false;
            }
        }

        true
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
