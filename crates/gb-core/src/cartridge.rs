//! Cartridge header parsing and memory bank controllers (MBCs).
//!
//! Reference: https://gbdev.io/pandocs/The_Cartridge_Header.html
//!            https://gbdev.io/pandocs/MBC1.html
//!            https://gbdev.io/pandocs/MBC3.html

use crate::CPU_HZ;
use std::fmt;

const ROM_BANK: usize = 0x4000;
const RAM_BANK: usize = 0x2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CartridgeError {
    TooSmall(usize),
    UnsupportedType(u8),
}

impl fmt::Display for CartridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CartridgeError::TooSmall(n) => {
                write!(f, "ROM is only {n} bytes; too small to contain a header")
            }
            CartridgeError::UnsupportedType(t) => {
                write!(f, "cartridge type ${t:02X} isn't supported yet")
            }
        }
    }
}

impl std::error::Error for CartridgeError {}

#[derive(Debug, Clone)]
pub struct Header {
    pub title: String,
    pub cart_type: u8,
    pub checksum_ok: bool,
}

#[derive(Debug, Clone)]
enum Mbc {
    /// 32 KiB ROM, optional 8 KiB RAM, no banking.
    None,
    Mbc1 {
        ram_enabled: bool,
        /// 5-bit register at $2000-$3FFF; 0 behaves as 1.
        rom_bank_low: u8,
        /// 2-bit register at $4000-$5FFF: upper ROM bank bits or RAM bank.
        bank2: u8,
        /// Banking mode at $6000-$7FFF.
        mode: u8,
    },
    Mbc3 {
        /// RAM and the clock are enabled together, by writing $0A to $0000-$1FFF.
        ram_enabled: bool,
        /// 7-bit register at $2000-$3FFF; 0 behaves as 1.
        rom_bank: u8,
        /// $4000-$5FFF: $00-$03 picks a RAM bank, $08-$0C an RTC register.
        ram_select: u8,
        /// Last value written to $6000-$7FFF; $00 then $01 latches the clock.
        latch_prev: u8,
        /// Only cartridge types $0F and $10 have the clock.
        rtc: Option<Rtc>,
    },
    // TODO(milestone 5): MBC5.
}

/// MBC3's real-time clock registers, in the order $08-$0C selects them.
/// https://gbdev.io/pandocs/MBC3.html
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RtcRegs {
    /// 0-59, but 6 bits wide, so 60-63 can be written.
    seconds: u8,
    /// 0-59, 6 bits wide.
    minutes: u8,
    /// 0-23, 5 bits wide.
    hours: u8,
    /// Low 8 bits of the 9-bit day counter.
    days_low: u8,
    /// Bit 0: day counter bit 8. Bit 6: halt (clock stopped). Bit 7: day
    /// counter overflowed past 511 (sticky until written to 0).
    control: u8,
}

const RTC_HALT: u8 = 0x40;
const RTC_CARRY: u8 = 0x80;

impl RtcRegs {
    fn get(&self, reg: u8) -> u8 {
        match reg {
            0x08 => self.seconds,
            0x09 => self.minutes,
            0x0A => self.hours,
            0x0B => self.days_low,
            _ => self.control,
        }
    }

    /// Writes keep only the bits each register has.
    fn set(&mut self, reg: u8, val: u8) {
        match reg {
            0x08 => self.seconds = val & 0x3F,
            0x09 => self.minutes = val & 0x3F,
            0x0A => self.hours = val & 0x1F,
            0x0B => self.days_low = val,
            _ => self.control = val & 0xC1,
        }
    }

    /// One second passes. A field at its normal maximum (59, 59, 23) wraps
    /// to 0 and carries into the next; one that runs off the top of its bits
    /// (63, 63, 31) wraps to 0 without carrying, as on hardware (this follows
    /// rtc3test's "invalid rollovers"; Pan Docs doesn't cover it).
    fn tick_second(&mut self) {
        let carry = |field: &mut u8, max: u8, mask: u8| match *field {
            f if f == max => {
                *field = 0;
                true
            }
            f => {
                *field = (f + 1) & mask;
                false
            }
        };
        if !carry(&mut self.seconds, 59, 0x3F)
            || !carry(&mut self.minutes, 59, 0x3F)
            || !carry(&mut self.hours, 23, 0x1F)
        {
            return;
        }
        let days = ((u16::from(self.control & 1) << 8) | u16::from(self.days_low)) + 1;
        if days > 0x1FF {
            self.control |= RTC_CARRY;
        }
        self.days_low = days as u8;
        self.control = (self.control & !1) | ((days >> 8) & 1) as u8;
    }
}

