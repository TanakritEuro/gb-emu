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
fn reports_illegal_opcodes() {
    // $DD is one of the 11 holes in the opcode table.
    let mut gb = GameBoy::new(rom(&[0xDD])).unwrap();
    let err = gb.run_frame().unwrap_err();
    assert_eq!(
        err,
        CpuError::Illegal {
            opcode: 0xDD,
            pc: 0x0150
        }
    );
    assert_eq!(
        err.to_string(),
        "illegal opcode DD at $0150 (real hardware locks up here)"
    );
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
fn framebuffer_never_moves() {
    // The browser keeps a view on this buffer in wasm memory instead of
    // copying it every frame, so its address must stay put, including when
    // the LCD is switched off (which blanks it) and back on.
    // $0150 XOR A ; LDH ($40),A (LCD off) ; LD A,$91 ; LDH ($40),A (on) ; JR -8
    let code = [0xAF, 0xE0, 0x40, 0x3E, 0x91, 0xE0, 0x40, 0x18, 0xF7];
    let mut gb = GameBoy::new(rom(&code)).unwrap();
    let addr = gb.framebuffer().as_ptr();
    for _ in 0..3 {
        gb.run_frame().unwrap();
        assert_eq!(gb.framebuffer().as_ptr(), addr);
        assert_eq!(gb.framebuffer().len(), 160 * 144 * 4);
    }
}

#[test]
fn battery_save_survives_a_power_cycle() {
    // An MBC1+RAM+BATTERY game that writes $42 to cartridge RAM:
    // LD A,$0A ; LD ($0000),A (enable RAM) ; LD A,$42 ; LD ($A000),A ; JR -2
    let code = [
        0x3E, 0x0A, 0xEA, 0x00, 0x00, 0x3E, 0x42, 0xEA, 0x00, 0xA0, 0x18, 0xFE,
    ];
    let mut image = rom(&code);
    image[0x147] = 0x03;
    image[0x149] = 0x02; // 8 KiB
    image[0x14D] = image[0x134..=0x14C]
        .iter()
        .fold(0u8, |x, &b| x.wrapping_sub(b).wrapping_sub(1));

    let mut gb = GameBoy::new(image.clone()).unwrap();
    assert!(gb.has_battery());
    gb.run_frame().unwrap();
    assert!(gb.take_save_dirty(), "the game wrote its save");
    let save = gb.save_data(1_700_000_000).unwrap();
    assert_eq!((save.len(), save[0]), (0x2000, 0x42));

    // Power off, power on with the same cartridge: the save comes back.
    let mut again = GameBoy::new(image).unwrap();
    again.load_save(&save, 1_700_000_100).unwrap();
    assert_eq!(again.save_data(1_700_000_100).unwrap(), save);
}

#[test]
fn a_frame_is_70224_cycles() {
    assert_eq!(CYCLES_PER_FRAME, 154 * 456);
}
