//! Headless runner for test ROMs.
//!
//! Blargg's test ROMs print their results over the serial port and end with
//! "Passed" or "Failed", so this runs frames until one of those shows up.
//!
//! Exit codes: 0 passed (or ran to the frame limit with no verdict expected),
//! 1 failed, 2 emulator or usage error, 3 frame limit hit with output but no verdict.

use gb_core::{GameBoy, CYCLES_PER_FRAME};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;

const USAGE: &str = "\
usage: gb-cli <rom.gb> [--frames N] [--doctor TRACE_FILE]

  --frames N            stop after N frames (default 3600, about a minute of Game Boy time)
  --doctor TRACE_FILE   write a Gameboy Doctor trace line before every instruction

examples:
  cargo run --release -p gb-cli -- \"roms/blargg/cpu_instrs/individual/06-ld r,r.gb\"
  cargo run --release -p gb-cli -- \"roms/blargg/cpu_instrs/individual/06-ld r,r.gb\" --doctor trace.log";

struct Args {
    rom: String,
    frames: u64,
    doctor: Option<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut rom = None;
    let mut frames = 3600;
    let mut doctor = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--doctor" => doctor = Some(args.next().ok_or("--doctor needs a file path")?),
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
        doctor,
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
    let mut gb = match GameBoy::new(rom) {
        Ok(gb) => gb,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    gb.set_doctor_mode(args.doctor.is_some());
    eprintln!("loaded \"{}\"", gb.title());

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

    let verdict = 'run: {
        for frame in 1..=args.frames {
            let result = match trace.as_mut() {
                Some(t) => run_frame_traced(&mut gb, t),
                None => gb.run_frame(),
            };

            let new_output = gb.take_serial_output();
            if !new_output.is_empty() {
                print!("{new_output}");
                let _ = io::stdout().flush();
                serial.push_str(&new_output);
            }

            if let Err(e) = result {
                eprintln!("\n✘ stopped in frame {frame}: {e}");
                break 'run 2;
            }
            if serial.contains("Passed") {
                eprintln!("\n✔ passed after {frame} frames");
                break 'run 0;
            }
            if serial.contains("Failed") {
                eprintln!("\n✘ failed after {frame} frames");
                break 'run 1;
            }
        }
        eprintln!("\nstopped after {} frames", args.frames);
        if serial.is_empty() {
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
    ExitCode::from(verdict)
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