/// MBC3's clock: live registers that tick, the latched copy games read, and
/// the time since the last tick. It runs on emulated time (one second per
/// `CPU_HZ` T-cycles), so it speeds up with fast-forward like real hardware
/// would at a faster clock.
/// TODO(milestone 5): with battery saves, let the frontend add the real time
/// that passed while the game was closed.
#[derive(Debug, Clone, Default)]
struct Rtc {
    live: RtcRegs,
    latched: RtcRegs,
    /// T-cycles since the last tick.
    subsecond: u32,
}

impl Rtc {
    fn tick(&mut self, cycles: u32) {
        if self.live.control & RTC_HALT != 0 {
            return; // halted: time, including the sub-second count, stands still
        }
        self.subsecond += cycles;
        while self.subsecond >= CPU_HZ {
            self.subsecond -= CPU_HZ;
            self.live.tick_second();
        }
    }

    /// Games write the live registers. The latched copy is updated too, so
    /// what was written reads back without latching again.
    /// Writing the seconds register restarts the current second (rtc3test's
    /// "sub-second writes"; writes to the other registers don't).
    fn write(&mut self, reg: u8, val: u8) {
        self.live.set(reg, val);
        self.latched.set(reg, val);
        if reg == 0x08 {
            self.subsecond = 0;
        }
    }
}

pub struct Cartridge {
    rom: Vec<u8>,
    ram: Vec<u8>,
    mbc: Mbc,
    pub header: Header,
}

/// The checksum the boot ROM verifies over $0134-$014C.
pub fn header_checksum(rom: &[u8]) -> u8 {
    rom[0x134..=0x14C]
        .iter()
        .fold(0u8, |x, &b| x.wrapping_sub(b).wrapping_sub(1))
}

