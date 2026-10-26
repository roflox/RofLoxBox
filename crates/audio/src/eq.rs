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

/// Low shelf at 200 Hz, mid peak at 1 kHz, high shelf at 5 kHz.
/// Gains are in dB; −26 dB and below act as a kill.
#[derive(Debug, Clone, Copy)]
pub struct Equalizer {
    rate: f32,
    bands: [Biquad; 3],
    gains: [f32; 3],
}

pub const EQ_MIN_DB: f32 = -26.0;
pub const EQ_MAX_DB: f32 = 6.0;

impl Equalizer {
    pub fn new(rate: f32) -> Self {
        let mut eq = Self { rate, bands: [Biquad::default(); 3], gains: [1.0; 3] };
        for band in 0..3 {
            eq.set_gain(band, 0.0);
        }
        eq
    }

    pub fn set_gain(&mut self, band: usize, gain_db: f32) {
        let gain_db = gain_db.clamp(EQ_MIN_DB - 40.0, EQ_MAX_DB);
        // Push a full kill further down so the band really disappears.
        let effective = if gain_db <= EQ_MIN_DB { -60.0 } else { gain_db };
        self.gains[band] = effective;
        match band {
            0 => self.bands[0].low_shelf(self.rate, 200.0, effective),
            1 => self.bands[1].peaking(self.rate, 1000.0, 0.7, effective),
            _ => self.bands[2].high_shelf(self.rate, 5000.0, effective),
        }
    }

    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 2] {
        if self.gains.iter().all(|&g| g == 0.0) {
            return frame;
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
