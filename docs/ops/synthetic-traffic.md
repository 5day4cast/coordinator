# Synthetic entry traffic

Use synth to test staggered players, abandoned entries, and duplicate or late submissions. Each run records its scenario, seed, timing bounds, and resolved player plan before competition creation.

These scenarios can pay real entries from the configured node. The remote CLI asks for confirmation unless `--yes` is supplied.

## Authorize operator writes

Browser writes require an allowed Origin and the dashboard's CSRF nonce. The dashboard adds the nonce automatically.
Cross-origin forms and browser requests without the nonce receive HTTP 403 before a run or payment starts.

Configure the public origin when a reverse proxy changes the Host header. Include the public port:

```toml
[server]
allowed_origins = ["https://synth.lab.tee8z.fyi:9443"]
# Optional: needed only for programmatic writes, including the remote CLI.
operator_token_file = "/run/secrets/synth-operator-token"
```

Without `allowed_origins`, synth compares the Origin with the request Host. It ignores forwarded headers.
The operator network must continue to restrict dashboard access. The CSRF nonce does not replace that access restriction.

Use a randomly generated token of at least 32 characters. Set `SYNTH_OPERATOR_TOKEN_FILE` or pass `--operator-token-file` to the remote CLI.
Programmatic POST requests require `Authorization: Bearer <token>` and must omit browser Origin and Fetch Metadata headers.
Without `operator_token_file`, programmatic writes are disabled. Browser controls and scheduled runs remain available.

## Limit ticket payments

`defaults.max_ticket_fees_sats` limits the total coordinator and network fees above the configured `entry_fee`. Its default is 1,000 sats.
For example, a 5,000-sat stake can pay a ticket of at most 6,000 sats. LND routing fees retain their separate configured cap.

Before payment, synth verifies the signed BOLT11 amount, hash, expiry, and paying-node network. It also checks the planned stake and fee cap.
Amountless invoices, fractional-sat mismatches, and arithmetic overflows fail before payment.
The entry trace stores the exact canonical invoice and validated values before LND receives the payment request.

## Recover uncertain rebalances

Scheduled and manual rebalances share one admission guard. A concurrent trigger fails before reading balances or creating invoices.
Run one synth process per database. The guard coordinates tasks within that process; it is not a distributed lease.

Each Lightning rebalance saves its invoice and payment hash before sending. A pending intent prevents replacement invoices for that leg across restarts.
The dashboard shows an uncertain result when the send response cannot establish the outcome.

On the next trigger, synth queries LND for the saved hash before reading balances:

| LND result | Action |
| --- | --- |
| Succeeded with the expected hash and amount | Record the transfer as moved. |
| Definitively failed | Record failure and release the intent. |
| Never initiated, with the invoice expired | Record failure and release the intent. |
| In flight, unavailable, or not initiated before expiry | Keep the intent pending and issue no replacement. |

A reconciliation pass does not also start a replacement on that leg. Subsequent passes read balances again before calculating another payment.
Keep LND payment history available until pending intents resolve. Do not delete pending database rows to retry an uncertain payment.
This recovery protocol covers Lightning channel rebalances. On-chain top-ups retain their existing confirmation handling.

## Configure entry timing

```toml
[defaults]
users = 3
max_ticket_fees_sats = 1000
entry_window_secs = 1200
observation_windows_secs = [7200, 10800, 14400, 600]

[defaults.entry_timing]
arrival = { min_secs = 0, max_secs = 90 }
before_payment = { min_secs = 5, max_secs = 60 }
before_submit = { min_secs = 10, max_secs = 120 }
deadline_margin_secs = 60

[scheduler]
enabled = true
interval_secs = 3600
scenarios = [
  "full_lifecycle",
  "abandoned_unpaid",
  "paid_abandonment",
  "duplicate_submission",
  "late_submission",
  "escrow_refund",
]
```

All ranges are inclusive seconds. Each player receives separate samples from the run's seeded random generator.

| Field | Meaning |
| --- | --- |
| `arrival` | Delay from competition creation before requesting a ticket. |
| `before_payment` | Wait after ticket registration, before payment. |
| `before_submit` | Wait after payment, before entry submission. |
| `deadline_margin_secs` | Time reserved before the entry deadline. Payment also reserves the backend's 60-second invoice cutoff. |

The maximum arrival and payment waits must fit before the invoice cutoff. All three maximum waits and the margin must fit inside the entry window.

Execution checks the actual competition deadline before payment and submission. Network delays can still cause a run to fail its deadline checks.

Entry windows must be 61–86,400 seconds. Observation windows must be 1–604,800 seconds.

The unpaid ticket reservation lasts 600 seconds. Both abandonment cases extend shorter entry windows before recording the run.

The minimum extended window is `600 + maximum arrival + maximum payment wait + maximum submission wait + max(margin, 60) + 120` seconds.

The example's 1,200-second entry window exceeds its 1,050-second minimum. The saved configuration contains the effective entry window.

## Select cases

