//! On-chain access for the coordinator.
//!
//! The wallet lives in LND: addresses, UTXOs, wallet coin selection, signing,
//! finalizing and publishing use LND's REST API (walletkit). The coordinator
//! assembles external escrow inputs and accounts for their witness fees, while
//! preserving LND's input leases in the funding PSBT. Chain lookups
//! that LND cannot answer for transactions it does not own (escrow and
//! outcome transactions) go to an electrs server over the Electrum protocol.
//! The coordinator's own key (the seed file) is still used for DLC escrow
//! signatures, which no wallet can produce on its behalf.
use crate::{
    config::{BitcoinSettings, LnSettings},
    infra::secrets::get_key,
};
use anyhow::anyhow;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use bitcoin::{
    absolute::LockTime,
    address::NetworkChecked,
    consensus::encode::serialize,
    ecdsa,
    hashes::{sha256, Hash},
    psbt::Input,
    secp256k1::{Message, Secp256k1, SecretKey},
    sighash::{EcdsaSighashType, SighashCache},
    transaction::Version,
    Address, Amount, Network, NetworkKind, OutPoint, Psbt, PublicKey, ScriptBuf, Sequence,
    Transaction, TxIn, TxOut, Txid, Weight, Witness,
};
use dlctix::{bitcoin::FeeRate, secp::Scalar};
use electrum_client::{Client as ElectrumClient, ConfigBuilder, ElectrumApi};
use futures::future::join_all;
use log::{debug, error, info, warn};
use reqwest::{Certificate, Client, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, fs, path::Path, str::FromStr, sync::Arc, time::Duration};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

mod funding;

// Needs to be over half of the last 10 blocks block time passed
// the expiry time for a block to be considered final and able to broadcast
pub const REQUIRED_CONFIRMATIONS_FOR_TIME: usize = 6;

/// Confirmation targets (in blocks) offered by `get_estimated_fee_rates`.
const FEE_TARGETS: [u16; 12] = [1, 2, 3, 4, 5, 6, 10, 12, 24, 144, 504, 1008];

/// Confirmation target (in blocks) for sweeps with no deadline: about a day.
pub const ECONOMY_FEE_TARGET: u16 = 144;

/// LND's fee rate floor, 253 sat/kWU: the 1 sat/vB relay minimum with room for rounding.
/// No sweep can pay less, so an output that is dust after this fee is never worth sweeping.
pub const LND_FEE_RATE_FLOOR: FeeRate = FeeRate::from_sat_per_kwu(253);

/// An unspent output of the LND wallet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletUtxo {
    pub outpoint: OutPoint,
    pub txout: TxOut,
    pub address: String,
    pub confirmations: u64,
}

impl WalletUtxo {
    pub fn is_confirmed(&self) -> bool {
        self.confirmations > 0
    }
}

/// The LND wallet's on-chain balance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WalletBalance {
    pub confirmed: Amount,
    pub unconfirmed: Amount,
    pub locked: Amount,
}

impl WalletBalance {
    pub fn total(&self) -> Amount {
        self.confirmed + self.unconfirmed
    }
}

/// The fee rate for confirmation within `conf_target` blocks from LND's estimates, with the
/// margin of [`fee_rate_from_estimate`].
pub fn fee_rate_for_target(
    fee_rates: &std::collections::HashMap<u16, f64>,
    conf_target: u16,
) -> Result<FeeRate, anyhow::Error> {
    let estimate = fee_rates.get(&conf_target).ok_or_else(|| {
        anyhow!("LND returned no fee estimate for a {conf_target}-block confirmation target")
    })?;
    fee_rate_from_estimate(*estimate)
}

/// Above an estimate, what a time-critical rate adds: this share of it in percent, and at least
/// [`FEE_MARGIN_MIN_SAT_PER_KWU`].
pub const FEE_MARGIN_PERCENT: u64 = 10;
/// 0.25 sat/vB.
pub const FEE_MARGIN_MIN_SAT_PER_KWU: u64 = 63;

/// The rate for a time-critical transaction from an LND estimate: the estimate plus the larger of
/// [`FEE_MARGIN_PERCENT`] and 0.25 sat/vB, at LND's sat/kWU precision, never below LND's floor.
/// The contract's outcome and closing transactions confirm a day or more after it is built, so
/// the margin covers some rise in fees. Rounding up to whole sat/vB instead doubled the rate at
/// LND's floor, 1.012 sat/vB.
pub fn fee_rate_from_estimate(sat_per_vb: f64) -> Result<FeeRate, anyhow::Error> {
    if !sat_per_vb.is_finite() || sat_per_vb < 0.0 {
        return Err(anyhow!("invalid fee estimate {sat_per_vb} sat/vB"));
    }
    // One vbyte is four weight units: sat/kWU = sat/vB * 1000 / 4.
    let estimate = (sat_per_vb * 250.0).round();
    if estimate > (u64::MAX / 2) as f64 {
        return Err(anyhow!("fee estimate {sat_per_vb} sat/vB is out of range"));
    }
    let estimate = estimate as u64;
    let margin = (estimate * FEE_MARGIN_PERCENT)
        .div_ceil(100)
        .max(FEE_MARGIN_MIN_SAT_PER_KWU);
    Ok(FeeRate::from_sat_per_kwu(estimate + margin).max(LND_FEE_RATE_FLOOR))
}

/// The fee rate for a sweep with no deadline: LND's estimate for [`ECONOMY_FEE_TARGET`] blocks,
/// or its next-block estimate if it has none, never below LND's floor.
///
/// Unlike [`fee_rate_for_target`] this keeps the estimate's precision. LND estimates in whole
/// sat/kWU, and its floor of 253 sat/kWU is 1.012 sat/vB, which rounding up to whole sat/vB
/// doubles. On an output of a few hundred sats that is the difference between a sweep and dust.
pub fn economy_fee_rate(
    fee_rates: &std::collections::HashMap<u16, f64>,
) -> Result<FeeRate, anyhow::Error> {
    let sat_per_vb = fee_rates
        .get(&ECONOMY_FEE_TARGET)
        .or_else(|| fee_rates.get(&1))
        .copied()
        .ok_or_else(|| {
            anyhow!(
                "LND returned no fee estimate for a {ECONOMY_FEE_TARGET}-block or next-block \
                 confirmation target"
            )
        })?;
    if !sat_per_vb.is_finite() || sat_per_vb < 0.0 {
        return Err(anyhow!("invalid fee estimate {sat_per_vb} sat/vB"));
    }
    // One vbyte is four weight units: sat/kWU = sat/vB * 1000 / 4.
    let sat_per_kwu = (sat_per_vb * 250.0).round() as u64;
    Ok(FeeRate::from_sat_per_kwu(sat_per_kwu).max(LND_FEE_RATE_FLOOR))
}

#[derive(Debug, Clone, Copy)]
pub struct PayoutOutputStatus {
    pub confirmation_height: Option<u32>,
    pub current_height: u32,
    pub unspent: bool,
}

/// The unspent outputs the chain holds at one script, mempool included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScriptFunds {
    pub outputs: u64,
    pub sats: u64,
    /// How many of the outputs, and of the sats, are not in a block yet.
    pub unconfirmed_outputs: u64,
    pub unconfirmed_sats: u64,
}

