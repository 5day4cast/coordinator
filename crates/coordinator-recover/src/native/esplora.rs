//! Chain state and broadcasting through any Esplora API.

use std::str::FromStr;

use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::{FeeRate, Network, Transaction, Txid};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::chain::{ChainView, Outspend, Query, TxStatus};

/// The public Esplora for `network`, if there is one: mempool.space on mainnet, Mutinynet's on
/// signet.
pub fn default_url(network: Network) -> Option<&'static str> {
    match network {
        Network::Bitcoin => Some("https://mempool.space/api"),
        Network::Signet => Some("https://mutinynet.com/api"),
        Network::Testnet => Some("https://mempool.space/testnet/api"),
        Network::Testnet4 => Some("https://mempool.space/testnet4/api"),
        _ => None,
    }
}

pub struct Esplora {
    client: Client,
    base: String,
}

/// Fee estimates in sat/vB by confirmation target, as Esplora's `/fee-estimates` gives them.
#[derive(Debug, Clone, Default)]
pub struct FeeEstimates(std::collections::BTreeMap<u16, f64>);

impl FeeEstimates {
    /// The estimate for the longest target within `blocks`, at least 1 sat/vB.
    pub fn within(&self, blocks: u16) -> FeeRate {
        let rate = self
            .0
            .range(..=blocks)
            .next_back()
            .map_or(1.0, |(_, rate)| rate.max(1.0));
        FeeRate::from_sat_per_kwu((rate * 250.0).ceil() as u64)
    }
}

#[derive(Deserialize)]
struct StatusJson {
    confirmed: bool,
    block_height: Option<u32>,
}

#[derive(Deserialize)]
struct OutspendJson {
    spent: bool,
    txid: Option<String>,
    status: Option<StatusJson>,
}

#[derive(Deserialize)]
struct BlockJson {
    mediantime: u64,
}

