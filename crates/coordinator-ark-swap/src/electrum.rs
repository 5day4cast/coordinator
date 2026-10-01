//! Chain lookups through the self-hosted electrs server's Electrum protocol.
//!
//! electrs is the trusted chain index. Transaction hashes, output scripts and amounts are
//! checked against raw transactions; this adapter does not independently validate the chain.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use ark_client::{Blockchain, Error, SpendStatus, TxStatus};
use ark_core::ExplorerUtxo;
use bitcoin::block::Header;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::{Address, BlockHash, Network, OutPoint, Script, Transaction, Txid};
use electrum_client::{Client, ConfigBuilder, ElectrumApi, GetHistoryRes, ListUnspentRes};
use tokio::sync::Semaphore;

const SOCKET_TIMEOUT_SECS: u8 = 3;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONCURRENT_OPERATIONS: usize = 2;
const MAX_ENTRIES: usize = 10_000;
const MAX_RPC_ROUND_TRIPS: usize = 1_024;
const RPC_BATCH_SIZE: usize = 64;

pub struct Electrum {
    url: Arc<str>,
    network: Network,
    permits: Arc<Semaphore>,
}

impl Electrum {
    pub async fn connect(url: &str, network: Network) -> anyhow::Result<Self> {
        ensure!(
            url.starts_with("tcp://") || url.starts_with("ssl://"),
            "electrum_url must use tcp:// or ssl://"
        );
        let chain = Self {
            url: Arc::from(url),
            network,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_OPERATIONS)),
        };
        chain.with_rpc(|_| Ok(())).await?;
        Ok(chain)
    }

    async fn with_rpc<T, F>(&self, operation: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&mut Request) -> anyhow::Result<T> + Send + 'static,
    {
        let permit = Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| Error::consumer("electrs chain lookups are busy"))?;
        let url = Arc::clone(&self.url);
        let network = self.network;
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let task = tokio::task::spawn_blocking(move || {
            // Cancellation of the async caller cannot admit another blocking operation early.
            let _permit = permit;
            let mut request = Request::connect(&url, network, deadline)?;
            let result = operation(&mut request)?;
            request.check_deadline()?;
            Ok::<_, anyhow::Error>(result)
        });
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task)
            .await
            .map_err(|_| Error::consumer("electrs chain lookup exceeded its deadline"))?
            .map_err(|error| Error::consumer(format!("electrs worker failed: {error}")))?
            .map_err(|error| Error::consumer(format!("electrs chain lookup: {error:#}")))
    }
}

/// One bounded operation and its short-lived connection. A failed socket is discarded; the
/// next operation reconnects without an internal retry queue or retained subscriptions.
struct Request {
    client: Client,
    deadline: Instant,
    calls: usize,
    transactions: HashMap<Txid, Arc<Transaction>>,
    headers: HashMap<usize, Header>,
}

impl Request {
    fn connect(url: &str, network: Network, deadline: Instant) -> anyhow::Result<Self> {
        ensure!(Instant::now() < deadline, "chain lookup deadline elapsed");
        let client = Client::from_config(
            url,
            ConfigBuilder::new()
                .timeout(Some(SOCKET_TIMEOUT_SECS))
                .retry(0)
                .build(),
        )?;
        let mut request = Self {
            client,
            deadline,
            calls: 0,
            transactions: HashMap::new(),
            headers: HashMap::new(),
        };
        ensure!(
            request.header(0)?.block_hash() == genesis_block(network).block_hash(),
            "electrs chain does not match configured Bitcoin network {network}"
        );
        Ok(request)
    }

    fn check_deadline(&self) -> anyhow::Result<()> {
        ensure!(
            Instant::now() < self.deadline,
            "chain lookup deadline elapsed"
        );
        Ok(())
    }

