//! The audio processing unit: four sound channels, mixed to stereo samples.
//!
//! The APU runs on its own 2 MHz clock: a tick every 2 dots, in either CPU
//! speed. Each channel counts its period down in those ticks (the square
//! waves and noise at half the rate) and steps through a waveform: two
//! square waves, a channel that plays 32 4-bit samples from wave RAM, and a
//! noise channel driven by a shift register. Their digital levels (0-15) go
//! through a DAC each, the mixer adds them per side as NR51 routes them, NR50
//! scales each side, and a high-pass filter (a capacitor on real hardware)
//! removes the DC offset. Frontends take the result as interleaved stereo
//! f32 samples at a rate they choose.
//!
//! The frame sequencer, run from DIV bit 4 (bit 5 in double speed), shapes
//! the notes: length timers stop them, envelopes fade them in or out, and
//! CH1's sweep bends its pitch.
//!
//! The channels' timings, to the tick, and their quirks follow SameBoy's APU
//! (Core/apu.c), whose behavior SameSuite's APU tests and blargg's sound
//! tests measure, as it does the original and CPU CGB C; later Colors differ
//! in places.
//!
//! References: https://gbdev.io/pandocs/Audio.html,
//! https://gbdev.io/pandocs/Audio_Registers.html,
//! https://gbdev.io/pandocs/Audio_details.html

use crate::state::{StateError, StateReader, StateWriter};
use crate::CPU_HZ;

/// The four duty cycles' waveforms, by the position the channel has just
/// stepped to (Pan Docs' NR11 table).
const DUTY: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1], // 12.5%
    [1, 0, 0, 0, 0, 0, 0, 1], // 25%
    [1, 0, 0, 0, 0, 1, 1, 1], // 50%
    [0, 1, 1, 1, 1, 1, 1, 0], // 75%
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

/// Where registers live in `Apu::regs` ($FF10 + index).
const NR10: usize = 0x00;
const NR11: usize = 0x01;
const NR12: usize = 0x02;
const NR21: usize = 0x06;
const NR22: usize = 0x07;
const NR42: usize = 0x11;
const NR43: usize = 0x12;
const NR44: usize = 0x13;
const NR50: usize = 0x14;
const NR51: usize = 0x15;

/// Output samples kept if nobody takes them (the CLI without --wav): about
/// two seconds, so memory stays bounded.
const MAX_BUFFERED: usize = 2 * 48_000 * 2;

/// What happens to the first DIV-APU event after the APU is switched on.
const SKIP_INACTIVE: u8 = 0;
const SKIP_SKIPPED: u8 = 1;
const SKIP_NEXT: u8 = 2;

/// An envelope's clock: armed when its countdown runs out, it moves the
/// volume on the next DIV-APU event. Arming it at volume 15 going up, or 0
/// going down, locks the envelope instead: it stops until a trigger.
#[derive(Debug, Clone, Copy, Default)]
struct EnvelopeClock {
    locked: bool,
    clock: bool,
    should_lock: bool,
}

impl EnvelopeClock {
    fn set(&mut self, value: bool, up: bool, volume: u8) {
        if self.clock == value {
            return;
        }
        if value {
            self.clock = true;
            self.should_lock = (volume == 15 && up) || (volume == 0 && !up);
        } else {
            self.clock = false;
            self.locked |= self.should_lock;
        }
    }

    fn save(&self, w: &mut StateWriter) {
        w.bytes(&[
            u8::from(self.locked),
            u8::from(self.clock),
            u8::from(self.should_lock),
        ]);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.locked = r.bool()?;
        self.clock = r.bool()?;
        self.should_lock = r.bool()?;
        Ok(())
    }
}

/// Writing NRx2 while the channel plays ("zombie mode") moves the volume,
/// the way the envelope's counter is wired up: one step of it.
fn nrx2_glitch_step(
    volume: &mut u8,
    value: u8,
    old: u8,
    countdown: &mut u8,
    lock: &mut EnvelopeClock,
) {
    if lock.clock {
        *countdown = value & 7;
    }
    let mut should_tick = value & 7 != 0 && old & 7 == 0 && !lock.locked;
    let should_invert = (value ^ old) & 8 != 0;
    if value & 0xF == 8 && old & 0xF == 8 && !lock.locked {
        should_tick = true;
    }
    if should_invert {
        if value & 8 != 0 {
            if old & 7 == 0 && !lock.locked {
                *volume ^= 0xF;
            } else {
                *volume = 0xE_u8.wrapping_sub(*volume) & 0xF;
            }
            should_tick = false;
        } else {
            *volume = 0x10_u8.wrapping_sub(*volume) & 0xF;
        }
    }
    if should_tick {
        *volume = if value & 8 != 0 {
            volume.wrapping_add(1)
        } else {
            volume.wrapping_sub(1)
        } & 0xF;
    } else if value & 7 == 0 && lock.clock {
        lock.set(false, false, 0);
    }
}

/// On the original and CPU CGB C, every NRx2 write passes through $FF on its
/// way, so the glitch runs twice: old to $FF, then $FF to the new value.
fn nrx2_glitch(volume: &mut u8, value: u8, old: u8, countdown: &mut u8, lock: &mut EnvelopeClock) {
    nrx2_glitch_step(volume, 0xFF, old, countdown, lock);
    nrx2_glitch_step(volume, value, 0xFF, countdown, lock);
}

/// A square wave channel (CH1, CH2).
#[derive(Debug, Clone, Default)]
struct Square {
    /// Length timer: 64 - NRx1's low bits, in 256 Hz clocks.
    length: u16,
    length_enabled: bool,
    volume: u8,
    /// 64 Hz clocks until the envelope's next step.
    volume_countdown: u8,
    envelope: EnvelopeClock,
    /// Position in the 8-step duty waveform.
    index: u8,
    /// Just triggered: the output stays 0 until the first step.
    suppressed: bool,
    /// Ticks until the next step: counts down from (2047 - period) * 2 + 1.
    countdown: u16,
    /// 11-bit period value from NRx3/NRx4.
    period: u16,
    /// Stepped since the last trigger.
    did_tick: bool,
    /// The countdown reloaded in the last tick run: a period write now
    /// reloads it again.
    just_reloaded: bool,
}

/// The wave channel (CH3).
#[derive(Debug, Clone, Default)]
struct Wave {
    /// NR30 bit 7: the DAC.
    enable: bool,
    length: u16,
    length_enabled: bool,
    /// NR32's level as a right shift: 4 (mute), 0, 1 or 2.
    shift: u8,
    period: u16,
    /// Ticks until the next sample: counts down from 2047 - period.
    countdown: u16,
    /// Which of the 32 samples was read last.
    index: u8,
    /// The wave RAM byte read last; what the channel is playing.
    byte: u8,
    /// A sample was read in the last tick run (the original's CPU only
    /// reaches wave RAM then).
    just_read: bool,
    /// Triggered since the DAC came on: the counter runs on, reading wave
    /// RAM, even once the channel has stopped.
    pulsed: bool,
    /// Ticks until a stopped channel's late read of wave RAM.
    bugged_read_countdown: u8,
}

/// The noise channel (CH4).
#[derive(Debug, Clone, Default)]
struct Noise {
    length: u16,
    length_enabled: bool,
    volume: u8,
    volume_countdown: u8,
    envelope: EnvelopeClock,
    lfsr: u16,
    /// NR43 bit 3: a 7-bit LFSR.
    narrow: bool,
    /// Ticks until the counter below counts: NR43's divider (0 counting as
    /// 0.5) times 4.
    counter_countdown: u8,
    /// A 14-bit counter; the LFSR steps when its bit NR43 selects rises.
    counter: u16,
    /// Ticks since power on, for which 4-tick phase things happen in.
    alignment: u8,
    current_lfsr_sample: bool,
    did_step_counter: bool,
    countdown_reloaded: bool,
    /// The original starts the channel 6 ticks late when triggered off its
    /// 4-tick phase.
    dmg_delayed_start: u8,
    counter_active: bool,
    background_counter_active: bool,
    lfsr_stepped_in_narrow: bool,
    lfsr_bit_7_before_step: bool,
    started_with_dac_disabled: bool,
}

