//! Mode 3, pixel by pixel. The PPU doesn't draw a line all at once: a
//! fetcher reads the background (or window) tile by tile into a pixel FIFO,
//! and one pixel leaves the FIFO per dot, colored through the palette
//! registers as they are at that moment. Reaching a sprite's X stalls the
//! fetcher while the sprite's row is fetched and mixed into a second FIFO.
//! So a register written mid-line shows from the pixel the PPU has got to,
//! and how long mode 3 lasts falls out of the work it does.
//!
//! https://gbdev.io/pandocs/pixel_fifo.html. The step-by-step timings follow
//! SameBoy's PPU (Core/display.c, https://github.com/LIJI32/SameBoy, MIT),
//! which matches Mealybug Tearoom's pictures of real hardware.

use super::{Ppu, Sprite, DMG_PALETTE, SCREEN_WIDTH};
use crate::state::{StateError, StateReader, StateWriter};
use crate::Model;

/// One pixel in a FIFO: its color index (0-3), its palette (0-7 on the
/// Color; for sprites on the original, 0 = OBP0, 1 = OBP1), its "background
/// over sprite" bit, and for sprites the OAM index that decides overlaps on
/// the Color.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Pixel {
    color: u8,
    palette: u8,
    bg_priority: bool,
    oam_index: u8,
}

impl Pixel {
    fn pack(self) -> [u8; 2] {
        [
            self.color | (self.palette << 2) | (u8::from(self.bg_priority) << 5),
            self.oam_index,
        ]
    }
    fn unpack([a, b]: [u8; 2]) -> Self {
        Self {
            color: a & 3,
            palette: (a >> 2) & 7,
            bg_priority: a & 0x20 != 0,
            oam_index: b,
        }
    }
}

/// A pixel FIFO: 8 pixels at most here (the hardware's holds 16, but the
/// fetcher only refills it when it's empty).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Fifo {
    pixels: [Pixel; 8],
    head: u8,
    len: u8,
}

impl Fifo {
    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn pop(&mut self) -> Option<Pixel> {
        if self.len == 0 {
            return None;
        }
        let p = self.pixels[usize::from(self.head)];
        self.head = (self.head + 1) & 7;
        self.len -= 1;
        Some(p)
    }

    /// A tile row's 8 pixels, leftmost first (bit 7 first, or bit 0 first
    /// when flipped). Only into an empty FIFO.
    fn push_row(&mut self, [lo, hi]: [u8; 2], palette: u8, bg_priority: bool, flip: bool) {
        self.head = 0;
        self.len = 8;
        for i in 0..8 {
            let bit = if flip { i } else { 7 - i };
            let color = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
            self.pixels[i] = Pixel {
                color,
                palette,
                bg_priority,
                oam_index: 0,
            };
        }
    }

    /// One blank pixel, into an empty FIFO.
    fn push_blank(&mut self) {
        self.head = 0;
        self.len = 1;
        self.pixels[0] = Pixel::default();
    }

    /// Mixes a sprite's row into the sprite FIFO: padded with transparent
    /// pixels to 8, each opaque sprite pixel goes where the FIFO's is
    /// transparent, or where it has lower priority (a higher `priority`).
    fn overlay_row(&mut self, [lo, hi]: [u8; 2], sprite: Pixel, flip: bool) {
        while self.len < 8 {
            self.pixels[usize::from((self.head + self.len) & 7)] = Pixel::default();
            self.len += 1;
        }
        for i in 0..8u8 {
            let bit = if flip { i } else { 7 - i };
            let color = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
            let target = &mut self.pixels[usize::from((self.head + i) & 7)];
            if color != 0 && (target.color == 0 || target.oam_index > sprite.oam_index) {
                *target = Pixel { color, ..sprite };
            }
        }
    }

    fn save(&self, w: &mut StateWriter) {
        w.bytes(&[self.head, self.len]);
        for p in self.pixels {
            w.bytes(&p.pack());
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let (head, len) = (r.u8()?, r.u8()?);
        if head > 7 || len > 8 {
            return Err(StateError::Corrupt("pixel FIFO"));
        }
        (self.head, self.len) = (head, len);
        for p in &mut self.pixels {
            let mut b = [0; 2];
            r.bytes(&mut b)?;
            *p = Pixel::unpack(b);
        }
        Ok(())
    }
}

/// The background fetcher's steps. A VRAM read takes 2 dots: the address is
/// worked out in the first, the byte read in the second. The push is tried
/// every dot until the FIFO is empty (and once right after the high byte).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum FetchStep {
    #[default]
    TileAddr,
    TileRead,
    LowAddr,
    LowRead,
    HighAddr,
    HighRead,
    Push,
}

