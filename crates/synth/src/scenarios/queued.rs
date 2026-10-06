//! Queued competitions: players enter without a seat count. When registration closes at the
//! observation start, the coordinator splits the complete tickets into even pools. Each pool is a
//! competition of its own, with its own oracle event, Keymeld session and Arkade batch, and runs
//! the usual lifecycle from escrow confirmation on. A queue too small for one pool is cancelled
//! and every escrow refunded.
//!
//! - `queued_split`: more players than one pool holds, 27 against pools of 2 to 25 by default.
//!   The queue forms `ceil(N / max)` pools whose sizes differ by at most one and which hold every
//!   entry exactly once. Each pool then runs to funding and on to awaiting its attestation.
//! - `queued_one_pool`: the default competition, one pool of up to 20 seats that pays 70% and
//!   30% from ten players and its winner the pot below that; 20 players by default. It takes at
//!   most 20 entries, so it never splits.
//! - `queued_too_few`: 2 players against a minimum of 3. The queue is cancelled and both escrows
//!   are refunded to the players' Lightning Address.
//! - `queued_leftover_refund`: 3 players enter and a fourth pays but never submits an entry. The
//!   pools form from the complete tickets alone and run, and the incomplete ticket is refunded
//!   from the queue.
//!
//! `--queue-players` sets how many players enter completely, and `--max-pool-players` the
//! largest pool, so a split can be tried with fewer payments. The `queue_max_entries` and
//! `places` settings set the queue's entry cap and the places its pools of ten or more pay.
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
use coordinator_core::keymeld::{
    capacity::supported_shape,
    pools::{PoolRules, MAX_POOL_PLAYERS},
};

pub const QUEUED_SPLIT: &str = "queued_split";
pub const QUEUED_ONE_POOL: &str = "queued_one_pool";
pub const QUEUED_TOO_FEW: &str = "queued_too_few";
pub const QUEUED_LEFTOVER_REFUND: &str = "queued_leftover_refund";

/// The default competition's seats: `queued_one_pool` plays as one pool of them.
pub const DEFAULT_SEATS: usize = 20;
/// Complete entries required before the default competition can form a pool.
pub const DEFAULT_MIN_PLAYERS: usize = 3;
/// The places the default competition pays: its winner takes the pot. `places = 2` pays 70%
/// and 30% once ten players entered, where the coordinator allows it.
pub const DEFAULT_PLACES: u32 = 1;

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
    /// The most entries the queue takes; the coordinator's default if None.
    pub max_entries: Option<u32>,
    /// The places its pools of ten or more pay.
    pub places: u32,
}

