//! EDIT's waveforms in detail, as a DAW draws them: each track's own audio,
//! its peaks light and its loudness (RMS) solid, filled from the middle, in
//! the clip's colour. Worked out once per track at every zoom (two bytes
//! per 16 frames, about 1.5 MB for a four-minute track) and placed where the
//! deck played it. What the mix did to it (EQ, fader) has lanes of its own
//! and doesn't change it.

use std::sync::Arc;

use deckhand_audio::TrackAudio;
use eframe::egui::{Color32, Mesh, Rect, pos2};

/// Frames per bin at the finest level; each level up is `FAN` times
/// coarser.
const FINEST: usize = 16;
const FAN: usize = 4;

/// Behind the waveform, so it reads on any clip's colour.
const LANE: Color32 = Color32::from_black_alpha(150);

/// A track's peak and RMS per bin, finest first, kept as bytes (the square
/// root of the level, 0–255, finer where it is quiet).
pub(super) struct TrackPeaks {
    rate: f64,
    levels: Vec<Vec<[u8; 2]>>,
    /// The loudest peak in the track: levels are relative to it.
    loudest: f32,
}

fn byte(level: f32) -> u8 {
    (level.clamp(0.0, 1.0).sqrt() * 255.0).round() as u8
}

fn level(b: u8) -> f32 {
    let v = f32::from(b) / 255.0;
    v * v
}

impl TrackPeaks {
    pub(super) fn of(audio: &TrackAudio) -> Self {
        let rate = f64::from(audio.sample_rate.max(1));
        let mut finest = Vec::with_capacity(audio.frames() / FINEST + 1);
        let mut loudest = 0f32;
        for bin in audio.to_f32().chunks(FINEST * 2) {
            let (mut peak, mut energy) = (0f32, 0f32);
            for pair in bin.chunks_exact(2) {
                let x = (pair[0] + pair[1]) * 0.5;
                peak = peak.max(x.abs());
                energy += x * x;
            }
            loudest = loudest.max(peak);
            let rms = (energy / (bin.len() / 2).max(1) as f32).sqrt();
            finest.push([byte(peak), byte(rms)]);
        }
        let mut levels = vec![finest];
        while levels.last().is_some_and(|l| l.len() > 1) {
            // Further up: the loudest peak, the RMS over the whole bin.
            let up = levels.last().expect("one at least").chunks(FAN).map(|bins| {
                let peak = bins.iter().map(|b| b[0]).max().unwrap_or(0);
                let energy: f32 = bins.iter().map(|b| level(b[1]).powi(2)).sum();
                [peak, byte((energy / bins.len() as f32).sqrt())]
            });
            levels.push(up.collect());
        }
        Self { rate, levels, loudest: loudest.max(1e-6) }
    }

    /// The peak and the RMS from `a` to `b` seconds into the track (in
    /// either order), relative to the track's loudest peak (0–1), from the
    /// coarsest level that still has a few bins there.
    pub(super) fn levels(&self, a: f64, b: f64) -> (f32, f32) {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let (from, to) = ((a * self.rate).max(0.0) as usize, (b * self.rate).max(0.0) as usize + 1);
        let span = to - from;
        let mut size = FINEST;
        let mut k = 0;
        while k + 1 < self.levels.len() && size * FAN * 2 <= span {
            size *= FAN;
            k += 1;
        }
        let bins = &self.levels[k];
        let (first, last) = (from / size, (to.saturating_sub(1) / size).min(bins.len().saturating_sub(1)));
        let (mut peak, mut rms) = (0u8, 0u8);
        for b in bins.get(first..=last).unwrap_or(&[]) {
            (peak, rms) = (peak.max(b[0]), rms.max(b[1]));
        }
        ((level(peak) / self.loudest).min(1.0), (level(rms) / self.loudest).min(1.0))
    }
}

/// One column of a waveform: the track's peak and RMS there (0–1).
#[derive(Clone, Copy, Default)]
pub(super) struct Column {
    pub peak: f32,
    pub rms: f32,
}

/// The waveform's columns from `left`, one per point, as a mesh on a dark
/// lane in `color`: the peaks light, the RMS solid over them, each filled
/// from the middle.
pub(super) fn mesh(columns: &[Column], left: f32, middle: f32, half: f32, color: Color32) -> Arc<Mesh> {
    let mut mesh = Mesh::default();
    mesh.add_colored_rect(
        Rect::from_min_max(pos2(left, middle - half - 1.0), pos2(left + columns.len() as f32, middle + half + 1.0)),
        LANE,
    );
    let (light, solid) = (color.gamma_multiply(0.55), color.lerp_to_gamma(Color32::WHITE, 0.25));
    let bar = |mesh: &mut Mesh, x: f32, h: f32, c: Color32| {
        if h >= 0.25 {
            mesh.add_colored_rect(Rect::from_min_max(pos2(x, middle - h), pos2(x + 1.0, middle + h)), c);
        }
    };
    for (i, c) in columns.iter().enumerate() {
        let x = left + i as f32;
        bar(&mut mesh, x, c.peak.min(1.0) * half, light);
        bar(&mut mesh, x, c.rms.min(1.0) * half, solid);
    }
    Arc::new(mesh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, secs: f64) -> TrackAudio {
        let rate = 48_000;
        let n = (secs * f64::from(rate)) as usize;
        let samples = (0..n)
            .flat_map(|i| {
                let v = (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin() as f32 * 0.8;
                [v, v]
            })
            .collect();
        TrackAudio::new(rate, samples)
    }

    /// A steady sine: its peak full at any zoom (a cycle or more), its RMS
    /// 1/√2 of it wherever a bin holds whole cycles.
    #[test]
    fn peak_and_rms_at_any_zoom() {
        let peaks = TrackPeaks::of(&tone(60.0, 2.0));
        for (a, b) in [(1.0, 1.0 + 1.0 / 60.0), (1.0, 1.1), (0.5, 1.5)] {
            let (peak, rms) = peaks.levels(a, b);
            assert!(peak > 0.97, "{a}..{b}: {peak}");
            if b - a >= 0.1 {
                assert!((rms - 0.707).abs() < 0.06, "{a}..{b}: {rms}");
            }
        }
        // Zoomed right in, part of a cycle: less than its peak.
        let (part, _) = peaks.levels(1.0, 1.0 + 0.1 / 60.0);
        assert!(part < 0.7, "{part}");
    }

    /// A click in silence shows where it is, even zoomed in to a few
    /// hundred frames.
    #[test]
    fn a_transient_stands_out() {
        let mut samples = vec![0.0; 96_000 * 2];
        for i in 48_000..48_040 {
            samples[i * 2] = 0.9;
            samples[i * 2 + 1] = 0.9;
        }
        let audio = TrackAudio::new(48_000, samples);
        let peaks = TrackPeaks::of(&audio);
        let at = |s: f64| peaks.levels(s, s + 0.002).0;
        assert!(at(1.0) > 0.9, "{}", at(1.0));
        assert!(at(0.99) < 0.01 && at(1.01) < 0.01, "{} {}", at(0.99), at(1.01));
    }
}
