//! Fee-bumping the coordinator's own stuck settlement transactions through their anchors.
//!
//! Contracts built since dlctix 0.2 put a pay-to-anchor output on their outcome, expiry and
//! split transactions. Each settlement step of a competition whose outcome (or expiry) or split
//! transaction is out and unconfirmed looks at it, at most every two minutes
//! ([`CHECK_EVERY`]), following `[cpfp_settings]`:
//!
//! 1. The transaction must have an anchor and have waited `after_secs` since it was broadcast.
//!    Contracts without anchors are left as they are.
//! 2. An outcome or expiry transaction the mempool no longer has is broadcast again. A split
//!    the chain backend does not know is left to settlement, which owns its witness.
//! 3. The target is the local estimate for `conf_target` blocks, with the margin every
//!    time-critical transaction gets. An attested outcome transaction within
//!    `urgent_within_secs` of the contract's expiry aims for `urgent_conf_target` instead: from
//!    the expiry on, the pre-signed expiry transaction is valid too and could take its place.
//! 4. Only a transaction paying less than the target on its own is bumped, and one bumped
//!    before only once the target is a quarter above that bump's, so the new child can replace
//!    the old one.
//! 5. The child spends the anchor and the smallest confirmed wallet coin that covers it, leased
//!    while the child is built, and pays its change to a new wallet address. It pays at most
//!    `max_fee_percent` of what its parent spends; past that, the rate the budget allows.
//!
//! Every attempt is counted in `coordinator_cpfp_bumps_total`, and what the children paid in
//! `coordinator_cpfp_fees_sat_total`. The child goes out through LND alone, so a parent below
//! the mempool's minimum fee, which needs package relay, is reported as `parent_rejected`.

use super::*;
use crate::config::CpfpSettings;
use crate::infra::bitcoin::{fee_rate_from_estimate, WalletUtxo};
use crate::metrics::{CPFP_BUMPS, CPFP_FEES_SAT};
use bitcoin::Witness;
use dlctix::{anchor, CpfpFundingInput, SignedContract};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

/// A child spending the anchor and one wallet coin into one change output is about 150 vB; a
/// coin must cover this much at the target besides the parent's shortfall.
const CHILD_VBYTES: u64 = 200;
/// What a coin must leave over the child's fee, so the change is not dust.
const CHANGE_MARGIN: Amount = Amount::from_sat(1_000);
/// How often one transaction is looked at, however often its competition steps.
const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(120);
/// How long the chosen wallet coin stays leased; the child spends it within seconds.
const COIN_LEASE_SECS: u64 = 600;
/// Transactions remembered before the confirmed ones are forgotten.
const MAX_REMEMBERED: usize = 10_000;

/// The settings and what the coordinator remembers of the transactions it fee-bumps.
pub(super) struct AnchorBumps {
    settings: CpfpSettings,
    parents: Mutex<HashMap<Txid, ParentState>>,
}

