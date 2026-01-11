# Coordinator → Keymeld Refactoring Plan

This document outlines the plan to refactor the coordinator crates to integrate with the keymeld SDK, removing complex local MuSig2 code in favor of the remote signing service.

---

## Executive Summary

The keymeld SDK provides:
- Full WASM support (can run in browser alongside current `client_validator`)
- Session-based keygen and signing with built-in polling
- DLC-specific helpers (`DlcSubsetBuilder`, `DlcBatchBuilder`) that map directly to dlctix types
- Encrypted session credential sharing via 32-byte seeds

This refactor will **remove ~1500+ lines** of complex MuSig2 orchestration code from the coordinator while enabling a cleaner separation of concerns.

---

## Target Project Structure

**3 Rust crates + frontend assets:**

```
coordinator/
├── crates/
│   ├── coordinator/             # Main server (binary + library)
│   │   ├── src/
│   │   │   ├── main.rs
│   │   │   ├── lib.rs
│   │   │   ├── config.rs
│   │   │   ├── startup.rs
│   │   │   ├── api/             # HTTP routes, extractors, Maud views
│   │   │   ├── domain/          # Business logic, typestate machine
│   │   │   └── infra/           # Bitcoin, Lightning, Oracle, DB clients
│   │   ├── frontend/            # Static assets (served by coordinator)
│   │   │   ├── public/          # Public UI
│   │   │   └── admin/           # Admin UI
│   │   └── migrations/
│   │
│   ├── coordinator-core/        # Shared types (server + WASM)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── types.rs         # Competition, Entry, Ticket, etc.
│   │       ├── errors.rs
│   │       └── validation.rs
│   │
│   └── coordinator-wasm/        # Browser WASM module
│       └── src/
│           ├── lib.rs
│           ├── nostr.rs         # Nostr auth + NIP-44
│           ├── wallet.rs        # Escrow PSBT signing
│           └── keymeld.rs       # Keymeld SDK wrapper
│
├── scripts/                     # Development & CI scripts
├── test-fixtures/               # Consolidated test data
├── flake.nix                    # Nix build & dev environment
├── flake.lock
└── Cargo.toml                   # Workspace root
```

**Dependency graph:**
```
    coordinator (server)
         │
    ┌────┴────┐
    ▼         ▼
coordinator-core  keymeld-sdk
    ▲         ▲
    └────┬────┘
         │
  coordinator-wasm
```

---

## Part 1: Project Structure Cleanup ✅ COMPLETE

The current project structure is haphazard with mixed concerns and inconsistent organization. This is the **first step** before any code refactoring.

### 1.1 Current Structure Problems

```
crates/
├── admin_ui/           # Static HTML/JS (not a Rust crate)
├── client_validator/   # WASM crate - does too much
│   ├── nostr/         # Nostr client (should be shared?)
│   └── wallet/        # Taproot wallet + MuSig + PSBT signing
├── public_ui/          # Static HTML/JS + compiled WASM (not a Rust crate)
└── server/             # Monolithic server crate
    └── src/
        ├── domain/     # Business logic (5500+ lines in competitions/)
        ├── routes/     # HTTP handlers
        └── *.rs        # Mixed concerns at root level
```

**Problems:**
1. `admin_ui` and `public_ui` aren't Rust crates but live in `crates/`
2. `client_validator` mixes Nostr client, wallet, MuSig, and PSBT signing
3. `server/src/` has 12 files at root level with mixed concerns
4. `domain/competitions/` has 5500+ lines across 3 files
5. No shared types between client and server
6. Test data scattered in `server/test_data/`
7. `.doppler/` configs that need to be replaced with Nix

### 1.2 Migration Steps

**Step 1: Create new directory structure**
```bash
mkdir -p crates/coordinator/src/{api/routes,api/views,domain/competitions/states,infra}
mkdir -p crates/coordinator/frontend/{public,admin}
mkdir -p crates/coordinator-core/src
mkdir -p crates/coordinator-wasm/src
mkdir -p scripts
mkdir -p test-fixtures/{keys,lightning,db}
```

**Step 2: Create workspace Cargo.toml**

```toml
[workspace]
members = [
    "crates/coordinator",
    "crates/coordinator-core",
    "crates/coordinator-wasm",
]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT"
repository = "https://github.com/5day4cast/coordinator"

[workspace.dependencies]
# Internal crates
coordinator-core = { path = "crates/coordinator-core" }

# Keymeld
keymeld-sdk = { path = "../keymeld/crates/keymeld-sdk" }

# Bitcoin
bitcoin = { version = "0.32", features = ["serde"] }
bdk_wallet = "1.0"
dlctix = { git = "https://github.com/conduition/dlctix" }

# Web
axum = { version = "0.7", features = ["macros"] }
tower-http = { version = "0.5", features = ["cors", "fs"] }
maud = { version = "0.26", features = ["axum"] }

# Async
tokio = { version = "1", features = ["full"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Database
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite"] }

# Nostr
nostr = "0.35"
nostr-sdk = "0.35"

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Error handling
anyhow = "1"
thiserror = "1"

# Time
time = { version = "0.3", features = ["serde"] }

# WASM (for coordinator-wasm)
wasm-bindgen = "0.2"
wasm-bindgen-futures = "0.4"
js-sys = "0.3"
web-sys = { version = "0.3", features = ["console"] }
```

**Step 3: Move server files to coordinator crate**

| From | To |
|------|-----|
| `server/src/main.rs` | `coordinator/src/main.rs` |
| `server/src/lib.rs` | `coordinator/src/lib.rs` |
| `server/src/config.rs` | `coordinator/src/config.rs` |
| `server/src/startup.rs` | `coordinator/src/startup.rs` |
| `server/src/routes/*.rs` | `coordinator/src/api/routes/*.rs` |
| `server/src/nostr_extractor.rs` | `coordinator/src/api/extractors.rs` |
| `server/src/domain/**` | `coordinator/src/domain/**` |
| `server/src/bitcoin_client.rs` | `coordinator/src/infra/bitcoin.rs` |
| `server/src/ln_client.rs` | `coordinator/src/infra/lightning.rs` |
| `server/src/oracle_client.rs` | `coordinator/src/infra/oracle.rs` |
| `server/src/escrow.rs` | `coordinator/src/infra/escrow.rs` |
| `server/src/db.rs` | `coordinator/src/infra/db.rs` |
| `server/src/secrets.rs` | `coordinator/src/infra/secrets.rs` |
| `server/migrations/` | `coordinator/migrations/` |

**Step 4: Move frontend files**

| From | To |
|------|-----|
| `public_ui/*.html` | `coordinator/frontend/public/` |
| `public_ui/*.js` | `coordinator/frontend/public/` |
| `public_ui/*.css` | `coordinator/frontend/public/` |
| `admin_ui/*` | `coordinator/frontend/admin/` |

**Step 5: Create coordinator-core with shared types**

Extract from `domain/competitions/mod.rs`:
- `Competition` struct (data only, no methods)
- `Entry` struct
- `Ticket` struct
- Common enums and error types

**Step 6: Refactor coordinator-wasm from client_validator**

Keep only:
- `nostr.rs` - Auth headers, NIP-44 encryption
- `wallet.rs` - Escrow PSBT signing (ECDSA only)
- `keymeld.rs` - New keymeld SDK wrapper

Remove (handled by keymeld):
- `generate_public_nonces()`
- `sign_aggregate_nonces()`
- `create_deterministic_rng()`

