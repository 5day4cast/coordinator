//! Following each run's money until it settles, and saving what was found as the run's trail.
//!
//! Every few seconds the tracker looks at the competition of each run whose money it still
//! follows. When the competition moved on, it looks everything up again: the competition, each
//! player's entry and payout, ark-swapd's swaps, the funding, outcome and closing transactions,
//! and the payments on the Lightning nodes synth can reach. Once the money settles, the run gets a
//! last step saying how. Money that is stuck fails the run, and is looked at every so often until
//! it moves, so its trail can say where it was held and how it left.
//!
//! Synth only reads: it never moves or refunds anyone's money.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{info, warn};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::{
    ended, judge, ChainTx, EntryPayment, EntryTrace, Evidence, Held, LatePayment, Money,
    PayeeCheck, PayoutSeen, PayoutState, RefundSeen, RouteHop, SwapSeen, Trail, VtxoSeen,
};
use crate::ark_swap::{ArkSwap, Swap};
use crate::client::competitions::CompetitionResponse;
use crate::client::CoordinatorClient;
use crate::crypto::keys::SynthUser;
use crate::db::{SynthDb, TestRun, TestStep, Verdict};
use crate::events::{Event, Events};
use crate::lnd::{Lnd, LndConfig, NodeIdentity, Tracked};
use crate::settlement::Settlement;

/// How synth follows runs' money, and where it points people to look it up.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TrailConfig {
    /// How often the followed runs' competitions are looked at.
    pub interval_secs: u64,
    /// How long after a run starts synth stops following its money. A lifecycle competition
    /// pays out about three hours after its run ends, once its outcome has 144 blocks on it.
    pub follow_timeout_secs: u64,
    /// How long synth keeps watching stuck money for it to move, from when it found it stuck.
    pub stuck_watch_secs: u64,
    /// A mempool.space-style explorer for the chain the contracts settle on, for links and fees.
    pub explorer_url: String,
    /// The oracle's public site, for links to each competition's event.
    pub oracle_url: String,
    /// The Arkade server, whose indexer says whether an escrow output was spent or swept and
    /// when it expires. Without it the run page shows the command to look it up.
    pub arkd_url: Option<String>,
    /// The node serving the players' Lightning Address. With it, synth checks that each payout
    /// paid that node rather than just that some invoice settled.
    pub payee: Option<PayeeConfig>,
}

impl Default for TrailConfig {
    fn default() -> Self {
        Self {
            interval_secs: 20,
            follow_timeout_secs: 8 * 3600,
            stuck_watch_secs: 14 * 24 * 3600,
            explorer_url: "https://mutinynet.com".to_string(),
            oracle_url: "https://4casttruth.win".to_string(),
            arkd_url: None,
            payee: None,
        }
    }
}

/// The node the players' payouts and refunds should reach.
#[derive(Debug, Clone, Deserialize)]
pub struct PayeeConfig {
    /// Its public key. Taken from `lnd` when that is set.
    #[serde(default)]
    pub pubkey: Option<String>,
    /// The node itself, with a macaroon that may read invoices, to confirm a payout on the
    /// payee's side.
    #[serde(default)]
    pub lnd: Option<LndConfig>,
}

/// How long a run's trail may go without a full look, even if its competition seems not to move:
/// payouts show up on the nodes and in ark-swapd without the competition's state changing.
const REFRESH_AT_LEAST_EVERY: Duration = Duration::from_secs(5 * 60);

/// How often stuck money is looked at, to notice it move.
const STUCK_RECHECK: Duration = Duration::from_secs(15 * 60);

/// How often ark-swapd is asked for swaps that record no escrow output.
const UNRECORDED_SWAPS_EVERY: Duration = Duration::from_secs(5 * 60);

/// A Lightning node synth can reach, by the name it gives itself.
struct Node {
    lnd: Lnd,
    identity: OnceCell<NodeIdentity>,
}

impl Node {
    fn new(lnd: Lnd) -> Self {
        Self {
            lnd,
            identity: OnceCell::new(),
        }
    }

    async fn identity(&self) -> Option<&NodeIdentity> {
        self.identity
            .get_or_try_init(|| self.lnd.identity())
            .await
            .ok()
    }

    async fn is(&self, pubkey: &str) -> bool {
        self.identity()
            .await
            .is_some_and(|identity| identity.pubkey == pubkey)
    }

    async fn name(&self) -> String {
        match self.identity().await {
            Some(identity) if !identity.alias.is_empty() => identity.alias.clone(),
            Some(identity) => identity.pubkey.chars().take(12).collect(),
            None => "an unreachable node".to_string(),
        }
    }
}

/// The node payouts should reach: its key, and the node itself if synth may read its invoices.
pub struct Payee {
    pub pubkey: Option<String>,
    pub lnd: Option<Lnd>,
}

