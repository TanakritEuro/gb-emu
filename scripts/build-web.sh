#!/usr/bin/env sh
# Builds the WebAssembly package into web/pkg (macOS / Linux / Git Bash).
#
# One-time setup:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.129 --locked
#
# Then: node scripts/serve.js   and open http://localhost:8765
set -e
cd "$(dirname "$0")/.."

cargo build -p gb-wasm --release --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript --out-dir web/pkg \
  target/wasm32-unknown-unknown/release/gb_wasm.wasm

echo "Built web/pkg. Serve the web/ folder and open it in a browser."
