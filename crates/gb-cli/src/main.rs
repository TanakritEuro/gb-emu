//! Headless runner for test ROMs.
//!
//! Blargg's test ROMs print their results over the serial port and end with
//! "Passed" or "Failed"; Mooneye's send a fixed byte sequence (see `verdict`);
//! Blargg's later ones (dmg_sound, ...) leave a result in cartridge RAM (see `ram_verdict`).
//! Others leave it in the CPU's registers, the same Fibonacci numbers for a
//! pass: the AGE tests and SameSuite at an `LD B,B`, and the 2016 Mooneye
//! tests (Wilbert Pol's extended suite) at the unused opcode $ED.
//! This runs frames until one of those shows up.
//!
//! Exit codes: 0 passed (or ran to the frame limit with no verdict expected),
//! 1 failed, 2 emulator or usage error, 3 frame limit hit with output (or an
//! `LD B,B` without the pass registers) but no verdict.

use gb_core::cpu::CpuError;
use gb_core::{FrameEnd, GameBoy, Model, CYCLES_PER_FRAME, SCREEN_HEIGHT, SCREEN_WIDTH};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;

const USAGE: &str = "\
usage: gb-cli <rom.gb> [--frames N] [--model dmg|cgb] [--doctor TRACE_FILE] [--wav AUDIO_FILE]
              [--screenshot IMAGE_FILE]

  --frames N            stop after N frames (default 3600, about a minute of Game Boy time)
  --model dmg|cgb       run on the original or the Color (default: the Color for games whose
                        header says they support it, the original for the rest)
  --doctor TRACE_FILE   write a Gameboy Doctor trace line before every instruction
  --wav AUDIO_FILE      record the sound to a 48 kHz stereo WAV file
  --screenshot IMAGE_FILE  save the last frame as a PPM image (e.g. to compare with
                        dmg-acid2's or cgb-acid2's reference picture)

examples:
  cargo run --release -p gb-cli -- \"roms/blargg/cpu_instrs/individual/06-ld r,r.gb\"
  cargo run --release -p gb-cli -- \"roms/blargg/cpu_instrs/individual/06-ld r,r.gb\" --doctor trace.log";

struct Args {
    rom: String,
    frames: u64,
    model: Option<Model>,
    doctor: Option<String>,
    wav: Option<String>,
    screenshot: Option<String>,
}

/// Sample rate for --wav.
const WAV_RATE: u32 = 48_000;

