# Synthetic entry traffic

Use synth to test staggered players, abandoned entries, and duplicate or late submissions. Each run records its scenario, seed, timing bounds, and resolved player plan before competition creation.

These scenarios can pay real entries from the configured node. The remote CLI asks for confirmation unless `--yes` is supplied.

## Control scenarios

Use **Scenarios** on the Synth dashboard to pause or enable each scenario. The table shows its configured lanes or **Manual only**, its recent results, and its last run. Changes are stored in Synth's database and survive service restarts.

Pause stops new scheduled and manual runs of that scenario. Runs already recorded continue, including entry payments, payout tracking, and refunds. Enabling a scenario permits the next scheduled run; it does not start one immediately or add a manual-only scenario to the scheduler. Browser controls use the dashboard's existing HTMX script and operator-write protection.

## Select eligible stations

Every new Synth competition checks Oracle's `/stations/eligible` endpoint. Scheduled lanes and manual scenario triggers rank eligible stations by forecast weather. A lane without a `picker` block uses weather selection. `mode = "fixed"` or `candidates = "configured"` restricts selection to configured stations that are also eligible.

The **Create competition** form preserves the operator's station choices and checks each against the eligible list. Each selected station must have a forecast in the requested observation window. Missing eligibility, an empty list, or insufficient forecast data stops creation before entry payments. Synth does not substitute stations from the general station directory. Successful eligibility responses are cached for up to ten minutes; failed requests are retried on the next attempt.

The default eligibility lookback is three days. Eligibility describes recent coverage; it cannot guarantee future reports. Settlement still waits for the Oracle's observation and coverage checks. Use the saved weather selection and the competition's Oracle event to investigate a delay.

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
| `stress_full_pool` | Players, 25 by default, arrive in a burst for a competition at the pool cap of 25 seats. See [Stress a full pool](#stress-a-full-pool). |

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

## Create a competition by hand

The dashboard's "Create a competition" form, above the live panels, makes one competition the way the scenarios make theirs. It is recorded as a `manual_competition` run, so the money tracker follows it like any other run.

| Field | Default | Rule |
| --- | --- | --- |
| Stations | All configured stations | 1 to 50 distinct IDs; add others as free text, separated by commas or spaces. |
| Entry fee | `defaults.entry_fee` | At least 1 sat. |
| Entry window | 1 hour | 61 seconds to 24 hours. |
| Observation window | The first configured window the oracle attests | One of `defaults.observation_windows_secs`, and 24 hours to 7 days or a 12-hour UTC half. A 12-hour window starts at the next 00:00 or 12:00 UTC after the entry window. |
| Seats | The pool cap, 25 | 2 to 25. |
| Listed | Unlisted | Listed puts the oracle event on the public list. |
| Fill with synth players | 0 | Up to the seats. |

With 0 players the competition is created and left open for people. With more, synth's players arrive spread over the entry window, using the configured entry timing, and pay from the configured node.
They are refused before anything is recorded while the coordinator pauses entries for network fees or the Arkade server.

The form waits up to a minute for the coordinator, then shows the competition's link and id, or the coordinator's refusal in its own words.
The CLI posts the same form:

```sh
synth run manual-competition \
  --url "$SYNTH_URL" --operator-token-file "$SYNTH_OPERATOR_TOKEN_FILE" \
  --stations KDEN,KJFK --entry-window 1h --window 1d [--players 5] \
  [--entry-fee 5000] [--seats 25] [--listed] --yes
```

No schedule or lane runs `manual_competition`.

## Stress a full pool

`stress_full_pool` pushes one competition to its upper bound. It creates a competition with as many seats as the coordinator gives one pool (25), unlisted unless the lane sets `unlisted = false`. Then `users` players arrive at random within `burst_window_secs` of its creation.
At most `concurrency` players at a time request a ticket, register, pay and submit, with no wait between steps. A refused player tries again after a short backoff, up to `retries` times.

Each player's step records how long each of its steps took and every refusal, with the HTTP status and the coordinator's message. The `stress_entries` step counts players admitted and refused, by reason, and names the slowest step.
The run passes when at least `min_admitted` players got in, the competition kicked off with all of them, and its money settled like `full_lifecycle`. Otherwise `stress_result` fails with the counts.

A player who has started paying never pays again, and a paid ticket holds its seat. The run pays for at most 25 tickets of at most `entry_fee + max_ticket_fees_sats` each, plus LND routing fees.
With the defaults' 1,000-sat entry fee and 1,000-sat fee cap, that is 50,000 sats.
Before creating anything, the run refuses to start if the coordinator's fee for a ticket now plus its network fee is above `max_ticket_fees_sats`. Synth's LND client reads no wallet balance, so the node's balance is not checked; keep the payer funded for the maximum.

It is off by default: no default schedule includes it. Run it from the dashboard or `synth run stress-full-pool`, or give it a lane of its own:

```toml
[[scheduler.lanes]]
name = "stress"
scenarios = ["stress_full_pool"]
interval_secs = 604800
entry_window_secs = 3600
observation_windows_secs = [86400]
# Set false to list its competitions on the oracle; only a lane of stress runs may.
unlisted = true

[scheduler.lanes.stress]
users = 25
burst_window_secs = 60
concurrency = 10
retries = 3
min_admitted = 25
```

The burst must end at least 180 seconds before entries close. Every setting is optional; the values shown are the defaults.

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

Stress runs add `synth_stress_step_seconds{step}` for each ticket, registration, payment and submission, refused or not. They also add `synth_stress_admitted_total` and `synth_stress_refused_total{reason}`. The reason is one of `paused`, `full`, `ticket`, `payment`, `registration`, `submission` or `other`.

The dashboard's Last Run card reads the latest completed run and its money verdict from the database. Its link and steps belong to that same run, including after a restart or while a newer run is in progress.
