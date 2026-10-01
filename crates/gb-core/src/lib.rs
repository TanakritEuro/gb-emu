//! Game Boy (DMG) emulator core.
//!
//! This crate does no I/O and has no platform dependencies. A frontend
//! (the CLI, the WebAssembly build, a future desktop app) hands it a ROM and
//! button presses, calls [`GameBoy::run_frame`], and reads back pixels and
//! serial output.

pub mod apu;
pub mod bus;
pub mod cartridge;
pub mod cpu;
pub mod disasm;
pub mod joypad;
pub mod ppu;
pub mod timer;

use bus::{interrupt, Bus};
use cpu::{Cpu, CpuError};

pub use cartridge::{Cartridge, CartridgeError, SaveError};
pub use disasm::Instruction;
pub use joypad::Button;

pub const SCREEN_WIDTH: usize = 160;
pub const SCREEN_HEIGHT: usize = 144;
/// CPU clock in T-cycles per second.
pub const CPU_HZ: u32 = 4_194_304;
/// T-cycles per frame: 154 scanlines × 456 dots.
pub const CYCLES_PER_FRAME: u32 = 70_224;

/// A whole Game Boy: CPU plus everything on the memory bus.
pub struct GameBoy {
    cpu: Cpu,
    bus: Bus,
}

impl GameBoy {
    /// Loads a ROM and starts in the state the boot ROM leaves behind,
    /// so execution begins at $0100 without needing a copy of the boot ROM.
    pub fn new(rom: Vec<u8>) -> Result<Self, CartridgeError> {
        let cart = Cartridge::from_rom(rom)?;
        let mut cpu = Cpu::new();
        cpu.reset_post_boot();
        Ok(Self {
            cpu,
            bus: Bus::new(cart),
        })
    }

    /// Executes one instruction and advances the rest of the hardware by the
    /// same number of T-cycles. Returns the T-cycles consumed.
    pub fn step(&mut self) -> Result<u32, CpuError> {
        let cycles = self.cpu.step(&mut self.bus)?;
        self.bus.tick(cycles);
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
        Ok(cycles)
    }

    /// Runs until one frame's worth of T-cycles has elapsed.
    pub fn run_frame(&mut self) -> Result<(), CpuError> {
        let mut elapsed = 0;
        while elapsed < CYCLES_PER_FRAME {
            elapsed += self.step()?;
        }
        Ok(())
    }

    /// The screen as RGBA bytes, row-major, 160 × 144 × 4. The buffer stays at
    /// the same address for the life of this `GameBoy`, so a frontend can
    /// keep a pointer or view to it instead of copying each frame.
    pub fn framebuffer(&self) -> &[u8] {
        self.bus.ppu.framebuffer()
    }

    pub fn set_button(&mut self, button: Button, pressed: bool) {
        if self.bus.joypad.set(button, pressed) {
            self.bus.if_reg |= interrupt::JOYPAD;
        }
    }

    /// Bytes the game sent over the link port since the last call.
    /// Test ROMs (Blargg's) print their results this way.
    pub fn take_serial_output(&mut self) -> String {
        self.bus.take_serial_output()
    }

    /// Sets the audio output rate (samples per second per channel), e.g. the
    /// browser's AudioContext.sampleRate. 48000 until set.
    pub fn set_sample_rate(&mut self, hz: u32) {
        self.bus.apu.set_sample_rate(hz);
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

    /// All 384 VRAM tiles as an RGBA image, 16 per row (see
    /// [`ppu::Ppu::tile_sheet`]), for a debugger.
    pub fn tile_sheet(&self) -> Vec<u8> {
        self.bus.ppu.tile_sheet()
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