/// Where mode 3 picks up when its wait runs out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Step {
    /// Not in mode 3.
    #[default]
    Idle,
    /// A new dot of drawing: does the window start here?
    Dot,
    /// Is there a sprite at this X to fetch?
    SpriteMatch,
    /// Letting the background fetcher get its row into the FIFO first.
    SpriteWait,
    SpriteAdvance,
    SpriteOam,
    SpriteLow,
    SpriteHigh,
    SpriteMix,
    /// Push a pixel to the screen and move the fetcher on.
    Output,
}

/// Mode 3's working state. The position counts pixels from -16: the first
/// 8 popped are junk the FIFO starts with, then SCX % 8 more are thrown
/// away for the fine scroll, and pixels 0-159 go to the screen.
#[derive(Debug, Clone, Default)]
pub(super) struct Mode3 {
    pub(super) step: Step,
    /// Dots to wait before the next step.
    wait: u8,
    fetch: FetchStep,
    tile_map_addr: u16,
    tile: u8,
    tile_attrs: u8,
    tile_data_addr: u16,
    tile_data: [u8; 2],
    bg: Fifo,
    sprites_fifo: Fifo,
    /// The pixel position, as a wrapping byte: 240-255 are -16 to -1.
    position: u8,
    /// The next screen column written.
    lcd_x: u8,
    window_active: bool,
    window_tile_x: u8,
    /// The window just started; the fine scroll has a quirk for it.
    window_fetching: bool,
    /// Sprites on this line, by X then OAM index; `next` is the next one.
    sprites: [Sprite; 10],
    sprite_count: u8,
    next: u8,
    sprite_fetching: bool,
    sprite_tile: u8,
    sprite_attrs: u8,
    sprite_data: [u8; 2],
    /// A pixel to push ahead of the FIFO (the window restart glitch).
    insert_pixel: bool,
    /// The window was turned off while it was being fetched: no blank
    /// pixels from it for the rest of the line (see `push_row`).
    no_window_glitch: bool,
    /// LCDC bit 5 as the original's window start check sees it: as of the
    /// last dot, so a write reaches it a dot after it reaches the fetcher.
    window_enable_seen: bool,
    /// The original's LCDC bit 5 went off just now: whether that was while
    /// the window was being fetched is looked at after the next dot (when
    /// the write's last stage lands).
    window_off_check: bool,
    /// The original's WX was written just now (for the next dot).
    wx_just_written: bool,
    /// The Color's LCDC bit 4 was just changed (`Some(set)`): a tile data
    /// read in the next dot glitches (see `read_tile_data`). Its write
    /// always runs that dot before the CPU goes on (`bus::WriteTiming`),
    /// so save states needn't hold it.
    tile_sel_glitch: Option<bool>,
    /// LCDC bit 4 as of when the tile data address was worked out.
    tiles_at_8000: bool,
    /// The byte the tile-select glitch latches and reads back.
    glitch_data: u8,
}

/// Pixel position -16, where every line starts.
const START: u8 = 0u8.wrapping_sub(16);

impl Ppu {
    /// The hardware is a Color (its fetcher timing), whatever mode it's in.
    fn color_hw(&self) -> bool {
        self.model == Model::Cgb
    }

    /// In mode 3, with the line's first pixel the next to go out.
    pub(crate) fn first_pixel_next(&self) -> bool {
        self.m3.step != Step::Idle && self.m3.position == 0
    }

    /// In mode 3, in the middle of fetching a sprite.
    pub(crate) fn fetching_sprite(&self) -> bool {
        self.m3.step != Step::Idle && self.m3.sprite_fetching
    }

    /// In mode 3, with the window just started (its first tile being
    /// fetched).
    #[cfg(test)]
    pub(super) fn fetching_window(&self) -> bool {
        self.m3.step != Step::Idle && self.m3.window_fetching
    }