    fn rpc<T>(
        &mut self,
        operation: impl FnOnce(&Client) -> Result<T, electrum_client::Error>,
    ) -> anyhow::Result<T> {
        self.check_deadline()?;
        ensure!(
            self.calls < MAX_RPC_ROUND_TRIPS,
            "chain lookup exceeds RPC limit"
        );
        self.calls += 1;
        let result = operation(&self.client)?;
        self.check_deadline()?;
        Ok(result)
    }

    fn header(&mut self, height: usize) -> anyhow::Result<Header> {
        self.check_deadline()?;
        if let Some(header) = self.headers.get(&height) {
            return Ok(*header);
        }
        let header = self.rpc(|client| client.block_header(height))?;
        self.headers.insert(height, header);
        Ok(header)
    }

    fn tip(&mut self) -> anyhow::Result<(usize, BlockHash)> {
        let tip = self.rpc(|client| client.block_headers_subscribe())?;
        Ok((tip.height, tip.header.block_hash()))
    }

    fn require_same_tip(&mut self, expected: (usize, BlockHash)) -> anyhow::Result<()> {
        ensure!(self.tip()? == expected, "chain tip changed during lookup");
        Ok(())
    }

    fn transaction(&mut self, txid: Txid) -> anyhow::Result<Option<Arc<Transaction>>> {
        self.check_deadline()?;
        if let Some(transaction) = self.transactions.get(&txid) {
            return Ok(Some(Arc::clone(transaction)));
        }
        let transaction = self.rpc(|client| match client.transaction_get(&txid) {
            Ok(transaction) => Ok(Some(transaction)),
            Err(error) if transaction_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        })?;
        let Some(transaction) = transaction else {
            return Ok(None);
        };
        ensure!(
            transaction.compute_txid() == txid,
            "transaction hash mismatch"
        );
        let transaction = Arc::new(transaction);
        self.transactions.insert(txid, Arc::clone(&transaction));
        Ok(Some(transaction))
    }

    /// Fetch the transactions and confirmation headers in bounded batches. One boarding
    /// address can collect many separate deposits; one round trip per deposit would exhaust
    /// the operation's call budget before it could board those outputs.
    fn preload_unspent(
        &mut self,
        entries: &[ListUnspentRes],
        tip: (usize, BlockHash),
    ) -> anyhow::Result<()> {
        let mut transactions = HashSet::new();
        let mut heights = HashSet::new();
        for entry in entries {
            self.check_deadline()?;
            ensure!(entry.height <= tip.0, "unspent output is above chain tip");
            if !self.transactions.contains_key(&entry.tx_hash) {
                transactions.insert(entry.tx_hash);
            }
            if entry.height > 0 && !self.headers.contains_key(&entry.height) {
                heights.insert(u32::try_from(entry.height).context("invalid block height")?);
            }
        }
        let transactions: Vec<_> = transactions.into_iter().collect();
        for txids in transactions.chunks(RPC_BATCH_SIZE) {
            let fetched = self.rpc(|client| client.batch_transaction_get(txids.iter()))?;
            ensure!(
                fetched.len() == txids.len(),
                "transaction batch length mismatch"
            );
            for (expected, transaction) in txids.iter().zip(fetched) {
                self.check_deadline()?;
                ensure!(
                    transaction.compute_txid() == *expected,
                    "transaction hash mismatch"
                );
                self.transactions.insert(*expected, Arc::new(transaction));
            }
        }
        let heights: Vec<_> = heights.into_iter().collect();
        for requested in heights.chunks(RPC_BATCH_SIZE) {
            let fetched =
                self.rpc(|client| client.batch_block_header(requested.iter().copied()))?;
            ensure!(
                fetched.len() == requested.len(),
                "header batch length mismatch"
            );
            // electrum-client matches response IDs to request order before returning headers.
            for (height, header) in requested.iter().zip(fetched) {
                self.check_deadline()?;
                let height = usize::try_from(*height)?;
                if height == tip.0 {
                    ensure!(
                        header.block_hash() == tip.1,
                        "confirmation header differs from chain tip"
                    );
                }
                self.headers.insert(height, header);
            }
        }
        Ok(())
    }

