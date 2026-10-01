//! The memory map. Every CPU read and write goes through here and is routed
//! to the cartridge, RAM, or a hardware register.
//!
//! Reference: https://gbdev.io/pandocs/Memory_Map.html

use crate::{cartridge::Cartridge, joypad::Joypad, ppu::Ppu, timer::Timer};

/// Bits of IF ($FF0F) and IE ($FFFF), in priority order.
pub mod interrupt {
    pub const VBLANK: u8 = 0x01;
    pub const STAT: u8 = 0x02;
    pub const TIMER: u8 = 0x04;
    pub const SERIAL: u8 = 0x08;
    pub const JOYPAD: u8 = 0x10;
}

pub struct Bus {
    pub cart: Cartridge,
    pub ppu: Ppu,
    pub timer: Timer,
    pub joypad: Joypad,
    wram: [u8; 0x2000],
    hram: [u8; 0x7F],
    /// Backing store for I/O registers nothing emulates yet (sound, mostly).
    io: [u8; 0x80],
    pub if_reg: u8,
    pub ie_reg: u8,
    serial_data: u8,
    serial_ctrl: u8,
    serial_out: Vec<u8>,
    pub doctor_mode: bool,
}

impl Bus {
    pub fn new(cart: Cartridge) -> Self {
        Self {
            cart,
            ppu: Ppu::new(),
            timer: Timer::new(),
            joypad: Joypad::new(),
            wram: [0; 0x2000],
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

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => self.cart.read_rom(addr),
            0x8000..=0x9FFF => self.ppu.read_vram(addr),
            0xA000..=0xBFFF => self.cart.read_ram(addr),
            0xC000..=0xDFFF => self.wram[(addr - 0xC000) as usize],
            0xE000..=0xFDFF => self.wram[(addr - 0xE000) as usize], // echo RAM
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
            0xFF0F => self.if_reg | 0xE0,
            0xFF44 if self.doctor_mode => 0x90,
            0xFF40..=0xFF4B => self.ppu.read_reg(addr),
            _ => self.io[(addr - 0xFF00) as usize],
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        match addr {
            0x0000..=0x7FFF => self.cart.write_rom(addr, val), // MBC registers
            0x8000..=0x9FFF => self.ppu.write_vram(addr, val),
            0xA000..=0xBFFF => self.cart.write_ram(addr, val),
            0xC000..=0xDFFF => self.wram[(addr - 0xC000) as usize] = val,
            0xE000..=0xFDFF => self.wram[(addr - 0xE000) as usize] = val,
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
            0xFF0F => self.if_reg = val | 0xE0,
            0xFF46 => self.oam_dma(val),
            0xFF40..=0xFF4B => self.ppu.write_reg(addr, val),
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

    /// Advances timer, PPU and cartridge clock, collecting any interrupts
    /// they raise.
    pub fn tick(&mut self, cycles: u32) {
        if self.timer.tick(cycles) {
            self.if_reg |= interrupt::TIMER;
        }
        self.if_reg |= self.ppu.tick(cycles);
        self.cart.tick(cycles);
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
        Bus::new(Cartridge::from_rom(rom_with_program(&[])).unwrap())
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
        let mut b = Bus::new(Cartridge::from_rom(rom).unwrap());
        b.write(0x0000, 0x0A); // enable RAM and clock
        b.tick(crate::CPU_HZ);
        b.write(0x6000, 0x00); // latch
        b.write(0x6000, 0x01);
        b.write(0x4000, 0x08); // seconds register
        assert_eq!(b.read(0xA000), 1);
    }

    #[test]
    fn word_access_is_little_endian() {
        let mut b = bus();
        b.write16(0xC000, 0xBEEF);
        assert_eq!(b.read(0xC000), 0xEF);
        assert_eq!(b.read16(0xC000), 0xBEEF);
    }
}
