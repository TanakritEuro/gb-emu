//! The pixel processing unit.
//!
//! Timing is in place: LY advances every 456 dots, VBlank fires at line 144,
//! and the LYC=LY comparison sets STAT. Each finished line is drawn into the
//! framebuffer: background, window, then sprites.
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
    /// Set once WY == LY at the start of a line; cleared at VBlank.
    wy_triggered: bool,
    /// Which window row the next window line draws; reset at VBlank.
    window_line: u8,
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
            wy_triggered: false,
            window_line: 0,
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
                self.wy_triggered = false;
                self.window_line = 0;
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

    /// Draws line LY into the framebuffer, all at once at the end of the line:
    /// the background, the window over it from WX-7 rightward, then sprites.
    /// TODO(accuracy): hardware pushes pixels through a FIFO during mode 3, so
    /// register writes in the middle of a line (e.g. SCX) take effect mid-line.
    fn render_scanline(&mut self) {
        let (sprites, count) = self.sprites_on_line();
        let sprites = &sprites[..count];
        // The window's "Y condition": once WY == LY at the start of a line, it
        // holds for the rest of the frame. https://gbdev.io/pandocs/Window.html
        if self.ly == self.wy {
            self.wy_triggered = true;
        }
        // LCDC bit 0 off blanks the background and the window on DMG: they
        // count as color 0 (for sprite priority too), shown through BGP.
        let bg_on = self.lcdc & 0x01 != 0;
        let window_on = bg_on && self.lcdc & 0x20 != 0 && self.wy_triggered && self.wx <= 166;
        // Screen column where the window starts. TODO(accuracy): WX 0 also
        // shifts it left by SCX % 8, and WX 166 has a DMG-only glitch.
        let window_x = i16::from(self.wx) - 7;

        let row = usize::from(self.ly) * SCREEN_WIDTH;
        for x in 0..SCREEN_WIDTH as u8 {
            // Background/window color index; a blanked background counts as 0.
            let bg_index = if !bg_on {
                0
            } else if window_on && i16::from(x) >= window_x {
                // The window doesn't scroll: its own map, from its (0,0).
                let wx = (i16::from(x) - window_x) as u8;
                self.map_pixel(self.lcdc & 0x40 != 0, wx, self.window_line)
            } else {
                let map_x = x.wrapping_add(self.scx);
                let map_y = self.ly.wrapping_add(self.scy);
                self.map_pixel(self.lcdc & 0x08 != 0, map_x, map_y)
            };
            // A blanked background shows BGP's color 0 (usually white).
            let mut shade = (self.bgp >> (bg_index * 2)) & 0x03;

            // The winning sprite pixel is picked first; only then does its
            // "BG over OBJ" bit decide whether BG colors 1-3 cover it.
            if let Some((color, attrs)) = self.sprite_pixel(sprites, x) {
                if attrs & 0x80 == 0 || bg_index == 0 {
                    let palette = if attrs & 0x10 != 0 {
                        self.obp1
                    } else {
                        self.obp0
                    };
                    shade = (palette >> (color * 2)) & 0x03;
                }
            }

            let i = (row + usize::from(x)) * 4;
            self.framebuffer[i..i + 4].copy_from_slice(&DMG_PALETTE[usize::from(shade)]);
        }

        // The window's own line counter only advances on lines it was drawn,
        // so hiding it for a few lines doesn't skip any of its rows.
        if window_on {
            self.window_line = self.window_line.wrapping_add(1);
        }
    }

    /// Color index (0-3, before BGP) at pixel (`x`, `y`) of a 256x256 tile map:
    /// $9C00 if `high_map`, else $9800. The background picks its map with LCDC
    /// bit 3 and scrolls over it with SCX/SCY (wrapping); the window uses LCDC
    /// bit 6. https://gbdev.io/pandocs/Scrolling.html
    fn map_pixel(&self, high_map: bool, x: u8, y: u8) -> u8 {
        let map = if high_map { 0x1C00 } else { 0x1800 };
        let tile = self.vram[map + usize::from(y / 8) * 32 + usize::from(x / 8)];
        self.tile_pixel(tile, x % 8, y % 8)
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
        self.tile_data_pixel(usize::from(base), col, row)
    }

    /// Color index of pixel (`col`, `row`) in the tile at VRAM offset `base`.
    fn tile_data_pixel(&self, base: usize, col: u8, row: u8) -> u8 {
        let addr = base + usize::from(row) * 2;
        let (lo, hi) = (self.vram[addr], self.vram[addr + 1]);
        let bit = 7 - col;
        (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1)
    }

    /// Sprite height from LCDC bit 2: 8 or 16 pixels.
    fn sprite_height(&self) -> i16 {
        if self.lcdc & 0x04 != 0 {
            16
        } else {
            8
        }
    }

    /// The sprites on line LY, in drawing-priority order (first wins), and how
    /// many there are. Like the hardware: walk OAM in order, keep the first 10
    /// whose rows cover LY (X doesn't matter, so off-screen ones still use up
    /// slots), then on DMG the smaller X wins, and OAM order breaks ties.
    /// Empty when LCDC bit 1 turns sprites off. https://gbdev.io/pandocs/OAM.html
    fn sprites_on_line(&self) -> ([Sprite; 10], usize) {
        let mut found = [Sprite::default(); 10];
        let mut count = 0;
        if self.lcdc & 0x02 != 0 {
            let height = self.sprite_height();
            for &[y, x, tile, attrs] in self.oam.as_chunks::<4>().0 {
                let top = i16::from(y) - 16;
                if (top..top + height).contains(&i16::from(self.ly)) {
                    found[count] = Sprite { y, x, tile, attrs };
                    count += 1;
                    if count == found.len() {
                        break;
                    }
                }
            }
        }
        // A stable sort, so equal X keeps OAM order.
        found[..count].sort_by_key(|s| s.x);
        (found, count)
    }

    /// The color index (1-3) and attributes of the highest-priority sprite
    /// with a visible pixel at screen column `x`. Color 0 is transparent, so a
    /// lower sprite can show through it.
    fn sprite_pixel(&self, sprites: &[Sprite], x: u8) -> Option<(u8, u8)> {
        let height = self.sprite_height();
        sprites.iter().find_map(|s| {
            let col = i16::from(x) - (i16::from(s.x) - 8);
            if !(0..8).contains(&col) {
                return None;
            }
            let mut row = i16::from(self.ly) - (i16::from(s.y) - 16);
            if s.attrs & 0x40 != 0 {
                row = height - 1 - row; // Y flip, over all 16 rows in 8x16
            }
            let col = if s.attrs & 0x20 != 0 { 7 - col } else { col }; // X flip
                                                                       // Sprites always use $8000 addressing. In 8x16 mode the tile
                                                                       // number's low bit is ignored: top half even, bottom half odd.
            let tile = if height == 16 {
                (s.tile & 0xFE) | u8::from(row >= 8)
            } else {
                s.tile
            };
            let color = self.tile_data_pixel(usize::from(tile) * 16, col as u8, (row % 8) as u8);
            (color != 0).then_some((color, s.attrs))
        })
    }
}

