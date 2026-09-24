//! Following each run's money until it settles, and saving what was found as the run's trail.
//!
//! Every few seconds the tracker looks at the competition of each recent run whose money has not
//! settled. When the competition moved on, it looks everything up again: the competition, each
//! player's entry and payout, ark-swapd's swaps, the funding and outcome transactions, and the
//! payouts on the Lightning nodes synth can reach. A run whose money settles gets a last step,
//! `money_settled`, and a run that passed but whose money got stuck is failed with the reason.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{info, warn};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::{judge, ChainTx, EntryTrace, Money, PayoutSeen, RefundSeen, SwapSeen, Trail};
use crate::ark_swap::ArkSwap;
use crate::client::competitions::CompetitionResponse;
use crate::client::CoordinatorClient;
use crate::crypto::keys::SynthUser;
use crate::db::{SynthDb, TestRun, TestStep};
use crate::events::{Event, Events};
use crate::lnd::{Lnd, NodeIdentity};
use crate::settlement::Settlement;

/// How synth follows runs' money, and where it points people to look it up.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TrailConfig {
    /// How often the unsettled runs' competitions are looked at.
    pub interval_secs: u64,
    /// How long after a run starts synth stops following its money. A lifecycle competition
    /// pays out about three hours after its run ends, once its outcome has 144 blocks on it.
    pub follow_timeout_secs: u64,
    /// A mempool.space-style explorer for the chain the contracts settle on, for links and fees.
    pub explorer_url: String,
    /// The oracle's public site, for links to each competition's event.
    pub oracle_url: String,
}

impl Default for TrailConfig {
    fn default() -> Self {
        Self {
            interval_secs: 20,
            follow_timeout_secs: 8 * 3600,
            explorer_url: "https://mutinynet.com".to_string(),
            oracle_url: "https://4casttruth.win".to_string(),
        }
    }
}

/// How long a run's trail may go without a full look, even if its competition seems not to move:
/// payouts show up on the nodes and in ark-swapd without the competition's state changing.
const REFRESH_AT_LEAST_EVERY: Duration = Duration::from_secs(5 * 60);

/// A Lightning node synth can reach, by the name it gives itself.
struct Node {
    lnd: Lnd,
    identity: OnceCell<NodeIdentity>,
}

impl Node {
    async fn identity(&self) -> Option<&NodeIdentity> {
        self.identity
            .get_or_try_init(|| self.lnd.identity())
            .await
            .ok()
    }

