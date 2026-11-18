//! Changing a whole track's sample rate (converting tracks, e.g. 96 kHz to
//! 44.1 kHz), with a windowed-sinc filter.
//!
//! `make_sincs`, `calculate_cutoff` (and its fitted constants) and `sinc`
//! are copied from rubato 5.1.0 (<https://github.com/HEnquist/rubato>),
//! adapted to `f64` and to this file; under rubato's MIT license:
//!
//! > Copyright (c) 2020 Henrik Enquist
//! >
//! > Permission is hereby granted, free of charge, to any person obtaining a
//! > copy of this software and associated documentation files (the
//! > "Software"), to deal in the Software without restriction, including
//! > without limitation the rights to use, copy, modify, merge, publish,
//! > distribute, sublicense, and/or sell copies of the Software, and to
//! > permit persons to whom the Software is furnished to do so, subject to
//! > the following conditions:
//! >
//! > The above copyright notice and this permission notice shall be included
//! > in all copies or substantial portions of the Software.
//! >
//! > THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
//! > OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
//! > MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
//! > IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
//! > CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
//! > TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
//! > SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//!
//! The window (Blackman-Harris, squared, as rubato's `BlackmanHarris2`) is
//! the textbook formula; the driver going through a whole track is ours.

/// Taps of the filter (each side half of them).
const SINC_LEN: usize = 128;
/// Steps between two input samples the filter is tabulated at; the
/// nearest one is used (timing off by at most 1/2048 of a sample).
const OVERSAMPLING: usize = 1024;

/// sinc(x) = sin(πx)/(πx). From rubato.
fn sinc(value: f64) -> f64 {
    if value == 0.0 {
        1.0
    } else {
        (value * std::f64::consts::PI).sin() / (value * std::f64::consts::PI)
    }
}

/// Blackman-Harris window of `npoints` (periodic), squared.
fn blackman_harris2(npoints: usize) -> Vec<f64> {
    let (a0, a1, a2, a3) = (0.35875, 0.48829, 0.14128, 0.01168);
    let tau = std::f64::consts::TAU;
    (0..npoints)
        .map(|n| {
            let x = n as f64 / npoints as f64;
            let w = a0 - a1 * (tau * x).cos() + a2 * (2.0 * tau * x).cos()
                - a3 * (3.0 * tau * x).cos();
            w * w
        })
        .collect()
}

/// The relative cutoff that suits a filter of `npoints` with the squared
/// Blackman-Harris window. From rubato (its fitted constants).
fn calculate_cutoff(npoints: usize) -> f64 {
    let (k1, k2, k3) = (13.745202940783823, 121.73532586374934, 5964.163279612051);
    let n = npoints as f64;
    1.0 / (k1 / n + k2 / (n * n) + k3 / (n * n * n) + 1.0)
}

/// The windowed sinc tabulated at `factor` sub-positions: `factor` rows of
/// `npoints`. From rubato.
fn make_sincs(npoints: usize, factor: usize, f_cutoff: f64) -> Vec<Vec<f32>> {
    let totpoints = npoints * factor;
    let window = blackman_harris2(totpoints);
    let mut y = Vec::with_capacity(totpoints);
    let mut sum = 0.0;
    for (x, w) in window.iter().enumerate() {
        let val = w * sinc((x as f64 - (totpoints / 2) as f64) * f_cutoff / factor as f64);
        sum += val;
        y.push(val);
    }
    sum /= factor as f64;
    let mut sincs = vec![vec![0f32; npoints]; factor];
    for p in 0..npoints {
        for n in 0..factor {
            sincs[factor - n - 1][p] = (y[factor * p + n] / sum) as f32;
        }
    }
    sincs
}

