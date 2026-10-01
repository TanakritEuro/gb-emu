//! The audio processing unit: four sound channels, mixed to stereo samples.
//!
//! Each channel is a counter clocked from the CPU clock that steps through a
//! waveform: two square waves, a channel that plays 32 4-bit samples from wave
//! RAM, and a noise channel driven by a shift register. Their digital levels
//! (0-15) go through a DAC each, the mixer adds them per side as NR51 routes
//! them, NR50 scales each side, and a high-pass filter (a capacitor on real
//! hardware) removes the DC offset. Frontends take the result as interleaved
//! stereo f32 samples at a rate they choose.
//!
//! A frame sequencer, clocked at 512 Hz from DIV, shapes the notes: length
//! timers stop them, envelopes fade them in or out, and CH1's sweep bends
//! its pitch.
//!
//! References: https://gbdev.io/pandocs/Audio.html,
//! https://gbdev.io/pandocs/Audio_Registers.html,
//! https://gbdev.io/pandocs/Audio_details.html

use crate::state::{StateError, StateReader, StateWriter};
use crate::CPU_HZ;

/// The four duty cycles' 8-step waveforms (Pan Docs' NR11 table).
const DUTY: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1], // 12.5%
    [0, 0, 0, 0, 0, 0, 1, 1], // 25%
    [0, 0, 0, 0, 1, 1, 1, 1], // 50%
    [1, 1, 1, 1, 1, 1, 0, 0], // 75%
];

/// Bits that always read back as 1 in $FF10-$FF25: write-only bits and unused
/// registers. (From blargg's dmg_sound "registers" test, as tabulated on the
/// gbdev wiki's "Gameboy sound hardware" page.)
const READ_MASK: [u8; 0x16] = [
    0x80, 0x3F, 0x00, 0xFF, 0xBF, // NR10-NR14
    0xFF, 0x3F, 0x00, 0xFF, 0xBF, // (unused), NR21-NR24
    0x7F, 0xFF, 0x9F, 0xFF, 0xBF, // NR30-NR34
    0xFF, 0xFF, 0x00, 0x00, 0xBF, // (unused), NR41-NR44
    0x00, 0x00, // NR50, NR51
];

/// Output samples kept if nobody takes them (the CLI without --wav): about
/// two seconds, so memory stays bounded.
const MAX_BUFFERED: usize = 2 * 48_000 * 2;

/// A length timer: switches its channel off after a set time. NRx1 sets it
/// (to 64 - n, or 256 - n on CH3), the frame sequencer clocks it at 256 Hz
/// while NRx4 bit 6 is set, and reaching 0 turns the channel off.
#[derive(Debug, Clone, Default)]
struct Length {
    counter: u16,
    enabled: bool,
}

impl Length {
    /// One 256 Hz clock. True if this one ran the timer out.
    fn clock(&mut self) -> bool {
        if self.enabled && self.counter > 0 {
            self.counter -= 1;
            self.counter == 0
        } else {
            false
        }
    }

    /// NRx4's length-enable bit, and on trigger the reload of an expired
    /// timer, with the quirks blargg's dmg_sound tests check (as described
    /// on the gbdev wiki): if the frame sequencer's next step won't clock
    /// lengths, turning length on clocks it once right away, and a trigger
    /// that reloads it with length on loads one less. Returns false if that
    /// extra clock ran the timer out without a trigger: the channel goes off.
    fn write_nrx4(&mut self, max: u16, val: u8, next_step: u8) -> bool {
        let enable = val & 0x40 != 0;
        let trigger = val & 0x80 != 0;
        let next_skips_length = next_step % 2 == 1;
        let mut keep = true;
        if next_skips_length && !self.enabled && enable && self.counter > 0 {
            self.counter -= 1;
            keep = self.counter > 0 || trigger;
        }
        self.enabled = enable;
        if trigger && self.counter == 0 {
            self.counter = if enable && next_skips_length {
                max - 1
            } else {
                max
            };
        }
        keep
    }
}

/// A volume envelope (CH1, CH2, CH4): every `pace` 64 Hz ticks the volume
/// moves one step up or down until it reaches 15 or 0. Pace 0 holds it.
#[derive(Debug, Clone, Default)]
struct Envelope {
    /// NRx2 as written: initial volume (bits 4-7), up (bit 3), pace (0-2).
    nrx2: u8,
    volume: u8,
    /// Direction and pace, taken from NRx2 when the channel triggers.
    up: bool,
    pace: u8,
    timer: u8,
}

impl Envelope {
    /// NRx2 bits 3-7 all clear (volume 0, going down) turns the DAC off.
    fn dac_on(&self) -> bool {
        self.nrx2 & 0xF8 != 0
    }

    fn trigger(&mut self) {
        self.volume = self.nrx2 >> 4;
        self.up = self.nrx2 & 0x08 != 0;
        self.pace = self.nrx2 & 0x07;
        self.timer = self.pace;
    }

    /// One 64 Hz tick.
    /// TODO(accuracy): writing NRx2 while the channel plays changes the
    /// volume in odd ways on hardware ("zombie mode").
    fn clock(&mut self) {
        if self.pace == 0 {
            return;
        }
        self.timer = self.timer.saturating_sub(1);
        if self.timer == 0 {
            self.timer = self.pace;
            if self.up && self.volume < 15 {
                self.volume += 1;
            } else if !self.up && self.volume > 0 {
                self.volume -= 1;
            }
        }
    }
}

/// CH1's frequency sweep, driven by NR10: pace (bits 4-6), subtract (bit 3)
/// and step (bits 0-2). Every `pace` 128 Hz ticks the period moves by
/// period >> step, computed from a shadow copy taken at trigger; a result
/// past 2047 turns CH1 off. https://gbdev.io/pandocs/Audio_Registers.html
#[derive(Debug, Clone, Default)]
struct Sweep {
    enabled: bool,
    shadow: u16,
    timer: u8,
    /// A subtraction has been done since the last trigger: clearing the
    /// subtract bit now turns CH1 off (Pan Docs: Audio_details).
    subtracted: bool,
}

