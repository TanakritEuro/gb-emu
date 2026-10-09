//! The original Game Boy's OAM corruption bug. During mode 2 the PPU reads
//! OAM a row (8 bytes, two objects) at a time; if the CPU puts an OAM address
//! ($FE00-$FEFF) on the bus then, by reading or writing it or by a 16-bit
//! increment or decrement passing through it (INC rr, DEC rr, PUSH, CALL,
//! RST, JR, an interrupt), the two collide: the row being read is overwritten
//! with a bitwise mix of itself and the rows before it. The Color fixed it.
//! The patterns are SameBoy's for the DMG (Core/memory.c), whose bitwise
//! formulas come from studying real consoles.
//! https://gbdev.io/pandocs/OAM_Corruption_Bug.html

use super::{Ppu, DOTS_PER_LINE, LINES_PER_FRAME, MODE3_DOT, VBLANK_LINE};
use crate::Model;

/// A write's (or an increment's) mix: ((a ^ c) & (b ^ c)) ^ c.
fn glitch(a: u16, b: u16, c: u16) -> u16 {
    ((a ^ c) & (b ^ c)) ^ c
}

/// A read's mix.
fn glitch_read(a: u16, b: u16, c: u16) -> u16 {
    b | (a & c)
}

fn glitch_read_secondary(a: u16, b: u16, c: u16, d: u16) -> u16 {
    (b & (a | c | d)) | (a & c & d)
}

fn glitch_tertiary_1(a: u16, b: u16, c: u16, d: u16, e: u16) -> u16 {
    c | (a & b & d & e)
}

fn glitch_tertiary_2(a: u16, b: u16, c: u16, d: u16, e: u16) -> u16 {
    (c & (a | b | d | e)) | (a & b & d & e)
}

fn glitch_tertiary_3(a: u16, b: u16, c: u16, d: u16, e: u16) -> u16 {
    (c & (a | b | d | e)) | (b & d & e)
}

/// Row $40's read mix, of the row and words 2, 3, 4, 7, 8 and 16 back.
/// Some DMGs give non-deterministic bits here; this is the DMGs that give
/// zeros (SameBoy).
fn glitch_quaternary(b: u16, c: u16, d: u16, e: u16, f: u16, g: u16, h: u16) -> u16 {
    (e & (h | g | (!d & f) | c | b)) | (c & g & h)
}

impl Ppu {
    /// The OAM row (as a byte offset) the original's OAM scan is reading
    /// now: the first, 4 dots before the scan starts, then each pair of
    /// objects' row a dot pair after the scan reaches it. None outside
    /// mode 2, on the Color, and on the line after switching on (no scan).
    pub(crate) fn oam_bug_row(&self) -> Option<usize> {
        if self.model != Model::Dmg || !self.lcd_on() {
            return None;
        }
        let next_scans = self.ly + 1 < VBLANK_LINE || self.ly == LINES_PER_FRAME - 1;
        if self.dot >= DOTS_PER_LINE - 4 {
            return next_scans.then_some(0);
        }
        if self.ly >= VBLANK_LINE || self.first_line || self.dot >= MODE3_DOT {
            return None;
        }
        Some(match self.dot {
            0 | 1 => 0,
            d => {
                let object = (d as usize - 2) / 2;
                (object & !1) * 4 + 8
            }
        })
    }

    fn oam_word(&self, offset: usize) -> u16 {
        u16::from_le_bytes([self.oam[offset], self.oam[offset + 1]])
    }

    fn set_oam_word(&mut self, offset: usize, val: u16) {
        [self.oam[offset], self.oam[offset + 1]] = val.to_le_bytes();
    }

    /// Copies a row's 8 bytes over another's.
    fn copy_oam_row(&mut self, from: usize, to: usize) {
        self.oam.copy_within(from..from + 8, to);
    }

