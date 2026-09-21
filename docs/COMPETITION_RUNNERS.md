# Competition runners

Each competition has its own task, a runner, woken by the events it waits for.
Two coordinators can run at once against one database, as during a blue/green deploy, so competitions never stop for a deploy.
The typestate machine is unchanged; only the driver changed.

Code: `crates/coordinator/src/domain/competitions/runners.rs`, `competition_steps.rs`, and `lease_store.rs`.

## Why

One watcher loop used to drive every competition in turn, then sleep for `sync_interval_secs`.

- **One slow step blocked every competition.**
  An Arkade kickoff waits for a batch, one or two 60-second sessions.
  The oracle client retries for up to 10 minutes.
  Each held up every other competition for its whole length.
- **Progress waited for the tick.**
  A competition ready to move, because its last ticket was paid, waited for the next tick.
- **Deploys stopped everything.**
  The old blue/green switch stopped the active slot before the standby started, so no coordinator ran in between.

## Runners

`CompetitionRunners` keeps a `DashMap` from competition ID to its runner's task.
`ensure` starts a runner through the map's entry API, so there is at most one per competition in a process.
A finished or panicked runner is replaced.

A runner repeats one step at a time:

1. Take the competition's lease, or wait for it, as below.
2. Run one step: `Coordinator::advance_competition`, which calls the unchanged `process_status` and saves the result.
3. Wait:
   - **Now:** the state changed to one that moves on at once (`is_immediate_transition`).
     After `max_immediate_steps` in a row, it waits one idle period instead.
   - **Until a time:** the state's `next_check`, or the next retry after a failure.
     A wake ends the wait early.
     Failed steps back off from 5 seconds, doubling, up to 5 minutes.
4. On `Finished`, meaning cancelled, completed, or its expiry broadcast, release the lease and stop.

A step always completes, even at shutdown, since an Arkade batch or a broadcast in flight must record its result.
Cancellation is checked between steps.
A semaphore bounds how many steps run at once (`max_concurrent_steps`).

### When a state is checked again

`CompetitionStatus::next_check(now, idle)` dispatches to states with their own policy; the rest wait at most `sync_interval_secs`.

| State | Next check without a wake |
| --- | --- |
| `AwaitingAttestation` | When the observation window closes, or the contract expires if sooner |
| Every other state | `sync_interval_secs` |
| A failed competition | An hour after it failed, when it is cancelled so cleanup can run |

### Wakes

`CompetitionWakes::wake(id)` notifies the competition's runner, and starts one if none is running.
A step sees every change woken before it began; a wake during a step makes the runner step again straight after.
Callers wake a competition after committing the change it should see:

- A competition is created.
- An entry is added.
- A ticket is paid: the invoice watcher, the invoice subscriber, and `handle_invoice_accepted`.
- An Arkade escrow is funded: `check_ark_swaps`.

Each wake is also recorded in `competition_wakes`.
Every coordinator polls that table each second, `wake_poll`, and wakes its own runners for other coordinators' wakes.
So a ticket paid through one coordinator moves a competition the other drives within about a second.
The sweep prunes wakes older than an hour.

### The sweep

Every `sweep_interval_secs` the sweep:

- Starts a runner for every active competition, after a restart or a crash.
- Releases held invoices and reclaims escrows of dead competitions.
  One coordinator at a time does this, under the `worker:competition-cleanup` lease.

## Leases

A competition is driven only by the coordinator holding its lease.
The `leases` table has one row per resource: `competition:<id>` or `worker:<name>`.

| Column | Meaning |
| --- | --- |
| `holder` | The process: `instance_name` (or `name`) plus a fresh UUID per start |
| `token` | Rises each time a different holder takes the lease |
| `expires_at` | UNIX milliseconds; the holder renews every third of `lease_ttl_secs` |

- **Taking a lease** is one upsert: it succeeds if the lease is free, expired, or already this holder's.
- **Fencing:** `update_competition_fenced` saves a competition only while the saver's holder and token are current, checked in the same `UPDATE`.
  A coordinator that lost the lease mid-step cannot overwrite the new holder's state.
- **Renewal during work:** `while_leased` renews while a step runs.
  If renewal finds the lease taken, the step still completes, its fenced save fails, and the runner drops the competition.
- **Handover:**
  - On shutdown, each runner finishes its step and releases its lease.
    The other coordinator's runner, retrying every 2 seconds, takes it.
  - After a crash, the leases expire after `lease_ttl_secs`, 30 seconds by default.

### Singleton workers

Some background workers have side effects that must happen in one process at a time.
Each tick runs under a `worker:<name>` lease (`WorkerLeases::tick`), which the worker keeps between ticks and releases when it stops.

| Worker | Why it is a singleton |
| --- | --- |
| `payout-watcher` | Sends winners' payments |
| `automatic-payouts` | Prepares winners' claims with Keymeld |
| `invoice-watcher` | Settles hold invoices and broadcasts escrow transactions |
| `escrow-swaps` | Records funded Arkade escrows |

The LND invoice and payment subscriptions, and the block watcher, only record idempotent state, so both coordinators run them.

## Running two coordinators

Both coordinators must share one SQLite database:

- **One host, one file.**
  Both processes open the same database files on a local filesystem, in WAL mode.
  SQLite's locks do not work over network filesystems.
  On lab-apps the two slots bind-mount the same data directory.
- **Additive migrations during an overlap.**
  The new version migrates the shared database while the old one still runs.
  A release may only add tables, columns, and indexes.
  Dropping or renaming waits for a later release, once no old version runs.
- **One replicator.**
  Litestream, or any backup, runs once for the database, outside the slots.
- **Traffic.**
  Both coordinators serve the API.
  The proxy moves traffic to the new slot once it is healthy, then the old slot stops.
- **Wakes cross over once a second.**
  An event handled by one coordinator reaches a competition the other drives at the other's next poll.

A deploy then runs:

1. Start the new slot. It migrates, then its runners wait on the leases the old slot holds.
2. Move traffic to the new slot.
3. Stop the old slot. Its runners finish their steps and release their leases, and the new slot takes over within seconds.

## Settings

In `[coordinator_settings]`:

| Setting | Default | Use |
| --- | --- | --- |
| `sync_interval_secs` | 15 | The longest a competition waits without a wake |
| `sweep_interval_secs` | 60 | How often the sweep runs |
| `lease_ttl_secs` | 30 | How long a lease lasts without renewal |
| `max_concurrent_steps` | 8 | Steps running at once |
| `instance_name` | `name` | Names this process in leases and logs, such as its slot |

## Tests

`runners_tests.rs` runs the runners against a real SQLite database with a stepper that records every step:

- A slow competition does not hold up another.
- A wake during a step runs it again.
- Concurrent wakes start one runner.
- Two coordinators share competitions without ever stepping one at once, and hand over on shutdown.
- A crashed coordinator's competitions move when its lease expires.
- A panicking runner is restarted by the sweep.
- Shutdown lets a step finish and releases the lease.
- A coordinator that lost the lease cannot save the competition.
- A singleton worker runs in one coordinator until it stops.
- A wake handled by one coordinator reaches the coordinator driving the competition.

## Not yet

- The lab-apps deployment: two coordinator slots sharing one data directory, with the proxy switching between them.
