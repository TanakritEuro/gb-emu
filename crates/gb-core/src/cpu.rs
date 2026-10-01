//! The Sharp SM83 CPU.
//!
//! Opcodes are decoded by bit pattern (see `Opcode`); `Cpu::execute` is the
//! whole instruction table laid out by group. Groups that aren't written
//! yet return [`CpuError::Unimplemented`], which tells you exactly which
//! opcode to write next and where the game hit it.
//! TODO(milestone 1): every arm in `execute`/`execute_cb` returning `MISSING`.
//!
//! Opcode reference: https://gbdev.io/gb-opcodes/optables/
//! Cycle counts here are T-cycles (4 per M-cycle).

use crate::bus::Bus;
use std::fmt;

pub const FLAG_Z: u8 = 0x80;
pub const FLAG_N: u8 = 0x40;
pub const FLAG_H: u8 = 0x20;
pub const FLAG_C: u8 = 0x10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Registers {
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,
}

impl Registers {
    pub fn af(&self) -> u16 {
        u16::from_be_bytes([self.a, self.f])
    }
    /// The low nibble of F doesn't exist in hardware and always reads 0.
    pub fn set_af(&mut self, v: u16) {
        let [hi, lo] = v.to_be_bytes();
        self.a = hi;
        self.f = lo & 0xF0;
    }
    pub fn bc(&self) -> u16 {
        u16::from_be_bytes([self.b, self.c])
    }
    pub fn set_bc(&mut self, v: u16) {
        [self.b, self.c] = v.to_be_bytes();
    }
    pub fn de(&self) -> u16 {
        u16::from_be_bytes([self.d, self.e])
    }
    pub fn set_de(&mut self, v: u16) {
        [self.d, self.e] = v.to_be_bytes();
    }
    pub fn hl(&self) -> u16 {
        u16::from_be_bytes([self.h, self.l])
    }
    pub fn set_hl(&mut self, v: u16) {
        [self.h, self.l] = v.to_be_bytes();
    }
    pub fn flag(&self, mask: u8) -> bool {
        self.f & mask != 0
    }
    pub fn set_flag(&mut self, mask: u8, on: bool) {
        if on {
            self.f |= mask;
        } else {
            self.f &= !mask;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpuError {
    /// `pc` is the address of the opcode byte (or of the $CB prefix).
    Unimplemented {
        opcode: u8,
        cb_prefixed: bool,
        pc: u16,
    },
    /// One of the 11 unused opcodes ($D3 $DB $DD $E3 $E4 $EB $EC $ED $F4
    /// $FC $FD). Real hardware hard-locks until powered off; reaching one
    /// almost always means the CPU jumped somewhere it shouldn't have.
    /// https://gbdev.io/pandocs/CPU_Instruction_Set.html
    Illegal { opcode: u8, pc: u16 },
}

impl fmt::Display for CpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CpuError::Unimplemented {
                opcode,
                cb_prefixed,
                pc,
            } => {
                let prefix = if *cb_prefixed { "CB " } else { "" };
                write!(f, "unimplemented opcode {prefix}{opcode:02X} at ${pc:04X}")
            }
            CpuError::Illegal { opcode, pc } => write!(
                f,
                "illegal opcode {opcode:02X} at ${pc:04X} (real hardware locks up here)"
            ),
        }
    }
}

impl std::error::Error for CpuError {}

/// An opcode byte split into the bit fields the instruction set is built on:
///
/// ```text
///   bit  7 6 | 5 4 3 | 2 1 0
///         x  |   y   |   z
///            | p   q |
/// ```
///
/// `x` picks one of four blocks. Within a block, `y` and `z` usually name an
/// 8-bit operand in the order B C D E H L (HL) A (written `r[y]`, `r[z]`),
/// `p` names a register pair (`rp[p]`: BC DE HL SP, or `rp2[p]`: BC DE HL AF
/// for PUSH/POP), and `q` picks between two variants. Depending on the group,
/// `y` is instead a condition (`cc[y]`: NZ Z NC C), an ALU operation, a bit
/// number, or an RST target.
///
/// Search "decoding gbz80 opcodes" for the full table in this notation, or see
/// the per-block tables at https://gbdev.io/pandocs/CPU_Instruction_Set.html
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Opcode {
    x: u8,
    y: u8,
    z: u8,
    p: u8,
    q: u8,
}

