# 5day4cast Coordinator

DLC-based fantasy weather prediction market coordinator with keymeld signing.

## Built With

- [dlctix](https://github.com/tee8z/dlctix) - Ticketed DLC transactions, MuSig2 signing and validation (crates.io `dlctix`)
- [keymeld](https://github.com/tee8z/keymeld) - Threshold signing for DLC contracts
- [bdk_wallet](https://github.com/bitcoindevkit/bdk) - Browser wallet functionality
- [nostr](https://github.com/rust-nostr/nostr) - Nostr protocol for user auth
- [maud](https://maud.lambda.xyz/) - Compile-time HTML templates
- [sqlite](https://sqlite.org/) - Database with Litestream replication

## Quick Start

### Prerequisites

- [Nix](https://nixos.org/download.html) with flakes enabled
- Docker (for k3d-based bitcoin stack)
- An electrs server on the same Bitcoin network as LND, reachable through `bitcoin_settings.electrum_url`

The local service helpers start Bitcoin, LND, Moto, and Keymeld.
Start electrs separately before starting the coordinator; `start-all` does not provide an Electrum server.

Native builds link to system OpenSSL. The Nix overlay supplies patched OpenSSL 3.6.4 and disables vendored OpenSSL.
Outside Nix, install a maintained OpenSSL development package with the current security fixes before building.

`run-keymeld` runs Keymeld the way its own local launcher does: Moto stands in for AWS KMS, three enclaves listen on local TCP ports, and the gateway runs in Keymeld's development environment with a generated channel credential under `data/keymeld`.
Simulated enclaves produce no Nitro attestation, so `config/local.toml` sets `keymeld_settings.dangerous_trust_unattested_enclaves = true`.
This exercises the coordinator's full funding, signing, and invoice flow without enclave hardware.

### Development Setup

```bash
# Enter nix development shell
nix develop

# Start all local services (bitcoin, lnd, moto, keymeld)
start-all

# Or start individual services
start-regtest      # Bitcoin regtest
setup-lnd          # LND nodes
setup-channels     # Open channels
run-keymeld        # Moto KMS + simulated Keymeld enclaves + gateway

# Run the coordinator
just admin-token
cargo run --bin coordinator -- --config ./config/local.toml

# Stop services
stop-all
```

### Using k3d Bitcoin Stack

For testing against the same infrastructure used in staging/production:

```bash
# In infrastructure repo
just bitcoin-dev        # Start k3d cluster with bitcoin stack
just bitcoin-dev-creds  # Export LND creds to coordinator/creds/

# Then run coordinator
cd ~/repos/coordinator
just admin-token
cargo run --bin coordinator -- --config ./config/local.toml
```

## Available Commands (in nix shell)

### Services
| Command | Description |
|---------|-------------|
| `start-all` | Start bitcoin, lnd, and keymeld |
| `stop-all` | Stop all services |
| `start-regtest` | Start bitcoind regtest |
| `stop-regtest` | Stop bitcoind |
| `setup-lnd` | Start LND nodes |
| `setup-channels` | Open channels between LND nodes |
| `stop-lnd` | Stop LND nodes |
| `run-keymeld` | Start keymeld gateway + enclaves |
| `stop-keymeld` | Stop keymeld |
| `mine-blocks N` | Mine N blocks |

### Database & S3
| Command | Description |
|---------|-------------|
| `run-moto` | Start S3 mock server |
| `stop-moto` | Stop S3 mock |
| `run-litestream` | Start database replication |
| `restore-litestream` | Restore from backup |

### Utilities
| Command | Description |
|---------|-------------|
| `clean-data` | Remove data/logs directories |

## Building

```bash
# Build coordinator binary
nix build .#coordinator

# Build docker image
nix build .#docker-coordinator

# Run tests
cargo test

# Run clippy
cargo clippy --all-targets
```

## Configuration

The coordinator reads from `./config/local.toml` by default. Key settings:

```toml
[bitcoin_settings]
network = "regtest"
# electrs, for chain lookups LND cannot answer (escrow and outcome transactions)
electrum_url = "tcp://localhost:60401"
# Optional block explorer linked from the admin wallet page
explorer_url = "http://localhost:9102"

[ln_settings]
base_url = "https://localhost:8080"
macaroon_file_path = "./creds/admin.macaroon"
tls_cert_path = "./creds/tls.cert"

[keymeld_settings]
gateway_url = "http://localhost:8090"
enabled = true

[coordinator_settings]
oracle_url = "http://localhost:9800"

[admin_settings]
# Operator listener: /admin pages, /api/v1/wallet/*, POST /api/v1/competitions.
# Never exposed on the public listener; keep it on loopback or a private network.
listen_addr = "127.0.0.1:9991"
# Bearer token for operators and tooling (synth); at least 32 characters.
token_file = "./creds/admin_token"
```

Public request limits and NIP-98 origin configuration are described in [Request controls](docs/REQUEST_HARDENING.md).

### Operator access

Operator routes are served only by the admin listener and require the token in
`admin_settings.token_file` (`just admin-token` generates one). Scripts send it
as `Authorization: Bearer <token>`; the browser signs in at `/admin/login`,
which sets a session cookie. The coordinator refuses to start without the
token file. `admin_settings.dangerous_allow_unauthenticated = true` disables
this for local development only; it is refused on mainnet and off loopback.
Set `ui_settings.private_url` to the origin the operator's browser uses to
reach the admin listener (a tunnel name in production).

Browser sessions use a Secure cookie. Use HTTPS for remote operator access or
`http://localhost:9991/admin/login` through a local port-forward.
Set `COORDINATOR_ADMIN_URL` and `COORDINATOR_ADMIN_TOKEN_FILE` when using `coord` against a remote coordinator.
The local CLI defaults to `http://localhost:9991` for operator requests.

The Helm chart exposes operator traffic on the separate `<release>-admin` ClusterIP Service.
The public Service and ingress expose only participant traffic.
Configure `secrets.adminToken.create` and `secrets.adminToken.value`, or use
`secrets.adminToken.external` with `secrets.adminToken.secretName`.
The existing Secret must contain a `token` key.
For synth, configure `config.coordinator.adminUrl` and `config.coordinator.adminTokenSecret`.
The synth Secret must exist in the synth namespace and contain the same operator token.
Restart the coordinator after rotating the token; credentials load only during startup.

For an existing Argo CD blue/green deployment, use a maintenance upgrade for v2.
The PreSync hook changes only the image; it cannot install the new token mount, admin port, or configuration.
Creating the admin Service alone does not prepare the old Deployment for v2.

1. After active competitions complete, pause automatic synchronization and participant traffic; back up databases and recovery material.
2. Record the public Service's active slot:

```bash
kubectl -n <namespace> get service <release> -o jsonpath='{.spec.selector.app\.kubernetes\.io/slot}{"\n"}'
```

Use the chart's full Service name when `fullnameOverride` or `nameOverride` is configured.
The result must be `blue` or `green`.

3. Keep `blueGreen.enabled=true` and set `blueGreen.activeSlot` to the recorded slot.
4. Set `blueGreen.switchoverEnabled=false`; retain the existing release name and PVC names.
5. Configure the operator token, admin origin, and remaining v2 settings described above and below.
6. Sync the full v2 chart, including its ConfigMap, token Secret, admin Service, and both Deployment specifications.
   For an external token Secret, provision that Secret before synchronization.
   The active Deployment restarts on its existing PVC; the standby remains stopped.
7. Verify active Deployment readiness, public health, and authenticated admin access before resuming traffic.
8. Verify that both Service selectors match the recorded slot.
9. Preserve the Argo CD ignore rules for both Deployments' `/spec/replicas` fields.
   Ignore `/spec/selector/app.kubernetes.io~1slot` for both the public and admin Services.
   Set `RespectIgnoreDifferences=true` in the Application's `spec.syncPolicy.syncOptions` before re-enabling the hook.
   Without this option, ignore rules affect comparisons only; Sync can revert the hook's changes.
   See [Argo CD's sync option documentation](https://argo-cd.readthedocs.io/en/stable/user-guide/sync-options/#respect-ignore-differences-configs).
10. Re-enable `blueGreen.switchoverEnabled` and resume automatic synchronization after verification.

Do not disable `blueGreen.enabled` during this procedure; doing so changes the Deployment and PVC names.
Subsequent image-only upgrades can use the PreSync hook, which switches both Service selectors.
Argo CD Sync must preserve those selector changes.

### Keymeld authorization upgrade

The SDK and Nix services use Keymeld `v0.4.1`, with the published dlctix `0.1.0` crate.
The coordinator, browser wallet, synth, and Keymeld SDK resolve the same dlctix package.
Deploy matching gateway, enclave, coordinator, and browser artifacts together.
Complete active competitions before upgrading and archive their session state.
Legacy session records lack authorization credentials and cannot resume under this protocol.

Configure `keymeld_settings.trusted_pcrs` with PCR0 or PCR8 from the reviewed enclave build.
Use hexadecimal, nonzero SHA-384 measurements; never copy trust pins from the gateway being verified.
Set `keymeld_settings.public_gateway_url` to the browser-reachable gateway address.
Allow the coordinator's exact browser origin in Keymeld's `server.cors_allowed_origins`.
For Helm, use `keymeld.trustedPcrs` and `keymeld.publicGatewayUrl`.
An enabled coordinator rejects missing or invalid trust pins unless `keymeld_settings.dangerous_trust_unattested_enclaves` is set.
That setting exists for local simulation and for staging where Keymeld runs simulated enclaves with Moto KMS instead of Nitro hardware.
The coordinator refuses it on mainnet or together with trust pins, warns at startup, and forwards it to browsers in the ticket response so they skip attestation for that gateway only.
For Helm, use `keymeld.dangerousTrustUnattestedEnclaves`.

The browser verifies fresh enclave attestation before encrypting its participant key.
Coordinator retains encrypted slot credentials and a separate signing credential with the pinned manifest and recipient proof.
Ticket responses contain registration context, never those private authority credentials.
Coordinator checks the completed roster against accepted entries before funding and signing.
Entry submission delegates unattended signing for that competition; participants do not approve each batch.

See [Keymeld security operations](https://github.com/tee8z/keymeld/blob/v0.4.1/docs/SECURITY_OPERATIONS.md) for enclave provisioning and hardware acceptance checks.
See [the authorization design](docs/KEYMELD_AUTHORIZATION_MIGRATION.md) for the credentials the coordinator holds and the checks it enforces.
Local mock tests do not establish Nitro attestation or live signing compatibility.

### NOAA Oracle 2.0 compatibility

Use the [NOAA Oracle 2.0 API](https://github.com/tee8z/noaa-oracle/blob/master/docs/attestation.md) with dlctix `0.1.0`.
The event response supplies `nonce_point`, the public nonce commitment.
The coordinator signs the exact JSON body with a fresh NIP-98 event for each HTTP attempt, including retries.

Set `coordinator_settings.oracle_url` to the oracle's configured `remote_url` origin.
For Helm, set `oracle.url` to that same origin.
Add the coordinator's Nostr public key to the oracle's `coordinator_pubkeys` allowlist.
Use the public key for `coordinator_settings.private_key_file`, not the browser user's key.
Without these settings, the oracle rejects event creation and entry submission.

The oracle accepts 2–25 entries and 1–5 winning places, with fewer places than entries.
It limits announcements to 20,000 outcomes.
Submit entries before the observation window ends.
Existing NOAA `expected_observations` entries remain supported.

Run the HTTP compatibility test against a disposable oracle with the test coordinator key allowlisted:

```bash
COORDINATOR_TEST_ORACLE_URL=http://127.0.0.1:9800 \
COORDINATOR_TEST_ORACLE_KEY=/path/to/test-coordinator.pem \
cargo test -p coordinator live_noaa_v2_create_read_and_submit_entries -- --ignored
```

The test creates an event, reads its announcement, and submits two entries.
It does not test weather ingestion or a funded on-chain settlement.

### Upgrade from the coordinator-managed wallet

The coordinator now uses the wallet in the configured LND node for on-chain funds.
Existing BDK wallet funds do not move automatically.

Before upgrading, complete active competitions with the previous version.
Use the previous version to transfer remaining BDK wallet funds to an address owned by LND.
Keep the old wallet database and key backups until you verify the transfer.

Keep the existing `bitcoin_settings.seed_path` key.
The coordinator still uses this key to sign DLC escrow inputs and Nostr events.
Back up LND wallet data through your LND deployment's backup procedure.
The chart no longer replicates `bitcoin.db` with Litestream.

Update configuration before starting the new version:

- Replace `bitcoin_settings.esplora_url` with `bitcoin_settings.electrum_url`.
  Use an Electrum endpoint on the same network as LND.
- Remove `bitcoin_settings.storage_file`.
- Set `bitcoin_settings.explorer_url` only when an HTTP block explorer is available.
- Verify `ln_settings` connects to the funded LND node with its TLS certificate and macaroon.

For Helm, replace `bitcoin.esploraUrl` with `bitcoin.electrumUrl` and optionally set `bitcoin.explorerUrl`.
For `wallet-cli`, use the equivalent `[bitcoin]` and `[ln]` sections in its configuration file.

The wallet balance API now returns `confirmed`, `unconfirmed`, and `locked` amounts in satoshis.
Update clients that read `immature`, `trusted_pending`, or `untrusted_pending`.
The wallet outputs API returns `outpoint`, `txout`, and `confirmations` for each unspent output.

### SQLite ownership and shutdown

Each database has one writable connection and a separate pool of query-only readers.
File databases use private connection caches with write-ahead logging (WAL).
Readers see committed changes without a manual checkpoint.
Legacy `write_max_connections` and `write_min_connections` settings are ignored; use `1` for both.

Each writer accepts up to 64 queued commands, plus its active command.
A full or closed queue rejects new commands before admission.
Accepted commands finish even when their callers disconnect.
A lost reply after admission means the outcome is unknown; do not retry the write automatically.

An unexpected writer exit stops HTTP and makes the process fail.
Shutdown drains HTTP, stops background producers, drains accepted writes, and then closes SQLite.
HTTP and background producer drains each have a 10-second timeout.
The writer drain and pool close each have a 15-second timeout.
Shutdown reports an error when a drain exceeds its timeout.
Successful writes confirm local commits; Litestream replication remains asynchronous.
The Helm chart allows 120 seconds for shutdown through `terminationGracePeriodSeconds`.
Its Litestream container does not guarantee a final remote sync after coordinator shutdown.

## Architecture

### Competition State Machine

Competitions move through a typestate machine; the current state flow is
documented in
[`crates/coordinator/src/domain/competitions/states/mod.rs`](crates/coordinator/src/domain/competitions/states/mod.rs),
next to the transition code. Any state can move to `Failed` or `Cancelled`.

### Frontend

The frontend uses Maud templates with co-located JavaScript:

```
src/templates/
├── layouts/base/     # Base layout + JS
├── pages/entries/    # Entry page + JS
├── pages/payouts/    # Payout page + JS
├── components/modals/# Modal component + JS
└── shared/           # Shared JS utilities
```

`build.rs` bundles and minifies the scripts and styles beside the templates
into the binary, served at content-hashed `/assets/` URLs; only the wasm-pack
output is read from `crates/public_ui/pkg` (`[ui_settings].ui_dir`).

## External Services

- **LND** - Lightning payments (HODL invoices) and the on-chain wallet (addresses, funding, signing, publishing)
- **electrs** - Chain lookups for transactions the LND wallet does not own (Electrum protocol)
- **Oracle** - Weather data and DLC attestations (4casttruth.win)
- **Keymeld** - Threshold signing for DLC contracts
