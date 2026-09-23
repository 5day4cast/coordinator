//! Moving the test network's money back to where the players pay from.
//!
//! Players pay every entry from one node into the node holding the coordinator's invoices, so the
//! payer's side of their channel only shrinks, and once it is spent no scenario can enter a
//! competition. Payouts and refunds send some back, but never all of it: the coordinator keeps
//! its fee. So between scenarios the receiving node pays the payer back over their channel
//! whenever the payer's share falls too low.

use std::sync::Arc;

use anyhow::{Context, Result};
use log::{error, info};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::Mutex;

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

/// What the rebalancer last saw of the channel, for the dashboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Observation {
    pub checked_at: OffsetDateTime,
    pub channel: Option<Channel>,
}

#[derive(Clone)]
pub struct Rebalancer {
    payer: Arc<Lnd>,
    source: Arc<Lnd>,
    config: RebalanceConfig,
    db: SynthDb,
    last: Arc<Mutex<Option<Observation>>>,
}

impl Rebalancer {
    pub fn new(payer: &LndConfig, config: RebalanceConfig, db: SynthDb) -> Result<Self> {
        Ok(Self {
            payer: Arc::new(Lnd::new(payer).context("open the paying node")?),
            source: Arc::new(Lnd::new(&config.source).context("open the source node")?),
            config,
            db,
            last: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn last(&self) -> Option<Observation> {
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
                error!("Rebalance failed: {e:?}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(self.config.interval_secs)).await;
        }
    }

    /// Check the channel, and pay the payer back if its share is too low.
    pub async fn rebalance(&self) -> Result<Option<u64>> {
        let source = self.source.pubkey().await.context("read the source node")?;
        let channel = self
            .payer
            .channel_with(&source)
            .await
            .context("read the payer's channels")?;
        *self.last.lock().await = Some(Observation {
            checked_at: OffsetDateTime::now_utc(),
            channel: channel.clone(),
        });
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
                channel_id: channel.id.clone(),
                amount_sats: amount,
                local_before_sats: channel.local_sats,
                capacity_sats: channel.local_sats + channel.remote_sats,
                error: outcome.as_ref().err().map(|e| format!("{e:#}")),
            })
            .await?;
        outcome.map(|()| Some(amount))
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
        }
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