fn parse_args() -> Result<Option<Args>, String> {
    let mut rom = None;
    let mut frames = 3600;
    let mut model = None;
    let mut doctor = None;
    let mut wav = None;
    let mut screenshot = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--doctor" => doctor = Some(args.next().ok_or("--doctor needs a file path")?),
            "--wav" => wav = Some(args.next().ok_or("--wav needs a file path")?),
            "--screenshot" => {
                screenshot = Some(args.next().ok_or("--screenshot needs a file path")?)
            }
            "--model" => {
                model = match args.next().as_deref() {
                    Some("dmg") => Some(Model::Dmg),
                    Some("cgb") => Some(Model::Cgb),
                    other => return Err(format!("--model needs dmg or cgb, not {other:?}")),
                }
            }
            "--frames" => {
                let n = args.next().ok_or("--frames needs a number")?;
                frames = n.parse().map_err(|_| format!("bad frame count: {n}"))?;
            }
            _ if rom.is_none() => rom = Some(arg),
            _ => return Err(format!("unexpected argument: {arg}")),
        }
    }
    let rom = rom.ok_or("no ROM given")?;
    Ok(Some(Args {
        rom,
        frames,
        model,
        doctor,
        wav,
        screenshot,
    }))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let rom = match std::fs::read(&args.rom) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: can't read {}: {e}", args.rom);
            return ExitCode::from(2);
        }
    };
    let mut gb = match GameBoy::with_model(rom, args.model) {
        Ok(gb) => gb,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    gb.set_doctor_mode(args.doctor.is_some());
    let model = match gb.model() {
        Model::Dmg => "Game Boy",
        Model::Cgb => "Game Boy Color",
    };
    eprintln!("loaded \"{}\" on a {model}", gb.title());

    let mut trace = match &args.doctor {
        Some(path) => match File::create(path) {
            Ok(f) => Some(BufWriter::new(f)),
            Err(e) => {
                eprintln!("error: can't create {path}: {e}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let mut serial = String::new();
    gb.set_sample_rate(WAV_RATE);
    gb.set_ld_b_b_breakpoint(true);
    // An LD B,B went by without the pass registers: a test that's done (and
    // failed), or just an LD B,B (Blargg's cpu_instrs runs them as tests).
    let mut ld_b_b_seen = false;
    let mut audio: Vec<f32> = Vec::new();

    let verdict = 'run: {
        for frame in 1..=args.frames {
            let result = match trace.as_mut() {
                Some(t) => run_frame_traced(&mut gb, t).map(|()| false),
                None => run_frame_checked(&mut gb, &mut ld_b_b_seen),
            };

            let sound = gb.take_audio();
            if args.wav.is_some() {
                audio.extend(sound);
            }

            let new_output = gb.take_serial_output();
            if !new_output.is_empty() {
                print!("{new_output}");
                let _ = io::stdout().flush();
                serial.push_str(&new_output);
            }

            match result {
                Ok(true) => {
                    eprintln!("\n✔ passed after {frame} frames (LD B,B with the pass registers)");
                    break 'run 0;
                }
                Ok(false) => {}
                Err(CpuError::Illegal { opcode: 0xED, .. }) if fibonacci(&gb) => {
                    eprintln!("\n✔ passed after {frame} frames ($ED with the pass registers)");
                    break 'run 0;
                }
                Err(CpuError::Illegal { opcode: 0xED, .. }) => {
                    eprintln!("\n✘ failed after {frame} frames ($ED without the pass registers)");
                    break 'run 1;
                }
                Err(e) => {
                    eprintln!("\n✘ stopped in frame {frame}: {e}");
                    break 'run 2;
                }
            }
            let from_ram = ram_verdict(&gb);
            if let Some((_, text)) = &from_ram {
                print!("{text}");
            }
            match verdict(&serial).or(from_ram.map(|(pass, _)| pass)) {
                Some(true) => {
                    eprintln!("\n✔ passed after {frame} frames");
                    break 'run 0;
                }
                Some(false) => {
                    eprintln!("\n✘ failed after {frame} frames");
                    break 'run 1;
                }
                None => {}
            }
        }
        eprintln!("\nstopped after {} frames", args.frames);
        if serial.is_empty() && !ld_b_b_seen {
            0
        } else {
            3
        }
    };

    if let Some(mut t) = trace {
        if let Err(e) = t.flush() {
            eprintln!("error: writing trace: {e}");
            return ExitCode::from(2);
        }
        eprintln!(
            "trace written to {}",
            args.doctor.as_deref().unwrap_or_default()
        );
    }
    if let Some(path) = &args.wav {
        if let Err(e) =
            File::create(path).and_then(|f| write_wav(BufWriter::new(f), &audio, WAV_RATE))
        {
            eprintln!("error: writing {path}: {e}");
            return ExitCode::from(2);
        }
        eprintln!(
            "{:.1} s of audio written to {path}",
            audio.len() as f64 / 2.0 / f64::from(WAV_RATE)
        );
    }
    if let Some(path) = &args.screenshot {
        if let Err(e) =
            File::create(path).and_then(|f| write_ppm(BufWriter::new(f), gb.framebuffer()))
        {
            eprintln!("error: writing {path}: {e}");
            return ExitCode::from(2);
        }
        eprintln!("screenshot written to {path}");
    }
    ExitCode::from(verdict)
}

/// Writes the RGBA screen as a binary PPM (P6): a tiny header, then RGB.
fn write_ppm(mut out: impl Write, rgba: &[u8]) -> io::Result<()> {
    write!(
        out,
        "P6
{SCREEN_WIDTH} {SCREEN_HEIGHT}
255
"
    )?;
    for px in rgba.as_chunks::<4>().0 {
        out.write_all(&px[..3])?;
    }
    out.flush()
}

/// Writes interleaved stereo f32 samples as a 16-bit PCM WAV file.
fn write_wav(mut out: impl Write, samples: &[f32], rate: u32) -> io::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + data_len).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    out.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    out.write_all(&1u16.to_le_bytes())?; // PCM
    out.write_all(&2u16.to_le_bytes())?; // stereo
    out.write_all(&rate.to_le_bytes())?;
    out.write_all(&(rate * 4).to_le_bytes())?; // bytes per second
    out.write_all(&4u16.to_le_bytes())?; // bytes per frame
    out.write_all(&16u16.to_le_bytes())?; // bits per sample
    out.write_all(b"data")?;
    out.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        let pcm = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        out.write_all(&pcm.to_le_bytes())?;
    }
    out.flush()
}

/// Mooneye's test ROMs send these six bytes over serial on a pass (the
/// Fibonacci numbers 3 5 8 13 21 34), and $42 ("B") six times on a fail.
/// See roms/mooneye-test-suite/README.markdown.
const MOONEYE_PASS: &str = "\x03\x05\x08\x0d\x15\x22";
const MOONEYE_FAIL: &str = "BBBBBB";

/// Pass (true), fail (false), or no verdict yet, from the serial output so far.
/// Understands Blargg's "Passed"/"Failed" text and Mooneye's byte signatures.
fn verdict(serial: &str) -> Option<bool> {
    if serial.contains("Passed") || serial.contains(MOONEYE_PASS) {
        Some(true)
    } else if serial.contains("Failed") || serial.contains(MOONEYE_FAIL) {
        Some(false)
    } else {
        None
    }
}

/// Blargg's later test ROMs (dmg_sound, mem_timing-2, oam_bug) report in
/// cartridge RAM instead of over serial: $DE $B0 $61 at $A001, a status at
/// $A000 ($80 while running, 0 for pass, else a failure code), and the result
/// text from $A004. Returns (passed, text) once the test has finished.
fn ram_verdict(gb: &GameBoy) -> Option<(bool, String)> {
    let bus = gb.bus();
    let signature = [bus.read(0xA001), bus.read(0xA002), bus.read(0xA003)];
    let status = bus.read(0xA000);
    if signature != [0xDE, 0xB0, 0x61] || status == 0x80 {
        return None;
    }
    let text: String = (0xA004..0xC000)
        .map(|addr| bus.read(addr))
        .take_while(|&b| b != 0)
        .map(char::from)
        .collect();
    Some((status == 0, text))
}

