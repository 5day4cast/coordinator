# Refactor Status

**Last Updated:** 2026-01-10
**Current Phase:** Part 4 In Progress - Keymeld SDK Integration (Server)

---

## In Progress: Part 4 - Keymeld SDK Integration (Server)

### What Was Done

1. **Added keymeld-sdk dependency:**
   - Updated workspace `Cargo.toml` to use git dependency
   - Enabled `client` and `dlctix` features
   - Updated dlctix to `external-signing-api` branch for compatibility

2. **Updated rand/rand_chacha to 0.9:**
   - Required for dlctix compatibility
   - Fixed deprecated `thread_rng()` calls to `rng()`
   - Updated `secrets.rs` to use byte-based key generation

3. **Added KeymeldSettings configuration:**
   ```rust
   pub struct KeymeldSettings {
       pub gateway_url: String,
       pub enabled: bool,
       pub keygen_timeout_secs: u64,
       pub signing_timeout_secs: u64,
       pub max_polling_attempts: u32,
       pub initial_polling_delay_ms: u64,
       pub max_polling_delay_ms: u64,
       pub polling_backoff_multiplier: f64,
   }
   ```

4. **Created KeymeldService wrapper (`infra/keymeld.rs`):**
   - `Keymeld` trait for abstracting signing operations
   - `KeymeldService` production implementation
   - `MockKeymeld` for testing without Keymeld server
   - `DlcKeygenSession` struct for session state
   - Integration with dlctix types via keymeld-sdk

### Next Steps for Part 4

1. **Wire KeymeldService into Coordinator struct:**
   - Add `Arc<dyn Keymeld>` to `Coordinator`
   - Pass keymeld service from startup.rs

2. **Add keymeld session ID storage to database:**
   - Add `keymeld_session_id` column to competitions table
   - Add `keymeld_session_secret` for restoring sessions

3. **Modify competition state machine:**
   - Add `AwaitingKeygen` state (after escrow confirmed)
   - Add `AwaitingSigning` state (after keygen complete)
   - Update transitions to use keymeld when enabled

4. **Replace MuSig2 code in coordinator.rs:**
   - Remove `SigningSession::<NonceSharingRound>` usage
   - Remove nonce collection/aggregation logic
   - Use `KeymeldService.create_dlc_keygen_session()` and `sign_dlc_batch()`

### Files Modified

- `Cargo.toml` (workspace) - Added keymeld-sdk dependency
- `crates/coordinator/Cargo.toml` - Added keymeld-sdk.workspace = true
- `crates/coordinator/src/config.rs` - Added KeymeldSettings
- `crates/coordinator/src/infra/mod.rs` - Added keymeld module
- `crates/coordinator/src/infra/keymeld.rs` - NEW: Keymeld service wrapper
- `crates/coordinator/src/infra/secrets.rs` - Updated for rand 0.9
- `crates/coordinator/src/domain/competitions/mod.rs` - Fixed deprecated rng
- `crates/coordinator/src/domain/invoices/invoice_watcher.rs` - Fixed deprecated rng
- `crates/coordinator-wasm/src/wallet/core.rs` - Fixed deprecated rng

---

## Completed: Part 3 - Typestate Machine (Partial) ✅

### What Was Done

Created typestate module structure in `domain/competitions/states/`:
- `mod.rs` - CompetitionStatus enum with 20 state variants
- Individual state files for each state type
- `HasCompetitionData` trait for extracting Competition from any state
- Transition methods that consume self and return new states

**Note:** Full integration with coordinator.rs is deferred until after Keymeld integration, as the keymeld states (AwaitingKeygen, AwaitingSigning) will replace MuSig-specific states.

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

4. **Created `justfile` with development commands**

5. **Created GitHub Actions CI workflow**

6. **Removed `.doppler/` directory**

---

## Completed: Part 1 - Project Structure Cleanup ✅

### What Was Done

1. **Created 3-crate workspace:**
   - `coordinator` - Main server binary + library
   - `coordinator-core` - Shared types (server + WASM)
   - `coordinator-wasm` - Browser WASM module

2. **Reorganized coordinator crate structure**

3. **Moved frontend assets**

4. **Removed old structure**

---

## Skipped: Part 2.5 - Oracle Interface Abstraction & Mock

**Status:** Deferred to later date per user request. Will be added when needed for testing.

---

## Remaining Parts

| Part | Description | Status |
|------|-------------|--------|
| 1 | Project Structure Cleanup | ✅ Complete |
| 2 | Nix Build System & CI | ✅ Complete |
| 2.5 | Oracle Interface Abstraction & Mock | ⏸️ Skipped (deferred) |
| 3 | Typestate Machine | ✅ Partial (states created, integration pending) |
| 4 | Keymeld SDK Integration (Server) | 🔄 In Progress |
| 5 | Keymeld SDK Integration (WASM) | Pending |
| 6 | Database Migration System | Pending |
| 7 | Escrow Simplification | Pending |
| 8 | Testing & Documentation | Pending |

---

## Build Status

```bash
$ cargo check
# All 3 crates compile successfully

$ cargo check -p coordinator
# Compiles with keymeld-sdk integration
```

---

## Notes for Next Session

- KeymeldService wrapper is complete but not yet wired into Coordinator
- Need to add database columns for keymeld session storage
- The current MuSig2 code in coordinator.rs will be replaced when keymeld is wired in
- Consider whether to keep backward compatibility (keymeld disabled mode) or fully replace
