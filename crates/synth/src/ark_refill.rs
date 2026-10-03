//! Keeping ark-swapd's Ark wallet funded from an on-chain payer.
//!
//! ark-swapd pays every Arkade entry's escrow from its own Ark wallet and is paid in Lightning,
//! so the wallet only drains. It takes in coins only at its boarding address: once they confirm,
//! the next batch boards them. Every `check_interval_secs` this reads the wallet, and when what
//! it can fund, what waits to be boarded and what synth has sent but ark-swapd has not yet shown
//! fall below `low_water_sats`, the payer sends enough on-chain to reach `target_sats`, within
//! the per-send and daily caps.
//!
//! Each send is saved before it is made and stays pending until ark-swapd shows it: confirmed
//! at the boarding address, or boarded by a batch since it confirmed. No other is sent while
//! one is pending. An operator pauses sending from the dashboard, as they pause a scenario.

use std::sync::Arc;

use anyhow::{Context, Result};
use log::{info, warn};
use serde::Deserialize;
use sqlx::SqlitePool;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::ark_swap::{ArkSwap, ArkSwapConfig, ArkWallet};
use crate::db::SynthDb;
use crate::events::{Event, Events};
use crate::lnd::{Lnd, LndConfig, WalletTransaction};

/// What every refill transaction is labelled in the payer's wallet.
pub const LABEL: &str = "synth-ark-refill";

/// The control the dashboard pauses refills with, kept beside the scenarios' controls.
pub const CONTROL: &str = "ark_refill";

/// How long a refill that confirmed may go unseen by ark-swapd before it stops holding back the
/// next one.
const UNSEEN_AFTER_SECS: i64 = 6 * 3600;

/// How long a sent refill may be missing from the payer's wallet before it is taken as dropped.
const DROPPED_AFTER_SECS: i64 = 3600;

#[derive(Debug, Clone, Deserialize)]
pub struct ArkRefillConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Refill once what ark-swapd can fund, what waits to be boarded, and what synth sent that
    /// ark-swapd does not show yet together fall below this.
    #[serde(default = "default_low_water_sats", alias = "low_water_sat")]
    pub low_water_sats: u64,
    /// Send enough to bring the wallet back to this.
    #[serde(default = "default_target_sats", alias = "target_sat")]
    pub target_sats: u64,
    /// The most one refill sends.
    #[serde(default = "default_max_send_sats", alias = "max_send_sat")]
    pub max_send_sats: u64,
    /// The most refills send in any 24 hours.
    #[serde(default = "default_max_daily_sats", alias = "max_daily_sat")]
    pub max_daily_sats: u64,
    /// The least time between two refills, whether or not the first went through.
    #[serde(default = "default_min_interval_secs")]
    pub min_interval_secs: u64,
    #[serde(default = "default_check_interval_secs")]
    pub check_interval_secs: u64,
    /// ark-swapd's API, where the wallet is read.
    pub ark_swap: ArkSwapConfig,
    /// The node that pays.
    pub lnd: ArkRefillLndConfig,
}

/// The on-chain payer: an LND REST endpoint whose macaroon has `onchain:read`, `onchain:write`
/// and `info:read`.
#[derive(Debug, Clone, Deserialize)]
pub struct ArkRefillLndConfig {
    pub rest_url: String,
    #[serde(alias = "macaroon_path")]
    pub macaroon_file: std::path::PathBuf,
    #[serde(default, alias = "tls_cert_path")]
    pub tls_cert_file: Option<std::path::PathBuf>,
    /// The fee rate to send at. LND estimates one when this is unset.
    #[serde(default)]
    pub sat_per_vbyte: Option<u64>,
}

impl ArkRefillLndConfig {
    fn lnd(&self) -> LndConfig {
        LndConfig {
            rest_url: self.rest_url.clone(),
            macaroon_file: self.macaroon_file.clone(),
            tls_cert_file: self.tls_cert_file.clone(),
            // It sends no Lightning payments.
            fee_limit_sats: 0,
            payment_timeout_secs: 30,
        }
    }
}

fn default_low_water_sats() -> u64 {
    100_000
}

fn default_target_sats() -> u64 {
    400_000
}

fn default_max_send_sats() -> u64 {
    500_000
}

fn default_max_daily_sats() -> u64 {
    1_500_000
}

fn default_min_interval_secs() -> u64 {
    1800
}

fn default_check_interval_secs() -> u64 {
    300
}

