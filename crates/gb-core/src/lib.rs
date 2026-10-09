//! Game Boy (DMG) emulator core.
//!
//! This crate does no I/O and has no platform dependencies. A frontend
//! (the CLI, the WebAssembly build, a future desktop app) hands it a ROM and
//! button presses, calls [`GameBoy::run_frame`], and reads back pixels and
//! serial output.

pub mod apu;
pub mod bus;
pub mod cartridge;
pub mod compat;
pub mod cpu;
pub mod disasm;
pub mod joypad;
pub mod ppu;
pub mod rewind;
pub mod serial;
pub mod state;
pub mod timer;

use bus::{interrupt, Bus};
use compat::CompatPalettes;
use cpu::{Cpu, CpuError};
use state::{StateReader, StateWriter};
use std::collections::BTreeSet;

pub use cartridge::{Cartridge, CartridgeError, SaveError};
pub use disasm::Instruction;
pub use joypad::Button;
pub use rewind::Rewind;
pub use state::StateError;

pub const SCREEN_WIDTH: usize = 160;
pub const SCREEN_HEIGHT: usize = 144;
/// CPU clock in T-cycles per second.
pub const CPU_HZ: u32 = 4_194_304;
/// T-cycles per frame: 154 scanlines × 456 dots.
pub const CYCLES_PER_FRAME: u32 = 70_224;

/// A whole Game Boy: CPU plus everything on the memory bus.
#[derive(Clone)]
pub struct GameBoy {
    cpu: Cpu,
    bus: Bus,
    /// Addresses where [`run_frame`](Self::run_frame) stops before running
    /// the instruction there. Debugger-only: the hardware has nothing like it.
    breakpoints: BTreeSet<u16>,
    /// Also stop before every `LD B,B` (see
    /// [`set_ld_b_b_breakpoint`](Self::set_ld_b_b_breakpoint)).
    ld_b_b_breakpoint: bool,
    /// The debugger stopped the CPU here (a breakpoint, or a step), so the
    /// next run starts by running this instruction instead of stopping on it.
    resume_here: bool,
    /// How far into the current frame [`run_frame`](Self::run_frame) has
    /// got, in normal-speed T-cycles: nonzero after it stopped early.
    frame_elapsed: u32,
    /// Compatibility mode palettes the host picked over the boot ROM's
    /// (see [`set_compat_palettes`](Self::set_compat_palettes)).
    chosen_palettes: Option<CompatPalettes>,
    /// Button changes from [`set_button_during_frame`](Self::set_button_during_frame),
    /// in order: each lands once [`run_frame`](Self::run_frame) has run
    /// that many more normal-speed T-cycles.
    pending_buttons: Vec<(u32, Button, bool)>,
    /// Picks when those land: xorshift, seeded from the ROM.
    input_rng: u64,
}

/// Which console to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// The original Game Boy (DMG).
    Dmg,
    /// The Game Boy Color: in Color mode for Color cartridges (double speed,
    /// color palettes, banked VRAM and WRAM), in its compatibility mode for
    /// original ones ([`compat`]).
    Cgb,
}

impl Model {
    /// The model a game is best played on: Color mode if its header says it
    /// knows about the Color, the original otherwise. (A real Color runs
    /// the others in a DMG compatibility mode, which isn't emulated.)
    pub fn for_cartridge(cart: &Cartridge) -> Self {
        if cart.header.cgb {
            Self::Cgb
        } else {
            Self::Dmg
        }
    }
}

/// How a call to [`GameBoy::run_frame`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameEnd {
    /// A whole frame's worth of T-cycles ran.
    Done,
    /// PC reached a breakpoint, partway through the frame. The instruction
    /// there hasn't run yet.
    Breakpoint,
    /// A link cable transfer is waiting for the partner's byte (see
    /// [`GameBoy::link_answer`]); everything stands still until it comes.
    LinkWait,
}

impl GameBoy {
    /// Loads a ROM and starts in the state the boot ROM leaves behind,
    /// so execution begins at $0100 without needing a copy of the boot ROM.
    /// The model comes from the cartridge header ([`Model::for_cartridge`]).
    pub fn new(rom: Vec<u8>) -> Result<Self, CartridgeError> {
        Self::with_model(rom, None)
    }