impl Cartridge {
    pub fn from_rom(rom: Vec<u8>) -> Result<Self, CartridgeError> {
        if rom.len() < 0x150 {
            return Err(CartridgeError::TooSmall(rom.len()));
        }
        let title = rom[0x134..0x144]
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '?'
                }
            })
            .collect::<String>()
            .trim_end()
            .to_string();
        let cart_type = rom[0x147];
        let ram_size = match rom[0x149] {
            0x02 => 0x2000,
            0x03 => 0x8000,
            0x04 => 0x20000,
            0x05 => 0x10000,
            _ => 0,
        };
        let mbc = match cart_type {
            0x00 | 0x08 | 0x09 => Mbc::None,
            0x01..=0x03 => Mbc::Mbc1 {
                ram_enabled: false,
                rom_bank_low: 1,
                bank2: 0,
                mode: 0,
            },
            // $0F MBC3+TIMER+BATTERY, $10 +RAM, $11 plain, $12 +RAM, $13 +RAM+BATTERY
            0x0F..=0x13 => Mbc::Mbc3 {
                ram_enabled: false,
                rom_bank: 1,
                ram_select: 0,
                latch_prev: 0xFF,
                rtc: matches!(cart_type, 0x0F | 0x10).then(Rtc::default),
            },
            t => return Err(CartridgeError::UnsupportedType(t)),
        };
        let header = Header {
            title,
            cart_type,
            checksum_ok: header_checksum(&rom) == rom[0x14D],
        };
        Ok(Self {
            rom,
            ram: vec![0; ram_size],
            mbc,
            header,
        })
    }

    fn rom_banks(&self) -> usize {
        (self.rom.len() / ROM_BANK).max(1)
    }

    pub fn read_rom(&self, addr: u16) -> u8 {
        let offset = match self.mbc {
            Mbc::None => addr as usize,
            Mbc::Mbc1 {
                rom_bank_low,
                bank2,
                mode,
                ..
            } => {
                let bank = if addr < 0x4000 {
                    if mode == 1 {
                        (bank2 as usize) << 5
                    } else {
                        0
                    }
                } else {
                    ((bank2 as usize) << 5) | rom_bank_low as usize
                };
                (bank % self.rom_banks()) * ROM_BANK + (addr as usize & 0x3FFF)
            }
            // $0000-$3FFF is always bank 0; $4000-$7FFF any bank, $20/$40/$60 too.
            Mbc::Mbc3 { rom_bank, .. } => {
                let bank = if addr < 0x4000 { 0 } else { rom_bank as usize };
                (bank % self.rom_banks()) * ROM_BANK + (addr as usize & 0x3FFF)
            }
        };
        self.rom.get(offset).copied().unwrap_or(0xFF)
    }

    /// Writes to ROM space never change ROM; they set MBC registers.
    pub fn write_rom(&mut self, addr: u16, val: u8) {
        match &mut self.mbc {
            Mbc::None => {}
            Mbc::Mbc1 {
                ram_enabled,
                rom_bank_low,
                bank2,
                mode,
            } => match addr {
                0x0000..=0x1FFF => *ram_enabled = val & 0x0F == 0x0A,
                0x2000..=0x3FFF => *rom_bank_low = (val & 0x1F).max(1),
                0x4000..=0x5FFF => *bank2 = val & 0x03,
                _ => *mode = val & 0x01,
            },
            Mbc::Mbc3 {
                ram_enabled,
                rom_bank,
                ram_select,
                latch_prev,
                rtc,
            } => match addr {
                0x0000..=0x1FFF => *ram_enabled = val & 0x0F == 0x0A,
                0x2000..=0x3FFF => *rom_bank = (val & 0x7F).max(1),
                0x4000..=0x5FFF => *ram_select = val,
                _ => {
                    // $00 then $01 copies the running clock into the
                    // registers games read.
                    if *latch_prev == 0x00 && val == 0x01 {
                        if let Some(rtc) = rtc {
                            rtc.latched = rtc.live;
                        }
                    }
                    *latch_prev = val;
                }
            },
        }
    }

    /// What $A000-$BFFF currently shows.
    fn ram_target(&self, addr: u16) -> RamTarget {
        let bank = match &self.mbc {
            Mbc::None => 0,
            Mbc::Mbc1 {
                ram_enabled: false, ..
            }
            | Mbc::Mbc3 {
                ram_enabled: false, ..
            } => return RamTarget::None,
            Mbc::Mbc1 { bank2, mode, .. } => {
                if *mode == 1 {
                    *bank2 as usize
                } else {
                    0
                }
            }
            Mbc::Mbc3 {
                ram_select, rtc, ..
            } => match ram_select {
                0x00..=0x03 => *ram_select as usize,
                0x08..=0x0C if rtc.is_some() => return RamTarget::Rtc(*ram_select),
                _ => return RamTarget::None,
            },
        };
        if self.ram.is_empty() {
            return RamTarget::None;
        }
        RamTarget::Ram((bank * RAM_BANK + (addr as usize - 0xA000)) % self.ram.len())
    }

    pub fn read_ram(&self, addr: u16) -> u8 {
        match self.ram_target(addr) {
            RamTarget::Ram(i) => self.ram[i],
            RamTarget::Rtc(reg) => match &self.mbc {
                Mbc::Mbc3 { rtc: Some(rtc), .. } => rtc.latched.get(reg),
                _ => 0xFF,
            },
            RamTarget::None => 0xFF,
        }
    }

    pub fn write_ram(&mut self, addr: u16, val: u8) {
        match self.ram_target(addr) {
            RamTarget::Ram(i) => self.ram[i] = val,
            RamTarget::Rtc(reg) => {
                if let Mbc::Mbc3 { rtc: Some(rtc), .. } = &mut self.mbc {
                    rtc.write(reg, val);
                }
            }
            RamTarget::None => {}
        }
    }

    /// Advances anything on the cartridge that runs on its own: MBC3's clock.
    pub fn tick(&mut self, cycles: u32) {
        if let Mbc::Mbc3 { rtc: Some(rtc), .. } = &mut self.mbc {
            rtc.tick(cycles);
        }
    }
}