    /// The fetcher's next step.
    #[cfg(test)]
    pub(super) fn next_fetch_step(&self) -> FetchStep {
        self.m3.fetch
    }

    /// LCDC bit 4 changed. On the Color, a tile data read in the next dot
    /// glitches (see [`read_tile_data`](Self::read_tile_data)).
    pub(super) fn tile_sel_changed(&mut self, set: bool) {
        if self.color_hw() {
            self.m3.tile_sel_glitch = Some(set);
        }
    }

    /// LCDC bit 1 went off. On the original, a sprite fetch under way is
    /// dropped: once its current wait runs out, drawing goes on as if the
    /// sprite weren't there (it's skipped, as sprites are with bit 1 off).
    pub(super) fn sprites_turned_off(&mut self) {
        if !self.color_hw() && self.fetching_sprite() {
            self.m3.sprite_fetching = false;
            self.m3.step = Step::Output;
        }
    }

    /// LCDC bit 5 went off. On the original, if that's while the window's
    /// first tile is being fetched, the window leaves no blank pixels (see
    /// [`push_row`](Self::push_row)) for the rest of the line.
    pub(super) fn window_turned_off(&mut self) {
        self.m3.window_off_check = !self.color_hw();
    }

    /// WX was written. On the original, the window's late start (see
    /// [`window_check`](Self::window_check)) can't happen in the next dot.
    pub(super) fn wx_written(&mut self) {
        self.m3.wx_just_written = !self.color_hw();
    }

    /// After each dot: what the original's window logic sees a dot late
    /// catches up. The CPU's writes to LCDC and WX on the original always
    /// run one more dot before it goes on (`bus::WriteTiming`), so the
    /// one-dot flags are never left set between instructions and save
    /// states needn't hold them.
    pub(super) fn end_dot(&mut self) {
        let m = &mut self.m3;
        m.wx_just_written = false;
        m.tile_sel_glitch = None;
        if std::mem::take(&mut m.window_off_check) && m.step != Step::Idle && m.window_fetching {
            m.no_window_glitch = true;
        }
        m.window_enable_seen = self.lcdc & 0x20 != 0;
    }

    /// Mode 3 begins: the FIFOs are emptied (the background one primed with
    /// 8 junk pixels), the sprites found by the OAM scan line up by X, and
    /// drawing starts 5 dots later.
    pub(super) fn start_mode3(&mut self) {
        // The window's "Y condition": once WY == LY at the start of a line,
        // it holds for the rest of the frame. https://gbdev.io/pandocs/Window.html
        if self.ly == self.wy {
            self.wy_triggered = true;
        }
        let (sprites, count) = self.scan_oam();
        let m = &mut self.m3;
        m.bg.clear();
        m.sprites_fifo.clear();
        m.bg.push_row([0, 0], 0, false, false);
        m.lcd_x = 0;
        m.position = START;
        m.fetch = FetchStep::TileAddr;
        m.window_active = false;
        m.window_fetching = false;
        m.insert_pixel = false;
        m.no_window_glitch = false;
        m.sprite_fetching = false;
        m.sprites = sprites;
        m.sprite_count = count;
        m.next = 0;
        m.step = Step::Dot;
        m.wait = 5;
    }

    /// The OAM scan: in OAM order, the first 10 sprites whose rows cover LY
    /// (X doesn't matter, so off-screen ones use up slots too), then sorted
    /// by X, OAM order breaking ties, which is the order the fetcher meets
    /// them in. https://gbdev.io/pandocs/OAM.html
    fn scan_oam(&self) -> ([Sprite; 10], u8) {
        let mut found = [Sprite::default(); 10];
        let mut count = 0;
        let height = self.sprite_height();
        for (index, &[y, x, tile, attrs]) in self.oam.as_chunks::<4>().0.iter().enumerate() {
            let top = i16::from(y) - 16;
            if (top..top + height).contains(&i16::from(self.ly)) {
                found[count] = Sprite {
                    y,
                    x,
                    tile,
                    attrs,
                    index: index as u8,
                };
                count += 1;
                if count == found.len() {
                    break;
                }
            }
        }
        found[..count].sort_by_key(|s| s.x); // stable: OAM order breaks ties
        (found, count as u8)
    }

