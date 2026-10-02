//! The memory map. Every CPU read and write goes through here and is routed
//! to the cartridge, RAM, or a hardware register.
//!
//! Reference: https://gbdev.io/pandocs/Memory_Map.html

use crate::state::{StateError, StateReader, StateWriter};
use crate::{
    apu::Apu, cartridge::Cartridge, compat, joypad::Joypad, ppu::Ppu, serial::Serial, timer::Timer,
    Model,
};

/// The Color's VRAM DMA, set up through HDMA1-5 ($FF51-$FF55).
#[derive(Debug, Clone, Copy)]
struct Hdma {
    /// Where the next block comes from (low 4 bits 0).
    src: u16,
    /// Where it goes, as an offset into VRAM: $0000-$1FF0.
    dst: u16,
    /// An HBlank copy is running.
    active: bool,
    /// Blocks still to copy, minus 1 (HDMA5 bits 0-6); $7F when idle.
    remaining: u8,
}

impl Default for Hdma {
    fn default() -> Self {
        Self {
            src: 0,
            dst: 0,
            active: false,
            remaining: 0x7F, // HDMA5 reads $FF
        }
    }
}

/// OAM DMA, started by writing a source page to $FF46: after an M-cycle to
/// start up, it copies $XX00-$XX9F to OAM one byte per M-cycle, 160 in all
/// (640 T-cycles, at the CPU's speed). While it copies, OAM belongs to it:
/// the CPU reads $FF there and its writes are lost. Writing $FF46 again
/// restarts it; the old copy carries on (and keeps OAM) until the new one
/// starts. https://gbdev.io/pandocs/OAM_DMA_Transfer.html
#[derive(Debug, Clone, Copy, Default)]
struct OamDma {
    /// A copy is running.
    active: bool,
    /// Source page of the running copy.
    page: u8,
    /// Bytes it has copied, 0-159.
    copied: u8,
    /// A copy asked for by a write to $FF46: it starts once `starting`
    /// M-cycles pass (0: none waiting).
    starting: u8,
    next_page: u8,
    /// CPU T-cycles toward the next M-cycle.
    cycles: u8,
}

/// When, within its M-cycle, a CPU write reaches a register. Memory takes
/// it as the M-cycle ends; some PPU registers are wired so they take it a
/// dot or two sooner, or in two stages, and a few registers a dot later.
/// Which pixel a mid-line write shows from depends on it. The timings are
/// SameBoy's (Core/sm83_cpu.c, https://github.com/LIJI32/SameBoy, MIT),
/// which match Mealybug Tearoom's pictures of real hardware.
#[derive(Debug, Clone, Copy)]
pub(crate) enum WriteTiming {
    /// As the M-cycle ends.
    End,
    /// This many dots before the M-cycle ends.
    Early(u32),
    /// A dot after the M-cycle ends (the next M-cycle is a dot shorter).
    Late,
    /// As the M-cycle ends, with one more dot run before the CPU goes on.
    EndThenDot,
    /// `first(old, new)` lands `early` dots before the M-cycle ends, the
    /// new value `then` dots after that.
    Staged {
        first: fn(u8, u8) -> u8,
        early: u32,
        then: u32,
    },
}

/// M-cycles from a write to $FF46 until the copy starts.
const OAM_DMA_START: u8 = 2;

/// Bits of IF ($FF0F) and IE ($FFFF), in priority order.
pub mod interrupt {
    pub const VBLANK: u8 = 0x01;
    pub const STAT: u8 = 0x02;
    pub const TIMER: u8 = 0x04;
    pub const SERIAL: u8 = 0x08;
    pub const JOYPAD: u8 = 0x10;
}

#[derive(Clone)]
pub struct Bus {
    pub cart: Cartridge,
    pub ppu: Ppu,
    pub timer: Timer,
    pub joypad: Joypad,
    pub apu: Apu,
    pub model: Model,
    /// A Color running an original cartridge (KEY0 bit 2, set by its boot
    /// ROM): the Color's own registers are shut, and the original's palette
    /// registers pick colors from palettes the boot ROM loaded.
    /// https://gbdev.io/pandocs/CGB_Registers.html#ff4c--key0sys-cgb-mode-only-cpu-mode-select
    pub compat: bool,
    /// Work RAM: 8 KiB on the original, 32 KiB in eight 4 KiB banks on the
    /// Color. $C000-$CFFF is always bank 0; $D000-$DFFF is bank 1, or on the
    /// Color whichever SVBK ($FF70) picks.
    wram: Box<[u8; 0x8000]>,
    /// SVBK's bank number, 0-7 (0 maps bank 1). Color only.
    svbk: u8,
    /// KEY1 bit 0: a STOP will switch speed. Color only.
    speed_armed: bool,
    /// KEY1 bit 7: the CPU, timer, serial and OAM DMA run twice as fast; the
    /// PPU, sound and cartridge clock don't. Color only.
    pub double_speed: bool,
    hdma: Hdma,
    oam_dma: OamDma,
    /// CPU T-cycles the CPU must wait for VRAM DMA; `GameBoy::step` runs
    /// the rest of the hardware through them.
    dma_stall: u32,
    /// The CPU is asleep in HALT, which pauses HBlank DMA.
    pub(crate) cpu_halted: bool,
    hram: [u8; 0x7F],
    /// Backing store for I/O registers nothing emulates yet.
    io: [u8; 0x80],
    pub if_reg: u8,
    pub ie_reg: u8,
    /// SB/SC and the link cable.
    pub serial: Serial,
    pub doctor_mode: bool,
}