impl Esplora {
    pub fn new(base: &str) -> Self {
        Self {
            client: Client::new(),
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    async fn get(&self, path: &str) -> Result<Option<reqwest::Response>, String> {
        let url = format!("{}{path}", self.base);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => Ok(Some(response)),
            status => Err(format!("{url}: {status}")),
        }
    }

    async fn get_text(&self, path: &str) -> Result<String, String> {
        self.get(path)
            .await?
            .ok_or_else(|| format!("{path}: not found"))?
            .text()
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<Option<T>, String> {
        match self.get(path).await? {
            Some(response) => response.json().await.map(Some).map_err(|e| e.to_string()),
            None => Ok(None),
        }
    }

    /// The tip's height and median time past.
    pub async fn tip(&self) -> Result<(u32, u64), String> {
        let hash = self.get_text("/blocks/tip/hash").await?;
        let block: BlockJson = self
            .get_json(&format!("/block/{}", hash.trim()))
            .await?
            .ok_or("the tip block is not found")?;
        let height = self
            .get_text("/blocks/tip/height")
            .await?
            .trim()
            .parse()
            .map_err(|e| format!("tip height: {e}"))?;
        Ok((height, block.mediantime))
    }

    pub async fn tx_status(&self, txid: Txid) -> Result<Option<TxStatus>, String> {
        let status: Option<StatusJson> = self.get_json(&format!("/tx/{txid}/status")).await?;
        Ok(status.map(|status| TxStatus {
            confirmed_height: status.block_height.filter(|_| status.confirmed),
        }))
    }

    pub async fn outspend(&self, txid: Txid, vout: u32) -> Result<Outspend, String> {
        let outspend: Option<OutspendJson> = self
            .get_json(&format!("/tx/{txid}/outspend/{vout}"))
            .await?;
        let Some(outspend) = outspend.filter(|outspend| outspend.spent) else {
            return Ok(Outspend {
                spent_by: None,
                confirmed_height: None,
            });
        };
        Ok(Outspend {
            spent_by: outspend
                .txid
                .as_deref()
                .map(Txid::from_str)
                .transpose()
                .map_err(|e| e.to_string())?,
            confirmed_height: outspend
                .status
                .filter(|status| status.confirmed)
                .and_then(|status| status.block_height),
        })
    }

    pub async fn transaction(&self, txid: Txid) -> Result<Option<Transaction>, String> {
        match self.get(&format!("/tx/{txid}/hex")).await? {
            Some(response) => {
                let hex = response.text().await.map_err(|e| e.to_string())?;
                deserialize_hex(hex.trim())
                    .map(Some)
                    .map_err(|e| format!("transaction {txid}: {e}"))
            }
            None => Ok(None),
        }
    }

    /// The median time past of the block at `height`.
    pub async fn median_time_at(&self, height: u32) -> Result<u64, String> {
        let hash = self.get_text(&format!("/block-height/{height}")).await?;
        let block: BlockJson = self
            .get_json(&format!("/block/{}", hash.trim()))
            .await?
            .ok_or_else(|| format!("block {height} is not found"))?;
        Ok(block.mediantime)
    }

    pub async fn broadcast(&self, tx: &Transaction) -> Result<Txid, String> {
        let url = format!("{}/tx", self.base);
        let response = self
            .client
            .post(&url)
            .body(serialize_hex(tx))
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "broadcast of {} refused: {body}",
                tx.compute_txid()
            ));
        }
        Txid::from_str(body.trim()).map_err(|e| format!("broadcast answer {body}: {e}"))
    }

    /// Broadcast `parent` with its CPFP `child` as a package (`POST /txs/package`), so a parent
    /// below the mempool minimum still relays. On an Esplora without the package endpoint they go
    /// one after the other, which works when the parent pays the relay minimum on its own, as
    /// anchored contract transactions do. A parent the chain already has is not sent again.
    pub async fn broadcast_package(
        &self,
        parent: &Transaction,
        child: &Transaction,
    ) -> Result<(), String> {
        let url = format!("{}/txs/package", self.base);
        let response = self
            .client
            .post(&url)
            .json(&[serialize_hex(parent), serialize_hex(child)])
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            // Bitcoin Core's submitpackage answer: "success", or why the package was refused.
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|answer| answer["package_msg"].as_str().map(str::to_owned));
            return match message.as_deref() {
                None | Some("success") => Ok(()),
                Some(message) => Err(format!(
                    "package of {} and {} refused: {message}",
                    parent.compute_txid(),
                    child.compute_txid()
                )),
            };
        }
        if !matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ) {
            return Err(format!(
                "package of {} and {} refused: {body}",
                parent.compute_txid(),
                child.compute_txid()
            ));
        }
        if self.tx_status(parent.compute_txid()).await?.is_none() {
            self.broadcast(parent).await?;
        }
        self.broadcast(child).await.map(|_| ())
    }

    /// Esplora's fee estimates, sat/vB by confirmation target in blocks.
    pub async fn fee_estimates(&self) -> Result<FeeEstimates, String> {
        let estimates: std::collections::HashMap<String, f64> =
            self.get_json("/fee-estimates").await?.unwrap_or_default();
        Ok(FeeEstimates(
            estimates
                .into_iter()
                .filter_map(|(target, rate)| Some((target.parse::<u16>().ok()?, rate)))
                .collect(),
        ))
    }

    /// The fee rate Esplora estimates for confirmation within `blocks`, at least 1 sat/vB.
    pub async fn fee_rate(&self, blocks: u16) -> Result<FeeRate, String> {
        Ok(self.fee_estimates().await?.within(blocks))
    }

    /// The average seconds between the last `blocks` blocks, by their median times past: about
    /// 600 on mainnet, 30 on Mutinynet.
    pub async fn block_interval(&self, tip: u32, tip_mtp: u64, blocks: u32) -> Result<u32, String> {
        let start = tip.saturating_sub(blocks);
        let count = tip - start;
        if count == 0 {
            return Ok(0);
        }
        let earlier = self.median_time_at(start).await?;
        let seconds = tip_mtp.saturating_sub(earlier) / u64::from(count);
        u32::try_from(seconds).map_err(|e| format!("block interval: {e}"))
    }

    /// Answer every lookup `chain` is missing, until a round asks for nothing new.
    pub async fn fill(&self, chain: &mut ChainView, queries: Vec<Query>) -> Result<(), String> {
        for query in queries {
            match query {
                Query::Tx(txid) => {
                    let status = self.tx_status(txid).await?;
                    chain.insert_tx(txid, status);
                }
                Query::Outspend(outpoint) => {
                    let outspend = self.outspend(outpoint.txid, outpoint.vout).await?;
                    chain.insert_outspend(outpoint, outspend);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_estimate_for_the_longest_target_within() {
        let estimates = FeeEstimates([(1, 20.0), (6, 5.5), (144, 0.5)].into_iter().collect());
        assert_eq!(estimates.within(1), FeeRate::from_sat_per_kwu(5_000));
        assert_eq!(estimates.within(6), FeeRate::from_sat_per_kwu(1_375));
        assert_eq!(estimates.within(100), FeeRate::from_sat_per_kwu(1_375));
        // Never below 1 sat/vB, and 1 sat/vB with no estimates at all.
        assert_eq!(estimates.within(1_000), FeeRate::from_sat_per_kwu(250));
        assert_eq!(
            FeeEstimates::default().within(6),
            FeeRate::from_sat_per_kwu(250)
        );
    }
}
