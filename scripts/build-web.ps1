# Builds the WebAssembly package into web/pkg (Windows / PowerShell).
#
# One-time setup:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.129 --locked
#
# Then serve web/ and open http://localhost:8765:
#   node scripts/serve.js

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

cargo build -p gb-wasm --release --target wasm32-unknown-unknown
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

wasm-bindgen --target web --no-typescript --out-dir web/pkg `
  target/wasm32-unknown-unknown/release/gb_wasm.wasm
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "Built web/pkg. Serve the web/ folder and open it in a browser."
