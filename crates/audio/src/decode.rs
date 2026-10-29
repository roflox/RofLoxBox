//! Decoding audio files into memory.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

/// A decoded track: interleaved stereo samples at the file's sample rate.
#[derive(Debug, Clone)]
pub struct TrackAudio {
    pub sample_rate: u32,
    /// `[left, right, left, right, …]`
    pub samples: Vec<f32>,
}

impl TrackAudio {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / f64::from(self.sample_rate)
    }

    /// The signal at a fractional frame position: linear or cubic (Hermite)
    /// interpolation between the neighbouring frames.
    pub fn sample_at(&self, position: f64, cubic: bool) -> [f32; 2] {
        let base = position.floor();
        let t = (position - base) as f32;
        let i = base as i64;
        let (b, c) = (self.frame(i), self.frame(i + 1));
        if !cubic {
            return [b[0] + (c[0] - b[0]) * t, b[1] + (c[1] - b[1]) * t];
        }
        let (a, d) = (self.frame(i - 1), self.frame(i + 2));
        std::array::from_fn(|ch| {
            let (a, b, c, d) = (a[ch], b[ch], c[ch], d[ch]);
            let c1 = 0.5 * (c - a);
            let c2 = a - 2.5 * b + 2.0 * c - 0.5 * d;
            let c3 = 0.5 * (d - a) + 1.5 * (b - c);
            ((c3 * t + c2) * t + c1) * t + b
        })
    }

    /// The stereo frame at `index`, or silence outside the track.
    pub fn frame(&self, index: i64) -> [f32; 2] {
        if index < 0 {
            return [0.0; 2];
        }
        let at = index as usize * 2;
        match self.samples.get(at..at + 2) {
            Some(frame) => [frame[0], frame[1]],
            None => [0.0; 2],
        }
    }
}

/// Decodes a whole file. Mono is duplicated to both channels; channels beyond
/// the first two are dropped.
pub fn decode_file(path: &Path) -> Result<TrackAudio> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .with_context(|| format!("unsupported audio file {}", path.display()))?;

    let track = format
        .first_track_known_codec(TrackType::Audio)
        .ok_or_else(|| anyhow!("no audio track in {}", path.display()))?;
    let track_id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.clone() else {
        return Err(anyhow!("no audio codec parameters in {}", path.display()));
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &decoder_options())
        .context("no decoder for this codec")?;

    let mut sample_rate = params.sample_rate.unwrap_or(44_100);
    let mut samples = Vec::new();
    let mut interleaved: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(err) => return Err(err).context("reading audio"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buffer = match decoder.decode(&packet) {
            Ok(buffer) => buffer,
            // A damaged packet is skipped rather than ending the track.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(err) => return Err(err).context("decoding audio"),
        };
        sample_rate = buffer.spec().rate();
        let channels = buffer.spec().channels().count().max(1);
        buffer.copy_to_vec_interleaved(&mut interleaved);
        samples.reserve(buffer.frames() * 2);
        for frame in interleaved.chunks_exact(channels) {
            let left = frame[0];
            let right = if channels > 1 { frame[1] } else { left };
            samples.push(left);
            samples.push(right);
        }
    }
    Ok(TrackAudio { sample_rate, samples })
}

/// Peak amplitude per 1/150 s, the resolution of rekordbox's detail
/// waveforms; used when a track has no analysis files.
pub fn peak_waveform(audio: &TrackAudio) -> Vec<f32> {
    let step = (audio.sample_rate as usize / 150).max(1);
    audio
        .samples
        .chunks(step * 2)
        .map(|chunk| chunk.iter().fold(0f32, |peak, s| peak.max(s.abs())))
        .collect()
}

/// Peak level of the low (< 200 Hz), mid and high (> 2.5 kHz) bands per
/// 1/150 s, each 0.0–1.0 relative to the loudest column; used for 3-band
/// waveforms when a track has no such analysis.
pub fn band_waveform(audio: &TrackAudio) -> Vec<[f32; 3]> {
    // One-pole low-passes are enough for a display.
    let coefficient =
        |cutoff: f32| 1.0 - (-2.0 * std::f32::consts::PI * cutoff / audio.sample_rate as f32).exp();
    let (a_low, a_high) = (coefficient(200.0), coefficient(2_500.0));
    let (mut low, mut below_high) = (0f32, 0f32);
    let step = (audio.sample_rate as usize / 150).max(1);
    let mut columns = Vec::with_capacity(audio.frames() / step + 1);
    let mut peak = [0f32; 3];
    for (i, frame) in audio.samples.chunks_exact(2).enumerate() {
        let x = (frame[0] + frame[1]) * 0.5;
        low += a_low * (x - low);
        below_high += a_high * (x - below_high);
        let bands = [low, below_high - low, x - below_high];
        for (p, b) in peak.iter_mut().zip(bands) {
            *p = p.max(b.abs());
        }
        if (i + 1) % step == 0 {
            columns.push(std::mem::take(&mut peak));
        }
    }
    let max = columns.iter().flatten().copied().fold(1e-6f32, f32::max);
    for column in &mut columns {
        for band in column.iter_mut() {
            *band = (*band / max).sqrt();
        }
    }
    columns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_hits_frames_and_follows_curves() {
        let samples: Vec<f32> = (0..8).flat_map(|i| [(i * i) as f32, -(i as f32)]).collect();
        let audio = TrackAudio { sample_rate: 1, samples };
        for cubic in [false, true] {
            assert_eq!(audio.sample_at(3.0, cubic), [9.0, -3.0]);
            assert_eq!(audio.sample_at(2.5, cubic)[1], -2.5);
        }
        // x² between 2 and 3 is 6.25 at 2.5: cubic gets it, linear says 6.5.
        assert_eq!(audio.sample_at(2.5, false)[0], 6.5);
        assert!((audio.sample_at(2.5, true)[0] - 6.25).abs() < 1e-6);
    }

    fn tone(freq: f32) -> TrackAudio {
        let rate = 44_100;
        let samples = (0..rate)
            .flat_map(|i| {
                let s = (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.5;
                [s, s]
            })
            .collect();
        TrackAudio { sample_rate: rate, samples }
    }

    fn loudest_band(audio: &TrackAudio) -> usize {
        let columns = band_waveform(audio);
        let sums = columns[20..]
            .iter()
            .fold([0f32; 3], |acc, c| [acc[0] + c[0], acc[1] + c[1], acc[2] + c[2]]);
        (0..3).max_by(|&a, &b| sums[a].total_cmp(&sums[b])).unwrap()
    }

    #[test]
    fn tones_land_in_their_band() {
        assert_eq!(loudest_band(&tone(60.0)), 0);
        assert_eq!(loudest_band(&tone(800.0)), 1);
        assert_eq!(loudest_band(&tone(9_000.0)), 2);
        assert_eq!(band_waveform(&tone(60.0)).len(), 150);
    }
}

/// Encoder delay and padding stay in, as rekordbox decodes: its cue points,
/// beat grids and waveforms count from the untrimmed start, and the length
/// (which sets how the waveform columns line up) includes the padding.
fn decoder_options() -> AudioDecoderOptions {
    let mut options = AudioDecoderOptions::default();
    options.gapless = false;
    options
}