    /// One dot of mode 3. Returns true when the line's last pixel went out:
    /// mode 3 ends and HBlank begins.
    pub(super) fn mode3_dot(&mut self) -> bool {
        if self.m3.wait > 0 {
            self.m3.wait -= 1;
            if self.m3.wait > 0 {
                return false;
            }
        }
        loop {
            let m = &mut self.m3;
            match m.step {
                Step::Idle => return false,
                Step::Dot => {
                    m.step = Step::SpriteMatch;
                    if self.window_check() {
                        return self.sleep(1);
                    }
                }
                Step::SpriteMatch => {
                    let x = self.sprite_match_x();
                    let m = &mut self.m3;
                    while m.next < m.sprite_count && m.sprites[usize::from(m.next)].x < x {
                        m.next += 1; // passed by (the fine scroll skipped over it)
                    }
                    let fetch = m.next < m.sprite_count
                        && m.sprites[usize::from(m.next)].x == x
                        && (self.lcdc & 0x02 != 0 || self.color_hw());
                    let m = &mut self.m3;
                    m.sprite_fetching = fetch;
                    m.step = if fetch {
                        Step::SpriteWait
                    } else {
                        Step::Output
                    };
                }
                // The background fetcher first finishes the row it's on and
                // gets something into its FIFO.
                Step::SpriteWait => {
                    if m.fetch < FetchStep::HighRead || m.bg.len == 0 {
                        self.advance_fetcher();
                        return self.sleep(1);
                    }
                    m.step = Step::SpriteAdvance;
                }
                // TODO(accuracy): on the Color (CPU CGB C), a sprite met with
                // the fetcher already at its high byte reads its data a dot
                // sooner than this (Mealybug's m3_lcdc_obj_size_change_scx);
                // dropping this dot for every sprite breaks the other tests.
                Step::SpriteAdvance => {
                    m.step = Step::SpriteOam;
                    self.advance_fetcher();
                    return self.sleep(1);
                }
                Step::SpriteOam => {
                    self.advance_fetcher();
                    let s = self.m3.sprites[usize::from(self.m3.next)];
                    let base = usize::from(s.index) * 4;
                    self.m3.sprite_tile = self.oam[base + 2];
                    self.m3.sprite_attrs = self.oam[base + 3];
                    self.m3.step = Step::SpriteLow;
                    return self.sleep(2);
                }
                Step::SpriteLow => {
                    let addr = self.sprite_row_addr();
                    self.m3.sprite_data[0] = self.vram[addr];
                    self.m3.step = Step::SpriteHigh;
                    return self.sleep(2);
                }
                Step::SpriteHigh => {
                    self.m3.sprite_fetching = false;
                    let addr = self.sprite_row_addr();
                    self.m3.sprite_data[1] = self.vram[addr + 1];
                    self.m3.glitch_data = self.m3.sprite_data[1];
                    self.m3.step = Step::SpriteMix;
                    return self.sleep(1);
                }
                Step::SpriteMix => {
                    let s = self.m3.sprites[usize::from(self.m3.next)];
                    let attrs = self.m3.sprite_attrs;
                    let pixel = Pixel {
                        color: 0,
                        palette: if self.cgb() {
                            attrs & 0x07
                        } else {
                            u8::from(attrs & 0x10 != 0)
                        },
                        bg_priority: attrs & 0x80 != 0,
                        // On the Color the lower OAM index wins an overlap;
                        // otherwise the sprite fetched first (smaller X) does.
                        oam_index: if self.cgb() && self.opri & 1 == 0 {
                            s.index
                        } else {
                            0
                        },
                    };
                    let data = self.m3.sprite_data;
                    self.m3
                        .sprites_fifo
                        .overlay_row(data, pixel, attrs & 0x20 != 0);
                    self.m3.next += 1;
                    self.m3.step = Step::SpriteMatch;
                }
                Step::Output => {
                    self.output_pixel();
                    self.advance_fetcher();
                    if self.m3.position == SCREEN_WIDTH as u8 {
                        self.finish_line();
                        return true;
                    }
                    self.m3.step = Step::Dot;
                    return self.sleep(1);
                }
            }
        }
    }

    /// Waits `dots` before the next step.
    fn sleep(&mut self, dots: u8) -> bool {
        self.m3.wait = dots;
        false
    }

    /// The X a sprite must have (OAM X, screen X + 8) to be fetched now.
    fn sprite_match_x(&self) -> u8 {
        let x = self.m3.position.wrapping_add(8);
        if x > START {
            0
        } else {
            x
        }
    }