impl ScriptFunds {
    /// The total of unspent outputs given as (value in sats, whether it is in a block).
    pub fn of(unspent: impl IntoIterator<Item = (u64, bool)>) -> Self {
        let mut funds = Self::default();
        for (sats, confirmed) in unspent {
            funds.outputs += 1;
            funds.sats = funds.sats.saturating_add(sats);
            if !confirmed {
                funds.unconfirmed_outputs += 1;
                funds.unconfirmed_sats = funds.unconfirmed_sats.saturating_add(sats);
            }
        }
        funds
    }
}

#[async_trait]
pub trait Bitcoin: Send + Sync {
    fn get_network(&self) -> Network;
    /// Signs the wallet's inputs through LND and the escrow inputs with the
    /// coordinator's key. Returns whether every input carries a signature.
    async fn sign_psbt_with_escrow_support(&self, psbt: &mut Psbt) -> Result<bool, anyhow::Error>;
    /// Finalizes escrow inputs from their signatures, then lets LND sign and
    /// finalize its own inputs. Returns whether every input is final.
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
    /// Keep the wallet inputs owned by this packet reserved until the given
    /// Unix timestamp. Persist the packet before giving it to other signers.
    async fn reserve_psbt_inputs_until(
        &self,
        psbt: &Psbt,
        deadline: u64,
    ) -> Result<(), anyhow::Error>;
    /// Release this packet's wallet inputs after a definite pre-publication failure.
    async fn release_psbt_inputs(&self, psbt: &Psbt) -> Result<(), anyhow::Error>;
    async fn get_spendable_utxo(&self, amount_sats: u64) -> Result<WalletUtxo, anyhow::Error>;
    async fn get_current_height(&self) -> Result<u32, anyhow::Error>;
    async fn get_confirmed_blockchain_time(&self, blocks: usize) -> Result<u64, anyhow::Error>;
    async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error>;
    /// LND's fee rate estimate in sat/vB for confirmation within `conf_target` blocks.
    async fn estimate_fee(&self, conf_target: u16) -> Result<f64, anyhow::Error>;
    async fn get_tx_confirmation_height(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error>;
    /// Includes mempool spends; wallet-owned UTXO lists are insufficient for DLC outputs.
    async fn payout_output_status(
        &self,
        outpoint: OutPoint,
        output: TxOut,
    ) -> Result<PayoutOutputStatus, anyhow::Error>;
    /// Read-only support lookup. `None` means the bounded history was checked completely.
    async fn spending_transaction(
        &self,
        _outpoint: OutPoint,
        _output: TxOut,
    ) -> Result<Option<Transaction>, anyhow::Error> {
        Err(anyhow!(
            "Spending transaction lookup is not available for this backend"
        ))
    }
    /// Read-only. `None` when this backend cannot list a script's outputs.
    async fn script_funds(&self, _script: ScriptBuf) -> Result<Option<ScriptFunds>, anyhow::Error> {
        Ok(None)
    }
    async fn broadcast(&self, transaction: &Transaction) -> Result<(), anyhow::Error>;
    async fn get_next_address(&self) -> Result<Address, anyhow::Error>;
    async fn get_public_key(&self) -> Result<PublicKey, anyhow::Error>;
    async fn get_derived_private_key(&self) -> Result<Scalar, anyhow::Error>;
    async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error>;
    /// Signs and finalizes every wallet input through LND.
    async fn sign_psbt(&self, psbt: &mut Psbt) -> Result<bool, anyhow::Error>;
    async fn list_utxos(&self) -> Vec<WalletUtxo>;
    async fn sync(&self) -> Result<(), anyhow::Error>;
    async fn get_balance(&self) -> Result<WalletBalance, anyhow::Error>;
    async fn get_outputs(&self) -> Result<Vec<WalletUtxo>, anyhow::Error>;
    async fn send_to_address(
        &self,
        send_options: SendOptions,
        selected_utxos: Vec<OutPoint>,
    ) -> Result<Txid, anyhow::Error>;
    /// Up to `count` block headers from `start` on, in height order: each block's hash and
    /// header time. Fewer when the chain is shorter.
    async fn block_headers(
        &self,
        start: u32,
        count: u32,
    ) -> Result<Vec<BlockSummary>, anyhow::Error> {
        let _ = (start, count);
        Err(anyhow!("This Bitcoin client cannot read block headers"))
    }
}

/// A block, as a header gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSummary {
    pub height: u32,
    pub hash: bitcoin::BlockHash,
    /// The header's time, UNIX seconds.
    pub time: u32,
}

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

    pub fn validate_fee(&self, psbt: &Psbt) -> Result<(), anyhow::Error> {
        if let Some(max_fee_sats) = self.max_fee {
            let fee = psbt
                .fee()
                .map_err(|error| anyhow!("Cannot validate transaction fee: {}", error))?;
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

// ---------------------------------------------------------------------------
// LND REST (walletkit) client
// ---------------------------------------------------------------------------

/// LND encodes 64-bit integers as JSON strings; accept both forms.
fn json_u64(value: &Value) -> Result<u64, anyhow::Error> {
    match value {
        Value::String(s) => s
            .parse::<u64>()
            .map_err(|e| anyhow!("bad integer {s}: {e}")),
        Value::Number(n) => n.as_u64().ok_or_else(|| anyhow!("bad integer {n}")),
        other => Err(anyhow!("unexpected integer value {other}")),
    }
}

struct LndWallet {
    base_url: Url,
    client: Client,
    macaroon: SecretString,
}

impl LndWallet {
    fn new(settings: &LnSettings) -> Result<Self, anyhow::Error> {
        let macaroon = read_macaroon_hex(&settings.macaroon_file_path)?;
        let base_url = Url::parse(&settings.base_url)?;
        if !matches!(base_url.scheme(), "http" | "https")
            || base_url.host_str().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(anyhow!(
                "LND base URL must be an HTTP(S) URL without credentials, query or fragment"
            ));
        }
        // Wallet mutations are never retried: a failed response can follow a
        // successful spend, address allocation, or input lease.
        let mut builder = Client::builder()
            .tls_built_in_root_certs(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none());
        if let Some(tls_cert_path) = &settings.tls_cert_path {
            let cert_bytes = fs::read(tls_cert_path)
                .map_err(|e| anyhow!("Failed to read LND tls cert {}: {}", tls_cert_path, e))?;
            let cert = Certificate::from_pem(&cert_bytes)
                .map_err(|e| anyhow!("Failed to parse LND tls cert: {}", e))?;
            builder = builder.add_root_certificate(cert);
        }
        let client = builder.build()?;
        Ok(Self {
            base_url,
            client,
            macaroon,
        })
    }

    fn url(&self, path: &str) -> Result<Url, anyhow::Error> {
        self.base_url
            .join(path)
            .map_err(|e| anyhow!("Bad LND url for {}: {}", path, e))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, anyhow::Error> {
        let response = self
            .client
            .get(self.url(path)?)
            .header("Grpc-Metadata-macaroon", self.macaroon.expose_secret())
            .send()
            .await
            .map_err(|e| anyhow!("LND request {} failed: {}", path, e))?;
        Self::decode(path, response).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T, anyhow::Error> {
        let response = self
            .client
            .post(self.url(path)?)
            .header("Grpc-Metadata-macaroon", self.macaroon.expose_secret())
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("LND request {} failed: {}", path, e))?;
        Self::decode(path, response).await
    }

    async fn decode<T: DeserializeOwned>(
        path: &str,
        response: reqwest::Response,
    ) -> Result<T, anyhow::Error> {
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
                .unwrap_or(text);
            return Err(anyhow!("LND {} returned {}: {}", path, status, message));
        }
        serde_json::from_str::<T>(&text)
            .map_err(|e| anyhow!("LND {} returned unexpected JSON ({}): {}", path, e, text))
    }

    async fn block_height(&self) -> Result<u32, anyhow::Error> {
        let info: Value = self.get("v1/getinfo").await?;
        u32::try_from(json_u64(&info["block_height"])?).map_err(Into::into)
    }

    async fn list_unspent(&self) -> Result<Vec<WalletUtxo>, anyhow::Error> {
        let response: Value = self
            .post(
                "v2/wallet/utxos",
                json!({ "min_confs": 0, "max_confs": i32::MAX }),
            )
            .await?;
        let mut utxos = Vec::new();
        for utxo in response
            .get("utxos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let outpoint = &utxo["outpoint"];
            let txid = Txid::from_str(outpoint["txid_str"].as_str().unwrap_or_default())?;
            let vout = u32::try_from(json_u64(&outpoint["output_index"])?)?;
            let script = ScriptBuf::from_hex(utxo["pk_script"].as_str().unwrap_or_default())?;
            utxos.push(WalletUtxo {
                outpoint: OutPoint { txid, vout },
                txout: TxOut {
                    value: Amount::from_sat(json_u64(&utxo["amount_sat"])?),
                    script_pubkey: script,
                },
                address: utxo["address"].as_str().unwrap_or_default().to_owned(),
                confirmations: json_u64(&utxo["confirmations"])?,
            });
        }
        Ok(utxos)
    }

    async fn balance(&self) -> Result<WalletBalance, anyhow::Error> {
        let response: Value = self.get("v1/balance/blockchain").await?;
        Ok(WalletBalance {
            confirmed: Amount::from_sat(json_u64(&response["confirmed_balance"])?),
            unconfirmed: Amount::from_sat(json_u64(&response["unconfirmed_balance"])?),
            locked: Amount::from_sat(json_u64(&response["locked_balance"])?),
        })
    }

    async fn next_address(&self, network: Network) -> Result<Address, anyhow::Error> {
        let response: Value = self
            .post(
                "v2/wallet/address/next",
                json!({ "type": "TAPROOT_PUBKEY", "change": false }),
            )
            .await?;
        let addr = response["addr"]
            .as_str()
            .ok_or_else(|| anyhow!("LND returned no address"))?;
        Address::from_str(addr)?
            .require_network(network)
            .map_err(|e| anyhow!("LND address is not for {}: {}", network, e))
    }

    /// Signs the wallet's inputs; other inputs pass through untouched.
    async fn sign_psbt(&self, psbt: &Psbt) -> Result<Psbt, anyhow::Error> {
        let response: Value = self
            .post(
                "v2/wallet/psbt/sign",
                json!({ "funded_psbt": BASE64.encode(psbt.serialize()) }),
            )
            .await?;
        decode_psbt(&response["signed_psbt"])
    }

    /// Signs and finalizes the wallet's inputs; every other input must already be final.
    async fn finalize_psbt(&self, psbt: &Psbt) -> Result<Psbt, anyhow::Error> {
        let response: Value = self
            .post(
                "v2/wallet/psbt/finalize",
                json!({ "funded_psbt": BASE64.encode(psbt.serialize()) }),
            )
            .await?;
        decode_psbt(&response["signed_psbt"])
    }

    async fn publish(&self, transaction: &Transaction, label: &str) -> Result<(), anyhow::Error> {
        let response: Value = self
            .post(
                "v2/wallet/tx",
                json!({ "tx_hex": BASE64.encode(serialize(transaction)), "label": label }),
            )
            .await?;
        match response["publish_error"].as_str() {
            Some("") => Ok(()),
            Some(err) => Err(anyhow!("LND refused to publish: {}", err)),
            None => Err(anyhow!(
                "LND returned no publication result; publication outcome is unknown"
            )),
        }
    }

    async fn send_all(
        &self,
        address: &Address,
        sat_per_vbyte: u64,
        outpoints: &[OutPoint],
    ) -> Result<Txid, anyhow::Error> {
        let outpoints: Vec<Value> = outpoints
            .iter()
            .map(|o| json!({ "txid_str": o.txid.to_string(), "output_index": o.vout }))
            .collect();
        let response: Value = self
            .post(
                "v1/transactions",
                json!({
                    "addr": address.to_string(),
                    "send_all": true,
                    "sat_per_vbyte": sat_per_vbyte.to_string(),
                    "min_confs": 1,
                    "spend_unconfirmed": false,
                    "outpoints": outpoints,
                    "label": "coordinator sweep",
                }),
            )
            .await?;
        Txid::from_str(response["txid"].as_str().unwrap_or_default())
            .map_err(|e| anyhow!("LND returned no txid: {}", e))
    }

    /// Fee rate in sat/vB for a confirmation target.
    async fn estimate_fee(&self, conf_target: u16) -> Result<f64, anyhow::Error> {
        let response: Value = self
            .get(&format!("v2/wallet/estimatefee/{}", conf_target))
            .await?;
        let sat_per_kw = json_u64(&response["sat_per_kw"])? as f64;
        Ok(sat_per_kw * 4.0 / 1000.0)
    }
}

fn decode_psbt(value: &Value) -> Result<Psbt, anyhow::Error> {
    let encoded = value
        .as_str()
        .ok_or_else(|| anyhow!("LND returned no PSBT"))?;
    let bytes = BASE64
        .decode(encoded)
        .map_err(|e| anyhow!("LND returned an undecodable PSBT: {}", e))?;
    Psbt::deserialize(&bytes).map_err(|e| anyhow!("LND returned an invalid PSBT: {}", e))
}

fn read_macaroon_hex(file_path: &str) -> Result<SecretString, anyhow::Error> {
    if Path::new(file_path).extension().and_then(|s| s.to_str()) != Some("macaroon") {
        return Err(anyhow!("Not a '.macaroon' file extension"));
    }
    let contents =
        fs::read(file_path).map_err(|e| anyhow!("Failed to read macaroon file: {}", e))?;
    Ok(SecretString::from(hex::encode(contents)))
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

pub struct BitcoinClient {
    pub network: Network,
    coordinator_secret: Zeroizing<[u8; 32]>,
    lnd: LndWallet,
    electrum: Arc<ElectrumClient>,
    payout_electrum_url: String,
}

impl BitcoinClient {
    pub async fn new(
        settings: &BitcoinSettings,
        ln_settings: &LnSettings,
    ) -> Result<BitcoinClient, anyhow::Error> {
        info!("Creating Bitcoin client with settings:");
        info!("  Seed path: {}", settings.seed_path);
        info!("  Network: {}", settings.network);
        info!("  LND: {}", ln_settings.base_url);
        info!("  Electrum: {}", settings.electrum_url);

        if let Some(parent) = Path::new(&settings.seed_path).parent() {
            fs::create_dir_all(parent)?;
        }
        // Creates the key on first start, as before.
        let coordinator_secret = Zeroizing::new(
            get_key::<SecretKey>(&settings.seed_path)
                .map_err(|e| anyhow!("Failed to load bitcoin private key: {}", e))?
                .secret_bytes(),
        );

        let lnd = LndWallet::new(ln_settings)?;
        let electrum_url = settings.electrum_url.clone();
        let electrum = tokio::task::spawn_blocking(move || {
            ElectrumClient::from_config(
                &electrum_url,
                ConfigBuilder::new().timeout(Some(15)).retry(3).build(),
            )
        })
        .await?
        .map_err(|e| {
            anyhow!(
                "Failed to connect to electrs at {}: {}",
                settings.electrum_url,
                e
            )
        })?;

        let client = BitcoinClient {
            network: settings.network,
            coordinator_secret,
            lnd,
            electrum: Arc::new(electrum),
            payout_electrum_url: settings.electrum_url.clone(),
        };

        let info: Value = client.lnd.get("v1/getinfo").await?;
        let expected_network = match settings.network {
            Network::Bitcoin => "mainnet",
            Network::Testnet => "testnet",
            Network::Testnet4 => "testnet4",
            Network::Signet => "signet",
            Network::Regtest => "regtest",
        };
        let correct_chain = info["chains"].as_array().is_some_and(|chains| {
            chains
                .iter()
                .any(|chain| chain["chain"] == "bitcoin" && chain["network"] == expected_network)
        });
        if !correct_chain {
            return Err(anyhow!(
                "LND chain does not match configured Bitcoin network {}",
                settings.network
            ));
        }
        let genesis = client
            .with_electrum(|electrum| electrum.block_header(0))
            .await?;
        if genesis.block_hash()
            != bitcoin::blockdata::constants::genesis_block(settings.network).block_hash()
        {
            return Err(anyhow!(
                "Electrum chain does not match configured Bitcoin network {}",
                settings.network
            ));
        }
        let synced = info["synced_to_chain"].as_bool().unwrap_or(false);
        info!(
            "LND wallet reachable: height {}, synced_to_chain {}",
            json_u64(&info["block_height"])?,
            synced
        );
        if !synced {
            warn!("LND is not synced to chain yet; on-chain operations may lag");
        }
        client.print_balance_info().await?;
        Ok(client)
    }

    pub async fn print_balance_info(&self) -> Result<(), anyhow::Error> {
        let balance = self.lnd.balance().await?;
        info!(
            "Wallet balance: {} sats confirmed, {} sats unconfirmed",
            balance.confirmed.to_sat(),
            balance.unconfirmed.to_sat()
        );
        let utxos = self.list_utxos().await;
        info!("Available UTXOs: {}", utxos.len());
        for utxo in &utxos {
            info!(
                "UTXO: {} - Amount: {} sats, confirmations: {}",
                utxo.outpoint, utxo.txout.value, utxo.confirmations
            );
        }
        Ok(())
    }

    async fn with_electrum<T, F>(&self, operation: F) -> Result<T, anyhow::Error>
    where
        T: Send + 'static,
        F: FnOnce(&ElectrumClient) -> Result<T, electrum_client::Error> + Send + 'static,
    {
        let electrum = Arc::clone(&self.electrum);
        tokio::task::spawn_blocking(move || operation(&electrum))
            .await?
            .map_err(|e| anyhow!("electrs request failed: {}", e))
    }

    fn coordinator_private_key(&self) -> Result<bitcoin::PrivateKey, anyhow::Error> {
        let secret_key = SecretKey::from_slice(&self.coordinator_secret[..])?;
        Ok(bitcoin::PrivateKey {
            compressed: true,
            network: NetworkKind::from(self.network),
            inner: secret_key,
        })
    }

    async fn sign_escrow_inputs(&self, psbt: &mut Psbt) -> Result<usize, anyhow::Error> {
        let coordinator_privkey = self.coordinator_private_key()?;
        let secp = Secp256k1::new();
        let coordinator_pubkey = PublicKey::from_private_key(&secp, &coordinator_privkey);

        debug!(
            "Checking for escrow inputs to sign with pubkey: {}",
            coordinator_pubkey
        );

        let tx = psbt.unsigned_tx.clone();
        let mut escrow_inputs_signed = 0;

        for (idx, input) in psbt.inputs.iter_mut().enumerate() {
            if input.partial_sigs.contains_key(&coordinator_pubkey) {
                debug!("Input {} already has coordinator signature", idx);
                continue;
            }

            // Escrow inputs are the ones carrying a witness script.
            let witness_script = match &input.witness_script {
                Some(ws) => ws,
                None => continue,
            };

            let pubkey_bytes = coordinator_pubkey.to_bytes();
            if !witness_script
                .as_bytes()
                .windows(pubkey_bytes.len())
                .any(|window| window == pubkey_bytes)
            {
                debug!(
                    "Input {} witness script doesn't contain coordinator pubkey",
                    idx
                );
                continue;
            }

            let witness_utxo = input
                .witness_utxo
                .as_ref()
                .ok_or_else(|| anyhow!("Missing witness_utxo for escrow input {}", idx))?;

            let script_bytes = witness_utxo.script_pubkey.as_bytes();
            if script_bytes.len() >= 34 && script_bytes[0] == 0x00 && script_bytes[1] == 0x20 {
                let expected_hash = &script_bytes[2..34];
                let actual_hash = sha256::Hash::hash(witness_script.as_bytes()).to_byte_array();
                if expected_hash != actual_hash.as_slice() {
                    return Err(anyhow!("Witness script hash mismatch for input {}", idx));
                }
            }

            let mut cache = SighashCache::new(&tx);
            let sighash = cache.p2wsh_signature_hash(
                idx,
                witness_script,
                witness_utxo.value,
                EcdsaSighashType::All,
            )?;
            let message = Message::from_digest(sighash.to_byte_array());
            let sig = secp.sign_ecdsa(&message, &coordinator_privkey.inner);
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

    /// Builds the final witness of every escrow input that has both signatures.
    fn finalize_escrow_inputs(psbt: &mut Psbt) -> Result<(), anyhow::Error> {
        for (index, input) in psbt.inputs.iter_mut().enumerate() {
            if input.final_script_witness.is_some() {
                continue;
            }
            let Some(witness_script) = input.witness_script.clone() else {
                continue;
            };
            debug!("Finalizing escrow input {}", index);

            // For escrow miniscript: wsh(or_d(multi(2,A,B),and_v(v:pk(B),and_v(v:sha256(H),older(144)))))
            // The 2-of-2 multisig path needs: an empty element (OP_FALSE selects
            // the first branch), both signatures in script order, the script.
            let mut witness = Witness::new();
            witness.push([]);

            let script_bytes = witness_script.as_bytes();
            let mut pubkeys_in_script = Vec::new();
            let mut i = 0;
            while i < script_bytes.len() {
                if script_bytes[i] == 0x21 && i + 33 < script_bytes.len() {
                    if let Ok(pk) = PublicKey::from_slice(&script_bytes[i + 1..i + 34]) {
                        pubkeys_in_script.push(pk);
                        if pubkeys_in_script.len() == 2 {
                            break;
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
            for pubkey in &pubkeys_in_script {
                let sig = input.partial_sigs.get(pubkey).ok_or_else(|| {
                    anyhow!("Missing signature for pubkey {} in input {}", pubkey, index)
                })?;
                witness.push_ecdsa_signature(sig);
            }
            witness.push(witness_script.as_bytes());

            input.final_script_witness = Some(witness);
            input.partial_sigs.clear();
            input.witness_script = None;
        }
        Ok(())
    }

    fn is_psbt_fully_signed(psbt: &Psbt) -> bool {
        psbt.inputs.iter().enumerate().all(|(i, input)| {
            if input.final_script_witness.is_some() || input.final_script_sig.is_some() {
                return true;
            }
            let signed = if input.witness_script.is_some() {
                // 2-of-2 escrow input
                input.partial_sigs.len() >= 2
            } else {
                !input.partial_sigs.is_empty() || input.tap_key_sig.is_some()
            };
            if !signed {
                debug!("Input {} is not fully signed yet", i);
            }
            signed
        })
    }

    fn template_psbt(
        outputs: Vec<TxOut>,
        selected_utxos: &[OutPoint],
        foreign_utxos: Vec<ForeignUtxo>,
    ) -> Result<Psbt, anyhow::Error> {
        let mut inputs = Vec::new();
        let mut psbt_inputs = Vec::new();
        for outpoint in selected_utxos {
            inputs.push(TxIn {
                previous_output: *outpoint,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            });
            psbt_inputs.push(Input::default());
        }
        for foreign in foreign_utxos {
            inputs.push(TxIn {
                previous_output: foreign.outpoint,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            });
            psbt_inputs.push(foreign.psbt);
        }
        let unsigned_tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: inputs,
            output: outputs,
        };
        let mut psbt = Psbt::from_unsigned_tx(unsigned_tx)?;
        psbt.inputs = psbt_inputs;
        Ok(psbt)
    }
}

#[async_trait]
impl Bitcoin for BitcoinClient {
    fn get_network(&self) -> Network {
        self.network
    }

    async fn get_public_key(&self) -> Result<PublicKey, anyhow::Error> {
        let private_key = self.coordinator_private_key()?;
        let secp = Secp256k1::new();
        Ok(PublicKey::from_private_key(&secp, &private_key))
    }

    async fn get_derived_private_key(&self) -> Result<Scalar, anyhow::Error> {
        Scalar::from_slice(&self.coordinator_secret[..])
            .map_err(|e| anyhow!("Failed to convert private key to scalar: {}", e))
    }

    async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error> {
        let txid = *txid;
        self.with_electrum(move |client| client.transaction_get(&txid))
            .await
            .map_err(|e| anyhow!("Transaction {} not found: {}", txid, e))
    }

    async fn sign_psbt_with_escrow_support(&self, psbt: &mut Psbt) -> Result<bool, anyhow::Error> {
        let escrow_signed = self.sign_escrow_inputs(psbt).await?;
        if escrow_signed > 0 {
            debug!(
                "Signed {} escrow inputs with the coordinator key",
                escrow_signed
            );
        }

        let has_wallet_inputs = psbt
            .inputs
            .iter()
            .any(|input| input.witness_script.is_none());
        if has_wallet_inputs {
            *psbt = self.lnd.sign_psbt(psbt).await?;
        }

        Ok(Self::is_psbt_fully_signed(psbt))
    }

    async fn finalize_psbt_with_escrow_support(
        &self,
        psbt: &mut Psbt,
    ) -> Result<bool, anyhow::Error> {
        Self::finalize_escrow_inputs(psbt)?;

        let has_open_inputs = psbt
            .inputs
            .iter()
            .any(|input| input.final_script_witness.is_none() && input.final_script_sig.is_none());
        if has_open_inputs {
            // LND must be the last signer: escrow inputs are final by now.
            *psbt = self.lnd.finalize_psbt(psbt).await?;
        }

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
        self.lnd
            .fund_with_foreign(
                self.network,
                TxOut {
                    value: amount,
                    script_pubkey,
                },
                fee_rate,
                selected_utxos,
                foreign_utxos,
            )
            .await
    }

    async fn sign_psbt(&self, psbt: &mut Psbt) -> Result<bool, anyhow::Error> {
        *psbt = self.lnd.finalize_psbt(psbt).await?;
        Ok(psbt
            .inputs
            .iter()
            .all(|input| input.final_script_witness.is_some() || input.final_script_sig.is_some()))
    }

    async fn reserve_psbt_inputs_until(
        &self,
        psbt: &Psbt,
        deadline: u64,
    ) -> Result<(), anyhow::Error> {
        self.lnd.reserve_psbt_inputs_until(psbt, deadline).await
    }

    async fn release_psbt_inputs(&self, psbt: &Psbt) -> Result<(), anyhow::Error> {
        self.lnd.release_psbt_inputs(psbt).await
    }

    async fn payout_output_status(
        &self,
        outpoint: OutPoint,
        output: TxOut,
    ) -> Result<PayoutOutputStatus, anyhow::Error> {
        let url = self.payout_electrum_url.clone();
        // Separate short-lived connection: no general Electrum retry backlog may
        // delay a payout chain snapshot, and each blocking RPC has a short timeout.
        tokio::task::spawn_blocking(move || -> Result<_, electrum_client::Error> {
            let client = ElectrumClient::from_config(
                &url,
                ConfigBuilder::new().timeout(Some(3)).retry(0).build(),
            )?;
            let history = client.script_get_history(&output.script_pubkey)?;
            let confirmation_height = history
                .iter()
                .find(|item| item.tx_hash == outpoint.txid)
                .and_then(|item| u32::try_from(item.height).ok())
                .filter(|height| *height > 0);
            let unspent = client
                .script_list_unspent(&output.script_pubkey)?
                .iter()
                .any(|item| {
                    item.tx_hash == outpoint.txid
                        && item.tx_pos == outpoint.vout as usize
                        && item.value == output.value.to_sat()
                });
            let current_height = u32::try_from(client.block_headers_subscribe()?.height)
                .map_err(|_| electrum_client::Error::Message("Invalid chain height".into()))?;
            Ok(PayoutOutputStatus {
                confirmation_height,
                current_height,
                unspent,
            })
        })
        .await?
        .map_err(Into::into)
    }

    async fn script_funds(&self, script: ScriptBuf) -> Result<Option<ScriptFunds>, anyhow::Error> {
        let url = self.payout_electrum_url.clone();
        // Its own connection: a script with hundreds of outputs takes the index a while to
        // list, and must not hold up the lookups that share the main one.
        tokio::task::spawn_blocking(move || -> Result<_, electrum_client::Error> {
            let client = ElectrumClient::from_config(
                &url,
                ConfigBuilder::new().timeout(Some(20)).retry(0).build(),
            )?;
            let unspent = client.script_list_unspent(&script)?;
            Ok(Some(ScriptFunds::of(
                unspent.iter().map(|item| (item.value, item.height > 0)),
            )))
        })
        .await?
        .map_err(Into::into)
    }

    async fn spending_transaction(
        &self,
        outpoint: OutPoint,
        output: TxOut,
    ) -> Result<Option<Transaction>, anyhow::Error> {
        let url = self.payout_electrum_url.clone();
        tokio::task::spawn_blocking(move || -> Result<_, anyhow::Error> {
            let started = std::time::Instant::now();
            let client = ElectrumClient::from_config(
                &url,
                ConfigBuilder::new().timeout(Some(2)).retry(0).build(),
            )?;
            let history = client.script_get_history(&output.script_pubkey)?;
            // Avoid an unbounded script-history crawl on reused addresses.
            if history.len() > 32 {
                return Err(anyhow!("Script history exceeds support lookup limit"));
            }
            for item in history
                .iter()
                .rev()
                .filter(|item| item.tx_hash != outpoint.txid)
            {
                if started.elapsed() > Duration::from_secs(2) {
                    return Err(anyhow!("Support lookup deadline reached"));
                }
                let tx = client.transaction_get(&item.tx_hash)?;
                if tx.compute_txid() != item.tx_hash {
                    return Err(anyhow!("Transaction identity mismatch"));
                }
                if tx
                    .input
                    .iter()
                    .any(|input| input.previous_output == outpoint)
                {
                    return Ok(Some(tx));
                }
            }
            Ok(None)
        })
        .await?
    }

    async fn get_tx_confirmation_height(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error> {
        let txid = *txid;
        let result = self
            .with_electrum(move |client| {
                let tx = match client.transaction_get(&txid) {
                    Ok(tx) => tx,
                    Err(electrum_client::Error::Protocol(_)) => return Ok(None),
                    Err(e) => return Err(e),
                };
                // Any output script's history lists this transaction with its height.
                for output in &tx.output {
                    let history = client.script_get_history(&output.script_pubkey)?;
                    if let Some(entry) = history.iter().find(|entry| entry.tx_hash == txid) {
                        return Ok(Some(entry.height));
                    }
                }
                Ok(None)
            })
            .await?;
        debug!("Transaction {} confirmation height: {:?}", txid, result);
        Ok(result.and_then(|height| (height > 0).then_some(height as u32)))
    }

    async fn get_spendable_utxo(&self, amount_sats: u64) -> Result<WalletUtxo, anyhow::Error> {
        let amount = Amount::from_sat(amount_sats);
        self.list_utxos()
            .await
            .into_iter()
            .find(|utxo| utxo.is_confirmed() && utxo.txout.value >= amount)
            .ok_or_else(|| anyhow!("No utxos with the request amount available"))
    }

    async fn get_next_address(&self) -> Result<Address, anyhow::Error> {
        self.lnd.next_address(self.network).await
    }

    async fn get_current_height(&self) -> Result<u32, anyhow::Error> {
        self.lnd.block_height().await
    }

    async fn block_headers(
        &self,
        start: u32,
        count: u32,
    ) -> Result<Vec<BlockSummary>, anyhow::Error> {
        let count = count.min(2016) as usize;
        let headers = self
            .with_electrum(move |client| client.block_headers(start as usize, count))
            .await?
            .headers;
        Ok(headers
            .iter()
            .zip(start..)
            .map(|(header, height)| BlockSummary {
                height,
                hash: header.block_hash(),
                time: header.time,
            })
            .collect())
    }

    async fn get_confirmed_blockchain_time(&self, blocks: usize) -> Result<u64, anyhow::Error> {
        if blocks == 0 || blocks > 2016 {
            return Err(anyhow!(
                "Block time window must be between 1 and 2016 blocks"
            ));
        }
        let tip = self.get_current_height().await? as usize;
        let count = blocks;
        let start = tip.saturating_sub(count - 1);
        let mut times = self
            .with_electrum(move |client| client.block_headers(start, count))
            .await?
            .headers
            .iter()
            .map(|header| header.time as u64)
            .collect::<Vec<_>>();
        if times.len() != count {
            return Err(anyhow!(
                "Insufficient blocks available to calculate confirmed time"
            ));
        }
        // Oldest of the most recent `blocks` headers, as before.
        times.sort_unstable_by(|a, b| b.cmp(a));
        times
            .get(blocks - 1)
            .copied()
            .ok_or_else(|| anyhow!("Missing confirmed block time"))
    }

    /// Fee rates in sat/vB keyed by confirmation target in blocks.
    async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error> {
        let estimates = join_all(
            FEE_TARGETS
                .iter()
                .map(|target| async move { (*target, self.lnd.estimate_fee(*target).await) }),
        )
        .await;
        let mut rates = HashMap::new();
        for (target, estimate) in estimates {
            match estimate {
                Ok(rate) => {
                    rates.insert(target, rate.max(1.0));
                }
                Err(e) => warn!("No fee estimate for {} blocks: {}", target, e),
            }
        }
        if rates.is_empty() {
            return Err(anyhow!("LND returned no fee estimates"));
        }
        Ok(rates)
    }

    async fn estimate_fee(&self, conf_target: u16) -> Result<f64, anyhow::Error> {
        self.lnd.estimate_fee(conf_target).await
    }

    async fn broadcast(&self, transaction: &Transaction) -> Result<(), anyhow::Error> {
        //TODO: add child-pays-for-parent if fees are too low
        self.lnd.publish(transaction, "coordinator").await
    }

    async fn list_utxos(&self) -> Vec<WalletUtxo> {
        match self.lnd.list_unspent().await {
            Ok(utxos) => utxos,
            Err(e) => {
                error!("Failed to list LND utxos: {}", e);
                Vec::new()
            }
        }
    }

    async fn sync(&self) -> Result<(), anyhow::Error> {
        // LND keeps its own wallet in sync; report its view for the logs.
        let height = self.lnd.block_height().await?;
        debug!("LND wallet at height {}", height);
        Ok(())
    }

    async fn get_balance(&self) -> Result<WalletBalance, anyhow::Error> {
        self.lnd.balance().await
    }

    async fn get_outputs(&self) -> Result<Vec<WalletUtxo>, anyhow::Error> {
        self.lnd.list_unspent().await
    }

    async fn send_to_address(
        &self,
        send_options: SendOptions,
        selected_utxos: Vec<OutPoint>,
    ) -> Result<Txid, anyhow::Error> {
        let dest_addr = send_options.get_destination_address(self.network)?;
        if send_options.amount.is_none() && send_options.max_fee.is_some() {
            return Err(anyhow!("A maximum fee requires an explicit send amount; LND sweeps publish before the fee can be checked"));
        }

        // Inputs: the explicit selection, or every UTXO of the source address.
        let mut inputs = selected_utxos;
        if inputs.is_empty() {
            if let Some(from) = &send_options.address_from {
                let source = Address::from_str(from)?.require_network(self.network)?;
                inputs = self
                    .list_utxos()
                    .await
                    .into_iter()
                    .filter(|utxo| {
                        utxo.is_confirmed()
                            && Address::from_script(&utxo.txout.script_pubkey, self.network)
                                .is_ok_and(|address| address == source)
                    })
                    .map(|utxo| utxo.outpoint)
                    .collect();
                if inputs.is_empty() {
                    return Err(anyhow!("No spendable UTXOs found for address: {}", from));
                }
            }
        }

        let fee_rates = self.get_estimated_fee_rates().await?;
        let sat_per_vbyte = fee_rates.get(&6).copied().unwrap_or(1.0).ceil() as u64;

        let Some(amount_sats) = send_options.amount else {
            return self
                .lnd
                .send_all(&dest_addr, sat_per_vbyte.max(1), &inputs)
                .await;
        };

        let template = Self::template_psbt(
            vec![TxOut {
                value: Amount::from_sat(amount_sats),
                script_pubkey: dest_addr.script_pubkey(),
            }],
            &inputs,
            vec![],
        )?;
        let psbt = self
            .lnd
            .fund_psbt(
                &template,
                FeeRate::from_sat_per_vb(sat_per_vbyte.max(1))
                    .ok_or_else(|| anyhow!("fee rate {sat_per_vbyte} sat/vB is out of range"))?,
            )
            .await?;
        let transaction = async {
            send_options.validate_fee(&psbt)?;
            let finalized = self.lnd.finalize_psbt(&psbt).await?;
            finalized.extract_tx().map_err(anyhow::Error::from)
        }
        .await;
        let tx = match transaction {
            Ok(tx) => tx,
            Err(error) => {
                if let Err(release_error) = self.lnd.release_psbt_inputs(&psbt).await {
                    warn!(
                        "Failed to release inputs after send preparation failed: {}",
                        release_error
                    );
                }
                return Err(error);
            }
        };
        self.lnd.publish(&tx, "coordinator send").await?;
        Ok(tx.compute_txid())
    }
}

/// How often the sync watcher reads the header subscription.
const HEADER_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// First and longest wait before subscribing to block headers again.
const HEADER_RECONNECT_MIN: Duration = Duration::from_secs(5);
const HEADER_RECONNECT_MAX: Duration = Duration::from_secs(120);

/// Tells the sync watcher when a new block arrives.
#[async_trait]
pub trait HeaderSource: Send + Sync {
    /// Opens the subscription, replacing any previous one, and returns the tip height.
    async fn subscribe(&self) -> Result<u32, anyhow::Error>;
    /// Whether a new header arrived since the last call. An error means the
    /// subscription is lost and must be opened again.
    async fn new_header(&self) -> Result<bool, anyhow::Error>;
}

/// Electrum's `blockchain.headers.subscribe` on a connection of its own. The server
/// pushes each new header on that connection; the client reads pushed messages only
/// while it waits for a reply, so every check sends a `server.ping` first.
pub struct ElectrumHeaders {
    url: String,
    client: std::sync::Mutex<Option<Arc<ElectrumClient>>>,
}

impl ElectrumHeaders {
    pub fn new(url: String) -> Self {
        Self {
            url,
            client: std::sync::Mutex::new(None),
        }
    }

    fn set_client(&self, client: Option<Arc<ElectrumClient>>) {
        *self.client.lock().unwrap_or_else(|e| e.into_inner()) = client;
    }
}

#[async_trait]
impl HeaderSource for ElectrumHeaders {
    async fn subscribe(&self) -> Result<u32, anyhow::Error> {
        self.set_client(None);
        let url = self.url.clone();
        let (client, height) = tokio::task::spawn_blocking(move || {
            let client = ElectrumClient::from_config(
                &url,
                ConfigBuilder::new().timeout(Some(10)).retry(0).build(),
            )?;
            let tip = client.block_headers_subscribe()?;
            Ok::<_, electrum_client::Error>((client, tip.height))
        })
        .await??;
        self.set_client(Some(Arc::new(client)));
        u32::try_from(height).map_err(|_| anyhow!("Invalid chain height {}", height))
    }

    async fn new_header(&self) -> Result<bool, anyhow::Error> {
        let client = self
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| anyhow!("Not subscribed to block headers"))?;
        let result = tokio::task::spawn_blocking(move || {
            client.ping()?;
            let mut arrived = false;
            while client.block_headers_pop()?.is_some() {
                arrived = true;
            }
            Ok::<_, electrum_client::Error>(arrived)
        })
        .await?;
        if result.is_err() {
            self.set_client(None);
        }
        Ok(result?)
    }
}

/// When the sync watcher runs its next pass: at once for a new block, otherwise every
/// `subscribed_interval` while the header subscription is up and every `sync_interval`
/// while it is down.
struct SyncSchedule {
    sync_interval: Duration,
    subscribed_interval: Duration,
    subscribed: bool,
    reconnect_delay: Duration,
    reconnect_at: tokio::time::Instant,
    last_pass: Option<tokio::time::Instant>,
}

impl SyncSchedule {
    fn new(sync_interval: Duration, subscribed_interval: Duration) -> Self {
        Self {
            sync_interval,
            subscribed_interval,
            subscribed: false,
            reconnect_delay: HEADER_RECONNECT_MIN,
            reconnect_at: tokio::time::Instant::now(),
            last_pass: None,
        }
    }

    fn interval(&self) -> Duration {
        if self.subscribed {
            self.subscribed_interval
        } else {
            self.sync_interval
        }
    }

    /// Reads the header source, subscribing again when due, and returns whether a
    /// pass is due now.
    async fn pass_due(&mut self, headers: Option<&dyn HeaderSource>) -> bool {
        let now = tokio::time::Instant::now();
        let mut new_block = false;
        if let Some(headers) = headers {
            if !self.subscribed && now >= self.reconnect_at {
                match headers.subscribe().await {
                    Ok(height) => {
                        info!("Block header subscription connected at height {}", height);
                        self.subscribed = true;
                        self.reconnect_delay = HEADER_RECONNECT_MIN;
                    }
                    Err(e) => {
                        debug!("Block header subscription failed: {}", e);
                        self.retry_later(now);
                    }
                }
            } else if self.subscribed {
                match headers.new_header().await {
                    Ok(arrived) => new_block = arrived,
                    Err(e) => {
                        warn!(
                            "Block header subscription dropped, refreshing every {:?} until it \
                             reconnects: {}",
                            self.sync_interval, e
                        );
                        self.subscribed = false;
                        self.retry_later(now);
                    }
                }
            }
        }
        let due = new_block
            || self
                .last_pass
                .is_none_or(|at| now.duration_since(at) >= self.interval());
        if due {
            self.last_pass = Some(now);
        }
        due
    }

    fn retry_later(&mut self, now: tokio::time::Instant) {
        self.reconnect_at = now + self.reconnect_delay;
        self.reconnect_delay = (self.reconnect_delay * 2).min(HEADER_RECONNECT_MAX);
    }
}

pub struct BitcoinSyncWatcher {
    bitcoin: Arc<dyn Bitcoin>,
    cancel_token: CancellationToken,
    sync_interval: Duration,
    subscribed_interval: Duration,
    headers: Option<Arc<dyn HeaderSource>>,
}

impl BitcoinSyncWatcher {
    pub fn new(
        bitcoin: Arc<dyn Bitcoin>,
        cancel_token: CancellationToken,
        sync_interval: Duration,
    ) -> Self {
        Self {
            bitcoin,
            cancel_token,
            sync_interval,
            subscribed_interval: sync_interval,
            headers: None,
        }
    }

    /// Refresh when `headers` reports a new block, and only every `subscribed_interval`
    /// otherwise while its subscription is up.
    pub fn with_headers(
        mut self,
        headers: Arc<dyn HeaderSource>,
        subscribed_interval: Duration,
    ) -> Self {
        self.headers = Some(headers);
        self.subscribed_interval = subscribed_interval;
        self
    }

    pub async fn watch(&self) -> Result<(), anyhow::Error> {
        info!("Starting Bitcoin sync watcher");
        let mut schedule = SyncSchedule::new(self.sync_interval, self.subscribed_interval);
        // Without a header source, the loop wakes only for each pass.
        let tick = if self.headers.is_some() {
            HEADER_CHECK_INTERVAL
        } else {
            self.sync_interval
        };

        loop {
            if self.cancel_token.is_cancelled() {
                info!("Bitcoin sync watcher received cancellation");
                break;
            }

            if schedule.pass_due(self.headers.as_deref()).await {
                match self.bitcoin.sync().await {
                    Ok(_) => debug!("Bitcoin wallet sync completed successfully"),
                    Err(e) => error!("Bitcoin wallet sync error: {}", e),
                }
            }

            tokio::select! {
                _ = sleep(tick) => continue,
                _ = self.cancel_token.cancelled() => {
                    info!("Bitcoin sync watcher cancelled during sleep");
                    break;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod sync_schedule_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    };

    const FALLBACK: Duration = Duration::from_secs(15);
    const SUBSCRIBED: Duration = Duration::from_secs(120);

    #[derive(Default)]
    struct FakeHeaders {
        refuse: AtomicBool,
        subscriptions: AtomicUsize,
        /// Each check takes the next result; an empty queue means no new header.
        checks: Mutex<Vec<Result<bool, ()>>>,
    }

    impl FakeHeaders {
        fn push(&self, result: Result<bool, ()>) {
            self.checks.lock().unwrap().insert(0, result);
        }
    }

    #[async_trait]
    impl HeaderSource for FakeHeaders {
        async fn subscribe(&self) -> Result<u32, anyhow::Error> {
            if self.refuse.load(Ordering::SeqCst) {
                return Err(anyhow!("refused"));
            }
            self.subscriptions.fetch_add(1, Ordering::SeqCst);
            Ok(100)
        }

        async fn new_header(&self) -> Result<bool, anyhow::Error> {
            match self.checks.lock().unwrap().pop() {
                Some(Ok(arrived)) => Ok(arrived),
                Some(Err(())) => Err(anyhow!("connection lost")),
                None => Ok(false),
            }
        }
    }

    /// Advances one check interval at a time and returns the seconds at which passes ran.
    async fn passes(
        schedule: &mut SyncSchedule,
        headers: Option<&dyn HeaderSource>,
        seconds: u64,
    ) -> Vec<u64> {
        let start = tokio::time::Instant::now();
        let mut ran = Vec::new();
        for _ in 0..seconds {
            if schedule.pass_due(headers).await {
                ran.push(start.elapsed().as_secs());
            }
            tokio::time::advance(HEADER_CHECK_INTERVAL).await;
        }
        ran
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_subscription_the_pass_runs_at_the_fallback_interval() {
        let mut schedule = SyncSchedule::new(FALLBACK, SUBSCRIBED);
        assert_eq!(passes(&mut schedule, None, 46).await, [0, 15, 30, 45]);

        let refusing = FakeHeaders::default();
        refusing.refuse.store(true, Ordering::SeqCst);
        let mut schedule = SyncSchedule::new(FALLBACK, SUBSCRIBED);
        assert_eq!(
            passes(&mut schedule, Some(&refusing), 46).await,
            [0, 15, 30, 45]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn while_subscribed_the_sweep_is_slow_and_a_new_block_runs_a_pass_at_once() {
        let headers = FakeHeaders::default();
        let mut schedule = SyncSchedule::new(FALLBACK, SUBSCRIBED);
        assert_eq!(passes(&mut schedule, Some(&headers), 10).await, [0]);
        assert_eq!(schedule.interval(), SUBSCRIBED);

        headers.push(Ok(true));
        assert_eq!(passes(&mut schedule, Some(&headers), 1).await, [0]);
        // The sweep counts from the last pass, which the block ran.
        assert_eq!(passes(&mut schedule, Some(&headers), 121).await, [119]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_subscription_falls_back_and_reconnects() {
        let headers = FakeHeaders::default();
        let mut schedule = SyncSchedule::new(FALLBACK, SUBSCRIBED);
        assert_eq!(passes(&mut schedule, Some(&headers), 1).await, [0]);

        headers.push(Err(()));
        headers.refuse.store(true, Ordering::SeqCst);
        // Lost at second 1: passes return to every 15 s from the last pass.
        assert_eq!(passes(&mut schedule, Some(&headers), 31).await, [14, 29]);
        assert_eq!(schedule.interval(), FALLBACK);

        headers.refuse.store(false, Ordering::SeqCst);
        passes(&mut schedule, Some(&headers), 60).await;
        assert_eq!(headers.subscriptions.load(Ordering::SeqCst), 2);
        assert_eq!(schedule.interval(), SUBSCRIBED);
    }
}

#[cfg(test)]
mod fee_rate_tests {
    use super::*;

    #[test]
    fn time_critical_rates_add_ten_percent_or_a_quarter_sat_per_vb() {
        let kwu = |sat_per_vb| fee_rate_from_estimate(sat_per_vb).unwrap().to_sat_per_kwu();
        // At LND's floor the quarter sat/vB is the larger: 1.264 sat/vB, not 2.
        assert_eq!(kwu(1.012), 253 + 63);
        assert_eq!(kwu(2.2), 550 + 63);
        // Above 2.5 sat/vB, ten percent is.
        assert_eq!(kwu(3.0), 750 + 75);
        assert_eq!(kwu(20.0), 5_000 + 500);
        // Never below LND's floor, and nothing invalid.
        assert_eq!(kwu(0.0), 253);
        assert!(fee_rate_from_estimate(f64::NAN).is_err());
        assert!(fee_rate_from_estimate(-1.0).is_err());
        assert!(fee_rate_from_estimate(f64::MAX).is_err());
    }

    #[test]
    fn economy_fee_rate_targets_a_day_at_lnds_precision() {
        assert!(FEE_TARGETS.contains(&ECONOMY_FEE_TARGET));

        // LND's floor as the client reads it: 253 sat/kWU * 4 / 1000 sat/vB.
        let floor = 253.0 * 4.0 / 1000.0;
        let rates = HashMap::from([(1, floor), (ECONOMY_FEE_TARGET, floor)]);
        assert_eq!(
            economy_fee_rate(&rates).unwrap(),
            FeeRate::from_sat_per_kwu(253)
        );
        // Time-critical transactions add a margin: 0.25 sat/vB at the floor.
        assert_eq!(
            fee_rate_for_target(&rates, 1).unwrap(),
            FeeRate::from_sat_per_kwu(253 + 63)
        );

        // A day's estimate, not the next block's.
        let rates = HashMap::from([(1, 20.0), (6, 8.0), (ECONOMY_FEE_TARGET, 3.0)]);
        assert_eq!(
            economy_fee_rate(&rates).unwrap(),
            FeeRate::from_sat_per_kwu(750)
        );

        // The next-block estimate when there is no day's estimate, never below the floor.
        assert_eq!(
            economy_fee_rate(&HashMap::from([(1, 5.0)])).unwrap(),
            FeeRate::from_sat_per_kwu(1_250)
        );
        assert_eq!(
            economy_fee_rate(&HashMap::from([(ECONOMY_FEE_TARGET, 0.5)])).unwrap(),
            LND_FEE_RATE_FLOOR
        );
        assert!(economy_fee_rate(&HashMap::from([(6, 5.0)])).is_err());
        assert!(economy_fee_rate(&HashMap::from([(ECONOMY_FEE_TARGET, f64::NAN)])).is_err());
    }
}
