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

/// A decoded track: interleaved stereo samples at the file's sample rate,
/// held as the file has them (see [`Depth`]): a 16-bit WAV takes half the
/// memory it would as floats, with every sample as it was.
#[derive(Debug, Clone)]
pub struct TrackAudio {
    pub sample_rate: u32,
    /// `[left, right, left, right, …]`
    samples: Samples,
    /// How many there are (both channels), kept: asked for on every frame.
    len: usize,
}

/// How a track's samples are held, after its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// 16-bit files, and lossy ones (MP3, AAC…: they have no bit depth, and
    /// their detail is far coarser than 16 bits).
    Int16,
    /// 24-bit files: three bytes a sample.
    Int24,
    /// Floating-point files (and audio made by the app).
    Float,
}

#[derive(Debug, Clone)]
enum Samples {
    Float(Vec<f32>),
    /// Times `step`, a sample's value: full scale over 32768, or more for a
    /// decoded lossy file louder than full scale (so it isn't cut off).
    Int16 { data: Vec<i16>, step: f32 },
    /// Little-endian, three bytes each, full scale.
    Int24(Vec<u8>),
}

const I16_FULL: f32 = 32768.0;
const I24_FULL: f32 = 8_388_608.0;

/// A 24-bit little-endian sample as a float.
#[inline]
fn int24(b: &[u8]) -> f32 {
    (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 * (1.0 / I24_FULL)
}

impl TrackAudio {
    /// Audio as floats, held as they are.
    pub fn new(sample_rate: u32, samples: Vec<f32>) -> Self {
        let len = samples.len();
        Self { sample_rate, samples: Samples::Float(samples), len }
    }

    /// Audio held at `depth`: from a file of that depth, every sample
    /// comes back as it was decoded.
    pub fn with_depth(sample_rate: u32, samples: Vec<f32>, depth: Depth) -> Self {
        let len = samples.len();
        let samples = match depth {
            Depth::Float => Samples::Float(samples),
            Depth::Int16 => {
                let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
                let scale = peak.max(1.0);
                let data = samples
                    .iter()
                    .map(|&s| (s / scale * I16_FULL).round().clamp(-I16_FULL, I16_FULL - 1.0) as i16)
                    .collect();
                Samples::Int16 { data, step: scale / I16_FULL }
            }
            Depth::Int24 => {
                let mut data = Vec::with_capacity(samples.len() * 3);
                for &s in &samples {
                    let v = (s * I24_FULL).round().clamp(-I24_FULL, I24_FULL - 1.0) as i32;
                    data.extend_from_slice(&v.to_le_bytes()[..3]);
                }
                Samples::Int24(data)
            }
        };
        Self { sample_rate, samples, len }
    }

    /// How its samples are held.
    pub fn depth(&self) -> Depth {
        match self.samples {
            Samples::Float(_) => Depth::Float,
            Samples::Int16 { .. } => Depth::Int16,
            Samples::Int24(_) => Depth::Int24,
        }
    }

    /// Samples (both channels): twice the frames.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The sample at `i` (interleaved: frame × 2 + channel).
    pub fn sample(&self, i: usize) -> f32 {
        match &self.samples {
            Samples::Float(s) => s[i],
            Samples::Int16 { data, step } => f32::from(data[i]) * step,
            Samples::Int24(data) => int24(&data[i * 3..i * 3 + 3]),
        }
    }

    /// Every sample as floats (interleaved): borrowed when held as floats,
    /// otherwise a copy for the while it's needed (analysing, exporting).
    pub fn to_f32(&self) -> std::borrow::Cow<'_, [f32]> {
        match &self.samples {
            Samples::Float(s) => std::borrow::Cow::Borrowed(s),
            _ => std::borrow::Cow::Owned((0..self.len()).map(|i| self.sample(i)).collect()),
        }
    }

    /// The memory its samples take, in bytes.
    pub fn bytes(&self) -> usize {
        match &self.samples {
            Samples::Float(s) => s.len() * 4,
            Samples::Int16 { data, .. } => data.len() * 2,
            Samples::Int24(data) => data.len(),
        }
    }

    pub fn frames(&self) -> usize {
        self.len() / 2
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / f64::from(self.sample_rate)
    }

    /// The signal at a fractional frame position: linear or cubic (Hermite)
    /// interpolation between the neighbouring frames.
    pub fn sample_at(&self, position: f64, cubic: bool) -> [f32; 2] {
        Frames::sample_at(self, position, cubic)
    }

    /// The stereo frame at `index`, or silence outside the track.
    #[inline(always)]
    pub fn frame(&self, index: i64) -> [f32; 2] {
        if index < 0 {
            return [0.0; 2];
        }
        // The deck's hot path: one look at how it is held, per frame.
        let at = index as usize * 2;
        match &self.samples {
            Samples::Float(s) => match s.get(at..at + 2) {
                Some(f) => [f[0], f[1]],
                None => [0.0; 2],
            },
            Samples::Int16 { data, step } => match data.get(at..at + 2) {
                Some(f) => [f32::from(f[0]) * step, f32::from(f[1]) * step],
                None => [0.0; 2],
            },
            Samples::Int24(data) => match data.get(at * 3..at * 3 + 6) {
                Some(b) => [int24(&b[..3]), int24(&b[3..])],
                None => [0.0; 2],
            },
        }
    }
}