/// What ark-swapd last said about swaps that record no escrow output.
#[derive(Debug, Clone)]
pub struct UnrecordedSwaps {
    pub checked_at: OffsetDateTime,
    pub swaps: Vec<Swap>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct Tracker {
    inner: Arc<Inner>,
}

struct Inner {
    client: CoordinatorClient,
    db: SynthDb,
    events: Events,
    /// The payer, first when there is one, and the other nodes synth reaches: where entries,
    /// payouts and refunds are paid from, and where ark-swapd's invoices are.
    nodes: Vec<Node>,
    has_payer: bool,
    payee_pubkey: Option<String>,
    payee_node: Option<Node>,
    ark_swap: Option<ArkSwap>,
    http: reqwest::Client,
    config: TrailConfig,
    /// Runs being refreshed now, so a page asking twice does not look everything up twice.
    busy: Mutex<HashSet<String>>,
    /// Each followed run's competition as last looked at, and when its trail was last refreshed.
    seen: Mutex<HashMap<String, (String, Instant)>>,
    unrecorded: Mutex<Option<(Instant, UnrecordedSwaps)>>,
}

impl Tracker {
    /// `payer` is the node the scenarios pay entries from; `others` are the other nodes synth
    /// reaches, such as the one the coordinator pays from.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: CoordinatorClient,
        db: SynthDb,
        events: Events,
        payer: Option<Lnd>,
        others: Vec<Lnd>,
        payee: Option<Payee>,
        ark_swap: Option<ArkSwap>,
        config: TrailConfig,
    ) -> Result<Self> {
        let (payee_pubkey, payee_node) = match payee {
            Some(payee) => (payee.pubkey, payee.lnd.map(Node::new)),
            None => (None, None),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                db,
                events,
                has_payer: payer.is_some(),
                nodes: payer.into_iter().chain(others).map(Node::new).collect(),
                payee_pubkey,
                payee_node,
                ark_swap,
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()?,
                config,
                busy: Mutex::default(),
                seen: Mutex::default(),
                unrecorded: Mutex::default(),
            }),
        })
    }

    pub fn config(&self) -> &TrailConfig {
        &self.inner.config
    }

    /// Where ark-swapd is, for naming lookups an operator can repeat.
    pub fn ark_swap_url(&self) -> Option<&str> {
        self.inner.ark_swap.as_ref().map(ArkSwap::url)
    }

    /// What ark-swapd last said about swaps that record no escrow output, if synth asks it.
    pub fn unrecorded_swaps(&self) -> Option<UnrecordedSwaps> {
        self.inner
            .unrecorded
            .lock()
            .expect("unrecorded swaps lock")
            .as_ref()
            .map(|(_, swaps)| swaps.clone())
    }

    /// Follow the runs' money for as long as synth runs.
    pub async fn run(&self) {
        info!(
            "Following each run's money every {}s, for up to {}s after it starts",
            self.inner.config.interval_secs, self.inner.config.follow_timeout_secs
        );
        loop {
            if let Err(e) = self.tick().await {
                warn!("Cannot follow the runs' money: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(self.inner.config.interval_secs)).await;
        }
    }

    async fn tick(&self) -> Result<()> {
        let runs = self.inner.db.runs_to_follow().await?;
        {
            // Forget the runs no longer followed, so the map stays as small as the queue.
            let followed: HashSet<&str> = runs.iter().map(|run| run.id.as_str()).collect();
            self.inner
                .seen
                .lock()
                .expect("seen lock")
                .retain(|id, _| followed.contains(id.as_str()));
        }
        for run in &runs {
            let Some(competition_id) = competition_of(run) else {
                continue;
            };
            if let Err(e) = self.follow(run, competition_id).await {
                warn!("Cannot follow run {}'s money: {e:#}", run.id);
            }
        }
        self.check_unrecorded_swaps().await;
        Ok(())
    }

    /// Look at a run's competition, and refresh the run's trail if it moved or is due. Stuck
    /// money is only looked at every so often.
    async fn follow(&self, run: &TestRun, competition_id: Uuid) -> Result<()> {
        let last = self
            .inner
            .seen
            .lock()
            .expect("seen lock")
            .get(&run.id)
            .cloned();
        if run.money.as_deref() == Some("stuck")
            && last
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < STUCK_RECHECK)
        {
            return Ok(());
        }
        let competition = self.inner.client.get_competition(&competition_id).await;
        let fingerprint = format!(
            "{}|{}",
            run.status,
            competition
                .as_ref()
                .map(fingerprint)
                .unwrap_or_else(|_| "unavailable".into())
        );
        let due = match &last {
            Some((seen, at)) => *seen != fingerprint || at.elapsed() >= REFRESH_AT_LEAST_EVERY,
            None => true,
        };
        if !due || !self.start(&run.id) {
            return Ok(());
        }
        let refreshed = self.refresh_run(run, competition_id, competition).await;
        self.finish(&run.id);
        refreshed?;
        self.inner
            .seen
            .lock()
            .expect("seen lock")
            .insert(run.id.clone(), (fingerprint, Instant::now()));
        Ok(())
    }

    /// Refresh a run's trail in the background, unless it is being refreshed already: for a page
    /// showing a run nobody has followed yet.
    pub fn request(&self, run_id: &str) {
        if !self.start(run_id) {
            return;
        }
        let tracker = self.clone();
        let run_id = run_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = tracker.refresh(&run_id).await {
                warn!("Cannot trace run {run_id}'s money: {e:#}");
            }
            tracker.finish(&run_id);
        });
    }

    /// Look everything about a run's money up now. None for a run with no competition.
    async fn refresh(&self, run_id: &str) -> Result<Option<Trail>> {
        let Some(run) = self.inner.db.get_run(run_id).await? else {
            return Ok(None);
        };
        let Some(competition_id) = competition_of(&run) else {
            return Ok(None);
        };
        let competition = self.inner.client.get_competition(&competition_id).await;
        self.refresh_run(&run, competition_id, competition)
            .await
            .map(Some)
    }

    fn start(&self, run_id: &str) -> bool {
        self.inner
            .busy
            .lock()
            .expect("busy lock")
            .insert(run_id.to_string())
    }

    fn finish(&self, run_id: &str) {
        self.inner.busy.lock().expect("busy lock").remove(run_id);
    }

    async fn refresh_run(
        &self,
        run: &TestRun,
        competition_id: Uuid,
        competition: Result<CompetitionResponse>,
    ) -> Result<Trail> {
        let now = OffsetDateTime::now_utc();
        let steps = self.inner.db.get_steps(&run.id).await?;
        let entries = entries_of(&steps);
        let previous = self.inner.db.get_trail(&run.id).await?;
        let previous = previous.as_ref();
        let mut gaps = Vec::new();

        let competition = match competition {
            Ok(competition) => Some(competition),
            Err(e) => {
                gaps.push(format!(
                    "the coordinator did not answer for the competition: {e:#}"
                ));
                None
            }
        };
        let settlement = competition
            .as_ref()
            .and_then(Settlement::of)
            .or_else(|| previous?.settlement.clone());
        let is_ended = competition.as_ref().is_some_and(ended)
            || previous.is_some_and(|previous| previous.ended());
        // Held money is looked at closely: the invoice's state, and the escrow output's.
        let inspect = is_ended || previous.is_some_and(|p| matches!(p.money, Money::Stuck { .. }));
        let players = self.players(&entries).await;

        let late_payments = self.late_payments(&entries, previous, &mut gaps).await;
        let unverified_entries = unverified_entries(&steps, &entries, &late_payments);
        if unverified_entries > 0 {
            gaps.push(format!(
                "{unverified_entries} entry payment(s) could not be verified either way"
            ));
        }
        let swaps = self.swaps(&entries, previous, inspect, &mut gaps).await;
        let funding_tx = match competition.as_ref().and_then(CompetitionResponse::funding) {
            Some((txid, vout)) => Some(
                self.chain_tx(
                    txid,
                    vout,
                    previous.and_then(|p| p.funding_tx.as_ref()),
                    &mut gaps,
                )
                .await,
            ),
            None => previous.and_then(|p| p.funding_tx.clone()),
        };
        let outcome_tx = match competition
            .as_ref()
            .and_then(CompetitionResponse::outcome_txid)
        {
            Some(txid) => Some(
                self.chain_tx(
                    txid,
                    None,
                    previous.and_then(|p| p.outcome_tx.as_ref()),
                    &mut gaps,
                )
                .await,
            ),
            None => previous.and_then(|p| p.outcome_tx.clone()),
        };
        let closing_txs = match &competition {
            Some(competition)
                if competition.delta_broadcasted_at.is_some()
                    || competition.expiry_broadcasted_at.is_some() =>
            {
                self.closing_txs(
                    funding_tx.as_ref(),
                    outcome_tx.as_ref(),
                    previous,
                    &mut gaps,
                )
                .await
            }
            _ => previous.map(|p| p.closing_txs.clone()).unwrap_or_default(),
        };
        let payouts = self
            .payouts(
                competition_id,
                &entries,
                &players,
                settlement.as_ref(),
                previous,
                &mut gaps,
            )
            .await;
        let refunds = if is_ended {
            self.refunds(competition_id, &entries, &players, previous, &mut gaps)
                .await
        } else {
            Vec::new()
        };

        let mut trail = Trail {
            refreshed_at: now,
            competition_id,
            competition: competition
                .clone()
                .map(CompetitionResponse::slim)
                .or_else(|| previous?.competition.clone()),
            settlement,
            swaps,
            late_payments,
            payouts,
            refunds,
            funding_tx,
            outcome_tx,
            closing_txs,
            money: Money::Following,
            held: None,
            gaps,
        };
        trail.money = judge(&Evidence {
            running: run.status == "running",
            competition: trail.competition.as_ref(),
            decided: trail
                .settlement
                .as_ref()
                .is_some_and(|settlement| settlement.decided.is_some()),
            payouts: &trail.payouts,
            refunds: &trail.refunds,
            paid_entries: trail.paid_entries(&entries).len(),
            unverified_entries,
            give_up: run.status != "running" && followed_long_enough(run, &self.inner.config),
            now,
        });
        trail.held = held(&trail, &entries, previous.and_then(|p| p.held.clone()), now);
        self.record(run, &trail, previous).await?;
        Ok(trail)
    }

    /// Save the trail, with the step its verdict adds and the run it fails, together.
    async fn record(&self, run: &TestRun, trail: &Trail, previous: Option<&Trail>) -> Result<()> {
        let was = previous.map(|p| p.money.label());
        let changed = was != Some(trail.money.label());
        // A run followed only after it finished, as when this version first starts, keeps the
        // status it finished with: its verdict is recorded, but history is not rewritten.
        let backfill = previous.is_none() && run.status != "running";
        let details = serde_json::to_string(&trail.money)?;
        let error = trail.money.reason().filter(|_| !trail.money.is_good());
        let step = match &trail.money {
            Money::Following => None,
            Money::Stuck { .. } if changed => Some("money_stuck"),
            _ if changed && trail.money.is_final() => Some("money_settled"),
            _ => None,
        };
        let fail = match (&trail.money, error) {
            (Money::Stuck { .. }, Some(reason)) if changed && !backfill => {
                info!("Run {}'s money is stuck: {reason}", run.id);
                Some(format!("Its steps passed, but {reason}"))
            }
            _ => None,
        };
        let since = run
            .completed_at
            .as_deref()
            .and_then(parse_time)
            .map(|at| (OffsetDateTime::now_utc() - at).whole_milliseconds().max(0) as i64)
            .unwrap_or(0);
        let watched_long_enough = trail.held.as_ref().is_some_and(|held| {
            OffsetDateTime::now_utc() - held.found
                > time::Duration::seconds(self.inner.config.stuck_watch_secs as i64)
        });
        let follow = match trail.money {
            Money::Stuck { .. } => !watched_long_enough,
            _ => !trail.money.is_final(),
        };
        self.inner
            .db
            .record_money(
                &run.id,
                &Verdict {
                    trail,
                    follow,
                    step: step.map(|name| (name, since, error, details.as_str())),
                    fail_passed_run: fail.as_deref(),
                },
            )
            .await?;
        if let Some(step) = step {
            self.inner.events.send(Event::StepFinished {
                run_id: run.id.clone(),
                step: step.to_string(),
                passed: error.is_none(),
            });
        }
        self.inner.events.send(Event::TrailUpdated {
            run_id: run.id.clone(),
        });
        Ok(())
    }

    /// The scenario's players, by name, with the keys their requests are signed with.
    async fn players(&self, entries: &[EntryTrace]) -> HashMap<String, SynthUser> {
        let names: HashSet<&str> = entries.iter().map(|entry| entry.user.as_str()).collect();
        self.inner
            .db
            .list_users()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|user| names.contains(user.name.as_str()))
            .filter_map(|user| {
                let player = SynthUser::from_secret_key(&user.name, &user.nostr_secret_key).ok()?;
                Some((user.name, player))
            })
            .collect()
    }

    /// What the payer says of each entry payment its step did not see finish. A definite answer
    /// (paid, failed, or never made) is kept and not asked again; a payment in flight is asked
    /// about again next time.
    async fn late_payments(
        &self,
        entries: &[EntryTrace],
        previous: Option<&Trail>,
        gaps: &mut Vec<String>,
    ) -> Vec<LatePayment> {
        let mut found = Vec::new();
        for entry in entries.iter().filter(|entry| entry.may_have_paid()) {
            let Some(hash) = entry.payment_hash.clone() else {
                continue;
            };
            if let Some(known) = previous.and_then(|p| {
                p.late_payments
                    .iter()
                    .find(|late| late.payment_hash == hash)
            }) {
                found.push(known.clone());
                continue;
            }
            let Some(payer) = self.inner.nodes.first().filter(|_| self.inner.has_payer) else {
                gaps.push(format!(
                    "no payer node is configured to say whether {}'s entry was paid",
                    entry.user
                ));
                continue;
            };
            let payment = match payer.lnd.track(&hash).await {
                Ok(Tracked::Succeeded(paid)) => {
                    let identity = payer.identity().await.cloned().unwrap_or_default();
                    Some(EntryPayment {
                        payer_alias: identity.alias,
                        payer_pubkey: identity.pubkey,
                        preimage: paid.preimage,
                        fee_msat: paid.fee_msat,
                        route: paid
                            .route
                            .into_iter()
                            .map(|hop| RouteHop {
                                chan_id: hop.chan_id,
                                pubkey: hop.pub_key,
                                alias: None,
                            })
                            .collect(),
                    })
                }
                Ok(Tracked::Failed | Tracked::NeverMade) => None,
                Ok(Tracked::InFlight) => {
                    gaps.push(format!("{}'s entry payment is still in flight", entry.user));
                    continue;
                }
                Err(e) => {
                    gaps.push(format!(
                        "{}'s entry payment on {}: {e:#}",
                        entry.user,
                        payer.name().await
                    ));
                    continue;
                }
            };
            found.push(LatePayment {
                user: entry.user.clone(),
                payment_hash: hash,
                payment,
            });
        }
        found
    }

    /// Each entry's swap into its escrow, from ark-swapd. A swap ark-swapd is done with is not
    /// looked up again. With `inspect`, the entry invoice's state and the escrow output are looked
    /// up too.
    async fn swaps(
        &self,
        entries: &[EntryTrace],
        previous: Option<&Trail>,
        inspect: bool,
        gaps: &mut Vec<String>,
    ) -> Vec<SwapSeen> {
        let earlier = |hash: &str| {
            previous?
                .swaps
                .iter()
                .find(|swap| swap.payment_hash == hash)
                .cloned()
        };
        let traced: Vec<&EntryTrace> = entries
            .iter()
            .filter(|entry| entry.payment_hash.is_some() && !entry.settled_by_test_endpoint)
            .filter(|entry| entry.paid || entry.may_have_paid())
            .collect();
        let Some(ark_swap) = &self.inner.ark_swap else {
            if !traced.is_empty() {
                gaps.push(
                    "ark-swapd is not configured, so the escrow swaps are not looked up".into(),
                );
            }
            return previous.map(|p| p.swaps.clone()).unwrap_or_default();
        };
        let mut swaps = Vec::new();
        for entry in traced {
            let hash = entry.payment_hash.as_deref().unwrap_or_default();
            let mut swap = match earlier(hash) {
                Some(swap) if swap.is_final() => swap,
                earlier => match ark_swap.swap_for(hash).await {
                    Ok(Some(found)) => SwapSeen {
                        user: entry.user.clone(),
                        payment_hash: found.payment_hash,
                        id: found.id,
                        state: found.state,
                        amount_sat: found.amount_sat,
                        escrow_address: found.escrow_address,
                        escrow_vtxo: found.escrow_vtxo,
                        ark_txid: found.ark_txid,
                        error: found.error,
                        invoice_state: earlier.as_ref().and_then(|e| e.invoice_state.clone()),
                        vtxo: earlier.and_then(|e| e.vtxo),
                    },
                    Ok(None) => {
                        gaps.push(format!(
                            "ark-swapd has no swap for {}'s payment",
                            entry.user
                        ));
                        continue;
                    }
                    Err(e) => {
                        // Keep what was known, and go on to the other entries' swaps.
                        gaps.push(format!("ark-swapd's swap for {}: {e:#}", entry.user));
                        match earlier {
                            Some(earlier) => earlier,
                            None => continue,
                        }
                    }
                },
            };
            if inspect {
                self.inspect_swap(entry, &mut swap, gaps).await;
            }
            swaps.push(swap);
        }
        swaps
    }

    /// Look up what holds an entry's money: its invoice on the node that issued it, and its
    /// escrow output in the Arkade server's indexer.
    async fn inspect_swap(&self, entry: &EntryTrace, swap: &mut SwapSeen, gaps: &mut Vec<String>) {
        let issuer = entry
            .invoice
            .as_deref()
            .and_then(|invoice| {
                invoice
                    .trim()
                    .parse::<lightning_invoice::Bolt11Invoice>()
                    .ok()
            })
            .map(|invoice| payee_of(&invoice));
        if swap.invoice_state.as_deref() != Some("SETTLED") {
            if let Some(node) = self.node_with(issuer.as_deref()).await {
                match node.lnd.lookup_invoice(&swap.payment_hash).await {
                    Ok(Some(invoice)) => swap.invoice_state = Some(invoice.state),
                    Ok(None) => swap.invoice_state = Some("not found".into()),
                    Err(e) => gaps.push(format!("{}'s entry invoice: {e:#}", entry.user)),
                }
            }
        }
        let (Some(arkd), Some(outpoint)) = (&self.inner.config.arkd_url, &swap.escrow_vtxo) else {
            return;
        };
        if swap
            .vtxo
            .as_ref()
            .is_some_and(|vtxo| vtxo.spent || vtxo.swept)
        {
            return;
        }
        match vtxo(&self.inner.http, arkd, outpoint).await {
            Ok(Some(found)) => swap.vtxo = Some(found),
            Ok(None) => gaps.push(format!(
                "the Arkade indexer does not know {}'s escrow output",
                entry.user
            )),
            Err(e) => gaps.push(format!("the Arkade indexer: {e:#}")),
        }
    }

    /// The reachable node with this public key.
    async fn node_with(&self, pubkey: Option<&str>) -> Option<&Node> {
        let pubkey = pubkey?;
        for node in self.inner.nodes.iter().chain(&self.inner.payee_node) {
            if node.is(pubkey).await {
                return Some(node);
            }
        }
        None
    }

    /// A transaction as the explorer sees it. One already confirmed with its fee known is not
    /// looked up again, and one the explorer cannot find now keeps what was known of it.
    async fn chain_tx(
        &self,
        txid: String,
        vout: Option<u32>,
        previous: Option<&ChainTx>,
        gaps: &mut Vec<String>,
    ) -> ChainTx {
        let previous = previous.filter(|p| p.txid == txid);
        if let Some(known) = previous.filter(|p| p.confirmed && p.fee_sat.is_some()) {
            return known.clone();
        }
        #[derive(Deserialize)]
        struct Found {
            fee: Option<u64>,
            status: Status,
            #[serde(default)]
            vout: Vec<Output>,
        }
        #[derive(Deserialize)]
        struct Status {
            confirmed: bool,
        }
        #[derive(Deserialize)]
        struct Output {
            value: u64,
        }
        match self.explorer::<Found>(&format!("tx/{txid}")).await {
            Ok(found) => ChainTx {
                txid,
                vout,
                fee_sat: found.fee,
                value_sat: Some(found.vout.iter().map(|output| output.value).sum()),
                confirmed: found.status.confirmed,
            },
            Err(e) => {
                gaps.push(format!("the explorer did not find {txid}: {e:#}"));
                previous.cloned().unwrap_or(ChainTx {
                    txid,
                    vout,
                    fee_sat: None,
                    value_sat: None,
                    confirmed: false,
                })
            }
        }
    }

    /// What spent the outcome's outputs, or the funding output other than the outcome: the delta
    /// or expiry transactions, where a pot that was not paid out went.
    async fn closing_txs(
        &self,
        funding: Option<&ChainTx>,
        outcome: Option<&ChainTx>,
        previous: Option<&Trail>,
        gaps: &mut Vec<String>,
    ) -> Vec<ChainTx> {
        #[derive(Deserialize)]
        struct Outspend {
            spent: bool,
            #[serde(default)]
            txid: Option<String>,
        }
        let mut spenders = Vec::new();
        if let Some(outcome) = outcome {
            match self
                .explorer::<Vec<Outspend>>(&format!("tx/{}/outspends", outcome.txid))
                .await
            {
                Ok(outspends) => spenders.extend(
                    outspends
                        .into_iter()
                        .filter(|outspend| outspend.spent)
                        .filter_map(|outspend| outspend.txid),
                ),
                Err(e) => gaps.push(format!("what spent the outcome's outputs: {e:#}")),
            }
        }
        if let Some((funding, vout)) = funding.and_then(|f| Some((f, f.vout?))) {
            match self
                .explorer::<Outspend>(&format!("tx/{}/outspend/{vout}", funding.txid))
                .await
            {
                Ok(outspend) => spenders.extend(
                    outspend
                        .txid
                        .filter(|txid| outcome.is_none_or(|outcome| &outcome.txid != txid)),
                ),
                Err(e) => gaps.push(format!("what spent the contract's funding: {e:#}")),
            }
        }
        spenders.sort();
        spenders.dedup();
        if spenders.is_empty() {
            return previous.map(|p| p.closing_txs.clone()).unwrap_or_default();
        }
        let mut closing = Vec::new();
        for txid in spenders {
            let known = previous.and_then(|p| p.closing_txs.iter().find(|tx| tx.txid == txid));
            closing.push(self.chain_tx(txid, None, known, gaps).await);
        }
        closing
    }

    async fn explorer<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!(
            "{}/api/{path}",
            self.inner.config.explorer_url.trim_end_matches('/')
        );
        let response = self.inner.http.get(&url).send().await?;
        anyhow::ensure!(response.status().is_success(), "{}", response.status());
        Ok(response.json().await?)
    }

    /// Each entered player's payout: their share, and what the coordinator and the reachable
    /// nodes say about paying it.
    async fn payouts(
        &self,
        competition_id: Uuid,
        entries: &[EntryTrace],
        players: &HashMap<String, SynthUser>,
        settlement: Option<&Settlement>,
        previous: Option<&Trail>,
        gaps: &mut Vec<String>,
    ) -> Vec<PayoutSeen> {
        let mut payouts = Vec::new();
        for entry in entries.iter().filter(|entry| entry.entry_submitted) {
            let (Some(entry_id), Some(player)) = (entry.entry_id, players.get(&entry.user)) else {
                continue;
            };
            let listed = match self
                .inner
                .client
                .list_entries(&player.nostr_keys, Some(&competition_id))
                .await
            {
                Ok(listed) => listed,
                Err(e) => {
                    gaps.push(format!("{}'s entries: {e:#}", entry.user));
                    payouts.extend(
                        previous
                            .and_then(|p| p.payouts.iter().find(|p| p.entry_id == entry_id))
                            .cloned(),
                    );
                    continue;
                }
            };
            let Some(listed) = listed.into_iter().find(|listed| listed.id == entry_id) else {
                continue;
            };
            let share = settlement.and_then(|settlement| {
                settlement
                    .shares
                    .iter()
                    .find(|share| share.pubkey == listed.ephemeral_pubkey)
            });
            let invoice = listed.payout_ln_invoice.as_deref().and_then(|invoice| {
                invoice
                    .trim()
                    .parse::<lightning_invoice::Bolt11Invoice>()
                    .ok()
            });
            let mut payout = PayoutSeen {
                user: entry.user.clone(),
                entry_id,
                pubkey: listed.ephemeral_pubkey.clone(),
                weight: share.map_or(0, |share| share.weight),
                owed_sats: share.map_or(0, |share| share.owed_sats),
                sent_at: listed.paid_out_at,
                invoice: listed.payout_ln_invoice.clone(),
                amount_sats: invoice
                    .as_ref()
                    .and_then(|i| i.amount_milli_satoshis())
                    .map(|msat| msat / 1000),
                payment_hash: invoice.as_ref().map(|i| i.payment_hash().to_string()),
                payee_pubkey: invoice.as_ref().map(payee_of),
                lightning_address: entry.lightning_address.clone(),
                ..PayoutSeen::default()
            };
            let earlier = previous.and_then(|p| {
                p.payouts
                    .iter()
                    .find(|p| p.payment_hash.is_some() && p.payment_hash == payout.payment_hash)
            });
            payout.payee = self.payee_check_of(&payout).await;
            // A confirmed payout is not looked up again, unless the payee's node could now
            // confirm it on its side.
            let settled = earlier.filter(|earlier| {
                earlier.is_confirmed()
                    && payout.payee != PayeeCheck::OtherNode
                    && (earlier.payee == PayeeCheck::SettledOnPayee
                        || self.inner.payee_node.is_none())
            });
            match settled {
                Some(earlier) => {
                    payout.amount_sats = earlier.amount_sats;
                    payout.preimage = earlier.preimage.clone();
                    payout.fee_msat = earlier.fee_msat;
                    payout.paid_by = earlier.paid_by.clone();
                    payout.payee_alias = earlier.payee_alias.clone();
                    if earlier.payee == PayeeCheck::SettledOnPayee {
                        payout.payee = PayeeCheck::SettledOnPayee;
                    }
                }
                None => {
                    if payout.sent_at.is_some() {
                        self.look_up_payout(&mut payout).await;
                    }
                    if payout.payee_alias.is_none() {
                        payout.payee_alias = match earlier {
                            Some(earlier) => earlier.payee_alias.clone(),
                            None => self.alias_of(payout.payee_pubkey.as_deref()).await,
                        };
                    }
                }
            }
            payouts.push(payout);
        }
        payouts
    }

    /// Whether the payout's invoice pays the configured payee, as far as its key tells.
    async fn payee_check_of(&self, payout: &PayoutSeen) -> PayeeCheck {
        let expected = match (&self.inner.payee_pubkey, &self.inner.payee_node) {
            (Some(pubkey), _) => Some(pubkey.clone()),
            (None, Some(node)) => node
                .identity()
                .await
                .map(|identity| identity.pubkey.clone()),
            (None, None) => None,
        };
        match (expected, &payout.payee_pubkey) {
            (Some(expected), Some(payee)) if &expected == payee => PayeeCheck::Matches,
            (Some(_), Some(_)) => PayeeCheck::OtherNode,
            _ => PayeeCheck::Unchecked,
        }
    }

    /// Find a payout on the nodes synth can reach: the preimage and fee from the node that paid
    /// it, and, when synth reaches the payee's node, whether that node was paid.
    async fn look_up_payout(&self, payout: &mut PayoutSeen) {
        let Some(hash) = payout.payment_hash.clone() else {
            return;
        };
        for node in &self.inner.nodes {
            if let Ok(Some(paid)) = node.lnd.payment(&hash).await {
                payout.amount_sats = Some(paid.value_sat);
                payout.preimage = Some(paid.preimage);
                payout.fee_msat = Some(paid.fee_msat);
                payout.paid_by = Some(node.name().await);
                break;
            }
        }
        if payout.payee == PayeeCheck::OtherNode {
            return;
        }
        let Some(node) = self.node_with(payout.payee_pubkey.as_deref()).await else {
            return;
        };
        if let Ok(Some(invoice)) = node.lnd.lookup_invoice(&hash).await {
            if invoice.state == "SETTLED" {
                payout.amount_sats = Some(invoice.amt_paid_sat);
                payout.preimage = Some(invoice.preimage);
                payout.payee_alias = Some(node.name().await);
                if self
                    .inner
                    .payee_node
                    .as_ref()
                    .is_some_and(|payee| std::ptr::eq(payee, node))
                {
                    payout.payee = PayeeCheck::SettledOnPayee;
                }
            }
        }
    }

    async fn alias_of(&self, pubkey: Option<&str>) -> Option<String> {
        let pubkey = pubkey?;
        if let Some(node) = self.node_with(Some(pubkey)).await {
            return Some(node.name().await);
        }
        self.inner
            .nodes
            .first()?
            .lnd
            .alias_of(pubkey)
            .await
            .ok()
            .flatten()
    }

    /// Each paid ticket's refund, once its competition ended. A settled one whose payment was
    /// found is not looked up again.
    async fn refunds(
        &self,
        competition_id: Uuid,
        entries: &[EntryTrace],
        players: &HashMap<String, SynthUser>,
        previous: Option<&Trail>,
        gaps: &mut Vec<String>,
    ) -> Vec<RefundSeen> {
        let mut refunds = Vec::new();
        for entry in entries
            .iter()
            .filter(|entry| entry.paid || entry.may_have_paid())
        {
            let (Some(ticket_id), Some(player)) = (entry.ticket_id, players.get(&entry.user))
            else {
                continue;
            };
            let earlier =
                previous.and_then(|p| p.refunds.iter().find(|r| r.ticket_id == ticket_id));
            if let Some(done) = earlier
                .filter(|r| r.is_settled() && (r.payment_hash.is_none() || r.preimage.is_some()))
            {
                refunds.push(done.clone());
                continue;
            }
            match self
                .inner
                .client
                .check_ticket_refund(&player.nostr_keys, &competition_id, &ticket_id)
                .await
            {
                Ok(Some(refund)) => {
                    let mut seen = RefundSeen {
                        user: entry.user.clone(),
                        ticket_id,
                        state: refund.state,
                        paid_sats: refund.paid_sats,
                        ark_txid: refund.ark_txid,
                        invoice: refund.invoice,
                        payment_hash: refund.payment_hash,
                        preimage: None,
                        fee_msat: None,
                        paid_by: None,
                    };
                    if let Some(hash) = seen.payment_hash.clone() {
                        for node in &self.inner.nodes {
                            if let Ok(Some(paid)) = node.lnd.payment(&hash).await {
                                seen.preimage = Some(paid.preimage);
                                seen.fee_msat = Some(paid.fee_msat);
                                seen.paid_by = Some(node.name().await);
                                break;
                            }
                        }
                    }
                    refunds.push(seen);
                }
                Ok(None) => {}
                Err(e) => {
                    gaps.push(format!("{}'s refund: {e:#}", entry.user));
                    refunds.extend(earlier.cloned());
                }
            }
        }
        refunds
    }

    /// Ask ark-swapd, every few minutes, for swaps that record no escrow output.
    async fn check_unrecorded_swaps(&self) {
        let Some(ark_swap) = &self.inner.ark_swap else {
            return;
        };
        let due = self
            .inner
            .unrecorded
            .lock()
            .expect("unrecorded swaps lock")
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= UNRECORDED_SWAPS_EVERY);
        if !due {
            return;
        }
        let checked = match ark_swap.swaps_without_escrow_vtxo().await {
            Ok(swaps) => UnrecordedSwaps {
                checked_at: OffsetDateTime::now_utc(),
                swaps,
                error: None,
            },
            Err(e) => UnrecordedSwaps {
                checked_at: OffsetDateTime::now_utc(),
                swaps: Vec::new(),
                error: Some(format!("{e:#}")),
            },
        };
        *self.inner.unrecorded.lock().expect("unrecorded swaps lock") =
            Some((Instant::now(), checked));
    }
}