**Step 7: Clean up old structure**
```bash
rm -rf crates/server
rm -rf crates/client_validator
rm -rf crates/admin_ui
rm -rf crates/public_ui
rm -rf .doppler/
rm dist-workspace.toml
```

### 1.3 Server Module Organization

**New `coordinator/src/` structure:**
```
src/
├── main.rs
├── lib.rs
├── config.rs
├── startup.rs
├── api/              # All HTTP concerns
│   ├── mod.rs
│   ├── routes/
│   │   ├── mod.rs
│   │   ├── competitions.rs
│   │   ├── entries.rs
│   │   ├── wallet.rs
│   │   └── health.rs
│   ├── extractors.rs
│   └── views/        # Maud templates
│       ├── mod.rs
│       ├── layouts.rs
│       ├── admin.rs
│       └── public.rs
├── domain/           # Business logic
│   ├── mod.rs
│   ├── competitions/
│   │   ├── mod.rs
│   │   ├── states/   # Typestate machine (Part 3)
│   │   ├── coordinator.rs
│   │   └── store.rs
│   ├── invoices/
│   ├── users/
│   └── keymeld.rs    # Keymeld SDK integration
└── infra/            # External services
    ├── mod.rs
    ├── bitcoin.rs
    ├── lightning.rs
    ├── oracle.rs
    ├── escrow.rs
    └── db.rs
```

### 1.4 Frontend Simplification

**Current `public_ui/` (16 JS files) → Simplified:**

| Keep | Remove (keymeld handles) |
|------|--------------------------|
| `main.js` | `musig_session_manager.js` |
| `auth.js` (from auth_manager) | `musig_session_registry.js` |
| `keymeld.js` (new) | Complex signing JS |
| `competitions.js` (simplified) | |

**Serving static files:**

```rust
// In coordinator/src/api/routes/mod.rs
use axum::routing::get_service;
use tower_http::services::ServeDir;

pub fn static_routes() -> Router {
    Router::new()
        .nest_service("/ui", get_service(ServeDir::new("frontend/public")))
        .nest_service("/admin", get_service(ServeDir::new("frontend/admin")))
}
```

---

## Part 2: Nix Build System & CI

Replace Doppler-based development with Nix for reproducible builds and real e2e testing against keymeld.

### 2.1 Goals

1. **Reproducible builds** - Same result on any machine
2. **Real e2e tests** - Run actual keymeld gateway + enclave
3. **Dev environment** - bitcoind, LND nodes, channels, block mining
4. **CI/CD** - GitHub Actions with Nix caching
5. **No Doppler** - Remove `.doppler/` dependency

### 2.2 Nix Flake Structure

