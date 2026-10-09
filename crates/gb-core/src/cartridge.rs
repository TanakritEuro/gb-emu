//! Cartridge header parsing and memory bank controllers (MBCs).
//!
//! Reference: https://gbdev.io/pandocs/The_Cartridge_Header.html
//!            https://gbdev.io/pandocs/MBC1.html
//!            https://gbdev.io/pandocs/MBC3.html

use crate::state::{fnv1a64, StateError, StateReader, StateWriter};
use crate::CPU_HZ;
use std::fmt;
use std::sync::Arc;

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

/// Why save data couldn't be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// The cartridge has no battery, so nothing on it survives power-off.
    NoBattery,
    /// The data doesn't fit this cartridge: `expected` lists the sizes it takes.
    WrongSize { got: usize, expected: Vec<usize> },
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::NoBattery => write!(f, "this cartridge has no battery, so it has no save"),
            SaveError::WrongSize { got, expected } => {
                let sizes: Vec<String> = expected.iter().map(|n| format!("{n}")).collect();
                write!(
                    f,
                    "save is {got} bytes; this cartridge's is {} bytes",
                    sizes.join(" or ")
                )
            }
        }
    }
}

impl std::error::Error for SaveError {}

/// Bytes of clock state that `.sav` files append after the RAM for MBC3
/// cartridges with a clock, in the format BGB and VBA-M use: the five live
/// registers and the five latched ones as little-endian u32s, then the Unix
/// time of the save as a u64 (44 bytes in the old VBA variant, with a u32).
/// https://bgb.bircd.org/rtcsave.html
const RTC_SAVE_LEN: usize = 48;
const RTC_SAVE_LEN_OLD: usize = 44;
/// The timestamp BGB writes when it doesn't know the time: don't catch up.
const RTC_NO_TIMESTAMP: u64 = 0x7FFF_FFFF_7FFF_FFFF;

