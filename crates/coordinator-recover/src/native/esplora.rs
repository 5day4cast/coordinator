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

    /// The fee rate Esplora estimates for confirmation within `blocks`, at least 1 sat/vB.
    pub async fn fee_rate(&self, blocks: u16) -> Result<FeeRate, String> {
        let estimates: std::collections::HashMap<String, f64> =
            self.get_json("/fee-estimates").await?.unwrap_or_default();
        // The estimate for the longest target within `blocks`.
        let rate = estimates
            .iter()
            .filter_map(|(target, rate)| Some((target.parse::<u16>().ok()?, *rate)))
            .filter(|(target, _)| *target <= blocks)
            .max_by_key(|(target, _)| *target)
            .map_or(1.0, |(_, rate)| rate.max(1.0));
        Ok(FeeRate::from_sat_per_kwu((rate * 250.0).ceil() as u64))
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
