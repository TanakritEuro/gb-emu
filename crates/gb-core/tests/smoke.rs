//! End-to-end checks through the public API.

use gb_core::{
    cpu::CpuError, Button, FrameEnd, GameBoy, Model, Rewind, StateError, CYCLES_PER_FRAME,
};

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
fn ld_b_b_can_be_a_breakpoint_anywhere() {
    // $0150 LD A,1 ; $0152 LD B,B ; $0153 INC A ; $0154 JR $0152
    let program = [0x3E, 0x01, 0x40, 0x3C, 0x18, 0xFC];
    let mut gb = GameBoy::new(rom(&program)).unwrap();
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Done), "off by default");
    let mut gb = GameBoy::new(rom(&program)).unwrap();
    gb.set_ld_b_b_breakpoint(true);
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Breakpoint));
    assert_eq!((gb.cpu().regs.pc, gb.cpu().regs.a), (0x0152, 1));
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Breakpoint));
    assert_eq!(
        (gb.cpu().regs.pc, gb.cpu().regs.a),
        (0x0152, 2),
        "once round"
    );
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

/// A program that keeps most of the hardware busy: it fills tile 0 (so the
/// screen is dark), starts the timer with its interrupt, plays three sound
/// channels, then scribbles over work RAM forever.
fn busy_rom() -> Vec<u8> {
    #[rustfmt::skip]
    let code = [
        0x21, 0x00, 0x80,       // $0150 LD HL,$8000
        0x3E, 0xFF,             //       LD A,$FF
        0x06, 0x10,             //       LD B,16
        0x22,                   // $0157 LD (HL+),A
        0x05,                   //       DEC B
        0x20, 0xFC,             //       JR NZ,$0157
        0x21, 0x00, 0xC0,       //       LD HL,$C000
        0x3E, 0x05, 0xE0, 0x07, //       TAC: timer on, 262144 Hz
        0x3E, 0x80, 0xE0, 0x26, //       NR52: sound on
        0x3E, 0xF0, 0xE0, 0x12, //       NR12: CH1 volume
        0x3E, 0x87, 0xE0, 0x14, //       NR14: CH1 trigger
        0x3E, 0x80, 0xE0, 0x1A, //       NR30: CH3 DAC on
        0x3E, 0x87, 0xE0, 0x1E, //       NR34: CH3 trigger
        0x3E, 0xF0, 0xE0, 0x21, //       NR42: CH4 volume
        0x3E, 0x80, 0xE0, 0x23, //       NR44: CH4 trigger
        0x3E, 0x04, 0xE0, 0xFF, //       IE: timer
        0xFB,                   //       EI
        0x22,                   // $0183 LD (HL+),A
        0x85,                   //       ADD A,L
        0xCB, 0xAC,             //       RES 5,H (stay in $C000-$DFFF)
        0x18, 0xFA,             //       JR $0183
    ];
    let mut r = rom(&code);
    r[0x50] = 0xD9; // the timer interrupt handler: RETI
    r[0x14D] = r[0x134..=0x14C]
        .iter()
        .fold(0u8, |x, &b| x.wrapping_sub(b).wrapping_sub(1));
    r
}

/// Marks `rom` as a Game Boy Color game ($0143 = $80) and fixes its checksum.
fn color(mut rom: Vec<u8>) -> Vec<u8> {
    rom[0x143] = 0x80;
    rom[0x14D] = rom[0x134..=0x14C]
        .iter()
        .fold(0u8, |x, &b| x.wrapping_sub(b).wrapping_sub(1));
    rom
}

/// `busy_rom` as a Color game that first switches to double speed and to
/// WRAM bank 3, so its scribbling covers banked RAM.
fn color_busy_rom() -> Vec<u8> {
    let mut r = color(busy_rom());
    #[rustfmt::skip]
    let prelude = [
        0x3E, 0x01, 0xE0, 0x4D, // KEY1: arm the speed switch
        0x10, 0x00,             // STOP: switch
        0x3E, 0x03, 0xE0, 0x70, // SVBK: WRAM bank 3 at $D000
        0xC3, 0x50, 0x01,       // JP $0150 (busy_rom's program)
    ];
    r[0x200..0x200 + prelude.len()].copy_from_slice(&prelude);
    r[0x101..0x104].copy_from_slice(&[0xC3, 0x00, 0x02]); // entry: JP $0200
    r
}

