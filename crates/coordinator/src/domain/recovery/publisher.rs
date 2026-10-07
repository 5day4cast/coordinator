//! Keeps the recovery records current, gets them to the relays, and retires them once their
//! money is settled.
//!
//! Every few seconds the publisher rebuilds the records of the competitions that changed since
//! its last look (the list triggers note every change to a competition's entries, tickets and
//! payouts), and of a few competitions and wallets from a full pass that starts at launch and
//! repeats every half hour. The full pass is also the backfill of records that existed before
//! this ran, and it looks again at completed competitions that still have records on the relays,
//! whose money may have settled since. A record whose plaintext did not change is skipped; a
//! changed one replaces its outbox row, and the outbox is published with retries.
//!
//! A record whose money is settled is retired after a grace period: a NIP-09 deletion is
//! published for it and it is not published again, unless its money moves again. See
//! docs/RECOVERY.md, "Retention".
//!
//! Nothing here runs in the entry, kickoff, payout or refund paths, and no error stops the
//! publisher: a relay or database failure is logged and tried again on a later tick.

use super::{content_digest, next_created_at, publish_to_relay, ticket_entry_id, Recovery};
use crate::{
    domain::{
        CompetitionStore, RecoveryAttempt, RecoveryDeletion, RecoveryOutboxEvent,
        RecoveryOutboxState, UserInfo, WorkerLeases,
    },
    infra::oracle::Oracle,
    metrics::{
        RECOVERY_DELETIONS, RECOVERY_OUTBOX_DEPTH, RECOVERY_RECORDS_LIVE, RECOVERY_RECORDS_SETTLED,
        RECOVERY_RECORD_KINDS, RECOVERY_RELAY_MISSING, RECOVERY_RELAY_PUBLISHES,
    },
};
use futures::future::join_all;
use log::{debug, info, warn};
use nostr::{Event, PublicKey, ToBech32};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TICK: Duration = Duration::from_secs(5);
/// [`TICK`] in seconds, the pace a republish is spread over.
pub const TICK_SECS: i64 = 5;
const FULL_PASS_EVERY: Duration = Duration::from_secs(30 * 60);
/// The full pass's pace: competitions and wallets per tick.
const PASS_COMPETITIONS_PER_TICK: usize = 5;
const PASS_WALLETS_PER_TICK: i64 = 100;
/// Events offered to the relays per tick.
const PUBLISH_BATCH: u32 = 50;
/// Records a republish makes due per tick: fewer than [`PUBLISH_BATCH`], so new versions keep
/// going out while a relay is backfilled.
pub const REPUBLISH_PER_TICK: usize = 20;
const RELAY_TIMEOUT: Duration = Duration::from_secs(15);
/// After this many attempts an event that at least one relay took is not offered again to
/// relays that refuse it. An unreachable relay does not make it give up: the event waits for it.
pub const MAX_ATTEMPTS: u32 = 8;
/// The longest an event waits between attempts.
pub const MAX_RETRY_DELAY_SECS: i64 = 300;
const ORACLE_KEY_RETRY: Duration = Duration::from_secs(60);
/// How often settled records are looked at for retirement, and the record gauges read.
const RETENTION_EVERY: Duration = Duration::from_secs(60);
/// Records retired per look.
const RETIRE_BATCH: u32 = 200;
const WORKER: &str = "recovery-records";

/// Seconds before an event is offered again after `attempts` attempts: a minute, doubling up
/// to five minutes.
pub fn retry_delay(attempts: u32) -> i64 {
    (30i64 << attempts.clamp(1, 4)).min(MAX_RETRY_DELAY_SECS)
}

fn unix_now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// When records whose money is settled are deleted from the relays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Publish NIP-09 deletions for them at all.
    pub delete_settled: bool,
    /// Seconds a record stays after its money is settled.
    pub grace_secs: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            delete_settled: true,
            grace_secs: 7 * 24 * 60 * 60,
        }
    }
}

/// Seconds between connection attempts to a relay that could not be reached. Meanwhile the
/// events due for it count as failed without a connection, so a relay that is gone does not hold
/// up every tick for the connection timeout.
pub const DOWN_RELAY_PROBE_SECS: i64 = 60;

/// Relays whose last connection failed, with when it was tried. When one answers again, the
/// events that waited out retries while it was unreachable are offered at once.
#[derive(Debug, Default)]
pub struct RelayHealth {
    down: HashMap<String, i64>,
}

impl RelayHealth {
    pub fn is_down(&self, relay: &str) -> bool {
        self.down.contains_key(relay)
    }

    /// Whether to connect to `relay` at `now`: always, unless it is down and was tried less
    /// than [`DOWN_RELAY_PROBE_SECS`] ago.
    fn try_now(&self, relay: &str, now: i64) -> bool {
        self.down
            .get(relay)
            .is_none_or(|tried| now - tried >= DOWN_RELAY_PROBE_SECS)
    }
}

#[derive(Default)]
struct Pass {
    /// Retries scheduled before this publisher started were made due.
    started: bool,
    /// Where the change scan resumes, in the list triggers' clock.
    since: Option<String>,
    competitions: VecDeque<Uuid>,
    /// The last wallet's npub read by the full pass, while it is reading wallets.
    wallets_after: Option<String>,
    next_at: Option<Instant>,
    retention_at: Option<Instant>,
    oracle_pubkey: Option<String>,
    oracle_checked_at: Option<Instant>,
    relays: RelayHealth,
}

pub struct RecoveryPublisher {
    recovery: Arc<Recovery>,
    store: Arc<CompetitionStore>,
    users: Arc<UserInfo>,
    oracle: Arc<dyn Oracle>,
    leases: Arc<WorkerLeases>,
    retention: Retention,
    cancel: CancellationToken,
}

