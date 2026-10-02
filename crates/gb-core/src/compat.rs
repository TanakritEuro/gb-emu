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

/// The palettes the boot ROM loads for `cart`. Nintendo's own games each
/// get theirs, found by the title checksum (and, where several titles share
/// a checksum, by the title's 4th letter); everything else gets
/// [`DEFAULT_PALETTES`].
/// TODO(accuracy): two of those games (marked in `COMBINATION_PER_CHECKSUM`)
/// rely on the Nintendo logo the boot ROM leaves in VRAM, which isn't there.
pub fn boot_palettes(cart: &Cartridge) -> CompatPalettes {
    combination(boot_combination(cart))
}

/// The palettes a real Color lets you pick for an original game by holding
/// a direction, maybe with A or B, during its boot animation: (the buttons,
/// which of the [`COMBINATIONS`]). Holding one overrides the boot ROM's
/// own choice. https://tcrf.net/Game_Boy_Color_Bootstrap_ROM
pub const BUTTON_PALETTES: [(&str, u8); 12] = [
    ("Up", 5),
    ("Up + A", 43),
    ("Up + B", 28),
    ("Down", 8),
    ("Down + A", 3),
    ("Down + B", 49),
    ("Left", 48),
    ("Left + A", 40),
    ("Left + B", 7),
    ("Right", 1),
    ("Right + A", 0),
    ("Right + B", 6),
];

/// The palettes for [`BUTTON_PALETTES`] entry `i`.
pub fn button_palettes(i: usize) -> Option<CompatPalettes> {
    BUTTON_PALETTES
        .get(i)
        .map(|&(_, combo)| combination(usize::from(combo)))
}

/// Which of the [`COMBINATIONS`] the boot ROM picks for `cart`.
fn boot_combination(cart: &Cartridge) -> usize {
    let Some(checksum) = title_checksum(cart) else {
        return 0;
    };
    let fourth_letter = cart.read_rom(0x0137);
    TITLE_CHECKSUMS
        .iter()
        .enumerate()
        .position(|(i, &c)| {
            c == checksum
                && (i < FIRST_DUPLICATE
                    || DUPLICATE_4TH_LETTERS[i - FIRST_DUPLICATE] == fourth_letter)
        })
        .map_or(0, |i| usize::from(COMBINATION_PER_CHECKSUM[i]))
}

/// The palettes of combination `index`.
fn combination(index: usize) -> CompatPalettes {
    let palette = |start: u8| {
        let s = usize::from(start);
        [COLORS[s], COLORS[s + 1], COLORS[s + 2], COLORS[s + 3]]
    };
    let [obj0, obj1, bg] = COMBINATIONS[index];
    CompatPalettes {
        bg: palette(bg),
        obj: [palette(obj0), palette(obj1)],
    }
}

// The boot ROM's tables, as SameBoy's open-source Color boot ROM has them
// (BootROMs/cgb_boot.asm, https://github.com/LIJI32/SameBoy), leaving out the
// palettes SameBoy adds of its own:
//
// Copyright (c) 2015-2026 Lior Halphon
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit
// persons to whom the Software is furnished to do so, subject to the
// following conditions:
//
// The above copyright notice and this permission notice shall be included
// in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
// USE OR OTHER DEALINGS IN THE SOFTWARE.

