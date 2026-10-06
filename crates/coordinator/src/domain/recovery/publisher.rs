//! Keeps the recovery records current and gets them to the relays.
//!
//! Every few seconds the publisher rebuilds the records of the competitions that changed since
//! its last look (the list triggers note every change to a competition's entries, tickets and
//! payouts), and of a few competitions and wallets from a full pass that starts at launch and
//! repeats every half hour. The full pass is also the backfill of records that existed before
//! this ran. A record whose plaintext did not change is skipped; a changed one replaces its
//! outbox row, and the outbox is published with retries.
//!
//! Nothing here runs in the entry, kickoff, payout or refund paths, and no error stops the
//! publisher: a relay or database failure is logged and tried again on a later tick.

use super::{content_digest, next_created_at, publish_to_relay, Recovery};
use crate::{
    domain::{
        CompetitionStore, RecoveryAttempt, RecoveryOutboxEvent, RecoveryOutboxState, UserInfo,
        WorkerLeases,
    },
    infra::oracle::Oracle,
    metrics::{RECOVERY_OUTBOX_DEPTH, RECOVERY_RELAY_PUBLISHES},
};
use futures::future::join_all;
use log::{debug, info, warn};
use nostr::{Event, PublicKey};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TICK: Duration = Duration::from_secs(5);
const FULL_PASS_EVERY: Duration = Duration::from_secs(30 * 60);
/// The full pass's pace: competitions and wallets per tick.
const PASS_COMPETITIONS_PER_TICK: usize = 5;
const PASS_WALLETS_PER_TICK: i64 = 100;
/// Events offered to the relays per tick.
const PUBLISH_BATCH: u32 = 50;
const RELAY_TIMEOUT: Duration = Duration::from_secs(15);
/// After this many attempts an event that at least one relay took is not offered again.
pub const MAX_ATTEMPTS: u32 = 8;
const ORACLE_KEY_RETRY: Duration = Duration::from_secs(60);
const WORKER: &str = "recovery-records";

/// Seconds before an event is offered again after `attempts` attempts: a minute, doubling up
/// to an hour.
pub fn retry_delay(attempts: u32) -> i64 {
    (30i64 << attempts.clamp(1, 7)).min(3600)
}

fn unix_now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[derive(Default)]
struct Pass {
    /// Where the change scan resumes, in the list triggers' clock.
    since: Option<String>,
    competitions: VecDeque<Uuid>,
    /// The last wallet's npub read by the full pass, while it is reading wallets.
    wallets_after: Option<String>,
    next_at: Option<Instant>,
    oracle_pubkey: Option<String>,
    oracle_checked_at: Option<Instant>,
}

pub struct RecoveryPublisher {
    recovery: Arc<Recovery>,
    store: Arc<CompetitionStore>,
    users: Arc<UserInfo>,
    oracle: Arc<dyn Oracle>,
    leases: Arc<WorkerLeases>,
    cancel: CancellationToken,
}

