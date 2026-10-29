//! BPM and beat grid detection for tracks that keep one tempo.
//!
//! Our own method, built from standard onset-based tempo estimation:
//! 1. An onset envelope (spectral flux: new energy per frame, every 5 ms),
//!    and a second one for the bass alone.
//! 2. A rough tempo from the envelope's autocorrelation, looked for within
//!    one octave of BPMs so that half and double tempos don't compete.
//! 3. The exact BPM and the phase: the envelope is folded at candidate beat
//!    lengths 0.01 BPM apart; the right one lines up every beat of the
//!    track, which gives the sharpest peak, and the peak is the phase.
//! 4. The downbeat: the beat of the bar with the strongest bass onsets.

use crate::spectrum::{for_each_frame, mono};

/// A detected grid: beats `60 / bpm` apart from `first_beat` (the first
/// beat at or after the start), which is beat `first_number` (1–4) of
/// its bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    pub bpm: f64,
    pub first_beat: f64,
    pub first_number: u8,
}

/// The default BPM search range: one octave that holds house, techno,
/// dubstep and drum & bass at their usual tempos.
pub const DEFAULT_RANGE: (f64, f64) = (98.0, 195.0);

/// Frames of the onset envelopes per second (roughly; see `Envelopes`).
const FRAMES_PER_SECOND: f64 = 200.0;
const FFT_SIZE: usize = 2048;
/// Bins up to this frequency feed the bass envelope.
const BASS_HZ: f64 = 150.0;
/// Bins up to this frequency feed the full envelope.
const TOP_HZ: f64 = 11_000.0;
/// The envelope peaks this much (negative: before) the beat as a DJ
/// would place it; measured against rekordbox's grids (median of 315
/// tracks), and taken off the phase.
const ENVELOPE_DELAY: f64 = -0.0097;

struct Envelopes {
    /// Seconds between frames.
    step: f64,
    /// The time of frame 0 (the middle of its window).
    offset: f64,
    full: Vec<f32>,
    bass: Vec<f32>,
}

fn envelopes(mono: &[f32], rate: u32) -> Envelopes {
    let hop = (f64::from(rate) / FRAMES_PER_SECOND).round().max(1.0) as usize;
    let bin_hz = f64::from(rate) / FFT_SIZE as f64;
    let bass_bins = (BASS_HZ / bin_hz).ceil() as usize + 1;
    let top_bins = ((TOP_HZ / bin_hz) as usize).min(FFT_SIZE / 2);
    let mut previous = vec![0.0f32; FFT_SIZE / 2 + 1];
    let (mut full, mut bass) = (Vec::new(), Vec::new());
    for_each_frame(mono, FFT_SIZE, hop, |_, magnitudes| {
        let (mut f, mut b) = (0.0f32, 0.0f32);
        for (k, (&m, p)) in magnitudes.iter().zip(previous.iter_mut()).enumerate().skip(1) {
            if k > top_bins {
                break;
            }
            // Log compression, so quiet parts count too.
            let level = (1.0 + 1000.0 * m).ln();
            let rise = (level - *p).max(0.0);
            *p = level;
            f += rise;
            if k <= bass_bins {
                b += rise;
            }
        }
        full.push(f);
        bass.push(b);
    });
    Envelopes {
        step: hop as f64 / f64::from(rate),
        offset: FFT_SIZE as f64 / 2.0 / f64::from(rate),
        full: emphasise(&full),
        bass: emphasise(&bass),
    }
}

/// Takes off the local average (about a second), keeping what stands out.
fn emphasise(envelope: &[f32]) -> Vec<f32> {
    const HALF: usize = 100;
    let mut prefix = vec![0.0f64; envelope.len() + 1];
    for (i, &v) in envelope.iter().enumerate() {
        prefix[i + 1] = prefix[i] + f64::from(v);
    }
    (0..envelope.len())
        .map(|i| {
            let (a, b) = (i.saturating_sub(HALF), (i + HALF + 1).min(envelope.len()));
            let mean = (prefix[b] - prefix[a]) / (b - a) as f64;
            (f64::from(envelope[i]) - mean).max(0.0) as f32
        })
        .collect()
}