/// Title checksums the boot ROM knows, in its search order. From index
/// [`FIRST_DUPLICATE`] on, several games share a checksum and the title's 4th
/// letter tells them apart.
const TITLE_CHECKSUMS: [u8; 94] = [
    0x00, // 0, Default
    0x88, // 1, ALLEY WAY
    0x16, // 2, YAKUMAN
    0x36, // 3, BASEBALL, (Game and Watch 2)
    0xD1, // 4, TENNIS
    0xDB, // 5, TETRIS
    0xF2, // 6, QIX
    0x3C, // 7, DR.MARIO
    0x8C, // 8, RADARMISSION
    0x92, // 9, F1RACE
    0x3D, // 10, YOSSY NO TAMAGO
    0x5C, // 11
    0x58, // 12, X
    0xC9, // 13, MARIOLAND2
    0x3E, // 14, YOSSY NO COOKIE
    0x70, // 15, ZELDA
    0x1D, // 16
    0x59, // 17
    0x69, // 18, TETRIS FLASH
    0x19, // 19, DONKEY KONG
    0x35, // 20, MARIO'S PICROSS
    0xA8, // 21
    0x14, // 22, POKEMON RED, (GAMEBOYCAMERA G)
    0xAA, // 23, POKEMON GREEN
    0x75, // 24, PICROSS 2
    0x95, // 25, YOSSY NO PANEPON
    0x99, // 26, KIRAKIRA KIDS
    0x34, // 27, GAMEBOY GALLERY
    0x6F, // 28, POCKETCAMERA
    0x15, // 29
    0xFF, // 30, BALLOON KID
    0x97, // 31, KINGOFTHEZOO
    0x4B, // 32, DMG FOOTBALL
    0x90, // 33, WORLD CUP
    0x17, // 34, OTHELLO
    0x10, // 35, SUPER RC PRO-AM
    0x39, // 36, DYNABLASTER
    0xF7, // 37, BOY AND BLOB GB2
    0xF6, // 38, MEGAMAN
    0xA2, // 39, STAR WARS-NOA
    0x49, // 40
    0x4E, // 41, WAVERACE
    0x43, // 42
    0x68, // 43, LOLO2
    0xE0, // 44, YOSHI'S COOKIE
    0x8B, // 45, MYSTIC QUEST
    0xF0, // 46
    0xCE, // 47, TOPRANKINGTENNIS
    0x0C, // 48, MANSELL
    0x29, // 49, MEGAMAN3
    0xE8, // 50, SPACE INVADERS
    0xB7, // 51, GAME&WATCH
    0x86, // 52, DONKEYKONGLAND95
    0x9A, // 53, ASTEROIDS/MISCMD
    0x52, // 54, STREET FIGHTER 2
    0x01, // 55, DEFENDER/JOUST
    0x9D, // 56, KILLERINSTINCT95
    0x71, // 57, TETRIS BLAST
    0x9C, // 58, PINOCCHIO
    0xBD, // 59
    0x5D, // 60, BA.TOSHINDEN
    0x6D, // 61, NETTOU KOF 95
    0x67, // 62
    0x3F, // 63, TETRIS PLUS
    0x6B, // 64, DONKEYKONGLAND 3
    0xB3, // 65, ???[B]????????
    0x46, // 66, SUP[E]R MARIOLAND
    0x28, // 67, GOL[F]
    0xA5, // 68, SOL[A]RSTRIKER
    0xC6, // 69, GBW[A]RS
    0xD3, // 70, KAE[R]UNOTAMENI
    0x27, // 71, ???[B]????????
    0x61, // 72, POK[E]MON BLUE
    0x18, // 73, DON[K]EYKONGLAND
    0x66, // 74, GAM[E]BOY GALLERY2
    0x6A, // 75, DON[K]EYKONGLAND 2
    0xBF, // 76, KID[ ]ICARUS
    0x0D, // 77, TET[R]IS2
    0xF4, // 78, ???[-]????????
    0xB3, // 79, MOG[U]RANYA
    0x46, // 80, ???[R]????????
    0x28, // 81, GAL[A]GA&GALAXIAN
    0xA5, // 82, BT2[R]AGNAROKWORLD
    0xC6, // 83, KEN[ ]GRIFFEY JR
    0xD3, // 84, ???[I]????????
    0x27, // 85, MAG[N]ETIC SOCCER
    0x61, // 86, VEG[A]S STAKES
    0x18, // 87, ???[I]????????
    0x66, // 88, MIL[L]I/CENTI/PEDE
    0x6A, // 89, MAR[I]O & YOSHI
    0xBF, // 90, SOC[C]ER
    0x0D, // 91, POK[E]BOM
    0xF4, // 92, G&W[ ]GALLERY
    0xB3, // 93, TET[R]IS ATTACK
];

const FIRST_DUPLICATE: usize = 65;