#[test]
fn the_header_picks_the_model_and_the_boot_state_says_which() {
    let gb = GameBoy::new(rom(&COUNTER)).unwrap();
    assert_eq!((gb.model(), gb.cpu().regs.a), (Model::Dmg, 0x01));
    let gb = GameBoy::new(color(rom(&COUNTER))).unwrap();
    assert_eq!(
        (gb.model(), gb.cpu().regs.a),
        (Model::Cgb, 0x11),
        "A = $11 on a Color"
    );
    let gb = GameBoy::with_model(color(rom(&COUNTER)), Some(Model::Dmg)).unwrap();
    assert_eq!(
        (gb.model(), gb.cpu().regs.a),
        (Model::Dmg, 0x01),
        "overridden"
    );
}

#[test]
fn the_cpu_waits_while_general_purpose_dma_copies() {
    #[rustfmt::skip]
    let code = [
        0x3E, 0xC0, 0xE0, 0x51, // HDMA1: source $C000
        0xAF, 0xE0, 0x52,       // HDMA2
        0x3E, 0x80, 0xE0, 0x53, // HDMA3: destination $8000
        0xAF, 0xE0, 0x54,       // HDMA4
        0x3E, 0x03,             // 4 blocks, general purpose
        0xE0, 0x55,             // $0160 LDH ($FF55),A: go
        0x18, 0xFE,             // spin
    ];
    let mut gb = GameBoy::new(color(rom(&code))).unwrap();
    while gb.cpu().regs.pc != 0x0160 {
        gb.step().unwrap();
    }
    assert_eq!(gb.step(), Ok(12 + 4 * 32), "LDH, then 8 µs per block");
    assert_eq!(gb.peek(0xFF55), 0xFF);
}

#[test]
fn double_speed_fits_twice_the_cpu_work_in_a_frame() {
    #[rustfmt::skip]
    let code = [
        0x3E, 0x01, 0xE0, 0x4D, // KEY1: arm
        0x10, 0x00,             // STOP: switch (on a Color)
        0x03,                   // $0156 INC BC   (8 T-cycles)
        0x18, 0xFD,             //       JR $0156 (12): 20 a time round
    ];
    let loops_per_frame = |model| {
        let mut gb = GameBoy::with_model(color(rom(&code)), Some(model)).unwrap();
        gb.run_frame().unwrap(); // switched, and into the loop
        let before = gb.cpu().regs.bc();
        gb.run_frame().unwrap();
        u32::from(gb.cpu().regs.bc().wrapping_sub(before))
    };
    let normal = loops_per_frame(Model::Dmg); // STOP doesn't switch there
    let double = loops_per_frame(Model::Cgb);
    assert!(normal.abs_diff(70_224 / 20) <= 1, "{normal}");
    assert!(double.abs_diff(2 * 70_224 / 20) <= 1, "{double}");
}

#[test]
fn a_color_state_carries_on_exactly_like_the_original() {
    let mut gb = GameBoy::new(color_busy_rom()).unwrap();
    run_frames(&mut gb, 10);
    assert_eq!(gb.peek(0xFF4D), 0xFE, "double speed by now");
    assert_eq!(gb.peek(0xFF70), 0xFB, "WRAM bank 3");
    let saved = gb.save_state();
    run_frames(&mut gb, 20);
    let original = gb.save_state();
    gb.load_state(&saved).unwrap();
    run_frames(&mut gb, 20);
    assert!(gb.save_state() == original, "reloaded in place");
    let mut fresh = GameBoy::new(color_busy_rom()).unwrap();
    fresh.load_state(&saved).unwrap();
    run_frames(&mut fresh, 20);
    assert!(
        fresh.save_state() == original,
        "loaded into a fresh Game Boy Color"
    );
    // Same ROM run as the other model: refused rather than half-loaded, and
    // the error says which console the state is for.
    let mut dmg = GameBoy::with_model(color_busy_rom(), Some(Model::Dmg)).unwrap();
    let err = dmg.load_state(&saved).unwrap_err();
    assert_eq!(err, StateError::WrongModel(Model::Cgb));
    assert_eq!(
        err.to_string(),
        "this save state was made on the Game Boy Color"
    );
}

#[test]
fn an_original_cartridge_on_the_color_runs_in_compatibility_mode() {
    // LD A,1 ; LDH ($4F),A (VBK) ; then spin.
    let code = [0x3E, 0x01, 0xE0, 0x4F, 0x18, 0xFE];
    let mut gb = GameBoy::with_model(rom(&code), Some(Model::Cgb)).unwrap();
    let r = gb.cpu().regs;
    assert_eq!(
        (r.a, r.e, r.hl()),
        (0x11, 0x08, 0x007C),
        "the boot ROM's registers"
    );
    gb.run_frame().unwrap();
    assert_eq!(gb.peek(0xFF4F), 0xFE, "VRAM bank 1 out of reach");
    // The screen is in color: BGP's shade 0 shows as the boot palette's
    // white, and the boot ROM's BGP ($FC) shows everything else black.
    let px = &gb.framebuffer()[..4];
    assert_eq!(px, [0xFF, 0xFF, 0xFF, 0xFF]);
    // The same cartridge on the original isn't in color.
    let gb = GameBoy::new(rom(&code)).unwrap();
    assert_eq!((gb.cpu().regs.a, gb.model()), (0x01, Model::Dmg));
}