/// Where an access to $A000-$BFFF goes.
enum RamTarget {
    /// Offset into cartridge RAM.
    Ram(usize),
    /// An MBC3 clock register, $08-$0C.
    Rtc(u8),
    /// Nothing: disabled, absent, or unmapped. Reads $FF, writes are dropped.
    None,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a ROM with a valid header. Each bank's first byte (except bank 0)
    /// holds its bank number so tests can see which bank is mapped.
    pub(crate) fn make_rom(cart_type: u8, banks: usize, ram_code: u8) -> Vec<u8> {
        let mut rom = vec![0u8; banks * ROM_BANK];
        for b in 1..banks {
            rom[b * ROM_BANK] = b as u8;
        }
        rom[0x134..0x134 + 4].copy_from_slice(b"TEST");
        rom[0x147] = cart_type;
        rom[0x148] = (banks / 2).trailing_zeros() as u8;
        rom[0x149] = ram_code;
        rom[0x14D] = header_checksum(&rom);
        rom
    }

    /// A 32 KiB ROM-only cartridge with `program` placed at the $0100 entry point.
    pub(crate) fn rom_with_program(program: &[u8]) -> Vec<u8> {
        let mut rom = make_rom(0x00, 2, 0);
        // Programs longer than 4 bytes would overwrite the header, so they go
        // at $0150 behind a JP.
        if program.len() <= 4 {
            rom[0x100..0x100 + program.len()].copy_from_slice(program);
        } else {
            rom[0x100..0x103].copy_from_slice(&[0xC3, 0x50, 0x01]); // JP $0150
            rom[0x150..0x150 + program.len()].copy_from_slice(program);
        }
        rom[0x14D] = header_checksum(&rom);
        rom
    }

    #[test]
    fn parses_header() {
        let cart = Cartridge::from_rom(make_rom(0x00, 2, 0)).unwrap();
        assert_eq!(cart.header.title, "TEST");
        assert!(cart.header.checksum_ok);
    }

    #[test]
    fn rejects_tiny_rom() {
        assert_eq!(
            Cartridge::from_rom(vec![0; 16]).err(),
            Some(CartridgeError::TooSmall(16))
        );
    }

    #[test]
    fn rejects_unsupported_type() {
        assert_eq!(
            Cartridge::from_rom(make_rom(0x05, 2, 0)).err(),
            Some(CartridgeError::UnsupportedType(0x05))
        );
    }

    #[test]
    fn mbc1_switches_rom_banks() {
        let mut cart = Cartridge::from_rom(make_rom(0x01, 8, 0)).unwrap();
        assert_eq!(cart.read_rom(0x4000), 1, "bank 1 mapped at power-on");
        cart.write_rom(0x2000, 5);
        assert_eq!(cart.read_rom(0x4000), 5);
        cart.write_rom(0x2000, 0);
        assert_eq!(cart.read_rom(0x4000), 1, "bank 0 request maps bank 1");
    }

    #[test]
    fn mbc1_ram_needs_enabling() {
        let mut cart = Cartridge::from_rom(make_rom(0x03, 4, 0x03)).unwrap();
        cart.write_ram(0xA000, 0x42);
        assert_eq!(cart.read_ram(0xA000), 0xFF, "disabled RAM reads open bus");
        cart.write_rom(0x0000, 0x0A);
        cart.write_ram(0xA000, 0x42);
        assert_eq!(cart.read_ram(0xA000), 0x42);
    }

    #[test]
    fn mbc3_rom_bank_is_7_bits_and_every_bank_works() {
        let mut cart = Cartridge::from_rom(make_rom(0x11, 128, 0)).unwrap();
        assert_eq!(cart.read_rom(0x4000), 1, "bank 1 mapped at power-on");
        cart.write_rom(0x2000, 0x20);
        assert_eq!(cart.read_rom(0x4000), 0x20, "MBC1 would map $21 here");
        cart.write_rom(0x2000, 0x7F);
        assert_eq!(cart.read_rom(0x4000), 0x7F);
        cart.write_rom(0x2000, 0x85);
        assert_eq!(cart.read_rom(0x4000), 0x05, "only 7 bits");
        cart.write_rom(0x2000, 0);
        assert_eq!(cart.read_rom(0x4000), 1, "bank 0 request maps bank 1");
        assert_eq!(cart.read_rom(0x0000), 0, "$0000-$3FFF is always bank 0");
    }