/// The 4th title letter for each checksum from [`FIRST_DUPLICATE`] on.
const DUPLICATE_4TH_LETTERS: &[u8; 29] = b"BEFAARBEKEK R-URAR INAILICE R";

/// The palette combination for each checksum in [`TITLE_CHECKSUMS`].
const COMBINATION_PER_CHECKSUM: [u8; 94] = [
    0,  // 0, Default Palette
    4,  // 1, ALLEY WAY
    5,  // 2, YAKUMAN
    35, // 3, BASEBALL, (Game and Watch 2)
    34, // 4, TENNIS
    3,  // 5, TETRIS
    31, // 6, QIX
    15, // 7, DR.MARIO
    10, // 8, RADARMISSION
    5,  // 9, F1RACE
    19, // 10, YOSSY NO TAMAGO
    36, // 11
    7,  // 12, X (relies on the boot logo's tile map)
    37, // 13, MARIOLAND2
    30, // 14, YOSSY NO COOKIE
    44, // 15, ZELDA
    21, // 16
    32, // 17
    31, // 18, TETRIS FLASH
    20, // 19, DONKEY KONG
    5,  // 20, MARIO'S PICROSS
    33, // 21
    13, // 22, POKEMON RED, (GAMEBOYCAMERA G)
    14, // 23, POKEMON GREEN
    5,  // 24, PICROSS 2
    29, // 25, YOSSY NO PANEPON
    5,  // 26, KIRAKIRA KIDS
    18, // 27, GAMEBOY GALLERY
    9,  // 28, POCKETCAMERA
    3,  // 29
    2,  // 30, BALLOON KID
    26, // 31, KINGOFTHEZOO
    25, // 32, DMG FOOTBALL
    25, // 33, WORLD CUP
    41, // 34, OTHELLO
    42, // 35, SUPER RC PRO-AM
    26, // 36, DYNABLASTER
    45, // 37, BOY AND BLOB GB2
    42, // 38, MEGAMAN
    45, // 39, STAR WARS-NOA
    36, // 40
    38, // 41, WAVERACE
    26, // 42, (relies on the boot logo's tile map)
    42, // 43, LOLO2
    30, // 44, YOSHI'S COOKIE
    41, // 45, MYSTIC QUEST
    34, // 46
    34, // 47, TOPRANKINGTENNIS
    5,  // 48, MANSELL
    42, // 49, MEGAMAN3
    6,  // 50, SPACE INVADERS
    5,  // 51, GAME&WATCH
    33, // 52, DONKEYKONGLAND95
    25, // 53, ASTEROIDS/MISCMD
    42, // 54, STREET FIGHTER 2
    42, // 55, DEFENDER/JOUST
    40, // 56, KILLERINSTINCT95
    2,  // 57, TETRIS BLAST
    16, // 58, PINOCCHIO
    25, // 59
    42, // 60, BA.TOSHINDEN
    42, // 61, NETTOU KOF 95
    5,  // 62
    0,  // 63, TETRIS PLUS
    39, // 64, DONKEYKONGLAND 3
    36, // 65
    22, // 66, SUPER MARIOLAND
    25, // 67, GOLF
    6,  // 68, SOLARSTRIKER
    32, // 69, GBWARS
    12, // 70, KAERUNOTAMENI
    36, // 71
    11, // 72, POKEMON BLUE
    39, // 73, DONKEYKONGLAND
    18, // 74, GAMEBOY GALLERY2
    39, // 75, DONKEYKONGLAND 2
    24, // 76, KID ICARUS
    31, // 77, TETRIS2
    50, // 78
    17, // 79, MOGURANYA
    46, // 80
    6,  // 81, GALAGA&GALAXIAN
    27, // 82, BT2RAGNAROKWORLD
    0,  // 83, KEN GRIFFEY JR
    47, // 84
    41, // 85, MAGNETIC SOCCER
    41, // 86, VEGAS STAKES
    0,  // 87
    0,  // 88, MILLI/CENTI/PEDE
    19, // 89, MARIO & YOSHI
    34, // 90, SOCCER
    23, // 91, POKEBOM
    18, // 92, G&W GALLERY
    29, // 93, TETRIS ATTACK
];

