//! The joypad register ($FF00). The game selects a row (d-pad or buttons)
//! by writing bits 4-5, then reads the low nibble, where 0 means pressed.
//!
//! Reference: https://gbdev.io/pandocs/Joypad_Input.html

use crate::state::{StateError, StateReader, StateWriter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Button {
    Right = 0,
    Left = 1,
    Up = 2,
    Down = 3,
    A = 4,
    B = 5,
    Select = 6,
    Start = 7,
}

impl Button {
    pub const ALL: [Button; 8] = [
        Button::Right,
        Button::Left,
        Button::Up,
        Button::Down,
        Button::A,
        Button::B,
        Button::Select,
        Button::Start,
    ];

    pub fn from_index(i: u8) -> Option<Self> {
        Self::ALL.get(i as usize).copied()
    }
}

#[derive(Debug, Clone)]
pub struct Joypad {
    /// One bit per [`Button`], 1 = held.
    held: u8,
    /// Bits 4-5 as last written; a 0 bit selects that row.
    select: u8,
}

impl Default for Joypad {
    fn default() -> Self {
        Self::new()
    }
}

impl Joypad {
    pub fn new() -> Self {
        Self {
            held: 0,
            select: 0x30,
        }
    }

    pub fn read(&self) -> u8 {
        let mut low = 0x0F;
        if self.select & 0x10 == 0 {
            low &= !(self.held & 0x0F);
        }
        if self.select & 0x20 == 0 {
            low &= !(self.held >> 4);
        }
        0xC0 | self.select | low
    }

    /// Only the row selection: which buttons are held belongs to the player,
    /// not to the state, so loading one never leaves a button stuck down.
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.tag(b"JOYP");
        w.u8(self.select);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"JOYP")?;
        self.select = r.u8()? & 0x30;
        Ok(())
    }

    pub fn write(&mut self, val: u8) {
        self.select = val & 0x30;
    }

    /// Updates a button. Returns true on a new press, which requests the
    /// joypad interrupt.
    pub fn set(&mut self, button: Button, pressed: bool) -> bool {
        let mask = 1 << button as u8;
        let was_held = self.held & mask != 0;
        if pressed {
            self.held |= mask;
        } else {
            self.held &= !mask;
        }
        pressed && !was_held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_selected_reads_all_released() {
        let mut j = Joypad::new();
        j.set(Button::Start, true);
        assert_eq!(j.read() & 0x0F, 0x0F);
    }

    #[test]
    fn selected_row_shows_pressed_as_zero() {
        let mut j = Joypad::new();
        j.set(Button::A, true);
        j.set(Button::Left, true);
        j.write(0x10); // bit 5 low: action buttons
        assert_eq!(j.read() & 0x0F, 0b1110);
        j.write(0x20); // bit 4 low: d-pad
        assert_eq!(j.read() & 0x0F, 0b1101);
    }

    #[test]
    fn only_new_presses_request_interrupt() {
        let mut j = Joypad::new();
        assert!(j.set(Button::B, true));
        assert!(!j.set(Button::B, true));
        assert!(!j.set(Button::B, false));
    }
}