impl ArkRefillConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.check_interval_secs > 0,
            "ark_refill.check_interval_secs must be positive"
        );
        anyhow::ensure!(
            self.low_water_sats <= self.target_sats,
            "ark_refill.low_water_sats must not be above target_sats"
        );
        anyhow::ensure!(
            self.max_send_sats > 0 && self.max_send_sats <= self.max_daily_sats,
            "ark_refill.max_send_sats must be positive and no more than max_daily_sats"
        );
        anyhow::ensure!(
            self.lnd.sat_per_vbyte != Some(0),
            "ark_refill.lnd.sat_per_vbyte must be positive when set"
        );
        Ok(())
    }

    fn limits(&self) -> Limits {
        Limits {
            low_water_sats: self.low_water_sats,
            target_sats: self.target_sats,
            max_send_sats: self.max_send_sats,
            max_daily_sats: self.max_daily_sats,
            min_interval_secs: self.min_interval_secs,
        }
    }
}

/// The settings a decision is made under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub low_water_sats: u64,
    pub target_sats: u64,
    pub max_send_sats: u64,
    pub max_daily_sats: u64,
    pub min_interval_secs: u64,
}

/// synth's own last refill, while ark-swapd does not show it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    pub amount_sats: u64,
    /// Whether the payer's wallet has it in a block.
    pub confirmed: bool,
}

/// What a check decides from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    pub paused: bool,
    pub payable_sats: u64,
    pub boarding_sats: u64,
    pub in_flight: Option<InFlight>,
    /// Since the last refill was attempted, whatever became of it.
    pub since_last_attempt_secs: Option<u64>,
    /// What refills that were not refused sent in the last 24 hours.
    pub sent_last_day_sats: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Send(u64),
    Skip(Skip),
}

/// Why a check sent nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Paused,
    /// The wallet, with what is on its way in, is at or above the low water mark.
    Enough,
    /// Coins wait at the boarding address for a batch; sending more would wait beside them.
    Boarding,
    /// synth's last refill is not in a block yet.
    Unconfirmed,
    /// synth's last refill confirmed, but ark-swapd does not show it yet.
    NotReflected,
    Cooldown {
        remaining_secs: u64,
    },
    /// The refill would take the last 24 hours past `max_daily_sats`.
    DailyCap,
}

impl Skip {
    pub fn describe(&self) -> String {
        match self {
            Skip::Paused => "paused from the dashboard".into(),
            Skip::Enough => "the wallet is above its low water mark".into(),
            Skip::Boarding => "coins wait at the boarding address for a batch".into(),
            Skip::Unconfirmed => "the last refill is not confirmed yet".into(),
            Skip::NotReflected => "ark-swapd does not show the last refill yet".into(),
            Skip::Cooldown { remaining_secs } => {
                format!("the last refill was too recent; {remaining_secs}s to go")
            }
            Skip::DailyCap => "the daily cap would be exceeded".into(),
        }
    }
}

/// Whether to refill, and how much: enough to bring the wallet and what is on its way in up to
/// the target, never more than one send may move.
pub fn decide(inputs: Inputs, limits: Limits) -> Decision {
    if inputs.paused {
        return Decision::Skip(Skip::Paused);
    }
    let in_flight = inputs.in_flight.map_or(0, |refill| refill.amount_sats);
    let held = inputs
        .payable_sats
        .saturating_add(inputs.boarding_sats)
        .saturating_add(in_flight);
    if held >= limits.low_water_sats {
        return Decision::Skip(Skip::Enough);
    }
    if inputs.boarding_sats > 0 {
        return Decision::Skip(Skip::Boarding);
    }
    match inputs.in_flight {
        Some(InFlight {
            confirmed: false, ..
        }) => return Decision::Skip(Skip::Unconfirmed),
        Some(InFlight {
            confirmed: true, ..
        }) => return Decision::Skip(Skip::NotReflected),
        None => {}
    }
    if let Some(since) = inputs.since_last_attempt_secs {
        if since < limits.min_interval_secs {
            return Decision::Skip(Skip::Cooldown {
                remaining_secs: limits.min_interval_secs - since,
            });
        }
    }
    let amount = limits
        .target_sats
        .saturating_sub(held)
        .min(limits.max_send_sats);
    if amount == 0 {
        return Decision::Skip(Skip::Enough);
    }
    if inputs.sent_last_day_sats.saturating_add(amount) > limits.max_daily_sats {
        return Decision::Skip(Skip::DailyCap);
    }
    Decision::Send(amount)
}

