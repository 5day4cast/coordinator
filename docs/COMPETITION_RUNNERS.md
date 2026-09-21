# Competition runners

Status: proposal, 2026-09-21.

Today one watcher loop drives every competition.
Each tick, `competition_handler` loads the active competitions and runs each through `process_status` in turn, then sleeps for `sync_interval_secs`.
This proposal gives each competition its own task, woken by the events it waits for.
The typestate machine stays as it is; only the driver changes.

## Why

- **One slow step blocks every competition.**
  An Arkade kickoff waits for a batch, which takes one or two 60-second sessions.
  The oracle client retries for up to 10 minutes.
  Keymeld signing grows with the pool.
  Each of these holds up every other competition for its whole length.
- **Progress waits for the tick.**
  A competition that is ready to move, because its last ticket was paid or its last registration arrived, waits for the next tick.
  Only transitions marked immediate chain within one tick today.
- **The queue design needs concurrent pools.**
  Several pools kicking off together should register in the same Arkade batch, not one after another.

## Goals

- Competitions progress independently, and a slow or failing step affects only its own competition.
- A competition moves as soon as the event it waits for happens: a paid ticket, a registration, a funded escrow, a new block, an attestation time.
- The typestate machine is unchanged: the same states, transitions, and `process_status`.
- At most one task drives a competition at a time, so a competition has a single writer.
- A crash in one competition's step does not stop the others, and the competition is picked up again.

## Design

### Runner registry

A `CompetitionRunners` registry holds `DashMap<Uuid, Runner>`.
A `Runner` holds the competition's wake signal, an `Arc<Notify>`, and its task handle.

- `ensure(id)` starts a runner unless one is running.
  It uses the map's entry API, so concurrent callers start at most one runner per competition.
  A runner whose task has finished, including by panic, is replaced.
- `wake(id)` calls `ensure(id)`, then `notify_one` on the runner's signal.
  `Notify` keeps a permit, so a wake that arrives during a step is not lost; the runner runs again straight after.

Runners are spawned on the service's `TaskTracker` and watch its `CancellationToken`.

### The runner loop

```rust
loop {
    // The store is the source of truth; reload it every step.
    let competition = store.get_competition(id).await?;
    if competition.is_finished() {
        break; // cleanup stays with the sweep, below
    }
    let status: CompetitionStatus = competition.into();
    let next = coordinator.process_status(status).await; // unchanged
    let wait = next.wait(now);
    store.update_competitions(vec![next.into_competition()]).await?;
    match wait {
        Wait::Now => continue,
        Wait::Until(deadline) => select! {
            _ = wake.notified() => {}
            _ = sleep_until(deadline) => {}
            _ = cancel.cancelled() => break,
        },
    }
}
```

A step always finishes, even at shutdown.
An Arkade batch or a broadcast in progress must record its result, so cancellation is checked only between steps.

### Wait policy per state

Each state says what it waits for, next to its transitions:

```rust
pub enum Wait {
    /// Run the next step now; replaces `is_immediate_transition`.
    Now,
    /// Sleep until a wake, or the deadline at the latest.
    Until(OffsetDateTime),
}

pub trait Progress {
    fn wait(&self, now: OffsetDateTime) -> Wait;
}
```

`CompetitionStatus::wait` dispatches to each state's `Progress` implementation, as `competition_id` does today.
For example:

| State | Woken by | Deadline |
| --- | --- | --- |
| `CollectingEntries`, `AwaitingEscrow` | a paid ticket, a funded escrow | entry close |
| `ContractCreated` | a Keymeld registration | a registration poll interval |
| `AwaitingSignatures` | (runs now) | a retry backoff after a failed batch |
| `FundingBroadcasted` and the other broadcast states | a new block | a block interval |
| `AwaitingAttestation` | nothing | the event's attestation time, then a retry interval |
| `EscrowConfirmed`, `EventCreated`, `EntriesSubmitted`, `SigningComplete`, `FundingConfirmed`, `FundingSettled` | (run now) | — |

A step that failed waits a backoff that grows per competition (for example 5 s, doubling to 5 minutes), kept in the runner's local state.
The runner also caps consecutive `Wait::Now` steps, as `MAX_CONSECUTIVE_STATES` does today, then yields.

### Event sources

Each event source calls `runners.wake(competition_id)` after committing its write:

- The invoice watcher and subscriber, when a ticket is paid.
- `check_ark_swaps`, when an escrow is funded.
- The entry and Keymeld registration routes.
- The bitcoin watcher, when a new block arrives.
  It wakes the runners in chain-waiting states.
  Alternatively, it publishes the tip on a `watch` channel, which those states select on as well.

### The sweep

The existing `CompetitionWatcher` becomes a slow sweep, for example every 60 seconds:

- It calls `ensure` for every active competition.
  This starts runners after a restart, and after a runner crashed.
- It keeps the work for dead competitions: `release_held_invoices` and `reclaim_escrows`.

The sweep is the safety net; events are the fast path.

### Shared resources

- **Wallet funding.**
  LND's `FundPsbt` leases the inputs it selects, so concurrent funding cannot pick the same coins.
  The reservation renewal stays as it is.
- **Keymeld.**
  The confidential service already takes a lock per keygen session.
- **SQLite.**
  Writes already go through one write queue.
- **Arkade.**
  Concurrent kickoffs register separate intents, which can land in the same batch.
- **Load.**
  A semaphore bounds how many steps run at once, so many competitions do not hit the oracle or LND together.
  A step holds its permit only while it runs.

## Testing

- A competition stuck in a slow step, such as an oracle mock that sleeps, does not delay another competition.
- A wake during a step is not lost.
- Concurrent wakes start one runner.
- A runner that panics is restarted by the sweep, and other runners keep going.
- Shutdown lets a step in progress finish and record its result.

## Rollout

1. Add the registry and runner, and have the watcher call `ensure` instead of processing inline.
   Keep the current interval as the deadline for every state.
2. Add wakes from the event sources.
3. Add the per-state wait policies, then lengthen the sweep interval.

## Open questions

- Whether terminal states should run their own cleanup in the runner, instead of in the sweep.
- Two coordinator processes, as in a blue/green deploy, would each run a runner per competition.
  If they can overlap, each competition needs a database lease, for example `runner_id` and `lease_expires_at`.