```nix
# flake.nix
{
  description = "Coordinator - DLC competition platform with keymeld signing";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    
    # Pull in keymeld for e2e testing
    keymeld = {
      url = "path:../keymeld";  # Or git URL in production
      # url = "github:5day4cast/keymeld";
    };
    
    # NOTE: Oracle is mocked for e2e tests (see Part 2.5)
    # Real oracle only needed for production deployment
  };

  nixConfig = {
    eval-cache = false;
    extra-substituters = [ "https://cache.nixos.org/" ];
    extra-trusted-public-keys = [
      "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
    ];
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane, keymeld, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable."1.88.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
          targets = [ "wasm32-unknown-unknown" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Common build environment
        commonEnvs = {
          SQLX_OFFLINE = "true";
          RUST_LOG = "info";
        };

        commonDeps = with pkgs; [
          pkg-config
          openssl
          sqlite
          curl
          jq
        ];

        # Build workspace dependencies once
        workspaceDeps = craneLib.buildDepsOnly {
          pname = "coordinator-workspace-deps";
          version = "0.1.0";
          src = craneLib.path ./.;
          buildInputs = commonDeps;
          nativeBuildInputs = commonDeps;
        };

        # Main coordinator binary
        coordinator = craneLib.buildPackage {
          pname = "coordinator";
          version = "0.1.0";
          src = craneLib.path ./.;
          cargoArtifacts = workspaceDeps;
          buildInputs = commonDeps;
          nativeBuildInputs = commonDeps;
          cargoExtraArgs = "--bin coordinator";
          
          postInstall = ''
            mkdir -p $out/share/coordinator
            cp -r crates/coordinator/migrations $out/share/coordinator/
            cp -r crates/coordinator/frontend $out/share/coordinator/
          '';
        } // commonEnvs;

        # WASM build
        coordinator-wasm = craneLib.buildPackage {
          pname = "coordinator-wasm";
          version = "0.1.0";
          src = craneLib.path ./.;
          cargoArtifacts = workspaceDeps;
          buildInputs = commonDeps ++ [ pkgs.wasm-pack ];
          nativeBuildInputs = commonDeps;
          
          buildPhase = ''
            cd crates/coordinator-wasm
            wasm-pack build --target web --out-dir ../../pkg
          '';
          
          installPhase = ''
            mkdir -p $out/lib
            cp -r pkg/* $out/lib/
          '';
        };

        # Get keymeld binaries from the keymeld flake
        keymeld-gateway = keymeld.packages.${system}.keymeld-gateway;
        keymeld-enclave = keymeld.packages.${system}.keymeld-enclave;

        # NOTE: Oracle is mocked - see Part 2.5 for MockOracle implementation

        # Development shell with everything needed
        devShell = pkgs.mkShell {
          buildInputs = commonDeps ++ [
            rustToolchain
            pkgs.just
            pkgs.sqlx-cli
            pkgs.wasm-pack
            
            # Bitcoin stack
            pkgs.bitcoind
            pkgs.bitcoin
            pkgs.electrs  # Or esplora
            
            # Lightning (LND)
            pkgs.lnd
            pkgs.lncli
            
            # Utilities
            pkgs.socat
            pkgs.jq
            pkgs.curl
            pkgs.procps
            
            # Keymeld binaries for e2e testing
            keymeld-gateway
            keymeld-enclave
            
            # Helper scripts
            self.packages.${system}.start-regtest
            self.packages.${system}.setup-channels
            self.packages.${system}.mine-blocks
            self.packages.${system}.run-keymeld
          ];

          shellHook = ''
            export DATA_DIR="$PWD/data"
            export LOGS_DIR="$PWD/logs"
            mkdir -p "$DATA_DIR" "$LOGS_DIR"
            
            echo "Coordinator Development Environment"
            echo "  Use 'just help' to see available commands"
            echo ""
            echo "Bitcoin stack:"
            echo "  start-regtest  - Start bitcoind in regtest mode"
            echo "  setup-channels - Create LND nodes with channels"
            echo "  mine-blocks N  - Mine N blocks"
            echo ""
            echo "Services (for e2e tests):"
            echo "  run-keymeld    - Start keymeld gateway + enclaves"
            echo "  (Oracle is mocked in tests - see Part 2.5)"
          '';

          inherit (commonEnvs) SQLX_OFFLINE RUST_LOG;
        };

        # Script: Start Bitcoin regtest
        start-regtest = pkgs.writeShellScriptBin "start-regtest" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          BITCOIN_DIR="$DATA_DIR/bitcoin"
          
          mkdir -p "$BITCOIN_DIR"
          
          # Create bitcoin.conf if not exists
          if [ ! -f "$BITCOIN_DIR/bitcoin.conf" ]; then
            cat > "$BITCOIN_DIR/bitcoin.conf" <<EOF
          regtest=1
          server=1
          txindex=1
          rpcuser=coordinator
          rpcpassword=coordinatorpass
          rpcallowip=127.0.0.1
          rpcbind=127.0.0.1
          fallbackfee=0.00001
          [regtest]
          rpcport=18443
          port=18444
          EOF
          fi
          
          echo "Starting bitcoind in regtest mode..."
          ${pkgs.bitcoind}/bin/bitcoind \
            -datadir="$BITCOIN_DIR" \
            -daemon \
            -printtoconsole=0
          
          # Wait for bitcoind to be ready
          echo "Waiting for bitcoind..."
          for i in {1..30}; do
            if ${pkgs.bitcoin}/bin/bitcoin-cli \
              -datadir="$BITCOIN_DIR" \
              -rpcuser=coordinator \
              -rpcpassword=coordinatorpass \
              getblockchaininfo > /dev/null 2>&1; then
              echo "bitcoind is ready!"
              break
            fi
            sleep 1
          done
          
          # Create a wallet if needed
          ${pkgs.bitcoin}/bin/bitcoin-cli \
            -datadir="$BITCOIN_DIR" \
            -rpcuser=coordinator \
            -rpcpassword=coordinatorpass \
            -named createwallet wallet_name="coordinator" descriptors=true 2>/dev/null || true
          
          echo "Bitcoin regtest ready on port 18443"
        '';

        # Script: Setup LND channels
        setup-channels = pkgs.writeShellScriptBin "setup-channels" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          LOGS_DIR="''${LOGS_DIR:-$PWD/logs}"
          
          LND1_DIR="$DATA_DIR/lnd1"
          LND2_DIR="$DATA_DIR/lnd2"
          
          mkdir -p "$LND1_DIR" "$LND2_DIR" "$LOGS_DIR"
          
          BITCOIN_CLI="${pkgs.bitcoin}/bin/bitcoin-cli -datadir=$DATA_DIR/bitcoin -rpcuser=coordinator -rpcpassword=coordinatorpass"
          
          echo "Starting LND nodes..."
          
          # Start LND 1 (coordinator's node)
          ${pkgs.lnd}/bin/lnd \
            --lnddir="$LND1_DIR" \
            --bitcoin.regtest \
            --bitcoin.node=bitcoind \
            --bitcoind.rpcuser=coordinator \
            --bitcoind.rpcpass=coordinatorpass \
            --bitcoind.rpchost=127.0.0.1:18443 \
            --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332 \
            --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333 \
            --rpclisten=127.0.0.1:10009 \
            --restlisten=127.0.0.1:8080 \
            --listen=127.0.0.1:9735 \
            --noseedbackup \
            > "$LOGS_DIR/lnd1.log" 2>&1 &
          echo $! > "$DATA_DIR/lnd1.pid"
          
          # Start LND 2 (participant's node)
          ${pkgs.lnd}/bin/lnd \
            --lnddir="$LND2_DIR" \
            --bitcoin.regtest \
            --bitcoin.node=bitcoind \
            --bitcoind.rpcuser=coordinator \
            --bitcoind.rpcpass=coordinatorpass \
            --bitcoind.rpchost=127.0.0.1:18443 \
            --bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332 \
            --bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333 \
            --rpclisten=127.0.0.1:10010 \
            --restlisten=127.0.0.1:8081 \
            --listen=127.0.0.1:9736 \
            --noseedbackup \
            > "$LOGS_DIR/lnd2.log" 2>&1 &
          echo $! > "$DATA_DIR/lnd2.pid"
          
          echo "Waiting for LND nodes to start..."
          sleep 5
          
          # Create wallets
          ${pkgs.lncli}/bin/lncli --lnddir="$LND1_DIR" --rpcserver=127.0.0.1:10009 create --no_macaroons 2>/dev/null || true
          ${pkgs.lncli}/bin/lncli --lnddir="$LND2_DIR" --rpcserver=127.0.0.1:10010 create --no_macaroons 2>/dev/null || true
          
          # Get node pubkeys
          LND1_PUBKEY=$(${pkgs.lncli}/bin/lncli --lnddir="$LND1_DIR" --rpcserver=127.0.0.1:10009 getinfo | ${pkgs.jq}/bin/jq -r '.identity_pubkey')
          LND2_PUBKEY=$(${pkgs.lncli}/bin/lncli --lnddir="$LND2_DIR" --rpcserver=127.0.0.1:10010 getinfo | ${pkgs.jq}/bin/jq -r '.identity_pubkey')
          
          # Fund LND1 wallet
          LND1_ADDR=$(${pkgs.lncli}/bin/lncli --lnddir="$LND1_DIR" --rpcserver=127.0.0.1:10009 newaddress p2wkh | ${pkgs.jq}/bin/jq -r '.address')
          $BITCOIN_CLI sendtoaddress "$LND1_ADDR" 10
          $BITCOIN_CLI -generate 6
          
          # Connect and open channel
          ${pkgs.lncli}/bin/lncli --lnddir="$LND1_DIR" --rpcserver=127.0.0.1:10009 \
            connect "$LND2_PUBKEY@127.0.0.1:9736"
          
          ${pkgs.lncli}/bin/lncli --lnddir="$LND1_DIR" --rpcserver=127.0.0.1:10009 \
            openchannel "$LND2_PUBKEY" 1000000
          
          # Mine blocks to confirm channel
          $BITCOIN_CLI -generate 6
          
          echo "LND nodes ready with channel!"
          echo "  LND1 RPC: 127.0.0.1:10009 (coordinator)"
          echo "  LND2 RPC: 127.0.0.1:10010 (participant)"
        '';

        # Script: Mine blocks
        mine-blocks = pkgs.writeShellScriptBin "mine-blocks" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          COUNT="''${1:-1}"
          
          ${pkgs.bitcoin}/bin/bitcoin-cli \
            -datadir="$DATA_DIR/bitcoin" \
            -rpcuser=coordinator \
            -rpcpassword=coordinatorpass \
            -generate "$COUNT"
          
          echo "Mined $COUNT blocks"
        '';

        # Script: Run keymeld for e2e tests
        run-keymeld = pkgs.writeShellScriptBin "run-keymeld" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          LOGS_DIR="''${LOGS_DIR:-$PWD/logs}"
          KEYMELD_DIR="$DATA_DIR/keymeld"
          
          mkdir -p "$KEYMELD_DIR" "$LOGS_DIR"
          
          echo "Starting keymeld enclaves..."
          
          # Start 3 enclaves on different ports
          for i in 0 1 2; do
            port=$((5000 + i))
            VSOCK_PORT=$port ENCLAVE_ID=$i TEST_MODE=true \
              ${keymeld-enclave}/bin/keymeld-enclave \
              > "$LOGS_DIR/enclave-$i.log" 2>&1 &
            echo $! > "$KEYMELD_DIR/enclave-$i.pid"
            echo "  Enclave $i started on port $port"
          done
          
          # Wait for enclaves to be ready
          sleep 2
          
          echo "Starting keymeld gateway..."
          
          # Start gateway
          KEYMELD_HOST=127.0.0.1 \
          KEYMELD_PORT=8090 \
          KEYMELD_DATABASE_PATH="$KEYMELD_DIR/keymeld.db" \
          TEST_MODE=true \
          ENCLAVE_3_HOST=127.0.0.1 \
          ENCLAVE_4_HOST=127.0.0.1 \
          ENCLAVE_5_HOST=127.0.0.1 \
            ${keymeld-gateway}/bin/keymeld-gateway \
            > "$LOGS_DIR/gateway.log" 2>&1 &
          echo $! > "$KEYMELD_DIR/gateway.pid"
          
          # Wait for gateway to be ready
          for i in {1..30}; do
            if curl -s http://127.0.0.1:8090/health > /dev/null 2>&1; then
              echo "Keymeld gateway ready on http://127.0.0.1:8090"
              break
            fi
            sleep 1
          done
          
          echo ""
          echo "Keymeld stack running!"
          echo "  Gateway: http://127.0.0.1:8090"
          echo "  Enclaves: ports 5000, 5001, 5002"
        '';

        # Script: Stop all services
        stop-all = pkgs.writeShellScriptBin "stop-all" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          
          echo "Stopping all services..."
          
          # Stop keymeld
          for f in "$DATA_DIR/keymeld"/*.pid; do
            [ -f "$f" ] && kill $(cat "$f") 2>/dev/null || true
            rm -f "$f"
          done
          
          # Stop LND nodes
          for f in "$DATA_DIR"/*.pid; do
            [ -f "$f" ] && kill $(cat "$f") 2>/dev/null || true
            rm -f "$f"
          done
          
          # Stop bitcoind
          ${pkgs.bitcoin}/bin/bitcoin-cli \
            -datadir="$DATA_DIR/bitcoin" \
            -rpcuser=coordinator \
            -rpcpassword=coordinatorpass \
            stop 2>/dev/null || true
          
          echo "All services stopped"
        '';

      in {
        packages = {
          default = coordinator;
          inherit coordinator coordinator-wasm;
          inherit start-regtest setup-channels mine-blocks run-keymeld stop-all;
        };

        devShells.default = devShell;
      });
}
```

