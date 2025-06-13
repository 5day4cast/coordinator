#!/bin/bash

# Define base paths
DOPPLER_PATH="$HOME/.doppler/v0.4.3/data"
TARGET_PATH="crates/server/test_data"
CREDS_PATH="./creds"

# Create necessary directories
mkdir -p "$TARGET_PATH/alice_ln" "$TARGET_PATH/bob_ln" "$TARGET_PATH/coord_ln" "$CREDS_PATH"

# Copy admin.macaroon files
cp "$DOPPLER_PATH/alice/.lnd/data/chain/bitcoin/regtest/admin.macaroon" "$TARGET_PATH/alice_ln/"
cp "$DOPPLER_PATH/bob/.lnd/data/chain/bitcoin/regtest/admin.macaroon" "$TARGET_PATH/bob_ln/"
cp "$DOPPLER_PATH/coord/.lnd/data/chain/bitcoin/regtest/admin.macaroon" "$TARGET_PATH/coord_ln/"

# Copy tls.cert files
cp "$DOPPLER_PATH/bob/.lnd/tls.cert" "$TARGET_PATH/bob_ln/"
cp "$DOPPLER_PATH/alice/.lnd/tls.cert" "$TARGET_PATH/alice_ln/"
cp "$DOPPLER_PATH/coord/.lnd/tls.cert" "$TARGET_PATH/coord_ln/"

# Copy additional files to creds directory
cp "$DOPPLER_PATH/coord/.lnd/tls.cert" "$CREDS_PATH/"
cp "$DOPPLER_PATH/coord/.lnd/data/chain/bitcoin/regtest/admin.macaroon" "$CREDS_PATH/"

echo "All files copied successfully!"
