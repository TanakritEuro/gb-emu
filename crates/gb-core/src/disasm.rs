//! A disassembler: turns the bytes at an address back into text like
//! `LD A,($FF44)`, for the browser's debugger panel.
//!
//! It splits opcodes into the same x/y/z/p/q fields as [`crate::cpu`] (see
//! `Opcode` there), so the two tables line up group for group.
//!
//! Instructions are 1 to 3 bytes long, and nothing in memory marks where one
//! starts, so code can only be disassembled *forward* from a known start such
//! as PC. Starting mid-instruction, or on data, gives plausible nonsense.
//!
//! Syntax mostly follows the opcode table (https://gbdev.io/gb-opcodes/optables/),
//! with two changes that make a debugger easier to read: jump targets are
//! absolute (`JR NZ,$0158`, not an offset), and high-page loads show the full
//! address (`LDH ($FF40),A`, not `$40`).

use crate::cpu::Opcode;

/// One disassembled instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// Length in bytes, including any $CB prefix: 1 to 3.
    pub len: u16,
    pub text: String,
}

/// 8-bit operands, indexed by `y` or `z`.
const R: [&str; 8] = ["B", "C", "D", "E", "H", "L", "(HL)", "A"];
/// Register pairs for loads and 16-bit arithmetic, indexed by `p`.
const RP: [&str; 4] = ["BC", "DE", "HL", "SP"];
/// Register pairs for PUSH and POP, indexed by `p`.
const RP2: [&str; 4] = ["BC", "DE", "HL", "AF"];
/// Conditions, indexed by `y` (or `y - 4` for JR).
const CC: [&str; 4] = ["NZ", "Z", "NC", "C"];
/// ALU operations with their left operand, indexed by `y`.
const ALU: [&str; 8] = [
    "ADD A,", "ADC A,", "SUB ", "SBC A,", "AND ", "XOR ", "OR ", "CP ",
];
/// $CB rotates and shifts, indexed by `y`.
const ROT: [&str; 8] = ["RLC", "RRC", "RL", "RR", "SLA", "SRA", "SWAP", "SRL"];
/// Block 0's one-byte accumulator and flag ops, indexed by `y`.
const ACC: [&str; 8] = ["RLCA", "RRCA", "RLA", "RRA", "DAA", "CPL", "SCF", "CCF"];