impl Default for AnchorBumps {
    /// Off, as for a coordinator built without `with_cpfp`.
    fn default() -> Self {
        Self {
            settings: CpfpSettings {
                enabled: false,
                ..CpfpSettings::default()
            },
            parents: Mutex::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ParentState {
    checked_at: Option<Instant>,
    /// Confirmed, or a split the chain backend does not know: nothing more to do.
    done: bool,
    /// The target at the last attempt that got as far as choosing a rate.
    target: Option<FeeRate>,
    /// The rate the last child that went out lifted the pair to.
    child_rate: Option<FeeRate>,
}

/// Which settlement transaction is waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Settling {
    Outcome,
    Expiry,
    Split,
}

impl Settling {
    fn label(self) -> &'static str {
        match self {
            Settling::Outcome => "outcome",
            Settling::Expiry => "expiry",
            Settling::Split => "split",
        }
    }
}

/// A waiting transaction whose check is due.
struct Candidate {
    settling: Settling,
    txid: Txid,
    /// As settlement stored it (outcome or expiry, witness included) or built it (split).
    tx: Transaction,
}

/// What a check did.
#[derive(Debug, PartialEq, Eq)]
enum Bumped {
    /// Confirmed, already paying enough, or not due again yet.
    Nothing,
    Broadcast {
        child: Txid,
        rate: FeeRate,
        fee: Amount,
        capped: bool,
    },
    OverBudget,
    NoCoin {
        needed: Amount,
    },
    ParentRejected(String),
}

/// The rate a child lifts its parent to.
#[derive(Debug, PartialEq, Eq)]
enum ChildRate {
    Target(FeeRate),
    /// The most the budget allows, below the target.
    Capped(FeeRate),
    /// The budget allows no more than the parent pays on its own.
    OverBudget,
}

/// Whether a transaction last bumped for `previous` should be bumped again for `target`: once
/// the target is a quarter higher, enough for the new child to replace the old one.
fn rebump_due(previous: Option<FeeRate>, target: FeeRate) -> bool {
    previous.is_none_or(|previous| {
        target.to_sat_per_kwu() >= previous.to_sat_per_kwu().saturating_mul(5) / 4
    })
}

/// The weight of a wallet coin's input once LND signs it, for the scripts LND's wallet uses.
fn wallet_input_weight(script_pubkey: &ScriptBuf) -> Option<InputWeightPrediction> {
    if script_pubkey.is_p2tr() {
        Some(InputWeightPrediction::P2TR_KEY_DEFAULT_SIGHASH)
    } else if script_pubkey.is_p2wpkh() {
        Some(InputWeightPrediction::P2WPKH_MAX)
    } else if script_pubkey.is_p2sh() {
        // Nested P2WPKH: a 22-byte redeem script push, then a signature and a key.
        Some(InputWeightPrediction::new(23, [72, 33]))
    } else {
        None
    }
}

/// The rate for a child of a parent paying `parent_fee` over `parent_vsize`: `target`, unless
/// the child would pay more than `budget`.
fn child_rate(target: FeeRate, parent_fee: Amount, parent_vsize: u64, budget: Amount) -> ChildRate {
    let package = parent_vsize.saturating_add(CHILD_VBYTES);
    let wanted = target.fee_vb(package).unwrap_or(Amount::MAX);
    if wanted
        .checked_sub(parent_fee)
        .is_none_or(|child_fee| child_fee <= budget)
    {
        return ChildRate::Target(target);
    }
    // sat/kWU = sat/vB × 250.
    let per_kwu = |fee: Amount, vsize: u64| fee.to_sat().saturating_mul(250) / vsize.max(1);
    let affordable = FeeRate::from_sat_per_kwu(per_kwu(
        parent_fee.checked_add(budget).unwrap_or(Amount::MAX),
        package,
    ));
    let own = FeeRate::from_sat_per_kwu(per_kwu(parent_fee, parent_vsize));
    if affordable > own {
        ChildRate::Capped(affordable)
    } else {
        ChildRate::OverBudget
    }
}

/// Whether an attested outcome transaction is within `within` seconds of the contract's
/// `expiry`, or past it, when the expiry transaction could take its place.
fn near_expiry(expiry: Option<u32>, now: i64, within: u64) -> bool {
    // Below this, a locktime is a block height, not a time; this coordinator's oracle uses times.
    expiry
        .filter(|expiry| *expiry >= 500_000_000)
        .is_some_and(|expiry| i64::from(expiry) - now <= i64::try_from(within).unwrap_or(i64::MAX))
}

impl Coordinator {
    /// Fee-bump stuck settlement transactions as `settings` say; off until this is called.
    pub fn with_cpfp(mut self, settings: CpfpSettings) -> Result<Self, anyhow::Error> {
        settings.validate()?;
        self.anchor_bumps.settings = settings;
        Ok(self)
    }

    fn bump_parents(&self) -> MutexGuard<'_, HashMap<Txid, ParentState>> {
        self.anchor_bumps
            .parents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn update_parent(&self, txid: Txid, update: impl FnOnce(&mut ParentState)) {
        update(self.bump_parents().entry(txid).or_default());
    }