    /// Starts the window if this is where it begins: WY reached, LCDC bit 5
    /// on (a dot late, on the original), and WX - 7 is the pixel position
    /// (WX 0 is special). The background FIFO is emptied and the fetcher
    /// starts over on the window's map. Returns true if that costs a dot
    /// (WX 0 with a fine scroll; on the Color too, its CPU CGB C pictures
    /// say, though SameBoy has it on the original only).
    fn window_check(&mut self) -> bool {
        let color = self.color_hw();
        let m = &mut self.m3;
        let mut extra_dot = false;
        let enabled = if color {
            self.lcdc & 0x20 != 0
        } else {
            m.window_enable_seen
        };
        if !m.window_active && self.wy_triggered && enabled {
            let pos = m.position;
            let activate = if self.wx == 0 {
                pos == 0u8.wrapping_sub(7)
                    || (pos == START && self.scx & 7 != 0)
                    || (0u8.wrapping_sub(15)..=0u8.wrapping_sub(8)).contains(&pos)
            } else if self.wx >= 166 + u8::from(color) {
                false
            } else if self.wx == pos.wrapping_add(7) {
                true
            } else if !color && self.wx == pos.wrapping_add(6) && !m.wx_just_written {
                // The original also starts it a pixel late, if it missed
                // its spot (switched on just after), unless WX was written
                // right then.
                // TODO(accuracy): SameBoy has some DMGs' LCD fall a column
                // behind here (the window's first pixel lands on the last
                // one drawn); Mealybug's DMG-blob pictures show no such shift.
                true
            } else {
                false
            };
            if activate {
                self.window_y = self.window_y.wrapping_add(1);
                m.window_tile_x = 0;
                m.bg.clear();
                extra_dot = self.wx == 0 && self.scx & 7 != 0;
                m.window_active = true;
                m.fetch = FetchStep::TileAddr;
                m.window_fetching = true;
            }
        }
        // The window starting over where it already is pushes a blank pixel
        // (on the Color too: Mealybug's CPU CGB C pictures, against SameBoy).
        if self.wx == m.position.wrapping_add(7)
            && m.window_active
            && !m.window_fetching
            && m.fetch == FetchStep::TileAddr
            && m.bg.len == 8
        {
            m.insert_pixel = true;
        }
        extra_dot
    }

    /// One step of the background/window fetcher. It reads LCDC, SCX, SCY
    /// and the tile map as it goes, so a change mid-line affects the tiles
    /// fetched from then on.
    fn advance_fetcher(&mut self) {
        let color = self.color_hw();
        let attributes = self.cgb();
        match self.m3.fetch {
            FetchStep::TileAddr => {
                if self.lcdc & 0x20 == 0 {
                    self.m3.window_active = false;
                }
                let y = self.fetcher_y();
                let m = &mut self.m3;
                let high_map = if m.window_active {
                    self.lcdc & 0x40 != 0
                } else {
                    self.lcdc & 0x08 != 0
                };
                let x = if m.window_active {
                    m.window_tile_x
                } else if m.position.wrapping_add(16) < 8 {
                    self.scx >> 3
                } else {
                    // The Color's fetcher runs a pixel behind, except while
                    // it's waiting on a sprite.
                    let behind = i32::from(color && !m.sprite_fetching);
                    let x = i32::from(self.scx) + i32::from(m.position) + 8 - behind;
                    ((x / 8) & 0x1F) as u8
                };
                let map = if high_map { 0x1C00 } else { 0x1800 };
                m.tile_map_addr = map + u16::from(x) + u16::from(y / 8) * 32;
                m.fetch = FetchStep::TileRead;
            }
            FetchStep::TileRead => {
                let m = &mut self.m3;
                m.tile = self.vram[usize::from(m.tile_map_addr)];
                m.tile_attrs = if attributes {
                    self.vram[0x2000 + usize::from(m.tile_map_addr)]
                } else {
                    0
                };
                m.fetch = FetchStep::LowAddr;
            }
            FetchStep::LowAddr => {
                self.m3.tile_data_addr = self.tile_row_addr();
                self.m3.tiles_at_8000 = self.lcdc & 0x10 != 0;
                self.m3.fetch = FetchStep::LowRead;
            }
            FetchStep::LowRead => {
                self.m3.tile_data[0] = self.read_tile_data(false);
                self.m3.fetch = FetchStep::HighAddr;
            }
            FetchStep::HighAddr => {
                self.m3.tile_data_addr = self.tile_row_addr() + 1;
                self.m3.tiles_at_8000 = self.lcdc & 0x10 != 0;
                self.m3.fetch = FetchStep::HighRead;
            }
            FetchStep::HighRead => {
                self.m3.tile_data[1] = self.read_tile_data(true);
                let m = &mut self.m3;
                if m.window_active {
                    m.window_tile_x = (m.window_tile_x + 1) & 0x1F;
                }
                m.fetch = FetchStep::Push;
                self.push_row();
            }
            FetchStep::Push => self.push_row(),
        }
    }

