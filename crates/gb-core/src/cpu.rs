//! The Sharp SM83 CPU.
//!
//! Opcodes are decoded by bit pattern (see `Opcode`); `Cpu::execute` is the
//! whole instruction table laid out by group, and `Cpu::execute_cb` the
//! $CB-prefixed one. Every legal opcode is implemented; the 11 illegal ones
//! return [`CpuError::Illegal`].
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
    /// One of the 11 unused opcodes ($D3 $DB $DD $E3 $E4 $EB $EC $ED $F4
    /// $FC $FD). Real hardware hard-locks until powered off; reaching one
    /// almost always means the CPU jumped somewhere it shouldn't have.
    /// https://gbdev.io/pandocs/CPU_Instruction_Set.html
    Illegal { opcode: u8, pc: u16 },
}

impl fmt::Display for CpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
pub(crate) struct Opcode {
    pub(crate) x: u8,
    pub(crate) y: u8,
    pub(crate) z: u8,
    pub(crate) p: u8,
    pub(crate) q: u8,
}

impl Opcode {
    pub(crate) fn new(byte: u8) -> Self {
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

/// [`Cpu::execute`] hit an illegal opcode. `step` adds the opcode and address
/// and turns it into [`CpuError::Illegal`].
#[derive(Debug)]
struct Illegal;

const ILLEGAL: Result<u32, Illegal> = Err(Illegal);

#[derive(Debug, Clone, Default)]
pub struct Cpu {
    pub regs: Registers,
    /// Interrupt master enable.
    pub ime: bool,
    /// EI takes effect after the instruction that follows it.
    ime_pending: bool,
    pub halted: bool,
    /// Set by HALT when it hits the HALT bug: the next opcode fetch doesn't
    /// advance PC. See the HALT arm in `execute`.
    halt_bug: bool,
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
        self.halt_bug = false;
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

    /// Register pair `rp[p]`: BC DE HL SP. Used by 16-bit loads and arithmetic.
    fn read_rp(&self, p: u8) -> u16 {
        match p {
            0 => self.regs.bc(),
            1 => self.regs.de(),
            2 => self.regs.hl(),
            _ => self.regs.sp,
        }
    }

    fn write_rp(&mut self, p: u8, val: u16) {
        match p {
            0 => self.regs.set_bc(val),
            1 => self.regs.set_de(val),
            2 => self.regs.set_hl(val),
            _ => self.regs.sp = val,
        }
    }

    /// Register pair `rp2[p]`: BC DE HL AF. PUSH and POP use AF where other
    /// instructions use SP.
    fn read_rp2(&self, p: u8) -> u16 {
        match p {
            3 => self.regs.af(),
            _ => self.read_rp(p),
        }
    }

    /// Writing AF goes through [`Registers::set_af`], which zeroes F's low
    /// nibble: those four flag bits don't exist in hardware.
    fn write_rp2(&mut self, p: u8, val: u16) {
        match p {
            3 => self.regs.set_af(val),
            _ => self.write_rp(p, val),
        }
    }

    /// Pushes a word: high byte to SP-1, then low byte to SP-2, in that order,
    /// as the hardware does. The stack grows downward.
    fn push16(&mut self, bus: &mut Bus, val: u16) {
        let [lo, hi] = val.to_le_bytes();
        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, hi);
        self.regs.sp = self.regs.sp.wrapping_sub(1);
        bus.write(self.regs.sp, lo);
    }

    /// Pops a word: low byte from SP, then high byte from SP+1.
    fn pop16(&mut self, bus: &Bus) -> u16 {
        let lo = bus.read(self.regs.sp);
        self.regs.sp = self.regs.sp.wrapping_add(1);
        let hi = bus.read(self.regs.sp);
        self.regs.sp = self.regs.sp.wrapping_add(1);
        u16::from_le_bytes([lo, hi])
    }

    /// Sets all four flags at once (F's low nibble stays 0).
    fn set_flags(&mut self, z: bool, n: bool, h: bool, c: bool) {
        self.regs.f = u8::from(z) << 7 | u8::from(n) << 6 | u8::from(h) << 5 | u8::from(c) << 4;
    }

    /// `alu[y] A, val`: ADD ADC SUB SBC AND XOR OR CP, picked by `y`.
    ///
    /// H is the carry out of bit 3 (for subtraction, the borrow into bit 4)
    /// and C the carry out of bit 7 (or the borrow). ADC and SBC count the
    /// incoming carry in both. AND always sets H; that's how the hardware is.
    /// https://gbdev.io/pandocs/CPU_Registers_and_Flags.html
    fn alu(&mut self, y: u8, val: u8) {
        let a = self.regs.a;
        let carry_in = u8::from(self.regs.flag(FLAG_C));
        match y {
            // ADD, ADC
            0 | 1 => {
                let c = if y == 1 { carry_in } else { 0 };
                let (sum, c1) = a.overflowing_add(val);
                let (sum, c2) = sum.overflowing_add(c);
                let h = (a & 0x0F) + (val & 0x0F) + c > 0x0F;
                self.regs.a = sum;
                self.set_flags(sum == 0, false, h, c1 || c2);
            }
            // SUB, SBC, and CP, which is SUB without storing the result
            2 | 3 | 7 => {
                let c = if y == 3 { carry_in } else { 0 };
                let (diff, b1) = a.overflowing_sub(val);
                let (diff, b2) = diff.overflowing_sub(c);
                let h = (a & 0x0F) < (val & 0x0F) + c;
                if y != 7 {
                    self.regs.a = diff;
                }
                self.set_flags(diff == 0, true, h, b1 || b2);
            }
            4 => {
                self.regs.a = a & val;
                self.set_flags(self.regs.a == 0, false, true, false);
            }
            5 => {
                self.regs.a = a ^ val;
                self.set_flags(self.regs.a == 0, false, false, false);
            }
            _ => {
                self.regs.a = a | val;
                self.set_flags(self.regs.a == 0, false, false, false);
            }
        }
    }

    /// INC for 8-bit operands. Leaves C alone, unlike ADD.
    fn inc8(&mut self, val: u8) -> u8 {
        let r = val.wrapping_add(1);
        self.regs.set_flag(FLAG_Z, r == 0);
        self.regs.set_flag(FLAG_N, false);
        self.regs.set_flag(FLAG_H, val & 0x0F == 0x0F);
        r
    }

    /// DEC for 8-bit operands. Leaves C alone, unlike SUB.
    fn dec8(&mut self, val: u8) -> u8 {
        let r = val.wrapping_sub(1);
        self.regs.set_flag(FLAG_Z, r == 0);
        self.regs.set_flag(FLAG_N, true);
        self.regs.set_flag(FLAG_H, val & 0x0F == 0);
        r
    }

    /// `rot[y] val`: RLC RRC RL RR SLA SRA SWAP SRL, the CB block 0 ops.
    /// The bit shifted out goes to C (SWAP clears C); RL and RR rotate
    /// through C as a ninth bit. Sets Z from the result, clears N and H.
    fn rotate(&mut self, y: u8, val: u8) -> u8 {
        let carry_in = u8::from(self.regs.flag(FLAG_C));
        let out_left = val & 0x80 != 0;
        let out_right = val & 0x01 != 0;
        let (r, c) = match y {
            0 => (val.rotate_left(1), out_left),            // RLC
            1 => (val.rotate_right(1), out_right),          // RRC
            2 => ((val << 1) | carry_in, out_left),         // RL
            3 => ((val >> 1) | (carry_in << 7), out_right), // RR
            4 => (val << 1, out_left),                      // SLA
            5 => ((val >> 1) | (val & 0x80), out_right),    // SRA: keeps the sign bit
            6 => (val.rotate_left(4), false),               // SWAP
            _ => (val >> 1, out_right),                     // SRL
        };
        self.set_flags(r == 0, false, false, c);
        r
    }

    /// ADD HL, val. H is the carry out of bit 11 and C out of bit 15 (the
    /// hardware adds the low bytes, then the high bytes with carry). Z is
    /// left alone, even for a zero result.
    fn add_hl(&mut self, val: u16) {
        let hl = self.regs.hl();
        let (sum, carry) = hl.overflowing_add(val);
        self.regs.set_flag(FLAG_N, false);
        self.regs
            .set_flag(FLAG_H, (hl & 0x0FFF) + (val & 0x0FFF) > 0x0FFF);
        self.regs.set_flag(FLAG_C, carry);
        self.regs.set_hl(sum);
    }

    /// Fetches a signed offset and returns SP + e8, for ADD SP,e8 and
    /// LD HL,SP+e8. H and C come from adding the offset's raw byte to SP's
    /// low byte as unsigned 8-bit numbers, whatever the sign: SP + (-1) on
    /// $0005 sets both. Z and N are cleared.
    /// https://gbdev.io/pandocs/CPU_Instruction_Set.html
    fn sp_plus_e8(&mut self, bus: &Bus) -> u16 {
        let e = self.fetch8(bus);
        let sp = self.regs.sp;
        let lo = sp.to_le_bytes()[0];
        let (_, carry) = lo.overflowing_add(e);
        self.set_flags(false, false, (lo & 0x0F) + (e & 0x0F) > 0x0F, carry);
        sp.wrapping_add_signed(i16::from(e as i8))
    }

    /// DAA: after adding or subtracting two BCD numbers (one decimal digit
    /// per nibble), corrects A back to BCD. N says which operation ran; H and
    /// C say which digits overflowed. An addition can also produce a digit
    /// above 9 without a carry, which shows up as A > $99 or a low nibble > 9.
    /// https://gbdev.io/pandocs/CPU_Registers_and_Flags.html
    fn daa(&mut self) {
        let mut a = self.regs.a;
        let mut carry = self.regs.flag(FLAG_C);
        let half = self.regs.flag(FLAG_H);
        let sub = self.regs.flag(FLAG_N);
        if sub {
            if carry {
                a = a.wrapping_sub(0x60);
            }
            if half {
                a = a.wrapping_sub(0x06);
            }
        } else {
            // Both checks look at A before adjusting; +$60 leaves the low
            // nibble unchanged, so doing the high digit first is safe.
            if carry || a > 0x99 {
                a = a.wrapping_add(0x60);
                carry = true;
            }
            if half || (a & 0x0F) > 0x09 {
                a = a.wrapping_add(0x06);
            }
        }
        self.regs.a = a;
        self.set_flags(a == 0, sub, false, carry);
    }

    /// Condition `cc[i]`: NZ Z NC C.
    fn condition(&self, i: u8) -> bool {
        match i {
            0 => !self.regs.flag(FLAG_Z),
            1 => self.regs.flag(FLAG_Z),
            2 => !self.regs.flag(FLAG_C),
            _ => self.regs.flag(FLAG_C),
        }
    }

    /// Relative jump. The offset is signed and counts from the address after
    /// the JR instruction, which is where PC already points.
    fn jr(&mut self, offset: u8) {
        self.regs.pc = self.regs.pc.wrapping_add_signed(i16::from(offset as i8));
    }

    /// Pushes the return address (PC, already past the CALL) and jumps.
    fn call(&mut self, bus: &mut Bus, addr: u16) {
        self.push16(bus, self.regs.pc);
        self.regs.pc = addr;
    }

    /// The address for block 0's `z=2` loads, picked by `p`: (BC) (DE) (HL+)
    /// (HL-). HL+ and HL- step HL after the access, for fast copy loops.
    fn indirect_addr(&mut self, p: u8) -> u16 {
        match p {
            0 => self.regs.bc(),
            1 => self.regs.de(),
            2 => {
                let hl = self.regs.hl();
                self.regs.set_hl(hl.wrapping_add(1));
                hl
            }
            _ => {
                let hl = self.regs.hl();
                self.regs.set_hl(hl.wrapping_sub(1));
                hl
            }
        }
    }

    /// Serves the highest-priority pending interrupt: clears its IF bit and
    /// IME, pushes PC and jumps to its vector ($40 VBlank, $48 STAT, $50
    /// Timer, $58 Serial, $60 Joypad). Returns the 20 T-cycles it takes:
    /// 2 internal M-cycles, 2 for the push, 1 to load PC.
    /// https://gbdev.io/pandocs/Interrupts.html
    ///
    /// TODO(accuracy): the IE/IF check happens on real hardware between the
    /// two pushes. If the high byte of PC lands on $FFFF (IE) and disables the
    /// interrupt, the CPU jumps to $0000 instead (Mooneye's ie_push test).
    fn dispatch_interrupt(&mut self, bus: &mut Bus, pending: u8) -> u32 {
        // Lowest bit wins: VBlank (bit 0) has the highest priority.
        let bit = pending.trailing_zeros() as u8;
        bus.if_reg &= !(1 << bit);
        self.ime = false;
        // EI ; HALT with an interrupt pending hits the HALT bug and then
        // dispatches here: the handler returns to the HALT, which runs again.
        // https://gbdev.io/pandocs/halt.html
        let ret = if std::mem::take(&mut self.halt_bug) {
            self.regs.pc.wrapping_sub(1)
        } else {
            self.regs.pc
        };
        self.push16(bus, ret);
        self.regs.pc = 0x40 + 8 * u16::from(bit);
        20
    }

    /// Executes one instruction, or dispatches an interrupt, and returns the
    /// T-cycles it took.
    pub fn step(&mut self, bus: &mut Bus) -> Result<u32, CpuError> {
        if self.halted {
            if bus.pending_interrupts() != 0 {
                self.halted = false;
            } else {
                return Ok(4);
            }
        }

        // Interrupts are checked between instructions. EI's one-instruction
        // delay works because IME only turns on at the end of the next step.
        let pending = bus.pending_interrupts();
        if self.ime && pending != 0 {
            return Ok(self.dispatch_interrupt(bus, pending));
        }

        let enable_ime_after = std::mem::take(&mut self.ime_pending);
        let pc = self.regs.pc;
        // After the HALT bug, PC isn't advanced past this opcode, so the
        // byte after HALT is read twice.
        let opcode = if std::mem::take(&mut self.halt_bug) {
            bus.read(pc)
        } else {
            self.fetch8(bus)
        };

        let cycles = match self.execute(Opcode::new(opcode), bus) {
            Ok(cycles) => cycles,
            Err(Illegal) => return Err(CpuError::Illegal { opcode, pc }),
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
    fn execute(&mut self, op: Opcode, bus: &mut Bus) -> Result<u32, Illegal> {
        match op.x {
            // Block 0: misc, 16-bit loads and arithmetic, INC/DEC, LD r,d8.
            0 => match op.z {
                0 => match op.y {
                    0 => Ok(4), // NOP
                    // LD (a16), SP: low byte to a16, high byte to a16+1
                    1 => {
                        let addr = self.fetch16(bus);
                        bus.write16(addr, self.regs.sp);
                        Ok(20)
                    }
                    // STOP: two bytes, the second ignored. Resets DIV.
                    // https://gbdev.io/pandocs/Reducing_Power_Consumption.html
                    // TODO(accuracy): a DMG really stops the CPU, timer and LCD
                    // until a button is pressed, and in some situations (Pan
                    // Docs' STOP flowchart) acts as 1 byte. Few games rely on it.
                    2 => {
                        self.fetch8(bus);
                        bus.write(0xFF04, 0);
                        Ok(4)
                    }
                    // JR e8
                    3 => {
                        let offset = self.fetch8(bus);
                        self.jr(offset);
                        Ok(12)
                    }
                    // JR cc[y-4], e8: the taken branch costs 4 more cycles
                    _ => {
                        let offset = self.fetch8(bus);
                        if self.condition(op.y - 4) {
                            self.jr(offset);
                            Ok(12)
                        } else {
                            Ok(8)
                        }
                    }
                },
                1 => match op.q {
                    // LD rp[p], d16
                    0 => {
                        let val = self.fetch16(bus);
                        self.write_rp(op.p, val);
                        Ok(12)
                    }
                    // ADD HL, rp[p]
                    _ => {
                        let val = self.read_rp(op.p);
                        self.add_hl(val);
                        Ok(8)
                    }
                },
                // q=0: LD (BC)/(DE)/(HL+)/(HL-), A
                // q=1: LD A, (BC)/(DE)/(HL+)/(HL-)
                2 => {
                    let addr = self.indirect_addr(op.p);
                    if op.q == 0 {
                        bus.write(addr, self.regs.a);
                    } else {
                        self.regs.a = bus.read(addr);
                    }
                    Ok(8)
                }
                // q=0: INC rp[p]  q=1: DEC rp[p]. No flags.
                // TODO(accuracy): with a pair pointing into OAM ($FE00-$FEFF)
                // during PPU mode 2, the DMG corrupts OAM (the "OAM bug").
                3 => {
                    let val = self.read_rp(op.p);
                    let r = if op.q == 0 {
                        val.wrapping_add(1)
                    } else {
                        val.wrapping_sub(1)
                    };
                    self.write_rp(op.p, r);
                    Ok(8)
                }
                // INC r[y]
                4 => {
                    let val = self.read_r8(bus, op.y);
                    let r = self.inc8(val);
                    self.write_r8(bus, op.y, r);
                    Ok(if op.y == 6 { 12 } else { 4 })
                }
                // DEC r[y]
                5 => {
                    let val = self.read_r8(bus, op.y);
                    let r = self.dec8(val);
                    self.write_r8(bus, op.y, r);
                    Ok(if op.y == 6 { 12 } else { 4 })
                }
                // LD r[y], d8
                6 => {
                    let val = self.fetch8(bus);
                    self.write_r8(bus, op.y, val);
                    Ok(if op.y == 6 { 12 } else { 8 })
                }
                _ => match op.y {
                    // RLCA RRCA RLA RRA: the CB rotates on A, but in 4 cycles
                    // and with Z always cleared, even for a zero result.
                    0..=3 => {
                        self.regs.a = self.rotate(op.y, self.regs.a);
                        self.regs.set_flag(FLAG_Z, false);
                        Ok(4)
                    }
                    // DAA
                    4 => {
                        self.daa();
                        Ok(4)
                    }
                    // CPL: A = !A, sets N and H
                    5 => {
                        self.regs.a = !self.regs.a;
                        self.regs.set_flag(FLAG_N, true);
                        self.regs.set_flag(FLAG_H, true);
                        Ok(4)
                    }
                    // SCF: set carry
                    6 => {
                        self.regs.set_flag(FLAG_N, false);
                        self.regs.set_flag(FLAG_H, false);
                        self.regs.set_flag(FLAG_C, true);
                        Ok(4)
                    }
                    // CCF: complement carry
                    _ => {
                        let c = self.regs.flag(FLAG_C);
                        self.regs.set_flag(FLAG_N, false);
                        self.regs.set_flag(FLAG_H, false);
                        self.regs.set_flag(FLAG_C, !c);
                        Ok(4)
                    }
                },
            },

            // Block 1: LD r[y], r[z]. The slot for LD (HL),(HL) is HALT.
            // HALT sleeps until an interrupt is pending (`step` wakes it). With
            // IME off and one already pending, it doesn't sleep and instead
            // triggers the HALT bug. https://gbdev.io/pandocs/halt.html
            // TODO(accuracy): HALT ; HALT under the bug just retriggers it here.
            // Check against nitro2k01's double-halt-cancel test
            // (github.com/nitro2k01/little-things-gb, not in the c-sp bundle).
            1 if op.y == 6 && op.z == 6 => {
                if !self.ime && bus.pending_interrupts() != 0 {
                    self.halt_bug = true;
                } else {
                    self.halted = true;
                }
                Ok(4)
            }
            1 => {
                let val = self.read_r8(bus, op.z);
                self.write_r8(bus, op.y, val);
                Ok(if op.y == 6 || op.z == 6 { 8 } else { 4 })
            }

            // Block 2: alu[y] A, r[z] with alu = ADD ADC SUB SBC AND XOR OR CP.
            2 => {
                let val = self.read_r8(bus, op.z);
                self.alu(op.y, val);
                Ok(if op.z == 6 { 8 } else { 4 })
            }

            // Block 3: control flow, stack, high-page loads, immediate ALU.
            _ => match op.z {
                0 => match op.y {
                    // RET cc[y]: checking the condition takes a cycle of its
                    // own, so taken is 20 (vs 16 for RET) and not taken is 8.
                    0..=3 => {
                        if self.condition(op.y) {
                            self.regs.pc = self.pop16(bus);
                            Ok(20)
                        } else {
                            Ok(8)
                        }
                    }
                    // LDH (a8), A: high page $FF00-$FFFF (I/O and HRAM)
                    4 => {
                        let addr = 0xFF00 | u16::from(self.fetch8(bus));
                        bus.write(addr, self.regs.a);
                        Ok(12)
                    }
                    // ADD SP, e8
                    5 => {
                        self.regs.sp = self.sp_plus_e8(bus);
                        Ok(16)
                    }
                    // LDH A, (a8)
                    6 => {
                        let addr = 0xFF00 | u16::from(self.fetch8(bus));
                        self.regs.a = bus.read(addr);
                        Ok(12)
                    }
                    // LD HL, SP+e8
                    _ => {
                        let val = self.sp_plus_e8(bus);
                        self.regs.set_hl(val);
                        Ok(12)
                    }
                },
                1 => match (op.q, op.p) {
                    // POP rp2[p]
                    (0, _) => {
                        let val = self.pop16(bus);
                        self.write_rp2(op.p, val);
                        Ok(12)
                    }
                    // RET
                    (_, 0) => {
                        self.regs.pc = self.pop16(bus);
                        Ok(16)
                    }
                    // RETI: RET, then set IME immediately (no one-instruction
                    // delay like EI). How it interacts with interrupt dispatch
                    // is milestone 2's business.
                    (_, 1) => {
                        self.regs.pc = self.pop16(bus);
                        self.ime = true;
                        Ok(16)
                    }
                    // JP HL: just a register copy, no extra cycle
                    (_, 2) => {
                        self.regs.pc = self.regs.hl();
                        Ok(4)
                    }
                    // LD SP, HL
                    _ => {
                        self.regs.sp = self.regs.hl();
                        Ok(8)
                    }
                },
                2 => match op.y {
                    // JP cc[y], a16
                    0..=3 => {
                        let addr = self.fetch16(bus);
                        if self.condition(op.y) {
                            self.regs.pc = addr;
                            Ok(16)
                        } else {
                            Ok(12)
                        }
                    }
                    // LDH (C), A
                    4 => {
                        bus.write(0xFF00 | u16::from(self.regs.c), self.regs.a);
                        Ok(8)
                    }
                    // LD (a16), A
                    5 => {
                        let addr = self.fetch16(bus);
                        bus.write(addr, self.regs.a);
                        Ok(16)
                    }
                    // LDH A, (C)
                    6 => {
                        self.regs.a = bus.read(0xFF00 | u16::from(self.regs.c));
                        Ok(8)
                    }
                    // LD A, (a16)
                    _ => {
                        let addr = self.fetch16(bus);
                        self.regs.a = bus.read(addr);
                        Ok(16)
                    }
                },
                3 => match op.y {
                    // JP a16
                    0 => {
                        self.regs.pc = self.fetch16(bus);
                        Ok(16)
                    }
                    1 => Ok(self.execute_cb(bus)),
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
                    // CALL cc[y], a16
                    0..=3 => {
                        let addr = self.fetch16(bus);
                        if self.condition(op.y) {
                            self.call(bus, addr);
                            Ok(24)
                        } else {
                            Ok(12)
                        }
                    }
                    _ => ILLEGAL, // $E4 $EC $F4 $FC
                },
                5 => match (op.q, op.p) {
                    // PUSH rp2[p]: 4 more cycles than POP, for the SP decrement
                    (0, _) => {
                        let val = self.read_rp2(op.p);
                        self.push16(bus, val);
                        Ok(16)
                    }
                    // CALL a16
                    (_, 0) => {
                        let addr = self.fetch16(bus);
                        self.call(bus, addr);
                        Ok(24)
                    }
                    _ => ILLEGAL, // $DD $ED $FD
                },
                // alu[y] A, d8
                6 => {
                    let val = self.fetch8(bus);
                    self.alu(op.y, val);
                    Ok(8)
                }
                // RST y*8: a one-byte CALL to $00, $08, ... $38
                _ => {
                    self.call(bus, u16::from(op.y) * 8);
                    Ok(16)
                }
            },
        }
    }

    /// Fetches and runs the opcode after a $CB prefix. The T-cycles returned
    /// include the prefix: 8 with a register, 16 with (HL) (read, then write
    /// back), 12 for BIT n,(HL), which only reads.
    fn execute_cb(&mut self, bus: &mut Bus) -> u32 {
        let op = Opcode::new(self.fetch8(bus));
        let val = self.read_r8(bus, op.z);
        let bit = 1u8 << op.y;
        let r = match op.x {
            // rot[y] r[z]
            0 => self.rotate(op.y, val),
            // BIT y, r[z]: Z = the bit is 0. Sets H, leaves C.
            1 => {
                self.regs.set_flag(FLAG_Z, val & bit == 0);
                self.regs.set_flag(FLAG_N, false);
                self.regs.set_flag(FLAG_H, true);
                return if op.z == 6 { 12 } else { 8 };
            }
            // RES y, r[z]
            2 => val & !bit,
            // SET y, r[z]
            _ => val | bit,
        };
        self.write_r8(bus, op.z, r);
        if op.z == 6 {
            16
        } else {
            8
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::interrupt;
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
    fn stop_skips_its_second_byte_and_resets_div() {
        // STOP $00 ; INC A
        let (mut cpu, mut bus) = setup(&[0x10, 0x00, 0x3C]);
        bus.tick(1000);
        assert_ne!(bus.read(0xFF04), 0, "DIV has been counting");
        cpu.regs.a = 0;
        assert_eq!(cpu.step(&mut bus), Ok(4));
        assert_eq!(cpu.regs.pc, 0x0102);
        assert_eq!(bus.read(0xFF04), 0);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.a, 1, "execution carries on after STOP");
    }

    /// A CPU with distinct values in every register and HL pointing at
    /// WRAM ($C123, holding $99), so r8 index 6 is a real memory operand.
    fn setup_loaded(program: &[u8]) -> (Cpu, Bus) {
        let (mut cpu, mut bus) = setup(program);
        cpu.regs.a = 0x11;
        cpu.regs.b = 0x22;
        cpu.regs.c = 0x33;
        cpu.regs.d = 0x44;
        cpu.regs.e = 0x55;
        cpu.regs.set_hl(0xC123);
        bus.write(0xC123, 0x99);
        (cpu, bus)
    }

    #[test]
    fn all_63_ld_r_r_copy_and_take_4_or_8_cycles() {
        for y in 0..8u8 {
            for z in 0..8u8 {
                if y == 6 && z == 6 {
                    continue; // HALT
                }
                let opcode = 0x40 | y << 3 | z;
                let (mut cpu, mut bus) = setup_loaded(&[opcode]);
                let f = cpu.regs.f;
                let src = cpu.read_r8(&bus, z);
                let want = if y == 6 || z == 6 { 8 } else { 4 };
                assert_eq!(cpu.step(&mut bus), Ok(want), "opcode {opcode:02X}");
                assert_eq!(cpu.read_r8(&bus, y), src, "opcode {opcode:02X}");
                assert_eq!(cpu.regs.f, f, "loads leave flags alone");
            }
        }
    }

    #[test]
    fn ld_r_d8_for_every_register() {
        for y in 0..8u8 {
            let opcode = 0x06 | y << 3;
            let (mut cpu, mut bus) = setup_loaded(&[opcode, 0x5A]);
            let want = if y == 6 { 12 } else { 8 };
            assert_eq!(cpu.step(&mut bus), Ok(want), "opcode {opcode:02X}");
            assert_eq!(cpu.read_r8(&bus, y), 0x5A, "opcode {opcode:02X}");
            assert_eq!(cpu.regs.pc, 0x0102);
        }
    }

    #[test]
    fn ld_through_bc_and_de() {
        // LD (BC),A ; LD A,(DE)
        let (mut cpu, mut bus) = setup_loaded(&[0x02, 0x1A]);
        cpu.regs.set_bc(0xC010);
        cpu.regs.set_de(0xC020);
        bus.write(0xC020, 0x7E);
        assert_eq!(cpu.step(&mut bus), Ok(8));
        assert_eq!(bus.read(0xC010), 0x11);
        assert_eq!(cpu.step(&mut bus), Ok(8));
        assert_eq!(cpu.regs.a, 0x7E);
    }

    #[test]
    fn ld_hl_plus_and_minus_step_hl_after_the_access() {
        // LD (HL+),A ; LD A,(HL-)
        let (mut cpu, mut bus) = setup_loaded(&[0x22, 0x3A]);
        bus.write(0xC124, 0x42);
        assert_eq!(cpu.step(&mut bus), Ok(8));
        assert_eq!(bus.read(0xC123), 0x11);
        assert_eq!(cpu.regs.hl(), 0xC124);
        assert_eq!(cpu.step(&mut bus), Ok(8));
        assert_eq!(cpu.regs.a, 0x42);
        assert_eq!(cpu.regs.hl(), 0xC123);
    }

    #[test]
    fn ld_hl_minus_wraps_at_zero() {
        // LD (HL-),A with HL=0 writes to $0000 (an MBC register) and wraps.
        let (mut cpu, mut bus) = setup_loaded(&[0x32]);
        cpu.regs.set_hl(0x0000);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.hl(), 0xFFFF);
    }

    #[test]
    fn ldh_with_immediate_offset() {
        // LDH ($80),A ; LDH A,($81)
        let (mut cpu, mut bus) = setup_loaded(&[0xE0, 0x80, 0xF0, 0x81]);
        bus.write(0xFF81, 0x6C);
        assert_eq!(cpu.step(&mut bus), Ok(12));
        assert_eq!(bus.read(0xFF80), 0x11);
        assert_eq!(cpu.step(&mut bus), Ok(12));
        assert_eq!(cpu.regs.a, 0x6C);
    }

    #[test]
    fn ldh_through_c() {
        // LDH (C),A ; LDH A,(C)
        let (mut cpu, mut bus) = setup_loaded(&[0xE2, 0xF2]);
        cpu.regs.c = 0x90;
        assert_eq!(cpu.step(&mut bus), Ok(8));
        assert_eq!(bus.read(0xFF90), 0x11);
        bus.write(0xFF90, 0xA5);
        assert_eq!(cpu.step(&mut bus), Ok(8));
        assert_eq!(cpu.regs.a, 0xA5);
    }

    #[test]
    fn ld_with_absolute_address() {
        // LD ($C200),A
        let (mut cpu, mut bus) = setup_loaded(&[0xEA, 0x00, 0xC2]);
        assert_eq!(cpu.step(&mut bus), Ok(16));
        assert_eq!(bus.read(0xC200), 0x11);
        // LD A,($C300)
        let (mut cpu, mut bus) = setup_loaded(&[0xFA, 0x00, 0xC3]);
        bus.write(0xC300, 0x3C);
        assert_eq!(cpu.step(&mut bus), Ok(16));
        assert_eq!(cpu.regs.a, 0x3C);
        assert_eq!(cpu.regs.pc, 0x0103);
    }

    #[test]
    fn ld_rp_d16_for_bc_de_hl_sp() {
        for p in 0..4u8 {
            let opcode = 0x01 | p << 4;
            let (mut cpu, mut bus) = setup(&[opcode, 0x34, 0x12]);
            assert_eq!(cpu.step(&mut bus), Ok(12), "opcode {opcode:02X}");
            assert_eq!(cpu.read_rp(p), 0x1234, "opcode {opcode:02X}");
            assert_eq!(cpu.regs.pc, 0x0103);
        }
    }

    #[test]
    fn ld_a16_sp_stores_little_endian() {
        let (mut cpu, mut bus) = setup(&[0x08, 0x00, 0xC2]);
        cpu.regs.sp = 0xBEEF;
        assert_eq!(cpu.step(&mut bus), Ok(20));
        assert_eq!(bus.read(0xC200), 0xEF);
        assert_eq!(bus.read(0xC201), 0xBE);
    }

    #[test]
    fn ld_sp_hl() {
        let (mut cpu, mut bus) = setup(&[0xF9]);
        cpu.regs.set_hl(0xD00D);
        assert_eq!(cpu.step(&mut bus), Ok(8));
        assert_eq!(cpu.regs.sp, 0xD00D);
    }

    #[test]
    fn push_writes_high_byte_above_low_byte() {
        let (mut cpu, mut bus) = setup(&[0xC5]); // PUSH BC
        cpu.regs.sp = 0xD000;
        cpu.regs.set_bc(0xABCD);
        assert_eq!(cpu.step(&mut bus), Ok(16));
        assert_eq!(cpu.regs.sp, 0xCFFE);
        assert_eq!(bus.read(0xCFFF), 0xAB);
        assert_eq!(bus.read(0xCFFE), 0xCD);
    }

    #[test]
    fn push_then_pop_round_trips_every_pair() {
        for p in 0..4u8 {
            let (push, pop) = (0xC5 | p << 4, 0xC1 | p << 4);
            let (mut cpu, mut bus) = setup(&[push, pop]);
            cpu.regs.sp = 0xD000;
            let val = cpu.read_rp2(p);
            cpu.step(&mut bus).unwrap();
            cpu.write_rp2(p, 0x0000);
            assert_eq!(cpu.step(&mut bus), Ok(12), "opcode {pop:02X}");
            assert_eq!(cpu.read_rp2(p), val, "opcode {pop:02X}");
            assert_eq!(cpu.regs.sp, 0xD000);
        }
    }

    #[test]
    fn pop_af_zeroes_the_low_nibble_of_f() {
        // PUSH BC ; POP AF with BC = $12FF: F can't hold the low $F.
        let (mut cpu, mut bus) = setup(&[0xC5, 0xF1]);
        cpu.regs.sp = 0xD000;
        cpu.regs.set_bc(0x12FF);
        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.a, 0x12);
        assert_eq!(cpu.regs.f, 0xF0);
    }

    #[test]
    fn stack_wraps_around_address_zero() {
        // With SP = 0, PUSH writes to $FFFF (IE) and $FFFE (HRAM).
        let (mut cpu, mut bus) = setup(&[0xD5, 0xE1]); // PUSH DE ; POP HL
        cpu.regs.sp = 0x0000;
        cpu.regs.set_de(0x1F42);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.sp, 0xFFFE);
        assert_eq!(bus.read(0xFFFF), 0x1F);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.hl(), 0x1F42);
        assert_eq!(cpu.regs.sp, 0x0000);
    }