    fn history(&mut self, script: &Script) -> anyhow::Result<Vec<GetHistoryRes>> {
        let history = self.rpc(|client| client.script_get_history(script))?;
        ensure!(
            history.len() <= MAX_ENTRIES,
            "script history exceeds lookup limit"
        );
        let mut seen = HashSet::new();
        for entry in &history {
            ensure!(entry.height >= -1, "invalid transaction height");
            ensure!(seen.insert(entry.tx_hash), "duplicate script history entry");
        }
        Ok(history)
    }

    fn broadcast(&mut self, transaction: &Transaction) -> anyhow::Result<()> {
        let txid = self.rpc(|client| client.transaction_broadcast(transaction))?;
        ensure!(
            txid == transaction.compute_txid(),
            "broadcast transaction hash mismatch"
        );
        Ok(())
    }
}

// romanz/electrs maps Bitcoin Core's missing-transaction response to code 2 while preserving
// its message. Other daemon errors, including a still-syncing txindex, must remain errors.
// See electrs src/electrum.rs (RpcError::DaemonError), and Bitcoin Core
// src/rpc/rawtransaction.cpp (getrawtransaction).
fn transaction_not_found(error: &electrum_client::Error) -> bool {
    let electrum_client::Error::Protocol(response) = error else {
        return false;
    };
    response.get("code").and_then(serde_json::Value::as_i64) == Some(2)
        && matches!(
            response.get("message").and_then(serde_json::Value::as_str),
            Some(
                "No such mempool or blockchain transaction. Use gettransaction for wallet transactions."
                    | "No such mempool transaction. Use -txindex or provide a block hash to enable blockchain transaction queries. Use gettransaction for wallet transactions."
            )
        )
}

impl Blockchain for Electrum {
    async fn find_outpoints(&self, address: &Address) -> Result<Vec<ExplorerUtxo>, Error> {
        let script = address.script_pubkey();
        self.with_rpc(move |request| {
            let tip = request.tip()?;
            let unspent = request.rpc(|client| client.script_list_unspent(&script))?;
            ensure!(
                unspent.len() <= MAX_ENTRIES,
                "unspent outputs exceed lookup limit"
            );
            request.preload_unspent(&unspent, tip)?;
            let mut seen = HashSet::new();
            let mut outputs = Vec::with_capacity(unspent.len());
            for entry in unspent {
                let vout = u32::try_from(entry.tx_pos).context("invalid output index")?;
                let outpoint = OutPoint::new(entry.tx_hash, vout);
                ensure!(seen.insert(outpoint), "duplicate unspent output");
                let transaction = request
                    .transaction(entry.tx_hash)?
                    .context("unspent transaction is missing")?;
                let output = transaction
                    .output
                    .get(entry.tx_pos)
                    .context("unspent output index is missing")?;
                ensure!(
                    output.script_pubkey == script,
                    "unspent output script mismatch"
                );
                ensure!(
                    output.value.to_sat() == entry.value,
                    "unspent output amount mismatch"
                );
                let (confirmations, confirmation_blocktime) = if entry.height == 0 {
                    (0, None)
                } else {
                    let depth = tip
                        .0
                        .checked_sub(entry.height)
                        .context("unspent output is above chain tip")?;
                    let confirmations = u64::try_from(depth)?
                        .checked_add(1)
                        .context("confirmation count overflow")?;
                    (
                        confirmations,
                        Some(u64::from(request.header(entry.height)?.time)),
                    )
                };
                outputs.push(ExplorerUtxo {
                    outpoint,
                    amount: output.value,
                    confirmation_blocktime,
                    confirmations,
                    is_spent: false,
                });
            }
            request.require_same_tip(tip)?;
            Ok(outputs)
        })
        .await
    }

