//! End-to-end checks through the public API.

use gb_core::{cpu::CpuError, FrameEnd, GameBoy, CYCLES_PER_FRAME};

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
fn a_debugger_step_runs_through_halt_to_the_interrupt_handler() {
    let code = [
        0xAF, //       XOR A
        0xE0, 0x0F, // LDH ($FF0F),A   clear IF
        0x3C, //       INC A
        0xE0, 0xFF, // LDH ($FFFF),A   IE = VBlank
        0xFB, //       EI
        0x76, //       HALT
    ];
    let mut gb = GameBoy::new(rom(&code)).unwrap();
    // The entry point's NOP and JP $0150, then the five instructions before HALT.
    for _ in 0..7 {
        assert!(gb.step_instruction(CYCLES_PER_FRAME).unwrap() <= 16);
    }
    assert_eq!(gb.cpu().regs.pc, 0x0157, "at HALT");
    // HALT, the sleep until VBlank, then the 20-cycle interrupt dispatch.
    let cycles = gb.step_instruction(2 * CYCLES_PER_FRAME).unwrap();
    assert!(cycles > 20 && cycles <= CYCLES_PER_FRAME, "{cycles}");
    assert_eq!(gb.cpu().regs.pc, 0x0040, "at the VBlank handler");
    assert!(!gb.cpu().halted);
}

#[test]
fn a_debugger_step_gives_up_on_a_halt_that_never_wakes() {
    let mut gb = GameBoy::new(rom(&[0x76])).unwrap(); // IE = 0
    gb.step().unwrap(); // NOP
    gb.step().unwrap(); // JP $0150
    let cycles = gb.step_instruction(1000).unwrap();
    assert!((1000..1004).contains(&cycles), "{cycles}");
    assert!(gb.cpu().halted);
}

#[test]
fn disassembles_from_memory() {
    let mut gb = GameBoy::new(rom(&[0x3E, 0x42])).unwrap();
    gb.step().unwrap(); // NOP at $0100
    let ins = gb.disassemble(gb.cpu().regs.pc); // JP $0150
    assert_eq!((ins.len, ins.text.as_str()), (3, "JP $0150"));
    assert_eq!(gb.disassemble(0x0150).text, "LD A,$42");
}

#[test]
fn peeking_reads_memory_without_running_anything() {
    // $0150 LD A,$42 ; LD ($C000),A ; spin
    let mut gb = GameBoy::new(rom(&[0x3E, 0x42, 0xEA, 0x00, 0xC0, 0x18, 0xFE])).unwrap();
    assert_eq!(gb.peek(0x0150), 0x3E, "ROM");
    for _ in 0..4 {
        gb.step().unwrap(); // NOP, JP, LD, LD
    }
    assert_eq!(gb.peek(0xC000), 0x42, "work RAM");
    assert_eq!(gb.peek(0xE000), 0x42, "its echo");
    let (pc, ly) = (gb.cpu().regs.pc, gb.peek(0xFF44));
    for _ in 0..100 {
        gb.peek(0xFF44);
    }
    assert_eq!(
        (gb.cpu().regs.pc, gb.peek(0xFF44)),
        (pc, ly),
        "no time passed"
    );
    assert_eq!(
        (gb.rom_bank(0x0000), gb.rom_bank(0x4000)),
        (0, 1),
        "ROM only"
    );
}

/// $0150 LD A,1 ; $0152 INC A ; $0153 JR $0152 (forever)
const COUNTER: [u8; 5] = [0x3E, 0x01, 0x3C, 0x18, 0xFD];

#[test]
fn a_breakpoint_stops_before_its_instruction_and_resuming_runs_it() {
    let mut gb = GameBoy::new(rom(&COUNTER)).unwrap();
    gb.set_breakpoint(0x0152, true);
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Breakpoint));
    assert_eq!(
        (gb.cpu().regs.pc, gb.cpu().regs.a),
        (0x0152, 1),
        "INC A not run yet"
    );
    // Resuming runs INC A rather than stopping on it again, and stops when
    // the loop comes back round.
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Breakpoint));
    assert_eq!((gb.cpu().regs.pc, gb.cpu().regs.a), (0x0152, 2));
    // A debugger step from there, then running, stops again next time round.
    gb.step_instruction(CYCLES_PER_FRAME).unwrap();
    assert_eq!(gb.cpu().regs.pc, 0x0153);
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Breakpoint));
    assert_eq!((gb.cpu().regs.pc, gb.cpu().regs.a), (0x0152, 3));
}

#[test]
fn a_breakpoint_on_the_first_instruction_of_a_frame_still_stops() {
    // Games that wait for VBlank start every frame at the same PC, so a run
    // must only skip the check when the debugger stopped it there.
    let mut gb = GameBoy::new(rom(&COUNTER)).unwrap();
    gb.set_breakpoint(0x0100, true);
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Breakpoint));
    assert_eq!(gb.cpu().regs.pc, 0x0100);
}

#[test]
fn resuming_from_a_pause_gets_past_a_breakpoint_set_right_there() {
    let mut gb = GameBoy::new(rom(&COUNTER)).unwrap();
    gb.run_frame().unwrap(); // paused somewhere in the loop
    let (pc, a) = (gb.cpu().regs.pc, gb.cpu().regs.a);
    gb.set_breakpoint(pc, true);
    assert_eq!(
        gb.run_frame(),
        Ok(FrameEnd::Breakpoint),
        "without: stops at once"
    );
    assert_eq!(gb.cpu().regs.a, a, "having run nothing");
    // The same again, but continuing the way a debugger's Resume does.
    let mut gb = GameBoy::new(rom(&COUNTER)).unwrap();
    gb.run_frame().unwrap();
    gb.set_breakpoint(pc, true);
    gb.resume_past_breakpoint();
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Breakpoint));
    assert_eq!(gb.cpu().regs.pc, pc, "stopped next time round");
    assert_eq!(gb.cpu().regs.a, a.wrapping_add(1), "after one more loop");
}

#[test]
fn breakpoints_can_be_listed_and_cleared() {
    let mut gb = GameBoy::new(rom(&COUNTER)).unwrap();
    gb.set_breakpoint(0x0153, true);
    gb.set_breakpoint(0x0150, true);
    gb.set_breakpoint(0x0150, true);
    assert_eq!(gb.breakpoints().collect::<Vec<_>>(), [0x0150, 0x0153]);
    gb.set_breakpoint(0x0153, false);
    gb.set_breakpoint(0x0150, false);
    assert_eq!(gb.breakpoints().count(), 0);
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Done));
}

#[test]
fn a_breakpoint_after_halt_waits_for_the_cpu_to_wake() {
    // HALT with IE = 0 never wakes: PC sits on $0151 the whole time, but the
    // instruction there never runs, so the frame runs to its end.
    let mut gb = GameBoy::new(rom(&[0x76, 0x00])).unwrap();
    gb.set_breakpoint(0x0151, true);
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Done));
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
