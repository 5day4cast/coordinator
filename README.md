# 5day4cast Coordinator

DLC-based fantasy weather prediction market coordinator with keymeld signing.

## Built With

- [dlctix](https://github.com/conduition/dlctix) - DLC cryptography and protocol implementation
- [keymeld](https://github.com/tee8z/keymeld) - Threshold signing for DLC contracts
- [bdk_wallet](https://github.com/bitcoindevkit/bdk) - Bitcoin wallet functionality
- [nostr-sdk](https://github.com/rust-nostr/nostr) - Nostr protocol for user auth
- [maud](https://maud.lambda.xyz/) - Compile-time HTML templates
- [sqlite](https://sqlite.org/) - Database with Litestream replication

## Quick Start

### Prerequisites

- [Nix](https://nixos.org/download.html) with flakes enabled
- Docker (for k3d-based bitcoin stack)

### Development Setup

```bash
# Enter nix development shell
nix develop

# Start all local services (bitcoin, lnd, keymeld)
start-all

# Or start individual services
start-regtest      # Bitcoin regtest
setup-lnd          # LND nodes
setup-channels     # Open channels
run-keymeld        # Keymeld gateway + enclaves

# Run the coordinator
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
network = "Regtest"
esplora_url = "http://localhost:9102"

[ln_settings]
base_url = "https://localhost:8080"
macaroon_file_path = "./creds/admin.macaroon"
tls_cert_path = "./creds/tls.cert"

[keymeld_settings]
gateway_url = "http://localhost:8090"
enabled = true

[coordinator_settings]
oracle_url = "http://localhost:9800"
```

## Architecture

### Competition State Machine

1. **Created** → Competition created
2. **EntriesCollected** → All entries paid
3. **EscrowFundsConfirmed** → Escrow transactions confirmed
4. **EventCreated** → Oracle event created
5. **EntriesSubmitted** → Entries submitted to oracle
6. **ContractCreated** → DLC contract parameters generated
7. **NoncesCollected** → User nonces collected
8. **AggregateNoncesGenerated** → Nonces aggregated
9. **PartialSignaturesCollected** → User signatures collected
10. **SigningComplete** → Signatures aggregated (via keymeld)
11. **FundingBroadcasted** → Funding tx broadcast
12. **FundingConfirmed** → Funding confirmed
13. **Attested** → Oracle attestation received
14. **OutcomeBroadcasted** → Outcome tx broadcast
15. **DeltaBroadcasted** → Cooperative close txs broadcast
16. **Completed** → All reclaim txs broadcast

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

JavaScript is bundled at compile time via `build.rs` into `crates/public_ui/`.

## External Services

- **LND** - Lightning payments (HODL invoices)
- **Esplora** - Bitcoin blockchain data
- **Oracle** - Weather data and DLC attestations (4casttruth.win)
- **Keymeld** - Threshold signing for DLC contracts
