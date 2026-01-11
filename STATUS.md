# Refactor Status

**Last Updated:** 2026-01-11
**Current Phase:** Part 7 Complete (Frontend Simplification & E2E Tests)

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

## Completed: Part 3 - Typestate Machine Architecture ✅

### What Was Done

1. **Created states module under domain/competitions:**
   ```
   domain/competitions/states/
   ├── mod.rs                    # CompetitionStatus enum, HasCompetitionData trait
   ├── processor.rs              # StateContext, StateProcessingError
   ├── created.rs                # Initial state
   ├── collecting_entries.rs     # Collecting participant entries
   ├── awaiting_escrow.rs        # Waiting for escrow confirmation
   ├── escrow_confirmed.rs       # Escrow transactions confirmed
   ├── event_created.rs          # Oracle event created
   ├── entries_submitted.rs      # Entries submitted to oracle
   ├── contract_created.rs       # DLC contract created
   ├── awaiting_signatures.rs    # Waiting for MuSig2 signatures (keymeld or legacy)
   ├── funding.rs                # SigningComplete, FundingBroadcasted, FundingConfirmed, FundingSettled
   ├── awaiting_attestation.rs   # Waiting for oracle attestation
   ├── settling.rs               # Attested, ExpiryBroadcasted, OutcomeBroadcasted, DeltaBroadcasted
   ├── completed.rs              # Terminal success state
   └── failed.rs                 # Failed and Cancelled terminal states
   ```

2. **Implemented CompetitionStatus enum:**
   - Wrapper enum for dynamic dispatch (20+ state variants)
   - `competition_id()`, `state_name()`, `is_terminal()`, `is_immediate_transition()` methods
   - `fail()` and `cancel()` methods for transitioning to terminal states
   - `into_competition()` for extracting Competition data
   - `From<Competition>` implementation for reconstructing state from DB

3. **Implemented HasCompetitionData trait:**
   - `competition()` - Get reference to underlying Competition
   - `competition_mut()` - Get mutable reference
   - `into_competition()` - Consume and extract Competition

4. **Each state struct has:**
   - `competition_id` field for quick access
   - State-specific fields (timestamps, data relevant to that state)
   - `from_competition()` constructor for DB reconstruction
   - Transition methods that consume `self` and return `CompetitionStatus`
   - State-specific query methods

5. **Updated coordinator.rs:**
   - `process_status()` method handles all state transitions
   - Matches on `CompetitionStatus` enum for state-specific logic
   - Immediate transitions handled in a loop for pass-through states
   - Error handling pushes to `competition.errors` and can trigger `fail()` transition

6. **Keymeld integration in AwaitingSignatures:**
   - Checks `is_keymeld_enabled()` to select signing flow
   - Keymeld mode: Uses remote MuSig2 via keymeld service
   - Legacy mode: Local nonce collection and signature aggregation

---

## Completed: Part 4 - Server-Side Keymeld Integration ✅

### What Was Done

1. **Keymeld trait and service implementation (`infra/keymeld.rs`):**
   - `Keymeld` trait with `create_dlc_keygen_session()` and `sign_dlc_batch()`
   - `KeymeldService` production implementation with SDK client
   - `MockKeymeld` for testing when keymeld is disabled
   - `DlcKeygenSession` and `StoredDlcKeygenSession` with hex serialization

2. **DLC-specific keygen and signing:**
   - Uses `DlcSubsetBuilder` to create outcome-based subsets
   - Uses `DlcBatchBuilder` to batch sign all DLC transactions
   - Maps outcome indices to keymeld subset IDs
   - Handles aggregate key generation and storage

3. **Session secret sharing via NIP-44:**
   - `get_keymeld_session_info()` in coordinator.rs
   - Encrypts session secret with participant's nostr pubkey
   - Returns `KeymeldSessionInfo` with gateway URL, session ID, encrypted secret

4. **Integration in coordinator.rs:**
   - `create_funding_psbt()` - Creates keygen session when keymeld enabled
   - `sign_dlc_contract()` - Uses keymeld for batch signing or falls back to legacy
   - `generate_aggregate_nonces_and_coord_partial_signatures()` - Skipped when keymeld enabled
   - Stores keygen session for later signing retrieval

5. **Database persistence:**
   - `store_keymeld_session()` and `get_keymeld_session()` in store.rs
   - Keymeld session stored with competition ID as key
   - Session secret stored securely (hex encoded)

---

## Completed: Part 5 - WASM Keymeld Integration ✅

### What Was Done

1. **KeymeldParticipant WASM wrapper (`coordinator-wasm/src/keymeld/mod.rs`):**
   - `KeymeldParticipant` struct with wasm-bindgen bindings
   - `KeymeldClientConfig` for gateway URL and polling settings
   - Uses keymeld-sdk with WASM-compatible async

2. **Keygen session joining:**
   - `join_keygen_session()` - Join existing session with session secret
   - Registers participant and waits for completion
   - Returns `KeygenResult` with session ID, secret, and aggregate key