    /// A tile data byte, from the address the fetcher worked out a dot ago.
    /// On the Color, a read in the dot right after LCDC bit 4 changes
    /// glitches. Bit 4 cleared: the byte is the tile's number (when the
    /// address was worked out for $8000 tiles and the tile is below $80),
    /// and what the read should have given is latched. Bit 4 set: the byte
    /// is the latch. The latch also takes each sprite's high byte, and each
    /// tile's high byte read from $8000 tiles.
    /// https://github.com/mattcurrie/mealybug-tearoom-tests/blob/master/the-comprehensive-game-boy-ppu-documentation.md#tile_sel-bit-4
    /// (CPU revision C; the latch's rules fitted to Mealybug Tearoom's
    /// m3_lcdc_tile_sel_change pictures, building on SameBoy's version)
    /// TODO(accuracy): revision D and later glitch differently on a clear.
    fn read_tile_data(&mut self, high: bool) -> u8 {
        let m = &mut self.m3;
        let read = self.vram[usize::from(m.tile_data_addr)];
        let data = match m.tile_sel_glitch {
            None => read,
            Some(false) if m.tiles_at_8000 && m.tile & 0x80 == 0 => m.tile,
            Some(false) => read,
            Some(true) => return m.glitch_data,
        };
        if (high && m.tiles_at_8000) || m.tile_sel_glitch.is_some() {
            m.glitch_data = read;
        }
        data
    }

    /// The fetcher's last step: the row goes into the background FIFO once
    /// it's empty. On the original, with the window off (LCDC bit 5, as the
    /// window logic sees it, a dot late) but its Y reached, a push where
    /// the window would start (WX - 7 the pixel position; past the line's
    /// end, WX 0) puts a single blank pixel in instead, and the row waits
    /// for the next try: the window comparison still happens, and glitches
    /// the FIFO (SameBoy, https://github.com/LIJI32/SameBoy/issues/278).
    fn push_row(&mut self) {
        let color = self.color_hw();
        let m = &mut self.m3;
        if m.bg.len > 0 {
            return;
        }
        if !color && self.wy_triggered && !m.window_enable_seen && !m.no_window_glitch {
            let at = match m.position.wrapping_add(7) {
                x @ 0..=167 => x,
                _ => 0,
            };
            if self.wx == at {
                m.bg.push_blank();
                return;
            }
        }
        let attrs = m.tile_attrs;
        m.bg.push_row(
            m.tile_data,
            attrs & 0x07,
            attrs & 0x80 != 0,
            attrs & 0x20 != 0,
        );
        m.fetch = FetchStep::TileAddr;
    }

    /// The row of the background (LY + SCY) or window (its own line counter)
    /// the fetcher reads.
    fn fetcher_y(&self) -> u8 {
        if self.m3.window_active {
            self.window_y
        } else {
            self.ly.wrapping_add(self.scy)
        }
    }

    /// The address of the fetched tile's row: LCDC bit 4 picks $8000 or
    /// signed $8800 addressing; on the Color the tile's attributes pick the
    /// VRAM bank and can flip it vertically.
    fn tile_row_addr(&self) -> u16 {
        let m = &self.m3;
        let mut addr = if self.lcdc & 0x10 != 0 {
            u16::from(m.tile) * 16
        } else {
            0x1000u16.wrapping_add_signed(i16::from(m.tile as i8) * 16)
        };
        if m.tile_attrs & 0x08 != 0 {
            addr += 0x2000;
        }
        let flip = if m.tile_attrs & 0x40 != 0 { 7 } else { 0 };
        addr + u16::from((self.fetcher_y() & 7) ^ flip) * 2
    }

