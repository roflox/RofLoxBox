//! Writing a track's audio in another format (converting tracks): WAV and
//! AIFF (16 or 24-bit), FLAC (flacenc) and MP3 (LAME), at its own or
//! another sample rate.

use std::path::Path;

use anyhow::{Context, Result, anyhow};

use crate::TrackAudio;

/// What a track is written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// 16 or 24 bits.
    Wav(u16),
    Aiff(u16),
    Flac(u16),
    /// Constant bitrate, kbit/s.
    Mp3(u32),
}

/// MP3 holds at most 48 kHz.
const MP3_MAX_RATE: u32 = 48_000;

/// Writes `audio` (stereo) to `path` as `encoding`, at `sample_rate` (its
/// own when `None`). Returns the sample rate written.
pub fn write_track(
    audio: &TrackAudio,
    path: &Path,
    encoding: Encoding,
    sample_rate: Option<u32>,
) -> Result<u32> {
    let own = audio.sample_rate;
    let mut rate = sample_rate.unwrap_or(own);
    if let Encoding::Mp3(_) = encoding
        && rate > MP3_MAX_RATE
    {
        // 88.2 / 176.4 kHz go to 44.1, others to 48.
        rate = if rate.is_multiple_of(44_100) { 44_100 } else { MP3_MAX_RATE };
    }
    let samples = crate::resample::resample(&audio.to_f32(), 2, own, rate);
    let bytes = match encoding {
        Encoding::Wav(bits) => wav(&samples, rate, bits),
        Encoding::Aiff(bits) => aiff(&samples, rate, bits),
        Encoding::Flac(bits) => flac(&samples, rate, bits)?,
        Encoding::Mp3(kbps) => mp3(&samples, rate, kbps)?,
    };
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(rate)
}

/// Whole numbers of `bits`, with triangular dither (one step's worth) when
/// going down to 16 bits.
fn quantize(samples: &[f32], bits: u16) -> Vec<i32> {
    let top = f64::from((1u32 << (bits - 1)) - 1);
    let dither = bits <= 16;
    // A small linear congruential generator: noise, not secrets.
    let mut seed: u32 = 0x1234_5678;
    let mut random = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(seed >> 8) / f64::from(1u32 << 24)
    };
    samples
        .iter()
        .map(|&s| {
            let noise = if dither { random() - random() } else { 0.0 };
            (f64::from(s) * top + noise).round().clamp(-top - 1.0, top) as i32
        })
        .collect()
}

fn wav(samples: &[f32], rate: u32, bits: u16) -> Vec<u8> {
    let bytes = usize::from(bits / 8);
    let data_len = (samples.len() * bytes) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend(b"RIFF");
    out.extend((36 + data_len).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes()); // PCM
    out.extend(2u16.to_le_bytes()); // channels
    out.extend(rate.to_le_bytes());
    out.extend((rate * 2 * bytes as u32).to_le_bytes());
    out.extend((2 * bytes as u16).to_le_bytes());
    out.extend(bits.to_le_bytes());
    out.extend(b"data");
    out.extend(data_len.to_le_bytes());
    for v in quantize(samples, bits) {
        out.extend(&v.to_le_bytes()[..bytes]);
    }
    out
}

fn aiff(samples: &[f32], rate: u32, bits: u16) -> Vec<u8> {
    let bytes = usize::from(bits / 8);
    let frames = (samples.len() / 2) as u32;
    let data_len = (samples.len() * bytes) as u32;
    let mut out = Vec::with_capacity(54 + data_len as usize);
    out.extend(b"FORM");
    out.extend((4 + 26 + 16 + data_len).to_be_bytes());
    out.extend(b"AIFFCOMM");
    out.extend(18u32.to_be_bytes());
    out.extend(2u16.to_be_bytes());
    out.extend(frames.to_be_bytes());
    out.extend(bits.to_be_bytes());
    out.extend(extended(f64::from(rate)));
    out.extend(b"SSND");
    out.extend((8 + data_len).to_be_bytes());
    out.extend([0u8; 8]); // offset, block size
    for v in quantize(samples, bits) {
        let be = v.to_be_bytes();
        out.extend(&be[4 - bytes..]);
    }
    out
}