### 2.3 Justfile Commands

```just
# justfile

# Default recipe
default:
    @just --list

# Development environment
dev:
    nix develop

# Build everything
build:
    nix develop -c cargo build --workspace --locked

# Build release
build-release:
    nix develop -c cargo build --workspace --release --locked

# Build WASM
build-wasm:
    nix develop -c wasm-pack build crates/coordinator-wasm --target web --out-dir ../../pkg

# Run tests
test:
    nix develop -c cargo nextest run --workspace --locked

# Format code
fmt:
    nix develop -c cargo fmt --all

# Lint
clippy:
    nix develop -c cargo clippy --workspace --all-targets -- -D warnings

# Check unused dependencies
machete:
    nix develop -c cargo machete --with-metadata

# Start development stack (oracle is mocked in tests)
start:
    start-regtest
    setup-channels
    run-keymeld
    @echo ""
    @echo "Development stack ready!"
    @echo "  Bitcoin RPC: http://coordinator:coordinatorpass@127.0.0.1:18443"
    @echo "  LND1 (coordinator): 127.0.0.1:10009"
    @echo "  LND2 (participant): 127.0.0.1:10010"
    @echo "  Keymeld: http://127.0.0.1:8090"
    @echo "  Oracle: mocked (see Part 2.5)"

# Stop all services
stop:
    stop-all

# Run coordinator server
run:
    nix develop -c cargo run --bin coordinator

# Run e2e tests
e2e:
    just start
    sleep 5
    nix develop -c cargo test --test e2e -- --test-threads=1
    just stop

# Clean data directory
clean:
    rm -rf data/ logs/

# Database migrations
migrate:
    nix develop -c sqlx migrate run --source crates/coordinator/migrations

# Mine some blocks (for testing)
mine n="1":
    mine-blocks {{n}}
```

### 2.4 GitHub Actions CI

```yaml
# .github/workflows/ci.yml
name: CI

on:
  push:
    branches: [master]
  pull_request:
    branches: [master]

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1

jobs:
  # Build with Nix, upload artifacts
  build:
    name: Build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Free disk space
        run: |
          sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc
          sudo docker image prune --all --force || true

      - name: Install Nix
        uses: DeterminateSystems/nix-installer-action@main

      - name: Setup Nix cache
        uses: DeterminateSystems/magic-nix-cache-action@main

      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry/
            ~/.cargo/git/
            target/
          key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}

      - name: Build
        run: nix develop -c cargo build --workspace --locked

      - name: Upload binaries
        uses: actions/upload-artifact@v4
        with:
          name: coordinator-binaries
          path: |
            target/debug/coordinator
          retention-days: 1

  # Format check
  fmt:
    name: Format
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          components: rustfmt
      - run: cargo fmt --all -- --check

  # Clippy
  clippy:
    name: Clippy
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          components: clippy
      - run: cargo clippy --workspace --all-targets -- -D warnings

  # Unit tests
  test:
    name: Test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/setup-rust-toolchain@v1
      - uses: taiki-e/install-action@v2
        with:
          tool: cargo-nextest
      - run: cargo nextest run --workspace --locked

  # E2E tests with keymeld
  e2e:
    name: E2E Tests
    runs-on: ubuntu-latest
    needs: [build, fmt, clippy, test]
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4

      - name: Free disk space
        run: |
          sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc
          sudo docker image prune --all --force || true

      - name: Install Nix
        uses: DeterminateSystems/nix-installer-action@main

      - name: Setup Nix cache
        uses: DeterminateSystems/magic-nix-cache-action@main

      - name: Download binaries
        uses: actions/download-artifact@v4
        with:
          name: coordinator-binaries
          path: target/debug/

      - name: Make executable
        run: chmod +x target/debug/coordinator

      - name: Run E2E tests
        run: |
          nix develop -c just e2e

      - name: Upload logs on failure
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: e2e-logs-${{ github.run_id }}
          path: |
            logs/
            data/
          retention-days: 7
```

### 2.5 Files to Delete

Remove Doppler and dist-workspace dependencies:

```bash
rm -rf .doppler/
rm dist-workspace.toml
```

Update `.gitignore`:
```
# Nix
result
result-*

# Development data
data/
logs/
pkg/

# SQLite
*.db
*.db-journal
*.db-wal
*.db-shm
```

---

## Part 2.5: Oracle Interface Abstraction & Mock

### 2.5.1 Goals

1. **Mock oracle for e2e tests** - Full control over attestation timing, deterministic outcomes
2. **Decouple from weather data** - Abstract interface to support future data sources (sports, prices, etc.)
3. **Consistent test contracts** - Same locking conditions every test run

### 2.5.2 Current Oracle Coupling

The coordinator is tightly coupled to weather-specific types:

```rust
// Current: Weather-specific observation types
pub struct WeatherChoices {
    pub stations: String,                    // NOAA station codes
    pub wind_speed: Option<ValueOptions>,    // Weather metric
    pub temp_high: Option<ValueOptions>,     // Weather metric
    pub temp_low: Option<ValueOptions>,      // Weather metric
}

pub enum ValueOptions {
    Over,   // Above forecast
    Par,    // Equals forecast
    Under,  // Below forecast
}
```

**Problems:**
1. Entry observations hardcoded to weather fields
2. Locations are NOAA station codes
3. Scoring semantics tied to weather comparisons
4. Can't reuse for sports, prices, or other data sources

### 2.5.3 Abstracted Oracle Interface

**Generic observation types:**

