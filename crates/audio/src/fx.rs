//! Beat FX and Color FX, built from textbook DSP: delay lines, a comb and
//! all-pass reverb, LFO-swept filters and a two-tap pitch shifter.
//!
//! Everything is allocated up front in `new`, so processing never allocates
//! on the audio thread.

use std::f32::consts::PI;

use crate::eq::{Biquad, Filter};

/// Longest delay a Beat FX can use, in seconds.
const MAX_DELAY_SECS: f32 = 6.0;
/// Filter coefficients of swept effects are updated every this many frames.
const SWEEP_EVERY: u32 = 16;

/// A stereo delay line with a fixed maximum length.
#[derive(Debug)]
struct Delay {
    buffer: Vec<[f32; 2]>,
    write: usize,
}

impl Delay {
    fn new(frames: usize) -> Self {
        Self { buffer: vec![[0.0; 2]; frames.max(4)], write: 0 }
    }

    fn push(&mut self, frame: [f32; 2]) {
        self.buffer[self.write] = frame;
        self.write = (self.write + 1) % self.buffer.len();
    }

    /// The frame pushed `delay` frames ago (fractional, at least 1).
    fn read(&self, delay: f32) -> [f32; 2] {
        let len = self.buffer.len();
        let delay = delay.clamp(1.0, (len - 2) as f32);
        let whole = delay.floor();
        let frac = delay - whole;
        let at = |d: usize| self.buffer[(self.write + len - d) % len];
        let (a, b) = (at(whole as usize), at(whole as usize + 1));
        [a[0] + (b[0] - a[0]) * frac, a[1] + (b[1] - a[1]) * frac]
    }

    fn clear(&mut self) {
        self.buffer.fill([0.0; 2]);
    }
}

/// Feedback comb filter with a damped (low-passed) feedback path.
#[derive(Debug)]
struct Comb {
    buffer: Vec<f32>,
    at: usize,
    damped: f32,
}

impl Comb {
    fn process(&mut self, x: f32, feedback: f32, damping: f32) -> f32 {
        let y = self.buffer[self.at];
        self.damped = y * (1.0 - damping) + self.damped * damping;
        self.buffer[self.at] = x + self.damped * feedback;
        self.at = (self.at + 1) % self.buffer.len();
        y
    }
}

/// Schroeder all-pass, used to diffuse the comb output.
#[derive(Debug)]
struct AllPass {
    buffer: Vec<f32>,
    at: usize,
}

impl AllPass {
    fn process(&mut self, x: f32) -> f32 {
        let delayed = self.buffer[self.at];
        let y = delayed - x;
        self.buffer[self.at] = x + delayed * 0.5;
        self.at = (self.at + 1) % self.buffer.len();
        y
    }
}

/// A small stereo room: parallel combs into series all-passes, with the
/// right channel's delays slightly longer for width.
#[derive(Debug)]
struct Reverb {
    combs: [Vec<Comb>; 2],
    allpasses: [Vec<AllPass>; 2],
}

impl Reverb {
    fn new(rate: f32) -> Self {
        // Mutually prime lengths in milliseconds, so echoes do not line up.
        const COMBS: [f32; 6] = [25.3, 26.9, 28.9, 30.7, 32.2, 33.8];
        const ALLPASSES: [f32; 3] = [5.1, 7.7, 12.6];
        let frames = |ms: f32, spread: f32| ((ms + spread) * rate / 1000.0) as usize;
        let channel = |spread: f32| {
            (
                COMBS
                    .iter()
                    .map(|&ms| Comb { buffer: vec![0.0; frames(ms, spread)], at: 0, damped: 0.0 })
                    .collect(),
                ALLPASSES
                    .iter()
                    .map(|&ms| AllPass { buffer: vec![0.0; frames(ms, spread)], at: 0 })
                    .collect(),
            )
        };
        let (left_combs, left_allpasses) = channel(0.0);
        let (right_combs, right_allpasses) = channel(0.52);
        Self { combs: [left_combs, right_combs], allpasses: [left_allpasses, right_allpasses] }
    }

    /// `size` 0.0–1.0 sets the decay time.
    fn process(&mut self, x: [f32; 2], size: f32) -> [f32; 2] {
        let feedback = 0.7 + 0.27 * size.clamp(0.0, 1.0);
        let input = (x[0] + x[1]) * 0.08;
        std::array::from_fn(|ch| {
            let mut y: f32 =
                self.combs[ch].iter_mut().map(|c| c.process(input, feedback, 0.3)).sum();
            for allpass in &mut self.allpasses[ch] {
                y = allpass.process(y);
            }
            y
        })
    }

