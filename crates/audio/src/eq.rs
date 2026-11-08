//! A three-band DJ equalizer built from biquad filters (RBJ cookbook).

use std::f32::consts::PI;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    /// Per-channel state `[x1, x2, y1, y2]`.
    state: [[f32; 4]; 2],
}

impl Biquad {
    pub(crate) fn process(&mut self, channel: usize, x: f32) -> f32 {
        let [x1, x2, y1, y2] = self.state[channel];
        let y = self.b[0] * x + self.b[1] * x1 + self.b[2] * x2 - self.a[0] * y1 - self.a[1] * y2;
        self.state[channel] = [x, x1, y, y1];
        y
    }

    fn set(&mut self, b: [f32; 3], a: [f32; 3]) {
        self.b = [b[0] / a[0], b[1] / a[0], b[2] / a[0]];
        self.a = [a[1] / a[0], a[2] / a[0]];
    }

    fn low_shelf(&mut self, rate: f32, freq: f32, gain_db: f32) {
        let a = 10f32.powf(gain_db / 40.0);
        let w = 2.0 * PI * freq / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / 2.0 * 2f32.sqrt();
        let sq = 2.0 * a.sqrt() * alpha;
        self.set(
            [
                a * ((a + 1.0) - (a - 1.0) * cos + sq),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - sq),
            ],
            [
                (a + 1.0) + (a - 1.0) * cos + sq,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - sq,
            ],
        );
    }

    fn high_shelf(&mut self, rate: f32, freq: f32, gain_db: f32) {
        let a = 10f32.powf(gain_db / 40.0);
        let w = 2.0 * PI * freq / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / 2.0 * 2f32.sqrt();
        let sq = 2.0 * a.sqrt() * alpha;
        self.set(
            [
                a * ((a + 1.0) + (a - 1.0) * cos + sq),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - sq),
            ],
            [
                (a + 1.0) - (a - 1.0) * cos + sq,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - sq,
            ],
        );
    }

    fn peaking(&mut self, rate: f32, freq: f32, q: f32, gain_db: f32) {
        let a = 10f32.powf(gain_db / 40.0);
        let w = 2.0 * PI * freq / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        self.set(
            [1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a],
            [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a],
        );
    }
}

/// How the three EQ knobs work and at which frequencies.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqSettings {
    /// Isolator: the signal is split into three bands that are turned up and
    /// down (fully left removes a band). EQ: shelving / peaking filters that
    /// cut down to −26 dB.
    pub isolator: bool,
    /// EQ mode: low shelf, mid peak and high shelf frequencies in Hz.
    pub low_hz: f32,
    pub mid_hz: f32,
    pub high_hz: f32,
    /// Isolator mode: where low meets mid and mid meets high, in Hz.
    pub split_low_hz: f32,
    pub split_high_hz: f32,
}

impl Default for EqSettings {
    /// The figures Pioneer DJ publishes for its club mixers (DJM-900NXS2:
    /// EQ +6 / −26 dB at 70 Hz, 1 kHz and 13 kHz). The isolator split points
    /// are our own choice.
    fn default() -> Self {
        Self {
            isolator: true,
            low_hz: 70.0,
            mid_hz: 1_000.0,
            high_hz: 13_000.0,
            split_low_hz: 250.0,
            split_high_hz: 4_000.0,
        }
    }
}

/// A Linkwitz-Riley crossover of 4th order (two Butterworth stages per
/// side): low + high add up to an all-pass, so the bands sum back flat.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct Crossover {
    low: [Biquad; 2],
    high: [Biquad; 2],
}

impl Crossover {
    fn set(&mut self, rate: f32, freq: f32) {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        for b in &mut self.low {
            b.low_pass(rate, freq, q);
        }
        for b in &mut self.high {
            b.high_pass(rate, freq, q);
        }
    }

    fn split(&mut self, ch: usize, x: f32) -> (f32, f32) {
        let low = self.low[0].process(ch, x);
        let low = self.low[1].process(ch, low);
        let high = self.high[0].process(ch, x);
        let high = self.high[1].process(ch, high);
        (low, high)
    }
}

/// Three-band DJ EQ or isolator (see [`EqSettings`]). Gains are in dB;
/// [`EQ_MIN_DB`] and below remove the band as an isolator, and cut by
/// 26 dB as an EQ.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Equalizer {
    rate: f32,
    settings: EqSettings,
    /// EQ mode filters.
    bands: [Biquad; 3],
    /// Isolator mode: split at the low point, then the upper part at the
    /// high point; the low band goes through the same high split as an
    /// all-pass so all three line up in phase.
    split_low: Crossover,
    split_high: Crossover,
    phase_match: Crossover,
    gains: [f32; 3],
}

pub const EQ_MIN_DB: f32 = -26.0;
pub const EQ_MAX_DB: f32 = 6.0;

impl Equalizer {
    pub fn new(rate: f32) -> Self {
        let mut eq = Self {
            rate,
            settings: EqSettings::default(),
            bands: [Biquad::default(); 3],
            split_low: Crossover::default(),
            split_high: Crossover::default(),
            phase_match: Crossover::default(),
            gains: [0.0; 3],
        };
        eq.configure(EqSettings::default());
        eq
    }