/// The node an invoice pays.
fn payee_of(invoice: &lightning_invoice::Bolt11Invoice) -> String {
    invoice
        .payee_pub_key()
        .copied()
        .unwrap_or_else(|| invoice.recover_payee_pub_key())
        .to_string()
}

/// An Arkade output, from the server's indexer. None if it does not know the output.
async fn vtxo(http: &reqwest::Client, arkd: &str, outpoint: &str) -> Result<Option<VtxoSeen>> {
    #[derive(Deserialize)]
    struct Found {
        #[serde(default)]
        vtxos: Vec<Indexed>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Indexed {
        #[serde(default, deserialize_with = "number")]
        amount: u64,
        #[serde(default, deserialize_with = "number", alias = "expires_at")]
        expires_at: u64,
        #[serde(default, alias = "is_spent")]
        is_spent: bool,
        #[serde(default, alias = "is_swept")]
        is_swept: bool,
        #[serde(default, alias = "spent_by")]
        spent_by: String,
        #[serde(default, alias = "settled_by")]
        settled_by: String,
    }
    fn number<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Number {
            Text(String),
            Plain(u64),
        }
        match Number::deserialize(deserializer)? {
            Number::Text(text) if text.is_empty() => Ok(0),
            Number::Text(text) => text.parse().map_err(serde::de::Error::custom),
            Number::Plain(number) => Ok(number),
        }
    }
    let response = http
        .get(format!("{}/v1/indexer/vtxos", arkd.trim_end_matches('/')))
        .query(&[("outpoints", outpoint)])
        .send()
        .await
        .context("ask the Arkade indexer")?;
    anyhow::ensure!(response.status().is_success(), "{}", response.status());
    let found: Found = response.json().await.context("read the Arkade indexer")?;
    let non_empty = |text: String| Some(text).filter(|text| !text.is_empty());
    Ok(found.vtxos.into_iter().next().map(|vtxo| VtxoSeen {
        amount_sat: vtxo.amount,
        // The indexer gives seconds; some versions give milliseconds.
        expires_at: Some(vtxo.expires_at)
            .filter(|at| *at > 0)
            .map(|at| if at > 10_000_000_000 { at / 1000 } else { at } as i64),
        spent: vtxo.is_spent,
        swept: vtxo.is_swept,
        spent_by: non_empty(vtxo.spent_by),
        settled_by: non_empty(vtxo.settled_by),
    }))
}