impl RecoveryPublisher {
    pub fn new(
        recovery: Arc<Recovery>,
        store: Arc<CompetitionStore>,
        users: Arc<UserInfo>,
        oracle: Arc<dyn Oracle>,
        leases: Arc<WorkerLeases>,
        retention: Retention,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            recovery,
            store,
            users,
            oracle,
            leases,
            retention,
            cancel,
        }
    }

    /// Run until cancelled. Only one coordinator publishes at a time, under a worker lease.
    pub async fn run(self) -> Result<(), anyhow::Error> {
        info!(
            "Publishing recovery records as {} to {} relays; settled records are {}",
            self.recovery.public_key().to_hex(),
            self.recovery.relays().len(),
            if self.retention.delete_settled {
                format!(
                    "deleted {} days after they settle",
                    self.retention.grace_secs / 86_400
                )
            } else {
                String::from("kept")
            }
        );
        let mut pass = Pass::default();
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                result = self.leases.tick(WORKER, self.tick(&mut pass)) => {
                    if let Some(Err(error)) = result {
                        warn!("Recovery records: {error:#}");
                    }
                }
            }
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = tokio::time::sleep(TICK) => {}
            }
        }
        self.leases.release(WORKER).await;
        Ok(())
    }

    async fn tick(&self, pass: &mut Pass) -> Result<(), anyhow::Error> {
        if !pass.started {
            // Retries scheduled before this start, perhaps while a relay was down, are not
            // waited out.
            let waiting = self.store.reset_recovery_backoff(unix_now()).await?;
            if waiting > 0 {
                info!("Recovery records: offering {waiting} events that were waiting to retry");
            }
            pass.started = true;
        }
        let pass_due = pass.next_at.is_none_or(|at| Instant::now() >= at);
        if pass_due && pass.competitions.is_empty() && pass.wallets_after.is_none() {
            if pass.since.is_none() {
                pass.since = Some(self.store.recovery_clock().await?);
            }
            pass.competitions = self.store.recovery_pass_competitions().await?.into();
            pass.wallets_after = Some(String::new());
            pass.next_at = Some(Instant::now() + FULL_PASS_EVERY);
            debug!(
                "Recovery records: full pass over {} competitions",
                pass.competitions.len()
            );
        }

        let mut competitions = Vec::new();
        if let Some(since) = pass.since.clone() {
            let (changed, latest) = self.store.recovery_changed_competitions(&since).await?;
            if let Some(latest) = latest {
                pass.since = Some(latest);
            }
            competitions.extend(changed);
        }
        for _ in 0..PASS_COMPETITIONS_PER_TICK {
            competitions.extend(pass.competitions.pop_front());
        }
        let mut seen = HashSet::new();
        for id in competitions {
            if seen.insert(id) {
                if let Err(error) = self.refresh_competition(id, pass).await {
                    warn!("Recovery records of competition {id}: {error:#}");
                }
            }
        }
        if let Err(error) = self.refresh_wallets(pass).await {
            warn!("Recovery wallet records: {error:#}");
        }
        if pass.retention_at.is_none_or(|at| Instant::now() >= at) {
            pass.retention_at = Some(Instant::now() + RETENTION_EVERY);
            if let Err(error) = self.retire_settled(unix_now()).await {
                warn!("Retiring settled recovery records: {error:#}");
            }
            if let Err(error) = self.read_gauges().await {
                warn!("Recovery record gauges: {error:#}");
            }
        }
        if let Err(error) = publish_due(
            &self.store,
            self.recovery.relays(),
            unix_now(),
            &mut pass.relays,
        )
        .await
        {
            warn!("Publishing recovery records: {error:#}");
        }
        RECOVERY_OUTBOX_DEPTH.set(self.store.recovery_outbox_depth().await?);
        Ok(())
    }

    /// The oracle's key for competition events, read once and cached; retried at most once a
    /// minute while the oracle does not answer.
    async fn oracle_pubkey(&self, pass: &mut Pass) -> Option<String> {
        if pass.oracle_pubkey.is_none()
            && pass
                .oracle_checked_at
                .is_none_or(|at| at.elapsed() >= ORACLE_KEY_RETRY)
        {
            pass.oracle_checked_at = Some(Instant::now());
            match tokio::time::timeout(Duration::from_secs(5), self.oracle.public_key()).await {
                Ok(Ok(key)) => pass.oracle_pubkey = Some(hex::encode(key.serialize())),
                Ok(Err(error)) => debug!("Oracle key for recovery records: {error}"),
                Err(_) => debug!("Oracle key for recovery records: no answer"),
            }
        }
        pass.oracle_pubkey.clone()
    }

    /// Rebuild a competition's records, and note which of them describe money that is settled.
    async fn refresh_competition(&self, id: Uuid, pass: &mut Pass) -> Result<(), anyhow::Error> {
        let outbox = self.store.recovery_outbox_states(Some(id)).await?;
        let now = unix_now();
        let mut settled = Settled::new(&outbox);
        let Some(competition) = self.store.recovery_competition(id).await? else {
            // Only a competition nobody paid into can be deleted, so none of its records holds
            // money.
            for d_tag in outbox.keys() {
                settled.mark(d_tag, true);
            }
            let (settled, unsettled) = settled.changes();
            self.store
                .set_recovery_settled(settled, unsettled, now)
                .await?;
            return Ok(());
        };
        let tickets = self.store.recovery_ticket_rows(id).await?;
        let held: HashSet<Uuid> = tickets
            .iter()
            .filter(|ticket| ticket.money_held)
            .filter_map(ticket_entry_id)
            .collect();
        let mut events = Vec::new();
        let mut described = HashSet::new();
        let mut entries_held = false;
        for (user, mut record) in self.recovery.entry_records(&competition, &tickets) {
            let d_tag = self.recovery.entry_d_tag(&user, record.entry_id);
            let money_held = held.contains(&record.entry_id);
            entries_held |= money_held;
            settled.mark(&d_tag, !money_held);
            described.insert(d_tag.clone());
            let digest = record.digest()?;
            let Some(created_at) = next_version(&outbox, &d_tag, &digest, money_held, now) else {
                continue;
            };
            record.updated_at = created_at.max(0) as u64;
            let event = self.recovery.entry_event(&user, &record, created_at)?;
            events.push(outbox_event(
                d_tag,
                "entry",
                Some(&user),
                Some(id),
                digest,
                &event,
                created_at,
            ));
        }
        // A record no ticket describes any more (its ticket went to another player) is kept
        // until the competition is over.
        for (d_tag, state) in &outbox {
            if state.kind == "entry" && !described.contains(d_tag) {
                entries_held |= competition.active;
                settled.mark(d_tag, !competition.active);
            }
        }
        // The contract is needed while any entry's money may still move.
        let contract_held = competition.active || entries_held;
        if competition.signed_contract.is_some() {
            let oracle_pubkey = self.oracle_pubkey(pass).await;
            let contents = self
                .recovery
                .competition_contents(&competition, oracle_pubkey.as_deref())
                .inspect_err(|error| warn!("Contract event of competition {id}: {error:#}"))
                .ok()
                .flatten();
            for (d_tag, content) in contents.unwrap_or_default() {
                let digest = content_digest(&content);
                settled.mark(&d_tag, !contract_held);
                let Some(created_at) = next_version(&outbox, &d_tag, &digest, contract_held, now)
                else {
                    continue;
                };
                let event =
                    self.recovery
                        .competition_event(id, d_tag.clone(), content, created_at)?;
                events.push(outbox_event(
                    d_tag,
                    "competition",
                    None,
                    Some(id),
                    digest,
                    &event,
                    created_at,
                ));
            }
        }
        for (d_tag, state) in &outbox {
            if state.kind == "competition" {
                settled.mark(d_tag, !contract_held);
            }
        }
        // New rows first, so the marks reach them.
        self.put(events, now).await?;
        let (settled, unsettled) = settled.changes();
        self.store
            .set_recovery_settled(settled, unsettled, now)
            .await?;
        Ok(())
    }

    /// Wallets of players who signed up in the last two minutes, a page of the full pass, and
    /// retired wallets whose players have money held again.
    async fn refresh_wallets(&self, pass: &mut Pass) -> Result<(), anyhow::Error> {
        let database = self.users.auth_database();
        let mut wallets = sqlx::query_as::<_, (String, String)>(
            "SELECT nostr_pubkey, encrypted_bitcoin_private_key FROM user
             WHERE created_at >= datetime('now', '-120 seconds') OR updated_at >= datetime('now', '-120 seconds')",
        )
        .fetch_all(database.read())
        .await?;
        if let Some(after) = pass.wallets_after.take() {
            let page = sqlx::query_as::<_, (String, String)>(
                "SELECT nostr_pubkey, encrypted_bitcoin_private_key FROM user
                 WHERE nostr_pubkey > ? ORDER BY nostr_pubkey LIMIT ?",
            )
            .bind(after)
            .bind(PASS_WALLETS_PER_TICK)
            .fetch_all(database.read())
            .await?;
            if page.len() as i64 == PASS_WALLETS_PER_TICK {
                pass.wallets_after = page.last().map(|(npub, _)| npub.clone());
            }
            wallets.extend(page);
        }
        let mut restore = HashSet::new();
        for hex in self.store.recovery_wallets_to_restore().await? {
            let Ok(user) = PublicKey::from_hex(&hex) else {
                continue;
            };
            let npub = user.to_bech32().unwrap_or_else(|never| match never {});
            let blob: Option<String> = sqlx::query_scalar(
                "SELECT encrypted_bitcoin_private_key FROM user WHERE nostr_pubkey = ?",
            )
            .bind(&npub)
            .fetch_optional(database.read())
            .await?;
            if let Some(blob) = blob {
                wallets.push((npub, blob));
                restore.insert(hex);
            }
        }
        if wallets.is_empty() {
            return Ok(());
        }
        let outbox = self.store.recovery_outbox_states(None).await?;
        let now = unix_now();
        let mut events = Vec::new();
        let mut seen = HashSet::new();
        for (npub, blob) in wallets {
            if !seen.insert(npub.clone()) {
                continue;
            }
            let Ok(user) = PublicKey::parse(&npub) else {
                continue;
            };
            let record = self.recovery.wallet_record(&blob);
            let d_tag = self.recovery.wallet_d_tag(&user);
            let digest = content_digest(&serde_json::to_string(&record)?);
            let needed = restore.contains(&user.to_hex());
            let Some(created_at) = next_version(&outbox, &d_tag, &digest, needed, now) else {
                continue;
            };
            let event = self.recovery.wallet_event(&user, &record, created_at)?;
            events.push(outbox_event(
                d_tag,
                "wallet",
                Some(&user),
                None,
                digest,
                &event,
                created_at,
            ));
        }
        self.put(events, now).await
    }

    async fn put(&self, events: Vec<RecoveryOutboxEvent>, now: i64) -> Result<(), anyhow::Error> {
        if events.is_empty() {
            return Ok(());
        }
        debug!("Recovery records: {} new versions", events.len());
        // Without relays the records are kept for the recovery file only.
        let published_at = self.recovery.relays().is_empty().then_some(now);
        self.store
            .put_recovery_events(events, now, published_at)
            .await?;
        Ok(())
    }

    /// Note which wallet records are settled, then retire the records whose money settled more
    /// than the grace period ago: each gets a NIP-09 deletion and is not published again.
    async fn retire_settled(&self, now: i64) -> Result<(), anyhow::Error> {
        self.store.settle_recovery_wallets(now).await?;
        if !self.retention.delete_settled {
            return Ok(());
        }
        let due = self
            .store
            .recovery_records_to_retire(now - self.retention.grace_secs, RETIRE_BATCH)
            .await?;
        if due.is_empty() {
            return Ok(());
        }
        let mut deletions = Vec::with_capacity(due.len());
        let mut kinds = HashMap::new();
        for record in due {
            let event_id = match serde_json::from_str::<Event>(&record.event_json) {
                Ok(event) => event.id.to_hex(),
                Err(error) => {
                    warn!("Recovery record {} cannot be read: {error}", record.d_tag);
                    continue;
                }
            };
            let deleted_at = next_created_at(now, Some(record.created_at));
            let deletion = self
                .recovery
                .deletion_event(&record.d_tag, &event_id, deleted_at)?;
            kinds.insert(record.d_tag.clone(), record.kind);
            deletions.push(RecoveryDeletion {
                d_tag: record.d_tag,
                content_sha256: record.content_sha256,
                created_at: record.created_at,
                deletion_json: serde_json::to_string(&deletion)?,
                deleted_at,
            });
        }
        // Without relays there is nothing to delete from.
        let published_at = self.recovery.relays().is_empty().then_some(now);
        let retired = self
            .store
            .put_recovery_deletions(deletions, now, published_at)
            .await?;
        if retired > 0 {
            info!("Recovery records: retiring {retired} records whose money is settled");
        }
        if published_at.is_some() {
            for kind in kinds.values() {
                RECOVERY_DELETIONS.with_label_values(&[kind.as_str()]).inc();
            }
        }
        Ok(())
    }

    async fn read_gauges(&self) -> Result<(), anyhow::Error> {
        let counts = self.store.recovery_counts().await?;
        for kind in RECOVERY_RECORD_KINDS {
            let count = counts.iter().find(|count| count.kind == kind);
            RECOVERY_RECORDS_LIVE
                .with_label_values(&[kind])
                .set(count.map_or(0, |count| count.live));
            RECOVERY_RECORDS_SETTLED
                .with_label_values(&[kind])
                .set(count.map_or(0, |count| count.settled));
        }
        RECOVERY_RELAY_MISSING.reset();
        for relay in self.recovery.relays() {
            let missing = self.store.recovery_relay_missing(relay).await?;
            RECOVERY_RELAY_MISSING
                .with_label_values(&[relay.as_str()])
                .set(missing);
        }
        Ok(())
    }
}

