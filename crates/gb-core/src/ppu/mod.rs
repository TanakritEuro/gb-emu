//! The pixel processing unit.
//!
//! Each line: mode 2 (the OAM scan), mode 3 (drawing, pixel by pixel through
//! a FIFO: see [`fifo`]), mode 0 (HBlank); lines 144-153 are VBlank. LY,
//! STAT and their interrupts follow it dot by dot.
//!
//! Reference: https://gbdev.io/pandocs/Rendering.html

mod fifo;
mod oam_bug;

use crate::bus::interrupt;
use crate::compat::CompatPalettes;
use crate::state::{StateError, StateReader, StateWriter};
use crate::{Model, SCREEN_HEIGHT, SCREEN_WIDTH};

/// Shades 0 (lightest) to 3 (darkest) as RGBA, in the classic green.
pub const DMG_PALETTE: [[u8; 4]; 4] = [
    [0xE0, 0xF8, 0xD0, 0xFF],
    [0x88, 0xC0, 0x70, 0xFF],
    [0x34, 0x68, 0x56, 0xFF],
    [0x08, 0x18, 0x20, 0xFF],
];

/// Size of [`Ppu::tile_sheet`]: 16 × 24 tiles of 8×8 pixels.
pub const TILE_SHEET_WIDTH: usize = 128;
pub const TILE_SHEET_HEIGHT: usize = 192;

/// RGBA for color `index` (0-3) of `palette` (0-7) in Color palette memory.
fn palette_color(palettes: &[u8; 64], palette: u8, index: u8) -> [u8; 4] {
    let i = usize::from(palette & 7) * 8 + usize::from(index & 3) * 2;
    rgba(u16::from_le_bytes([palettes[i], palettes[i + 1]]))
}

/// RGBA for an RGB555 color (red in bits 0-4, green 5-9, blue 10-14). Each
/// 5-bit level becomes 8 bits as (v << 3) | (v >> 2), so 31 is 255, the
/// conversion cgb-acid2's reference image uses. Real Color screens look
/// paler; TODO(accuracy): an optional color correction.
fn rgba(color: u16) -> [u8; 4] {
    let level = |shift: u16| {
        let v = ((color >> shift) & 0x1F) as u8;
        (v << 3) | (v >> 2)
    };
    [level(0), level(5), level(10), 0xFF]
}

/// The RGB555 color `rgba` made a pixel (the top 5 bits of each channel).
/// Moves a palette index (BCPS/OCPS) on to the next byte if its bit 7 asks
/// for that, wrapping within the 64 bytes.
fn step_palette_index(index: &mut u8) {
    if *index & 0x80 != 0 {
        *index = 0x80 | ((*index + 1) & 0x3F);
    }
}

fn rgb555(px: &[u8; 4]) -> u16 {
    u16::from(px[0] >> 3) | (u16::from(px[1] >> 3) << 5) | (u16::from(px[2] >> 3) << 10)
}

const DOTS_PER_LINE: u32 = 456;
/// Where mode 3 (drawing) starts on lines 0-143, after the OAM scan.
const MODE3_DOT: u32 = 80;
/// The earliest mode 3 ends and mode 0 (HBlank) begins: 172 dots of drawing.
/// A line ends it when its FIFO has drawn 160 pixels (see [`fifo`]).
const HBLANK_DOT: u32 = MODE3_DOT + 172;
const LINES_PER_FRAME: u8 = 154;
const VBLANK_LINE: u8 = 144;
/// The frame's last line, where LY reads 153 only briefly (see
/// [`Ppu::ly_reg`]).
const LAST_LINE: u8 = 153;
/// On line 153, LY == LYC compares with 153 for its first 4 dots, with
/// nothing for the next 4, then with 0 (into line 0).
const LYC_153_UNTIL: u32 = 4;
const LYC_0_FROM: u32 = 8;

#[derive(Clone)]
pub struct Ppu {
    model: Model,
    /// A Color in compatibility mode. See [`Ppu::enter_compat_mode`].
    compat: bool,
    /// 8 KiB on the original; two 8 KiB banks on the Color, bank 1 at
    /// `0x2000..`. Bank 1 holds more tiles and, behind each tile map entry,
    /// that tile's attributes.
    vram: Box<[u8; 0x4000]>,
    /// The bank the CPU sees at $8000-$9FFF (VBK, $FF4F). Color only.
    vram_bank: u8,
    /// Color palette memory: 8 palettes x 4 colors x 2 bytes (RGB555, low
    /// byte first), for the background and for sprites. Color only.
    /// https://gbdev.io/pandocs/Palettes.html#lcd-color-palettes-cgb-only
    bg_palettes: [u8; 64],
    obj_palettes: [u8; 64],
    /// BCPS / OCPS ($FF68 / $FF6A): bits 0-5 pick the palette byte that
    /// BCPD / OCPD ($FF69 / $FF6B) reach; with bit 7 set, each write to the
    /// data register moves on to the next byte.
    bcps: u8,
    ocps: u8,
    /// OPRI ($FF6C) bit 0: 0 = overlapping sprites go by OAM order (the
    /// Color's way, what the boot ROM picks for Color games), 1 = by X
    /// first, like the original. Color only.
    /// https://gbdev.io/pandocs/CGB_Registers.html#ff6c--opri-cgb-mode-only-object-priority-mode
    opri: u8,
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
    pub(crate) dot: u32,
    /// Set once WY == LY at the start of a line; cleared at VBlank.
    wy_triggered: bool,
    /// The dot where this line's mode 3 ends and HBlank begins.
    hblank_dot: u32,
    /// The LCD was just switched on and line 0 hasn't finished: a line with
    /// no OAM scan. See [`Ppu::turn_on`].
    first_line: bool,
    /// Dots until WY == LY is looked at again after a WY write.
    wy_check_in: u32,
    /// An OAM DMA is starting or copying: the OAM scan can't read OAM (the
    /// bus keeps this up to date).
    pub(crate) oam_dma_busy: bool,
    /// While it copies, the OAM byte it writes next.
    pub(crate) oam_dma_dest: Option<u8>,
    /// The window's own line counter: the row it last drew, $FF before
    /// its first line of the frame. It counts only lines it shows on, so
    /// hiding it for a while doesn't skip any of its rows.
    window_y: u8,
    /// Mode 3's fetcher and FIFOs, mid-line.
    m3: fifo::Mode3,
    /// The Color's CPU runs at double speed (the bus keeps this up to
    /// date): some edges fall differently against it.
    pub(crate) double_speed: bool,
    /// The STAT interrupt line as of the last check, for edge detection.
    stat_line: bool,
    /// IF bits raised by register writes, handed over on the next `tick`.
    pending_irq: u8,
    /// HBlanks (of lines 0-143) begun since the bus last asked: the Color's
    /// HBlank DMA copies a block in each.
    hblanks: u32,
    /// Dots until the HBlank just begun counts for HBlank DMA: it's asked
    /// for a little after STAT reads mode 0 (SameBoy has it 3 dots after its
    /// mode 0, which starts a dot before ours; 2 in double speed). Gambatte's
    /// hdma_start tests pin 2 here, 1 in double speed.
    hblank_in: u8,
    /// RGBA, allocated once and never replaced: frontends may keep a pointer
    /// to it (the browser draws straight from wasm memory).
    framebuffer: Box<[u8]>,
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new()
    }
}

impl Ppu {
    /// Register values as the boot ROM leaves them.
    pub fn new() -> Self {
        Self::with_model(Model::Dmg)
    }

    /// As the boot ROM leaves it when the game starts at $0100. The original
    /// is near the end of VBlank, at line 153's dot 395 (LY already reads 0):
    /// gbmicrotest's poweron tests read STAT, LY, OAM and VRAM at set
    /// M-cycles from boot, and its first mode 2 comes 64 dots later. Within
    /// the M-cycle the phase is pinned by Mooneye's tests that run from boot
    /// without switching the LCD off (hblank_ly_scx_timing, di_timing,
    /// halt_ime1_timing2, intr_1_2_timing). The Color's boot ROM hands over
    /// just after VBlank begins, at line 144's dot 163: Gambatte's
    /// display_startstate tests read LY and STAT from there, and the dot
    /// keeps the phase AGE's and Mooneye's Color tests pin.
    pub fn post_boot(model: Model) -> Self {
        let mut p = Self::with_model(model);
        if model == Model::Dmg {
            p.ly = LAST_LINE;
            p.dot = 395;
        } else {
            p.ly = VBLANK_LINE;
            p.dot = 163;
        }
        p.update_stat_bits();
        p
    }

    /// What the original's boot ROM leaves in VRAM after showing the logo:
    /// the cartridge's Nintendo logo (header bytes $0104-$0133) as tiles 1
    /// to 24, the ® as tile $19, and the tile map that placed them. Games
    /// and test ROMs reuse them (Mealybug Tearoom's draw the ® from tile
    /// $19). Each logo byte becomes 4 rows of a tile: its high nibble, then
    /// its low nibble, each bit doubled across and each row twice down, in
    /// one bitplane (color 1).
    /// https://gbdev.io/pandocs/Power_Up_Sequence.html#logo-check
    pub fn leave_boot_logo(&mut self, logo: &[u8; 48]) {
        let double =
            |nibble: u8| (0..4).fold(0u8, |acc, i| acc | (((nibble >> i) & 1) * 3) << (i * 2));
        let mut addr = 0x0010;
        for &byte in logo {
            for nibble in [byte >> 4, byte & 0x0F] {
                for _ in 0..2 {
                    self.vram[addr] = double(nibble);
                    addr += 2;
                }
            }
        }
        self.leave_trademark();
        // The map: $9904-$990F tiles 1-12, $9924-$992F 13-24, the ® at $9910.
        for i in 0..12u8 {
            self.vram[0x1904 + usize::from(i)] = 1 + i;
            self.vram[0x1924 + usize::from(i)] = 13 + i;
        }
        self.vram[0x1910] = 0x19;
    }

    /// The ® the boot ROM leaves as tile $19 (one bitplane: color 1).
    /// TODO(accuracy): the Color's boot ROM shows its own logo, and only the
    /// ® (which Mealybug Tearoom's Color pictures show) is left here when it
    /// runs an original cartridge; what else it leaves in VRAM isn't.
    pub fn leave_trademark(&mut self) {
        const TRADEMARK: [u8; 8] = [0x3C, 0x42, 0xB9, 0xA5, 0xB9, 0xA5, 0x42, 0x3C];
        for (row, &bits) in TRADEMARK.iter().enumerate() {
            self.vram[0x0190 + row * 2] = bits;
        }
    }

    pub fn with_model(model: Model) -> Self {
        Self {
            model,
            compat: false,
            vram: Box::new([0; 0x4000]),
            vram_bank: 0,
            // The boot ROM makes every background color white ($7FFF) and
            // leaves the sprite colors unset.
            bg_palettes: [0xFF, 0x7F].repeat(32).try_into().unwrap_or([0xFF; 64]),
            obj_palettes: [0; 64],
            bcps: 0,
            ocps: 0,
            opri: 0,
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
            hblank_dot: HBLANK_DOT,
            first_line: false,
            wy_check_in: 0,
            oam_dma_busy: false,
            oam_dma_dest: None,
            window_y: 0xFF,
            m3: fifo::Mode3::default(),
            double_speed: false,
            stat_line: false,
            pending_irq: 0,
            hblanks: 0,
            hblank_in: 0,
            framebuffer: DMG_PALETTE[0]
                .repeat(SCREEN_WIDTH * SCREEN_HEIGHT)
                .into_boxed_slice(),
        }
    }

