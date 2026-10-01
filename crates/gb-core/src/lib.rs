//! Game Boy (DMG) emulator core.
//!
//! This crate does no I/O and has no platform dependencies. A frontend
//! (the CLI, the WebAssembly build, a future desktop app) hands it a ROM and
//! button presses, calls [`GameBoy::run_frame`], and reads back pixels and
//! serial output.

pub mod bus;
pub mod cartridge;
pub mod cpu;
pub mod joypad;
pub mod ppu;
pub mod timer;

use bus::{interrupt, Bus};
use cpu::{Cpu, CpuError};

pub use cartridge::{Cartridge, CartridgeError};
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

    /// Runs until one frame's worth of T-cycles has elapsed.
    pub fn run_frame(&mut self) -> Result<(), CpuError> {
        let mut elapsed = 0;
        while elapsed < CYCLES_PER_FRAME {
            elapsed += self.step()?;
        }
        Ok(())
    }

    /// The screen as RGBA bytes, row-major, 160 × 144 × 4.
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

    pub fn cpu(&self) -> &Cpu {
        &self.cpu
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }
}