/// Whether `address` is an address on the payer's chain, as LND names it in `v1/getinfo`.
pub fn validate_address(address: &str, network: &str) -> Result<()> {
    use dlctix::bitcoin::{address::NetworkUnchecked, Address, Network};
    let network = match network {
        "mainnet" => Network::Bitcoin,
        "testnet" | "testnet3" => Network::Testnet,
        "testnet4" => Network::Testnet4,
        "signet" => Network::Signet,
        "regtest" => Network::Regtest,
        other => anyhow::bail!("the payer is on {other}, which refills do not support"),
    };
    let parsed: Address<NetworkUnchecked> = address
        .parse()
        .with_context(|| format!("ark-swapd's boarding address {address:?} is not an address"))?;
    anyhow::ensure!(
        parsed.is_valid_for_network(network),
        "ark-swapd's boarding address {address} is not for the payer's network ({network:?})"
    );
    Ok(())
}

/// A refill, as the database keeps it.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Refill {
    pub id: String,
    pub address: String,
    pub amount_sats: i64,
    /// The payer's block height when it was sent; its transaction is looked for from there.
    pub height: i64,
    /// `sending` until LND answers, then `sent`, `confirmed` and `arrived`; or `failed` when it
    /// was refused, `dropped` when the payer's wallet lost it, and `unseen` when ark-swapd never
    /// showed it.
    pub status: String,
    pub txid: Option<String>,
    pub error_message: Option<String>,
    pub payable_before_sats: i64,
    /// UNIX seconds, as are the two below.
    pub created_at: i64,
    pub updated_at: i64,
    pub confirmed_at: Option<i64>,
}

impl Refill {
    pub fn created_time(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(self.created_at).unwrap_or(OffsetDateTime::UNIX_EPOCH)
    }
}

pub(crate) async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ark_refills (
            id TEXT PRIMARY KEY,
            address TEXT NOT NULL,
            amount_sats INTEGER NOT NULL,
            height INTEGER NOT NULL,
            status TEXT NOT NULL,
            txid TEXT,
            error_message TEXT,
            payable_before_sats INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            confirmed_at INTEGER
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS ark_refills_by_creation ON ark_refills (created_at)")
        .execute(pool)
        .await?;
    Ok(())
}

/// The refills the database keeps, newest first.
pub async fn list(db: &SynthDb, limit: i64) -> Result<Vec<Refill>> {
    Ok(sqlx::query_as::<_, Refill>(
        "SELECT * FROM ark_refills ORDER BY created_at DESC, rowid DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db.pool())
    .await?)
}

/// The newest refill still on its way in, if any.
async fn pending(db: &SynthDb) -> Result<Option<Refill>> {
    Ok(sqlx::query_as::<_, Refill>(
        "SELECT * FROM ark_refills WHERE status IN ('sending', 'sent', 'confirmed') \
         ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .fetch_optional(db.pool())
    .await?)
}

async fn last_attempt_at(db: &SynthDb) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar::<_, Option<i64>>("SELECT max(created_at) FROM ark_refills")
            .fetch_one(db.pool())
            .await?,
    )
}

/// When a refill last went through to the payer's wallet, for the metric.
pub async fn last_success_at(db: &SynthDb) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT max(created_at) FROM ark_refills WHERE txid IS NOT NULL",
    )
    .fetch_one(db.pool())
    .await?)
}

/// What refills sent since `since`, counting all but those LND refused or lost.
async fn sent_since(db: &SynthDb, since: i64) -> Result<u64> {
    let sent: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(amount_sats), 0) FROM ark_refills \
         WHERE created_at >= ? AND status NOT IN ('failed', 'dropped')",
    )
    .bind(since)
    .fetch_one(db.pool())
    .await?;
    Ok(u64::try_from(sent).unwrap_or(0))
}

async fn begin(db: &SynthDb, refill: &Refill) -> Result<()> {
    sqlx::query(
        "INSERT INTO ark_refills (id, address, amount_sats, height, status, txid, error_message, \
         payable_before_sats, created_at, updated_at, confirmed_at) \
         VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, ?, ?, NULL)",
    )
    .bind(&refill.id)
    .bind(&refill.address)
    .bind(refill.amount_sats)
    .bind(refill.height)
    .bind(&refill.status)
    .bind(refill.payable_before_sats)
    .bind(refill.created_at)
    .bind(refill.updated_at)
    .execute(db.pool())
    .await?;
    Ok(())
}