```rust
// New: Generic observation framework
pub struct ObservationChoice {
    pub source_id: String,           // Generic identifier (station, team, ticker)
    pub metric: String,              // Metric name (temp_high, score, price)
    pub prediction: Comparison,      // What user predicts
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Comparison {
    Over,    // Value will be higher than reference
    Equal,   // Value will equal reference (within tolerance)
    Under,   // Value will be lower than reference
}

// Event creation with generic data source config
pub struct CreateEvent {
    pub id: Uuid,
    pub signing_date: OffsetDateTime,
    pub observation_window: ObservationWindow,
    pub data_sources: Vec<DataSourceConfig>,
    pub metrics: Vec<MetricConfig>,
    pub scoring: ScoringConfig,
    // Competition params
    pub number_of_places_win: usize,
    pub total_allowed_entries: usize,
    pub entry_fee: u64,
    pub coordinator_fee_percentage: u8,
}

pub struct ObservationWindow {
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
}

pub struct DataSourceConfig {
    pub id: String,                  // "KLWV", "BTC-USD", "LAL-vs-BOS"
    pub source_type: DataSourceType,
}

// Start with Weather only - add more variants as needed
pub enum DataSourceType {
    Weather { station_code: String },
    // Future variants (not implemented yet):
    // PriceFeed { ticker: String, exchange: String },
    // Sports { event_id: String, provider: String },
    // Custom { provider: String, params: serde_json::Value },
}

pub struct MetricConfig {
    pub name: String,                // "temp_high", "closing_price", "total_score"
    pub comparison_tolerance: Option<f64>,  // For "Equal" comparison
}

pub struct ScoringConfig {
    pub exact_match_points: u32,     // Points for Equal/Par
    pub direction_match_points: u32, // Points for Over/Under correct
    pub tiebreaker: Tiebreaker,
}

pub enum Tiebreaker {
    EarliestEntry,                   // Current: older entry wins
    HighestScore,                    // Total points
    Random { seed: [u8; 32] },       // Deterministic random
}
```

**Updated Oracle trait:**

```rust
#[async_trait]
pub trait Oracle: Send + Sync {
    /// Create an event with generic observation config
    async fn create_event(&self, event: CreateEvent) -> Result<Event, OracleError>;
    
    /// Get event with optional attestation
    async fn get_event(&self, event_id: Uuid) -> Result<Event, OracleError>;
    
    /// Submit entries with generic observations
    async fn submit_entries(&self, entries: SubmitEntries) -> Result<(), OracleError>;
    
    /// Get oracle public key for verification
    async fn pubkey(&self) -> Result<XOnlyPublicKey, OracleError>;
}

pub struct SubmitEntries {
    pub event_id: Uuid,
    pub entries: Vec<EntrySubmission>,
}

pub struct EntrySubmission {
    pub id: Uuid,
    pub observations: Vec<ObservationChoice>,
}
```

### 2.5.4 Mock Oracle for E2E Tests

The mock oracle provides deterministic behavior for testing:

```rust
// coordinator/src/infra/oracle_mock.rs

use std::sync::{Arc, RwLock};
use dlctix::{EventLockingConditions, MaybeScalar, Scalar};

/// Mock oracle with manual attestation control
pub struct MockOracle {
    /// Oracle keypair (deterministic from seed)
    keypair: Keypair,
    /// Stored events
    events: Arc<RwLock<HashMap<Uuid, MockEvent>>>,
    /// Pending attestations (set by test)
    pending_attestations: Arc<RwLock<HashMap<Uuid, Outcome>>>,
}

struct MockEvent {
    config: CreateEvent,
    nonce: Scalar,
    locking_conditions: EventLockingConditions,
    entries: Vec<EntrySubmission>,
    attestation: Option<MaybeScalar>,
}

impl MockOracle {
    /// Create with deterministic seed for reproducible tests
    pub fn new(seed: [u8; 32]) -> Self {
        let keypair = Keypair::from_seed(&seed);
        Self {
            keypair,
            events: Arc::new(RwLock::new(HashMap::new())),
            pending_attestations: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Test helper: Queue an attestation for an event
    /// The attestation will be returned on next get_event() call
    pub fn queue_attestation(&self, event_id: Uuid, outcome: Outcome) {
        self.pending_attestations
            .write()
            .unwrap()
            .insert(event_id, outcome);
    }
    
    /// Test helper: Get the locking conditions for verification
    pub fn get_locking_conditions(&self, event_id: Uuid) -> Option<EventLockingConditions> {
        self.events
            .read()
            .unwrap()
            .get(&event_id)
            .map(|e| e.locking_conditions.clone())
    }
    
    /// Test helper: List all entries for an event
    pub fn get_entries(&self, event_id: Uuid) -> Vec<EntrySubmission> {
        self.events
            .read()
            .unwrap()
            .get(&event_id)
            .map(|e| e.entries.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Oracle for MockOracle {
    async fn create_event(&self, config: CreateEvent) -> Result<Event, OracleError> {
        // Generate deterministic nonce from event ID
        let nonce = Scalar::from_bytes(&sha256(&config.id.as_bytes()))?;
        
        // Calculate all possible outcomes and locking points
        let outcomes = generate_outcomes(
            config.total_allowed_entries,
            config.number_of_places_win,
        );
        
        let locking_conditions = EventLockingConditions::new(
            &self.keypair.public_key(),
            &nonce.base_point_mul(),
            &outcomes,
            config.signing_date + Duration::days(1), // expiry
        );
        
        let event = MockEvent {
            config: config.clone(),
            nonce,
            locking_conditions: locking_conditions.clone(),
            entries: vec![],
            attestation: None,
        };
        
        self.events.write().unwrap().insert(config.id, event);
        
        Ok(Event {
            id: config.id,
            nonce,
            event_announcement: locking_conditions,
            attestation: None,
        })
    }
    
    async fn get_event(&self, event_id: Uuid) -> Result<Event, OracleError> {
        let mut events = self.events.write().unwrap();
        let event = events.get_mut(&event_id)
            .ok_or(OracleError::NotFound)?;
        
        // Check if test has queued an attestation
        if event.attestation.is_none() {
            if let Some(outcome) = self.pending_attestations.write().unwrap().remove(&event_id) {
                // Generate attestation for the specified outcome
                let attestation = dlctix::attestation_secret(
                    &self.keypair.secret_key(),
                    &event.nonce,
                    &outcome.to_bytes(),
                );
                event.attestation = Some(attestation);
            }
        }
        
        Ok(Event {
            id: event_id,
            nonce: event.nonce,
            event_announcement: event.locking_conditions.clone(),
            attestation: event.attestation,
        })
    }
    
    async fn submit_entries(&self, entries: SubmitEntries) -> Result<(), OracleError> {
        let mut events = self.events.write().unwrap();
        let event = events.get_mut(&entries.event_id)
            .ok_or(OracleError::NotFound)?;
        
        event.entries.extend(entries.entries);
        Ok(())
    }
    
    async fn pubkey(&self) -> Result<XOnlyPublicKey, OracleError> {
        Ok(self.keypair.public_key().x_only_public_key().0)
    }
}

/// Outcome represents winner positions
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Indices of winning entries (0-indexed, in order of placement)
    pub winners: Vec<usize>,
}

impl Outcome {
    pub fn new(winners: Vec<usize>) -> Self {
        Self { winners }
    }
    
    /// All entries refunded (no winners)
    pub fn refund_all(entry_count: usize) -> Self {
        Self { winners: (0..entry_count).collect() }
    }
    
    pub fn to_bytes(&self) -> Vec<u8> {
        // Encode as sorted entry indices
        let mut bytes = vec![];
        for &idx in &self.winners {
            bytes.extend(&(idx as u32).to_be_bytes());
        }
        bytes
    }
}
```

### 2.5.5 E2E Test Usage