    pub fn framebuffer(&self) -> &[u8] {
        &self.framebuffer
    }

    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.tag(b"PPU ");
        w.sized_bytes(&self.vram[..self.vram_size()]);
        w.u8(self.vram_bank);
        w.bytes(&self.oam);
        w.bytes(&[
            self.lcdc, self.stat, self.scy, self.scx, self.ly, self.lyc, self.dma, self.bgp,
            self.obp0, self.obp1, self.wy, self.wx,
        ]);
        w.u32(self.dot);
        w.bool(self.wy_triggered);
        w.u8(self.wy_check_in as u8);
        w.u8(self.hblank_in);
        w.u16(self.hblank_dot as u16);
        w.bool(self.first_line);
        w.u8(self.window_y);
        w.bool(self.stat_line);
        w.u8(self.pending_irq);
        w.bytes(&self.bg_palettes);
        w.bytes(&self.obj_palettes);
        w.bytes(&[self.bcps, self.ocps, self.opri]);
        self.save_mode3(w);
        // The picture, so a loaded state shows its own frame straight away.
        // On the original every pixel is one of four shades: 2 bits each, 4
        // per byte. On the Color it's any RGB555 color: 2 bytes each.
        if self.model == Model::Cgb {
            let mut colors = Vec::with_capacity(SCREEN_WIDTH * SCREEN_HEIGHT * 2);
            for px in self.framebuffer.as_chunks::<4>().0 {
                colors.extend_from_slice(&rgb555(px).to_le_bytes());
            }
            w.bytes(&colors);
        } else {
            let mut packed = vec![0u8; SCREEN_WIDTH * SCREEN_HEIGHT / 4];
            for (i, px) in self.framebuffer.as_chunks::<4>().0.iter().enumerate() {
                let shade = DMG_PALETTE.iter().position(|c| c == px).unwrap_or(0) as u8;
                packed[i / 4] |= shade << ((i % 4) * 2);
            }
            w.bytes(&packed);
        }
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"PPU ")?;
        let size = self.vram_size();
        r.sized_bytes(&mut self.vram[..size])?;
        self.vram_bank = r.u8()? & 1;
        r.bytes(&mut self.oam)?;
        let mut regs = [0; 12];
        r.bytes(&mut regs)?;
        [
            self.lcdc, self.stat, self.scy, self.scx, self.ly, self.lyc, self.dma, self.bgp,
            self.obp0, self.obp1, self.wy, self.wx,
        ] = regs;
        self.dot = r.u32()?;
        if self.dot >= DOTS_PER_LINE || self.ly >= LINES_PER_FRAME {
            return Err(StateError::Corrupt("PPU position"));
        }
        self.wy_triggered = r.bool()?;
        self.wy_check_in = u32::from(r.u8()?.min(6));
        self.hblank_in = r.u8()?.min(2);
        self.hblank_dot = u32::from(r.u16()?);
        if !(HBLANK_DOT..=DOTS_PER_LINE).contains(&self.hblank_dot) {
            return Err(StateError::Corrupt("PPU mode 3 length"));
        }
        self.first_line = r.bool()?;
        self.window_y = r.u8()?;
        self.stat_line = r.bool()?;
        self.pending_irq = r.u8()?;
        r.bytes(&mut self.bg_palettes)?;
        r.bytes(&mut self.obj_palettes)?;
        self.bcps = r.u8()?;
        self.ocps = r.u8()?;
        self.opri = r.u8()? & 1;
        self.load_mode3(r)?;
        let pixels = self.framebuffer.as_chunks_mut::<4>().0;
        if self.model == Model::Cgb {
            let mut colors = vec![0u8; SCREEN_WIDTH * SCREEN_HEIGHT * 2];
            r.bytes(&mut colors)?;
            for (px, c) in pixels.iter_mut().zip(colors.as_chunks::<2>().0) {
                *px = rgba(u16::from_le_bytes(*c));
            }
        } else {
            let mut packed = vec![0u8; SCREEN_WIDTH * SCREEN_HEIGHT / 4];
            r.bytes(&mut packed)?;
            for (i, px) in pixels.iter_mut().enumerate() {
                *px = DMG_PALETTE[usize::from((packed[i / 4] >> ((i % 4) * 2)) & 3)];
            }
        }
        Ok(())
    }

    /// Takes over `old`'s framebuffer allocation (copying this one's picture
    /// into it), so the screen stays at the address frontends point at when
    /// a loaded state replaces the PPU.
    pub(crate) fn keep_framebuffer_of(&mut self, old: &mut Ppu) {
        old.framebuffer.copy_from_slice(&self.framebuffer);
        std::mem::swap(&mut self.framebuffer, &mut old.framebuffer);
    }

    /// The Color's own features are on: tile attributes, its palettes picked
    /// per tile and sprite, VRAM bank 1. Not on the original, nor on a Color
    /// in compatibility mode.
    fn cgb(&self) -> bool {
        self.model == Model::Cgb && !self.compat
    }

    /// A Color running an original cartridge, as its boot ROM leaves it:
    /// `palettes` loaded into background palette 0 and sprite palettes 0
    /// and 1, sprites overlapping by X as on the original (OPRI), and from
    /// then on BGP, OBP0 and OBP1 picking their shades from those colors.
    /// See [`crate::compat`].
    pub fn enter_compat_mode(&mut self, palettes: CompatPalettes) {
        self.compat = true;
        self.opri = 1;
        self.set_compat_palettes(palettes);
        // Where the boot ROM's auto-incrementing writes left the indexes:
        // 8 bytes of background colors, 16 of sprite colors.
        self.bcps = 0x88;
        self.ocps = 0x90;
    }

    /// Puts `palettes` where compatibility mode takes its colors from:
    /// background palette 0 and sprite palettes 0 and 1. The game can't
    /// reach palette memory in that mode, so only the host changes these.
    pub fn set_compat_palettes(&mut self, palettes: CompatPalettes) {
        let put = |ram: &mut [u8; 64], palette: usize, colors: [u16; 4]| {
            for (i, c) in colors.into_iter().enumerate() {
                ram[palette * 8 + i * 2..][..2].copy_from_slice(&c.to_le_bytes());
            }
        };
        put(&mut self.bg_palettes, 0, palettes.bg);
        put(&mut self.obj_palettes, 0, palettes.obj[0]);
        put(&mut self.obj_palettes, 1, palettes.obj[1]);
    }

    /// The Color's palette registers, $FF68-$FF6B, and OPRI, $FF6C. Unused
    /// bit 6 of the index registers reads 1, as do OPRI's bits 1-7. Reading the data never moves the index.
    /// During mode 3 the CPU can't reach palette memory: see the bus's
    /// `cpu_read`/`cpu_write`.
    pub fn read_color_reg(&self, addr: u16) -> u8 {
        match addr {
            0xFF6C => 0xFE | self.opri,
            0xFF68 => self.bcps | 0x40,
            0xFF69 => self.bg_palettes[usize::from(self.bcps & 0x3F)],
            0xFF6A => self.ocps | 0x40,
            _ => self.obj_palettes[usize::from(self.ocps & 0x3F)],
        }
    }

    pub fn write_color_reg(&mut self, addr: u16, val: u8) {
        /// Writes the byte `index` picks, then moves it on if it auto-increments.
        fn write(palettes: &mut [u8; 64], index: &mut u8, val: u8) {
            palettes[usize::from(*index & 0x3F)] = val;
            step_palette_index(index);
        }
        match addr {
            0xFF6C => self.opri = val & 1,
            0xFF68 => self.bcps = val & 0xBF,
            0xFF69 => write(&mut self.bg_palettes, &mut self.bcps, val),
            0xFF6A => self.ocps = val & 0xBF,
            _ => write(&mut self.obj_palettes, &mut self.ocps, val),
        }
    }

    /// A write to BCPD/OCPD ($FF69/$FF6B) that mode 3 kept out of palette
    /// memory: the byte is lost, but the index still moves on.
    pub fn lost_palette_write(&mut self, addr: u16) {
        step_palette_index(if addr == 0xFF69 {
            &mut self.bcps
        } else {
            &mut self.ocps
        });
    }

    /// Whether the PPU holds OAM, so a CPU access (a write if `write`) gets
    /// no further. It reads OAM in modes 2 and 3: the OAM scan, then the
    /// sprites it draws.
    ///
    /// The edges depend on the access: a read samples later in its M-cycle
    /// than a write lands, so reads meet the hold 4 dots sooner. They find
    /// OAM taken from 4 dots before a line begins (as LY moves on) through
    /// mode 3; writes find it taken in mode 2 except its last 4 dots, and in
    /// mode 3 (and on the Color, from 4 dots before the line too, as in
    /// SameBoy). The first line after the LCD comes on has no OAM scan.
    /// (Mooneye's lcdon_timing and lcdon_write_timing, and
    /// intr_2_oam_ok_timing for where the hold ends.)
    pub fn oam_locked(&self, write: bool) -> bool {
        if !self.lcd_on() {
            return false;
        }
        let next_scans = self.ly + 1 < VBLANK_LINE || self.ly == LINES_PER_FRAME - 1;
        if self.ly >= VBLANK_LINE || self.dot >= self.hblank_dot {
            // Reads meet the next line's OAM scan before it starts, and on
            // the Color writes do too (AGE's oam-write-cgbBCE). In double
            // speed reads don't, and writes only in the line's last 2 dots
            // (AGE's oam-read and oam-write; SameBoy).
            let from = match (write, self.double_speed) {
                (false, false) => DOTS_PER_LINE - 4,
                (true, false) if self.model == Model::Cgb => DOTS_PER_LINE - 4,
                (true, true) => DOTS_PER_LINE - 2,
                _ => return false,
            };
            return next_scans && self.dot >= from;
        }
        if self.first_line {
            self.dot >= if write { MODE3_DOT } else { self.mode3_start() }
        } else {
            !write || !(MODE3_DOT - 4..MODE3_DOT).contains(&self.dot)
        }
    }

    /// Whether the PPU holds VRAM (and on the Color, palette memory): in
    /// mode 3, where it fetches tiles. On the original, reads find it taken
    /// from 4 dots before mode 3, except on the first line after the LCD
    /// comes on; on the Color not until mode 3 (AGE's vram-read, and
    /// SameBoy), and on that first line not until 5 dots into it. See
    /// [`Ppu::oam_locked`].
    pub fn vram_locked(&self, write: bool) -> bool {
        self.mode3_locked(write, 5)
    }

    /// Whether the PPU holds the Color's palette memory: as VRAM, except
    /// that on the first line after switching on it takes it 2 dots into
    /// mode 3 (SameBoy).
    pub fn palettes_locked(&self, write: bool) -> bool {
        self.mode3_locked(write, 2)
    }

    /// Held in mode 3 (see [`Ppu::vram_locked`]); on the Color's first
    /// line after switching on, from `first_line_delay` dots into it.
    fn mode3_locked(&self, write: bool, first_line_delay: u32) -> bool {
        let from = match (self.model, self.first_line) {
            (Model::Cgb, true) => {
                if self.double_speed {
                    self.mode3_start()
                } else {
                    MODE3_DOT + first_line_delay
                }
            }
            (Model::Cgb, false) => MODE3_DOT,
            (Model::Dmg, true) => MODE3_DOT,
            (Model::Dmg, false) if write => MODE3_DOT,
            (Model::Dmg, false) => MODE3_DOT - 4,
        };
        self.lcd_on() && self.ly < VBLANK_LINE && (from..self.hblank_dot).contains(&self.dot)
    }

    /// How many HBlanks began since the last call.
    pub fn take_hblanks(&mut self) -> u32 {
        std::mem::take(&mut self.hblanks)
    }

    /// The VRAM bank the CPU sees, 0 or 1 (always 0 on the original).
    pub fn vram_bank(&self) -> u8 {
        self.vram_bank
    }
    pub fn set_vram_bank(&mut self, bank: u8) {
        self.vram_bank = bank & 1;
    }

    /// The part of `vram` this model has.
    fn vram_size(&self) -> usize {
        if self.model == Model::Cgb {
            0x4000
        } else {
            0x2000
        }
    }

    pub fn read_vram(&self, addr: u16) -> u8 {
        self.vram[usize::from(self.vram_bank) * 0x2000 + usize::from(addr - 0x8000)]
    }
    pub fn write_vram(&mut self, addr: u16, val: u8) {
        self.vram[usize::from(self.vram_bank) * 0x2000 + usize::from(addr - 0x8000)] = val;
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
            0xFF44 => self.ly_reg(),
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

    /// Where this line's mode 3 shows: dot 80, except on the first line
    /// after switching on in double speed, 2 dots later (AGE's
    /// stat-mode-ds, oam-read and vram-read).
    fn mode3_start(&self) -> u32 {
        if self.first_line && self.double_speed {
            MODE3_DOT + 2
        } else {
            MODE3_DOT
        }
    }

    /// What LY ($FF44) reads: the line, except that it moves on to the next
    /// one 2 dots before this one ends. (Mooneye's hblank_ly_scx_timing
    /// times it from the HBlank interrupt; in normal speed the CPU only
    /// sees it to the M-cycle, so it's AGE's double-speed ly and its
    /// spsw-mode0, which shifts the CPU against the LCD, that pin it to the
    /// dot.) And line 153 reads 0 from its first dot, so 153 shows only in
    /// line 152's last 2 dots: the frame turns over a line early as far as
    /// LY goes (Wilbert Pol's ly_lyc_153 and ly_new_frame, AGE's ly). That's
    /// the original and CPU CGB C and earlier, in normal speed; in double
    /// speed, and on later Colors, 153 shows for 4 dots more.
    /// https://gbdev.io/pandocs/STAT.html#ff44--ly-lcd-y-coordinate-read-only
    fn ly_reg(&self) -> u8 {
        let zero_from = if self.double_speed { 4 } else { 0 };
        if self.ly == LAST_LINE && self.dot >= zero_from {
            0
        } else if self.dot >= DOTS_PER_LINE - 2 {
            (self.ly + 1) % LINES_PER_FRAME
        } else {
            self.ly
        }
    }

    pub fn write_reg(&mut self, addr: u16, val: u8) {
        match addr {
            0xFF40 => {
                let was_on = self.lcd_on();
                if self.lcdc & !val & 0x02 != 0 {
                    self.sprites_turned_off();
                }
                if self.lcdc & !val & 0x20 != 0 {
                    self.window_turned_off();
                }
                if (self.lcdc ^ val) & 0x10 != 0 {
                    self.tile_sel_changed(val & 0x10 != 0);
                }
                self.lcdc = val;
                if was_on && !self.lcd_on() {
                    self.turn_off();
                } else if !was_on && self.lcd_on() {
                    self.turn_on();
                }
            }
            // Bits 0-2 (mode, LYC flag) are read-only. A new enable mask can
            // raise the STAT line right away. (The original's STAT write bug,
            // $FF for a dot first, is in how the CPU's write lands:
            // `bus::WriteTiming`.)
            0xFF41 => {
                self.stat = (self.stat & 0x07) | (val & 0x78);
                self.check_stat_line();
            }
            0xFF42 => self.scy = val,
            0xFF43 => self.scx = val,
            0xFF44 => {} // LY is read-only
            // LY == LYC is compared constantly, so a new LYC counts at once.
            0xFF45 => {
                self.lyc = val;
                self.update_stat_bits();
                self.check_stat_line();
            }
            0xFF46 => self.dma = val,
            0xFF47 => self.bgp = val,
            0xFF48 => self.obp0 = val,
            0xFF49 => self.obp1 = val,
            0xFF4A => {
                // WY == LY is looked at again a few dots after a write, so a
                // write mid-line can still start the window on this line if
                // the fetcher hasn't passed WX (Gambatte's window/late_wy
                // tests; SameBoy schedules the check on a 4-dot grid).
                self.wy = val;
                self.wy_check_in = match (self.model, self.double_speed) {
                    (Model::Dmg, _) => 3,
                    (Model::Cgb, false) => 6,
                    (Model::Cgb, true) => 3,
                };
            }
            0xFF4B => {
                self.wx = val;
                self.wx_written();
            }
            _ => {}
        }
    }

    pub(crate) fn lcd_on(&self) -> bool {
        self.lcdc & 0x80 != 0
    }

    /// LCDC bit 7 cleared: the PPU stops, LY goes back to 0, STAT reports
    /// mode 0, and the screen goes blank (white on DMG). The LY == LYC flag
    /// and the STAT interrupt line keep their last values while it's off:
    /// the comparison only runs with the PPU (Mooneye's stat_lyc_onoff).
    fn turn_off(&mut self) {
        self.ly = 0;
        self.dot = 0;
        self.stat &= !0x03;
        self.m3 = fifo::Mode3::default(); // a line cut short
        self.wy_triggered = false;
        self.window_y = 0xFF;
        // Blank it in place: the buffer's address must not change.
        for px in self.framebuffer.as_chunks_mut::<4>().0 {
            *px = DMG_PALETTE[0];
        }
    }

    /// LCDC bit 7 set: the PPU starts over from the top of line 0, comparing
    /// LY with LYC straight away (an interrupt if that turns the flag on).
    /// That first line has no OAM scan: it starts in mode 0 and goes
    /// straight to mode 3 at dot 80 (Mooneye's lcdon_timing). It's also
    /// short: it starts at dot 2 on the original, pinned by Mooneye, which
    /// reads in whole M-cycles (intr_2_mode0_timing_sprites needs the later
    /// lines 2 dots earlier, and lcdon_timing and lcdon_write_timing fail
    /// starting at dot 1 or 3). The Color starts a dot later still (SameBoy
    /// has only the original wait a dot after switching on), which Mealybug
    /// Tearoom's Color pictures need.
    /// TODO(accuracy): the first frame after turning it back on stays blank.
    /// The Color's first line differs in other ways too (lcdon_timing fails
    /// on it); otherwise it's treated like the original's here.
    fn turn_on(&mut self) {
        self.dot = match (self.model, self.double_speed) {
            (Model::Dmg, _) => 2,
            (Model::Cgb, false) => 3,
            // In double speed a dot behind (AGE's ly, stat-mode-ds, stat-int).
            (Model::Cgb, true) => 2,
        };
        self.first_line = true;
        self.hblank_dot = HBLANK_DOT;
        self.update_stat_bits();
        self.check_stat_line();
    }

    /// Advances by `cycles` dots, one at a time, so mode and LY == LYC change
    /// exactly when they should. Returns IF bits to request.
    pub fn tick(&mut self, cycles: u32) -> u8 {
        let mut irq = std::mem::take(&mut self.pending_irq);
        if !self.lcd_on() {
            self.end_dot();
            return irq;
        }
        for _ in 0..cycles {
            if self.wy_check_in > 0 {
                self.wy_check_in -= 1;
                if self.wy_check_in == 0
                    && self.ly < VBLANK_LINE
                    && self.ly == self.wy
                    && self.lcdc & 0x20 != 0
                {
                    self.wy_triggered = true;
                }
            }
            self.dot += 1;
            if self.hblank_in > 0 {
                self.hblank_in -= 1;
                if self.hblank_in == 0 {
                    self.hblanks += 1;
                }
            }
            self.oam_scan_dot();
            // Mode 3 draws the line pixel by pixel and lasts as long as that
            // takes; HBlank (and a Color's HBlank DMA block) follows.
            if self.ly < VBLANK_LINE {
                if self.dot == MODE3_DOT {
                    self.hblank_dot = DOTS_PER_LINE; // until the line is done
                    self.start_mode3();
                } else if self.mode3_dot() {
                    self.hblank_dot = self.dot;
                    self.hblank_in = if self.double_speed { 1 } else { 2 };
                }
            }
            self.end_dot();
            if self.dot == DOTS_PER_LINE {
                self.dot = 0;
                self.ly = (self.ly + 1) % LINES_PER_FRAME;
                self.first_line = false;
                self.oam_scan_dot();
                if self.ly == VBLANK_LINE {
                    irq |= interrupt::VBLANK;
                    self.wy_triggered = false;
                    self.window_y = 0xFF;
                }
            }
            self.update_stat_bits();
            self.check_stat_line();
        }
        irq | std::mem::take(&mut self.pending_irq)
    }

    /// Mode 2 reads an object's Y and X every 2 dots: on the original as
    /// each pair of dots ends (object i at dot 2i + 2), on the Color as it
    /// begins (dot 2i), as SameBoy has them.
    fn oam_scan_dot(&mut self) {
        if self.ly >= VBLANK_LINE || self.first_line || self.dot % 2 == 1 {
            return;
        }
        let offset = if self.model == Model::Dmg { 2 } else { 0 };
        if let Some(index) = self
            .dot
            .checked_sub(offset)
            .map(|d| d / 2)
            .filter(|&i| i < 40)
        {
            self.scan_object(index as u8);
        }
    }

    /// Sets STAT's mode bits and LY == LYC flag. Each line is mode 2 (OAM
    /// scan), 3 (drawing, until the FIFO has put out 160 pixels), then 0 (HBlank);
    /// lines 144-153 are mode 1 (VBlank). On the original, VBlank shows mode
    /// 0 for its last dot (Wilbert Pol's ly_lyc_0-GS; not an HBlank for the
    /// STAT interrupt).
    ///
    /// LY == LYC compares the line, except in a line's last 4 dots, where
    /// LY already reads the next one: on the original the flag reads 0
    /// there, and on the Color it holds what it was (a new LYC isn't
    /// compared until the next line: Wilbert Pol's ly_lyc-C and
    /// ly_lyc_write-C). Line 153 compares as LY reads it: 153 briefly, then
    /// 0 (see [`LYC_153_UNTIL`]).
    fn update_stat_bits(&mut self) {
        let vblank_over = self.ly == LAST_LINE && self.dot == DOTS_PER_LINE - 1;
        let mode = if !self.lcd_on() || vblank_over {
            0
        } else if self.ly >= VBLANK_LINE {
            1
        } else if self.dot < MODE3_DOT || (self.first_line && self.dot < self.mode3_start()) {
            if self.first_line {
                0
            } else {
                2
            }
        } else if self.dot < self.hblank_dot {
            3
        } else {
            0
        };
        // While the LCD is off the flag keeps its value.
        let compared = if self.ly != LAST_LINE {
            (self.dot < DOTS_PER_LINE - 4).then_some(self.ly)
        } else if self.dot < LYC_153_UNTIL {
            Some(LAST_LINE)
        } else if self.dot >= LYC_0_FROM {
            Some(0)
        } else {
            None
        };
        let frozen =
            self.model == Model::Cgb && self.ly != LAST_LINE && self.dot >= DOTS_PER_LINE - 4;
        let coincide = if !self.lcd_on() || frozen {
            self.stat & 0x04
        } else if compared == Some(self.lyc) {
            0x04
        } else {
            0
        };
        self.stat = (self.stat & !0x07) | coincide | mode;
    }

    /// The four STAT sources, each gated by its enable bit (6: LY == LYC,
    /// 5: mode 2, 4: mode 1, 3: mode 0), are OR'd into one line, and the
    /// interrupt fires only on that line's rising edge. So a source turning
    /// on while another already holds the line high fires nothing ("STAT
    /// blocking"). https://gbdev.io/pandocs/Interrupt_Sources.html
    ///
    /// The sources don't quite follow STAT's mode bits, as in SameBoy's PPU
    /// (Core/display.c). HBlank and VBlank hold the line for as long as they
    /// last, but the mode 2 source only pulses, for one dot as mode 2 is
    /// about to begin: a dot before STAT shows it (except into line 0). So
    /// enabling it, or the original's STAT write bug, during mode 2 fires
    /// nothing (Wilbert Pol's stat_write_if). It also pulses once for line
    /// 144, though that has no OAM scan: a dot before VBlank on the original
    /// (Wilbert Pol's intr_2_timing), an M-cycle before on the Color
    /// (Mooneye's vblank_stat_intr-C). The HBlank source comes on a dot
    /// after STAT shows mode 0, and not for the mode 0 that the first line
    /// after switching on starts with (Wilbert Pol's intr_0_timing). Mealybug Tearoom's pictures depend on it (its
    /// tests sync to lines with the mode 2 interrupt while running), and so
    /// does Mooneye's timing with the CPU halted (see `Cpu::halted_m_cycle`).
    fn check_stat_line(&mut self) {
        if !self.lcd_on() {
            return;
        }
        let mode = self.stat & 0x03;
        // In double speed the pulse comes a dot earlier still (AGE's
        // stat-int).
        let early_dot = if self.double_speed { 454 } else { 455 };
        let dot = match self.dot {
            d if d == early_dot => 455,
            455 => 0xFFFF,
            d => d,
        };
        let mode2_pulse = match (self.ly, dot) {
            (0, 0) => mode == 2, // not on the first line after switching on
            (143, 452) => self.model == Model::Cgb,
            (143, 455) => self.model == Model::Dmg,
            (ly, 455) => ly < VBLANK_LINE - 1,
            _ => false,
        };
        let line = (self.stat & 0x40 != 0 && self.stat & 0x04 != 0)
            || (self.stat & 0x20 != 0 && mode2_pulse)
            || (self.stat & 0x10 != 0 && mode == 1)
            || (self.stat & 0x08 != 0
                && mode == 0
                && self.ly < VBLANK_LINE
                && (self.dot != self.hblank_dot || self.double_speed)
                && !(self.first_line && self.dot < MODE3_DOT));
        if line && !self.stat_line {
            self.pending_irq |= interrupt::STAT;
        }
        self.stat_line = line;
    }

    /// Color index (0-3, before the palette) at pixel (`x`, `y`) of a 256x256
    /// tile map, and that tile's attributes (always 0 on the original).
    /// $9C00 if `high_map`, else $9800. The background picks its map with
    /// LCDC bit 3 and scrolls over it with SCX/SCY (wrapping); the window
    /// uses LCDC bit 6. https://gbdev.io/pandocs/Scrolling.html
    ///
    /// On the Color each map entry has an attribute byte at the same place in
    /// VRAM bank 1: bits 0-2 the palette, bit 3 the VRAM bank of the tile's
    /// pixels, bit 5 X flip, bit 6 Y flip, bit 7 priority over sprites.
    /// https://gbdev.io/pandocs/Tile_Maps.html#bg-map-attributes-cgb-mode-only
    fn map_pixel(&self, high_map: bool, x: u8, y: u8) -> (u8, u8) {
        let map = if high_map { 0x1C00 } else { 0x1800 };
        let entry = map + usize::from(y / 8) * 32 + usize::from(x / 8);
        let tile = self.vram[entry];
        let attrs = if self.cgb() {
            self.vram[0x2000 + entry]
        } else {
            0
        };
        let col = if attrs & 0x20 != 0 { 7 - x % 8 } else { x % 8 };
        let row = if attrs & 0x40 != 0 { 7 - y % 8 } else { y % 8 };
        let bank = (attrs >> 3) & 1;
        (self.tile_pixel(tile, bank, col, row), attrs)
    }

    /// Color index of pixel (`col`, `row`) in background/window tile `tile`
    /// of VRAM bank `bank`.
    ///
    /// Each tile row is two bytes: the low bit of every pixel's color, then
    /// the high bit, leftmost pixel in bit 7. LCDC bit 4 picks the addressing:
    /// set, tiles 0-255 start at $8000; clear, tiles are signed (-128..127)
    /// around $9000. https://gbdev.io/pandocs/Tile_Data.html
    fn tile_pixel(&self, tile: u8, bank: u8, col: u8, row: u8) -> u8 {
        let base = if self.lcdc & 0x10 != 0 {
            u16::from(tile) * 16
        } else {
            0x1000u16.wrapping_add_signed(i16::from(tile as i8) * 16)
        };
        self.tile_data_pixel(usize::from(bank) * 0x2000 + usize::from(base), col, row)
    }

    /// RGBA for sprite color `index` (1-3) in `palette`: on the original 0
    /// is OBP0 and 1 OBP1 (attribute bit 4); on the Color, sprite palettes
    /// 0-7 (attribute bits 0-2). In compatibility mode, OBP0/OBP1's shade
    /// picks a color from sprite palette 0 or 1.
    fn obj_color(&self, palette: u8, index: u8) -> [u8; 4] {
        if self.cgb() {
            return palette_color(&self.obj_palettes, palette, index);
        }
        let obp = if palette == 1 { self.obp1 } else { self.obp0 };
        let shade = (obp >> (index * 2)) & 0x03;
        if self.compat {
            palette_color(&self.obj_palettes, palette, shade)
        } else {
            DMG_PALETTE[usize::from(shade)]
        }
    }

    /// RGBA for background color `index` (0-3): through BGP on the original,
    /// from background palette `palette` (0-7) on the Color. In compatibility
    /// mode, BGP's shade picks a color from background palette 0.
    fn bg_color(&self, palette: u8, index: u8) -> [u8; 4] {
        if self.cgb() {
            return palette_color(&self.bg_palettes, palette, index);
        }
        let shade = (self.bgp >> (index * 2)) & 0x03;
        if self.compat {
            palette_color(&self.bg_palettes, 0, shade)
        } else {
            DMG_PALETTE[usize::from(shade)]
        }
    }

    /// Color index of pixel (`col`, `row`) in the tile at VRAM offset `base`.
    fn tile_data_pixel(&self, base: usize, col: u8, row: u8) -> u8 {
        let addr = base + usize::from(row) * 2;
        let (lo, hi) = (self.vram[addr], self.vram[addr + 1]);
        let bit = 7 - col;
        (((hi >> bit) & 1) << 1) | ((lo >> bit) & 1)
    }

    // Debugger views: pictures of VRAM, read without changing anything.

    /// All 384 tiles at $8000-$97FF of VRAM `bank` (0, or 1 on the Color)
    /// as RGBA, [`TILE_SHEET_WIDTH`] × [`TILE_SHEET_HEIGHT`]: 16 tiles per
    /// row in address order, so tile n of the $8000 block is at column n % 16,
    /// row n / 16. Colors go through BGP, as the background would show them
    /// (sprites use OBP0/OBP1), or on the Color background palette 0.
    /// https://gbdev.io/pandocs/Tile_Data.html
    pub fn tile_sheet(&self, bank: u8) -> Vec<u8> {
        let base = if self.cgb() {
            usize::from(bank & 1) * 0x2000
        } else {
            0
        };
        let mut out = vec![0; TILE_SHEET_WIDTH * TILE_SHEET_HEIGHT * 4];
        for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % TILE_SHEET_WIDTH, i / TILE_SHEET_WIDTH);
            let tile = (y / 8) * 16 + x / 8;
            let index = self.tile_data_pixel(base + tile * 16, (x % 8) as u8, (y % 8) as u8);
            *px = self.bg_color(0, index);
        }
        out
    }

    /// A whole 256×256 tile map as RGBA: $9C00 if `high_map`, else $9800.
    /// Drawn as the background would be: tile numbers read with the
    /// addressing LCDC bit 4 selects, and on the Color each tile's
    /// attributes (bank, flips, palette). https://gbdev.io/pandocs/Tile_Maps.html
    pub fn tile_map_image(&self, high_map: bool) -> Vec<u8> {
        let mut out = vec![0; 256 * 256 * 4];
        for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (index, attrs) = self.map_pixel(high_map, (i % 256) as u8, (i / 256) as u8);
            *px = self.bg_color(attrs & 0x07, index);
        }
        out
    }

    /// All of VRAM, both banks on the Color, for a debugger.
    pub fn vram(&self) -> &[u8] {
        &self.vram[..self.vram_size()]
    }

    /// Sprite height from LCDC bit 2: 8 or 16 pixels.
    fn sprite_height(&self) -> i16 {
        if self.lcdc & 0x04 != 0 {
            16
        } else {
            8
        }
    }
}