    /// Fee-bump the outcome, expiry or split transaction `competition` broadcast, if it has an
    /// anchor, has not confirmed and pays less than it should (see the module docs). Best
    /// effort: failures are logged and counted, and settlement carries on, since the
    /// transaction still pays its own pre-signed fee.
    pub(super) async fn bump_stuck_settlement_tx(&self, competition: &Competition) {
        let Some(candidate) = self.bump_candidate(competition, OffsetDateTime::now_utc()) else {
            return;
        };
        let label = candidate.settling.label();
        let txid = candidate.txid;
        let result = match self.bump(competition, &candidate).await {
            Ok(Bumped::Nothing) => return,
            Ok(Bumped::Broadcast {
                child,
                rate,
                fee,
                capped,
            }) => {
                info!(
                    "Competition {} bumped its {label} transaction {txid} with CPFP child {child}: \
                     {} sat/vB for the pair, {} sats{}",
                    competition.id,
                    rate.to_sat_per_vb_ceil(),
                    fee.to_sat(),
                    if capped {
                        ", capped by cpfp_settings.max_fee_percent"
                    } else {
                        ""
                    }
                );
                if capped {
                    "capped"
                } else {
                    "broadcast"
                }
            }
            Ok(Bumped::OverBudget) => {
                if self
                    .reported
                    .is_new("cpfp over budget", competition.id, &txid.to_string())
                {
                    warn!(
                        "Competition {}'s {label} transaction {txid} is stuck, and a child within \
                         cpfp_settings.max_fee_percent cannot pay more than it does",
                        competition.id
                    );
                }
                "over_budget"
            }
            Ok(Bumped::NoCoin { needed }) => {
                if self
                    .reported
                    .is_new("cpfp no coin", competition.id, &txid.to_string())
                {
                    warn!(
                        "Competition {}'s {label} transaction {txid} is stuck, and no confirmed \
                         wallet coin of {} sats or more is free to pay a CPFP child",
                        competition.id,
                        needed.to_sat()
                    );
                }
                "no_coin"
            }
            Ok(Bumped::ParentRejected(error)) => {
                if self
                    .reported
                    .is_new("cpfp parent rejected", competition.id, &error)
                {
                    warn!(
                        "Competition {}'s {label} transaction {txid} is not in the mempool and \
                         was refused again: {error}. Below the mempool's minimum fee it needs a \
                         package with a child (submitpackage), which LND cannot send",
                        competition.id
                    );
                }
                "parent_rejected"
            }
            Err(error) => {
                warn!(
                    "Competition {} could not bump its {label} transaction {txid}: {error:#}",
                    competition.id
                );
                "failed"
            }
        };
        CPFP_BUMPS.with_label_values(&[label, result]).inc();
    }

    /// The anchored settlement transaction `competition` waits on, if its check is due. Does no
    /// I/O, so competitions with nothing to bump cost nothing.
    fn bump_candidate(&self, competition: &Competition, now: OffsetDateTime) -> Option<Candidate> {
        let settings = &self.anchor_bumps.settings;
        if !settings.enabled {
            return None;
        }
        let signed = competition.signed_contract.as_ref()?;
        let (settling, tx, broadcast_at) = match competition.delta_broadcasted_at {
            None => {
                let tx = competition.outcome_transaction.as_ref()?;
                let expiry_txid = signed
                    .dlc()
                    .unsigned_outcome_txs()
                    .get(&Outcome::Expiry)
                    .map(Transaction::compute_txid);
                let settling = if Some(tx.compute_txid()) == expiry_txid {
                    Settling::Expiry
                } else {
                    Settling::Outcome
                };
                (settling, tx, competition.outcome_broadcasted_at?)
            }
            Some(broadcast_at) => {
                let outcome = competition.get_current_outcome().ok()?;
                (
                    Settling::Split,
                    signed.unsigned_split_tx(&outcome)?,
                    broadcast_at,
                )
            }
        };
        anchor::find_anchor(tx)?;
        let after = time::Duration::seconds(i64::try_from(settings.after_secs).unwrap_or(i64::MAX));
        if now - broadcast_at < after {
            return None;
        }
        let txid = tx.compute_txid();
        let mut parents = self.bump_parents();
        if parents.len() > MAX_REMEMBERED {
            parents.retain(|_, state| !state.done);
        }
        let state = parents.entry(txid).or_default();
        if state.done
            || state
                .checked_at
                .is_some_and(|at| at.elapsed() < CHECK_EVERY)
        {
            return None;
        }
        state.checked_at = Some(Instant::now());
        Some(Candidate {
            settling,
            txid,
            tx: tx.clone(),
        })
    }

