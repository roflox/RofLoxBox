//! A three-band DJ equalizer built from biquad filters (RBJ cookbook).

use std::f32::consts::PI;

#[derive(Debug, Clone, Copy, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, Clone, Copy, Default)]
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
#[derive(Debug, Clone, Copy)]
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

/// One-knob DJ filter: low-pass left of center, high-pass right of center.
#[derive(Debug, Clone, Copy)]
pub struct Filter {
    rate: f32,
    biquad: Biquad,
    active: bool,
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

impl Filter {
    pub fn new(rate: f32) -> Self {
        Self { rate, biquad: Biquad::default(), active: false }
    }

    /// `position` −1.0 … 1.0; values near 0 bypass the filter.
    pub fn set(&mut self, position: f32) {
        let x = position.clamp(-1.0, 1.0);
        self.active = x.abs() > 0.02;
        let amount = (x.abs() - 0.02) / 0.98;
        if x < 0.0 {
            // 20 kHz down to 60 Hz, exponentially.
            self.biquad.low_pass(self.rate, 20_000.0 * (60.0f32 / 20_000.0).powf(amount), 0.9);
        } else {
            // 20 Hz up to 8 kHz.
            self.biquad.high_pass(self.rate, 20.0 * (8_000.0f32 / 20.0).powf(amount), 0.9);
        }
    }

    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 2] {
        if !self.active {
            return frame;
        }
        [self.biquad.process(0, frame[0]), self.biquad.process(1, frame[1])]
    }
}

#[cfg(test)]
mod eq_tests {
    use super::*;

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
