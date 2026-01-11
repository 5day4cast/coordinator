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

    # Keymeld for e2e testing
    keymeld = {
      url = "github:tee8z/keymeld";
    };
  };

  nixConfig = {
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

        # Use Rust 1.92.0 to match current toolchain
        rustToolchain = pkgs.rust-bin.stable."1.85.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
          targets = [ "wasm32-unknown-unknown" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Common environment variables
        commonEnvs = {
          SQLX_OFFLINE = "true";
          RUST_LOG = "info";
          CARGO_INCREMENTAL = "1";
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        };

        # Common build dependencies
        commonNativeBuildInputs = with pkgs; [
          pkg-config
          cmake
          perl  # Required for openssl-sys build
          llvmPackages.libclang  # Required for bindgen (sqlite3-sys)
        ];

        commonBuildInputs = with pkgs; [
          openssl
          sqlite
          curl
        ];

        # Source filtering for Rust builds
        src = pkgs.lib.cleanSourceWith {
          src = craneLib.path ./.;
          filter = path: type:
            (craneLib.filterCargoSources path type)
            || (builtins.match ".*\\.sql$" path != null)
            || (builtins.match ".*migrations.*" path != null)
            || (builtins.match ".*\\.html$" path != null)
            || (builtins.match ".*\\.js$" path != null)
            || (builtins.match ".*\\.css$" path != null);
        };

        # Build workspace dependencies once (for caching)
        workspaceDeps = craneLib.buildDepsOnly ({
          pname = "coordinator-workspace-deps";
          version = "0.1.0";
          inherit src;
          buildInputs = commonBuildInputs;
          nativeBuildInputs = commonNativeBuildInputs;
        } // commonEnvs);

        # Main coordinator binary
        coordinator = craneLib.buildPackage ({
          pname = "coordinator";
          version = "0.1.0";
          inherit src;
          cargoArtifacts = workspaceDeps;
          buildInputs = commonBuildInputs;
          nativeBuildInputs = commonNativeBuildInputs;
          cargoExtraArgs = "--bin coordinator";

          postInstall = ''
            mkdir -p $out/share/coordinator
            cp -r crates/coordinator/migrations $out/share/coordinator/
            if [ -d crates/coordinator/frontend ]; then
              cp -r crates/coordinator/frontend $out/share/coordinator/
            fi
          '';
        } // commonEnvs);

        # Wallet CLI binary
        wallet-cli = craneLib.buildPackage ({
          pname = "wallet-cli";
          version = "0.1.0";
          inherit src;
          cargoArtifacts = workspaceDeps;
          buildInputs = commonBuildInputs;
          nativeBuildInputs = commonNativeBuildInputs;
          cargoExtraArgs = "--bin wallet-cli";
        } // commonEnvs);

        # Get keymeld binaries from the keymeld flake
        keymeld-gateway = keymeld.packages.${system}.keymeld-gateway;
        keymeld-enclave = keymeld.packages.${system}.keymeld-enclave;

        # ============================================
        # Helper Scripts for Development Environment
        # ============================================

        # Script: Start Bitcoin regtest
        start-regtest = pkgs.writeShellScriptBin "start-regtest" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          LOGS_DIR="''${LOGS_DIR:-$PWD/logs}"
          BITCOIN_DIR="$DATA_DIR/bitcoin"

          mkdir -p "$BITCOIN_DIR" "$LOGS_DIR"

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
          zmqpubrawblock=tcp://127.0.0.1:28332
          zmqpubrawtx=tcp://127.0.0.1:28333
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

        # Script: Stop Bitcoin
        stop-regtest = pkgs.writeShellScriptBin "stop-regtest" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"

          ${pkgs.bitcoin}/bin/bitcoin-cli \
            -datadir="$DATA_DIR/bitcoin" \
            -rpcuser=coordinator \
            -rpcpassword=coordinatorpass \
            stop 2>/dev/null || echo "bitcoind not running"

          echo "Bitcoin stopped"
        '';

        # Script: Mine blocks
        mine-blocks = pkgs.writeShellScriptBin "mine-blocks" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          COUNT="''${1:-1}"

          # Get an address from the wallet
          ADDR=$(${pkgs.bitcoin}/bin/bitcoin-cli \
            -datadir="$DATA_DIR/bitcoin" \
            -rpcuser=coordinator \
            -rpcpassword=coordinatorpass \
            getnewaddress)

          ${pkgs.bitcoin}/bin/bitcoin-cli \
            -datadir="$DATA_DIR/bitcoin" \
            -rpcuser=coordinator \
            -rpcpassword=coordinatorpass \
            generatetoaddress "$COUNT" "$ADDR"

          echo "Mined $COUNT blocks"
        '';

        # Script: Setup LND nodes with channels
        setup-lnd = pkgs.writeShellScriptBin "setup-lnd" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          LOGS_DIR="''${LOGS_DIR:-$PWD/logs}"

          LND1_DIR="$DATA_DIR/lnd1"
          LND2_DIR="$DATA_DIR/lnd2"

          mkdir -p "$LND1_DIR" "$LND2_DIR" "$LOGS_DIR"

          BITCOIN_CLI="${pkgs.bitcoin}/bin/bitcoin-cli -datadir=$DATA_DIR/bitcoin -rpcuser=coordinator -rpcpassword=coordinatorpass"

          echo "Starting LND node 1 (coordinator)..."

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
            --debuglevel=info \
            > "$LOGS_DIR/lnd1.log" 2>&1 &
          echo $! > "$DATA_DIR/lnd1.pid"

          echo "Starting LND node 2 (participant)..."

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
            --debuglevel=info \
            > "$LOGS_DIR/lnd2.log" 2>&1 &
          echo $! > "$DATA_DIR/lnd2.pid"

          echo "Waiting for LND nodes to initialize..."
          sleep 5

          # Wait for LND1 to be ready
          for i in {1..30}; do
            if ${pkgs.lnd}/bin/lncli --lnddir="$LND1_DIR" --rpcserver=127.0.0.1:10009 --network=regtest getinfo > /dev/null 2>&1; then
              echo "LND1 is ready!"
              break
            fi
            sleep 1
          done

          # Wait for LND2 to be ready
          for i in {1..30}; do
            if ${pkgs.lnd}/bin/lncli --lnddir="$LND2_DIR" --rpcserver=127.0.0.1:10010 --network=regtest getinfo > /dev/null 2>&1; then
              echo "LND2 is ready!"
              break
            fi
            sleep 1
          done

          echo ""
          echo "LND nodes started!"
          echo "  LND1 RPC: 127.0.0.1:10009 (coordinator)"
          echo "  LND1 REST: 127.0.0.1:8080"
          echo "  LND2 RPC: 127.0.0.1:10010 (participant)"
          echo "  LND2 REST: 127.0.0.1:8081"
          echo ""
          echo "Run 'setup-channels' to create channels between nodes"
        '';

        # Script: Setup channels between LND nodes
        setup-channels = pkgs.writeShellScriptBin "setup-channels" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"

          LND1_DIR="$DATA_DIR/lnd1"
          LND2_DIR="$DATA_DIR/lnd2"

          BITCOIN_CLI="${pkgs.bitcoin}/bin/bitcoin-cli -datadir=$DATA_DIR/bitcoin -rpcuser=coordinator -rpcpassword=coordinatorpass"
          LNCLI1="${pkgs.lnd}/bin/lncli --lnddir=$LND1_DIR --rpcserver=127.0.0.1:10009 --network=regtest"
          LNCLI2="${pkgs.lnd}/bin/lncli --lnddir=$LND2_DIR --rpcserver=127.0.0.1:10010 --network=regtest"

          echo "Getting node pubkeys..."
          LND1_PUBKEY=$($LNCLI1 getinfo | ${pkgs.jq}/bin/jq -r '.identity_pubkey')
          LND2_PUBKEY=$($LNCLI2 getinfo | ${pkgs.jq}/bin/jq -r '.identity_pubkey')

          echo "LND1 pubkey: $LND1_PUBKEY"
          echo "LND2 pubkey: $LND2_PUBKEY"

          # Fund LND1 wallet
          echo "Funding LND1 wallet..."
          LND1_ADDR=$($LNCLI1 newaddress p2wkh | ${pkgs.jq}/bin/jq -r '.address')
          $BITCOIN_CLI sendtoaddress "$LND1_ADDR" 10

          # Fund LND2 wallet
          echo "Funding LND2 wallet..."
          LND2_ADDR=$($LNCLI2 newaddress p2wkh | ${pkgs.jq}/bin/jq -r '.address')
          $BITCOIN_CLI sendtoaddress "$LND2_ADDR" 10

          # Mine blocks to confirm
          echo "Mining blocks to confirm funding..."
          mine-blocks 6

          # Wait for wallets to sync
          sleep 3

          # Connect LND1 to LND2
          echo "Connecting LND1 to LND2..."
          $LNCLI1 connect "$LND2_PUBKEY@127.0.0.1:9736" || true

          # Open channel from LND1 to LND2
          echo "Opening channel LND1 -> LND2 (1,000,000 sats)..."
          $LNCLI1 openchannel "$LND2_PUBKEY" 1000000

          # Mine blocks to confirm channel
          echo "Mining blocks to confirm channel..."
          mine-blocks 6

          # Wait for channel to be active
          echo "Waiting for channel to become active..."
          sleep 5

          echo ""
          echo "Channel setup complete!"
          $LNCLI1 listchannels | ${pkgs.jq}/bin/jq '.channels[] | {remote_pubkey, capacity, local_balance, remote_balance}'
        '';

        # Script: Stop LND nodes
        stop-lnd = pkgs.writeShellScriptBin "stop-lnd" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"

          for pidfile in "$DATA_DIR/lnd1.pid" "$DATA_DIR/lnd2.pid"; do
            if [ -f "$pidfile" ]; then
              pid=$(cat "$pidfile")
              if kill -0 "$pid" 2>/dev/null; then
                echo "Stopping LND (pid $pid)..."
                kill "$pid" || true
              fi
              rm -f "$pidfile"
            fi
          done

          echo "LND nodes stopped"
        '';

        # Script: Run keymeld for e2e tests
        run-keymeld = pkgs.writeShellScriptBin "run-keymeld" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          LOGS_DIR="''${LOGS_DIR:-$PWD/logs}"
          KEYMELD_DIR="$DATA_DIR/keymeld"

          mkdir -p "$KEYMELD_DIR" "$LOGS_DIR"

          echo "Starting keymeld enclaves (simulated)..."

          # Start 3 enclaves on different ports
          for i in 0 1 2; do
            port=$((5000 + i))
            VSOCK_PORT=$port \
            ENCLAVE_ID=$i \
            TEST_MODE=true \
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
          ENCLAVE_0_HOST=127.0.0.1 \
          ENCLAVE_0_PORT=5000 \
          ENCLAVE_1_HOST=127.0.0.1 \
          ENCLAVE_1_PORT=5001 \
          ENCLAVE_2_HOST=127.0.0.1 \
          ENCLAVE_2_PORT=5002 \
            ${keymeld-gateway}/bin/keymeld-gateway \
            > "$LOGS_DIR/gateway.log" 2>&1 &
          echo $! > "$KEYMELD_DIR/gateway.pid"

          # Wait for gateway to be ready
          echo "Waiting for keymeld gateway..."
          for i in {1..30}; do
            if ${pkgs.curl}/bin/curl -s http://127.0.0.1:8090/health > /dev/null 2>&1; then
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

        # Script: Stop keymeld
        stop-keymeld = pkgs.writeShellScriptBin "stop-keymeld" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          KEYMELD_DIR="$DATA_DIR/keymeld"

          for pidfile in "$KEYMELD_DIR"/*.pid; do
            if [ -f "$pidfile" ]; then
              pid=$(cat "$pidfile")
              if kill -0 "$pid" 2>/dev/null; then
                echo "Stopping process (pid $pid)..."
                kill "$pid" || true
              fi
              rm -f "$pidfile"
            fi
          done

          echo "Keymeld stopped"
        '';

        # Script: Start all services
        start-all = pkgs.writeShellScriptBin "start-all" ''
          set -e
          echo "Starting all development services..."
          echo ""

          start-regtest
          echo ""

          setup-lnd
          echo ""

          setup-channels
          echo ""

          run-keymeld
          echo ""

          echo "=========================================="
          echo "Development stack ready!"
          echo "=========================================="
          echo ""
          echo "Bitcoin RPC: http://coordinator:coordinatorpass@127.0.0.1:18443"
          echo "LND1 (coordinator): 127.0.0.1:10009, REST: 127.0.0.1:8080"
          echo "LND2 (participant): 127.0.0.1:10010, REST: 127.0.0.1:8081"
          echo "Keymeld Gateway: http://127.0.0.1:8090"
          echo ""
          echo "Use 'stop-all' to shut down all services"
        '';

        # Script: Stop all services
        stop-all = pkgs.writeShellScriptBin "stop-all" ''
          set -e
          echo "Stopping all services..."

          stop-keymeld || true
          stop-lnd || true
          stop-regtest || true

          echo "All services stopped"
        '';

        # Script: Clean data directories
        clean-data = pkgs.writeShellScriptBin "clean-data" ''
          set -e
          DATA_DIR="''${DATA_DIR:-$PWD/data}"
          LOGS_DIR="''${LOGS_DIR:-$PWD/logs}"

          echo "Cleaning data and logs directories..."
          rm -rf "$DATA_DIR" "$LOGS_DIR"
          echo "Done"
        '';

        # ============================================
        # Development Shell
        # ============================================
        devShell = pkgs.mkShell {
          buildInputs = commonBuildInputs ++ [
            rustToolchain

            # Build tools
            pkgs.just
            pkgs.jq
            pkgs.curl

            # Database
            pkgs.sqlite
            pkgs.sqlx-cli

            # WASM
            pkgs.wasm-pack
            pkgs.wasm-bindgen-cli

            # Bitcoin stack
            pkgs.bitcoind
            pkgs.bitcoin

            # Lightning
            pkgs.lnd

            # Utilities
            pkgs.socat
            pkgs.procps
            pkgs.netcat

            # Keymeld binaries for e2e testing
            keymeld-gateway
            keymeld-enclave

            # Helper scripts
            start-regtest
            stop-regtest
            mine-blocks
            setup-lnd
            setup-channels
            stop-lnd
            run-keymeld
            stop-keymeld
            start-all
            stop-all
            clean-data
          ];

          nativeBuildInputs = commonNativeBuildInputs;

          shellHook = ''
            export DATA_DIR="$PWD/data"
            export LOGS_DIR="$PWD/logs"
            mkdir -p "$DATA_DIR" "$LOGS_DIR"

            # Clear nix eval cache to prevent SQLite conflicts
            rm -rf ~/.cache/nix/eval-cache-* 2>/dev/null || true

            echo ""
            echo "Coordinator Development Environment"
            echo "===================================="
            echo ""
            echo "Quick start:"
            echo "  just start    - Start all services (bitcoin, lnd, keymeld)"
            echo "  just stop     - Stop all services"
            echo "  just test     - Run tests"
            echo "  just build    - Build the project"
            echo ""
            echo "Individual services:"
            echo "  start-regtest   - Start bitcoind in regtest mode"
            echo "  setup-lnd       - Start LND nodes"
            echo "  setup-channels  - Create channels between LND nodes"
            echo "  run-keymeld     - Start keymeld gateway + enclaves"
            echo "  mine-blocks N   - Mine N blocks"
            echo ""
            echo "Use 'just help' to see all available commands"
            echo ""
          '';

          inherit (commonEnvs) SQLX_OFFLINE RUST_LOG CARGO_INCREMENTAL;
        };


        # Docker image for k8s deployment
        docker-coordinator = pkgs.dockerTools.buildLayeredImage {
          name = "coordinator";
          tag = "latest";
          contents = [
            coordinator
            pkgs.cacert
            pkgs.tzdata
          ];
          config = {
            Cmd = [ "${coordinator}/bin/coordinator" ];
            Env = [
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "RUST_LOG=info"
            ];
            ExposedPorts = {
              "8080/tcp" = {};
            };
            WorkingDir = "/data";
            Volumes = {
              "/data" = {};
            };
          };
        };

      in {
        packages = {
          default = coordinator;
          inherit coordinator wallet-cli docker-coordinator;
          inherit start-regtest stop-regtest mine-blocks;
          inherit setup-lnd setup-channels stop-lnd;
          inherit run-keymeld stop-keymeld;
          inherit start-all stop-all clean-data;
        };

        devShells.default = devShell;

        # For CI: expose checks
        checks = {
          inherit coordinator;

          # Clippy check
          coordinator-clippy = craneLib.cargoClippy ({
            inherit src;
            cargoArtifacts = workspaceDeps;
            buildInputs = commonBuildInputs;
            nativeBuildInputs = commonNativeBuildInputs;
            cargoClippyExtraArgs = "--all-targets -- -D warnings";
          } // commonEnvs);

          # Format check
          coordinator-fmt = craneLib.cargoFmt {
            inherit src;
          };

          # Test check
          coordinator-test = craneLib.cargoTest ({
            inherit src;
            cargoArtifacts = workspaceDeps;
            buildInputs = commonBuildInputs;
            nativeBuildInputs = commonNativeBuildInputs;
          } // commonEnvs);
        };
      });
}
