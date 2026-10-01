//! JavaScript-facing wrapper around [`gb_core::GameBoy`].
//!
//! Errors become thrown JS `Error`s, so web/main.js can show messages like
//! "illegal opcode DD at $0150" right on the page.

use gb_core::{Button, FrameEnd, GameBoy, Model, Rewind, CPU_HZ, CYCLES_PER_FRAME};
use wasm_bindgen::prelude::*;

/// Rewind history: a snapshot every 2 frames (so rewinding one snapshot per
/// frame runs time backwards at double speed), for up to 30 seconds or
/// 32 MB, whichever comes first.
const REWIND_EVERY: u32 = 2;
const REWIND_SECONDS: usize = 30;
const REWIND_BYTES: usize = 32 << 20;

/// Frames per second: 4194304 / 70224, about 59.73.
const FPS: f64 = CPU_HZ as f64 / CYCLES_PER_FRAME as f64;

#[wasm_bindgen]
pub struct Emulator {
    gb: GameBoy,
    rewind: Rewind,
}

#[wasm_bindgen]
impl Emulator {
    #[wasm_bindgen(constructor)]
    pub fn new(rom: Vec<u8>) -> Result<Emulator, JsError> {
        let snapshots = (REWIND_SECONDS as f64 * FPS) as usize / REWIND_EVERY as usize;
        GameBoy::new(rom)
            .map(|gb| Emulator {
                gb,
                rewind: Rewind::new(REWIND_EVERY, snapshots, REWIND_BYTES),
            })
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Runs one frame, and keeps a rewind snapshot when one is due.
    /// Returns true if it stopped early at a breakpoint (see `set_breakpoint`).
    pub fn run_frame(&mut self) -> Result<bool, JsError> {
        let end = self
            .gb
            .run_frame()
            .map_err(|e| JsError::new(&e.to_string()))?;
        self.rewind.record(&self.gb);
        Ok(end == FrameEnd::Breakpoint)
    }

    /// Goes back to the newest rewind snapshot (2 frames apart), forgetting
    /// it. False when the history is used up.
    pub fn rewind_step(&mut self) -> bool {
        self.rewind.step_back(&mut self.gb)
    }

    /// How far back rewinding can go, in seconds of game time.
    pub fn rewind_seconds(&self) -> f64 {
        f64::from(self.rewind.frames_available()) / FPS
    }

    /// Memory the rewind history uses, in bytes.
    pub fn rewind_bytes(&self) -> usize {
        self.rewind.bytes()
    }

    /// A copy of the screen as RGBA, 160 × 144 × 4 bytes (about 92 KB).
    /// Handy for tests and screenshots; the page draws from
    /// `framebuffer_ptr` instead, without copying.
    pub fn framebuffer(&self) -> Vec<u8> {
        self.gb.framebuffer().to_vec()
    }

    /// Where the screen's RGBA bytes live in wasm memory. JS can wrap
    /// `framebuffer_len()` bytes from here in a `Uint8ClampedArray` over
    /// `memory.buffer` and hand it to `ImageData` with no copy. The address
    /// is fixed for this `Emulator`, but the view must be rebuilt if wasm
    /// memory grows (that detaches the old `ArrayBuffer`).
    pub fn framebuffer_ptr(&self) -> *const u8 {
        self.gb.framebuffer().as_ptr()
    }

    pub fn framebuffer_len(&self) -> usize {
        self.gb.framebuffer().len()
    }

    /// `button` is the index of a `gb_core::Button`:
    /// 0 Right, 1 Left, 2 Up, 3 Down, 4 A, 5 B, 6 Select, 7 Start.
    pub fn set_button(&mut self, button: u8, pressed: bool) {
        if let Some(b) = Button::from_index(button) {
            self.gb.set_button(b, pressed);
        }
    }

    /// Sets the audio output rate: the AudioContext's `sampleRate`.
    pub fn set_sample_rate(&mut self, hz: f64) {
        if hz.is_finite() && hz >= 1.0 {
            self.gb.set_sample_rate(hz as u32);
        }
    }

    /// Sound made since the last call: interleaved left/right samples, about
    /// -1..1 (a Float32Array of 2 x ~800 per frame at 48 kHz).
    pub fn take_audio(&mut self) -> Vec<f32> {
        self.gb.take_audio()
    }

    /// Whether the game has a battery save to keep.
    pub fn has_battery(&self) -> bool {
        self.gb.has_battery()
    }

    /// The battery save as `.sav` bytes, or undefined without a battery.
    /// `now` is Unix time in seconds (`Date.now() / 1000`).
    pub fn save_data(&self, now: f64) -> Option<Vec<u8>> {
        self.gb.save_data(unix_seconds(now))
    }

    /// Loads a battery save; best right after construction, before the first
    /// frame. Throws if it doesn't fit this cartridge.
    pub fn load_save(&mut self, data: &[u8], now: f64) -> Result<(), JsError> {
        self.gb
            .load_save(data, unix_seconds(now))
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// True (once) if the game changed its save since the last call.
    pub fn take_save_dirty(&mut self) -> bool {
        self.gb.take_save_dirty()
    }

    /// A save state: the whole machine as bytes (about 24 KB plus cartridge RAM).
    pub fn save_state(&self) -> Vec<u8> {
        self.gb.save_state()
    }

    /// Restores a save state. Throws, changing nothing, if it is for another
    /// game, from another version of the emulator, or damaged.
    pub fn load_state(&mut self, state: &[u8]) -> Result<(), JsError> {
        self.gb
            .load_state(state)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    pub fn title(&self) -> String {
        self.gb.title().to_string()
    }

    pub fn take_serial(&mut self) -> String {
        self.gb.take_serial_output()
    }

    // Debugger

    /// Runs one instruction, or dispatches an interrupt, and returns the
    /// T-cycles that took. In HALT it runs until an interrupt wakes the CPU,
    /// giving up after a second of Game Boy time.
    pub fn step(&mut self) -> Result<u32, JsError> {
        self.gb
            .step_instruction(CPU_HZ)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Sets (`on`) or clears a breakpoint: `run_frame` stops before running
    /// the instruction at `addr` (in whichever ROM bank is mapped there).
    pub fn set_breakpoint(&mut self, addr: u16, on: bool) {
        self.gb.set_breakpoint(addr, on);
    }

    /// Makes the next `run_frame` run the instruction at PC even if it has a
    /// breakpoint. Call it when continuing from a pause.
    pub fn resume_past_breakpoint(&mut self) {
        self.gb.resume_past_breakpoint();
    }

    /// A snapshot of the CPU registers.
    pub fn cpu_state(&self) -> CpuState {
        let cpu = self.gb.cpu();
        let r = &cpu.regs;
        CpuState {
            a: r.a,
            f: r.f,
            b: r.b,
            c: r.c,
            d: r.d,
            e: r.e,
            h: r.h,
            l: r.l,
            sp: r.sp,
            pc: r.pc,
            ime: cpu.ime,
            halted: cpu.halted,
        }
    }

    /// `len` bytes of memory from `start` as the CPU sees it (wrapping past
    /// $FFFF), read without side effects.
    pub fn memory(&self, start: u16, len: u16) -> Vec<u8> {
        (0..len)
            .map(|i| self.gb.peek(start.wrapping_add(i)))
            .collect()
    }

    /// The ROM bank mapped at `addr` ($0000-$7FFF).
    pub fn rom_bank(&self, addr: u16) -> usize {
        self.gb.rom_bank(addr)
    }

    /// All 384 tiles of VRAM bank `bank` (1 only on the Color) as RGBA,
    /// 128 × 192 pixels (16 tiles per row), colored through BGP, or on the
    /// Color background palette 0.
    pub fn tile_sheet(&self, bank: u8) -> Vec<u8> {
        self.gb.tile_sheet(bank)
    }

    /// All of VRAM: 8 KB, or 16 KB on the Color (bank 1 from offset $2000).
    pub fn vram(&self) -> Vec<u8> {
        self.gb.vram().to_vec()
    }

    /// Whether this is running as a Game Boy Color.
    pub fn is_color(&self) -> bool {
        self.gb.model() == Model::Cgb
    }

    /// A tile map as RGBA, 256 × 256: $9C00 if `high_map`, else $9800.
    pub fn tile_map(&self, high_map: bool) -> Vec<u8> {
        self.gb.tile_map(high_map)
    }

    /// `count` instructions from `addr`, one line each, like
    /// `"0150  3E 01     LD A,$01"`.
    pub fn disassemble(&self, addr: u16, count: usize) -> Vec<String> {
        let mut addr = addr;
        let mut lines = Vec::with_capacity(count);
        for _ in 0..count {
            let ins = self.gb.disassemble(addr);
            let bytes: Vec<String> = (0..ins.len)
                .map(|i| format!("{:02X}", self.gb.peek(addr.wrapping_add(i))))
                .collect();
            lines.push(format!("{addr:04X}  {:<9} {}", bytes.join(" "), ins.text));
            addr = addr.wrapping_add(ins.len);
        }
        lines
    }
}

/// The CPU registers at one moment, for the debugger panel.
#[wasm_bindgen]
#[derive(Clone, Copy)]
pub struct CpuState {
    pub a: u8,
    /// Flags in the top four bits: Z N H C.
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,
    /// Interrupt master enable.
    pub ime: bool,
    /// Asleep in HALT, waiting for an interrupt.
    pub halted: bool,
}

/// JS time (a float of seconds) as whole Unix seconds; nonsense becomes 0.
fn unix_seconds(now: f64) -> u64 {
    if now.is_finite() && now > 0.0 {
        now as u64
    } else {
        0
    }
}