/// One OAM entry: Y+16, X+8, tile number, attributes (bit 7 BG over OBJ,
/// 6 Y flip, 5 X flip, 4 OBP1 on the original; on the Color 3 VRAM bank,
/// 0-2 palette). https://gbdev.io/pandocs/OAM.html
#[derive(Debug, Clone, Copy, Default)]
struct Sprite {
    y: u8,
    x: u8,
    tile: u8,
    attrs: u8,
    /// Its place in OAM, 0-39.
    index: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Ppu {
        /// Draws line LY the way the hardware does: from the start of the
        /// line through its OAM scan and mode 3, leaving it at the start of
        /// HBlank.
        fn render_scanline(&mut self) {
            self.dot = 0;
            self.oam_scan_dot(); // the Color reads object 0 at dot 0
            for _ in 0..DOTS_PER_LINE - 1 {
                self.tick(1);
                if self.dot > MODE3_DOT && self.m3.step == fifo::Step::Idle {
                    break;
                }
            }
        }
    }

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

    /// A PPU with STAT interrupt enables `enable`, 100 dots into line 0
    /// (past the line's start, in mode 3).
    fn stat_ppu(enable: u8) -> Ppu {
        let mut p = Ppu::new();
        p.write_reg(0xFF41, enable);
        p.tick(100);
        p
    }