    async fn bump(
        &self,
        competition: &Competition,
        candidate: &Candidate,
    ) -> Result<Bumped, anyhow::Error> {
        let signed = competition
            .signed_contract
            .as_ref()
            .ok_or_else(|| anyhow!("Competition {} has no signed contract", competition.id))?;
        let txid = candidate.txid;
        if self
            .bitcoin
            .get_tx_confirmation_height(&txid)
            .await?
            .is_some()
        {
            self.update_parent(txid, |state| state.done = true);
            return Ok(Bumped::Nothing);
        }
        // The mempool's copy, witness included.
        let parent = match self.bitcoin.get_raw_transaction(&txid).await {
            Ok(parent) => parent,
            Err(_) if candidate.settling == Settling::Split => {
                // Never broadcast, when every winner closed together, or evicted.
                self.update_parent(txid, |state| state.done = true);
                return Ok(Bumped::Nothing);
            }
            Err(_) => {
                if let Err(error) = self.broadcast_or_known(&candidate.tx).await {
                    return Ok(Bumped::ParentRejected(format!("{error:#}")));
                }
                info!(
                    "Competition {} broadcast its {} transaction {txid} again: the mempool had \
                     lost it",
                    competition.id,
                    candidate.settling.label()
                );
                candidate.tx.clone()
            }
        };
        let parent_fee = signed
            .presigned_tx_fee(&parent)
            .ok_or_else(|| anyhow!("Transaction {txid} is not one of the contract's"))?;
        let parent_vsize = parent.weight().to_vbytes_ceil();

        let settings = &self.anchor_bumps.settings;
        let urgent = candidate.settling == Settling::Outcome
            && near_expiry(
                signed.params().event.expiry,
                OffsetDateTime::now_utc().unix_timestamp(),
                settings.urgent_within_secs,
            );
        let conf_target = if urgent {
            settings.urgent_conf_target
        } else {
            settings.conf_target
        };
        let target = fee_rate_from_estimate(self.bitcoin.estimate_fee(conf_target).await?)?;
        if target
            .fee_vb(parent_vsize)
            .is_some_and(|wanted| parent_fee >= wanted)
        {
            return Ok(Bumped::Nothing);
        }
        let state = self.bump_parents().get(&txid).copied().unwrap_or_default();
        if !rebump_due(state.target, target) {
            return Ok(Bumped::Nothing);
        }
        self.update_parent(txid, |state| state.target = Some(target));

        let spent = parent
            .output
            .iter()
            .try_fold(parent_fee, |sum, output| sum.checked_add(output.value))
            .ok_or_else(|| anyhow!("Transaction {txid}'s value overflows"))?;
        let budget =
            Amount::from_sat(spent.to_sat().saturating_mul(settings.max_fee_percent) / 100);
        let (rate, capped) = match child_rate(target, parent_fee, parent_vsize, budget) {
            ChildRate::Target(rate) => (rate, false),
            ChildRate::Capped(rate) => (rate, true),
            ChildRate::OverBudget => return Ok(Bumped::OverBudget),
        };
        // A replacement must pay more than the child it replaces.
        if !rebump_due(state.child_rate, rate) {
            return Ok(Bumped::OverBudget);
        }

        let needed = rate
            .fee_vb(parent_vsize.saturating_add(CHILD_VBYTES))
            .and_then(|fee| fee.checked_sub(parent_fee))
            .unwrap_or(Amount::ZERO)
            .checked_add(CHANGE_MARGIN)
            .unwrap_or(Amount::MAX);
        let Some((coin, lease)) = self.lease_wallet_coin(needed).await else {
            return Ok(Bumped::NoCoin { needed });
        };
        let child = match self.sign_cpfp_child(signed, &parent, &coin, rate).await {
            Ok(child) => child,
            Err(error) => {
                if let Err(release) = self.bitcoin.release_output(coin.outpoint, lease).await {
                    warn!("Could not release wallet coin {}: {release}", coin.outpoint);
                }
                return Err(error);
            }
        };
        // A refused child keeps its lease until it lapses: the refusal may be ambiguous.
        self.broadcast_or_known(&child).await?;
        self.update_parent(txid, |state| state.child_rate = Some(rate));
        let fee = anchor::find_anchor(&parent)
            .map(|(_, anchor)| anchor.value)
            .unwrap_or(Amount::ZERO)
            .checked_add(coin.txout.value)
            .and_then(|paid_in| {
                paid_in.checked_sub(child.output.iter().map(|output| output.value).sum())
            })
            .unwrap_or(Amount::ZERO);
        CPFP_FEES_SAT.inc_by(fee.to_sat());
        Ok(Bumped::Broadcast {
            child: child.compute_txid(),
            rate,
            fee,
            capped,
        })
    }

    /// The smallest confirmed wallet coin worth at least `needed`, leased under a new id.
    /// Coins LND has leased for a funding transaction are not listed; a coin another task
    /// leases meanwhile is refused here and the next one tried.
    async fn lease_wallet_coin(&self, needed: Amount) -> Option<(WalletUtxo, [u8; 32])> {
        let mut coins: Vec<WalletUtxo> = self
            .bitcoin
            .list_utxos()
            .await
            .into_iter()
            .filter(|coin| {
                coin.is_confirmed()
                    && coin.txout.value >= needed
                    && wallet_input_weight(&coin.txout.script_pubkey).is_some()
            })
            .collect();
        coins.sort_by_key(|coin| coin.txout.value);
        for coin in coins {
            let lease: [u8; 32] = rand::random();
            match self
                .bitcoin
                .lease_output(coin.outpoint, lease, COIN_LEASE_SECS)
                .await
            {
                Ok(()) => return Some((coin, lease)),
                Err(error) => debug!("Wallet coin {} is not free: {error}", coin.outpoint),
            }
        }
        None
    }