/// The beat length (in frames) the envelope repeats at most strongly,
/// within `range` BPM.
fn rough_period(envelope: &[f32], step: f64, range: (f64, f64)) -> Option<f64> {
    let shortest = (60.0 / range.1 / step).floor() as usize;
    let longest = (60.0 / range.0 / step).ceil() as usize;
    if shortest < 2 || envelope.len() < longest * 8 {
        return None;
    }
    let correlation = |lag: usize| -> f64 {
        envelope.iter().zip(&envelope[lag..]).map(|(a, b)| f64::from(a * b)).sum::<f64>()
            / (envelope.len() - lag) as f64
    };
    let acf: Vec<f64> = (0..=longest * 4 + 1).map(correlation).collect();
    // A beat length also repeats at two and four beats (bars).
    let score = |lag: usize| acf[lag] + 0.5 * acf[2 * lag] + 0.25 * acf[4 * lag];
    let best = (shortest..=longest).max_by(|&a, &b| score(a).total_cmp(&score(b)))?;
    // Between frames, from the neighbours.
    let (l, c, r) = (score(best - 1), score(best), score(best + 1));
    let denominator = l - 2.0 * c + r;
    let shift = if denominator < 0.0 { 0.5 * (l - r) / denominator } else { 0.0 };
    Some(best as f64 + shift.clamp(-0.5, 0.5))
}

/// Phase bins per beat when folding.
const PHASE_BINS: usize = 96;

/// The envelope folded at `period` frames: its strength by phase, lightly
/// smoothed (circularly).
fn fold(envelope: &[f32], period: f64) -> [f64; PHASE_BINS] {
    let mut bins = [0.0f64; PHASE_BINS];
    for (i, &v) in envelope.iter().enumerate() {
        let phase = (i as f64 / period).fract();
        let at = phase * PHASE_BINS as f64;
        // Split between the two nearest bins.
        let lower = at.floor() as usize % PHASE_BINS;
        let weight = at.fract();
        bins[lower] += f64::from(v) * (1.0 - weight);
        bins[(lower + 1) % PHASE_BINS] += f64::from(v) * weight;
    }
    let mut smooth = [0.0f64; PHASE_BINS];
    for (i, s) in smooth.iter_mut().enumerate() {
        let at = |d: isize| bins[(i as isize + d).rem_euclid(PHASE_BINS as isize) as usize];
        *s = 0.25 * at(-1) + 0.5 * at(0) + 0.25 * at(1);
    }
    smooth
}

/// The strongest bin of a fold and where it peaks (fraction of a beat).
fn peak(bins: &[f64; PHASE_BINS]) -> (f64, f64) {
    let (i, &value) =
        bins.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap_or((0, &0.0));
    let at = |d: isize| bins[(i as isize + d).rem_euclid(PHASE_BINS as isize) as usize];
    let (l, r) = (at(-1), at(1));
    let denominator = l - 2.0 * value + r;
    let shift = if denominator < 0.0 { 0.5 * (l - r) / denominator } else { 0.0 };
    (value, ((i as f64 + shift.clamp(-0.5, 0.5)) / PHASE_BINS as f64).rem_euclid(1.0))
}

/// How sharply the envelope lines up at `bpm`: the peak of the fold over
/// its average.
fn sharpness(envelope: &[f32], step: f64, bpm: f64) -> (f64, f64) {
    let bins = fold(envelope, 60.0 / bpm / step);
    let mean = bins.iter().sum::<f64>() / PHASE_BINS as f64;
    let (top, phase) = peak(&bins);
    (if mean > 0.0 { top / mean } else { 0.0 }, phase)
}

/// The BPM near `rough` (within ±2 %) whose fold is sharpest, in steps of
/// 0.01, preferring a whole number when it is about as sharp.
fn exact_bpm(envelope: &[f32], step: f64, rough: f64) -> f64 {
    let candidates = |from: f64, to: f64, by: f64| {
        let n = ((to - from) / by).round() as usize;
        (0..=n).map(move |i| from + i as f64 * by)
    };
    let best_of = |list: &mut dyn Iterator<Item = f64>| {
        list.map(|bpm| (sharpness(envelope, step, bpm).0, bpm))
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .unwrap_or((0.0, rough))
    };
    // Coarse, then fine around the best.
    let (_, coarse) = best_of(&mut candidates(rough * 0.98, rough * 1.02, 0.05));
    let (score, fine) = best_of(&mut candidates(coarse - 0.06, coarse + 0.06, 0.01));
    let whole = fine.round();
    let (whole_score, _) = sharpness(envelope, step, whole);
    if (whole - fine).abs() <= 0.1 && whole_score >= 0.97 * score {
        whole
    } else {
        (fine * 100.0).round() / 100.0
    }
}

