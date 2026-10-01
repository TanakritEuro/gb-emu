//! Cartridge header parsing and memory bank controllers (MBCs).
//!
//! Reference: https://gbdev.io/pandocs/The_Cartridge_Header.html
//!            https://gbdev.io/pandocs/MBC1.html

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
    // TODO(milestone 5): MBC3 (+ real-time clock), MBC5.
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
        };
        self.rom.get(offset).copied().unwrap_or(0xFF)
    }

    /// Writes to ROM space never change ROM; they set MBC registers.
    pub fn write_rom(&mut self, addr: u16, val: u8) {
        if let Mbc::Mbc1 {
            ram_enabled,
            rom_bank_low,
            bank2,
            mode,
        } = &mut self.mbc
        {
            match addr {
                0x0000..=0x1FFF => *ram_enabled = val & 0x0F == 0x0A,
                0x2000..=0x3FFF => *rom_bank_low = (val & 0x1F).max(1),
                0x4000..=0x5FFF => *bank2 = val & 0x03,
                _ => *mode = val & 0x01,
            }
        }
    }

    fn ram_offset(&self, addr: u16) -> Option<usize> {
        if self.ram.is_empty() {
            return None;
        }
        let bank = match self.mbc {
            Mbc::None => 0,
            Mbc::Mbc1 {
                ram_enabled: false, ..
            } => return None,
            Mbc::Mbc1 { bank2, mode, .. } => {
                if mode == 1 {
                    bank2 as usize
                } else {
                    0
                }
            }
        };
        Some((bank * RAM_BANK + (addr as usize - 0xA000)) % self.ram.len())
    }

    pub fn read_ram(&self, addr: u16) -> u8 {
        self.ram_offset(addr).map_or(0xFF, |i| self.ram[i])
    }

    pub fn write_ram(&mut self, addr: u16, val: u8) {
        if let Some(i) = self.ram_offset(addr) {
            self.ram[i] = val;
        }
    }
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
            Cartridge::from_rom(make_rom(0x13, 2, 0)).err(),
            Some(CartridgeError::UnsupportedType(0x13))
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
}