/// Palette combinations: where the sprite palette for OBP0, the one for
/// OBP1 and the background palette start in [`COLORS`]. Mostly whole
/// palettes (a multiple of 4), but a few start mid-palette.
const COMBINATIONS: [[u8; 3]; 51] = [
    [16, 16, 116],   // 0, Right + A
    [72, 72, 72],    // 1, Right
    [80, 80, 80],    // 2
    [96, 96, 96],    // 3, Down + A
    [36, 36, 36],    // 4
    [0, 0, 0],       // 5, Up
    [108, 108, 108], // 6, Right + B
    [20, 20, 20],    // 7, Left + B
    [48, 48, 48],    // 8, Down
    [104, 104, 104], // 9
    [64, 32, 32],    // 10
    [16, 112, 112],  // 11
    [16, 8, 8],      // 12
    [12, 16, 16],    // 13
    [16, 116, 116],  // 14
    [112, 16, 112],  // 15
    [8, 68, 8],      // 16
    [64, 64, 32],    // 17
    [16, 16, 28],    // 18
    [16, 16, 72],    // 19
    [16, 16, 80],    // 20
    [76, 76, 36],    // 21
    [15, 15, 44],    // 22
    [68, 68, 8],     // 23
    [16, 16, 8],     // 24
    [16, 16, 12],    // 25
    [112, 112, 0],   // 26
    [12, 12, 0],     // 27
    [0, 0, 4],       // 28, Up + B
    [72, 88, 72],    // 29
    [80, 88, 80],    // 30
    [96, 88, 96],    // 31
    [64, 88, 32],    // 32
    [68, 16, 52],    // 33
    [111, 0, 56],    // 34
    [111, 16, 60],   // 35
    [76, 91, 36],    // 36
    [64, 112, 40],   // 37
    [16, 92, 112],   // 38
    [68, 88, 8],     // 39
    [16, 0, 8],      // 40, Left + A
    [16, 112, 12],   // 41
    [112, 12, 0],    // 42
    [12, 112, 16],   // 43, Up + A
    [84, 112, 16],   // 44
    [12, 112, 0],    // 45
    [100, 12, 112],  // 46
    [0, 112, 32],    // 47
    [16, 12, 112],   // 48, Left
    [112, 12, 24],   // 49, Down + B
    [16, 112, 116],  // 50
];

