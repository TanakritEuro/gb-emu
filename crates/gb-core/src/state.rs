//! Save states: the whole machine as bytes, and back.
//!
//! A state holds everything that changes while a game runs (CPU registers,
//! RAM, VRAM, every chip's internal counters, the cartridge's MBC registers,
//! RAM and clock). It leaves out the ROM, which never changes, and keeps a
//! fingerprint of it instead, so a state can't be loaded into another game.
//! It also leaves out what belongs to the host rather than the Game Boy:
//! the audio output rate, buttons being held, breakpoints.
//!
//! Layout (little-endian):
//!
//! ```text
//! "GBST"  magic
//! u16     format version (VERSION)
//! u64     ROM fingerprint (FNV-1a over the whole ROM)
//! u32     payload length
//! ...     payload: one section per component, each starting with a tag
//! u32     FNV-1a checksum of the payload
//! ```
//!
//! Each component writes and reads its own section (`save_state` /
//! `load_state` next to its fields), in the order `GameBoy::save_state`
//! calls them. Any change to what a section holds must bump [`VERSION`].

use crate::Model;
use std::fmt;

const MAGIC: &[u8; 4] = b"GBST";
/// Bump whenever any section's contents change.
/// 2: Game Boy Color: the model, WRAM and VRAM banks, KEY1.
/// 3: Color palettes, and the Color's picture in RGB555.
/// 4: OPRI, the Color's sprite priority mode.
/// 5: the Color's VRAM DMA.
/// 6: the serial port in its own section, with transfer progress.
/// 7: OAM DMA in progress (it now takes its real 160 M-cycles); where the
/// line's mode 3 ends.
/// 8: the pixel FIFO mid-line (fetcher, both FIFOs, the line's sprites),
/// and the window's line counter counting from $FF.
/// 9: whether the CPU has only just halted.
/// 10: mid-line glitches: whether the window's blank pixel is off for the rest
/// of the line, LCDC bit 4 as the tile data address was worked out, and the
/// Color's tile-select latch.
/// 11: the speed switch under way (its pause, the PPU freeze), double
/// speed's half dot.
/// 12: the APU counts in its own 2 MHz ticks, as SameBoy does (the channels'
/// counters, the frame sequencer's divider, the sweep's calculation); the
/// timer passes it both DIV-APU edges.
/// 13: the OAM scan under way (the sprites found so far, the Y and X on its
/// bus).
/// 14: the Color's $FEA0-$FEFF bytes.
/// 15: whether an illegal opcode locked the CPU up.
/// 16: the PPU's WY check after a WY write.
pub const VERSION: u16 = 18;
/// Bytes before the payload: magic, version, fingerprint, length.
const HEADER_LEN: usize = 4 + 2 + 8 + 4;

/// Why a save state couldn't be loaded. The machine is left as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    /// Not a save state at all.
    NotAState,
    /// Made by a different version of the emulator.
    Version(u16),
    /// Made with a different ROM.
    WrongGame,
    /// Damaged: cut short, a bad checksum, or values that don't fit.
    Corrupt(&'static str),
    /// Made on the other console (the one given), for the same game.
    WrongModel(Model),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAState => write!(f, "not a save state"),
            Self::Version(v) => write!(
                f,
                "save state format {v} is from another version of the emulator (this one reads {VERSION})"
            ),
            Self::WrongGame => write!(f, "this save state is for a different game"),
            Self::Corrupt(what) => write!(f, "damaged save state: {what}"),
            Self::WrongModel(model) => write!(
                f,
                "this save state was made on the {}",
                match model {
                    Model::Dmg => "Game Boy",
                    Model::Cgb => "Game Boy Color",
                }
            ),
        }
    }
}

impl std::error::Error for StateError {}

/// FNV-1a, 64-bit: the ROM fingerprint.
pub fn fnv1a64(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// FNV-1a, 32-bit: the payload checksum.
fn fnv1a32(data: &[u8]) -> u32 {
    data.iter().fold(0x811c_9dc5, |h, &b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// Builds a state's payload; [`StateWriter::finish`] wraps it in the header.
pub struct StateWriter {
    buf: Vec<u8>,
}

impl StateWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(32 * 1024),
        }
    }

    /// Starts a component's section.
    pub fn tag(&mut self, tag: &[u8; 4]) {
        self.buf.extend_from_slice(tag);
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Bytes whose length the reader already knows (e.g. a fixed array).
    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    /// Bytes preceded by their length, for buffers whose size varies (cart RAM).
    pub fn sized_bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.bytes(v);
    }

    /// The finished state for the ROM with fingerprint `rom_hash`.
    pub fn finish(self, rom_hash: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.buf.len() + 4);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&rom_hash.to_le_bytes());
        out.extend_from_slice(&(self.buf.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.buf);
        out.extend_from_slice(&fnv1a32(&self.buf).to_le_bytes());
        out
    }
}

impl Default for StateWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads a state's payload back, after [`StateReader::open`] has checked
/// the header and checksum.
pub struct StateReader<'a> {
    data: &'a [u8],
    pos: usize,
}

type Result<T> = std::result::Result<T, StateError>;

