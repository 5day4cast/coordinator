#!/usr/bin/env bash
# Build the browser module (coordinator-wasm) the way it ships: the
# `wasm-release` Cargo profile, wasm-bindgen, then wasm-opt.
#
# Usage: scripts/build-wasm.sh <out-dir>
#
# Needs the wasm32-unknown-unknown target, wasm-bindgen at the version in
# Cargo.lock, and wasm-opt from binaryen version 125. The flake's
# `coordinator-wasm` package runs the same steps; keep the two in step.
set -euo pipefail

out=${1:?usage: scripts/build-wasm.sh <out-dir>}
root=$(cd "$(dirname "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}

cargo build --locked --manifest-path "$root/Cargo.toml" \
  --profile wasm-release --target wasm32-unknown-unknown -p coordinator-wasm
wasm-bindgen --target web --out-dir "$out" --out-name coordinator_wasm \
  "$target_dir/wasm32-unknown-unknown/wasm-release/coordinator_wasm.wasm"
wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-multivalue \
  "$out/coordinator_wasm_bg.wasm" -o "$out/coordinator_wasm_bg.wasm"