/// The track's grid, for a track of one tempo; `None` when it is too short
/// or has no steady beat.
pub fn beat_grid(samples: &[f32], rate: u32, range: (f64, f64)) -> Option<Grid> {
    let mono = mono(samples);
    let env = envelopes(&mono, rate);
    if env.full.iter().all(|&v| v <= 0.0) {
        return None;
    }
    let rough = 60.0 / (rough_period(&env.full, env.step, range)? * env.step);
    // Syncopated patterns (drum & bass kicks every 1.5 beats) can make two
    // thirds or three halves of the tempo look as likely: the candidate
    // that lines the whole track up most sharply wins.
    let bpm = [rough, rough * 1.5, rough / 1.5]
        .into_iter()
        .filter(|&b| (range.0..=range.1).contains(&b))
        .map(|b| exact_bpm(&env.full, env.step, b))
        .map(|b| (sharpness(&env.full, env.step, b).0, b))
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, b)| b)?;
    let beat = 60.0 / bpm;
    let (_, phase) = sharpness(&env.full, env.step, bpm);
    let first_beat = (phase * beat + env.offset - ENVELOPE_DELAY).rem_euclid(beat);

    // The downbeat: which of four beats has the most bass onset.
    let mut by_beat = [0.0f64; 4];
    let reach = (0.02 / env.step).round() as isize;
    let beats = (env.bass.len() as f64 * env.step / beat) as usize;
    for k in 0..beats {
        let time = first_beat + k as f64 * beat + ENVELOPE_DELAY - env.offset;
        let centre = (time / env.step).round() as isize;
        let around = |e: &[f32]| -> f64 {
            (centre - reach..=centre + reach)
                .filter_map(|i| usize::try_from(i).ok().and_then(|i| e.get(i)))
                .map(|&v| f64::from(v))
                .sum()
        };
        // Bass only: adding the full band lets snares on 2 and 4 win.
        by_beat[k % 4] += around(&env.bass);
    }
    let downbeat = (0..4).max_by(|&a, &b| by_beat[a].total_cmp(&by_beat[b])).unwrap_or(0);
    // The first beat's number when beat `downbeat` (0-based) is a "1".
    let first_number = ((4 - downbeat) % 4 + 1) as u8;
    Some(Grid { bpm, first_beat, first_number })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clicks at `bpm` from `offset`, the first of every four louder and
    /// with more bass, as interleaved stereo.
    fn clicks(bpm: f64, offset: f64, seconds: f64, rate: u32) -> Vec<f32> {
        let n = (seconds * f64::from(rate)) as usize;
        let mut out = vec![0.0f32; n * 2];
        let beat = 60.0 / bpm;
        let mut k = 0;
        loop {
            let t = offset + k as f64 * beat;
            let start = (t * f64::from(rate)) as usize;
            if start >= n {
                break;
            }
            let bar_start = k % 4 == 0;
            for i in 0..(f64::from(rate) * 0.05) as usize {
                let Some(frame) = out.get_mut((start + i) * 2..(start + i) * 2 + 2) else { break };
                let s = i as f32 / rate as f32;
                let decay = (-s * 60.0).exp();
                let bass =
                    if bar_start { 0.8 } else { 0.2 } * (s * 55.0 * std::f32::consts::TAU).sin();
                let click = 0.3 * (s * 3000.0 * std::f32::consts::TAU).sin();
                let v = (bass + click) * decay;
                frame[0] = v;
                frame[1] = v;
            }
            k += 1;
        }
        out
    }

    #[test]
    fn finds_tempo_phase_and_downbeat_of_clicks() {
        let rate = 44_100;
        for (bpm, offset) in [(174.0, 0.12), (128.0, 0.3), (140.0, 0.05)] {
            let grid = beat_grid(&clicks(bpm, offset, 60.0, rate), rate, DEFAULT_RANGE).unwrap();
            assert_eq!(grid.bpm, bpm, "{grid:?}");
            let beat = 60.0 / bpm;
            let error = (grid.first_beat - offset.rem_euclid(beat) + beat / 2.0).rem_euclid(beat)
                - beat / 2.0;
            assert!(error.abs() < 0.02, "{bpm}: {grid:?}");
            // The first click (at `offset`, under one beat) is a downbeat.
            assert_eq!(grid.first_number, 1, "{bpm}: {grid:?}");
        }
    }

    #[test]
    fn silence_has_no_grid() {
        assert_eq!(
            beat_grid(&vec![0.0; 44_100 * 2 * 30], 44_100, DEFAULT_RANGE).map(|g| g.bpm),
            None
        );
    }
}