#[derive(Debug, Clone)]
pub struct Header {
    pub title: String,
    pub cart_type: u8,
    pub checksum_ok: bool,
    /// $0143 bit 7: the game knows about the Game Boy Color ($80 runs on
    /// both, $C0 on the Color only), so a Color starts it in Color mode.
    /// https://gbdev.io/pandocs/The_Cartridge_Header.html#0143--cgb-flag
    pub cgb: bool,
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
        /// 7-bit register at $2000-$3FFF (8 bits on the MBC30); 0 behaves
        /// as 1.
        rom_bank: u8,
        /// $4000-$5FFF: $00-$03 picks a RAM bank ($00-$07 on the MBC30),
        /// $08-$0C an RTC register.
        ram_select: u8,
        /// Last value written to $6000-$7FFF; $00 then $01 latches the clock.
        latch_prev: u8,
        /// Only cartridge types $0F and $10 have the clock.
        rtc: Option<Rtc>,
        /// The MBC30 (the Japanese Pokémon Crystal's): 256 ROM banks (4 MB)
        /// and 8 RAM banks. The header calls it an MBC3; only its ROM or RAM
        /// size gives it away. https://gbdev.io/pandocs/MBC3.html
        mbc30: bool,
    },
    /// 16 ROM banks and 512 four-bit RAM cells built into the chip.
    /// https://gbdev.io/pandocs/MBC2.html
    Mbc2 {
        ram_enabled: bool,
        /// 4 bits; 0 behaves as 1.
        rom_bank: u8,
    },
    /// https://gbdev.io/pandocs/MBC5.html
    Mbc5 {
        ram_enabled: bool,
        /// 9 bits: low 8 from $2000-$2FFF, bit 8 from $3000-$3FFF. Unlike
        /// MBC1/MBC3, 0 really selects bank 0.
        rom_bank: u16,
        /// $4000-$5FFF, up to 16 banks of 8 KiB.
        ram_bank: u8,
        /// Rumble cartridges wire bit 3 of the RAM bank register to the motor,
        /// so only bits 0-2 pick a bank.
        rumble: bool,
    },
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

    /// `secs` seconds pass, as if `tick_second` ran that many times, but
    /// without looping over a long absence (a year is 31 million seconds).
    /// Does nothing while halted.
    fn advance(&mut self, mut secs: u64) {
        if self.control & RTC_HALT != 0 {
            return;
        }
        // Fields outside their usual range count on oddly (see tick_second);
        // step through that one second at a time until it's back in range.
        while secs > 0 && (self.seconds > 59 || self.minutes > 59 || self.hours > 23) {
            self.tick_second();
            secs -= 1;
        }
        if secs == 0 {
            return; // the arithmetic below would also "fix" an out-of-range time
        }
        let time_of_day =
            u64::from(self.seconds) + 60 * u64::from(self.minutes) + 3600 * u64::from(self.hours);
        let total = time_of_day + secs;
        let rem = total % 86_400;
        self.seconds = (rem % 60) as u8;
        self.minutes = (rem / 60 % 60) as u8;
        self.hours = (rem / 3600) as u8;
        let days = ((u64::from(self.control & 1) << 8) | u64::from(self.days_low)) + total / 86_400;
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
/// would at a faster clock. Real time that passed while the game was closed
/// is added when a save is loaded (`Cartridge::load_save`).
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

#[derive(Clone)]
pub struct Cartridge {
    /// Shared, not copied, when the cartridge is cloned (save states do).
    rom: Arc<[u8]>,
    /// [`state::fnv1a64`](crate::state::fnv1a64) of the ROM.
    rom_hash: u64,
    ram: Vec<u8>,
    mbc: Mbc,
    pub header: Header,
    /// Set when the game writes RAM or the clock; see `take_save_dirty`.
    save_dirty: bool,
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
        let cgb = rom[0x143] & 0x80 != 0;
        // On Color games the title is 15 bytes: $0143 is the flag.
        let title_end = if cgb { 0x143 } else { 0x144 };
        let title = rom[0x134..title_end]
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
        let ram_size = match (cart_type, rom[0x149]) {
            // MBC2's RAM is inside the chip; the header says 0.
            (0x05 | 0x06, _) => 512,
            (_, 0x02) => 0x2000,
            (_, 0x03) => 0x8000,
            (_, 0x04) => 0x20000,
            (_, 0x05) => 0x10000,
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
                mbc30: rom.len() > 0x20_0000 || ram_size > 0x8000,
            },
            // $05 MBC2, $06 MBC2+BATTERY
            0x05 | 0x06 => Mbc::Mbc2 {
                ram_enabled: false,
                rom_bank: 1,
            },
            // $19 MBC5, $1A +RAM, $1B +RAM+BATTERY, $1C-$1E the same with rumble
            0x19..=0x1E => Mbc::Mbc5 {
                ram_enabled: false,
                rom_bank: 1,
                ram_bank: 0,
                rumble: cart_type >= 0x1C,
            },
            t => return Err(CartridgeError::UnsupportedType(t)),
        };
        let header = Header {
            title,
            cart_type,
            checksum_ok: header_checksum(&rom) == rom[0x14D],
            cgb,
        };
        Ok(Self {
            rom_hash: fnv1a64(&rom),
            rom: rom.into(),
            ram: vec![0; ram_size],
            mbc,
            header,
            save_dirty: false,
        })
    }

    /// A fingerprint of the ROM, so save states can tell games apart.
    pub fn rom_hash(&self) -> u64 {
        self.rom_hash
    }

    /// The MBC's registers, the RAM and the clock; not the ROM.
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.tag(b"CART");
        match &self.mbc {
            Mbc::None => w.u8(0),
            Mbc::Mbc1 {
                ram_enabled,
                rom_bank_low,
                bank2,
                mode,
            } => {
                w.u8(1);
                w.bool(*ram_enabled);
                w.bytes(&[*rom_bank_low, *bank2, *mode]);
            }
            Mbc::Mbc2 {
                ram_enabled,
                rom_bank,
            } => {
                w.u8(2);
                w.bool(*ram_enabled);
                w.u8(*rom_bank);
            }
            Mbc::Mbc3 {
                ram_enabled,
                rom_bank,
                ram_select,
                latch_prev,
                rtc,
                ..
            } => {
                w.u8(3);
                w.bool(*ram_enabled);
                w.bytes(&[*rom_bank, *ram_select, *latch_prev]);
                w.bool(rtc.is_some());
                if let Some(rtc) = rtc {
                    for regs in [&rtc.live, &rtc.latched] {
                        w.bytes(&[
                            regs.seconds,
                            regs.minutes,
                            regs.hours,
                            regs.days_low,
                            regs.control,
                        ]);
                    }
                    w.u32(rtc.subsecond);
                }
            }
            Mbc::Mbc5 {
                ram_enabled,
                rom_bank,
                ram_bank,
                rumble,
            } => {
                w.u8(5);
                w.bool(*ram_enabled);
                w.u16(*rom_bank);
                w.u8(*ram_bank);
                w.bool(*rumble);
            }
        }
        w.sized_bytes(&self.ram);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"CART")?;
        let kind = r.u8()?;
        let wrong_mbc = Err(StateError::Corrupt("a different kind of cartridge"));
        match &mut self.mbc {
            Mbc::None if kind == 0 => {}
            Mbc::Mbc1 {
                ram_enabled,
                rom_bank_low,
                bank2,
                mode,
            } if kind == 1 => {
                *ram_enabled = r.bool()?;
                [*rom_bank_low, *bank2, *mode] = [r.u8()?, r.u8()?, r.u8()?];
            }
            Mbc::Mbc2 {
                ram_enabled,
                rom_bank,
            } if kind == 2 => {
                *ram_enabled = r.bool()?;
                *rom_bank = r.u8()?;
            }
            Mbc::Mbc3 {
                ram_enabled,
                rom_bank,
                ram_select,
                latch_prev,
                rtc,
                ..
            } if kind == 3 => {
                *ram_enabled = r.bool()?;
                [*rom_bank, *ram_select, *latch_prev] = [r.u8()?, r.u8()?, r.u8()?];
                if r.bool()? != rtc.is_some() {
                    return wrong_mbc;
                }
                if let Some(rtc) = rtc {
                    for regs in [&mut rtc.live, &mut rtc.latched] {
                        let mut b = [0; 5];
                        r.bytes(&mut b)?;
                        let [seconds, minutes, hours, days_low, control] = b;
                        *regs = RtcRegs {
                            seconds,
                            minutes,
                            hours,
                            days_low,
                            control,
                        };
                    }
                    rtc.subsecond = r.u32()?;
                }
            }
            Mbc::Mbc5 {
                ram_enabled,
                rom_bank,
                ram_bank,
                rumble,
            } if kind == 5 => {
                *ram_enabled = r.bool()?;
                *rom_bank = r.u16()?;
                *ram_bank = r.u8()?;
                *rumble = r.bool()?;
            }
            _ => return wrong_mbc,
        }
        r.sized_bytes(&mut self.ram)?;
        // Going back to a state changes the battery save too: store it.
        self.save_dirty = self.has_battery();
        Ok(())
    }

    /// Whether the cartridge has a battery keeping its RAM (and MBC3's
    /// clock) alive with the power off, i.e. whether it has a save.
    pub fn has_battery(&self) -> bool {
        matches!(
            self.header.cart_type,
            0x03 | 0x06 | 0x09 | 0x0D | 0x0F | 0x10 | 0x13 | 0x1B | 0x1E
        )
    }

    fn rtc(&self) -> Option<&Rtc> {
        match &self.mbc {
            Mbc::Mbc3 { rtc, .. } => rtc.as_ref(),
            _ => None,
        }
    }

    /// The save, as a `.sav` file: cartridge RAM, then for MBC3 with a clock
    /// the 48-byte clock block stamped with `now` (Unix seconds, from the
    /// frontend: the core never reads the system clock). None without a
    /// battery.
    pub fn save_data(&self, now: u64) -> Option<Vec<u8>> {
        if !self.has_battery() {
            return None;
        }
        let mut data = self.ram.clone();
        if let Some(rtc) = self.rtc() {
            for regs in [rtc.live, rtc.latched] {
                for reg in 0x08..=0x0C {
                    data.extend_from_slice(&u32::from(regs.get(reg)).to_le_bytes());
                }
            }
            data.extend_from_slice(&now.to_le_bytes());
        }
        Some(data)
    }

    /// Restores a save made by `save_data` or another emulator. The clock is
    /// moved on by the time between the save's timestamp and `now`, as if the
    /// cartridge's battery had kept it running. A save without the clock
    /// block (just RAM) is accepted too; the clock then starts from zero.
    pub fn load_save(&mut self, data: &[u8], now: u64) -> Result<(), SaveError> {
        if !self.has_battery() {
            return Err(SaveError::NoBattery);
        }
        let ram_len = self.ram.len();
        let has_rtc = self.rtc().is_some();
        let wrong_size = || SaveError::WrongSize {
            got: data.len(),
            expected: if has_rtc {
                vec![ram_len + RTC_SAVE_LEN, ram_len]
            } else {
                vec![ram_len]
            },
        };
        if data.len() < ram_len {
            return Err(wrong_size());
        }
        let (ram, clock) = data.split_at(ram_len);
        if !(clock.is_empty() || has_rtc && matches!(clock.len(), RTC_SAVE_LEN | RTC_SAVE_LEN_OLD))
        {
            return Err(wrong_size());
        }

        let mbc2 = matches!(self.mbc, Mbc::Mbc2 { .. });
        for (dst, &src) in self.ram.iter_mut().zip(ram) {
            *dst = if mbc2 { src & 0x0F } else { src };
        }
        if let (Mbc::Mbc3 { rtc: Some(rtc), .. }, false) = (&mut self.mbc, clock.is_empty()) {
            // Little-endian integer from `len` bytes at `i` (lengths checked above).
            let le_at = |i: usize, len: usize| {
                let mut b = [0u8; 8];
                b[..len].copy_from_slice(&clock[i..i + len]);
                u64::from_le_bytes(b)
            };
            for (n, reg) in (0x08..=0x0C).enumerate() {
                rtc.live.set(reg, le_at(n * 4, 4) as u8);
                rtc.latched.set(reg, le_at(20 + n * 4, 4) as u8);
            }
            let saved_at = le_at(40, clock.len() - 40);
            if saved_at != RTC_NO_TIMESTAMP && now > saved_at {
                rtc.live.advance(now - saved_at);
            }
        }
        self.save_dirty = false;
        Ok(())
    }

    /// True once if the game has written cartridge RAM or the clock since the
    /// last call: the frontend's cue to store the save.
    pub fn take_save_dirty(&mut self) -> bool {
        std::mem::take(&mut self.save_dirty)
    }

    fn rom_banks(&self) -> usize {
        (self.rom.len() / ROM_BANK).max(1)
    }

    /// Which 16 KiB ROM bank is mapped at `addr` ($0000-$7FFF) right now.
    pub fn rom_bank(&self, addr: u16) -> usize {
        let bank = match self.mbc {
            Mbc::None => return usize::from(addr >= 0x4000),
            Mbc::Mbc1 {
                rom_bank_low,
                bank2,
                mode,
                ..
            } => {
                if addr < 0x4000 {
                    if mode == 1 {
                        (bank2 as usize) << 5
                    } else {
                        0
                    }
                } else {
                    ((bank2 as usize) << 5) | rom_bank_low as usize
                }
            }
            // $0000-$3FFF is always bank 0; $4000-$7FFF any bank, $20/$40/$60 too.
            Mbc::Mbc2 { rom_bank, .. } | Mbc::Mbc3 { rom_bank, .. } => {
                if addr < 0x4000 {
                    0
                } else {
                    rom_bank as usize
                }
            }
            Mbc::Mbc5 { rom_bank, .. } => {
                if addr < 0x4000 {
                    0
                } else {
                    rom_bank as usize
                }
            }
        };
        bank % self.rom_banks()
    }

    pub fn read_rom(&self, addr: u16) -> u8 {
        let offset = self.rom_bank(addr) * ROM_BANK + (addr as usize & 0x3FFF);
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
                mbc30,
            } => match addr {
                0x0000..=0x1FFF => *ram_enabled = val & 0x0F == 0x0A,
                0x2000..=0x3FFF => *rom_bank = (val & if *mbc30 { 0xFF } else { 0x7F }).max(1),
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
            // One register area for both: address bit 8 clear is RAM enable,
            // set is the ROM bank. $4000-$7FFF does nothing.
            Mbc::Mbc2 {
                ram_enabled,
                rom_bank,
            } => match addr {
                0x0000..=0x3FFF if addr & 0x0100 == 0 => *ram_enabled = val & 0x0F == 0x0A,
                0x0000..=0x3FFF => *rom_bank = (val & 0x0F).max(1),
                _ => {}
            },
            Mbc::Mbc5 {
                ram_enabled,
                rom_bank,
                ram_bank,
                rumble,
            } => match addr {
                0x0000..=0x1FFF => *ram_enabled = val & 0x0F == 0x0A,
                0x2000..=0x2FFF => *rom_bank = (*rom_bank & 0x100) | u16::from(val),
                0x3000..=0x3FFF => *rom_bank = (*rom_bank & 0xFF) | (u16::from(val & 1) << 8),
                0x4000..=0x5FFF => *ram_bank = val & if *rumble { 0x07 } else { 0x0F },
                _ => {} // $6000-$7FFF does nothing on MBC5
            },
        }
    }

    /// What $A000-$BFFF currently shows.
    fn ram_target(&self, addr: u16) -> RamTarget {
        let bank = match &self.mbc {
            Mbc::None => 0,
            // 512 cells repeated across $A000-$BFFF: only address bits 0-8 count.
            Mbc::Mbc2 { ram_enabled, .. } => {
                return if *ram_enabled {
                    RamTarget::Ram(usize::from(addr & 0x01FF))
                } else {
                    RamTarget::None
                };
            }
            Mbc::Mbc1 {
                ram_enabled: false, ..
            }
            | Mbc::Mbc3 {
                ram_enabled: false, ..
            }
            | Mbc::Mbc5 {
                ram_enabled: false, ..
            } => return RamTarget::None,
            Mbc::Mbc5 { ram_bank, .. } => *ram_bank as usize,
            Mbc::Mbc1 { bank2, mode, .. } => {
                if *mode == 1 {
                    *bank2 as usize
                } else {
                    0
                }
            }
            Mbc::Mbc3 {
                ram_select,
                rtc,
                mbc30,
                ..
            } => match ram_select {
                0x00..=0x03 => *ram_select as usize,
                0x04..=0x07 if *mbc30 => *ram_select as usize,
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
            // MBC2's cells are 4 bits wide; the upper bits aren't connected and
            // read as 1s. (Pan Docs calls them undefined; Mooneye's
            // mbc2/bits_unused expects 1s.)
            RamTarget::Ram(i) if matches!(self.mbc, Mbc::Mbc2 { .. }) => 0xF0 | self.ram[i],
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
            RamTarget::Ram(i) if matches!(self.mbc, Mbc::Mbc2 { .. }) => self.ram[i] = val & 0x0F,
            RamTarget::Ram(i) => self.ram[i] = val,
            RamTarget::Rtc(reg) => {
                if let Mbc::Mbc3 { rtc: Some(rtc), .. } = &mut self.mbc {
                    rtc.write(reg, val);
                }
            }
            RamTarget::None => return,
        }
        self.save_dirty = true;
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

    /// Builds a ROM with a valid header. Each bank's first two bytes (except
    /// bank 0's) hold its bank number, low byte first, so tests can see which
    /// bank is mapped (see `bank_at`).
    pub(crate) fn make_rom(cart_type: u8, banks: usize, ram_code: u8) -> Vec<u8> {
        let mut rom = vec![0u8; banks * ROM_BANK];
        for b in 1..banks {
            rom[b * ROM_BANK..b * ROM_BANK + 2].copy_from_slice(&(b as u16).to_le_bytes());
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
    fn the_cgb_flag_marks_color_games_and_ends_their_title_early() {
        let mut rom = make_rom(0x00, 2, 0);
        rom[0x134..0x144].copy_from_slice(b"FIFTEEN LETTERSX");
        rom[0x143] = 0x80;
        let cart = Cartridge::from_rom(rom.clone()).unwrap();
        assert!(cart.header.cgb);
        assert_eq!(cart.header.title, "FIFTEEN LETTERS", "$0143 isn't title");
        rom[0x143] = 0xC0;
        assert!(
            Cartridge::from_rom(rom.clone()).unwrap().header.cgb,
            "Color only"
        );
        rom[0x143] = b'X';
        let dmg = Cartridge::from_rom(rom).unwrap();
        assert!(!dmg.header.cgb);
        assert_eq!(
            dmg.header.title, "FIFTEEN LETTERSX",
            "16 letters on the original"
        );
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
            Cartridge::from_rom(make_rom(0x22, 2, 0)).err(),
            Some(CartridgeError::UnsupportedType(0x22))
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

    /// Which ROM bank is mapped at `base` ($0000 or $4000), from make_rom's tags.
    fn bank_at(cart: &Cartridge, base: u16) -> u16 {
        u16::from_le_bytes([cart.read_rom(base), cart.read_rom(base + 1)])
    }

    #[test]
    fn rom_bank_names_the_bank_that_reads_come_from() {
        // After each MBC write, rom_bank must agree with the bank make_rom's
        // tags say is mapped.
        type Case = (u8, usize, &'static [(u16, u8)]); // type, ROM banks, writes
        let cases: [Case; 5] = [
            (0x00, 2, &[]),
            // MBC1: bank 5, then mode 1 with bank2 = 1 maps bank $20 at $0000.
            (
                0x01,
                64,
                &[(0x2000, 5), (0x4000, 1), (0x6000, 1), (0x2000, 0)],
            ),
            (0x06, 16, &[(0x2100, 0x0F)]),
            (0x11, 128, &[(0x2000, 0x7F), (0x2000, 0)]),
            (0x19, 512, &[(0x2000, 0x23), (0x3000, 1), (0x2000, 0)]),
        ];
        for (cart_type, banks, writes) in cases {
            let mut cart = Cartridge::from_rom(make_rom(cart_type, banks, 0)).unwrap();
            for &(addr, val) in [(0x0000, 0x00)].iter().chain(writes) {
                cart.write_rom(addr, val);
                for base in [0x0000, 0x4000, 0x7FFE] {
                    let tag = bank_at(&cart, base & 0x4000);
                    assert_eq!(
                        cart.rom_bank(base),
                        usize::from(tag),
                        "type {cart_type:02X} after ${addr:04X} = {val:02X}, at ${base:04X}"
                    );
                }
            }
        }
    }

    #[test]
    fn mbc2_address_bit_8_picks_ram_enable_or_rom_bank() {
        let mut cart = Cartridge::from_rom(make_rom(0x06, 16, 0)).unwrap();
        assert_eq!(bank_at(&cart, 0x4000), 1, "bank 1 mapped at power-on");
        cart.write_rom(0x2100, 0x05); // bit 8 set: ROM bank
        assert_eq!(bank_at(&cart, 0x4000), 5);
        cart.write_rom(0x0100, 0x0F); // anywhere in $0000-$3FFF with bit 8 set
        assert_eq!(bank_at(&cart, 0x4000), 15);
        cart.write_rom(0x3F00, 0x13); // only 4 bits
        assert_eq!(bank_at(&cart, 0x4000), 3);
        cart.write_rom(0x2100, 0x00);
        assert_eq!(bank_at(&cart, 0x4000), 1, "0 means 1");
        cart.write_rom(0x2000, 0x07); // bit 8 clear: that's RAM enable
        assert_eq!(bank_at(&cart, 0x4000), 1, "not a bank write");
        cart.write_rom(0x4100, 0x07); // $4000-$7FFF does nothing
        assert_eq!(bank_at(&cart, 0x4000), 1);
        assert_eq!(bank_at(&cart, 0x0000), 0, "$0000-$3FFF is always bank 0");
    }

    #[test]
    fn mbc2_ram_is_512_nibbles_repeated_across_a000_bfff() {
        // The header's RAM size says 0: MBC2's RAM is inside the chip.
        let mut cart = Cartridge::from_rom(make_rom(0x06, 4, 0)).unwrap();
        cart.write_ram(0xA000, 0x05);
        assert_eq!(cart.read_ram(0xA000), 0xFF, "disabled at power-on");
        cart.write_rom(0x0000, 0x1A); // low nibble $A, bit 8 clear: enable
        cart.write_ram(0xA000, 0x3C);
        assert_eq!(cart.read_ram(0xA000), 0xFC, "4 bits kept; the top reads 1s");
        cart.write_ram(0xA1FF, 0x07);
        assert_eq!(cart.read_ram(0xA1FF), 0xF7);
        assert_eq!(cart.read_ram(0xA200), 0xFC, "$A200 echoes $A000");
        assert_eq!(cart.read_ram(0xBFFF), 0xF7, "$BFFF echoes $A1FF");
        cart.write_ram(0xB000, 0x09);
        assert_eq!(
            cart.read_ram(0xA000),
            0xF9,
            "writes through the echo land too"
        );
        cart.write_rom(0x0000, 0x0B);
        assert_eq!(cart.read_ram(0xA000), 0xFF, "any other value disables");
    }

    #[test]
    fn mbc5_rom_bank_is_9_bits_and_0_means_0() {
        let mut cart = Cartridge::from_rom(make_rom(0x19, 512, 0)).unwrap();
        assert_eq!(bank_at(&cart, 0x4000), 1, "bank 1 mapped at power-on");
        cart.write_rom(0x2000, 0x23);
        assert_eq!(bank_at(&cart, 0x4000), 0x023);
        cart.write_rom(0x3000, 0x01);
        assert_eq!(bank_at(&cart, 0x4000), 0x123, "bit 8 from $3000");
        cart.write_rom(0x2FFF, 0xFF);
        assert_eq!(bank_at(&cart, 0x4000), 0x1FF, "low byte keeps bit 8");
        cart.write_rom(0x3000, 0xFE);
        assert_eq!(bank_at(&cart, 0x4000), 0x0FF, "only bit 0 of $3000 counts");
        cart.write_rom(0x2000, 0x00);
        assert_eq!(bank_at(&cart, 0x4000), 0, "bank 0 really is bank 0");
        assert_eq!(bank_at(&cart, 0x0000), 0, "$0000-$3FFF is always bank 0");
    }

    #[test]
    fn mbc5_ram_has_up_to_16_banks() {
        let mut cart = Cartridge::from_rom(make_rom(0x1B, 4, 0x04)).unwrap(); // 128 KiB
        cart.write_ram(0xA000, 1);
        assert_eq!(cart.read_ram(0xA000), 0xFF, "disabled at power-on");
        cart.write_rom(0x0000, 0x0A);
        for bank in 0..16 {
            cart.write_rom(0x4000, bank);
            cart.write_ram(0xBFFF, 0x80 | bank);
        }
        for bank in 0..16 {
            cart.write_rom(0x4000, bank);
            assert_eq!(cart.read_ram(0xBFFF), 0x80 | bank, "bank {bank}");
        }
        cart.write_rom(0x0000, 0x00);
        assert_eq!(cart.read_ram(0xBFFF), 0xFF, "disabled again");
    }

    #[test]
    fn mbc5_rumble_bit_is_not_a_ram_bank_bit() {
        // MBC5+RUMBLE+RAM+BATTERY with 32 KiB: bit 3 drives the motor.
        let mut cart = Cartridge::from_rom(make_rom(0x1E, 4, 0x03)).unwrap();
        cart.write_rom(0x0000, 0x0A);
        cart.write_rom(0x4000, 0x02);
        cart.write_ram(0xA000, 0x42);
        cart.write_rom(0x4000, 0x0A); // bank 2 with the motor on
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
    fn a_4mb_mbc3_is_an_mbc30_with_8_bank_bits_and_8_ram_banks() {
        let mut cart = Cartridge::from_rom(make_rom(0x11, 256, 0)).unwrap();
        cart.write_rom(0x2000, 0x85);
        assert_eq!(cart.read_rom(0x4000), 0x85, "all 8 bits");
        cart.write_rom(0x2000, 0xFF);
        assert_eq!(cart.read_rom(0x4000), 0xFF);

        // 64 KB of RAM: banks 4-7 exist too.
        let mut cart = Cartridge::from_rom(make_rom(0x13, 4, 0x05)).unwrap();
        cart.write_rom(0x0000, 0x0A);
        for bank in 0..8 {
            cart.write_rom(0x4000, bank);
            cart.write_ram(0xA000, 0x10 + bank);
        }
        for bank in 0..8 {
            cart.write_rom(0x4000, bank);
            assert_eq!(cart.read_ram(0xA000), 0x10 + bank, "bank {bank}");
        }
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

    fn cart_state(cart: &Cartridge) -> Vec<u8> {
        let mut w = StateWriter::new();
        cart.save_state(&mut w);
        w.finish(cart.rom_hash())
    }

    fn load_cart_state(cart: &mut Cartridge, state: &[u8]) -> Result<(), StateError> {
        let mut r = StateReader::open(state, cart.rom_hash())?;
        cart.load_state(&mut r)?;
        r.finish()
    }

    #[test]
    fn mbc3_banks_ram_and_clock_round_trip_through_a_state() {
        // MBC3+TIMER+RAM+BATTERY, 32 KiB of RAM.
        let mut cart = Cartridge::from_rom(make_rom(0x10, 64, 0x03)).unwrap();
        cart.write_rom(0x0000, 0x0A); // RAM and clock on
        cart.write_rom(0x2000, 5);
        cart.write_rom(0x4000, 2);
        cart.write_ram(0xA123, 0x42);
        set_time(&mut cart, 300, 13, 37, 42, 0);
        cart.tick(CPU_HZ / 2); // half a second into the next one
        latch(&mut cart);
        cart.write_rom(0x4000, 0x0A); // leave the hours selected
        cart.take_save_dirty();
        let state = cart_state(&cart);

        // Change everything the state holds.
        cart.write_rom(0x2000, 9);
        cart.write_rom(0x4000, 2);
        cart.write_ram(0xA123, 0x00);
        set_time(&mut cart, 1, 2, 3, 4, 0);
        latch(&mut cart);
        cart.write_rom(0x0000, 0x00);

        load_cart_state(&mut cart, &state).unwrap();
        assert!(cart_state(&cart) == state, "everything back");
        assert_eq!(bank_at(&cart, 0x4000), 5);
        assert_eq!(cart.read_ram(0xA000), 13, "hours selected, latched 13");
        cart.write_rom(0x4000, 2);
        assert_eq!(cart.read_ram(0xA123), 0x42, "RAM bank 2");
        assert!(cart.take_save_dirty(), "the battery save changed with it");
        // The sub-second count came back too: half a second more ticks over.
        cart.tick(CPU_HZ / 2);
        latch(&mut cart);
        assert_eq!(read_rtc(&mut cart, 0x08), 43);
    }

    #[test]
    fn a_state_from_another_kind_of_cartridge_is_refused() {
        let mbc5 = Cartridge::from_rom(make_rom(0x19, 4, 0)).unwrap();
        let mut w = StateWriter::new();
        mbc5.save_state(&mut w);
        let mut mbc1 = Cartridge::from_rom(make_rom(0x01, 4, 0)).unwrap();
        let state = w.finish(mbc1.rom_hash()); // pretend it's for this ROM
        assert!(matches!(
            load_cart_state(&mut mbc1, &state),
            Err(StateError::Corrupt(_))
        ));
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

    /// MBC1+RAM+BATTERY, 32 KiB RAM, enabled.
    fn battery_cart() -> Cartridge {
        let mut cart = Cartridge::from_rom(make_rom(0x03, 4, 0x03)).unwrap();
        cart.write_rom(0x0000, 0x0A);
        cart
    }

    #[test]
    fn ram_save_round_trips() {
        let mut cart = battery_cart();
        cart.write_rom(0x6000, 1); // RAM banking mode, so all 4 banks are reachable
        for bank in 0..4u8 {
            cart.write_rom(0x4000, bank);
            cart.write_ram(0xA000, 0x10 + bank);
            cart.write_ram(0xBFFF, 0x20 + bank);
        }
        let save = cart.save_data(1_000).unwrap();
        assert_eq!(save.len(), 0x8000, "just the RAM: no clock on MBC1");

        let mut fresh = battery_cart();
        fresh.load_save(&save, 2_000).unwrap();
        assert_eq!(fresh.save_data(2_000).unwrap(), save);
        fresh.write_rom(0x6000, 1);
        fresh.write_rom(0x4000, 3);
        assert_eq!(fresh.read_ram(0xBFFF), 0x23);
    }

    #[test]
    fn saves_need_a_battery_and_the_right_size() {
        let mut no_battery = Cartridge::from_rom(make_rom(0x02, 4, 0x03)).unwrap();
        assert!(!no_battery.has_battery());
        assert_eq!(no_battery.save_data(0), None);
        assert_eq!(
            no_battery.load_save(&[0; 0x8000], 0),
            Err(SaveError::NoBattery)
        );

        let mut cart = battery_cart();
        let err = cart.load_save(&[0; 100], 0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "save is 100 bytes; this cartridge's is 32768 bytes"
        );
        assert!(
            cart.load_save(&[0; 0x8000 + 48], 0).is_err(),
            "no clock to restore"
        );

        let mut rtc = rtc_cart();
        let err = rtc.load_save(&[0; 10], 0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "save is 10 bytes; this cartridge's is 32816 or 32768 bytes"
        );
    }

    #[test]
    fn save_dirty_flags_game_writes_only() {
        let mut cart = battery_cart();
        assert!(!cart.take_save_dirty());
        cart.write_ram(0xA000, 1);
        assert!(cart.take_save_dirty());
        assert!(!cart.take_save_dirty(), "reported once");
        cart.write_rom(0x0000, 0x00); // RAM disabled: the write goes nowhere
        cart.write_ram(0xA000, 2);
        assert!(!cart.take_save_dirty());
        cart.write_ram(0xA000, 2);
        let save = cart.save_data(0).unwrap();
        cart.load_save(&save, 0).unwrap();
        assert!(!cart.take_save_dirty(), "loading isn't a change to store");
    }

    #[test]
    fn mbc2_save_is_512_nibbles() {
        let mut cart = Cartridge::from_rom(make_rom(0x06, 4, 0)).unwrap();
        cart.write_rom(0x0000, 0x0A);
        cart.write_ram(0xA1FF, 0x0C);
        let save = cart.save_data(0).unwrap();
        assert_eq!(save.len(), 512);
        assert_eq!(save[511], 0x0C);

        let mut fresh = Cartridge::from_rom(make_rom(0x06, 4, 0)).unwrap();
        let mut foreign = save.clone();
        foreign[0] = 0xAB; // another emulator may store the top bits
        fresh.load_save(&foreign, 0).unwrap();
        fresh.write_rom(0x0000, 0x0A);
        assert_eq!(fresh.read_ram(0xA000), 0xFB, "only the low nibble is kept");
        assert_eq!(fresh.read_ram(0xA1FF), 0xFC);
    }

    #[test]
    fn rtc_save_uses_the_bgb_layout() {
        let mut cart = rtc_cart();
        set_time(&mut cart, 0x103, 4, 5, 6, 0);
        latch(&mut cart);
        cart.write_rom(0x4000, 0x00);
        cart.write_ram(0xA000, 0x77);
        let save = cart.save_data(0x0123_4567_89AB).unwrap();
        assert_eq!(save.len(), 0x8000 + 48);
        assert_eq!(save[0], 0x77);
        let clock = &save[0x8000..];
        let u32s: Vec<u32> = (0..10)
            .map(|i| {
                u32::from_le_bytes([
                    clock[i * 4],
                    clock[i * 4 + 1],
                    clock[i * 4 + 2],
                    clock[i * 4 + 3],
                ])
            })
            .collect();
        assert_eq!(
            u32s[..5],
            [6, 5, 4, 0x03, 0x01],
            "live: s m h days days-high"
        );
        assert_eq!(u32s[5..], [6, 5, 4, 0x03, 0x01], "latched");
        assert_eq!(
            &clock[40..48],
            &0x0123_4567_89ABu64.to_le_bytes(),
            "timestamp"
        );
    }

    #[test]
    fn loading_an_rtc_save_catches_the_clock_up() {
        let mut cart = rtc_cart();
        set_time(&mut cart, 10, 23, 0, 0, 0);
        let save = cart.save_data(1_000_000).unwrap();

        // Two days, an hour and 30 seconds later.
        let mut later = rtc_cart();
        later
            .load_save(&save, 1_000_000 + 2 * 86_400 + 3_600 + 30)
            .unwrap();
        assert_eq!(time(&mut later), (13, 0, 0, 30, 0));

        // The old 44-byte format, with a 32-bit timestamp.
        let mut old = save.clone();
        old.truncate(0x8000 + 44);
        let mut cart = rtc_cart();
        cart.load_save(&old, 1_000_000 + 60).unwrap();
        assert_eq!(time(&mut cart), (10, 23, 1, 0, 0));

        // BGB's "no timestamp" marker, and a clock from the future: no catch-up.
        let mut unknown = save.clone();
        unknown[0x8000 + 40..].copy_from_slice(&RTC_NO_TIMESTAMP.to_le_bytes());
        let mut cart = rtc_cart();
        cart.load_save(&unknown, 2_000_000).unwrap();
        assert_eq!(time(&mut cart), (10, 23, 0, 0, 0));
        let mut cart = rtc_cart();
        cart.load_save(&save, 999_000).unwrap();
        assert_eq!(time(&mut cart), (10, 23, 0, 0, 0));
    }

    #[test]
    fn a_halted_clock_stays_put_while_the_game_is_closed() {
        let mut cart = rtc_cart();
        set_time(&mut cart, 1, 2, 3, 4, RTC_HALT);
        let save = cart.save_data(0).unwrap();
        let mut later = rtc_cart();
        later.load_save(&save, 86_400 * 30).unwrap();
        assert_eq!(time(&mut later), (1, 2, 3, 4, RTC_HALT));
    }

    #[test]
    fn rtc_catch_up_matches_ticking_one_second_at_a_time() {
        // A small deterministic generator, so failures reproduce.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for case in 0..300 {
            // Mostly normal times, some out of range, days anywhere (so some
            // runs overflow past 511 and set the carry).
            let regs = RtcRegs {
                seconds: next(64) as u8,
                minutes: next(64) as u8,
                hours: next(32) as u8,
                days_low: next(256) as u8,
                control: (next(2) as u8) | if next(4) == 0 { RTC_CARRY } else { 0 },
            };
            let secs = match case % 3 {
                0 => next(200),
                1 => next(5_000),
                _ => next(200_000),
            };
            let mut stepped = regs;
            for _ in 0..secs {
                stepped.tick_second();
            }
            let mut jumped = regs;
            jumped.advance(secs);
            assert_eq!(jumped, stepped, "case {case}: {regs:?} + {secs} s");
        }
    }
}