```rust
// tests/e2e/competition_flow.rs

#[tokio::test]
async fn test_full_competition_lifecycle() {
    // Create mock oracle with deterministic seed
    let oracle = Arc::new(MockOracle::new([0u8; 32]));
    
    // Start coordinator with mock oracle
    let app = TestApp::new()
        .with_oracle(oracle.clone())
        .with_keymeld(keymeld_url)
        .build()
        .await;
    
    // Create competition
    let comp = app.create_competition(CreateCompetition {
        entry_fee: 5000,
        total_entries: 3,
        places_win: 1,
        ..Default::default()
    }).await;
    
    // Add entries
    let entry1 = app.add_entry(&comp.id, user1_keys).await;
    let entry2 = app.add_entry(&comp.id, user2_keys).await;
    let entry3 = app.add_entry(&comp.id, user3_keys).await;
    
    // Wait for funding to confirm
    app.mine_blocks(6).await;
    app.wait_for_state(&comp.id, "funding_confirmed").await;
    
    // Trigger attestation - entry2 wins!
    oracle.queue_attestation(comp.oracle_event_id, Outcome::new(vec![1]));
    
    // Poll until attested
    app.wait_for_state(&comp.id, "attested").await;
    
    // Verify outcome transaction was broadcast
    let outcome_tx = app.get_outcome_tx(&comp.id).await;
    assert!(outcome_tx.is_some());
    
    // Verify winner got paid
    let entry2_balance = app.get_entry_balance(&entry2.id).await;
    assert!(entry2_balance > 0);
}

#[tokio::test]
async fn test_refund_on_tie() {
    let oracle = Arc::new(MockOracle::new([1u8; 32]));
    let app = TestApp::new().with_oracle(oracle.clone()).build().await;
    
    let comp = app.create_competition_with_entries(3).await;
    app.wait_for_state(&comp.id, "funding_confirmed").await;
    
    // All entries refunded
    oracle.queue_attestation(comp.oracle_event_id, Outcome::refund_all(3));
    
    app.wait_for_state(&comp.id, "completed").await;
    
    // All entries should have their funds returned
    for entry in &comp.entries {
        let balance = app.get_entry_balance(&entry.id).await;
        assert_eq!(balance, comp.entry_fee);
    }
}

#[tokio::test]
async fn test_expiry_path() {
    let oracle = Arc::new(MockOracle::new([2u8; 32]));
    let app = TestApp::new().with_oracle(oracle.clone()).build().await;
    
    let comp = app.create_competition_with_entries(3).await;
    app.wait_for_state(&comp.id, "funding_confirmed").await;
    
    // Don't queue attestation - let it expire
    // Fast-forward time past expiry
    app.advance_time(Duration::days(2)).await;
    
    // Coordinator should use expiry path
    app.wait_for_state(&comp.id, "completed").await;
    
    // Funds should be returned via expiry transactions
}
```

### 2.5.6 Flake Update - Remove Oracle Dependency

Since we're mocking the oracle, remove it from the dev environment:

```nix
# In flake.nix inputs - REMOVE noaa-oracle
inputs = {
    # ... other inputs
    keymeld = { url = "path:../keymeld"; };
    # noaa-oracle removed - using mock for e2e tests
};

# In devShell - REMOVE oracle-server
buildInputs = commonDeps ++ [
    # ... 
    keymeld-gateway
    keymeld-enclave
    # oracle-server removed
    
    self.packages.${system}.start-regtest
    self.packages.${system}.setup-channels
    self.packages.${system}.mine-blocks
    self.packages.${system}.run-keymeld
    # run-oracle removed
];
```

### 2.5.7 Migration Path

**Phase 1: Add abstraction layer (backward compatible)**
1. Create generic `ObservationChoice` alongside existing `WeatherChoices`
2. Add `Oracle` trait with generic types
3. Implement `Oracle` for existing `OracleClient` (maps weather types)

**Phase 2: Add mock oracle**
1. Implement `MockOracle` with manual attestation control
2. Add e2e tests using mock
3. Validate all test scenarios pass

**Phase 3: Deprecate weather-specific types**
1. Update `CreateEvent` to use generic types
2. Update entry submission to use `ObservationChoice`
3. Remove `WeatherChoices`, `ValueOptions` from coordinator

**Phase 4: Support new data sources (future)**
1. Implement price feed oracle
2. Implement sports oracle
3. Each oracle implements same `Oracle` trait

### 2.5.8 Benefits

| Aspect | Before | After |
|--------|--------|-------|
| E2E tests | Require real oracle + weather data | Deterministic, instant |
| Test control | Wait for real attestation timing | Trigger attestation on demand |
| Reproducibility | Depends on weather API | Same results every run |
| Data sources | Weather only | Pluggable (ready for sports, prices later) |
| Contract coupling | Weather-specific types | Generic observation types |

---

## Part 3: Typestate Machine Architecture

The current `coordinator.rs` uses a runtime enum-based state machine where state is derived from timestamps and flags on the `Competition` struct. This refactor introduces a **compile-time typestate pattern** inspired by the keymeld enclave's operation states.

### 3.1 Current Problems

The existing approach has several issues:

```rust
// Current: State derived from ~15 timestamp fields
pub fn get_state(&self) -> CompetitionState {
    if self.cancelled_at.is_some() {
        return CompetitionState::Cancelled;
    }
    if self.failed_at.is_some() {
        return CompetitionState::Failed;
    }
    // ... 15+ more conditions checking timestamps
}

// Current: 20+ states in a flat enum
pub enum CompetitionState {
    Created,
    EntriesCollected,
    EscrowFundsConfirmed,
    EventCreated,
    EntriesSubmitted,
    ContractCreated,
    NoncesCollected,           // REMOVE with keymeld
    AggregateNoncesGenerated,  // REMOVE with keymeld
    PartialSignaturesCollected, // REMOVE with keymeld
    SigningComplete,
    FundingBroadcasted,
    FundingConfirmed,
    FundingSettled,
    Attested,
    // ... more states
}
```

**Problems:**
1. State is implicit (derived from timestamps), not explicit
2. Invalid state transitions are possible at runtime
3. Each state handler must validate it's in the correct state
4. Data available in each state is not type-enforced
5. MuSig-specific states (NoncesCollected, etc.) pollute the enum

### 3.2 Typestate Pattern from Keymeld

The keymeld enclave uses explicit typestate structs:

```rust
// Each state is a separate struct with state-specific data
pub struct Initialized { session_id, created_at, ... }
pub struct DistributingSecrets { session_id, session_secret, musig_processor, ... }
pub struct Completed { session_id, aggregate_pubkey, ... }

// State transitions are explicit method calls that consume self
impl Initialized {
    pub fn init_session(self, cmd: &InitCommand) -> Result<KeygenStatus, Error> {
        // ... validation and processing ...
        Ok(KeygenStatus::Distributing(DistributingSecrets::from(self)))
    }
}

// Enum wrapper for dynamic dispatch when needed
pub enum KeygenStatus {
    Initialized(Initialized),
    Distributing(DistributingSecrets),
    Completed(Completed),
    Failed(Failed),
}
```

### 3.3 Proposed Competition Typestate Design

```
coordinator/src/domain/competitions/
├── mod.rs                    # Re-exports, Competition struct (data only)
├── states/
│   ├── mod.rs               # CompetitionStatus enum, shared traits
│   ├── created.rs           # Created state
│   ├── collecting_entries.rs
│   ├── awaiting_escrow.rs
│   ├── awaiting_keygen.rs   # NEW: Keymeld keygen phase
│   ├── awaiting_signing.rs  # NEW: Keymeld signing phase
│   ├── funding.rs           # Broadcasting, confirming, settling
│   ├── awaiting_attestation.rs
│   ├── settling.rs          # Outcome/delta broadcast
│   ├── completed.rs
│   └── failed.rs
├── transitions.rs           # Transition logic and validation
└── coordinator.rs           # Simplified orchestration
```