    /// Like [`new`](Self::new), but `Some(model)` overrides the choice,
    /// e.g. to run a test ROM on a particular console. An original
    /// cartridge on the Color runs in its compatibility mode ([`compat`]).
    pub fn with_model(rom: Vec<u8>, model: Option<Model>) -> Result<Self, CartridgeError> {
        let cart = Cartridge::from_rom(rom)?;
        let model = model.unwrap_or_else(|| Model::for_cartridge(&cart));
        let input_rng = cart.rom_hash() | 1;
        let bus = Bus::new(cart, model);
        let mut cpu = Cpu::new();
        cpu.reset_post_boot(model);
        if bus.compat {
            cpu.regs = compat::boot_registers(&bus.cart);
        }
        Ok(Self {
            cpu,
            bus,
            breakpoints: BTreeSet::new(),
            ld_b_b_breakpoint: false,
            resume_here: false,
            frame_elapsed: 0,
            chosen_palettes: None,
            pending_buttons: Vec::new(),
            input_rng,
        })
    }

    pub fn model(&self) -> Model {
        self.bus.model
    }

    /// A Game Boy Color running an original cartridge, in its
    /// compatibility mode ([`compat`]).
    pub fn is_compat(&self) -> bool {
        self.bus.compat
    }

    /// The palettes the Color's boot ROM picks for this cartridge (what
    /// [`set_compat_palettes`](Self::set_compat_palettes)`(None)` shows).
    pub fn boot_palettes(&self) -> CompatPalettes {
        compat::boot_palettes(&self.bus.cart)
    }

    /// In compatibility mode, shows the game in `palettes` instead of the
    /// ones the boot ROM picked (`None` goes back to those), as holding a
    /// button combination at boot would ([`compat::BUTTON_PALETTES`]). The
    /// choice sticks through loading save states and rewinding: it belongs
    /// to whoever is playing, not to the game. Returns false, changing
    /// nothing, outside compatibility mode.
    pub fn set_compat_palettes(&mut self, palettes: Option<CompatPalettes>) -> bool {
        if !self.bus.compat {
            return false;
        }
        self.chosen_palettes = palettes;
        self.bus
            .ppu
            .set_compat_palettes(palettes.unwrap_or_else(|| self.boot_palettes()));
        true
    }

    /// Executes one instruction and advances the rest of the hardware by the
    /// same number of T-cycles. Returns the T-cycles consumed.
    pub fn step(&mut self) -> Result<u32, CpuError> {
        // The CPU runs the rest of the hardware as it goes, M-cycle by M-cycle.
        let mut cycles = self.cpu.step(&mut self.bus)?;
        // While the Color's VRAM DMA copies, the CPU waits and the rest of
        // the hardware carries on (which can start the next HBlank block).
        loop {
            self.bus.run_hdma();
            let stall = self.bus.take_dma_stall();
            if stall == 0 {
                break;
            }
            self.bus.tick(stall);
            cycles += stall;
        }
        Ok(cycles)
    }

    /// A debugger's step: like [`step`](Self::step), but a CPU asleep in HALT
    /// keeps running until an interrupt wakes it (or `max_cycles` pass), so
    /// each call ends where an instruction or interrupt handler starts.
    /// Returns the T-cycles that passed.
    pub fn step_instruction(&mut self, max_cycles: u32) -> Result<u32, CpuError> {
        let mut cycles = self.step()?;
        while self.cpu.halted && cycles < max_cycles {
            cycles += self.step()?;
        }
        // Stopped by the debugger: running on from here mustn't stop here.
        self.resume_here = true;
        Ok(cycles)
    }

