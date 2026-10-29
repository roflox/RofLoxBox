//! Waveforms in the forms rekordbox's analysis files hold.
//!
//! The audio is mixed to mono, split into bands with second-order
//! Butterworth filters, and measured in columns of 1/150 s. The colour
//! detail waveform (`PWV5`) uses three bands: below 100 Hz, 300 Hz – 3 kHz and
//! above 1.5 kHz. Each column's colour comes from the band levels, its height
//! from the column's peak relative to the loudest column, squared.

/// Columns per second of the detail waveforms.
pub const COLUMNS_PER_SECOND: u32 = 150;
/// The measurements behind them are taken every millisecond.
const RECORDS_PER_SECOND: usize = 1000;

/// A second-order Butterworth low- or high-pass (Q = 1/√2), direct form I.
#[derive(Debug, Clone)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad {
    fn new(rate: u32, cutoff: f64, high_pass: bool) -> Self {
        let w = std::f64::consts::TAU * cutoff / f64::from(rate);
        let (sin, cos) = w.sin_cos();
        let alpha = sin * std::f64::consts::FRAC_1_SQRT_2;
        let a0 = 1.0 + alpha;
        let b = if high_pass {
            let b0 = (1.0 + cos) * 0.5 / a0;
            [b0, -(1.0 + cos) / a0, b0]
        } else {
            let b0 = (1.0 - cos) * 0.5 / a0;
            [b0, (1.0 - cos) / a0, b0]
        };
        Self { b, a: [-2.0 * cos / a0, (1.0 - alpha) / a0], x: [0.0; 2], y: [0.0; 2] }
    }

    fn low(rate: u32, cutoff: f64) -> Self {
        Self::new(rate, cutoff, false)
    }

    fn high(rate: u32, cutoff: f64) -> Self {
        Self::new(rate, cutoff, true)
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// One sample of the mono signal that is measured: the two channels'
/// average, or — when their levels are almost the same — the louder one, so
/// that opposite phases do not cancel out. Samples are full scale ±1.
fn mono(left: f32, right: f32) -> f64 {
    // 16-bit samples as value / 32767 (decoders give / 32768).
    const SCALE: f64 = 32768.0 / 32767.0;
    let (l, r) = (f64::from(left) * SCALE, f64::from(right) * SCALE);
    if (l.abs() - r.abs()).abs() >= 0.001 {
        (l + r) * 0.5
    } else if l.abs() <= r.abs() {
        r
    } else {
        l
    }
}

/// The measurements of one column used by the colour waveform: the mono
/// signal's extremes and the band peaks, all ±1 full scale.
#[derive(Debug, Clone, Copy, Default)]
struct ColorColumn {
    max: f64,
    min: f64,
    low: f64,
    mid: f64,
    high: f64,
}

/// The colour waveform's measurements every millisecond: the mono
/// signal's extremes and the peaks of its three bands.
fn color_records(samples: &[f32], rate: u32) -> Vec<ColorColumn> {
    let frames = samples.len() / 2;
    let count = (frames * RECORDS_PER_SECOND).div_ceil(rate as usize);
    let mut low = Biquad::low(rate, 100.0);
    let mut mid = [Biquad::high(rate, 300.0), Biquad::low(rate, 3000.0)];
    let mut high = Biquad::high(rate, 1500.0);
    let mut records = Vec::with_capacity(count);
    let mut frame = 0;
    for index in 0..count {
        let end = ((index + 1) * rate as usize).div_ceil(RECORDS_PER_SECOND).min(frames);
        let mut record = ColorColumn { max: f64::MIN, min: f64::MAX, ..ColorColumn::default() };
        while frame < end {
            let x = mono(samples[2 * frame], samples[2 * frame + 1]);
            let above_300 = mid[0].process(x);
            record.max = record.max.max(x);
            record.min = record.min.min(x);
            record.low = record.low.max(low.process(x).abs());
            record.mid = record.mid.max(mid[1].process(above_300).abs());
            record.high = record.high.max(high.process(x).abs());
            frame += 1;
        }
        records.push(record);
    }
    records
}

/// 16-bit full scale, as the measurements are kept.
fn to_i16(value: f64) -> f32 {
    (value * 32768.0).clamp(-32768.0, 32767.0).trunc() as f32
}

/// The colour for band levels (16-bit scale): each channel relative
/// to the strongest band, red and green pulled back while the highs are
/// weak, green pulled back as red rises, blue lifted.
pub fn zoom_colour(low: f32, mid: f32, high: f32) -> [u8; 3] {
    let strongest = low.max(mid).max(high);
    if strongest == 0.0 {
        return [255; 3];
    }
    let scale = 255.0 / strongest;
    let blue = (high * scale).clamp(0.0, 255.0);
    let (mut red, mut green) = (low * scale, mid * scale);
    if blue < 64.0 {
        let shared = green * 0.001_307_189_7 * red;
        let shared = shared - shared * blue / 64.0;
        red -= shared;
        green -= shared;
    }
    let brighter = red.max(blue).min(128.0);
    let green = (green - brighter * green * 0.002_343_75).clamp(0.0, 255.0);
    let red = red.clamp(0.0, 255.0);
    let blue = (blue * 1.3).min(255.0);
    [red as u8, green as u8, blue as u8]
}

/// The colour detail waveform (`PWV5`): per column 3 bits each of red,
/// green and blue and 5 bits of height, as stored in the file
/// (`rrrgggbbbhhhhh00`, big-endian).
pub fn color_detail(samples: &[f32], rate: u32) -> Vec<u16> {
    let records = color_records(samples, rate);
    let window = |i: usize, radius: usize, of: fn(&ColorColumn) -> f64| {
        records[i.saturating_sub(radius)..(i + radius + 1).min(records.len())]
            .iter()
            .map(|r| to_i16(of(r)))
            .fold(0.0, f32::max)
    };
    // Every millisecond goes into the column it falls in (the counts are
    // rounded up, which sets how they line up): lows windowed over ±12 ms
    // and mids over ±1 ms so they do not flicker, then the maxima of the
    // band levels and the signal's extremes per column.
    let columns = (samples.len() / 2 * COLUMNS_PER_SECOND as usize).div_ceil(rate as usize);
    let mut merged = vec![[0.0f32, 0.0, 0.0, f32::MIN, f32::MAX]; columns];
    for (i, record) in records.iter().enumerate() {
        let Some(m) = merged.get_mut(i * columns / records.len()) else { break };
        m[0] = m[0].max(window(i, 12, |r| r.low));
        m[1] = m[1].max(window(i, 1, |r| r.mid));
        m[2] = m[2].max(to_i16(record.high));
        m[3] = m[3].max(to_i16(record.max));
        m[4] = m[4].min(to_i16(record.min));
    }
    let mut heights = Vec::with_capacity(columns);
    let mut colours = Vec::with_capacity(columns);
    for [low, mid, high, max, min] in merged {
        let full = zoom_colour(low, mid, high);
        let mut rgb = full.map(|c| c >> 5);
        // A column too dark to see shows its strongest band at least.
        if full.iter().all(|&c| c < 0x20) {
            let top = *full.iter().max().expect("three channels");
            for (bits, value) in rgb.iter_mut().zip(full) {
                if value == top {
                    *bits = 1;
                }
            }
        }
        colours.push(rgb);
        heights.push(max.abs().max(min.abs()).min(32767.0));
    }
    // Heights relative to the loudest column, squared, in 5 bits.
    let loudest = heights.iter().copied().fold(0.0, f32::max);
    colours
        .into_iter()
        .zip(heights)
        .map(|([r, g, b], h)| {
            let relative = if loudest > 0.0 {
                (f64::from(h) * 32767.0 / f64::from(loudest)).trunc()
            } else {
                0.0
            };
            let height = ((relative * relative * 2.932_745_123_302_746_6e-8) as u16) & 0x1f;
            u16::from(r) << 13 | u16::from(g) << 10 | u16::from(b) << 7 | height << 2
        })
        .collect()
}

/// Record A: the overview's measurements every millisecond (16-bit scale):
/// the mono signal's extremes, the peak below 400 Hz, and the peaks of the
/// bands below 200 Hz, 200 Hz – 2 kHz and above 2 kHz, each weighted by the
/// share of samples in which that band was the strongest.
fn overview_records(samples: &[f32], rate: u32) -> Vec<[f32; 6]> {
    let frames = samples.len() / 2;
    let count = (frames * RECORDS_PER_SECOND).div_ceil(rate as usize);
    let mut below_400 = Biquad::low(rate, 400.0);
    let mut low = [Biquad::low(rate, 200.0), Biquad::low(rate, 200.0)];
    let mut mid = [
        Biquad::high(rate, 200.0),
        Biquad::high(rate, 200.0),
        Biquad::low(rate, 2000.0),
        Biquad::low(rate, 2000.0),
    ];
    let mut high = [Biquad::high(rate, 2000.0), Biquad::high(rate, 2000.0)];
    let mut records = Vec::with_capacity(count);
    let mut frame = 0;
    for index in 0..count {
        let end = ((index + 1) * rate as usize).div_ceil(RECORDS_PER_SECOND).min(frames);
        let len = end.saturating_sub(frame).max(1) as f64;
        let (mut max, mut min) = (f64::MIN, f64::MAX);
        let mut peaks = [0.0f64; 4];
        let mut wins = [0u32; 3];
        while frame < end {
            let x = mono(samples[2 * frame], samples[2 * frame + 1]);
            let b = below_400.process(x);
            let l = low.iter_mut().fold(x, |v, f| f.process(v));
            let m = mid.iter_mut().fold(x, |v, f| f.process(v));
            let h = high.iter_mut().fold(x, |v, f| f.process(v));
            max = max.max(x);
            min = min.min(x);
            for (peak, v) in peaks.iter_mut().zip([b, l, m, h]) {
                *peak = peak.max(v.abs());
            }
            // The strongest band (signed values; lows win ties, then highs).
            let strongest = l.max(m).max(h);
            if strongest == l {
                wins[0] += 1;
            } else if strongest == h {
                wins[2] += 1;
            } else {
                wins[1] += 1;
            }
            frame += 1;
        }
        let weighted = |peak: f64, wins: u32| {
            if peak >= 1.0 {
                32767.0
            } else {
                (peak * 32768.0 * f64::from(wins) / len).trunc() as f32
            }
        };
        records.push([
            to_i16(max),
            to_i16(min),
            to_i16(peaks[0]).min(32767.0),
            weighted(peaks[1], wins[0]),
            weighted(peaks[2], wins[1]),
            weighted(peaks[3], wins[2]),
        ]);
    }
    records
}

/// Columns of the colour overview.
pub const OVERVIEW_COLUMNS: usize = 1200;

/// The colour overview (`PWV4`): 1200 columns of six signed bytes — the
/// mono signal's maximum and minimum, the peak below 400 Hz, and the low,
/// mid and high levels (lows over ±12 ms, mids over ±1 ms), each ÷ 256.
pub fn color_overview(samples: &[f32], rate: u32) -> Vec<[i8; 6]> {
    let records = overview_records(samples, rate);
    let window = |i: usize, radius: usize, field: usize| {
        records[i.saturating_sub(radius)..(i + radius + 1).min(records.len())]
            .iter()
            .map(|r| r[field])
            .fold(0.0, f32::max)
    };
    let mut merged = vec![[f32::MIN, f32::MAX, 0.0, 0.0, 0.0, 0.0]; OVERVIEW_COLUMNS];
    if records.is_empty() {
        return vec![[0; 6]; OVERVIEW_COLUMNS];
    }
    let mut filled = vec![false; OVERVIEW_COLUMNS];
    for (i, record) in records.iter().enumerate() {
        let column = i * OVERVIEW_COLUMNS / records.len();
        let m = &mut merged[column];
        filled[column] = true;
        m[0] = m[0].max(record[0]);
        m[1] = m[1].min(record[1]);
        m[2] = m[2].max(record[2]);
        m[3] = m[3].max(window(i, 12, 3));
        m[4] = m[4].max(window(i, 1, 4));
        m[5] = m[5].max(record[5]);
    }
    // Tracks shorter than 1.2 s repeat columns.
    for i in 1..OVERVIEW_COLUMNS {
        if !filled[i] {
            merged[i] = merged[i - 1];
        }
    }
    merged.into_iter().map(|values| values.map(|v| (v * 128.0 / 32768.0) as i32 as i8)).collect()
}

/// A sample as a 16-bit value (×32768, clamped, truncated).
fn sample_i16(value: f32) -> i32 {
    (value * 32768.0).clamp(-32768.0, 32767.0) as i32
}

/// The monochrome detail waveform (\`PWV3\`): one byte per column of
/// \`rate / 150\` frames — 5 bits of height (the column's peak, relative to
/// the loudest sample, squared) and 3 bits of whiteness (how little of the
/// peak is below 150 Hz: 0 when it is nearly all bass, 7 when there is none).
pub fn mono_detail(samples: &[f32], rate: u32) -> Vec<u8> {
    let frames = samples.len() / 2;
    let per_column = (rate / COLUMNS_PER_SECOND).max(1) as usize;
    let mono: Vec<i32> = (0..frames)
        .map(|f| {
            let (l, r) = (sample_i16(samples[2 * f]), sample_i16(samples[2 * f + 1]));
            (f64::from(l + r) * 0.5) as i32
        })
        .collect();
    let loudest = mono.iter().map(|m| m.unsigned_abs().min(32767)).max().unwrap_or(0);
    // 150 Hz Butterworth low-pass (bilinear transform), direct form I.
    let k = (std::f64::consts::PI * 150.0 / f64::from(rate)).tan();
    let k2 = k * k;
    let den = 1.0 + std::f64::consts::SQRT_2 * k + k2;
    let (b0, b1, b2) = (k2 / den, 2.0 * k2 / den, k2 / den);
    let (a1, a2) = ((2.0 * k2 - 2.0) / den, (1.0 - std::f64::consts::SQRT_2 * k + k2) / den);
    let (mut x1, mut x2, mut y1, mut y2) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    mono.chunks(per_column)
        .map(|column| {
            let mut peak = 0u32;
            let mut bass = 0u32;
            for &m in column {
                peak = peak.max(m.unsigned_abs().min(32767));
                let x = f64::from(m);
                let y = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
                (x2, x1, y2, y1) = (x1, x, y1, y);
                bass = bass.max((y as i32).unsigned_abs().min(32767));
            }
            let whiteness = if peak == 0 {
                7
            } else {
                let ratio = f64::from(bass) / f64::from(peak);
                // One step per eighth: mostly bass is dark (0), none is white (7).
                (7 - (ratio * 8.0).floor().clamp(0.0, 7.0) as u8).min(7)
            };
            let level = if loudest == 0 {
                0.0
            } else {
                (f64::from(peak) * 32767.0 / f64::from(loudest)).trunc()
            };
            let height = ((level * level * 2.932_745_123_302_746_6e-8) as u8).min(31);
            height | whiteness << 5
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_white_and_flat() {
        assert_eq!(zoom_colour(0.0, 0.0, 0.0), [255, 255, 255]);
        let detail = color_detail(&vec![0.0; 44_100 * 2], 44_100);
        assert_eq!(detail.len(), 150);
        assert!(detail.iter().all(|c| c & 0x7c == 0), "no height");
    }

    #[test]
    fn bass_is_red_and_treble_blue() {
        let [r, g, b] = zoom_colour(30_000.0, 2_000.0, 1_000.0);
        assert!(r > 200 && g < 60 && b < 60, "{r} {g} {b}");
        let [r, _, b] = zoom_colour(1_000.0, 2_000.0, 30_000.0);
        assert!(b == 255 && r < 40);
    }
}