/// One OAM entry: Y+16, X+8, tile number, attributes (bit 7 BG over OBJ,
/// 6 Y flip, 5 X flip, 4 OBP1). https://gbdev.io/pandocs/OAM.html
#[derive(Debug, Clone, Copy, Default)]
struct Sprite {
    y: u8,
    x: u8,
    tile: u8,
    attrs: u8,
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
    fn lcdc_bit_0_off_shows_bgp_color_0() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8000, striped(0xFF, 0xFF));
        p.lcdc &= !0x01;
        p.render_scanline();
        assert_eq!(shade_at(&p, 0, 0), 0, "identity BGP: white");

        // dmg-acid2's hair rows rely on this being BGP's color 0, not white.
        p.bgp = 0b00_00_00_10;
        p.render_scanline();
        assert_eq!(shade_at(&p, 0, 0), 2);
    }

    /// Background all blank (tile 0). Window on, using the $9C00 map, whose
    /// first tile row is tile 1 (color 1) and second is tile 2 (color 2): the
    /// shade on screen says which window row was drawn.
    fn window_ppu(wx: u8, wy: u8) -> Ppu {
        let mut p = bg_ppu();
        p.lcdc |= 0x20 | 0x40;
        p.wx = wx;
        p.wy = wy;
        put_tile(&mut p, 0x8010, striped(0xFF, 0x00));
        put_tile(&mut p, 0x8020, striped(0x00, 0xFF));
        for col in 0..32 {
            p.write_vram(0x9C00 + col, 1);
            p.write_vram(0x9C20 + col, 2);
        }
        p
    }

    /// Runs whole scanlines (each is drawn as it finishes).
    fn lines(p: &mut Ppu, n: u32) {
        p.tick(456 * n);
    }

    #[test]
    fn window_starts_at_wx_minus_7_and_wy() {
        let mut p = window_ppu(27, 16);
        lines(&mut p, 17);
        assert_eq!(shade_at(&p, 30, 15), 0, "above WY");
        assert_eq!(shade_at(&p, 19, 16), 0, "left of WX-7");
        assert_eq!(shade_at(&p, 20, 16), 1, "window row 0");
        assert_eq!(shade_at(&p, 159, 16), 1);
    }

    #[test]
    fn window_does_not_scroll() {
        let mut p = window_ppu(7, 0);
        p.scx = 100;
        p.scy = 100;
        lines(&mut p, 9);
        assert_eq!(shade_at(&p, 0, 0), 1);
        assert_eq!(shade_at(&p, 0, 8), 2, "window row 8, whatever SCY says");
    }

    #[test]
    fn window_line_counter_only_counts_drawn_lines() {
        let mut p = window_ppu(7, 0);
        lines(&mut p, 5); // lines 0-4 draw window rows 0-4
        p.lcdc &= !0x20;
        lines(&mut p, 5); // lines 5-9: window hidden
        assert_eq!(shade_at(&p, 0, 7), 0, "background while hidden");
        p.lcdc |= 0x20;
        lines(&mut p, 4); // lines 10-13 draw window rows 5-8
        assert_eq!(shade_at(&p, 0, 10), 1, "row 5, not row 10");
        assert_eq!(shade_at(&p, 0, 12), 1, "row 7");
        assert_eq!(shade_at(&p, 0, 13), 2, "row 8");
    }

    #[test]
    fn wy_match_is_latched_for_the_rest_of_the_frame() {
        let mut p = window_ppu(7, 5);
        lines(&mut p, 6); // lines 0-5
        assert_eq!(shade_at(&p, 0, 4), 0);
        assert_eq!(shade_at(&p, 0, 5), 1, "LY == WY: window starts");
        p.wy = 200;
        lines(&mut p, 1);
        assert_eq!(shade_at(&p, 0, 6), 1, "still on after WY changes");

        // A WY that LY never reaches means no window this frame.
        let mut p = window_ppu(7, 200);
        lines(&mut p, 144);
        assert!((0..144).all(|y| shade_at(&p, 0, y) == 0));
    }

    #[test]
    fn window_restarts_from_row_0_each_frame() {
        let mut p = window_ppu(7, 0);
        lines(&mut p, 154 + 9); // a whole frame, then lines 0-8 of the next
        assert_eq!(shade_at(&p, 0, 0), 1);
        assert_eq!(shade_at(&p, 0, 8), 2);
    }

    #[test]
    fn window_hidden_by_lcdc_bit_0_or_wx_past_166() {
        let mut p = window_ppu(7, 0);
        p.lcdc &= !0x01;
        lines(&mut p, 1);
        assert_eq!(shade_at(&p, 0, 0), 0, "LCDC bit 0 blanks the window too");

        let mut p = window_ppu(167, 0);
        lines(&mut p, 3);
        p.wx = 7;
        lines(&mut p, 1);
        assert_eq!(shade_at(&p, 0, 2), 0, "off-screen at WX 167");
        assert_eq!(shade_at(&p, 0, 3), 1, "and its rows weren't used up");
        assert_eq!(p.window_line, 1);
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

    /// Background blank, sprites on (8x8), OBP0 identity, OBP1 sends color 1
    /// to shade 3. Tiles 1/2/3 are solid colors 1/2/3; tile 4 is color 0 on
    /// its left half and color 1 on its right half.
    fn sprite_ppu() -> Ppu {
        let mut p = bg_ppu();
        p.lcdc |= 0x02;
        p.obp0 = 0b11_10_01_00;
        p.obp1 = 0b00_00_11_00;
        put_tile(&mut p, 0x8010, striped(0xFF, 0x00));
        put_tile(&mut p, 0x8020, striped(0x00, 0xFF));
        put_tile(&mut p, 0x8030, striped(0xFF, 0xFF));
        put_tile(&mut p, 0x8040, striped(0x0F, 0x00));
        p
    }

    /// OAM entry `i` with *screen* coordinates (the +16/+8 offsets added here).
    fn put_sprite(p: &mut Ppu, i: u16, x: i16, y: i16, tile: u8, attrs: u8) {
        let base = 0xFE00 + i * 4;
        p.write_oam(base, (y + 16) as u8);
        p.write_oam(base + 1, (x + 8) as u8);
        p.write_oam(base + 2, tile);
        p.write_oam(base + 3, attrs);
    }

    fn draw_line(p: &mut Ppu, ly: u8) {
        p.ly = ly;
        p.render_scanline();
    }

    fn shades(p: &Ppu, y: usize, xs: std::ops::Range<usize>) -> Vec<usize> {
        xs.map(|x| shade_at(p, x, y)).collect()
    }

    #[test]
    fn sprite_position_is_offset_by_8_and_16() {
        let mut p = sprite_ppu();
        put_sprite(&mut p, 0, 20, 10, 1, 0);
        draw_line(&mut p, 10);
        assert_eq!(shades(&p, 10, 19..29), [0, 1, 1, 1, 1, 1, 1, 1, 1, 0]);
        draw_line(&mut p, 9);
        assert_eq!(shade_at(&p, 20, 9), 0, "above it");
        draw_line(&mut p, 17);
        assert_eq!(shade_at(&p, 20, 17), 1, "its last row");
        draw_line(&mut p, 18);
        assert_eq!(shade_at(&p, 20, 18), 0, "below it");
    }

    #[test]
    fn sprite_color_0_is_transparent_and_palette_comes_from_obp() {
        let mut p = sprite_ppu();
        for col in 0..32 {
            p.write_vram(0x9800 + col, 2); // background color 2
        }
        put_sprite(&mut p, 0, 0, 0, 4, 0);
        draw_line(&mut p, 0);
        assert_eq!(shades(&p, 0, 0..8), [2, 2, 2, 2, 1, 1, 1, 1], "OBP0");

        put_sprite(&mut p, 0, 0, 0, 4, 0x10);
        draw_line(&mut p, 0);
        assert_eq!(shades(&p, 0, 0..8), [2, 2, 2, 2, 3, 3, 3, 3], "OBP1");
    }

    #[test]
    fn sprites_flip_horizontally_and_vertically() {
        let mut p = sprite_ppu();
        // Tile 5: just its top-left pixel set.
        let mut tile = [0; 16];
        tile[0] = 0x80;
        put_tile(&mut p, 0x8050, tile);

        put_sprite(&mut p, 0, 0, 0, 5, 0);
        draw_line(&mut p, 0);
        assert_eq!((shade_at(&p, 0, 0), shade_at(&p, 7, 0)), (1, 0));

        put_sprite(&mut p, 0, 0, 0, 5, 0x20); // X flip
        draw_line(&mut p, 0);
        assert_eq!((shade_at(&p, 0, 0), shade_at(&p, 7, 0)), (0, 1));

        put_sprite(&mut p, 0, 0, 0, 5, 0x40); // Y flip: now the bottom row
        draw_line(&mut p, 0);
        assert_eq!(shade_at(&p, 0, 0), 0);
        draw_line(&mut p, 7);
        assert_eq!(shade_at(&p, 0, 7), 1);
    }

    #[test]
    fn tall_sprites_ignore_the_low_tile_bit_and_flip_all_16_rows() {
        let mut p = sprite_ppu();
        p.lcdc |= 0x04;
        put_tile(&mut p, 0x8060, striped(0xFF, 0x00)); // tile 6: color 1
        put_tile(&mut p, 0x8070, striped(0x00, 0xFF)); // tile 7: color 2
        put_sprite(&mut p, 0, 0, 0, 7, 0); // odd number: top is still tile 6
        for (ly, want) in [(0, 1), (7, 1), (8, 2), (15, 2), (16, 0)] {
            draw_line(&mut p, ly);
            assert_eq!(shade_at(&p, 0, usize::from(ly)), want, "line {ly}");
        }
        put_sprite(&mut p, 0, 0, 0, 6, 0x40);
        for (ly, want) in [(0, 2), (15, 1)] {
            draw_line(&mut p, ly);
            assert_eq!(shade_at(&p, 0, usize::from(ly)), want, "flipped, line {ly}");
        }
    }

    #[test]
    fn lcdc_bit_1_turns_sprites_off() {
        let mut p = sprite_ppu();
        p.lcdc &= !0x02;
        put_sprite(&mut p, 0, 0, 0, 3, 0);
        draw_line(&mut p, 0);
        assert_eq!(shade_at(&p, 0, 0), 0);
    }

    #[test]
    fn only_the_first_10_sprites_on_a_line_count_even_off_screen_ones() {
        let mut p = sprite_ppu();
        for i in 0..9 {
            put_sprite(&mut p, i, -8, 0, 3, 0); // X = 0: off-screen, still counted
        }
        put_sprite(&mut p, 9, 50, 0, 3, 0); // the 10th: drawn
        put_sprite(&mut p, 10, 90, 0, 3, 0); // the 11th: dropped
        draw_line(&mut p, 0);
        assert_eq!(shade_at(&p, 50, 0), 3);
        assert_eq!(shade_at(&p, 90, 0), 0);
    }

    #[test]
    fn overlapping_sprites_smaller_x_then_oam_order_wins() {
        let mut p = sprite_ppu();
        put_sprite(&mut p, 0, 10, 0, 1, 0);
        put_sprite(&mut p, 1, 6, 0, 3, 0); // later in OAM but further left
        draw_line(&mut p, 0);
        assert_eq!(shades(&p, 0, 10..16), [3, 3, 3, 3, 1, 1]);

        let mut p = sprite_ppu();
        put_sprite(&mut p, 0, 10, 0, 1, 0);
        put_sprite(&mut p, 1, 10, 0, 3, 0); // same X: OAM order decides
        draw_line(&mut p, 0);
        assert_eq!(shade_at(&p, 10, 0), 1);
    }

    #[test]
    fn a_winning_sprites_transparent_pixel_shows_the_one_below() {
        let mut p = sprite_ppu();
        put_sprite(&mut p, 0, 6, 0, 4, 0); // color 0 at 6-9, color 1 at 10-13
        put_sprite(&mut p, 1, 8, 0, 3, 0); // color 3 at 8-15
        draw_line(&mut p, 0);
        assert_eq!(shades(&p, 0, 6..16), [0, 0, 3, 3, 1, 1, 1, 1, 3, 3]);
    }

    #[test]
    fn bg_over_obj_uses_the_bg_color_index_not_its_shade() {
        let mut p = sprite_ppu();
        p.bgp = 0b11_10_00_00; // BG color 1 looks white (shade 0)
        p.write_vram(0x9800, 1); // screen x 0-7: BG color 1; 8-15: color 0
        put_sprite(&mut p, 0, 4, 0, 3, 0x80);
        draw_line(&mut p, 0);
        assert_eq!(shades(&p, 0, 4..12), [0, 0, 0, 0, 3, 3, 3, 3]);
    }

    #[test]
    fn a_winning_sprite_with_bg_priority_hides_the_sprite_below_it() {
        let mut p = sprite_ppu();
        for col in 0..32 {
            p.write_vram(0x9800 + col, 1); // BG color 1 everywhere
        }
        put_sprite(&mut p, 0, 0, 0, 3, 0x80); // wins (smaller X), BG over OBJ
        put_sprite(&mut p, 1, 1, 0, 2, 0); // would be visible on its own
        draw_line(&mut p, 0);
        assert_eq!(shade_at(&p, 1, 0), 1, "BG, not the lower sprite");
        assert_eq!(
            shade_at(&p, 8, 0),
            2,
            "past the first sprite: lower one shows"
        );
    }

    #[test]
    fn sprites_still_show_when_lcdc_bit_0_blanks_the_background() {
        let mut p = sprite_ppu();
        for col in 0..32 {
            p.write_vram(0x9800 + col, 1);
        }
        p.lcdc &= !0x01;
        put_sprite(&mut p, 0, 0, 0, 3, 0x80);
        draw_line(&mut p, 0);
        assert_eq!(shade_at(&p, 0, 0), 3, "blank BG counts as color 0");
    }
}