    fn clear(&mut self) {
        for comb in self.combs.iter_mut().flatten() {
            comb.buffer.fill(0.0);
            comb.damped = 0.0;
        }
        for allpass in self.allpasses.iter_mut().flatten() {
            allpass.buffer.fill(0.0);
        }
    }
}

/// Pitch shifting with two read taps that slide through a short delay line
/// and cross-fade, so the sound is resampled without changing its length.
#[derive(Debug)]
struct PitchShifter {
    delay: Delay,
    window: f32,
    phase: f32,
}

impl PitchShifter {
    fn new(rate: f32) -> Self {
        let window = rate * 0.05;
        Self { delay: Delay::new(window as usize + 8), window, phase: 0.0 }
    }

    /// `ratio` 2.0 = an octave up.
    fn process(&mut self, x: [f32; 2], ratio: f32) -> [f32; 2] {
        self.delay.push(x);
        self.phase = (self.phase + (1.0 - ratio) / self.window).rem_euclid(1.0);
        let other = (self.phase + 0.5) % 1.0;
        let (a, b) = (
            self.delay.read(2.0 + self.phase * self.window),
            self.delay.read(2.0 + other * self.window),
        );
        // sin² + cos² = 1: the taps fade in and out without a level dip.
        let ga = (PI * self.phase).sin().powi(2);
        let gb = (PI * other).sin().powi(2);
        [a[0] * ga + b[0] * gb, a[1] * ga + b[1] * gb]
    }
}

/// First-order all-pass stage of a phaser.
#[derive(Debug, Clone, Copy, Default)]
struct PhaseStage {
    x1: [f32; 2],
    y1: [f32; 2],
}

impl PhaseStage {
    fn process(&mut self, ch: usize, x: f32, a: f32) -> f32 {
        let y = a * x + self.x1[ch] - a * self.y1[ch];
        self.x1[ch] = x;
        self.y1[ch] = y;
        y
    }
}

/// Six-stage phaser with its notches at `freq`.
#[derive(Debug, Default)]
struct Phaser {
    stages: [PhaseStage; 6],
    feedback: [f32; 2],
}

impl Phaser {
    fn process(&mut self, x: [f32; 2], freq: f32, rate: f32) -> [f32; 2] {
        let t = (PI * freq / rate).tan();
        let a = (t - 1.0) / (t + 1.0);
        std::array::from_fn(|ch| {
            let mut y = x[ch] + self.feedback[ch] * 0.5;
            for stage in &mut self.stages {
                y = stage.process(ch, y, a);
            }
            self.feedback[ch] = y;
            (x[ch] + y) * 0.5
        })
    }
}

/// The Beat FX types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BeatFxKind {
    Delay,
    #[default]
    Echo,
    LowCutEcho,
    Spiral,
    Helix,
    Reverb,
    Flanger,
    Phaser,
    Filter,
    Trans,
    Pitch,
    Roll,
    MobiusSaw,
    MobiusTri,
}

impl BeatFxKind {
    pub const ALL: [BeatFxKind; 14] = [
        Self::Delay,
        Self::Echo,
        Self::LowCutEcho,
        Self::Spiral,
        Self::Helix,
        Self::Reverb,
        Self::Flanger,
        Self::Phaser,
        Self::Filter,
        Self::Trans,
        Self::Pitch,
        Self::Roll,
        Self::MobiusSaw,
        Self::MobiusTri,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Delay => "DELAY",
            Self::Echo => "ECHO",
            Self::LowCutEcho => "LOW CUT ECHO",
            Self::Spiral => "SPIRAL",
            Self::Helix => "HELIX",
            Self::Reverb => "REVERB",
            Self::Flanger => "FLANGER",
            Self::Phaser => "PHASER",
            Self::Filter => "FILTER",
            Self::Trans => "TRANS",
            Self::Pitch => "PITCH",
            Self::Roll => "ROLL",
            Self::MobiusSaw => "MOBIUS SAW",
            Self::MobiusTri => "MOBIUS TRI",
        }
    }

    /// Echo-like effects keep sounding after they are switched off.
    fn has_tail(self) -> bool {
        matches!(
            self,
            Self::Delay | Self::Echo | Self::LowCutEcho | Self::Spiral | Self::Helix | Self::Reverb
        )
    }
}

