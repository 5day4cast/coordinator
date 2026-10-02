//! One Arkade subscription for the escrows of every pending escrow swap.
//!
//! Rather than list each escrow's address on Arkade until its VTXO appears, the coordinator
//! asks the server's indexer to tell it about transactions at the escrows' scripts. A swap's
//! script is added when the swap is made and removed once its ticket is paid or the swap ends
//! unpaid. An event about a pending escrow settles its swap with the same checks as the
//! periodic one, from the VTXOs the event carries.
//!
//! The subscription can miss what happens while it is down, so `check_ark_swaps` still lists
//! the escrows it has not settled, in one listing, every thirty seconds.

use super::ark_coordinator::escrow_script;
use super::*;
use crate::domain::competitions::{Arkade, PendingArkSwap};
use bitcoin::hex::DisplayHex;
use coordinator_ark::{
    ScriptTransaction, SubscriptionEvent, SubscriptionStream, VirtualTxOutPoint,
};
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How often the escrows the subscription has not settled are listed on Arkade.
pub(crate) const ESCROW_SWEEP_EVERY: Duration = Duration::from_secs(30);
/// How long the subscription may send nothing, not even a heartbeat, before it is taken to
/// have dropped.
const ESCROW_SUBSCRIPTION_SILENCE: Duration = Duration::from_secs(180);
/// How often a live subscription checks that this process still runs the escrow swaps.
const ESCROW_LEASE_CHECK_EVERY: Duration = Duration::from_secs(10);
/// The worker whose lease decides which process watches: the one checking the swaps.
const ESCROW_SWAPS: &str = "escrow-swaps";

/// The escrows the subscription should watch, what it reported about them, and when they were
/// last listed.
#[derive(Default)]
pub(crate) struct EscrowWatch {
    state: Mutex<WatchState>,
    /// Woken when the scripts to watch change.
    changed: tokio::sync::Notify,
    last_sweep: Mutex<Option<Instant>>,
    /// Held while a swap is settled, one at a time.
    pub(in crate::domain::competitions) settling: tokio::sync::Mutex<()>,
    /// Every sweep is due and every retry immediate: for tests.
    immediate: AtomicBool,
}

#[derive(Default)]
struct WatchState {
    /// The output scripts, hex, of the pending swaps' escrows.
    scripts: HashSet<String>,
    /// The VTXOs the subscription reported at those scripts, until their swap is done.
    seen: HashMap<String, Vec<VirtualTxOutPoint>>,
}

impl EscrowWatch {
    #[cfg(test)]
    pub(crate) fn set_immediate(&self) {
        self.immediate.store(true, Ordering::Relaxed);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Watch the escrow at the encoded Ark `address`.
    pub(crate) fn watch(&self, address: &str) {
        let Some(script) = escrow_script(address) else {
            return;
        };
        if self.add_script(script) {
            self.changed.notify_one();
        }
    }

    /// Watch exactly the escrows at `addresses`.
    pub(crate) fn replace<'a>(&self, addresses: impl IntoIterator<Item = &'a str>) {
        let scripts: HashSet<String> = addresses.into_iter().filter_map(escrow_script).collect();
        if self.replace_scripts(scripts) {
            self.changed.notify_one();
        }
    }

    /// Stop watching the escrow at `address`, whose swap is done.
    pub(crate) fn forget(&self, address: &str) {
        let Some(script) = escrow_script(address) else {
            return;
        };
        if self.remove_script(&script) {
            self.changed.notify_one();
        }
    }

    fn add_script(&self, script: String) -> bool {
        self.state().scripts.insert(script)
    }

    fn replace_scripts(&self, scripts: HashSet<String>) -> bool {
        let mut state = self.state();
        if state.scripts == scripts {
            return false;
        }
        state.seen.retain(|script, _| scripts.contains(script));
        state.scripts = scripts;
        true
    }

    fn remove_script(&self, script: &str) -> bool {
        let mut state = self.state();
        state.seen.remove(script);
        state.scripts.remove(script)
    }

    /// The VTXOs the subscription reported at the escrow at `address`.
    pub(crate) fn seen(&self, address: &str) -> Vec<VirtualTxOutPoint> {
        escrow_script(address)
            .and_then(|script| self.state().seen.get(&script).cloned())
            .unwrap_or_default()
    }

