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

## Milestone 4 — Playable in the browser ✅

- [x] Gamepad API support alongside the keyboard
- [x] Touch controls for phones
- [x] Pause, reset, speed toggle (fast-forward is very satisfying)
- [x] Zero-copy framebuffer (a `Uint8ClampedArray` view on wasm memory)

**Done when:** you can play a homebrew game from [Homebrew Hub](https://hh.gbdev.io/)
start to finish on your phone.
(Done: played start to finish on a phone at https://tanakriteuro.github.io/gb-emu/.)

## Milestone 5 — More cartridges and saves

- [x] MBC3 with the real-time clock (Pokémon-style games use it) (rtc3test: all 3 suites match)
- [x] MBC5 (Mooneye `emulator-only/mbc5`: 8/8)
- [x] MBC2: 16 ROM banks, 512 × 4-bit RAM built into the chip (Mooneye `emulator-only/mbc2`: 7/7)
- [x] Battery saves: export/import `.sav`, keep them in the browser between visits

**Done when:** Mooneye's MBC tests pass and a save survives a page reload.
(Saves survive reloads, with the clock catching up. Mooneye MBC: 27/28; `mbc1/multicart_rom_8Mb`
needs MBC1 multicart wiring, which isn't planned.)

## Milestone 6 — Sound ✅

- [x] Four channels: two square waves (one with sweep), wave, noise (plus mixer, high-pass filter, `gb-cli --wav`)
- [x] Frame sequencer: length, envelope, sweep (blargg `dmg_sound`: 9/12; 09, 10 and 12 need CH3 wave-RAM access while playing)
- [x] Output through a Web Audio `AudioWorklet`; let audio drive timing to avoid crackle

**Done when:** music sounds right and doesn't pop.
(Done: Tobu Tobu Girl's music plays cleanly, paced by the audio clock with no underruns.)

## Milestone 7 — Show-off features ✅

Pick whichever sound most fun:

- [x] Deploy to GitHub Pages from CI, so the README has a "Play it" link
      (https://tanakriteuro.github.io/gb-emu/, from `.github/workflows/ci.yml`)
- [x] Debugger panel: registers, memory hex view, VRAM tile viewer, breakpoints, step button
  - [x] Registers, flags and the next instructions (a disassembler), with Step and Step frame
  - [x] Memory hex view: any address, live while running
  - [x] VRAM tile viewer: all 384 tiles, and the background map with the visible area
  - [x] Breakpoints: stop when PC reaches an address (in whichever ROM bank is mapped there)
- [x] Save states and rewind (hold a key to run time backwards)
  - [x] Save states in the core: the whole machine to bytes and back, refusing other games' states
  - [x] Save states in the browser: slots per game, kept between visits
  - [x] Rewind: keep recent states, hold a key to run backwards (30 s of history in ~350 KB)
- [x] Game Boy Color support (double-speed CPU, color palettes, VRAM banks)
  - [x] Color mode: picked from the header, the Color's boot state, VRAM and WRAM banks, double
        speed (Blargg `cpu_instrs`/`instr_timing` pass in Color mode, `interrupt_time` passes;
        `cgb_sound` 8/12. Mooneye's Color tests need DMG compatibility mode: Milestone 9)
  - [x] Color palettes and per-tile background attributes (palette, VRAM bank, flips, priority)
        (cgb-acid2: every pixel not drawn by a sprite matches)
  - [x] Color sprites: palette and bank bits, OAM-order priority, LCDC bit 0 as master priority
        (`cgb-acid2` matches pixel for pixel)
  - [x] HDMA: copying to VRAM all at once, or a block per HBlank (and lines are now drawn as
        HBlank begins, so HBlank writes show from the next line)
  - [x] A Game Boy Color homebrew game plays in the browser (Tobu Tobu Girl Deluxe, at 60 fps,
        with sound, battery saves, save states and rewind)
- [x] Link cable over WebRTC: two browsers, two-player games
  - [x] Serial port with a partner in the core: transfers take their real time, a master's byte
        goes to the partner and comes back with theirs (or $FF with no cable), a slave answers
        when clocked; two Game Boys linked in a test swap bytes
  - [x] Two browser tabs linked on one computer, to get the browser side right without a network
        (about 1000 bytes a second, as fast as the real cable)
  - [x] WebRTC between two browsers, connected by copying a code each way (no server; STUN, no
        TURN relay, so the strictest networks can't link)
  - [x] A two-player homebrew game plays over the link: none free to download turned up, so
        Link Pong (homebrew/link-pong, our own, in assembly with RGBDS), on the page as
        "try Link Pong"; two linked Game Boys play it in lockstep, byte for byte the same

## Milestone 8 — Mooneye PPU timing ✅

Mooneye's `acceptance/ppu` tests time the PPU to the T-cycle. Games mostly don't care, but
this is where the hardware gets interesting.

- [x] Memory accesses at their own M-cycle: the CPU runs the rest of the hardware between the
      reads and writes of an instruction, checks interrupts at the end of the opcode fetch
      (where HALT wakes too), and picks the interrupt after pushing PC's high byte. OAM DMA
      takes its real 160 M-cycles and holds OAM meanwhile. DIV starts at $AB.
      (All of Mooneye's instruction timing, OAM DMA, interrupt and timer tests pass, and
      Blargg's `mem_timing` 1 and 2; `acceptance/ppu`: 5/12; Mooneye overall 89/100 here)
- [x] Mode 3 length: 172 dots plus SCX's fine scroll, sprites and the window, moving the start
      of HBlank (`hblank_ly_scx_timing`, `intr_2_mode0_timing_sprites`; LY now reads the next
      line 4 dots early. `acceptance/ppu`: 7/12; Mooneye overall 91/100 here)
- [x] OAM and VRAM closed to the CPU while the PPU reads them, in modes 2 and 3, and the
      Color's palette data in mode 3 (`intr_2_oam_ok_timing`. `acceptance/ppu`: 8/12; Mooneye
      overall 92/100 here; Tobu Tobu Girl, its Color edition and Link Pong draw the same)
- [x] Switching the LCD on, and line 144: the first line's quirks (`lcdon_timing`,
      `lcdon_write_timing`), LY == LYC as the LCD goes off and on (`stat_lyc_onoff`), the mode 2
      interrupt at line 144 (`vblank_stat_intr`). Reads meet the PPU's hold on OAM and VRAM
      4 dots before writes do

**Done when:** Mooneye `acceptance/ppu` passes 12/12.
(Done: 12/12. Mooneye overall 96/100 here; the other 4 are unused I/O bits, boot I/O
values, the serial clock's boot alignment and an MBC1 multicart.)

## Milestone 9 — Original games in color ✅

A Game Boy Color plays original Game Boy cartridges in its "DMG compatibility mode": the boot
ROM loads a few palettes (per game, for Nintendo's) and the original's palette registers pick
from them. On a real Color you could also hold a button combination at boot to choose.

- [x] Compatibility mode in the core: a Color with an original cartridge shuts its own registers
      (VRAM and WRAM banks, double speed, HDMA, palette data), BGP/OBP0/OBP1 pick colors from the
      boot ROM's palettes (the default one so far), and the boot ROM's registers. Unused I/O
      reads $FF; the Color's undocumented $FF72-$FF77. (Mooneye: `boot_regs-cgb`,
      `boot_hwio-C`, `unused_hwio-C`, `vblank_stat_intr-C` pass, and so now do `boot_hwio` and
      `unused_hwio` on the original: 98/100 here)
- [x] The boot ROM's palette for each of Nintendo's games: a checksum of the title, and its 4th
      letter where checksums collide (94 titles, 51 palette combinations; tables from SameBoy's
      boot ROM, which match Pokémon Red's, Blue's and others' title checksums)
- [x] In the browser: play original games on a Game Boy or a Game Boy Color, and pick one of
      the 12 palettes a real Color offers for button combinations at boot (the Console panel;
      both choices remembered, the palette per game, and kept through save states and rewind)

**Done when:** an original game plays in color in the browser, in a palette you picked.
(Done: Tobu Tobu Girl plays in the Color's automatic palette and in any of the 12.)

## Milestone 10 — Pixel by pixel ✅

The PPU doesn't draw a line at once: in mode 3 a fetcher reads tiles into a pixel FIFO and one
pixel leaves it per dot, through the palette registers as they are at that moment. Games (and
Mealybug Tearoom's tests, `roms/mealybug-tearoom-tests/ppu`) change registers mid-line and expect
to see it land on the exact pixel.

- [x] A pixel FIFO renderer: the background/window fetcher and the sprite fetches, dot by dot;
      mode 3's length comes out of it instead of a formula (Mooneye, dmg-acid2 and cgb-acid2
      keep passing). With it, writes to the PPU's registers land at their own dot of the
      M-cycle (the original's palettes old OR new for a dot, its STAT write bug), and the line
      after switching the LCD on is short. Mealybug: 4/24 on the original, 13/27 on the Color
- [x] The original's mid-line quirks: LCDC's bits reaching the fetcher a dot before the pixels
      (and turning them off early at the line's first pixel), sprite fetches cut short by
      switching sprites off, the hidden window's blank pixel, the window starting a pixel late,
      SCY two dots early. Mealybug on the original: 24/24 (and still 20/27 on the Color)
- [x] The Color's own fetcher timings: the tile-select glitch both ways (clearing LCDC bit 4
      mid-read gives the tile's number, setting it gives back a latched byte), WX 0 with a fine
      scroll costing a dot and the window restart's blank pixel on the Color too. Mealybug on
      the Color: 26/27 against its CPU CGB C pictures (m3_lcdc_obj_size_change_scx is left: a
      sprite fetch a dot early in one case, marked `TODO(accuracy)`)

**Done when:** every Mealybug Tearoom PPU test matches its picture on the original.
(Before: 1/24 on the original, 1/27 on the Color. After: 24/24 and 26/27.)

---

## Milestone 11 — LY, LYC and STAT at the edges ✅

Where a line or a frame turns over, LY, the LY == LYC flag and STAT's mode don't change all at
once, and line 153 is odd: LY reads 153 for only a few dots, then 0 for the rest of the line.
Wilbert Pol's extended Mooneye tests (`roms/mooneye-test-suite-wilbertpol`) and the AGE tests
(`roms/age-test-roms`) read them dot by dot.

- [x] Line 153 and the new frame: LY reads 153 for only 4 dots (the end of line 152), LY == LYC
      matches 153 then 0 through line 153, the original shows mode 0 for a dot before line 0, and
      on the Color the LY == LYC flag holds through a line's last 4 dots. gb-cli also takes
      register verdicts (`LD B,B`, and the old Mooneye exit `$ED`). Wilbert Pol's `ly_lyc_*` and `ly_new_frame`: 13/16
      (`ly_lyc_0-C`, `ly_lyc_153-C` and `ly_new_frame-C` were measured on a later Color, which
      reads 153 longer: AGE's `ly` shows CPU CGB B and C read it like the original)
- [x] STAT at line edges: the mode 2 source is a one-dot pulse as mode 2 begins (so writing STAT
      during mode 2 fires nothing), line 144's comes a dot before VBlank on the original, the
      first line after switching on starts with a mode 0 that isn't an HBlank and a mode 3 2 dots
      longer, VBlank ends with a dot of mode 0 on CPU CGB C too, WY only matches with the window
      on, and HALT looks for an interrupt an M-cycle after its own (the HALT bug). Wilbert Pol's
      `gpu` tests: 54/58 (the other 4 are a later Color's); AGE `stat-mode`, `stat-int`, `halt`
      and `stat-mode-sprites` pass except their double-speed parts. Wilbert Pol 105/121 overall,
      AGE 22/55
- [x] OAM and VRAM access at line edges: on the Color, OAM writes are held from 4 dots before a
      line and VRAM reads only from mode 3, and on the first line after switching on it takes
      palette memory 2 dots into mode 3 and VRAM 5; the original's palette writes land whole when
      the line's first pixel is next out. AGE `oam-read`, `oam-write`, `vram-read` pass in
      single speed, and all 8 of its screenshot tests (`m3-bg-*`) match. Left: `oam-write-dmgC`'s
      row its author marks as depending on when the LCD was last switched off
      (`lcd-align-ly` needs double speed)

**Done when:** Wilbert Pol's `acceptance/gpu` tests pass, and AGE's tests for the original
(DMG C) and the Color (CPU CGB B/C) pass, except where they need a later Color or double speed.
(Before: Wilbert Pol 91/121 overall, AGE 9/55. After: 105/121, the rest being a later Color's,
other consoles' boot states or tools; AGE 24/55 by register plus its 8 screenshot tests, the
rest double speed, CPU CGB E, or the one row above.)

---

## Milestone 12 — The Color's double speed ✅

STOP with KEY1 armed switches the Color's CPU between 4 and 8 MHz. The switch isn't instant: the
CPU sits still for a while as the clocks change over, the timer keeps counting, and afterwards the
PPU and sound (which stay at normal speed) meet the CPU on half-dot boundaries. The AGE tests
(`roms/age-test-roms`) measure the switch and repeat their PPU tests in double speed.

- [x] The speed switch: into double speed it lands 6 T-cycles after STOP, back at once; with no
      interrupt pending STOP reads its second byte and the CPU pauses, halted, for $20008
      T-cycles (an interrupt ends it early), else the second byte runs. STOP's DIV reset lands 8
      T-cycles later (4 with interrupts on) and only bumps a 4096 Hz TIMA if its bit was set 4
      T-cycles before too; the PPU stands still for a T-cycle or two. AGE `speed-switch`: 4/5
      (`spsw-ch2-lc-delay` is the sound's length counter, left for the sound)
- [x] The PPU in double speed: a CPU T-cycle is half a dot, carried over between M-cycles. LY
      moves on 2 dots before a line ends (4 in the old model; only double speed sees the
      difference) and line 153 shows 153 for 4 dots more; switching on starts a dot later, the
      first line's mode 3 shows 2 dots later, the mode 2 and HBlank sources fire a dot earlier,
      OAM reads aren't held before a line and writes only in its last 2 dots, and LCDC's tile
      select doesn't glitch. AGE `ly`, `stat-mode`, `stat-int`, `oam`, `vram` and the `-ds` tests
      pass for CPU CGB B/C. Left: `lcd-align-ly` (a switch can leave the CPU half a dot off the
      PPU in normal speed, so LY reads old and new as it changes; SameBoy doesn't do it either)

**Done when:** AGE's tests for CPU CGB B/C pass in double speed as well as single.
(Before: AGE 24/55 by register plus 8 screenshot tests. After: 36/55 plus all 10; for CPU CGB
B/C only `lcd-align-ly` and `spsw-ch2-lc-delay` are left, the rest being CPU CGB E or
`oam-write-dmgC`'s row. Wilbert Pol unchanged at 109/121.)

---

## Milestone 13 — Sound to the tick ✅

The APU runs on its own 2 MHz clock (the square and noise channels at 1 MHz), and every channel
counts its period down in those ticks, so a trigger, a period write or a DIV write lands on a
particular tick. Its frame sequencer follows both edges of DIV's bit 4, and many quirks fall out
of how the counters are wired: wave RAM while channel 3 plays, NRx2 writes during a note ("zombie
mode"), the sweep's delayed calculation. The Color's PCM12/PCM34 registers show each channel's
level, which is how SameSuite (`roms/same-suite/apu`) measures them. The timings follow SameBoy's
APU, for the original and CPU CGB C.

- [x] The APU's own clock: the channels count in 2 MHz ticks, with their trigger delays (channel 3's
      first sample (2047 - period) + 4 ticks after a trigger, the squares' first step after their
      period plus a few ticks), the frame sequencer runs from DIV-APU's falling edge and arms
      envelopes on its rising one, switching the APU on with DIV's bit set skips an event, and
      PCM12/PCM34 read the levels (on CPU CGB C glitched in the M-cycle they change). SameSuite
      `apu`: the 5 general tests and channel 3's, except the 2 for earlier Colors
- [x] Wave RAM and power: while channel 3 plays the CPU reaches the byte it's reading (on the
      original only in the tick it reads it), retriggering the original's channel 3 as it reads
      corrupts wave RAM, a stopped channel 3 reads whatever is on the bus, and the Color's power
      off clears the length timers. Blargg `dmg_sound` and `cgb_sound`: 12/12 each
- [x] Envelopes, sweep and noise: NRx2 writes during a note move the volume, envelopes take their
      direction and pace from NRx2 as they step, the sweep's overflow check comes a few 1 MHz ticks
      after a trigger or sweep, NR10 and NR43 writes glitch as on CPU CGB C, and the noise counter's
      start depends on its phase. In double speed, DIV-APU events arrive an M-cycle late after every
      other switch, and DIV-APU only moves to the other DIV bit when STOP's DIV reset lands (AGE
      `spsw-ch2-lc-delay`)

**Done when:** blargg's sound tests pass, SameSuite's APU tests that a CPU CGB C passes on hardware
(channel 3 and the general ones) pass, and AGE's `spsw-ch2-lc-delay`.
(Before: blargg `dmg_sound` 9/12, `cgb_sound` 8/12, SameSuite `apu` 3/64. After: 12/12, 12/12,
30/64: on CPU CGB C, PCM12/PCM34 read glitched for channels 1, 2 and 4 in ways not yet understood,
so their tests fail on that console too, and some tests are for other revisions. AGE 37/55; for
CPU CGB B/C only `lcd-align-ly` is left.)

---

## Milestone 14 — The OAM bug ✅

On the original Game Boy, mode 2 reads OAM a row (8 bytes, two objects) at a time. If the CPU puts
an OAM address on the bus then, by reading or writing it or by a 16-bit increment passing through
it (INC rr, DEC rr, PUSH, CALL, RST, JR, an interrupt), the row being read is overwritten with a
bitwise mix of itself and the rows before it. The Color fixed it. Patterns after SameBoy's DMG.
https://gbdev.io/pandocs/OAM_Corruption_Bug.html

- [x] Writes and increments: the row being scanned (row 0 just before the scan, then each pair of
      objects' row) mixes its first word with the row before and copies the rest of it; the CPU's
      internal M-cycles of INC/DEC rr, PUSH, CALL, RST, JR, LD SP,HL and interrupts put their
      register on the bus. Blargg `oam_bug` 1-6
- [x] Reads: a read mixes the scanned row into the row before (rows $00, $20 .. and $10, $30 ..
      reaching further back), and reads just before the scan or in its last 4 dots, where OAM is held
      for reads but not writes, mix the row read into the first or last row. Blargg `oam_bug` 7, 8

**Done when:** Blargg's `oam_bug` passes on the original.
(Before: 3/8. After: 8/8 in the combined ROM. The single `7-timing_effect` prints an OAM dump for
every timing that corrupts, which overruns its 8 KB text buffer into the test's own code; the
combined ROM checks the same results by CRC and passes.)

---

## Milestone 15 — gbmicrotest ✅

GBMicrotest (`roms/gbmicrotest`) is 513 tiny tests, checked on an original Game Boy, that each read
one register at one exact M-cycle and write a pass byte to $FF82. Most run straight from the
boot ROM's hand-over without resetting the LCD, so they pin down exactly where the hardware is when
a game starts.

- [x] The hand-over: the original's boot ROM leaves the PPU at line 153's dot 395, near the end of
      VBlank (LY already reads 0), not line 0's dot 3: 64 dots, a whole number of M-cycles, so the
      phase Mooneye's boot tests pin is unchanged. gbmicrotest's `poweron_*` tests, and the 28
      `hblank_int_scx*` tests and `line_65_ly`, which time from boot
- [x] IF writes land as their M-cycle ends, so an interrupt raised in the dot after a write that
      clears IF stays (`vblank_int_if_c`, `vblank2_int_if_c`, `lyc1_int_if_edge_c`)

**Done when:** gbmicrotest's tests with a verdict pass on the original.
(Before: 423/513. After: 480/513: 31 are test benches with no verdict, and 2 seem to expect the
impossible: `halt_op_dupe_delay` reads DIV as $55 about 63 M-cycles after resetting it, and
`stat_write_glitch_l154_d` expects no VBlank flag a whole frame after switching the LCD on.)

---

## Milestone 16 — The small suites ✅

Most of the small picture-based test ROMs in `roms/` already match their reference screenshots
(cgb-acid-hell, turtle-tests, scribbltests, little-things' firstwhite). Four don't.

- [x] MBC30: a 4 MB MBC3 (the Japanese Pokémon Crystal's chip) has an 8-bit ROM bank register and
      8 RAM banks. `mbc3-tester` matches on both consoles (by shade on the Color, whose reference
      uses other compatibility colors)
- [x] OAM DMA and the PPU: the OAM scan reads each object's Y and X in mode 2, 2 dots apart; while a
      DMA copies it can't, and keeps seeing the last pair it read, so every object scanned then looks
      like that one. The sprite fetcher's tile and attribute reads land on the bytes the DMA is
      writing. `strikethrough` matches on both consoles
- [x] bully: RAM powers up scrambled (pseudo-random, seeded from the ROM so runs stay
      reproducible); OAM DMA takes the bus it reads from, so the CPU's reads there get the byte it
      just copied and its writes go astray (cartridge and WRAM share a bus on the original, WRAM has
      its own on the Color); and the copy's first byte comes in the M-cycle it takes OAM. `bully`
      matches
- [x] Input during the frame: a player's press lands at a pseudo-random point of the next frame
      (seeded from the ROM), not always on the line where the frontend runs the next frame, so
      games that seed their random numbers from LY at a press get varied seeds; gb-cli's
      `--press FRAME:BUTTONS` presses buttons for tests. Telling LYs passes on both consoles

**Done when:** all four match their reference screenshots. (Before: 0 of mbc3-tester,
strikethrough, bully and Telling LYs. After: all four, on both consoles where they run on both.)

---

## Milestone 17 — Gambatte's test suite

Gambatte's tests (`roms/gambatte`, about 3500 ROMs, checked on an original and a CPU CGB C) each
run 15 frames and then show a hex result on screen, match a screenshot, or make sound or not, as
their file names say. They're the broadest suite left: grouped by what they test, the failures
say what to work on next.

- [x] A harness: `gambatte-tests` (in gb-cli) runs them as Gambatte's testrunner.cpp does. First
      count: 4321/5225 checks (the Color 2703/3352, the original 1618/1873)
- [x] OAM DMA, after Gambatte (its tests check it on hardware): a CPU access on the bus the copy
      reads from gets the byte just copied, and a write lands in that OAM byte instead (ANDed with it
      on the original when copying from WRAM); on the Color WRAM stays reachable during a copy from
      elsewhere, and a copy from $E000 up reads $FF. The copy stands still while the CPU is halted.
      $FEA0-$FEFF reads $00 on the original and is 72 bytes of RAM on CPU CGB C. `oamdma`: 526 -> 772
      of 811. Left: copies from $FE00/$FF00 on the original, a sprite timing edge after a late copy
      (`late_sp*_2`), and a halt that catches the copy's last M-cycle (`late_halt_stat_2`)
- [ ] The Color's VRAM DMA (`dma`: 113 failing)
- [ ] The next largest groups (window, `arg`, sound, serial, mode 1, mode 0, LCD offset)

**Done when:** the OAM and VRAM DMA groups pass, and each other group is either fixed or
understood. Before and after counts go here.

---

## References

- [Pan Docs](https://gbdev.io/pandocs/) — the hardware reference
- [Opcode table](https://gbdev.io/gb-opcodes/optables/) — cycles and flags for every instruction
- [Gameboy Doctor](https://github.com/robert/gameboy-doctor) — finds the first instruction where your CPU diverges
- [Test ROM collection](https://github.com/c-sp/game-boy-test-roms)
- [dmg-acid2](https://github.com/mattcurrie/dmg-acid2) — PPU rendering test
- [Homebrew Hub](https://hh.gbdev.io/) — free, legal games to play
