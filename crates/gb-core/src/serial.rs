//! The serial port: the link cable.
//!
//! SB ($FF01) holds a byte; SC ($FF02) starts a transfer (bit 7) and picks
//! the clock (bit 0: 1 = this side drives it, the "master"; 0 = the partner
//! does). The master shifts SB out a bit at a time, most significant first,
//! while the partner's bits shift in, so after 8 clocks the two Game Boys
//! have swapped bytes; both clear SC bit 7 and request a serial interrupt.
//! The internal clock runs at 8192 Hz, 512 T-cycles a bit (the Color's
//! SC bit 1 makes it 32x faster). With no cable, a master reads $FF.
//! https://gbdev.io/pandocs/Serial_Data_Transfer_(Link_Cable).html
//!
//! That clock isn't a timer of its own: it comes from the free-running
//! counter behind DIV. Each falling edge of counter bit 7 (bit 2 for the
//! fast clock) flips it, and each time it goes low a master shifts one bit,
//! so the first bit goes at the divider's next such edge, wherever SC was
//! written (timings after SameBoy's serial port; Mooneye's boot_sclk_align).
//!
//! A partner is another emulator somewhere else, so its byte can't be
//! known when a transfer starts: the host carries bytes across. As master,
//! the byte to send waits in [`Serial::take_out`]; the partner's reply comes
//! back through [`Serial::answer`]. If the transfer's time is up before the
//! reply arrives, [`Serial::waiting`] says so and the whole machine should
//! wait (the game sees a transfer that took its usual time). As slave, the
//! partner's byte comes through [`Serial::clocked`], which answers at once.

use crate::state::{StateError, StateReader, StateWriter};

#[derive(Debug, Clone)]
pub struct Serial {
    cgb: bool,
    sb: u8,
    sc: u8,
    /// Bits a master transfer has shifted so far (8 and still running:
    /// waiting for the partner's byte).
    bits: u8,
    /// The internal serial clock's level: it flips at each falling edge of
    /// the counter bit it comes from, and a bit shifts when it goes low.
    clock_high: bool,
    /// A cable is in: master transfers swap with a partner instead of
    /// reading $FF.
    plugged: bool,
    /// The partner's byte for the current master transfer, if it's here.
    reply: Option<u8>,
    /// A byte sent as master that the host hasn't picked up yet.
    out: Option<u8>,
    /// Every byte sent as master, for the host (test ROMs print this way).
    log: Vec<u8>,
}

