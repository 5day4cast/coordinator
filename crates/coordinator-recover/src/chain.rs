//! What the chain says about an entry's transactions, as the caller fetched it.
//!
//! The recovery logic does no I/O. It reads chain state from a [`ChainView`], which records every
//! lookup it could not answer. The caller fetches those ([`ChainView::missing`]), adds the
//! answers and runs the logic again, until nothing is missing. Each round follows the money one
//! transaction further, so a few rounds cover any entry.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use bitcoin::{OutPoint, Txid};
use serde::{Deserialize, Serialize};

/// A lookup the caller should make, by Esplora's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Query {
    /// `GET /tx/{txid}/status`.
    Tx(Txid),
    /// `GET /tx/{txid}/outspend/{vout}`.
    Outspend(OutPoint),
}

/// A transaction the chain knows: in the mempool, or confirmed at a height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxStatus {
    pub confirmed_height: Option<u32>,
}

/// Whether an output is spent, and by what.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outspend {
    pub spent_by: Option<Txid>,
    /// The spending transaction's height, once confirmed.
    pub confirmed_height: Option<u32>,
}

/// An answer to a [`Query`], as the browser page hands it back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// `None` when the chain does not know the transaction.
    Tx {
        txid: Txid,
        status: Option<TxStatus>,
    },
    Outspend {
        outpoint: OutPoint,
        outspend: Outspend,
    },
}

#[derive(Debug, Default)]
pub struct ChainView {
    /// The best block's height, once known.
    pub tip_height: Option<u32>,
    /// The best block's median time past: what a timestamp locktime is checked against.
    pub median_time_past: Option<u64>,
    /// Seconds between blocks lately, if the caller measured them: for saying about when a
    /// height is reached. Mutinynet makes a block about every 30 seconds, mainnet every 600.
    pub block_interval: Option<u32>,
    txs: BTreeMap<Txid, Option<TxStatus>>,
    outspends: BTreeMap<OutPoint, Outspend>,
    missing: RefCell<BTreeSet<Query>>,
}

impl ChainView {
    pub fn new(tip_height: u32, median_time_past: u64) -> Self {
        Self {
            tip_height: Some(tip_height),
            median_time_past: Some(median_time_past),
            ..Self::default()
        }
    }

    pub fn set_tip(&mut self, tip_height: u32, median_time_past: u64) {
        self.tip_height = Some(tip_height);
        self.median_time_past = Some(median_time_past);
    }

    /// The seconds between blocks lately; zero leaves it unknown.
    pub fn set_block_interval(&mut self, seconds: u32) {
        self.block_interval = (seconds > 0).then_some(seconds);
    }

    pub fn insert_tx(&mut self, txid: Txid, status: Option<TxStatus>) {
        self.txs.insert(txid, status);
    }

    pub fn insert_outspend(&mut self, outpoint: OutPoint, outspend: Outspend) {
        self.outspends.insert(outpoint, outspend);
    }

    pub fn answer(&mut self, answer: Answer) {
        match answer {
            Answer::Tx { txid, status } => self.insert_tx(txid, status),
            Answer::Outspend { outpoint, outspend } => self.insert_outspend(outpoint, outspend),
        }
    }

    /// The chain's view of `txid`: `None` until it is fetched, then `Some(None)` if the chain
    /// does not know it.
    pub fn tx(&self, txid: Txid) -> Option<Option<TxStatus>> {
        let known = self.txs.get(&txid).copied();
        if known.is_none() {
            self.missing.borrow_mut().insert(Query::Tx(txid));
        }
        known
    }

    /// The spender of `outpoint`, `None` until it is fetched.
    pub fn outspend(&self, outpoint: OutPoint) -> Option<Outspend> {
        let known = self.outspends.get(&outpoint).copied();
        if known.is_none() {
            self.missing.borrow_mut().insert(Query::Outspend(outpoint));
        }
        known
    }

    /// The lookups asked for since the last call, and not yet answered.
    pub fn missing(&self) -> Vec<Query> {
        std::mem::take(&mut *self.missing.borrow_mut())
            .into_iter()
            .filter(|query| match query {
                Query::Tx(txid) => !self.txs.contains_key(txid),
                Query::Outspend(outpoint) => !self.outspends.contains_key(outpoint),
            })
            .collect()
    }

    /// Whether a transaction confirmed at `height` has at least `blocks` confirmations in the
    /// next block, so that a `blocks` relative locktime on its output is satisfied there.
    pub fn matured(&self, height: u32, blocks: u16) -> bool {
        self.tip_height
            .is_some_and(|tip| tip + 1 >= height.saturating_add(u32::from(blocks)))
    }

    /// How many blocks are still to be mined before a transaction can go into block `height`:
    /// 0 once the next block can take it. `None` until the tip is known.
    pub fn blocks_until(&self, height: u32) -> Option<u32> {
        self.tip_height
            .map(|tip| height.saturating_sub(tip.saturating_add(1)))
    }

    /// About how long `blocks` take to mine, if the block interval is known.
    pub fn time_for(&self, blocks: u32) -> Option<u64> {
        self.block_interval
            .map(|interval| u64::from(blocks) * u64::from(interval))
    }

    /// When block `height` opens, in words: `block 1073 (in 72 blocks, about 36 min)`.
    pub fn describe_height(&self, height: u32) -> String {
        match self.blocks_until(height) {
            Some(0) => format!("block {height}, which the next block reaches"),
            Some(blocks) => match self.time_for(blocks) {
                Some(seconds) => format!(
                    "block {height} (in {blocks} blocks, about {})",
                    duration(seconds)
                ),
                None => format!("block {height} (in {blocks} blocks)"),
            },
            None => format!("block {height}"),
        }
    }
}

/// `seconds` in words, rounded: `less than a minute`, `36 min`, `5 h 20 min`, `3 d 4 h`.
pub fn duration(seconds: u64) -> String {
    if seconds < 60 {
        return "less than a minute".into();
    }
    let minutes = (seconds + 30) / 60;
    if minutes < 60 {
        return format!("{minutes} min");
    }
    if minutes < 48 * 60 {
        return format!("{} h {} min", minutes / 60, minutes % 60);
    }
    let hours = (seconds + 1_800) / 3_600;
    format!("{} d {} h", hours / 24, hours % 24)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_when_a_height_opens() {
        let mut chain = ChainView::default();
        assert_eq!(chain.blocks_until(110), None);
        assert_eq!(chain.describe_height(110), "block 110");
        chain.set_tip(100, 0);
        // The next block is 101, so a transaction for block 110 waits for 9 more.
        assert_eq!(chain.blocks_until(110), Some(9));
        assert_eq!(chain.blocks_until(101), Some(0));
        assert_eq!(chain.blocks_until(50), Some(0));
        assert_eq!(chain.describe_height(110), "block 110 (in 9 blocks)");
        chain.set_block_interval(30);
        assert_eq!(
            chain.describe_height(110),
            "block 110 (in 9 blocks, about 5 min)"
        );
        assert_eq!(
            chain.describe_height(101),
            "block 101, which the next block reaches"
        );
        chain.set_block_interval(0);
        assert_eq!(chain.block_interval, None);
    }

    #[test]
    fn rounds_durations() {
        assert_eq!(duration(0), "less than a minute");
        assert_eq!(duration(59), "less than a minute");
        assert_eq!(duration(89), "1 min");
        assert_eq!(duration(36 * 60), "36 min");
        assert_eq!(duration(5 * 3_600 + 20 * 60), "5 h 20 min");
        assert_eq!(duration(3 * 86_400 + 4 * 3_600), "3 d 4 h");
    }
}