/// A square wave channel (CH1, CH2).
#[derive(Debug, Clone, Default)]
struct Square {
    enabled: bool,
    dac: bool,
    duty: u8,
    /// Position in the 8-step duty waveform.
    step: u8,
    /// 11-bit period value from NRx3/NRx4.
    period: u16,
    /// T-cycles until the next duty step.
    timer: u32,
    env: Envelope,
    length: Length,
}

impl Square {
    /// The duty position moves at 1048576 / (2048 - period) Hz, every
    /// (2048 - period) * 4 T-cycles.
    fn period_cycles(&self) -> u32 {
        (2048 - u32::from(self.period)) * 4
    }

    fn tick(&mut self, mut cycles: u32) {
        while cycles >= self.timer {
            cycles -= self.timer;
            self.timer = self.period_cycles();
            self.step = (self.step + 1) & 7;
        }
        self.timer -= cycles;
    }

    fn digital(&self) -> u8 {
        if self.enabled {
            DUTY[usize::from(self.duty)][usize::from(self.step)] * self.env.volume
        } else {
            0
        }
    }

    /// Restarts the note: the channel turns on (if its DAC is), the period
    /// timer reloads and the envelope restarts. The duty position carries on.
    fn trigger(&mut self) {
        self.enabled = self.dac;
        self.timer = self.period_cycles();
        self.env.trigger();
    }

    fn set_envelope(&mut self, val: u8) {
        self.env.nrx2 = val;
        self.dac = self.env.dac_on();
        self.enabled &= self.dac;
    }
}

/// The wave channel (CH3).
#[derive(Debug, Clone, Default)]
struct Wave {
    enabled: bool,
    dac: bool,
    /// NR32 bits 5-6: 0 mute, 1 full, 2 half, 3 quarter volume.
    level: u8,
    period: u16,
    timer: u32,
    /// Which of the 32 samples was read last.
    position: u8,
    /// The last sample read; what the channel is playing.
    sample: u8,
    length: Length,
}

impl Wave {
    /// Samples advance at 2097152 / (2048 - period) Hz.
    fn period_cycles(&self) -> u32 {
        (2048 - u32::from(self.period)) * 2
    }

    fn tick(&mut self, mut cycles: u32, ram: &[u8; 16]) {
        while cycles >= self.timer {
            cycles -= self.timer;
            self.timer = self.period_cycles();
            self.position = (self.position + 1) & 31;
            // Upper nibble first.
            let byte = ram[usize::from(self.position / 2)];
            self.sample = if self.position.is_multiple_of(2) {
                byte >> 4
            } else {
                byte & 0x0F
            };
        }
        self.timer -= cycles;
    }

    fn digital(&self) -> u8 {
        match (self.enabled, self.level) {
            (false, _) | (true, 0) => 0,
            (true, level) => self.sample >> (level - 1),
        }
    }

    /// Restarts at the top of wave RAM. The first sample read is index 1;
    /// until then the previous sample keeps playing (Pan Docs).
    /// TODO(accuracy): hardware also waits a few cycles before that first read.
    fn trigger(&mut self) {
        self.enabled = self.dac;
        self.timer = self.period_cycles();
        self.position = 0;
    }
}

/// The noise channel (CH4).
#[derive(Debug, Clone, Default)]
struct Noise {
    enabled: bool,
    dac: bool,
    env: Envelope,
    length: Length,
    /// NR43: clock shift (bits 4-7), 7-bit mode (bit 3), divider (bits 0-2).
    nr43: u8,
    lfsr: u16,
    timer: u32,
}

impl Noise {
    /// The LFSR steps at 262144 / (divider * 2^shift) Hz with divider 0
    /// counting as 0.5: every (divider ? 16 * divider : 8) << shift T-cycles.
    fn period_cycles(&self) -> u32 {
        let divider = u32::from(self.nr43 & 7);
        let base = if divider == 0 { 8 } else { 16 * divider };
        base << (self.nr43 >> 4)
    }

    fn tick(&mut self, mut cycles: u32) {
        // Clock shifts 14 and 15 stop the LFSR.
        if self.nr43 >> 4 >= 14 {
            return;
        }
        while cycles >= self.timer {
            cycles -= self.timer;
            self.timer = self.period_cycles();
            self.step_lfsr();
        }
        self.timer -= cycles;
    }

    /// XNOR of bits 0 and 1 goes into bit 15 (and bit 7 in 7-bit mode), then
    /// everything shifts right. https://gbdev.io/pandocs/Audio_details.html
    fn step_lfsr(&mut self) {
        let bit = !(self.lfsr ^ (self.lfsr >> 1)) & 1;
        self.lfsr = (self.lfsr & 0x7FFF) | (bit << 15);
        if self.nr43 & 0x08 != 0 {
            self.lfsr = (self.lfsr & !0x80) | (bit << 7);
        }
        self.lfsr >>= 1;
    }

    /// Bit 0 picks between silence and the volume.
    fn digital(&self) -> u8 {
        if self.enabled && self.lfsr & 1 != 0 {
            self.env.volume
        } else {
            0
        }
    }

    fn trigger(&mut self) {
        self.enabled = self.dac;
        self.timer = self.period_cycles();
        self.env.trigger();
        self.lfsr = 0;
    }

    fn set_envelope(&mut self, val: u8) {
        self.env.nrx2 = val;
        self.dac = self.env.dac_on();
        self.enabled &= self.dac;
    }
}

/// A DAC: digital 0 is analog +1 and 15 is -1; an off DAC outputs 0.
fn dac(on: bool, digital: u8) -> f32 {
    if on {
        1.0 - f32::from(digital) / 7.5
    } else {
        0.0
    }
}