impl RecoveryPublisher {
    pub fn new(
        recovery: Arc<Recovery>,
        store: Arc<CompetitionStore>,
        users: Arc<UserInfo>,
        oracle: Arc<dyn Oracle>,
        leases: Arc<WorkerLeases>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            recovery,
            store,
            users,
            oracle,
            leases,
            cancel,
        }
    }

    /// Run until cancelled. Only one coordinator publishes at a time, under a worker lease.
    pub async fn run(self) -> Result<(), anyhow::Error> {
        info!(
            "Publishing recovery records as {} to {} relays",
            self.recovery.public_key().to_hex(),
            self.recovery.relays().len()
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
        let pass_due = pass.next_at.is_none_or(|at| Instant::now() >= at);
        if pass_due && pass.competitions.is_empty() && pass.wallets_after.is_none() {
            if pass.since.is_none() {
                pass.since = Some(self.store.recovery_clock().await?);
            }
            pass.competitions = self.store.recovery_open_competitions().await?.into();
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
        if let Err(error) = publish_due(&self.store, self.recovery.relays(), unix_now()).await {
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

    async fn refresh_competition(&self, id: Uuid, pass: &mut Pass) -> Result<(), anyhow::Error> {
        let Some(competition) = self.store.recovery_competition(id).await? else {
            return Ok(());
        };
        let tickets = self.store.recovery_ticket_rows(id).await?;
        let outbox = self.store.recovery_outbox_states(Some(id)).await?;
        let now = unix_now();
        let mut events = Vec::new();
        for (user, mut record) in self.recovery.entry_records(&competition, &tickets) {
            let d_tag = self.recovery.entry_d_tag(&user, record.entry_id);
            let digest = record.digest()?;
            let Some(created_at) = changed(&outbox, &d_tag, &digest, now) else {
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
                let Some(created_at) = changed(&outbox, &d_tag, &digest, now) else {
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
        self.put(events, now).await
    }

    /// Wallets of players who signed up in the last two minutes, and a page of the full pass.
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
        if wallets.is_empty() {
            return Ok(());
        }
        let outbox = self.store.recovery_outbox_states(None).await?;
        let now = unix_now();
        let mut events = Vec::new();
        for (npub, blob) in wallets {
            let Ok(user) = PublicKey::parse(&npub) else {
                continue;
            };
            let record = self.recovery.wallet_record(&blob);
            let d_tag = self.recovery.wallet_d_tag(&user);
            let digest = content_digest(&serde_json::to_string(&record)?);
            let Some(created_at) = changed(&outbox, &d_tag, &digest, now) else {
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
}

/// Offer the events due at `now` to every relay that has not taken them yet, and record
/// the attempts.
pub async fn publish_due(
    store: &CompetitionStore,
    relays: &[String],
    now: i64,
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
    let answers = join_all(relays.iter().map(|relay| {
        let batch: Vec<(String, String)> = due
            .iter()
            .zip(&ids)
            .filter(|(event, _)| !event.accepted_relays.contains(relay))
            .map(|(event, id)| (id.clone(), event.event_json.clone()))
            .collect();
        async move {
            if batch.is_empty() {
                return Ok(HashMap::new());
            }
            publish_to_relay(relay, &batch, RELAY_TIMEOUT).await
        }
    }))
    .await;

    let mut attempts = Vec::with_capacity(due.len());
    for (event, id) in due.into_iter().zip(&ids) {
        let mut accepted = event.accepted_relays;
        let mut errors = Vec::new();
        for (relay, answer) in relays.iter().zip(&answers) {
            if accepted.contains(relay) {
                continue;
            }
            let failure = match answer {
                Ok(answers) => match answers.get(id) {
                    Some(Ok(())) => None,
                    Some(Err(reason)) => Some(format!("{relay}: {reason}")),
                    None => Some(format!("{relay}: no answer")),
                },
                Err(error) => Some(format!("{relay}: {error:#}")),
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
        let done = relays.iter().all(|relay| accepted.contains(relay))
            || (tries >= MAX_ATTEMPTS && !accepted.is_empty());
        if !errors.is_empty() {
            debug!("Recovery event {}: {}", event.d_tag, errors.join("; "));
        }
        attempts.push(RecoveryAttempt {
            d_tag: event.d_tag,
            content_sha256: event.content_sha256,
            accepted_relays: accepted,
            published_at: done.then_some(now),
            next_attempt_at: now + retry_delay(tries),
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        });
    }
    store.record_recovery_attempts(attempts).await?;
    Ok(())
}

/// The `created_at` of a record's new version, or `None` when the outbox already holds this
/// version.
fn changed(
    outbox: &HashMap<String, RecoveryOutboxState>,
    d_tag: &str,
    digest: &str,
    now: i64,
) -> Option<i64> {
    let previous = outbox.get(d_tag);
    if previous.is_some_and(|previous| previous.content_sha256 == digest) {
        return None;
    }
    Some(next_created_at(
        now,
        previous.map(|previous| previous.created_at),
    ))
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