    async fn find_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
        let txid = *txid;
        self.with_rpc(move |request| {
            Ok(request
                .transaction(txid)?
                .map(|transaction| (*transaction).clone()))
        })
        .await
    }

    async fn get_tx_status(&self, txid: &Txid) -> Result<TxStatus, Error> {
        let txid = *txid;
        self.with_rpc(move |request| {
            let Some(transaction) = request.transaction(txid)? else {
                return Ok(TxStatus { confirmed_at: None });
            };
            ensure!(
                transaction.output.len() <= MAX_ENTRIES,
                "transaction outputs exceed lookup limit"
            );
            let tip = request.tip()?;
            let mut scripts = HashSet::new();
            for output in &transaction.output {
                if !scripts.insert(&output.script_pubkey) {
                    continue;
                }
                if let Some(entry) = request
                    .history(&output.script_pubkey)?
                    .iter()
                    .find(|entry| entry.tx_hash == txid)
                {
                    let confirmed_at = if entry.height > 0 {
                        let height = usize::try_from(entry.height)?;
                        ensure!(height <= tip.0, "transaction is above chain tip");
                        Some(i64::from(request.header(height)?.time))
                    } else {
                        None
                    };
                    request.require_same_tip(tip)?;
                    return Ok(TxStatus { confirmed_at });
                }
            }
            anyhow::bail!("transaction is missing from its script history")
        })
        .await
    }

    async fn get_output_status(&self, txid: &Txid, vout: u32) -> Result<SpendStatus, Error> {
        let outpoint = OutPoint::new(*txid, vout);
        self.with_rpc(move |request| {
            let output_index = usize::try_from(vout)?;
            let transaction = request
                .transaction(outpoint.txid)?
                .context("funding transaction is missing")?;
            let output = transaction
                .output
                .get(output_index)
                .context("funding output index is missing")?;
            let tip = request.tip()?;
            for entry in request.history(&output.script_pubkey)? {
                let candidate = request
                    .transaction(entry.tx_hash)?
                    .context("history transaction is missing")?;
                if candidate
                    .input
                    .iter()
                    .any(|input| input.previous_output == outpoint)
                {
                    request.require_same_tip(tip)?;
                    return Ok(SpendStatus {
                        spend_txid: Some(entry.tx_hash),
                    });
                }
            }
            let unspent =
                request.rpc(|client| client.script_list_unspent(&output.script_pubkey))?;
            ensure!(
                unspent.len() <= MAX_ENTRIES,
                "unspent outputs exceed lookup limit"
            );
            ensure!(
                unspent.iter().any(|entry| entry.tx_hash == outpoint.txid
                    && entry.tx_pos == output_index
                    && entry.value == output.value.to_sat()),
                "output is neither unspent nor spent by a verified history transaction"
            );
            request.require_same_tip(tip)?;
            Ok(SpendStatus { spend_txid: None })
        })
        .await
    }

    async fn broadcast(&self, transaction: &Transaction) -> Result<(), Error> {
        let transaction = transaction.clone();
        self.with_rpc(move |request| request.broadcast(&transaction))
            .await
    }

    async fn get_fee_rate(&self) -> Result<f64, Error> {
        self.with_rpc(|request| {
            let btc_per_kb = request.rpc(|client| client.estimate_fee(1))?;
            // Electrum reports BTC/kB; ark-client expects sat/vB.
            let sat_per_vb = btc_per_kb * 100_000.0;
            ensure!(
                sat_per_vb.is_finite() && sat_per_vb > 0.0,
                "electrs fee estimate is unavailable or invalid"
            );
            Ok(sat_per_vb)
        })
        .await
    }

    async fn broadcast_package(&self, transactions: &[&Transaction]) -> Result<(), Error> {
        if transactions.len() >= MAX_RPC_ROUND_TRIPS {
            return Err(Error::consumer(
                "transaction package exceeds broadcast limit",
            ));
        }
        let transactions: Vec<Transaction> = transactions
            .iter()
            .map(|transaction| (**transaction).clone())
            .collect();
        self.with_rpc(move |request| {
            // Preserve parent-before-child submission without requiring an electrs extension.
            for transaction in transactions {
                request.broadcast(&transaction)?;
            }
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
#[path = "electrum_tests.rs"]
mod tests;