    /// The address of the fetched sprite's row, from LCDC bit 2 as it is
    /// now: 8x16 sprites ignore the tile number's low bit, and flip over
    /// all 16 rows. On the Color, attribute bit 3 picks the VRAM bank.
    fn sprite_row_addr(&self) -> usize {
        let m = &self.m3;
        let s = m.sprites[usize::from(m.next)];
        let tall = self.lcdc & 0x04 != 0;
        let mask = if tall { 15 } else { 7 };
        let mut row = self.ly.wrapping_sub(s.y.wrapping_sub(16)) & mask;
        if m.sprite_attrs & 0x40 != 0 {
            row ^= mask;
        }
        let tile = if tall {
            m.sprite_tile & 0xFE
        } else {
            m.sprite_tile
        };
        let bank = if self.cgb() && m.sprite_attrs & 0x08 != 0 {
            0x2000
        } else {
            0
        };
        bank + usize::from(tile) * 16 + usize::from(row) * 2
    }

    /// Pops a pixel off each FIFO, mixes them and puts the result on the
    /// screen, through the palettes as they are now. Pixels before
    /// position 0 are thrown away instead: the junk the FIFO started with,
    /// then the fine scroll's SCX % 8.
    fn output_pixel(&mut self) {
        let color_hw = self.color_hw();
        // A sprite at X = 0 still to be fetched holds everything up.
        let m = &self.m3;
        if m.next < m.sprite_count
            && m.sprites[usize::from(m.next)].x == 0
            && (self.lcdc & 0x02 != 0 || color_hw)
        {
            return;
        }
        let m = &mut self.m3;
        if m.bg.len == 0 {
            return;
        }
        let bg = if std::mem::take(&mut m.insert_pixel) {
            Pixel::default()
        } else {
            m.bg.pop().unwrap_or_default()
        };
        let mut bg_priority = bg.bg_priority;
        let mut sprite = None;
        if let Some(s) = m.sprites_fifo.pop() {
            if s.color != 0 && self.lcdc & 0x02 != 0 {
                bg_priority |= s.bg_priority;
                sprite = Some(s);
            }
        }

        // Throwing away the start: once the position's low bits reach
        // SCX's, what's left is skipped to -8, so SCX % 8 pixels go.
        let pos = m.position;
        if pos.wrapping_add(16) < 8 {
            // (With the window just started and SCX % 8 = 7, a pixel early.)
            if pos & 7 == self.scx & 7 || (m.window_fetching && pos & 7 == 6 && self.scx & 7 == 7) {
                m.position = 0u8.wrapping_sub(8);
            } else if pos == 0u8.wrapping_sub(9) {
                m.position = START;
                return;
            }
        }
        m.window_fetching = false;
        if m.position >= SCREEN_WIDTH as u8 {
            m.position = m.position.wrapping_add(1);
            return;
        }

        // LCDC bit 0 off: on the original the background is blank (color
        // 0, so it never covers a sprite); on the Color it just loses its
        // priority over sprites.
        let mut bg_color = bg.color;
        if self.lcdc & 0x01 == 0 {
            if self.cgb() {
                bg_priority = false;
            } else {
                bg_color = 0;
            }
        }
        if bg_color != 0 && bg_priority {
            sprite = None;
        }
        let rgba = match sprite {
            Some(s) => self.obj_color(s.palette, s.color),
            None => self.bg_color(bg.palette, bg_color),
        };
        let x = usize::from(self.m3.lcd_x);
        let i = (usize::from(self.ly) * SCREEN_WIDTH + x) * 4;
        self.framebuffer[i..i + 4].copy_from_slice(&rgba);
        self.m3.position += 1;
        self.m3.lcd_x += 1;
    }

    /// Mode 3 is over. Columns the LCD didn't get (it can fall behind the
    /// PPU) repeat the last color.
    fn finish_line(&mut self) {
        let row = usize::from(self.ly) * SCREEN_WIDTH;
        for x in usize::from(self.m3.lcd_x)..SCREEN_WIDTH {
            let fill: [u8; 4] = if x == 0 {
                self.bg_color(0, 0)
            } else {
                self.framebuffer[(row + x - 1) * 4..][..4]
                    .try_into()
                    .unwrap_or(DMG_PALETTE[0])
            };
            self.framebuffer[(row + x) * 4..][..4].copy_from_slice(&fill);
        }
        self.m3.position = START;
        self.m3.step = Step::Idle;
        self.m3.window_active = false;
    }