    #[test]
    fn mbc3_ram_banks_need_enabling() {
        let mut cart = Cartridge::from_rom(make_rom(0x13, 4, 0x03)).unwrap();
        cart.write_ram(0xA000, 0x42);
        assert_eq!(cart.read_ram(0xA000), 0xFF, "disabled");
        cart.write_rom(0x0000, 0x0A);
        for bank in 0..4 {
            cart.write_rom(0x4000, bank);
            cart.write_ram(0xA123, 0x10 + bank);
        }
        for bank in 0..4 {
            cart.write_rom(0x4000, bank);
            assert_eq!(cart.read_ram(0xA123), 0x10 + bank, "bank {bank}");
        }
        cart.write_rom(0x0000, 0x00);
        assert_eq!(cart.read_ram(0xA123), 0xFF, "disabled again");
    }

    #[test]
    fn mbc3_without_a_clock_has_no_rtc_registers() {
        let mut cart = Cartridge::from_rom(make_rom(0x13, 4, 0x03)).unwrap();
        cart.write_rom(0x0000, 0x0A);
        cart.write_rom(0x4000, 0x08);
        cart.write_ram(0xA000, 5);
        assert_eq!(cart.read_ram(0xA000), 0xFF);
    }

    /// MBC3+TIMER+RAM+BATTERY with RAM and the clock enabled.
    fn rtc_cart() -> Cartridge {
        let mut cart = Cartridge::from_rom(make_rom(0x10, 4, 0x03)).unwrap();
        cart.write_rom(0x0000, 0x0A);
        cart
    }

    fn latch(cart: &mut Cartridge) {
        cart.write_rom(0x6000, 0x00);
        cart.write_rom(0x6000, 0x01);
    }

    fn read_rtc(cart: &mut Cartridge, reg: u8) -> u8 {
        cart.write_rom(0x4000, reg);
        cart.read_ram(0xA000)
    }

    fn write_rtc(cart: &mut Cartridge, reg: u8, val: u8) {
        cart.write_rom(0x4000, reg);
        cart.write_ram(0xA000, val);
    }

    /// Sets the time (days must fit in 9 bits) and the control flags.
    fn set_time(cart: &mut Cartridge, days: u16, h: u8, m: u8, s: u8, flags: u8) {
        write_rtc(cart, 0x08, s);
        write_rtc(cart, 0x09, m);
        write_rtc(cart, 0x0A, h);
        write_rtc(cart, 0x0B, days as u8);
        write_rtc(cart, 0x0C, flags | (days >> 8) as u8);
    }

    /// Latches, then reads (days, hours, minutes, seconds, control).
    fn time(cart: &mut Cartridge) -> (u16, u8, u8, u8, u8) {
        latch(cart);
        let control = read_rtc(cart, 0x0C);
        let days = (u16::from(control & 1) << 8) | u16::from(read_rtc(cart, 0x0B));
        let (h, m, s) = (
            read_rtc(cart, 0x0A),
            read_rtc(cart, 0x09),
            read_rtc(cart, 0x08),
        );
        (days, h, m, s, control & 0xC0)
    }

    fn seconds(cart: &mut Cartridge, n: u32) {
        for _ in 0..n {
            cart.tick(CPU_HZ);
        }
    }

    #[test]
    fn rtc_reads_the_latched_time_until_latched_again() {
        let mut cart = rtc_cart();
        seconds(&mut cart, 3);
        assert_eq!(read_rtc(&mut cart, 0x08), 0, "nothing latched yet");
        latch(&mut cart);
        assert_eq!(read_rtc(&mut cart, 0x08), 3);
        seconds(&mut cart, 2);
        assert_eq!(read_rtc(&mut cart, 0x08), 3, "still the latched value");
        cart.write_rom(0x6000, 0x01); // $01 without $00 first: no latch
        assert_eq!(read_rtc(&mut cart, 0x08), 3);
        latch(&mut cart);
        assert_eq!(read_rtc(&mut cart, 0x08), 5);
    }

    #[test]
    fn rtc_counts_one_second_per_cpu_hz_cycles() {
        let mut cart = rtc_cart();
        cart.tick(CPU_HZ - 1);
        assert_eq!(time(&mut cart).3, 0);
        cart.tick(1);
        assert_eq!(time(&mut cart).3, 1);
    }

    #[test]
    fn rtc_rolls_seconds_into_minutes_hours_and_days() {
        let mut cart = rtc_cart();
        set_time(&mut cart, 255, 23, 59, 59, 0);
        seconds(&mut cart, 1);
        assert_eq!(time(&mut cart), (256, 0, 0, 0, 0));
        set_time(&mut cart, 3, 10, 59, 59, 0);
        seconds(&mut cart, 1);
        assert_eq!(time(&mut cart), (3, 11, 0, 0, 0));
    }

