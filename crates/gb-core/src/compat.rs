//! The Game Boy Color running an original Game Boy cartridge ("DMG
//! compatibility mode"). Its boot ROM sees $0143 bit 7 clear, writes $04 to
//! KEY0 to turn its own features off, and loads three 4-color palettes: one
//! for the background and one each for OBP0 and OBP1. From then on BGP,
//! OBP0 and OBP1 pick their shades from those colors instead of the
//! original's four greys, which is how the Color colors games that know
//! nothing about color.
//!
//! https://gbdev.io/pandocs/Power_Up_Sequence.html#compatibility-palettes

use crate::cartridge::Cartridge;
use crate::cpu::Registers;

/// The colors the boot ROM loads, as RGB555 (bits 0-4 red, 5-9 green,
/// 10-14 blue), lightest first: what shades 0-3 become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompatPalettes {
    pub bg: [u16; 4],
    /// For OBP0 and OBP1.
    pub obj: [[u16; 4]; 2],
}

/// What the boot ROM picks for any game it doesn't recognize: green and blue
/// for the background, red for sprites.
pub const DEFAULT_PALETTES: CompatPalettes = CompatPalettes {
    bg: [0x7FFF, 0x1BEF, 0x6180, 0x0000],
    obj: [[0x7FFF, 0x421F, 0x1CF2, 0x0000]; 2],
};

/// The registers the Color's boot ROM leaves at $0100 for an original
/// cartridge. A = $11 still says "Game Boy Color"; B is the title
/// checksum for Nintendo's games, 0 for others, and HL follows from it.
/// https://gbdev.io/pandocs/Power_Up_Sequence.html#cpu-registers
pub fn boot_registers(cart: &Cartridge) -> Registers {
    let b = title_checksum(cart).unwrap_or(0);
    let hl: u16 = if b == 0x43 || b == 0x58 {
        0x991A
    } else {
        0x007C
    };
    let [h, l] = hl.to_be_bytes();
    Registers {
        a: 0x11,
        f: 0x80,
        b,
        c: 0x00,
        d: 0x00,
        e: 0x08,
        h,
        l,
        sp: 0xFFFE,
        pc: 0x0100,
    }
}

/// The sum of the 16 title bytes ($0134-$0143), for games Nintendo
/// published: old licensee code $01, or $33 with new licensee code "01".
fn title_checksum(cart: &Cartridge) -> Option<u8> {
    let old = cart.read_rom(0x014B);
    let nintendo = old == 0x01
        || (old == 0x33 && cart.read_rom(0x0144) == b'0' && cart.read_rom(0x0145) == b'1');
    nintendo.then(|| (0x0134..=0x0143).fold(0u8, |sum, addr| sum.wrapping_add(cart.read_rom(addr))))
}

/// The palettes the boot ROM would load for `cart`.
/// TODO(milestone 9): Nintendo's own games get palettes picked from a table
/// by a checksum of the title.
pub fn boot_palettes(_cart: &Cartridge) -> CompatPalettes {
    DEFAULT_PALETTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cartridge::{header_checksum, tests::rom_with_program};

    /// An original cartridge titled `title`, from `licensee` (old code).
    fn cart(licensee: u8, title: &[u8]) -> Cartridge {
        let mut rom = rom_with_program(&[]);
        rom[0x134..0x144].fill(0);
        rom[0x134..0x134 + title.len()].copy_from_slice(title);
        rom[0x14B] = licensee;
        rom[0x14D] = header_checksum(&rom);
        Cartridge::from_rom(rom).unwrap()
    }

    #[test]
    fn b_holds_the_title_checksum_for_nintendos_games_only() {
        let sum = b"TETRIS".iter().fold(0u8, |s, &c| s.wrapping_add(c));
        let r = boot_registers(&cart(0x01, b"TETRIS"));
        assert_eq!(
            (r.a, r.f, r.b, r.c, r.d, r.e),
            (0x11, 0x80, sum, 0, 0, 0x08)
        );
        assert_eq!(r.hl(), 0x007C);
        assert_eq!(boot_registers(&cart(0x0A, b"TETRIS")).b, 0, "not Nintendo");
        // B = $43 (or $58): HL points into the logo's tile map instead.
        assert_eq!(boot_registers(&cart(0x01, &[0x43])).hl(), 0x991A);
    }
}