    /// The child of `parent` spending its anchor and `coin`, so the pair pays `rate`, signed by
    /// LND. The anchor needs no signature: its final witness is empty.
    async fn sign_cpfp_child(
        &self,
        signed: &SignedContract,
        parent: &Transaction,
        coin: &WalletUtxo,
        rate: FeeRate,
    ) -> Result<Transaction, anyhow::Error> {
        let (_, anchor_output) =
            anchor::find_anchor(parent).ok_or_else(|| anyhow!("Transaction has no anchor"))?;
        let anchor_output = anchor_output.clone();
        let weight = wallet_input_weight(&coin.txout.script_pubkey).ok_or_else(|| {
            anyhow!(
                "Cannot size a wallet input paying {}",
                coin.txout.script_pubkey
            )
        })?;
        let change = self.bitcoin.get_next_address().await?.script_pubkey();
        let child = signed.cpfp_child_template(
            parent,
            &[CpfpFundingInput {
                outpoint: coin.outpoint,
                prevout: coin.txout.clone(),
                weight,
            }],
            change,
            rate,
        )?;
        let mut psbt = Psbt::from_unsigned_tx(child)?;
        // LND finalizes only its own coin, and needs every prevout for a taproot sighash.
        psbt.inputs[0].witness_utxo = Some(anchor_output);
        psbt.inputs[0].final_script_witness = Some(Witness::new());
        psbt.inputs[1].witness_utxo = Some(coin.txout.clone());
        if !self.bitcoin.sign_psbt(&mut psbt).await? {
            return Err(anyhow!("LND did not sign the CPFP child's wallet input"));
        }
        Ok(psbt.extract_tx()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::KeymeldSettings,
        infra::{
            bitcoin::{PayoutOutputStatus, SendOptions, WalletBalance},
            db::{DBConnection, DatabasePoolConfig, DatabaseType},
            keymeld::KeymeldService,
            lightning_mock::MockLnClient,
            lnurl_mock::MockLnurlPay,
            oracle_mock::MockOracle,
        },
    };
    use dlctix::{AnchorParams, EventLockingConditions, MarketMaker};

    fn rate(sat_per_vb: u32) -> FeeRate {
        FeeRate::from_sat_per_vb_u32(sat_per_vb)
    }

    #[test]
    fn rebumps_wait_for_a_quarter_higher_rate() {
        assert!(rebump_due(None, rate(5)));
        assert!(!rebump_due(Some(rate(20)), rate(20)));
        assert!(!rebump_due(Some(rate(20)), rate(24)));
        assert!(rebump_due(Some(rate(20)), rate(25)));
    }

    #[test]
    fn wallet_inputs_are_sized_by_script() {
        let key = Scalar::from_slice(&[3; 32]).unwrap().base_point_mul();
        let p2tr = ScriptBuf::new_p2tr_tweaked(TweakedPublicKey::dangerous_assume_tweaked(
            dlctix::convert_point(key),
        ));
        assert_eq!(
            wallet_input_weight(&p2tr).unwrap().weight(),
            InputWeightPrediction::P2TR_KEY_DEFAULT_SIGHASH.weight()
        );
        assert!(wallet_input_weight(&ScriptBuf::new()).is_none());
    }

    #[test]
    fn children_pay_at_most_the_budget() {
        let parent_vsize = 200;
        let parent_fee = Amount::from_sat(200);
        // 10 sat/vB for the 400 vB package is 4,000 sats: 3,800 from the child.
        assert_eq!(
            child_rate(rate(10), parent_fee, parent_vsize, Amount::from_sat(3_800)),
            ChildRate::Target(rate(10))
        );
        // With 1,800 the pair pays 2,000 sats over 400 vB: 5 sat/vB.
        assert_eq!(
            child_rate(rate(10), parent_fee, parent_vsize, Amount::from_sat(1_800)),
            ChildRate::Capped(rate(5))
        );
        // A budget that cannot lift the pair above the parent's own 1 sat/vB is no use.
        assert_eq!(
            child_rate(rate(10), parent_fee, parent_vsize, Amount::from_sat(100)),
            ChildRate::OverBudget
        );
    }

    #[test]
    fn an_outcome_is_urgent_near_and_past_the_expiry() {
        let expiry = 1_800_000_000;
        let hours = |n: i64| n * 3_600;
        assert!(!near_expiry(
            Some(expiry),
            i64::from(expiry) - hours(7),
            6 * 3_600
        ));
        assert!(near_expiry(
            Some(expiry),
            i64::from(expiry) - hours(5),
            6 * 3_600
        ));
        assert!(near_expiry(
            Some(expiry),
            i64::from(expiry) + hours(1),
            6 * 3_600
        ));
        assert!(!near_expiry(None, 0, 6 * 3_600));
        // A block height expiry is not compared with the clock.
        assert!(!near_expiry(Some(900_000), 0, 6 * 3_600));
    }

    mockall::mock! {
        Chain {}
        #[async_trait::async_trait]
        impl Bitcoin for Chain {
            fn get_network(&self) -> bitcoin::Network;
            async fn sign_psbt_with_escrow_support(&self, psbt: &mut Psbt) -> Result<bool, anyhow::Error>;
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
            async fn reserve_psbt_inputs_until(
                &self,
                psbt: &Psbt,
                deadline: u64,
            ) -> Result<(), anyhow::Error>;
            async fn release_psbt_inputs(&self, psbt: &Psbt) -> Result<(), anyhow::Error>;
            async fn get_spendable_utxo(&self, amount_sats: u64) -> Result<WalletUtxo, anyhow::Error>;
            async fn lease_output(
                &self,
                outpoint: OutPoint,
                id: [u8; 32],
                seconds: u64,
            ) -> Result<(), anyhow::Error>;
            async fn release_output(&self, outpoint: OutPoint, id: [u8; 32]) -> Result<(), anyhow::Error>;
            async fn get_current_height(&self) -> Result<u32, anyhow::Error>;
            async fn get_confirmed_blockchain_time(&self, blocks: usize) -> Result<u64, anyhow::Error>;
            async fn get_estimated_fee_rates(&self) -> Result<HashMap<u16, f64>, anyhow::Error>;
            async fn estimate_fee(&self, conf_target: u16) -> Result<f64, anyhow::Error>;
            async fn get_tx_confirmation_height(&self, txid: &Txid) -> Result<Option<u32>, anyhow::Error>;
            async fn payout_output_status(
                &self,
                outpoint: OutPoint,
                output: TxOut,
            ) -> Result<PayoutOutputStatus, anyhow::Error>;
            async fn broadcast(&self, transaction: &Transaction) -> Result<(), anyhow::Error>;
            async fn get_next_address(&self) -> Result<bitcoin::Address, anyhow::Error>;
            async fn get_public_key(&self) -> Result<BitcoinPublicKey, anyhow::Error>;
            async fn get_derived_private_key(&self) -> Result<Scalar, anyhow::Error>;
            async fn get_raw_transaction(&self, txid: &Txid) -> Result<Transaction, anyhow::Error>;
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
        }
    }

    /// A contract between the market maker and three players sharing each outcome, at 1 sat/vB,
    /// with an anchor if `anchor`.
    fn signed_contract(anchor: Option<AnchorParams>, expiry: u32) -> (SignedContract, Scalar) {
        let market_maker = Scalar::from_slice(&[7; 32]).unwrap();
        let players = [1, 3, 5].map(|key| Scalar::from_slice(&[key; 32]).unwrap());
        let params = ContractParameters {
            market_maker: MarketMaker {
                pubkey: market_maker.base_point_mul(),
            },
            players: players
                .iter()
                .enumerate()
                .map(|(index, key)| Player {
                    pubkey: key.base_point_mul(),
                    ticket_hash: dlctix::hashlock::sha256(&[index as u8 + 10; 32]),
                    payout_hash: dlctix::hashlock::sha256(&[index as u8 + 20; 32]),
                })
                .collect(),
            event: EventLockingConditions {
                locking_points: vec![Scalar::from_slice(&[10; 32])
                    .unwrap()
                    .base_point_mul()
                    .into()],
                expiry: Some(expiry),
            },
            outcome_payouts: [Outcome::Attestation(0), Outcome::Expiry]
                .into_iter()
                .map(|outcome| (outcome, PayoutWeights::from([(0, 1), (1, 1), (2, 1)])))
                .collect(),
            fee_rate: FeeRate::from_sat_per_vb_u32(1),
            funding_value: Amount::from_sat(1_000_000),
            relative_locktime_block_delta: 72,
            anchor,
            outcome_bound_splits: false,
        };
        let dlc = TicketedDLC::new(params, OutPoint::null()).unwrap();
        let mut rng = ChaCha20Rng::from_seed([42; 32]);
        let mut sessions: BTreeMap<_, _> = std::iter::once(market_maker)
            .chain(players)
            .map(|key| {
                (
                    key.base_point_mul(),
                    SigningSession::new(dlc.clone(), &mut rng, key).unwrap(),
                )
            })
            .collect();
        let nonces = sessions
            .iter()
            .map(|(key, session)| (*key, session.our_public_nonces().clone()))
            .collect();
        let coordinator = sessions
            .remove(&market_maker.base_point_mul())
            .unwrap()
            .aggregate_nonces_and_compute_partial_signatures(nonces)
            .unwrap();
        let signatures = sessions
            .into_iter()
            .map(|(key, session)| {
                let contributor = session
                    .compute_partial_signatures(coordinator.aggregated_nonces().clone())
                    .unwrap();
                (key, contributor.our_partial_signatures().clone())
            })
            .collect();
        (
            coordinator.aggregate_all_signatures(signatures).unwrap(),
            market_maker,
        )
    }

    fn wallet_coin(value: u64) -> WalletUtxo {
        let key = Scalar::from_slice(&[9; 32]).unwrap().base_point_mul();
        WalletUtxo {
            outpoint: OutPoint::new(Txid::from_byte_array([4; 32]), 1),
            txout: TxOut {
                value: Amount::from_sat(value),
                script_pubkey: ScriptBuf::new_p2tr_tweaked(
                    TweakedPublicKey::dangerous_assume_tweaked(dlctix::convert_point(key)),
                ),
            },
            address: String::new(),
            confirmations: 6,
        }
    }

    /// A coordinator on `chain` with fee-bumping on as by default, and a competition whose
    /// attested outcome transaction went out two hours ago.
    async fn settling(
        mut chain: MockChain,
        anchor: Option<AnchorParams>,
    ) -> (tempfile::TempDir, Coordinator, Competition) {
        let now = OffsetDateTime::now_utc();
        let expiry = (now + time::Duration::days(3)).unix_timestamp() as u32;
        let (contract, market_maker) = signed_contract(anchor, expiry);
        chain
            .expect_get_derived_private_key()
            .returning(move || Ok(market_maker));
        let directory = tempfile::tempdir().unwrap();
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let coordinator = Coordinator::new(
            Arc::new(MockOracle::new([12; 32])),
            CompetitionStore::new(database),
            Arc::new(chain),
            Arc::new(MockLnClient::new()),
            Arc::new(MockLnurlPay::new(bitcoin::Network::Regtest)),
            Arc::new(
                KeymeldService::new(KeymeldSettings::default(), Uuid::now_v7(), &[1; 32]).unwrap(),
            ),
            None,
            72,
            1,
            "anchor-bump-test".into(),
            false,
            1,
        )
        .await
        .unwrap()
        .with_cpfp(CpfpSettings::default())
        .unwrap();
        let mut competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: now - time::Duration::hours(5),
            start_observation_date: now - time::Duration::hours(7),
            end_observation_date: now - time::Duration::hours(6),
            locations: vec!["KDEN".into()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 3,
            entry_fee: 1_000,
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(0),
            total_competition_pool: 1_000_000,
            relative_locktime_block_delta: Some(72),
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
            contract_options: None,
        });
        let attestation = Scalar::from_slice(&[10; 32]).unwrap();
        competition.event_announcement = Some(contract.params().event.clone());
        competition.attestation = Some(attestation.into());
        competition.outcome_transaction = Some(contract.signed_outcome_tx(0, attestation).unwrap());
        competition.outcome_broadcasted_at = Some(now - time::Duration::hours(2));
        competition.signed_contract = Some(contract);
        (directory, coordinator, competition)
    }