/// What is held while money is stuck, and what happened to it once it moved.
fn held(
    trail: &Trail,
    entries: &[EntryTrace],
    previous: Option<Held>,
    now: OffsetDateTime,
) -> Option<Held> {
    match (&trail.money, previous) {
        (Money::Stuck { reason, since }, previous) => {
            let (sats, nearest_expiry) = holding(trail, entries);
            Some(match previous.filter(|held| held.until.is_none()) {
                Some(held) => Held {
                    sats,
                    nearest_expiry,
                    ..held
                },
                None => Held {
                    since: *since,
                    found: now,
                    reason: reason.clone(),
                    sats,
                    nearest_expiry,
                    until: None,
                    then: None,
                },
            })
        }
        (moved, Some(held)) if held.until.is_none() => Some(Held {
            until: Some(now),
            then: Some(match moved {
                Money::Refunded => {
                    let txids: Vec<&str> = trail
                        .refunds
                        .iter()
                        .filter_map(|refund| refund.ark_txid.as_deref())
                        .collect();
                    if txids.is_empty() {
                        "refunded".to_string()
                    } else {
                        format!("refunded by Arkade transaction {}", txids.join(", "))
                    }
                }
                Money::PaidOut => "paid out to the winners".to_string(),
                Money::Unverified { reason } => {
                    format!("synth stopped following it: {reason}")
                }
                other => format!("it moved on: {}", other.words()),
            }),
            ..held
        }),
        (_, previous) => previous,
    }
}

