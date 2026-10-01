//! The pixel processing unit.
//!
//! Timing is in place: LY advances every 456 dots, VBlank fires at line 144,
//! and the LYC=LY comparison sets STAT. Each finished line is drawn into the
//! framebuffer; so far that's the background layer only.
//!
//! Reference: https://gbdev.io/pandocs/Rendering.html

use crate::bus::interrupt;
use crate::{SCREEN_HEIGHT, SCREEN_WIDTH};

/// Shades 0 (lightest) to 3 (darkest) as RGBA, in the classic green.
pub const DMG_PALETTE: [[u8; 4]; 4] = [
    [0xE0, 0xF8, 0xD0, 0xFF],
    [0x88, 0xC0, 0x70, 0xFF],
    [0x34, 0x68, 0x56, 0xFF],
    [0x08, 0x18, 0x20, 0xFF],
];

const DOTS_PER_LINE: u32 = 456;
const LINES_PER_FRAME: u8 = 154;
const VBLANK_LINE: u8 = 144;

pub struct Ppu {
    vram: [u8; 0x2000],
    oam: [u8; 0xA0],
    pub lcdc: u8,
    pub stat: u8,
    pub scy: u8,
    pub scx: u8,
    pub ly: u8,
    pub lyc: u8,
    pub dma: u8,
    pub bgp: u8,
    pub obp0: u8,
    pub obp1: u8,
    pub wy: u8,
    pub wx: u8,
    /// Position within the current scanline, 0..456.
    dot: u32,
    framebuffer: Vec<u8>,
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new()
    }
}

impl Ppu {
    /// Register values as the boot ROM leaves them.
    pub fn new() -> Self {
        Self {
            vram: [0; 0x2000],
            oam: [0; 0xA0],
            lcdc: 0x91,
            stat: 0x85,
            scy: 0,
            scx: 0,
            ly: 0,
            lyc: 0,
            dma: 0xFF,
            bgp: 0xFC,
            obp0: 0xFF,
            obp1: 0xFF,
            wy: 0,
            wx: 0,
            dot: 0,
            framebuffer: DMG_PALETTE[0].repeat(SCREEN_WIDTH * SCREEN_HEIGHT),
        }
    }

    pub fn framebuffer(&self) -> &[u8] {
        &self.framebuffer
    }

    pub fn read_vram(&self, addr: u16) -> u8 {
        self.vram[(addr - 0x8000) as usize]
    }
    pub fn write_vram(&mut self, addr: u16, val: u8) {
        self.vram[(addr - 0x8000) as usize] = val;
    }
    pub fn read_oam(&self, addr: u16) -> u8 {
        self.oam[(addr - 0xFE00) as usize]
    }
    pub fn write_oam(&mut self, addr: u16, val: u8) {
        self.oam[(addr - 0xFE00) as usize] = val;
    }

    pub fn read_reg(&self, addr: u16) -> u8 {
        match addr {
            0xFF40 => self.lcdc,
            0xFF41 => self.stat | 0x80,
            0xFF42 => self.scy,
            0xFF43 => self.scx,
            0xFF44 => self.ly,
            0xFF45 => self.lyc,
            0xFF46 => self.dma,
            0xFF47 => self.bgp,
            0xFF48 => self.obp0,
            0xFF49 => self.obp1,
            0xFF4A => self.wy,
            0xFF4B => self.wx,
            _ => 0xFF,
        }
    }

    pub fn write_reg(&mut self, addr: u16, val: u8) {
        match addr {
            0xFF40 => self.lcdc = val,
            // Bits 0-2 (mode, LYC flag) are read-only.
            0xFF41 => self.stat = (self.stat & 0x07) | (val & 0x78),
            0xFF42 => self.scy = val,
            0xFF43 => self.scx = val,
            0xFF44 => {} // LY is read-only
            0xFF45 => self.lyc = val,
            0xFF46 => self.dma = val,
            0xFF47 => self.bgp = val,
            0xFF48 => self.obp0 = val,
            0xFF49 => self.obp1 = val,
            0xFF4A => self.wy = val,
            0xFF4B => self.wx = val,
            _ => {}
        }
    }

    /// Advances by `cycles` dots. Returns IF bits to request.
    pub fn tick(&mut self, cycles: u32) -> u8 {
        if self.lcdc & 0x80 == 0 {
            // TODO(milestone 3): turning the LCD off resets LY to 0 and mode to 0.
            return 0;
        }
        let mut irq = 0;
        self.dot += cycles;
        while self.dot >= DOTS_PER_LINE {
            self.dot -= DOTS_PER_LINE;
            if self.ly < VBLANK_LINE {
                self.render_scanline();
            }
            self.ly = (self.ly + 1) % LINES_PER_FRAME;
            if self.ly == VBLANK_LINE {
                irq |= interrupt::VBLANK;
            }
            let coincide = self.ly == self.lyc;
            self.stat = (self.stat & !0x04) | if coincide { 0x04 } else { 0 };
            if coincide && self.stat & 0x40 != 0 {
                irq |= interrupt::STAT;
            }
        }
        self.update_mode();
        // TODO(milestone 3): STAT interrupts on mode 0/1/2 entry (bits 3-5).
        irq
    }

