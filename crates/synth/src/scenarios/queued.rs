//! Queued competitions: players enter without a seat count. When registration closes at the
//! observation start, the coordinator splits the complete tickets into even pools. Each pool is a
//! competition of its own, with its own oracle event, Keymeld session and Arkade batch, and runs
//! the usual lifecycle from escrow confirmation on. A queue too small for one pool is cancelled
//! and every escrow refunded.
//!
//! - `queued_split`: more players than one pool holds, 27 against pools of 2 to 25 by default.
//!   The queue forms `ceil(N / max)` pools whose sizes differ by at most one and which hold every
//!   entry exactly once. Each pool then runs to funding and on to awaiting its attestation.
//! - `queued_one_pool`: fewer players than a full pool, 5 by default: one pool of them all.
//! - `queued_too_few`: 2 players against a minimum of 3. The queue is cancelled and both escrows
//!   are refunded to the players' Lightning Address.
//! - `queued_leftover_refund`: 3 players enter and a fourth pays but never submits an entry. The
//!   pools form from the complete tickets alone and run, and the incomplete ticket is refunded
//!   from the queue.
//!
//! `--queue-players` sets how many players enter completely, and `--max-pool-players` the
//! largest pool, so a split can be tried with fewer payments.
//!
//! Every queued entry waits in an Arkade escrow that a real payment funds, and names the
//! Lightning Address it is refunded to if its queue never starts. So these scenarios pay from
//! `lnd`, and need a `lightning_address`.
//!
//! TODO: a pool whose kickoff batch fails, whose tickets are then refunded through the pool's own
//! session while the other pools fund. Nothing a player does after paying can fail a batch: the
//! coordinator validates each key deposit before it shows the invoice, and keeps the deposit
//! itself. The scenario needs an operator test hook on the coordinator that fails one pool's
//! batch, like the one that settles invoices. It would then follow that pool to `failed`, collect
//! its tickets' refunds at the pool's id, and follow the other pools to funding.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Context, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use time::OffsetDateTime;
use uuid::Uuid;

use super::common::{run_step, wait_for_pools, wait_for_pools_state, wait_for_state, Steps};
use super::types::*;
use crate::client::competitions::{CompetitionKind, CreateQueuedCompetition, PoolSummary};
use crate::client::CoordinatorClient;
use crate::crypto::keys::SynthUser;
use crate::trail::EntryTrace;
use coordinator_core::keymeld::pools::{PoolRules, MAX_POOL_PLAYERS};

pub const QUEUED_SPLIT: &str = "queued_split";
pub const QUEUED_ONE_POOL: &str = "queued_one_pool";
pub const QUEUED_TOO_FEW: &str = "queued_too_few";
pub const QUEUED_LEFTOVER_REFUND: &str = "queued_leftover_refund";

/// Whether `scenario` enters a queued competition.
pub fn is_queued(scenario: &str) -> bool {
    [
        QUEUED_SPLIT,
        QUEUED_ONE_POOL,
        QUEUED_TOO_FEW,
        QUEUED_LEFTOVER_REFUND,
    ]
    .contains(&scenario)
}

/// Shortest entry window for a queued scenario: each ticket's key deposit is checked by the
/// enclave before its invoice is shown, and each payment is swapped into an escrow.
const QUEUED_ENTRY_WINDOW_SECS: u64 = 300;
/// Shortest entry window for a split, with dozens of players paying at once.
const SPLIT_ENTRY_WINDOW_SECS: u64 = 600;

/// The states a pool passes through after it forms: it starts with its escrows confirmed.
const POOL_STATES: [&str; 7] = [
    "event_created",
    "entries_submitted",
    "contract_created",
    "signing_complete",
    "funding_broadcasted",
    "funding_confirmed",
    "awaiting_attestation",
];

/// The queue a queued scenario creates, and who enters it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueShape {
    pub rules: PoolRules,
    /// Players who enter completely.
    pub players: usize,
    /// Players who pay and never submit an entry.
    pub abandoned: usize,
}