/// Runs a frame through, stopping at each `LD B,B` to look at the
/// registers. True if one of them came with the pass registers.
fn run_frame_checked(gb: &mut GameBoy, ld_b_b_seen: &mut bool) -> Result<bool, CpuError> {
    loop {
        match gb.run_frame()? {
            FrameEnd::Breakpoint if fibonacci(gb) => return Ok(true),
            FrameEnd::Breakpoint => *ld_b_b_seen = true,
            FrameEnd::Done | FrameEnd::LinkWait => return Ok(false),
        }
    }
}

/// B C D E H L hold 3 5 8 13 21 34: the pass signal of tests that report
/// in the registers.
fn fibonacci(gb: &GameBoy) -> bool {
    let r = &gb.cpu().regs;
    [r.b, r.c, r.d, r.e, r.h, r.l] == [3, 5, 8, 13, 21, 34]
}

fn run_frame_traced(gb: &mut GameBoy, out: &mut impl Write) -> Result<(), gb_core::cpu::CpuError> {
    let mut elapsed = 0;
    while elapsed < CYCLES_PER_FRAME {
        // Trace write errors (disk full) surface when the file is flushed.
        let _ = writeln!(out, "{}", gb.doctor_line());
        elapsed += gb.step()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ROM that runs `code` from $0150.
    fn rom(code: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x100..0x104].copy_from_slice(&[0x00, 0xC3, 0x50, 0x01]);
        rom[0x150..0x150 + code.len()].copy_from_slice(code);
        rom
    }

    /// LD B,3 LD C,5 LD D,8 LD E,13 LD H,21 LD L,34 (the pass registers).
    const FIBONACCI: [u8; 12] = [0x06, 3, 0x0E, 5, 0x16, 8, 0x1E, 13, 0x26, 21, 0x2E, 34];

    #[test]
    fn ld_b_b_with_the_fibonacci_registers_is_a_pass() {
        // An LD B,B before the registers are set doesn't count; the one after does.
        let code = [&[0x40][..], &FIBONACCI, &[0x40, 0x18, 0xFE]].concat();
        let mut gb = GameBoy::new(rom(&code)).unwrap();
        gb.set_ld_b_b_breakpoint(true);
        let mut seen = false;
        assert_eq!(run_frame_checked(&mut gb, &mut seen), Ok(true));
        assert!(seen, "the first LD B,B was noted");
    }

    #[test]
    fn the_old_mooneye_exit_is_ed_with_the_verdict_in_the_registers() {
        let code = [&FIBONACCI[..], &[0xED]].concat();
        let mut gb = GameBoy::new(rom(&code)).unwrap();
        assert!(matches!(
            gb.run_frame(),
            Err(CpuError::Illegal { opcode: 0xED, .. })
        ));
        assert!(fibonacci(&gb));
    }

    #[test]
    fn verdict_understands_blargg_and_mooneye() {
        assert_eq!(verdict("cpu_instrs\n\nPassed all tests\n"), Some(true));
        assert_eq!(verdict("01-special\n\nFailed #6\n"), Some(false));
        assert_eq!(verdict("\x03\x05\x08\x0d\x15\x22"), Some(true));
        assert_eq!(verdict("BBBBBB"), Some(false));
        assert_eq!(verdict("\x03\x05\x08"), None, "pass bytes still arriving");
        assert_eq!(verdict("running..."), None);
    }

    #[test]
    fn screenshots_are_ppm_with_the_alpha_dropped() {
        let mut rgba = vec![0u8; SCREEN_WIDTH * SCREEN_HEIGHT * 4];
        rgba[..8].copy_from_slice(&[1, 2, 3, 255, 4, 5, 6, 255]);
        let mut out = Vec::new();
        write_ppm(&mut out, &rgba).unwrap();
        let header = b"P6
160 144
255
";
        assert_eq!(&out[..header.len()], header);
        assert_eq!(&out[header.len()..header.len() + 6], [1, 2, 3, 4, 5, 6]);
        assert_eq!(out.len(), header.len() + 160 * 144 * 3);
    }

    #[test]
    fn wav_has_a_valid_header_and_16_bit_samples() {
        let mut out = Vec::new();
        write_wav(&mut out, &[0.0, 1.0, -1.0, 2.0], 48_000).unwrap();
        assert_eq!(&out[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 36 + 8);
        assert_eq!(&out[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([out[22], out[23]]), 2, "stereo");
        assert_eq!(u32::from_le_bytes(out[24..28].try_into().unwrap()), 48_000);
        assert_eq!(&out[36..40], b"data");
        let pcm: Vec<i16> = out[44..]
            .chunks(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(pcm, [0, i16::MAX, -i16::MAX, i16::MAX], "clamped to -1..1");
    }
}
