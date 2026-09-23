#!/usr/bin/env bash
# Run the Linux release's cargo builds. The builder image precompiles dependencies with
# `cook`, and the release runs `build`; both pass the same arguments, so the release
# reuses every dependency the image compiled.
set -euo pipefail

fail() { printf 'release-cargo: %s\n' "$*" >&2; exit 1; }
[[ $# -eq 2 ]] || fail 'usage: release-cargo.sh cook|build linux|wasm'

case "$2" in
  # One invocation, so every binary resolves the same dependency features.
  linux) args=(--target x86_64-unknown-linux-gnu
    -p coordinator
    -p coordinator-verifier-enclave --features coordinator-verifier-enclave/lnurl
    -p coordinator-lnurl-relay -p coordinator-ark-swap -p coordinator-synth) ;;
  wasm)
    # secp256k1-sys's wasm sysroot lacks a memmove declaration.
    export CC_wasm32_unknown_unknown=clang
    export CFLAGS_wasm32_unknown_unknown="-Wno-error=implicit-function-declaration"
    args=(--target wasm32-unknown-unknown -p coordinator-wasm) ;;
  *) fail "unknown build: $2" ;;
esac

case "$1" in
  cook) exec cargo chef cook --recipe-path "${RECIPE:-recipe.json}" --release --locked "${args[@]}" ;;
  build) exec cargo build --release --locked "${args[@]}" ;;
  *) fail "unknown command: $1" ;;
esac