/// CH1's frequency sweep, driven by NR10: pace (bits 4-6), subtract (bit 3)
/// and step (bits 0-2). Every `pace` 128 Hz clocks the period moves by
/// period >> step; the next value is worked out a few 1 MHz ticks later,
/// and one past 2047 turns CH1 off. https://gbdev.io/pandocs/Audio_Registers.html
#[derive(Debug, Clone, Default)]
struct Sweep {
    /// 128 Hz clocks; a sweep happens when this reaches 7.
    countdown: u8,
    /// 1 MHz ticks until the calculation is done.
    calculate_countdown: u8,
    /// 1 MHz ticks until that countdown starts.
    reload_timer: u8,
    /// What the calculation adds: period >> step (inverted when subtracting).
    addend: u16,
    /// The period the calculation works from.
    shadow: u16,
    unshifted: bool,
    instant_calculation_done: bool,
    /// Ticks after a trigger in which the calculation keeps the old period.
    restart_hold: u8,
    completed_addend: u16,
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
    /// Color hardware (CPU CGB C) rather than the original.
    cgb: bool,
    pub(crate) double_speed: bool,
    /// The address the CPU last put on the bus: a stopped CH3's late read
    /// takes wave RAM's byte from it.
    pub(crate) address_bus: u16,
    /// DIV's DIV-APU bit as NR52 is written (the bus keeps it up to date).
    pub(crate) div_bit_high: bool,
    /// NR52 bit 7. Off clears every register but wave RAM and ignores writes.
    power: bool,
    /// DIV-APU events arrive an M-cycle late (see [`Apu::speed_switched`]).
    event_delay: bool,
    /// Falling edges held back by that delay, and whether the last came
    /// from a DIV write.
    held_events: u8,
    held_by_write: bool,
    /// Raw values written to $FF10-$FF25, for reading back.
    regs: [u8; 0x16],
    wave_ram: [u8; 16],
    /// CPU T-cycles not yet run as ticks.
    cycle_carry: u8,
    /// DIV-APU events counted: lengths clock when it turns odd, the sweep
    /// every 4th, envelopes every 8th.
    div_divider: u8,
    skip_div_event: u8,
    /// The 1 MHz clock's phase against the 2 MHz ticks.
    lf_div: u8,
    active: [bool; 4],
    /// Each channel's digital level (0-15), as PCM12/PCM34 read them.
    samples: [u8; 4],
    /// CPU CGB C reads PCM12/PCM34 glitched in the M-cycle a channel's
    /// level changes; these mask it.
    pcm_mask: [u8; 2],
    sq: [Square; 2],
    wave: Wave,
    noise: Noise,
    sweep: Sweep,
    sample_rate: u32,
    /// Dots x sample rate since the last output sample; a sample is due
    /// each time this passes CPU_HZ.
    sample_clock: u64,
    /// Sum of mixer output x dots since the last sample, per side, and the
    /// dots it covers: each sample is the average over its interval.
    acc: [f32; 2],
    acc_cycles: u32,
    /// The high-pass filter's capacitor, per side, and its charge factor
    /// for one sample.
    capacitor: [f32; 2],
    charge_factor: f32,
    /// The high-pass filter is on (the host can turn it off for the raw
    /// mixer output).
    high_pass: bool,
    /// Interleaved left/right output, waiting for the frontend.
    samples_out: Vec<f32>,
}

impl Default for Apu {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Square {
    fn save(&self, w: &mut StateWriter) {
        w.u16(self.length);
        w.bool(self.length_enabled);
        w.bytes(&[self.volume, self.volume_countdown, self.index]);
        self.envelope.save(w);
        w.bool(self.suppressed);
        w.u16(self.countdown);
        w.u16(self.period);
        w.bool(self.did_tick);
        w.bool(self.just_reloaded);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.length = r.u16()?.min(64);
        self.length_enabled = r.bool()?;
        self.volume = r.u8()? & 0xF;
        self.volume_countdown = r.u8()? & 7;
        self.index = r.u8()? & 7;
        self.envelope.load(r)?;
        self.suppressed = r.bool()?;
        self.countdown = r.u16()?;
        self.period = r.u16()? & 0x7FF;
        self.did_tick = r.bool()?;
        self.just_reloaded = r.bool()?;
        Ok(())
    }
}

impl Wave {
    fn save(&self, w: &mut StateWriter) {
        w.bool(self.enable);
        w.u16(self.length);
        w.bool(self.length_enabled);
        w.u8(self.shift);
        w.u16(self.period);
        w.u16(self.countdown);
        w.bytes(&[self.index, self.byte, self.bugged_read_countdown]);
        w.bool(self.just_read);
        w.bool(self.pulsed);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enable = r.bool()?;
        self.length = r.u16()?.min(256);
        self.length_enabled = r.bool()?;
        self.shift = r.u8()?.min(4);
        self.period = r.u16()? & 0x7FF;
        self.countdown = r.u16()?;
        self.index = r.u8()? & 31;
        self.byte = r.u8()?;
        self.bugged_read_countdown = r.u8()?;
        self.just_read = r.bool()?;
        self.pulsed = r.bool()?;
        Ok(())
    }
}

impl Noise {
    fn save(&self, w: &mut StateWriter) {
        w.u16(self.length);
        w.bool(self.length_enabled);
        w.bytes(&[self.volume, self.volume_countdown]);
        self.envelope.save(w);
        w.u16(self.lfsr);
        w.u16(self.counter);
        w.bytes(&[
            self.counter_countdown,
            self.alignment,
            self.dmg_delayed_start,
        ]);
        for flag in [
            self.narrow,
            self.current_lfsr_sample,
            self.did_step_counter,
            self.countdown_reloaded,
            self.counter_active,
            self.background_counter_active,
            self.lfsr_stepped_in_narrow,
            self.lfsr_bit_7_before_step,
            self.started_with_dac_disabled,
        ] {
            w.bool(flag);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.length = r.u16()?.min(64);
        self.length_enabled = r.bool()?;
        self.volume = r.u8()? & 0xF;
        self.volume_countdown = r.u8()? & 7;
        self.envelope.load(r)?;
        self.lfsr = r.u16()? & 0x7FFF;
        self.counter = r.u16()? & 0x3FFF;
        self.counter_countdown = r.u8()?;
        self.alignment = r.u8()?;
        self.dmg_delayed_start = r.u8()?;
        for flag in [
            &mut self.narrow,
            &mut self.current_lfsr_sample,
            &mut self.did_step_counter,
            &mut self.countdown_reloaded,
            &mut self.counter_active,
            &mut self.background_counter_active,
            &mut self.lfsr_stepped_in_narrow,
            &mut self.lfsr_bit_7_before_step,
            &mut self.started_with_dac_disabled,
        ] {
            *flag = r.bool()?;
        }
        Ok(())
    }
}

impl Sweep {
    fn save(&self, w: &mut StateWriter) {
        w.bytes(&[
            self.countdown,
            self.calculate_countdown,
            self.reload_timer,
            self.restart_hold,
        ]);
        w.u16(self.addend);
        w.u16(self.shadow);
        w.u16(self.completed_addend);
        w.bool(self.unshifted);
        w.bool(self.instant_calculation_done);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.countdown = r.u8()? & 7;
        self.calculate_countdown = r.u8()?;
        self.reload_timer = r.u8()?;
        self.restart_hold = r.u8()?;
        self.addend = r.u16()?;
        self.shadow = r.u16()?;
        self.completed_addend = r.u16()?;
        self.unshifted = r.bool()?;
        self.instant_calculation_done = r.bool()?;
        Ok(())
    }
}

impl Apu {
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.tag(b"APU ");
        w.bool(self.power);
        w.bytes(&self.regs);
        w.bytes(&self.wave_ram);
        w.bytes(&[
            self.cycle_carry,
            self.div_divider,
            self.skip_div_event,
            self.lf_div,
        ]);
        for on in self.active {
            w.bool(on);
        }
        w.bytes(&self.samples);
        w.bytes(&self.pcm_mask);
        self.sq[0].save(w);
        self.sq[1].save(w);
        self.wave.save(w);
        self.noise.save(w);
        self.sweep.save(w);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(b"APU ")?;
        self.power = r.bool()?;
        r.bytes(&mut self.regs)?;
        r.bytes(&mut self.wave_ram)?;
        self.cycle_carry = r.u8()? & 3;
        self.div_divider = r.u8()?;
        self.skip_div_event = r.u8()?;
        if self.skip_div_event > SKIP_NEXT {
            return Err(StateError::Corrupt("APU DIV event skip"));
        }
        self.lf_div = r.u8()? & 1;
        for on in &mut self.active {
            *on = r.bool()?;
        }
        r.bytes(&mut self.samples)?;
        for s in &mut self.samples {
            *s &= 0xF;
        }
        r.bytes(&mut self.pcm_mask)?;
        self.sq[0].load(r)?;
        self.sq[1].load(r)?;
        self.wave.load(r)?;
        self.noise.load(r)?;
        self.sweep.load(r)
    }
}

impl Apu {
    /// The state the boot ROM leaves: powered, full volume, and CH1 done
    /// playing the boot chime. `cgb` is Color hardware (CPU CGB C).
    /// https://gbdev.io/pandocs/Power_Up_Sequence.html
    pub fn new(cgb: bool) -> Self {
        let mut apu = Self {
            cgb,
            double_speed: false,
            address_bus: 0,
            div_bit_high: false,
            power: false,
            event_delay: false,
            held_events: 0,
            held_by_write: false,
            regs: [0; 0x16],
            wave_ram: [0; 16],
            cycle_carry: 0,
            div_divider: 0,
            skip_div_event: SKIP_INACTIVE,
            lf_div: 0,
            active: [false; 4],
            samples: [0; 4],
            pcm_mask: [0xFF; 2],
            sq: Default::default(),
            wave: Wave::default(),
            noise: Noise::default(),
            sweep: Sweep::default(),
            sample_rate: 48_000,
            sample_clock: 0,
            acc: [0.0; 2],
            acc_cycles: 0,
            capacitor: [0.0; 2],
            charge_factor: 0.0,
            high_pass: true,
            samples_out: Vec::new(),
        };
        apu.set_sample_rate(48_000);
        apu.write(0xFF26, 0x80);
        for (addr, val) in [
            (0xFF10, 0x80),
            (0xFF11, 0xBF),
            (0xFF12, 0xF3),
            (0xFF14, 0x3F),
            (0xFF24, 0x77),
            (0xFF25, 0xF3),
        ] {
            apu.write(addr, val);
        }
        // The chime's envelope has faded to 0, but CH1 is still on.
        apu.active[0] = true;
        apu.sq[0].volume = 0;
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

    /// Turns the high-pass filter off (or on again): samples are then the
    /// mixer's output as it is, DC offset and all.
    pub fn set_high_pass_filter(&mut self, on: bool) {
        self.high_pass = on;
    }

    /// Takes the samples made so far: interleaved left, right, as f32 in
    /// roughly -1..1.
    pub fn take_samples(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.samples_out)
    }