    fn flags(z: bool, n: bool, h: bool, c: bool) -> u8 {
        let mut f = 0;
        for (on, bit) in [(z, FLAG_Z), (n, FLAG_N), (h, FLAG_H), (c, FLAG_C)] {
            if on {
                f |= bit;
            }
        }
        f
    }

    /// Runs `alu[y] A, val` on a fresh CPU and returns (A, F).
    fn run_alu(y: u8, a: u8, val: u8, carry: bool) -> (u8, u8) {
        let mut cpu = Cpu::new();
        cpu.regs.a = a;
        cpu.regs.set_flag(FLAG_C, carry);
        cpu.alu(y, val);
        (cpu.regs.a, cpu.regs.f)
    }

    #[test]
    fn add_half_carry_and_carry_are_independent() {
        let (z, h, c, no) = (true, true, true, false);
        // $0F + $01: carry out of bit 3 only
        assert_eq!(run_alu(0, 0x0F, 0x01, no), (0x10, flags(no, no, h, no)));
        // $F0 + $10: carry out of bit 7 only
        assert_eq!(run_alu(0, 0xF0, 0x10, no), (0x00, flags(z, no, no, c)));
        // $3A + $C6: both, and zero
        assert_eq!(run_alu(0, 0x3A, 0xC6, no), (0x00, flags(z, no, h, c)));
        // ADD ignores an incoming carry
        assert_eq!(run_alu(0, 0x01, 0x01, c), (0x02, flags(no, no, no, no)));
    }