impl Serial {
    pub fn new(cgb: bool) -> Self {
        Self {
            cgb,
            sb: 0,
            sc: 0,
            bits: 0,
            clock_high: false,
            plugged: false,
            reply: None,
            out: None,
            log: Vec::new(),
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF01 => self.sb,
            // Unused bits read 1; bit 1 only exists on the Color.
            _ => self.sc | if self.cgb { 0x7C } else { 0x7E },
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        if addr == 0xFF01 {
            // TODO(accuracy): with a cable in, SB changes as a whole when the
            // partner's byte comes, not a bit at a time.
            self.sb = val;
            return;
        }
        // A write restarts the bit count, and ends a high clock phase early,
        // shifting a bit if a transfer was running (SameBoy).
        self.bits = 0;
        if self.clock_high {
            self.clock_edge();
        }
        self.sc = val & if self.cgb { 0x83 } else { 0x81 };
        if self.sc & 0x81 == 0x81 {
            // Master: clocking starts at the divider's next edge.
            self.log.push(self.sb);
            self.reply = None;
            self.out = self.plugged.then_some(self.sb);
        }
    }

    /// Falling edges of counter bits 7 and 2 (from the timer) since the
    /// last call: the 8192 Hz clock comes from bit 7, the Color's fast one
    /// from bit 2. The counter runs at the CPU's speed, so double speed
    /// doubles the real rate. True when a transfer finished (a serial
    /// interrupt is due).
    pub fn divider_falls(&mut self, falls: [u32; 2]) -> bool {
        let fast = self.cgb && self.sc & 0x02 != 0;
        let mut done = false;
        for _ in 0..falls[usize::from(fast)] {
            done |= self.clock_edge();
        }
        done
    }

    /// The internal clock flips; when it goes low, a master shifts a bit.
    /// True when that finished a transfer.
    fn clock_edge(&mut self) -> bool {
        self.clock_high = !self.clock_high;
        if self.clock_high || self.sc & 0x81 != 0x81 {
            return false;
        }
        if self.plugged {
            // The partner's byte comes whole (see answer()).
            if self.bits == 8 {
                return false; // waiting() for it
            }
            self.bits += 1;
            if self.bits < 8 {
                return false;
            }
            return match self.reply.take() {
                Some(byte) => {
                    self.finish(byte);
                    true
                }
                None => false, // waiting(): it's still on its way
            };
        }
        // Nobody there: the input line reads 1s.
        self.sb = self.sb << 1 | 1;
        self.bits += 1;
        if self.bits == 8 {
            self.bits = 0;
            self.sc &= 0x7F;
            return true;
        }
        false
    }

    /// A master transfer's time is up but the partner's byte hasn't come:
    /// the machine should stop until [`answer`](Self::answer) brings it.
    pub fn waiting(&self) -> bool {
        self.plugged && self.bits == 8 && self.sc & 0x81 == 0x81
    }

    fn finish(&mut self, received: u8) {
        self.sb = received;
        self.sc &= 0x7F;
        self.out = None;
        self.bits = 0;
    }

    /// Plugs the cable in or pulls it out. Pulling it out ends a transfer
    /// that was waiting for the partner, as a disconnected master would.
    /// Returns true if that finished a transfer (a serial interrupt is due).
    pub fn plug(&mut self, plugged: bool) -> bool {
        self.plugged = plugged;
        if !plugged {
            self.out = None;
            self.reply = None;
            if self.bits == 8 && self.sc & 0x81 == 0x81 {
                self.finish(0xFF);
                return true;
            }
        }
        false
    }

    /// The byte this side clocked out as master, for the host to carry to
    /// the partner (once).
    pub fn take_out(&mut self) -> Option<u8> {
        self.out.take()
    }

    /// The partner's byte for this side's master transfer. Returns true if
    /// that finished the transfer (it was waiting); otherwise it's kept
    /// until the transfer's time is up.
    pub fn answer(&mut self, byte: u8) -> bool {
        if self.sc & 0x81 != 0x81 {
            return false; // no transfer to answer (it was cancelled)
        }
        if self.bits == 8 {
            self.finish(byte);
            true
        } else {
            self.reply = Some(byte);
            false
        }
    }

    /// The partner, as master, clocked `byte` in. A slave that's ready (SC
    /// = $80) swaps it for SB and finishes; returns the byte that went back
    /// and whether a serial interrupt is due. Not ready, it sends $FF back.
    /// TODO(accuracy): the swap takes the partner's 8 clocks; here it's
    /// instant on this side. And a slave that isn't ready still shifts on
    /// real hardware.
    pub fn clocked(&mut self, byte: u8) -> (u8, bool) {
        if self.sc & 0x81 != 0x80 {
            return (0xFF, false);
        }
        let sent = self.sb;
        self.finish(byte);
        (sent, true)
    }

    /// Bytes sent as master since the last call.
    pub fn take_log(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.log)
    }