    /// Drop what the subscription reported at the escrow at `address`, once it proved not
    /// enough to settle its swap.
    pub(crate) fn clear_seen(&self, address: &str) {
        if let Some(script) = escrow_script(address) {
            self.state().seen.remove(&script);
        }
    }

    /// The scripts to watch.
    fn scripts(&self) -> HashSet<String> {
        self.state().scripts.clone()
    }

    /// Record what `transaction` did at watched scripts, and return those scripts.
    fn saw(&self, transaction: &ScriptTransaction) -> Vec<String> {
        let mut state = self.state();
        let WatchState { scripts, seen } = &mut *state;
        let mut touched: HashSet<String> = transaction
            .scripts
            .iter()
            .filter(|script| scripts.contains(*script))
            .cloned()
            .collect();
        let spent = transaction
            .spent_vtxos
            .iter()
            .map(|vtxo| VirtualTxOutPoint {
                is_spent: true,
                ..vtxo.clone()
            });
        for vtxo in transaction.new_vtxos.iter().cloned().chain(spent) {
            let script = vtxo.script.as_bytes().to_lower_hex_string();
            if !scripts.contains(&script) {
                continue;
            }
            let at_script = seen.entry(script.clone()).or_default();
            at_script.retain(|known| known.outpoint != vtxo.outpoint);
            at_script.push(vtxo);
            touched.insert(script);
        }
        touched.into_iter().collect()
    }

    /// Whether the escrows are due to be listed, and if so, that they are listed now.
    pub(crate) fn sweep_due(&self) -> bool {
        if self.immediate.load(Ordering::Relaxed) {
            return true;
        }
        let mut last = self
            .last_sweep
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|at| at.elapsed() < ESCROW_SWEEP_EVERY) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    /// How long to wait before subscribing again after the `failures`th failure in a row: two
    /// seconds, doubling up to a minute.
    fn retry_after(&self, failures: u32) -> Duration {
        if self.immediate.load(Ordering::Relaxed) {
            return Duration::from_millis(10);
        }
        resubscribe_delay(failures)
    }

    fn check_lease_every(&self) -> Duration {
        if self.immediate.load(Ordering::Relaxed) {
            return Duration::from_millis(50);
        }
        ESCROW_LEASE_CHECK_EVERY
    }
}

/// How long to wait after the `failures`th subscription failure in a row.
fn resubscribe_delay(failures: u32) -> Duration {
    let seconds = 2u64.saturating_mul(1u64 << failures.saturating_sub(1).min(5));
    Duration::from_secs(seconds.min(60))
}

/// An open subscription: its ID, its stream, and the scripts it watches.
struct LiveSubscription {
    id: String,
    events: SubscriptionStream,
    scripts: HashSet<String>,
    last_message: tokio::time::Instant,
    opened_at: tokio::time::Instant,
}

fn subscription_recovered(live: &Option<LiveSubscription>) -> bool {
    live.as_ref()
        .is_some_and(|live| live.opened_at.elapsed() >= Duration::from_secs(60))
}

/// Why a subscription ended.
enum SubscriptionEnd {
    Cancelled,
    /// Another process checks the escrow swaps, so it watches them too.
    NotLeased,
    /// The stream ended or failed. Recovery requires a full minute of uptime.
    Dropped {
        recovered: bool,
        reason: String,
    },
}