/// Where the Beat FX is inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FxTarget {
    Channel(usize),
    Master,
    Sampler,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeatFxParams {
    pub kind: BeatFxKind,
    pub on: bool,
    /// The depth knob, 0.0–1.0.
    pub depth: f32,
    /// The effect's beat length (beat × multiplier) in seconds.
    pub beat_secs: f32,
    pub target: FxTarget,
    /// Release FX: the dry signal is cut and the echo plays out.
    pub release: bool,
}

impl Default for BeatFxParams {
    fn default() -> Self {
        Self {
            kind: BeatFxKind::default(),
            on: false,
            depth: 0.5,
            beat_secs: 0.5,
            target: FxTarget::Master,
            release: false,
        }
    }
}

/// One Beat FX unit.
#[derive(Debug)]
pub struct BeatFx {
    rate: f32,
    params: BeatFxParams,
    delay: Delay,
    /// Delay time the echoes glide towards (Spiral glides slowly).
    delay_frames: f32,
    reverb: Reverb,
    pitch: PitchShifter,
    phasers: [Phaser; 2],
    flange: Delay,
    filter: Biquad,
    tone: Biquad,
    /// LFO phase 0.0–1.0, one cycle per beat length.
    lfo: f32,
    /// Roll: frames captured since it was switched on.
    roll_filled: f32,
    tick: u32,
    was_on: bool,
}

impl BeatFx {
    pub fn new(rate: u32) -> Self {
        let rate = rate as f32;
        Self {
            rate,
            params: BeatFxParams::default(),
            delay: Delay::new((rate * MAX_DELAY_SECS) as usize),
            delay_frames: rate * 0.5,
            reverb: Reverb::new(rate),
            pitch: PitchShifter::new(rate),
            phasers: [Phaser::default(), Phaser::default()],
            flange: Delay::new((rate * 0.02) as usize),
            filter: Biquad::default(),
            tone: Biquad::default(),
            lfo: 0.0,
            roll_filled: 0.0,
            tick: 0,
            was_on: false,
        }
    }

    pub fn params(&self) -> BeatFxParams {
        self.params
    }

    pub fn set(&mut self, params: BeatFxParams) {
        if params.kind != self.params.kind {
            // A new effect starts clean, without the old one's echoes.
            self.delay.clear();
            self.reverb.clear();
            self.flange.clear();
        }
        self.params = params;
    }

    /// Whether the unit changes the sound at all right now.
    pub fn active(&self) -> bool {
        self.params.on || self.params.kind.has_tail()
    }