#[derive(Clone)]
pub struct Apu {
    /// NR52 bit 7. Off clears every register but wave RAM and ignores writes.
    power: bool,
    ch1: Square,
    ch2: Square,
    ch3: Wave,
    ch4: Noise,
    sweep: Sweep,
    /// The frame sequencer's next step, 0-7. It advances on each DIV-APU
    /// event (512 Hz): lengths on even steps (256 Hz), CH1's sweep on steps
    /// 2 and 6 (128 Hz), envelopes on step 7 (64 Hz).
    frame_step: u8,
    /// Raw values written to $FF10-$FF25, for reading back.
    regs: [u8; 0x16],
    wave_ram: [u8; 16],
    sample_rate: u32,
    /// T-cycles x sample rate since the last output sample; a sample is due
    /// each time this passes CPU_HZ.
    sample_clock: u64,
    /// Sum of mixer output x cycles since the last sample, per side, and the
    /// cycles it covers: each sample is the average over its interval.
    acc: [f32; 2],
    acc_cycles: u32,
    /// The high-pass filter's capacitor, per side, and its charge factor
    /// for one sample.
    capacitor: [f32; 2],
    charge_factor: f32,
    /// Interleaved left/right output, waiting for the frontend.
    samples: Vec<f32>,
}

impl Default for Apu {
    fn default() -> Self {
        Self::new()
    }
}

// Save states: the channels' internal counters, but not the output stage
// (sample rate, filter, buffered samples), which belongs to the host.

impl Length {
    fn save(&self, w: &mut StateWriter) {
        w.u16(self.counter);
        w.bool(self.enabled);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.counter = r.u16()?;
        self.enabled = r.bool()?;
        Ok(())
    }
}

impl Envelope {
    fn save(&self, w: &mut StateWriter) {
        w.bytes(&[self.nrx2, self.volume, self.pace, self.timer]);
        w.bool(self.up);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let mut b = [0; 4];
        r.bytes(&mut b)?;
        [self.nrx2, self.volume, self.pace, self.timer] = b;
        self.up = r.bool()?;
        if self.volume > 15 {
            return Err(StateError::Corrupt("envelope volume"));
        }
        Ok(())
    }
}

impl Square {
    fn save(&self, w: &mut StateWriter) {
        w.bool(self.enabled);
        w.bool(self.dac);
        w.bytes(&[self.duty, self.step]);
        w.u16(self.period);
        w.u32(self.timer);
        self.env.save(w);
        self.length.save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enabled = r.bool()?;
        self.dac = r.bool()?;
        self.duty = r.u8()?;
        self.step = r.u8()?;
        if self.duty > 3 || self.step > 7 {
            return Err(StateError::Corrupt("square duty step"));
        }
        self.period = r.u16()?;
        self.timer = r.u32()?;
        self.env.load(r)?;
        self.length.load(r)
    }
}

impl Wave {
    fn save(&self, w: &mut StateWriter) {
        w.bool(self.enabled);
        w.bool(self.dac);
        w.bytes(&[self.level, self.position, self.sample]);
        w.u16(self.period);
        w.u32(self.timer);
        self.length.save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enabled = r.bool()?;
        self.dac = r.bool()?;
        let mut b = [0; 3];
        r.bytes(&mut b)?;
        [self.level, self.position, self.sample] = b;
        if self.level > 3 || self.position > 31 {
            return Err(StateError::Corrupt("wave position"));
        }
        self.period = r.u16()?;
        self.timer = r.u32()?;
        self.length.load(r)
    }
}

impl Noise {
    fn save(&self, w: &mut StateWriter) {
        w.bool(self.enabled);
        w.bool(self.dac);
        w.u8(self.nr43);
        w.u16(self.lfsr);
        w.u32(self.timer);
        self.env.save(w);
        self.length.save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enabled = r.bool()?;
        self.dac = r.bool()?;
        self.nr43 = r.u8()?;
        self.lfsr = r.u16()?;
        self.timer = r.u32()?;
        self.env.load(r)?;
        self.length.load(r)
    }
}

impl Sweep {
    fn save(&self, w: &mut StateWriter) {
        w.bool(self.enabled);
        w.u16(self.shadow);
        w.u8(self.timer);
        w.bool(self.subtracted);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enabled = r.bool()?;
        self.shadow = r.u16()?;
        self.timer = r.u8()?;
        self.subtracted = r.bool()?;
        Ok(())
    }
}

impl Apu {
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.tag(b"APU ");
        w.bool(self.power);
        self.ch1.save(w);
        self.ch2.save(w);
        self.ch3.save(w);
        self.ch4.save(w);
        self.sweep.save(w);
        w.u8(self.frame_step);
        w.bytes(&self.regs);
        w.bytes(&self.wave_ram);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"APU ")?;
        self.power = r.bool()?;
        self.ch1.load(r)?;
        self.ch2.load(r)?;
        self.ch3.load(r)?;
        self.ch4.load(r)?;
        self.sweep.load(r)?;
        self.frame_step = r.u8()?;
        if self.frame_step > 7 {
            return Err(StateError::Corrupt("frame sequencer step"));
        }
        r.bytes(&mut self.regs)?;
        r.bytes(&mut self.wave_ram)
    }
}

