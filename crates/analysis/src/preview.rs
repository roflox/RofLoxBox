//! The preview waveforms rekordbox shows on players' overview strips:
//! `PWAV` (400 columns) and `PWV2` (100 "dots").
//!
//! The audio is mixed to mono and handled in blocks of `rate / 75` frames.
//! Three bands are followed: a band around 150 Hz, 283 Hz – 7.9 kHz and
//! above 7.9 kHz. An envelope follower on each band marks peaks (a rise that
//! comes at least 0.32 s after the previous one); each peak is worth the
//! highest envelope reached before the next.
//!
//! A `PWAV` column's height comes from the track's average level over that
//! 1/400 of the track, turned down where there are few or quiet bass peaks.
//! Its whiteness compares the band peaks: bass-heavy columns are dark, ones
//! without bass white. The `PWV2` dots count bass peak strength per 1/100.

/// Columns of the `PWAV` preview.
pub const PREVIEW_COLUMNS: usize = 400;
/// Dots of the `PWV2` preview.
pub const DOTS: usize = 100;

/// Peaks remembered per band; later ones are ignored.
const MAX_PEAKS: usize = 36_000;

/// `2π` rounded to 14 digits (a little short of `TAU`): the reference
/// previews come out exactly with it.
#[allow(clippy::approx_constant, reason = "matches the reference files, not TAU")]
const TWO_PI: f64 = 6.283_185_307_179_58;

/// Level thresholds for heights 25 down to 3 (below the last: 2). They are
/// fractions of full scale: 1.55, 1.2, 0.94 … 0.067 × 32768.
const HEIGHT_STEPS: [f32; 23] = [
    50790.4, 39321.6, 30801.92, 25886.72, 23265.28, 21299.2, 19333.12, 17367.04, 15892.48,
    14909.44, 13926.4, 12943.36, 11960.32, 10977.28, 9994.24, 9011.2, 8028.16, 7045.12, 6062.08,
    5079.04, 4096.0, 3112.96, 2195.456,
];

/// Bass peak sums for dot sizes 15 down to 4; above `0x517` is 3, else 2.
const DOT_STEPS: [i64; 12] =
    [0x50c3, 0x4026, 0x32f5, 0x287a, 0x2026, 0x198a, 0x1449, 0x101d, 0xccc, 0xa2a, 0x813, 0x66a];

fn height(level: f32) -> u8 {
    if level <= 0.0 {
        return 2;
    }
    HEIGHT_STEPS.iter().position(|&step| step <= level).map_or(2, |i| 25 - i as u8)
}

fn dot(sum: i64) -> u8 {
    match DOT_STEPS.iter().position(|&step| step <= sum) {
        Some(i) => 15 - i as u8,
        None if sum > 0x517 => 3,
        None if sum == 0 => 1,
        None => 2,
    }
}

/// One second-order section, direct form I in double precision.
#[derive(Debug, Clone, Default)]
struct Stage {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Stage {
    /// RBJ cookbook designs: `sin`/`cos` of the cutoff and `alpha`.
    fn design(rate: f32, cutoff: f32, q: f32) -> (f64, f64, f64, f64) {
        let (sin, cos) = (f64::from(cutoff) * TWO_PI / f64::from(rate)).sin_cos();
        let alpha = sin / (f64::from(q) + f64::from(q));
        let norm = 1.0 / (alpha + 1.0);
        (cos, alpha, norm, (-alpha).mul_add(norm, norm))
    }

    fn band_pass(rate: f32, centre: f32, q: f32) -> Self {
        let (cos, alpha, norm, a2) = Self::design(rate, centre, q);
        let b0 = alpha * norm;
        Self { b: [b0, 0.0, -alpha * norm], a: [cos * -2.0 * norm, a2], ..Self::default() }
    }

    fn high_pass(rate: f32, cutoff: f32, q: f32) -> Self {
        let (cos, _, norm, a2) = Self::design(rate, cutoff, q);
        let b0 = cos.mul_add(0.5, 0.5) * norm;
        Self { b: [b0, -(cos + 1.0) * norm, b0], a: [cos * -2.0 * norm, a2], ..Self::default() }
    }

