//! End-to-end checks through the public API.

use gb_core::{cpu::CpuError, GameBoy, CYCLES_PER_FRAME};

/// A 32 KiB ROM-only cartridge with a valid header and `code` at $0150.
/// The entry point at $0100 is NOP; JP $0150, like real games.
fn rom(code: &[u8]) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[0x100..0x104].copy_from_slice(&[0x00, 0xC3, 0x50, 0x01]);
    rom[0x134..0x13D].copy_from_slice(b"SMOKETEST");
    rom[0x150..0x150 + code.len()].copy_from_slice(code);
    rom[0x14D] = rom[0x134..=0x14C]
        .iter()
        .fold(0u8, |x, &b| x.wrapping_sub(b).wrapping_sub(1));
    rom
}

#[test]
fn runs_a_frame_of_a_tiny_program() {
    // $0150 LD SP,$FFFE ; $0153 XOR A ; $0154 JP $0153 (spin forever)
    let code = [0x31, 0xFE, 0xFF, 0xAF, 0xC3, 0x53, 0x01];
    let mut gb = GameBoy::new(rom(&code)).unwrap();
    assert_eq!(gb.title(), "SMOKETEST");
    gb.run_frame().unwrap();
    assert_eq!(gb.cpu().regs.sp, 0xFFFE);
    assert_eq!(gb.cpu().regs.a, 0);
    assert_eq!(gb.framebuffer().len(), 160 * 144 * 4);
}

#[test]
fn reports_the_first_missing_instruction() {
    // LD A,$42 isn't implemented yet. When it is, change this test to use an
    // opcode that still isn't (or delete it once the CPU is complete).
    let mut gb = GameBoy::new(rom(&[0x3E, 0x42])).unwrap();
    let err = gb.run_frame().unwrap_err();
    assert_eq!(
        err,
        CpuError::Unimplemented {
            opcode: 0x3E,
            cb_prefixed: false,
            pc: 0x0150
        }
    );
    assert_eq!(err.to_string(), "unimplemented opcode 3E at $0150");
}

#[test]
fn doctor_line_matches_gameboy_doctor_format() {
    let mut gb = GameBoy::new(rom(&[])).unwrap();
    gb.set_doctor_mode(true);
    assert_eq!(
        gb.doctor_line(),
        "A:01 F:B0 B:00 C:13 D:00 E:D8 H:01 L:4D SP:FFFE PC:0100 PCMEM:00,C3,50,01"
    );
    assert_eq!(gb.bus().read(0xFF44), 0x90);
}

#[test]
fn halt_with_nothing_pending_burns_cycles() {
    // HALT with IE = 0 never wakes, but the rest of the hardware keeps running.
    let mut gb = GameBoy::new(rom(&[0x76])).unwrap();
    for _ in 0..3 {
        gb.run_frame().unwrap();
    }
    assert!(gb.cpu().halted);
}

#[test]
fn a_frame_is_70224_cycles() {
    assert_eq!(CYCLES_PER_FRAME, 154 * 456);
}