    /// Mode 2 (OAM scan) → 3 (drawing) → 0 (HBlank) per line; 1 during VBlank.
    /// Mode 3 really lasts 172-289 dots depending on sprites and scrolling.
    fn update_mode(&mut self) {
        let mode = if self.ly >= VBLANK_LINE {
            1
        } else if self.dot < 80 {
            2
        } else if self.dot < 252 {
            3
        } else {
            0
        };
        self.stat = (self.stat & !0x03) | mode;
    }

    /// Draws line LY into the framebuffer, all at once at the end of the line.
    /// TODO(accuracy): hardware pushes pixels through a FIFO during mode 3, so
    /// register writes in the middle of a line (e.g. SCX) take effect mid-line.
    /// TODO(milestone 3): the window, then sprites.
    fn render_scanline(&mut self) {
        let row = usize::from(self.ly) * SCREEN_WIDTH;
        for x in 0..SCREEN_WIDTH as u8 {
            // LCDC bit 0 off blanks the background (and window) on DMG.
            let shade = if self.lcdc & 0x01 == 0 {
                0
            } else {
                let index = self.bg_color_index(x);
                (self.bgp >> (index * 2)) & 0x03
            };
            let i = (row + usize::from(x)) * 4;
            self.framebuffer[i..i + 4].copy_from_slice(&DMG_PALETTE[usize::from(shade)]);
        }
    }

    /// Color index (0-3, before BGP) of the background pixel at screen column
    /// `x` on line LY. SCX/SCY move the screen over the 256x256 map, wrapping.
    /// https://gbdev.io/pandocs/Scrolling.html
    fn bg_color_index(&self, x: u8) -> u8 {
        let map_x = x.wrapping_add(self.scx);
        let map_y = self.ly.wrapping_add(self.scy);
        // LCDC bit 3: tile map at $9800 or $9C00 (offsets into VRAM).
        let map = if self.lcdc & 0x08 != 0 {
            0x1C00
        } else {
            0x1800
        };
        let tile = self.vram[map + usize::from(map_y / 8) * 32 + usize::from(map_x / 8)];
        self.tile_pixel(tile, map_x % 8, map_y % 8)
    }

