//! Refunding an entry escrow whose competition never kicked off.
//!
//! 1. The coordinator resolves the player's Lightning Address and asks for a swap that commits to
//!    that invoice's payment hash. `mint` returns the swap's script and address.
//! 2. The coordinator refunds the escrow into that swap, with Keymeld signing as the player.
//! 3. The coordinator pays the invoice and reports the preimage the payment revealed.
//! 4. `refund_tick` claims the swap into this service's wallet, once the refund has paid it.
//!
//! The service can only take the swap by revealing that preimage, so it is paid for the coins it
//! claims. If it never claims, the player takes the swap back after the deadline, which costs the
//! service the payment it already made, so a stalled claim is an operator's problem.

use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::Context;
use ark_core::server::VirtualTxOutPoint;
use ark_core::ArkAddress;
use bitcoin::{Amount, OutPoint, XOnlyPublicKey};
use coordinator_ark_escrow::{RefundSwap, SwapTerms, VtxoScript};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::store::{Refund, RefundState, Store};
use crate::swap::{unix_now, Swapper};
use crate::wallet::payment_among;

/// How long one pass of `refund_tick` claims for. A claim takes seconds, and escrow payments
/// wait for the pass to end, so a backlog of claims is worked through a few at a time. The
/// refunds a pass does not reach go first in the next.
const REFUND_TICK_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// How often, at most, the swaps of paid refunds are looked up on Arkade. The claims due by then
/// share one listing, so the refunds the coordinator pays in a burst cost a lookup between them
/// rather than one each. The player is paid already, and the coordinator pays only while the
/// swap's deadline is a quarter of an hour or more away: the claim has that long, and after it
/// until the player takes the swap back.
const CLAIM_LOOKUP_EVERY: Duration = Duration::from_secs(30);

/// The lookups of paid refunds' swaps: when the last listing began, and the swap VTXOs it found
/// that are not claimed yet. Each waits for its refund's turn, so a pass that runs out of time
/// does not look it up again.
#[derive(Default)]
pub struct ClaimLookups {
    looked_up_at: std::sync::Mutex<Option<Instant>>,
    found: std::sync::Mutex<HashMap<Uuid, OutPoint>>,
}