    #[test]
    fn rtc_day_overflow_sets_a_sticky_carry() {
        let mut cart = rtc_cart();
        set_time(&mut cart, 511, 23, 59, 59, 0);
        seconds(&mut cart, 1);
        assert_eq!(time(&mut cart), (0, 0, 0, 0, RTC_CARRY));
        set_time(&mut cart, 511, 23, 59, 59, RTC_CARRY);
        seconds(&mut cart, 1);
        assert_eq!(time(&mut cart).4, RTC_CARRY, "stays set");
        write_rtc(&mut cart, 0x0C, 0);
        assert_eq!(time(&mut cart).4, 0, "until the game clears it");
    }

    #[test]
    fn rtc_halt_stops_time_and_keeps_the_part_second() {
        let mut cart = rtc_cart();
        let before = CPU_HZ / 10 * 4; // 0.4 s into the second
        cart.tick(before);
        write_rtc(&mut cart, 0x0C, RTC_HALT);
        seconds(&mut cart, 10);
        assert_eq!(time(&mut cart).3, 0, "halted");
        write_rtc(&mut cart, 0x0C, 0);
        cart.tick(CPU_HZ - before - 1);
        assert_eq!(time(&mut cart).3, 0, "one cycle short");
        cart.tick(1);
        assert_eq!(time(&mut cart).3, 1, "the 0.4 s from before counts");
    }

    #[test]
    fn rtc_registers_keep_only_their_bits() {
        let mut cart = rtc_cart();
        write_rtc(&mut cart, 0x0C, RTC_HALT); // don't tick while checking
        for (reg, want) in [(0x08, 0x3F), (0x09, 0x3F), (0x0A, 0x1F), (0x0B, 0xFF)] {
            write_rtc(&mut cart, reg, 0xFF);
            assert_eq!(read_rtc(&mut cart, reg), want, "reg {reg:02X}");
        }
        write_rtc(&mut cart, 0x0C, 0xFF);
        assert_eq!(read_rtc(&mut cart, 0x0C), 0xC1);
    }

    #[test]
    fn rtc_out_of_range_values_count_on_and_wrap_without_carrying() {
        let mut cart = rtc_cart();
        set_time(&mut cart, 7, 28, 63, 60, 0); // 28:63:60, an "invalid" time
        seconds(&mut cart, 1);
        assert_eq!(time(&mut cart), (7, 28, 63, 61, 0));

        set_time(&mut cart, 7, 5, 10, 63, 0);
        seconds(&mut cart, 1);
        assert_eq!(time(&mut cart), (7, 5, 10, 0, 0), "63 s wraps, no minute");

        set_time(&mut cart, 7, 5, 63, 59, 0);
        seconds(&mut cart, 1);
        assert_eq!(time(&mut cart), (7, 5, 0, 0, 0), "63 min wraps, no hour");

        set_time(&mut cart, 7, 31, 59, 59, 0);
        seconds(&mut cart, 1);
        assert_eq!(time(&mut cart), (7, 0, 0, 0, 0), "31 h wraps, no day");

        set_time(&mut cart, 7, 5, 61, 59, 0);
        seconds(&mut cart, 1);
        assert_eq!(
            time(&mut cart),
            (7, 5, 62, 0, 0),
            "minute 61 still counts up"
        );
    }

    #[test]
    fn writing_seconds_restarts_the_second_but_other_writes_dont() {
        let mut cart = rtc_cart();
        cart.tick(CPU_HZ / 10 * 9); // 0.9 s in
        write_rtc(&mut cart, 0x08, 10);
        cart.tick(CPU_HZ / 2);
        assert_eq!(time(&mut cart).3, 10, "a full second from the write");
        cart.tick(CPU_HZ / 2);
        assert_eq!(time(&mut cart).3, 11);

        let mut cart = rtc_cart();
        let before = CPU_HZ / 10 * 9;
        cart.tick(before);
        write_rtc(&mut cart, 0x09, 5);
        cart.tick(CPU_HZ - before);
        assert_eq!(
            time(&mut cart),
            (0, 0, 5, 1, 0),
            "the rest of the second later"
        );
    }
}