/// An 80-bit IEEE extended float (AIFF's sample rate), for a positive whole number.
fn extended(value: f64) -> [u8; 10] {
    let whole = value as u64;
    if whole == 0 {
        return [0; 10];
    }
    let shift = whole.leading_zeros();
    let exponent = 16_383 + 63 - shift as u16;
    let mantissa = whole << shift;
    let mut out = [0u8; 10];
    out[..2].copy_from_slice(&exponent.to_be_bytes());
    out[2..].copy_from_slice(&mantissa.to_be_bytes());
    out
}

fn flac(samples: &[f32], rate: u32, bits: u16) -> Result<Vec<u8>> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| anyhow!("FLAC settings: {e:?}"))?;
    let source = flacenc::source::MemSource::from_samples(
        &quantize(samples, bits),
        2,
        usize::from(bits),
        rate as usize,
    );
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| anyhow!("encoding FLAC: {e:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).map_err(|e| anyhow!("writing FLAC: {e:?}"))?;
    let mut bytes = sink.as_slice().to_vec();
    // STREAMINFO's smallest block size leaves out the last (shorter) one,
    // as the format says; flacenc counts it, and some readers (symphonia)
    // then won't read the file. Every block but the last is the fixed size.
    if bytes.len() >= 12 && bytes.starts_with(b"fLaC") {
        let (min, max) = bytes[8..12].split_at_mut(2);
        min.copy_from_slice(max);
    }
    Ok(bytes)
}

fn mp3(samples: &[f32], rate: u32, kbps: u32) -> Result<Vec<u8>> {
    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Quality};
    let bitrate = match kbps {
        128 => Bitrate::Kbps128,
        192 => Bitrate::Kbps192,
        256 => Bitrate::Kbps256,
        _ => Bitrate::Kbps320,
    };
    let setup = |e: mp3lame_encoder::BuildError| anyhow!("MP3 settings: {e:?}");
    let mut builder = Builder::new().context("starting LAME")?;
    builder.set_num_channels(2).map_err(setup)?;
    builder.set_sample_rate(rate).map_err(setup)?;
    builder.set_brate(bitrate).map_err(setup)?;
    builder.set_quality(Quality::NearBest).map_err(setup)?;
    // The info frame tells players where the music starts (without the
    // encoder's delay), so cues and grids stay where they were.
    builder.set_to_write_vbr_tag(true).map_err(setup)?;
    let mut encoder = builder.build().map_err(setup)?;
    let mut data = Vec::new();
    for chunk in samples.chunks(2 * 4096) {
        data.reserve(mp3lame_encoder::max_required_buffer_size(chunk.len() / 2));
        encoder
            .encode_to_vec(InterleavedPcm(chunk), &mut data)
            .map_err(|e| anyhow!("encoding MP3: {e:?}"))?;
    }
    data.reserve(7200);
    encoder.flush_to_vec::<FlushNoGap>(&mut data).map_err(|e| anyhow!("encoding MP3: {e:?}"))?;
    let mut out = Vec::with_capacity(data.len() + 1024);
    out.reserve(encoder.lame_tag_size());
    encoder.lame_tag_encode_to_vec(&mut out);
    out.extend(data);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aiff_sample_rates_as_extended_floats() {
        // 44.1 kHz as every AIFF writer gives it.
        assert_eq!(extended(44_100.0), [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
    }

    /// Each format reads back as what was written.
    #[test]
    fn written_tracks_read_back() {
        let rate = 48_000;
        let tone: Vec<f32> = (0..rate as usize)
            .flat_map(|i| {
                let v = (std::f64::consts::TAU * 440.0 * i as f64 / f64::from(rate)).sin() as f32;
                [v * 0.5, v * 0.5]
            })
            .collect();
        let audio = TrackAudio::new(rate, tone);
        let dir = std::env::temp_dir().join(format!("rofloxbox-encode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (encoding, name) in [
            (Encoding::Wav(16), "a.wav"),
            (Encoding::Wav(24), "b.wav"),
            (Encoding::Aiff(16), "c.aiff"),
            (Encoding::Flac(16), "d.flac"),
            (Encoding::Mp3(320), "e.mp3"),
        ] {
            let path = dir.join(name);
            let written = write_track(&audio, &path, encoding, Some(44_100)).unwrap();
            assert_eq!(written, 44_100);
            let back = crate::decode_file(&path).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(back.sample_rate, 44_100, "{name}");
            // MP3 may lose a frame's worth at the ends; the others none.
            let secs = back.duration_secs();
            assert!((secs - 1.0).abs() < 0.03, "{name}: {secs} s");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