impl ClaimLookups {
    fn found_swaps(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, OutPoint>> {
        self.found
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn last(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.looked_up_at
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Whether a listing may begin at `now`.
    fn due(&self, now: Instant) -> bool {
        self.last()
            .is_none_or(|at| now.saturating_duration_since(at) >= CLAIM_LOOKUP_EVERY)
    }

    /// A listing began at `at`.
    fn looked_up(&self, at: Instant) {
        *self.last() = Some(at);
    }

    /// The VTXO a listing found paying refund `id`'s swap, until it is claimed.
    fn found(&self, id: Uuid) -> Option<OutPoint> {
        self.found_swaps().get(&id).copied()
    }

    fn remember(&self, id: Uuid, vtxo: OutPoint) {
        self.found_swaps().insert(id, vtxo);
    }

    /// Forget what was found for refund `id`, once a claim of it was tried.
    fn forget(&self, id: Uuid) {
        self.found_swaps().remove(&id);
    }

    /// Forget what was found for any refund but `waiting`.
    fn keep_only(&self, waiting: &[Uuid]) {
        self.found_swaps().retain(|id, _| waiting.contains(id));
    }
}

/// A paid refund's swap, to look up on Arkade.
struct SwapLookup {
    refund: Uuid,
    address: ArkAddress,
    amount: Amount,
    /// UNIX seconds: when the refund was minted. No VTXO before that paid its swap.
    since: i64,
}

/// The VTXO that paid each refund's swap, by refund, from one listing of all their addresses by
/// `list`: unspent, worth the refund, and created since it was minted. A refund it leaves out is
/// not paid yet.
async fn look_up_swaps<F, Fut>(
    swaps: &[SwapLookup],
    list: F,
) -> anyhow::Result<HashMap<Uuid, OutPoint>>
where
    F: FnOnce(Vec<ArkAddress>) -> Fut,
    Fut: Future<Output = anyhow::Result<Vec<VirtualTxOutPoint>>>,
{
    if swaps.is_empty() {
        return Ok(HashMap::new());
    }
    let listed = list(swaps.iter().map(|swap| swap.address).collect()).await?;
    Ok(swaps
        .iter()
        .filter_map(|swap| {
            let script = swap.address.to_p2tr_script_pubkey();
            let at_swap: Vec<VirtualTxOutPoint> = listed
                .iter()
                .filter(|vtxo| vtxo.script == script)
                .cloned()
                .collect();
            payment_among(&at_swap, swap.amount, swap.since, None).map(|vtxo| (swap.refund, vtxo))
        })
        .collect())
}

impl Swapper {
    /// Mint the swap an escrow's refund pays, or return the one already minted for this invoice.
    pub async fn mint_refund(
        &self,
        payment_hash: [u8; 32],
        amount_sat: u64,
        player: XOnlyPublicKey,
        deadline: u32,
    ) -> anyhow::Result<Refund> {
        let amount = Amount::from_sat(amount_sat);
        anyhow::ensure!(amount >= self.wallet.dust(), "the amount is below dust");
        let hash = hex::encode(payment_hash);
        if let Some(minted) = self.store.refund_for_hash(&hash).await? {
            anyhow::ensure!(
                minted.amount_sat == amount_sat && minted.player_key == player.to_string(),
                "a swap for this invoice was minted for a different amount or player"
            );
            return Ok(minted);
        }

        let now = unix_now();
        let created_at = u32::try_from(now).context("the clock is out of range")?;
        let locktime = bitcoin::absolute::LockTime::from_time(deadline)
            .map_err(|error| anyhow::anyhow!("the deadline is not a timestamp: {error}"))?;
        let exit_delay = self.wallet.exit_delay();
        let swap = RefundSwap::new(SwapTerms {
            player,
            swapper: self.wallet.swapper_key(),
            server: self.wallet.server_key(),
            payment_hash,
            deadline: locktime,
            exit_delay,
            unilateral_reclaim_delay: SwapTerms::unilateral_reclaim_delay_for(
                locktime, exit_delay, created_at,
            )?,
        })?;
        self.wallet.accepts(swap.vtxo_script())?;

        let refund = Refund {
            id: Uuid::now_v7(),
            payment_hash: hash,
            amount_sat,
            player_key: player.to_string(),
            deadline: i64::from(deadline),
            swap_tap_tree: hex::encode(swap.vtxo_script().encode_tap_tree()),
            swap_address: swap.address(self.wallet.hrp())?.encode(),
            state: RefundState::Minted,
            preimage: None,
            swap_vtxo: None,
            claim_txid: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        self.store.insert_refund(&refund).await?;
        log::info!(
            "refund {} minted a swap at {} for {amount_sat} sats",
            refund.id,
            refund.swap_address
        );
        Ok(refund)
    }

    /// Record the preimage the coordinator's payment revealed. `refund_tick` claims the swap.
    ///
    /// It does not claim here: a claim waits for the wallet's other sends, and a boarding holds
    /// the wallet for a whole batch, far longer than the coordinator waits for an answer.
    pub async fn refund_paid(&self, id: Uuid, preimage: [u8; 32]) -> anyhow::Result<Refund> {
        record_preimage(&self.store, id, preimage).await
    }
    /// Claim the refunds whose swaps have been paid, and retire those the player may take back,
    /// for up to `REFUND_TICK_BUDGET`, starting after the last refund the previous pass reached.
    pub async fn refund_tick(&self) {
        let mut refunds = match self.store.unclaimed_refunds().await {
            Ok(refunds) => refunds,
            Err(error) => {
                log::error!("list unclaimed refunds: {error:#}");
                return;
            }
        };
        let last = *self
            .refund_turn
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let start = turn_start(refunds.iter().map(|refund| refund.id), last);
        refunds.rotate_left(start);
        self.look_up_claims(&refunds).await;
        let started = std::time::Instant::now();
        let total = refunds.len();
        for (reached, mut refund) in refunds.into_iter().enumerate() {
            if started.elapsed() >= REFUND_TICK_BUDGET {
                log::debug!(
                    "{} of {total} refunds wait for the next pass",
                    total - reached
                );
                break;
            }
            let id = refund.id;
            if let Err(error) = self.advance_refund(&mut refund).await {
                log::warn!("refund {id}: {error:#}");
            }
            *self
                .refund_turn
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(id);
        }
    }

    /// Look up on Arkade, in one listing, the swaps of the paid refunds whose lookup is due,
    /// at most once every `CLAIM_LOOKUP_EVERY`. Each refund keeps its own backoff between
    /// lookups that miss, as `Store::reserve_indexer_lookup` paces it.
    async fn look_up_claims(&self, refunds: &[Refund]) {
        let waiting: Vec<&Refund> = refunds
            .iter()
            .filter(|refund| refund.preimage.is_some() && refund.claim_txid.is_none())
            .collect();
        self.claim_lookups
            .keep_only(&waiting.iter().map(|refund| refund.id).collect::<Vec<_>>());
        let started = Instant::now();
        if waiting.is_empty() || !self.claim_lookups.due(started) {
            return;
        }
        let now = unix_now();
        let mut swaps = Vec::new();
        for refund in waiting {
            if self.claim_lookups.found(refund.id).is_some() {
                continue;
            }
            match self
                .store
                .reserve_indexer_lookup("refund", refund.id, now)
                .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    log::error!("refund {}: reserve its swap's lookup: {error:#}", refund.id);
                    continue;
                }
            }
            match self.wallet.escrow_address(&refund.swap_address) {
                Ok(address) => swaps.push(SwapLookup {
                    refund: refund.id,
                    address,
                    amount: Amount::from_sat(refund.amount_sat),
                    since: refund.created_at,
                }),
                Err(error) => log::warn!("refund {}: {error:#}", refund.id),
            }
        }
        if swaps.is_empty() {
            return;
        }
        self.claim_lookups.looked_up(started);
        match look_up_swaps(&swaps, |addresses| self.wallet.vtxos_at(addresses)).await {
            Ok(found) => {
                for swap in &swaps {
                    match found.get(&swap.refund) {
                        Some(vtxo) => self.claim_lookups.remember(swap.refund, *vtxo),
                        None => log::debug!("refund {}: its swap is not funded yet", swap.refund),
                    }
                }
            }
            Err(error) => log::warn!(
                "look up the swaps of {} paid refunds: {error:#}",
                swaps.len()
            ),
        }
    }

    async fn advance_refund(&self, refund: &mut Refund) -> anyhow::Result<()> {
        if unix_now() >= refund.deadline {
            // The claim leaf has no timelock: a swap this service paid for can still be claimed
            // after the deadline, until the player takes it back.
            if refund.preimage.is_some() {
                if let Err(error) = self.claim_refund(refund).await {
                    log::debug!("refund {}: late claim failed: {error:#}", refund.id);
                }
                if refund.state == RefundState::Claimed {
                    return Ok(());
                }
            }
            if refund.state == RefundState::Reclaimable {
                return Ok(());
            }
            let unclaimed = refund.preimage.is_some();
            self.transition_refund(
                refund,
                RefundState::Reclaimable,
                unclaimed.then(|| "the deadline passed before the swap was claimed".to_string()),
            )
            .await?;
            if unclaimed {
                log::error!(
                    "refund {} paid the player but never claimed its swap",
                    refund.id
                );
            }
            return Ok(());
        }
        if refund.preimage.is_some() {
            self.claim_refund(refund).await?;
        }
        Ok(())
    }

    /// Claim the swap, once the refund has paid it and `look_up_claims` found the VTXO that did.
    async fn claim_refund(&self, refund: &mut Refund) -> anyhow::Result<()> {
        // An earlier pass may have claimed it since this one listed it.
        if let Some(current) = self.store.refund(refund.id).await? {
            *refund = current;
        }
        if refund.claim_txid.is_some() {
            self.claim_lookups.forget(refund.id);
            self.store.clear_indexer_lookup("refund", refund.id).await?;
            return Ok(());
        }
        let Some(preimage) = refund.preimage.as_deref() else {
            return Ok(());
        };
        let Some(outpoint) = self.claim_lookups.found(refund.id) else {
            return Ok(());
        };
        // A claim that fails is looked up again once its lookup is due.
        self.claim_lookups.forget(refund.id);
        let preimage = bytes32(preimage)?;
        let swap = self.refund_swap_of(refund)?;
        let amount = Amount::from_sat(refund.amount_sat);
        refund.swap_vtxo = Some(outpoint.to_string());
        let txid = self
            .wallet
            .claim_refund_swap(&swap, outpoint, amount, preimage)
            .await?;
        refund.claim_txid = Some(txid.to_string());
        self.transition_refund(refund, RefundState::Claimed, None)
            .await?;
        self.store.clear_indexer_lookup("refund", refund.id).await?;
        log::info!("refund {} claimed its swap in {txid}", refund.id);
        Ok(())
    }

    fn refund_swap_of(&self, refund: &Refund) -> anyhow::Result<RefundSwap> {
        let tap_tree = hex::decode(&refund.swap_tap_tree)?;
        let vtxo = VtxoScript::decode_tap_tree(&tap_tree)?;
        Ok(RefundSwap::from_vtxo_script(&vtxo)?)
    }

    async fn transition_refund(
        &self,
        refund: &mut Refund,
        state: RefundState,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        refund.state = state;
        refund.error = error;
        refund.updated_at = unix_now();
        self.store.update_refund(refund).await
    }
}

/// Record `preimage` as settling refund `id`'s invoice, and mark the refund paid.
async fn record_preimage(store: &Store, id: Uuid, preimage: [u8; 32]) -> anyhow::Result<Refund> {
    let mut refund = store
        .refund(id)
        .await?
        .with_context(|| format!("no refund {id}"))?;
    let hash: [u8; 32] = Sha256::digest(preimage).into();
    anyhow::ensure!(
        hex::encode(hash) == refund.payment_hash,
        "this preimage does not settle the refund's invoice"
    );
    if refund.preimage.is_none() {
        refund.preimage = Some(hex::encode(preimage));
        refund.state = RefundState::Paid;
        refund.error = None;
        refund.updated_at = unix_now();
        store.update_refund(&refund).await?;
    }
    Ok(refund)
}

/// Where a pass over refunds listed as `ids`, oldest first, starts: after `last`, the last one
/// the previous pass reached, and from the oldest once it has gone round. Refund ids are UUIDv7,
/// so they order as the refunds were minted, and `last` need not be listed any more.
fn turn_start(mut ids: impl Iterator<Item = Uuid>, last: Option<Uuid>) -> usize {
    last.and_then(|last| ids.position(|id| id > last))
        .unwrap_or(0)
}

fn bytes32(hex_value: &str) -> anyhow::Result<[u8; 32]> {
    hex::decode(hex_value)?
        .try_into()
        .ok()
        .context("expected 32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minted(payment_hash: [u8; 32]) -> Refund {
        Refund {
            id: Uuid::now_v7(),
            payment_hash: hex::encode(payment_hash),
            amount_sat: 20_000,
            player_key: "14".repeat(32),
            deadline: 1_790_003_600,
            swap_tap_tree: "aa".into(),
            swap_address: "tark1refund".into(),
            state: RefundState::Minted,
            preimage: None,
            swap_vtxo: None,
            claim_txid: None,
            error: None,
            created_at: 1_790_000_000,
            updated_at: 1_790_000_000,
        }
    }

    #[tokio::test]
    async fn a_paid_refund_is_recorded_while_a_boarding_holds_the_wallet() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(
            &directory.path().join("swaps.sqlite"),
            crate::preimages::PreimageKey::from_wallet_secret(&[1u8; 32]),
        )
        .await
        .unwrap();
        let preimage = [7u8; 32];
        let refund = minted(Sha256::digest(preimage).into());
        store.insert_refund(&refund).await.unwrap();

        // A boarding or a renewal holds one of the wallet's locks for a whole batch.
        let sending = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let _boarding = sending.clone().lock_owned().await;

        let paid = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            record_preimage(&store, refund.id, preimage),
        )
        .await
        .expect("recording the payment waits for no send")
        .unwrap();
        assert_eq!(paid.state, RefundState::Paid);
        assert_eq!(paid.preimage, Some(hex::encode(preimage)));
        // `refund_tick` claims it later.
        let unclaimed = store.unclaimed_refunds().await.unwrap();
        assert_eq!(unclaimed.len(), 1);
        assert_eq!(unclaimed[0].preimage, paid.preimage);

        // A repeated report is answered the same way, and a wrong preimage is refused.
        let again = record_preimage(&store, refund.id, preimage).await.unwrap();
        assert_eq!(again.state, RefundState::Paid);
        assert!(record_preimage(&store, refund.id, [8u8; 32]).await.is_err());
        assert!(record_preimage(&store, Uuid::now_v7(), preimage)
            .await
            .is_err());
    }