/// Stereo audio a deck plays from: a track, or a track with its stems at
/// their levels.
pub trait Frames {
    fn rate(&self) -> u32;

    /// The stereo frame at `index`, or silence outside the audio.
    fn frame(&self, index: i64) -> [f32; 2];

    /// The signal at a fractional frame position: linear or cubic (Hermite)
    /// interpolation between the neighbouring frames.
    fn sample_at(&self, position: f64, cubic: bool) -> [f32; 2] {
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
}

impl Frames for TrackAudio {
    fn rate(&self) -> u32 {
        self.sample_rate
    }

    fn frame(&self, index: i64) -> [f32; 2] {
        TrackAudio::frame(self, index)
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

    let depth = depth_of(&params);
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
    Ok(TrackAudio::with_depth(sample_rate, samples, depth))
}

/// How a file's audio is held: as its bits, 16-bit for lossy files.
fn depth_of(params: &symphonia::core::codecs::audio::AudioCodecParameters) -> Depth {
    use symphonia::core::audio::sample::SampleFormat as F;
    use symphonia::core::codecs::audio::well_known as c;
    // PCM (WAV, AIFF): its codec says how it is stored.
    let int16 = [
        c::CODEC_ID_PCM_S16LE, c::CODEC_ID_PCM_S16LE_PLANAR, c::CODEC_ID_PCM_S16BE, c::CODEC_ID_PCM_S16BE_PLANAR,
        c::CODEC_ID_PCM_U16LE, c::CODEC_ID_PCM_U16LE_PLANAR, c::CODEC_ID_PCM_U16BE, c::CODEC_ID_PCM_U16BE_PLANAR,
        c::CODEC_ID_PCM_S8, c::CODEC_ID_PCM_S8_PLANAR, c::CODEC_ID_PCM_U8, c::CODEC_ID_PCM_U8_PLANAR,
        c::CODEC_ID_PCM_ALAW, c::CODEC_ID_PCM_MULAW,
    ];
    let int24 = [
        c::CODEC_ID_PCM_S24LE, c::CODEC_ID_PCM_S24LE_PLANAR, c::CODEC_ID_PCM_S24BE, c::CODEC_ID_PCM_S24BE_PLANAR,
        c::CODEC_ID_PCM_U24LE, c::CODEC_ID_PCM_U24LE_PLANAR, c::CODEC_ID_PCM_U24BE, c::CODEC_ID_PCM_U24BE_PLANAR,
    ];
    let float = [
        c::CODEC_ID_PCM_S32LE, c::CODEC_ID_PCM_S32LE_PLANAR, c::CODEC_ID_PCM_S32BE, c::CODEC_ID_PCM_S32BE_PLANAR,
        c::CODEC_ID_PCM_U32LE, c::CODEC_ID_PCM_U32LE_PLANAR, c::CODEC_ID_PCM_U32BE, c::CODEC_ID_PCM_U32BE_PLANAR,
        c::CODEC_ID_PCM_F32LE, c::CODEC_ID_PCM_F32LE_PLANAR, c::CODEC_ID_PCM_F32BE, c::CODEC_ID_PCM_F32BE_PLANAR,
        c::CODEC_ID_PCM_F64LE, c::CODEC_ID_PCM_F64LE_PLANAR, c::CODEC_ID_PCM_F64BE, c::CODEC_ID_PCM_F64BE_PLANAR,
    ];
    if int16.contains(&params.codec) {
        return Depth::Int16;
    }
    if int24.contains(&params.codec) {
        return Depth::Int24;
    }
    if float.contains(&params.codec) || matches!(params.sample_format, Some(F::F32 | F::F64)) {
        return Depth::Float;
    }
    // Lossless (FLAC, ALAC) as its bits; lossy ones have none: 16-bit.
    match params.bits_per_sample.or(params.bits_per_coded_sample) {
        Some(bits) if bits <= 16 => Depth::Int16,
        Some(bits) if bits <= 24 => Depth::Int24,
        Some(_) => Depth::Float,
        None => Depth::Int16,
    }
}

/// Peak amplitude per 1/150 s, the resolution of rekordbox's detail
/// waveforms; used when a track has no analysis files.
pub fn peak_waveform(audio: &TrackAudio) -> Vec<f32> {
    peak_waveform_at(audio, 150)
}

/// Peak level per `1 / per_second` s.
pub fn peak_waveform_at(audio: &TrackAudio, per_second: u32) -> Vec<f32> {
    column_ranges(audio, per_second)
        .map(|(a, b)| (a * 2..b * 2).map(|i| audio.sample(i)).fold(0f32, |peak, s| peak.max(s.abs())))
        .collect()
}

/// Each column's frames: column `k` from `k · rate / per_second` on, so
/// columns stay on time even where the rate doesn't divide evenly (44.1 kHz
/// at 600 a second is 73.5 frames each).
fn column_ranges(audio: &TrackAudio, per_second: u32) -> impl Iterator<Item = (usize, usize)> {
    let (rate, per_second) = (u64::from(audio.sample_rate.max(1)), u64::from(per_second.max(1)));
    let frames = audio.frames() as u64;
    let start = move |k: u64| (k * rate / per_second).min(frames) as usize;
    (0..)
        .map(move |k| (start(k), start(k + 1)))
        .take_while(|&(a, b)| b > a)
}

/// Peak level of the low (< 200 Hz), mid and high (> 2.5 kHz) bands per
/// 1/150 s, each 0.0–1.0 relative to the loudest column; used for 3-band
/// waveforms when a track has no such analysis.
pub fn band_waveform(audio: &TrackAudio) -> Vec<[f32; 3]> {
    band_waveform_at(audio, 150)
}

/// Splits a signal into lows (under about 200 Hz), mids and highs (over
/// about 2.5 kHz) for waveforms, each band with two-pole filters of its own
/// (taking one band from another lets the mids into the highs).
pub struct BandSplit {
    low: [OnePole; 2],
    mid_low: [OnePole; 2],
    mid_high: [OnePole; 2],
    high: [OnePole; 2],
}

/// A one-pole low-pass, or high-pass (what the low-pass takes out).
#[derive(Clone, Copy)]
struct OnePole {
    a: f32,
    y: f32,
    high: bool,
}

impl OnePole {
    fn new(hz: f32, rate: f32, high: bool) -> Self {
        Self { a: 1.0 - (-2.0 * std::f32::consts::PI * hz / rate).exp(), y: 0.0, high }
    }

