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
}