    /// STAT interrupts requested over the next `dots` dots.
    fn count_stat(p: &mut Ppu, dots: u32) -> u32 {
        (0..dots)
            .map(|_| u32::from(p.tick(1) & interrupt::STAT != 0))
            .sum()
    }

    const FRAME: u32 = crate::CYCLES_PER_FRAME;

    #[test]
    fn each_mode_source_fires_on_entry() {
        // 144 visible lines each enter mode 0 and mode 2 once (the mode 2
        // count includes line 0 of the next frame, and on the original line
        // 144's as well); mode 1 once per frame.
        assert_eq!(count_stat(&mut stat_ppu(0x08), FRAME), 144, "HBlank");
        assert_eq!(count_stat(&mut stat_ppu(0x20), FRAME), 145, "OAM scan");
        assert_eq!(count_stat(&mut stat_ppu(0x10), FRAME), 1, "VBlank");
    }

    #[test]
    fn the_boot_rom_leaves_the_logo_and_the_trademark_in_vram() {
        let mut logo = [0u8; 48];
        logo[0] = 0xCE; // the real logo's first byte
        let mut p = Ppu::new();
        p.leave_boot_logo(&logo);
        // $CE: nibble $C doubled is $F0, two rows; $E doubled is $FC, two
        // rows; one bitplane.
        let tile_1: Vec<u8> = (0..8).map(|r| p.vram[0x10 + r * 2]).collect();
        assert_eq!(&tile_1[..4], &[0xF0, 0xF0, 0xFC, 0xFC]);
        assert_eq!(p.vram[0x11], 0, "the other bitplane stays clear");
        assert_eq!(p.vram[0x190], 0x3C, "tile $19: the ®'s top row");
        assert_eq!(
            (p.vram[0x1904], p.vram[0x190F]),
            (1, 12),
            "top row of the map"
        );
        assert_eq!((p.vram[0x1924], p.vram[0x192F]), (13, 24), "bottom row");
        assert_eq!(p.vram[0x1910], 0x19, "the ® after the top row");
    }

    #[test]
    fn the_mode_2_interrupt_comes_a_dot_early_and_hblank_a_dot_late() {
        for model in [Model::Dmg, Model::Cgb] {
            // The last dot of line 0 already has line 1's mode 2 interrupt.
            let mut p = Ppu::with_model(model);
            p.write_reg(0xFF41, 0x20);
            p.tick(454);
            assert_eq!(p.tick(1) & interrupt::STAT, interrupt::STAT, "{model:?}");
            assert_eq!(p.stat & 0x03, 0, "while STAT still shows HBlank");
            // HBlank's interrupt waits a dot after STAT shows mode 0.
            let mut p = Ppu::with_model(model);
            p.write_reg(0xFF41, 0x08);
            assert_eq!(p.tick(252) & interrupt::STAT, 0, "{model:?}");
            assert_eq!(p.stat & 0x03, 0, "{model:?}");
            assert_eq!(p.tick(1) & interrupt::STAT, interrupt::STAT, "{model:?}");
        }
    }

    #[test]
    fn the_originals_boot_rom_hands_over_near_the_end_of_vblank() {
        // Line 153, dot 395: VBlank, with LY already reading 0. Line 0's
        // mode 2 comes 61 dots later, after VBlank's last dot of mode 0.
        let mut p = Ppu::post_boot(Model::Dmg);
        assert_eq!((p.read_reg(0xFF44), p.stat & 0x03), (0, 1));
        p.tick(60);
        assert_eq!(p.stat & 0x03, 0, "dot 455");
        p.tick(1);
        assert_eq!((p.ly, p.dot, p.stat & 0x03), (0, 0, 2));
        let p = Ppu::post_boot(Model::Cgb);
        assert_eq!(
            (p.ly, p.dot, p.stat & 0x03),
            (144, 163, 1),
            "the Color: VBlank"
        );
    }

    #[test]
    fn the_mode_2_source_fires_for_line_144_before_vblank() {
        // A dot before VBlank on the original, as for every line's mode 2;
        // an M-cycle before on the Color.
        let mut p = stat_ppu(0x20);
        p.tick(456 * 144 - 100 - 2);
        assert_eq!(p.tick(1), interrupt::STAT, "line 143, dot 455");
        assert_eq!(p.tick(1), interrupt::VBLANK);
        let mut p = Ppu::with_model(Model::Cgb);
        p.write_reg(0xFF41, 0x20);
        p.tick(456 * 144 - 5);
        assert_eq!(p.tick(1), interrupt::STAT, "line 143, dot 452");
        assert_eq!(p.tick(4), interrupt::VBLANK);
    }

    #[test]
    fn the_mode_2_source_is_a_pulse_as_mode_2_begins() {
        // Enabling it during mode 2 fires nothing (the line was low again),
        // and nor does the original's STAT write bug then (Wilbert Pol's
        // stat_write_if). HBlank's source is a level: enabling it during
        // HBlank fires.
        let mut p = Ppu::new();
        p.tick(456 + 20); // line 1's mode 2
        p.write_reg(0xFF41, 0x20);
        assert_eq!(p.tick(1) & interrupt::STAT, 0);
        p.write_reg(0xFF41, 0x38); // all three mode sources, as the bug does
        assert_eq!(p.tick(1) & interrupt::STAT, 0);
        p.tick(300); // HBlank
        p.write_reg(0xFF41, 0x00);
        p.write_reg(0xFF41, 0x08);
        assert_eq!(p.tick(1) & interrupt::STAT, interrupt::STAT);
    }

    #[test]
    fn switching_on_starts_with_a_mode_0_that_isnt_an_hblank() {
        // The first line after switching on shows mode 0 until mode 3, but
        // the HBlank source doesn't count it (Wilbert Pol's intr_0_timing);
        // and its mode 3 lasts 2 dots longer (AGE's stat-mode).
        let mut p = Ppu::new();
        p.write_reg(0xFF40, 0x11);
        p.write_reg(0xFF41, 0x08);
        p.write_reg(0xFF40, 0x91);
        assert_eq!(p.stat & 0x03, 0);
        assert_eq!(count_stat(&mut p, 60), 0);
        let mode3_end = |p: &mut Ppu| {
            while p.stat & 0x03 != 3 {
                p.tick(1);
            }
            while p.stat & 0x03 == 3 {
                p.tick(1);
            }
            p.dot
        };
        let first = mode3_end(&mut p);
        assert_eq!(first, mode3_end(&mut p) + 2, "against the next line");
    }