    #[test]
    fn a_pass_over_refunds_starts_where_the_last_one_stopped() {
        let ids: Vec<Uuid> = (1..=4).map(Uuid::from_u128).collect();
        let start = |last| turn_start(ids.iter().copied(), last);
        assert_eq!(start(None), 0);
        assert_eq!(start(Some(ids[0])), 1);
        assert_eq!(start(Some(ids[2])), 3);
        // Round again from the oldest once the newest was reached.
        assert_eq!(start(Some(ids[3])), 0);
        // The last one reached was claimed since, and is no longer listed.
        let listed = [ids[0], ids[2], ids[3]];
        assert_eq!(turn_start(listed.iter().copied(), Some(ids[1])), 1);
        assert_eq!(turn_start(std::iter::empty(), Some(ids[1])), 0);
    }

    /// A swap's Ark address, for test key `byte`.
    fn swap_address(byte: u8) -> ArkAddress {
        let key = |byte: u8| {
            bitcoin::secp256k1::SecretKey::from_slice(&[byte; 32])
                .unwrap()
                .x_only_public_key(&bitcoin::key::Secp256k1::new())
                .0
        };
        ArkAddress::new(
            bitcoin::Network::Regtest,
            key(1),
            bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(key(byte)),
        )
    }

    fn listed(address: ArkAddress, txid: u8, sat: u64, created_at: i64) -> VirtualTxOutPoint {
        use bitcoin::hashes::Hash;
        VirtualTxOutPoint {
            outpoint: OutPoint::new(bitcoin::Txid::from_byte_array([txid; 32]), 0),
            created_at,
            expires_at: created_at + 86_400,
            amount: Amount::from_sat(sat),
            script: address.to_p2tr_script_pubkey(),
            is_preconfirmed: true,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
            depth: 1,
        }
    }