    /// A write to OAM, or an increment or decrement through it, during the
    /// scan: the row's first word mixes with the row before; the rest of
    /// it becomes a copy of that row.
    pub(crate) fn oam_bug_write(&mut self, addr: u16) {
        if !(0xFE00..0xFF00).contains(&addr) {
            return;
        }
        // Row $A0 is past the 40 objects: its bytes are the unusable
        // $FEA0-$FEFF, so nothing shows.
        let Some(row) = self.oam_bug_row().filter(|&r| (8..0xA0).contains(&r)) else {
            return;
        };
        let mixed = glitch(
            self.oam_word(row),
            self.oam_word(row - 8),
            self.oam_word(row - 4),
        );
        self.set_oam_word(row, mixed);
        self.oam.copy_within(row - 6..row, row + 2);
    }

    /// A read of OAM during the scan. Which mix depends on the row; rows
    /// $10, $30 .. and $00, $20 .. also reach two rows back.
    pub(crate) fn oam_bug_read(&mut self, addr: u16) {
        if !(0xFE00..0xFF00).contains(&addr) {
            return;
        }
        // Row $A0 is past the 40 objects: its bytes are the unusable
        // $FEA0-$FEFF, so nothing shows.
        let Some(row) = self.oam_bug_row().filter(|&r| (8..0xA0).contains(&r)) else {
            return;
        };
        let w = |p: &Self, offset: usize| p.oam_word(offset);
        match row & 0x18 {
            0x10 => {
                if row < 0x98 {
                    let mixed = glitch_read_secondary(
                        w(self, row - 16),
                        w(self, row - 8),
                        w(self, row),
                        w(self, row - 4),
                    );
                    self.set_oam_word(row - 8, mixed);
                    self.copy_oam_row(row - 8, row - 16);
                }
            }
            0x00 => {
                if row < 0x98 {
                    let mixed = if row == 0x40 {
                        glitch_quaternary(
                            w(self, row),
                            w(self, row - 4),
                            w(self, row - 6),
                            w(self, row - 8),
                            w(self, row - 14),
                            w(self, row - 16),
                            w(self, row - 32),
                        )
                    } else {
                        let op = match row {
                            0x20 => glitch_tertiary_2,
                            0x60 => glitch_tertiary_3,
                            _ => glitch_tertiary_1,
                        };
                        op(
                            w(self, row),
                            w(self, row - 4),
                            w(self, row - 8),
                            w(self, row - 16),
                            w(self, row - 32),
                        )
                    };
                    self.set_oam_word(row - 8, mixed);
                    self.copy_oam_row(row - 8, row - 16);
                    self.copy_oam_row(row - 8, row - 32);
                }
            }
            _ => {
                let mixed = glitch_read(w(self, row), w(self, row - 8), w(self, row - 4));
                self.set_oam_word(row - 8, mixed);
                self.set_oam_word(row, mixed);
            }
        }
        self.copy_oam_row(row - 8, row);
        if row == 0x80 {
            self.copy_oam_row(row, 0);
        }
    }