impl Opcode {
    fn new(byte: u8) -> Self {
        let y = (byte >> 3) & 7;
        Self {
            x: byte >> 6,
            y,
            z: byte & 7,
            p: y >> 1,
            q: y & 1,
        }
    }
}

/// Why [`Cpu::execute`] couldn't run an opcode. `step` adds the address and
/// turns it into a [`CpuError`].
#[derive(Debug)]
enum Fault {
    Unimplemented,
    Illegal,
}

/// An instruction group that hasn't been written yet.
const MISSING: Result<u32, Fault> = Err(Fault::Unimplemented);
const ILLEGAL: Result<u32, Fault> = Err(Fault::Illegal);

#[derive(Debug, Clone, Default)]
pub struct Cpu {
    pub regs: Registers,
    /// Interrupt master enable.
    pub ime: bool,
    /// EI takes effect after the instruction that follows it.
    ime_pending: bool,
    pub halted: bool,
}

impl Cpu {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register values the DMG boot ROM leaves behind when it jumps to $0100.
    pub fn reset_post_boot(&mut self) {
        self.regs = Registers {
            a: 0x01,
            f: 0xB0,
            b: 0x00,
            c: 0x13,
            d: 0x00,
            e: 0xD8,
            h: 0x01,
            l: 0x4D,
            sp: 0xFFFE,
            pc: 0x0100,
        };
        self.ime = false;
        self.ime_pending = false;
        self.halted = false;
    }

    fn fetch8(&mut self, bus: &Bus) -> u8 {
        let v = bus.read(self.regs.pc);
        self.regs.pc = self.regs.pc.wrapping_add(1);
        v
    }

    fn fetch16(&mut self, bus: &Bus) -> u16 {
        let lo = self.fetch8(bus);
        let hi = self.fetch8(bus);
        u16::from_le_bytes([lo, hi])
    }