    #[test]
    fn adc_counts_the_carry_in_h_and_c() {
        let (z, h, c, no) = (true, true, true, false);
        // $0F + $00 + 1: the carry alone causes the half carry
        assert_eq!(run_alu(1, 0x0F, 0x00, c), (0x10, flags(no, no, h, no)));
        // $FF + $00 + 1: and the full carry
        assert_eq!(run_alu(1, 0xFF, 0x00, c), (0x00, flags(z, no, h, c)));
    }

    #[test]
    fn sub_and_sbc_borrows() {
        let (z, n, h, c, no) = (true, true, true, true, false);
        assert_eq!(run_alu(2, 0x3E, 0x3E, no), (0x00, flags(z, n, no, no)));
        // $E < $F in the low nibble: half borrow, no full borrow
        assert_eq!(run_alu(2, 0x3E, 0x0F, no), (0x2F, flags(no, n, h, no)));
        // $3E < $40: full borrow only
        assert_eq!(run_alu(2, 0x3E, 0x40, no), (0xFE, flags(no, n, no, c)));
        // SBC: $00 - $FF - 1 wraps to 0 and borrows from both nibbles
        assert_eq!(run_alu(3, 0x00, 0xFF, c), (0x00, flags(z, n, h, c)));
        // SBC: the carry alone causes the half borrow
        assert_eq!(run_alu(3, 0x30, 0x00, c), (0x2F, flags(no, n, h, no)));
    }