/// What stuck money amounts to, in sats, and the soonest one of its escrows expires or opens its
/// refund leaf. Escrows hold it until the contract is funded; the contract holds it after.
fn holding(trail: &Trail, entries: &[EntryTrace]) -> (u64, Option<i64>) {
    let contracted = trail.competition.as_ref().is_some_and(super::contracted);
    if contracted {
        let states = super::payout_states(&trail.payouts, trail.ended());
        let unpaid: u64 = trail
            .payouts
            .iter()
            .zip(&states)
            .filter(|(_, state)| state.is_owed() && **state != PayoutState::Paid)
            .map(|(payout, _)| payout.owed_sats)
            .sum();
        let pot = trail.settlement.as_ref().map(|s| s.pot_sats);
        return (if unpaid > 0 { unpaid } else { pot.unwrap_or(0) }, None);
    }
    let mut sats = 0;
    let mut nearest: Option<i64> = None;
    for entry in trail.paid_entries(entries) {
        if trail.refund_of(entry).is_some_and(RefundSeen::is_settled) {
            continue;
        }
        let swap = trail.swap_of(entry);
        sats += swap
            .map(|swap| swap.amount_sat)
            .or(entry.amount_sats)
            .unwrap_or(0);
        let expiries = [
            swap.and_then(|swap| swap.vtxo.as_ref()?.expires_at),
            entry.escrow.map(|escrow| escrow.refund_at),
        ];
        for at in expiries.into_iter().flatten() {
            nearest = Some(nearest.map_or(at, |nearest| nearest.min(at)));
        }
    }
    (sats, nearest)
}