#[test]
fn a_chosen_palette_sticks_through_save_states_and_rewind() {
    // An empty screen: everything is BGP color 0, shade 0. That's white in
    // the boot ROM's palette, and $639F (cream) in Up + B's.
    let up_b = gb_core::compat::BUTTON_PALETTES
        .iter()
        .position(|&(name, _)| name == "Up + B")
        .and_then(gb_core::compat::button_palettes);
    let cream = [0xFF, 0xE7, 0xC6, 0xFF];
    let white = [0xFF, 0xFF, 0xFF, 0xFF];
    let corner = |gb: &GameBoy| <[u8; 4]>::try_from(&gb.framebuffer()[..4]).unwrap();

    let mut gb = GameBoy::with_model(rom(&[0x18, 0xFE]), Some(Model::Cgb)).unwrap();
    let mut rewind = Rewind::new(1, 100, 1 << 20);
    run_frames(&mut gb, 2);
    assert_eq!(corner(&gb), white);
    let saved = gb.save_state();
    rewind.record(&gb);

    assert!(gb.set_compat_palettes(up_b));
    run_frames(&mut gb, 1);
    assert_eq!(corner(&gb), cream);
    // A state and a rewind snapshot from before the choice keep it.
    gb.load_state(&saved).unwrap();
    run_frames(&mut gb, 1);
    assert_eq!(corner(&gb), cream, "after loading a state");
    assert!(rewind.step_back(&mut gb));
    run_frames(&mut gb, 1);
    assert_eq!(corner(&gb), cream, "after rewinding");
    // Back to the boot ROM's choice.
    gb.set_compat_palettes(None);
    run_frames(&mut gb, 1);
    assert_eq!(corner(&gb), white);
    // Not in compatibility mode: nothing to choose.
    let mut dmg = GameBoy::new(rom(&[0x18, 0xFE])).unwrap();
    assert!(!dmg.is_compat() && !dmg.set_compat_palettes(up_b));
}

#[test]
fn a_state_saved_during_oam_dma_carries_on_the_copy() {
    // LD A,$80 ; LDH ($46),A ; then NOPs: DMA from VRAM runs under them.
    // (From WRAM it would take the bus the NOPs come from on the original.)
    let mut gb = GameBoy::new(rom(&[0x3E, 0x80, 0xE0, 0x46])).unwrap();
    for _ in 0..6 {
        gb.step().unwrap(); // NOP, JP, LD, LDH, two NOPs: mid-copy
    }
    assert_eq!(gb.peek(0xFF46), 0x80);
    let saved = gb.save_state();
    let after = |gb: &mut GameBoy| {
        for _ in 0..200 {
            gb.step().unwrap();
        }
        gb.save_state()
    };
    let original = after(&mut gb);
    let mut fresh = GameBoy::new(rom(&[0x3E, 0x80, 0xE0, 0x46])).unwrap();
    fresh.load_state(&saved).unwrap();
    assert!(after(&mut fresh) == original);
}

#[test]
fn a_players_press_lands_during_the_next_frame() {
    // LD A,$10 ; LDH ($00),A (read the A/B/Select/Start row) ; JR -2
    let mut gb = GameBoy::new(rom(&[0x3E, 0x10, 0xE0, 0x00, 0x18, 0xFE])).unwrap();
    gb.run_frame().unwrap();
    let a_held = |gb: &GameBoy| gb.peek(0xFF00) & 0x01 == 0;
    gb.set_button_during_frame(Button::A, true);
    assert!(!a_held(&gb), "not yet");
    gb.run_frame().unwrap();
    assert!(a_held(&gb), "somewhere in the frame");
    // A release queued after a press lands after it, whatever the delays.
    gb.set_button_during_frame(Button::A, false);
    gb.set_button_during_frame(Button::A, true);
    gb.set_button_during_frame(Button::A, false);
    gb.run_frame().unwrap();
    assert!(!a_held(&gb));
}

