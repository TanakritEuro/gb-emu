//! The memory map. Every CPU read and write goes through here and is routed
//! to the cartridge, RAM, or a hardware register.
//!
//! Reference: https://gbdev.io/pandocs/Memory_Map.html

use crate::state::{StateError, StateReader, StateWriter};
use crate::{apu::Apu, cartridge::Cartridge, joypad::Joypad, ppu::Ppu, timer::Timer, Model};

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
    hram: [u8; 0x7F],
    /// Backing store for I/O registers nothing emulates yet.
    io: [u8; 0x80],
    pub if_reg: u8,
    pub ie_reg: u8,
    serial_data: u8,
    serial_ctrl: u8,
    serial_out: Vec<u8>,
    pub doctor_mode: bool,
}

impl Bus {
    pub fn new(cart: Cartridge, model: Model) -> Self {
        Self {
            cart,
            ppu: Ppu::with_model(model),
            timer: Timer::new(),
            joypad: Joypad::new(),
            apu: Apu::new(),
            model,
            wram: Box::new([0; 0x8000]),
            svbk: 0,
            speed_armed: false,
            double_speed: false,
            hram: [0; 0x7F],
            io: [0; 0x80],
            if_reg: 0xE1,
            ie_reg: 0,
            serial_data: 0,
            serial_ctrl: 0,
            serial_out: Vec::new(),
            doctor_mode: false,
        }
    }