async fn update(
    db: &SynthDb,
    id: &str,
    status: &str,
    txid: Option<&str>,
    error: Option<&str>,
    confirmed_at: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE ark_refills SET status = ?, txid = coalesce(?, txid), \
         error_message = coalesce(?, error_message), \
         confirmed_at = coalesce(?, confirmed_at), updated_at = ? WHERE id = ?",
    )
    .bind(status)
    .bind(txid)
    .bind(error)
    .bind(confirmed_at)
    .bind(OffsetDateTime::now_utc().unix_timestamp())
    .bind(id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Whether refills are paused from the dashboard. Missing means not paused; an unreadable
/// control pauses them.
pub async fn paused(db: &SynthDb) -> Result<bool> {
    Ok(
        !sqlx::query_scalar::<_, bool>("SELECT enabled FROM scenario_controls WHERE scenario = ?")
            .bind(CONTROL)
            .fetch_optional(db.pool())
            .await?
            .unwrap_or(true),
    )
}

pub async fn set_paused(db: &SynthDb, paused: bool) -> Result<()> {
    sqlx::query(
        "INSERT INTO scenario_controls (scenario, enabled, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(scenario) DO UPDATE SET enabled = excluded.enabled, \
         updated_at = excluded.updated_at",
    )
    .bind(CONTROL)
    .bind(!paused)
    .bind(OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// What a pending refill's transaction shows in the payer's wallet, and in ark-swapd's.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Progress {
    /// Still as it was.
    Waiting,
    /// A crash before LND answered left no transaction id; this is the one found for it.
    Found {
        txid: String,
        confirmed: bool,
    },
    Confirmed,
    Arrived,
    Dropped,
    Unseen,
    /// No transaction was made before the crash.
    NeverSent,
}

/// Where a pending refill got to: from the payer's wallet while it confirms, and from ark-swapd's
/// once it has.
fn progress(
    refill: &Refill,
    transactions: &[WalletTransaction],
    wallet: &ArkWallet,
    now: i64,
) -> Progress {
    let age = now - refill.created_at;
    let amount = u64::try_from(refill.amount_sats).unwrap_or(0);
    let matched = match &refill.txid {
        Some(txid) => transactions.iter().find(|tx| &tx.tx_hash == txid),
        // Only a refill to the same address for the same amount, labelled as synth labels them.
        None => transactions
            .iter()
            .find(|tx| tx.label == LABEL && tx.pays(&refill.address, amount)),
    };
    match refill.status.as_str() {
        "sending" => match matched {
            Some(tx) => Progress::Found {
                txid: tx.tx_hash.clone(),
                confirmed: tx.num_confirmations > 0,
            },
            None if age >= DROPPED_AFTER_SECS => Progress::NeverSent,
            None => Progress::Waiting,
        },
        "sent" => match matched {
            Some(tx) if tx.num_confirmations > 0 => Progress::Confirmed,
            Some(_) => Progress::Waiting,
            None if age >= DROPPED_AFTER_SECS => Progress::Dropped,
            None => Progress::Waiting,
        },
        "confirmed" => {
            let confirmed_at = refill.confirmed_at.unwrap_or(refill.updated_at);
            let boarded = wallet
                .last_board_success_at
                .is_some_and(|at| at >= confirmed_at);
            if wallet.boarding_sat > 0 || boarded {
                Progress::Arrived
            } else if now - confirmed_at >= UNSEEN_AFTER_SECS {
                Progress::Unseen
            } else {
                Progress::Waiting
            }
        }
        _ => Progress::Waiting,
    }
}

/// What the last check saw and decided, for the dashboard.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    pub checked_at: Option<OffsetDateTime>,
    pub next_check_at: Option<OffsetDateTime>,
    pub wallet: Option<ArkWallet>,
    pub decision: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct ArkRefiller {
    ark_swap: Arc<ArkSwap>,
    payer: Arc<Lnd>,
    config: ArkRefillConfig,
    db: SynthDb,
    events: Events,
    last: Arc<Mutex<Observation>>,
    operation: Arc<Mutex<()>>,
}

impl ArkRefiller {
    pub fn new(config: ArkRefillConfig, db: SynthDb, events: Events) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            ark_swap: Arc::new(ArkSwap::new(&config.ark_swap).context("open ark-swapd")?),
            payer: Arc::new(Lnd::new(&config.lnd.lnd()).context("open the refill payer")?),
            config,
            db,
            events,
            last: Arc::default(),
            operation: Arc::default(),
        })
    }

    pub fn config(&self) -> &ArkRefillConfig {
        &self.config
    }

    pub async fn last(&self) -> Observation {
        self.last.lock().await.clone()
    }

    pub async fn run_scheduled(&self) {
        info!(
            "Refilling ark-swapd's Ark wallet below {} sats, checking every {}s",
            self.config.low_water_sats, self.config.check_interval_secs
        );
        if let Ok(Some(at)) = last_success_at(&self.db).await {
            crate::server::metrics::record_ark_refill_last_success(at);
        }
        let interval = std::time::Duration::from_secs(self.config.check_interval_secs);
        loop {
            if let Err(error) = self.check().await {
                warn!("Ark wallet refill check failed: {error:#}");
                self.last.lock().await.error = Some(format!("{error:#}"));
            }
            self.last.lock().await.next_check_at = Some(OffsetDateTime::now_utc() + interval);
            self.events.send(Event::ArkRefillChecked);
            tokio::time::sleep(interval).await;
        }
    }

    /// Read the wallet, follow the pending refill, and send one if the wallet needs it.
    pub async fn check(&self) -> Result<Decision> {
        let _permit = self.operation.lock().await;
        let now = OffsetDateTime::now_utc();
        {
            let mut last = self.last.lock().await;
            last.checked_at = Some(now);
            last.error = None;
        }
        let wallet = self.ark_swap.wallet().await?;
        self.last.lock().await.wallet = Some(wallet.clone());

        let in_flight = self.follow_pending(&wallet, now.unix_timestamp()).await?;
        let since_last_attempt_secs = last_attempt_at(&self.db)
            .await?
            .map(|at| u64::try_from(now.unix_timestamp() - at).unwrap_or(0));
        let inputs = Inputs {
            paused: paused(&self.db).await.unwrap_or(true),
            payable_sats: wallet.payable(),
            boarding_sats: wallet.boarding_sat,
            in_flight,
            since_last_attempt_secs,
            sent_last_day_sats: sent_since(&self.db, now.unix_timestamp() - 86_400).await?,
        };
        let decision = decide(inputs, self.config.limits());
        self.last.lock().await.decision = Some(match decision {
            Decision::Send(amount) => format!("sending {amount} sats"),
            Decision::Skip(skip) => skip.describe(),
        });
        if let Decision::Send(amount) = decision {
            self.send(&wallet, amount).await?;
        }
        Ok(decision)
    }

    /// Bring the pending refill up to date. Returns it if it is still on its way in.
    async fn follow_pending(&self, wallet: &ArkWallet, now: i64) -> Result<Option<InFlight>> {
        let Some(refill) = pending(&self.db).await? else {
            return Ok(None);
        };
        let transactions = if refill.status == "confirmed" {
            Vec::new()
        } else {
            self.payer
                .transactions_since(u64::try_from(refill.height).unwrap_or(0))
                .await
                .context("look the refill up in the payer's wallet")?
        };
        let amount_sats = u64::try_from(refill.amount_sats).unwrap_or(0);
        let still = |confirmed: bool| -> Result<Option<InFlight>> {
            Ok(Some(InFlight {
                amount_sats,
                confirmed,
            }))
        };
        match progress(&refill, &transactions, wallet, now) {
            Progress::Waiting => still(refill.status == "confirmed"),
            Progress::Found { txid, confirmed } => {
                let status = if confirmed { "confirmed" } else { "sent" };
                update(
                    &self.db,
                    &refill.id,
                    status,
                    Some(&txid),
                    None,
                    confirmed.then_some(now),
                )
                .await?;
                info!(
                    "Found the refill {} made before a restart: {txid}",
                    refill.id
                );
                still(confirmed)
            }
            Progress::Confirmed => {
                update(&self.db, &refill.id, "confirmed", None, None, Some(now)).await?;
                still(true)
            }
            Progress::Arrived => {
                update(&self.db, &refill.id, "arrived", None, None, None).await?;
                Ok(None)
            }
            Progress::Dropped => {
                let reason = "the payer's wallet no longer has the transaction";
                warn!("Ark wallet refill {} dropped: {reason}", refill.id);
                crate::server::metrics::record_ark_refill_failure();
                update(&self.db, &refill.id, "dropped", None, Some(reason), None).await?;
                Ok(None)
            }
            Progress::NeverSent => {
                let reason = "no transaction was found for it after a restart";
                warn!("Ark wallet refill {} failed: {reason}", refill.id);
                crate::server::metrics::record_ark_refill_failure();
                update(&self.db, &refill.id, "failed", None, Some(reason), None).await?;
                Ok(None)
            }
            Progress::Unseen => {
                let reason = "ark-swapd never showed it at its boarding address";
                warn!("Ark wallet refill {} unseen: {reason}", refill.id);
                crate::server::metrics::record_ark_refill_failure();
                update(&self.db, &refill.id, "unseen", None, Some(reason), None).await?;
                Ok(None)
            }
        }
    }

    async fn send(&self, wallet: &ArkWallet, amount: u64) -> Result<()> {
        let chain = self.payer.chain().await.context("read the payer's chain")?;
        if let Err(error) = validate_address(&wallet.boarding_address, &chain.network) {
            crate::server::metrics::record_ark_refill_failure();
            return Err(error);
        }
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let refill = Refill {
            id: Uuid::now_v7().to_string(),
            address: wallet.boarding_address.clone(),
            amount_sats: i64::try_from(amount).context("refill amount")?,
            height: i64::try_from(chain.block_height).unwrap_or(0),
            status: "sending".into(),
            txid: None,
            error_message: None,
            payable_before_sats: i64::try_from(wallet.payable()).unwrap_or(i64::MAX),
            created_at: now,
            updated_at: now,
            confirmed_at: None,
        };
        // Saved first, so a crash while LND sends leaves a refill to look for, not a second one.
        begin(&self.db, &refill).await?;
        match self
            .payer
            .send_coins(
                &refill.address,
                amount,
                LABEL,
                self.config.lnd.sat_per_vbyte,
            )
            .await
        {
            Ok(txid) => {
                update(&self.db, &refill.id, "sent", Some(&txid), None, None).await?;
                info!(
                    "Refilled ark-swapd's Ark wallet, which could fund {} sats, with {amount} sats \
                     on-chain to {}: {txid}",
                    wallet.payable(),
                    refill.address
                );
                crate::server::metrics::record_ark_refill(amount, now);
                Ok(())
            }
            Err(error) => {
                let reason = format!("{error:#}");
                // A send LND answered with an error made no transaction. One whose answer was
                // lost may have, so it stays `sending` and the next check looks for it.
                let answered = !error.chain().any(|cause| {
                    cause.downcast_ref::<reqwest::Error>().is_some_and(|e| {
                        e.is_timeout() || e.is_request() || e.is_body() || e.is_decode()
                    })
                });
                if answered {
                    update(&self.db, &refill.id, "failed", None, Some(&reason), None).await?;
                }
                crate::server::metrics::record_ark_refill_failure();
                warn!("Ark wallet refill of {amount} sats failed: {reason}");
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            low_water_sats: 100_000,
            target_sats: 400_000,
            max_send_sats: 500_000,
            max_daily_sats: 1_500_000,
            min_interval_secs: 1800,
        }
    }

    fn drained() -> Inputs {
        Inputs {
            paused: false,
            payable_sats: 40_000,
            boarding_sats: 0,
            in_flight: None,
            since_last_attempt_secs: None,
            sent_last_day_sats: 0,
        }
    }

    #[test]
    fn a_check_sends_only_what_its_inputs_allow() {
        let cases = [
            (
                drained(),
                Decision::Send(360_000),
                "below low water: up to the target",
            ),
            (
                Inputs {
                    payable_sats: 142_572,
                    ..drained()
                },
                Decision::Skip(Skip::Enough),
                "above low water",
            ),
            (
                Inputs {
                    payable_sats: 100_000,
                    ..drained()
                },
                Decision::Skip(Skip::Enough),
                "exactly at low water",
            ),
            (
                Inputs {
                    paused: true,
                    ..drained()
                },
                Decision::Skip(Skip::Paused),
                "paused from the dashboard",
            ),
            (
                Inputs {
                    boarding_sats: 20_000,
                    ..drained()
                },
                Decision::Skip(Skip::Boarding),
                "coins already wait for a batch",
            ),
            (
                Inputs {
                    boarding_sats: 80_000,
                    ..drained()
                },
                Decision::Skip(Skip::Enough),
                "boarding counts toward the low water mark",
            ),
            (
                Inputs {
                    in_flight: Some(InFlight {
                        amount_sats: 30_000,
                        confirmed: false,
                    }),
                    ..drained()
                },
                Decision::Skip(Skip::Unconfirmed),
                "the previous refill is not in a block",
            ),
            (
                Inputs {
                    in_flight: Some(InFlight {
                        amount_sats: 30_000,
                        confirmed: true,
                    }),
                    ..drained()
                },
                Decision::Skip(Skip::NotReflected),
                "the previous refill confirmed but ark-swapd does not show it",
            ),
            (
                Inputs {
                    in_flight: Some(InFlight {
                        amount_sats: 360_000,
                        confirmed: false,
                    }),
                    ..drained()
                },
                Decision::Skip(Skip::Enough),
                "what is on its way in counts toward the low water mark",
            ),
            (
                Inputs {
                    since_last_attempt_secs: Some(600),
                    ..drained()
                },
                Decision::Skip(Skip::Cooldown {
                    remaining_secs: 1200,
                }),
                "within the minimum interval",
            ),
            (
                Inputs {
                    since_last_attempt_secs: Some(1800),
                    ..drained()
                },
                Decision::Send(360_000),
                "the minimum interval has passed",
            ),
            (
                Inputs {
                    sent_last_day_sats: 1_200_000,
                    ..drained()
                },
                Decision::Skip(Skip::DailyCap),
                "the send would pass the daily cap",
            ),
            (
                Inputs {
                    sent_last_day_sats: 1_140_000,
                    ..drained()
                },
                Decision::Send(360_000),
                "the send reaches the daily cap exactly",
            ),
        ];
        for (inputs, expected, case) in cases {
            assert_eq!(decide(inputs, limits()), expected, "{case}");
        }
    }

    #[test]
    fn a_refill_is_clamped_to_one_send() {
        let small = Limits {
            max_send_sats: 200_000,
            ..limits()
        };
        assert_eq!(decide(drained(), small), Decision::Send(200_000));
        let empty = Inputs {
            payable_sats: 0,
            ..drained()
        };
        assert_eq!(decide(empty, limits()), Decision::Send(400_000));
        let generous = Limits {
            target_sats: 2_000_000,
            ..limits()
        };
        assert_eq!(decide(empty, generous), Decision::Send(500_000));
    }

    #[test]
    fn the_config_reads_synths_names_and_the_short_ones() {
        let config: ArkRefillConfig = toml::from_str(
            r#"
enabled = true
low_water_sat = 100000
target_sats = 400000

[ark_swap]
url = "http://127.0.0.1:9737"
token_file = "/run/secrets/ark-swap.token"

[lnd]
rest_url = "https://payer.example:8080"
macaroon_path = "/run/secrets/refill.macaroon"
tls_cert_path = "/run/secrets/payer.tls.cert"
sat_per_vbyte = 2
"#,
        )
        .unwrap();
        assert!(config.enabled);
        assert_eq!(config.low_water_sats, 100_000);
        assert_eq!(config.max_send_sats, 500_000);
        assert_eq!(config.max_daily_sats, 1_500_000);
        assert_eq!(config.min_interval_secs, 1800);
        assert_eq!(config.check_interval_secs, 300);
        assert_eq!(config.lnd.sat_per_vbyte, Some(2));
        assert_eq!(
            config.lnd.macaroon_file,
            std::path::Path::new("/run/secrets/refill.macaroon")
        );
        config.validate().unwrap();
        let backwards = ArkRefillConfig {
            low_water_sats: 500_000,
            ..config.clone()
        };
        assert!(backwards.validate().is_err());
        let over = ArkRefillConfig {
            max_send_sats: 2_000_000,
            ..config
        };
        assert!(over.validate().is_err());
    }

    #[test]
    fn only_an_address_on_the_payers_chain_is_paid() {
        let signet = "tb1p2drzv0jzgnyppxnmycdne0tv24tujvqe6ayt7vstdw5gv58rcv4qtapn20";
        validate_address(signet, "signet").unwrap();
        assert!(validate_address(signet, "mainnet").is_err());
        assert!(validate_address("not an address", "signet").is_err());
        assert!(validate_address(signet, "simnet").is_err());
    }

    fn refill(status: &str) -> Refill {
        Refill {
            id: "r".into(),
            address: "tb1pboard".into(),
            amount_sats: 250_000,
            height: 100,
            status: status.into(),
            txid: (status != "sending").then(|| "f00d".to_string()),
            error_message: None,
            payable_before_sats: 40_000,
            created_at: 1_000,
            updated_at: 1_000,
            confirmed_at: (status == "confirmed").then_some(2_000),
        }
    }

    fn transaction(txid: &str, confirmations: u64) -> WalletTransaction {
        WalletTransaction {
            tx_hash: txid.into(),
            num_confirmations: confirmations,
            label: LABEL.into(),
            output_details: vec![crate::lnd::OutputDetail {
                address: "tb1pboard".into(),
                amount: 250_000,
            }],
        }
    }

    #[test]
    fn a_refill_is_followed_until_ark_swapd_shows_it() {
        let wallet = ArkWallet::default();
        let sent = refill("sent");
        assert_eq!(
            progress(&sent, &[transaction("f00d", 0)], &wallet, 1_100),
            Progress::Waiting
        );
        assert_eq!(
            progress(&sent, &[transaction("f00d", 1)], &wallet, 1_100),
            Progress::Confirmed
        );
        assert_eq!(progress(&sent, &[], &wallet, 1_100), Progress::Waiting);
        assert_eq!(
            progress(&sent, &[], &wallet, 1_000 + DROPPED_AFTER_SECS),
            Progress::Dropped
        );

        let confirmed = refill("confirmed");
        assert_eq!(progress(&confirmed, &[], &wallet, 2_100), Progress::Waiting);
        let boarding = ArkWallet {
            boarding_sat: 250_000,
            ..Default::default()
        };
        assert_eq!(
            progress(&confirmed, &[], &boarding, 2_100),
            Progress::Arrived
        );
        let boarded = ArkWallet {
            last_board_success_at: Some(2_050),
            ..Default::default()
        };
        assert_eq!(
            progress(&confirmed, &[], &boarded, 2_100),
            Progress::Arrived
        );
        let boarded_before = ArkWallet {
            last_board_success_at: Some(1_900),
            ..Default::default()
        };
        assert_eq!(
            progress(&confirmed, &[], &boarded_before, 2_100),
            Progress::Waiting,
            "a batch before it confirmed did not take it"
        );
        assert_eq!(
            progress(&confirmed, &[], &wallet, 2_000 + UNSEEN_AFTER_SECS),
            Progress::Unseen
        );
    }

    #[test]
    fn a_refill_cut_short_by_a_restart_is_found_by_its_label_and_output() {
        let sending = refill("sending");
        let wallet = ArkWallet::default();
        assert_eq!(
            progress(&sending, &[transaction("beef", 1)], &wallet, 1_100),
            Progress::Found {
                txid: "beef".into(),
                confirmed: true
            }
        );
        let unlabelled = WalletTransaction {
            label: String::new(),
            ..transaction("beef", 1)
        };
        assert_eq!(
            progress(&sending, std::slice::from_ref(&unlabelled), &wallet, 1_100),
            Progress::Waiting
        );
        assert_eq!(
            progress(&sending, &[unlabelled], &wallet, 1_000 + DROPPED_AFTER_SECS),
            Progress::NeverSent
        );
    }

    #[tokio::test]
    async fn refills_are_kept_counted_and_paused() {
        let directory = tempfile::tempdir().unwrap();
        let db = SynthDb::new(directory.path().join("synth.db").to_str().unwrap())
            .await
            .unwrap();
        assert!(!paused(&db).await.unwrap());
        set_paused(&db, true).await.unwrap();
        assert!(paused(&db).await.unwrap());
        set_paused(&db, false).await.unwrap();
        assert!(!paused(&db).await.unwrap());

        let mut first = refill("sending");
        first.id = "a".into();
        begin(&db, &first).await.unwrap();
        update(&db, "a", "sent", Some("f00d"), None, None)
            .await
            .unwrap();
        let mut refused = refill("sending");
        refused.id = "b".into();
        refused.created_at = 1_500;
        begin(&db, &refused).await.unwrap();
        update(&db, "b", "failed", None, Some("insufficient funds"), None)
            .await
            .unwrap();

        assert_eq!(
            sent_since(&db, 0).await.unwrap(),
            250_000,
            "refused sends nothing"
        );
        assert_eq!(sent_since(&db, 1_200).await.unwrap(), 0);
        assert_eq!(last_attempt_at(&db).await.unwrap(), Some(1_500));
        assert_eq!(last_success_at(&db).await.unwrap(), Some(1_000));
        let pending = pending(&db).await.unwrap().expect("the sent one");
        assert_eq!(
            (pending.id.as_str(), pending.txid.as_deref()),
            ("a", Some("f00d"))
        );
        assert_eq!(pending.status, "sent");
        let listed = list(&db, 10).await.unwrap();
        assert_eq!(
            listed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(
            listed[0].error_message.as_deref(),
            Some("insufficient funds")
        );
    }
}