/// Which records' money is settled, as far as the outbox does not know it yet.
struct Settled<'a> {
    outbox: &'a HashMap<String, RecoveryOutboxState>,
    settled: Vec<String>,
    unsettled: Vec<String>,
    marked: HashSet<String>,
}

impl<'a> Settled<'a> {
    fn new(outbox: &'a HashMap<String, RecoveryOutboxState>) -> Self {
        Self {
            outbox,
            settled: Vec::new(),
            unsettled: Vec::new(),
            marked: HashSet::new(),
        }
    }

    /// Note whether the record under `d_tag` is settled; the first mark of a record counts.
    /// A record this competition's outbox does not hold (new, or moved here with its ticket from
    /// a queued competition that found it settled) is marked unsettled when it is not.
    fn mark(&mut self, d_tag: &str, settled: bool) {
        if !self.marked.insert(d_tag.to_owned()) {
            return;
        }
        let state = self.outbox.get(d_tag);
        let known = state.is_some_and(|state| state.settled_at.is_some());
        if settled && !known {
            self.settled.push(d_tag.to_owned());
        } else if !settled && (known || state.is_none()) {
            self.unsettled.push(d_tag.to_owned());
        }
    }

    /// The records newly settled, and those no longer settled.
    fn changes(self) -> (Vec<String>, Vec<String>) {
        (self.settled, self.unsettled)
    }
}