    fn nrx1(index: usize) -> usize {
        if index == 0 {
            NR11
        } else {
            NR21
        }
    }

    fn nrx2(index: usize) -> usize {
        if index == 0 {
            NR12
        } else {
            NR22
        }
    }

    /// A channel's DAC: NRx2 bits 3-7 (NR30 bit 7 for CH3), not all clear.
    fn dac_on(&self, ch: usize) -> bool {
        match ch {
            0 => self.regs[NR12] & 0xF8 != 0,
            1 => self.regs[NR22] & 0xF8 != 0,
            2 => self.wave.enable,
            _ => self.regs[NR42] & 0xF8 != 0,
        }
    }

    /// A new digital level for a channel. With its DAC off the level holds.
    fn update_sample(&mut self, ch: usize, value: u8) {
        if self.dac_on(ch) {
            self.samples[ch] = value;
        }
    }

    fn update_square_sample(&mut self, i: usize) {
        let sq = &self.sq[i];
        if sq.suppressed {
            return;
        }
        let duty = usize::from(self.regs[Self::nrx1(i)] >> 6);
        let value = DUTY[duty][usize::from(sq.index)] * sq.volume;
        self.update_sample(i, value);
    }

    /// Upper nibble first; shifted right by the level.
    fn update_wave_sample(&mut self) {
        let nibble = if self.wave.index & 1 == 1 {
            self.wave.byte & 0x0F
        } else {
            self.wave.byte >> 4
        };
        self.update_sample(2, nibble >> self.wave.shift);
    }

    fn update_noise_sample(&mut self) {
        let value = if self.noise.current_lfsr_sample {
            self.noise.volume
        } else {
            0
        };
        self.update_sample(3, value);
    }

    /// Clears the per-M-cycle PCM glitch mask (SameBoy resets it as each
    /// stretch of cycles begins).
    pub(crate) fn begin_cycles(&mut self) {
        self.pcm_mask = [0xFF; 2];
        for _ in 0..std::mem::take(&mut self.held_events) {
            self.frame_sequencer_event(self.held_by_write);
        }
    }

    /// The CPU switched speed. Each switch into double speed while the APU
    /// is on moves its DIV-APU events an M-cycle later, or back (AGE's
    /// spsw-ch2-lc-delay); switching the APU on sets them right.
    pub(crate) fn speed_switched(&mut self, double: bool) {
        self.double_speed = double;
        if double && self.power {
            self.event_delay = !self.event_delay;
        }
    }

    /// PCM12 ($FF76) and PCM34 ($FF77): the playing channels' digital
    /// levels, CH1/CH3 in the low nibble (Color only).
    /// https://gbdev.io/pandocs/Audio_details.html
    pub fn read_pcm(&self, addr: u16) -> u8 {
        let level = |ch: usize| {
            if self.active[ch] {
                self.samples[ch]
            } else {
                0
            }
        };
        if addr == 0xFF76 {
            (level(1) << 4 | level(0)) & self.pcm_mask[0]
        } else {
            (level(3) << 4 | level(2)) & self.pcm_mask[1]
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF26 => {
                let status = (0..4).fold(0, |s, ch| s | u8::from(self.active[ch]) << ch);
                u8::from(self.power) << 7 | 0x70 | status
            }
            0xFF10..=0xFF25 => {
                let i = usize::from(addr - 0xFF10);
                self.regs[i] | READ_MASK[i]
            }
            0xFF30..=0xFF3F => match self.wave_ram_index(addr) {
                Some(i) => self.wave_ram[i],
                None => 0xFF,
            },
            _ => 0xFF, // $FF27-$FF2F
        }
    }

    /// Which wave RAM byte the CPU reaches at `addr`. While CH3 plays it's
    /// the byte CH3 is reading, and on the original only in the tick CH3
    /// reads it (blargg's "wave read while on").
    fn wave_ram_index(&self, addr: u16) -> Option<usize> {
        if !self.active[2] {
            Some(usize::from(addr - 0xFF30))
        } else if !self.cgb && !self.wave.just_read {
            None
        } else {
            Some(usize::from(self.wave.index / 2))
        }
    }

    pub fn write(&mut self, addr: u16, val: u8) {
        // While off only NR52 and wave RAM take writes, and on the original
        // the length part of NRx1 too (blargg's "len ctr during power").
        let length_reg = matches!(addr, 0xFF11 | 0xFF16 | 0xFF1B | 0xFF20);
        if !self.power && addr != 0xFF26 && addr < 0xFF30 && (self.cgb || !length_reg) {
            return;
        }
        match addr {
            0xFF30..=0xFF3F => {
                if let Some(i) = self.wave_ram_index(addr) {
                    self.wave_ram[i] = val;
                }
            }
            0xFF26 => self.write_nr52(val),
            0xFF10..=0xFF25 => self.write_register(addr, val),
            _ => {}
        }
    }

    /// NR52: switching the APU off clears every register but wave RAM (and
    /// on the original the length timers survive into the next power on);
    /// switching it on restarts the frame sequencer, skipping its first
    /// event if DIV's bit is set right now.
    /// https://gbdev.io/pandocs/Audio_details.html#power-control
    fn write_nr52(&mut self, val: u8) {
        let lengths = [
            self.sq[0].length,
            self.sq[1].length,
            self.wave.length,
            self.noise.length,
        ];
        let on = val & 0x80 != 0;
        if on && !self.power {
            self.reset_state();
            self.lf_div = 1;
            self.wave.shift = 4;
            if self.div_bit_high {
                self.skip_div_event = SKIP_NEXT;
                self.div_divider = 1;
            }
            self.sq[0].countdown = 0xFFFF;
            self.sq[1].countdown = 0xFFFF;
            self.event_delay = false;
            self.power = true;
        } else if !on && self.power {
            for ch in 0..4 {
                self.update_sample(ch, 0);
            }
            self.reset_state();
            self.regs = [0; 0x16];
            self.power = false;
        }
        if !self.cgb && on {
            [
                self.sq[0].length,
                self.sq[1].length,
                self.wave.length,
                self.noise.length,
            ] = lengths;
        }
    }

    /// Everything SameBoy's `memset` of the APU clears.
    fn reset_state(&mut self) {
        self.cycle_carry = 0;
        self.div_divider = 0;
        self.skip_div_event = SKIP_INACTIVE;
        self.lf_div = 0;
        self.active = [false; 4];
        self.samples = [0; 4];
        self.pcm_mask = [0; 2];
        self.sq = Default::default();
        self.wave = Wave::default();
        self.noise = Noise::default();
        self.sweep = Sweep::default();
    }

    fn write_register(&mut self, addr: u16, val: u8) {
        let reg = usize::from(addr - 0xFF10);
        let mut stored = val;
        match addr {
            // NR50/NR51 only matter to the mixer, which reads `regs`.
            0xFF24 | 0xFF25 => {}
            0xFF10 => self.write_nr10(val),
            0xFF11 | 0xFF16 => {
                let i = usize::from(addr == 0xFF16);
                self.sq[i].length = 0x40 - u16::from(val & 0x3F);
                if !self.power {
                    stored &= 0x3F;
                }
            }
            0xFF12 | 0xFF17 => {
                let i = usize::from(addr == 0xFF17);
                if val & 0xF8 == 0 {
                    // The DAC goes off, and the channel with it.
                    self.regs[reg] = val;
                    self.active[i] = false;
                    self.update_sample(i, 0);
                } else if self.active[i] {
                    let sq = &mut self.sq[i];
                    nrx2_glitch(
                        &mut sq.volume,
                        val,
                        self.regs[reg],
                        &mut sq.volume_countdown,
                        &mut sq.envelope,
                    );
                    self.update_square_sample(i);
                }
            }
            0xFF13 | 0xFF18 => {
                let sq = &mut self.sq[usize::from(addr == 0xFF18)];
                sq.period = (sq.period & 0x700) | u16::from(val);
                if sq.just_reloaded {
                    sq.countdown = (sq.period ^ 0x7FF) * 2 + 1;
                }
            }
            0xFF14 | 0xFF19 => self.write_square_nrx4(usize::from(addr == 0xFF19), reg, val),
            0xFF1A => self.write_nr30(val),
            0xFF1B => self.wave.length = 0x100 - u16::from(val),
            0xFF1C => {
                self.wave.shift = [4, 0, 1, 2][usize::from((val >> 5) & 3)];
                if self.active[2] {
                    self.update_wave_sample();
                }
            }
            0xFF1D => {
                self.wave.period = (self.wave.period & 0x700) | u16::from(val);
                if self.wave.bugged_read_countdown == 1 {
                    self.wave.countdown = self.wave.period ^ 0x7FF;
                }
            }
            0xFF1E => self.write_nr34(val),
            0xFF20 => self.noise.length = 0x40 - u16::from(val & 0x3F),
            0xFF21 => self.write_nr42(val),
            0xFF22 => self.write_nr43(val),
            0xFF23 => self.write_nr44(val),
            _ => {}
        }
        self.regs[reg] = stored;
    }