impl QueueShape {
    /// The shape of `scenario`'s queue, with `config`'s overrides. None for a scenario that
    /// enters a single competition.
    pub fn of(scenario: &str, config: &ScenarioConfig) -> Result<Option<Self>> {
        if !is_queued(scenario) {
            return Ok(None);
        }
        let max = config.max_pool_players.unwrap_or(MAX_POOL_PLAYERS);
        let (min, default_players, abandoned) = match scenario {
            QUEUED_SPLIT => (2, 27, 0),
            QUEUED_ONE_POOL => (2, 5, 0),
            QUEUED_TOO_FEW => (3, 2, 0),
            _ => (2, 3, 1),
        };
        let rules = PoolRules::new(min, max)
            .with_context(|| format!("{scenario} needs pools of {min} to {max} players"))?;
        let players = config.queue_players.unwrap_or(default_players);
        let shape = Self {
            rules,
            players,
            abandoned,
        };
        ensure!(
            (1..=100).contains(&shape.users()),
            "a queued scenario takes 1 to 100 players"
        );
        match scenario {
            QUEUED_SPLIT => ensure!(
                players > max,
                "{scenario} needs more players than a pool holds ({max})"
            ),
            QUEUED_ONE_POOL => ensure!(
                (min..=max).contains(&players),
                "{scenario} needs {min} to {max} players, for one pool"
            ),
            QUEUED_TOO_FEW => ensure!(
                players < min,
                "{scenario} needs fewer players than the smallest pool ({min})"
            ),
            _ => ensure!(
                players >= min,
                "{scenario} needs at least {min} players to form a pool"
            ),
        }
        Ok(Some(shape))
    }

    /// Everyone who pays.
    pub fn users(&self) -> usize {
        self.players + self.abandoned
    }

    /// The pool sizes the complete tickets split into, or None if they are too few for one.
    pub fn sizes(&self) -> Option<Vec<usize>> {
        self.rules.sizes(self.players)
    }

    /// The entry window the queue needs, at least `configured`.
    pub fn entry_window_secs(&self, scenario: &str, configured: u64) -> u64 {
        let floor = if scenario == QUEUED_SPLIT {
            SPLIT_ENTRY_WINDOW_SECS
        } else {
            QUEUED_ENTRY_WINDOW_SECS
        };
        configured.max(floor)
    }
}

pub(super) async fn create_queue(
    client: &CoordinatorClient,
    config: &ScenarioConfig,
    shape: &QueueShape,
) -> Result<Uuid> {
    let now = OffsetDateTime::now_utc();
    let entry_window = time::Duration::seconds(config.entry_window_secs as i64);
    let observation_window = time::Duration::seconds(config.observation_window_secs as i64);
    let queue = CreateQueuedCompetition {
        id: Uuid::now_v7(),
        signing_date: now
            + entry_window
            + observation_window
            + time::Duration::seconds(config.signing_delay_secs as i64),
        start_observation_date: now + entry_window,
        end_observation_date: now + entry_window + observation_window,
        locations: config.stations.clone(),
        number_of_values_per_entry: config.stations.len() * 3,
        entry_fee: config.entry_fee,
        coordinator_fee_basis_points: 1000,
        coordinator_fee_percentage: 10,
        min_players: shape.rules.min_players(),
        max_pool_size: shape.rules.max_players(),
        max_entries: None,
    };
    let created = client.create_queued_competition(&queue).await?;
    ensure!(
        created.kind == CompetitionKind::Queued,
        "the coordinator created a {:?} competition, not a queue",
        created.kind
    );
    Ok(created.id)
}

/// Check a queue is the one asked for, before anyone pays into it.
pub(super) async fn check_queue(
    client: &CoordinatorClient,
    queue_id: &Uuid,
    shape: &QueueShape,
) -> Result<()> {
    let queue = client.get_competition(queue_id).await?;
    ensure!(
        queue.kind == CompetitionKind::Queued,
        "competition {queue_id} is not a queue"
    );
    ensure!(
        queue.pool_rules == Some(shape.rules),
        "the queue's pool rules are {:?}, not {:?}",
        queue.pool_rules,
        shape.rules
    );
    Ok(())
}

