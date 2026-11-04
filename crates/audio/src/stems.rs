//! A track's stems for live mixing, in four parts like rekordbox: vocals,
//! instruments, bass and drums.
//!
//! Only the vocal, bass and drum stems are kept; the instruments are what
//! remains of the track without them. So with every part up the deck plays exactly
//! the original, whatever the model's stems add up to.

use std::sync::atomic::{AtomicBool, AtomicI16, Ordering};

use crate::decode::{Frames, TrackAudio};

/// The parts, in the order of their levels.
pub const VOCALS: usize = 0;
pub const INSTRUMENTS: usize = 1;
pub const BASS: usize = 2;
pub const DRUMS: usize = 3;
pub const PARTS: usize = 4;

/// Vocal, bass and drum stems, at the track's sample rate and length.
///
/// They can arrive piece by piece while the deck plays (see
/// [`StemLayers::empty`]): samples are atomics written by the separating
/// thread, and a block plays the original until its stems are in.
#[derive(Debug, Default)]
pub struct StemLayers {
    vocals: Option<Layer>,
    bass: Option<Layer>,
    drums: Option<Layer>,
    /// Frames per block of `ready`.
    block: usize,
    ready: Box<[AtomicBool]>,
}

/// One stem: interleaved stereo, 16-bit to keep long tracks small, scaled
/// so a stem louder than full scale isn't cut off.
#[derive(Debug)]
struct Layer {
    samples: Box<[AtomicI16]>,
    /// The sample value that 32767 stands for (at least 1).
    scale: f32,
}

impl Layer {
    fn silent(frames: usize, scale: f32) -> Self {
        Self { samples: (0..frames * 2).map(|_| AtomicI16::new(0)).collect(), scale }
    }

    fn set(&self, frame: usize, [l, r]: [f32; 2]) {
        let to_i16 = |s: f32| (s / self.scale * 32_767.0).round().clamp(-32_767.0, 32_767.0) as i16;
        if let Some(pair) = self.samples.get(frame * 2..frame * 2 + 2) {
            pair[0].store(to_i16(l), Ordering::Relaxed);
            pair[1].store(to_i16(r), Ordering::Relaxed);
        }
    }

    fn get(&self, frame: usize) -> [f32; 2] {
        let unit = self.scale / 32_767.0;
        match self.samples.get(frame * 2..frame * 2 + 2) {
            Some(pair) => [
                f32::from(pair[0].load(Ordering::Relaxed)) * unit,
                f32::from(pair[1].load(Ordering::Relaxed)) * unit,
            ],
            None => [0.0; 2],
        }
    }
}

/// Headroom of stems that arrive piece by piece (their peak is not known
/// in advance): twice full scale.
const PIECE_SCALE: f32 = 2.0;

impl StemLayers {
    /// Takes the stems (at any sample rate) for a track of `frames` frames
    /// at `rate`, all ready. Allocates and resamples: not for the audio thread.
    pub fn new(
        vocals: Option<&TrackAudio>,
        bass: Option<&TrackAudio>,
        drums: Option<&TrackAudio>,
        rate: u32,
        frames: usize,
    ) -> Self {
        let layer = |stem: &TrackAudio| {
            let step = f64::from(stem.sample_rate) / f64::from(rate.max(1));
            // A little above the peak: cubic interpolation can overshoot it.
            let scale = stem.samples.iter().fold(1.0_f32, |peak, s| peak.max(s.abs())) * 1.01;
            let layer = Layer::silent(frames, scale);
            for i in 0..frames {
                let sample = if stem.sample_rate == rate {
                    stem.frame(i as i64)
                } else {
                    stem.sample_at(i as f64 * step, true)
                };
                layer.set(i, sample);
            }
            layer
        };
        Self {
            vocals: vocals.map(layer),
            bass: bass.map(layer),
            drums: drums.map(layer),
            block: frames.max(1),
            ready: Box::new([AtomicBool::new(true)]),
        }
    }

    /// Silent stems for the parts in `has` (vocals, bass, drums), filled in
    /// later with [`StemLayers::set`] and made audible a block of `block`
    /// frames at a time with [`StemLayers::set_ready`].
    pub fn empty(has: [bool; 3], frames: usize, block: usize) -> Self {
        let layer = |has: bool| has.then(|| Layer::silent(frames, PIECE_SCALE));
        let block = block.max(1);
        Self {
            vocals: layer(has[0]),
            bass: layer(has[1]),
            drums: layer(has[2]),
            block,
            ready: (0..frames.div_ceil(block).max(1)).map(|_| AtomicBool::new(false)).collect(),
        }
    }