    /// NRx4 of a square channel: the period's high bits, the length enable
    /// and the trigger.
    fn write_square_nrx4(&mut self, i: usize, reg: usize, val: u8) {
        let was_active = self.active[i];
        let trigger = val & 0x80 != 0;
        {
            let sq = &mut self.sq[i];
            // Dropping the period from $7xx just as the countdown reloads:
            // the step it took is taken back (SameBoy's workaround for the
            // write landing a tick late).
            if !trigger
                && was_active
                && self.regs[reg] & 7 == 7
                && val & 7 != 7
                && sq.countdown & 1 == 1
                && sq.did_tick
                && sq.countdown >> 1 == sq.period ^ 0x7FF
            {
                sq.index = sq.index.wrapping_sub(1) & 7;
                sq.suppressed = false;
            }
            sq.period = (sq.period & 0xFF) | u16::from(val & 7) << 8;
            if sq.just_reloaded {
                sq.countdown = (sq.period ^ 0x7FF) * 2 + 1;
            }
        }
        if trigger {
            let nrx2 = self.regs[Self::nrx2(i)];
            let lf_div = u16::from(self.lf_div);
            let sq = &mut self.sq[i];
            // The duty position carries on; only switching the APU off
            // resets it. The first step comes after a delay of a few ticks,
            // 2 fewer if the channel was already playing.
            sq.envelope.locked = false;
            sq.envelope.clock = false;
            sq.did_tick = false;
            let delay = if !was_active {
                if self.double_speed {
                    6 + lf_div
                } else {
                    6 - lf_div
                }
            } else {
                4 - lf_div
            };
            sq.countdown = (sq.period ^ 0x7FF) * 2 + delay;
            sq.volume = nrx2 >> 4;
            if was_active {
                self.update_square_sample(i);
            }
            let sq = &mut self.sq[i];
            sq.volume_countdown = nrx2 & 7;
            if nrx2 & 0xF8 != 0 && !was_active {
                self.active[i] = true;
                self.update_sample(i, 0);
                self.sq[i].suppressed = true;
            }
            let sq = &mut self.sq[i];
            if sq.length == 0 {
                sq.length = 0x40;
                sq.length_enabled = false;
            }
            if i == 0 {
                self.trigger_sweep(was_active);
            }
        }
        self.length_enable_glitch(i, val, 0x3F);
    }

    /// CH1's trigger restarts the sweep. With a nonzero step the overflow
    /// check also runs, a few 1 MHz ticks later.
    fn trigger_sweep(&mut self, was_active: bool) {
        let nr10 = self.regs[NR10];
        let s = &mut self.sweep;
        s.instant_calculation_done = false;
        s.shadow = 0;
        s.completed_addend = 0;
        if nr10 & 7 != 0 {
            s.calculate_countdown = nr10 & 7;
            s.reload_timer = if self.lf_div ^ u8::from(!self.double_speed) != 0 {
                3
            } else {
                2
            };
            s.unshifted = false;
            if !was_active {
                s.reload_timer += 1;
            }
            s.addend = self.sq[0].period >> (nr10 & 7);
        } else {
            s.addend = 0;
        }
        s.restart_hold = 2 - self.lf_div + if self.cgb { 2 } else { 0 };
        s.countdown = ((nr10 >> 4) & 7) ^ 7;
    }

    /// Turning length on while the frame sequencer's last event clocked
    /// lengths clocks this one once more, at once (blargg's "len ctr").
    /// If that runs it out the channel stops, unless this write triggers it,
    /// in which case it reloads one short.
    fn length_enable_glitch(&mut self, ch: usize, val: u8, reload: u16) {
        let enable = val & 0x40 != 0;
        let odd = self.div_divider & 1 == 1;
        let (length, enabled) = match ch {
            0 | 1 => (&mut self.sq[ch].length, &mut self.sq[ch].length_enabled),
            2 => (&mut self.wave.length, &mut self.wave.length_enabled),
            _ => (&mut self.noise.length, &mut self.noise.length_enabled),
        };
        let mut stop = false;
        if enable && !*enabled && odd && *length > 0 {
            *length -= 1;
            if *length == 0 {
                if val & 0x80 != 0 {
                    *length = reload;
                } else {
                    stop = true;
                }
            }
        }
        *enabled = enable;
        if stop {
            self.active[ch] = false;
            self.update_sample(ch, 0);
        }
    }

    /// NR10. Leaving subtract mode after a subtraction stops CH1; writes as
    /// a calculation is under way can disturb it.
    fn write_nr10(&mut self, val: u8) {
        if self.sweep.calculate_countdown > 0 || self.sweep.reload_timer > 0 {
            self.nr10_write_glitch(val);
        }
        self.regs[NR10] = val;
        // On the original and CPU CGB C the check uses subtract mode's +1
        // whatever the old mode was.
        if self.sweep.shadow + self.sweep.completed_addend + 1 > 0x7FF && val & 8 == 0 {
            self.active[0] = false;
            self.update_sample(0, 0);
        }
        self.trigger_sweep_calculation(false);
    }

    /// An NR10 write while a calculation counts down (CPU CGB C and older).
    fn nr10_write_glitch(&mut self, val: u8) {
        let s = &mut self.sweep;
        if s.reload_timer == 1 && self.lf_div == 0 {
            if self.double_speed {
                // Instance-specific corruption, as two of SameBoy's CPU CGB Cs.
                const CORRUPTION: [u8; 8] = [7, 7, 5, 7, 3, 3, 5, 7];
                s.calculate_countdown = CORRUPTION[usize::from(s.calculate_countdown & 7)];
            }
        } else if s.reload_timer > 1 {
            if self.double_speed {
                s.calculate_countdown = val & 7;
            }
        } else if s.calculate_countdown > 0 {
            let zombie_step = if self.regs[NR10] & 7 == 0 {
                self.lf_div ^ u8::from(self.double_speed) != 0
            } else {
                self.double_speed && s.calculate_countdown == 1
            };
            if zombie_step {
                s.calculate_countdown -= 1;
                if s.calculate_countdown <= 1 {
                    s.calculate_countdown = 0;
                    self.sweep_calculation_done();
                }
            }
        }
    }

    /// The sweep's calculation finishing. The overflow check adds the
    /// addend to the shadow period (the hardware's look-ahead: the period
    /// after next must fit too).
    fn sweep_calculation_done(&mut self) {
        let nr10 = self.regs[NR10];
        let s = &mut self.sweep;
        if s.restart_hold == 0 {
            s.shadow = self.sq[0].period;
        }
        if nr10 & 8 != 0 {
            s.addend ^= 0x7FF;
        }
        if s.shadow + s.addend > 0x7FF && nr10 & 8 == 0 {
            self.active[0] = false;
            self.update_sample(0, 0);
        }
        self.sweep.completed_addend = self.sweep.addend;
    }

    /// A 128 Hz sweep clock (or an NR10 write): when the countdown reaches
    /// 7, the period takes the last calculation's result and the next
    /// calculation starts.
    fn trigger_sweep_calculation(&mut self, during_div_write: bool) {
        let nr10 = self.regs[NR10];
        if nr10 & 0x70 == 0 || self.sweep.countdown != 7 {
            return;
        }
        if nr10 & 7 != 0 {
            self.sq[0].period =
                (self.sweep.addend + self.sweep.shadow + u16::from(nr10 & 8 != 0)) & 0x7FF;
        }
        let s = &mut self.sweep;
        if s.restart_hold == 0 {
            s.addend = self.sq[0].period >> (nr10 & 7);
        }
        s.calculate_countdown = nr10 & 7;
        s.reload_timer = 1 + self.lf_div;
        if !self.double_speed && during_div_write {
            s.reload_timer = 1;
        }
        s.unshifted = nr10 & 7 == 0;
        s.countdown = ((nr10 >> 4) & 7) ^ 7;
        if s.calculate_countdown == 0 {
            s.instant_calculation_done = true;
        }
    }

    /// NR30: the wave DAC. Turning it off stops CH3; just as CH3 reads
    /// wave RAM, that read goes astray.
    fn write_nr30(&mut self, val: u8) {
        self.wave.enable = val & 0x80 != 0;
        if !self.wave.enable {
            self.wave.pulsed = false;
            if self.active[2] {
                if self.wave.countdown == 0 {
                    // TODO(accuracy): SameBoy takes the byte at the CPU's PC
                    // (low 4 bits); the address bus is the nearest we have.
                    self.wave.byte = self.wave_ram[usize::from(self.address_bus & 0xF)];
                } else if self.wave.just_read {
                    self.wave.byte = self.wave_ram[0xA];
                }
            }
            self.active[2] = false;
            self.update_sample(2, 0);
        }
    }