/// The entries a run's steps recorded, finished or not.
pub fn entries_of(steps: &[TestStep]) -> Vec<EntryTrace> {
    steps
        .iter()
        .filter(|step| step.step_name.ends_with("_enter"))
        .filter_map(|step| step.details_json.as_deref())
        .filter_map(|details| serde_json::from_str::<EntryTrace>(details).ok())
        .collect()
}

/// How many entries may have moved money that synth cannot account for: an entry step whose
/// details are missing or unreadable, and an entry that reached paying without its step seeing the
/// payment end, which the paying node could not answer for either.
fn unverified_entries(
    steps: &[TestStep],
    entries: &[EntryTrace],
    late_payments: &[LatePayment],
) -> usize {
    let unreadable = steps
        .iter()
        .filter(|step| step.step_name.ends_with("_enter"))
        .filter(|step| {
            step.details_json
                .as_deref()
                .and_then(|details| serde_json::from_str::<serde_json::Value>(details).ok())
                .filter(|details| details.get("paid").is_some_and(|paid| paid.is_boolean()))
                .and_then(|details| serde_json::from_value::<EntryTrace>(details).ok())
                .is_none()
        })
        .count();
    let unanswered = entries
        .iter()
        .filter(|entry| {
            entry.may_have_paid()
                && !late_payments
                    .iter()
                    .any(|late| Some(&late.payment_hash) == entry.payment_hash.as_ref())
        })
        .count();
    unreadable + unanswered
}

fn competition_of(run: &TestRun) -> Option<Uuid> {
    run.competition_id.as_deref()?.parse().ok()
}

/// What changes when a competition moves on in a way that matters to its money.
fn fingerprint(competition: &CompetitionResponse) -> String {
    format!(
        "{}|{}|{}|{:?}|{}",
        competition
            .state
            .as_deref()
            .unwrap_or(competition.inferred_status()),
        competition.total_paid_entries,
        competition.total_paid_out_entries,
        competition.funding_outpoint,
        competition.attestation.is_some(),
    )
}

fn parse_time(at: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()
}

fn followed_long_enough(run: &TestRun, config: &TrailConfig) -> bool {
    parse_time(&run.started_at).is_some_and(|started| {
        OffsetDateTime::now_utc() - started
            >= time::Duration::seconds(config.follow_timeout_secs as i64)
    })
}

