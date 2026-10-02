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
    /// CPU T-cycles until a master transfer's 8 bits are out; 0 when none.
    cycles_left: u32,
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
            cycles_left: 0,
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
            // TODO(accuracy): writing SB mid-transfer changes the bits still
            // to shift; here SB only changes as a whole, at the end.
            self.sb = val;
            return;
        }
        self.sc = val & if self.cgb { 0x83 } else { 0x81 };
        if self.sc & 0x81 == 0x81 {
            // Master: start clocking.
            // TODO(accuracy): the internal clock comes from the free-running
            // divider, so the first bit goes at its next edge rather than a
            // whole bit time after this write (Mooneye's boot_sclk_align).
            self.cycles_left = 8 * self.bit_cycles();
            self.log.push(self.sb);
            self.reply = None;
            self.out = self.plugged.then_some(self.sb);
        } else {
            self.cycles_left = 0; // a slave waits to be clocked
        }
    }

    /// CPU T-cycles per bit: 512 (8192 Hz), or 16 with the Color's fast
    /// clock. The clock comes from the CPU's, so double speed halves the real
    /// time but not these counts.
    fn bit_cycles(&self) -> u32 {
        if self.cgb && self.sc & 0x02 != 0 {
            16
        } else {
            512
        }
    }

    /// Advances a master transfer by `cycles` CPU T-cycles. True when it
    /// finishes (a serial interrupt is due).
    pub fn tick(&mut self, cycles: u32) -> bool {
        if self.cycles_left == 0 {
            return false;
        }
        self.cycles_left = self.cycles_left.saturating_sub(cycles);
        if self.cycles_left > 0 {
            return false;
        }
        if !self.plugged {
            self.finish(0xFF); // nobody there: the input line reads 1s
            return true;
        }
        match self.reply.take() {
            Some(byte) => {
                self.finish(byte);
                true
            }
            None => false, // waiting(): the partner's byte is still on its way
        }
    }

    /// A master transfer's time is up but the partner's byte hasn't come:
    /// the machine should stop until [`answer`](Self::answer) brings it.
    pub fn waiting(&self) -> bool {
        self.plugged && self.cycles_left == 0 && self.sc & 0x81 == 0x81
    }

    fn finish(&mut self, received: u8) {
        self.sb = received;
        self.sc &= 0x7F;
        self.out = None;
    }

    /// Plugs the cable in or pulls it out. Pulling it out ends a transfer
    /// that was waiting for the partner, as a disconnected master would.
    /// Returns true if that finished a transfer (a serial interrupt is due).
    pub fn plug(&mut self, plugged: bool) -> bool {
        self.plugged = plugged;
        if !plugged {
            self.out = None;
            self.reply = None;
            if self.sc & 0x81 == 0x81 && self.cycles_left == 0 {
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
        if self.cycles_left == 0 {
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
        w.bytes(&[self.sb, self.sc]);
        w.u32(self.cycles_left);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"SIO ")?;
        self.sb = r.u8()?;
        self.sc = r.u8()? & 0x83;
        self.cycles_left = r.u32()?;
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

    #[test]
    fn with_no_cable_a_master_reads_ff_after_8_bit_times() {
        let mut s = Serial::new(false);
        master(&mut s, 0x42);
        assert_eq!(s.read(0xFF02), 0xFF, "bit 7 set while it runs");
        assert!(!s.tick(8 * 512 - 1));
        assert!(s.tick(1), "the interrupt is due");
        assert_eq!(s.read(0xFF01), 0xFF);
        assert_eq!(s.read(0xFF02), 0x7F, "bit 7 cleared");
        assert_eq!(s.take_log(), [0x42], "what went out");
        assert_eq!(s.take_out(), None, "nothing for a partner");
        assert!(!s.waiting());
    }

    #[test]
    fn the_colors_fast_clock_is_32_times_quicker() {
        let mut s = Serial::new(true);
        s.write(0xFF02, 0x83);
        assert_eq!(s.read(0xFF02), 0xFF);
        assert!(s.tick(8 * 16));
        assert_eq!(s.read(0xFF02), 0x7F, "bits 1 and 0 stay; only bit 7 clears");
        let mut dmg = Serial::new(false);
        dmg.write(0xFF02, 0x83);
        assert!(!dmg.tick(8 * 16), "no fast clock on the original");
    }

    #[test]
    fn plugged_in_a_master_swaps_with_the_partner() {
        let mut s = Serial::new(false);
        s.plug(true);
        master(&mut s, 0x42);
        assert_eq!(s.take_out(), Some(0x42));
        assert_eq!(s.take_out(), None, "once");
        assert!(!s.answer(0x99), "early: kept until the time is up");
        assert!(s.tick(8 * 512));
        assert_eq!(s.read(0xFF01), 0x99);
        assert!(!s.waiting());
    }

    #[test]
    fn a_late_answer_makes_the_master_wait_then_finishes_it() {
        let mut s = Serial::new(false);
        s.plug(true);
        master(&mut s, 0x01);
        assert!(!s.tick(8 * 512), "time's up, but no byte yet");
        assert!(s.waiting());
        assert_eq!(s.read(0xFF02) & 0x80, 0x80, "still running for the game");
        assert!(s.answer(0x55));
        assert!(!s.waiting());
        assert_eq!((s.read(0xFF01), s.read(0xFF02) & 0x80), (0x55, 0));
    }

    #[test]
    fn pulling_the_cable_out_ends_a_waiting_transfer_with_ff() {
        let mut s = Serial::new(false);
        s.plug(true);
        master(&mut s, 0x01);
        s.tick(8 * 512);
        assert!(s.plug(false));
        assert_eq!(s.read(0xFF01), 0xFF);
        assert!(!s.waiting());
    }

    #[test]
    fn a_ready_slave_swaps_when_clocked_and_an_unready_one_sends_ff() {
        let mut s = Serial::new(false);
        s.write(0xFF01, 0x77);
        assert_eq!(s.clocked(0x10), (0xFF, false), "SC bit 7 not set");
        assert_eq!(s.read(0xFF01), 0x77, "untouched");
        s.write(0xFF02, 0x80); // listen
        assert!(!s.tick(100_000), "a slave doesn't time out");
        assert_eq!(s.clocked(0x10), (0x77, true));
        assert_eq!((s.read(0xFF01), s.read(0xFF02)), (0x10, 0x7E));
        assert_eq!(s.take_log(), [] as [u8; 0], "a slave sends nothing itself");
    }
}