impl Bus {
    pub fn new(cart: Cartridge, model: Model) -> Self {
        let compat = model == Model::Cgb && !cart.header.cgb;
        let mut ppu = Ppu::post_boot(model);
        if model == Model::Dmg {
            let logo = std::array::from_fn(|i| cart.read_rom(0x0104 + i as u16));
            ppu.leave_boot_logo(&logo);
        }
        if compat {
            ppu.enter_compat_mode(compat::boot_palettes(&cart));
            ppu.leave_trademark();
        }
        Self {
            cart,
            ppu,
            timer: Timer::post_boot(model),
            joypad: Joypad::post_boot(model),
            apu: Apu::new(),
            model,
            compat,
            wram: Box::new([0; 0x8000]),
            svbk: 0,
            speed_armed: false,
            double_speed: false,
            hdma: Hdma::default(),
            oam_dma: OamDma::default(),
            dma_stall: 0,
            cpu_halted: false,
            hram: [0; 0x7F],
            io: [0; 0x80],
            if_reg: 0xE1,
            ie_reg: 0,
            serial: Serial::new(model == Model::Cgb && !compat),
            doctor_mode: false,
        }
    }

    /// RAM, I/O and IF/IE, then each chip's own section. Leaves out Doctor
    /// mode: the host's, not the Game Boy's.
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.tag(b"BUS ");
        w.u8(self.model as u8);
        w.sized_bytes(&self.wram[..self.wram_size()]);
        w.u8(self.svbk);
        w.bool(self.speed_armed);
        w.bool(self.double_speed);
        w.u16(self.hdma.src);
        w.u16(self.hdma.dst);
        w.bool(self.hdma.active);
        w.u8(self.hdma.remaining);
        let dma = &self.oam_dma;
        w.bool(dma.active);
        w.bytes(&[
            dma.page,
            dma.copied,
            dma.starting,
            dma.next_page,
            dma.cycles,
        ]);
        w.bytes(&self.hram);
        w.bytes(&self.io);
        w.bytes(&[self.if_reg, self.ie_reg]);
        self.serial.save_state(w);
        self.cart.save_state(w);
        self.ppu.save_state(w);
        self.timer.save_state(w);
        self.joypad.save_state(w);
        self.apu.save_state(w);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"BUS ")?;
        let made_on = r.u8()?;
        if made_on != self.model as u8 {
            return Err(match made_on {
                m if m == Model::Dmg as u8 => StateError::WrongModel(Model::Dmg),
                m if m == Model::Cgb as u8 => StateError::WrongModel(Model::Cgb),
                _ => StateError::Corrupt("console"),
            });
        }
        let size = self.wram_size();
        r.sized_bytes(&mut self.wram[..size])?;
        self.svbk = r.u8()? & 7;
        self.speed_armed = r.bool()?;
        self.double_speed = r.bool()?;
        self.hdma = Hdma {
            src: r.u16()? & 0xFFF0,
            dst: r.u16()? & 0x1FF0,
            active: r.bool()?,
            remaining: r.u8()? & 0x7F,
        };
        self.timer.set_double_speed(self.double_speed);
        let active = r.bool()?;
        let mut dma = [0; 5];
        r.bytes(&mut dma)?;
        let [page, copied, starting, next_page, cycles] = dma;
        if copied >= 0xA0 || starting > OAM_DMA_START || cycles >= 4 {
            return Err(StateError::Corrupt("OAM DMA"));
        }
        self.oam_dma = OamDma {
            active,
            page,
            copied,
            starting,
            next_page,
            cycles,
        };
        r.bytes(&mut self.hram)?;
        r.bytes(&mut self.io)?;
        self.if_reg = r.u8()?;
        self.ie_reg = r.u8()?;
        self.serial.load_state(r)?;
        self.cart.load_state(r)?;
        self.ppu.load_state(r)?;
        self.timer.load_state(r)?;
        self.joypad.load_state(r)?;
        self.apu.load_state(r)
    }

    /// The Color's own features are on: a Color running a Color cartridge
    /// (Pan Docs' "CGB mode"), not one in compatibility mode.
    fn cgb(&self) -> bool {
        self.model == Model::Cgb && !self.compat
    }

    /// The part of `wram` this model has.
    fn wram_size(&self) -> usize {
        if self.model == Model::Cgb {
            0x8000
        } else {
            0x2000
        }
    }

    /// Where $C000-$DFFF (or its echo at $E000-$FDFF) is in `wram`.
    /// https://gbdev.io/pandocs/CGB_Registers.html#ff70--svbk-cgb-mode-only-wram-bank
    fn wram_index(&self, addr: u16) -> usize {
        let offset = usize::from(addr & 0x1FFF);
        if offset < 0x1000 {
            offset
        } else {
            let bank = usize::from(self.svbk.max(1)); // 0 maps bank 1
            bank * 0x1000 + offset - 0x1000
        }
    }

    /// STOP calls this: with KEY1 armed, a Color switches speed instead of
    /// stopping. Returns whether it did.
    /// https://gbdev.io/pandocs/CGB_Registers.html#ff4d--key1-cgb-mode-only-prepare-speed-switch
    pub fn speed_switch(&mut self) -> bool {
        if !(self.cgb() && self.speed_armed) {
            return false;
        }
        self.speed_armed = false;
        self.double_speed = !self.double_speed;
        self.timer.set_double_speed(self.double_speed);
        true
    }

    /// VRAM DMA's registers, $FF51-$FF55 (Color only; HDMA1-4 can't be read).
    fn read_hdma(&self, addr: u16) -> u8 {
        match addr {
            // Bit 7 is 0 while an HBlank copy is running.
            0xFF55 => u8::from(!self.hdma.active) << 7 | self.hdma.remaining,
            // TODO(accuracy): what HDMA1-4 read back varies; $FF is common.
            _ => 0xFF,
        }
    }

    /// https://gbdev.io/pandocs/CGB_Registers.html#lcd-vram-dma-transfers
    fn write_hdma(&mut self, addr: u16, val: u8) {
        let h = &mut self.hdma;
        match addr {
            // The source's and destination's low 4 bits are ignored, and the
            // destination is always in VRAM ($8000-$9FF0).
            0xFF51 => h.src = (h.src & 0x00FF) | (u16::from(val) << 8),
            0xFF52 => h.src = (h.src & 0xFF00) | u16::from(val & 0xF0),
            0xFF53 => h.dst = (h.dst & 0x00FF) | (u16::from(val & 0x1F) << 8),
            0xFF54 => h.dst = (h.dst & 0xFF00) | u16::from(val & 0xF0),
            // Writing bit 7 = 0 during an HBlank copy stops it.
            _ if h.active && val & 0x80 == 0 => h.active = false,
            // Bit 7 = 0: general-purpose, everything now while the CPU waits.
            _ if val & 0x80 == 0 => {
                for _ in 0..=val & 0x7F {
                    self.hdma_block();
                }
                self.hdma.remaining = 0x7F; // reads $FF: done
            }
            // Bit 7 = 1: a block of $10 bytes in each HBlank.
            // TODO(accuracy): started with the LCD off, hardware copies one
            // block straight away; here nothing happens until HBlanks resume.
            _ => {
                h.active = true;
                h.remaining = val & 0x7F;
            }
        }
    }

    /// Copies $10 bytes to VRAM (the bank VBK selects) and charges the CPU
    /// the time: about 8 µs, which is 32 T-cycles, or 64 in double speed.
    fn hdma_block(&mut self) {
        for i in 0..0x10 {
            let byte = self.read(self.hdma.src.wrapping_add(i));
            let dst = 0x8000 | ((self.hdma.dst + i) & 0x1FFF);
            self.ppu.write_vram(dst, byte);
        }
        self.hdma.src = self.hdma.src.wrapping_add(0x10);
        self.hdma.dst = (self.hdma.dst + 0x10) & 0x1FF0;
        self.dma_stall += if self.double_speed { 64 } else { 32 };
    }

    /// CPU T-cycles the CPU now has to wait for VRAM DMA.
    pub fn take_dma_stall(&mut self) -> u32 {
        std::mem::take(&mut self.dma_stall)
    }

    /// The real time `cpu_cycles` of CPU T-cycles take, in normal-speed
    /// T-cycles: half as long in double speed.
    pub fn real_cycles(&self, cpu_cycles: u32) -> u32 {
        if self.double_speed {
            cpu_cycles / 2
        } else {
            cpu_cycles
        }
    }

    /// Whether something else has `addr` so the CPU can't reach it: OAM
    /// while OAM DMA copies or the PPU scans and draws (modes 2 and 3), VRAM
    /// and the Color's palette data while the PPU draws (mode 3).
    /// https://gbdev.io/pandocs/Rendering.html#ppu-modes
    /// TODO(accuracy): during OAM DMA the DMG's CPU can't use the bus the copy
    /// reads from either (cartridge and WRAM, or VRAM): it gets the byte being
    /// copied. Code waiting for DMA runs from HRAM, so games don't notice.
    /// The PPU's edges differ a little for reads and writes: see
    /// [`Ppu::oam_locked`].
    fn cpu_locked_out(&self, addr: u16, write: bool) -> bool {
        match addr {
            0x8000..=0x9FFF => self.ppu.vram_locked(write),
            0xFE00..=0xFEFF => self.oam_dma.active || self.ppu.oam_locked(write),
            0xFF69 | 0xFF6B => self.cgb() && self.ppu.vram_locked(write),
            _ => false,
        }
    }

    /// When a CPU write to `addr` lands within its M-cycle (see
    /// [`WriteTiming`]). Differs between the original, the Color, and the
    /// Color in double speed.
    /// TODO(accuracy): the Color's palettes are CPU revision C's (revision D
    /// and later take writes a dot sooner); the Color's LCDC tile-select
    /// glitch and the original's WX "just written" dot aren't modeled.
    pub(crate) fn write_timing(&self, addr: u16) -> WriteTiming {
        use WriteTiming::*;
        if !(0xFF00..=0xFF7F).contains(&addr) {
            return End;
        }
        let dmg = self.model == Model::Dmg;
        let double = self.double_speed;
        match addr {
            0xFF0F => Late, // IF
            // LCDC: the original's background-enable bit lands a dot early.
            0xFF40 if dmg => Staged {
                first: |old, new| old | (new & 0x01),
                early: 2,
                then: 1,
            },
            0xFF40 if double => Staged {
                first: |old, new| (new & !0x81) | (old & 0x81),
                early: 2,
                then: 2,
            },
            // STAT: on the original it reads as all ones for a dot (the
            // STAT write bug: a spurious interrupt if any source is active).
            0xFF41 if dmg => Staged {
                first: |_, _| 0xFF,
                early: 0,
                then: 1,
            },
            0xFF41 if double => Staged {
                first: |old, new| (new & !0x08) | (old & 0x08),
                early: 0,
                then: 1,
            },
            0xFF41 => Staged {
                first: |old, new| (old & 0x40) | (new & !0x40),
                early: 0,
                then: 1,
            },
            0xFF42 if dmg => Early(1),           // SCY
            0xFF43 if dmg || double => Early(2), // SCX
            0xFF45 if !dmg && !double => Late,   // LYC
            // BGP, OBP0, OBP1: the original's are read by the LCD directly,
            // and for a dot hold the old and new values ORed together.
            0xFF47..=0xFF49 if dmg => Staged {
                first: |old, new| old | new,
                early: 2,
                then: 1,
            },
            0xFF47..=0xFF49 if !double => Early(1),
            0xFF4B if dmg => EndThenDot, // WX
            0xFF4B if !double => Late,
            _ => End,
        }
    }

    /// A read by the CPU: like [`read`](Self::read), but memory the CPU is
    /// locked out of reads $FF.
    pub fn cpu_read(&self, addr: u16) -> u8 {
        if self.cpu_locked_out(addr, false) {
            return 0xFF;
        }
        self.read(addr)
    }

    /// A write by the CPU: lost where it's locked out (though a palette
    /// write still moves the palette index on).
    pub fn cpu_write(&mut self, addr: u16, val: u8) {
        if self.cpu_locked_out(addr, true) {
            if matches!(addr, 0xFF69 | 0xFF6B) {
                self.ppu.lost_palette_write(addr);
            }
            return;
        }
        self.write(addr, val);
    }

    /// Reads memory as it is, with no regard for who has the bus: for DMA,
    /// debuggers and tests.
    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => self.cart.read_rom(addr),
            0x8000..=0x9FFF => self.ppu.read_vram(addr),
            0xA000..=0xBFFF => self.cart.read_ram(addr),
            0xC000..=0xFDFF => self.wram[self.wram_index(addr)], // $E000-: echo RAM
            0xFE00..=0xFE9F => self.ppu.read_oam(addr),
            0xFEA0..=0xFEFF => 0xFF, // unusable
            0xFF00..=0xFF7F => self.read_io(addr),
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize],
            0xFFFF => self.ie_reg,
        }
    }

    fn read_io(&self, addr: u16) -> u8 {
        match addr {
            0xFF00 => self.joypad.read(),
            0xFF01 | 0xFF02 => self.serial.read(addr),
            0xFF04..=0xFF07 => self.timer.read(addr),
            0xFF10..=0xFF3F => self.apu.read(addr),
            0xFF0F => self.if_reg | 0xE0,
            0xFF44 if self.doctor_mode => 0x90,
            0xFF40..=0xFF4B => self.ppu.read_reg(addr),
            // In compatibility mode VBK still reads (bank 0, for good), and so
            // do the palette index registers and OPRI, as the boot ROM left
            // them (Mooneye's boot_hwio-C).
            0xFF4F | 0xFF68 | 0xFF6A | 0xFF6C if self.compat => self.read_color_io(addr),
            // Color registers: unused bits read 1; on the original, all of it.
            0xFF4D | 0xFF4F | 0xFF51..=0xFF55 | 0xFF68..=0xFF6C | 0xFF70 if !self.cgb() => 0xFF,
            0xFF4D | 0xFF4F | 0xFF51..=0xFF55 | 0xFF68..=0xFF6C | 0xFF70 => {
                self.read_color_io(addr)
            }
            // The Color's undocumented registers: $FF72-$FF73 read back
            // anything, $FF74 too (in Color mode only), $FF75 bits 4-6.
            // https://gbdev.io/pandocs/CGB_Registers.html#undocumented-registers
            0xFF72 | 0xFF73 if self.model == Model::Cgb => self.io[usize::from(addr - 0xFF00)],
            0xFF74 if self.cgb() => self.io[0x74],
            0xFF75 if self.model == Model::Cgb => self.io[0x75] | 0x8F,
            // PCM12/PCM34: the channels' current output levels.
            // TODO(accuracy): the real levels; 0 is what they read while silent.
            0xFF76 | 0xFF77 if self.model == Model::Cgb => 0x00,
            // TODO(accuracy): the Color's infrared port; nothing to receive.
            0xFF56 if self.cgb() => self.io[0x56],
            // Nothing there: reads $FF.
            _ => 0xFF,
        }
    }

    /// The Color's own registers.
    fn read_color_io(&self, addr: u16) -> u8 {
        match addr {
            0xFF4D => 0x7E | (u8::from(self.double_speed) << 7) | u8::from(self.speed_armed),
            0xFF4F => 0xFE | self.ppu.vram_bank(),
            0xFF51..=0xFF55 => self.read_hdma(addr),
            0xFF68..=0xFF6C => self.ppu.read_color_reg(addr),
            _ => 0xF8 | self.svbk,
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        match addr {
            0x0000..=0x7FFF => self.cart.write_rom(addr, val), // MBC registers
            0x8000..=0x9FFF => self.ppu.write_vram(addr, val),
            0xA000..=0xBFFF => self.cart.write_ram(addr, val),
            0xC000..=0xFDFF => self.wram[self.wram_index(addr)] = val,
            0xFE00..=0xFE9F => self.ppu.write_oam(addr, val),
            0xFEA0..=0xFEFF => {}
            0xFF00..=0xFF7F => self.write_io(addr, val),
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize] = val,
            0xFFFF => self.ie_reg = val,
        }
    }

    fn write_io(&mut self, addr: u16, val: u8) {
        match addr {
            0xFF00 => self.joypad.write(val),
            0xFF01 | 0xFF02 => self.serial.write(addr, val),
            0xFF04..=0xFF07 => self.timer.write(addr, val),
            0xFF10..=0xFF3F => self.apu.write(addr, val),
            0xFF0F => self.if_reg = val | 0xE0,
            0xFF46 => self.oam_dma(val),
            0xFF40..=0xFF4B => self.ppu.write_reg(addr, val),
            // In compatibility mode only the palette index registers still
            // take writes, which change nothing anyone sees.
            0xFF68 | 0xFF6A if self.compat => self.ppu.write_color_reg(addr, val),
            0xFF4D | 0xFF4F | 0xFF51..=0xFF55 | 0xFF68..=0xFF6C | 0xFF70 if !self.cgb() => {}
            0xFF4D => self.speed_armed = val & 1 != 0,
            0xFF4F => self.ppu.set_vram_bank(val & 1),
            0xFF51..=0xFF55 => self.write_hdma(addr, val),
            0xFF68..=0xFF6C => self.ppu.write_color_reg(addr, val),
            0xFF70 => self.svbk = val & 7,
            _ => self.io[(addr - 0xFF00) as usize] = val,
        }
    }

    pub fn read16(&self, addr: u16) -> u16 {
        u16::from_le_bytes([self.read(addr), self.read(addr.wrapping_add(1))])
    }

    pub fn write16(&mut self, addr: u16, val: u16) {
        let [lo, hi] = val.to_le_bytes();
        self.write(addr, lo);
        self.write(addr.wrapping_add(1), hi);
    }

    /// A write to $FF46: asks for a copy from page `page`. See [`OamDma`].
    fn oam_dma(&mut self, page: u8) {
        self.ppu.dma = page;
        self.oam_dma.next_page = page;
        self.oam_dma.starting = OAM_DMA_START;
    }

    /// One M-cycle of OAM DMA: the running copy moves a byte, and a copy
    /// asked for gets closer to starting. Sources $E000 and up read WRAM
    /// ($FE00 copies from $DE00), as on the original.
    /// TODO(accuracy): the Color reads other things from $E000 up.
    fn oam_dma_m_cycle(&mut self) {
        let dma = &mut self.oam_dma;
        if dma.active {
            let mut src = u16::from_be_bytes([dma.page, dma.copied]);
            if src >= 0xE000 {
                src -= 0x2000;
            }
            let i = u16::from(dma.copied);
            dma.copied += 1;
            dma.active = dma.copied < 0xA0;
            let byte = self.read(src);
            self.ppu.write_oam(0xFE00 + i, byte);
        }
        let dma = &mut self.oam_dma;
        if dma.starting > 0 {
            dma.starting -= 1;
            if dma.starting == 0 {
                dma.active = true;
                dma.page = dma.next_page;
                dma.copied = 0;
            }
        }
    }

    /// Advances timer, PPU, cartridge clock and APU by `cycles` CPU
    /// T-cycles, collecting any interrupts they raise. In double speed the
    /// timer keeps pace with the CPU, but the PPU, sound and clock see half.
    pub fn tick(&mut self, cycles: u32) {
        if self.oam_dma.active || self.oam_dma.starting > 0 {
            let mut total = u32::from(self.oam_dma.cycles) + cycles;
            while total >= 4 && (self.oam_dma.active || self.oam_dma.starting > 0) {
                total -= 4;
                self.oam_dma_m_cycle();
            }
            self.oam_dma.cycles = (total % 4) as u8;
        }
        if self.timer.tick(cycles) {
            self.if_reg |= interrupt::TIMER;
        }
        if self.serial.tick(cycles) {
            self.if_reg |= interrupt::SERIAL;
        }
        let real = self.real_cycles(cycles);
        self.if_reg |= self.ppu.tick(real);
        // An HBlank copy moves one block per HBlank, paused while the CPU
        // is halted. (Only the Color can start one.)
        for _ in 0..self.ppu.take_hblanks() {
            if self.hdma.active && !self.cpu_halted {
                self.hdma_block();
                if self.hdma.remaining == 0 {
                    self.hdma.active = false;
                    self.hdma.remaining = 0x7F;
                } else {
                    self.hdma.remaining -= 1;
                }
            }
        }
        self.cart.tick(real);
        for _ in 0..self.timer.take_div_apu_ticks() {
            self.apu.frame_sequencer_tick();
        }
        self.apu.tick(real);
    }

    /// Interrupts that are both requested (IF) and enabled (IE).
    pub fn pending_interrupts(&self) -> u8 {
        self.if_reg & self.ie_reg & 0x1F
    }

    pub fn take_serial_output(&mut self) -> String {
        let bytes = self.serial.take_log();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cartridge::tests::rom_with_program;

    fn bus() -> Bus {
        Bus::new(
            Cartridge::from_rom(rom_with_program(&[])).unwrap(),
            Model::Dmg,
        )
    }

    #[test]
    fn echo_ram_mirrors_wram() {
        let mut b = bus();
        b.write(0xC123, 0x42);
        assert_eq!(b.read(0xE123), 0x42);
        b.write(0xE200, 0x99);
        assert_eq!(b.read(0xC200), 0x99);
    }

    #[test]
    fn serial_transfer_is_captured_and_raises_interrupt() {
        let mut b = bus();
        b.if_reg = 0;
        for &c in b"Hi" {
            b.write(0xFF01, c);
            b.write(0xFF02, 0x81);
            assert_eq!(b.if_reg & interrupt::SERIAL, 0, "8 bits take time");
            b.tick(8 * 512);
            assert_eq!(b.if_reg & interrupt::SERIAL, interrupt::SERIAL);
            assert_eq!(b.read(0xFF02) & 0x80, 0, "transfer flag clears when done");
            b.if_reg = 0;
        }
        assert_eq!(b.take_serial_output(), "Hi");
    }

    #[test]
    fn oam_dma_takes_160_m_cycles_and_holds_oam_meanwhile() {
        let mut b = bus();
        b.write(0xFF40, 0x00); // LCD off: only the copy holds OAM
        for i in 0..0xA0u16 {
            b.write(0xC100 + i, 0x10 + i as u8);
        }
        b.write(0xFE00, 0x99);
        b.cpu_write(0xFF46, 0xC1);
        b.tick(4);
        assert_eq!(b.cpu_read(0xFE00), 0x99, "an M-cycle to start up");
        b.tick(4);
        assert_eq!(b.cpu_read(0xFE00), 0xFF, "OAM belongs to the copy");
        b.cpu_write(0xFE00, 0x55); // lost
        b.tick(4);
        assert_eq!(b.read(0xFE00), 0x10, "the first byte");
        assert_eq!(b.read(0xFE01), 0x00, "not the second yet");
        b.tick(4 * 158);
        assert_eq!(b.cpu_read(0xFE9F), 0xFF, "still copying");
        b.tick(4);
        assert_eq!(b.cpu_read(0xFE9F), 0x10 + 0x9F, "done: 160 M-cycles held");
        assert_eq!(b.cpu_read(0xFE00), 0x10, "the CPU's write was lost");
    }

    #[test]
    fn restarting_oam_dma_keeps_oam_held_until_the_new_copy_starts() {
        let mut b = bus();
        b.write(0xFF40, 0x00); // LCD off: only the copy holds OAM
        b.write(0xC200, 0x77);
        b.cpu_write(0xFF46, 0xC1);
        b.tick(4 * 100);
        b.cpu_write(0xFF46, 0xC2);
        b.tick(4 * 2);
        assert_eq!(b.cpu_read(0xFE00), 0xFF, "the old copy carried on");
        assert_eq!(b.read(0xFF46), 0xC2);
        b.tick(4 * 160);
        assert_eq!(b.cpu_read(0xFE00), 0x77, "the new copy, from the start");
    }

    /// What the CPU gets reading VRAM and OAM, both holding $42, `dots` into
    /// line 1 of a fresh frame (line 0, just after the LCD comes on, has no
    /// OAM scan, and starts a few dots in).
    fn cpu_sees(b: &mut Bus, dots: u32) -> (u8, u8) {
        b.write(0xFF40, 0x00);
        b.write(0x8000, 0x42);
        b.write(0xFE00, 0x42);
        b.write(0xFF40, 0x91); // back on: line 0 starts over
        let line_0 = if b.model == Model::Dmg {
            456 - 2
        } else {
            456 - 3
        };
        b.tick(line_0 + dots);
        (b.cpu_read(0x8000), b.cpu_read(0xFE00))
    }

    #[test]
    fn the_cpu_is_locked_out_of_oam_in_modes_2_and_3_and_vram_in_mode_3() {
        let mut b = bus();
        assert_eq!(cpu_sees(&mut b, 40), (0x42, 0xFF), "mode 2: OAM busy");
        assert_eq!(cpu_sees(&mut b, 100), (0xFF, 0xFF), "mode 3: both busy");
        assert_eq!(cpu_sees(&mut b, 300), (0x42, 0x42), "HBlank: free");
        assert_eq!(cpu_sees(&mut b, 456 * 145), (0x42, 0x42), "VBlank: free");
        // Locked out, writes are lost; the PPU's own copies aren't affected.
        cpu_sees(&mut b, 100);
        b.cpu_write(0x8000, 0x99);
        b.cpu_write(0xFE00, 0x99);
        assert_eq!((b.read(0x8000), b.read(0xFE00)), (0x42, 0x42));
        // With the LCD off, all of it is the CPU's.
        b.write(0xFF40, 0x00);
        b.cpu_write(0x8000, 0x99);
        assert_eq!(b.cpu_read(0x8000), 0x99);
    }

    #[test]
    fn mode_3_lasts_longer_with_scroll_and_so_does_the_lock() {
        let mut b = bus();
        b.ppu.scx = 7;
        assert_eq!(cpu_sees(&mut b, 252 + 6).0, 0xFF, "still drawing");
        assert_eq!(cpu_sees(&mut b, 252 + 7).0, 0x42);
    }

    #[test]
    fn color_palette_writes_in_mode_3_are_lost_but_move_the_index_on() {
        let mut b = cgb_bus();
        cpu_sees(&mut b, 100); // mode 3
        b.cpu_write(0xFF68, 0x80); // BCPS: byte 0, auto-increment
        b.cpu_write(0xFF69, 0x12);
        assert_eq!(b.cpu_read(0xFF68), 0xC1, "the index moved on");
        assert_eq!(b.cpu_read(0xFF69), 0xFF, "data unreadable in mode 3");
        b.tick(300 - 100); // HBlank
        b.cpu_write(0xFF68, 0x80);
        assert_eq!(b.cpu_read(0xFF69), 0xFF, "the write was lost: still white");
    }

    #[test]
    fn oam_dma_from_e000_up_reads_wram() {
        let mut b = bus();
        b.write(0xDE05, 0x42);
        b.cpu_write(0xFF46, 0xFE);
        b.tick(4 * 162);
        assert_eq!(b.read(0xFE05), 0x42);
    }

    #[test]
    fn bus_ticks_drive_the_cartridge_clock() {
        // MBC3+TIMER+RAM+BATTERY: one second of bus time ticks the RTC.
        let rom = crate::cartridge::tests::make_rom(0x10, 4, 0x03);
        let mut b = Bus::new(Cartridge::from_rom(rom).unwrap(), Model::Dmg);
        b.write(0x0000, 0x0A); // enable RAM and clock
        b.tick(crate::CPU_HZ);
        b.write(0x6000, 0x00); // latch
        b.write(0x6000, 0x01);
        b.write(0x4000, 0x08); // seconds register
        assert_eq!(b.read(0xA000), 1);
    }

    /// A Color with a Color cartridge ($0143 = $80), so in Color mode.
    fn cgb_bus() -> Bus {
        let mut rom = rom_with_program(&[]);
        rom[0x143] = 0x80;
        rom[0x14D] = crate::cartridge::header_checksum(&rom);
        Bus::new(Cartridge::from_rom(rom).unwrap(), Model::Cgb)
    }

    /// A Color with an original cartridge: compatibility mode.
    fn compat_bus() -> Bus {
        Bus::new(
            Cartridge::from_rom(rom_with_program(&[])).unwrap(),
            Model::Cgb,
        )
    }

    #[test]
    fn compatibility_mode_shuts_the_colors_own_registers() {
        let mut b = compat_bus();
        assert!(b.compat);
        b.write(0xFF4F, 1); // VBK
        b.write(0xFF70, 3); // SVBK
        b.write(0xFF4D, 1); // KEY1
        assert_eq!(b.read(0xFF4F), 0xFE, "VRAM bank 0 for good");
        assert_eq!(b.read(0xFF70), 0xFF);
        assert_eq!(b.read(0xFF4D), 0xFF);
        assert!(!b.speed_switch(), "no double speed");
        b.write(0xD000, 0x12);
        assert_eq!(b.wram[0x1000], 0x12, "$D000 is WRAM bank 1");
        // The palette indexes as the boot ROM left them; the data is shut.
        assert_eq!((b.read(0xFF68), b.read(0xFF6A)), (0xC8, 0xD0));
        b.write(0xFF68, 0x80);
        b.write(0xFF69, 0x12);
        assert_eq!(b.read(0xFF69), 0xFF);
        assert_eq!(
            b.read(0xFF6C),
            0xFF,
            "OPRI: sprites by X, as on the original"
        );
        // Undocumented $FF74 is Color mode only; $FF72 is there either way.
        b.write(0xFF72, 0x5A);
        b.write(0xFF74, 0x5A);
        assert_eq!((b.read(0xFF72), b.read(0xFF74)), (0x5A, 0xFF));
    }

    #[test]
    fn unused_io_reads_ff_and_the_colors_extras_only_exist_there() {
        let mut b = bus();
        for addr in [0xFF03, 0xFF08, 0xFF4C, 0xFF50, 0xFF72, 0xFF75, 0xFF7F] {
            b.write(addr, 0x00);
            assert_eq!(b.read(addr), 0xFF, "${addr:04X}");
        }
        let mut b = cgb_bus();
        b.write(0xFF75, 0x00);
        assert_eq!(b.read(0xFF75), 0x8F, "only bits 4-6 are there");
        b.write(0xFF74, 0x5A);
        assert_eq!(b.read(0xFF74), 0x5A);
    }

    #[test]
    fn color_wram_has_eight_banks_at_d000() {
        let mut b = cgb_bus();
        b.write(0xC000, 0xC0);
        for bank in 1..8 {
            b.write(0xFF70, bank);
            b.write(0xD000, 0x10 + bank);
        }
        for bank in 1..8 {
            b.write(0xFF70, bank);
            assert_eq!(b.read(0xD000), 0x10 + bank, "bank {bank}");
            assert_eq!(b.read(0xC000), 0xC0, "$C000-$CFFF is always bank 0");
            assert_eq!(b.read(0xF000), 0x10 + bank, "the echo follows the bank");
        }
        b.write(0xFF70, 0);
        assert_eq!(b.read(0xD000), 0x11, "bank 0 maps bank 1");
        assert_eq!(b.read(0xFF70), 0xF8, "SVBK reads back 0 (unused bits 1)");
        b.write(0xFF70, 0xFB);
        assert_eq!(b.read(0xFF70), 0xFB, "3 bits");
        assert_eq!(b.read(0xD000), 0x13);
    }

    #[test]
    fn color_vram_has_two_banks() {
        let mut b = cgb_bus();
        b.write(0x8000, 0xAA);
        b.write(0xFF4F, 1);
        assert_eq!(b.read(0xFF4F), 0xFF, "bank 1, unused bits 1");
        assert_eq!(b.read(0x8000), 0x00, "a different bank");
        b.write(0x9FFF, 0xBB);
        b.write(0xFF4F, 0xFE);
        assert_eq!(b.read(0xFF4F), 0xFE, "only bit 0 counts");
        assert_eq!(b.read(0x8000), 0xAA);
        assert_eq!(b.read(0x9FFF), 0x00);
    }

    #[test]
    fn key1_arms_a_speed_switch_that_stop_performs() {
        let mut b = cgb_bus();
        assert_eq!(b.read(0xFF4D), 0x7E, "normal speed, not armed");
        assert!(!b.speed_switch(), "not armed: STOP doesn't switch");
        b.write(0xFF4D, 0x01);
        assert_eq!(b.read(0xFF4D), 0x7F);
        assert!(b.speed_switch());
        assert!(b.double_speed);
        assert_eq!(b.read(0xFF4D), 0xFE, "double speed, disarmed");
        b.write(0xFF4D, 0x01);
        assert!(b.speed_switch());
        assert_eq!(b.read(0xFF4D), 0x7E, "and back");
    }

    #[test]
    fn the_original_has_no_color_registers() {
        let mut b = bus();
        b.write(0xD000, 0x55);
        for reg in [
            0xFF4D, 0xFF4F, 0xFF55, 0xFF68, 0xFF69, 0xFF6A, 0xFF6B, 0xFF6C, 0xFF70,
        ] {
            b.write(reg, 0x01);
            assert_eq!(b.read(reg), 0xFF, "${reg:04X}");
        }
        assert_eq!(b.read(0xD000), 0x55, "SVBK did nothing");
        assert!(!b.speed_switch());
    }

    /// Puts `bytes` at $C000 and points HDMA from there to VRAM $8000.
    fn hdma_bus(bytes: &[u8]) -> Bus {
        let mut b = cgb_bus();
        for (i, &v) in bytes.iter().enumerate() {
            b.write(0xC000 + i as u16, v);
        }
        b.write(0xFF51, 0xC0);
        b.write(0xFF52, 0x0F); // low 4 bits ignored: $C000
        b.write(0xFF53, 0xE0); // only bits 12-8 count: $0000
        b.write(0xFF54, 0x0F); // low 4 bits ignored
        b
    }

    fn vram_bytes(b: &Bus, from: u16, n: u16) -> Vec<u8> {
        (from..from + n).map(|a| b.read(a)).collect()
    }

    #[test]
    fn general_purpose_dma_copies_everything_at_once_and_the_cpu_waits() {
        let data: Vec<u8> = (1..=0x30).collect();
        let mut b = hdma_bus(&data);
        assert_eq!(b.read(0xFF55), 0xFF, "idle");
        b.write(0xFF55, 0x01); // 2 blocks of $10
        assert_eq!(vram_bytes(&b, 0x8000, 0x20), data[..0x20]);
        assert_eq!(b.read(0x8020), 0, "no more than asked");
        assert_eq!(b.read(0xFF55), 0xFF, "done");
        assert_eq!(b.take_dma_stall(), 2 * 32, "8 µs a block");
        assert_eq!(b.read(0xFF51), 0xFF, "the address registers can't be read");
    }

    #[test]
    fn dma_goes_to_the_selected_vram_bank_and_costs_more_cycles_in_double_speed() {
        let mut b = hdma_bus(&[0xAB; 0x10]);
        b.write(0xFF4F, 1);
        b.write(0xFF4D, 1);
        b.speed_switch();
        b.write(0xFF55, 0x00);
        assert_eq!(b.read(0x8000), 0xAB);
        b.write(0xFF4F, 0);
        assert_eq!(b.read(0x8000), 0x00, "bank 0 untouched");
        assert_eq!(
            b.take_dma_stall(),
            64,
            "the same 8 µs is twice the CPU cycles"
        );
    }

    #[test]
    fn hblank_dma_copies_a_block_per_hblank_and_can_be_stopped() {
        let data: Vec<u8> = (1..=0x40).collect();
        let mut b = hdma_bus(&data);
        b.write(0xFF55, 0x83); // 4 blocks, one per HBlank
        assert_eq!(b.read(0xFF55), 0x03, "bit 7 = 0: running");
        assert_eq!(b.read(0x8000), 0, "nothing until an HBlank");
        b.tick(252); // line 0's HBlank begins
        assert_eq!(vram_bytes(&b, 0x8000, 0x10), data[..0x10]);
        assert_eq!(b.read(0x8010), 0);
        assert_eq!(b.read(0xFF55), 0x02);
        assert_eq!(b.take_dma_stall(), 32);
        b.tick(456);
        assert_eq!(vram_bytes(&b, 0x8000, 0x20), data[..0x20]);
        b.write(0xFF55, 0x00); // stop
        assert_eq!(b.read(0xFF55), 0x81, "stopped with 2 blocks left");
        b.tick(456);
        assert_eq!(b.read(0x8020), 0, "no more after stopping");
    }

    #[test]
    fn hblank_dma_finishes_and_pauses_while_the_cpu_is_halted() {
        let mut b = hdma_bus(&[0x11; 0x20]);
        b.write(0xFF55, 0x81); // 2 blocks
        b.cpu_halted = true;
        b.tick(456);
        assert_eq!(b.read(0x8000), 0, "halted: this HBlank is skipped");
        b.cpu_halted = false;
        b.tick(456);
        b.tick(456);
        assert_eq!(vram_bytes(&b, 0x8000, 0x20), [0x11; 0x20]);
        assert_eq!(b.read(0xFF55), 0xFF, "finished");
    }

    #[test]
    fn double_speed_gives_the_ppu_half_the_cycles() {
        let mut b = cgb_bus();
        b.write(0xFF4D, 0x01);
        b.speed_switch();
        let ly = b.ppu.ly;
        b.tick(456);
        assert_eq!(b.ppu.ly, ly, "456 CPU cycles is half a line now");
        b.tick(456);
        assert_eq!(b.ppu.ly, ly + 1);
    }

    #[test]
    fn word_access_is_little_endian() {
        let mut b = bus();
        b.write16(0xC000, 0xBEEF);
        assert_eq!(b.read(0xC000), 0xEF);
        assert_eq!(b.read16(0xC000), 0xBEEF);
    }
}