    /// Writes one frame of a part (`VOCALS`, `BASS` or `DRUMS`).
    pub fn set(&self, part: usize, frame: usize, sample: [f32; 2]) {
        if let Some(layer) = self.layer(part) {
            layer.set(frame, sample);
        }
    }

    /// One frame of a part as written so far.
    pub fn get(&self, part: usize, frame: usize) -> [f32; 2] {
        self.layer(part).map_or([0.0; 2], |layer| layer.get(frame))
    }

    /// Makes the stems of block `index` play.
    pub fn set_ready(&self, index: usize) {
        if let Some(ready) = self.ready.get(index) {
            ready.store(true, Ordering::Release);
        }
    }

    /// Whether the frame's block has its stems.
    pub fn ready_at(&self, frame: i64) -> bool {
        frame >= 0
            && self
                .ready
                .get(frame as usize / self.block)
                .is_some_and(|r| r.load(Ordering::Acquire))
    }

    /// Whether each block has its stems, from the track's start; a block
    /// is the track's length over their count.
    pub fn ready_blocks(&self) -> impl Iterator<Item = bool> + '_ {
        self.ready.iter().map(|r| r.load(Ordering::Relaxed))
    }

    /// The share of the track whose stems are in, 0–1.
    pub fn ready_share(&self) -> f32 {
        let done = self.ready.iter().filter(|r| r.load(Ordering::Relaxed)).count();
        done as f32 / self.ready.len() as f32
    }

    fn layer(&self, part: usize) -> Option<&Layer> {
        match part {
            VOCALS => self.vocals.as_ref(),
            BASS => self.bass.as_ref(),
            DRUMS => self.drums.as_ref(),
            _ => None,
        }
    }

    /// Whether the part can be turned down on its own (the instruments
    /// always can).
    pub fn has(&self, part: usize) -> bool {
        part == INSTRUMENTS || self.layer(part).is_some()
    }
}

/// A track with its parts at the given levels.
pub struct Mixed<'a> {
    pub audio: &'a TrackAudio,
    pub stems: &'a StemLayers,
    pub levels: [f32; PARTS],
}

impl Frames for Mixed<'_> {
    fn rate(&self) -> u32 {
        self.audio.sample_rate
    }

    /// track = vocals + bass + drums + instruments, so the mix is the track
    /// at the instruments' level plus the other parts moved from there.
    /// Where the stems aren't in yet, the track plays as it is.
    fn frame(&self, index: i64) -> [f32; 2] {
        let [l, r] = self.audio.frame(index);
        if !self.stems.ready_at(index) {
            return [l, r];
        }
        let instruments = self.levels[INSTRUMENTS];
        let mut out = [l * instruments, r * instruments];
        for (part, level) in [VOCALS, BASS, DRUMS].map(|p| (p, self.levels[p])) {
            if let Some(layer) = self.stems.layer(part) {
                let [sl, sr] = layer.get(index as usize);
                let delta = level - instruments;
                out[0] += sl * delta;
                out[1] += sr * delta;
            }
        }
        out
    }
}

/// The gains that make `stems` add up to `track` most closely (least
/// squares), for models that scale their stems (Demucs turns each one down
/// on its own when it would clip). Stems at another rate are matched by
/// time. 1.0 for each when the fit fails.
pub fn fit_gains(track: &TrackAudio, stems: &[&TrackAudio]) -> Vec<f32> {
    let n = stems.len();
    let mut ata = vec![vec![0.0_f64; n]; n];
    let mut atb = vec![0.0_f64; n];
    let rate = f64::from(track.sample_rate.max(1));
    // Every 7th frame is plenty and keeps it quick.
    for frame in (0..track.frames()).step_by(7) {
        let t = frame as f64 / rate;
        let x: Vec<[f32; 2]> =
            stems.iter().map(|s| s.sample_at(t * f64::from(s.sample_rate), false)).collect();
        let y = track.frame(frame as i64);
        for channel in 0..2 {
            for r in 0..n {
                let xr = f64::from(x[r][channel]);
                atb[r] += xr * f64::from(y[channel]);
                for c in 0..n {
                    ata[r][c] += xr * f64::from(x[c][channel]);
                }
            }
        }
    }
    solve(ata, atb)
        .filter(|g| g.iter().all(|g| (0.25..=4.0).contains(g)))
        .map_or_else(|| vec![1.0; n], |g| g.iter().map(|&g| g as f32).collect())
}