fn run_frames(gb: &mut GameBoy, n: usize) {
    for _ in 0..n {
        gb.run_frame().unwrap();
        gb.take_audio();
    }
}

/// Sends `byte` over the link cable, as master (SC = $81) or slave ($80),
/// waits for the transfer to finish, and stores what came back at $C000.
fn link_program(byte: u8, sc: u8) -> Vec<u8> {
    #[rustfmt::skip]
    let code = [
        0x3E, byte, 0xE0, 0x01, // SB = byte
        0x3E, sc, 0xE0, 0x02,   // SC: go (master) or listen (slave)
        0xF0, 0x02,             // $0158 LDH A,(SC)
        0x87,                   //       ADD A,A: bit 7 into carry
        0x38, 0xFB,             //       JR C,$0158 until it's done
        0xF0, 0x01,             //       LDH A,(SB)
        0xEA, 0x00, 0xC0,       //       LD ($C000),A
        0x18, 0xFE,             //       spin
    ];
    rom(&code)
}

/// Plays the host between two linked Game Boys: whatever one clocks out as
/// master goes to the other, and the answer comes back.
fn carry(from: &mut GameBoy, to: &mut GameBoy) {
    while let Some(byte) = from.take_link_out() {
        let back = to.link_clocked(byte);
        from.link_answer(back);
    }
}

/// Runs both for `frames` frames, carrying bytes across; returns how often
/// a frame stopped to wait for the partner.
fn run_linked(a: &mut GameBoy, b: &mut GameBoy, frames: usize) -> usize {
    let mut waits = 0;
    for _ in 0..frames {
        for flip in [false, true] {
            let (x, y) = if flip {
                (&mut *b, &mut *a)
            } else {
                (&mut *a, &mut *b)
            };
            while x.run_frame().unwrap() == FrameEnd::LinkWait {
                waits += 1;
                carry(x, y);
            }
            carry(x, y);
        }
    }
    waits
}

#[test]
fn two_linked_game_boys_swap_bytes() {
    let mut slave = GameBoy::new(link_program(0x99, 0x80)).unwrap();
    let mut master = GameBoy::new(link_program(0x42, 0x81)).unwrap();
    slave.plug_link(true);
    master.plug_link(true);
    run_frames(&mut slave, 1); // listening before the master starts
    let waits = run_linked(&mut master, &mut slave, 2);
    assert_eq!(master.peek(0xC000), 0x99, "the master got the slave's byte");
    assert_eq!(slave.peek(0xC000), 0x42, "and the slave the master's");
    assert!(waits > 0, "the master waited for the answer");
    for gb in [&master, &slave] {
        assert_eq!(gb.peek(0xFF0F) & 0x08, 0x08, "serial interrupt requested");
    }
}

#[test]
fn a_master_reads_ff_with_no_cable_or_nobody_listening() {
    let mut alone = GameBoy::new(link_program(0x42, 0x81)).unwrap();
    run_frames(&mut alone, 1);
    assert_eq!(alone.peek(0xC000), 0xFF, "no cable");

    // A cable, but the other side never sets SC to listen.
    let mut master = GameBoy::new(link_program(0x42, 0x81)).unwrap();
    let mut deaf = GameBoy::new(rom(&[0x18, 0xFE])).unwrap();
    master.plug_link(true);
    deaf.plug_link(true);
    run_linked(&mut master, &mut deaf, 2);
    assert_eq!(master.peek(0xC000), 0xFF);
}

#[test]
fn a_frame_cut_short_finishes_its_own_time_instead_of_starting_over() {
    // The master waits for its answer about 4100 T-cycles into the frame.
    let mut master = GameBoy::new(link_program(0x42, 0x81)).unwrap();
    master.plug_link(true);
    let div = |gb: &GameBoy| gb.peek(0xFF04); // +1 every 256 T-cycles
    let before = div(&master);
    assert_eq!(master.run_frame(), Ok(FrameEnd::LinkWait));
    master.take_link_out();
    master.link_answer(0x99);
    assert_eq!(master.run_frame(), Ok(FrameEnd::Done));
    // One frame in all (70224 / 256 = 274.3 DIV ticks), not one and a bit.
    let ticks = u32::from(div(&master).wrapping_sub(before));
    assert!((274 % 256..=275 % 256).contains(&ticks), "{ticks}");
    assert_eq!(master.peek(0xC000), 0x99);
}

#[test]
fn pulling_the_cable_frees_a_waiting_master() {
    let mut master = GameBoy::new(link_program(0x42, 0x81)).unwrap();
    master.plug_link(true);
    assert_eq!(master.run_frame(), Ok(FrameEnd::LinkWait));
    assert_eq!(master.run_frame(), Ok(FrameEnd::LinkWait), "still waiting");
    master.plug_link(false);
    run_frames(&mut master, 1);
    assert_eq!(master.peek(0xC000), 0xFF);
}