impl Apu {
    /// The state the boot ROM leaves: powered, full volume, and CH1 done
    /// playing the boot chime. https://gbdev.io/pandocs/Power_Up_Sequence.html
    pub fn new() -> Self {
        let mut apu = Self {
            power: false,
            ch1: Square::default(),
            ch2: Square::default(),
            ch3: Wave::default(),
            ch4: Noise::default(),
            sweep: Sweep::default(),
            frame_step: 0,
            regs: [0; 0x16],
            wave_ram: [0; 16],
            sample_rate: 48_000,
            sample_clock: 0,
            acc: [0.0; 2],
            acc_cycles: 0,
            capacitor: [0.0; 2],
            charge_factor: 0.0,
            samples: Vec::new(),
        };
        apu.set_sample_rate(48_000);
        apu.write(0xFF26, 0x80);
        for (addr, val) in [
            (0xFF10, 0x80),
            (0xFF11, 0xBF),
            (0xFF12, 0xF3),
            (0xFF14, 0x3F),
        ] {
            apu.write(addr, val);
        }
        apu.write(0xFF24, 0x77);
        apu.write(0xFF25, 0xF3);
        // The chime's envelope has faded to 0, but CH1 is still on.
        apu.ch1.enabled = true;
        apu.ch1.env.volume = 0;
        // Start with the capacitor charged to the current level, as if the
        // console had been on a while, so the first samples don't jump.
        apu.capacitor = apu.mix();
        apu
    }

    /// Output sample rate (per channel), e.g. the browser's AudioContext rate.
    pub fn set_sample_rate(&mut self, hz: u32) {
        self.sample_rate = hz.max(1);
        // Pan Docs: charge factor 0.999958 per T-cycle on DMG.
        self.charge_factor =
            0.999958f64.powf(f64::from(CPU_HZ) / f64::from(self.sample_rate)) as f32;
    }

