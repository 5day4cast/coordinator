#!/bin/bash
set -e

echo "Starting custom build..."
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "Starting custom build..."
echo "CARGO_DIST_TARGET=${CARGO_DIST_TARGET}"
echo "Project root: ${PROJECT_ROOT}"

# Install wasm-pack
if ! command -v wasm-pack &> /dev/null; then
    echo "Installing wasm-pack..."
    curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
fi

# Build WASM
echo "Building WASM..."
wasm-pack build --target web --out-dir ../../crates/public_ui/dist "${PROJECT_ROOT}/crates/client_validator"

echo "Build completed"
