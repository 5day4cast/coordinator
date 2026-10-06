//! The service's own Ark wallet: the liquidity that pays escrows.
//!
//! Top it up by sending VTXOs to its Ark address, for example from `mutinynet.arkade.money`.
//! Or send on-chain coins to its boarding address and call `POST /v1/wallet/board`. Until a batch
//! boards them, confirmed coins there show in the view as `boarding_sat`, not as spendable.
//!
//! Its coins are read from the indexer's listings of unspent VTXOs only. The address collects a
//! spent VTXO for every swap and refund, and ark-client's own balance and coin selection page
//! through all of them, a hundred at a time.

use std::collections::HashSet;
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ark_client::{Blockchain, Client, InMemorySwapStorage, OfflineClient, OfflineClientConfig};
use ark_core::send::{
    build_offchain_transactions, sign_ark_transaction, sign_checkpoint_transaction,
    OffchainTransactions, SendReceiver, VtxoInput,
};
use ark_core::server::{GetVtxosRequest, VirtualTxOutPoint};
use ark_core::{ArkAddress, ExplorerUtxo};
use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::psbt;
use bitcoin::secp256k1::SecretKey;
use bitcoin::secp256k1::{self, schnorr};
use bitcoin::{Address, Amount, OutPoint, Txid};
use coordinator_ark::{ArkServer, ArkTransport};
use coordinator_ark_escrow::{RefundSwap, SwapPath};
use serde::Serialize;

use crate::coins::{self, Coins, Margins};
use crate::config::Config;
use crate::electrum::Electrum;
use crate::onchain_wallet::BoardingOnlyWallet;
use crate::swap::unix_now;

/// How many VTXOs the indexer is asked for at a time.
const VTXO_PAGE_SIZE: i32 = 100;
/// How long the indexer has to answer with one page.
const VTXO_PAGE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `GET /v1/wallet` answers from the last reading of the wallet.
const VIEW_TTL: Duration = Duration::from_secs(10);
/// A reading of the wallet slower than this is logged.
const SLOW_VIEW: Duration = Duration::from_secs(5);

type ArkClient = Client<Electrum, BoardingOnlyWallet, InMemorySwapStorage>;

/// Where Arkade carries a condition's witness, for the server to finalize with.
fn condition_key() -> psbt::raw::Key {
    psbt::raw::Key {
        type_value: 222,
        key: ark_core::VTXO_CONDITION_KEY.to_vec(),
    }
}

/// The witness elements, as the server decodes them: a count, then each length and value.
fn encode_witness(elements: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = bitcoin::consensus::serialize(&bitcoin::VarInt(elements.len() as u64));
    for element in elements {
        bytes.extend(bitcoin::consensus::serialize(&bitcoin::VarInt(
            element.len() as u64,
        )));
        bytes.extend_from_slice(element);
    }
    bytes
}

/// The condition witness that satisfies a refund swap's claim condition: see
/// [`ArkWallet::claim_refund_swap`].
fn claim_condition_witness(preimage: &[u8; 32]) -> Vec<u8> {
    encode_witness(&[vec![0x01], preimage.to_vec()])
}

pub struct ArkWallet {
    client: ArkClient,
    server: ArkServer,
    /// The service's own key. It signs its sends, and claims the refund swaps that pay it.
    keypair: Keypair,
    /// One send at a time, so concurrent swaps never select the same VTXOs.
    sending: tokio::sync::Mutex<()>,
    /// Where the boarding address's on-chain coins are read from.
    chain: Arc<Electrum>,
    margins: Margins,
    /// The last reading of the wallet, for `view`.
    view: Cached<WalletView>,
    /// How the last boards and renewals went, for `view`.
    boards: BoardOutcomes,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalletView {
    pub ark_address: String,
    pub boarding_address: String,
    /// Spendable VTXOs that came straight from a batch.
    pub confirmed_sat: u64,
    /// Spendable VTXOs that came from an Arkade transaction.
    pub pre_confirmed_sat: u64,
    /// The part of the two above that may pay an escrow: it has the pay margin of life or more.
    pub payable_sat: u64,
    /// The rest of them: too close to expiry to pay an escrow, until a batch renews it.
    pub expiring_sat: u64,
    /// Swept, expired or below dust: not spendable until a batch recovers it.
    pub recoverable_sat: u64,
    /// UNIX seconds. When the first spendable VTXO expires.
    pub earliest_expiry: Option<i64>,
    /// Confirmed, unspent coins at the boarding address: what the next board would move in.
    /// They stay here until a batch takes them, so a top-up the server cannot board shows here.
    pub boarding_sat: u64,
    /// The last board or renewal the Arkade server failed, since this instance started.
    pub last_board_failure: Option<BoardFailure>,
    /// UNIX seconds. When a batch last took a board or a renewal, since this instance started.
    pub last_board_success_at: Option<i64>,
}

/// A board or renewal the Arkade server failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoardFailure {
    /// UNIX seconds.
    pub at: i64,
    pub message: String,
}

