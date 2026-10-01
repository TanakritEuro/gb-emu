//! DIV/TIMA/TMA/TAC timer.
//!
//! DIV is the upper byte of a 16-bit counter that ticks every T-cycle. TIMA
//! increments when a selected bit of that counter falls from 1 to 0, which is
//! how real hardware does it (and why writing DIV can bump TIMA).
//!
//! Reference: https://gbdev.io/pandocs/Timer_and_Divider_Registers.html

#[derive(Debug, Clone, Default)]
pub struct Timer {
    counter: u16,
    tima: u8,
    tma: u8,
    tac: u8,
}

impl Timer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Which bit of the internal counter clocks TIMA, if the timer is enabled.
    fn tima_bit(&self) -> Option<u16> {
        let bit = match self.tac & 0x03 {
            0 => 9, // 4096 Hz
            1 => 3, // 262144 Hz
            2 => 5, // 65536 Hz
            _ => 7, // 16384 Hz
        };
        (self.tac & 0x04 != 0).then_some(bit)
    }

    fn increment_tima(&mut self) -> bool {
        let (v, overflow) = self.tima.overflowing_add(1);
        // TODO(accuracy): on hardware the reload and interrupt happen 4
        // T-cycles after the overflow, and TIMA reads 0 meanwhile.
        self.tima = if overflow { self.tma } else { v };
        overflow
    }

    /// Advances by `cycles` T-cycles. Returns true if TIMA overflowed.
    pub fn tick(&mut self, cycles: u32) -> bool {
        let mut irq = false;
        for _ in 0..cycles {
            let old = self.counter;
            self.counter = self.counter.wrapping_add(1);
            if let Some(bit) = self.tima_bit() {
                if (old >> bit) & 1 == 1 && (self.counter >> bit) & 1 == 0 {
                    irq |= self.increment_tima();
                }
            }
        }
        irq
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
            0xFF04 => self.counter = 0,
            0xFF05 => self.tima = val,
            0xFF06 => self.tma = val,
            0xFF07 => self.tac = val & 0x07,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tima_counts_at_selected_rate() {
        let mut t = Timer::new();
        t.write(0xFF07, 0b101); // enabled, every 16 T-cycles
        t.tick(16 * 3);
        assert_eq!(t.read(0xFF05), 3);
    }

    #[test]
    fn tima_overflow_reloads_tma_and_requests_interrupt() {
        let mut t = Timer::new();
        t.write(0xFF06, 0x42);
        t.write(0xFF05, 0xFF);
        t.write(0xFF07, 0b101);
        assert!(t.tick(16));
        assert_eq!(t.read(0xFF05), 0x42);
    }

    #[test]
    fn disabled_timer_does_not_count() {
        let mut t = Timer::new();
        t.write(0xFF07, 0b001);
        t.tick(1000);
        assert_eq!(t.read(0xFF05), 0);
    }
}