/// `samples` (interleaved, `channels` of them) at `from` Hz, at `to` Hz.
pub fn resample(samples: &[f32], channels: usize, from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() || channels == 0 {
        return samples.to_vec();
    }
    let ratio = f64::from(to) / f64::from(from);
    // Down: below the new Nyquist frequency, so nothing folds back.
    let cutoff = calculate_cutoff(SINC_LEN) * ratio.min(1.0);
    let sincs = make_sincs(SINC_LEN, OVERSAMPLING, cutoff);
    let frames_in = samples.len() / channels;
    let frames_out = (frames_in as f64 * ratio).round() as usize;
    // Each channel apart, with silence around it for the filter's reach.
    let pad = SINC_LEN;
    let mut out = vec![0f32; frames_out * channels];
    let mut wave = vec![0f32; frames_in + 2 * pad];
    for channel in 0..channels {
        for (frame, slot) in wave[pad..pad + frames_in].iter_mut().enumerate() {
            *slot = samples[frame * channels + channel];
        }
        for frame in 0..frames_out {
            // Where it falls between input samples; the table's rows are a
            // sub-position apart (row `s` is `(s + 1) / factor` along).
            let t = frame as f64 / ratio - 1.0 / OVERSAMPLING as f64;
            let index = t.floor();
            let sub = ((t - index) * OVERSAMPLING as f64).round() as usize;
            let (index, sub) = if sub >= OVERSAMPLING {
                (index as isize + 1, sub - OVERSAMPLING)
            } else {
                (index as isize, sub)
            };
            let start = index + 1 - (SINC_LEN / 2) as isize + pad as isize;
            let Ok(start) = usize::try_from(start) else { continue };
            let Some(taps) = wave.get(start..start + SINC_LEN) else { continue };
            let value: f32 = taps.iter().zip(&sincs[sub]).map(|(x, h)| x * h).sum();
            out[frame * channels + channel] = value;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, rate: u32, secs: f64) -> Vec<f32> {
        (0..(f64::from(rate) * secs) as usize)
            .flat_map(|i| {
                let v = (std::f64::consts::TAU * hz * i as f64 / f64::from(rate)).sin() as f32;
                [v * 0.5, v * 0.5]
            })
            .collect()
    }

    /// How strongly `hz` is in the left channel (a single DFT bin).
    fn level(samples: &[f32], rate: u32, hz: f64) -> f64 {
        let left: Vec<f64> = samples.iter().step_by(2).map(|&v| f64::from(v)).collect();
        let (mut re, mut im) = (0.0, 0.0);
        for (i, v) in left.iter().enumerate() {
            let phase = std::f64::consts::TAU * hz * i as f64 / f64::from(rate);
            re += v * phase.cos();
            im += v * phase.sin();
        }
        (re * re + im * im).sqrt() * 2.0 / left.len() as f64
    }

    #[test]
    fn rubato_sincs_sum_to_the_factor() {
        // As rubato's own test: each sub-position's taps sum to one.
        let sincs = make_sincs(32, 8, 0.9);
        let sum: f64 = sincs.iter().map(|v| v.iter().map(|&x| f64::from(x)).sum::<f64>()).sum();
        assert!((sum - 8.0).abs() < 1e-4, "{sum}");
        assert!((calculate_cutoff(128) - 0.894).abs() < 0.001);
    }

    #[test]
    fn a_tone_keeps_its_pitch_and_level() {
        let out = resample(&tone(1000.0, 96_000, 1.0), 2, 96_000, 44_100);
        assert_eq!(out.len(), 44_100 * 2);
        let found = level(&out, 44_100, 1000.0);
        assert!((found - 0.5).abs() < 0.01, "{found}");
        let up = resample(&tone(1000.0, 44_100, 1.0), 2, 44_100, 48_000);
        assert!((level(&up, 48_000, 1000.0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn what_the_new_rate_cant_hold_is_filtered_out() {
        // 30 kHz at 96 kHz has no place at 44.1 kHz: it mustn't fold back
        // (to 14.1 kHz).
        let out = resample(&tone(30_000.0, 96_000, 0.5), 2, 96_000, 44_100);
        assert!(level(&out, 44_100, 14_100.0) < 0.001);
    }
}