### 3.4 State Struct Examples

**Created State:**

```rust
// states/created.rs
use super::{CompetitionStatus, CollectingEntries};

#[derive(Debug)]
pub struct Created {
    pub competition_id: Uuid,
    pub created_at: OffsetDateTime,
    pub event_config: CreateEvent,
}

impl Created {
    pub fn new(event_config: CreateEvent) -> Self {
        Self {
            competition_id: event_config.id,
            created_at: OffsetDateTime::now_utc(),
            event_config,
        }
    }
    
    /// Transition to CollectingEntries when first entry is added
    pub fn add_first_entry(
        self,
        entry: Entry,
        escrow_tx: Transaction,
    ) -> Result<CompetitionStatus, CompetitionError> {
        info!("Competition {} receiving first entry", self.competition_id);
        
        let collecting = CollectingEntries::from_created(self, entry, escrow_tx)?;
        Ok(CompetitionStatus::CollectingEntries(collecting))
    }
}
```

**Awaiting Keygen State (NEW - Keymeld integration):**

```rust
// states/awaiting_keygen.rs
use super::{CompetitionStatus, AwaitingSigning};
use keymeld_sdk::managers::KeygenSession;

#[derive(Debug)]
pub struct AwaitingKeygen {
    pub competition_id: Uuid,
    pub created_at: OffsetDateTime,
    pub event_config: CreateEvent,
    pub entries: Vec<Entry>,
    pub contract_parameters: ContractParameters,
    pub keymeld_session_id: SessionId,
}

impl AwaitingKeygen {
    /// Called when keymeld keygen completes
    pub fn keygen_complete(
        self,
        aggregate_pubkey: XOnlyPublicKey,
        subset_pubkeys: BTreeMap<Uuid, XOnlyPublicKey>,
    ) -> Result<CompetitionStatus, CompetitionError> {
        info!(
            "Competition {} keygen complete, transitioning to AwaitingSigning",
            self.competition_id
        );
        
        let signing = AwaitingSigning::from_keygen_complete(
            self,
            aggregate_pubkey,
            subset_pubkeys,
        )?;
        
        Ok(CompetitionStatus::AwaitingSigning(signing))
    }
}
```

### 3.5 Status Enum for Dynamic Dispatch

```rust
// states/mod.rs
pub enum CompetitionStatus {
    Created(Created),
    CollectingEntries(CollectingEntries),
    AwaitingEscrow(AwaitingEscrow),
    EscrowConfirmed(EscrowConfirmed),
    AwaitingKeygen(AwaitingKeygen),
    AwaitingSigning(AwaitingSigning),
    FundingReady(FundingReady),
    FundingBroadcasted(FundingBroadcasted),
    FundingConfirmed(FundingConfirmed),
    AwaitingAttestation(AwaitingAttestation),
    OutcomeBroadcasted(OutcomeBroadcasted),
    DeltaBroadcasted(DeltaBroadcasted),
    Completed(Completed),
    Failed(Failed),
}

impl CompetitionStatus {
    pub fn competition_id(&self) -> Uuid {
        match self {
            Self::Created(s) => s.competition_id,
            Self::CollectingEntries(s) => s.competition_id,
            // ... etc
        }
    }
    
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Created(_) => "created",
            Self::CollectingEntries(_) => "collecting_entries",
            Self::AwaitingKeygen(_) => "awaiting_keygen",
            Self::AwaitingSigning(_) => "awaiting_signing",
            // ... etc
        }
    }
}
```

### 3.6 State Transition Diagram

```
┌─────────┐    ┌──────────────────┐    ┌───────────────┐    ┌─────────────┐
│ Created │───►│CollectingEntries │───►│AwaitingEscrow │───►│EscrowConfirm│
└─────────┘    └──────────────────┘    └───────────────┘    └──────┬──────┘
                                                                   │
                                                                   ▼
                                                           ┌──────────────┐
                                                           │AwaitingKeygen│
                                                           └──────┬───────┘
                                                                  │
                                                                  ▼
                                                           ┌───────────────┐
                                                           │AwaitingSigning│
                                                           └──────┬────────┘
                                                                  │
                                                                  ▼
                                                           ┌─────────────┐
                                                           │FundingReady │
                                                           └──────┬──────┘
                                                                  │
                                                                  ▼
                                                    ┌────────────────────────┐
                                                    │   FundingBroadcasted   │
                                                    └───────────┬────────────┘
                                                                │
                                                                ▼
                                                    ┌────────────────────────┐
                                                    │    FundingConfirmed    │
                                                    └───────────┬────────────┘
                                                                │
                                                                ▼
                                                    ┌────────────────────────┐
                                                    │   AwaitingAttestation  │
                                                    └───────────┬────────────┘
                                                                │
                                                   ┌────────────┴────────────┐
                                                   ▼                         ▼
                                      ┌─────────────────┐       ┌─────────────────┐
                                      │OutcomeBroadcasted│       │ExpiryBroadcasted│
                                      └────────┬────────┘       └────────┬────────┘
                                               │                         │
                                               ▼                         │
                                      ┌─────────────────┐                │
                                      │ DeltaBroadcasted │                │
                                      └────────┬────────┘                │
                                               │                         │
                                               ▼                         ▼
                                      ┌─────────────────────────────────────┐
                                      │             Completed               │
                                      └─────────────────────────────────────┘

(any state can transition to Failed)
```

---

## Part 4: Server-Side Keymeld Integration

### 4.1 Files to Modify

| File | Current Purpose | Changes |
|------|-----------------|---------|
| `domain/competitions/coordinator.rs` | MuSig2 orchestration, nonce aggregation | Replace with keymeld SDK calls |
| `domain/keymeld.rs` | NEW | Keymeld SDK wrapper |
| `infra/db.rs` | SQLite persistence | Add keymeld session ID storage |

### 4.2 Code to Remove

```rust
// REMOVE: Local MuSig session management (~300 lines)
- create_deterministic_rng()
- SigningSession::<NonceSharingRound>::new() calls
- aggregate_nonces_and_compute_partial_signatures()
- Manual nonce collection/aggregation logic

// REMOVE: dlctix signing data construction (~200 lines)
- Manual OutcomeSigningInfo building
- Manual SplitSigningInfo building

// REMOVE: Participant signature polling (~150 lines)
- Custom polling loops for nonce collection
- Timeout handling for signature rounds
```

### 4.3 New Keymeld Integration

```rust
// domain/keymeld.rs
use keymeld_sdk::{
    KeyMeldClient, KeyMeldClientBuilder,
    managers::{KeygenOptions, SigningOptions},
    credentials::SessionCredentials,
    dlctix::{DlcSubsetBuilder, DlcBatchBuilder},
};

pub struct KeymeldService {
    client: KeyMeldClient,
    coordinator_credentials: SessionCredentials,
}

impl KeymeldService {
    pub fn new(url: &str, credentials: SessionCredentials) -> Result<Self> {
        let client = KeyMeldClientBuilder::new(url)
            .with_user_credentials(credentials.clone())
            .build()?;
        Ok(Self { client, coordinator_credentials: credentials })
    }
    
    pub async fn create_keygen_session(
        &self,
        competition: &Competition,
        participant_ids: Vec<UserId>,
    ) -> Result<KeygenSession> {
        let subsets = DlcSubsetBuilder::new(&competition.contract_params)
            .market_maker(self.coordinator_user_id())
            .players(participant_ids)
            .build()?;
        
        self.client.keygen()
            .create_session_with_subsets(participants, subsets, KeygenOptions::default())
            .await
    }
    
    pub async fn create_signing_session(
        &self,
        keygen: &KeygenSession,
        signing_data: &SigningData,
    ) -> Result<SigningSession> {
        let items = DlcBatchBuilder::new(signing_data)
            .with_subset_ids(&keygen.subset_ids)
            .build()?;
        
        self.client.signer()
            .sign_batch(keygen, items, SigningOptions::default())
            .await
    }
}
```