    pub fn configure(&mut self, settings: EqSettings) {
        let nyquist = self.rate * 0.45;
        let clamp = |f: f32| f.clamp(20.0, nyquist);
        self.settings = EqSettings {
            low_hz: clamp(settings.low_hz),
            mid_hz: clamp(settings.mid_hz),
            high_hz: clamp(settings.high_hz),
            split_low_hz: clamp(settings.split_low_hz),
            split_high_hz: clamp(settings.split_high_hz.max(settings.split_low_hz * 1.5)),
            ..settings
        };
        let s = self.settings;
        self.split_low.set(self.rate, s.split_low_hz);
        self.split_high.set(self.rate, s.split_high_hz);
        self.phase_match.set(self.rate, s.split_high_hz);
        for band in 0..3 {
            self.set_gain(band, self.gains[band]);
        }
    }

    /// A band's gain in dB, as last set.
    pub fn gain(&self, band: usize) -> f32 {
        self.gains[band.min(2)]
    }

    pub fn set_gain(&mut self, band: usize, gain_db: f32) {
        let gain_db = gain_db.clamp(EQ_MIN_DB - 40.0, EQ_MAX_DB);
        self.gains[band] = gain_db;
        // As an EQ the cut stops at −26 dB.
        let eq_db = gain_db.max(EQ_MIN_DB);
        let s = self.settings;
        match band {
            0 => self.bands[0].low_shelf(self.rate, s.low_hz, eq_db),
            1 => self.bands[1].peaking(self.rate, s.mid_hz, 0.7, eq_db),
            _ => self.bands[2].high_shelf(self.rate, s.high_hz, eq_db),
        }
    }

    /// Linear level of a band in isolator mode (0 when fully cut).
    fn isolator_gain(&self, band: usize) -> f32 {
        let db = self.gains[band];
        if db <= EQ_MIN_DB { 0.0 } else { 10f32.powf(db / 20.0) }
    }

    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 2] {
        if self.gains.iter().all(|&g| g == 0.0) {
            return frame;
        }
        if self.settings.isolator {
            let g = [self.isolator_gain(0), self.isolator_gain(1), self.isolator_gain(2)];
            return std::array::from_fn(|ch| {
                let (low, rest) = self.split_low.split(ch, frame[ch]);
                let (mid, high) = self.split_high.split(ch, rest);
                let (a, b) = self.phase_match.split(ch, low);
                (a + b) * g[0] + mid * g[1] + high * g[2]
            });
        }
        let mut out = frame;
        for band in &mut self.bands {
            for (channel, sample) in out.iter_mut().enumerate() {
                *sample = band.process(channel, *sample);
            }
        }
        out
    }
}

impl Biquad {
    pub(crate) fn low_pass(&mut self, rate: f32, freq: f32, q: f32) {
        let w = 2.0 * PI * freq / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        self.set(
            [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        );
    }

    pub(crate) fn high_pass(&mut self, rate: f32, freq: f32, q: f32) {
        let w = 2.0 * PI * freq / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        self.set(
            [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        );
    }
}

/// One-knob DJ filter: low-pass left of center, high-pass right of center.
///
/// A state-variable filter (Simper's trapezoidal SVF): its low- and
/// high-pass come from the same state and stay stable while the cutoff
/// moves, so the knob can sweep and cross the center without clicks. The
/// knob glides to its new place over a few milliseconds (no steps from a
/// 7-bit controller), and near the center the filter fades into the dry
/// signal instead of switching off.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Filter {
    rate: f32,
    /// Where the knob is, −1.0 … 1.0.
    target: f32,
    /// Where the filter is, gliding to `target`.
    position: f32,
    /// Per-channel state `[ic1eq, ic2eq]`.
    state: [[f32; 2]; 2],
    /// The coefficients for `position`: `[a1, a2, a3, k]`.
    coefficients: [f32; 4],
}

impl Default for Filter {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

/// Resonance at the cutoff, as before (Q 0.9).
const FILTER_Q: f32 = 0.9;
/// How long the filter takes to follow the knob (to about 63 %).
const FILTER_GLIDE_SECS: f32 = 0.008;
/// Knob values this close to the center fade into the dry signal.
const FILTER_DEAD_ZONE: f32 = 0.02;

impl Filter {
    pub fn new(rate: f32) -> Self {
        let mut filter =
            Self { rate, target: 0.0, position: 0.0, state: [[0.0; 2]; 2], coefficients: [0.0; 4] };
        filter.update();
        filter
    }

    /// `position` −1.0 … 1.0; the center leaves the sound alone.
    pub fn set(&mut self, position: f32) {
        self.target = position.clamp(-1.0, 1.0);
    }

    /// The cutoff for a knob position: 20 kHz down to 60 Hz to the left,
    /// 20 Hz up to 8 kHz to the right, both exponential.
    fn cutoff(&self, x: f32) -> f32 {
        let amount = ((x.abs() - FILTER_DEAD_ZONE) / (1.0 - FILTER_DEAD_ZONE)).max(0.0);
        let hz = if x < 0.0 {
            20_000.0 * (60.0f32 / 20_000.0).powf(amount)
        } else {
            20.0 * (8_000.0f32 / 20.0).powf(amount)
        };
        hz.min(self.rate * 0.45)
    }