    pub fn process(&mut self, x: [f32; 2]) -> [f32; 2] {
        let p = self.params;
        let on = p.on || p.release;
        if on && !self.was_on {
            self.roll_filled = 0.0;
            self.lfo = 0.0;
        }
        self.was_on = on;
        let beat = (p.beat_secs * self.rate).clamp(16.0, self.rate * (MAX_DELAY_SECS - 0.1));
        self.lfo = (self.lfo + 1.0 / beat) % 1.0;
        self.tick = self.tick.wrapping_add(1);
        let sweep = self.tick.is_multiple_of(SWEEP_EVERY);
        let d = p.depth.clamp(0.0, 1.0);
        let dry = if p.release { [0.0; 2] } else { x };
        // What goes into echo-like effects: nothing once switched off, so the tail fades.
        let send = if on && !p.release { x } else { [0.0; 2] };
        let mix = |a: [f32; 2], b: [f32; 2], wet: f32| {
            [a[0] * (1.0 - wet) + b[0] * wet, a[1] * (1.0 - wet) + b[1] * wet]
        };
        let add = |a: [f32; 2], b: [f32; 2], wet: f32| [a[0] + b[0] * wet, a[1] + b[1] * wet];

        match p.kind {
            BeatFxKind::Delay | BeatFxKind::Echo | BeatFxKind::LowCutEcho | BeatFxKind::Spiral => {
                let glide = if p.kind == BeatFxKind::Spiral { 0.00005 } else { 0.01 };
                self.delay_frames += (beat - self.delay_frames) * glide;
                let echo = self.delay.read(self.delay_frames);
                let (feedback, level) = match p.kind {
                    BeatFxKind::Delay => (0.15, d),
                    BeatFxKind::Spiral => (0.55 + 0.4 * d, 0.8),
                    _ => (0.35 + 0.4 * d, 0.6 + 0.4 * d),
                };
                let mut fed = [send[0] + echo[0] * feedback, send[1] + echo[1] * feedback];
                if p.kind == BeatFxKind::LowCutEcho {
                    if sweep || self.tick == 1 {
                        self.tone.high_pass(self.rate, 300.0 + 1200.0 * d, 0.7);
                    }
                    fed = [self.tone.process(0, fed[0]), self.tone.process(1, fed[1])];
                }
                self.delay.push(fed);
                add(dry, echo, level)
            }
            BeatFxKind::Helix => {
                // Holds one beat and keeps repeating it, fading new sound in less.
                let held = self.delay.read(beat);
                let input = if on { 1.0 - 0.9 * d } else { 0.0 };
                let feedback = if on { 0.97 } else { 0.6 };
                self.delay.push([
                    send[0] * input + held[0] * feedback,
                    send[1] * input + held[1] * feedback,
                ]);
                add([dry[0] * (1.0 - 0.5 * d), dry[1] * (1.0 - 0.5 * d)], held, d)
            }
            BeatFxKind::Reverb => {
                let wet = self.reverb.process(send, d);
                add(dry, wet, 0.3 + 0.7 * d)
            }
            _ if !on => x,
            BeatFxKind::Flanger => {
                let lfo = 0.5 - 0.5 * (2.0 * PI * self.lfo).cos();
                let tap = self.flange.read(self.rate * (0.0005 + 0.006 * lfo));
                self.flange.push([x[0] + tap[0] * 0.6, x[1] + tap[1] * 0.6]);
                mix(x, [(x[0] + tap[0]) * 0.5, (x[1] + tap[1]) * 0.5], d)
            }
            BeatFxKind::Phaser => {
                let lfo = 0.5 - 0.5 * (2.0 * PI * self.lfo).cos();
                let freq = 200.0 * 20f32.powf(lfo);
                mix(x, self.phasers[0].process(x, freq, self.rate), d)
            }
            BeatFxKind::MobiusSaw | BeatFxKind::MobiusTri => {
                // Two phasers half a cycle apart, each faded in and out over its
                // sweep: an endless rise (saw) or rise and fall (triangle).
                let shape = |phase: f32| match p.kind {
                    BeatFxKind::MobiusSaw => phase,
                    _ => 1.0 - (2.0 * phase - 1.0).abs(),
                };
                let (pa, pb) = (self.lfo, (self.lfo + 0.5) % 1.0);
                let a = self.phasers[0].process(x, 150.0 * 30f32.powf(shape(pa)), self.rate);
                let b = self.phasers[1].process(x, 150.0 * 30f32.powf(shape(pb)), self.rate);
                let (ga, gb) = ((PI * pa).sin().powi(2), (PI * pb).sin().powi(2));
                mix(x, [a[0] * ga + b[0] * gb, a[1] * ga + b[1] * gb], d)
            }
            BeatFxKind::Filter => {
                if sweep || self.tick == 1 {
                    let lfo = 0.5 - 0.5 * (2.0 * PI * self.lfo).cos();
                    self.filter.low_pass(self.rate, 150.0 * 60f32.powf(lfo), 1.0 + 4.0 * d);
                }
                let y = [self.filter.process(0, x[0]), self.filter.process(1, x[1])];
                mix(x, y, 0.3 + 0.7 * d)
            }
            BeatFxKind::Trans => {
                // Cuts the sound for the second half of each beat, with 2 ms edges.
                let edge = 0.002 * self.rate / beat;
                let gate = if self.lfo < 0.5 {
                    (self.lfo / edge).min(1.0).min((0.5 - self.lfo) / edge)
                } else {
                    0.0
                };
                let gain = 1.0 - d * (1.0 - gate);
                [x[0] * gain, x[1] * gain]
            }
            BeatFxKind::Pitch => {
                let ratio = 2f32.powf(d);
                self.pitch.process(x, ratio)
            }
            BeatFxKind::Roll => {
                // Records one beat after switching on, then repeats it.
                let looped = if self.roll_filled < beat {
                    self.roll_filled += 1.0;
                    self.delay.push(x);
                    x
                } else {
                    let repeat = self.delay.read(beat);
                    self.delay.push(repeat);
                    repeat
                };
                mix(x, looped, 0.5 + 0.5 * d)
            }
        }
    }
}

/// The Color FX types; the knob of each channel controls the one selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorFxKind {
    #[default]
    Filter,
    Space,
    DubEcho,
    Crush,
    Noise,
    Pitch,
}

impl ColorFxKind {
    pub const ALL: [ColorFxKind; 6] =
        [Self::Filter, Self::Space, Self::DubEcho, Self::Crush, Self::Noise, Self::Pitch];

