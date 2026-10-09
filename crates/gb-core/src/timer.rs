//! DIV/TIMA/TMA/TAC timer.
//!
//! DIV is the upper byte of a 16-bit counter that ticks every T-cycle. TIMA
//! increments on a falling edge of "timer enabled AND selected counter bit",
//! which is how real hardware does it. That's why writing DIV or TAC can bump
//! TIMA: anything that makes that signal fall counts.
//!
//! Reference: https://gbdev.io/pandocs/Timer_and_Divider_Registers.html and
//! https://gbdev.io/pandocs/Timer_Obscure_Behaviour.html

use crate::state::{StateError, StateReader, StateWriter};
use crate::Model;

/// Where TIMA is in the two M-cycles after it overflows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Reload {
    #[default]
    Idle,
    /// "Cycle A": TIMA reads $00. Holds the T-cycles left before the reload.
    /// Writing TIMA now cancels the reload and the interrupt.
    Pending(u8),
    /// "Cycle B": TMA was just copied in and the interrupt requested. Holds
    /// the T-cycles left. TIMA writes are ignored; TMA writes go to TIMA too.
    Reloading(u8),
}

#[derive(Debug, Clone, Default)]
pub struct Timer {
    counter: u16,
    tima: u8,
    tma: u8,
    tac: u8,
    reload: Reload,
    /// Falling edges of DIV bit 4 (counter bit 12) not yet passed to the
    /// APU: they clock its frame sequencer ("DIV-APU", 512 Hz).
    div_apu: u32,
    /// Rising edges of that bit not yet passed on: they arm the APU's
    /// envelopes (see [`Apu::div_secondary_event`](crate::apu::Apu::div_secondary_event)).
    div_apu_rises: u32,
    /// The last falling edge came from a DIV write.
    div_apu_by_write: bool,
    /// T-cycles since the DIV-APU bit last rose (saturating).
    since_rise: u8,
    /// A speed switch waiting for STOP's DIV reset to move DIV-APU to
    /// the other bit.
    speed_at_reset: Option<bool>,
    /// Color double speed: the counter runs twice as fast, so DIV-APU comes
    /// from DIV bit 5 (counter bit 13) to stay at 512 Hz.
    /// https://gbdev.io/pandocs/Audio_details.html#div-apu
    double_speed: bool,
    /// T-cycles until STOP's DIV reset lands (see [`Timer::stop_reset`]).
    reset_in: u8,
    /// The 4096 Hz input (counter bit 9) 4 T-cycles before that reset.
    bit9_before: bool,
    /// Falling edges of counter bits 7 and 2 not yet passed to the serial
    /// port: they drive its internal clock (8192 Hz, or the Color's fast
    /// 262144 Hz).
    serial_falls: [u32; 2],
}

