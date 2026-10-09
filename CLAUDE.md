# gb-emu

A Game Boy (DMG) and Game Boy Color (CGB) emulator in Rust that runs in the browser via WebAssembly.

## Layout

- `crates/gb-core/` — the emulator. Pure Rust, no I/O, no `unsafe`, no dependencies. Two models
  (`Model::Dmg`, `Model::Cgb`), picked from the cartridge header unless overridden.
  - `cpu.rs` SM83 CPU (registers, fetch/decode/execute). Each memory access is its own M-cycle
    (`Cpu::read`/`write`/`idle`), which first ticks the rest of the hardware 4 T-cycles; some
    I/O registers take a write a dot or two off the M-cycle's end (`bus::WriteTiming`)
  - `bus.rs` memory map; routes every read/write; IF/IE; serial; OAM DMA; the Color's WRAM banks,
    speed switch and VRAM DMA (HDMA). The CPU goes through `cpu_read`/`cpu_write`, which lock it
    out of OAM/VRAM while DMA or the PPU has them; `read`/`write` are the raw view (debugger, DMA)
  - `cartridge.rs` header parsing + MBCs (ROM-only, MBC1, MBC2, MBC3 with its real-time clock, MBC5)
  - `compat.rs` a Color running an original cartridge ("DMG compatibility mode"): what its boot
    ROM leaves (palettes, registers). `Bus::compat` shuts the Color's own registers; the PPU's
    `cgb()` means Color features are on, `model` which console it is (timing quirks)
  - `ppu/mod.rs` line timing, STAT/LY, OAM/VRAM locks, palettes, framebuffer; debugger pictures of
    VRAM (tile sheet, tile maps). `ppu/fifo.rs` mode 3: the fetcher and pixel FIFOs drawing a line
    pixel by pixel (timings after SameBoy's PPU). `ppu/oam_bug.rs` the original's OAM corruption bug
  - `timer.rs` DIV/TIMA/TMA/TAC
  - `apu.rs` sound: four channels counting in the APU's own 2 MHz ticks (timings and quirks after
    SameBoy's APU), frame sequencer (length, envelope, sweep) on both DIV-APU edges, the Color's
    PCM12/PCM34, mixer, high-pass filter
  - `joypad.rs` $FF00
  - `serial.rs` SB/SC and the link cable: real transfer timing; the host carries bytes to a
    partner (`GameBoy::take_link_out` / `link_answer` / `link_clocked`, `FrameEnd::LinkWait`)
  - `disasm.rs` disassembler for the debugger panel (same x/y/z decoding as `cpu.rs`)
  - `rewind.rs` rewind history: recent states as XOR deltas, newest first
  - `state.rs` save state format; each component has `save_state`/`load_state` next to its
    fields. Adding state to a component means adding it there too and bumping `state::VERSION`
  - `tests/smoke.rs` end-to-end tests through the public `GameBoy` API
- `crates/gb-cli/` — headless runner for test ROMs; `--doctor` writes Gameboy Doctor traces, `--wav` records audio
- `crates/gb-wasm/` — wasm-bindgen wrapper (`Emulator` class) used by `web/`
- `web/` — static frontend (`index.html`, `main.js`, `style.css`; `input.js` maps keyboard/gamepad/touch to buttons, `timing.js` paces frames, `saves.js` keeps battery saves in localStorage, `states.js` save state slots in IndexedDB, `console.js` the Console panel (which console original games run on, the Color's palettes for them), `link.js` the link cable (carries bytes to a partner), `rtc.js` its WebRTC connection (invite/reply codes), `audio.js` + `audio-worklet.js` + `audio-queue.js` play sound, `debugger.js` + `memview.js` + `vramview.js` + `breakpoints.js` are the debugger panel); `web/pkg/` is generated
- `scripts/build-web.ps1` / `build-web.sh` — build `web/pkg`
- `.cargo/config.toml` — on Windows (GNU) link with Rust's bundled MinGW linker; MSYS2's ld
  crashes on Rust DLLs
- `scripts/serve.js` — zero-dependency Node dev server for `web/` (port 8765)
- `homebrew/` — our own Game Boy games in RGBDS assembly (`link-pong/`: two-player Pong over the
  link cable); `scripts/build-homebrew.js` builds them into `web/games/` (build products, like
  `web/pkg/`). `crates/gb-core/tests/link_pong.rs` plays it on two linked Game Boys
- `.github/workflows/ci.yml` — tests every push; pushes to `main` also deploy `web/` to GitHub Pages
  (https://tanakriteuro.github.io/gb-emu/)
- `roms/` — test ROMs, git-ignored. Never commit ROM files.
- `docs/ROADMAP.md` — milestones and where we are

## Commands

```sh
cargo test --workspace                                  # all tests; must pass before any commit
cargo clippy --workspace --all-targets -- -D warnings  # lint; keep it clean
node --test "web/*.test.js"                             # frontend tests (input, timing, saves, states, link, rtc codes, audio, debugger)
cargo run --release -p gb-cli -- <rom.gb>              # run a test ROM headlessly
cargo run --release -p gb-cli -- <rom.gb> --doctor trace.log   # CPU trace for Gameboy Doctor
cargo run --release -p gb-cli -- <rom.gb> --wav out.wav        # record the sound (48 kHz WAV)
cargo run --release -p gb-cli -- <rom.gb> --model dmg           # force a model (dmg or cgb)
cargo run --release -p gb-cli -- <rom.gb> --screenshot out.ppm  # save the last frame (PPM)
cargo run --release -p gb-cli -- <rom.gb> --press 30:a+start    # hold buttons 5 frames from frame 30
./scripts/build-web.ps1                                 # build the browser version (Windows)
node scripts/serve.js                                   # serve it at http://localhost:8765
node scripts/build-homebrew.js                          # build homebrew/ into web/games/ (needs RGBDS)
```

gb-cli exit codes: 0 passed, 1 failed (Blargg "Failed" over serial or a failure code at $A000, or Mooneye
fail bytes, or the 2016 Mooneye exit $ED without the pass registers), 2 emulator/usage error
(e.g. illegal opcode), 3 frame limit reached without a verdict. Tests that report in the registers
(AGE, SameSuite at `LD B,B`; Wilbert Pol's Mooneye at $ED) pass with B C D E H L = 3 5 8 13 21 34.

## Conventions

- Cycle counts are **T-cycles** (4.194304 MHz; 1 M-cycle = 4 T-cycles). `Cpu::step` returns them.
  The CPU ticks the bus itself as it goes, so nothing else ticks for its cycles; the opcode
  table's counts are checked against the M-cycles spent on every instruction the unit tests run.
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