    /// Runs until one frame's worth of time has passed (70224 T-cycles at
    /// normal speed, twice as many CPU cycles in double speed), or until PC
    /// reaches a breakpoint or a link transfer has to wait. A frame that
    /// stopped early carries on from where it was on the next call, so
    /// stopping never adds time: [`FrameEnd::Done`] comes once per 70224.
    pub fn run_frame(&mut self) -> Result<FrameEnd, CpuError> {
        while self.frame_elapsed < CYCLES_PER_FRAME {
            if self.bus.serial.waiting() {
                return Ok(FrameEnd::LinkWait);
            }
            if self.at_breakpoint() {
                self.resume_here = true;
                return Ok(FrameEnd::Breakpoint);
            }
            let cycles = self.step()?;
            let real = self.bus.real_cycles(cycles);
            self.frame_elapsed += real;
            self.land_buttons(real);
        }
        self.frame_elapsed = 0;
        Ok(FrameEnd::Done)
    }

    /// Whether the instruction at PC is about to run with a breakpoint on
    /// it. While HALT sleeps, PC sits on the next instruction without
    /// running it, so that doesn't count. Clears `resume_here` (the first
    /// check after the debugger stopped is the instruction it stopped on).
    fn at_breakpoint(&mut self) -> bool {
        let resuming = std::mem::take(&mut self.resume_here);
        let pc = self.cpu.regs.pc;
        !resuming
            && !self.cpu.halted
            && ((!self.breakpoints.is_empty() && self.breakpoints.contains(&pc))
                || (self.ld_b_b_breakpoint && self.bus.read(pc) == 0x40))
    }

    /// Makes the next [`run_frame`](Self::run_frame) run the instruction at
    /// PC even if it has a breakpoint, like stopping there would. For a
    /// debugger continuing from a pause: it should always get past where it
    /// is, even if a breakpoint was just set right there.
    pub fn resume_past_breakpoint(&mut self) {
        self.resume_here = true;
    }

    /// A save state: the whole machine as bytes, to hand back to
    /// [`load_state`](Self::load_state) later (see [`state`] for the
    /// format). About 23 KiB plus the cartridge's RAM.
    pub fn save_state(&self) -> Vec<u8> {
        let mut w = StateWriter::new();
        self.cpu.save_state(&mut w);
        self.bus.save_state(&mut w);
        w.finish(self.bus.cart.rom_hash())
    }

    /// Puts the machine back the way [`save_state`](Self::save_state) found
    /// it. All or nothing: a state for another game, from another version,
    /// or damaged, is refused and leaves everything as it was. Host-side
    /// settings stay: the audio output rate, held buttons, breakpoints.
    pub fn load_state(&mut self, state: &[u8]) -> Result<(), StateError> {
        let mut r = StateReader::open(state, self.bus.cart.rom_hash())?;
        // Load into a copy (the ROM is shared, not copied) and swap it in
        // only once every section has loaded.
        let mut next = self.clone();
        next.cpu.load_state(&mut r)?;
        next.bus.load_state(&mut r)?;
        r.finish()?;
        next.bus.ppu.keep_framebuffer_of(&mut self.bus.ppu);
        next.resume_here = false; // a fresh start here: breakpoints apply
        if self.bus.compat {
            let palettes = self.chosen_palettes.unwrap_or_else(|| self.boot_palettes());
            next.bus.ppu.set_compat_palettes(palettes);
        }
        *self = next;
        Ok(())
    }

    /// Sets (`on`) or clears a breakpoint: [`run_frame`](Self::run_frame)
    /// stops before running the instruction at `addr`. Addresses are CPU
    /// addresses, so one in $4000-$7FFF stops in whichever ROM bank is
    /// mapped there at the time.
    pub fn set_breakpoint(&mut self, addr: u16, on: bool) {
        if on {
            self.breakpoints.insert(addr);
        } else {
            self.breakpoints.remove(&addr);
        }
    }

    /// Makes `LD B,B` (opcode $40, which does nothing) a breakpoint
    /// wherever it is, as in debuggers like BGB. Test ROMs use it to say
    /// they're done: Mooneye's and the AGE and SameSuite tests, with their
    /// verdict in the registers.
    pub fn set_ld_b_b_breakpoint(&mut self, on: bool) {
        self.ld_b_b_breakpoint = on;
    }