    /// The refunds the coordinator pays in a burst are looked up together: one listing of all
    /// their swaps' addresses, read for each swap at its own address.
    #[tokio::test]
    async fn the_swaps_of_due_claims_are_looked_up_in_one_listing() {
        let minted_at = 1_790_000_000;
        let swaps: Vec<SwapLookup> = (2..=5)
            .map(|byte| SwapLookup {
                refund: Uuid::from_u128(u128::from(byte)),
                address: swap_address(byte),
                amount: Amount::from_sat(20_000),
                since: minted_at,
            })
            .collect();
        let mut spent = listed(swap_address(4), 4, 20_000, minted_at + 5);
        spent.is_spent = true;
        let answer = vec![
            listed(swap_address(2), 2, 20_000, minted_at + 5),
            // The right amount at another swap's address pays only that swap.
            listed(swap_address(9), 9, 20_000, minted_at + 5),
            // Short of the refund, or older than it, it paid something else.
            listed(swap_address(3), 3, 19_999, minted_at + 5),
            listed(swap_address(5), 5, 20_000, minted_at - 1),
            spent,
            listed(swap_address(5), 6, 20_000, minted_at),
        ];
        let asked = std::sync::Mutex::new(Vec::new());
        let found = look_up_swaps(&swaps, |addresses| {
            asked.lock().unwrap().push(addresses);
            async move { Ok(answer) }
        })
        .await
        .unwrap();
        assert_eq!(
            asked.into_inner().unwrap(),
            vec![swaps.iter().map(|swap| swap.address).collect::<Vec<_>>()],
            "one listing for every swap"
        );
        let paid = |txid: u8| {
            use bitcoin::hashes::Hash;
            OutPoint::new(bitcoin::Txid::from_byte_array([txid; 32]), 0)
        };
        assert_eq!(
            found,
            HashMap::from([(Uuid::from_u128(2), paid(2)), (Uuid::from_u128(5), paid(6)),])
        );

        // Nothing to look up asks nothing.
        let none = look_up_swaps(&[], |_| async {
            Err::<Vec<VirtualTxOutPoint>, _>(anyhow::anyhow!("asked for nothing"))
        })
        .await
        .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn claims_share_a_lookup_every_half_minute_and_keep_what_it_found_until_claimed() {
        let lookups = ClaimLookups::default();
        let start = Instant::now();
        assert!(lookups.due(start));
        lookups.looked_up(start);
        assert!(!lookups.due(start + CLAIM_LOOKUP_EVERY - Duration::from_millis(1)));
        assert!(lookups.due(start + CLAIM_LOOKUP_EVERY));

        let (first, second) = (Uuid::from_u128(1), Uuid::from_u128(2));
        lookups.remember(first, OutPoint::null());
        lookups.remember(second, OutPoint::null());
        assert_eq!(lookups.found(first), Some(OutPoint::null()));
        lookups.forget(first);
        assert_eq!(lookups.found(first), None);
        // A refund no longer waiting for its claim leaves nothing behind.
        lookups.keep_only(&[first]);
        assert_eq!(lookups.found(second), None);
    }
}