    fn low_pass(rate: f32, cutoff: f32, q: f32) -> Self {
        let (cos, _, norm, a2) = Self::design(rate, cutoff, q);
        let b0 = (-cos).mul_add(0.5, 0.5) * norm;
        Self {
            b: [b0, (-cos).mul_add(norm, norm), b0],
            a: [cos * -2.0 * norm, a2],
            ..Self::default()
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let feed = self.x[1].mul_add(self.b[2], self.x[0].mul_add(self.b[1], x * self.b[0]));
        let back = self.y[1].mul_add(self.a[1], self.y[0] * self.a[0]);
        let y = feed - back;
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// A band: filter stages in series; the output is |level| × 32768.
struct Band(Vec<Stage>);

impl Band {
    fn level(&mut self, x: f32) -> f32 {
        let y = self.0.iter_mut().fold(f64::from(x), |v, stage| stage.process(v));
        (y as f32).abs() * 32768.0
    }
}

/// Timing shared by the envelope followers, in samples.
#[derive(Clone, Copy)]
struct Timing {
    block: usize,
    total: i64,
    /// A rise this soon after the last peak belongs to it.
    merge: f64,
    /// The envelope holds this long before decaying…
    hold: f64,
    /// …and then drops by `DECAY` once per this many samples.
    decay_every: i64,
}

const DECAY: f32 = 0.993_830_3;

/// Envelope follower marking peaks in one band, sample by sample.
/// `values[i]` ends up as the highest envelope of peak `i`.
struct Peaks {
    values: Vec<f32>,
    count: usize,
    since: i64,
    blocks: i64,
    /// The end of the track was reached (and a closing peak added).
    ended: bool,
    /// Fraction of the envelope a sample must reach to count as a rise.
    rise: f32,
}

impl Peaks {
    const FLOOR: f32 = 0.01;

    /// Whether `v` is above the floor, compared in double precision,
    /// which puts `0.01f32` itself below it.
    fn above_floor(v: f32) -> bool {
        f64::from(v) > 0.01
    }

    fn new(rise: f32) -> Self {
        let mut values = vec![0.0; MAX_PEAKS];
        values[0] = Self::FLOOR;
        Self { values, count: 0, since: 0, blocks: 0, ended: false, rise }
    }

    fn set(&mut self, index: usize, value: f32) {
        if let Some(slot) = self.values.get_mut(index) {
            *slot = value;
        }
    }

    /// Runs one block; returns whether a new peak started in it.
    fn run(&mut self, block: &[f32], timing: Timing) -> bool {
        let mut started = false;
        let mut envelope = self.values.get(self.count).copied().unwrap_or(0.75);
        let mut held = 0i64;
        let first = self.blocks * timing.block as i64;
        for (i, &x) in block.iter().enumerate() {
            self.since += 1;
            if self.values[0] == Self::FLOOR {
                // Nothing yet: the first sound starts the first peak.
                if Self::above_floor(x) {
                    self.values[0] = x;
                    self.since = 0;
                    self.count = 1;
                    envelope = x;
                    started = true;
                }
            } else if (Self::above_floor(envelope) && x < envelope * self.rise)
                || (!Self::above_floor(envelope) && f64::from(x) < 0.01)
            {
                if self.since as f64 <= timing.hold {
                    held = 0;
                } else {
                    held += 1;
                    if held >= timing.decay_every {
                        held = 0;
                        let decayed = envelope * DECAY;
                        envelope = if Self::above_floor(decayed) { decayed } else { Self::FLOOR };
                    }
                }
            } else if (self.since as f64) < timing.merge {
                envelope = envelope.max(x);
            } else {
                started = true;
                // `count` is at least 1 here: the first peak was found above.
                self.set(self.count - 1, envelope);
                self.since = 0;
                held = 0;
                self.count += 1;
                envelope = x;
            }
            if first + i as i64 == timing.total - 1 {
                self.set(self.count, envelope);
                self.count += 1;
                self.ended = true;
            }
        }
        self.set(self.count, envelope);
        self.blocks += 1;
        started
    }
}

/// The coarser follower behind the dots: it looks at the highest sample of
/// every four and closes a peak's value when the next one starts.
struct CoarsePeaks {
    values: Vec<f32>,
    count: usize,
    since: i64,
    blocks: i64,
    ended: bool,
}

impl CoarsePeaks {
    const FLOOR: f32 = 327.68;
    const RISE: f32 = 0.75;

    fn new() -> Self {
        let mut values = vec![0.0; MAX_PEAKS];
        values[0] = Peaks::FLOOR;
        Self { values, count: 0, since: 0, blocks: 0, ended: false }
    }

    fn set(&mut self, index: usize, value: f32) {
        if let Some(slot) = self.values.get_mut(index) {
            *slot = value;
        }
    }

    fn run(&mut self, block: &[f32], timing: Timing) {
        let mut envelope = self.values.get(self.count).copied().unwrap_or(0.0);
        let mut held = 0i64;
        let mut highest = 0.0f32;
        let first = self.blocks * timing.block as i64;
        let partial = block.len() < timing.block;
        for (i, &x) in block.iter().enumerate() {
            self.since += 1;
            highest = highest.max(x);
            let last = first + i as i64 == timing.total - 1;
            if self.since & 3 != 0 && !(partial && last) {
                continue;
            }
            let x = std::mem::take(&mut highest);
            if self.values[0] == Peaks::FLOOR {
                if x > Self::FLOOR {
                    self.since = 0;
                    self.values[0] = x;
                    self.count = 1;
                    envelope = x;
                }
            } else if (envelope > Self::FLOOR && x < envelope * Self::RISE)
                || (envelope <= Self::FLOOR && x < Self::FLOOR)
            {
                if self.since as f64 <= timing.hold {
                    held = 0;
                } else {
                    held += 4;
                    if held >= timing.decay_every {
                        held = 0;
                        envelope = (envelope * DECAY).max(Self::FLOOR);
                    }
                }
            } else if self.since as f64 >= timing.merge {
                self.set(self.count, envelope);
                self.since = 0;
                held = 0;
                self.count += 1;
                envelope = x;
            } else {
                envelope = envelope.max(x);
            }
            if last {
                self.set(self.count, envelope);
                self.count += 1;
                self.ended = true;
            }
        }
        self.set(self.count, envelope);
        self.blocks += 1;
    }
}

/// Peak values summed into 200 bins (each filling two columns) as time
/// passes. A bin moves on only when a new peak arrives after its time is up.
#[derive(Default)]
struct PeakBins {
    sums: Vec<f32>,
    column: usize,
    sum: f32,
    elapsed: f32,
    last_block: i64,
    taken: usize,
}

impl PeakBins {
    fn new() -> Self {
        Self { sums: vec![0.0; PREVIEW_COLUMNS + 2], ..Self::default() }
    }

    /// Takes the peaks finished so far; `false` stops the block (one whose
    /// bins ran past the end is dropped).
    fn take(&mut self, peaks: &Peaks, block_no: i64, scale: &Scale) -> bool {
        if self.column >= PREVIEW_COLUMNS {
            return true;
        }
        // Until the end, the newest peak is still open.
        let usable = if peaks.ended { peaks.count } else { peaks.count.saturating_sub(1) };
        if !peaks.ended && self.taken >= usable {
            return true;
        }
        self.sums[self.column] = self.sum;
        let mut elapsed = self.elapsed + (block_no - self.last_block) as f32;
        self.last_block = block_no;
        for p in self.taken..usable {
            if scale.bin <= elapsed {
                let value = self.sum * 7500.0 * scale.inverse;
                self.sums[self.column] = value;
                self.sums[self.column + 1] = value;
                let mut stop = self.column >= PREVIEW_COLUMNS - 2;
                self.column += 2;
                elapsed -= scale.bin;
                while !stop && scale.bin <= elapsed {
                    self.sums[self.column] = 0.0;
                    self.sums[self.column + 1] = 0.0;
                    stop = self.column >= PREVIEW_COLUMNS - 2;
                    self.column += 2;
                    elapsed -= scale.bin;
                }
                if stop {
                    break;
                }
                self.sums[self.column] = 0.0;
                self.sum = 0.0;
            }
            let closing = peaks.ended && p + 1 == peaks.count;
            if !closing && self.column < PREVIEW_COLUMNS && p < MAX_PEAKS {
                self.sum += peaks.values[p] as i32 as f32;
                self.sums[self.column] = self.sum;
            }
        }
        self.elapsed = elapsed;
        self.taken = usable;
        peaks.ended || self.column <= PREVIEW_COLUMNS
    }
}

/// Block counts that set the time scale.
struct Scale {
    /// One over the blocks in the track, rounded to `f32`.
    inverse: f32,
    /// Blocks per peak bin.
    bin: f32,
    /// Blocks per dot.
    per_dot: i64,
}

/// The dots: the first newly finished coarse peak of each block, summed per
/// 1/100 of the track.
#[derive(Default)]
struct DotSums {
    sums: Vec<i64>,
    column: usize,
    sum: i64,
    taken: usize,
}

impl DotSums {
    fn new() -> Self {
        Self { sums: vec![0; DOTS + 1], ..Self::default() }
    }

    fn take(&mut self, peaks: &CoarsePeaks, block_no: i64, scale: &Scale) {
        if self.column >= DOTS {
            return;
        }
        let target = if peaks.ended {
            DOTS
        } else {
            let at = if scale.per_dot == 0 { 0 } else { block_no / scale.per_dot };
            at.min(DOTS as i64 - 1) as usize
        };
        self.sums[self.column] = self.sum;
        if self.column < target {
            let span = 2 * scale.per_dot;
            self.sums[self.column] = if span == 0 { 0 } else { self.sum * 75 / span };
            for c in self.column + 1..=target.min(DOTS - 1) {
                self.sums[c] = 0;
            }
            self.column = target;
        }
        let fresh = peaks.count > self.taken && block_no >= 51 && self.column < DOTS;
        if fresh && self.taken < MAX_PEAKS {
            self.sums[self.column] += peaks.values[self.taken] as i64;
        }
        if self.column < DOTS {
            self.sum = self.sums[self.column];
        }
        self.taken = peaks.count;
    }
}

/// Builds the previews block by block.
struct Preview {
    timing: Timing,
    scale: Scale,
    bands: [Band; 3],
    low: Peaks,
    mid: Peaks,
    high: Peaks,
    low_dots: CoarsePeaks,
    mid_dots: CoarsePeaks,
    block_no: i64,
    // The average level, reported every 9 blocks or at a bass peak.
    level_sum: f32,
    level_blocks: i32,
    bass_peak: bool,
    after_peak: bool,
    late: bool,
    report_column: usize,
    levels: Vec<f32>,
    level_column: usize,
    level: f32,
    bins: [PeakBins; 3],
    dots: [DotSums; 2],
    finished: usize,
    finished_dots: usize,
    columns: Vec<u8>,
    dot_sizes: Vec<u8>,
}

impl Preview {
    fn new(rate: u32, frames: usize) -> Self {
        let block = (rate / 75).max(1) as usize;
        let rate_f = rate as f32;
        let total = frames as i64;
        // The time scale counts a partial last block; the dots don't.
        let blocks = (total as f32 / block as f32).ceil();
        let whole_blocks = total / block as i64;
        let per_dot = if (-99..=99).contains(&whole_blocks) { 1 } else { whole_blocks / 100 };
        let q = std::f32::consts::FRAC_1_SQRT_2;
        Self {
            timing: Timing {
                block,
                total,
                merge: f64::from(rate) * 0.32,
                hold: f64::from(rate) * 1.714,
                decay_every: (f64::from(rate) * 0.011_609_977_324_263_039) as i32 as i64,
            },
            scale: Scale { inverse: 1.0 / blocks, bin: blocks * 0.005, per_dot },
            bands: [
                Band(vec![Stage::band_pass(rate_f, 150.0, 0.9); 2]),
                Band(vec![
                    Stage::high_pass(rate_f, 283.0, q),
                    Stage::low_pass(rate_f, 7900.0, q),
                    Stage::high_pass(rate_f, 283.0, q),
                    Stage::low_pass(rate_f, 7900.0, q),
                ]),
                Band(vec![Stage::high_pass(rate_f, 7900.0, q); 2]),
            ],
            low: Peaks::new(0.75),
            mid: Peaks::new(0.75),
            high: Peaks::new(0.66),
            low_dots: CoarsePeaks::new(),
            mid_dots: CoarsePeaks::new(),
            block_no: 0,
            level_sum: 0.0,
            level_blocks: 0,
            bass_peak: false,
            after_peak: false,
            late: false,
            report_column: 0,
            levels: vec![0.0; PREVIEW_COLUMNS],
            level_column: 0,
            level: 0.0,
            bins: [PeakBins::new(), PeakBins::new(), PeakBins::new()],
            dots: [DotSums::new(), DotSums::new()],
            finished: 0,
            finished_dots: 0,
            columns: vec![2 | 5 << 5; PREVIEW_COLUMNS],
            dot_sizes: vec![1; DOTS],
        }
    }

    /// Sum of |mono| over a block, added up in 16 lanes and then the rest
    /// one by one; this rounding matches the reference files.
    fn block_sum(&self, mono: &[f32]) -> f32 {
        let whole = mono.len() / 16 * 16;
        let mut sum = self.level_sum;
        if whole > 0 {
            let mut lanes = [[0.0f32; 4]; 4];
            lanes[0][0] = sum;
            for chunk in mono[..whole].chunks_exact(16) {
                for (v, lane) in lanes.iter_mut().enumerate() {
                    for (k, acc) in lane.iter_mut().enumerate() {
                        *acc += chunk[v * 4 + k].abs();
                    }
                }
            }
            let t: [f32; 4] =
                std::array::from_fn(|k| lanes[3][k] + (lanes[2][k] + (lanes[1][k] + lanes[0][k])));
            sum = (t[0] + t[1]) + (t[2] + t[3]);
        }
        mono[whole..].iter().fold(sum, |s, x| x.abs() + s)
    }

    fn block(&mut self, mono: &[f32], bands: &[Vec<f32>; 3]) {
        self.block_no += 1;
        let timing = self.timing;
        if self.low.run(&bands[0], timing) {
            self.bass_peak = true;
        }
        self.low_dots.run(&bands[0], timing);

        // The average level, reported after 9 blocks or at a bass peak.
        let sum = self.block_sum(mono);
        let counted = self.level_blocks;
        self.level_blocks += 1;
        let report = if counted >= 8 || self.bass_peak {
            let mean = sum / (timing.block as i32 * self.level_blocks) as f32;
            if self.after_peak {
                self.late = true;
                self.after_peak = false;
            }
            if self.bass_peak {
                self.after_peak = true;
            }
            self.level_blocks = 0;
            self.bass_peak = false;
            self.level_sum = 0.0;
            Some(mean)
        } else {
            self.level_sum = sum;
            None
        };

        self.mid.run(&bands[1], timing);
        self.mid_dots.run(&bands[1], timing);
        self.high.run(&bands[2], timing);

        // Levels into columns.
        let scale = &self.scale;
        let mut column = self.level_column;
        let reached = (((self.block_no as f32) * 400.0 * scale.inverse) as i64)
            .min(PREVIEW_COLUMNS as i64 - 1);
        let target = if self.low.ended { PREVIEW_COLUMNS } else { reached as usize };
        if let Some(slot) = self.levels.get_mut(column) {
            *slot = self.level;
        }
        if column < target {
            self.levels[column] = self.level * 15000.0 * scale.inverse;
            for c in column..target {
                if c < PREVIEW_COLUMNS - 1 {
                    self.levels[c + 1] = 0.0;
                }
            }
            column = target;
        }
        if let Some(mean) = report.filter(|_| column < PREVIEW_COLUMNS) {
            if self.late {
                let value = if column != self.report_column {
                    mean * 15000.0 * scale.inverse
                } else {
                    mean
                };
                self.levels[self.report_column] += value;
            } else {
                self.levels[column] += mean;
                self.report_column = column;
            }
            self.late = false;
        }
        if column < PREVIEW_COLUMNS {
            self.level = self.levels[column];
        }
        self.level_column = column;

        // Peaks into bins.
        for (bins, peaks) in self.bins.iter_mut().zip([&self.low, &self.mid, &self.high]) {
            if !bins.take(peaks, self.block_no, scale) {
                return;
            }
        }

        let all_ended = self.low.ended && self.mid.ended && self.high.ended;
        let ready = if all_ended {
            PREVIEW_COLUMNS
        } else {
            self.bins.iter().map(|b| b.column).fold(self.level_column, usize::min)
        };
        for c in self.finished..ready.min(PREVIEW_COLUMNS) {
            self.columns[c] = self.column(c);
        }
        self.finished = self.finished.max(ready);

        for (sums, peaks) in self.dots.iter_mut().zip([&self.low_dots, &self.mid_dots]) {
            sums.take(peaks, self.block_no, scale);
        }
        let ready = if self.low_dots.ended && self.mid_dots.ended {
            DOTS
        } else {
            self.dots[0].column.min(self.dots[1].column)
        };
        for c in self.finished_dots..ready.min(DOTS) {
            let (low, mid) = (self.dots[0].sums[c], self.dots[1].sums[c]);
            self.dot_sizes[c] = if low == 0 { dot(i64::from(mid != 0)) } else { dot(low) };
        }
        self.finished_dots = self.finished_dots.max(ready);
    }

    /// One finished `PWAV` column: height and whiteness.
    fn column(&self, c: usize) -> u8 {
        let decibels = |v: f32| -> i32 {
            if v <= 0.0 {
                return 0;
            }
            let db = ((f64::from(v) * 3.051_757_812_5e-5).log10() * 20.0) as f32 + 30.5;
            db.clamp(0.0, 30.0) as i32
        };
        let level = self.levels[c];
        let low = self.bins[0].sums[c];
        let (bass, mid, treble) = (
            decibels(low * 0.5),
            decibels(self.bins[1].sums[c] * 0.25),
            decibels(self.bins[2].sums[c]),
        );
        // Few or weak bass peaks turn the level down, to a quarter at least.
        let weighted = (low * 0.75 * (1.0 / 32768.0) + 0.25).min(1.0) * level;
        let whiteness = if bass < 14 {
            if mid - treble < 4 { 5 } else { 4 }
        } else if mid - treble < 0 {
            if treble < bass { 2 } else { 3 }
        } else if mid < bass {
            0
        } else {
            1
        };
        height(weighted) | whiteness << 5
    }
}

/// Mono: both channels as 16-bit values, summed and scaled to ±1.
fn mono(left: f32, right: f32) -> f32 {
    let i16 = |v: f32| (v * 32768.0).clamp(-32768.0, 32767.0) as i32 as f32;
    (i16(left) + i16(right)) * (1.0 / 65536.0)
}

/// The `PWAV` preview (400 bytes: 5 bits of height, 3 of whiteness) and the
/// `PWV2` dots (100 bytes, sizes 1–15) of interleaved stereo samples.
pub fn preview(samples: &[f32], rate: u32) -> (Vec<u8>, Vec<u8>) {
    // One frame less than the file holds, as the reference files have it.
    let frames = (samples.len() / 2).saturating_sub(1);
    let mut preview = Preview::new(rate, frames);
    let block = preview.timing.block;
    let mut mono_block = Vec::with_capacity(block);
    let mut bands: [Vec<f32>; 3] = Default::default();
    for chunk in samples[..frames * 2].chunks(block * 2) {
        mono_block.clear();
        bands.iter_mut().for_each(Vec::clear);
        for frame in chunk.chunks_exact(2) {
            let m = mono(frame[0], frame[1]);
            mono_block.push(m * 32768.0);
            for (band, out) in preview.bands.iter_mut().zip(bands.iter_mut()) {
                out.push(band.level(m));
            }
        }
        preview.block(&mono_block, &bands);
    }
    (preview.columns, preview.dot_sizes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps() {
        assert_eq!(height(0.0), 2);
        assert_eq!(height(60_000.0), 25);
        assert_eq!(height(4096.0), 5);
        assert_eq!(dot(0), 1);
        assert_eq!(dot(1), 2);
        assert_eq!(dot(0x518), 3);
        assert_eq!(dot(0x50c3), 15);
        assert_eq!(HEIGHT_STEPS[0].to_bits(), 0x4746_6666);
        assert_eq!(HEIGHT_STEPS[22].to_bits(), 0x4509_374c);
    }

    #[test]
    fn silence() {
        let (columns, dots) = preview(&vec![0.0; 44_100 * 2 * 10], 44_100);
        assert_eq!(columns.len(), PREVIEW_COLUMNS);
        assert_eq!(dots.len(), DOTS);
        assert!(columns.iter().all(|c| c & 31 == 2));
        assert!(dots.iter().all(|&d| d == 1));
    }

    /// Short 150 Hz bursts every 0.35 s: the loud columns are dark (bass),
    /// and every dot after the first second has a bass peak in it.
    #[test]
    fn bass_pulses() {
        let rate = 44_100;
        let every = rate * 35 / 100;
        let samples: Vec<f32> = (0..rate * 40 + 123)
            .flat_map(|i| {
                let t = (i % every) as f32 / rate as f32;
                let v = if t < 0.1 { 0.8 * (t * 150.0 * std::f32::consts::TAU).sin() } else { 0.0 };
                [v, v]
            })
            .collect();
        let (columns, dots) = preview(&samples, rate as u32);
        // From the second second on: before that no bass peak has closed.
        let loud: Vec<u8> = columns[10..].iter().copied().filter(|c| c & 31 >= 10).collect();
        assert!(loud.len() > 50, "{columns:?}");
        assert!(loud.iter().all(|c| c >> 5 <= 3), "{columns:?}");
        assert!(dots[3..].iter().all(|&d| d > 1), "{dots:?}");
    }
}