    /// NR34: CH3's trigger restarts at sample 0 of wave RAM. The first
    /// sample is read (2047 - period) + 3 ticks later; until then the last
    /// byte read keeps playing (Pan Docs). Retriggering on the original just
    /// as CH3 reads wave RAM corrupts its first bytes.
    fn write_nr34(&mut self, val: u8) {
        let w = &mut self.wave;
        w.period = (w.period & 0xFF) | u16::from(val & 7) << 8;
        if val & 0x80 != 0 {
            w.pulsed = true;
            if !self.cgb && self.active[2] && w.countdown == 0 {
                let offset = usize::from(((w.index + 1) >> 1) & 0xF);
                if offset < 4 {
                    self.wave_ram[0] = self.wave_ram[offset];
                } else {
                    let start = offset & !3;
                    self.wave_ram.copy_within(start..start + 4, 0);
                }
            }
            let w = &mut self.wave;
            w.index = 0;
            if self.active[2] && w.countdown == 0 {
                w.byte = self.wave_ram[0];
            }
            if self.wave.enable {
                self.active[2] = true;
                let first = (self.wave.byte >> 4) >> self.wave.shift;
                self.update_sample(2, first);
            }
            let w = &mut self.wave;
            w.countdown = (w.period ^ 0x7FF) + 3;
            if w.length == 0 {
                w.length = 0x100;
                w.length_enabled = false;
            }
        }
        self.length_enable_glitch(2, val, 0xFF);
    }

    /// NR42. Turning the DAC off stops CH4 and its counter.
    fn write_nr42(&mut self, val: u8) {
        if val & 0xF8 == 0 {
            if self.active[3] && self.regs[NR43] & 7 != 0 {
                if self.noise.counter_countdown <= 2 {
                    self.noise.counter = (self.noise.counter + 1) & 0x3FFF;
                }
                self.noise.background_counter_active = false;
            }
            self.regs[NR42] = val;
            self.active[3] = false;
            self.update_sample(3, 0);
            self.noise.counter_active = false;
        } else if self.active[3] {
            let n = &mut self.noise;
            nrx2_glitch(
                &mut n.volume,
                val,
                self.regs[NR42],
                &mut n.volume_countdown,
                &mut n.envelope,
            );
            self.update_noise_sample();
        }
    }

    /// NR43: the noise clock. Changing the counter bit the LFSR watches can
    /// look like a rising edge to it, and step it.
    fn write_nr43(&mut self, val: u8) {
        let alignment = usize::from(self.noise.alignment & 3);
        if self.noise.countdown_reloaded {
            let divisor = match (val & 7) << 2 {
                0 => 2,
                d => d + [2, 1, 4, 3][alignment],
            };
            self.noise.counter_countdown = divisor;
            let counter = self.noise.counter;
            let old_reg = self.regs[NR43];
            let bit = |c: u16, reg: u8| (c >> (reg >> 4)) & 1 != 0;
            if !bit(counter, old_reg) && bit(counter, val) && (counter >> 7) & 1 != 0 {
                let previous = counter.wrapping_sub(1) & 0x3FFF;
                if bit(previous, old_reg) && !bit(previous, val) && (previous >> 7) & 1 != 0 {
                    self.step_lfsr();
                }
            }
        }
        // Every write passes through $FF on the way.
        self.nr43_write(0xFF);
        self.nr43_write(val);
    }

    /// One stage of an NR43 write (CPU CGB C and the original).
    fn nr43_write(&mut self, new: u8) {
        self.noise.narrow = new & 8 != 0;
        let old = self.regs[NR43];
        self.regs[NR43] = new;
        if old & 0xF0 == new & 0xF0 {
            return;
        }
        let mut counter = self.noise.counter;
        if self.noise.countdown_reloaded {
            counter |= counter.wrapping_sub(1) & 0x3FFF;
        }
        let bit = |reg: u8| (u32::from(counter) >> (reg >> 4)) & 1 != 0;
        let old_bit = bit(old);
        let glitch_bit = bit((old & 0x7F) | (new & 0x80));
        let new_bit = bit(new);
        if old_bit == new_bit && new_bit != glitch_bit {
            // A glitching write. With the new bit set the revisions
            // modelled here don't step; otherwise it's a plain step.
            if !new_bit {
                self.step_lfsr();
            }
        } else if !old_bit && new_bit {
            let narrow = self.noise.narrow;
            self.noise.narrow = true;
            self.step_lfsr();
            self.noise.narrow = narrow;
            if new & 0xF0 <= 0x20 && glitch_bit && counter & 8 == 0 {
                self.step_lfsr();
                let (high, low) = if self.noise.narrow {
                    (0x4040, 0x2020)
                } else {
                    (0x4000, 0x2000)
                };
                self.noise.lfsr &= !high;
                self.noise.lfsr |= (self.noise.lfsr & low) << 1;
            }
        } else if new & 0xF0 <= 0x20 && !glitch_bit && !new_bit && !old_bit && counter & 8 != 0 {
            self.step_lfsr();
        }
    }

    /// NR44: CH4's trigger.
    fn write_nr44(&mut self, val: u8) {
        if val & 0x80 != 0 {
            self.noise.envelope.locked = false;
            self.noise.envelope.clock = false;
            if !self.cgb && self.noise.alignment & 3 != 0 {
                self.noise.dmg_delayed_start = 6;
            } else {
                self.noise.lfsr = 0;
                self.prepare_noise_start();
                let nr42 = self.regs[NR42];
                let n = &mut self.noise;
                n.volume = nr42 >> 4;
                n.current_lfsr_sample = false;
                n.volume_countdown = nr42 & 7;
                n.did_step_counter = n.alignment & 3 == 2;
                if nr42 & 0xF8 != 0 {
                    self.active[3] = true;
                    self.update_sample(3, 0);
                }
                let n = &mut self.noise;
                if n.length == 0 {
                    n.length = 0x40;
                    n.length_enabled = false;
                }
            }
        }
        self.length_enable_glitch(3, val, 0x3F);
    }

    /// Sets up the noise counter for a trigger: when its first count comes
    /// depends on the 4-tick phase, the divider and what the counter was
    /// doing (SameBoy's prepare_noise_start, for CPU CGB C and older).
    fn prepare_noise_start(&mut self) {
        let nr43 = self.regs[NR43];
        let active = self.active[3];
        let ds = self.double_speed;
        let n = &mut self.noise;
        n.counter_active = self.regs[NR42] & 0xF8 != 0;
        let was_started_with_dac_disabled = n.started_with_dac_disabled;
        n.started_with_dac_disabled = !n.counter_active;
        let mut divisor = nr43 & 7;
        let was_background_counting = n.background_counter_active;
        n.background_counter_active = true;
        let mut instant_step = false;
        let mut div_1_glitch = false;
        let align = n.alignment;

        // About to count: the count happens now (in double speed a tick
        // earlier too).
        if divisor > 1 && (n.counter_countdown == 1 || (n.counter_countdown == 2 && active && ds)) {
            n.counter = (n.counter + 1) & 0x3FFF;
        } else if n.counter_countdown == 2 && align & 3 == 0 && active {
            if divisor == 0 {
                divisor = 8;
            } else if divisor == 1 {
                if !n.did_step_counter {
                    div_1_glitch = true;
                }
                let mask = 1u32 << (nr43 >> 4);
                let old_bit = u32::from(n.counter) & mask != 0;
                n.counter = (n.counter + 1) & 0x3FFF;
                let new_bit = u32::from(n.counter) & mask != 0;
                instant_step = new_bit && !old_bit;
            }
        }
        let mut countdown: i16 = if divisor == 0 {
            6
        } else {
            i16::from(divisor) * 4 + 6
        };
        if align & 1 == 1 {
            if divisor == 0 {
                countdown += 1;
            } else if align & 2 != 0 {
                if divisor == 1 && !active {
                    countdown += 1;
                } else {
                    countdown -= 3;
                }
            } else {
                countdown -= 1;
                if divisor == 1 && active {
                    countdown -= 4;
                }
            }
        } else if divisor != 0 {
            if align & 2 != 0 {
                if ds && divisor == 1 {
                    countdown += 2;
                } else {
                    countdown -= 2;
                }
            } else if (divisor > 1 && !ds) || (divisor == 1 && active && nr43 & 0xF0 == 0) {
                countdown -= 4;
            }
        } else if ds {
            countdown += 2;
        }
        // The counter running on in the background, with the DAC off.
        if divisor > 1 {
            if !n.counter_active && align & 3 == 0 {
                countdown += 4;
            }
        } else if was_background_counting && !active && align & 3 == 0 {
            if divisor == 0 {
                if was_started_with_dac_disabled {
                    countdown += 28;
                }
            } else {
                countdown -= 4;
            }
        }
        if divisor == 0 && was_background_counting && !active && ds {
            countdown -= 1;
        }
        if div_1_glitch {
            countdown -= 4;
        }
        n.counter_countdown = countdown.clamp(0, 255) as u8;
        n.lfsr = if divisor == 0 && active && align & 3 == 3 {
            0x0055
        } else {
            0
        };
        if instant_step {
            self.step_lfsr();
        }
    }