#[test]
fn a_loaded_state_carries_on_exactly_like_the_original() {
    let mut gb = GameBoy::new(busy_rom()).unwrap();
    run_frames(&mut gb, 10);
    let saved = gb.save_state();
    run_frames(&mut gb, 20);
    let original = gb.save_state();

    // Back into the same machine, which has moved on: anything a state
    // forgot would keep its later value and make the runs drift apart.
    gb.load_state(&saved).unwrap();
    run_frames(&mut gb, 20);
    assert!(gb.save_state() == original, "reloaded in place");

    // Into one that has only just booted: forgotten fields keep power-on values.
    let mut fresh = GameBoy::new(busy_rom()).unwrap();
    fresh.load_state(&saved).unwrap();
    run_frames(&mut fresh, 20);
    assert!(
        fresh.save_state() == original,
        "loaded into a fresh Game Boy"
    );
}

#[test]
fn a_loaded_state_shows_its_picture_without_moving_the_framebuffer() {
    let mut gb = GameBoy::new(busy_rom()).unwrap();
    run_frames(&mut gb, 2); // the dark tile is on screen now
    let state = gb.save_state();
    let mut fresh = GameBoy::new(busy_rom()).unwrap(); // still showing white
    let ptr = fresh.framebuffer().as_ptr();
    assert!(fresh.framebuffer() != gb.framebuffer());
    fresh.load_state(&state).unwrap();
    assert!(fresh.framebuffer() == gb.framebuffer(), "the saved picture");
    assert_eq!(fresh.framebuffer().as_ptr(), ptr, "at the same address");
}

#[test]
fn states_for_other_games_or_damaged_ones_are_refused_and_change_nothing() {
    let mut gb = GameBoy::new(busy_rom()).unwrap();
    run_frames(&mut gb, 3);
    let before = gb.save_state();
    let other = GameBoy::new(rom(&COUNTER)).unwrap().save_state();
    assert_eq!(gb.load_state(&other), Err(StateError::WrongGame));
    let mut damaged = before.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 0x40;
    assert!(matches!(
        gb.load_state(&damaged),
        Err(StateError::Corrupt(_))
    ));
    assert_eq!(gb.load_state(b"hello"), Err(StateError::NotAState));
    assert!(gb.save_state() == before, "nothing changed");
}

#[test]
fn host_settings_stay_when_a_state_loads() {
    let mut gb = GameBoy::new(rom(&COUNTER)).unwrap();
    let state = gb.save_state();
    gb.set_breakpoint(0x0152, true);
    gb.load_state(&state).unwrap();
    assert_eq!(
        gb.breakpoints().collect::<Vec<_>>(),
        [0x0152],
        "breakpoints"
    );
    assert_eq!(gb.run_frame(), Ok(FrameEnd::Breakpoint));
}

#[test]
fn rewinding_steps_back_through_exactly_the_recorded_states() {
    let mut gb = GameBoy::new(busy_rom()).unwrap();
    let mut rewind = Rewind::new(2, 1000, usize::MAX);
    let mut recorded = Vec::new();
    for frame in 1..=20 {
        run_frames(&mut gb, 1);
        rewind.record(&gb);
        if frame % 2 == 0 {
            recorded.push(gb.save_state());
        }
    }
    assert_eq!(rewind.len(), 10);
    assert_eq!(rewind.frames_available(), 20);
    // The busy program rewrites thousands of bytes a frame; even so the
    // history costs well under ten whole states.
    let whole = recorded[0].len();
    assert!(
        rewind.bytes() < 10 * whole,
        "{} vs {}",
        rewind.bytes(),
        10 * whole
    );

    for expected in recorded.iter().rev() {
        assert!(rewind.step_back(&mut gb));
        assert!(gb.save_state() == *expected);
    }
    assert!(!rewind.step_back(&mut gb), "nothing older");

    // Playing on after a rewind records a new future from there.
    run_frames(&mut gb, 2);
    rewind.record(&gb);
    rewind.record(&gb);
    assert_eq!(rewind.len(), 1);
}

#[test]
fn a_state_is_small() {
    let state = GameBoy::new(busy_rom()).unwrap().save_state();
    // 8 KiB work RAM + 8 KiB VRAM + the picture at 2 bits a pixel, and change.
    assert!(state.len() < 24 * 1024, "{} bytes", state.len());
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