    fn next(&mut self, x: f32) -> f32 {
        self.y += self.a * (x - self.y);
        if self.high { x - self.y } else { self.y }
    }
}

impl BandSplit {
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1) as f32;
        let two = |hz, high| [OnePole::new(hz, rate, high); 2];
        Self { low: two(200.0, false), mid_low: two(200.0, true), mid_high: two(2_500.0, false), high: two(2_500.0, true) }
    }

    /// The next sample's low, mid and high.
    pub fn next(&mut self, x: f32) -> [f32; 3] {
        let twice = |f: &mut [OnePole; 2], x| {
            let y = f[0].next(x);
            f[1].next(y)
        };
        let low = twice(&mut self.low, x);
        let mid = twice(&mut self.mid_high, twice(&mut self.mid_low, x));
        let high = twice(&mut self.high, x);
        [low, mid, high]
    }
}

/// [`band_waveform`] with `per_second` columns a second.
pub fn band_waveform_at(audio: &TrackAudio, per_second: u32) -> Vec<[f32; 3]> {
    let mut split = BandSplit::new(audio.sample_rate);
    let ends: Vec<usize> = column_ranges(audio, per_second).map(|(_, b)| b).collect();
    let mut next = ends.iter().copied().peekable();
    let mut columns = Vec::with_capacity(ends.len());
    let mut peak = [0f32; 3];
    for (i, frame) in audio.to_f32().chunks_exact(2).enumerate() {
        let bands = split.next((frame[0] + frame[1]) * 0.5);
        for (p, b) in peak.iter_mut().zip(bands) {
            *p = p.max(b.abs());
        }
        if next.peek() == Some(&(i + 1)) {
            next.next();
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

    /// Held at a file's own depth, every sample comes back as decoded: all
    /// 16-bit values; 24-bit ones across the range; a lossy file's overs
    /// above full scale are kept, not cut off.
    #[test]
    fn depths_keep_every_sample() {
        let values: Vec<f32> = (-32768..32768).map(|k| k as f32 / 32768.0).collect();
        let held = TrackAudio::with_depth(44_100, values.clone(), Depth::Int16);
        assert_eq!(held.bytes(), values.len() * 2);
        assert!((0..values.len()).all(|i| held.sample(i) == values[i]));
        let values: Vec<f32> = (-8_388_608..8_388_608).step_by(997).map(|k| k as f32 / 8_388_608.0).collect();
        let held = TrackAudio::with_depth(44_100, values.clone(), Depth::Int24);
        assert_eq!(held.bytes(), values.len() * 3);
        assert!((0..values.len()).all(|i| held.sample(i) == values[i]));
        let loud = vec![0.5, -1.25, 1.1, 0.0];
        let held = TrackAudio::with_depth(44_100, loud.clone(), Depth::Int16);
        assert!((0..4).all(|i| (held.sample(i) - loud[i]).abs() < 1e-4), "{:?}", held.to_f32());
    }


    /// Files kept as they are (`DECKHAND_DEPTH_FILES`: paths, `:`-separated):
    /// prints each one's depth, memory and largest difference from floats.
    #[test]
    #[ignore]
    fn files_keep_their_depth() {
        for path in std::env::var("DECKHAND_DEPTH_FILES").unwrap().split(':') {
            let audio = decode_file(Path::new(path)).unwrap();
            println!("{path}: {:?}, {} bytes for {} samples", audio.depth(), audio.bytes(), audio.len());
        }
    }


    /// 120 BPM clicks for 5 minutes at 44.1 kHz: at 600 columns a second
    /// (73.5 frames each) the last click is still in its own column, not
    /// seconds early.
    #[test]
    fn waveform_columns_do_not_drift() {
        let rate = 44_100u32;
        let frames = rate as usize * 300;
        let mut samples = vec![0f32; frames * 2];
        for beat in 0..600 {
            let at = beat * rate as usize / 2;
            for s in &mut samples[at * 2..(at + 20) * 2] {
                *s = 0.9;
            }
        }
        let audio = TrackAudio::new(rate, samples);
        let peaks = peak_waveform_at(&audio, 600);
        assert_eq!(peaks.len(), 180_000);
        let last = peaks.iter().rposition(|&p| p > 0.5).unwrap();
        assert_eq!(last, 599 * 300, "{} columns off", last as i64 - 599 * 300);
        let bands = band_waveform_at(&audio, 600);
        assert_eq!(bands.len(), 180_000);
        let loudest = (179_000..180_000).max_by(|&a, &b| bands[a][2].total_cmp(&bands[b][2])).unwrap();
        assert!((loudest as i64 - 599 * 300).abs() <= 1, "{loudest}");
    }

    use super::*;

    #[test]
    fn interpolation_hits_frames_and_follows_curves() {
        let samples: Vec<f32> = (0..8).flat_map(|i| [(i * i) as f32, -(i as f32)]).collect();
        let audio = TrackAudio::new(1, samples);
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
        TrackAudio::new(rate, samples)
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