    #[test]
    fn the_lyc_flag_reads_0_while_ly_reads_the_next_line() {
        let mut p = Ppu::new();
        p.write_reg(0xFF45, 0);
        p.tick(451);
        assert_eq!(p.read_reg(0xFF41) & 0x04, 0x04, "LY 0 == LYC 0");
        p.tick(1);
        assert_eq!(p.read_reg(0xFF41) & 0x04, 0, "LY reads 1 now");
    }

    #[test]
    fn the_lyc_flag_and_stat_line_hold_while_the_lcd_is_off() {
        // LY == LYC as the LCD goes off: the flag stays, and a new LYC
        // doesn't change it (the comparison doesn't run).
        let mut p = stat_ppu(0x40);
        p.write_reg(0xFF45, 0);
        p.write_reg(0xFF40, 0x11);
        p.write_reg(0xFF45, 1);
        assert_eq!(p.read_reg(0xFF41) & 0x04, 0x04);
        // On again, with LY 0 == LYC 0: the flag just stays set, so the
        // line never fell and there's no interrupt.
        p.write_reg(0xFF45, 0);
        p.write_reg(0xFF40, 0x91);
        assert_eq!(p.tick(1) & interrupt::STAT, 0);

        // Off with the flag clear, on with LY == LYC: an interrupt.
        let mut p = stat_ppu(0x40);
        p.write_reg(0xFF45, 9);
        p.write_reg(0xFF40, 0x11);
        assert_eq!(p.read_reg(0xFF41) & 0x04, 0);
        p.write_reg(0xFF45, 0);
        assert_eq!(p.read_reg(0xFF41) & 0x04, 0, "not compared while off");
        p.write_reg(0xFF40, 0x91);
        assert_eq!(p.read_reg(0xFF41) & 0x04, 0x04, "compared as it comes on");
        assert_eq!(p.tick(1) & interrupt::STAT, interrupt::STAT);
    }

    #[test]
    fn the_first_line_after_switching_on_skips_the_oam_scan() {
        let mut p = stat_ppu(0x20); // the mode 2 interrupt
        p.write_reg(0xFF40, 0x11);
        p.write_reg(0xFF40, 0x91); // line 0 starts at dot 2
        assert_eq!(count_stat(&mut p, 77), 0, "no mode 2 on line 0");
        assert_eq!((p.stat & 0x03, p.oam_locked(false)), (0, false));
        p.tick(1);
        assert_eq!((p.stat & 0x03, p.oam_locked(false)), (3, true), "dot 80");
        assert_eq!(count_stat(&mut p, 456 - 80), 1, "line 1 scans OAM");
        assert_eq!(p.stat & 0x03, 2);
    }

    #[test]
    fn reads_meet_the_ppus_hold_on_oam_and_vram_4_dots_before_writes() {
        // Line 1 of a frame (line 0 after switching on is different).
        let mut p = Ppu::new();
        let at = |p: &mut Ppu, dot: u32| {
            let target = 456 + dot;
            let now = u32::from(p.ly) * 456 + p.dot;
            p.tick(target - now);
            (
                p.oam_locked(false),
                p.oam_locked(true),
                p.vram_locked(false),
                p.vram_locked(true),
            )
        };
        //        (OAM read, OAM write, VRAM read, VRAM write) locked?
        assert_eq!(at(&mut p, 0), (true, true, false, false), "mode 2");
        assert_eq!(at(&mut p, 76), (true, false, true, false), "end of mode 2");
        assert_eq!(at(&mut p, 80), (true, true, true, true), "mode 3");
        assert_eq!(at(&mut p, 252), (false, false, false, false), "HBlank");
        assert_eq!(at(&mut p, 452), (true, false, false, false), "LY moved on");

        // The Color: VRAM reads aren't held before mode 3, and OAM writes
        // are held from 4 dots before a line too (AGE's vram-read and
        // oam-write).
        let mut p = Ppu::with_model(Model::Cgb);
        p.lcdc = 0x91;
        assert_eq!(at(&mut p, 76), (true, false, false, false), "end of mode 2");
        assert_eq!(at(&mut p, 80), (true, true, true, true), "mode 3");
        assert_eq!(at(&mut p, 452), (true, true, false, false), "LY moved on");
    }

    #[test]
    fn the_colors_first_line_after_switching_on_takes_vram_late() {
        // Mode 3 shows at dot 80, but the Color takes palette memory 2 dots
        // later and VRAM 5 (SameBoy; AGE's vram-read-cgbBCE); the original
        // takes VRAM at once.
        let switched_on = |model| {
            let mut p = Ppu::with_model(model);
            p.write_reg(0xFF40, 0x11);
            p.write_reg(0xFF40, 0x91);
            while p.dot != MODE3_DOT {
                p.tick(1);
            }
            p
        };
        let mut p = switched_on(Model::Cgb);
        assert_eq!(p.stat & 0x03, 3);
        assert_eq!(
            (p.vram_locked(false), p.palettes_locked(false)),
            (false, false)
        );
        p.tick(2);
        assert_eq!(
            (p.vram_locked(false), p.palettes_locked(true)),
            (false, true)
        );
        p.tick(3);
        assert!(p.vram_locked(false) && p.vram_locked(true));
        assert!(switched_on(Model::Dmg).vram_locked(false));
    }

    #[test]
    fn lyc_source_fires_once_when_ly_reaches_lyc() {
        let mut p = stat_ppu(0x40);
        p.write_reg(0xFF45, 10);
        assert_eq!(count_stat(&mut p, 456 * 9), 0);
        assert_eq!(p.tick(456) & interrupt::STAT, interrupt::STAT);
        assert_eq!(p.ly, 10);
        assert_eq!(count_stat(&mut p, FRAME - 456 * 10), 0, "once per frame");
    }

    #[test]
    fn stat_blocking_back_to_back_sources_fire_once() {
        // HBlank of line 143 runs straight into VBlank: the line never drops,
        // so VBlank adds nothing.
        assert_eq!(count_stat(&mut stat_ppu(0x18), FRAME), 144);
        // Each HBlank runs straight into the next line's OAM scan, so only
        // the OAM scan right after VBlank (line 0) gets a fresh edge.
        assert_eq!(count_stat(&mut stat_ppu(0x28), FRAME), 145);
    }

    #[test]
    fn register_writes_can_raise_the_stat_line_immediately() {
        // LYC moved onto the current line. (LYC starts at 0 == LY, which
        // already holds the line high, so move it away first to drop it.)
        let mut p = stat_ppu(0x40);
        p.write_reg(0xFF45, 99);
        assert_eq!(p.tick(1) & interrupt::STAT, 0);
        p.write_reg(0xFF45, p.ly);
        assert_eq!(p.tick(1) & interrupt::STAT, interrupt::STAT);

        // Enabling the HBlank source while in HBlank
        let mut p = stat_ppu(0x00);
        p.tick(200); // dot 300: mode 0
        p.write_reg(0xFF41, 0x08);
        assert_eq!(p.tick(1) & interrupt::STAT, interrupt::STAT);
    }

