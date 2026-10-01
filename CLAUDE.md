# gb-emu

A Game Boy (DMG) emulator in Rust that runs in the browser via WebAssembly.

## Layout

- `crates/gb-core/` — the emulator. Pure Rust, no I/O, no `unsafe`, no dependencies.
  - `cpu.rs` SM83 CPU (registers, fetch/decode/execute)
  - `bus.rs` memory map; routes every read/write; IF/IE; serial; OAM DMA
  - `cartridge.rs` header parsing + MBCs (ROM-only, MBC1)
  - `ppu.rs` scanline timing, STAT/LY, framebuffer (scanline renderer: background, window, sprites)
  - `timer.rs` DIV/TIMA/TMA/TAC
  - `joypad.rs` $FF00
  - `tests/smoke.rs` end-to-end tests through the public `GameBoy` API
- `crates/gb-cli/` — headless runner for test ROMs; `--doctor` writes Gameboy Doctor traces
- `crates/gb-wasm/` — wasm-bindgen wrapper (`Emulator` class) used by `web/`
- `web/` — static frontend (`index.html`, `main.js`, `style.css`); `web/pkg/` is generated
- `scripts/build-web.ps1` / `build-web.sh` — build `web/pkg`
- `scripts/serve.js` — zero-dependency Node dev server for `web/` (port 8765)
- `roms/` — test ROMs, git-ignored. Never commit ROM files.
- `docs/ROADMAP.md` — milestones and where we are

## Commands

```sh
cargo test --workspace                                  # all tests; must pass before any commit
cargo clippy --workspace --all-targets -- -D warnings  # lint; keep it clean
cargo run --release -p gb-cli -- <rom.gb>              # run a test ROM headlessly
cargo run --release -p gb-cli -- <rom.gb> --doctor trace.log   # CPU trace for Gameboy Doctor
./scripts/build-web.ps1                                 # build the browser version (Windows)
node scripts/serve.js                                   # serve it at http://localhost:8765
```

gb-cli exit codes: 0 passed, 1 failed (Blargg "Failed" or Mooneye fail bytes), 2 emulator/usage error
(e.g. illegal opcode), 3 frame limit reached without a verdict.

## Conventions

- Cycle counts are **T-cycles** (4.194304 MHz; 1 M-cycle = 4 T-cycles). `Cpu::step` returns them.
- Hardware behavior follows Pan Docs (https://gbdev.io/pandocs/) and the opcode table
  (https://gbdev.io/gb-opcodes/optables/). Cite the relevant Pan Docs page in a doc
  comment when implementing a non-obvious behavior.
- Every new instruction group or hardware behavior gets a unit test next to the code.
  Flag behavior (especially H and C on 8-bit vs 16-bit ops) gets explicit tests.
- Never `panic!`/`todo!()` in emulation paths; return a `CpuError` instead, so the CLI
  and browser can report what went wrong (e.g. `CpuError::Illegal` for the 11 illegal opcodes).
- Accuracy shortcuts are fine but must be marked `TODO(accuracy): ...` with what real hardware does.
- Unfinished roadmap work is marked `TODO(milestone N)`. Search for these to find next steps.
- `gb-core` stays platform-free: anything touching files, time, audio devices or the DOM
  belongs in `gb-cli` or `gb-wasm`/`web`.
- `wasm-bindgen` is pinned with `=`; `wasm-bindgen-cli` must be the same version.

## Debugging workflow

1. Run the failing test ROM with gb-cli. Blargg ROMs print the failing case over serial.
2. For CPU bugs, use Gameboy Doctor (https://github.com/robert/gameboy-doctor):
   `cargo run --release -p gb-cli -- "roms/blargg/cpu_instrs/individual/03-op sp,hl.gb" --doctor trace.log`
   then `python gameboy-doctor trace.log cpu_instrs 3`. It reports the first diverging line.
   Doctor mode forces LY ($FF44) to read $90; that's expected. The CLI writes the trace
   file itself because Windows PowerShell's `>` would save it as UTF-16.
3. Write a unit test that reproduces the bug before fixing it.

## Collaboration style

I'm building this to have something fun to show, and to understand how the hardware works.
- Work one roadmap step at a time; don't jump ahead to later milestones unasked.
- Before implementing a hardware component, give a short explanation of how the real
  hardware behaves (a few sentences, not an essay).
- After finishing a step, update the checkboxes in `docs/ROADMAP.md`.