/// Solves `a x = b` by Gaussian elimination with partial pivoting.
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for c in 0..n {
        let pivot = (c..n).max_by(|&i, &j| a[i][c].abs().total_cmp(&a[j][c].abs()))?;
        if a[pivot][c].abs() < 1e-9 {
            return None;
        }
        a.swap(c, pivot);
        b.swap(c, pivot);
        for r in c + 1..n {
            let f = a[r][c] / a[c][c];
            let pivot_row = a[c].clone();
            for (x, p) in a[r].iter_mut().zip(&pivot_row).skip(c) {
                *x -= f * p;
            }
            b[r] -= f * b[c];
        }
    }
    for c in (0..n).rev() {
        for k in c + 1..n {
            b[c] -= a[c][k] * b[k];
        }
        b[c] /= a[c][c];
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant(rate: u32, frames: usize, value: f32) -> TrackAudio {
        TrackAudio { sample_rate: rate, samples: vec![value; frames * 2] }
    }

    #[test]
    fn parts_add_up_to_the_track() {
        let track = constant(48_000, 100, 0.6);
        let vocals = constant(48_000, 100, 0.25);
        let bass = constant(48_000, 100, 0.05);
        let drums = constant(44_100, 92, 0.15);
        let stems = StemLayers::new(Some(&vocals), Some(&bass), Some(&drums), 48_000, 100);
        let mix = |levels| Mixed { audio: &track, stems: &stems, levels }.frame(10)[0];
        assert!((mix([1.0; 4]) - 0.6).abs() < 1e-4);
        // Without vocals: bass, drums and the rest (0.15) remain.
        assert!((mix([0.0, 1.0, 1.0, 1.0]) - 0.35).abs() < 1e-4);
        assert!((mix([1.0, 0.0, 0.0, 0.0]) - 0.25).abs() < 1e-4);
        assert!((mix([0.0, 0.0, 1.0, 0.0]) - 0.05).abs() < 1e-4);
        assert!((mix([0.0, 0.0, 0.0, 1.0]) - 0.15).abs() < 1e-4);
        assert!((mix([0.0, 1.0, 0.0, 0.0]) - 0.15).abs() < 1e-4);
        assert!(stems.has(VOCALS) && stems.has(BASS) && stems.has(DRUMS));
    }

    #[test]
    fn a_missing_stem_stays_in_the_instruments() {
        let track = constant(48_000, 10, 0.5);
        let vocals = constant(48_000, 10, 0.2);
        let stems = StemLayers::new(Some(&vocals), None, None, 48_000, 10);
        assert!(!stems.has(DRUMS) && !stems.has(BASS));
        let frame = Mixed { audio: &track, stems: &stems, levels: [0.0, 1.0, 0.0, 0.0] }.frame(3);
        assert!((frame[0] - 0.3).abs() < 1e-4);
        // Past the end: silence.
        assert_eq!(Mixed { audio: &track, stems: &stems, levels: [1.0; 4] }.frame(20), [0.0; 2]);
    }

    #[test]
    fn pieces_play_once_ready() {
        let track = constant(48_000, 100, 0.6);
        let stems = StemLayers::empty([true, false, false], 100, 40);
        for frame in 0..100 {
            stems.set(VOCALS, frame, [0.25; 2]);
        }
        let no_vocals = |frame| {
            Mixed { audio: &track, stems: &stems, levels: [0.0, 1.0, 1.0, 1.0] }.frame(frame)[0]
        };
        // Nothing in yet: the track as it is.
        assert!((no_vocals(50) - 0.6).abs() < 1e-3);
        stems.set_ready(1);
        assert!((no_vocals(50) - 0.35).abs() < 1e-3);
        assert!((no_vocals(10) - 0.6).abs() < 1e-3);
        assert!((stems.ready_share() - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn loud_stems_are_not_cut_off() {
        let track = constant(48_000, 10, 0.9);
        let vocals = constant(48_000, 10, 1.4);
        let stems = StemLayers::new(Some(&vocals), None, None, 48_000, 10);
        let frame = Mixed { audio: &track, stems: &stems, levels: [1.0, 0.0, 0.0, 0.0] }.frame(3);
        assert!((frame[0] - 1.4).abs() < 1e-3);
    }

    #[test]
    fn gains_undo_a_models_scaling() {
        // Two parts, which the model turned down by 0.5 and 0.8.
        let rate = 8_000;
        let wave = |f: f32| -> Vec<f32> {
            (0..4_000).flat_map(|i| [(i as f32 * f).sin() * 0.4; 2]).collect()
        };
        let (a, b) = (wave(0.031), wave(0.17));
        let track: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
        let track = TrackAudio { sample_rate: rate, samples: track };
        let scaled = |w: &[f32], g: f32| TrackAudio {
            sample_rate: rate,
            samples: w.iter().map(|x| x * g).collect(),
        };
        let gains = fit_gains(&track, &[&scaled(&a, 0.5), &scaled(&b, 0.8)]);
        assert!((gains[0] - 2.0).abs() < 1e-3 && (gains[1] - 1.25).abs() < 1e-3, "{gains:?}");
    }
}