    /// An anchored outcome transaction paying 1 sat/vB, unconfirmed two hours after it went out
    /// while the estimate is 50: one child from the wallet's coin lifts the pair to the estimate
    /// with its margin, and the next step does not look at it again so soon.
    #[tokio::test]
    async fn a_stuck_anchored_outcome_is_bumped_from_the_wallet() {
        let broadcasts = Arc::new(std::sync::Mutex::new(Vec::<Transaction>::new()));
        let mut chain = MockChain::new();
        chain
            .expect_get_tx_confirmation_height()
            .returning(|_| Ok(None));
        let parent = Arc::new(std::sync::Mutex::new(None::<Transaction>));
        let known = parent.clone();
        chain.expect_get_raw_transaction().returning(move |txid| {
            known
                .lock()
                .unwrap()
                .clone()
                .filter(|tx| tx.compute_txid() == *txid)
                .ok_or_else(|| anyhow!("unknown transaction"))
        });
        chain
            .expect_estimate_fee()
            .withf(|target| *target == 6)
            .returning(|_| Ok(50.0));
        chain
            .expect_list_utxos()
            .returning(|| vec![wallet_coin(5_000), wallet_coin(2_000_000)]);
        chain.expect_lease_output().returning(|_, _, _| Ok(()));
        chain.expect_get_next_address().returning(|| {
            Ok(
                bitcoin::Address::from_str("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080")
                    .unwrap()
                    .assume_checked(),
            )
        });
        chain.expect_sign_psbt().returning(|psbt| {
            psbt.inputs[1].final_script_witness = Some(Witness::from_slice(&[[1u8; 64]]));
            Ok(true)
        });
        let record = broadcasts.clone();
        chain.expect_broadcast().returning(move |transaction| {
            record.lock().unwrap().push(transaction.clone());
            Ok(())
        });
        let (_directory, coordinator, competition) =
            settling(chain, Some(AnchorParams::default())).await;
        let outcome_tx = competition.outcome_transaction.clone().unwrap();
        *parent.lock().unwrap() = Some(outcome_tx.clone());

        coordinator.bump_stuck_settlement_tx(&competition).await;
        let sent = broadcasts.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "one child");
        let child = &sent[0];
        let (anchor, anchor_output) = anchor::find_anchor(&outcome_tx).unwrap();
        assert_eq!(child.input[0].previous_output, anchor);
        assert!(child.input[0].witness.is_empty());
        // The 5,000-sat coin cannot pay for it; the larger one does.
        assert_eq!(child.input[1].previous_output, wallet_coin(0).outpoint);
        let signed = competition.signed_contract.as_ref().unwrap();
        let parent_fee = signed.presigned_tx_fee(&outcome_tx).unwrap();
        let child_fee = anchor_output.value + Amount::from_sat(2_000_000) - child.output[0].value;
        let target = fee_rate_from_estimate(50.0).unwrap();
        let package = outcome_tx.weight().to_vbytes_ceil() + child.weight().to_vbytes_ceil();
        assert!(parent_fee + child_fee >= target.fee_vb(package).unwrap());
        assert!(parent_fee + child_fee <= target.fee_vb(package + 10).unwrap());