    async fn name(&self) -> String {
        match self.identity().await {
            Some(identity) if !identity.alias.is_empty() => identity.alias.clone(),
            Some(identity) => identity.pubkey.chars().take(12).collect(),
            None => "an unreachable node".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct Tracker {
    inner: Arc<Inner>,
}

struct Inner {
    client: CoordinatorClient,
    db: SynthDb,
    events: Events,
    nodes: Vec<Node>,
    ark_swap: Option<ArkSwap>,
    explorer: reqwest::Client,
    config: TrailConfig,
    /// The Lightning Address payouts go to, as the scenarios register it.
    lightning_address: Option<String>,
    /// Runs being refreshed now, so a page asking twice does not look everything up twice.
    busy: Mutex<HashSet<String>>,
    /// Each followed run's competition as last looked at, and when its trail was last refreshed.
    seen: Mutex<HashMap<String, (String, Instant)>>,
}

impl Tracker {
    pub fn new(
        client: CoordinatorClient,
        db: SynthDb,
        events: Events,
        nodes: Vec<Lnd>,
        ark_swap: Option<ArkSwap>,
        config: TrailConfig,
        lightning_address: Option<String>,
    ) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                db,
                events,
                nodes: nodes
                    .into_iter()
                    .map(|lnd| Node {
                        lnd,
                        identity: OnceCell::new(),
                    })
                    .collect(),
                ark_swap,
                explorer: reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()?,
                config,
                lightning_address,
                busy: Mutex::default(),
                seen: Mutex::default(),
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

    /// Follow the recent runs' money for as long as synth runs.
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
        for run in self.inner.db.runs_to_follow().await? {
            let Some(competition_id) = competition_of(&run) else {
                continue;
            };
            if let Err(e) = self.follow(&run, competition_id).await {
                warn!("Cannot follow run {}'s money: {e:#}", run.id);
            }
        }
        Ok(())
    }

    /// Look at a run's competition, and refresh the run's trail if it moved or is due.
    async fn follow(&self, run: &TestRun, competition_id: Uuid) -> Result<()> {
        let competition = self.inner.client.get_competition(&competition_id).await?;
        let fingerprint = format!("{}|{}", run.status, fingerprint(&competition));
        let due = {
            let seen = self.inner.seen.lock().expect("seen lock");
            match seen.get(&run.id) {
                Some((last, at)) => *last != fingerprint || at.elapsed() >= REFRESH_AT_LEAST_EVERY,
                None => true,
            }
        };
        if !due || !self.start(&run.id) {
            return Ok(());
        }
        let refreshed = self
            .refresh_run(run, competition_id, Some(competition))
            .await;
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
        self.refresh_run(&run, competition_id, None).await.map(Some)
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
        competition: Option<CompetitionResponse>,
    ) -> Result<Trail> {
        let steps = self.inner.db.get_steps(&run.id).await?;
        let entries = entries_of(&steps);
        let previous = self.inner.db.get_trail(&run.id).await?;
        let mut gaps = Vec::new();

        let competition = match competition {
            Some(competition) => Some(competition),
            None => match self.inner.client.get_competition(&competition_id).await {
                Ok(competition) => Some(competition),
                Err(e) => {
                    gaps.push(format!(
                        "the coordinator did not answer for the competition: {e:#}"
                    ));
                    None
                }
            },
        };
        let settlement = competition
            .as_ref()
            .and_then(Settlement::of)
            .or_else(|| previous.as_ref()?.settlement.clone());
        let swaps = self.swaps(&entries, previous.as_ref(), &mut gaps).await;
        let funding_tx = match competition.as_ref().and_then(CompetitionResponse::funding) {
            Some((txid, vout)) => Some(
                self.chain_tx(
                    txid,
                    vout,
                    previous.as_ref().and_then(|p| p.funding_tx.as_ref()),
                    &mut gaps,
                )
                .await,
            ),
            None => previous.as_ref().and_then(|p| p.funding_tx.clone()),
        };
        let outcome_tx = match competition
            .as_ref()
            .and_then(CompetitionResponse::outcome_txid)
        {
            Some(txid) => Some(
                self.chain_tx(
                    txid,
                    None,
                    previous.as_ref().and_then(|p| p.outcome_tx.as_ref()),
                    &mut gaps,
                )
                .await,
            ),
            None => previous.as_ref().and_then(|p| p.outcome_tx.clone()),
        };
        let payouts = self
            .payouts(
                competition_id,
                &entries,
                settlement.as_ref(),
                previous.as_ref(),
                &mut gaps,
            )
            .await;
        let ended = competition
            .as_ref()
            .is_some_and(|c| c.cancelled_at.is_some() || c.failed_at.is_some());
        let refunds = if ended {
            self.refunds(competition_id, &entries, previous.as_ref(), &mut gaps)
                .await
        } else {
            Vec::new()
        };

        let paid = entries.iter().filter(|entry| entry.paid).count();
        let give_up = run.status != "running" && followed_long_enough(run, &self.inner.config);
        let money = match &competition {
            // A run still going decides nothing yet, even before anyone has paid.
            _ if run.status == "running" => Money::Following,
            Some(competition) => judge(competition, &payouts, &refunds, paid, give_up),
            None if give_up => Money::TimedOut {
                reason: "the coordinator never answered for the competition".to_string(),
            },
            None => Money::Following,
        };
        let trail = Trail {
            refreshed_at: OffsetDateTime::now_utc(),
            competition_id,
            competition: competition
                .map(CompetitionResponse::slim)
                .or_else(|| previous.as_ref()?.competition.clone()),
            settlement,
            swaps,
            payouts,
            refunds,
            funding_tx,
            outcome_tx,
            money,
            gaps,
        };
        self.inner.db.save_trail(&run.id, &trail).await?;
        let was_settled = previous.is_some_and(|p| p.money.is_settled());
        if trail.money.is_settled() && !was_settled {
            self.record_settled(run, &trail.money).await?;
        }
        self.inner.events.send(Event::TrailUpdated {
            run_id: run.id.clone(),
        });
        Ok(trail)
    }

    /// Give a run whose money settled its last step, and fail it if it passed but the money did
    /// not end where it should.
    async fn record_settled(&self, run: &TestRun, money: &Money) -> Result<()> {
        let since = run
            .completed_at
            .as_deref()
            .and_then(parse_time)
            .map(|at| (OffsetDateTime::now_utc() - at).whole_milliseconds().max(0) as i64)
            .unwrap_or(0);
        let error = money.reason().filter(|_| !money.is_good());
        self.inner
            .db
            .add_step(
                &run.id,
                "money_settled",
                since,
                error,
                Some(&serde_json::to_string(money)?),
            )
            .await?;
        if let Some(reason) = error {
            info!("Run {}'s money is {}: {reason}", run.id, money.label());
            self.inner
                .db
                .fail_passed_run(&run.id, &format!("Its steps passed, but {reason}"))
                .await?;
        }
        self.inner.events.send(Event::StepFinished {
            run_id: run.id.clone(),
            step: "money_settled".to_string(),
            passed: error.is_none(),
        });
        Ok(())
    }

    /// Each paid entry's swap into its escrow, from ark-swapd. A swap ark-swapd is done with is
    /// not looked up again.
    async fn swaps(
        &self,
        entries: &[EntryTrace],
        previous: Option<&Trail>,
        gaps: &mut Vec<String>,
    ) -> Vec<SwapSeen> {
        let earlier = |hash: &str| {
            previous?
                .swaps
                .iter()
                .find(|swap| swap.payment_hash == hash)
                .cloned()
        };
        let Some(ark_swap) = &self.inner.ark_swap else {
            if entries.iter().any(|entry| entry.paid) {
                gaps.push(
                    "ark-swapd is not configured, so the escrow swaps are not looked up".into(),
                );
            }
            return previous.map(|p| p.swaps.clone()).unwrap_or_default();
        };
        let mut swaps = Vec::new();
        for entry in entries.iter().filter(|entry| entry.paid) {
            let Some(hash) = &entry.payment_hash else {
                continue;
            };
            if let Some(swap) = earlier(hash).filter(SwapSeen::is_final) {
                swaps.push(swap);
                continue;
            }
            match ark_swap.swap_for(hash).await {
                Ok(Some(swap)) => swaps.push(SwapSeen {
                    user: entry.user.clone(),
                    payment_hash: swap.payment_hash,
                    id: swap.id,
                    state: swap.state,
                    amount_sat: swap.amount_sat,
                    escrow_address: swap.escrow_address,
                    escrow_vtxo: swap.escrow_vtxo,
                    ark_txid: swap.ark_txid,
                    error: swap.error,
                }),
                Ok(None) => gaps.push(format!(
                    "ark-swapd has no swap for {}'s payment",
                    entry.user
                )),
                Err(e) => {
                    gaps.push(format!("ark-swapd's swaps: {e:#}"));
                    swaps.extend(earlier(hash));
                    break;
                }
            }
        }
        swaps
    }

    /// A transaction as the explorer sees it. One already confirmed with its fee known is not
    /// looked up again.
    async fn chain_tx(
        &self,
        txid: String,
        vout: Option<u32>,
        previous: Option<&ChainTx>,
        gaps: &mut Vec<String>,
    ) -> ChainTx {
        if let Some(known) =
            previous.filter(|p| p.txid == txid && p.confirmed && p.fee_sat.is_some())
        {
            return known.clone();
        }
        #[derive(Deserialize)]
        struct Found {
            fee: Option<u64>,
            status: Status,
        }
        #[derive(Deserialize)]
        struct Status {
            confirmed: bool,
        }
        let url = format!(
            "{}/api/tx/{txid}",
            self.inner.config.explorer_url.trim_end_matches('/')
        );
        let found = async {
            let response = self.inner.explorer.get(&url).send().await?;
            anyhow::ensure!(response.status().is_success(), "{}", response.status());
            Ok(response.json::<Found>().await?)
        }
        .await;
        match found {
            Ok(found) => ChainTx {
                txid,
                vout,
                fee_sat: found.fee,
                confirmed: found.status.confirmed,
            },
            Err(e) => {
                gaps.push(format!("the explorer did not find {txid}: {e:#}"));
                ChainTx {
                    txid,
                    vout,
                    fee_sat: None,
                    confirmed: false,
                }
            }
        }
    }

    /// Each entered player's payout: their share, and what the coordinator and the reachable
    /// nodes say about paying it.
    async fn payouts(
        &self,
        competition_id: Uuid,
        entries: &[EntryTrace],
        settlement: Option<&Settlement>,
        previous: Option<&Trail>,
        gaps: &mut Vec<String>,
    ) -> Vec<PayoutSeen> {
        let users = self.inner.db.list_users().await.unwrap_or_default();
        let mut payouts = Vec::new();
        for entry in entries.iter().filter(|entry| entry.entry_submitted) {
            let Some(entry_id) = entry.entry_id else {
                continue;
            };
            let Some(user) = users
                .iter()
                .find(|user| user.name == entry.user)
                .and_then(|user| {
                    SynthUser::from_secret_key(&user.name, &user.nostr_secret_key).ok()
                })
            else {
                continue;
            };
            let listed = match self
                .inner
                .client
                .list_entries(&user.nostr_keys, Some(&competition_id))
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
                payee_pubkey: invoice.as_ref().map(|i| {
                    i.payee_pub_key()
                        .copied()
                        .unwrap_or_else(|| i.recover_payee_pub_key())
                        .to_string()
                }),
                lightning_address: self.inner.lightning_address.clone(),
                ..PayoutSeen::default()
            };
            let earlier = previous.and_then(|p| {
                p.payouts
                    .iter()
                    .find(|p| p.payment_hash.is_some() && p.payment_hash == payout.payment_hash)
            });
            match earlier {
                Some(earlier) if earlier.is_confirmed() => {
                    payout.preimage = earlier.preimage.clone();
                    payout.fee_msat = earlier.fee_msat;
                    payout.paid_by = earlier.paid_by.clone();
                    payout.payee_alias = earlier.payee_alias.clone();
                }
                _ => {
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

    /// Find a payout on the nodes synth can reach: the preimage and fee from the node that paid
    /// it, or the preimage from the node it paid.
    async fn look_up_payout(&self, payout: &mut PayoutSeen) {
        let Some(hash) = payout.payment_hash.clone() else {
            return;
        };
        for node in &self.inner.nodes {
            if let Ok(Some(paid)) = node.lnd.payment(&hash).await {
                payout.preimage = Some(paid.preimage);
                payout.fee_msat = Some(paid.fee_msat);
                payout.paid_by = Some(node.name().await);
                return;
            }
        }
        for node in &self.inner.nodes {
            let issued = node
                .identity()
                .await
                .is_some_and(|identity| Some(&identity.pubkey) == payout.payee_pubkey.as_ref());
            if !issued {
                continue;
            }
            if let Ok(Some(invoice)) = node.lnd.lookup_invoice(&hash).await {
                if invoice.state == "SETTLED" {
                    payout.preimage = Some(invoice.preimage);
                    payout.payee_alias = Some(node.name().await);
                }
            }
        }
    }

    async fn alias_of(&self, pubkey: Option<&str>) -> Option<String> {
        let pubkey = pubkey?;
        for node in &self.inner.nodes {
            if node
                .identity()
                .await
                .is_some_and(|identity| identity.pubkey == pubkey)
            {
                return Some(node.name().await);
            }
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

    /// Each paid ticket's refund, once its competition was cancelled. A settled one is not
    /// looked up again.
    async fn refunds(
        &self,
        competition_id: Uuid,
        entries: &[EntryTrace],
        previous: Option<&Trail>,
        gaps: &mut Vec<String>,
    ) -> Vec<RefundSeen> {
        let users = self.inner.db.list_users().await.unwrap_or_default();
        let mut refunds = Vec::new();
        for entry in entries.iter().filter(|entry| entry.paid) {
            let Some(ticket_id) = entry.ticket_id else {
                continue;
            };
            let earlier =
                previous.and_then(|p| p.refunds.iter().find(|r| r.ticket_id == ticket_id));
            if let Some(settled) = earlier.filter(|r| r.state == "settled") {
                refunds.push(settled.clone());
                continue;
            }
            let Some(user) = users
                .iter()
                .find(|user| user.name == entry.user)
                .and_then(|user| {
                    SynthUser::from_secret_key(&user.name, &user.nostr_secret_key).ok()
                })
            else {
                continue;
            };
            match self
                .inner
                .client
                .check_ticket_refund(&user.nostr_keys, &competition_id, &ticket_id)
                .await
            {
                Ok(Some(refund)) => refunds.push(RefundSeen {
                    user: entry.user.clone(),
                    ticket_id,
                    state: refund.state,
                    paid_sats: refund.paid_sats,
                    ark_txid: refund.ark_txid,
                }),
                Ok(None) => {}
                Err(e) => {
                    gaps.push(format!("{}'s refund: {e:#}", entry.user));
                    refunds.extend(earlier.cloned());
                }
            }
        }
        refunds
    }
}

/// The entries a run's steps recorded.
pub fn entries_of(steps: &[TestStep]) -> Vec<EntryTrace> {
    steps
        .iter()
        .filter(|step| step.step_name.ends_with("_enter"))
        .filter_map(|step| step.details_json.as_deref())
        .filter_map(|details| serde_json::from_str::<EntryTrace>(details).ok())
        .collect()
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

/// Open the nodes a tracker can look payouts up on: the payer, and the node entries are paid to.
pub fn nodes(configs: &[&crate::lnd::LndConfig]) -> Vec<Lnd> {
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