    /// Reads the 8-bit operand an opcode's 3-bit field names (`r[y]`/`r[z]`):
    /// B C D E H L (HL) A. Index 6 is the byte in memory at HL, which costs
    /// an extra 4 T-cycles per access; the caller's cycle count covers that.
    /// https://gbdev.io/pandocs/CPU_Instruction_Set.html
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "first used by the 8-bit load step")
    )]
    fn read_r8(&self, bus: &Bus, r: u8) -> u8 {
        match r {
            0 => self.regs.b,
            1 => self.regs.c,
            2 => self.regs.d,
            3 => self.regs.e,
            4 => self.regs.h,
            5 => self.regs.l,
            6 => bus.read(self.regs.hl()),
            _ => self.regs.a,
        }
    }

    /// Writes the 8-bit operand named by a 3-bit field. See [`Cpu::read_r8`].
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "first used by the 8-bit load step")
    )]
    fn write_r8(&mut self, bus: &mut Bus, r: u8, val: u8) {
        match r {
            0 => self.regs.b = val,
            1 => self.regs.c = val,
            2 => self.regs.d = val,
            3 => self.regs.e = val,
            4 => self.regs.h = val,
            5 => self.regs.l = val,
            6 => bus.write(self.regs.hl(), val),
            _ => self.regs.a = val,
        }
    }

    /// Executes one instruction and returns the T-cycles it took.
    pub fn step(&mut self, bus: &mut Bus) -> Result<u32, CpuError> {
        // TODO(milestone 2): if IME is set and (IE & IF) != 0, dispatch the
        // highest-priority interrupt here: clear its IF bit, clear IME, push PC,
        // jump to $40/$48/$50/$58/$60. 20 T-cycles.
        if self.halted {
            if bus.pending_interrupts() != 0 {
                self.halted = false;
            } else {
                return Ok(4);
            }
        }

        let enable_ime_after = std::mem::take(&mut self.ime_pending);
        let pc = self.regs.pc;
        let opcode = self.fetch8(bus);

        let cycles = match self.execute(Opcode::new(opcode), bus) {
            Ok(cycles) => cycles,
            Err(Fault::Unimplemented) => {
                // For CB-prefixed instructions, report the byte after $CB.
                let cb_prefixed = opcode == 0xCB;
                let opcode = if cb_prefixed {
                    bus.read(pc.wrapping_add(1))
                } else {
                    opcode
                };
                return Err(CpuError::Unimplemented {
                    opcode,
                    cb_prefixed,
                    pc,
                });
            }
            Err(Fault::Illegal) => return Err(CpuError::Illegal { opcode, pc }),
        };

        // EI followed directly by DI leaves interrupts disabled.
        if enable_ime_after && opcode != 0xF3 {
            self.ime = true;
        }
        Ok(cycles)
    }

    /// Runs one opcode whose byte has already been fetched. Returns T-cycles.
    ///
    /// Arms follow the four blocks (`x`), then `z`, then `y`/`p`/`q`.
    /// Notation is explained on [`Opcode`].
    fn execute(&mut self, op: Opcode, bus: &mut Bus) -> Result<u32, Fault> {
        match op.x {
            // Block 0: misc, 16-bit loads and arithmetic, INC/DEC, LD r,d8.
            0 => match op.z {
                0 => match op.y {
                    0 => Ok(4),   // NOP
                    1 => MISSING, // LD (a16), SP
                    2 => MISSING, // STOP
                    3 => MISSING, // JR e8
                    _ => MISSING, // JR cc[y-4], e8
                },
                1 => match (op.q, op.p) {
                    // LD SP, d16
                    (0, 3) => {
                        self.regs.sp = self.fetch16(bus);
                        Ok(12)
                    }
                    (0, _) => MISSING, // LD rp[p], d16
                    _ => MISSING,      // ADD HL, rp[p]
                },
                // q=0: LD (BC)/(DE)/(HL+)/(HL-), A
                // q=1: LD A, (BC)/(DE)/(HL+)/(HL-)
                2 => MISSING,
                3 => MISSING, // q=0: INC rp[p]  q=1: DEC rp[p]
                4 => MISSING, // INC r[y]
                5 => MISSING, // DEC r[y]
                6 => MISSING, // LD r[y], d8
                _ => MISSING, // y: RLCA RRCA RLA RRA DAA CPL SCF CCF
            },

            // Block 1: LD r[y], r[z]. The slot for LD (HL),(HL) is HALT.
            1 if op.y == 6 && op.z == 6 => {
                self.halted = true;
                Ok(4)
            }
            1 => MISSING,

            // Block 2: alu[y] A, r[z] with alu = ADD ADC SUB SBC AND XOR OR CP.
            // XOR A: A ^= A is always 0, so Z=1 and N/H/C=0.
            2 if op.y == 5 && op.z == 7 => {
                self.regs.a = 0;
                self.regs.f = FLAG_Z;
                Ok(4)
            }
            2 => MISSING,

            // Block 3: control flow, stack, high-page loads, immediate ALU.
            _ => match op.z {
                0 => match op.y {
                    0..=3 => MISSING, // RET cc[y]
                    4 => MISSING,     // LDH (a8), A
                    5 => MISSING,     // ADD SP, e8
                    6 => MISSING,     // LDH A, (a8)
                    _ => MISSING,     // LD HL, SP+e8
                },
                1 => match (op.q, op.p) {
                    (0, _) => MISSING, // POP rp2[p]
                    (_, 0) => MISSING, // RET
                    (_, 1) => MISSING, // RETI
                    (_, 2) => MISSING, // JP HL
                    _ => MISSING,      // LD SP, HL
                },
                2 => match op.y {
                    0..=3 => MISSING, // JP cc[y], a16
                    4 => MISSING,     // LDH (C), A
                    5 => MISSING,     // LD (a16), A
                    6 => MISSING,     // LDH A, (C)
                    _ => MISSING,     // LD A, (a16)
                },
                3 => match op.y {
                    // JP a16
                    0 => {
                        self.regs.pc = self.fetch16(bus);
                        Ok(16)
                    }
                    1 => self.execute_cb(bus),
                    // DI
                    6 => {
                        self.ime = false;
                        Ok(4)
                    }
                    // EI
                    7 => {
                        self.ime_pending = true;
                        Ok(4)
                    }
                    _ => ILLEGAL, // $D3 $DB $E3 $EB
                },
                4 => match op.y {
                    0..=3 => MISSING, // CALL cc[y], a16
                    _ => ILLEGAL,     // $E4 $EC $F4 $FC
                },
                5 => match (op.q, op.p) {
                    (0, _) => MISSING, // PUSH rp2[p]
                    (_, 0) => MISSING, // CALL a16
                    _ => ILLEGAL,      // $DD $ED $FD
                },
                6 => MISSING, // alu[y] A, d8
                _ => MISSING, // RST y*8
            },
        }
    }

    /// Fetches and runs the opcode after a $CB prefix. The T-cycles returned
    /// include the prefix: 8 with a register, 16 with (HL), 12 for BIT n,(HL).
    fn execute_cb(&mut self, bus: &mut Bus) -> Result<u32, Fault> {
        let op = Opcode::new(self.fetch8(bus));
        match op.x {
            0 => MISSING, // y: RLC RRC RL RR SLA SRA SWAP SRL, on r[z]
            1 => MISSING, // BIT y, r[z]
            2 => MISSING, // RES y, r[z]
            _ => MISSING, // SET y, r[z]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cartridge::tests::rom_with_program;
    use crate::cartridge::Cartridge;

    fn setup(program: &[u8]) -> (Cpu, Bus) {
        let cart = Cartridge::from_rom(rom_with_program(program)).unwrap();
        let mut cpu = Cpu::new();
        cpu.reset_post_boot();
        (cpu, Bus::new(cart))
    }

    #[test]
    fn register_pairs_round_trip() {
        let mut r = Registers::default();
        r.set_bc(0x1234);
        r.set_de(0xABCD);
        r.set_hl(0xBEEF);
        assert_eq!((r.b, r.c), (0x12, 0x34));
        assert_eq!(r.de(), 0xABCD);
        assert_eq!(r.hl(), 0xBEEF);
    }

    #[test]
    fn af_masks_low_nibble_of_f() {
        let mut r = Registers::default();
        r.set_af(0x12FF);
        assert_eq!(r.a, 0x12);
        assert_eq!(r.f, 0xF0);
    }

    #[test]
    fn nop_advances_pc() {
        let (mut cpu, mut bus) = setup(&[0x00]);
        assert_eq!(cpu.step(&mut bus), Ok(4));
        assert_eq!(cpu.regs.pc, 0x0101);
    }

    #[test]
    fn jp_jumps() {
        let (mut cpu, mut bus) = setup(&[0xC3, 0x50, 0x01]);
        assert_eq!(cpu.step(&mut bus), Ok(16));
        assert_eq!(cpu.regs.pc, 0x0150);
    }

    #[test]
    fn xor_a_clears_a_and_sets_only_z() {
        let (mut cpu, mut bus) = setup(&[0xAF]);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.a, 0);
        assert_eq!(cpu.regs.f, FLAG_Z);
    }

    #[test]
    fn ei_takes_effect_after_next_instruction() {
        let (mut cpu, mut bus) = setup(&[0xFB, 0x00]);
        cpu.step(&mut bus).unwrap();
        assert!(!cpu.ime);
        cpu.step(&mut bus).unwrap();
        assert!(cpu.ime);
    }

    #[test]
    fn r8_indices_follow_b_c_d_e_h_l_hl_a() {
        let (mut cpu, mut bus) = setup(&[]);
        cpu.regs.set_hl(0xC000);
        bus.write(0xC000, 0x66);
        cpu.regs.b = 0x00;
        cpu.regs.c = 0x11;
        cpu.regs.d = 0x22;
        cpu.regs.e = 0x33;
        cpu.regs.a = 0x77;
        // H and L are $C0/$00 here because they hold the pointer.
        let read: Vec<u8> = (0..8).map(|r| cpu.read_r8(&bus, r)).collect();
        assert_eq!(read, [0x00, 0x11, 0x22, 0x33, 0xC0, 0x00, 0x66, 0x77]);
    }

    #[test]
    fn write_r8_hits_the_named_register_only() {
        for r in [0, 1, 2, 3, 4, 5, 7] {
            let (mut cpu, mut bus) = setup(&[]);
            let before = cpu.regs;
            cpu.write_r8(&mut bus, r, 0xAB);
            assert_eq!(cpu.read_r8(&bus, r), 0xAB, "index {r}");
            let mut expected = before;
            match r {
                0 => expected.b = 0xAB,
                1 => expected.c = 0xAB,
                2 => expected.d = 0xAB,
                3 => expected.e = 0xAB,
                4 => expected.h = 0xAB,
                5 => expected.l = 0xAB,
                _ => expected.a = 0xAB,
            }
            assert_eq!(cpu.regs, expected, "index {r}");
        }
    }

    #[test]
    fn r8_index_6_is_memory_at_hl() {
        let (mut cpu, mut bus) = setup(&[]);
        cpu.regs.set_hl(0xC123);
        let before = cpu.regs;
        cpu.write_r8(&mut bus, 6, 0x5A);
        assert_eq!(bus.read(0xC123), 0x5A);
        assert_eq!(cpu.read_r8(&bus, 6), 0x5A);
        assert_eq!(cpu.regs, before, "(HL) writes leave registers alone");
    }

    #[test]
    fn opcode_splits_into_fields() {
        // HALT = 01 110 110
        assert_eq!(
            Opcode::new(0x76),
            Opcode {
                x: 1,
                y: 6,
                z: 6,
                p: 3,
                q: 0
            }
        );
        // CB prefix = 11 001 011
        assert_eq!(
            Opcode::new(0xCB),
            Opcode {
                x: 3,
                y: 1,
                z: 3,
                p: 0,
                q: 1
            }
        );
    }

    #[test]
    fn exactly_the_eleven_holes_are_illegal() {
        let illegal: Vec<u8> = (0..=255u8)
            .filter(|&op| {
                let (mut cpu, mut bus) = setup(&[op, 0x00, 0x00]);
                matches!(cpu.step(&mut bus), Err(CpuError::Illegal { .. }))
            })
            .collect();
        assert_eq!(
            illegal,
            [0xD3, 0xDB, 0xDD, 0xE3, 0xE4, 0xEB, 0xEC, 0xED, 0xF4, 0xFC, 0xFD]
        );
    }

    #[test]
    fn illegal_opcode_reports_where() {
        let (mut cpu, mut bus) = setup(&[0xDD]);
        let err = cpu.step(&mut bus).unwrap_err();
        assert_eq!(
            err,
            CpuError::Illegal {
                opcode: 0xDD,
                pc: 0x0100
            }
        );
        assert_eq!(
            err.to_string(),
            "illegal opcode DD at $0100 (real hardware locks up here)"
        );
    }

    #[test]
    fn unimplemented_cb_opcode_reports_the_second_byte() {
        let (mut cpu, mut bus) = setup(&[0xCB, 0x37]);
        let err = cpu.step(&mut bus).unwrap_err();
        assert_eq!(
            err,
            CpuError::Unimplemented {
                opcode: 0x37,
                cb_prefixed: true,
                pc: 0x0100
            }
        );
        assert_eq!(err.to_string(), "unimplemented opcode CB 37 at $0100");
    }

    #[test]
    fn ld_sp_d16_loads_sp() {
        let (mut cpu, mut bus) = setup(&[0x31, 0x34, 0x12]);
        assert_eq!(cpu.step(&mut bus), Ok(12));
        assert_eq!(cpu.regs.sp, 0x1234);
        assert_eq!(cpu.regs.pc, 0x0103);
    }

    #[test]
    fn halt_sets_halted() {
        let (mut cpu, mut bus) = setup(&[0x76]);
        assert_eq!(cpu.step(&mut bus), Ok(4));
        assert!(cpu.halted);
    }

    #[test]
    fn di_right_after_ei_keeps_interrupts_off() {
        let (mut cpu, mut bus) = setup(&[0xFB, 0xF3, 0x00]);
        for _ in 0..3 {
            cpu.step(&mut bus).unwrap();
        }
        assert!(!cpu.ime);
    }

    #[test]
    fn unimplemented_opcode_reports_where() {
        let (mut cpu, mut bus) = setup(&[0x00, 0x3E, 0x42]);
        cpu.step(&mut bus).unwrap();
        assert_eq!(
            cpu.step(&mut bus),
            Err(CpuError::Unimplemented {
                opcode: 0x3E,
                cb_prefixed: false,
                pc: 0x0101
            })
        );
    }
}
