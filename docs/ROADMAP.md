# Roadmap

From "unimplemented opcode 3E" to a Game Boy emulator you can send people a link to.
Each milestone ends with something you can see or a test ROM that passes.

Test ROMs: download the latest release of
[c-sp/game-boy-test-roms](https://github.com/c-sp/game-boy-test-roms) and unzip it into
`roms/`. It bundles Blargg's tests, Mooneye, dmg-acid2 and more.

---

## Milestone 0 — Toolchain and first run ✅ (scaffold)

- [x] Cargo workspace: `gb-core`, `gb-cli`, `gb-wasm`
- [x] Memory bus, cartridge header, MBC1, timer, joypad, PPU timing, serial capture
- [x] Headless runner with Gameboy Doctor trace output
- [x] Browser frontend that loads a ROM and reports errors on the page
- [x] **You:** install the wasm target and `wasm-bindgen-cli` (see README), build the web
      version, drop a ROM in, and see "unimplemented opcode …" appear

## Milestone 1 — The CPU ✅

The big one: all 256 base opcodes and 256 `CB`-prefixed opcodes (minus 11 illegal ones).

- [x] Decode by bit pattern instead of 500 match arms: split the opcode into
      `x = op >> 6`, `y = (op >> 3) & 7`, `z = op & 7` and most groups fall out
      (e.g. `x == 1` is `LD r, r'`, except `0x76` HALT). Search "decoding gbz80 opcodes".
- [x] Helpers for reading/writing an 8-bit operand by index (B C D E H L (HL) A)
- [x] 8-bit loads, 16-bit loads, `PUSH`/`POP` (remember F's low nibble is always 0)
- [x] 8-bit ALU with correct H and C flags; `DAA` last, it's notoriously fiddly
- [x] 16-bit arithmetic: `ADD HL,rr`, `ADD SP,e8`, `LD HL,SP+e8` (flags from the low byte!)
- [x] Jumps, calls, returns, `RST`, conditional cycle counts
- [x] `CB` prefix: rotates, shifts, `SWAP`, `BIT`/`RES`/`SET` (plus `RLCA`/`RRCA`/`RLA`/`RRA`, which share the rotate logic)
- [x] Gameboy Doctor clean on `cpu_instrs` individual 01, 03–11 (all pass; Doctor wasn't needed, the only failure was the missing RETI)

**Done when:** `cpu_instrs/individual/` 01 and 03–11 print "Passed".
(02 needs interrupts, which is the next milestone.)

## Milestone 2 — Interrupts and timing ✅

- [x] Interrupt dispatch in `Cpu::step`: priority order, push PC,
      clear IF bit and IME, 20 T-cycles
- [x] `HALT` wake-up rules, the HALT bug (IME=0 with a pending interrupt). (`RETI` itself landed in milestone 1: `cpu_instrs` 07 needs it.)
- [x] `STOP` ($10 $00): the combined `cpu_instrs.gb` runs it between tests. On DMG, treat it as a
      2-byte NOP for now (real hardware sleeps until a button press; `TODO(accuracy)` that)
- [x] Timer edge cases marked `TODO(accuracy)` (Mooneye `acceptance/timer`: 13/13)

**Done when:** `cpu_instrs.gb` (all 11 in one ROM) passes, and `instr_timing.gb` passes.

## Milestone 3 — Pixels ✅

- [x] Background: tile data, tile maps, SCX/SCY scrolling, BGP palette
- [x] Window layer (WX/WY, its own line counter)
- [x] Sprites: OAM scan (10 per line), 8×8 and 8×16, flips, OBP0/OBP1, priority (dmg-acid2 matches pixel for pixel)
- [x] STAT interrupts on mode changes; LCD off/on behavior (Mooneye `acceptance/ppu`: 3/12; the rest need variable mode 3 timing and the LCD-on/line-144 quirks)

**Done when:** `dmg-acid2.gb` matches its reference image pixel for pixel.
First homebrew title screen shows up in the browser. 🎉
(Both done: dmg-acid2 matches exactly; Tobu Tobu Girl from Homebrew Hub reaches its title screen.)

## Milestone 4 — Playable in the browser

- [x] Gamepad API support alongside the keyboard
- [x] Touch controls for phones
- [x] Pause, reset, speed toggle (fast-forward is very satisfying)
- [x] Zero-copy framebuffer (a `Uint8ClampedArray` view on wasm memory)

**Done when:** you can play a homebrew game from [Homebrew Hub](https://hh.gbdev.io/)
start to finish on your phone.
(Pending: all four steps are in, but the phone playthrough hasn't been done yet. Needs the
page reachable from a phone: `serve.js` on the LAN, or the GitHub Pages deploy in milestone 7.)

## Milestone 5 — More cartridges and saves

- [x] MBC3 with the real-time clock (Pokémon-style games use it) (rtc3test: all 3 suites match)
- [x] MBC5 (Mooneye `emulator-only/mbc5`: 8/8)
- [x] MBC2: 16 ROM banks, 512 × 4-bit RAM built into the chip (Mooneye `emulator-only/mbc2`: 7/7)
- [x] Battery saves: export/import `.sav`, keep them in the browser between visits

**Done when:** Mooneye's MBC tests pass and a save survives a page reload.
(Saves survive reloads, with the clock catching up. Mooneye MBC: 27/28; `mbc1/multicart_rom_8Mb`
needs MBC1 multicart wiring, which isn't planned.)

## Milestone 6 — Sound

- [x] Four channels: two square waves (one with sweep), wave, noise (plus mixer, high-pass filter, `gb-cli --wav`)
- [x] Frame sequencer: length, envelope, sweep (blargg `dmg_sound`: 9/12; 09, 10 and 12 need CH3 wave-RAM access while playing)
- [ ] Output through a Web Audio `AudioWorklet`; let audio drive timing to avoid crackle

**Done when:** music sounds right and doesn't pop.

## Milestone 7 — Show-off features

Pick whichever sound most fun:

- [ ] Deploy to GitHub Pages from CI, so the README has a "Play it" link
- [ ] Debugger panel: registers, memory hex view, VRAM tile viewer, breakpoints, step button
- [ ] Save states and rewind (hold a key to run time backwards)
- [ ] Game Boy Color support (double-speed CPU, color palettes, VRAM banks)
- [ ] Link cable over WebRTC: two browsers, two-player games

---

## References

- [Pan Docs](https://gbdev.io/pandocs/) — the hardware reference
- [Opcode table](https://gbdev.io/gb-opcodes/optables/) — cycles and flags for every instruction
- [Gameboy Doctor](https://github.com/robert/gameboy-doctor) — finds the first instruction where your CPU diverges
- [Test ROM collection](https://github.com/c-sp/game-boy-test-roms)
- [dmg-acid2](https://github.com/mattcurrie/dmg-acid2) — PPU rendering test
- [Homebrew Hub](https://hh.gbdev.io/) — free, legal games to play