impl Timer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The timer as the boot ROM leaves it. On the original, DIV reads $AB
    /// at $0100: the internal counter is $ABCC when the first opcode is
    /// fetched, which is $ABC8 here, where an M-cycle runs the hardware before
    /// the CPU's access (Mooneye's boot_div).
    /// https://gbdev.io/pandocs/Power_Up_Sequence.html#hardware-registers
    /// TODO(accuracy): the Color's boot ROM leaves another value, which
    /// depends on its logo animation; it starts at 0 here.
    pub fn post_boot(model: Model) -> Self {
        Self {
            counter: if model == Model::Dmg { 0xABC8 } else { 0 },
            ..Self::default()
        }
    }

    /// The CPU switched speed. DIV-APU moves to the other counter bit when
    /// STOP's DIV reset lands, not before, so a switch to normal speed with
    /// bit 13 set and bit 12 clear is a falling edge to the APU (AGE's
    /// spsw-ch2-lc-delay).
    pub fn set_double_speed(&mut self, on: bool) {
        if self.reset_in > 0 {
            self.speed_at_reset = Some(on);
            return;
        }
        self.double_speed = on;
    }

    /// The counter bit whose falling edge is a DIV-APU event.
    fn div_apu_bit(&self) -> u16 {
        if self.double_speed {
            0x2000
        } else {
            0x1000
        }
    }

    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.tag(b"TIMR");
        w.u16(self.counter);
        w.bytes(&[self.tima, self.tma, self.tac]);
        let (stage, left) = match self.reload {
            Reload::Idle => (0, 0),
            Reload::Pending(n) => (1, n),
            Reload::Reloading(n) => (2, n),
        };
        w.bytes(&[stage, left]);
        w.u32(self.div_apu);
        w.u32(self.div_apu_rises);
        let speed_at_reset = match self.speed_at_reset {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        };
        w.bytes(&[
            self.reset_in,
            u8::from(self.bit9_before),
            u8::from(self.div_apu_by_write),
            self.since_rise,
            speed_at_reset,
            u8::from(self.double_speed),
        ]);
        w.u32(self.serial_falls[0]);
        w.u32(self.serial_falls[1]);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"TIMR")?;
        self.counter = r.u16()?;
        self.tima = r.u8()?;
        self.tma = r.u8()?;
        self.tac = r.u8()?;
        let (stage, left) = (r.u8()?, r.u8()?);
        self.reload = match stage {
            0 => Reload::Idle,
            1 => Reload::Pending(left),
            2 => Reload::Reloading(left),
            _ => return Err(StateError::Corrupt("timer reload stage")),
        };
        self.div_apu = r.u32()?;
        self.div_apu_rises = r.u32()?;
        self.reset_in = r.u8()?.min(8);
        self.bit9_before = r.u8()? != 0;
        self.div_apu_by_write = r.u8()? != 0;
        self.since_rise = r.u8()?;
        self.speed_at_reset = match r.u8()? {
            0 => None,
            1 => Some(false),
            2 => Some(true),
            _ => return Err(StateError::Corrupt("timer speed switch")),
        };
        self.double_speed = r.bool()?;
        self.serial_falls = [r.u32()?, r.u32()?];
        Ok(())
    }

    /// The signal TIMA's falling-edge detector watches: the timer is enabled
    /// and the counter bit TAC selects is 1. Pan Docs numbers these bits per
    /// M-cycle (1, 3, 5, 7); counting T-cycles they're 3, 5, 7, 9.
    fn signal(&self) -> bool {
        let bit = match self.tac & 0x03 {
            0 => 9, // 4096 Hz
            1 => 3, // 262144 Hz
            2 => 5, // 65536 Hz
            _ => 7, // 16384 Hz
        };
        self.tac & 0x04 != 0 && (self.counter >> bit) & 1 == 1
    }

    /// Runs `change` and increments TIMA if it made the signal fall.
    /// Also counts both edges of counter bit 12 (13 in double speed) for the
    /// APU, including the falling one a DIV write causes when the bit was
    /// set (Pan Docs: Audio_details).
    fn update(&mut self, change: impl FnOnce(&mut Self)) {
        let before = self.signal();
        let apu_bit = self.div_apu_bit();
        let apu_bit_before = self.counter & apu_bit != 0;
        let counter_before = self.counter;
        change(self);
        self.count_serial_falls(counter_before);
        if before && !self.signal() {
            self.increment_tima();
        }
        match (apu_bit_before, self.counter & apu_bit != 0) {
            (true, false) => {
                self.div_apu += 1;
                self.div_apu_by_write = false;
            }
            (false, true) => {
                self.div_apu_rises += 1;
                self.since_rise = 0;
            }
            _ => {}
        }
    }

    fn count_serial_falls(&mut self, before: u16) {
        let fell = before & !self.counter;
        for (n, bit) in self.serial_falls.iter_mut().zip([0x80, 0x04]) {
            if fell & bit != 0 {
                *n += 1;
            }
        }
    }

    /// Falling edges of counter bit 7 and of bit 2 since the last call,
    /// which the bus forwards to the serial port.
    pub fn take_serial_falls(&mut self) -> [u32; 2] {
        std::mem::take(&mut self.serial_falls)
    }

    /// DIV-APU edges since the last call, which the bus forwards to the APU:
    /// (falling, rising, whether the last falling one was a DIV write).
    pub fn take_div_apu_edges(&mut self) -> (u32, u32, bool) {
        (
            std::mem::take(&mut self.div_apu),
            std::mem::take(&mut self.div_apu_rises),
            std::mem::take(&mut self.div_apu_by_write),
        )
    }

    /// Whether the DIV-APU bit is set now: the APU, switched on then, skips
    /// its first event (SameBoy, Core/apu.c, GB_apu_init).
    pub fn div_apu_bit_high(&self) -> bool {
        self.counter & self.div_apu_bit() != 0
    }

    /// On overflow TIMA reads $00 for one M-cycle; the reload comes after.
    fn increment_tima(&mut self) {
        let (v, overflow) = self.tima.overflowing_add(1);
        self.tima = v;
        if overflow {
            self.reload = Reload::Pending(4);
        }
    }

    /// Advances by `cycles` T-cycles. Returns true if TIMA was reloaded from
    /// TMA, which is when the timer interrupt is requested.
    pub fn tick(&mut self, cycles: u32) -> bool {
        let mut irq = false;
        for _ in 0..cycles {
            self.reload = match self.reload {
                Reload::Idle => Reload::Idle,
                Reload::Pending(1) => {
                    self.tima = self.tma;
                    irq = true;
                    Reload::Reloading(4)
                }
                Reload::Pending(n) => Reload::Pending(n - 1),
                Reload::Reloading(1) => Reload::Idle,
                Reload::Reloading(n) => Reload::Reloading(n - 1),
            };
            self.since_rise = self.since_rise.saturating_add(1);
            self.update(|t| t.counter = t.counter.wrapping_add(1));
            if self.reset_in > 0 {
                self.reset_in -= 1;
                match self.reset_in {
                    4 => self.bit9_before = self.counter & 0x200 != 0,
                    0 => self.stop_reset_lands(),
                    _ => {}
                }
            }
        }
        irq
    }

    /// STOP resets DIV, 8 T-cycles on with interrupts off (4 with them on,
    /// as SameBoy has it). Like any DIV reset it can bump TIMA, except that
    /// the 4096 Hz input must also have been set 4 T-cycles earlier. Measured
    /// on CPU CGB B/C by AGE's spsw-div (when DIV first ticks after a speed
    /// switch) and spsw-tima (which resets bump TIMA).
    pub fn stop_reset(&mut self, ime_off: bool) {
        self.reset_in = if ime_off { 8 } else { 4 };
        self.bit9_before = self.counter & 0x200 != 0;
    }

    fn stop_reset_lands(&mut self) {
        if self.tac & 0x07 == 0x04 {
            // 4096 Hz: the input must have been set 4 T-cycles before too.
            let tima_bumps = self.bit9_before && self.counter & 0x200 != 0;
            let apu_edge = self.counter & self.div_apu_bit() != 0;
            let counter_before = self.counter;
            self.counter = 0;
            self.count_serial_falls(counter_before);
            if tima_bumps {
                self.increment_tima();
            }
            if apu_edge && self.since_rise > 0 {
                self.div_apu += 1;
                self.div_apu_by_write = true;
            } else if apu_edge {
                // See reset_div: the rise this count made never happened.
                self.div_apu_rises = self.div_apu_rises.saturating_sub(1);
            }
        } else {
            self.reset_div(true);
        }
        if let Some(on) = self.speed_at_reset.take() {
            self.double_speed = on;
        }
    }

    /// Resets DIV, by a write or STOP. STOP's reset takes the place of
    /// that T-cycle's count: if the count raised the DIV-APU bit, the APU
    /// sees neither that rise nor the reset's fall (AGE's
    /// spsw-ch2-lc-delay).
    fn reset_div(&mut self, stop: bool) {
        let events = self.div_apu;
        let replaces_rise = stop && self.since_rise == 0;
        self.update(|t| t.counter = 0);
        if self.div_apu > events {
            if replaces_rise {
                self.div_apu -= 1;
                self.div_apu_rises = self.div_apu_rises.saturating_sub(1);
            } else {
                self.div_apu_by_write = true;
            }
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF04 => (self.counter >> 8) as u8,
            0xFF05 => self.tima,
            0xFF06 => self.tma,
            0xFF07 => self.tac | 0xF8,
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        match addr {
            // Resetting the counter can make the watched bit fall.
            0xFF04 => self.reset_div(false),
            0xFF05 => match self.reload {
                Reload::Pending(_) => {
                    self.tima = val;
                    self.reload = Reload::Idle;
                }
                Reload::Reloading(_) => {}
                Reload::Idle => self.tima = val,
            },
            0xFF06 => {
                self.tma = val;
                if let Reload::Reloading(_) = self.reload {
                    self.tima = val;
                }
            }
            // A new clock select or disabling the timer can make it fall too
            // (DMG behavior; CGB differs).
            0xFF07 => self.update(|t| t.tac = val & 0x07),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_original_boot_rom_leaves_div_at_ab() {
        assert_eq!(Timer::post_boot(Model::Dmg).read(0xFF04), 0xAB);
    }

    #[test]
    fn div_counts_every_256_cycles() {
        let mut t = Timer::new();
        t.tick(255);
        assert_eq!(t.read(0xFF04), 0);
        t.tick(1);
        assert_eq!(t.read(0xFF04), 1);
        t.write(0xFF04, 0x77);
        assert_eq!(t.read(0xFF04), 0, "any write resets DIV");
    }

    #[test]
    fn div_bit_4_falling_clocks_the_apu_at_512_hz() {
        let mut t = Timer::new();
        t.tick(8192 - 1);
        assert_eq!(t.take_div_apu_edges().0, 0);
        t.tick(1);
        assert_eq!(t.take_div_apu_edges().0, 1, "one per 8192 T-cycles");
        t.tick(crate::CPU_HZ);
        assert_eq!(t.take_div_apu_edges().0, 512);
    }

    #[test]
    fn in_double_speed_div_bit_5_clocks_the_apu_so_it_stays_at_512_hz() {
        // Double speed: twice the CPU cycles per second, so 2 * CPU_HZ of
        // them is still one second, and still 512 events.
        let mut t = Timer::new();
        t.set_double_speed(true);
        t.tick(16384 - 1);
        assert_eq!(t.take_div_apu_edges().0, 0, "not at 8192 any more");
        t.tick(1);
        assert_eq!(t.take_div_apu_edges().0, 1);
        t.tick(2 * crate::CPU_HZ);
        assert_eq!(t.take_div_apu_edges().0, 512);
    }

    #[test]
    fn writing_div_can_clock_the_apu_early() {
        let mut t = Timer::new();
        t.tick(4096); // counter bit 12 (DIV bit 4) rises
        assert_eq!(t.take_div_apu_edges(), (0, 1, false));
        t.write(0xFF04, 0);
        assert_eq!(
            t.take_div_apu_edges(),
            (1, 0, true),
            "the reset is a falling edge"
        );
        t.tick(4095);
        t.write(0xFF04, 0); // bit 12 was 0: no edge
        assert_eq!(t.take_div_apu_edges(), (0, 0, false));
    }

    #[test]
    fn stops_reset_landing_as_the_apu_bit_would_rise_hides_both_edges() {
        // The reset takes the place of that T-cycle's count: from $0FF8 the
        // 8th count would make $1000. One T-cycle earlier, the bit rises and
        // the reset is a falling edge as usual.
        for (start, edges) in [(0x0FF8, (0, 0, false)), (0x0FF9, (1, 1, true))] {
            let mut t = Timer::new();
            t.counter = start;
            t.stop_reset(true);
            t.tick(8);
            assert_eq!(t.take_div_apu_edges(), edges, "from {start:#06x}");
        }
    }

    #[test]
    fn div_apu_moves_to_the_other_bit_when_stops_reset_lands() {
        // In double speed with bit 13 set and bit 12 clear: back to normal
        // speed, the APU still watches bit 13 until STOP's reset clears it.
        let mut t = Timer::new();
        t.set_double_speed(true);
        t.counter = 0x2000;
        t.stop_reset(true);
        t.set_double_speed(false);
        assert!(t.double_speed, "not yet");
        t.tick(8);
        assert!(!t.double_speed);
        assert_eq!(t.take_div_apu_edges(), (1, 0, true), "bit 13 fell");
    }

    #[test]
    fn stop_resets_div_8_cycles_on_or_4_with_interrupts_on() {
        for (ime_off, delay) in [(true, 8), (false, 4)] {
            let mut t = Timer::new();
            t.tick(0x500);
            t.stop_reset(ime_off);
            t.tick(delay - 1);
            assert_ne!(t.read(0xFF04), 0, "not yet");
            t.tick(1);
            assert_eq!(t.read(0xFF04), 0, "{delay} cycles on");
        }
    }

    #[test]
    fn stops_reset_bumps_a_4096_hz_tima_only_if_its_bit_was_set_4_cycles_before() {
        // Counter bit 9 is the 4096 Hz input. A plain DIV reset bumps TIMA
        // whenever it's set; STOP's also needs it set 4 T-cycles earlier.
        for (start, bumps) in [(0x200, true), (0x1FA, false)] {
            let mut t = Timer::new();
            t.write(0xFF07, 0b100);
            t.counter = start;
            t.stop_reset(true);
            t.tick(8);
            assert_eq!(t.read(0xFF05), u8::from(bumps), "from {start:#x}");
        }
    }

    #[test]
    fn tima_counts_at_selected_rate() {
        let mut t = Timer::new();
        t.write(0xFF07, 0b101); // enabled, every 16 T-cycles
        t.tick(16 * 3);
        assert_eq!(t.read(0xFF05), 3);
    }

    /// A timer at the fastest rate (every 16 T-cycles) about to overflow, with
    /// TMA = $42.
    fn about_to_overflow() -> Timer {
        let mut t = Timer::new();
        t.write(0xFF06, 0x42);
        t.write(0xFF05, 0xFF);
        t.write(0xFF07, 0b101);
        t
    }

    #[test]
    fn tima_overflow_reads_zero_for_one_m_cycle_then_reloads() {
        let mut t = about_to_overflow();
        assert!(!t.tick(16), "overflow itself doesn't request the interrupt");
        assert_eq!(t.read(0xFF05), 0x00);
        assert!(!t.tick(3));
        assert_eq!(t.read(0xFF05), 0x00, "still $00 for the whole M-cycle");
        assert!(t.tick(1), "reload and interrupt 4 T-cycles after overflow");
        assert_eq!(t.read(0xFF05), 0x42);
    }

    #[test]
    fn writing_tima_while_it_reads_zero_cancels_the_reload() {
        let mut t = about_to_overflow();
        t.tick(16);
        t.write(0xFF05, 0x10);
        assert!(!t.tick(8), "no interrupt");
        assert_eq!(t.read(0xFF05), 0x10, "no TMA reload");
    }

    #[test]
    fn writing_tima_during_the_reload_cycle_is_ignored() {
        let mut t = about_to_overflow();
        t.tick(20);
        t.write(0xFF05, 0x10);
        assert_eq!(t.read(0xFF05), 0x42);
        t.tick(4);
        t.write(0xFF05, 0x10);
        assert_eq!(t.read(0xFF05), 0x10, "writes work again a cycle later");
    }

    #[test]
    fn writing_tma_during_the_reload_cycle_reaches_tima() {
        let mut t = about_to_overflow();
        t.tick(20);
        t.write(0xFF06, 0x99);
        assert_eq!(t.read(0xFF05), 0x99);
        assert_eq!(t.read(0xFF06), 0x99);
    }

    #[test]
    fn writing_div_ticks_tima_if_the_watched_bit_was_set() {
        // Fastest rate watches counter bit 3.
        let mut t = Timer::new();
        t.write(0xFF07, 0b101);
        t.tick(8); // bit 3 is now 1
        t.write(0xFF04, 0);
        assert_eq!(t.read(0xFF05), 1, "reset made bit 3 fall");

        t.tick(4); // bit 3 is 0
        t.write(0xFF04, 0);
        assert_eq!(t.read(0xFF05), 1, "no falling edge, no tick");
    }

    #[test]
    fn writing_tac_ticks_tima_when_the_signal_falls() {
        // Switch from bit 3 (set) to bit 5 (clear): falling edge.
        let mut t = Timer::new();
        t.write(0xFF07, 0b101);
        t.tick(8);
        t.write(0xFF07, 0b110);
        assert_eq!(t.read(0xFF05), 1);

        // Disabling while the watched bit is set: falling edge (DMG).
        let mut t = Timer::new();
        t.write(0xFF07, 0b101);
        t.tick(8);
        t.write(0xFF07, 0b001);
        assert_eq!(t.read(0xFF05), 1);

        // Enabling while the bit is set is a rising edge: nothing.
        let mut t = Timer::new();
        t.write(0xFF07, 0b001);
        t.tick(8);
        t.write(0xFF07, 0b101);
        assert_eq!(t.read(0xFF05), 0);
    }

    #[test]
    fn disabled_timer_does_not_count() {
        let mut t = Timer::new();
        t.write(0xFF07, 0b001);
        t.tick(1000);
        assert_eq!(t.read(0xFF05), 0);
    }
}