    /// Takes the samples made so far: interleaved left, right, as f32 in
    /// roughly -1..1.
    pub fn take_samples(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.samples)
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF26 => {
                let on = [
                    self.ch1.enabled,
                    self.ch2.enabled,
                    self.ch3.enabled,
                    self.ch4.enabled,
                ];
                let status = on
                    .iter()
                    .enumerate()
                    .fold(0, |s, (i, &e)| s | u8::from(e) << i);
                u8::from(self.power) << 7 | 0x70 | status
            }
            0xFF10..=0xFF25 => {
                let i = usize::from(addr - 0xFF10);
                self.regs[i] | READ_MASK[i]
            }
            0xFF30..=0xFF3F => self.wave_ram[usize::from(addr - 0xFF30)],
            _ => 0xFF, // $FF27-$FF2F
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        match addr {
            // Wave RAM is always writable.
            // TODO(accuracy): while CH3 plays, the CPU really reaches the
            // byte CH3 is reading instead.
            0xFF30..=0xFF3F => self.wave_ram[usize::from(addr - 0xFF30)] = val,
            0xFF26 => self.set_power(val & 0x80 != 0),
            0xFF10..=0xFF25 if self.power => {
                self.regs[usize::from(addr - 0xFF10)] = val;
                self.write_register(addr, val);
            }
            // While off, the DMG still takes length writes (only the length
            // part of NRx1); blargg's "len ctr during power" checks this.
            0xFF11 | 0xFF16 | 0xFF1B | 0xFF20 => self.write_length(addr, val),
            _ => {}
        }
    }

    // TODO(accuracy): on the Game Boy Color, powering off also clears the
    // length counters, and length writes while off are ignored (blargg
    // cgb_sound 08 and 11 check this); this does the DMG thing on both.
    // https://gbdev.io/pandocs/Audio_details.html#power-control
    fn set_power(&mut self, on: bool) {
        if self.power && !on {
            // Off: every register clears (wave RAM stays). On the DMG the
            // length timers' counts survive, though length stays disabled
            // until NRx4 enables it again.
            let lengths = [
                self.ch1.length.counter,
                self.ch2.length.counter,
                self.ch3.length.counter,
                self.ch4.length.counter,
            ];
            self.regs = [0; 0x16];
            self.ch1 = Square::default();
            self.ch2 = Square::default();
            self.ch3 = Wave::default();
            self.ch4 = Noise::default();
            self.sweep = Sweep::default();
            [
                self.ch1.length.counter,
                self.ch2.length.counter,
                self.ch3.length.counter,
                self.ch4.length.counter,
            ] = lengths;
        }
        if !self.power && on {
            // Turned on: the frame sequencer starts again from step 0.
            self.frame_step = 0;
        }
        self.power = on;
    }

    /// The length part of NRx1: 64 - n (256 - n on CH3) until the channel stops.
    fn write_length(&mut self, addr: u16, val: u8) {
        match addr {
            0xFF11 => self.ch1.length.counter = 64 - u16::from(val & 0x3F),
            0xFF16 => self.ch2.length.counter = 64 - u16::from(val & 0x3F),
            0xFF1B => self.ch3.length.counter = 256 - u16::from(val),
            _ => self.ch4.length.counter = 64 - u16::from(val & 0x3F),
        }
    }

    fn write_register(&mut self, addr: u16, val: u8) {
        let set_low = |period: &mut u16| *period = (*period & 0x700) | u16::from(val);
        let set_high = |period: &mut u16| *period = (*period & 0xFF) | (u16::from(val & 7) << 8);
        let trigger = val & 0x80 != 0;
        let step = self.frame_step;
        match addr {
            0xFF10 => {
                // Leaving subtract mode after a subtraction stops CH1.
                if self.sweep.subtracted && val & 0x08 == 0 {
                    self.ch1.enabled = false;
                }
            }
            0xFF11 => {
                self.ch1.duty = val >> 6;
                self.write_length(addr, val);
            }
            0xFF12 => self.ch1.set_envelope(val),
            0xFF13 => set_low(&mut self.ch1.period),
            0xFF14 => {
                let keep = self.ch1.length.write_nrx4(64, val, step);
                set_high(&mut self.ch1.period);
                if trigger {
                    self.ch1.trigger();
                    self.sweep_trigger();
                }
                self.ch1.enabled &= keep;
            }
            0xFF16 => {
                self.ch2.duty = val >> 6;
                self.write_length(addr, val);
            }
            0xFF17 => self.ch2.set_envelope(val),
            0xFF18 => set_low(&mut self.ch2.period),
            0xFF19 => {
                let keep = self.ch2.length.write_nrx4(64, val, step);
                set_high(&mut self.ch2.period);
                if trigger {
                    self.ch2.trigger();
                }
                self.ch2.enabled &= keep;
            }
            0xFF1A => {
                self.ch3.dac = val & 0x80 != 0;
                self.ch3.enabled &= self.ch3.dac;
            }
            0xFF1B => self.write_length(addr, val),
            0xFF1C => self.ch3.level = (val >> 5) & 3,
            0xFF1D => set_low(&mut self.ch3.period),
            0xFF1E => {
                let keep = self.ch3.length.write_nrx4(256, val, step);
                set_high(&mut self.ch3.period);
                if trigger {
                    self.ch3.trigger();
                }
                self.ch3.enabled &= keep;
            }
            0xFF20 => self.write_length(addr, val),
            0xFF21 => self.ch4.set_envelope(val),
            0xFF22 => self.ch4.nr43 = val,
            0xFF23 => {
                let keep = self.ch4.length.write_nrx4(64, val, step);
                if trigger {
                    self.ch4.trigger();
                }
                self.ch4.enabled &= keep;
            }
            _ => {} // NR50/NR51 are read from `regs` when mixing
        }
    }

    /// One DIV-APU event (512 Hz, from DIV bit 4 falling): runs the frame
    /// sequencer's next step. https://gbdev.io/pandocs/Audio_details.html
    pub fn frame_sequencer_tick(&mut self) {
        if !self.power {
            return;
        }
        let step = self.frame_step;
        self.frame_step = (step + 1) & 7;
        if step.is_multiple_of(2) {
            // Lengths count down whether or not their channel is playing.
            self.ch1.enabled &= !self.ch1.length.clock();
            self.ch2.enabled &= !self.ch2.length.clock();
            self.ch3.enabled &= !self.ch3.length.clock();
            self.ch4.enabled &= !self.ch4.length.clock();
        }
        if step == 2 || step == 6 {
            self.sweep_clock();
        }
        if step == 7 {
            self.ch1.env.clock();
            self.ch2.env.clock();
            self.ch4.env.clock();
        }
    }

    /// NR10 as (pace, subtract, step).
    fn nr10(&self) -> (u8, bool, u8) {
        let nr10 = self.regs[0];
        ((nr10 >> 4) & 7, nr10 & 0x08 != 0, nr10 & 7)
    }

    /// The next period: shadow +/- shadow >> step. Past 2047 turns CH1 off.
    fn sweep_next(&mut self) -> u16 {
        let (_, subtract, step) = self.nr10();
        let delta = self.sweep.shadow >> step;
        let next = if subtract {
            self.sweep.subtracted = true;
            self.sweep.shadow - delta
        } else {
            self.sweep.shadow + delta
        };
        if next > 2047 {
            self.ch1.enabled = false;
        }
        next
    }

    /// On trigger: copy the period, restart the sweep timer (pace 0 counts
    /// as 8 here), and with a nonzero step check right away for overflow.
    fn sweep_trigger(&mut self) {
        let (pace, _, step) = self.nr10();
        self.sweep.shadow = self.ch1.period;
        self.sweep.timer = if pace == 0 { 8 } else { pace };
        self.sweep.enabled = pace != 0 || step != 0;
        self.sweep.subtracted = false;
        if step != 0 {
            self.sweep_next();
        }
    }

    /// One 128 Hz tick: every `pace` of them, move the period and check that
    /// the move after that won't overflow either.
    fn sweep_clock(&mut self) {
        self.sweep.timer = self.sweep.timer.saturating_sub(1);
        if self.sweep.timer > 0 {
            return;
        }
        let (pace, _, step) = self.nr10();
        self.sweep.timer = if pace == 0 { 8 } else { pace };
        if self.sweep.enabled && pace != 0 {
            let next = self.sweep_next();
            if next <= 2047 && step != 0 {
                self.sweep.shadow = next;
                self.ch1.period = next;
                self.sweep_next();
            }
        }
    }

    /// Advances the channels by `cycles` T-cycles, adding output samples as
    /// they come due.
    pub fn tick(&mut self, mut cycles: u32) {
        let rate = u64::from(self.sample_rate);
        while cycles > 0 {
            // Run up to the next output sample, or to the end of `cycles`.
            let until_sample = (u64::from(CPU_HZ) - self.sample_clock).div_ceil(rate);
            let step = cycles.min(until_sample as u32).max(1);
            self.ch1.tick(step);
            self.ch2.tick(step);
            self.ch3.tick(step, &self.wave_ram);
            self.ch4.tick(step);
            let [left, right] = self.mix();
            self.acc[0] += left * step as f32;
            self.acc[1] += right * step as f32;
            self.acc_cycles += step;
            self.sample_clock += u64::from(step) * rate;
            if self.sample_clock >= u64::from(CPU_HZ) {
                self.sample_clock -= u64::from(CPU_HZ);
                self.emit_sample();
            }
            cycles -= step;
        }
    }

    /// The mixer: each channel's DAC output, added per side as NR51 routes
    /// it (bits 4-7 left, 0-3 right, CH1 lowest), times NR50's per-side
    /// volume + 1. Scaled from the -32..32 that can reach down to -1..1.
    fn mix(&self) -> [f32; 2] {
        let outs = [
            dac(self.ch1.dac, self.ch1.digital()),
            dac(self.ch2.dac, self.ch2.digital()),
            dac(self.ch3.dac, self.ch3.digital()),
            dac(self.ch4.dac, self.ch4.digital()),
        ];
        let nr50 = self.regs[0x14];
        let nr51 = self.regs[0x15];
        let side = |shift: u8| -> f32 {
            (0..4)
                .filter(|&ch| nr51 >> (ch + shift) & 1 != 0)
                .map(|ch| outs[usize::from(ch)])
                .sum()
        };
        let left = side(4) * f32::from((nr50 >> 4 & 7) + 1);
        let right = side(0) * f32::from((nr50 & 7) + 1);
        [left / 32.0, right / 32.0]
    }

    /// Averages the mixer output since the last sample, then runs it through
    /// the high-pass filter: the capacitor pulls the signal towards 0, which
    /// removes the DC offset of quiet-but-enabled DACs.
    fn emit_sample(&mut self) {
        let cycles = self.acc_cycles.max(1) as f32;
        for side in 0..2 {
            let input = self.acc[side] / cycles;
            let out = input - self.capacitor[side];
            self.capacitor[side] = input - out * self.charge_factor;
            if self.samples.len() < MAX_BUFFERED {
                self.samples.push(out);
            }
        }
        self.acc = [0.0; 2];
        self.acc_cycles = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boot state with any samples so far thrown away.
    fn apu() -> Apu {
        let mut a = Apu::new();
        a.take_samples();
        a
    }

    fn write_all(a: &mut Apu, writes: &[(u16, u8)]) {
        for &(addr, val) in writes {
            a.write(addr, val);
        }
    }

    /// Runs `seconds` and returns (left, right) sample streams.
    fn run(a: &mut Apu, seconds: f64) -> (Vec<f32>, Vec<f32>) {
        a.take_samples();
        a.tick((f64::from(CPU_HZ) * seconds) as u32);
        let s = a.take_samples();
        let left = s.iter().step_by(2).copied().collect();
        let right = s.iter().skip(1).step_by(2).copied().collect();
        (left, right)
    }

    /// How many times the signal goes from below 0 to 0 or above: its
    /// frequency, over one second.
    fn rising_crossings(s: &[f32]) -> usize {
        s.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count()
    }

    fn peak_to_peak(s: &[f32]) -> f32 {
        let lo = s.iter().copied().fold(f32::MAX, f32::min);
        let hi = s.iter().copied().fold(f32::MIN, f32::max);
        hi - lo
    }

    /// CH1 at volume 15 on both sides, with `duty` and 11-bit `period`, triggered.
    fn square(a: &mut Apu, duty: u8, period: u16) {
        write_all(
            a,
            &[
                (0xFF25, 0x11),
                (0xFF11, duty << 6),
                (0xFF12, 0xF0),
                (0xFF13, period as u8),
                (0xFF14, 0x80 | (period >> 8) as u8),
            ],
        );
    }

    #[test]
    fn square_wave_plays_the_right_pitch() {
        // 131072 / (2048 - 1750) = 439.8 Hz, about concert A.
        let mut a = apu();
        square(&mut a, 2, 1750);
        let (left, right) = run(&mut a, 1.0);
        assert_eq!(left.len(), 48_000, "one second at 48 kHz");
        let hz = rising_crossings(&left);
        assert!((438..=441).contains(&hz), "{hz} Hz");
        assert_eq!(rising_crossings(&right), hz, "routed to both sides");
    }

    #[test]
    fn duty_cycle_sets_the_share_of_time_high() {
        for (duty, share) in [(0, 0.125), (1, 0.25), (2, 0.5), (3, 0.75)] {
            let mut a = apu();
            square(&mut a, duty, 1750);
            run(&mut a, 0.5); // let the high-pass filter settle
            let (left, _) = run(&mut a, 1.0);
            // The DAC inverts: a high step is a negative output.
            let low = left.iter().filter(|&&x| x < 0.0).count() as f64 / left.len() as f64;
            assert!(
                (low - share).abs() < 0.03,
                "duty {duty}: {low:.3} vs {share}"
            );
        }
    }

    #[test]
    fn wave_channel_plays_wave_ram_at_its_rate_and_level() {
        let mut a = apu();
        // A triangle: 0..15 then 15..0, two samples per byte, high nibble first.
        for i in 0..16u8 {
            let (hi, lo) = if i < 8 {
                (2 * i, 2 * i + 1)
            } else {
                (31 - 2 * i, 30 - 2 * i)
            };
            a.write(0xFF30 + u16::from(i), (hi << 4) | lo);
        }
        // 65536 / (2048 - 1900) = 442.8 Hz
        let period = 1900u16;
        write_all(
            &mut a,
            &[
                (0xFF25, 0x44),
                (0xFF1A, 0x80),
                (0xFF1C, 0x20),
                (0xFF1D, period as u8),
                (0xFF1E, 0x80 | (period >> 8) as u8),
            ],
        );
        // Rerouting changed the DC level; let the high-pass filter settle.
        run(&mut a, 0.5);
        let (full, _) = run(&mut a, 1.0);
        let hz = rising_crossings(&full);
        assert!((441..=444).contains(&hz), "{hz} Hz");

        a.write(0xFF1C, 0x40); // 50%: samples shifted right once
        run(&mut a, 0.2);
        let (half, _) = run(&mut a, 0.5);
        let ratio = peak_to_peak(&full) / peak_to_peak(&half);
        assert!((1.8..2.3).contains(&ratio), "full/half amplitude {ratio}");

        a.write(0xFF1C, 0x00); // mute
        run(&mut a, 0.2);
        let (muted, _) = run(&mut a, 0.2);
        assert!(peak_to_peak(&muted) < 0.001);
    }

    /// Steps until the LFSR's low bits return to where they started.
    fn lfsr_period(seven_bit: bool) -> usize {
        let mut n = Noise {
            nr43: if seven_bit { 0x08 } else { 0 },
            dac: true,
            ..Default::default()
        };
        n.trigger();
        let mask = if seven_bit { 0x7F } else { 0x7FFF };
        let start = n.lfsr & mask;
        (1..=40_000)
            .find(|_| {
                n.step_lfsr();
                n.lfsr & mask == start
            })
            .unwrap_or(0)
    }

    #[test]
    fn noise_lfsr_repeats_every_32767_or_127_steps() {
        assert_eq!(lfsr_period(false), 32767);
        assert_eq!(lfsr_period(true), 127);
    }

    #[test]
    fn noise_clock_shift_14_and_15_stop_the_lfsr() {
        let mut n = Noise {
            nr43: 0xE0,
            dac: true,
            ..Default::default()
        };
        n.trigger();
        n.tick(1_000_000);
        assert_eq!(n.lfsr, 0);
        // Back to a normal shift. The timer still holds the long period it
        // was loaded with (writing NR43 doesn't reload it), so give it time.
        n.nr43 = 0x00;
        n.tick(200_000);
        assert_ne!(n.lfsr, 0, "runs again at a normal shift");
    }

    #[test]
    fn noise_sounds_like_noise() {
        let mut a = apu();
        write_all(
            &mut a,
            &[
                (0xFF25, 0x88),
                (0xFF21, 0xF0),
                (0xFF22, 0x21),
                (0xFF23, 0x80),
            ],
        );
        let (left, _) = run(&mut a, 1.0);
        assert!(peak_to_peak(&left) > 0.3, "audible");
        // Not a single tone: crossings are many and irregular.
        assert!(rising_crossings(&left) > 1000);
    }

    #[test]
    fn dac_off_turns_the_channel_off() {
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF19, 0x80)]);
        assert_eq!(a.read(0xFF26) & 0x02, 0x02, "CH2 on");
        a.write(0xFF17, 0x00); // volume 0, decreasing: DAC off
        assert_eq!(a.read(0xFF26) & 0x02, 0, "CH2 off");
        a.write(0xFF19, 0x80);
        assert_eq!(a.read(0xFF26) & 0x02, 0, "no trigger with the DAC off");
        a.write(0xFF17, 0x08); // volume 0 but increasing: DAC on
        a.write(0xFF19, 0x80);
        assert_eq!(a.read(0xFF26) & 0x02, 0x02);
    }

    #[test]
    fn registers_read_back_with_unused_bits_set() {
        let mut a = apu();
        for addr in 0xFF10..=0xFF25 {
            a.write(addr, 0x00);
        }
        let read: Vec<u8> = (0xFF10..=0xFF25).map(|addr| a.read(addr)).collect();
        assert_eq!(read, READ_MASK);
        for addr in 0xFF27..=0xFF2F {
            assert_eq!(a.read(addr), 0xFF, "{addr:04X} unused");
        }
        a.write(0xFF26, 0x00);
        assert_eq!(a.read(0xFF26), 0x70, "off: bits 4-6 read as 1");
    }

    #[test]
    fn power_off_clears_registers_and_ignores_writes_but_keeps_wave_ram() {
        let mut a = apu();
        a.write(0xFF30, 0xAB);
        square(&mut a, 2, 1750);
        a.write(0xFF26, 0x00);
        assert_eq!(a.read(0xFF12), 0x00, "cleared");
        assert_eq!(a.read(0xFF26) & 0x0F, 0, "every channel off");
        a.write(0xFF12, 0xF0);
        assert_eq!(a.read(0xFF12), 0x00, "writes ignored while off");
        assert_eq!(a.read(0xFF30), 0xAB, "wave RAM kept");
        a.write(0xFF26, 0x80);
        a.write(0xFF12, 0xF0);
        assert_eq!(a.read(0xFF12), 0xF0, "writable again once on");
    }

    #[test]
    fn nr51_pans_and_nr50_scales_each_side() {
        let mut a = apu();
        square(&mut a, 2, 1750);
        a.write(0xFF25, 0x10); // CH1 left only
        run(&mut a, 0.5);
        let (left, right) = run(&mut a, 0.5);
        assert!(peak_to_peak(&left) > 0.1);
        assert!(peak_to_peak(&right) < 0.001, "nothing on the right");

        a.write(0xFF24, 0x07); // left volume 0 (x1), right 7 (x8)
        a.write(0xFF25, 0x11);
        run(&mut a, 0.5);
        let (quiet, loud) = run(&mut a, 0.5);
        let ratio = peak_to_peak(&loud) / peak_to_peak(&quiet);
        assert!((7.5..8.5).contains(&ratio), "right/left {ratio}");
    }

    #[test]
    fn sample_rate_is_whatever_the_frontend_asks_for() {
        let mut a = apu();
        assert_eq!(run(&mut a, 1.0).0.len(), 48_000);
        a.set_sample_rate(44_100);
        let n = run(&mut a, 1.0).0.len();
        assert!((44_099..=44_101).contains(&n), "{n}");
    }

    #[test]
    fn silence_after_boot_is_silent() {
        let mut a = apu();
        let (left, right) = run(&mut a, 0.5);
        assert!(peak_to_peak(&left) < 0.001 && peak_to_peak(&right) < 0.001);
    }

    /// Runs `n` frame sequencer steps (DIV-APU events).
    fn steps(a: &mut Apu, n: u32) {
        for _ in 0..n {
            a.frame_sequencer_tick();
        }
    }

    fn ch_on(a: &Apu, ch: u8) -> bool {
        a.read(0xFF26) & (1 << ch) != 0
    }

    #[test]
    fn length_timer_stops_the_note() {
        // CH2, length 10 (64 - 54), length enabled, triggered at step 0.
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF16, 54), (0xFF19, 0xC0)]);
        // Lengths clock on even steps: the 10th clock is the 19th step.
        steps(&mut a, 18);
        assert!(ch_on(&a, 1), "9 clocks in, still playing");
        steps(&mut a, 1);
        assert!(!ch_on(&a, 1), "10th clock stops it");
    }

    #[test]
    fn without_length_enable_the_note_keeps_going() {
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF16, 63), (0xFF19, 0x80)]);
        steps(&mut a, 1000);
        assert!(ch_on(&a, 1));
    }

    #[test]
    fn wave_channel_length_counts_from_256() {
        let mut a = apu();
        write_all(&mut a, &[(0xFF1A, 0x80), (0xFF1B, 0x00), (0xFF1E, 0xC0)]);
        steps(&mut a, 2 * 255);
        assert!(ch_on(&a, 2), "255 clocks in");
        steps(&mut a, 1);
        assert!(!ch_on(&a, 2), "256th clock");
    }

    #[test]
    fn trigger_reloads_an_expired_length() {
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF16, 63), (0xFF19, 0xC0)]);
        steps(&mut a, 2); // 1 clock: expired
        assert!(!ch_on(&a, 1));
        a.write(0xFF19, 0xC0); // retrigger: back to 64
        steps(&mut a, 2 * 63);
        assert!(ch_on(&a, 1), "63 clocks of 64");
    }

    #[test]
    fn enabling_length_before_an_odd_step_clocks_it_once_more() {
        // With the next step odd (no length clock), turning length on clocks
        // it straight away: a length of 1 runs out at once.
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF16, 63), (0xFF19, 0x80)]);
        steps(&mut a, 1); // next step is 1
        assert!(ch_on(&a, 1));
        a.write(0xFF19, 0x40); // length on, no trigger
        assert!(!ch_on(&a, 1), "the extra clock ran it out");

        // Before an even step there's no extra clock.
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF16, 63), (0xFF19, 0x80)]);
        steps(&mut a, 2); // next step is 2
        a.write(0xFF19, 0x40);
        assert!(ch_on(&a, 1));
    }

    #[test]
    fn envelope_fades_out_and_in() {
        // Volume 15, down, pace 1: one step per 64 Hz tick (every 8 steps).
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF1), (0xFF19, 0x80)]);
        assert_eq!(a.ch2.env.volume, 15);
        steps(&mut a, 8);
        assert_eq!(a.ch2.env.volume, 14);
        steps(&mut a, 8 * 20);
        assert_eq!(a.ch2.env.volume, 0, "stops at 0");
        assert!(ch_on(&a, 1), "a faded channel is still on");

        // Volume 0, up, pace 2: one step every 16.
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0x0A), (0xFF19, 0x80)]);
        steps(&mut a, 16 * 3);
        assert_eq!(a.ch2.env.volume, 3);
        steps(&mut a, 16 * 20);
        assert_eq!(a.ch2.env.volume, 15, "stops at 15");

        // Pace 0 holds the volume.
        let mut a = apu();
        write_all(&mut a, &[(0xFF21, 0x90), (0xFF23, 0x80)]);
        steps(&mut a, 800);
        assert_eq!(a.ch4.env.volume, 9);
    }

    /// CH1 at volume 15 with NR10 = `nr10`, period `period`, triggered.
    fn sweeping(nr10: u8, period: u16) -> Apu {
        let mut a = apu();
        write_all(
            &mut a,
            &[
                (0xFF10, nr10),
                (0xFF12, 0xF0),
                (0xFF13, period as u8),
                (0xFF14, 0x80 | (period >> 8) as u8),
            ],
        );
        a
    }

    #[test]
    fn sweep_raises_the_pitch_until_it_overflows() {
        // Pace 1, up, step 1: each sweep tick adds period >> 1.
        let mut a = sweeping(0x11, 0x100);
        // Sweep ticks on steps 2 and 6, so the first comes on the 3rd step.
        steps(&mut a, 3);
        assert_eq!(a.ch1.period, 0x180);
        steps(&mut a, 4);
        assert_eq!(a.ch1.period, 0x240);
        steps(&mut a, 4);
        assert_eq!(a.ch1.period, 0x360);
        steps(&mut a, 4);
        assert_eq!(a.ch1.period, 0x510);
        assert!(ch_on(&a, 0), "the look-ahead, 0x798, still fits");
        steps(&mut a, 4);
        assert_eq!(a.ch1.period, 0x798);
        assert!(!ch_on(&a, 0), "the look-ahead, 0x798 + 0x3CC, passes 2047");
    }

    #[test]
    fn sweep_down_lowers_the_pitch() {
        // Pace 2, subtract, step 2: every other sweep tick, minus period >> 2.
        let mut a = sweeping(0x2A, 0x400);
        steps(&mut a, 7); // sweep ticks at steps 2 and 6
        assert_eq!(a.ch1.period, 0x300);
        assert!(ch_on(&a, 0));
    }

    #[test]
    fn sweep_overflow_is_checked_on_trigger() {
        // Step 1: 0x700 + 0x380 > 2047 straight away.
        let a = sweeping(0x01, 0x700);
        assert!(!ch_on(&a, 0));
        // Step 0 does no calculation on trigger.
        let a = sweeping(0x10, 0x700);
        assert!(ch_on(&a, 0));
    }

    #[test]
    fn leaving_subtract_mode_after_a_subtraction_stops_ch1() {
        let mut a = sweeping(0x19, 0x400); // pace 1, subtract, step 1
        a.write(0xFF10, 0x11); // back to adding
        assert!(!ch_on(&a, 0), "trigger already subtracted once");

        let mut a = sweeping(0x10, 0x400); // adding, step 0
        a.write(0xFF10, 0x18);
        a.write(0xFF10, 0x10);
        assert!(ch_on(&a, 0), "no subtraction yet, so no harm");
    }

    #[test]
    fn power_off_keeps_length_counts_and_power_on_restarts_the_sequencer() {
        let mut a = apu();
        a.write(0xFF16, 54); // CH2 length 10
        steps(&mut a, 3);
        a.write(0xFF26, 0x00);
        a.write(0xFF16, 60); // still writable while off: length 4
        a.write(0xFF26, 0x80);
        assert_eq!(a.frame_step, 0, "sequencer restarts");
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF19, 0xC0)]);
        steps(&mut a, 7);
        assert!(!ch_on(&a, 1), "the length written while off counted");
    }
}
