//! Master Tempo: changing the tempo without changing the key.
//!
//! WSOLA (waveform-similarity overlap-add). The output is built from short
//! grains of the original audio played at their natural speed, overlapped by
//! half and cross-faded. The playback position advances at the deck's tempo,
//! so faster playback skips audio between grains and slower playback repeats
//! some. Each new grain starts where it best matches the audio the previous
//! grain would have continued with, within a small window around the
//! playback position, which avoids phase cancellation in the cross-fade.

use serde::{Deserialize, Serialize};

use crate::decode::Frames;

/// Grain length in seconds; grains overlap by half.
const GRAIN_SECS: f64 = 0.04;
/// How far a grain may start from the playback position, in seconds.
const SEARCH_SECS: f64 = 0.012;
/// Every n-th frame is compared while searching, to keep the search cheap.
const COMPARE_STRIDE: usize = 4;
/// Candidate starts tried in the coarse pass, before refining around the best.
const COARSE_STEP: i64 = 8;

/// Where a [`Stretcher`] is in its grains.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct StretchState {
    current: Option<f64>,
    previous: f64,
    offset: usize,
}

#[derive(Debug)]
pub struct Stretcher {
    /// Output frames per hop (half a grain).
    hop: usize,
    /// Rising half of a sin² window; the falling half is `1 - rise`.
    rise: Vec<f32>,
    /// Source frame where the grain fading in started, `None` when idle.
    current: Option<f64>,
    /// Source frame where the grain fading out started.
    previous: f64,
    /// Output frames into the current hop.
    offset: usize,
}

impl Stretcher {
    /// For an output stream at `output_rate`; allocates, so call it outside
    /// the audio callback.
    pub fn new(output_rate: u32) -> Self {
        let hop = ((f64::from(output_rate) * GRAIN_SECS / 2.0) as usize).max(16);
        let rise = (0..hop)
            .map(|i| {
                let x = (i as f32 + 0.5) / hop as f32 * std::f32::consts::FRAC_PI_2;
                x.sin() * x.sin()
            })
            .collect();
        Self { hop, rise, current: None, previous: 0.0, offset: 0 }
    }

    /// Where it is in its grains.
    pub(crate) fn state(&self) -> StretchState {
        StretchState { current: self.current, previous: self.previous, offset: self.offset }
    }

    pub(crate) fn restore(&mut self, state: StretchState) {
        (self.current, self.previous, self.offset) = (state.current, state.previous, state.offset);
    }

    /// Forgets the grain in progress, e.g. after the stretcher was bypassed.
    pub fn reset(&mut self) {
        self.current = None;
    }

    /// The next output frame. `position` is the deck's playback position in
    /// source frames, and `step` is source frames per output frame at the
    /// original tempo (the ratio of the sample rates).
    pub fn next<A: Frames + ?Sized>(
        &mut self,
        audio: &A,
        position: f64,
        step: f64,
        cubic: bool,
    ) -> [f32; 2] {
        let current = match self.current {
            Some(current) if self.offset < self.hop => current,
            Some(current) => {
                self.previous = current;
                self.offset = 0;
                let start =
                    self.best_start(audio, current + self.hop as f64 * step, position, step);
                self.current = Some(start);
                start
            }
            None => {
                // Start as if the previous grain had led up to `position`.
                self.previous = position - self.hop as f64 * step;
                self.offset = 0;
                self.current = Some(position);
                position
            }
        };
        let i = self.offset;
        let fading_out = audio.sample_at(self.previous + (self.hop + i) as f64 * step, cubic);
        let fading_in = audio.sample_at(current + i as f64 * step, cubic);
        let rise = self.rise[i];
        self.offset += 1;
        [
            fading_out[0] * (1.0 - rise) + fading_in[0] * rise,
            fading_out[1] * (1.0 - rise) + fading_in[1] * rise,
        ]
    }

    /// The start near `target` whose audio best matches `continuation`, the
    /// audio that would have followed the previous grain.
    fn best_start<A: Frames + ?Sized>(
        &self,
        audio: &A,
        continuation: f64,
        target: f64,
        step: f64,
    ) -> f64 {
        let reach = (SEARCH_SECS * f64::from(audio.rate())) as i64;
        let score =
            |offset: i64| self.similarity(audio, continuation, target + offset as f64, step);
        let mut best = (0, score(0));
        let mut offset = -reach;
        while offset <= reach {
            let s = score(offset);
            if s > best.1 {
                best = (offset, s);
            }
            offset += COARSE_STEP;
        }
        let coarse = best.0;
        for offset in (coarse - COARSE_STEP + 1)..(coarse + COARSE_STEP) {
            let s = score(offset);
            if s > best.1 {
                best = (offset, s);
            }
        }
        target + best.0 as f64
    }

    /// Normalised correlation of the mono audio over one hop.
    fn similarity<A: Frames + ?Sized>(&self, audio: &A, a: f64, b: f64, step: f64) -> f32 {
        let (mut dot, mut energy) = (0f32, 0f32);
        for i in (0..self.hop).step_by(COMPARE_STRIDE) {
            let x = mono(audio, a + i as f64 * step);
            let y = mono(audio, b + i as f64 * step);
            dot += x * y;
            energy += y * y;
        }
        dot / (energy + 1e-9).sqrt()
    }
}

fn mono<A: Frames + ?Sized>(audio: &A, position: f64) -> f32 {
    let [l, r] = audio.frame(position.round() as i64);
    l + r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::TrackAudio;

    fn sine(rate: u32, hz: f64, secs: f64) -> TrackAudio {
        let frames = (f64::from(rate) * secs) as usize;
        let samples = (0..frames)
            .flat_map(|i| {
                let v = (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin() as f32;
                [v, v]
            })
            .collect();
        TrackAudio { sample_rate: rate, samples }
    }

    /// Zero crossings per second of the left channel.
    fn pitch(out: &[[f32; 2]], rate: u32) -> f64 {
        let crossings = out.windows(2).filter(|w| (w[0][0] < 0.0) != (w[1][0] < 0.0)).count();
        crossings as f64 / 2.0 / (out.len() as f64 / f64::from(rate))
    }

    #[test]
    fn keeps_the_pitch_at_other_tempos() {
        let rate = 44_100;
        let audio = sine(rate, 440.0, 4.0);
        for tempo in [0.8, 1.0, 1.25] {
            let mut stretcher = Stretcher::new(rate);
            let mut position = 0.0;
            let mut out = Vec::new();
            for _ in 0..rate {
                out.push(stretcher.next(&audio, position, 1.0, true));
                position += tempo;
            }
            let hz = pitch(&out[4410..], rate);
            assert!((hz - 440.0).abs() < 4.0, "tempo {tempo}: {hz} Hz");
            // Overlapping grains of a steady tone keep a steady level.
            let quiet = out[4410..].windows(200).filter(|w| w.iter().all(|s| s[0].abs() < 0.3));
            assert_eq!(quiet.count(), 0, "tempo {tempo}: the cross-fades cancel out");
        }
    }

    #[test]
    fn converts_the_sample_rate_without_changing_the_pitch() {
        let audio = sine(48_000, 440.0, 2.0);
        let mut stretcher = Stretcher::new(44_100);
        let step = 48_000.0 / 44_100.0;
        let mut position = 0.0;
        let mut out = Vec::new();
        for _ in 0..44_100 {
            out.push(stretcher.next(&audio, position, step, true));
            position += step * 1.1;
        }
        let hz = pitch(&out[4410..], 44_100);
        assert!((hz - 440.0).abs() < 4.0, "{hz} Hz");
    }
}