    pub fn label(self) -> &'static str {
        match self {
            Self::Filter => "FILTER",
            Self::Space => "SPACE",
            Self::DubEcho => "DUB ECHO",
            Self::Crush => "CRUSH",
            Self::Noise => "NOISE",
            Self::Pitch => "PITCH",
        }
    }
}

/// One channel's Color FX. The knob runs −1.0 … 1.0 with no effect at the
/// center; most types low-pass to the left and high-pass to the right.
#[derive(Debug)]
pub struct ColorFx {
    rate: f32,
    kind: ColorFxKind,
    position: f32,
    filter: Filter,
    tone: Biquad,
    reverb: Reverb,
    delay: Delay,
    pitch: PitchShifter,
    held: [f32; 2],
    hold_count: f32,
    noise: u32,
}

impl ColorFx {
    pub fn new(rate: u32) -> Self {
        let rate = rate as f32;
        Self {
            rate,
            kind: ColorFxKind::Filter,
            position: 0.0,
            filter: Filter::new(rate),
            tone: Biquad::default(),
            reverb: Reverb::new(rate),
            delay: Delay::new((rate * 1.0) as usize),
            pitch: PitchShifter::new(rate),
            held: [0.0; 2],
            hold_count: 0.0,
            noise: 0x1234_5678,
        }
    }

    pub fn set_kind(&mut self, kind: ColorFxKind) {
        if kind != self.kind {
            self.kind = kind;
            self.delay.clear();
            self.reverb.clear();
            self.set_position(self.position);
        }
    }

    pub fn set_position(&mut self, position: f32) {
        self.position = position.clamp(-1.0, 1.0);
        self.filter.set(self.position);
        // The tone filter colours the effect: darker left, thinner right.
        let amount = self.position.abs();
        if self.position < 0.0 {
            self.tone.low_pass(self.rate, 12_000.0 * (300.0f32 / 12_000.0).powf(amount), 0.8);
        } else {
            self.tone.high_pass(self.rate, 40.0 * (3_000.0f32 / 40.0).powf(amount), 0.8);
        }
    }

    fn white_noise(&mut self) -> f32 {
        // xorshift32
        self.noise ^= self.noise << 13;
        self.noise ^= self.noise >> 17;
        self.noise ^= self.noise << 5;
        self.noise as f32 / u32::MAX as f32 * 2.0 - 1.0
    }

