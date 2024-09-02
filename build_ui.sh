#!/bin/bash

# Build the WASM module
wasm-pack build --target web --out-dir ../../crates/public_ui/dist crates/client_validator
