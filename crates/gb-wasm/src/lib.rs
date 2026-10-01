//! JavaScript-facing wrapper around [`gb_core::GameBoy`].
//!
//! Errors become thrown JS `Error`s, so web/main.js can show messages like
//! "unimplemented opcode 3E at $0150" right on the page.

use gb_core::{Button, GameBoy};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct Emulator {
    gb: GameBoy,
}

#[wasm_bindgen]
impl Emulator {
    #[wasm_bindgen(constructor)]
    pub fn new(rom: Vec<u8>) -> Result<Emulator, JsError> {
        GameBoy::new(rom)
            .map(|gb| Emulator { gb })
            .map_err(|e| JsError::new(&e.to_string()))
    }

    pub fn run_frame(&mut self) -> Result<(), JsError> {
        self.gb
            .run_frame()
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// RGBA, 160 × 144 × 4 bytes. Copies each frame (about 92 KB), which is
    /// fine at 60 fps; a zero-copy view into wasm memory is an easy later win.
    pub fn framebuffer(&self) -> Vec<u8> {
        self.gb.framebuffer().to_vec()
    }

    /// `button` is the index of a `gb_core::Button`:
    /// 0 Right, 1 Left, 2 Up, 3 Down, 4 A, 5 B, 6 Select, 7 Start.
    pub fn set_button(&mut self, button: u8, pressed: bool) {
        if let Some(b) = Button::from_index(button) {
            self.gb.set_button(b, pressed);
        }
    }

    pub fn title(&self) -> String {
        self.gb.title().to_string()
    }

    pub fn take_serial(&mut self) -> String {
        self.gb.take_serial_output()
    }
}
