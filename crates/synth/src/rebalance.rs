//! Moving the test network's money back to where the scenarios spend it from.
//!
//! Every entry moves money in two directions that never reverse on their own:
//!
//! - Lightning: players pay from one node into the node holding the coordinator's invoices, so
//!   the payer's side of their channel only shrinks. The receiving node pays the payer back over
//!   their channel whenever the payer's share falls too low.
//! - Arkade: ark-swapd pays each escrow from its own Arkade wallet and is paid in Lightning, so
//!   that wallet only shrinks. The payer sends it coins on-chain at its boarding address whenever
//!   it runs low, and ark-swapd boards them once they confirm.

use std::sync::Arc;

use anyhow::{Context, Result};
use log::{error, info};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::ark_swap::{ArkSwap, ArkSwapConfig, ArkWallet};
use crate::db::{Rebalance, SynthDb};
use crate::lnd::{Channel, Lnd, LndConfig};

#[derive(Debug, Clone, Deserialize)]
pub struct RebalanceConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    /// The node entries are paid to, which pays the balance back. It needs a macaroon that may
    /// send payments; the payer's needs to create invoices too.
    pub source: LndConfig,
    /// Rebalance once the payer holds less than this percentage of the channel.
    #[serde(default = "default_low_percent")]
    pub low_percent: u64,
    /// Rebalance up to this percentage.
    #[serde(default = "default_target_percent")]
    pub target_percent: u64,
    /// The most one rebalance moves.
    #[serde(default = "default_max_sats")]
    pub max_sats: u64,
    /// Keeping ark-swapd's Arkade wallet funded. The payer's macaroon must also be able to send
    /// on-chain.
    #[serde(default)]
    pub arkade: Option<ArkadeTopUpConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArkadeTopUpConfig {
    pub ark_swap: ArkSwapConfig,
    /// Top up once ark-swapd can fund less than this.
    #[serde(default = "default_arkade_low_sats")]
    pub low_sats: u64,
    /// What one top-up sends.
    #[serde(default = "default_arkade_top_up_sats")]
    pub top_up_sats: u64,
    /// How long a top-up gets to confirm and board before another is sent.
    #[serde(default = "default_arkade_settle_secs")]
    pub settle_secs: u64,
}

fn default_arkade_low_sats() -> u64 {
    50_000
}

fn default_arkade_top_up_sats() -> u64 {
    200_000
}

fn default_arkade_settle_secs() -> u64 {
    3600
}

fn default_interval_secs() -> u64 {
    600
}

fn default_low_percent() -> u64 {
    30
}

fn default_target_percent() -> u64 {
    50
}

fn default_max_sats() -> u64 {
    200_000
}

/// What the rebalancer last saw, for the dashboard.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Observation {
    pub checked_at: Option<OffsetDateTime>,
    pub channel: Option<Channel>,
    pub arkade: Option<ArkWallet>,
}

/// What one rebalance moved, by leg.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Moved {
    pub channel_sats: Option<u64>,
    pub arkade_sats: Option<u64>,
}

#[derive(Clone)]
pub struct Rebalancer {
    payer: Arc<Lnd>,
    source: Arc<Lnd>,
    ark_swap: Option<Arc<ArkSwap>>,
    config: RebalanceConfig,
    db: SynthDb,
    last: Arc<Mutex<Observation>>,
}

/// The kinds of rebalance the database records.
const CHANNEL: &str = "channel";
const ARKADE: &str = "arkade";

impl Rebalancer {
    pub fn new(payer: &LndConfig, config: RebalanceConfig, db: SynthDb) -> Result<Self> {
        let ark_swap = match &config.arkade {
            Some(arkade) => Some(Arc::new(
                ArkSwap::new(&arkade.ark_swap).context("open ark-swapd")?,
            )),
            None => None,
        };
        Ok(Self {
            payer: Arc::new(Lnd::new(payer).context("open the paying node")?),
            source: Arc::new(Lnd::new(&config.source).context("open the source node")?),
            ark_swap,
            config,
            db,
            last: Arc::new(Mutex::new(Observation::default())),
        })
    }