    #[test]
    fn logic_ops_flags() {
        let (z, h, c, no) = (true, true, true, false);
        // AND always sets H, and clears C even if it was set
        assert_eq!(run_alu(4, 0x5A, 0x3F, c), (0x1A, flags(no, no, h, no)));
        assert_eq!(run_alu(4, 0xF0, 0x0F, no), (0x00, flags(z, no, h, no)));
        // XOR and OR clear H and C
        assert_eq!(run_alu(5, 0xFF, 0xFF, c), (0x00, flags(z, no, no, no)));
        assert_eq!(run_alu(6, 0x50, 0x05, c), (0x55, flags(no, no, no, no)));
    }

    #[test]
    fn cp_sets_flags_like_sub_but_keeps_a() {
        let (n, c, no) = (true, true, false);
        assert_eq!(run_alu(7, 0x3E, 0x40, no), (0x3E, flags(no, n, no, c)));
        assert_eq!(run_alu(7, 0x3E, 0x3E, no), (0x3E, flags(true, n, no, no)));
    }

    /// An independent model of the ALU: wide signed arithmetic, with H found
    /// by the XOR trick (bit 4 of a^b^result is the carry/borrow into bit 4).
    fn reference_alu(y: u8, a: u8, b: u8, carry: bool) -> (u8, u8) {
        let (a32, b32) = (i32::from(a), i32::from(b));
        let (r, n, h, c) = match y {
            0 | 1 => {
                let r = a32 + b32 + i32::from(y == 1 && carry);
                (r, false, (a32 ^ b32 ^ r) & 0x10 != 0, r > 0xFF)
            }
            2 | 3 | 7 => {
                let r = a32 - b32 - i32::from(y == 3 && carry);
                (r, true, (a32 ^ b32 ^ r) & 0x10 != 0, r < 0)
            }
            4 => (a32 & b32, false, true, false),
            5 => (a32 ^ b32, false, false, false),
            _ => (a32 | b32, false, false, false),
        };
        let r = (r & 0xFF) as u8;
        let new_a = if y == 7 { a } else { r };
        (new_a, flags(r == 0, n, h, c))
    }