    /// XNOR of bits 0 and 1 goes into bit 14 (and bit 6 in 7-bit mode) as
    /// everything shifts right. https://gbdev.io/pandocs/Audio_details.html
    fn step_lfsr(&mut self) {
        let n = &mut self.noise;
        n.lfsr_bit_7_before_step = n.lfsr & 0x80 != 0;
        let high = if n.narrow { 0x4040 } else { 0x4000 };
        let bit = (n.lfsr ^ (n.lfsr >> 1) ^ 1) & 1 != 0;
        n.lfsr >>= 1;
        if bit {
            n.lfsr |= high;
        } else {
            n.lfsr &= !high;
        }
        n.current_lfsr_sample = n.lfsr & 1 != 0;
        n.lfsr_stepped_in_narrow = n.narrow;
        if self.active[3] {
            self.update_noise_sample();
        }
    }

    /// A DIV-APU event: the falling edge of DIV bit 4 (bit 5 in double
    /// speed), 512 times a second. Lengths clock on every other one, the
    /// sweep on every 4th; the envelopes count down every 8th and move on
    /// the event after they were armed. `during_div_write`: a DIV write
    /// made the edge. https://gbdev.io/pandocs/Audio_details.html#div-apu
    pub fn div_event(&mut self, during_div_write: bool) {
        if self.event_delay {
            self.held_events += 1;
            self.held_by_write = during_div_write;
            return;
        }
        self.frame_sequencer_event(during_div_write);
    }

    fn frame_sequencer_event(&mut self, during_div_write: bool) {
        self.pcm_mask = [0xFF; 2];
        if !self.power {
            return;
        }
        match self.skip_div_event {
            SKIP_NEXT => {
                self.skip_div_event = SKIP_SKIPPED;
                return;
            }
            SKIP_SKIPPED => self.skip_div_event = SKIP_INACTIVE,
            _ => self.div_divider = self.div_divider.wrapping_add(1),
        }
        if self.div_divider & 7 == 7 {
            for sq in &mut self.sq {
                if !sq.envelope.clock {
                    sq.volume_countdown = sq.volume_countdown.wrapping_sub(1) & 7;
                }
            }
            if !self.noise.envelope.clock {
                self.noise.volume_countdown = self.noise.volume_countdown.wrapping_sub(1) & 7;
            }
        }
        for i in 0..2 {
            if self.sq[i].envelope.clock {
                self.tick_square_envelope(i);
            }
        }
        if self.noise.envelope.clock {
            self.tick_noise_envelope();
        }
        if self.div_divider & 1 == 1 {
            for ch in 0..4 {
                let (length, enabled) = match ch {
                    0 | 1 => (&mut self.sq[ch].length, self.sq[ch].length_enabled),
                    2 => (&mut self.wave.length, self.wave.length_enabled),
                    _ => (&mut self.noise.length, self.noise.length_enabled),
                };
                if enabled && *length > 0 {
                    *length -= 1;
                    if *length == 0 {
                        self.active[ch] = false;
                        self.update_sample(ch, 0);
                    }
                }
            }
        }
        if self.div_divider & 3 == 3 {
            self.sweep.countdown = (self.sweep.countdown + 1) & 7;
            self.trigger_sweep_calculation(during_div_write);
        }
    }

    /// The rising edge of the DIV-APU bit: envelopes whose countdown has
    /// run out reload it and are armed for the next event.
    pub fn div_secondary_event(&mut self) {
        self.pcm_mask = [0xFF; 2];
        if !self.power {
            return;
        }
        for i in 0..2 {
            let nrx2 = self.regs[Self::nrx2(i)];
            let sq = &mut self.sq[i];
            if self.active[i] && sq.volume_countdown == 0 {
                sq.volume_countdown = nrx2 & 7;
                sq.envelope.set(nrx2 & 7 != 0, nrx2 & 8 != 0, sq.volume);
            }
        }
        let nr42 = self.regs[NR42];
        let n = &mut self.noise;
        if self.active[3] && n.volume_countdown == 0 {
            n.volume_countdown = nr42 & 7;
            n.envelope.set(nr42 & 7 != 0, nr42 & 8 != 0, n.volume);
        }
    }

    /// One envelope step, unless locked at 0 or 15.
    /// TODO(accuracy): CPU CGB C's PCM registers glitch here in double
    /// speed in other ways too.
    fn tick_square_envelope(&mut self, i: usize) {
        let nrx2 = self.regs[Self::nrx2(i)];
        let sq = &mut self.sq[i];
        sq.envelope.set(false, false, 0);
        if sq.envelope.locked || nrx2 & 7 == 0 {
            return;
        }
        if self.double_speed {
            if i == 0 {
                self.pcm_mask[0] &= sq.volume | 0xF1;
            } else {
                self.pcm_mask[0] &= (sq.volume << 4) | 0x3F;
            }
        }
        sq.volume = if nrx2 & 8 != 0 {
            sq.volume.wrapping_add(1)
        } else {
            sq.volume.wrapping_sub(1)
        } & 0xF;
        if self.active[i] {
            self.update_square_sample(i);
        }
    }

    fn tick_noise_envelope(&mut self) {
        let nr42 = self.regs[NR42];
        let n = &mut self.noise;
        n.envelope.set(false, false, 0);
        if n.envelope.locked || nr42 & 7 == 0 {
            return;
        }
        if self.double_speed {
            self.pcm_mask[1] &= (n.volume << 4) | 0x1F;
        }
        n.volume = if nr42 & 8 != 0 {
            n.volume.wrapping_add(1)
        } else {
            n.volume.wrapping_sub(1)
        } & 0xF;
        if self.active[3] {
            self.update_noise_sample();
        }
    }

    /// Advances by `cycles` CPU T-cycles: a tick every 2 of them (every 4
    /// in double speed), adding output samples as they come due.
    pub fn tick(&mut self, cycles: u32) {
        let per_tick = if self.double_speed { 4 } else { 2 };
        let total = u32::from(self.cycle_carry) + cycles;
        let ticks = total / per_tick;
        self.cycle_carry = (total % per_tick) as u8;
        if ticks == 0 {
            return;
        }
        self.run(ticks);
        // A tick is 2 dots. The mix is taken as it stands now, for all of
        // them: the bus runs a few ticks at a time.
        let [left, right] = self.mix();
        let rate = u64::from(self.sample_rate);
        let mut dots = ticks * 2;
        while dots > 0 {
            let until_sample = (u64::from(CPU_HZ) - self.sample_clock).div_ceil(rate);
            let step = dots.min(until_sample as u32).max(1);
            self.acc[0] += left * step as f32;
            self.acc[1] += right * step as f32;
            self.acc_cycles += step;
            self.sample_clock += u64::from(step) * rate;
            if self.sample_clock >= u64::from(CPU_HZ) {
                self.sample_clock -= u64::from(CPU_HZ);
                self.emit_sample();
            }
            dots -= step;
        }
    }

    /// Runs the channels for `cycles` ticks.
    fn run(&mut self, mut cycles: u32) {
        if cycles == 0 {
            return;
        }
        // A delayed CH4 start falls inside this run: run up to it first.
        let delayed = u32::from(self.noise.dmg_delayed_start);
        if delayed > 0 && delayed < cycles {
            self.run(delayed);
            cycles -= delayed;
        }
        if self.wave.bugged_read_countdown > 0 {
            if u32::from(self.wave.bugged_read_countdown) <= cycles {
                self.wave.bugged_read_countdown = 0;
                self.wave.byte = self.wave_ram[usize::from(self.address_bus & 0xF)];
                if self.active[2] {
                    self.update_wave_sample();
                }
            } else {
                self.wave.bugged_read_countdown -= cycles as u8;
            }
        }
        let mut start_ch4 = false;
        let delayed = u32::from(self.noise.dmg_delayed_start);
        if delayed == cycles {
            self.noise.dmg_delayed_start = 0;
            start_ch4 = true;
        } else if delayed > cycles {
            self.noise.dmg_delayed_start -= cycles as u8;
        }

        // The 1 MHz clock's phase.
        self.lf_div ^= (cycles & 1) as u8;
        self.noise.alignment = self.noise.alignment.wrapping_add(cycles as u8);
        self.run_sweep(cycles);
        for i in 0..2 {
            if self.active[i] {
                self.run_square(i, cycles);
            }
        }
        self.run_wave(cycles);
        self.run_noise(cycles);
        if start_ch4 {
            let nr44 = self.regs[NR44] | 0x80;
            self.write_register(0xFF23, nr44);
        }
    }

    fn run_sweep(&mut self, cycles: u32) {
        let mut sweep_cycles = cycles / 2;
        if cycles & 1 == 1 && self.lf_div == 0 {
            sweep_cycles += 1;
        }
        let reload = u32::from(self.sweep.reload_timer);
        if reload > sweep_cycles {
            self.sweep.reload_timer -= sweep_cycles as u8;
            sweep_cycles = 0;
        } else {
            if reload > 0
                && self.sweep.calculate_countdown == 0
                && self.sweep.instant_calculation_done
            {
                self.sweep_calculation_done();
            }
            self.sweep.instant_calculation_done = false;
            sweep_cycles -= reload;
            self.sweep.reload_timer = 0;
        }
        // The calculation pauses while NR10's step is 0.
        if self.sweep.calculate_countdown > 0 && (self.regs[NR10] & 7 != 0 || self.sweep.unshifted)
        {
            if u32::from(self.sweep.calculate_countdown) > sweep_cycles {
                self.sweep.calculate_countdown -= sweep_cycles as u8;
            } else {
                self.sweep.calculate_countdown = 0;
                self.sweep_calculation_done();
            }
        }
        let hold = &mut self.sweep.restart_hold;
        *hold = (u32::from(*hold).saturating_sub(cycles)) as u8;
    }