3. **Signing participation:**
   - `participate_in_signing()` - Ready for signing with restored session
   - `get_signing_status()` - Poll session status
   - Returns `SigningResult` and `SessionStatus` types

4. **Error handling:**
   - `KeymeldClientError` with SDK, config, session, serialization variants
   - Proper conversion to `JsValue` for JavaScript consumption

---

## Completed: Part 6 - Session Secret Sharing (NIP-44) ✅

### What Was Done

1. **Keymeld info integrated into FundedContract response:**
   - No separate endpoint - keymeld info included in `GET /api/v1/competitions/{id}/contract`
   - `KeymeldSigningInfo` struct added to `FundedContract.keymeld` field
   - Only present when keymeld is enabled and user has an entry

2. **NIP-44 encryption of session secrets:**
   - Uses nostr-sdk NIP-44 v2 encryption
   - Encrypts session secret with participant's nostr pubkey
   - `get_keymeld_signing_info()` method in coordinator.rs

3. **KeymeldSigningInfo fields:**
   - `enabled` - Whether keymeld is enabled on coordinator
   - `gateway_url` - Keymeld gateway URL
   - `session_id` - Active keygen session ID
   - `encrypted_session_secret` - NIP-44 encrypted secret (hex)
   - `user_id` - Participant's keymeld user ID

---

## Completed: Part 7 - Frontend Simplification & E2E Tests ✅

### What Was Done

1. **Removed legacy MuSig signing code (~830 lines deleted):**
   - Deleted `musig_session_manager.js`
   - Deleted `musig_session_registry.js`
   - Deleted `signing_progress_ui.js`
   - Updated `index.html` to remove deleted script references

2. **Added keymeld client for browser:**
   - Created `keymeld_client.js` with simple keymeld join flow
   - `joinKeymeldSession()` - Join keygen session with NIP-44 decryption
   - `pollForKeymeldInfo()` - Poll until payment unlocks keymeld info
   - `completeKeymeldRegistration()` - Full registration flow

3. **Updated entry submission flow:**
   - `entry.js` now uses keymeld after payment
   - User pays → joins keymeld → submits entry
   - Removed complex MuSig state machine

4. **Added Playwright E2E test framework:**
   - Created `e2e/package.json` with Playwright dependencies
   - Created `e2e/playwright.config.ts` for Chromium
   - Created `e2e/tsconfig.json` for TypeScript support
   - Created `e2e/tests/entry-submission.spec.ts` with test suites:
     - Basic UI tests (page load, navigation, modals)
     - Authentication tests (register, login)
     - Competition viewing tests
     - Entry form tests (up to submission, before payment)

5. **Added Playwright to Nix dev environment:**
   - Added `nodejs_22` and `playwright-driver.browsers` to flake.nix
   - Set `PLAYWRIGHT_BROWSERS_PATH` and `PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD`
   - Tests run with Nix-provided Chromium

6. **Updated justfile with Playwright commands:**
   - `playwright-install` - Install npm deps and browsers
   - `playwright` - Run tests
   - `playwright-headed` - Run with visible browser
   - `playwright-ui` - Interactive UI mode
   - `playwright-debug` - Debug mode
   - `playwright-codegen` - Record tests

7. **Added missing config:**
   - Added `[keymeld_settings]` to `config/local.toml`

### Running E2E Tests

```bash
# Enter nix shell (gets Node.js, Playwright, all services)
nix develop

# Start services and coordinator
just start
just run  # in another terminal

# Run tests
just playwright-install  # First time only
just playwright          # Run all tests
just playwright-headed   # With visible browser
```

### Notes
- Full payment/keymeld integration tests should be in a larger harness
- Current tests cover UI flow up to entry submission (before payment)

---

## Next: Part 8 - Admin UI (Maud + HTMX)

### Goals
- Add Maud templates for admin interface
- Competition management UI
- User management UI

---

## Remaining Parts

| Part | Description | Status |
|------|-------------|--------|
| 1 | Project Structure Cleanup | ✅ Complete |
| 2 | Nix Build System & CI | ✅ Complete |
| 2.5 | Oracle Interface Abstraction & Mock | Deferred |
| 2.6 | Litestream & DatabaseWriter | ✅ Complete |
| 3 | Typestate Machine | ✅ Complete |
| 4 | Keymeld SDK Integration (Server) | ✅ Complete |
| 5 | Keymeld SDK Integration (WASM) | ✅ Complete |
| 6 | Session Secret Sharing (NIP-44) | ✅ Complete |
| 7 | Frontend Simplification & E2E Tests | ✅ Complete |
| 8 | Admin UI (Maud + HTMX) | Pending |

---

## Notes

- First `nix develop` takes time to build keymeld from source - subsequent runs are cached
- Dev shell requires keymeld gateway/enclave to be built (pulls from GitHub)
- E2E tests in CI will use Nix to start all services
- DataSourceType simplified to Weather only (other variants can be added later)
- Frontend uses keymeld for signing (legacy MuSig code removed)