/// Open the nodes a tracker can look payments up on: the payer, and the node entries are paid to.
pub fn nodes(configs: &[&LndConfig]) -> Vec<Lnd> {
    configs
        .iter()
        .filter_map(|config| {
            Lnd::new(config)
                .context("open a node to trace payments on")
                .map_err(|e| warn!("{e:#}"))
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, response::IntoResponse, routing::get, Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        tracker: Tracker,
        server: tokio::task::JoinHandle<()>,
        _data: tempfile::TempDir,
    }

    /// One server answers for the coordinator, the payer's node, the explorer, and ark-swapd.
    async fn fixture(router: Router, with_payer: bool, with_ark_swap: bool) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let data = tempfile::tempdir().unwrap();
        let db = SynthDb::new(data.path().join("synth.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let payer = with_payer.then(|| {
            let macaroon = data.path().join("test.macaroon");
            std::fs::write(&macaroon, [0u8]).unwrap();
            Lnd::new(&LndConfig {
                rest_url: url.clone(),
                macaroon_file: macaroon,
                tls_cert_file: None,
                fee_limit_sats: 100,
                payment_timeout_secs: 1,
            })
            .unwrap()
        });
        let ark_swap = with_ark_swap.then(|| {
            let token = data.path().join("token");
            std::fs::write(&token, "secret").unwrap();
            ArkSwap::new(&crate::ark_swap::ArkSwapConfig {
                url: url.clone(),
                token_file: token,
            })
            .unwrap()
        });
        Fixture {
            tracker: Tracker::new(
                CoordinatorClient::new(&url, None),
                db,
                Events::new(),
                payer,
                Vec::new(),
                None,
                ark_swap,
                TrailConfig {
                    follow_timeout_secs: 0,
                    explorer_url: url.clone(),
                    ..TrailConfig::default()
                },
            )
            .unwrap(),
            server,
            _data: data,
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl Fixture {
        fn db(&self) -> &SynthDb {
            &self.tracker.inner.db
        }

        fn follow_for(&mut self, secs: u64) {
            Arc::get_mut(&mut self.tracker.inner)
                .unwrap()
                .config
                .follow_timeout_secs = secs;
        }

        /// A finished run that made a competition and entered `entries`.
        async fn run(&self, competition_id: Uuid, entries: &[serde_json::Value]) -> TestRun {
            let db = self.db();
            let run_id = db.create_run("full_lifecycle", None).await.unwrap();
            let details = serde_json::json!({ "competition_id": competition_id }).to_string();
            db.add_step(&run_id, "create_competition", 1, None, Some(&details))
                .await
                .unwrap();
            for entry in entries {
                db.add_step(
                    &run_id,
                    &format!("user_{}_enter", entry["user"].as_str().unwrap()),
                    1,
                    None,
                    Some(&entry.to_string()),
                )
                .await
                .unwrap();
            }
            db.complete_run(&run_id, None).await.unwrap();
            db.get_run(&run_id).await.unwrap().unwrap()
        }
    }

    fn competition(id: Uuid, json: serde_json::Value) -> CompetitionResponse {
        let mut base = serde_json::json!({
            "id": id,
            "created_at": "2026-09-24T01:00:00Z",
            "event_submission": {},
        });
        base.as_object_mut()
            .unwrap()
            .extend(json.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[tokio::test]
    async fn an_unavailable_competition_is_unverified_without_a_second_fetch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let fixture = fixture(
            Router::new().fallback(move || {
                let calls = counted.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }),
            false,
            false,
        )
        .await;
        let alice = serde_json::json!({
            "user": "alice", "nostr_pubkey": "00", "paid": true, "amount_sats": 1000
        });
        let run = fixture.run(Uuid::now_v7(), &[alice]).await;

        fixture.tracker.tick().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one coordinator lookup");
        let trail = fixture.db().get_trail(&run.id).await.unwrap().unwrap();
        assert!(matches!(trail.money, Money::Unverified { .. }));
        assert!(trail.gaps.iter().any(|gap| gap.contains("503")));
        assert_eq!(
            fixture.db().get_run(&run.id).await.unwrap().unwrap().status,
            "passed",
            "money synth could not verify does not fail the run"
        );
        assert!(fixture.db().runs_to_follow().await.unwrap().is_empty());
        fixture.tracker.tick().await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the unverified run left the queue"
        );
        assert!(
            fixture.tracker.inner.seen.lock().unwrap().is_empty(),
            "runs no longer followed are forgotten"
        );
    }

    #[tokio::test]
    async fn incomplete_entry_evidence_is_followed_until_synth_gives_up() {
        let payment = serde_json::json!({
            "user": "alice", "nostr_pubkey": "00", "paid": false,
            "ticket_id": Uuid::now_v7(), "payment_hash": "00".repeat(32)
        })
        .to_string();
        for details in [
            None,
            Some("{bad json"),
            Some("{}"),
            Some(r#"{"user":"alice","nostr_pubkey":"00"}"#),
            // No payer node is configured to say whether this one was paid.
            Some(payment.as_str()),
        ] {
            let mut fixture = fixture(Router::new(), false, false).await;
            fixture.follow_for(3600);
            let db = fixture.db().clone();
            let competition_id = Uuid::now_v7();
            let run = fixture.run(competition_id, &[]).await;
            db.add_step(&run.id, "user_alice_enter", 1, None, details)
                .await
                .unwrap();
            let competition = competition(competition_id, serde_json::json!({}));

            let trail = fixture
                .tracker
                .refresh_run(&run, competition_id, Ok(competition.clone()))
                .await
                .unwrap();
            assert!(matches!(trail.money, Money::Following), "{details:?}");
            assert!(trail
                .gaps
                .iter()
                .any(|gap| gap.contains("could not be verified")));

            fixture.follow_for(0);
            let trail = fixture
                .tracker
                .refresh_run(&run, competition_id, Ok(competition))
                .await
                .unwrap();
            assert!(
                matches!(trail.money, Money::Unverified { .. }),
                "{details:?}"
            );
        }
    }

    fn track_route(hash: &str, response: serde_json::Value) -> Router {
        let hash = hash.to_string();
        Router::new()
            .route(
                "/v2/router/track/{hash}",
                get(
                    move |axum::extract::Path(asked): axum::extract::Path<String>| {
                        let response = response.clone();
                        let hash = hash.clone();
                        async move {
                            use base64::{engine::general_purpose::URL_SAFE, Engine};
                            if URL_SAFE.decode(&asked).map(hex::encode).ok() == Some(hash) {
                                Json(response).into_response()
                            } else {
                                StatusCode::NOT_FOUND.into_response()
                            }
                        }
                    },
                ),
            )
            .route(
                "/v1/getinfo",
                get(|| async {
                    Json(serde_json::json!({"identity_pubkey": "thor", "alias": "thor"}))
                }),
            )
    }

    /// A restart mid-payment leaves the entry's step with the ticket and hash but not the
    /// payment. The payer says whether it went out: here it did, so the entry counts as paid.
    #[tokio::test]
    async fn an_entry_paid_while_synth_restarted_is_found_on_the_payer() {
        let hash = "cd".repeat(32);
        let paid = serde_json::json!({"result": {
            "payment_hash": hash, "payment_preimage": "ef".repeat(32),
            "status": "SUCCEEDED", "value_sat": "1100", "fee_msat": "20"
        }});
        let fixture = fixture(track_route(&hash, paid), true, false).await;
        let competition_id = Uuid::now_v7();
        let interrupted = serde_json::json!({
            "user": "alice", "nostr_pubkey": "00", "paid": false, "amount_sats": 1100,
            "ticket_id": Uuid::now_v7(), "payment_hash": hash
        });
        let run = fixture.run(competition_id, &[interrupted]).await;
        let created = competition(competition_id, serde_json::json!({}));
        let trail = fixture
            .tracker
            .refresh_run(&run, competition_id, Ok(created))
            .await
            .unwrap();
        assert_eq!(trail.late_payments.len(), 1);
        let entries = entries_of(&fixture.db().get_steps(&run.id).await.unwrap());
        assert_eq!(trail.paid_entries(&entries).len(), 1);
        assert_eq!(
            trail.late_payment(&entries[0]).unwrap().fee_msat,
            20,
            "the payer's record of the payment"
        );
        assert!(
            !matches!(trail.money, Money::NothingPaid),
            "{:?}",
            trail.money
        );
    }

    /// The payer never made the payment: nothing went out, and that is a definite answer.
    #[tokio::test]
    async fn an_entry_the_payer_never_paid_moved_no_money() {
        let hash = "cd".repeat(32);
        let never = serde_json::json!({"error": {"code": 5, "message": "payment isn't initiated"}});
        let fixture = fixture(track_route(&hash, never), true, false).await;
        let competition_id = Uuid::now_v7();
        let interrupted = serde_json::json!({
            "user": "alice", "nostr_pubkey": "00", "paid": false, "amount_sats": 1100,
            "ticket_id": Uuid::now_v7(), "payment_hash": hash
        });
        let run = fixture.run(competition_id, &[interrupted]).await;
        let trail = fixture
            .tracker
            .refresh_run(
                &run,
                competition_id,
                Ok(competition(competition_id, serde_json::json!({}))),
            )
            .await
            .unwrap();
        assert_eq!(trail.money, Money::NothingPaid);
    }

    /// Stuck money on a run synth followed fails the run, in the same write as the trail and the
    /// step saying so. A run first traced after it finished keeps its status.
    #[tokio::test]
    async fn stuck_money_fails_a_followed_run_but_not_a_backfilled_one() {
        let fixture = fixture(Router::new(), false, false).await;
        let competition_id = Uuid::now_v7();
        let alice = serde_json::json!({
            "user": "alice", "nostr_pubkey": "00", "paid": true, "amount_sats": 1100,
            "ticket_id": Uuid::now_v7(), "payment_hash": "ab".repeat(32)
        });
        let an_hour_ago = (OffsetDateTime::now_utc() - time::Duration::hours(1))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let cancelled = competition(
            competition_id,
            serde_json::json!({ "cancelled_at": an_hour_ago }),
        );

        let backfilled = fixture
            .run(competition_id, std::slice::from_ref(&alice))
            .await;
        let trail = fixture
            .tracker
            .refresh_run(&backfilled, competition_id, Ok(cancelled.clone()))
            .await
            .unwrap();
        assert!(matches!(trail.money, Money::Stuck { .. }));
        let run = fixture.db().get_run(&backfilled.id).await.unwrap().unwrap();
        assert_eq!(run.status, "passed", "history is not rewritten");
        assert_eq!(trail.held.as_ref().unwrap().sats, 1100);

        let followed = fixture.run(competition_id, &[alice]).await;
        let mut running = followed.clone();
        running.status = "running".into();
        fixture
            .tracker
            .refresh_run(&running, competition_id, Ok(cancelled.clone()))
            .await
            .unwrap();
        fixture
            .tracker
            .refresh_run(&followed, competition_id, Ok(cancelled))
            .await
            .unwrap();
        let run = fixture.db().get_run(&followed.id).await.unwrap().unwrap();
        assert_eq!(run.status, "failed");
        assert!(run.error_message.unwrap().contains("before its contract"));
        let steps = fixture.db().get_steps(&followed.id).await.unwrap();
        assert_eq!(steps.last().unwrap().step_name, "money_stuck");
        assert_eq!(
            fixture.db().runs_to_follow().await.unwrap().len(),
            2,
            "stuck money is watched until it moves"
        );
    }

    /// Once stuck money moves, the trail says where it was held and how it left.
    #[test]
    fn money_that_moves_keeps_where_it_was_held() {
        let since = time::macros::datetime!(2026-09-24 03:00:00 UTC);
        let later = time::macros::datetime!(2026-09-24 09:00:00 UTC);
        let mut trail = Trail {
            refreshed_at: later,
            competition_id: Uuid::now_v7(),
            competition: None,
            settlement: None,
            swaps: Vec::new(),
            late_payments: Vec::new(),
            payouts: Vec::new(),
            refunds: vec![RefundSeen {
                user: "alice".into(),
                ticket_id: Uuid::now_v7(),
                state: "settled".into(),
                paid_sats: 1080,
                ark_txid: Some("ff".into()),
                invoice: None,
                payment_hash: None,
                preimage: None,
                fee_msat: None,
                paid_by: None,
            }],
            funding_tx: None,
            outcome_tx: None,
            closing_txs: Vec::new(),
            money: Money::Refunded,
            held: None,
            gaps: Vec::new(),
        };
        let earlier = Held {
            since,
            found: since,
            reason: "not refunded".into(),
            sats: 1090,
            nearest_expiry: None,
            until: None,
            then: None,
        };
        let moved = held(&trail, &[], Some(earlier.clone()), later).unwrap();
        assert_eq!(moved.since, since);
        assert_eq!(moved.until, Some(later));
        assert_eq!(
            moved.then.as_deref(),
            Some("refunded by Arkade transaction ff")
        );
        trail.held = Some(moved.clone());
        assert_eq!(
            held(&trail, &[], Some(moved.clone()), later),
            Some(moved),
            "the record stays once the money moved"
        );
    }

    /// An explorer that cannot answer now does not undo what it said before.
    #[tokio::test]
    async fn an_explorer_error_keeps_a_confirmed_transaction_confirmed() {
        let fixture = fixture(Router::new(), false, false).await;
        let known = ChainTx {
            txid: "aa".into(),
            vout: Some(0),
            fee_sat: None,
            value_sat: Some(3000),
            confirmed: true,
        };
        let mut gaps = Vec::new();
        let tx = fixture
            .tracker
            .chain_tx("aa".into(), Some(0), Some(&known), &mut gaps)
            .await;
        assert_eq!(tx, known);
        assert_eq!(gaps.len(), 1);
    }

    /// ark-swapd failing for one entry does not drop the others' swaps.
    #[tokio::test]
    async fn an_ark_swapd_error_for_one_entry_keeps_the_rest() {
        let bob_hash = "bb".repeat(32);
        let found = serde_json::json!({
            "id": Uuid::now_v7(), "escrow_address": "tark1bob", "amount_sat": 1090,
            "payment_hash": bob_hash, "state": "settled", "escrow_vtxo": "ee:0"
        });
        let router = Router::new().route(
            "/v1/swaps",
            get(
                move |axum::extract::Query(query): axum::extract::Query<
                    HashMap<String, String>,
                >| {
                    let found = found.clone();
                    async move {
                        if query.get("payment_hash").map(String::as_str)
                            == found["payment_hash"].as_str()
                        {
                            Json(found).into_response()
                        } else {
                            StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        }
                    }
                },
            ),
        );
        let fixture = fixture(router, false, true).await;
        let entry = |user: &str, hash: &str| EntryTrace {
            user: user.into(),
            payment_hash: Some(hash.into()),
            paid: true,
            ..EntryTrace::default()
        };
        let entries = [entry("alice", &"aa".repeat(32)), entry("bob", &bob_hash)];
        let mut gaps = Vec::new();
        let swaps = fixture
            .tracker
            .swaps(&entries, None, false, &mut gaps)
            .await;
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].escrow_address, "tark1bob");
        assert!(gaps.iter().any(|gap| gap.contains("alice")));
    }

    #[tokio::test]
    async fn payout_confirmation_uses_amount_observed_by_either_lightning_node() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use sha2::{Digest, Sha256};

        let preimage = [42u8; 32];
        let hash = hex::encode(Sha256::digest(preimage));
        for payer_visible in [true, false] {
            let paid = serde_json::json!({"result": {
                "payment_hash": hash, "payment_preimage": hex::encode(preimage),
                "status": "SUCCEEDED", "value_sat": "900", "fee_msat": "20"
            }});
            let invoice = serde_json::json!({
                "state": "SETTLED", "r_preimage": STANDARD.encode(preimage), "amt_paid_sat": "900"
            });
            let router = Router::new()
                .route(
                    "/v2/router/track/{hash}",
                    get(move || {
                        let paid = paid.clone();
                        async move {
                            if payer_visible {
                                Json(paid).into_response()
                            } else {
                                StatusCode::NOT_FOUND.into_response()
                            }
                        }
                    }),
                )
                .route(
                    "/v1/getinfo",
                    get(|| async {
                        Json(serde_json::json!({"identity_pubkey": "payee", "alias": "test node"}))
                    }),
                )
                .route(
                    "/v1/invoice/{hash}",
                    get(move || async move { Json(invoice) }),
                );
            let fixture = fixture(router, true, false).await;

            // The invoice asks for 1000, but the observed payment delivered only 900.
            let mut short = PayoutSeen {
                owed_sats: 1000,
                amount_sats: Some(1000),
                payment_hash: Some(hash.clone()),
                payee_pubkey: Some("payee".into()),
                ..PayoutSeen::default()
            };
            fixture.tracker.look_up_payout(&mut short).await;
            assert_eq!(short.amount_sats, Some(900));
            assert!(
                !short.is_confirmed(),
                "an invoice amount cannot hide a short payment"
            );

            // An amountless invoice can still be proven paid from the node's receipt.
            let mut amountless = PayoutSeen {
                owed_sats: 900,
                amount_sats: None,
                preimage: None,
                ..short
            };
            fixture.tracker.look_up_payout(&mut amountless).await;
            assert_eq!(amountless.amount_sats, Some(900));
            assert!(amountless.is_confirmed());
        }
    }

    /// With a payee configured, a payout whose invoice pays another node is caught.
    #[tokio::test]
    async fn a_payout_to_a_node_other_than_the_payee_is_caught() {
        let mut fixture = fixture(Router::new(), false, false).await;
        Arc::get_mut(&mut fixture.tracker.inner)
            .unwrap()
            .payee_pubkey = Some("freya".into());
        let payout = |payee: &str| PayoutSeen {
            owed_sats: 1000,
            payee_pubkey: Some(payee.into()),
            ..PayoutSeen::default()
        };
        assert_eq!(
            fixture.tracker.payee_check_of(&payout("freya")).await,
            PayeeCheck::Matches
        );
        assert_eq!(
            fixture.tracker.payee_check_of(&payout("mallory")).await,
            PayeeCheck::OtherNode
        );
    }

    fn step(name: &str, details: serde_json::Value) -> TestStep {
        TestStep {
            id: Uuid::now_v7().to_string(),
            run_id: "r".into(),
            step_name: name.into(),
            status: "passed".into(),
            started_at: String::new(),
            completed_at: None,
            duration_ms: Some(1),
            details_json: Some(details.to_string()),
            error_message: None,
        }
    }

    /// Only entry steps carry entries; a refund step's details must not read as one.
    #[test]
    fn entries_are_read_from_the_entry_steps() {
        let steps = [
            step(
                "create_competition",
                serde_json::json!({ "competition_id": Uuid::now_v7() }),
            ),
            step(
                "user_alice_enter",
                serde_json::json!({ "user": "alice", "nostr_pubkey": "00", "paid": true }),
            ),
            step(
                "refund_alice",
                serde_json::json!({ "paid_sats": 1000, "ark_txid": null }),
            ),
        ];
        let entries = entries_of(&steps);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].user, "alice");
    }

    #[test]
    fn a_run_is_followed_until_its_timeout_after_it_started() {
        let config = TrailConfig::default();
        let run = |started: OffsetDateTime| TestRun {
            id: "r".into(),
            scenario: "full_lifecycle".into(),
            status: "passed".into(),
            started_at: started
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            completed_at: None,
            error_message: None,
            config_json: None,
            competition_id: None,
            money: None,
        };
        let now = OffsetDateTime::now_utc();
        assert!(!followed_long_enough(
            &run(now - time::Duration::hours(3)),
            &config
        ));
        assert!(followed_long_enough(
            &run(now - time::Duration::hours(9)),
            &config
        ));
    }
}