/// Offer the events due at `now` to every relay that has not taken them yet, and record
/// the attempts. A relay unreachable on its last try that answers now has every event waiting
/// out a retry offered again at once.
pub async fn publish_due(
    store: &CompetitionStore,
    relays: &[String],
    now: i64,
    health: &mut RelayHealth,
) -> Result<(), anyhow::Error> {
    if relays.is_empty() {
        return Ok(());
    }
    let due = store.due_recovery_events(now, PUBLISH_BATCH).await?;
    if due.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = due
        .iter()
        .map(|event| {
            serde_json::from_str::<Event>(&event.event_json)
                .map(|event| event.id.to_hex())
                .unwrap_or_default()
        })
        .collect();
    // `None` for a relay that took every due event already, so nothing was sent to it.
    let answers = join_all(relays.iter().map(|relay| {
        let batch: Vec<(String, String)> = due
            .iter()
            .zip(&ids)
            .filter(|(event, _)| !event.accepted_relays.contains(relay))
            .map(|(event, id)| (id.clone(), event.event_json.clone()))
            .collect();
        let try_now = health.try_now(relay, now);
        async move {
            if batch.is_empty() {
                return None;
            }
            if !try_now {
                return Some(Err(anyhow::anyhow!("unreachable on its last try")));
            }
            Some(publish_to_relay(relay, &batch, RELAY_TIMEOUT).await)
        }
    }))
    .await;

    let mut recovered = Vec::new();
    for (relay, answer) in relays.iter().zip(&answers) {
        match answer {
            Some(Err(error)) => {
                if health.try_now(relay, now) && health.down.insert(relay.clone(), now).is_none() {
                    warn!("Recovery relay {relay} is unreachable: {error:#}");
                }
            }
            Some(Ok(_)) if health.down.remove(relay).is_some() => {
                recovered.push(relay.clone());
            }
            Some(Ok(_)) | None => {}
        }
    }

    let mut attempts = Vec::with_capacity(due.len());
    for (event, id) in due.into_iter().zip(&ids) {
        let accepted_before = event.accepted_relays.clone();
        let mut accepted = event.accepted_relays;
        let mut errors = Vec::new();
        for (relay, answer) in relays.iter().zip(&answers) {
            if accepted.contains(relay) {
                continue;
            }
            let failure = match answer {
                Some(Ok(answers)) => match answers.get(id) {
                    Some(Ok(())) => None,
                    Some(Err(reason)) => Some(format!("{relay}: {reason}")),
                    None => Some(format!("{relay}: no answer")),
                },
                Some(Err(error)) => Some(format!("{relay}: {error:#}")),
                // Nothing was sent: every due event had reached this relay.
                None => continue,
            };
            match failure {
                None => {
                    RECOVERY_RELAY_PUBLISHES
                        .with_label_values(&["accepted"])
                        .inc();
                    accepted.push(relay.clone());
                }
                Some(failure) => {
                    RECOVERY_RELAY_PUBLISHES
                        .with_label_values(&["failed"])
                        .inc();
                    errors.push(failure);
                }
            }
        }
        let tries = event.attempts + 1;
        // A relay that could not be reached has not refused the event; it gets it once it is
        // back, however long that takes.
        let unreachable = relays
            .iter()
            .zip(&answers)
            .any(|(relay, answer)| !accepted.contains(relay) && matches!(answer, Some(Err(_))));
        let done = relays.iter().all(|relay| accepted.contains(relay))
            || (tries >= MAX_ATTEMPTS && !accepted.is_empty() && !unreachable);
        if !errors.is_empty() {
            debug!("Recovery event {}: {}", event.d_tag, errors.join("; "));
        }
        if done && event.deletion.is_some() {
            RECOVERY_DELETIONS
                .with_label_values(&[event.kind.as_str()])
                .inc();
        }
        attempts.push(RecoveryAttempt {
            d_tag: event.d_tag,
            content_sha256: event.content_sha256,
            deletion: event.deletion,
            attempts: event.attempts,
            accepted_before,
            accepted_relays: accepted,
            published_at: done.then_some(now),
            next_attempt_at: now + retry_delay(tries),
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        });
    }
    store.record_recovery_attempts(attempts).await?;
    if !recovered.is_empty() {
        let waiting = store.reset_recovery_backoff(now).await?;
        info!(
            "Recovery relay {} answers again; offering the {waiting} events waiting to retry now",
            recovered.join(", ")
        );
    }
    Ok(())
}