    pub async fn last(&self) -> Observation {
        self.last.lock().await.clone()
    }

    pub fn config(&self) -> &RebalanceConfig {
        &self.config
    }

    pub async fn run_scheduled(&self) {
        info!(
            "Rebalancing every {}s below {}% of the channel",
            self.config.interval_secs, self.config.low_percent
        );
        loop {
            if let Err(e) = self.rebalance().await {
                error!("Rebalance failed: {e:#}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(self.config.interval_secs)).await;
        }
    }

    /// Check both legs, moving money on each that has run low. One leg failing does not stop the
    /// other.
    pub async fn rebalance(&self) -> Result<Moved> {
        self.last.lock().await.checked_at = Some(OffsetDateTime::now_utc());
        let channel = self.rebalance_channel().await;
        let arkade = match &self.ark_swap {
            Some(ark_swap) => self.top_up_arkade(ark_swap).await,
            None => Ok(None),
        };
        match (channel, arkade) {
            (Ok(channel_sats), Ok(arkade_sats)) => Ok(Moved {
                channel_sats,
                arkade_sats,
            }),
            (Err(e), Ok(_)) | (Ok(_), Err(e)) => Err(e),
            (Err(channel), Err(arkade)) => Err(anyhow::anyhow!("{channel:#}; and {arkade:#}")),
        }
    }

    /// Check the channel, and pay the payer back if its share is too low.
    async fn rebalance_channel(&self) -> Result<Option<u64>> {
        let source = self.source.pubkey().await.context("read the source node")?;
        let channel = self
            .payer
            .channel_with(&source)
            .await
            .context("read the payer's channels")?;
        self.last.lock().await.channel = channel.clone();
        let channel =
            channel.context("the payer has no active channel with the source node to rebalance")?;

        let Some(amount) = amount_to_move(&channel, &self.config) else {
            return Ok(None);
        };
        info!(
            "Rebalancing {amount} sats to the payer, which holds {} of {} sats",
            channel.local_sats,
            channel.local_sats + channel.remote_sats
        );
        let outcome = self.move_sats(&channel, amount).await;
        self.db
            .record_rebalance(&Rebalance {
                kind: CHANNEL,
                channel_id: channel.id.clone(),
                amount_sats: amount,
                local_before_sats: channel.local_sats,
                capacity_sats: channel.local_sats + channel.remote_sats,
                txid: None,
                error: outcome.as_ref().err().map(|e| format!("{e:#}")),
            })
            .await?;
        outcome.map(|()| Some(amount))
    }

    /// Send ark-swapd's wallet coins on-chain if it is running low and no earlier top-up is still
    /// on its way in.
    async fn top_up_arkade(&self, ark_swap: &ArkSwap) -> Result<Option<u64>> {
        let Some(config) = &self.config.arkade else {
            return Ok(None);
        };
        let wallet = ark_swap.wallet().await?;
        self.last.lock().await.arkade = Some(wallet.clone());
        let last_top_up = self.db.last_rebalance_at(ARKADE).await?;
        let since =
            last_top_up.map(|at| (OffsetDateTime::now_utc() - at).whole_seconds().max(0) as u64);
        if !needs_top_up(wallet.spendable_sat(), since, config) {
            return Ok(None);
        }
        let amount = config.top_up_sats;
        info!(
            "Topping up ark-swapd, which can fund {} sats, with {amount} sats on-chain",
            wallet.spendable_sat()
        );
        let sent = self
            .payer
            .send_on_chain(&wallet.boarding_address, amount, "synth: top up ark-swapd")
            .await
            .context("have the payer send ark-swapd coins on-chain");
        self.db
            .record_rebalance(&Rebalance {
                kind: ARKADE,
                channel_id: "ark-swapd".to_string(),
                amount_sats: amount,
                local_before_sats: wallet.spendable_sat(),
                capacity_sats: 0,
                txid: sent.as_ref().ok().cloned(),
                error: sent.as_ref().err().map(|e| format!("{e:#}")),
            })
            .await?;
        sent.map(|_| Some(amount))
    }

    async fn move_sats(&self, channel: &Channel, amount: u64) -> Result<()> {
        let invoice = self
            .payer
            .invoice(amount, "synth rebalance")
            .await
            .context("have the payer invoice the source")?;
        self.source
            .pay_through(&invoice, &channel.id)
            .await
            .context("have the source pay the payer")
    }
}

/// Whether ark-swapd needs coins: it can fund less than the low mark, and no top-up has been sent
/// recently enough to still be confirming or boarding.
fn needs_top_up(
    spendable_sat: u64,
    since_last_top_up_secs: Option<u64>,
    config: &ArkadeTopUpConfig,
) -> bool {
    spendable_sat < config.low_sats
        && since_last_top_up_secs.is_none_or(|since| since >= config.settle_secs)
}

/// How much to move to bring the payer back to its target share, if it has fallen below the low
/// one. Balances exclude the channel reserve and commitment fees, so the target is of what the
/// two sides hold, not of the channel's capacity.
fn amount_to_move(channel: &Channel, config: &RebalanceConfig) -> Option<u64> {
    let held = channel.local_sats + channel.remote_sats;
    if held == 0 || channel.local_sats * 100 >= held * config.low_percent {
        return None;
    }
    let target = held * config.target_percent / 100;
    let amount = target
        .saturating_sub(channel.local_sats)
        .min(config.max_sats)
        .min(channel.remote_sats);
    (amount > 0).then_some(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RebalanceConfig {
        RebalanceConfig {
            enabled: true,
            interval_secs: 600,
            source: LndConfig {
                rest_url: String::new(),
                macaroon_file: "unused".into(),
                tls_cert_file: None,
                fee_limit_sats: 0,
                payment_timeout_secs: 60,
            },
            low_percent: 30,
            target_percent: 50,
            max_sats: 200_000,
            arkade: None,
        }
    }

    fn arkade() -> ArkadeTopUpConfig {
        ArkadeTopUpConfig {
            ark_swap: ArkSwapConfig {
                url: String::new(),
                token_file: "unused".into(),
            },
            low_sats: 50_000,
            top_up_sats: 200_000,
            settle_secs: 3600,
        }
    }

    #[test]
    fn ark_swapd_is_topped_up_only_when_low_and_no_top_up_is_on_its_way() {
        assert!(!needs_top_up(60_000, None, &arkade()), "not low yet");
        assert!(
            needs_top_up(200, None, &arkade()),
            "drained, and never topped up"
        );
        assert!(
            !needs_top_up(200, Some(600), &arkade()),
            "the last top-up may still be confirming or boarding"
        );
        assert!(
            needs_top_up(200, Some(3600), &arkade()),
            "a top-up that never arrived is tried again"
        );
    }

    fn channel(local_sats: u64, remote_sats: u64) -> Channel {
        Channel {
            id: "1".into(),
            local_sats,
            remote_sats,
        }
    }

    #[test]
    fn a_channel_above_the_low_share_is_left_alone() {
        assert_eq!(amount_to_move(&channel(300_000, 700_000), &config()), None);
        assert_eq!(
            amount_to_move(&channel(963_503, 1_035_218), &config()),
            None
        );
    }

    #[test]
    fn a_drained_payer_is_brought_back_to_its_target_share() {
        assert_eq!(
            amount_to_move(&channel(250_000, 750_000), &config()),
            Some(200_000),
            "never more than one rebalance may move"
        );
        let generous = RebalanceConfig {
            max_sats: 1_000_000,
            ..config()
        };
        assert_eq!(
            amount_to_move(&channel(250_000, 750_000), &generous),
            Some(250_000)
        );
    }

    #[test]
    fn an_empty_channel_has_nothing_to_move() {
        assert_eq!(amount_to_move(&channel(0, 0), &config()), None);
    }
}