/// Disassembles the instruction at `addr`, reading memory through `read`.
/// The 11 illegal opcodes come out as `DB $DD` (a data byte), length 1.
pub fn disassemble(read: impl Fn(u16) -> u8, addr: u16) -> Instruction {
    let at = |i: u16| read(addr.wrapping_add(i));
    let d8 = || format!("${:02X}", at(1));
    let a16 = || format!("${:04X}", u16::from_le_bytes([at(1), at(2)]));
    // JR's operand counts from the address after the instruction.
    let jr_target = || {
        format!(
            "${:04X}",
            addr.wrapping_add(2)
                .wrapping_add_signed(i16::from(at(1) as i8))
        )
    };
    // ADD SP,e8 and LD HL,SP+e8 take a signed byte: "-$02", or "$02" after `plus`.
    let e8 = |plus: &str| {
        let e = at(1) as i8;
        let sign = if e < 0 { "-" } else { plus };
        format!("{sign}${:02X}", e.unsigned_abs())
    };

    let byte = at(0);
    let op = Opcode::new(byte);
    let (y, z, p) = (usize::from(op.y), usize::from(op.z), usize::from(op.p));
    let (len, text): (u16, String) = match op.x {
        0 => match op.z {
            0 => match op.y {
                0 => (1, "NOP".into()),
                1 => (3, format!("LD ({}),SP", a16())),
                2 => (2, "STOP".into()),
                3 => (2, format!("JR {}", jr_target())),
                _ => (2, format!("JR {},{}", CC[y - 4], jr_target())),
            },
            1 if op.q == 0 => (3, format!("LD {},{}", RP[p], a16())),
            1 => (1, format!("ADD HL,{}", RP[p])),
            2 => {
                let mem = ["(BC)", "(DE)", "(HL+)", "(HL-)"][p];
                if op.q == 0 {
                    (1, format!("LD {mem},A"))
                } else {
                    (1, format!("LD A,{mem}"))
                }
            }
            3 if op.q == 0 => (1, format!("INC {}", RP[p])),
            3 => (1, format!("DEC {}", RP[p])),
            4 => (1, format!("INC {}", R[y])),
            5 => (1, format!("DEC {}", R[y])),
            6 => (2, format!("LD {},{}", R[y], d8())),
            _ => (1, ACC[y].into()),
        },
        1 if byte == 0x76 => (1, "HALT".into()),
        1 => (1, format!("LD {},{}", R[y], R[z])),
        2 => (1, format!("{}{}", ALU[y], R[z])),
        _ => match op.z {
            0 => match op.y {
                0..=3 => (1, format!("RET {}", CC[y])),
                4 => (2, format!("LDH ($FF{:02X}),A", at(1))),
                5 => (2, format!("ADD SP,{}", e8(""))),
                6 => (2, format!("LDH A,($FF{:02X})", at(1))),
                _ => (2, format!("LD HL,SP{}", e8("+"))),
            },
            1 if op.q == 0 => (1, format!("POP {}", RP2[p])),
            1 => (1, ["RET", "RETI", "JP HL", "LD SP,HL"][p].into()),
            2 => match op.y {
                0..=3 => (3, format!("JP {},{}", CC[y], a16())),
                4 => (1, "LDH (C),A".into()),
                5 => (3, format!("LD ({}),A", a16())),
                6 => (1, "LDH A,(C)".into()),
                _ => (3, format!("LD A,({})", a16())),
            },
            3 => match op.y {
                0 => (3, format!("JP {}", a16())),
                1 => (2, cb(at(1))),
                6 => (1, "DI".into()),
                7 => (1, "EI".into()),
                _ => (1, format!("DB ${byte:02X}")),
            },
            4 if op.y < 4 => (3, format!("CALL {},{}", CC[y], a16())),
            5 if op.q == 0 => (1, format!("PUSH {}", RP2[p])),
            5 if op.p == 0 => (3, format!("CALL {}", a16())),
            4 | 5 => (1, format!("DB ${byte:02X}")),
            6 => (2, format!("{}{}", ALU[y], d8())),
            _ => (1, format!("RST ${:02X}", op.y * 8)),
        },
    };
    Instruction { len, text }
}