impl QueueShape {
    /// The shape of `scenario`'s queue, with `config`'s overrides. None for a scenario that
    /// enters a single competition.
    pub fn of(scenario: &str, config: &ScenarioConfig) -> Result<Option<Self>> {
        if !is_queued(scenario) {
            return Ok(None);
        }
        let max = Self::max_pool(scenario, config);
        let one_pool = scenario == QUEUED_ONE_POOL;
        let (min, default_players, abandoned) = match scenario {
            QUEUED_SPLIT => (2, 27, 0),
            QUEUED_ONE_POOL => (DEFAULT_MIN_PLAYERS, DEFAULT_SEATS.min(max), 0),
            QUEUED_TOO_FEW => (3, 2, 0),
            _ => (2, 3, 1),
        };
        let rules = PoolRules::new(min, max)
            .with_context(|| format!("{scenario} needs pools of {min} to {max} players"))?;
        let players = config.queue_players.unwrap_or(default_players);
        // The default competition takes no more entries than its one pool seats.
        let max_entries = config.queue_max_entries.or(one_pool.then_some(max as u32));
        let places = config.places.unwrap_or(DEFAULT_PLACES);
        ensure!(
            supported_shape(max, places as usize),
            "{scenario}: pools of up to {max} cannot pay {places} places; two places need pools \
             of at most 20"
        );
        let shape = Self {
            rules,
            players,
            abandoned,
            max_entries,
            places,
        };
        ensure!(
            (1..=100).contains(&shape.users()),
            "a queued scenario takes 1 to 100 players"
        );
        ensure!(
            max_entries.is_none_or(|cap| cap as usize >= shape.users()),
            "{scenario} plans {} paid tickets but its entry cap is {}",
            shape.users(),
            max_entries.unwrap_or_default()
        );
        match scenario {
            QUEUED_SPLIT => ensure!(
                players > max,
                "{scenario} needs more players than a pool holds ({max})"
            ),
            QUEUED_ONE_POOL => ensure!(
                (min..=max).contains(&players) && max_entries.is_none_or(|cap| cap as usize <= max),
                "{scenario} needs {min} to {max} players, and an entry cap of at most {max}, for \
                 one pool"
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

    /// The largest pool of `scenario`'s queue: `config`'s, or for `queued_one_pool` the default
    /// competition's seats, and for the others as large as the coordinator allows.
    pub fn max_pool(scenario: &str, config: &ScenarioConfig) -> usize {
        config
            .max_pool_players
            .unwrap_or(if scenario == QUEUED_ONE_POOL {
                DEFAULT_SEATS
            } else {
                MAX_POOL_PLAYERS
            })
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
    let times = config.competition_times(OffsetDateTime::now_utc());
    let queue = CreateQueuedCompetition {
        id: times.id,
        signing_date: times.signing,
        start_observation_date: times.start,
        end_observation_date: times.end,
        locations: config.stations.clone(),
        number_of_values_per_entry: config.values_per_entry(),
        entry_fee: config.entry_fee,
        coordinator_fee_basis_points: 300,
        coordinator_fee_percentage: 3,
        min_players: shape.rules.min_players(),
        max_pool_size: shape.rules.max_players(),
        max_entries: shape.max_entries,
        number_of_places_win: shape.places as usize,
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
    if let Some(cap) = shape.max_entries {
        ensure!(
            queue.max_entries == Some(cap),
            "the queue takes {:?} entries, not {cap}",
            queue.max_entries
        );
    }
    let places =
        coordinator_core::keymeld::queued::pool_places(shape.places, shape.rules.max_players());
    ensure!(
        queue
            .event_submission
            .get("number_of_places_win")
            .and_then(serde_json::Value::as_u64)
            == Some(u64::from(places)),
        "the queue's reference event does not pay the requested {places} places"
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
    // Other people's entries can make a queue too small for a pool by synth's count big enough.
    let cancelled = if shape.sizes().is_none() {
        match run_step("wait_cancelled", || {
            wait_for_state(client, queue_id, "cancelled", &kickoff)
        })
        .await
        {
            Ok((step, ())) => {
                steps.push(step);
                true
            }
            Err(mut step) => {
                let formed = client
                    .get_competition(queue_id)
                    .await
                    .is_ok_and(|queue| queue.pools_formed_at.is_some());
                if !formed {
                    return Err(step);
                }
                step.status = StepStatus::Skipped;
                step.error = None;
                step.details = Some(serde_json::json!({ "pools_formed_with_other_players": true }));
                steps.push(*step);
                false
            }
        }
    } else {
        false
    };
    if cancelled {
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
    follow_pools(client, users, config, traces, &pools, &placed, steps).await?;

    // A paid ticket without an entry stays on the queue, which refunds it.
    let leftover: Vec<EntryTrace> = traces
        .iter()
        .filter(|trace| trace.paid && trace.ticket_id.is_some_and(|id| !placed.contains_key(&id)))
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

/// Follow every pool to its attestation. A pool whose kickoff check fails, as it can when network
/// fees rise after the run drew its players, is cancelled and refunds its entries: synth's players'
/// refunds are collected at the pool, and the other pools are followed on. Any other failure fails
/// the run.
async fn follow_pools(
    client: &CoordinatorClient,
    users: &[SynthUser],
    config: &ScenarioConfig,
    traces: &[EntryTrace],
    pools: &[PoolSummary],
    placed: &BTreeMap<Uuid, Uuid>,
    steps: &mut Steps,
) -> std::result::Result<(), Box<StepResult>> {
    let mut active = pools.to_vec();
    for state in POOL_STATES {
        while !active.is_empty() {
            let failed = match run_step(&format!("wait_pools_{state}"), || {
                wait_for_pools_state(client, &active, state, config)
            })
            .await
            {
                Ok((step, ())) => {
                    steps.push(step);
                    break;
                }
                Err(failed) => failed,
            };
            let mut dropped = Vec::new();
            for pool in &active {
                let Ok(competition) = client.get_competition(&pool.competition_id).await else {
                    continue;
                };
                if competition.failed_kickoff() {
                    dropped.push((pool.clone(), competition.kickoff_check));
                }
            }
            if dropped.is_empty() {
                return Err(failed);
            }
            for (pool, check) in dropped {
                active.retain(|other| other.competition_id != pool.competition_id);
                steps.push(StepResult {
                    name: format!("pool_{}_kickoff_failed", pool.pool_index),
                    status: StepStatus::Passed,
                    duration_ms: failed.duration_ms,
                    details: Some(serde_json::json!({
                        "pool": pool.competition_id,
                        "kickoff_check": check,
                    })),
                    error: None,
                });
                let refunded: Vec<EntryTrace> = traces
                    .iter()
                    .filter(|trace| {
                        trace.paid
                            && trace
                                .ticket_id
                                .is_some_and(|id| placed.get(&id) == Some(&pool.competition_id))
                    })
                    .cloned()
                    .collect();
                super::user_behavior::collect_refunds(
                    client,
                    users,
                    &pool.competition_id,
                    config,
                    &refunded,
                    steps,
                )
                .await?;
            }
        }
    }
    Ok(())
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

/// Check the queue's pools against the players' own entries, and return each placed ticket's
/// pool.
async fn verify_pools(
    client: &CoordinatorClient,
    users: &[SynthUser],
    queue_id: &Uuid,
    shape: &QueueShape,
    pools: &[PoolSummary],
    traces: &[EntryTrace],
) -> Result<BTreeMap<Uuid, Uuid>> {
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
    let pool_ids: Vec<_> = pools.iter().map(|pool| pool.competition_id).collect();
    let placements = placements(client, users, traces, &pool_ids).await?;
    check_split(&shape.rules, pools, &complete, &placements)?;
    Ok(placements)
}

/// Each submitted entry's competition, by ticket: its pool once pools form. Each player lists
/// their own entries, since only they can.
async fn placements(
    client: &CoordinatorClient,
    users: &[SynthUser],
    traces: &[EntryTrace],
    pool_ids: &[Uuid],
) -> Result<BTreeMap<Uuid, Uuid>> {
    let mut lookups = FuturesUnordered::new();
    for trace in traces.iter().filter(|trace| trace.entry_submitted) {
        let ticket = trace.ticket_id.context("an entry without a ticket")?;
        let user = users
            .iter()
            .find(|user| user.name == trace.user)
            .context("scenario user")?;
        lookups.push(async move {
            let entries = client.list_entries_in(&user.nostr_keys, pool_ids).await?;
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
/// the sizes the rules give (so they differ by at most one), and every one of synth's complete
/// tickets (`complete`) in exactly one of them. Other people's entries can share the pools, so the
/// split is of every entry the pools list, synth's and theirs.
pub fn check_split(
    rules: &PoolRules,
    pools: &[PoolSummary],
    complete: &[Uuid],
    placements: &BTreeMap<Uuid, Uuid>,
) -> Result<()> {
    let total: usize = pools.iter().map(|pool| pool.players).sum();
    ensure!(
        total >= complete.len(),
        "the pools list {total} players, fewer than synth's {} complete tickets",
        complete.len()
    );
    let mut expected = rules
        .sizes(total)
        .with_context(|| format!("{total} tickets are too few for a pool"))?;
    ensure!(
        pools.len() == expected.len(),
        "{total} tickets formed {} pools; expected {}",
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
            placed <= pool.players,
            "pool {} lists {} players but holds {placed} of synth's entries",
            pool.pool_index,
            pool.players
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "queued_tests.rs"]
mod tests;