/// How the wallet's boards and renewals last went, kept in memory. The coordinator reads it to
/// learn whether the Arkade server takes batches while it runs none of its own.
#[derive(Debug, Default)]
pub struct BoardOutcomes(std::sync::Mutex<Outcomes>);

#[derive(Debug, Default)]
struct Outcomes {
    failure: Option<BoardFailure>,
    success_at: Option<i64>,
}

impl BoardOutcomes {
    /// Count a board or renewal at `now`. A batch that took the coins is a success and a failure
    /// of the server's is a failure; nothing to board, or a failure on this side, is neither.
    pub fn record(&self, outcome: &anyhow::Result<Option<Txid>>, now: i64) {
        let mut outcomes = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        match outcome {
            Ok(Some(_)) => outcomes.success_at = Some(now),
            Ok(None) => {}
            Err(error) => {
                let message = format!("{error:#}");
                if server_fault(&message) {
                    outcomes.failure = Some(BoardFailure { at: now, message });
                }
            }
        }
    }

    /// Put the last outcomes in `view`, which may be a cached reading of the wallet.
    pub fn stamp(&self, view: &mut WalletView) {
        let outcomes = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        view.last_board_failure = outcomes.failure.clone();
        view.last_board_success_at = outcomes.success_at;
    }
}

/// Whether a board or renewal failed on the Arkade server's side: a batch the server gave up on,
/// or a request it answered with an internal error or could not be reached for. The same
/// classification as `coordinator_ark::Error::is_server_fault`, read from the message, because
/// ark-client keeps the server's gRPC status inside an opaque error.
fn server_fault(message: &str) -> bool {
    const FAULTS: [&str; 3] = [
        // ark-client's error for the server's `BatchFailed` event.
        "batch failed ",
        // How a gRPC `Internal` or `Unavailable` status prints.
        "code: 'Internal error'",
        "code: 'The service is currently unavailable'",
    ];
    FAULTS.iter().any(|fault| message.contains(fault))
}

/// A value kept for `ttl`, so callers in quick succession share one reading of it.
pub struct Cached<T> {
    ttl: Duration,
    held: tokio::sync::Mutex<Option<(std::time::Instant, T)>>,
}