/// The `created_at` of a record's new version, or `None` when there is none to publish: the
/// outbox holds this version already, or the record was retired and `money_held` says its money
/// is still settled. A retired record whose money moves again is published again, after its
/// deletion.
fn next_version(
    outbox: &HashMap<String, RecoveryOutboxState>,
    d_tag: &str,
    digest: &str,
    money_held: bool,
    now: i64,
) -> Option<i64> {
    match outbox.get(d_tag) {
        None => Some(now),
        Some(state) if state.deleted_at.is_some() => {
            money_held.then(|| next_created_at(now, Some(state.latest())))
        }
        Some(state) if state.content_sha256 == digest => None,
        Some(state) => Some(next_created_at(now, Some(state.created_at))),
    }
}

fn outbox_event(
    d_tag: String,
    kind: &'static str,
    user: Option<&PublicKey>,
    competition_id: Option<Uuid>,
    content_sha256: String,
    event: &Event,
    created_at: i64,
) -> RecoveryOutboxEvent {
    RecoveryOutboxEvent {
        d_tag,
        kind,
        user_pubkey: user.map(PublicKey::to_hex),
        competition_id,
        content_sha256,
        event_json: serde_json::to_string(event).unwrap_or_default(),
        created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{UserStore, WorkerLeases},
        infra::{
            db::{DBConnection, DatabasePoolConfig, DatabaseType},
            oracle_mock::MockOracle,
        },
    };
    use bitcoin::Network;
    use nostr::{Keys, SecretKey};
    use sha2::{Digest, Sha256};
    use std::sync::Mutex;

    fn secret(byte: u8) -> SecretKey {
        let mut bytes = [0u8; 32];
        bytes[31] = byte;
        SecretKey::from_slice(&bytes).unwrap()
    }

    /// A publisher over fresh databases, publishing to a test relay that takes every event.
    struct Fixture {
        publisher: RecoveryPublisher,
        recovery: Arc<Recovery>,
        store: Arc<CompetitionStore>,
        competitions: DBConnection,
        users: DBConnection,
        /// Every event the relay was sent.
        seen: Arc<Mutex<Vec<serde_json::Value>>>,
        _directory: tempfile::TempDir,
    }

    impl Fixture {
        async fn new(grace_secs: i64) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().to_str().unwrap();
            let competitions = DBConnection::new(
                path,
                "competitions",
                DatabasePoolConfig::default(),
                DatabaseType::Competitions,
            )
            .await
            .unwrap();
            let users = DBConnection::new(
                path,
                "users",
                DatabasePoolConfig::default(),
                DatabaseType::Users,
            )
            .await
            .unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = seen.clone();
            let relay = super::super::relay::test_relay::spawn(Arc::new(
                move |event: &serde_json::Value| {
                    log.lock().unwrap().push(event.clone());
                    Some((true, String::new()))
                },
            ))
            .await;
            let recovery = Arc::new(Recovery::new(
                secret(1),
                Network::Signet,
                vec![relay],
                "https://arkd.example".into(),
            ));
            let store = Arc::new(CompetitionStore::new(competitions.clone()));
            let publisher = RecoveryPublisher::new(
                recovery.clone(),
                store.clone(),
                Arc::new(UserInfo::new(UserStore::new(users.clone()))),
                Arc::new(MockOracle::new([12; 32])),
                Arc::new(WorkerLeases::new(
                    store.clone(),
                    "test".into(),
                    Duration::from_secs(60),
                )),
                Retention {
                    delete_settled: true,
                    grace_secs,
                },
                CancellationToken::new(),
            );
            Self {
                publisher,
                recovery,
                store,
                competitions,
                users,
                seen,
                _directory: directory,
            }
        }

        async fn sql(&self, sql: &'static str, binds: Vec<String>) {
            self.competitions
                .execute_write(move |pool| async move {
                    let mut query = sqlx::query(sql);
                    for bind in binds {
                        query = query.bind(bind);
                    }
                    query.execute(&pool).await?;
                    Ok(())
                })
                .await
                .unwrap();
        }

        async fn add_user(&self, player: &Keys) {
            let npub = player.public_key().to_bech32().unwrap();
            self.users
                .execute_write(move |pool| async move {
                    sqlx::query(
                        "INSERT INTO user (nostr_pubkey, encrypted_bitcoin_private_key, network)
                         VALUES (?, 'wallet blob', 'signet')",
                    )
                    .bind(npub)
                    .execute(&pool)
                    .await?;
                    Ok(())
                })
                .await
                .unwrap();
        }

        /// A competition still running, with one paid entry of `player`'s. Returns the
        /// competition, ticket and entry ids.
        async fn add_entry(&self, player: &Keys) -> (Uuid, Uuid, Uuid) {
            let (competition, ticket, entry) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
            let preimage = [9u8; 32];
            let hash = hex::encode(Sha256::digest(preimage));
            let entry_key = dlctix::secp::Scalar::from_slice(&[7; 32])
                .unwrap()
                .base_point_mul()
                .to_string();
            self.sql(
                "INSERT INTO competitions (id, created_at, event_submission)
                 VALUES (?, datetime('now'), '{}')",
                vec![competition.to_string()],
            )
            .await;
            self.sql(
                "INSERT INTO tickets (id, event_id, encrypted_preimage, hash, reserved_by,
                                      paid_at, settled_at)
                 VALUES (?, ?, ?, ?, ?, datetime('now'), datetime('now'))",
                vec![
                    ticket.to_string(),
                    competition.to_string(),
                    hex::encode(preimage),
                    hash,
                    player.public_key().to_hex(),
                ],
            )
            .await;
            self.sql(
                "INSERT INTO entries (id, event_id, ticket_id, pubkey, ephemeral_pubkey,
                                      payout_hash, entry_submission)
                 VALUES (?, ?, ?, ?, ?, ?, '{}')",
                vec![
                    entry.to_string(),
                    competition.to_string(),
                    ticket.to_string(),
                    player.public_key().to_hex(),
                    entry_key,
                    "ab".repeat(32),
                ],
            )
            .await;
            (competition, ticket, entry)
        }

        async fn held(&self, competition: Uuid) -> bool {
            let rows = self.store.recovery_ticket_rows(competition).await.unwrap();
            assert_eq!(rows.len(), 1);
            rows[0].money_held
        }

        async fn state(&self, d_tag: &str, competition: Option<Uuid>) -> RecoveryOutboxState {
            self.store
                .recovery_outbox_states(competition)
                .await
                .unwrap()
                .remove(d_tag)
                .unwrap()
        }

        async fn publish(&self, pass: &mut Pass, now: i64) {
            publish_due(&self.store, self.recovery.relays(), now, &mut pass.relays)
                .await
                .unwrap();
        }

        fn sent(&self, kind: u64) -> Vec<serde_json::Value> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event["kind"] == kind)
                .cloned()
                .collect()
        }
    }

    /// Money is held while the competition runs, a payment or escrow is open, a winner is owed
    /// or a payout is held; a written-off escrow refund leaves the escrow to the player, so it
    /// is held too.
    #[tokio::test]
    async fn money_is_held_until_every_way_it_could_still_move_is_closed() {
        let fixture = Fixture::new(60).await;
        let player = Keys::new(secret(2));
        let (competition, ticket, entry) = fixture.add_entry(&player).await;
        let id = competition.to_string();
        assert!(fixture.held(competition).await, "running");

        fixture
            .sql(
                "UPDATE competitions SET completed_at = datetime('now') WHERE id = ?",
                vec![id.clone()],
            )
            .await;
        assert!(!fixture.held(competition).await, "completed");

        fixture
            .sql(
                "INSERT INTO payout_holds (entry_id, payment_hash, amount_sats, reason, held_at)
                 VALUES (?, 'hash', 1000, 'restore', datetime('now'))",
                vec![entry.to_string()],
            )
            .await;
        assert!(fixture.held(competition).await, "payout held");
        fixture
            .sql(
                "UPDATE payout_holds SET released_at = datetime('now') WHERE entry_id = ?",
                vec![entry.to_string()],
            )
            .await;
        assert!(!fixture.held(competition).await, "payout released");

        fixture
            .sql(
                "INSERT INTO owed_winners (entry_id, competition_id, amount_sats, owed_since)
                 VALUES (?, ?, 1000, datetime('now'))",
                vec![entry.to_string(), id.clone()],
            )
            .await;
        assert!(fixture.held(competition).await, "winner owed");
        fixture
            .sql(
                "UPDATE owed_winners SET claimed_on_chain_at = datetime('now') WHERE entry_id = ?",
                vec![entry.to_string()],
            )
            .await;
        assert!(!fixture.held(competition).await, "winner claimed");

        // Cancelled before its contract was funded, with the escrow funded.
        fixture
            .sql(
                "UPDATE competitions SET completed_at = NULL, cancelled_at = datetime('now')
                 WHERE id = ?",
                vec![id.clone()],
            )
            .await;
        assert!(!fixture.held(competition).await, "cancelled, nothing open");
        fixture
            .sql(
                "INSERT INTO ticket_ark_escrows (ticket_id, ticket_hash, escrow_tap_tree,
                                                 escrow_address, vtxo_outpoint, vtxo_sats, funded_at)
                 SELECT id, hash, '00', 'tark1escrow', 'aa:0', 5000, 1 FROM tickets WHERE id = ?",
                vec![ticket.to_string()],
            )
            .await;
        assert!(fixture.held(competition).await, "escrow funded");
        fixture
            .sql(
                "INSERT INTO ticket_ark_refund_write_offs (ticket_id, ticket_hash, reason, written_off_at)
                 SELECT id, hash, 'stuck', 1 FROM tickets WHERE id = ?",
                vec![ticket.to_string()],
            )
            .await;
        assert!(fixture.held(competition).await, "refund written off");
        fixture
            .sql(
                "INSERT INTO ticket_ark_refunds (ticket_id, refund_id, invoice, payment_hash,
                                                 fee_sats, state, created_at, updated_at)
                 VALUES (?, 'refund', 'lnbc', 'hash', 0, 'settled', 1, 1)",
                vec![ticket.to_string()],
            )
            .await;
        assert!(!fixture.held(competition).await, "escrow refunded");

        // A held Lightning payment is cancelled, or not.
        fixture
            .sql(
                "UPDATE tickets SET settled_at = NULL WHERE id = ?",
                vec![ticket.to_string()],
            )
            .await;
        assert!(fixture.held(competition).await, "payment held");
        fixture
            .sql(
                "UPDATE tickets SET invoice_cancelled_at = datetime('now') WHERE id = ?",
                vec![ticket.to_string()],
            )
            .await;
        assert!(!fixture.held(competition).await, "payment cancelled");
    }

    /// The whole retention cycle: a record is kept while the money runs, deleted once it has
    /// been settled for the grace period and not published again, and published again if its
    /// money moves after all. The wallet record follows its player's entries.
    #[tokio::test]
    async fn settled_records_are_deleted_after_their_grace_and_return_if_money_moves() {
        let fixture = Fixture::new(100).await;
        let player = Keys::new(secret(2));
        fixture.add_user(&player).await;
        let (competition, _, entry) = fixture.add_entry(&player).await;
        let entry_tag = fixture.recovery.entry_d_tag(&player.public_key(), entry);
        let wallet_tag = fixture.recovery.wallet_d_tag(&player.public_key());
        let mut pass = Pass::default();

        // While the competition runs the records are published and kept.
        fixture
            .publisher
            .refresh_competition(competition, &mut pass)
            .await
            .unwrap();
        fixture.publisher.refresh_wallets(&mut pass).await.unwrap();
        let now = unix_now();
        fixture.publish(&mut pass, now).await;
        assert_eq!(fixture.store.recovery_outbox_depth().await.unwrap(), 0);
        assert_eq!(fixture.sent(30078).len(), 2);
        fixture.publisher.retire_settled(now + 1_000).await.unwrap();
        let entry_state = fixture.state(&entry_tag, Some(competition)).await;
        assert_eq!(entry_state.settled_at, None);
        assert_eq!(fixture.state(&wallet_tag, None).await.settled_at, None);
        assert_eq!(fixture.store.recovery_outbox_depth().await.unwrap(), 0);

        // It completes: the entry's money is settled.
        fixture
            .sql(
                "UPDATE competitions SET completed_at = datetime('now') WHERE id = ?",
                vec![competition.to_string()],
            )
            .await;
        fixture
            .publisher
            .refresh_competition(competition, &mut pass)
            .await
            .unwrap();
        let settled_at = fixture
            .state(&entry_tag, Some(competition))
            .await
            .settled_at
            .expect("settled");

        // Within the grace period nothing is deleted; the wallet's own grace starts.
        fixture
            .publisher
            .retire_settled(settled_at + 50)
            .await
            .unwrap();
        assert_eq!(fixture.store.recovery_outbox_depth().await.unwrap(), 0);
        assert_eq!(
            fixture.state(&wallet_tag, None).await.settled_at,
            Some(settled_at + 50)
        );

        // After it the entry's record is deleted, by address and by id.
        fixture
            .publisher
            .retire_settled(settled_at + 100)
            .await
            .unwrap();
        assert_eq!(fixture.store.recovery_outbox_depth().await.unwrap(), 1);
        fixture.publish(&mut pass, settled_at + 100).await;
        assert_eq!(fixture.store.recovery_outbox_depth().await.unwrap(), 0);
        let deletions = fixture.sent(5);
        assert_eq!(deletions.len(), 1);
        let published = fixture.sent(30078);
        let record = published
            .iter()
            .find(|event| {
                event["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tag| tag[0] == "d" && tag[1] == entry_tag.as_str())
            })
            .unwrap();
        let tags = deletions[0]["tags"].as_array().unwrap();
        let tag = |name: &str| {
            tags.iter()
                .find(|tag| tag[0] == name)
                .map(|tag| tag[1].as_str().unwrap().to_owned())
        };
        assert_eq!(tag("e").as_deref(), record["id"].as_str());
        assert_eq!(
            tag("a"),
            Some(format!(
                "30078:{}:{entry_tag}",
                fixture.recovery.public_key().to_hex()
            ))
        );
        assert_eq!(tag("k").as_deref(), Some("30078"));
        assert_eq!(
            deletions[0]["pubkey"],
            fixture.recovery.public_key().to_hex()
        );
        assert!(deletions[0]["created_at"].as_i64() > record["created_at"].as_i64());

        // A retired record is not published again, nor put in the recovery file.
        fixture
            .publisher
            .refresh_competition(competition, &mut pass)
            .await
            .unwrap();
        fixture.publish(&mut pass, settled_at + 200).await;
        assert_eq!(fixture.sent(30078).len(), 2);
        let kit = fixture
            .store
            .recovery_kit_events(&player.public_key().to_hex())
            .await
            .unwrap();
        assert!(kit.iter().all(|(kind, _)| kind == "wallet"), "{kit:?}");

        // The wallet goes once its own grace is over.
        fixture
            .publisher
            .retire_settled(settled_at + 150)
            .await
            .unwrap();
        fixture.publish(&mut pass, settled_at + 150).await;
        assert_eq!(fixture.sent(5).len(), 2);
        let counts = fixture.store.recovery_counts().await.unwrap();
        assert!(counts
            .iter()
            .all(|count| count.live == 0 && count.retired == 1));

        // A winner turns out to be owed: the entry's record and the wallet come back, after
        // their deletions.
        fixture
            .sql(
                "INSERT INTO owed_winners (entry_id, competition_id, amount_sats, owed_since)
                 VALUES (?, ?, 1000, datetime('now'))",
                vec![entry.to_string(), competition.to_string()],
            )
            .await;
        fixture
            .publisher
            .refresh_competition(competition, &mut pass)
            .await
            .unwrap();
        fixture.publisher.refresh_wallets(&mut pass).await.unwrap();
        let entry_state = fixture.state(&entry_tag, Some(competition)).await;
        assert_eq!(entry_state.deleted_at, None);
        assert_eq!(entry_state.settled_at, None);
        assert!(entry_state.created_at > settled_at + 100);
        let wallet_state = fixture.state(&wallet_tag, None).await;
        assert_eq!(wallet_state.deleted_at, None);
        fixture.publish(&mut pass, settled_at + 1_000).await;
        assert_eq!(fixture.sent(30078).len(), 4);
        fixture
            .publisher
            .retire_settled(settled_at + 10_000)
            .await
            .unwrap();
        assert_eq!(fixture.state(&wallet_tag, None).await.settled_at, None);
        assert_eq!(fixture.store.recovery_outbox_depth().await.unwrap(), 0);
    }

    fn state(digest: &str, created_at: i64, deleted_at: Option<i64>) -> RecoveryOutboxState {
        RecoveryOutboxState {
            kind: "entry".into(),
            content_sha256: digest.into(),
            created_at,
            settled_at: deleted_at.map(|at| at - 100),
            deleted_at,
        }
    }

    #[test]
    fn retries_wait_at_most_five_minutes() {
        let delays: Vec<i64> = (1..=10).map(retry_delay).collect();
        assert_eq!(delays, [60, 120, 240, 300, 300, 300, 300, 300, 300, 300]);
        assert_eq!(retry_delay(0), 60);
        assert_eq!(retry_delay(u32::MAX), MAX_RETRY_DELAY_SECS);
    }

    #[test]
    fn a_retired_record_is_published_again_only_once_its_money_moves() {
        let outbox = HashMap::from([
            ("live".to_string(), state("a", 1_000, None)),
            ("retired".to_string(), state("a", 1_000, Some(2_000))),
        ]);
        // New, changed and unchanged live records.
        assert_eq!(next_version(&outbox, "new", "a", false, 500), Some(500));
        assert_eq!(next_version(&outbox, "live", "a", true, 1_500), None);
        assert_eq!(
            next_version(&outbox, "live", "b", false, 1_500),
            Some(1_500)
        );
        assert_eq!(next_version(&outbox, "live", "b", false, 900), Some(1_001));
        // A retired record stays retired while its money is settled, even if it changed.
        assert_eq!(next_version(&outbox, "retired", "b", false, 3_000), None);
        // Once money moves again its next version follows the deletion.
        assert_eq!(
            next_version(&outbox, "retired", "a", true, 1_500),
            Some(2_001)
        );
        assert_eq!(
            next_version(&outbox, "retired", "a", true, 3_000),
            Some(3_000)
        );
    }

    #[test]
    fn only_changes_in_settlement_are_written() {
        let mut settled_state = state("a", 1, None);
        settled_state.settled_at = Some(5);
        let outbox = HashMap::from([
            ("settled".to_string(), settled_state),
            ("open".to_string(), state("a", 1, None)),
        ]);
        let mut settled = Settled::new(&outbox);
        settled.mark("settled", true);
        settled.mark("open", true);
        settled.mark("new", true);
        settled.mark("new-held", false);
        // The first mark of a record counts.
        settled.mark("open", false);
        // A record this outbox does not hold may carry a mark from another competition.
        assert_eq!(
            settled.changes(),
            (
                vec!["open".to_string(), "new".to_string()],
                vec!["new-held".to_string()]
            )
        );

        let mut settled = Settled::new(&outbox);
        settled.mark("settled", false);
        settled.mark("open", false);
        assert_eq!(
            settled.changes(),
            (Vec::<String>::new(), vec!["settled".to_string()])
        );
    }
}