    pub fn process(&mut self, x: [f32; 2]) -> [f32; 2] {
        let amount = self.position.abs();
        let active = amount > 0.02;
        match self.kind {
            ColorFxKind::Filter => self.filter.process(x),
            ColorFxKind::Space => {
                let send = if active { x } else { [0.0; 2] };
                let wet = self.reverb.process(send, 0.4 + 0.6 * amount);
                let wet = [self.tone.process(0, wet[0]), self.tone.process(1, wet[1])];
                [x[0] + wet[0] * amount * 1.5, x[1] + wet[1] * amount * 1.5]
            }
            ColorFxKind::DubEcho => {
                // A dotted eighth at 120 BPM, with the tone filter in the loop.
                let echo = self.delay.read(self.rate * 0.375);
                let send = if active { x } else { [0.0; 2] };
                let feedback = 0.45 + 0.4 * amount;
                let fed = [send[0] + echo[0] * feedback, send[1] + echo[1] * feedback];
                self.delay.push([self.tone.process(0, fed[0]), self.tone.process(1, fed[1])]);
                [x[0] + echo[0] * 0.8, x[1] + echo[1] * 0.8]
            }
            _ if !active => x,
            ColorFxKind::Crush => {
                // Fewer bits and a lower sample rate as the knob turns.
                let hold = 1.0 + amount * 30.0;
                self.hold_count += 1.0;
                if self.hold_count >= hold {
                    self.hold_count -= hold;
                    let levels = 2f32.powf(12.0 - 9.0 * amount);
                    self.held = x.map(|s| (s * levels).round() / levels);
                }
                [self.tone.process(0, self.held[0]), self.tone.process(1, self.held[1])]
            }
            ColorFxKind::Noise => {
                let n = self.white_noise() * amount * 0.3;
                let filtered = self.filter.process([x[0] + n, x[1] + n]);
                [filtered[0], filtered[1]]
            }
            ColorFxKind::Pitch => self.pitch.process(x, 2f32.powf(self.position)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn tone(i: usize, hz: f32) -> [f32; 2] {
        let v = (2.0 * PI * hz * i as f32 / RATE as f32).sin() * 0.5;
        [v, v]
    }

    #[test]
    fn echo_repeats_after_one_beat_and_fades_after_switching_off() {
        let mut fx = BeatFx::new(RATE);
        fx.set(BeatFxParams {
            kind: BeatFxKind::Echo,
            on: true,
            depth: 0.5,
            beat_secs: 0.1,
            ..Default::default()
        });
        // Let the delay time settle, then send one click.
        for _ in 0..RATE {
            fx.process([0.0; 2]);
        }
        let mut out = vec![fx.process([1.0, 1.0])];
        fx.set(BeatFxParams { on: false, ..fx.params() });
        out.extend((1..RATE as usize).map(|_| fx.process([0.0; 2])));
        let peak =
            |from: usize, to: usize| out[from..to].iter().map(|s| s[0].abs()).fold(0.0, f32::max);
        assert!(peak(4790, 4810) > 0.5, "first echo at 100 ms");
        assert!(peak(100, 4700) < 1e-3, "nothing between");
        assert!(peak(9590, 9610) > 0.2, "second echo");
        assert!(peak(40_000, 48_000) < 0.05, "the tail fades");
    }

    #[test]
    fn roll_repeats_what_it_recorded() {
        let mut fx = BeatFx::new(RATE);
        fx.set(BeatFxParams {
            kind: BeatFxKind::Roll,
            on: true,
            depth: 1.0,
            beat_secs: 0.01,
            ..Default::default()
        });
        let out: Vec<_> = (0..2000).map(|i| fx.process([i as f32, 0.0])).collect();
        // 480 frames are recorded, then repeated.
        assert_eq!(out[480][0], 0.0);
        assert_eq!(out[1000][0], 40.0);
    }

    #[test]
    fn trans_cuts_half_of_each_beat() {
        let mut fx = BeatFx::new(RATE);
        fx.set(BeatFxParams {
            kind: BeatFxKind::Trans,
            on: true,
            depth: 1.0,
            beat_secs: 0.1,
            ..Default::default()
        });
        let out: Vec<_> = (0..4800).map(|_| fx.process([1.0, 1.0])[0]).collect();
        assert!(out[1200] > 0.99 && out[3600] < 1e-6);
    }

    #[test]
    fn pitch_shifter_raises_the_frequency() {
        let mut shifter = PitchShifter::new(RATE as f32);
        let out: Vec<f32> =
            (0..RATE as usize).map(|i| shifter.process(tone(i, 220.0), 2.0)[0]).collect();
        // Strongest of a few candidate frequencies, measured by correlation.
        let power = |hz: f32| {
            let (mut s, mut c) = (0.0, 0.0);
            for (i, v) in out[4800..].iter().enumerate() {
                let w = 2.0 * PI * hz * i as f32 / RATE as f32;
                s += v * w.sin();
                c += v * w.cos();
            }
            s * s + c * c
        };
        assert!(power(440.0) > 10.0 * power(220.0));
    }

    #[test]
    fn every_effect_stays_finite_and_bounded() {
        for kind in BeatFxKind::ALL {
            let mut fx = BeatFx::new(RATE);
            fx.set(BeatFxParams {
                kind,
                on: true,
                depth: 1.0,
                beat_secs: 0.05,
                ..Default::default()
            });
            for i in 0..RATE as usize {
                let y = fx.process(tone(i, 330.0));
                assert!(y.iter().all(|v| v.is_finite() && v.abs() < 8.0), "{kind:?}: {y:?}");
            }
        }
        for kind in ColorFxKind::ALL {
            let mut fx = ColorFx::new(RATE);
            fx.set_kind(kind);
            for position in [-1.0, -0.5, 0.0, 0.5, 1.0] {
                fx.set_position(position);
                for i in 0..RATE as usize / 4 {
                    let y = fx.process(tone(i, 330.0));
                    assert!(y.iter().all(|v| v.is_finite() && v.abs() < 8.0), "{kind:?}: {y:?}");
                }
            }
        }
    }

    #[test]
    fn color_fx_center_is_transparent() {
        for kind in ColorFxKind::ALL {
            let mut fx = ColorFx::new(RATE);
            fx.set_kind(kind);
            fx.set_position(0.0);
            for i in 0..1000 {
                let x = tone(i, 330.0);
                assert_eq!(fx.process(x), x, "{kind:?}");
            }
        }
    }
}