impl<T: Clone> Cached<T> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            held: tokio::sync::Mutex::new(None),
        }
    }

    /// The value held, if it was read within `ttl`; otherwise what `read` returns, which is
    /// then held. Callers that arrive during a reading wait for it rather than start another,
    /// and a failed reading is not held.
    pub async fn get<F, Fut>(&self, read: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let mut held = self.held.lock().await;
        if let Some((read_at, value)) = held.as_ref() {
            if read_at.elapsed() < self.ttl {
                return Ok(value.clone());
            }
        }
        let value = read().await?;
        *held = Some((std::time::Instant::now(), value.clone()));
        Ok(value)
    }
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
        let blockchain = Arc::new(Electrum::connect(&config.electrum_url, config.network).await?);
        let wallet = Arc::new(BoardingOnlyWallet);
        let client = OfflineClient::with_keypair(
            OfflineClientConfig {
                ark_server_url: config.ark_server_url.clone(),
                // Boarding waits for a batch, and batches come once a session.
                timeout: Duration::from_secs(2 * server.info().session_duration + 30),
                ..Default::default()
            },
            keypair,
            blockchain.clone(),
            wallet,
            Arc::new(InMemorySwapStorage::new()),
        )
        .connect()
        .await
        .map_err(|error| anyhow::anyhow!("connect the Ark wallet: {error}"))?;
        Ok(Self {
            client,
            server,
            keypair,
            sending: tokio::sync::Mutex::new(()),
            chain: blockchain,
            margins: Margins::from_days(config.renew_margin_days, config.pay_margin_days),
            view: Cached::new(VIEW_TTL),
            boards: BoardOutcomes::default(),
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

    /// The service's own key, which a refund swap names as its swapper.
    pub fn swapper_key(&self) -> bitcoin::XOnlyPublicKey {
        self.keypair.x_only_public_key().0
    }

    /// The Arkade server's signer key, which co-signs every collaborative spend.
    pub fn server_key(&self) -> bitcoin::XOnlyPublicKey {
        self.server.rules().signer
    }

    /// The server's shortest exit delay, which every VTXO this service mints must respect.
    pub fn exit_delay(&self) -> coordinator_ark_escrow::RelativeTimelock {
        self.server.rules().min_exit_delay
    }

    pub fn hrp(&self) -> &'static str {
        self.server.hrp()
    }

    /// Check the Arkade server would accept a VTXO script this service mints.
    pub fn accepts(&self, vtxo: &coordinator_ark_escrow::VtxoScript) -> anyhow::Result<()> {
        Ok(self.server.rules().check(vtxo)?)
    }

    /// How long the Arkade server's batch sessions last.
    pub fn session_duration(&self) -> Duration {
        Duration::from_secs(self.server.info().session_duration)
    }

    pub fn dust(&self) -> Amount {
        self.server.info().dust
    }

    /// How much life a coin needs to be left alone, and to pay an escrow.
    pub fn margins(&self) -> Margins {
        self.margins
    }

    /// Pay `amount` to `address` in an Arkade transaction, returning its txid.
    ///
    /// The escrow inherits the expiry of the coins that pay it, so only coins with the pay
    /// margin of life or more are spent, the longest-lived first. Without enough of them
    /// nothing is sent.
    pub async fn pay(&self, address: ArkAddress, amount: Amount) -> anyhow::Result<Txid> {
        let _one_at_a_time = self.sending.lock().await;
        let held = self.coins().await?;
        let now = unix_now();
        let pay_secs = self.margins.pay_secs;
        let inputs = held
            .select(amount.to_sat(), self.dust().to_sat(), now, pay_secs)
            .with_context(|| {
                format!(
                    "no coins with {} days of life left cover {} sat: {} sat spendable; \
                     {} sat awaiting renewal",
                    pay_secs / coins::DAY_SECS,
                    coins::grouped(amount.to_sat()),
                    coins::grouped(held.payable_sat(now, pay_secs)),
                    coins::grouped(held.awaiting_renewal_sat(now, pay_secs)),
                )
            })?;
        self.client
            .send_selection(&inputs, vec![SendReceiver::bitcoin(address, amount)])
            .await
            .map_err(|error| anyhow::anyhow!("pay the escrow: {error}"))
    }

    /// The VTXO at `address` worth `amount` that paid it: the output of `paid_in` when the Ark
    /// transaction is known, otherwise an unspent one created at or after `since` (UNIX seconds).
    ///
    /// After a crash between paying and recording it, this finds the payment instead of paying twice.
    /// The indexer can list a payment a little after `pay` returns, so `None` may only mean "not yet".
    pub async fn paid_vtxo(
        &self,
        address: ArkAddress,
        amount: Amount,
        since: i64,
        paid_in: Option<Txid>,
    ) -> anyhow::Result<Option<OutPoint>> {
        let response = self
            .server
            .client()
            .grpc()
            .list_vtxos(GetVtxosRequest::new_for_addresses(std::iter::once(address)))
            .await?;
        Ok(payment_among(&response.vtxos, amount, since, paid_in))
    }

    /// Claim a refund swap that this service paid for, into its own wallet.
    ///
    /// The swap's claim leaf needs the invoice preimage as well as this service's signature, so
    /// the coins can only be taken once the player has been paid. Arkade carries a condition's
    /// witness in a PSBT field the server reads when it builds the final witness, so the preimage
    /// travels with each signature rather than on the input.
    pub async fn claim_refund_swap(
        &self,
        swap: &RefundSwap,
        outpoint: OutPoint,
        amount: Amount,
        preimage: [u8; 32],
    ) -> anyhow::Result<Txid> {
        anyhow::ensure!(
            swap.terms().swapper == self.swapper_key(),
            "this swap names another swap service"
        );
        let (address, _) = self
            .client
            .get_offchain_address()
            .await
            .map_err(|error| anyhow::anyhow!("this wallet has no offchain address: {error}"))?;
        let leaf = swap.script(SwapPath::Claim).clone();
        let input = VtxoInput::new(
            leaf.clone(),
            None,
            swap.vtxo_script()
                .control_block(&leaf)
                .context("the claim leaf is in the swap's tree")?,
            swap.vtxo_script().scripts().to_vec(),
            swap.script_pubkey(),
            amount,
            outpoint,
            Vec::new(),
        );
        // The swap is drained, so the change address is never used.
        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &[SendReceiver::bitcoin(address, amount)],
            &address,
            std::slice::from_ref(&input),
            self.server.info(),
        )
        .map_err(|error| anyhow::anyhow!("build the claim: {error}"))?;

        // The claim leaf is `SHA256 <hash> EQUALVERIFY VERIFY <swapper> CHECKSIGVERIFY <server>
        // CHECKSIG`. EQUALVERIFY leaves nothing for the leaf's VERIFY, so a true element sits
        // under the preimage. arkd evaluates the condition on its own and needs exactly one true
        // element left; with the preimage alone the stack ends empty, which arkd reports as
        // INVALID_SIGNATURE.
        let condition_witness = claim_condition_witness(&preimage);
        let sign = |input: &mut psbt::Input,
                    message: secp256k1::Message|
         -> Result<
            Vec<(schnorr::Signature, bitcoin::XOnlyPublicKey)>,
            ark_core::Error,
        > {
            input
                .unknown
                .insert(condition_key(), condition_witness.clone());
            let signature = Secp256k1::new().sign_schnorr_no_aux_rand(&message, &self.keypair);
            Ok(vec![(signature, self.keypair.x_only_public_key().0)])
        };
        sign_ark_transaction(sign, &mut ark_tx, 0)
            .map_err(|error| anyhow::anyhow!("sign the claim: {error}"))?;
        let ark_txid = ark_tx.unsigned_tx.compute_txid();

        let _one_at_a_time = self.sending.lock().await;
        let submitted = self
            .server
            .client()
            .submit_offchain(ark_tx, checkpoint_txs)
            .await?;
        let mut checkpoint = submitted
            .checkpoints
            .first()
            .context("the server returned no checkpoint for the claim")?
            .clone();
        sign_checkpoint_transaction(sign, &mut checkpoint)
            .map_err(|error| anyhow::anyhow!("sign the claim's checkpoint: {error}"))?;
        self.server
            .client()
            .finalize_offchain(ark_txid, vec![checkpoint])
            .await?;
        Ok(ark_txid)
    }

    /// The wallet's unspent VTXOs, from the indexer's listings of spendable and of recoverable
    /// ones. Neither lists the spent VTXOs, which are most of what the address has held.
    pub async fn coins(&self) -> anyhow::Result<Coins> {
        let info = self
            .client
            .server_info()
            .await
            .map_err(|error| anyhow::anyhow!("read the server's info: {error}"))?;
        let addresses = self
            .client
            .get_offchain_addresses()
            .await
            .map_err(|error| anyhow::anyhow!("read the Ark addresses: {error}"))?;
        let now = unix_now();
        // The server no longer co-signs for a signer key past its cutoff: what sits under one
        // cannot be sent or renewed, only recovered once it expires.
        let unsigned: HashSet<bitcoin::ScriptBuf> = addresses
            .iter()
            .filter(|(_, vtxo)| info.signer_requires_recovery_at(vtxo.server_pk(), now))
            .map(|(address, _)| address.to_p2tr_script_pubkey())
            .collect();
        let request =
            || GetVtxosRequest::new_for_addresses(addresses.iter().map(|(address, _)| *address));
        let filter = |error| anyhow::anyhow!("filter the VTXO listing: {error}");
        let (mut vtxos, recoverable) = tokio::try_join!(
            self.list_vtxos(request().spendable_only().map_err(filter)?),
            self.list_vtxos(request().recoverable_only().map_err(filter)?),
        )?;
        vtxos.extend(recoverable);
        vtxos.retain(|vtxo| {
            !unsigned.contains(&vtxo.script) || coins::recoverable_at(vtxo, info.dust, now)
        });
        Ok(Coins::classify(&vtxos, info.dust, now))
    }

    /// Every VTXO `request` matches, a page at a time.
    async fn list_vtxos(&self, request: GetVtxosRequest) -> anyhow::Result<Vec<VirtualTxOutPoint>> {
        let mut vtxos = Vec::new();
        let mut index = 0;
        loop {
            let page = request.clone().with_page(VTXO_PAGE_SIZE, index);
            let response = tokio::time::timeout(
                VTXO_PAGE_TIMEOUT,
                self.server.client().grpc().list_vtxos(page),
            )
            .await
            .context("the indexer took too long to list the wallet's VTXOs")??;
            vtxos.extend(response.vtxos);
            match response.page {
                Some(page) if page.next < page.total && page.next > index => index = page.next,
                _ => return Ok(vtxos),
            }
        }
    }

    /// The wallet's addresses and balance, read at most once every `VIEW_TTL`, and how its
    /// boards last went.
    pub async fn view(&self) -> anyhow::Result<WalletView> {
        let mut view = self
            .view
            .get(|| async {
                let started = std::time::Instant::now();
                let view = self.read_view().await;
                let took = started.elapsed();
                if took > SLOW_VIEW {
                    log::warn!("reading the Ark wallet took {:.1}s", took.as_secs_f64());
                }
                view
            })
            .await?;
        self.boards.stamp(&mut view);
        Ok(view)
    }

    async fn read_view(&self) -> anyhow::Result<WalletView> {
        let coins = self.coins().await?;
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
        let pre_confirmed_sat: u64 = coins
            .spendable
            .iter()
            .filter(|coin| coin.preconfirmed)
            .map(|coin| coin.sat)
            .sum();
        let payable_sat = coins.payable_sat(unix_now(), self.margins.pay_secs);
        let boarding_sat = self.boarding_sat_at(&boarding_address).await?;
        Ok(WalletView {
            ark_address: ark_address.encode(),
            boarding_address: boarding_address.to_string(),
            confirmed_sat: coins.spendable_sat() - pre_confirmed_sat,
            pre_confirmed_sat,
            payable_sat,
            expiring_sat: coins.spendable_sat() - payable_sat,
            recoverable_sat: coins.recoverable_sat(),
            earliest_expiry: coins.earliest_expiry(),
            boarding_sat,
            last_board_failure: None,
            last_board_success_at: None,
        })
    }

    /// Confirmed, unspent coins at the boarding address: what the next board would move in.
    pub async fn confirmed_boarding_sat(&self) -> anyhow::Result<u64> {
        let address = self
            .client
            .get_boarding_address()
            .await
            .map_err(|error| anyhow::anyhow!("read the boarding address: {error}"))?;
        self.boarding_sat_at(&address).await
    }

    async fn boarding_sat_at(&self, address: &Address) -> anyhow::Result<u64> {
        let outputs = self
            .chain
            .find_outpoints(address)
            .await
            .map_err(|error| anyhow::anyhow!("list the boarding outputs: {error}"))?;
        Ok(boarding_sat(&outputs))
    }

    /// Move confirmed boarding outputs, and recoverable VTXOs, into fresh VTXOs in the next batch.
    /// VTXOs near expiry are left alone: `renew` settles those.
    pub async fn board(&self) -> anyhow::Result<Option<Txid>> {
        let _one_at_a_time = self.sending.lock().await;
        let mut rng = <rand08::rngs::StdRng as rand08::SeedableRng>::from_entropy();
        let boarded = self
            .client
            .settle(&mut rng)
            .await
            .map_err(|error| anyhow::anyhow!("board: {error}"));
        self.boards.record(&boarded, unix_now());
        boarded
    }

    /// Settle `coins` into one fresh VTXO in the next batch, which gives it a new batch's
    /// whole life. Recoverable coins come back this way, and only this way.
    ///
    /// Like a boarding, it holds the wallet's sends for the batch, so no swap spends a coin
    /// the batch is taking.
    pub async fn renew(&self, coins: &[OutPoint]) -> anyhow::Result<Option<Txid>> {
        let _one_at_a_time = self.sending.lock().await;
        let mut rng = <rand08::rngs::StdRng as rand08::SeedableRng>::from_entropy();
        let renewed = self
            .client
            .settle_vtxos(&mut rng, coins, &[])
            .await
            .map_err(|error| anyhow::anyhow!("renew: {error}"));
        self.boards.record(&renewed, unix_now());
        renewed
    }
}

