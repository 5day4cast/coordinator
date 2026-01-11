# Coordinator Development Commands
# Run `just help` to see all available commands

# Default recipe - show help
default:
    @just --list

# ============================================
# Build Commands
# ============================================

# Build the entire workspace
build:
    cargo build --workspace

# Build in release mode
build-release:
    cargo build --workspace --release

# Build the coordinator binary only
build-coordinator:
    cargo build --bin coordinator

# Build the wallet CLI only
build-wallet-cli:
    cargo build --bin wallet-cli

# Build WASM module
build-wasm:
    wasm-pack build crates/coordinator-wasm --target web --out-dir ../../pkg

# Build WASM module in release mode
build-wasm-release:
    wasm-pack build crates/coordinator-wasm --target web --release --out-dir ../../pkg

# ============================================
# Test Commands
# ============================================

# Run all tests
test:
    cargo test --workspace

# Run tests with output
test-verbose:
    cargo test --workspace -- --nocapture

# Run a specific test
test-one name:
    cargo test --workspace {{name}} -- --nocapture

# Run integration tests only
test-integration:
    cargo test --workspace --test '*'

# Run unit tests only
test-unit:
    cargo test --workspace --lib

# ============================================
# Code Quality
# ============================================

# Run clippy linter
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Format code
fmt:
    cargo fmt --all

# Check formatting without modifying
fmt-check:
    cargo fmt --all -- --check

# Run all checks (format, clippy, test)
check-all: fmt-check clippy test

# ============================================
# Database Commands
# ============================================

# Run database migrations
migrate:
    sqlx migrate run --source crates/coordinator/migrations/competitions
    sqlx migrate run --source crates/coordinator/migrations/users

# Create a new migration
migrate-add name:
    sqlx migrate add -r {{name}} --source crates/coordinator/migrations/competitions

# Prepare SQLx offline data
sqlx-prepare:
    cargo sqlx prepare --workspace

# ============================================
# Development Stack
# ============================================

# Start all development services (bitcoin, lnd, keymeld)
start:
    start-all

# Stop all development services
stop:
    stop-all

# Restart all services
restart: stop start

# Clean data directories and restart
fresh: stop
    clean-data
    @just start

# ============================================
# Individual Services
# ============================================

# Start Bitcoin regtest
bitcoin-start:
    start-regtest

# Stop Bitcoin
bitcoin-stop:
    stop-regtest

# Mine N blocks (default: 1)
mine n="1":
    mine-blocks {{n}}

# Start LND nodes
lnd-start:
    setup-lnd

# Setup channels between LND nodes
lnd-channels:
    setup-channels

# Stop LND nodes
lnd-stop:
    stop-lnd

# Start keymeld (gateway + enclaves)
keymeld-start:
    run-keymeld

# Stop keymeld
keymeld-stop:
    stop-keymeld

# ============================================
# Run Commands
# ============================================

# Run the coordinator server
run:
    cargo run --bin coordinator

# Run coordinator with debug logging
run-debug:
    RUST_LOG=debug cargo run --bin coordinator

# Run coordinator with trace logging
run-trace:
    RUST_LOG=trace cargo run --bin coordinator

# Run the wallet CLI
wallet *args:
    cargo run --bin wallet-cli -- {{args}}

# ============================================
# E2E Testing (Rust integration tests)
# ============================================

# Run e2e tests (starts services, runs tests, stops services)
e2e: start
    @echo "Waiting for services to be ready..."
    @sleep 5
    cargo test --test e2e -- --test-threads=1 || (just stop && exit 1)
    @just stop

# Run e2e tests without managing services (assumes services are running)
e2e-only:
    cargo test --test e2e -- --test-threads=1

# ============================================
# Playwright E2E Tests (browser-based)
# ============================================

# Install Playwright dependencies
playwright-install:
    cd e2e && npm install && npx playwright install chromium

# Run Playwright tests (services must be running)
playwright:
    cd e2e && npm test

# Run Playwright tests with visible browser
playwright-headed:
    cd e2e && npm run test:headed

# Run Playwright tests with interactive UI
playwright-ui:
    cd e2e && npm run test:ui

# Run Playwright tests in debug mode
playwright-debug:
    cd e2e && npm run test:debug

# Generate Playwright test code by recording
playwright-codegen:
    cd e2e && npm run codegen http://localhost:9990

# ============================================
# Utility Commands
# ============================================

# Clean build artifacts
clean:
    cargo clean

# Clean data and logs directories
clean-data:
    clean-data

# Clean everything (build artifacts + data)
clean-all: clean clean-data

# Show service status
status:
    @echo "=== Bitcoin ==="
    @bitcoin-cli -datadir=data/bitcoin -rpcuser=coordinator -rpcpassword=coordinatorpass getblockchaininfo 2>/dev/null | jq '{blocks, chain}' || echo "Not running"
    @echo ""
    @echo "=== LND1 (coordinator) ==="
    @lncli --lnddir=data/lnd1 --rpcserver=127.0.0.1:10009 --network=regtest getinfo 2>/dev/null | jq '{alias, num_active_channels, synced_to_chain}' || echo "Not running"
    @echo ""
    @echo "=== LND2 (participant) ==="
    @lncli --lnddir=data/lnd2 --rpcserver=127.0.0.1:10010 --network=regtest getinfo 2>/dev/null | jq '{alias, num_active_channels, synced_to_chain}' || echo "Not running"
    @echo ""
    @echo "=== Keymeld ==="
    @curl -s http://127.0.0.1:8090/health 2>/dev/null && echo "Running" || echo "Not running"

# View logs for a service
logs service:
    @if [ -f "logs/{{service}}.log" ]; then \
        tail -f "logs/{{service}}.log"; \
    else \
        echo "Log file not found: logs/{{service}}.log"; \
        echo "Available logs:"; \
        ls -la logs/ 2>/dev/null || echo "No logs directory"; \
    fi

# ============================================
# CI Commands
# ============================================

# Run CI checks (what CI will run)
ci: fmt-check clippy test

# Build for CI (release mode)
ci-build:
    cargo build --workspace --release --locked

# ============================================
# Help
# ============================================

# Show detailed help
help:
    @echo "Coordinator Development Commands"
    @echo "================================="
    @echo ""
    @echo "Quick Start:"
    @echo "  just start     - Start all services (bitcoin, lnd, keymeld)"
    @echo "  just run       - Run the coordinator server"
    @echo "  just stop      - Stop all services"
    @echo ""
    @echo "Development:"
    @echo "  just build     - Build the project"
    @echo "  just test      - Run tests"
    @echo "  just check-all - Run all code quality checks"
    @echo ""
    @echo "E2E Testing (Rust):"
    @echo "  just e2e       - Run full e2e tests (manages services)"
    @echo "  just e2e-only  - Run e2e tests (services must be running)"
    @echo ""
    @echo "Playwright Testing (Browser):"
    @echo "  just playwright-install - Install Playwright and browsers"
    @echo "  just playwright         - Run Playwright tests"
    @echo "  just playwright-headed  - Run with visible browser"
    @echo "  just playwright-ui      - Run with interactive UI"
    @echo ""
    @echo "Services:"
    @echo "  just status    - Show status of all services"
    @echo "  just logs X    - Tail logs for service X (lnd1, lnd2, gateway, enclave-0, etc.)"
    @echo "  just mine N    - Mine N blocks"
    @echo ""
    @echo "Run 'just --list' to see all available commands"