/// The instruction after a $CB prefix.
fn cb(byte: u8) -> String {
    let op = Opcode::new(byte);
    let (y, z) = (usize::from(op.y), usize::from(op.z));
    match op.x {
        0 => format!("{} {}", ROT[y], R[z]),
        1 => format!("BIT {y},{}", R[z]),
        2 => format!("RES {y},{}", R[z]),
        _ => format!("SET {y},{}", R[z]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Disassembles `bytes` placed at `addr` (memory elsewhere reads $00).
    fn dis_at(addr: u16, bytes: &[u8]) -> (u16, String) {
        let read = |a: u16| {
            let i = usize::from(a.wrapping_sub(addr));
            bytes.get(i).copied().unwrap_or(0)
        };
        let ins = disassemble(read, addr);
        (ins.len, ins.text)
    }

    fn dis(bytes: &[u8]) -> (u16, String) {
        dis_at(0x0150, bytes)
    }

    fn text(bytes: &[u8]) -> String {
        dis(bytes).1
    }

    #[test]
    fn block_0_loads_and_arithmetic() {
        assert_eq!(dis(&[0x00]), (1, "NOP".into()));
        assert_eq!(dis(&[0x08, 0x34, 0x12]), (3, "LD ($1234),SP".into()));
        assert_eq!(dis(&[0x10, 0x00]), (2, "STOP".into()));
        assert_eq!(dis(&[0x01, 0xCD, 0xAB]), (3, "LD BC,$ABCD".into()));
        assert_eq!(text(&[0x31, 0xFE, 0xFF]), "LD SP,$FFFE");
        assert_eq!(text(&[0x29]), "ADD HL,HL");
        assert_eq!(text(&[0x02]), "LD (BC),A");
        assert_eq!(text(&[0x22]), "LD (HL+),A");
        assert_eq!(text(&[0x3A]), "LD A,(HL-)");
        assert_eq!(text(&[0x13]), "INC DE");
        assert_eq!(text(&[0x3B]), "DEC SP");
        assert_eq!(text(&[0x34]), "INC (HL)");
        assert_eq!(text(&[0x0D]), "DEC C");
        assert_eq!(dis(&[0x3E, 0x01]), (2, "LD A,$01".into()));
        assert_eq!(text(&[0x36, 0xFF]), "LD (HL),$FF");
        assert_eq!(text(&[0x07]), "RLCA");
        assert_eq!(text(&[0x27]), "DAA");
        assert_eq!(text(&[0x3F]), "CCF");
    }

    #[test]
    fn relative_jumps_show_the_absolute_target() {
        // The offset counts from the next instruction, at $0152.
        assert_eq!(dis(&[0x18, 0x04]), (2, "JR $0156".into()));
        assert_eq!(text(&[0x20, 0xFE]), "JR NZ,$0150", "-2 jumps to itself");
        assert_eq!(text(&[0x38, 0x80]), "JR C,$00D2", "-128");
        assert_eq!(
            dis_at(0xFFFE, &[0x28, 0x01]).1,
            "JR Z,$0001",
            "wraps around"
        );
    }

    #[test]
    fn block_1_and_2_registers_and_alu() {
        assert_eq!(text(&[0x41]), "LD B,C");
        assert_eq!(text(&[0x7E]), "LD A,(HL)");
        assert_eq!(text(&[0x70]), "LD (HL),B");
        assert_eq!(text(&[0x76]), "HALT", "where LD (HL),(HL) would be");
        assert_eq!(text(&[0x80]), "ADD A,B");
        assert_eq!(text(&[0x8E]), "ADC A,(HL)");
        assert_eq!(text(&[0x97]), "SUB A");
        assert_eq!(text(&[0x9A]), "SBC A,D");
        assert_eq!(text(&[0xA3]), "AND E");
        assert_eq!(text(&[0xAF]), "XOR A");
        assert_eq!(text(&[0xB4]), "OR H");
        assert_eq!(text(&[0xBD]), "CP L");
    }

    #[test]
    fn block_3_control_flow_and_stack() {
        assert_eq!(dis(&[0xC0]), (1, "RET NZ".into()));
        assert_eq!(text(&[0xC9]), "RET");
        assert_eq!(text(&[0xD9]), "RETI");
        assert_eq!(text(&[0xE9]), "JP HL");
        assert_eq!(text(&[0xF9]), "LD SP,HL");
        assert_eq!(text(&[0xC1]), "POP BC");
        assert_eq!(text(&[0xF1]), "POP AF");
        assert_eq!(text(&[0xF5]), "PUSH AF");
        assert_eq!(dis(&[0xC3, 0x50, 0x01]), (3, "JP $0150".into()));
        assert_eq!(text(&[0xDA, 0x00, 0x40]), "JP C,$4000");
        assert_eq!(dis(&[0xCD, 0x00, 0x20]), (3, "CALL $2000".into()));
        assert_eq!(text(&[0xC4, 0x00, 0x20]), "CALL NZ,$2000");
        assert_eq!(dis(&[0xFF]), (1, "RST $38".into()));
        assert_eq!(text(&[0xC7]), "RST $00");
        assert_eq!(text(&[0xF3]), "DI");
        assert_eq!(text(&[0xFB]), "EI");
        assert_eq!(dis(&[0xFE, 0x90]), (2, "CP $90".into()));
        assert_eq!(text(&[0xC6, 0x01]), "ADD A,$01");
    }

    #[test]
    fn high_page_and_absolute_loads() {
        assert_eq!(dis(&[0xE0, 0x40]), (2, "LDH ($FF40),A".into()));
        assert_eq!(dis(&[0xF0, 0x44]), (2, "LDH A,($FF44)".into()));
        assert_eq!(dis(&[0xE2]), (1, "LDH (C),A".into()));
        assert_eq!(text(&[0xF2]), "LDH A,(C)");
        assert_eq!(dis(&[0xEA, 0x00, 0xC0]), (3, "LD ($C000),A".into()));
        assert_eq!(text(&[0xFA, 0x00, 0xC0]), "LD A,($C000)");
    }

    #[test]
    fn stack_pointer_offsets_are_signed() {
        assert_eq!(dis(&[0xE8, 0x02]), (2, "ADD SP,$02".into()));
        assert_eq!(text(&[0xE8, 0xFE]), "ADD SP,-$02");
        assert_eq!(text(&[0xF8, 0x05]), "LD HL,SP+$05");
        assert_eq!(text(&[0xF8, 0x80]), "LD HL,SP-$80");
    }

    #[test]
    fn cb_prefixed_instructions_are_two_bytes() {
        assert_eq!(dis(&[0xCB, 0x00]), (2, "RLC B".into()));
        assert_eq!(text(&[0xCB, 0x1E]), "RR (HL)");
        assert_eq!(text(&[0xCB, 0x37]), "SWAP A");
        assert_eq!(text(&[0xCB, 0x3F]), "SRL A");
        assert_eq!(text(&[0xCB, 0x7C]), "BIT 7,H");
        assert_eq!(text(&[0xCB, 0x86]), "RES 0,(HL)");
        assert_eq!(text(&[0xCB, 0xFF]), "SET 7,A");
    }

    #[test]
    fn illegal_opcodes_are_data_bytes() {
        for byte in [
            0xD3, 0xDB, 0xDD, 0xE3, 0xE4, 0xEB, 0xEC, 0xED, 0xF4, 0xFC, 0xFD,
        ] {
            assert_eq!(dis(&[byte]), (1, format!("DB ${byte:02X}")));
        }
    }

    #[test]
    fn every_opcode_has_a_length_of_1_to_3() {
        for byte in 0..=255u8 {
            let (len, text) = dis(&[byte, 0xCB, 0x00]);
            assert!((1..=3).contains(&len), "{byte:02X}: {len}");
            assert!(!text.is_empty());
        }
    }

    /// The real check on lengths: run each instruction on the CPU and see how
    /// far PC moves. Jumps, calls, returns and RST are left out, since they
    /// move PC somewhere else (JR's zero offset lands on the next one, so it
    /// stays in); so are the illegal opcodes.
    #[test]
    fn lengths_match_how_far_the_cpu_moves_pc() {
        use crate::bus::Bus;
        use crate::cartridge::{tests::rom_with_program, Cartridge};
        use crate::cpu::Cpu;

        let run = |program: &[u8]| {
            let cart = Cartridge::from_rom(rom_with_program(program)).unwrap();
            let (mut cpu, mut bus) = (Cpu::new(), Bus::new(cart));
            cpu.reset_post_boot();
            let pc = cpu.regs.pc;
            cpu.step(&mut bus).unwrap();
            let len = disassemble(|a| bus.read(a), pc).len;
            (cpu.regs.pc.wrapping_sub(pc), len)
        };
        #[rustfmt::skip]
        let skip = [
            0xC0, 0xC8, 0xD0, 0xD8, 0xC9, 0xD9,       // RET cc, RET, RETI
            0xC2, 0xCA, 0xD2, 0xDA, 0xC3, 0xE9,       // JP cc, JP, JP HL
            0xC4, 0xCC, 0xD4, 0xDC, 0xCD,             // CALL cc, CALL
            0xD3, 0xDB, 0xDD, 0xE3, 0xE4, 0xEB, 0xEC, 0xED, 0xF4, 0xFC, 0xFD, // illegal
        ];
        let is_rst = |b: u8| b & 0xC7 == 0xC7;
        for byte in (0..=255u8).filter(|&b| !skip.contains(&b) && !is_rst(b)) {
            let (moved, len) = run(&[byte, 0x00, 0x00]);
            assert_eq!(moved, len, "opcode {byte:02X}");
        }
        for byte in 0..=255u8 {
            assert_eq!(run(&[0xCB, byte]), (2, 2), "opcode CB {byte:02X}");
        }
    }
}