    /// The breakpoints, lowest address first.
    pub fn breakpoints(&self) -> impl Iterator<Item = u16> + '_ {
        self.breakpoints.iter().copied()
    }

    /// The screen as RGBA bytes, row-major, 160 × 144 × 4. The buffer stays at
    /// the same address for the life of this `GameBoy`, so a frontend can
    /// keep a pointer or view to it instead of copying each frame.
    pub fn framebuffer(&self) -> &[u8] {
        self.bus.ppu.framebuffer()
    }

    /// Presses or releases a button now, between instructions.
    pub fn set_button(&mut self, button: Button, pressed: bool) {
        if self.bus.joypad.set(button, pressed) {
            self.bus.if_reg |= interrupt::JOYPAD;
        }
    }

    /// A player's press or release: it lands at some point during the next
    /// frame's worth of [`run_frame`](Self::run_frame), as a real one can
    /// come at any line. Frontends run whole frames between input events, so
    /// applying them at once would always land on the same line, and games
    /// that seed their random numbers from LY at a press would always get
    /// the same seed (Telling LYs checks). Where in the frame is
    /// pseudo-random, seeded from the ROM, so runs stay reproducible; changes
    /// land in the order they were made.
    pub fn set_button_during_frame(&mut self, button: Button, pressed: bool) {
        let x = &mut self.input_rng;
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        let mut delay = (*x % u64::from(CYCLES_PER_FRAME)) as u32;
        if let Some(&(last, ..)) = self.pending_buttons.last() {
            delay = delay.max(last);
        }
        self.pending_buttons.push((delay, button, pressed));
    }

    /// Counts `cycles` off the pending button changes, applying those due.
    fn land_buttons(&mut self, cycles: u32) {
        if self.pending_buttons.is_empty() {
            return;
        }
        for pending in &mut self.pending_buttons {
            pending.0 = pending.0.saturating_sub(cycles);
        }
        while let Some(&(0, button, pressed)) = self.pending_buttons.first() {
            self.pending_buttons.remove(0);
            self.set_button(button, pressed);
        }
    }

    /// Bytes the game sent over the link port since the last call.
    /// Test ROMs (Blargg's) print their results this way.
    pub fn take_serial_output(&mut self) -> String {
        self.bus.take_serial_output()
    }

    // The link cable. A partner is another Game Boy somewhere else; the host
    // carries bytes between them (see the serial module).

    /// Plugs a link cable in, or pulls it out. With no cable a master reads
    /// $FF; with one, transfers swap bytes with whatever the host brings.
    pub fn plug_link(&mut self, plugged: bool) {
        if self.bus.serial.plug(plugged) {
            self.bus.if_reg |= interrupt::SERIAL;
        }
    }

    /// A byte this Game Boy clocked out as master: carry it to the partner
    /// (its [`link_clocked`](Self::link_clocked)) and bring back the answer
    /// for [`link_answer`](Self::link_answer).
    pub fn take_link_out(&mut self) -> Option<u8> {
        self.bus.serial.take_out()
    }

    /// The partner's byte for this Game Boy's transfer. If
    /// [`run_frame`](Self::run_frame) stopped with [`FrameEnd::LinkWait`],
    /// this lets it carry on.
    pub fn link_answer(&mut self, byte: u8) {
        if self.bus.serial.answer(byte) {
            self.bus.if_reg |= interrupt::SERIAL;
        }
    }

    /// The partner, as master, clocked `byte` into this Game Boy. Returns the
    /// byte that goes back: SB if the game was listening (SC = $80), else $FF.
    pub fn link_clocked(&mut self, byte: u8) -> u8 {
        let (back, done) = self.bus.serial.clocked(byte);
        if done {
            self.bus.if_reg |= interrupt::SERIAL;
        }
        back
    }

    /// Sets the audio output rate (samples per second per channel), e.g. the
    /// browser's AudioContext.sampleRate. 48000 until set.
    pub fn set_sample_rate(&mut self, hz: u32) {
        self.bus.apu.set_sample_rate(hz);
    }

    /// Turns the sound's high-pass filter (the capacitor that removes the
    /// DC offset) off, for the mixer's raw output, or on again.
    pub fn set_high_pass_filter(&mut self, on: bool) {
        self.bus.apu.set_high_pass_filter(on);
    }

    /// Audio made since the last call: interleaved left/right f32 samples in
    /// roughly -1..1, at the rate set by `set_sample_rate`.
    pub fn take_audio(&mut self) -> Vec<f32> {
        self.bus.apu.take_samples()
    }

    /// Whether this game has a battery save (cartridge RAM, and for MBC3 the
    /// clock, that survive power-off).
    pub fn has_battery(&self) -> bool {
        self.bus.cart.has_battery()
    }

    /// The battery save as `.sav` bytes (the format BGB and VBA-M use), or
    /// None if the game has no battery. `now` is the current Unix time in
    /// seconds; it stamps the clock so loading can catch it up later.
    pub fn save_data(&self, now: u64) -> Option<Vec<u8>> {
        self.bus.cart.save_data(now)
    }

    /// Loads a battery save. Best done right after `new`, before the game
    /// runs, like plugging in a cartridge. `now` is the current Unix time.
    pub fn load_save(&mut self, data: &[u8], now: u64) -> Result<(), SaveError> {
        self.bus.cart.load_save(data, now)
    }

    /// True (once) if the game has changed its save since the last call.
    pub fn take_save_dirty(&mut self) -> bool {
        self.bus.cart.take_save_dirty()
    }

    pub fn title(&self) -> &str {
        &self.bus.cart.header.title
    }

    /// Gameboy Doctor expects LY ($FF44) to always read $90 so traces line up.
    pub fn set_doctor_mode(&mut self, on: bool) {
        self.bus.doctor_mode = on;
    }

    /// CPU state in Gameboy Doctor's log format, taken before the next
    /// instruction runs.
    pub fn doctor_line(&self) -> String {
        let r = &self.cpu.regs;
        let mem = |i: u16| self.bus.read(r.pc.wrapping_add(i));
        format!(
            "A:{:02X} F:{:02X} B:{:02X} C:{:02X} D:{:02X} E:{:02X} H:{:02X} L:{:02X} \
             SP:{:04X} PC:{:04X} PCMEM:{:02X},{:02X},{:02X},{:02X}",
            r.a,
            r.f,
            r.b,
            r.c,
            r.d,
            r.e,
            r.h,
            r.l,
            r.sp,
            r.pc,
            mem(0),
            mem(1),
            mem(2),
            mem(3)
        )
    }

    /// The byte the CPU would read at `addr`, for a debugger. Unlike a CPU
    /// read it takes no time and changes nothing.
    pub fn peek(&self, addr: u16) -> u8 {
        self.bus.read(addr)
    }

    /// The instruction at `addr`, as text. Reads memory like [`peek`](Self::peek).
    pub fn disassemble(&self, addr: u16) -> Instruction {
        disasm::disassemble(|a| self.bus.read(a), addr)
    }

    /// Which ROM bank the cartridge maps at `addr` ($0000-$7FFF): 0 at
    /// $0000-$3FFF on most cartridges, and whatever the game picked at
    /// $4000-$7FFF.
    pub fn rom_bank(&self, addr: u16) -> usize {
        self.bus.cart.rom_bank(addr)
    }

    /// All 384 tiles of a VRAM bank as an RGBA image, 16 per row (see
    /// [`ppu::Ppu::tile_sheet`]), for a debugger.
    pub fn tile_sheet(&self, bank: u8) -> Vec<u8> {
        self.bus.ppu.tile_sheet(bank)
    }

    /// All of VRAM (both banks on the Color: bank 1 from offset $2000),
    /// whichever bank the CPU has selected, for a debugger.
    pub fn vram(&self) -> &[u8] {
        self.bus.ppu.vram()
    }

    /// The 256×256 tile map at $9C00 (`high_map`) or $9800 as an RGBA image,
    /// for a debugger.
    pub fn tile_map(&self, high_map: bool) -> Vec<u8> {
        self.bus.ppu.tile_map_image(high_map)
    }

    pub fn cpu(&self) -> &Cpu {
        &self.cpu
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }
}