    fn update(&mut self) {
        let g = (PI * self.cutoff(self.position) / self.rate).tan();
        let k = 1.0 / FILTER_Q;
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        self.coefficients = [a1, a2, g * a2, k];
    }

    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 2] {
        if self.position != self.target {
            let step = 1.0 - (-1.0 / (FILTER_GLIDE_SECS * self.rate)).exp();
            self.position += (self.target - self.position) * step;
            if (self.target - self.position).abs() < 1e-4 {
                self.position = self.target;
            }
            self.update();
        }
        let [a1, a2, a3, k] = self.coefficients;
        let low = self.position < 0.0;
        // 0 at the center, 1 from the edge of the dead zone on.
        let wet = (self.position.abs() / FILTER_DEAD_ZONE).min(1.0);
        let mut out = frame;
        for (channel, sample) in out.iter_mut().enumerate() {
            let [ic1, ic2] = self.state[channel];
            let v0 = *sample;
            let v3 = v0 - ic2;
            let v1 = a1 * ic1 + a2 * v3;
            let v2 = ic2 + a2 * ic1 + a3 * v3;
            self.state[channel] = [2.0 * v1 - ic1, 2.0 * v2 - ic2];
            let filtered = if low { v2 } else { v0 - k * v1 - v2 };
            *sample = v0 + (filtered - v0) * wet;
        }
        out
    }
}

#[cfg(test)]
mod eq_tests {
    use super::*;

    /// The largest jump between neighbouring samples of a slow sine sent
    /// through the filter while the knob jumps across the center and back.
    #[test]
    fn filter_does_not_click_when_switched_fast() {
        let rate = 48_000.0;
        let mut filter = Filter::new(rate);
        let mut last = 0.0f32;
        let mut worst = 0.0f32;
        for i in 0..48_000 {
            // Hard left, hard right, center, every 20 ms.
            if i % 960 == 0 {
                filter.set([-1.0, 1.0, 0.0, 0.6, -0.4][(i / 960) % 5]);
            }
            let x = (2.0 * PI * 100.0 * i as f32 / rate).sin() * 0.5;
            let y = filter.process([x, x])[0];
            worst = worst.max((y - last).abs());
            last = y;
        }
        // A 100 Hz sine at 0.5 moves at most 0.0066 per sample.
        assert!(worst < 0.05, "jump of {worst}");
    }

    #[test]
    fn filter_center_is_dry_and_ends_cut() {
        let rate = 48_000.0;
        let level = |position: f32, hz: f32| {
            let mut filter = Filter::new(rate);
            filter.set(position);
            let (mut input, mut output) = (0f32, 0f32);
            for i in 0..48_000 {
                let x = (2.0 * PI * hz * i as f32 / rate).sin();
                let y = filter.process([x, x])[0];
                if i > 24_000 {
                    input += x * x;
                    output += y * y;
                }
            }
            10.0 * (output / input).log10()
        };
        assert!(level(0.0, 1_000.0).abs() < 0.01);
        assert!(level(-1.0, 5_000.0) < -30.0, "low-pass cuts highs");
        assert!(level(1.0, 200.0) < -30.0, "high-pass cuts lows");
        assert!(level(-1.0, 30.0) > -3.0, "low-pass keeps lows");
    }

    /// Level of a sine at `hz` after the EQ, relative to before, in dB.
    fn response(eq: &mut Equalizer, hz: f32) -> f32 {
        let rate = 48_000.0;
        let (mut input, mut output) = (0f32, 0f32);
        for i in 0..48_000 {
            let x = (2.0 * std::f32::consts::PI * hz * i as f32 / rate).sin();
            let y = eq.process([x, x])[0];
            if i > 24_000 {
                input += x * x;
                output += y * y;
            }
        }
        10.0 * (output / input).log10()
    }

    #[test]
    fn isolator_sums_flat_and_kills_bands() {
        let mut eq = Equalizer::new(48_000.0);
        eq.set_gain(1, -0.001); // anything not exactly flat goes through the filters
        for hz in [50.0, 250.0, 1_000.0, 4_000.0, 12_000.0] {
            let db = response(&mut eq, hz);
            assert!(db.abs() < 0.3, "{hz} Hz: {db} dB");
        }
        eq.set_gain(0, EQ_MIN_DB);
        assert!(response(&mut eq, 40.0) < -30.0, "low band removed");
        assert!(response(&mut eq, 1_000.0).abs() < 1.0, "mids stay");
    }

    #[test]
    fn eq_mode_cuts_26_db_at_its_frequencies() {
        let mut eq = Equalizer::new(48_000.0);
        eq.configure(EqSettings { isolator: false, ..EqSettings::default() });
        eq.set_gain(1, EQ_MIN_DB);
        let db = response(&mut eq, 1_000.0);
        assert!((db + 26.0).abs() < 1.0, "{db}");
        assert!(response(&mut eq, 100.0) > -3.0, "lows untouched");
    }
}