        // Checked again only after a while, and not bumped again at the same estimate.
        coordinator.bump_stuck_settlement_tx(&competition).await;
        assert_eq!(broadcasts.lock().unwrap().len(), 1);
        coordinator.update_parent(outcome_tx.compute_txid(), |state| state.checked_at = None);
        coordinator.bump_stuck_settlement_tx(&competition).await;
        assert_eq!(broadcasts.lock().unwrap().len(), 1);
    }

    /// A contract without anchors, or a transaction that has not waited long enough, costs no
    /// chain lookup at all: the mock has no expectations to meet them.
    #[tokio::test]
    async fn nothing_is_looked_up_without_an_anchor_or_before_the_wait() {
        let (_directory, coordinator, competition) = settling(MockChain::new(), None).await;
        coordinator.bump_stuck_settlement_tx(&competition).await;

        let (_directory, coordinator, mut competition) =
            settling(MockChain::new(), Some(AnchorParams::default())).await;
        competition.outcome_broadcasted_at = Some(OffsetDateTime::now_utc());
        coordinator.bump_stuck_settlement_tx(&competition).await;

        // Off, it does nothing either.
        let (_directory, coordinator, competition) =
            settling(MockChain::new(), Some(AnchorParams::default())).await;
        let coordinator = coordinator
            .with_cpfp(CpfpSettings {
                enabled: false,
                ..CpfpSettings::default()
            })
            .unwrap();
        coordinator.bump_stuck_settlement_tx(&competition).await;
    }

    /// A confirmed transaction is remembered as done.
    #[tokio::test]
    async fn a_confirmed_transaction_is_left_alone() {
        let mut chain = MockChain::new();
        chain
            .expect_get_tx_confirmation_height()
            .times(1)
            .returning(|_| Ok(Some(100)));
        let (_directory, coordinator, competition) =
            settling(chain, Some(AnchorParams::default())).await;
        coordinator.bump_stuck_settlement_tx(&competition).await;
        let txid = competition
            .outcome_transaction
            .as_ref()
            .unwrap()
            .compute_txid();
        coordinator.update_parent(txid, |state| state.checked_at = None);
        coordinator.bump_stuck_settlement_tx(&competition).await;
    }
}