impl<'a> StateReader<'a> {
    /// Checks the header, fingerprint and checksum of `state`, and returns a
    /// reader over its payload.
    pub fn open(state: &'a [u8], rom_hash: u64) -> Result<Self> {
        if state.len() < HEADER_LEN || &state[..4] != MAGIC {
            return Err(StateError::NotAState);
        }
        let version = u16::from_le_bytes([state[4], state[5]]);
        if version != VERSION {
            return Err(StateError::Version(version));
        }
        let hash = u64::from_le_bytes(state[6..14].try_into().unwrap_or_default());
        if hash != rom_hash {
            return Err(StateError::WrongGame);
        }
        let len = u32::from_le_bytes(state[14..18].try_into().unwrap_or_default()) as usize;
        let payload = state
            .get(HEADER_LEN..HEADER_LEN + len)
            .ok_or(StateError::Corrupt("cut short"))?;
        let sum = state
            .get(HEADER_LEN + len..)
            .filter(|rest| rest.len() == 4)
            .ok_or(StateError::Corrupt("wrong length"))?;
        if fnv1a32(payload).to_le_bytes() != sum {
            return Err(StateError::Corrupt("checksum mismatch"));
        }
        Ok(Self {
            data: payload,
            pos: 0,
        })
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let out = self
            .data
            .get(self.pos..self.pos + n)
            .ok_or(StateError::Corrupt("cut short"))?;
        self.pos += n;
        Ok(out)
    }

    /// Checks that the next section is `tag`.
    pub fn tag(&mut self, tag: &[u8; 4]) -> Result<()> {
        if self.take(4)? == tag {
            Ok(())
        } else {
            Err(StateError::Corrupt("sections out of order"))
        }
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(StateError::Corrupt("bad flag")),
        }
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes([self.u8()?, self.u8()?]))
    }
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from(self.u32()?) | (u64::from(self.u32()?) << 32))
    }
    /// Fills `out` (a fixed-size buffer).
    pub fn bytes(&mut self, out: &mut [u8]) -> Result<()> {
        out.copy_from_slice(self.take(out.len())?);
        Ok(())
    }
    /// Fills `out` from bytes written with `sized_bytes`, which must be
    /// exactly `out.len()` long.
    pub fn sized_bytes(&mut self, out: &mut [u8]) -> Result<()> {
        if self.u32()? as usize != out.len() {
            return Err(StateError::Corrupt("buffer size doesn't match"));
        }
        self.bytes(out)
    }

    /// Checks that everything was read.
    pub fn finish(self) -> Result<()> {
        if self.pos == self.data.len() {
            Ok(())
        } else {
            Err(StateError::Corrupt("unexpected bytes at the end"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut w = StateWriter::new();
        w.tag(b"TEST");
        w.u8(0x12);
        w.bool(true);
        w.u16(0x3456);
        w.u32(0x789A_BCDE);
        w.u64(0x0102_0304_0506_0708);
        w.bytes(&[1, 2, 3]);
        w.sized_bytes(&[9, 8]);
        w.finish(42)
    }

    #[test]
    fn values_round_trip() {
        let state = sample();
        let mut r = StateReader::open(&state, 42).unwrap();
        r.tag(b"TEST").unwrap();
        assert_eq!(r.u8(), Ok(0x12));
        assert_eq!(r.bool(), Ok(true));
        assert_eq!(r.u16(), Ok(0x3456));
        assert_eq!(r.u32(), Ok(0x789A_BCDE));
        assert_eq!(r.u64(), Ok(0x0102_0304_0506_0708));
        let mut three = [0; 3];
        r.bytes(&mut three).unwrap();
        assert_eq!(three, [1, 2, 3]);
        let mut two = [0; 2];
        r.sized_bytes(&mut two).unwrap();
        assert_eq!(two, [9, 8]);
        r.finish().unwrap();
    }

    #[test]
    fn the_header_is_checked() {
        let state = sample();
        assert_eq!(
            StateReader::open(b"nope", 42).err(),
            Some(StateError::NotAState)
        );
        assert_eq!(
            StateReader::open(&state, 7).err(),
            Some(StateError::WrongGame)
        );
        let mut old = state.clone();
        old[4] = 0;
        old[5] = 0;
        assert_eq!(
            StateReader::open(&old, 42).err(),
            Some(StateError::Version(0))
        );
    }

    #[test]
    fn damage_is_caught() {
        let state = sample();
        let mut flipped = state.clone();
        flipped[HEADER_LEN + 5] ^= 0x01;
        assert_eq!(
            StateReader::open(&flipped, 42).err(),
            Some(StateError::Corrupt("checksum mismatch"))
        );
        let short = &state[..state.len() - 6];
        assert!(matches!(
            StateReader::open(short, 42),
            Err(StateError::Corrupt(_))
        ));
        let mut long = state.clone();
        long.push(0);
        assert!(matches!(
            StateReader::open(&long, 42),
            Err(StateError::Corrupt(_))
        ));
    }

    #[test]
    fn reading_checks_tags_sizes_and_leftovers() {
        let state = sample();
        let mut r = StateReader::open(&state, 42).unwrap();
        assert!(r.tag(b"NOPE").is_err());

        let mut r = StateReader::open(&state, 42).unwrap();
        r.tag(b"TEST").unwrap();
        r.u8().unwrap();
        r.bool().unwrap();
        r.u16().unwrap();
        r.u32().unwrap();
        r.u64().unwrap();
        r.bytes(&mut [0; 3]).unwrap();
        assert!(r.sized_bytes(&mut [0; 5]).is_err(), "size must match");

        let mut r = StateReader::open(&state, 42).unwrap();
        r.tag(b"TEST").unwrap();
        assert!(r.finish().is_err(), "unread bytes");
    }

    #[test]
    fn a_bad_flag_is_corrupt() {
        let mut w = StateWriter::new();
        w.u8(2);
        let state = w.finish(1);
        let mut r = StateReader::open(&state, 1).unwrap();
        assert!(r.bool().is_err());
    }
}
