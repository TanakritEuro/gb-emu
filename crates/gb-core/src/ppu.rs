//! The pixel processing unit.
//!
//! Timing is in place: LY advances every 456 dots, VBlank fires at line 144,
//! and the LYC=LY comparison sets STAT. Drawing pixels is milestone 3; until
//! then the screen stays blank.
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

    /// Milestone 3: draw line `self.ly` into the framebuffer.
    ///
    /// Background first: for each x in 0..160, find the tile under
    /// (x + SCX, LY + SCY) in the tile map LCDC bit 3 selects, read the two
    /// bytes for that tile row (addressing mode from LCDC bit 4), pull out the
    /// 2-bit color index, map it through BGP, write DMG_PALETTE[shade].
    /// Then the window, then sprites.
    fn render_scanline(&mut self) {}
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
}
