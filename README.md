# gb-emu

A Game Boy emulator written in Rust, running in the browser through WebAssembly.

Status: early. The CPU runs a handful of instructions; see [docs/ROADMAP.md](docs/ROADMAP.md).

## Setup (once)

1. Install Rust: https://rustup.rs
2. Add the WebAssembly target and the bindings tool (the version must match `crates/gb-wasm/Cargo.toml`):

   ```sh
   rustup target add wasm32-unknown-unknown
   cargo install wasm-bindgen-cli --version 0.2.129 --locked
   ```

3. Download test ROMs from https://github.com/c-sp/game-boy-test-roms/releases and unzip into `roms/`.

## Run

```sh
cargo test --workspace                       # tests
cargo run --release -p gb-cli -- <rom.gb>    # headless, prints test results
```

Browser version (Windows):

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-web.ps1
node scripts/serve.js
```

macOS / Linux: `./scripts/build-web.sh`, then `node scripts/serve.js`.

Open http://localhost:8765 and drop a `.gb` file onto the screen.
(`serve.js` needs only Node. Any static server works, e.g. `python -m http.server 8765 -d web`;
avoid port 8080 on Windows, which is often reserved.)

Controls: arrows = d-pad, X = A, Z = B, Enter = Start, Shift = Select.

## Working with Claude Code

`CLAUDE.md` describes the project for Claude Code. Some good ways to start:

- "Read docs/ROADMAP.md and start milestone 1: set up the opcode decoder."
- `/test-rom roms/blargg/cpu_instrs/individual/06-ld r,r.gb` runs a test ROM and investigates whatever it reports.