/// After every player has entered: follow the queue to its pools, and each pool through its
/// lifecycle, or to its cancellation; then collect the refunds of every paid ticket no pool took.
#[allow(clippy::too_many_arguments)]
pub(super) async fn after_entries(
    client: &CoordinatorClient,
    users: &[SynthUser],
    queue_id: &Uuid,
    config: &ScenarioConfig,
    shape: &QueueShape,
    deadline: OffsetDateTime,
    traces: &[EntryTrace],
    steps: &mut Steps,
) -> std::result::Result<(), Box<StepResult>> {
    // Registration closes at the deadline and kickoff starts at the first block after it.
    let mut kickoff = config.clone();
    kickoff.state_timeout_secs = super::user_behavior::cancellation_budget_secs(
        config.state_timeout_secs,
        deadline,
        OffsetDateTime::now_utc(),
    );
    if shape.sizes().is_none() {
        let (step, ()) = run_step("wait_cancelled", || {
            wait_for_state(client, queue_id, "cancelled", &kickoff)
        })
        .await?;
        steps.push(step);
        let (step, ()) = run_step("verify_no_pools", || async {
            let queue = client.get_competition(queue_id).await?;
            ensure!(
                queue.pools.is_empty() && queue.pools_formed_at.is_none(),
                "a queue too small for a pool formed {} pools",
                queue.pools.len()
            );
            Ok(())
        })
        .await?;
        steps.push(step);
        let paid: Vec<EntryTrace> = traces.iter().filter(|trace| trace.paid).cloned().collect();
        return super::user_behavior::collect_refunds(
            client, users, queue_id, config, &paid, steps,
        )
        .await;
    }

    let (step, pools) = run_step("wait_pools_formed", || {
        wait_for_pools(client, queue_id, &kickoff)
    })
    .await?;
    steps.push(step);
    let (mut step, placed) = run_step("verify_pools", || {
        verify_pools(client, users, queue_id, shape, &pools, traces)
    })
    .await?;
    step.details = Some(serde_json::json!({
        "pools": pools,
        "placed": placed.len(),
    }));
    steps.push(step);
    for state in POOL_STATES {
        let (step, ()) = run_step(&format!("wait_pools_{state}"), || {
            wait_for_pools_state(client, &pools, state, config)
        })
        .await?;
        steps.push(step);
    }

    // A paid ticket without an entry stays on the queue, which refunds it.
    let leftover: Vec<EntryTrace> = traces
        .iter()
        .filter(|trace| trace.paid && trace.ticket_id.is_some_and(|id| !placed.contains(&id)))
        .cloned()
        .collect();
    ensure_step("verify_leftover", leftover.len() == shape.abandoned, || {
        format!(
            "{} paid tickets were left out of the pools; expected {}",
            leftover.len(),
            shape.abandoned
        )
    })?;
    if leftover.is_empty() {
        return Ok(());
    }
    super::user_behavior::collect_refunds(client, users, queue_id, config, &leftover, steps).await
}

fn ensure_step(
    name: &str,
    holds: bool,
    error: impl FnOnce() -> String,
) -> std::result::Result<(), Box<StepResult>> {
    if holds {
        return Ok(());
    }
    Err(Box::new(StepResult {
        name: name.into(),
        status: StepStatus::Failed,
        duration_ms: 0,
        details: None,
        error: Some(error()),
    }))
}

/// Check the queue's pools against the players' own entries, and return the tickets placed.
async fn verify_pools(
    client: &CoordinatorClient,
    users: &[SynthUser],
    queue_id: &Uuid,
    shape: &QueueShape,
    pools: &[PoolSummary],
    traces: &[EntryTrace],
) -> Result<BTreeSet<Uuid>> {
    for pool in pools {
        let child = client.get_competition(&pool.competition_id).await?;
        ensure!(
            child.kind == CompetitionKind::Pool
                && child.parent_id == Some(*queue_id)
                && child.pool_index == Some(pool.pool_index),
            "competition {} is not pool {} of queue {queue_id}",
            pool.competition_id,
            pool.pool_index
        );
    }
    let complete: Vec<Uuid> = traces
        .iter()
        .filter(|trace| trace.paid && trace.entry_submitted)
        .filter_map(|trace| trace.ticket_id)
        .collect();
    ensure!(
        complete.len() == shape.players,
        "{} players entered completely; expected {}",
        complete.len(),
        shape.players
    );
    let placements = placements(client, users, traces).await?;
    check_split(&shape.rules, pools, &complete, &placements)?;
    Ok(placements.into_keys().collect())
}