    /// A read where OAM is held for reads but not yet (or no longer) for
    /// writes: just before the scan starts, and in its last 4 dots. Before,
    /// the first row mixes with the row read; at the end, the last row with
    /// the row read.
    pub(crate) fn oam_bug_read_edge(&mut self, addr: u16) {
        if !(0xFE00..0xFEA0).contains(&addr) {
            return;
        }
        let read_row = usize::from(addr & 0xF8);
        let read_word = usize::from(addr & 0xFE);
        match self.oam_bug_row() {
            Some(0) => {
                let mixed = glitch_read(
                    self.oam_word(0),
                    self.oam_word(read_row),
                    self.oam_word(read_word),
                );
                self.set_oam_word(read_row, mixed);
                self.set_oam_word(0, mixed);
                self.oam.copy_within(read_row + 2..read_row + 8, 2);
            }
            Some(0xA0) => {
                let target = usize::from(addr & 7) | 0x98;
                let target = target & !1;
                let a = self.oam_word(0x9C);
                let b = self.oam_word(target);
                let majority = |a: u16, b: u16, c: u16| (a & b) | (a & c) | (b & c);
                let mixed = match addr & 7 {
                    0 | 1 => Some(majority(a, b, self.oam_word(read_row))),
                    2 | 3 => Some(majority(a, b, self.oam_word(read_word))),
                    6 | 7 => Some(glitch_read(a, b, self.oam_word(read_row))),
                    _ => None,
                };
                if let Some(mixed) = mixed {
                    self.set_oam_word(target, mixed);
                }
                self.copy_oam_row(0x98, read_row);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original with the LCD on, OAM holding 0, 1, 2 .. $9F, run to
    /// `dot` of line `ly`.
    fn scanning(ly: u8, dot: u32) -> Ppu {
        let mut p = Ppu::new();
        p.lcdc = 0x91;
        for (i, b) in p.oam.iter_mut().enumerate() {
            *b = i as u8;
        }
        while (p.ly, p.dot) != (ly, dot) {
            p.tick(1);
        }
        p
    }

    fn row(p: &Ppu, offset: usize) -> [u8; 8] {
        p.oam[offset..offset + 8].try_into().unwrap()
    }

    #[test]
    fn the_scanned_row_moves_on_every_other_object() {
        let at = |ly, dot| scanning(ly, dot).oam_bug_row();
        assert_eq!(at(1, 0), Some(0), "before the first pair");
        assert_eq!(at(1, 2), Some(0x08));
        assert_eq!(at(1, 5), Some(0x08), "objects 0 and 1 share a row");
        assert_eq!(at(1, 6), Some(0x10));
        assert_eq!(at(1, 79), Some(0xA0));
        assert_eq!(at(1, 80), None, "mode 3");
        assert_eq!(at(1, 452), Some(0), "the next line's scan is coming");
        assert_eq!(at(143, 452), None, "VBlank is next");
        assert_eq!(at(153, 452), Some(0));
        let mut p = scanning(1, 10);
        p.model = Model::Cgb;
        assert_eq!(p.oam_bug_row(), None, "the Color has no bug");
    }

    #[test]
    fn a_write_turns_the_scanned_row_into_a_mix_of_the_one_before() {
        // Row $10 at dot 6. Its first word mixes with row $08's ($0908 and
        // $0D0C), which here gives $0908; the rest copies row $08.
        let mut p = scanning(1, 6);
        p.oam_bug_write(0xFE42);
        assert_eq!(row(&p, 0x10), [8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(row(&p, 0x18), [24, 25, 26, 27, 28, 29, 30, 31], "untouched");
        p.oam_bug_write(0xFF00);
        assert_eq!(row(&p, 0x18)[0], 24, "outside OAM: nothing");
    }

    #[test]
    fn a_read_mixes_the_scanned_row_into_the_one_before_too() {
        // Row $18: both it and row $10 take b | (a & c) of their first
        // words ($1110 here), then row $18 copies row $10.
        let mut p = scanning(1, 10);
        assert_eq!(p.oam_bug_row(), Some(0x18));
        p.oam_bug_read(0xFE00);
        assert_eq!(row(&p, 0x10), [16, 17, 18, 19, 20, 21, 22, 23]);
        assert_eq!(row(&p, 0x18), [16, 17, 18, 19, 20, 21, 22, 23]);
    }

    #[test]
    fn a_read_just_before_the_scan_mixes_the_row_read_into_the_first() {
        // Dot 452: OAM is held for reads, the scan is about to read row 0.
        // Reading $FE10 makes row 0 a mix of itself and row $10.
        let mut p = scanning(0, 452);
        p.oam_bug_read_edge(0xFE10);
        assert_eq!(row(&p, 0), [16, 17, 18, 19, 20, 21, 22, 23]);
        assert_eq!(row(&p, 0x10)[..2], [16, 17]);
    }
}