    #[test]
    fn alu_matches_reference_for_every_input() {
        for y in 0..8u8 {
            for a in 0..=255u8 {
                for b in 0..=255u8 {
                    for carry in [false, true] {
                        assert_eq!(
                            run_alu(y, a, b, carry),
                            reference_alu(y, a, b, carry),
                            "alu[{y}] a={a:02X} b={b:02X} carry={carry}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn alu_opcodes_route_operand_and_cycles() {
        // Block 2: alu[y] A, r[z]
        for y in 0..8u8 {
            for z in 0..8u8 {
                let opcode = 0x80 | y << 3 | z;
                let (mut cpu, mut bus) = setup_loaded(&[opcode]);
                let mut want = cpu.clone();
                want.alu(y, cpu.read_r8(&bus, z));
                let cycles = if z == 6 { 8 } else { 4 };
                assert_eq!(cpu.step(&mut bus), Ok(cycles), "opcode {opcode:02X}");
                assert_eq!(
                    (cpu.regs.a, cpu.regs.f),
                    (want.regs.a, want.regs.f),
                    "opcode {opcode:02X}"
                );
            }
        }
        // alu[y] A, d8
        for y in 0..8u8 {
            let opcode = 0xC6 | y << 3;
            let (mut cpu, mut bus) = setup_loaded(&[opcode, 0x9C]);
            let mut want = cpu.clone();
            want.alu(y, 0x9C);
            assert_eq!(cpu.step(&mut bus), Ok(8), "opcode {opcode:02X}");
            assert_eq!(
                (cpu.regs.a, cpu.regs.f),
                (want.regs.a, want.regs.f),
                "opcode {opcode:02X}"
            );
            assert_eq!(cpu.regs.pc, 0x0102);
        }
    }

    #[test]
    fn inc_and_dec_leave_carry_alone() {
        for carry in [false, true] {
            let mut cpu = Cpu::new();
            cpu.regs.set_flag(FLAG_C, carry);
            assert_eq!(cpu.inc8(0x0F), 0x10);
            assert_eq!(cpu.regs.f, flags(false, false, true, carry));
            assert_eq!(cpu.inc8(0xFF), 0x00);
            assert_eq!(cpu.regs.f, flags(true, false, true, carry));
            assert_eq!(cpu.inc8(0x41), 0x42);
            assert_eq!(cpu.regs.f, flags(false, false, false, carry));
            assert_eq!(cpu.dec8(0x10), 0x0F);
            assert_eq!(cpu.regs.f, flags(false, true, true, carry));
            assert_eq!(cpu.dec8(0x01), 0x00);
            assert_eq!(cpu.regs.f, flags(true, true, false, carry));
            assert_eq!(cpu.dec8(0x00), 0xFF);
            assert_eq!(cpu.regs.f, flags(false, true, true, carry));
        }
    }

    #[test]
    fn inc_dec_opcodes_hit_every_operand() {
        for y in 0..8u8 {
            let (inc, dec) = (0x04 | y << 3, 0x05 | y << 3);
            let (mut cpu, mut bus) = setup_loaded(&[inc, dec]);
            let before = cpu.read_r8(&bus, y);
            let cycles = if y == 6 { 12 } else { 4 };
            assert_eq!(cpu.step(&mut bus), Ok(cycles), "opcode {inc:02X}");
            assert_eq!(cpu.read_r8(&bus, y), before.wrapping_add(1));
            assert_eq!(cpu.step(&mut bus), Ok(cycles), "opcode {dec:02X}");
            assert_eq!(cpu.read_r8(&bus, y), before);
        }
    }

    #[test]
    fn cpl_scf_ccf_flags() {
        // CPL ; SCF ; CCF ; CCF, starting with Z set so we see it untouched
        let (mut cpu, mut bus) = setup(&[0x2F, 0x37, 0x3F, 0x3F]);
        cpu.regs.a = 0x35;
        cpu.regs.f = FLAG_Z;
        assert_eq!(cpu.step(&mut bus), Ok(4));
        assert_eq!(cpu.regs.a, 0xCA);
        assert_eq!(cpu.regs.f, flags(true, true, true, false));
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.f, flags(true, false, false, true));
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.f, flags(true, false, false, false));
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.f, flags(true, false, false, true));
    }

    fn bcd(n: u8) -> u8 {
        ((n / 10) << 4) | (n % 10)
    }

    #[test]
    fn daa_turns_binary_results_into_decimal() {
        for x in 0..100u8 {
            for y in 0..100u8 {
                for carry_in in [false, true] {
                    let ci = u8::from(carry_in);
                    let mut cpu = Cpu::new();

                    // ADC (ADD when carry_in is false), then DAA
                    cpu.regs.a = bcd(x);
                    cpu.regs.set_flag(FLAG_C, carry_in);
                    cpu.alu(1, bcd(y));
                    cpu.daa();
                    let sum = x + y + ci;
                    let msg = format!("{x} + {y} + {ci}");
                    assert_eq!(cpu.regs.a, bcd(sum % 100), "{msg}");
                    assert_eq!(
                        cpu.regs.f,
                        flags(sum % 100 == 0, false, false, sum >= 100),
                        "{msg}"
                    );

                    // SBC (SUB when carry_in is false), then DAA
                    cpu.regs.a = bcd(x);
                    cpu.regs.set_flag(FLAG_C, carry_in);
                    cpu.alu(3, bcd(y));
                    cpu.daa();
                    let borrow = x < y + ci;
                    let diff = (100 + x - y - ci) % 100;
                    let msg = format!("{x} - {y} - {ci}");
                    assert_eq!(cpu.regs.a, bcd(diff), "{msg}");
                    assert_eq!(cpu.regs.f, flags(diff == 0, true, false, borrow), "{msg}");
                }
            }
        }
    }

    #[test]
    fn add_hl_flags_come_from_bits_11_and_15() {
        let mut cpu = Cpu::new();
        cpu.regs.set_hl(0x0FFF);
        cpu.add_hl(0x0001);
        assert_eq!(cpu.regs.hl(), 0x1000);
        assert_eq!(cpu.regs.f, flags(false, false, true, false));

        // $00FF + $0001: a carry out of bit 7 doesn't count
        cpu.regs.set_hl(0x00FF);
        cpu.add_hl(0x0001);
        assert_eq!(cpu.regs.f, flags(false, false, false, false));

        cpu.regs.set_hl(0x8000);
        cpu.add_hl(0x8000);
        assert_eq!(cpu.regs.hl(), 0x0000);
        assert_eq!(cpu.regs.f, flags(false, false, false, true));
    }

    #[test]
    fn add_hl_leaves_z_alone_and_clears_n() {
        for z in [false, true] {
            let mut cpu = Cpu::new();
            cpu.regs.f = flags(z, true, false, false);
            cpu.regs.set_hl(0x1234);
            cpu.add_hl(0x1111);
            assert_eq!(cpu.regs.f, flags(z, false, false, false));
        }
    }

    #[test]
    fn add_hl_matches_reference_on_interesting_values() {
        let values = [
            0x0000, 0x0001, 0x00FF, 0x0100, 0x0F00, 0x0FFF, 0x1000, 0x7FFF, 0x8000, 0x8A23, 0xF000,
            0xFFFF,
        ];
        for a in values {
            for b in values {
                let mut cpu = Cpu::new();
                cpu.regs.set_hl(a);
                cpu.add_hl(b);
                let r = u32::from(a) + u32::from(b);
                let h = (u32::from(a) ^ u32::from(b) ^ r) & 0x1000 != 0;
                assert_eq!(cpu.regs.hl(), r as u16, "{a:04X} + {b:04X}");
                assert_eq!(
                    cpu.regs.f,
                    flags(false, false, h, r > 0xFFFF),
                    "{a:04X} + {b:04X}"
                );
            }
        }
    }

    #[test]
    fn add_hl_opcodes_for_every_pair() {
        for p in 0..4u8 {
            let opcode = 0x09 | p << 4;
            let (mut cpu, mut bus) = setup_loaded(&[opcode]);
            cpu.regs.sp = 0x0F0F;
            let want = cpu.regs.hl().wrapping_add(cpu.read_rp(p));
            assert_eq!(cpu.step(&mut bus), Ok(8), "opcode {opcode:02X}");
            assert_eq!(cpu.regs.hl(), want, "opcode {opcode:02X}");
        }
    }

    #[test]
    fn inc_dec_rr_wrap_and_leave_flags_alone() {
        for p in 0..4u8 {
            let (inc, dec) = (0x03 | p << 4, 0x0B | p << 4);
            let (mut cpu, mut bus) = setup(&[inc, dec, dec]);
            cpu.write_rp(p, 0xFFFF);
            cpu.regs.f = 0xF0;
            assert_eq!(cpu.step(&mut bus), Ok(8), "opcode {inc:02X}");
            assert_eq!(cpu.read_rp(p), 0x0000, "opcode {inc:02X}");
            assert_eq!(cpu.step(&mut bus), Ok(8), "opcode {dec:02X}");
            assert_eq!(cpu.read_rp(p), 0xFFFF, "opcode {dec:02X}");
            cpu.step(&mut bus).unwrap();
            assert_eq!(cpu.read_rp(p), 0xFFFE, "opcode {dec:02X}");
            assert_eq!(cpu.regs.f, 0xF0, "16-bit INC/DEC touch no flags");
        }
    }

    #[test]
    fn sp_plus_e8_flags_come_from_the_low_byte() {
        // SP + (-1) on $0005: the result goes down, yet $05 + $FF carries
        // out of both bit 3 and bit 7.
        let (mut cpu, mut bus) = setup(&[0xF8, 0xFF]); // LD HL, SP-1
        cpu.regs.sp = 0x0005;
        assert_eq!(cpu.step(&mut bus), Ok(12));
        assert_eq!(cpu.regs.hl(), 0x0004);
        assert_eq!(cpu.regs.sp, 0x0005, "LD HL,SP+e8 leaves SP alone");
        assert_eq!(cpu.regs.f, flags(false, false, true, true));

        // $FFF8 + 8 = $0000, but Z is always cleared
        let (mut cpu, mut bus) = setup(&[0xE8, 0x08]); // ADD SP, 8
        cpu.regs.sp = 0xFFF8;
        cpu.regs.f = FLAG_Z | FLAG_N;
        assert_eq!(cpu.step(&mut bus), Ok(16));
        assert_eq!(cpu.regs.sp, 0x0000);
        assert_eq!(cpu.regs.f, flags(false, false, true, true));
        assert_eq!(cpu.regs.pc, 0x0102);

        // $00FF + 1 = $0100: carries out of the low byte, not out of bit 15
        let (mut cpu, mut bus) = setup(&[0xE8, 0x01]);
        cpu.regs.sp = 0x00FF;
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.sp, 0x0100);
        assert_eq!(cpu.regs.f, flags(false, false, true, true));
    }

    #[test]
    fn sp_plus_e8_matches_reference_for_every_low_byte_and_offset() {
        let (mut cpu, mut bus) = setup(&[]);
        for hi in [0x00u8, 0x7F, 0xFF] {
            for lo in 0..=255u8 {
                for e in 0..=255u8 {
                    let sp = u16::from_be_bytes([hi, lo]);
                    // Fetch the offset from WRAM rather than building a ROM
                    // for each of the ~200k cases.
                    bus.write(0xC000, e);
                    cpu.regs.pc = 0xC000;
                    cpu.regs.sp = sp;
                    let got = cpu.sp_plus_e8(&bus);

                    // Reference: signed 16-bit add, flags by the XOR trick
                    // on the sign-extended offset.
                    let offset = i32::from(e as i8);
                    let r = i32::from(sp) + offset;
                    let x = i32::from(sp) ^ offset ^ r;
                    let msg = format!("SP={sp:04X} e={e:02X}");
                    assert_eq!(got, (r & 0xFFFF) as u16, "{msg}");
                    assert_eq!(
                        cpu.regs.f,
                        flags(false, false, x & 0x10 != 0, x & 0x100 != 0),
                        "{msg}"
                    );
                }
            }
        }
    }

    /// Runs `program` from WRAM at $C000 (no 4-byte limit, room to jump
    /// around) with SP = $D000.
    fn setup_wram(program: &[u8]) -> (Cpu, Bus) {
        let (mut cpu, mut bus) = setup(&[]);
        for (i, &b) in program.iter().enumerate() {
            bus.write(0xC000 + i as u16, b);
        }
        cpu.regs.pc = 0xC000;
        cpu.regs.sp = 0xD000;
        (cpu, bus)
    }

    /// F values to try, and for each cc (NZ Z NC C) whether it holds.
    const FLAG_CASES: [u8; 4] = [0, FLAG_Z, FLAG_C, FLAG_Z | FLAG_C];
    const CC_HOLDS: [[bool; 4]; 4] = [
        [true, false, true, false], // NZ
        [false, true, false, true], // Z
        [true, true, false, false], // NC
        [false, false, true, true], // C
    ];

    #[test]
    fn jr_offset_counts_from_the_next_instruction() {
        // JR +$10 at $C000: lands at $C002 + $10
        let (mut cpu, mut bus) = setup_wram(&[0x18, 0x10]);
        assert_eq!(cpu.step(&mut bus), Ok(12));
        assert_eq!(cpu.regs.pc, 0xC012);

        // JR -2 jumps back onto itself
        let (mut cpu, mut bus) = setup_wram(&[0x18, 0xFE]);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.pc, 0xC000);

        // JR -128 is the furthest back
        let (mut cpu, mut bus) = setup_wram(&[0x18, 0x80]);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.pc, 0xC002 - 128);
    }

    #[test]
    fn jr_cc_taken_and_not_taken() {
        for cc in 0..4u8 {
            for (i, &f) in FLAG_CASES.iter().enumerate() {
                let opcode = 0x20 | cc << 3;
                let (mut cpu, mut bus) = setup_wram(&[opcode, 0x10]);
                cpu.regs.f = f;
                let taken = CC_HOLDS[cc as usize][i];
                let (cycles, pc) = if taken { (12, 0xC012) } else { (8, 0xC002) };
                let msg = format!("opcode {opcode:02X} F={f:02X}");
                assert_eq!(cpu.step(&mut bus), Ok(cycles), "{msg}");
                assert_eq!(cpu.regs.pc, pc, "{msg}");
            }
        }
    }

    #[test]
    fn jp_cc_taken_and_not_taken() {
        for cc in 0..4u8 {
            for (i, &f) in FLAG_CASES.iter().enumerate() {
                let opcode = 0xC2 | cc << 3;
                let (mut cpu, mut bus) = setup_wram(&[opcode, 0x00, 0xC1]);
                cpu.regs.f = f;
                let taken = CC_HOLDS[cc as usize][i];
                let (cycles, pc) = if taken { (16, 0xC100) } else { (12, 0xC003) };
                let msg = format!("opcode {opcode:02X} F={f:02X}");
                assert_eq!(cpu.step(&mut bus), Ok(cycles), "{msg}");
                assert_eq!(cpu.regs.pc, pc, "{msg}");
            }
        }
    }

    #[test]
    fn jp_hl() {
        let (mut cpu, mut bus) = setup_wram(&[0xE9]);
        cpu.regs.set_hl(0x4000);
        assert_eq!(cpu.step(&mut bus), Ok(4));
        assert_eq!(cpu.regs.pc, 0x4000);
    }

    #[test]
    fn call_pushes_the_return_address_and_ret_pops_it() {
        // $C000 CALL $C010 ; ... ; $C010 RET
        let mut program = vec![0xCD, 0x10, 0xC0];
        program.resize(0x10, 0x00);
        program.push(0xC9);
        let (mut cpu, mut bus) = setup_wram(&program);

        assert_eq!(cpu.step(&mut bus), Ok(24));
        assert_eq!(cpu.regs.pc, 0xC010);
        assert_eq!(cpu.regs.sp, 0xCFFE);
        assert_eq!(bus.read16(0xCFFE), 0xC003, "return address is after CALL");

        assert_eq!(cpu.step(&mut bus), Ok(16));
        assert_eq!(cpu.regs.pc, 0xC003);
        assert_eq!(cpu.regs.sp, 0xD000);
    }

    #[test]
    fn call_cc_taken_and_not_taken() {
        for cc in 0..4u8 {
            for (i, &f) in FLAG_CASES.iter().enumerate() {
                let opcode = 0xC4 | cc << 3;
                let (mut cpu, mut bus) = setup_wram(&[opcode, 0x00, 0xC1]);
                cpu.regs.f = f;
                let taken = CC_HOLDS[cc as usize][i];
                let msg = format!("opcode {opcode:02X} F={f:02X}");
                if taken {
                    assert_eq!(cpu.step(&mut bus), Ok(24), "{msg}");
                    assert_eq!(cpu.regs.pc, 0xC100, "{msg}");
                    assert_eq!(cpu.regs.sp, 0xCFFE, "{msg}");
                    assert_eq!(bus.read16(0xCFFE), 0xC003, "{msg}");
                } else {
                    assert_eq!(cpu.step(&mut bus), Ok(12), "{msg}");
                    assert_eq!(cpu.regs.pc, 0xC003, "{msg}");
                    assert_eq!(cpu.regs.sp, 0xD000, "nothing pushed: {msg}");
                }
            }
        }
    }

    #[test]
    fn ret_cc_taken_and_not_taken() {
        for cc in 0..4u8 {
            for (i, &f) in FLAG_CASES.iter().enumerate() {
                let opcode = 0xC0 | cc << 3;
                let (mut cpu, mut bus) = setup_wram(&[opcode]);
                cpu.regs.sp = 0xCFFE;
                bus.write16(0xCFFE, 0xC200);
                cpu.regs.f = f;
                let taken = CC_HOLDS[cc as usize][i];
                let msg = format!("opcode {opcode:02X} F={f:02X}");
                if taken {
                    assert_eq!(cpu.step(&mut bus), Ok(20), "{msg}");
                    assert_eq!(cpu.regs.pc, 0xC200, "{msg}");
                    assert_eq!(cpu.regs.sp, 0xD000, "{msg}");
                } else {
                    assert_eq!(cpu.step(&mut bus), Ok(8), "{msg}");
                    assert_eq!(cpu.regs.pc, 0xC001, "{msg}");
                    assert_eq!(cpu.regs.sp, 0xCFFE, "{msg}");
                }
            }
        }
    }

    #[test]
    fn rst_calls_one_of_eight_fixed_addresses() {
        for y in 0..8u8 {
            let opcode = 0xC7 | y << 3;
            let (mut cpu, mut bus) = setup_wram(&[opcode]);
            assert_eq!(cpu.step(&mut bus), Ok(16), "opcode {opcode:02X}");
            assert_eq!(cpu.regs.pc, u16::from(y) * 8, "opcode {opcode:02X}");
            assert_eq!(bus.read16(cpu.regs.sp), 0xC001, "opcode {opcode:02X}");
        }
    }

    #[test]
    fn reti_returns_and_enables_interrupts_immediately() {
        let (mut cpu, mut bus) = setup_wram(&[0xD9]);
        cpu.regs.sp = 0xCFFE;
        bus.write16(0xCFFE, 0xC200);
        assert_eq!(cpu.step(&mut bus), Ok(16));
        assert_eq!(cpu.regs.pc, 0xC200);
        assert_eq!(cpu.regs.sp, 0xD000);
        assert!(cpu.ime, "no EI-style delay");
    }

    /// Runs `rot[y]` on a fresh CPU and returns (result, F).
    fn run_rotate(y: u8, val: u8, carry: bool) -> (u8, u8) {
        let mut cpu = Cpu::new();
        cpu.regs.set_flag(FLAG_C, carry);
        let r = cpu.rotate(y, val);
        (r, cpu.regs.f)
    }

    #[test]
    fn rotate_and_shift_examples() {
        let (z, c, no) = (true, true, false);
        let cases = [
            // (op, value, carry in, result, Z, C out)
            (0, 0x85, no, 0x0B, no, c),  // RLC: bit 7 wraps to bit 0 and C
            (1, 0x01, no, 0x80, no, c),  // RRC: bit 0 wraps to bit 7 and C
            (2, 0x80, no, 0x00, z, c),   // RL: old C (0) enters bit 0
            (2, 0x11, c, 0x23, no, no),  // RL: old C (1) enters bit 0
            (3, 0x01, no, 0x00, z, c),   // RR: old C (0) enters bit 7
            (3, 0x8A, c, 0xC5, no, no),  // RR: old C (1) enters bit 7
            (4, 0xFF, no, 0xFE, no, c),  // SLA: 0 enters bit 0
            (5, 0x8A, no, 0xC5, no, no), // SRA: bit 7 is kept
            (5, 0x01, no, 0x00, z, c),   // SRA
            (6, 0xF1, c, 0x1F, no, no),  // SWAP clears C even if it was set
            (6, 0x00, no, 0x00, z, no),  // SWAP
            (7, 0xFF, no, 0x7F, no, c),  // SRL: 0 enters bit 7
            (7, 0x01, no, 0x00, z, c),   // SRL
        ];
        for (y, val, carry, want, wz, wc) in cases {
            assert_eq!(
                run_rotate(y, val, carry),
                (want, flags(wz, false, false, wc)),
                "rot[{y}] {val:02X} carry={carry}"
            );
        }
    }

    #[test]
    fn rl_and_rr_rotate_nine_bits_through_carry() {
        // Nine RLs (or RRs) bring a 9-bit value (8 bits + C) back to start;
        // eight RLCs (or RRCs) do the same for 8 bits.
        for (y, times) in [(0, 8), (1, 8), (2, 9), (3, 9)] {
            for val in 0..=255u8 {
                for carry in [false, true] {
                    let mut cpu = Cpu::new();
                    cpu.regs.set_flag(FLAG_C, carry);
                    let mut v = val;
                    for _ in 0..times {
                        v = cpu.rotate(y, v);
                    }
                    let msg = format!("rot[{y}] x{times} on {val:02X} carry={carry}");
                    assert_eq!(v, val, "{msg}");
                    if times == 9 {
                        assert_eq!(cpu.regs.flag(FLAG_C), carry, "{msg}");
                    }
                }
            }
        }
    }

    #[test]
    fn sla_matches_add_a_a() {
        for val in 0..=255u8 {
            let (r, f) = run_rotate(4, val, false);
            let (sum, add_f) = run_alu(0, val, val, false);
            assert_eq!(r, sum, "{val:02X}");
            assert_eq!(
                f & (FLAG_Z | FLAG_C),
                add_f & (FLAG_Z | FLAG_C),
                "{val:02X}"
            );
        }
    }

    #[test]
    fn bit_tests_one_bit_sets_h_and_keeps_c() {
        for carry in [false, true] {
            for b in 0..8u8 {
                // BIT b, B
                let opcode = 0x40 | b << 3;
                let (mut cpu, mut bus) = setup_wram(&[0xCB, opcode, 0xCB, opcode]);
                cpu.regs.f = flags(false, true, false, carry);
                cpu.regs.b = 1 << b;
                assert_eq!(cpu.step(&mut bus), Ok(8));
                assert_eq!(cpu.regs.f, flags(false, false, true, carry), "bit {b} set");
                cpu.regs.b = !(1 << b);
                cpu.step(&mut bus).unwrap();
                assert_eq!(cpu.regs.f, flags(true, false, true, carry), "bit {b} clear");
            }
        }
    }

    #[test]
    fn every_cb_opcode_hits_its_operand_with_the_right_cycles() {
        for cb in 0..=255u8 {
            let op = Opcode::new(cb);
            let (mut cpu, mut bus) = setup_loaded(&[]);
            for (i, b) in [0xCB, cb].into_iter().enumerate() {
                bus.write(0xC000 + i as u16, b);
            }
            cpu.regs.pc = 0xC000;
            cpu.regs.f = FLAG_C;
            let val = cpu.read_r8(&bus, op.z);
            let mut model = cpu.clone();
            let bit = 1u8 << op.y;
            let want_val = match op.x {
                0 => model.rotate(op.y, val),
                1 => val,
                2 => val & !bit,
                _ => val | bit,
            };
            let want_cycles = match (op.x, op.z) {
                (1, 6) => 12, // BIT n,(HL) only reads
                (_, 6) => 16, // read, modify, write back
                _ => 8,
            };
            let msg = format!("CB {cb:02X}");
            assert_eq!(cpu.step(&mut bus), Ok(want_cycles), "{msg}");
            assert_eq!(cpu.read_r8(&bus, op.z), want_val, "{msg}");
            assert_eq!(cpu.regs.pc, 0xC002, "{msg}");
            match op.x {
                0 => assert_eq!(cpu.regs.f, model.regs.f, "{msg}"),
                1 => assert_eq!(
                    cpu.regs.f,
                    flags(val & bit == 0, false, true, true),
                    "{msg}"
                ),
                _ => assert_eq!(cpu.regs.f, FLAG_C, "RES/SET keep flags: {msg}"),
            }
        }
    }

    #[test]
    fn accumulator_rotates_match_cb_but_always_clear_z() {
        let (base, mut bus) = setup_wram(&[]);
        for y in 0..4u8 {
            // RLCA/RRCA/RLA/RRA at $C000; CB RLC/RRC/RL/RR A at $C010.
            // The short form's opcode is also the CB form's second byte.
            let short = y << 3 | 0x07;
            bus.write(0xC000, short);
            bus.write(0xC010, 0xCB);
            bus.write(0xC011, short);
            for val in 0..=255u8 {
                for carry in [false, true] {
                    let mut cpu = base.clone();
                    cpu.regs.a = val;
                    cpu.regs.set_flag(FLAG_C, carry);
                    let mut cb_cpu = cpu.clone();
                    cb_cpu.regs.pc = 0xC010;

                    let msg = format!("{short:02X} on {val:02X} carry={carry}");
                    assert_eq!(cpu.step(&mut bus), Ok(4), "{msg}");
                    assert_eq!(cb_cpu.step(&mut bus), Ok(8), "{msg}");
                    assert_eq!(cpu.regs.a, cb_cpu.regs.a, "{msg}");
                    assert_eq!(cpu.regs.f, cb_cpu.regs.f & !FLAG_Z, "{msg}");
                }
            }
        }
    }

    /// A CPU running from WRAM with IME on and IE/IF set to `ie`/`if_`.
    fn setup_irq(program: &[u8], ie: u8, if_: u8) -> (Cpu, Bus) {
        let (mut cpu, mut bus) = setup_wram(program);
        cpu.ime = true;
        bus.ie_reg = ie;
        bus.if_reg = if_;
        (cpu, bus)
    }

    #[test]
    fn interrupt_dispatch_pushes_pc_and_jumps_to_the_vector() {
        let (mut cpu, mut bus) = setup_irq(&[0x00], interrupt::VBLANK, interrupt::VBLANK);
        assert_eq!(cpu.step(&mut bus), Ok(20));
        assert_eq!(cpu.regs.pc, 0x0040);
        assert_eq!(cpu.regs.sp, 0xCFFE);
        assert_eq!(
            bus.read16(0xCFFE),
            0xC000,
            "the interrupted instruction's address"
        );
        assert!(!cpu.ime, "IME is cleared so the handler isn't interrupted");
        assert_eq!(bus.if_reg & interrupt::VBLANK, 0, "IF bit acknowledged");
    }

    #[test]
    fn each_interrupt_has_its_own_vector() {
        let sources = [
            (interrupt::VBLANK, 0x40),
            (interrupt::STAT, 0x48),
            (interrupt::TIMER, 0x50),
            (interrupt::SERIAL, 0x58),
            (interrupt::JOYPAD, 0x60),
        ];
        for (bit, vector) in sources {
            let (mut cpu, mut bus) = setup_irq(&[0x00], 0x1F, bit);
            cpu.step(&mut bus).unwrap();
            assert_eq!(cpu.regs.pc, vector, "IF bit {bit:02X}");
        }
    }

    #[test]
    fn lower_bits_win_and_only_one_is_acknowledged() {
        let (mut cpu, mut bus) = setup_irq(&[0x00], 0x1F, interrupt::TIMER | interrupt::STAT);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.pc, 0x48, "STAT beats Timer");
        assert_eq!(bus.if_reg & 0x1F, interrupt::TIMER, "Timer still pending");
        cpu.ime = true;
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.pc, 0x50);
        assert_eq!(bus.if_reg & 0x1F, 0);
    }

    #[test]
    fn no_dispatch_without_ime_or_ie() {
        // Requested but not enabled in IE
        let (mut cpu, mut bus) = setup_irq(&[0x00], interrupt::VBLANK, interrupt::TIMER);
        assert_eq!(cpu.step(&mut bus), Ok(4));
        assert_eq!(cpu.regs.pc, 0xC001);
        assert_eq!(
            bus.if_reg & interrupt::TIMER,
            interrupt::TIMER,
            "stays requested"
        );

        // Enabled and requested, but IME is off
        let (mut cpu, mut bus) = setup_irq(&[0x00], interrupt::TIMER, interrupt::TIMER);
        cpu.ime = false;
        assert_eq!(cpu.step(&mut bus), Ok(4));
        assert_eq!(cpu.regs.pc, 0xC001);
        assert_eq!(bus.if_reg & interrupt::TIMER, interrupt::TIMER);
    }

    #[test]
    fn ei_lets_one_more_instruction_run_before_dispatch() {
        // EI ; NOP ; NOP with an interrupt already pending
        let (mut cpu, mut bus) = setup_irq(&[0xFB, 0x00, 0x00], interrupt::TIMER, interrupt::TIMER);
        cpu.ime = false;
        assert_eq!(cpu.step(&mut bus), Ok(4)); // EI
        assert_eq!(cpu.step(&mut bus), Ok(4)); // NOP still runs
        assert_eq!(cpu.step(&mut bus), Ok(20)); // then the interrupt
        assert_eq!(bus.read16(cpu.regs.sp), 0xC002, "returns to the second NOP");
    }

    #[test]
    fn reti_after_dispatch_resumes_with_interrupts_on() {
        let (mut cpu, mut bus) = setup_irq(&[0x00], interrupt::VBLANK, interrupt::VBLANK);
        cpu.step(&mut bus).unwrap();
        // The vector is in ROM, so run RETI from WRAM by hand.
        bus.write(0xC100, 0xD9);
        cpu.regs.pc = 0xC100;
        assert_eq!(cpu.step(&mut bus), Ok(16));
        assert_eq!(cpu.regs.pc, 0xC000);
        assert_eq!(cpu.regs.sp, 0xD000);
        assert!(cpu.ime);
    }

    #[test]
    fn halt_wakes_into_the_interrupt_handler() {
        let (mut cpu, mut bus) = setup_irq(&[0x76, 0x00], interrupt::TIMER, 0);
        cpu.step(&mut bus).unwrap(); // HALT
        assert_eq!(cpu.step(&mut bus), Ok(4), "still halted");
        bus.if_reg |= interrupt::TIMER;
        assert_eq!(cpu.step(&mut bus), Ok(20));
        assert!(!cpu.halted);
        assert_eq!(cpu.regs.pc, 0x50);
        assert_eq!(bus.read16(cpu.regs.sp), 0xC001, "returns after the HALT");
    }

    #[test]
    fn halt_with_ime_off_wakes_without_calling_the_handler() {
        // HALT ; INC A with IME off and nothing pending yet
        let (mut cpu, mut bus) = setup_irq(&[0x76, 0x3C], interrupt::TIMER, 0);
        cpu.ime = false;
        cpu.regs.a = 0;
        cpu.step(&mut bus).unwrap();
        assert!(cpu.halted);
        assert_eq!(cpu.step(&mut bus), Ok(4), "still halted");

        bus.if_reg |= interrupt::TIMER;
        assert_eq!(cpu.step(&mut bus), Ok(4), "wakes and runs INC A");
        assert!(!cpu.halted);
        assert_eq!(cpu.regs.a, 1);
        assert_eq!(cpu.regs.pc, 0xC002);
        assert_eq!(cpu.regs.sp, 0xD000, "no handler call");
        assert_eq!(
            bus.if_reg & interrupt::TIMER,
            interrupt::TIMER,
            "IF stays set"
        );
    }

    #[test]
    fn pending_interrupt_with_ime_on_is_served_before_halt_runs() {
        // Interrupts are checked between instructions, so HALT never gets to
        // sleep; the handler returns to it and it sleeps then.
        let (mut cpu, mut bus) = setup_irq(&[0x76], interrupt::TIMER, interrupt::TIMER);
        assert_eq!(cpu.step(&mut bus), Ok(20));
        assert!(!cpu.halted);
        assert_eq!(cpu.regs.pc, 0x50);
        assert_eq!(bus.read16(cpu.regs.sp), 0xC000, "returns to the HALT");
    }

    #[test]
    fn halt_bug_runs_the_next_byte_twice() {
        // HALT ; INC A with IME off and an interrupt already pending
        let (mut cpu, mut bus) = setup_irq(&[0x76, 0x3C, 0x00], interrupt::TIMER, interrupt::TIMER);
        cpu.ime = false;
        cpu.regs.a = 0;
        cpu.step(&mut bus).unwrap();
        assert!(!cpu.halted, "HALT doesn't sleep");
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.pc, 0xC001, "PC didn't advance");
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.a, 2, "INC A ran twice");
        assert_eq!(cpu.regs.pc, 0xC002);
    }

    #[test]
    fn halt_bug_rereads_the_opcode_as_its_own_operand() {
        // HALT ; LD A,$3C runs as LD A,$3E (the opcode byte read again as the
        // operand) followed by INC A ($3C).
        let (mut cpu, mut bus) = setup_irq(&[0x76, 0x3E, 0x3C], interrupt::TIMER, interrupt::TIMER);
        cpu.ime = false;
        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.a, 0x3E);
        assert_eq!(cpu.regs.pc, 0xC002);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.a, 0x3F);
    }

    #[test]
    fn halt_bug_then_rst_returns_to_the_rst() {
        // HALT ; RST $28 under the bug: the pushed return address is the RST.
        let (mut cpu, mut bus) = setup_irq(&[0x76, 0xEF], interrupt::TIMER, interrupt::TIMER);
        cpu.ime = false;
        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.regs.pc, 0x28);
        assert_eq!(bus.read16(cpu.regs.sp), 0xC001);
    }

    #[test]
    fn ei_then_halt_with_pending_interrupt_returns_to_the_halt() {
        // EI ; HALT with IME off and an interrupt pending
        let (mut cpu, mut bus) = setup_irq(&[0xFB, 0x76, 0x00], interrupt::TIMER, interrupt::TIMER);
        cpu.ime = false;
        cpu.step(&mut bus).unwrap(); // EI
        cpu.step(&mut bus).unwrap(); // HALT: IME is still off, so the bug
        assert_eq!(cpu.step(&mut bus), Ok(20));
        assert_eq!(cpu.regs.pc, 0x50);
        assert_eq!(
            bus.read16(cpu.regs.sp),
            0xC001,
            "returns to the HALT itself"
        );
    }
}