| Case | Expected behavior |
| --- | --- |
| `full_lifecycle` | All players register, pay, and submit. The money tracker follows settlement afterward. |
| `abandoned_unpaid` | One player leaves without paying. A replacement reuses the seat after its reservation expires. |
| `paid_abandonment` | One player pays but does not submit. Its paid seat remains unavailable after the unpaid reservation period. Cancellation must refund paid tickets. |
| `duplicate_submission` | One player repeats the same entry submission. The duplicate must not create another entry. |
| `late_submission` | One player pays, then submits after entry closes. The submission must fail; cancellation must refund paid tickets. |
| `escrow_refund` | An intentionally unfilled competition cancels and refunds its paid tickets. |
| `queued_split` | 27 players enter a queued competition with pools of 2 to 25. When registration closes it must form `ceil(27 / 25)` pools whose sizes differ by at most one and which hold every entry exactly once. Each pool is then followed to funding and on to awaiting its attestation. |
| `queued_one_pool` | 5 players enter a queued competition. It must form one pool of all 5, which is followed like a split's pools. |
| `queued_too_few` | 2 players enter a queued competition that needs 3 per pool. It must be cancelled without pools and refund both escrows to the players' Lightning Address. |
| `queued_leftover_refund` | 4 players pay into a queued competition and one never submits an entry. The pool must form from the 3 complete tickets and run, and the incomplete ticket must be refunded from the queue. |

The planner chooses one exceptional player for each named behavior case. Other players follow the complete-entry plan.

Cases that require escrow refunds need a configured payer and a Lightning Address. These prerequisites are checked before competition creation.

Queued cases need both too, since every queued entry waits in an Arkade escrow and names the Lightning Address it is refunded to. Each queued case sets its own player count; `--queue-players` changes how many players enter completely, and `--max-pool-players` the largest pool. For example, `--queue-players 5 --max-pool-players 3` splits into pools of 3 and 2 with five payments instead of 27. Queued cases keep an entry window of at least 300 seconds, and 600 for `queued_split`.

A pool whose kickoff batch fails is not covered yet. It needs an operator test hook on the coordinator that fails one pool's batch.

The money tracker follows the queue and each child pool. Authenticated entry lists assign submitted tickets to their pools. Saved assignments survive coordinator outages and synth restarts. Unassigned paid tickets remain on the queue for refund tracking. Conflicting or missing placement evidence prevents a successful money verdict.

Each pool keeps its own settlement, payout receipts, refunds, and transaction evidence. The run succeeds only after every tracked payment reaches a verified final outcome. Run details and exports show pool identities and separate ledgers.

Tracking deadlines use the latest observation, signing, contract-expiry, and escrow-refund terms, plus the configured grace period. The parent keeps the latest child deadline. Expected settlement can therefore continue beyond eight hours. Unverified trails remain eligible for later checks. The database migration reopens historical unverified and timed-out trails.

Scheduled runs cycle through every scenario at one observation duration, then advance to the next duration. Every configured case receives every configured duration.

The scheduler waits `interval_secs` after each run finishes. Settlement tracking continues separately.

## Start or replay a run

Start a manual case with explicit bounds:

```sh
synth run duplicate-submission \
  --url "$SYNTH_URL" --operator-token-file "$SYNTH_OPERATOR_TOKEN_FILE" \
  --users 3 --seed 42 \
  --entry-window-secs 1200 --observation-window-secs 7200 \
  --arrival-min-secs 0 --arrival-max-secs 90 \
  --before-payment-min-secs 5 --before-payment-max-secs 60 \
  --before-submit-min-secs 10 --before-submit-max-secs 120 \
  --deadline-margin-secs 60 --wait --timeout 8h
```

Use the recorded seed, scenario, users, timing bounds, and observation duration to replay a plan. The same inputs reproduce player waits and weather picks.

A replay creates new identifiers and timestamps. Network timing and weather outcomes can differ.

The direct operator command `coord synth run` accepts the same timing flags. The dashboard lists all supported cases from the runner's scenario registry.

## Preserve older configurations

Without `scheduler.scenarios`, synth uses the existing `scheduler.scenario` value. Its default remains `full_lifecycle`.

Without timing fields, all waits and the configured margin remain zero. Existing single-scenario schedules retain observation-duration rotation.

The scalar `defaults.observation_window_secs` remains supported. The plural field accepts either a scalar or a list.

An omitted seed is generated before the run is saved. Set `defaults.seed` for a reproducible scheduled sequence; each successive run increments that seed.
The sequence restarts when synth restarts.

## Check recorded results

The run's `config_json` records `planned_scenario`, `seed`, `entry_plan`, and its resolved duration. The executor loads this saved configuration.

Each entry trace records planned waits, elapsed waits, ticket details, and whether payment started. The runner saves payment intent before calling the payer.

If a progress write or acknowledgement fails, that actor stops before its next payment. Concurrent actors already paying are allowed to finish and record their results.

A failed run does not prove that no payment occurred. Inspect its entry traces and money trail before any retry.

An expected rejection must have the specific entry error being tested. Authentication failures, transport failures, and server errors fail the scenario.

## Read monitoring metrics

`/metrics` registers collectors at server startup. Run counters and duration histograms describe scenarios completed by the current process; they reset on restart.

`synth_competition_lifecycle_healthy` reads persisted settlement evidence. It is `1` for the latest assessed full lifecycle with verified payouts, `0` for a failed or stuck outcome, and `NaN` when evidence is absent or unverified. A newer run still within its settlement deadline does not erase an earlier assessed result. An overdue unresolved run does. The gauge also becomes `NaN` if no full-lifecycle money trail has refreshed for 30 minutes.

`synth_last_successful_run_timestamp` is the persisted time when payouts were first verified, not the earlier time when scenario steps ended. Repeated refreshes do not advance it. Read this timestamp with the health gauge to distinguish recent success from old evidence. Existing verified rows migrate using their last stored trail refresh time because earlier verification timestamps were not retained.

The dashboard's Last Run card reads the latest completed run and its money verdict from the database. Its link and steps belong to that same run, including after a restart or while a newer run is in progress.