    fn run_square(&mut self, i: usize, cycles: u32) {
        let mut left = cycles;
        while left > u32::from(self.sq[i].countdown) {
            left -= u32::from(self.sq[i].countdown) + 1;
            let sq = &mut self.sq[i];
            sq.countdown = (sq.period ^ 0x7FF) * 2 + 1;
            sq.index = (sq.index + 1) & 7;
            sq.suppressed = false;
            sq.did_tick = true;
            if left == 0 && self.samples[i] == 0 {
                self.pcm_mask[0] &= if i == 0 { 0xF0 } else { 0x0F };
            }
            self.update_square_sample(i);
        }
        let sq = &mut self.sq[i];
        sq.just_reloaded = left == 0;
        if left > 0 {
            sq.countdown -= left as u16;
        }
    }

    fn run_wave(&mut self, cycles: u32) {
        self.wave.just_read = false;
        let mut left = cycles;
        if self.active[2] {
            while left > u32::from(self.wave.countdown) {
                left -= u32::from(self.wave.countdown) + 1;
                let w = &mut self.wave;
                w.countdown = w.period ^ 0x7FF;
                w.index = (w.index + 1) & 31;
                w.byte = self.wave_ram[usize::from(w.index >> 1)];
                w.just_read = true;
                self.update_wave_sample();
            }
            if left > 0 {
                self.wave.countdown -= left as u16;
                self.wave.just_read = false;
            }
        } else if self.wave.enable && self.wave.pulsed {
            // Stopped by its length but with the DAC on, CH3's counter runs
            // on and its reads take whatever the CPU has on the bus.
            while left > u32::from(self.wave.countdown) {
                left -= u32::from(self.wave.countdown) + 1;
                self.wave.countdown = self.wave.period ^ 0x7FF;
                if left > 0 {
                    self.wave.byte = self.wave_ram[usize::from(self.address_bus & 0xF)];
                } else {
                    self.wave.bugged_read_countdown = 1;
                }
            }
            if left > 0 {
                self.wave.countdown -= left as u16;
            }
            if self.wave.countdown == 0 {
                self.wave.bugged_read_countdown = 2;
            }
        }
    }

    fn run_noise(&mut self, cycles: u32) {
        let n = &self.noise;
        if !n.counter_active && !n.background_counter_active {
            return;
        }
        let nr43 = self.regs[NR43];
        let divisor = match (nr43 & 7) << 2 {
            0 => 2,
            d => d,
        };
        if self.noise.counter_countdown == 0 {
            self.noise.counter_countdown = divisor;
        }
        let mut left = cycles;
        while left >= u32::from(self.noise.counter_countdown) {
            left -= u32::from(self.noise.counter_countdown);
            let n = &mut self.noise;
            n.counter_countdown = divisor;
            // Shifts 14 and 15 watch bits the 14-bit counter doesn't have.
            let mask = 1u32 << (nr43 >> 4);
            let old_bit = u32::from(n.counter) & mask != 0;
            n.counter = (n.counter + 1) & 0x3FFF;
            n.did_step_counter = true;
            let new_bit = u32::from(n.counter) & mask != 0;
            if new_bit && !old_bit && self.active[3] {
                if left == 0 && self.samples[3] == 0 && !self.double_speed {
                    self.pcm_mask[1] &= 0x0F;
                }
                self.step_lfsr();
            }
        }
        let n = &mut self.noise;
        if left > 0 {
            n.counter_countdown -= left as u8;
            n.countdown_reloaded = false;
        } else {
            n.countdown_reloaded = true;
        }
    }

    /// The mixer: each channel's DAC output, added per side as NR51 routes
    /// it (bits 4-7 left, 0-3 right, CH1 lowest), times NR50's per-side
    /// volume + 1. Scaled from the -32..32 that can reach down to -1..1.
    fn mix(&self) -> [f32; 2] {
        let outs: [f32; 4] = std::array::from_fn(|ch| dac(self.dac_on(ch), self.samples[ch]));
        let nr50 = self.regs[NR50];
        let nr51 = self.regs[NR51];
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
            let mut out = input - self.capacitor[side];
            self.capacitor[side] = input - out * self.charge_factor;
            if !self.high_pass {
                out = input;
            }
            if self.samples_out.len() < MAX_BUFFERED {
                self.samples_out.push(out);
            }
        }
        self.acc = [0.0; 2];
        self.acc_cycles = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original's boot state with any samples so far thrown away.
    fn apu() -> Apu {
        let mut a = Apu::new(false);
        a.take_samples();
        a
    }

    fn write_all(a: &mut Apu, writes: &[(u16, u8)]) {
        for &(addr, val) in writes {
            a.write(addr, val);
        }
    }

    /// Runs `cycles` T-cycles an M-cycle at a time, as the bus does.
    fn tick(a: &mut Apu, cycles: u32) {
        for _ in 0..cycles / 4 {
            a.tick(4);
        }
        a.tick(cycles % 4);
    }

    /// Runs `seconds` and returns (left, right) sample streams.
    fn run(a: &mut Apu, seconds: f64) -> (Vec<f32>, Vec<f32>) {
        a.take_samples();
        tick(a, (f64::from(CPU_HZ) * seconds) as u32);
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
    fn lfsr_period(narrow: bool) -> usize {
        let mut a = apu();
        a.noise.narrow = narrow;
        let mask = if narrow { 0x7F } else { 0x7FFF };
        let start = a.noise.lfsr & mask;
        (1..=40_000)
            .find(|_| {
                a.step_lfsr();
                a.noise.lfsr & mask == start
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
        let mut a = apu();
        write_all(&mut a, &[(0xFF21, 0xF0), (0xFF22, 0xE0), (0xFF23, 0x80)]);
        tick(&mut a, 1_000_000);
        assert_eq!(a.noise.lfsr, 0);
        a.write(0xFF22, 0x00);
        tick(&mut a, 1000);
        assert_ne!(a.noise.lfsr, 0, "runs again at a normal shift");
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
    fn without_the_high_pass_filter_a_quiet_dac_holds_its_dc_level() {
        // CH2's DAC on at volume 0: digital 0 is analog +1, a DC level the
        // filter drains away. Without the filter it stays.
        let mut a = apu();
        a.set_high_pass_filter(false);
        write_all(&mut a, &[(0xFF25, 0x22), (0xFF17, 0x08), (0xFF19, 0x80)]);
        let (left, _) = run(&mut a, 0.5);
        let last = *left.last().unwrap();
        assert!(last > 0.01, "{last}");
        assert!(left[left.len() / 2..].iter().all(|&s| s == last));
    }

    #[test]
    fn silence_after_boot_is_silent() {
        let mut a = apu();
        let (left, right) = run(&mut a, 0.5);
        assert!(peak_to_peak(&left) < 0.001 && peak_to_peak(&right) < 0.001);
    }

    /// Runs `n` DIV-APU periods: a falling edge, then half a period later
    /// the rising one, with the channels ticking in between.
    fn steps(a: &mut Apu, n: u32) {
        for _ in 0..n {
            a.div_event(false);
            tick(a, 4096);
            a.div_secondary_event();
            tick(a, 4096);
        }
    }

    fn ch_on(a: &Apu, ch: u8) -> bool {
        a.read(0xFF26) & (1 << ch) != 0
    }

    #[test]
    fn length_timer_stops_the_note() {
        // CH2, length 10 (64 - 54), length enabled, triggered before the
        // first event.
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF16, 54), (0xFF19, 0xC0)]);
        // Lengths clock on odd events: the 10th clock is the 19th event.
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
    fn enabling_length_after_a_length_clock_clocks_it_once_more() {
        // Just after an event that clocked lengths, turning length on clocks
        // it straight away: a length of 1 runs out at once.
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF16, 63), (0xFF19, 0x80)]);
        steps(&mut a, 1);
        assert!(ch_on(&a, 1));
        a.write(0xFF19, 0x40); // length on, no trigger
        assert!(!ch_on(&a, 1), "the extra clock ran it out");

