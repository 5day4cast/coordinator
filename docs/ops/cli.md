# Driving competitions and synth runs from a terminal

Two thin command-line clients, for an operator or a script:

- `coordinator admin …` talks to the coordinator's operator listener (`admin_settings.listen_addr`).
- `synth run …` and `synth runs …` talk to a running synth's HTTP API.

Neither does anything its HTTP API does not: they are the same requests the admin page and the
synth dashboard make. Hosts, ports and paths below are placeholders.

## `coordinator admin`

```sh
export COORDINATOR_ADMIN_URL=http://<operator-host>:9991      # or --url
export COORDINATOR_ADMIN_TOKEN_FILE=/path/to/admin_token       # or --token-file
```

The token is read from the file and sent as `Authorization: Bearer …`. It is never taken as a
command-line value, where the process list would show it. Leave the token file unset only for a
listener started with `dangerous_allow_unauthenticated` for development.

Every command prints a table, or JSON with `--json`.

```sh
# Competitions, newest first; filter by state (repeatable), or `active` for any unfinished one.
coordinator admin competitions list
coordinator admin competitions list --state active
coordinator admin competitions list --state awaiting_attestation --state failed --json

# One competition: terms, state, settlement milestones, the last errors kept on it, and how many
# of its funded Arkade escrows have been refunded.
coordinator admin competitions show <competition-id>

# Create one. The fields and defaults are the admin page's: 3 entries of 5000 sats, a 5% fee,
# one value per entry, one winner, observation from +6h to +24h, signing by +33h.
# Times are RFC 3339 or relative to now (+90s, +30m, +6h, +2d).
coordinator admin competitions create --stations KDEN,KJFK
coordinator admin competitions create --stations KDEN --entry-fee 1000 --max-entries 5 \
  --start +10m --end +40m --signing +50m --unlisted
coordinator admin competitions create --stations KDEN --dry-run   # print the request only

# Cancel one nobody has paid into. It asks first; --yes skips that (required without a terminal).
coordinator admin competitions cancel <competition-id>
coordinator admin competitions cancel <competition-id> --yes
```

`cancel` (alias `delete`) is the admin page's delete: it removes a competition with no paid
entries. Once an entry is paid the coordinator refuses, and the competition runs its course; a
competition that fails or never fills is cancelled and refunded by the coordinator itself.

The commands use these operator-listener endpoints, which scripts may call directly with the same
bearer token:

| Method | Path | |
| --- | --- | --- |
| `GET` | `/api/v1/admin/competitions` | every competition, with state, milestones, errors and refund progress |
| `GET` | `/api/v1/admin/competitions/{id}` | one competition, the same shape |
| `POST` | `/api/v1/competitions` | create (JSON `CreateEvent`) |
| `DELETE` | `/api/v1/admin/competitions/{id}` | delete a competition with no paid entries |

## `synth`

```sh
export SYNTH_URL=http://<synth-host>:9980   # or --url
```

Synth's API has no authentication of its own: anyone who can reach its port can start runs,
which pay for entries, and call its rebalance endpoint, which moves money. Keep it on a network
that admits operators alone, or behind a proxy that does. These commands send no credentials.

Run kinds: `full-lifecycle` and `escrow-refund` (`full_lifecycle` and `escrow_refund` work too).

```sh
# Start a run. It pays for entries from synth's node, so it asks first; --yes skips that.
# Prints the run id.
synth run full-lifecycle --yes
synth run escrow-refund --users 2 --yes

# Start and follow: prints each step as it changes, then waits for the money to settle.
synth run full-lifecycle --yes --wait
synth run escrow-refund --yes --wait --timeout 3h --interval 10

# Recorded runs, and one run's steps and money trail (ledger and every hop).
synth runs list --limit 50
synth runs show <run-id>
synth runs show <run-id> --json
```

`synth run --wait` exits with:

| Code | Meaning |
| --- | --- |
| 0 | The run passed and its money ended where it should (paid out, refunded, or nothing paid). Also 0, with a warning, when synth stopped following the money before it could verify it. |
| 1 | The run failed or was interrupted (or the command itself failed). |
| 3 | The run's money is stuck. |
| 4 | `--timeout` passed first; the run goes on. |

Without a command, `synth [config.toml]` serves the dashboard and API as before.

The commands use `POST /api/run?scenario=…&users=…` (which now answers with the new run's
`run_id`), `GET /api/runs/{id}` (a run and its steps), `GET /api/history?limit=…`,
and `GET /runs/{id}/trail.json`.