/// The wallet's secret key, created on first start with owner-only permissions.
pub fn load_or_create_key(path: &Path) -> anyhow::Result<Keypair> {
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

/// The VTXO among an address's `vtxos` that a payment of `amount` created: the output of
/// `paid_in` when the Ark transaction is known, spent or not; otherwise an unspent one created
/// at or after `since` (UNIX seconds).
fn payment_among(
    vtxos: &[ark_core::server::VirtualTxOutPoint],
    amount: Amount,
    since: i64,
    paid_in: Option<Txid>,
) -> Option<OutPoint> {
    vtxos
        .iter()
        .find(|vtxo| {
            vtxo.amount == amount
                && match paid_in {
                    Some(txid) => vtxo.outpoint.txid == txid,
                    None => vtxo.created_at >= since && !vtxo.is_spent,
                }
        })
        .map(|vtxo| vtxo.outpoint)
}

/// What of the boarding address's outputs the next board would move in: those confirmed and
/// not yet spent.
fn boarding_sat(outputs: &[ExplorerUtxo]) -> u64 {
    outputs
        .iter()
        .filter(|output| output.confirmations > 0 && !output.is_spent)
        .map(|output| output.amount.to_sat())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim leaf ends its condition in EQUALVERIFY and then runs VERIFY, so the witness
    /// must put a true element under the preimage: `[0x01, preimage]`, encoded as a count, then
    /// each length and value. With the preimage alone, arkd's condition check (and Script)
    /// finds an empty stack, which arkd reports as INVALID_SIGNATURE.
    #[test]
    fn the_claim_witness_leaves_one_true_element_for_the_claim_leaf() {
        use bitcoin::opcodes::all::{OP_EQUALVERIFY, OP_SHA256, OP_VERIFY};
        use coordinator_ark_escrow::{RelativeTimelock, SwapTerms};
        let key = |byte: u8| {
            bitcoin::secp256k1::SecretKey::from_slice(&[byte; 32])
                .unwrap()
                .x_only_public_key(&Secp256k1::new())
                .0
        };
        let deadline = bitcoin::absolute::LockTime::from_time(1_800_000_000).unwrap();
        let exit_delay = RelativeTimelock::Seconds(512 * 10);
        let payment_hash = [7u8; 32];
        let swap = RefundSwap::new(SwapTerms {
            player: key(1),
            swapper: key(2),
            server: key(3),
            payment_hash,
            deadline,
            exit_delay,
            unilateral_reclaim_delay: SwapTerms::unilateral_reclaim_delay_for(
                deadline,
                exit_delay,
                1_799_000_000,
            )
            .unwrap(),
        })
        .unwrap();
        let leaf = swap.script(SwapPath::Claim).as_bytes();
        let mut condition = vec![OP_SHA256.to_u8(), 32];
        condition.extend_from_slice(&payment_hash);
        condition.extend([OP_EQUALVERIFY.to_u8(), OP_VERIFY.to_u8()]);
        assert_eq!(&leaf[..condition.len()], condition.as_slice());

        let preimage = [9u8; 32];
        let mut expected = vec![2, 1, 1, 32];
        expected.extend_from_slice(&preimage);
        assert_eq!(claim_condition_witness(&preimage), expected);
    }

    fn vtxo(
        txid: u8,
        amount: u64,
        created_at: i64,
        spent: bool,
    ) -> ark_core::server::VirtualTxOutPoint {
        use bitcoin::hashes::Hash;
        ark_core::server::VirtualTxOutPoint {
            outpoint: OutPoint::new(Txid::from_byte_array([txid; 32]), 0),
            created_at,
            expires_at: created_at + 86_400,
            amount: Amount::from_sat(amount),
            script: bitcoin::ScriptBuf::new(),
            is_preconfirmed: true,
            is_swept: false,
            is_unrolled: false,
            is_spent: spent,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
            depth: 0,
        }
    }

    #[test]
    fn only_confirmed_unspent_boarding_outputs_await_boarding() {
        use bitcoin::hashes::Hash;
        let output = |txid: u8, sat: u64, confirmations: u64, is_spent: bool| ExplorerUtxo {
            outpoint: OutPoint::new(Txid::from_byte_array([txid; 32]), 0),
            amount: Amount::from_sat(sat),
            confirmation_blocktime: (confirmations > 0).then_some(1_800_000_000),
            confirmations,
            is_spent,
        };
        assert_eq!(boarding_sat(&[]), 0);
        assert_eq!(
            boarding_sat(&[
                // Two top-ups the server has not boarded yet.
                output(1, 200_000, 12, false),
                output(2, 200_000, 3, false),
                // One still in the mempool, and one a batch already took.
                output(3, 50_000, 0, false),
                output(4, 70_000, 40, true),
            ]),
            400_000
        );
    }

    #[test]
    fn the_wallet_view_reports_what_awaits_boarding() {
        let view = WalletView {
            ark_address: "tark1".to_string(),
            boarding_address: "tb1q".to_string(),
            confirmed_sat: 0,
            pre_confirmed_sat: 1_000,
            payable_sat: 1_000,
            expiring_sat: 0,
            recoverable_sat: 0,
            earliest_expiry: None,
            boarding_sat: 400_000,
            last_board_failure: None,
            last_board_success_at: None,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["boarding_sat"], 400_000);
    }

    /// A board as ark-client fails it when the server answers with `status`.
    fn refused_board(status: tonic::Status) -> anyhow::Result<Option<Txid>> {
        Err(anyhow::anyhow!(
            "board: Failed to join batch: request failed{status}"
        ))
    }

    #[test]
    fn the_wallet_view_reports_the_last_failed_and_the_last_successful_board() {
        use bitcoin::hashes::Hash;
        let boards = BoardOutcomes::default();
        let mut view = WalletView {
            ark_address: "tark1".to_string(),
            boarding_address: "tb1q".to_string(),
            confirmed_sat: 0,
            pre_confirmed_sat: 0,
            payable_sat: 0,
            expiring_sat: 0,
            recoverable_sat: 0,
            earliest_expiry: None,
            boarding_sat: 400_000,
            last_board_failure: None,
            last_board_success_at: None,
        };
        boards.stamp(&mut view);
        let json = serde_json::to_value(&view).unwrap();
        assert!(json["last_board_failure"].is_null());
        assert!(json["last_board_success_at"].is_null());

        let rescan = tonic::Status::internal(
            "INTERNAL_ERROR (0): failed to rescan boarding utxos: HTTP 500",
        );
        boards.record(&refused_board(rescan), 1_000);
        boards.stamp(&mut view);
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["last_board_failure"]["at"], 1_000);
        assert!(json["last_board_failure"]["message"]
            .as_str()
            .unwrap()
            .contains("failed to rescan boarding utxos"));
        assert!(json["last_board_success_at"].is_null());

        // Nothing to board, and a failure on this side, change nothing.
        boards.record(&Ok(None), 1_060);
        boards.record(&Err(anyhow::anyhow!("board: not enough funds")), 1_060);
        boards.stamp(&mut view);
        assert_eq!(view.last_board_failure.as_ref().unwrap().at, 1_000);

        boards.record(&Ok(Some(Txid::from_byte_array([1; 32]))), 1_120);
        boards.stamp(&mut view);
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["last_board_success_at"], 1_120);
        assert_eq!(
            json["last_board_failure"]["at"], 1_000,
            "the failure is kept"
        );
    }

    #[test]
    fn only_the_servers_own_failures_count_against_a_board() {
        let fault = |outcome: anyhow::Result<Option<Txid>>| {
            server_fault(&format!("{:#}", outcome.unwrap_err()))
        };
        assert!(fault(refused_board(tonic::Status::internal(
            "INTERNAL_ERROR (0): failed to rescan boarding utxos: HTTP 500"
        ))));
        assert!(fault(refused_board(tonic::Status::unavailable(
            "connection refused"
        ))));
        assert!(fault(Err(anyhow::anyhow!(
            "renew: Failed to join batch: batch failed 7f3a: failed to create commitment tx"
        ))));
        assert!(!fault(refused_board(tonic::Status::invalid_argument(
            "VTXO_ALREADY_SPENT (6): already spent"
        ))));
        assert!(!fault(Err(anyhow::anyhow!(
            "board: Failed to join batch: not enough funds to cover fees"
        ))));
    }

    #[tokio::test]
    async fn the_wallet_is_read_once_for_callers_within_the_cache_time() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let readings = AtomicU64::new(0);
        let read = || async {
            // A reading takes a while, as one through arkd does.
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<u64, anyhow::Error>(readings.fetch_add(1, Ordering::SeqCst) + 1)
        };
        let cached = Cached::new(Duration::from_millis(200));

        // Callers at once, and those soon after, share one reading.
        let (first, second) = tokio::join!(cached.get(read), cached.get(read));
        assert_eq!((first.unwrap(), second.unwrap()), (1, 1));
        assert_eq!(cached.get(read).await.unwrap(), 1);
        assert_eq!(readings.load(Ordering::SeqCst), 1);

        // Once it is stale the wallet is read again.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(cached.get(read).await.unwrap(), 2);
        assert_eq!(cached.get(read).await.unwrap(), 2);

        // A failed reading is not kept: the next caller reads again.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let failed = cached
            .get(|| async { Err::<u64, _>(anyhow::anyhow!("arkd is away")) })
            .await;
        assert!(failed.is_err());
        assert_eq!(cached.get(read).await.unwrap(), 3);
    }

    #[test]
    fn a_payment_is_the_vtxo_its_ark_transaction_created() {
        use bitcoin::hashes::Hash;
        let paid_in = Some(Txid::from_byte_array([2; 32]));
        let amount = Amount::from_sat(6_300);
        let listed = [
            vtxo(1, 6_300, 1_000, false),
            vtxo(2, 6_000, 1_000, false),
            vtxo(2, 6_300, 1_000, true),
        ];
        assert_eq!(
            payment_among(&listed, amount, 900, paid_in),
            Some(listed[2].outpoint),
            "the right transaction and amount, even once spent"
        );
        assert_eq!(
            payment_among(&listed[..2], amount, 900, paid_in),
            None,
            "another transaction's VTXO, or the wrong amount, is not the payment"
        );
    }

    #[test]
    fn without_its_transaction_a_payment_is_an_unspent_vtxo_made_since_the_swap() {
        let amount = Amount::from_sat(6_300);
        let listed = [
            vtxo(1, 6_300, 800, false),
            vtxo(2, 6_300, 1_000, true),
            vtxo(3, 6_300, 1_000, false),
        ];
        assert_eq!(
            payment_among(&listed, amount, 900, None),
            Some(listed[2].outpoint)
        );
        assert_eq!(payment_among(&listed[..2], amount, 900, None), None);
    }
}