/// Each submitted entry's competition, by ticket: its pool once pools form. Each player lists
/// their own entries, since only they can.
async fn placements(
    client: &CoordinatorClient,
    users: &[SynthUser],
    traces: &[EntryTrace],
) -> Result<BTreeMap<Uuid, Uuid>> {
    let mut lookups = FuturesUnordered::new();
    for trace in traces.iter().filter(|trace| trace.entry_submitted) {
        let ticket = trace.ticket_id.context("an entry without a ticket")?;
        let user = users
            .iter()
            .find(|user| user.name == trace.user)
            .context("scenario user")?;
        lookups.push(async move {
            let entries = client.list_entries(&user.nostr_keys, None).await?;
            let mut found = entries.iter().filter(|entry| entry.ticket_id == ticket);
            let entry = found
                .next()
                .with_context(|| format!("{}'s entry for ticket {ticket} is gone", user.name))?;
            ensure!(
                found.next().is_none(),
                "ticket {ticket} has more than one entry"
            );
            Ok::<_, anyhow::Error>((ticket, entry.event_id))
        });
    }
    let mut placements = BTreeMap::new();
    while let Some(placement) = lookups.next().await {
        let (ticket, competition) = placement?;
        placements.insert(ticket, competition);
    }
    Ok(placements)
}

/// Check how a queue split its complete tickets: as many pools as the rules give for them, with
/// the sizes the rules give (so they differ by at most one), and every complete ticket's entry in
/// exactly one of them.
pub fn check_split(
    rules: &PoolRules,
    pools: &[PoolSummary],
    complete: &[Uuid],
    placements: &BTreeMap<Uuid, Uuid>,
) -> Result<()> {
    let mut expected = rules
        .sizes(complete.len())
        .with_context(|| format!("{} tickets are too few for a pool", complete.len()))?;
    ensure!(
        pools.len() == expected.len(),
        "{} tickets formed {} pools; expected {}",
        complete.len(),
        pools.len(),
        expected.len()
    );
    let indexes: BTreeSet<u32> = pools.iter().map(|pool| pool.pool_index).collect();
    ensure!(
        indexes.len() == pools.len() && indexes.iter().copied().eq(0..pools.len() as u32),
        "pool indexes {indexes:?} are not 0 to {}",
        pools.len() - 1
    );
    let mut sizes: Vec<usize> = pools.iter().map(|pool| pool.players).collect();
    sizes.sort_unstable();
    expected.sort_unstable();
    ensure!(
        sizes == expected,
        "pool sizes {sizes:?}; expected {expected:?}"
    );
    let mut members: BTreeMap<Uuid, usize> =
        pools.iter().map(|pool| (pool.competition_id, 0)).collect();
    ensure!(members.len() == pools.len(), "two pools share an id");
    let unique: BTreeSet<&Uuid> = complete.iter().collect();
    ensure!(unique.len() == complete.len(), "a ticket entered twice");
    for ticket in complete {
        let pool = placements
            .get(ticket)
            .with_context(|| format!("ticket {ticket}'s entry is in no pool"))?;
        *members
            .get_mut(pool)
            .with_context(|| format!("ticket {ticket}'s entry is in {pool}, not a pool"))? += 1;
    }
    ensure!(
        placements.len() == complete.len(),
        "{} entries were placed for {} complete tickets",
        placements.len(),
        complete.len()
    );
    for pool in pools {
        let placed = members[&pool.competition_id];
        ensure!(
            placed == pool.players,
            "pool {} lists {} players but holds {placed} of the entries",
            pool.pool_index,
            pool.players
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "queued_tests.rs"]
mod tests;