        // After one that didn't there's no extra clock.
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF16, 63), (0xFF19, 0x80)]);
        steps(&mut a, 2);
        a.write(0xFF19, 0x40);
        assert!(ch_on(&a, 1));
    }

    #[test]
    fn envelope_fades_out_and_in() {
        // Volume 15, down, pace 1: one step per 64 Hz tick (every 8 events).
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0xF1), (0xFF19, 0x80)]);
        assert_eq!(a.sq[1].volume, 15);
        steps(&mut a, 8);
        assert_eq!(a.sq[1].volume, 14);
        steps(&mut a, 8 * 20);
        assert_eq!(a.sq[1].volume, 0, "stops at 0");
        assert!(ch_on(&a, 1), "a faded channel is still on");

        // Volume 0, up, pace 2: one step every 16.
        let mut a = apu();
        write_all(&mut a, &[(0xFF17, 0x0A), (0xFF19, 0x80)]);
        steps(&mut a, 16 * 3);
        assert_eq!(a.sq[1].volume, 3);
        steps(&mut a, 16 * 20);
        assert_eq!(a.sq[1].volume, 15, "stops at 15");

        // Pace 0 holds the volume.
        let mut a = apu();
        write_all(&mut a, &[(0xFF21, 0x90), (0xFF23, 0x80)]);
        steps(&mut a, 800);
        assert_eq!(a.noise.volume, 9);
    }

    /// CH1 at volume 15 with NR10 = `nr10`, period `period`, triggered,
    /// and given time for the sweep's calculation.
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
        tick(&mut a, 64);
        a
    }

    #[test]
    fn sweep_raises_the_pitch_until_it_overflows() {
        // Pace 1, up, step 1: each sweep clock adds period >> 1.
        let mut a = sweeping(0x11, 0x100);
        // The sweep clocks on every 4th event, starting with the 3rd.
        steps(&mut a, 3);
        assert_eq!(a.sq[0].period, 0x180);
        steps(&mut a, 4);
        assert_eq!(a.sq[0].period, 0x240);
        steps(&mut a, 4);
        assert_eq!(a.sq[0].period, 0x360);
        steps(&mut a, 4);
        assert_eq!(a.sq[0].period, 0x510);
        assert!(ch_on(&a, 0), "the look-ahead, 0x798, still fits");
        steps(&mut a, 4);
        assert_eq!(a.sq[0].period, 0x798);
        assert!(!ch_on(&a, 0), "the look-ahead, 0x798 + 0x3CC, passes 2047");
    }

    #[test]
    fn sweep_down_lowers_the_pitch() {
        // Pace 2, subtract, step 2: every other sweep clock, minus period >> 2.
        let mut a = sweeping(0x2A, 0x400);
        steps(&mut a, 7);
        assert_eq!(a.sq[0].period, 0x300);
        assert!(ch_on(&a, 0));
    }

    #[test]
    fn sweep_overflow_is_checked_after_trigger() {
        // Step 1: 0x700 + 0x380 > 2047, found a few ticks after the trigger.
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
    fn power_off_keeps_length_counts_written_while_off_and_power_on_restarts_the_sequencer() {
        let mut a = apu();
        a.write(0xFF16, 54); // CH2 length 10
        steps(&mut a, 3);
        a.write(0xFF26, 0x00);
        a.write(0xFF16, 60); // still writable while off: length 4
        a.write(0xFF26, 0x80);
        assert_eq!(a.div_divider, 0, "sequencer restarts");
        write_all(&mut a, &[(0xFF17, 0xF0), (0xFF19, 0xC0)]);
        steps(&mut a, 6);
        assert!(ch_on(&a, 1));
        steps(&mut a, 1);
        assert!(!ch_on(&a, 1), "the length written while off counted");
    }

    /// The Color's boot state, ticked an M-cycle at a time as the bus does
    /// (clearing the PCM glitch mask as each M-cycle starts).
    fn cgb_apu() -> Apu {
        let mut a = Apu::new(true);
        a.take_samples();
        a
    }

    fn m_cycle(a: &mut Apu) {
        a.begin_cycles();
        a.tick(4);
    }

    #[test]
    fn pcm12_shows_a_square_channel_from_its_first_step() {
        // CH2 at volume 15, 75% duty, period $700: (2047 - $700) * 2 + 5 =
        // 515 ticks to the first step, which lands on a high step. Until
        // then the channel outputs 0.
        let mut a = cgb_apu();
        write_all(
            &mut a,
            &[
                (0xFF16, 0xC0),
                (0xFF17, 0xF0),
                (0xFF18, 0x00),
                (0xFF19, 0x87),
            ],
        );
        let mut m_cycles = 0;
        while a.read_pcm(0xFF76) == 0 {
            m_cycle(&mut a);
            m_cycles += 1;
            assert!(m_cycles < 1000);
        }
        // The step is at tick 516, the end of M-cycle 258; CPU CGB C reads
        // the level as it changes as 0, so it shows an M-cycle later.
        assert_eq!(m_cycles, 259);
        assert_eq!(a.read_pcm(0xFF76), 0xF0, "CH2 in the high nibble");
    }

    #[test]
    fn pcm34_shows_channel_3_from_its_first_sample() {
        // The first sample (index 1: byte 0's low nibble) is read
        // (2047 - period) + 4 ticks after the trigger: 259 for $700.
        let mut a = cgb_apu();
        for i in 0..16 {
            a.write(0xFF30 + i, 0xAB);
        }
        write_all(
            &mut a,
            &[
                (0xFF1A, 0x80),
                (0xFF1C, 0x20),
                (0xFF1D, 0x00),
                (0xFF1E, 0x87),
            ],
        );
        let mut m_cycles = 0;
        while a.read_pcm(0xFF77) == 0 {
            m_cycle(&mut a);
            m_cycles += 1;
            assert!(m_cycles < 1000);
        }
        assert_eq!(m_cycles, 130, "tick 259 is in M-cycle 130");
        assert_eq!(a.read_pcm(0xFF77), 0x0B, "CH3 in the low nibble");
    }

    /// Wave RAM holding $00, $11 .. $FF, and CH3 playing at period $7FE:
    /// its first read 5 ticks after the trigger, then one every 2 ticks.
    fn wave_playing(cgb: bool) -> Apu {
        let mut a = if cgb { cgb_apu() } else { apu() };
        for i in 0..16u8 {
            a.write(0xFF30 + u16::from(i), i * 0x11);
        }
        write_all(
            &mut a,
            &[
                (0xFF1A, 0x80),
                (0xFF1C, 0x20),
                (0xFF1D, 0xFE),
                (0xFF1E, 0x87),
            ],
        );
        a
    }

    #[test]
    fn while_ch3_plays_the_cpu_reaches_the_byte_it_reads() {
        // A tick at a time. The Color always gets CH3's byte; the original
        // only in the tick CH3 reads it, and $FF otherwise.
        let reads = |cgb| -> Vec<u8> {
            let mut a = wave_playing(cgb);
            (0..8)
                .map(|_| {
                    a.tick(2);
                    a.read(0xFF3F)
                })
                .collect()
        };
        assert_eq!(
            reads(true),
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x11]
        );
        assert_eq!(
            reads(false),
            [0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0xFF, 0x11, 0xFF]
        );

        // Writes go the same way.
        let mut a = wave_playing(false);
        a.tick(2);
        a.write(0xFF3F, 0x99);
        assert_eq!(a.wave_ram[0xF], 0xFF, "missed");
        a.tick(8);
        a.write(0xFF3F, 0x99);
        assert_eq!(a.wave_ram[0], 0x99, "tick 5: CH3 just read byte 0");
    }

    #[test]
    fn retriggering_the_originals_ch3_as_it_reads_corrupts_wave_ram() {
        // After the read at tick 5 (sample 1), tick 6 is the one before the
        // next read: retriggering then copies byte 1 over byte 0.
        for cgb in [false, true] {
            let mut a = wave_playing(cgb);
            for _ in 0..6 {
                a.tick(2);
            }
            a.write(0xFF1E, 0x87);
            let expected = if cgb { 0x00 } else { 0x11 };
            assert_eq!(a.wave_ram[0], expected, "cgb: {cgb}");
        }
    }

    #[test]
    fn writing_x8_to_a_playing_channels_nrx2_raises_its_volume() {
        // "Zombie mode": on CPU CGB C and the original, each write of volume
        // 0, up, pace 0 to a playing channel moves the volume up a step.
        let mut a = cgb_apu();
        write_all(&mut a, &[(0xFF17, 0x08), (0xFF19, 0x80)]);
        assert_eq!(a.sq[1].volume, 0);
        for _ in 0..3 {
            a.write(0xFF17, 0x08);
        }
        assert_eq!(a.sq[1].volume, 3);
    }

    #[test]
    fn the_colors_power_off_clears_lengths_and_ignores_length_writes() {
        // CH2 length 10, then power cycled with a write of length 4 while
        // off: the original keeps it; the Color ignores the write and
        // starts from 0, which a trigger turns into 64.
        for (cgb, length) in [(false, 4), (true, 64)] {
            let mut a = if cgb { cgb_apu() } else { apu() };
            a.write(0xFF16, 54);
            a.write(0xFF26, 0x00);
            a.write(0xFF16, 60);
            a.write(0xFF26, 0x80);
            write_all(&mut a, &[(0xFF17, 0xF0), (0xFF19, 0x80)]);
            assert_eq!(a.sq[1].length, length, "cgb: {cgb}");
        }
    }

    #[test]
    fn switching_into_double_speed_delays_div_apu_events_an_m_cycle_or_back() {
        let mut a = cgb_apu();
        a.speed_switched(true);
        let divider = a.div_divider;
        a.div_event(false);
        assert_eq!(a.div_divider, divider, "held for an M-cycle");
        a.begin_cycles();
        assert_eq!(a.div_divider, divider + 1);

        // The next switch into double speed undoes it.
        a.speed_switched(false);
        a.speed_switched(true);
        a.div_event(false);
        assert_eq!(a.div_divider, divider + 2);

        // So does switching the APU on.
        a.speed_switched(false);
        a.speed_switched(true);
        a.write(0xFF26, 0x00);
        a.write(0xFF26, 0x80);
        a.div_event(false);
        assert_eq!(a.div_divider, 1);
    }
}
