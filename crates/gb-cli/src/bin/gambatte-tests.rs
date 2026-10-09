//! Runs Gambatte's test suite (`roms/gambatte`) the way its testrunner.cpp
//! does, and reports what passes.
//!
//! Each test runs for 15 frames from the post-boot state, on the original
//! (`dmg08`) and/or the Color (`cgb04c`), as its file name says. Then:
//! - `_out<hex>`: the screen's top-left 8x8 tiles must show those hex digits,
//!   drawn in Gambatte's font, black on white;
//! - `_outaudio0` / `_outaudio1`: the last frame's sound must be silent
//!   (constant) / not;
//! - a PNG next to the ROM (`_dmg08.png`, `_cgb04c.png`, `_dmg08_cgb04c.png`):
//!   the whole screen must match it, in Gambatte's colors (greys on the
//!   original; on the Color its own RGB conversion).
//!
//! usage: gambatte-tests <dir or rom>... [--fail-list FILE]
//! Prints a line per failure and a summary per directory and console.

use gb_core::ppu::DMG_PALETTE;
use gb_core::{GameBoy, Model, SCREEN_HEIGHT, SCREEN_WIDTH};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const FRAMES: usize = 15;

fn main() -> ExitCode {
    let mut paths = Vec::new();
    let mut fail_list = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--fail-list" {
            fail_list = args.next();
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    if paths.is_empty() {
        eprintln!("usage: gambatte-tests <dir or rom>... [--fail-list FILE]");
        return ExitCode::from(2);
    }
    let mut roms = Vec::new();
    for p in &paths {
        collect_roms(p, &mut roms);
    }
    roms.sort();

    // (directory, console) -> (passed, run)
    let mut tally: BTreeMap<(String, &str), (u32, u32)> = BTreeMap::new();
    let mut failures = Vec::new();
    for rom in &roms {
        for (model, check) in checks(rom) {
            let console = if model == Model::Dmg { "dmg" } else { "cgb" };
            let (pass, shown) = run(rom, model, &check).unwrap_or((false, String::new()));
            let dir = rom
                .parent()
                .and_then(Path::file_name)
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default();
            let t = tally.entry((dir, console)).or_default();
            t.1 += 1;
            if pass {
                t.0 += 1;
            } else {
                failures.push(format!("{console} {} {shown}", rom.display()));
            }
        }
    }
    let (mut passed, mut run_total) = (0, 0);
    let mut per_console: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    for ((dir, console), (p, n)) in &tally {
        println!("{dir:<28} {console} {p:>4}/{n:<4}");
        passed += p;
        run_total += n;
        let c = per_console.entry(console).or_default();
        c.0 += p;
        c.1 += n;
    }
    for (console, (p, n)) in &per_console {
        println!("{console}: {p}/{n}");
    }
    println!("total: {passed}/{run_total}");
    if let Some(file) = fail_list {
        if let Err(e) = std::fs::write(&file, failures.join("\n") + "\n") {
            eprintln!("error: writing {file}: {e}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

fn collect_roms(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for e in entries.flatten() {
                collect_roms(&e.path(), out);
            }
        }
    } else if matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("gb" | "gbc")
    ) {
        out.push(path.to_path_buf());
    }
}

/// What a run is checked against.
enum Check {
    Hex(String),
    Audio(bool),
    Png(PathBuf),
}

/// The runs a ROM gets, by its file name (testrunner.cpp's main()).
fn checks(rom: &Path) -> Vec<(Model, Check)> {
    let stem = rom.with_extension("");
    let name = stem
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let after = |key: &str| -> Option<Check> {
        let rest = &name[name.find(key)? + key.len()..];
        Some(match rest {
            r if r.starts_with("audio0") => Check::Audio(false),
            r if r.starts_with("audio1") => Check::Audio(true),
            r => Check::Hex(r.chars().take_while(char::is_ascii_hexdigit).collect()),
        })
    };
    let mut runs = Vec::new();
    if name.contains("dmg08_cgb04c_out") {
        for model in [Model::Cgb, Model::Dmg] {
            runs.extend(after("dmg08_cgb04c_out").map(|c| (model, c)));
        }
    } else if name.contains("dmg08_out") {
        runs.extend(after("cgb04c_out").map(|c| (Model::Cgb, c)));
        runs.extend(after("dmg08_out").map(|c| (Model::Dmg, c)));
    } else if name.contains("_out") {
        runs.extend(after("_out").map(|c| (Model::Cgb, c)));
    }
    let png = |suffix: &str| {
        let p = PathBuf::from(format!("{}{suffix}", stem.display()));
        p.exists().then_some(p)
    };
    if let Some(p) = png("_dmg08_cgb04c.png") {
        runs.push((Model::Cgb, Check::Png(p.clone())));
        runs.push((Model::Dmg, Check::Png(p)));
    } else {
        if let Some(p) = png("_cgb04c.png") {
            runs.push((Model::Cgb, Check::Png(p)));
        }
        if let Some(p) = png("_dmg08.png") {
            runs.push((Model::Dmg, Check::Png(p)));
        }
    }
    runs
}

/// Runs `rom` for 15 frames on `model` and checks the result. Also
/// returns what a hex test showed (`?` for a tile that's no digit).
fn run(rom: &Path, model: Model, check: &Check) -> Option<(bool, String)> {
    let mut gb = GameBoy::with_model(std::fs::read(rom).ok()?, Some(model)).ok()?;
    gb.set_high_pass_filter(false);
    let mut audio = Vec::new();
    for _ in 0..FRAMES {
        // An illegal opcode is reported once; the CPU stays locked up and
        // the rest runs on, as on hardware (undef_ops).
        let _ = gb.run_frame();
        audio = gb.take_audio();
    }
    let screen = gambatte_colors(&gb, model);
    let shown = match check {
        Check::Hex(digits) => read_digits(&screen, digits.len()),
        _ => String::new(),
    };
    let pass = match check {
        Check::Hex(digits) => digits_match(&screen, digits),
        Check::Audio(sound) => {
            let first = audio.first().copied().unwrap_or(0.0);
            let silent = audio.iter().all(|&s| (s - first).abs() < 1e-6);
            silent != *sound
        }
        Check::Png(path) => match read_png(path) {
            Some(png) => png
                .iter()
                .zip(&screen)
                .all(|(a, b)| (a ^ b) & 0xF8F8F8 == 0),
            None => false,
        },
    };
    Some((pass, shown))
}

/// The hex digits the first `n` tiles of the top row show.
fn read_digits(screen: &[u32], n: usize) -> String {
    (0..n)
        .map(|i| {
            (0..16u32)
                .find(|&d| digits_match_at(screen, i, d))
                .and_then(|d| char::from_digit(d, 16))
                .map_or('?', |c| c.to_ascii_uppercase())
        })
        .collect()
}

/// The screen as Gambatte's testrunner sees it: 0xRRGGBB per pixel.
fn gambatte_colors(gb: &GameBoy, model: Model) -> Vec<u32> {
    gb.framebuffer()
        .chunks(4)
        .map(|px| {
            if model == Model::Dmg {
                let shade = DMG_PALETTE
                    .iter()
                    .position(|c| c[..3] == px[..3])
                    .unwrap_or(0);
                (3 - shade as u32) * 85 * 0x010101
            } else {
                // Back to 5-bit channels, then Gambatte's conversion.
                let [r, g, b] = [px[0] >> 3, px[1] >> 3, px[2] >> 3].map(u32::from);
                let red = (r * 13 + g * 2 + b) >> 1;
                let green = (g * 3 + b) << 1;
                let blue = (r * 3 + g * 2 + b * 11) >> 1;
                red << 16 | green << 8 | blue
            }
        })
        .collect()
}

/// Gambatte's 8x8 hex digits, a row a byte (bit 7 the left pixel), black
/// on white (testrunner.cpp's tileFromChar).
const DIGITS: [[u8; 8]; 16] = [
    [0x00, 0x7F, 0x41, 0x41, 0x41, 0x41, 0x41, 0x7F], // 0
    [0x00, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08], // 1
    [0x00, 0x7F, 0x01, 0x01, 0x7F, 0x40, 0x40, 0x7F], // 2
    [0x00, 0x7F, 0x01, 0x01, 0x3F, 0x01, 0x01, 0x7F], // 3
    [0x00, 0x41, 0x41, 0x41, 0x7F, 0x01, 0x01, 0x01], // 4
    [0x00, 0x7F, 0x40, 0x40, 0x7E, 0x01, 0x01, 0x7E], // 5
    [0x00, 0x7F, 0x40, 0x40, 0x7F, 0x41, 0x41, 0x7F], // 6
    [0x00, 0x7F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10], // 7
    [0x00, 0x3E, 0x41, 0x41, 0x3E, 0x41, 0x41, 0x3E], // 8
    [0x00, 0x7F, 0x41, 0x41, 0x7F, 0x01, 0x01, 0x7F], // 9
    [0x00, 0x08, 0x22, 0x41, 0x7F, 0x41, 0x41, 0x41], // A
    [0x00, 0x7E, 0x41, 0x41, 0x7E, 0x41, 0x41, 0x7E], // B
    [0x00, 0x3E, 0x41, 0x40, 0x40, 0x40, 0x41, 0x3E], // C
    [0x00, 0x7E, 0x41, 0x41, 0x41, 0x41, 0x41, 0x7E], // D
    [0x00, 0x7F, 0x40, 0x40, 0x7F, 0x40, 0x40, 0x7F], // E
    [0x00, 0x7F, 0x40, 0x40, 0x7F, 0x40, 0x40, 0x40], // F
];

/// Whether the screen's first tiles of the top row show `digits`.
fn digits_match(screen: &[u32], digits: &str) -> bool {
    digits.chars().enumerate().all(|(i, c)| {
        c.to_digit(16)
            .is_some_and(|d| digits_match_at(screen, i, d))
    })
}

/// Whether tile `i` of the top row shows hex digit `d`.
fn digits_match_at(screen: &[u32], i: usize, d: u32) -> bool {
    (0..8).all(|y| {
        (0..8).all(|x| {
            let px = screen[y * SCREEN_WIDTH + i * 8 + x] & 0xF8F8F8;
            let black = DIGITS[d as usize][y] & (0x80 >> x) != 0;
            px == if black { 0 } else { 0xF8F8F8 }
        })
    })
}

/// Reads an 8-bit RGBA, non-interlaced PNG of the screen's size, as
/// 0xRRGGBB per pixel.
fn read_png(path: &Path) -> Option<Vec<u32>> {
    let data = std::fs::read(path).ok()?;
    if data.get(..8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut pos = 8;
    let mut idat = Vec::new();
    let (mut width, mut height) = (0, 0);
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let kind = &data[pos + 4..pos + 8];
        let body = data.get(pos + 8..pos + 8 + len)?;
        match kind {
            b"IHDR" => {
                width = u32::from_be_bytes(body[0..4].try_into().ok()?) as usize;
                height = u32::from_be_bytes(body[4..8].try_into().ok()?) as usize;
                if body[8] != 8 || body[9] != 6 || body[12] != 0 {
                    return None; // only 8-bit RGBA, not interlaced
                }
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + len;
    }
    if (width, height) != (SCREEN_WIDTH, SCREEN_HEIGHT) {
        return None;
    }
    let raw = inflate(idat.get(2..)?)?; // skip the zlib header
    let stride = width * 4;
    let mut rows = vec![0u8; stride * height];
    for y in 0..height {
        let line = raw.get(y * (stride + 1)..(y + 1) * (stride + 1))?;
        let (filter, line) = (line[0], &line[1..]);
        for x in 0..stride {
            let a = if x >= 4 { rows[y * stride + x - 4] } else { 0 };
            let b = if y > 0 { rows[(y - 1) * stride + x] } else { 0 };
            let c = if x >= 4 && y > 0 {
                rows[(y - 1) * stride + x - 4]
            } else {
                0
            };
            let predicted = match filter {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                4 => {
                    let p = i16::from(a) + i16::from(b) - i16::from(c);
                    let (pa, pb, pc) = (
                        (p - i16::from(a)).abs(),
                        (p - i16::from(b)).abs(),
                        (p - i16::from(c)).abs(),
                    );
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => return None,
            };
            rows[y * stride + x] = line[x].wrapping_add(predicted);
        }
    }
    Some(
        rows.chunks(4)
            .map(|p| u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]))
            .collect(),
    )
}

/// DEFLATE (RFC 1951), after Mark Adler's puff.c.
fn inflate(input: &[u8]) -> Option<Vec<u8>> {
    struct Bits<'a> {
        data: &'a [u8],
        pos: usize,
        buf: u32,
        count: u32,
    }
    impl Bits<'_> {
        fn get(&mut self, n: u32) -> Option<u32> {
            while self.count < n {
                self.buf |= u32::from(*self.data.get(self.pos)?) << self.count;
                self.pos += 1;
                self.count += 8;
            }
            let v = self.buf & ((1u32 << n) - 1);
            self.buf >>= n;
            self.count -= n;
            Some(v)
        }
    }
    /// A canonical Huffman code: how many codes of each length, and the
    /// symbols in code order.
    struct Huffman {
        counts: [u16; 16],
        symbols: Vec<u16>,
    }
    impl Huffman {
        fn new(lengths: &[u8]) -> Self {
            let mut counts = [0u16; 16];
            for &l in lengths {
                counts[usize::from(l)] += 1;
            }
            counts[0] = 0;
            let mut offsets = [0u16; 16];
            for l in 1..15 {
                offsets[l + 1] = offsets[l] + counts[l];
            }
            let mut symbols = vec![0u16; lengths.len()];
            for (s, &l) in lengths.iter().enumerate() {
                if l != 0 {
                    symbols[usize::from(offsets[usize::from(l)])] = s as u16;
                    offsets[usize::from(l)] += 1;
                }
            }
            Self { counts, symbols }
        }

        fn decode(&self, bits: &mut Bits) -> Option<u16> {
            let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
            for len in 1..16 {
                code |= bits.get(1)? as i32;
                let count = i32::from(self.counts[len]);
                if code - count < first {
                    return self.symbols.get((index + code - first) as usize).copied();
                }
                index += count;
                first += count;
                first <<= 1;
                code <<= 1;
            }
            None
        }
    }
    const LEN_BASE: [u16; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LEN_EXTRA: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DIST_BASE: [u16; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DIST_EXTRA: [u8; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];
    let mut bits = Bits {
        data: input,
        pos: 0,
        buf: 0,
        count: 0,
    };
    let mut out = Vec::new();
    loop {
        let last = bits.get(1)? == 1;
        match bits.get(2)? {
            0 => {
                // Stored: byte-aligned length, its complement, the bytes.
                bits.buf = 0;
                bits.count = 0;
                let p = bits.pos;
                let len = usize::from(u16::from_le_bytes(input.get(p..p + 2)?.try_into().ok()?));
                out.extend_from_slice(input.get(p + 4..p + 4 + len)?);
                bits.pos = p + 4 + len;
            }
            kind @ (1 | 2) => {
                let (lit, dist) = if kind == 1 {
                    let mut lengths = [8u8; 288];
                    lengths[144..256].fill(9);
                    lengths[256..280].fill(7);
                    (Huffman::new(&lengths), Huffman::new(&[5; 30]))
                } else {
                    let nlen = bits.get(5)? as usize + 257;
                    let ndist = bits.get(5)? as usize + 1;
                    let ncode = bits.get(4)? as usize + 4;
                    const ORDER: [usize; 19] = [
                        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                    ];
                    let mut code_lengths = [0u8; 19];
                    for &i in &ORDER[..ncode] {
                        code_lengths[i] = bits.get(3)? as u8;
                    }
                    let code = Huffman::new(&code_lengths);
                    let mut lengths = vec![0u8; nlen + ndist];
                    let mut i = 0;
                    while i < nlen + ndist {
                        let sym = code.decode(&mut bits)?;
                        let (value, repeat) = match sym {
                            0..=15 => (sym as u8, 1),
                            16 => (*lengths.get(i.checked_sub(1)?)?, 3 + bits.get(2)?),
                            17 => (0, 3 + bits.get(3)?),
                            _ => (0, 11 + bits.get(7)?),
                        };
                        for _ in 0..repeat {
                            *lengths.get_mut(i)? = value;
                            i += 1;
                        }
                    }
                    (
                        Huffman::new(&lengths[..nlen]),
                        Huffman::new(&lengths[nlen..]),
                    )
                };
                loop {
                    let sym = usize::from(lit.decode(&mut bits)?);
                    match sym {
                        0..=255 => out.push(sym as u8),
                        256 => break,
                        _ => {
                            let s = sym - 257;
                            let len = usize::from(*LEN_BASE.get(s)?)
                                + bits.get(u32::from(LEN_EXTRA[s]))? as usize;
                            let d = usize::from(dist.decode(&mut bits)?);
                            let back = usize::from(*DIST_BASE.get(d)?)
                                + bits.get(u32::from(DIST_EXTRA[d]))? as usize;
                            let start = out.len().checked_sub(back)?;
                            for k in 0..len {
                                out.push(out[start + k]);
                            }
                        }
                    }
                }
            }
            _ => return None,
        }
        if last {
            return Some(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models(name: &str) -> Vec<Model> {
        checks(Path::new(name))
            .into_iter()
            .map(|(m, _)| m)
            .collect()
    }

    #[test]
    fn file_names_say_which_consoles_and_what_to_expect() {
        assert_eq!(
            models("x/a_dmg08_cgb04c_out3.gbc"),
            [Model::Cgb, Model::Dmg]
        );
        assert_eq!(
            models("x/a_dmg08_out1_cgb04c_out2.gbc"),
            [Model::Cgb, Model::Dmg]
        );
        assert_eq!(models("x/a_cgb04c_outE0.gbc"), [Model::Cgb]);
        assert_eq!(models("x/a_ds_1_out0.gbc"), [Model::Cgb]);
        assert!(models("x/dumper.gbc").is_empty());
        let (_, check) = checks(Path::new("x/a_dmg08_out1F_cgb04c_out2.gbc")).remove(1);
        assert!(matches!(check, Check::Hex(d) if d == "1F"));
        let (_, check) = checks(Path::new("x/a_dmg08_cgb04c_outaudio1.gbc")).remove(0);
        assert!(matches!(check, Check::Audio(true)));
    }

    #[test]
    fn digits_are_read_off_the_top_left_tiles() {
        let mut screen = vec![0xF8F8F8; SCREEN_WIDTH * SCREEN_HEIGHT];
        for (i, d) in [0xAusize, 0x7].into_iter().enumerate() {
            for y in 0..8 {
                for x in 0..8 {
                    if DIGITS[d][y] & (0x80 >> x) != 0 {
                        screen[y * SCREEN_WIDTH + i * 8 + x] = 0;
                    }
                }
            }
        }
        assert!(digits_match(&screen, "A7"));
        assert!(digits_match(&screen, "A"), "only as many as expected");
        assert!(!digits_match(&screen, "A8"));
    }

    #[test]
    fn inflate_reads_stored_fixed_and_dynamic_blocks() {
        assert_eq!(
            inflate(&[0x01, 0x02, 0x00, 0xFD, 0xFF, 0x68, 0x69]).unwrap(),
            b"hi"
        );
        let fixed = [0xCB, 0x48, 0xCD, 0xC9, 0xC9, 0x57, 0xC8, 0x40, 0x90, 0x00];
        assert_eq!(inflate(&fixed).unwrap(), b"hello hello hello");
        #[rustfmt::skip]
        let dynamic = [
            0xED, 0xC6, 0x49, 0x01, 0x00, 0x20, 0x08, 0x00, 0xB0, 0xAC, 0x78, 0x20, 0xDA, 0x3F,
            0x80, 0x35, 0x78, 0x6C, 0xAF, 0x45, 0xED, 0x71, 0x73, 0xBE, 0xB3, 0xC2, 0xCC, 0xCC,
            0xCC, 0xCC, 0xCC, 0x9A, 0xEE, 0x03,
        ];
        let expected: Vec<u8> = (0..3000).map(|i| b"abcdefghij"[(i * 7) % 10]).collect();
        assert_eq!(inflate(&dynamic).unwrap(), expected);
    }
}