    /// RAM, I/O, IF/IE and serial, then each chip's own section. Leaves out
    /// the serial output not yet taken and Doctor mode: the host's, not the
    /// Game Boy's.
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.tag(b"BUS ");
        w.u8(self.model as u8);
        w.sized_bytes(&self.wram[..self.wram_size()]);
        w.u8(self.svbk);
        w.bool(self.speed_armed);
        w.bool(self.double_speed);
        w.bytes(&self.hram);
        w.bytes(&self.io);
        w.bytes(&[self.if_reg, self.ie_reg, self.serial_data, self.serial_ctrl]);
        self.cart.save_state(w);
        self.ppu.save_state(w);
        self.timer.save_state(w);
        self.joypad.save_state(w);
        self.apu.save_state(w);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"BUS ")?;
        if r.u8()? != self.model as u8 {
            return Err(StateError::Corrupt("made on a different model"));
        }
        let size = self.wram_size();
        r.sized_bytes(&mut self.wram[..size])?;
        self.svbk = r.u8()? & 7;
        self.speed_armed = r.bool()?;
        self.double_speed = r.bool()?;
        self.timer.set_double_speed(self.double_speed);
        r.bytes(&mut self.hram)?;
        r.bytes(&mut self.io)?;
        let mut b = [0; 4];
        r.bytes(&mut b)?;
        [self.if_reg, self.ie_reg, self.serial_data, self.serial_ctrl] = b;
        self.cart.load_state(r)?;
        self.ppu.load_state(r)?;
        self.timer.load_state(r)?;
        self.joypad.load_state(r)?;
        self.apu.load_state(r)
    }

    fn cgb(&self) -> bool {
        self.model == Model::Cgb
    }

    /// The part of `wram` this model has.
    fn wram_size(&self) -> usize {
        if self.cgb() {
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

    /// The real time `cpu_cycles` of CPU T-cycles take, in normal-speed
    /// T-cycles: half as long in double speed.
    pub fn real_cycles(&self, cpu_cycles: u32) -> u32 {
        if self.double_speed {
            cpu_cycles / 2
        } else {
            cpu_cycles
        }
    }

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
            0xFF01 => self.serial_data,
            0xFF02 => self.serial_ctrl | 0x7E,
            0xFF04..=0xFF07 => self.timer.read(addr),
            0xFF10..=0xFF3F => self.apu.read(addr),
            0xFF0F => self.if_reg | 0xE0,
            0xFF44 if self.doctor_mode => 0x90,
            0xFF40..=0xFF4B => self.ppu.read_reg(addr),
            // Color registers: unused bits read 1; on the original, all of it.
            0xFF4D | 0xFF4F | 0xFF68..=0xFF6B | 0xFF70 if !self.cgb() => 0xFF,
            0xFF4D => 0x7E | (u8::from(self.double_speed) << 7) | u8::from(self.speed_armed),
            0xFF4F => 0xFE | self.ppu.vram_bank(),
            0xFF68..=0xFF6B => self.ppu.read_color_reg(addr),
            0xFF70 => 0xF8 | self.svbk,
            _ => self.io[(addr - 0xFF00) as usize],
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
            0xFF01 => self.serial_data = val,
            0xFF02 => {
                self.serial_ctrl = val;
                // Transfer requested with the internal clock: there's no link
                // partner, so "send" the byte instantly and capture it.
                if val & 0x81 == 0x81 {
                    self.serial_out.push(self.serial_data);
                    self.serial_data = 0xFF;
                    self.serial_ctrl &= 0x7F;
                    self.if_reg |= interrupt::SERIAL;
                }
            }
            0xFF04..=0xFF07 => self.timer.write(addr, val),
            0xFF10..=0xFF3F => self.apu.write(addr, val),
            0xFF0F => self.if_reg = val | 0xE0,
            0xFF46 => self.oam_dma(val),
            0xFF40..=0xFF4B => self.ppu.write_reg(addr, val),
            0xFF4D | 0xFF4F | 0xFF68..=0xFF6B | 0xFF70 if !self.cgb() => {}
            0xFF4D => self.speed_armed = val & 1 != 0,
            0xFF4F => self.ppu.set_vram_bank(val & 1),
            0xFF68..=0xFF6B => self.ppu.write_color_reg(addr, val),
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

    /// Copies 160 bytes from $XX00 into OAM. Real hardware takes 640 T-cycles
    /// and blocks most of the bus meanwhile; doing it instantly is fine until
    /// a game depends on the timing.
    fn oam_dma(&mut self, page: u8) {
        self.ppu.dma = page;
        let src = u16::from(page) << 8;
        for i in 0..0xA0 {
            let byte = self.read(src + i);
            self.ppu.write_oam(0xFE00 + i, byte);
        }
    }

    /// Advances timer, PPU, cartridge clock and APU by `cycles` CPU
    /// T-cycles, collecting any interrupts they raise. In double speed the
    /// timer keeps pace with the CPU, but the PPU, sound and clock see half.
    pub fn tick(&mut self, cycles: u32) {
        if self.timer.tick(cycles) {
            self.if_reg |= interrupt::TIMER;
        }
        let real = self.real_cycles(cycles);
        self.if_reg |= self.ppu.tick(real);
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
        let bytes = std::mem::take(&mut self.serial_out);
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
        }
        assert_eq!(b.take_serial_output(), "Hi");
        assert_eq!(b.if_reg & interrupt::SERIAL, interrupt::SERIAL);
        assert_eq!(b.read(0xFF02) & 0x80, 0, "transfer flag clears when done");
    }

    #[test]
    fn oam_dma_copies_a_page() {
        let mut b = bus();
        for i in 0..0xA0u16 {
            b.write(0xC100 + i, i as u8);
        }
        b.write(0xFF46, 0xC1);
        assert_eq!(b.read(0xFE00), 0x00);
        assert_eq!(b.read(0xFE9F), 0x9F);
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

    fn cgb_bus() -> Bus {
        Bus::new(
            Cartridge::from_rom(rom_with_program(&[])).unwrap(),
            Model::Cgb,
        )
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
        for reg in [0xFF4D, 0xFF4F, 0xFF68, 0xFF69, 0xFF6A, 0xFF6B, 0xFF70] {
            b.write(reg, 0x01);
            assert_eq!(b.read(reg), 0xFF, "${reg:04X}");
        }
        assert_eq!(b.read(0xD000), 0x55, "SVBK did nothing");
        assert!(!b.speed_switch());
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
