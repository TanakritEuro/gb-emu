# gb-emu

A Game Boy and Game Boy Color emulator written in Rust, running in the browser through WebAssembly.

**[▶ Play it](https://tanakriteuro.github.io/gb-emu/)** in your browser: drop in a `.gb` or `.gbc`
file (free, legal homebrew games are on [Homebrew Hub](https://hh.gbdev.io/), e.g. Tobu Tobu Girl,
or its Color edition, Tobu Tobu Girl Deluxe).

Status: plays original Game Boy (DMG) and Game Boy Color games with sound, battery saves, save
states, rewind and a debugger, in the browser or headless. Games that support the Color run in
color; original games run as on the original Game Boy, or in color on a Game Boy Color with the
palette of your choice. Next up: [docs/ROADMAP.md](docs/ROADMAP.md).

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

Every push runs the tests on GitHub Actions (`.github/workflows/ci.yml`); pushes to `main` also
build the browser version and publish it to GitHub Pages, which is where the Play it link points.
(`serve.js` needs only Node. Any static server works, e.g. `python -m http.server 8765 -d web`;
avoid port 8080 on Windows, which is often reserved.)

Controls: arrows = d-pad, X = A, Z = B, Enter = Start, Shift = Select.
Gamepads work too (standard layout): d-pad or left stick, right face button = A,
bottom face button = B, Start, Select (Back). Press a button once so the browser exposes it.
On phones and tablets, on-screen controls appear under the screen (multi-touch, slide between
buttons). Add `?touch` to the URL to show them on a desktop.

Pause, Reset and a speed button (1×/2×/4×) sit under the screen. Keys: P pause, R reset,
F speed, hold Space to fast-forward at 8×.

Battery saves are kept in the browser (localStorage, per ROM) and stored a moment after the
game saves and when the page closes. Export .sav / Import .sav move them to and from other
emulators (BGB/VBA-M format, including the MBC3 clock, which catches up on time that passed).

Rewind: hold Backspace (or the ⏪ button) to run time backwards at double speed, up to 30 seconds;
let go and play on from there. While paused, each press steps back two frames. The button shows
how much history there is.

Console: original Game Boy games run as on the original (green screen) unless you pick Game Boy
Color in the Console panel. The Color colors them with the palette its startup ROM picks for the
game, or one of the 12 it offers for button combinations held at startup, which you can choose
there too. Both choices are remembered in the browser, the palette per game.

Save states: four slots per game under the screen, each with a picture and when it was saved,
kept in the browser (IndexedDB) between visits. Keys: 1-4 pick a slot, S saves, L loads. A state
is the whole machine, so loading one also puts the battery save (and the MBC3 clock) back to how
they were then.

Link cable: two-player games can link two copies of the emulator. Both players load the game and
open **Link cable**, then:

- **Over the internet** (WebRTC, browser to browser, no server): one player presses **Invite a
  friend** and sends the code it shows (by chat, email, anything); the other presses **Join with
  a code**, pastes it, and sends back the reply code; the first pastes that and presses
  **Connect**. The codes include your network address, so share them only with the person you're
  playing with. Each byte waits for a network round trip, so link-heavy games run slower than
  on one computer, and some strict networks (offices, some mobile data) block the direct
  connection altogether (there's no relay server).
- **On one computer:** two windows of the same browser, each pressing **Link with another tab**.
  Keep both visible (browsers pause background tabs). Bytes cross at the real link speed, about
  1 KB/s; closing either window unplugs the other.

To try it, there's **Link Pong**, a two-player Pong written for this emulator (in Game Boy
assembly, [homebrew/link-pong](homebrew/link-pong/link-pong.asm)): press **try Link Pong** on the
screen in both browsers, link up, and press START on one (it becomes player 1). SELECT plays
the computer instead. Up and down move your paddle; first to 9 wins. Building it locally needs
[RGBDS](https://rgbds.gbdev.io): `node scripts/build-homebrew.js` puts it in `web/games/`.

Sound starts with your first click, tap or key press (browsers require one). The Sound button
or M mutes it. Fast-forward is silent.

The Debugger panel under the screen shows the CPU registers, flags and the next instructions.
Step (or N) pauses and runs one instruction; while the CPU sleeps in HALT, one step runs until
an interrupt wakes it. Step frame runs one frame.
Click an instruction to set a breakpoint there (or type an address under Breakpoints): the game
pauses just before running it. Resume runs on to the next one. Breakpoints are CPU addresses, so
one in $4000-$7FFF stops in whichever ROM bank is mapped there.
Under that, a memory view shows 256 bytes at a time, live: type an address (`C000`, `PC`, `LY`)
and press Enter, jump to a region, or follow PC, SP or HL. Bytes the game just wrote light up;
click one to see its value and what it is.
The VRAM view shows all 384 tiles and a whole 256×256 tile map, with the part on screen outlined
(and the window's part, when it's on). Hover to see a tile's number and address; click to open
it in the memory view.

## Working with Claude Code

`CLAUDE.md` describes the project for Claude Code. Some good ways to start:

- "Read docs/ROADMAP.md and start milestone 1: set up the opcode decoder."
- `/test-rom roms/blargg/cpu_instrs/individual/06-ld r,r.gb` runs a test ROM and investigates whatever it reports.

## License

MIT; see [LICENSE](LICENSE). Game ROMs are not included and are not covered by it.

The Game Boy Color's palettes for original games (`crates/gb-core/src/compat.rs`) come from
[SameBoy](https://github.com/LIJI32/SameBoy)'s open-source boot ROM, © Lior Halphon, under
the MIT (Expat) license; its notice is next to the tables.
