# Refactor Status

**Last Updated:** 2026-01-11
**Current Phase:** Part 2.6 Complete (Litestream + DatabaseWriter)

---

## Completed: Part 2.6 - Litestream Database Replication ✅

### What Was Done

1. **Added Moto (S3 mock) to flake.nix:**
   - Python environment with moto, flask, boto3
   - `run-moto` script starts S3 mock on port 4566
   - `stop-moto` script for cleanup
   - Auto-creates `coordinator-db-backups` bucket

2. **Added Litestream to flake.nix:**
   - `run-litestream` script starts database replication
   - `restore-litestream` script for recovery
   - Supports point-in-time restore via `LITESTREAM_TIMESTAMP`

3. **Created litestream config files:**
   - `config/litestream.yml` - Development (uses Moto)
   - `config/litestream.production.yml` - Production (uses real S3)
   - Configured for all 3 databases: bitcoin.db, competitions.db, users.db

4. **Updated Helm chart for litestream:**
   - Added litestream values with multi-database support
   - Init container for restore on pod startup
   - Sidecar container for continuous replication
   - ConfigMap template generates litestream.yml from values

5. **Implemented DatabaseWriter pattern:**
   - `DatabaseWriter` struct with channel-based serialization in `infra/db.rs`
   - `DatabaseWriteError` error type for write operation errors
   - `DBConnection.execute_write()` method for WAL-safe serialized writes
   - Each database (competitions, users) has its own `DatabaseWriter` instance

6. **Migrated all write operations to execute_write():**
   - `domain/competitions/store.rs` - All 20+ write operations migrated
   - `domain/users/store.rs` - All 3 write operations migrated
   - `DBConnection.write()` deprecated but kept for backwards compatibility
   - No deprecation warnings remain - `cargo check` passes clean

7. **Added sqlx offline mode support:**
   - Created `.cargo/config.toml` with `SQLX_OFFLINE=true`
   - Prepared for future migration to `sqlx::query!` macros

### Testing Litestream Locally

```bash
# Start moto S3 mock
run-moto

# Start litestream replication
run-litestream

# To restore from backup
restore-litestream
```

---

## Completed: Part 2 - Nix Build System & CI ✅

### What Was Done

1. **Created `flake.nix` for reproducible builds:**
   - Uses crane for Rust builds with caching
   - References keymeld from GitHub (`github:tee8z/keymeld`)
   - Exports `coordinator` and `wallet-cli` packages
   - Includes development shell with all required tools

2. **Development environment includes:**
   - Rust 1.85.0 toolchain with WASM target
   - Bitcoin stack: `bitcoind`, `bitcoin-cli`
   - Lightning: `lnd`, `lncli`
   - Keymeld: `keymeld-gateway`, `keymeld-enclave`
   - Database tools: `sqlite`, `sqlx-cli`
   - WASM tools: `wasm-pack`, `wasm-bindgen-cli`

3. **Helper scripts for local development:**
   - `start-regtest` - Start bitcoind in regtest mode
   - `setup-lnd` - Start two LND nodes
   - `setup-channels` - Create channels between LND nodes
   - `run-keymeld` - Start keymeld gateway + enclaves
   - `start-all` / `stop-all` - Manage entire stack
   - `mine-blocks N` - Mine N blocks

4. **Created `justfile` with development commands:**
   - `just build` / `just build-release`
   - `just test` / `just test-verbose`
   - `just clippy` / `just fmt`
   - `just start` / `just stop` - Manage dev services
   - `just e2e` - Run end-to-end tests
   - `just status` - Show service status

5. **Created GitHub Actions CI workflow (`.github/workflows/ci.yml`):**
   - Format check (`cargo fmt`)
   - Clippy lint check
   - Unit tests
   - Build check with artifact upload
   - E2E tests with full stack (Bitcoin, LND, Keymeld)

6. **Removed `.doppler/` directory:**
   - Replaced with Nix-based development environment

7. **Updated `.gitignore`:**
   - Added Nix artifacts (`result`, `result-*`, `.direnv/`)
   - Added development data directories (`data/`, `logs/`)
   - Added WASM build output (`pkg/`)
   - Added SQLite databases

### Build Status

```bash
$ nix build .#coordinator
# Successfully builds coordinator binary

$ cargo check
# All 3 crates compile successfully
```

### New Files Created

- `flake.nix` - Nix flake for reproducible builds
- `flake.lock` - Lock file for flake inputs
- `justfile` - Development command runner
- `.github/workflows/ci.yml` - CI workflow

---

## Completed: Part 1 - Project Structure Cleanup ✅

### What Was Done

1. **Created 3-crate workspace:**
   - `coordinator` - Main server binary + library
   - `coordinator-core` - Shared types (server + WASM)
   - `coordinator-wasm` - Browser WASM module

2. **Reorganized coordinator crate:**
   ```
   crates/coordinator/src/
   ├── main.rs
   ├── lib.rs
   ├── config.rs
   ├── startup.rs
   ├── api/
   │   ├── mod.rs
   │   ├── extractors.rs
   │   ├── routes/
   │   └── views/
   ├── domain/
   │   ├── mod.rs
   │   ├── competitions/
   │   ├── invoices/
   │   └── users/
   ├── infra/
   │   ├── mod.rs
   │   ├── bitcoin.rs
   │   ├── db.rs
   │   ├── escrow.rs
   │   ├── lightning.rs
   │   ├── oracle.rs
   │   └── secrets.rs
   └── bin/
       └── wallet_cli.rs
   ```

3. **Moved frontend assets:**
   - `crates/admin_ui/` → `crates/coordinator/frontend/admin/`
   - `crates/public_ui/` → `crates/coordinator/frontend/public/`

4. **Removed old structure:**
   - Deleted `crates/server/`
   - Deleted `crates/client_validator/`
   - Deleted `crates/admin_ui/`
   - Deleted `crates/public_ui/`

---

## Next: Part 2.5 - Oracle Interface Abstraction & Mock

### Goals
- Create abstract `Oracle` trait for pluggable data sources
- Implement `MockOracle` for deterministic e2e tests
- Decouple from weather-specific types
- Enable testing without real oracle dependency

### Key Changes
- Add `Oracle` trait in `coordinator/src/infra/oracle.rs`
- Create `MockOracle` implementation
- Update tests to use mock oracle

---

## Remaining Parts

| Part | Description | Status |
|------|-------------|--------|
| 1 | Project Structure Cleanup | ✅ Complete |
| 2 | Nix Build System & CI | ✅ Complete |
| 2.5 | Oracle Interface Abstraction & Mock | Pending |
| 2.6 | Litestream & DatabaseWriter | ✅ Complete |
| 3 | Typestate Machine | Pending |
| 4 | Keymeld SDK Integration (Server) | Pending |
| 5 | Keymeld SDK Integration (WASM) | Pending |
| 6 | Database Migration System | Pending |
| 7 | Escrow Simplification | Pending |
| 8 | Testing & Documentation | Pending |

---

## Notes for Next Session

- First `nix develop` takes time to build keymeld from source - subsequent runs are cached
- Dev shell requires keymeld gateway/enclave to be built (pulls from GitHub)
- E2E tests in CI will use Nix to start all services
- DataSourceType simplified to Weather only (other variants can be added later)
- Frontend JS files still reference old `client_validator` WASM - will need updating in Part 5
