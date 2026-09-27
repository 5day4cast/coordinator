# Synthetic entry traffic

Use synth to test staggered players, abandoned entries, and duplicate or late submissions. Each run records its scenario, seed, timing bounds, and resolved player plan before competition creation.

These scenarios can pay real entries from the configured node. The remote CLI asks for confirmation unless `--yes` is supplied.

## Configure entry timing

```toml
[defaults]
users = 3
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

The planner chooses one exceptional player for each named behavior case. Other players follow the complete-entry plan.

Cases that require escrow refunds need a configured payer and a Lightning Address. These prerequisites are checked before competition creation.

Scheduled runs cycle through every scenario at one observation duration, then advance to the next duration. Every configured case receives every configured duration.

The scheduler waits `interval_secs` after each run finishes. Settlement tracking continues separately.

## Start or replay a run

Start a manual case with explicit bounds:

```sh
synth run duplicate-submission \
  --url "$SYNTH_URL" --users 3 --seed 42 \
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