    #[test]
    fn lcd_off_resets_ly_reports_mode_0_and_goes_quiet() {
        let mut p = stat_ppu(0x78); // every STAT source on
        p.tick(456 * 50);
        p.write_reg(0xFF40, p.lcdc & !0x80);
        assert_eq!(p.read_reg(0xFF44), 0, "LY");
        assert_eq!(p.read_reg(0xFF41) & 0x03, 0, "mode 0");
        assert!(
            p.framebuffer().chunks(4).all(|px| px == DMG_PALETTE[0]),
            "blank"
        );
        assert_eq!(p.tick(FRAME), 0, "no VBlank or STAT while off");
        assert_eq!(p.ly, 0);

        // Back on: starts over from the top of line 0 (in mode 0: that line
        // has no OAM scan).
        p.write_reg(0xFF40, p.lcdc | 0x80);
        p.tick(10);
        assert_eq!((p.ly, p.read_reg(0xFF41) & 0x03), (0, 0));
        p.tick(456 - 10);
        assert_eq!((p.ly, p.read_reg(0xFF41) & 0x03), (1, 2));
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

    /// The shade (0-3) at (x, y) of a `width`-pixel-wide RGBA debug image.
    fn image_shade(image: &[u8], width: usize, x: usize, y: usize) -> usize {
        let i = (y * width + x) * 4;
        DMG_PALETTE
            .iter()
            .position(|c| c[..] == image[i..i + 4])
            .expect("a palette color")
    }

    #[test]
    fn tile_sheet_lays_out_all_384_tiles_16_per_row() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8010, striped(0xFF, 0xFF)); // tile 1: color 3
        put_tile(&mut p, 0x9000, striped(0xFF, 0x00)); // tile 256: color 1
        put_tile(&mut p, 0x97F0, striped(0x00, 0xFF)); // tile 383: color 2
        let sheet = p.tile_sheet(0);
        assert_eq!(sheet.len(), TILE_SHEET_WIDTH * TILE_SHEET_HEIGHT * 4);
        let at = |x, y| image_shade(&sheet, TILE_SHEET_WIDTH, x, y);
        assert_eq!(at(0, 0), 0, "tile 0 is blank");
        assert_eq!((at(8, 0), at(15, 7)), (3, 3), "tile 1, next to it");
        assert_eq!(at(16, 0), 0, "tile 2");
        assert_eq!(at(0, 128), 1, "tile 256 starts row 16");
        assert_eq!(at(127, 191), 2, "tile 383 is last");
    }

    #[test]
    fn debug_views_show_colors_through_bgp() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8010, striped(0xFF, 0xFF));
        p.bgp = 0b00_11_11_11; // color 3 shows lightest, 0-2 darkest
        let sheet = p.tile_sheet(0);
        assert_eq!(image_shade(&sheet, TILE_SHEET_WIDTH, 8, 0), 0);
        assert_eq!(image_shade(&sheet, TILE_SHEET_WIDTH, 0, 0), 3);
    }

    #[test]
    fn tile_map_image_draws_a_whole_map() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8010, striped(0xFF, 0xFF)); // tile 1 ($8000 mode)
        put_tile(&mut p, 0x9010, striped(0xFF, 0x00)); // tile 1 ($8800 mode)
        p.write_vram(0x9800 + 32 + 2, 1); // map 0, column 2, row 1
        p.write_vram(0x9C00 + 31 * 32 + 31, 1); // map 1, bottom-right corner
        let map0 = p.tile_map_image(false);
        assert_eq!(map0.len(), 256 * 256 * 4);
        assert_eq!(image_shade(&map0, 256, 16, 8), 3);
        assert_eq!(image_shade(&map0, 256, 15, 8), 0);
        let map1 = p.tile_map_image(true);
        assert_eq!(image_shade(&map1, 256, 255, 255), 3, "$9C00 map");
        assert_eq!(image_shade(&map1, 256, 16, 8), 0);
        p.lcdc &= !0x10;
        let signed = p.tile_map_image(false);
        assert_eq!(
            image_shade(&signed, 256, 16, 8),
            1,
            "LCDC bit 4: tile 1 at $9010"
        );
    }

    /// A Color PPU, LCD and BG on, $8000 addressing, $9800 map.
    fn cgb_ppu() -> Ppu {
        let mut p = Ppu::with_model(Model::Cgb);
        p.lcdc = 0x91;
        p
    }

    /// Sets color `index` of background palette `palette` to `rgb555`.
    fn set_bg_color(p: &mut Ppu, palette: u8, index: u8, rgb555: u16) {
        p.write_color_reg(0xFF68, 0x80 | (palette * 8 + index * 2));
        let [lo, hi] = rgb555.to_le_bytes();
        p.write_color_reg(0xFF69, lo);
        p.write_color_reg(0xFF69, hi);
    }

    fn rgba_at(p: &Ppu, x: usize, y: usize) -> [u8; 4] {
        let i = (y * SCREEN_WIDTH + x) * 4;
        p.framebuffer()[i..i + 4].try_into().unwrap()
    }

    const RED: u16 = 0x001F;
    const GREEN: u16 = 0x03E0;
    const BLUE: u16 = 0x7C00;

    #[test]
    fn rgb555_becomes_rgba_the_way_cgb_acid2_expects() {
        assert_eq!(rgba(0x7FFF), [255, 255, 255, 255]);
        assert_eq!(rgba(0x0000), [0, 0, 0, 255]);
        assert_eq!(rgba(RED), [255, 0, 0, 255]);
        assert_eq!(rgba(GREEN), [0, 255, 0, 255]);
        assert_eq!(rgba(BLUE), [0, 0, 255, 255]);
        assert_eq!(rgba(0x0001), [8, 0, 0, 255], "(1 << 3) | (1 >> 2)");
        assert_eq!(rgba(0x0010), [132, 0, 0, 255], "(16 << 3) | (16 >> 2)");
        for c in [0x0000, 0x1234, 0x7FFF, 0x5555, 0x2AAA] {
            assert_eq!(rgb555(&rgba(c)), c, "round trip {c:04X}");
        }
    }

    #[test]
    fn palette_data_auto_increments_on_writes_but_not_reads() {
        let mut p = cgb_ppu();
        p.write_color_reg(0xFF68, 0x80 | 0x3E);
        assert_eq!(p.read_color_reg(0xFF68), 0xFE, "bit 6 reads 1");
        p.write_color_reg(0xFF69, 0x11);
        p.write_color_reg(0xFF69, 0x22);
        assert_eq!(p.read_color_reg(0xFF68), 0xC0, "wrapped from 63 to 0");
        p.write_color_reg(0xFF69, 0x33);
        p.write_color_reg(0xFF68, 0x3E); // no auto-increment
        assert_eq!(p.read_color_reg(0xFF69), 0x11);
        assert_eq!(p.read_color_reg(0xFF69), 0x11, "reading doesn't move on");
        p.write_color_reg(0xFF69, 0x44);
        assert_eq!(p.read_color_reg(0xFF68), 0x7E, "without bit 7 it stays");
        assert_eq!(p.read_color_reg(0xFF69), 0x44);
        p.write_color_reg(0xFF68, 0x00);
        assert_eq!(p.read_color_reg(0xFF69), 0x33);
        // Sprite palettes are separate.
        p.write_color_reg(0xFF6A, 0x80);
        p.write_color_reg(0xFF6B, 0x55);
        assert_eq!(p.read_color_reg(0xFF6A), 0xC1);
        assert_eq!(p.read_color_reg(0xFF69), 0x33, "background untouched");
    }

    #[test]
    fn the_boot_rom_leaves_the_background_white() {
        let mut p = cgb_ppu();
        p.render_scanline();
        assert_eq!(rgba_at(&p, 0, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn color_tiles_pick_their_palette_and_bank_from_vram_bank_1() {
        let mut p = cgb_ppu();
        set_bg_color(&mut p, 0, 1, RED);
        set_bg_color(&mut p, 5, 1, GREEN);
        set_bg_color(&mut p, 5, 2, BLUE);
        put_tile(&mut p, 0x8010, striped(0xFF, 0x00)); // tile 1, bank 0: color 1
        p.set_vram_bank(1);
        put_tile(&mut p, 0x8010, striped(0x00, 0xFF)); // tile 1, bank 1: color 2
        p.write_vram(0x9801, 0x05); // map entry 1: palette 5
        p.write_vram(0x9802, 0x0D); // map entry 2: palette 5, tile from bank 1
        p.set_vram_bank(0);
        for entry in 0..3 {
            p.write_vram(0x9800 + entry, 1);
        }
        p.render_scanline();
        assert_eq!(rgba_at(&p, 0, 0), rgba(RED), "palette 0, color 1");
        assert_eq!(rgba_at(&p, 8, 0), rgba(GREEN), "palette 5, color 1");
        assert_eq!(
            rgba_at(&p, 16, 0),
            rgba(BLUE),
            "palette 5, color 2 from bank 1"
        );
    }

    #[test]
    fn color_tiles_can_be_flipped() {
        let mut p = cgb_ppu();
        set_bg_color(&mut p, 0, 1, RED);
        // Tile 1: only the top-left pixel set.
        let mut tile = [0u8; 16];
        tile[0] = 0x80;
        put_tile(&mut p, 0x8010, tile);
        for entry in 0..4 {
            p.write_vram(0x9800 + entry, 1);
        }
        p.set_vram_bank(1);
        p.write_vram(0x9801, 0x20); // X flip
        p.write_vram(0x9802, 0x40); // Y flip
        p.write_vram(0x9803, 0x60); // both
        p.set_vram_bank(0);
        let lit = |p: &Ppu, x, y| rgba_at(p, x, y) == rgba(RED);
        for ly in [0, 7] {
            p.ly = ly;
            p.render_scanline();
        }
        assert!(lit(&p, 0, 0) && !lit(&p, 7, 0), "unflipped: top left");
        assert!(lit(&p, 15, 0) && !lit(&p, 8, 0), "X flip: top right");
        assert!(lit(&p, 16, 7) && !lit(&p, 16, 0), "Y flip: bottom left");
        assert!(lit(&p, 31, 7), "both: bottom right");
    }

    #[test]
    fn lcdc_bit_0_does_not_hide_the_color_background() {
        let mut p = cgb_ppu();
        set_bg_color(&mut p, 0, 3, BLUE);
        put_tile(&mut p, 0x8000, striped(0xFF, 0xFF));
        p.lcdc &= !0x01;
        p.render_scanline();
        assert_eq!(rgba_at(&p, 0, 0), rgba(BLUE));
    }

    #[test]
    fn the_original_ignores_vram_bank_1_attributes() {
        let mut p = bg_ppu(); // DMG
        put_tile(&mut p, 0x8000, striped(0xFF, 0x00)); // tile 0, color 1
        p.vram[0x2000] = 0x20; // where a Color would look for attributes
        p.render_scanline();
        assert_eq!(shade_at(&p, 0, 0), 1, "drawn plainly through BGP");
    }

    #[test]
    fn a_color_picture_survives_a_save_state() {
        let mut p = cgb_ppu();
        set_bg_color(&mut p, 0, 0, 0x1234);
        p.render_scanline();
        let mut w = StateWriter::new();
        p.save_state(&mut w);
        let state = w.finish(0);
        let mut q = cgb_ppu();
        let mut r = StateReader::open(&state, 0).unwrap();
        q.load_state(&mut r).unwrap();
        r.finish().unwrap();
        assert_eq!(rgba_at(&q, 0, 0), rgba(0x1234));
        assert_eq!(
            q.read_color_reg(0xFF68),
            0xC2,
            "the index moved on past the color"
        );
        assert_eq!(q.bg_palettes, p.bg_palettes);
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
        let row: Vec<u8> = (0..8).map(|col| p.tile_pixel(0, 0, col, 0)).collect();
        assert_eq!(row, [0, 2, 3, 3, 3, 3, 2, 0]);
    }

    #[test]
    fn lcdc_bit_4_selects_tile_addressing() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8000, striped(0xFF, 0x00)); // tile 0, $8000 mode
        put_tile(&mut p, 0x9000, striped(0x00, 0xFF)); // tile 0, $8800 mode
        put_tile(&mut p, 0x8800, striped(0xFF, 0xFF)); // tile $80 in both
        assert_eq!(p.tile_pixel(0x00, 0, 0, 0), 1);
        assert_eq!(p.tile_pixel(0x80, 0, 0, 0), 3);
        p.lcdc &= !0x10;
        assert_eq!(p.tile_pixel(0x00, 0, 0, 0), 2, "signed: tile 0 is at $9000");
        assert_eq!(
            p.tile_pixel(0x80, 0, 0, 0),
            3,
            "signed: tile -128 is at $8800"
        );
        assert_eq!(
            p.tile_pixel(0x7F, 0, 0, 0),
            0,
            "tile 127 is at $97F0 (blank)"
        );
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
    fn each_line_is_drawn_as_hblank_begins_so_hblank_writes_show_on_the_next() {
        let mut p = bg_ppu();
        put_tile(&mut p, 0x8010, striped(0xFF, 0xFF)); // tile 1: color 3
        p.write_vram(0x9801, 1); // map column 1
        p.tick(HBLANK_DOT); // line 0 drawn now
        p.tick(2);
        assert_eq!(p.take_hblanks(), 1);
        p.scx = 8; // during line 0's HBlank
        p.tick(DOTS_PER_LINE); // into line 1's HBlank
        assert_eq!(shade_at(&p, 0, 0), 0, "line 0 was already drawn");
        assert_eq!(shade_at(&p, 0, 1), 3, "line 1 scrolled");
        p.take_hblanks();
        p.tick(crate::CYCLES_PER_FRAME);
        assert_eq!(
            p.take_hblanks(),
            144,
            "one per visible line, none in VBlank"
        );
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
    fn writing_wy_mid_line_looks_at_wy_and_ly_again_a_few_dots_later() {
        // Line 2, mode 3 under way, window on, WY $FF: no match at the line's
        // start. WY = 2 then matches 3 dots later on the original, 6 on the
        // Color (Gambatte's window/late_wy).
        for (model, after) in [(Model::Dmg, 3), (Model::Cgb, 6)] {
            let mut p = Ppu::with_model(model);
            p.lcdc = 0xB1;
            p.wy = 0xFF;
            run_to(&mut p, 2, 85);
            assert!(!p.wy_triggered);
            p.write_reg(0xFF4A, 2);
            p.tick(after - 1);
            assert!(!p.wy_triggered, "{model:?}: not yet");
            p.tick(1);
            assert!(p.wy_triggered, "{model:?}");
        }
    }

    #[test]
    fn wy_only_matches_while_the_window_is_on() {
        // WY = 0 with the window off on line 0: no match, so turning the
        // window on for line 1 shows nothing this frame.
        let mut p = window_ppu(7, 0);
        p.lcdc &= !0x20;
        lines(&mut p, 1);
        p.lcdc |= 0x20;
        lines(&mut p, 1);
        assert_eq!(shade_at(&p, 0, 1), 0);
        assert!(!p.wy_triggered);
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
        assert_eq!(p.window_y, 0, "row 0 was the one it drew");
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

    /// Starts the line over from dot 0, for sprites put in place just now
    /// (the Color reads object 0 at dot 0).
    fn begin_line(p: &mut Ppu) {
        p.dot = 0;
        p.oam_scan_dot();
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

    // Color sprites.

    /// Sets color `index` of sprite palette `palette` to `rgb555`.
    fn set_obj_color(p: &mut Ppu, palette: u8, index: u8, rgb555: u16) {
        p.write_color_reg(0xFF6A, 0x80 | (palette * 8 + index * 2));
        let [lo, hi] = rgb555.to_le_bytes();
        p.write_color_reg(0xFF6B, lo);
        p.write_color_reg(0xFF6B, hi);
    }

    /// A Color PPU with sprites on; tile 1 is all color 1 in bank 0 and all
    /// color 2 in bank 1. Sprite palettes 0 and 6 are set up; the background
    /// is white (the boot palettes) with blank tile 0.
    fn cgb_sprite_ppu() -> Ppu {
        let mut p = cgb_ppu();
        p.lcdc |= 0x02;
        put_tile(&mut p, 0x8010, striped(0xFF, 0x00));
        p.set_vram_bank(1);
        put_tile(&mut p, 0x8010, striped(0x00, 0xFF));
        p.set_vram_bank(0);
        set_obj_color(&mut p, 0, 1, RED);
        set_obj_color(&mut p, 0, 2, BLUE);
        set_obj_color(&mut p, 6, 1, GREEN);
        p
    }

    #[test]
    fn color_sprites_take_their_palette_from_bits_0_to_2() {
        let mut p = cgb_sprite_ppu();
        put_sprite(&mut p, 0, 0, 0, 1, 0x00);
        put_sprite(&mut p, 1, 8, 0, 1, 0x06);
        put_sprite(&mut p, 2, 16, 0, 1, 0x10); // bit 4 is the original's OBP1
        draw_line(&mut p, 0);
        assert_eq!(rgba_at(&p, 0, 0), rgba(RED), "palette 0");
        assert_eq!(rgba_at(&p, 8, 0), rgba(GREEN), "palette 6");
        assert_eq!(rgba_at(&p, 16, 0), rgba(RED), "bit 4 means nothing here");
    }

    #[test]
    fn color_sprites_can_take_their_tile_from_bank_1() {
        let mut p = cgb_sprite_ppu();
        put_sprite(&mut p, 0, 0, 0, 1, 0x08);
        draw_line(&mut p, 0);
        assert_eq!(rgba_at(&p, 0, 0), rgba(BLUE), "color 2, from bank 1");
    }

    #[test]
    fn overlapping_color_sprites_go_by_oam_order_alone() {
        let mut p = cgb_sprite_ppu();
        put_sprite(&mut p, 0, 4, 0, 1, 0x00); // red, further right
        put_sprite(&mut p, 1, 0, 0, 1, 0x06); // green, smaller X
        draw_line(&mut p, 0);
        assert_eq!(rgba_at(&p, 4, 0), rgba(RED), "OAM 0 wins despite its X");
        assert_eq!(rgba_at(&p, 0, 0), rgba(GREEN), "where they don't overlap");
        // OPRI = 1: the original's way, smaller X first.
        p.write_color_reg(0xFF6C, 0x01);
        assert_eq!(p.read_color_reg(0xFF6C), 0xFF);
        draw_line(&mut p, 0);
        assert_eq!(rgba_at(&p, 4, 0), rgba(GREEN));
    }

    #[test]
    fn color_background_priority_has_three_switches() {
        let mut p = cgb_sprite_ppu();
        // BG tile 1 (color 1, white through the boot palette) under the
        // first four columns of tiles; columns 2 and 3 have attribute bit 7.
        for col in 0..4 {
            p.write_vram(0x9800 + col, 1);
        }
        p.set_vram_bank(1);
        p.write_vram(0x9802, 0x80);
        p.write_vram(0x9803, 0x80);
        p.set_vram_bank(0);
        put_sprite(&mut p, 0, 0, 0, 1, 0x00); // over a plain tile
        put_sprite(&mut p, 1, 8, 0, 1, 0x80); // sprite bit 7
        put_sprite(&mut p, 2, 16, 0, 1, 0x00); // tile bit 7
        put_sprite(&mut p, 3, 32, 0, 1, 0x80); // both, but over BG color 0
        draw_line(&mut p, 0);
        let white = [255, 255, 255, 255];
        assert_eq!(rgba_at(&p, 0, 0), rgba(RED), "neither bit: sprite on top");
        assert_eq!(
            rgba_at(&p, 8, 0),
            white,
            "sprite bit 7: BG colors 1-3 cover it"
        );
        assert_eq!(rgba_at(&p, 16, 0), white, "tile bit 7: same");
        assert_eq!(rgba_at(&p, 32, 0), rgba(RED), "BG color 0 never covers");
        // LCDC bit 0 off: sprites go on top regardless.
        p.lcdc &= !0x01;
        draw_line(&mut p, 0);
        assert_eq!(rgba_at(&p, 8, 0), rgba(RED));
        assert_eq!(rgba_at(&p, 16, 0), rgba(RED));
    }

    #[test]
    fn the_original_ignores_the_color_sprite_bits() {
        let mut p = sprite_ppu(); // DMG, obp0 identity
        put_sprite(&mut p, 0, 0, 0, 1, 0x0F); // bank and palette bits set
        put_sprite(&mut p, 1, 0, 1, 1, 0x00); // same place a line lower
        draw_line(&mut p, 0);
        assert_eq!(shade_at(&p, 0, 0), 1, "tile 1 from bank 0 through OBP0");
        // And X still wins over OAM order (below the first two sprites).
        put_sprite(&mut p, 2, 4, 10, 1, 0x00);
        put_sprite(&mut p, 3, 0, 10, 2, 0x00);
        draw_line(&mut p, 10);
        assert_eq!(shade_at(&p, 4, 10), 2, "smaller X first on the original");
    }

    /// Runs a PPU from the start of line 0 to its HBlank: the dot where STAT
    /// first reads mode 0 after the OAM scan.
    fn hblank_start(p: &mut Ppu) -> u32 {
        for dot in 1..DOTS_PER_LINE {
            p.tick(1);
            if dot > MODE3_DOT && p.stat & 0x03 == 0 {
                return dot;
            }
        }
        DOTS_PER_LINE
    }

    #[test]
    fn mode_3_lasts_172_dots_plus_the_fine_scroll() {
        for scx in [0, 1, 7, 8, 13] {
            let mut p = bg_ppu();
            p.scx = scx;
            assert_eq!(hblank_start(&mut p), 252 + u32::from(scx % 8), "SCX {scx}");
        }
    }

    #[test]
    fn the_window_adds_6_dots_on_lines_it_shows() {
        assert_eq!(hblank_start(&mut window_ppu(7, 0)), 258);
        assert_eq!(hblank_start(&mut window_ppu(7, 1)), 252, "WY not reached");
        assert_eq!(hblank_start(&mut window_ppu(167, 0)), 252, "off the right");
    }

    #[test]
    fn sprites_add_6_dots_each_plus_a_wait_for_the_tile_under_them() {
        // OAM X positions, and the dots they add: Pan Docs' figures (6 a
        // sprite, plus the pixels of its tile right of it, less 2, for the
        // first on a tile). They come out of the FIFO's fetches.
        let cases: &[(&[u8], u32)] = &[
            (&[8], 6 + 5),             // at a tile's left edge: waits 5
            (&[12], 6 + 1),            // halfway: waits 1
            (&[15], 6),                // at its right end: no wait
            (&[8, 8], 6 + 5 + 6),      // a second on the tile doesn't wait
            (&[8, 16], 6 + 5 + 6 + 5), // the next tile waits again
            (&[16, 8], 6 + 5 + 6 + 5), // whatever the OAM order
            (&[0, 0], 6 + 5 + 6),      // X = 0 waits as at a tile's start
            (&[168], 0),               // never reached
        ];
        for &(xs, dots) in cases {
            let mut p = sprite_ppu();
            for (i, &x) in xs.iter().enumerate() {
                put_sprite(&mut p, i as u16, i16::from(x) - 8, 0, 1, 0);
            }
            assert_eq!(hblank_start(&mut p), 252 + dots, "sprites at {xs:?}");
        }
        // SCX moves the tiles under them: X = 8 is now halfway into one.
        let mut p = sprite_ppu();
        p.scx = 4;
        put_sprite(&mut p, 0, 0, 0, 1, 0);
        assert_eq!(hblank_start(&mut p), 252 + 4 + (6 + 1));
    }

    /// Runs a PPU on to its line's HBlank; returns that dot.
    fn run_to_hblank(p: &mut Ppu) -> u32 {
        while p.dot <= MODE3_DOT || p.stat & 0x03 != 0 {
            p.tick(1);
        }
        p.dot
    }

    #[test]
    fn during_an_oam_dma_the_scan_sees_the_last_object_it_read_again_and_again() {
        // Object 4 is on line 1, the rest off-screen. A DMA starts as the
        // scan reaches object 5: from then on the scan keeps seeing object
        // 4's Y and X, so objects 5-13 join it, all at its X.
        let mut p = sprite_ppu();
        put_sprite(&mut p, 4, 40, 1, 1, 0);
        run_to(&mut p, 1, 11);
        p.oam_dma_busy = true;
        run_to(&mut p, 1, MODE3_DOT);
        let expected: Vec<(u8, u8)> = (4..14).map(|i| (i, 48)).collect();
        assert_eq!(p.line_sprites(), expected);
    }

    #[test]
    fn during_an_oam_dma_the_fetcher_reads_where_it_writes() {
        // The DMA writes byte $11 next: a sprite's tile read gets byte $10,
        // its attributes byte $11, whichever sprite it is.
        let mut p = sprite_ppu();
        for (i, b) in p.oam.iter_mut().enumerate() {
            *b = i as u8;
        }
        p.oam_dma_dest = Some(0x11);
        assert_eq!(
            (p.oam_fetch(4 * 7 + 2), p.oam_fetch(4 * 7 + 3)),
            (0x10, 0x11)
        );
        p.oam_dma_dest = None;
        assert_eq!(p.oam_fetch(4 * 7 + 2), 30);
    }

    #[test]
    fn sprites_off_mid_fetch_cut_the_fetch_short_on_the_original() {
        // A sprite at the line's start costs 11 dots. Turning sprites off a
        // dot into its fetch: the original drops the rest of it (2 dots
        // spent); the Color, which fetches sprites whether they're on or
        // not, carries on.
        for (mut p, saved) in [(sprite_ppu(), 9), (cgb_sprite_ppu(), 0)] {
            put_sprite(&mut p, 0, 0, 0, 1, 0);
            begin_line(&mut p);
            let full = run_to_hblank(&mut p.clone());
            while !p.fetching_sprite() {
                p.tick(1);
            }
            p.tick(1);
            p.write_reg(0xFF40, p.lcdc & !0x02);
            assert_eq!(run_to_hblank(&mut p), full - saved, "{:?}", p.model);
        }
    }

    /// `window_ppu`, but with the window off and the background color 1.
    /// If WY is 0, the window was on when line 0 started, so WY has matched.
    fn hidden_window_ppu(wx: u8, wy: u8) -> Ppu {
        let mut p = window_ppu(wx, wy);
        p.wy_triggered = wy == 0;
        p.lcdc &= !0x20;
        for col in 0..32 {
            p.write_vram(0x9800 + col, 1);
        }
        p
    }

    #[test]
    fn the_originals_hidden_window_leaves_a_blank_pixel_where_it_would_start() {
        // Window off, but WY reached: where the window would start (pixel
        // 16, at a tile's edge) the fetcher pushes a blank pixel first.
        let mut p = hidden_window_ppu(7 + 16, 0);
        lines(&mut p, 1);
        assert_eq!(shades(&p, 0, 14..19), [1, 1, 0, 1, 1]);
        // Not before WY is reached.
        let mut p = hidden_window_ppu(7 + 16, 1);
        lines(&mut p, 1);
        assert_eq!(shades(&p, 0, 14..19), [1; 5]);
        // Not on the Color.
        let mut p = cgb_ppu();
        p.wx = 7 + 16;
        put_tile(&mut p, 0x8010, striped(0xFF, 0x00));
        for col in 0..32 {
            p.write_vram(0x9800 + col, 1);
        }
        set_bg_color(&mut p, 0, 1, RED);
        lines(&mut p, 1);
        assert_eq!(rgba_at(&p, 16, 0), rgba(RED));
    }

    #[test]
    fn turning_the_window_off_as_it_starts_leaves_no_blank_pixel() {
        // The window starts at pixel 16 and is turned off while its first
        // tile is fetched: the background goes on, with no blank pixel.
        let mut p = hidden_window_ppu(7 + 16, 0);
        p.lcdc |= 0x20;
        while !p.fetching_window() {
            p.tick(1);
        }
        p.write_reg(0xFF40, p.lcdc & !0x20);
        lines(&mut p, 1);
        assert_eq!(shades(&p, 0, 14..19), [1; 5]);
    }

    #[test]
    fn the_originals_window_starts_a_pixel_late_if_turned_on_just_after_its_spot() {
        // WX 7 + 16: the window's spot is pixel 16, which is next out after
        // dot 108. The original's window logic sees LCDC a dot late.
        let window_from = |on_after_dot: u32, wx_after_dot: Option<u32>| {
            let mut p = window_ppu(7 + 16, 0);
            p.wy_triggered = true; // WY matched earlier, with the window on
            p.lcdc &= !0x20;
            for dot in 0..DOTS_PER_LINE {
                if dot == on_after_dot {
                    p.write_reg(0xFF40, p.lcdc | 0x20);
                }
                if Some(dot) == wx_after_dot {
                    p.write_reg(0xFF4B, p.wx);
                }
                p.tick(1);
            }
            (0..SCREEN_WIDTH).position(|x| shade_at(&p, x, 0) == 1)
        };
        assert_eq!(window_from(107, None), Some(16), "in time");
        assert_eq!(window_from(108, None), Some(17), "a pixel late");
        assert_eq!(window_from(109, None), None, "too late");
        // Not if WX was written just then.
        assert_eq!(window_from(108, Some(109)), None);
    }

    #[test]
    fn hblank_dma_and_drawing_follow_the_longer_mode_3() {
        let mut p = bg_ppu();
        p.scx = 5;
        p.tick(252 + 4);
        assert_eq!(p.stat & 0x03, 3, "still drawing");
        p.tick(1);
        assert_eq!(p.stat & 0x03, 0);
        p.tick(1);
        assert_eq!(p.take_hblanks(), 0, "HBlank DMA's block comes 2 dots in");
        p.tick(1);
        assert_eq!(p.take_hblanks(), 1);
    }

    #[test]
    fn ly_moves_on_2_dots_before_the_line_ends() {
        let mut p = Ppu::new();
        p.tick(453);
        assert_eq!(p.read_reg(0xFF44), 0);
        p.tick(1);
        assert_eq!(p.read_reg(0xFF44), 1);
        assert_eq!((p.ly, p.stat & 0x03), (0, 0), "still line 0's HBlank");
        p.tick(456 * 153 - 2);
        assert_eq!((p.ly, p.read_reg(0xFF44)), (153, 0), "and from 153 to 0");
    }

    /// Runs a PPU from where it is to `dot` of line `ly`.
    fn run_to(p: &mut Ppu, ly: u8, dot: u32) {
        while (p.ly, p.dot) != (ly, dot) {
            p.tick(1);
        }
    }

    #[test]
    fn in_double_speed_the_first_lines_mode_3_and_the_oam_locks_shift() {
        // Switched on in double speed, line 0 starts a dot later than in
        // normal speed, at dot 2, and its mode 3 shows 2 dots late, OAM
        // reads held with it; writes are held from dot 80 as ever (AGE's
        // stat-mode-ds and oam-read/oam-write).
        let mut p = Ppu::with_model(Model::Cgb);
        p.double_speed = true;
        p.write_reg(0xFF40, 0x11);
        p.write_reg(0xFF40, 0x91);
        assert_eq!(p.dot, 2);
        run_to(&mut p, 0, 81);
        assert_eq!(
            (p.stat & 0x03, p.oam_locked(false), p.oam_locked(true)),
            (0, false, true)
        );
        p.tick(1);
        assert_eq!((p.stat & 0x03, p.oam_locked(false)), (3, true), "dot 82");

        // Before the next line's OAM scan, reads aren't held at all, and
        // writes only in the line's last 2 dots.
        run_to(&mut p, 1, 453);
        assert_eq!((p.oam_locked(false), p.oam_locked(true)), (false, false));
        p.tick(1);
        assert_eq!((p.oam_locked(false), p.oam_locked(true)), (false, true));
    }

    #[test]
    fn in_double_speed_the_mode_2_and_hblank_sources_fire_a_dot_early() {
        // The mode 2 source fires at dot 454 of the line before (455 in
        // normal speed); HBlank's in the dot STAT shows mode 0 (a dot after
        // in normal speed). AGE's stat-int.
        for (double, mode2_dot, hblank_after) in [(false, 455, 1), (true, 454, 0)] {
            let mut p = Ppu::with_model(Model::Cgb);
            p.double_speed = double;
            p.lcdc = 0x91;
            p.write_reg(0xFF41, 0x20);
            run_to(&mut p, 1, mode2_dot - 1);
            assert_eq!(p.tick(1) & interrupt::STAT, interrupt::STAT);
            p.tick(10);
            p.write_reg(0xFF41, 0x08);
            let mut mode0 = None;
            let fired = loop {
                let irq = p.tick(1);
                if p.stat & 0x03 == 0 {
                    mode0.get_or_insert(p.dot);
                }
                if irq & interrupt::STAT != 0 {
                    break p.dot;
                }
            };
            assert_eq!(
                mode0.map(|m| fired - m),
                Some(hblank_after),
                "double: {double}, {mode0:?} {fired}"
            );
        }
    }

    #[test]
    fn line_153_shows_153_for_only_2_dots_in_normal_speed() {
        // LY moves on to 153 2 dots before line 152 ends, as on every line,
        // but reads 0 from line 153's first dot (CPU CGB C and the original;
        // later Colors hold 153 a little longer). In double speed it holds
        // for 4 dots more (AGE's ly).
        for (mut p, zero_from) in [
            (Ppu::new(), 0),
            (Ppu::with_model(Model::Cgb), 0),
            (Ppu::with_model(Model::Cgb), 4),
        ] {
            p.double_speed = zero_from > 0;
            p.lcdc = 0x91;
            run_to(&mut p, 152, 453);
            assert_eq!(p.read_reg(0xFF44), 152);
            p.tick(1);
            assert_eq!(p.read_reg(0xFF44), 153, "{:?}", p.model);
            if zero_from > 0 {
                run_to(&mut p, 153, zero_from - 1);
                assert_eq!(p.read_reg(0xFF44), 153, "double speed");
            }
            run_to(&mut p, 153, zero_from);
            assert_eq!(p.read_reg(0xFF44), 0, "{:?}", p.model);
            run_to(&mut p, 153, 300);
            assert_eq!(p.read_reg(0xFF44), 0);
        }
    }

    #[test]
    fn on_line_153_lyc_matches_153_for_4_dots_then_0() {
        // LYC = 153: the flag is set for line 153's first 4 dots. LYC = 0:
        // set from dot 8, through to line 0 (no gap at the frame's turn).
        let flag_at = |lyc: u8, ly: u8, dot: u32| {
            let mut p = Ppu::new();
            p.lyc = lyc;
            run_to(&mut p, ly, dot);
            p.stat & 0x04 != 0
        };
        assert!(flag_at(153, 153, 0));
        assert!(flag_at(153, 153, 3));
        assert!(!flag_at(153, 153, 4));
        assert!(!flag_at(0, 153, 7));
        assert!(flag_at(0, 153, 8));
        assert!(flag_at(0, 153, 455), "and on into line 0");
        assert!(flag_at(0, 0, 0));
    }

    #[test]
    fn vblank_ends_with_a_dot_of_mode_0() {
        // VBlank's mode 1 ends a dot early, showing mode 0 for it, on the
        // original and CPU CGB C (AGE's stat-mode; later Colors go straight
        // on to line 0's mode 2).
        for mut p in [Ppu::new(), Ppu::with_model(Model::Cgb)] {
            p.lcdc = 0x91;
            run_to(&mut p, 153, 454);
            assert_eq!(p.stat & 0x03, 1);
            p.tick(1);
            assert_eq!(p.stat & 0x03, 0, "{:?}", p.model);
            p.tick(1);
            assert_eq!((p.ly, p.stat & 0x03), (0, 2));
        }
    }

    #[test]
    fn in_a_lines_last_4_dots_ly_lyc_reads_0_on_the_original_and_holds_on_the_color() {
        // LYC = 2. On the original the flag reads 0 for line 2's last 4
        // dots; on the Color it keeps its value there, and a new LYC isn't
        // compared until line 3 starts.
        for (mut p, held) in [(Ppu::new(), false), (Ppu::with_model(Model::Cgb), true)] {
            p.lcdc = 0x91;
            p.lyc = 2;
            run_to(&mut p, 2, 451);
            assert_ne!(p.stat & 0x04, 0);
            p.tick(1);
            assert_eq!(p.stat & 0x04 != 0, held, "{:?}", p.model);
        }
        let mut p = Ppu::with_model(Model::Cgb);
        p.lcdc = 0x91;
        p.lyc = 0xF0;
        run_to(&mut p, 2, 452);
        p.write_reg(0xFF45, 2);
        assert_eq!(p.stat & 0x04, 0, "no match this late");
        assert_eq!(p.pending_irq, 0);
    }

    /// A Color running an original game: the original's way of drawing (so
    /// BGP's shades, through the boot palettes) on the Color's hardware.
    /// LCD and background on, $8000 tiles, $9800 map.
    fn compat_ppu() -> Ppu {
        let mut p = Ppu::with_model(Model::Cgb);
        p.enter_compat_mode(crate::compat::DEFAULT_PALETTES);
        p.lcdc = 0x91;
        p.bgp = 0b11_10_01_00;
        p
    }

    /// The color index (through BGP's identity shades) at (x, 0) in
    /// compatibility mode.
    fn compat_index(p: &Ppu, x: usize) -> usize {
        let bg = crate::compat::DEFAULT_PALETTES.bg;
        (0..4)
            .find(|&i| rgba_at(p, x, 0) == rgba(bg[i]))
            .expect("a background color")
    }

    /// Runs line 0 to the first dot, past pixel 16, after which the
    /// fetcher reads a tile's low data byte; writes LCDC there and draws
    /// the rest of the line. Returns the line's color indexes.
    fn write_lcdc_before_a_low_byte_read(p: &mut Ppu, lcdc: u8) -> Vec<usize> {
        while p.dot < MODE3_DOT + 30 || p.next_fetch_step() != fifo::FetchStep::LowRead {
            p.tick(1);
        }
        p.write_reg(0xFF40, lcdc);
        lines(p, 1);
        (0..SCREEN_WIDTH).map(|x| compat_index(p, x)).collect()
    }

    #[test]
    fn clearing_tile_select_glitches_the_colors_next_tile_read() {
        // Tile 1 is solid color 3 at $8010 and blank at $9010. LCDC bit 4
        // cleared just before a low byte read: on the Color that byte is
        // the tile's number (1), so the tile shows color 1 in its last
        // pixel only. The original reads it normally: the tile mixes $8010's
        // low byte with $9010's high byte, color 1 all along.
        let setup = |mut p: Ppu| {
            put_tile(&mut p, 0x8010, striped(0xFF, 0xFF));
            for col in 0..32 {
                p.write_vram(0x9800 + col, 1);
            }
            p
        };
        let line = write_lcdc_before_a_low_byte_read(&mut setup(compat_ppu()), 0x81);
        let glitched = line.iter().position(|&i| i == 1).expect("a glitched tile");
        assert_eq!(line[glitched - 7..=glitched], [0, 0, 0, 0, 0, 0, 0, 1]);

        let mut p = setup(bg_ppu());
        p.compat = false;
        let line: Vec<usize> = {
            while p.dot < MODE3_DOT + 30 || p.next_fetch_step() != fifo::FetchStep::LowRead {
                p.tick(1);
            }
            p.write_reg(0xFF40, 0x81);
            lines(&mut p, 1);
            (0..SCREEN_WIDTH).map(|x| shade_at(&p, x, 0)).collect()
        };
        let mixed = line.iter().position(|&i| i == 1).expect("the mixed tile");
        assert_eq!(
            line[mixed..mixed + 9],
            [1, 1, 1, 1, 1, 1, 1, 1, 0],
            "{line:?}"
        );
    }

    #[test]
    fn in_double_speed_clearing_tile_select_doesnt_glitch() {
        // As in the test above, but the Color in double speed reads the
        // tile like the original: $8010's low byte, $9010's high byte, so
        // color 1 all along (AGE's m3-bg-lcdc-ds).
        let mut p = compat_ppu();
        p.double_speed = true;
        put_tile(&mut p, 0x8010, striped(0xFF, 0xFF));
        for col in 0..32 {
            p.write_vram(0x9800 + col, 1);
        }
        let line = write_lcdc_before_a_low_byte_read(&mut p, 0x81);
        let mixed = line.iter().position(|&i| i == 1).expect("the mixed tile");
        assert_eq!(line[mixed..mixed + 8], [1; 8], "{line:?}");
    }

    #[test]
    fn setting_tile_select_reads_back_the_colors_latched_byte() {
        // Bit 4 clear: tile 1 is blank ($9010). A sprite with sprites off
        // (the Color still fetches it) leaves its high byte, $0F, in the
        // latch. Setting bit 4 just before a low byte read: that byte is
        // the latch, the high byte comes from $8010 ($FF): colors 2 and 3.
        let mut p = compat_ppu();
        p.lcdc = 0x81;
        put_tile(&mut p, 0x8010, striped(0x00, 0xFF));
        put_tile(&mut p, 0x8020, striped(0x00, 0x0F));
        for col in 0..32 {
            p.write_vram(0x9800 + col, 1);
        }
        put_sprite(&mut p, 0, 0, 0, 2, 0);
        begin_line(&mut p);
        let line = write_lcdc_before_a_low_byte_read(&mut p, 0x91);
        let glitched = line.iter().position(|&i| i == 3).expect("a glitched tile");
        assert_eq!(line[glitched - 4..glitched + 4], [2, 2, 2, 2, 3, 3, 3, 3]);
    }

    #[test]
    fn the_window_at_wx_0_with_a_fine_scroll_costs_a_dot_on_both_consoles() {
        // WX 0 and SCX % 8 = 3: the window starts before the fine scroll
        // is thrown away, which costs a dot (Mealybug's
        // m3_window_timing_wx_0, on the original and the Color alike).
        let length = |mut p: Ppu, wx: u8| {
            p.lcdc |= 0x20;
            p.wx = wx;
            p.scx = 3;
            hblank_start(&mut p)
        };
        for p in [bg_ppu(), compat_ppu()] {
            let model = p.model;
            assert_eq!(length(p.clone(), 0), length(p, 7) + 1, "{model:?}");
        }
    }

    #[test]
    fn the_window_starting_over_where_it_is_pushes_a_blank_pixel_on_both_consoles() {
        // The window is showing from pixel 0; WX is moved to pixel 16 just
        // as the window reaches it, with a fresh row in the FIFO: one blank
        // pixel goes in there, on the Color too.
        for mut p in [window_ppu(7, 0), {
            let mut p = window_ppu(7, 0);
            p.model = Model::Cgb;
            p.enter_compat_mode(crate::compat::DEFAULT_PALETTES);
            p
        }] {
            p.tick(MODE3_DOT + 20);
            p.write_reg(0xFF4B, 7 + 16);
            lines(&mut p, 1);
            let px = |x| rgba_at(&p, x, 0);
            assert_ne!(px(16), px(15), "{:?}", p.model);
            assert_eq!(px(17), px(15), "{:?}", p.model);
        }
    }

    #[test]
    fn compatibility_mode_takes_shades_from_the_boot_palettes() {
        use crate::compat::DEFAULT_PALETTES;
        let mut p = Ppu::with_model(Model::Cgb);
        p.enter_compat_mode(DEFAULT_PALETTES);
        p.lcdc = 0x93; // on, tiles at $8000, sprites, background
        p.bgp = 0b00_01_10_11; // color 0 shows as shade 3
        p.obp1 = 0b00_00_10_00; // color 1 shows as shade 2
                                // Tile attributes in VRAM bank 1 are ignored (palette 7 here).
        p.set_vram_bank(1);
        p.write_vram(0x9800, 0x07);
        p.set_vram_bank(0);
        put_tile(&mut p, 0x8010, striped(0xFF, 0x00)); // tile 1: color 1
                                                       // A sprite on OBP1; the Color's palette bits in its attributes are
                                                       // ignored too.
        put_sprite(&mut p, 0, 8, 0, 1, 0x10 | 0x07);
        draw_line(&mut p, 0);
        assert_eq!(rgba_at(&p, 0, 0), rgba(DEFAULT_PALETTES.bg[3]));
        assert_eq!(rgba_at(&p, 8, 0), rgba(DEFAULT_PALETTES.obj[1][2]));
    }
}