/// The boot ROM's colors, RGB555, four to a palette.
const COLORS: [u16; 120] = [
    0x7FFF, 0x32BF, 0x00D0, 0x0000, // 0
    0x639F, 0x4279, 0x15B0, 0x04CB, // 1
    0x7FFF, 0x6E31, 0x454A, 0x0000, // 2
    0x7FFF, 0x1BEF, 0x0200, 0x0000, // 3
    0x7FFF, 0x421F, 0x1CF2, 0x0000, // 4
    0x7FFF, 0x5294, 0x294A, 0x0000, // 5
    0x7FFF, 0x03FF, 0x012F, 0x0000, // 6
    0x7FFF, 0x03EF, 0x01D6, 0x0000, // 7
    0x7FFF, 0x42B5, 0x3DC8, 0x0000, // 8
    0x7E74, 0x03FF, 0x0180, 0x0000, // 9
    0x67FF, 0x77AC, 0x1A13, 0x2D6B, // 10
    0x7ED6, 0x4BFF, 0x2175, 0x0000, // 11
    0x53FF, 0x4A5F, 0x7E52, 0x0000, // 12
    0x4FFF, 0x7ED2, 0x3A4C, 0x1CE0, // 13
    0x03ED, 0x7FFF, 0x255F, 0x0000, // 14
    0x036A, 0x021F, 0x03FF, 0x7FFF, // 15
    0x7FFF, 0x01DF, 0x0112, 0x0000, // 16
    0x231F, 0x035F, 0x00F2, 0x0009, // 17
    0x7FFF, 0x03EA, 0x011F, 0x0000, // 18
    0x299F, 0x001A, 0x000C, 0x0000, // 19
    0x7FFF, 0x027F, 0x001F, 0x0000, // 20
    0x7FFF, 0x03E0, 0x0206, 0x0120, // 21
    0x7FFF, 0x7EEB, 0x001F, 0x7C00, // 22
    0x7FFF, 0x3FFF, 0x7E00, 0x001F, // 23
    0x7FFF, 0x03FF, 0x001F, 0x0000, // 24
    0x03FF, 0x001F, 0x000C, 0x0000, // 25
    0x7FFF, 0x033F, 0x0193, 0x0000, // 26
    0x0000, 0x4200, 0x037F, 0x7FFF, // 27
    0x7FFF, 0x7E8C, 0x7C00, 0x0000, // 28
    0x7FFF, 0x1BEF, 0x6180, 0x0000, // 29
];

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

    #[test]
    fn the_tables_fit_together() {
        assert_eq!(
            TITLE_CHECKSUMS.len(),
            FIRST_DUPLICATE + DUPLICATE_4TH_LETTERS.len()
        );
        assert!(COMBINATION_PER_CHECKSUM
            .iter()
            .all(|&c| usize::from(c) < COMBINATIONS.len()));
        assert!(COMBINATIONS
            .iter()
            .flatten()
            .all(|&start| usize::from(start) + 4 <= COLORS.len()));
        // Combination 0 is the well-known default (typed in from its colors:
        // #FFFFFF #7BFF31 #0063C5 #000000 and #FFFFFF #FF8484 #943A3A #000000).
        assert_eq!(combination(0), DEFAULT_PALETTES);
    }

    #[test]
    fn nintendos_games_get_their_own_palettes() {
        // Real titles: their checksums ($14, $61) are in the table.
        let red = boot_palettes(&cart(0x01, b"POKEMON RED"));
        assert_eq!(red.bg, [0x7FFF, 0x421F, 0x1CF2, 0x0000], "reds");
        // $61 is shared, and the 4th letter, E, picks Pokémon Blue's.
        let blue = boot_palettes(&cart(0x01, b"POKEMON BLUE"));
        assert_eq!(blue.bg, [0x7FFF, 0x7E8C, 0x7C00, 0x0000], "blues");
        // The new licensee code "01" counts as Nintendo too.
        let mut rom = rom_with_program(&[]);
        rom[0x134..0x144].fill(0);
        rom[0x134..0x13F].copy_from_slice(b"POKEMON RED");
        rom[0x14B] = 0x33;
        rom[0x144..0x146].copy_from_slice(b"01");
        rom[0x14D] = header_checksum(&rom);
        assert_eq!(boot_palettes(&Cartridge::from_rom(rom).unwrap()), red);
    }

    #[test]
    fn button_combinations_pick_the_known_palettes() {
        let named = |name| {
            let i = BUTTON_PALETTES
                .iter()
                .position(|&(n, _)| n == name)
                .unwrap();
            button_palettes(i).unwrap()
        };
        // Up is the brown one; Left + B grey all over.
        assert_eq!(named("Up").bg, [0x7FFF, 0x32BF, 0x00D0, 0x0000]);
        let grey = [0x7FFF, 0x5294, 0x294A, 0x0000];
        let left_b = named("Left + B");
        assert_eq!((left_b.bg, left_b.obj), (grey, [grey, grey]));
        assert_eq!(named("Right + A"), DEFAULT_PALETTES, "same as the default");
        assert_eq!(button_palettes(BUTTON_PALETTES.len()), None);
    }

    #[test]
    fn everything_else_gets_the_default() {
        let default = DEFAULT_PALETTES;
        assert_eq!(
            boot_palettes(&cart(0x0A, b"POKEMON RED")),
            default,
            "not Nintendo"
        );
        assert_eq!(boot_palettes(&cart(0x01, b"NO SUCH GAME")), default);
        // A shared checksum with a 4th letter the table doesn't list.
        let mut title = *b"POKEMON BLUE";
        title[3] = b'Z';
        title[11] = title[11].wrapping_add(b'E').wrapping_sub(b'Z'); // same sum
        assert_eq!(boot_palettes(&cart(0x01, &title)), default);
    }
}