    /// SB, SC and how far a transfer has got. The cable and anything in
    /// flight belong to the host, so a loaded state keeps the current cable
    /// and starts with nothing in flight.
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.tag(b"SIO ");
        w.bytes(&[self.sb, self.sc, self.bits, u8::from(self.clock_high)]);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"SIO ")?;
        self.sb = r.u8()?;
        self.sc = r.u8()? & 0x83;
        self.bits = r.u8()?.min(8);
        self.clock_high = r.bool()?;
        self.reply = None;
        self.out = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master(s: &mut Serial, byte: u8) {
        s.write(0xFF01, byte);
        s.write(0xFF02, 0x81);
    }

    /// The counter behind DIV, as the timer runs it.
    struct Div(u16);

    impl Div {
        /// Runs it `cycles` T-cycles, passing its bit 7 and bit 2 falls on.
        /// True if a transfer finished.
        fn run(&mut self, s: &mut Serial, cycles: u32) -> bool {
            let mut done = false;
            for _ in 0..cycles {
                let before = self.0;
                self.0 = self.0.wrapping_add(1);
                let fell = before & !self.0;
                done |= s.divider_falls([(fell >> 7 & 1).into(), (fell >> 2 & 1).into()]);
            }
            done
        }
    }

    #[test]
    fn with_no_cable_sb_shifts_left_taking_1s_a_bit_at_a_time() {
        let mut s = Serial::new(false);
        let mut div = Div(0);
        master(&mut s, 0x00);
        div.run(&mut s, 512);
        assert_eq!(s.read(0xFF01), 0x01);
        div.run(&mut s, 3 * 512);
        assert_eq!(s.read(0xFF01), 0x0F);
        s.write(0xFF01, 0x80); // a write mid-transfer changes what's left
        div.run(&mut s, 512);
        assert_eq!(s.read(0xFF01), 0x01);
    }

    #[test]
    fn the_first_bit_goes_at_the_dividers_next_edge() {
        // Counter bit 7 falls at 256 (clock high), then at 512 (low: shift).
        let mut s = Serial::new(false);
        let mut div = Div(0);
        div.run(&mut s, 100);
        master(&mut s, 0x00);
        div.run(&mut s, 411);
        assert_eq!(s.read(0xFF01), 0x00);
        div.run(&mut s, 1);
        assert_eq!(s.read(0xFF01), 0x01, "412 cycles after SC, not 512");
        assert!(!div.run(&mut s, 7 * 512 - 1));
        assert!(div.run(&mut s, 1));
    }

    #[test]
    fn writing_sc_while_the_clock_is_high_ends_that_half_early() {
        let mut s = Serial::new(false);
        let mut div = Div(0);
        master(&mut s, 0x00);
        div.run(&mut s, 256); // clock high
        master(&mut s, 0x00); // goes low: the running transfer shifts
        assert_eq!(s.read(0xFF01), 0x01, "and that bit counts for the new one");
        div.run(&mut s, 512); // high at 512, low (a bit) at 768
        assert_eq!(s.read(0xFF01), 0x03);
        assert!(!div.run(&mut s, 6 * 512 - 1));
        assert!(div.run(&mut s, 1));
    }

    #[test]
    fn with_no_cable_a_master_reads_ff_after_8_bit_times() {
        let mut s = Serial::new(false);
        let mut div = Div(0);
        master(&mut s, 0x42);
        assert_eq!(s.read(0xFF02), 0xFF, "bit 7 set while it runs");
        assert!(!div.run(&mut s, 8 * 512 - 1));
        assert!(div.run(&mut s, 1), "the interrupt is due");
        assert_eq!(s.read(0xFF01), 0xFF);
        assert_eq!(s.read(0xFF02), 0x7F, "bit 7 cleared");
        assert_eq!(s.take_log(), [0x42], "what went out");
        assert_eq!(s.take_out(), None, "nothing for a partner");
        assert!(!s.waiting());
    }

    #[test]
    fn the_colors_fast_clock_is_32_times_quicker() {
        let mut s = Serial::new(true);
        let mut div = Div(0);
        s.write(0xFF02, 0x83);
        assert_eq!(s.read(0xFF02), 0xFF);
        assert!(div.run(&mut s, 8 * 16));
        assert_eq!(s.read(0xFF02), 0x7F, "bits 1 and 0 stay; only bit 7 clears");
        let mut dmg = Serial::new(false);
        dmg.write(0xFF02, 0x83);
        assert!(!div.run(&mut dmg, 8 * 16), "no fast clock on the original");
    }

    #[test]
    fn plugged_in_a_master_swaps_with_the_partner() {
        let mut s = Serial::new(false);
        let mut div = Div(0);
        s.plug(true);
        master(&mut s, 0x42);
        assert_eq!(s.take_out(), Some(0x42));
        assert_eq!(s.take_out(), None, "once");
        assert!(!s.answer(0x99), "early: kept until the time is up");
        assert!(div.run(&mut s, 8 * 512));
        assert_eq!(s.read(0xFF01), 0x99);
        assert!(!s.waiting());
    }

    #[test]
    fn a_late_answer_makes_the_master_wait_then_finishes_it() {
        let mut s = Serial::new(false);
        let mut div = Div(0);
        s.plug(true);
        master(&mut s, 0x01);
        assert!(!div.run(&mut s, 8 * 512), "time's up, but no byte yet");
        assert!(s.waiting());
        assert_eq!(s.read(0xFF02) & 0x80, 0x80, "still running for the game");
        assert!(s.answer(0x55));
        assert!(!s.waiting());
        assert_eq!((s.read(0xFF01), s.read(0xFF02) & 0x80), (0x55, 0));
    }

    #[test]
    fn pulling_the_cable_out_ends_a_waiting_transfer_with_ff() {
        let mut s = Serial::new(false);
        let mut div = Div(0);
        s.plug(true);
        master(&mut s, 0x01);
        div.run(&mut s, 8 * 512);
        assert!(s.plug(false));
        assert_eq!(s.read(0xFF01), 0xFF);
        assert!(!s.waiting());
    }

    #[test]
    fn a_ready_slave_swaps_when_clocked_and_an_unready_one_sends_ff() {
        let mut s = Serial::new(false);
        let mut div = Div(0);
        s.write(0xFF01, 0x77);
        assert_eq!(s.clocked(0x10), (0xFF, false), "SC bit 7 not set");
        assert_eq!(s.read(0xFF01), 0x77, "untouched");
        s.write(0xFF02, 0x80); // listen
        assert!(!div.run(&mut s, 100_000), "a slave doesn't time out");
        assert_eq!(s.clocked(0x10), (0x77, true));
        assert_eq!((s.read(0xFF01), s.read(0xFF02)), (0x10, 0x7E));
        assert_eq!(s.take_log(), [] as [u8; 0], "a slave sends nothing itself");
    }
}