impl Coordinator {
    /// Watch the pending swaps' escrows through one Arkade subscription until `cancel`, in the
    /// process that checks the escrow swaps.
    ///
    /// A subscription that drops is logged once, and opened again with the escrows pending
    /// then, after two seconds, doubling up to a minute while it keeps failing.
    pub async fn watch_ark_escrows(&self, cancel: CancellationToken) -> Result<(), anyhow::Error> {
        let Some(ark) = self.ark() else {
            return Ok(());
        };
        let mut failures = 0u32;
        loop {
            let end = self.escrow_subscription(ark, &cancel).await;
            crate::metrics::ESCROW_SUBSCRIPTION_UP.set(0);
            let wait = match end {
                SubscriptionEnd::Cancelled => return Ok(()),
                SubscriptionEnd::NotLeased => self.escrow_watch.check_lease_every(),
                SubscriptionEnd::Dropped { recovered, reason } => {
                    if recovered {
                        failures = 0;
                    }
                    failures = failures.saturating_add(1);
                    let wait = self.escrow_watch.retry_after(failures);
                    if failures == 1 {
                        warn!(
                            "The Arkade escrow subscription dropped: {reason}; subscribing \
                             again, and listing escrows every {}s meanwhile",
                            ESCROW_SWEEP_EVERY.as_secs()
                        );
                    } else {
                        debug!(
                            "The Arkade escrow subscription failed again: {reason}; next try \
                             in {}s",
                            wait.as_secs()
                        );
                    }
                    wait
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Whether this process holds, or could take, the lease of the escrow swaps.
    async fn holds_escrow_lease(&self) -> bool {
        self.worker_leases()
            .tick(ESCROW_SWAPS, std::future::ready(()))
            .await
            .is_some()
    }

    /// Run one subscription, from the swaps pending now, until it ends.
    async fn escrow_subscription(
        &self,
        ark: &Arkade,
        cancel: &CancellationToken,
    ) -> SubscriptionEnd {
        if !self.holds_escrow_lease().await {
            return SubscriptionEnd::NotLeased;
        }
        // Start from the stored swaps, which include ones another process made.
        match self.competition_store.pending_ark_swaps().await {
            Ok(pending) => self.escrow_watch.replace(
                pending
                    .iter()
                    .map(|pending| pending.escrow_address.as_str()),
            ),
            Err(e) => {
                return SubscriptionEnd::Dropped {
                    recovered: false,
                    reason: format!("cannot read the pending escrow swaps: {e}"),
                }
            }
        }
        let every = self.escrow_watch.check_lease_every();
        let mut lease_check = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        let mut live: Option<LiveSubscription> = None;
        let end = loop {
            if let Err(e) = self.update_escrow_subscription(ark, &mut live).await {
                break SubscriptionEnd::Dropped {
                    recovered: subscription_recovered(&live),
                    reason: e.to_string(),
                };
            }
            tokio::select! {
                _ = cancel.cancelled() => break SubscriptionEnd::Cancelled,
                _ = self.escrow_watch.changed.notified() => {}
                _ = lease_check.tick() => {
                    if !self.holds_escrow_lease().await {
                        break SubscriptionEnd::NotLeased;
                    }
                }
                message = next_escrow_event(&mut live) => match message {
                    Ok(Some(event)) => self.escrow_event(ark, event).await,
                    Ok(None) => break SubscriptionEnd::Dropped {
                        recovered: subscription_recovered(&live),
                        reason: "the server ended the stream".into(),
                    },
                    Err(reason) => break SubscriptionEnd::Dropped { recovered: subscription_recovered(&live), reason },
                },
            }
        };
        // A subscription left behind would cost the server until it noticed; one that dropped
        // is gone already.
        if let (SubscriptionEnd::Cancelled | SubscriptionEnd::NotLeased, Some(live)) = (&end, live)
        {
            let scripts = live.scripts.into_iter().collect();
            let unsubscribe = ark.transport.unsubscribe_scripts(&live.id, scripts);
            if let Ok(Err(e)) = tokio::time::timeout(Duration::from_secs(5), unsubscribe).await {
                debug!(
                    "Could not end the Arkade escrow subscription {}: {e}",
                    live.id
                );
            }
        }
        end
    }

    /// Bring the subscription in line with the escrows to watch, opening it once there is one.
    async fn update_escrow_subscription(
        &self,
        ark: &Arkade,
        live: &mut Option<LiveSubscription>,
    ) -> Result<(), coordinator_ark::Error> {
        let scripts = self.escrow_watch.scripts();
        match live {
            // The server is not asked for a subscription to nothing.
            None if scripts.is_empty() => {}
            None => {
                let id = ark
                    .transport
                    .subscribe_scripts(scripts.iter().cloned().collect(), None)
                    .await?;
                let events = ark.transport.subscription_events(&id).await?;
                info!(
                    "Watching {} pending escrows on Arkade in subscription {id}",
                    scripts.len()
                );
                crate::metrics::ESCROW_SUBSCRIPTION_UP.set(1);
                *live = Some(LiveSubscription {
                    id,
                    events,
                    scripts,
                    last_message: tokio::time::Instant::now(),
                    opened_at: tokio::time::Instant::now(),
                });
            }
            Some(live) => {
                let added: Vec<String> = scripts.difference(&live.scripts).cloned().collect();
                let removed: Vec<String> = live.scripts.difference(&scripts).cloned().collect();
                if !added.is_empty() {
                    ark.transport
                        .subscribe_scripts(added, Some(live.id.clone()))
                        .await?;
                }
                if !removed.is_empty() {
                    ark.transport.unsubscribe_scripts(&live.id, removed).await?;
                }
                live.scripts = scripts;
            }
        }
        Ok(())
    }

    async fn escrow_event(&self, ark: &Arkade, event: SubscriptionEvent) {
        let transaction = match event {
            SubscriptionEvent::Transaction(transaction) => transaction,
            SubscriptionEvent::Started(id) => {
                debug!("Arkade opened the escrow subscription stream {id}");
                return;
            }
            SubscriptionEvent::Heartbeat => return,
        };
        crate::metrics::ESCROW_EVENTS.inc();
        let scripts = self.escrow_watch.saw(&transaction);
        if scripts.is_empty() {
            return;
        }
        if let Err(e) = self.settle_watched_escrows(ark, &scripts).await {
            warn!(
                "Could not settle the escrows Arkade transaction {} paid: {e}",
                transaction.txid
            );
        }
    }

    /// Settle the pending swaps whose escrows are at `scripts`, as far as ark-swapd reports
    /// them paid. One it does not yet is settled by the next check of the swaps, from what the
    /// subscription reported.
    async fn settle_watched_escrows(&self, ark: &Arkade, scripts: &[String]) -> Result<(), Error> {
        let watched = |pending: &PendingArkSwap| {
            escrow_script(&pending.escrow_address).is_some_and(|script| scripts.contains(&script))
        };
        for pending in self.competition_store.pending_ark_swaps().await? {
            if !watched(&pending) {
                continue;
            }
            let swap = match ark.swaps.swap(pending.swap_id).await {
                Ok(swap) => swap,
                Err(e) => {
                    self.report_swap(&pending, format!("cannot be read from ark-swapd: {e:#}"));
                    continue;
                }
            };
            if !swap.state.player_paid() || (swap.escrow_vtxo.is_none() && swap.ark_txid.is_none())
            {
                continue;
            }
            // An event that names the script but carries no VTXO there says only where to look.
            let mut vtxos = self.escrow_watch.seen(&pending.escrow_address);
            if vtxos.is_empty() {
                vtxos = match ark
                    .transport
                    .vtxos(vec![pending.escrow_address.clone()])
                    .await
                {
                    Ok(vtxos) => vtxos,
                    Err(e) => {
                        self.report_swap(
                            &pending,
                            format!("cannot list its escrow on Arkade: {e}"),
                        );
                        continue;
                    }
                };
            }
            self.settle_paid_swap(ark, &pending, &swap, &vtxos).await?;
        }
        Ok(())
    }
}

/// The next event of the open subscription, `None` when its stream ends, or why it failed. It
/// fails when the server sends nothing, not even a heartbeat, for too long. Without an open
/// subscription, never.
async fn next_escrow_event(
    live: &mut Option<LiveSubscription>,
) -> Result<Option<SubscriptionEvent>, String> {
    let Some(live) = live else {
        return std::future::pending().await;
    };
    let deadline = live.last_message + ESCROW_SUBSCRIPTION_SILENCE;
    match tokio::time::timeout_at(deadline, live.events.next()).await {
        Err(_) => Err(format!(
            "nothing, not even a heartbeat, for {}s",
            ESCROW_SUBSCRIPTION_SILENCE.as_secs()
        )),
        Ok(None) => Ok(None),
        Ok(Some(Err(e))) => Err(e.to_string()),
        Ok(Some(Ok(event))) => {
            live.last_message = tokio::time::Instant::now();
            Ok(Some(event))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_subscription_waits_longer_each_time_up_to_a_minute() {
        assert_eq!(resubscribe_delay(1).as_secs(), 2);
        assert_eq!(resubscribe_delay(2).as_secs(), 4);
        assert_eq!(resubscribe_delay(5).as_secs(), 32);
        assert_eq!(resubscribe_delay(6).as_secs(), 60);
        assert_eq!(resubscribe_delay(40).as_secs(), 60);
    }

    #[test]
    fn escrows_are_listed_at_most_every_thirty_seconds() {
        let watch = EscrowWatch::default();
        assert!(watch.sweep_due());
        assert!(!watch.sweep_due(), "listed a moment ago");
        *watch.last_sweep.lock().unwrap() = Some(Instant::now() - ESCROW_SWEEP_EVERY);
        assert!(watch.sweep_due());
    }
}