    /// Color index of pixel (`col`, `row`) in background/window tile `tile`.
    ///
    /// Each tile row is two bytes: the low bit of every pixel's color, then
    /// the high bit, leftmost pixel in bit 7. LCDC bit 4 picks the addressing:
    /// set, tiles 0-255 start at $8000; clear, tiles are signed (-128..127)
    /// around $9000. https://gbdev.io/pandocs/Tile_Data.html
    fn tile_pixel(&self, tile: u8, col: u8, row: u8) -> u8 {
        let base = if self.lcdc & 0x10 != 0 {
            u16::from(tile) * 16
        } else {
            0x1000u16.wrapping_add_signed(i16::from(tile as i8) * 16)
        };
        let addr = usize::from(base) + usize::from(row) * 2;
        let (lo, hi) = (self.vram[addr], self.vram[addr + 1]);
        let bit = 7 - col;
        (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ly_advances_every_456_dots() {
        let mut p = Ppu::new();
        p.tick(455);
        assert_eq!(p.ly, 0);
        p.tick(1);
        assert_eq!(p.ly, 1);
    }

    #[test]
    fn vblank_fires_once_per_frame_at_line_144() {
        let mut p = Ppu::new();
        let mut vblanks = 0;
        for _ in 0..(crate::CYCLES_PER_FRAME / 4) {
            if p.tick(4) & interrupt::VBLANK != 0 {
                vblanks += 1;
                assert_eq!(p.ly, 144);
            }
        }
        assert_eq!(vblanks, 1);
        assert_eq!(p.ly, 0, "a full frame wraps back to line 0");
    }

    #[test]
    fn stat_reports_mode_and_lyc_match() {
        let mut p = Ppu::new();
        p.lyc = 2;
        p.tick(10);
        assert_eq!(p.stat & 0x03, 2, "OAM scan at the start of a line");
        p.tick(100);
        assert_eq!(p.stat & 0x03, 3, "drawing");
        p.tick(456 * 2 - 110);
        assert_eq!(p.ly, 2);
        assert_ne!(p.stat & 0x04, 0, "LY == LYC flag");
    }

    /// Writes one 8x8 tile (16 bytes) at VRAM address `addr`.
    fn put_tile(p: &mut Ppu, addr: u16, bytes: [u8; 16]) {
        for (i, b) in bytes.into_iter().enumerate() {
            p.write_vram(addr + i as u16, b);
        }
    }

    /// A tile whose every row is `lo`, `hi`.
    fn striped(lo: u8, hi: u8) -> [u8; 16] {
        let mut t = [0; 16];
        for row in t.chunks_mut(2) {
            row.copy_from_slice(&[lo, hi]);
        }
        t
    }

    /// The shade (0-3) drawn at screen (x, y), recovered from the RGBA.
    fn shade_at(p: &Ppu, x: usize, y: usize) -> usize {
        let i = (y * SCREEN_WIDTH + x) * 4;
        let px = &p.framebuffer()[i..i + 4];
        DMG_PALETTE
            .iter()
            .position(|c| c == px)
            .expect("a palette color")
    }

    /// LCD on, BG on, $8000 addressing, $9800 map, identity palette.
    fn bg_ppu() -> Ppu {
        let mut p = Ppu::new();
        p.lcdc = 0x91;
        p.bgp = 0b11_10_01_00;
        p
    }

    #[test]
    fn tile_row_bitplanes_combine_into_color_indexes() {
        // Pan Docs' example row: $3C $7E is 0 2 3 3 3 3 2 0.
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8000, striped(0x3C, 0x7E));
        let row: Vec<u8> = (0..8).map(|col| p.tile_pixel(0, col, 0)).collect();
        assert_eq!(row, [0, 2, 3, 3, 3, 3, 2, 0]);
    }

    #[test]
    fn lcdc_bit_4_selects_tile_addressing() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8000, striped(0xFF, 0x00)); // tile 0, $8000 mode
        put_tile(&mut p, 0x9000, striped(0x00, 0xFF)); // tile 0, $8800 mode
        put_tile(&mut p, 0x8800, striped(0xFF, 0xFF)); // tile $80 in both
        assert_eq!(p.tile_pixel(0x00, 0, 0), 1);
        assert_eq!(p.tile_pixel(0x80, 0, 0), 3);
        p.lcdc &= !0x10;
        assert_eq!(p.tile_pixel(0x00, 0, 0), 2, "signed: tile 0 is at $9000");
        assert_eq!(p.tile_pixel(0x80, 0, 0), 3, "signed: tile -128 is at $8800");
        assert_eq!(p.tile_pixel(0x7F, 0, 0), 0, "tile 127 is at $97F0 (blank)");
    }

    #[test]
    fn lcdc_bit_3_selects_the_tile_map() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8010, striped(0xFF, 0xFF)); // tile 1: all color 3
        p.write_vram(0x9C00, 1);
        p.render_scanline();
        assert_eq!(shade_at(&p, 0, 0), 0, "$9800 map points at blank tile 0");
        p.lcdc |= 0x08;
        p.render_scanline();
        assert_eq!(shade_at(&p, 0, 0), 3, "$9C00 map points at tile 1");
    }

    #[test]
    fn scroll_moves_the_screen_over_the_map_and_wraps() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8010, striped(0xFF, 0xFF));
        // Tile 1 in map column 31, row 31: the bottom-right corner.
        p.write_vram(0x9800 + 31 * 32 + 31, 1);

        p.render_scanline();
        assert_eq!(shade_at(&p, 0, 0), 0);

        // Scroll so screen (0,0) lands on map pixel (248, 248).
        p.scx = 248;
        p.scy = 248;
        p.render_scanline();
        let row: Vec<usize> = (0..10).map(|x| shade_at(&p, x, 0)).collect();
        assert_eq!(row, [3, 3, 3, 3, 3, 3, 3, 3, 0, 0], "wraps to column 0");

        // Fine scrolling: SCX 252 shows only the right half of that tile.
        p.scx = 252;
        p.render_scanline();
        let row: Vec<usize> = (0..6).map(|x| shade_at(&p, x, 0)).collect();
        assert_eq!(row, [3, 3, 3, 3, 0, 0]);
    }

    #[test]
    fn bgp_maps_color_indexes_to_shades() {
        let mut p = bg_ppu();
        // One row with all four colors, left to right: 0 1 2 3 0 1 2 3.
        put_tile(&mut p, 0x8000, striped(0b0101_0101, 0b0011_0011));
        p.render_scanline();
        let row: Vec<usize> = (0..4).map(|x| shade_at(&p, x, 0)).collect();
        assert_eq!(row, [0, 1, 2, 3], "identity palette");

        p.bgp = 0b00_01_10_11; // inverted
        p.render_scanline();
        let row: Vec<usize> = (0..4).map(|x| shade_at(&p, x, 0)).collect();
        assert_eq!(row, [3, 2, 1, 0]);
    }

    #[test]
    fn lcdc_bit_0_off_blanks_the_background() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8000, striped(0xFF, 0xFF));
        p.lcdc &= !0x01;
        p.render_scanline();
        assert_eq!(shade_at(&p, 0, 0), 0);
    }

    #[test]
    fn finished_lines_are_drawn_during_tick() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8010, striped(0xFF, 0xFF));
        p.write_vram(0x9800 + 32, 1); // map row 1 = screen lines 8-15
        p.tick(456 * 9);
        assert_eq!(shade_at(&p, 0, 7), 0);
        assert_eq!(shade_at(&p, 0, 8), 3);
        assert_eq!(shade_at(&p, 8, 8), 0);
    }
}