---

## Part 5: Client-Side Keymeld Integration (WASM)

### 5.1 What Stays in coordinator-wasm

- `sign_funding_psbt()` - Escrow transactions use standard ECDSA, not MuSig
- Nostr authentication - Separate concern from MuSig signing
- BIP86 key derivation - Still needed for escrow addresses

### 5.2 New Keymeld WASM Wrapper

```rust
// coordinator-wasm/src/keymeld.rs
use keymeld_sdk::{KeyMeldClient, credentials::SessionCredentials};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct KeymeldWasm {
    client: KeyMeldClient,
}

#[wasm_bindgen]
impl KeymeldWasm {
    #[wasm_bindgen(constructor)]
    pub fn new(url: &str, user_id: &str, session_secret_hex: &str) -> Result<KeymeldWasm, JsValue> {
        let secret: [u8; 32] = hex::decode(session_secret_hex)
            .map_err(|e| JsValue::from_str(&e.to_string()))?
            .try_into()
            .map_err(|_| JsValue::from_str("Invalid secret length"))?;
            
        let credentials = SessionCredentials::from_session_secret(&secret)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
            
        let client = KeyMeldClientBuilder::new(url)
            .with_user_id(user_id.into())
            .with_session_credentials(credentials)
            .build()
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
            
        Ok(Self { client })
    }
    
    #[wasm_bindgen]
    pub async fn join_keygen(&self, session_id: &str) -> Result<(), JsValue> {
        let session = self.client.keygen()
            .join_session(session_id.into())
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
            
        session.wait_for_completion().await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(())
    }
    
    #[wasm_bindgen]
    pub async fn approve_signing(&self, session_id: &str) -> Result<(), JsValue> {
        let session = self.client.signer()
            .get_session(session_id.into())
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
            
        session.approve().await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(())
    }
}
```

---

## Part 6: Session Secret Sharing

### 6.1 The Problem

- Coordinator creates keymeld session with `SessionCredentials::new()`
- Frontend needs the same credentials to participate
- Session secret is 32 bytes that derives all session keys
- Must be transmitted securely to the browser

### 6.2 Solution: NIP-44 Encryption

Since participants authenticate via Nostr (NIP-98), use NIP-44 encryption:

**Server-side:**
```rust
use nostr::nips::nip44;

fn encrypt_session_secret(
    coordinator_keys: &Keys,
    participant_pubkey: &PublicKey,
    session_secret: &[u8; 32],
) -> Result<String> {
    nip44::encrypt(
        coordinator_keys.secret_key()?,
        participant_pubkey,
        session_secret,
        nip44::Version::V2,
    )
}
```

**Client-side (WASM):**
```rust
pub fn decrypt_session_secret(
    user_keys: &Keys,
    coordinator_pubkey: &str,
    encrypted: &str,
) -> Result<[u8; 32], JsValue> {
    let coordinator_pk = PublicKey::from_hex(coordinator_pubkey)?;
    let decrypted = nip44::decrypt(user_keys.secret_key()?, &coordinator_pk, encrypted)?;
    decrypted.try_into()
}
```

---

## Part 7: Frontend Architecture (Maud + HTMX)

### 7.1 Hybrid Approach

**Use Maud + HTMX for:**
- Competition table rendering
- Entry list updates
- Status displays
- Admin interfaces

**Keep minimal JS + WASM for:**
- Nostr authentication (requires user's private key)
- Escrow PSBT signing (requires user's private key)
- Keymeld session participation (requires session credentials)

### 7.2 Example Maud Template

```rust
// api/views/competitions.rs
use maud::{html, Markup};

pub fn competition_row(comp: &Competition) -> Markup {
    html! {
        tr hx-get={"/competitions/" (comp.id)} 
           hx-trigger="every 5s"
           hx-swap="outerHTML" {
            td { (comp.name) }
            td { (comp.entries.len()) "/" (comp.max_entries) }
            td class="status" { (comp.state.display_name()) }
            td { (comp.entry_fee_sats) " sats" }
            td {
                @if comp.can_join() {
                    button hx-post={"/competitions/" (comp.id) "/join"}
                           hx-target="#entry-modal" {
                        "Join"
                    }
                }
            }
        }
    }
}
```

---

## Part 8: Admin UI (Full Maud + HTMX)

The admin UI requires no client-side cryptography, making it ideal for 100% server-side rendering.

```rust
// api/views/admin/wallet.rs
pub fn wallet_page() -> Markup {
    html! {
        div class="columns" {
            div class="column" {
                div class="box" 
                     hx-get="/admin/wallet/balance"
                     hx-trigger="load, every 30s" {
                    "Loading..."
                }
            }
            div class="column" {
                (send_form())
            }
        }
    }
}

pub fn balance_fragment(confirmed: u64, unconfirmed: u64) -> Markup {
    html! {
        h2 class="subtitle" { "Balance" }
        p class="title" { (confirmed) " sats" }
        p class="subtitle" { "(" (unconfirmed) " unconfirmed)" }
    }
}
```

---

## Implementation Order

1. **Part 1: Project Structure** - Reorganize files, create workspace
2. **Part 2: Nix Build** - Set up flake.nix, remove Doppler
3. **Part 3: Typestate Machine** - Refactor coordinator.rs
4. **Part 4: Server Keymeld** - Integrate keymeld-sdk
5. **Part 5: WASM Keymeld** - Update coordinator-wasm
6. **Part 6: Session Secrets** - Implement NIP-44 sharing
7. **Part 7: Frontend** - Add Maud + HTMX
8. **Part 8: Admin UI** - Full server-side rendering

---

## Appendix A: API Changes

### New Endpoints

```
POST /api/competitions/{id}/keymeld/join
  Response: { entry_id, keymeld_user_id, keymeld_session_id, encrypted_session_secret }

GET /api/competitions/{id}/keymeld/status
  Response: { keygen_status, signing_status, participant_approvals }
```

### Deprecated Endpoints

```
POST /api/competitions/{id}/nonces      // Replaced by keymeld
POST /api/competitions/{id}/signatures  // Replaced by keymeld
```

---

## Appendix B: Keymeld SDK Quick Reference

```rust
// Initialization
let client = KeyMeldClientBuilder::new(url)
    .with_user_credentials(creds)
    .build()?;

// Keygen with DLC subsets
let subsets = DlcSubsetBuilder::new(&contract)
    .market_maker(mm_id)
    .players(player_ids)
    .build()?;

let keygen = client.keygen()
    .create_session_with_subsets(participants, subsets, opts)
    .await?;

// Batch signing
let items = DlcBatchBuilder::new(&signing_data)
    .with_subset_ids(&keygen.subset_ids)
    .build()?;

let signing = client.signer()
    .sign_batch(&keygen, items, opts)
    .await?;
```