    pub(super) fn save_mode3(&self, w: &mut StateWriter) {
        let m = &self.m3;
        w.bytes(&[
            m.step as u8,
            m.wait,
            m.fetch as u8,
            m.tile,
            m.tile_attrs,
            m.tile_data[0],
            m.tile_data[1],
            m.position,
            m.lcd_x,
            u8::from(m.window_active),
            m.window_tile_x,
            u8::from(m.window_fetching),
            m.sprite_count,
            m.next,
            u8::from(m.sprite_fetching),
            m.sprite_tile,
            m.sprite_attrs,
            m.sprite_data[0],
            m.sprite_data[1],
            u8::from(m.insert_pixel),
            u8::from(m.no_window_glitch),
            u8::from(m.window_enable_seen),
            u8::from(m.tiles_at_8000),
            m.glitch_data,
        ]);
        w.u16(m.tile_map_addr);
        w.u16(m.tile_data_addr);
        m.bg.save(w);
        m.sprites_fifo.save(w);
        for s in m.sprites {
            w.bytes(&[s.y, s.x, s.tile, s.attrs, s.index]);
        }
    }

    pub(super) fn load_mode3(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let mut b = [0u8; 24];
        r.bytes(&mut b)?;
        let step = match b[0] {
            0 => Step::Idle,
            1 => Step::Dot,
            2 => Step::SpriteMatch,
            3 => Step::SpriteWait,
            4 => Step::SpriteAdvance,
            5 => Step::SpriteOam,
            6 => Step::SpriteLow,
            7 => Step::SpriteHigh,
            8 => Step::SpriteMix,
            9 => Step::Output,
            _ => return Err(StateError::Corrupt("PPU mode 3 step")),
        };
        let fetch = match b[2] {
            0 => FetchStep::TileAddr,
            1 => FetchStep::TileRead,
            2 => FetchStep::LowAddr,
            3 => FetchStep::LowRead,
            4 => FetchStep::HighAddr,
            5 => FetchStep::HighRead,
            6 => FetchStep::Push,
            _ => return Err(StateError::Corrupt("PPU fetcher step")),
        };
        if b[8] as usize > SCREEN_WIDTH || b[12] > 10 || b[13] > b[12] {
            return Err(StateError::Corrupt("PPU mode 3"));
        }
        let m = &mut self.m3;
        m.step = step;
        m.wait = b[1];
        m.fetch = fetch;
        m.tile = b[3];
        m.tile_attrs = b[4];
        m.tile_data = [b[5], b[6]];
        m.position = b[7];
        m.lcd_x = b[8];
        m.window_active = b[9] != 0;
        m.window_tile_x = b[10] & 0x1F;
        m.window_fetching = b[11] != 0;
        m.sprite_count = b[12];
        m.next = b[13];
        m.sprite_fetching = b[14] != 0;
        m.sprite_tile = b[15];
        m.sprite_attrs = b[16];
        m.sprite_data = [b[17], b[18]];
        m.insert_pixel = b[19] != 0;
        m.no_window_glitch = b[20] != 0;
        m.window_enable_seen = b[21] != 0;
        m.tiles_at_8000 = b[22] != 0;
        m.glitch_data = b[23];
        m.tile_sel_glitch = None;
        m.window_off_check = false;
        m.wx_just_written = false;
        // Addresses into VRAM (both banks): masked to stay inside it.
        m.tile_map_addr = r.u16()? & 0x1FFF;
        m.tile_data_addr = r.u16()? & 0x3FFF;
        m.bg.load(r)?;
        m.sprites_fifo.load(r)?;
        for s in &mut m.sprites {
            let mut e = [0u8; 5];
            r.bytes(&mut e)?;
            let [y, x, tile, attrs, index] = e;
            if index >= 40 {
                return Err(StateError::Corrupt("PPU sprite"));
            }
            *s = Sprite {
                y,
                x,
                tile,
                attrs,
                index,
            };
        }
        Ok(())
    }
}
