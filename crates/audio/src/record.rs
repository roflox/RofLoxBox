//! Recording the master output to a WAV file.
//!
//! The audio thread pushes master samples into a lock-free ring buffer; a
//! writer thread takes them out and writes 24-bit PCM. If the writer ever
//! falls behind by more than the buffer, samples are counted as dropped
//! rather than holding up the audio.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};

/// Seconds of audio the ring buffer holds while the writer catches up.
const BUFFER_SECS: usize = 4;

/// What the audio thread holds while recording.
pub struct RecordTap {
    samples: rtrb::Producer<f32>,
    dropped: Arc<AtomicU64>,
}

impl std::fmt::Debug for RecordTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecordTap")
    }
}

impl RecordTap {
    /// Adds one stereo frame; never blocks.
    pub(crate) fn push(&mut self, frame: [f32; 2]) {
        if self.samples.slots() < 2 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let _ = self.samples.push(frame[0]);
        let _ = self.samples.push(frame[1]);
    }
}

/// A recording in progress.
pub struct Recording {
    path: PathBuf,
    rate: u32,
    frames: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    writer: Option<JoinHandle<Result<()>>>,
}

impl Recording {
    /// Creates the file and the writer; hand the tap to the engine.
    pub(crate) fn start(path: &Path, rate: u32) -> Result<(Self, RecordTap)> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let (producer, consumer) = rtrb::RingBuffer::new(rate as usize * 2 * BUFFER_SECS);
        let frames = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let counter = frames.clone();
        let writer = std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || write_wav(file, rate, consumer, &counter))?;
        let recording = Self {
            path: path.to_path_buf(),
            rate,
            frames,
            dropped: dropped.clone(),
            writer: Some(writer),
        };
        Ok((recording, RecordTap { samples: producer, dropped }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Length recorded so far.
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.frames.load(Ordering::Relaxed) as f64 / f64::from(self.rate))
    }

    /// Frames lost because the disk could not keep up.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Waits for the writer to finish the file; call after the engine let go
    /// of the tap.
    pub(crate) fn finish(mut self) -> Result<PathBuf> {
        if let Some(writer) = self.writer.take() {
            writer.join().map_err(|_| anyhow::anyhow!("the recorder stopped unexpectedly"))??;
        }
        Ok(self.path.clone())
    }
}

/// Writes 24-bit stereo PCM until the tap is dropped, then fixes the header.
fn write_wav(
    file: File,
    rate: u32,
    mut samples: rtrb::Consumer<f32>,
    frames: &AtomicU64,
) -> Result<()> {
    let mut out = BufWriter::with_capacity(1 << 16, file);
    write_header(&mut out, rate, 0)?;
    let mut written: u64 = 0;
    loop {
        let available = samples.slots();
        if available == 0 {
            if samples.is_abandoned() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let chunk = samples.read_chunk(available - available % 2)?;
        let (a, b) = chunk.as_slices();
        for &s in a.iter().chain(b) {
            let v = (s.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
            out.write_all(&v.to_le_bytes()[..3])?;
        }
        let count = a.len() + b.len();
        chunk.commit_all();
        written += count as u64 / 2;
        frames.store(written, Ordering::Relaxed);
    }
    out.flush()?;
    let mut file = out.into_inner().map_err(|e| e.into_error())?;
    file.seek(SeekFrom::Start(0))?;
    write_header(&mut file, rate, written)?;
    file.sync_all()?;
    Ok(())
}

/// Writes interleaved stereo `samples` to a 24-bit WAV file at `path`.
pub fn write_wav_file(path: &Path, rate: u32, samples: &[f32]) -> Result<()> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut out = BufWriter::with_capacity(1 << 16, file);
    write_header(&mut out, rate, samples.len() as u64 / 2)?;
    for &s in &samples[..samples.len() - samples.len() % 2] {
        let v = (s.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
        out.write_all(&v.to_le_bytes()[..3])?;
    }
    out.flush().with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn write_header(out: &mut impl Write, rate: u32, frames: u64) -> Result<()> {
    const CHANNELS: u16 = 2;
    const BYTES: u16 = 3;
    let data =
        (frames * u64::from(CHANNELS) * u64::from(BYTES)).min(u64::from(u32::MAX - 36)) as u32;
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + data).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    out.write_all(&16u32.to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?; // PCM
    out.write_all(&CHANNELS.to_le_bytes())?;
    out.write_all(&rate.to_le_bytes())?;
    out.write_all(&(rate * u32::from(CHANNELS * BYTES)).to_le_bytes())?;
    out.write_all(&(CHANNELS * BYTES).to_le_bytes())?;
    out.write_all(&(BYTES * 8).to_le_bytes())?;
    out.write_all(b"data")?;
    out.write_all(&data.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_valid_24_bit_wav() {
        let path = std::env::temp_dir().join(format!("deckhand-rec-{}.wav", std::process::id()));
        let (recording, mut tap) = Recording::start(&path, 48_000).unwrap();
        for i in 0..4_800 {
            let v = (i as f32 / 4_800.0) - 0.5;
            tap.push([v, -v]);
        }
        drop(tap);
        let path = recording.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 24);
        let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(data, 4_800 * 6);
        assert_eq!(bytes.len(), 44 + data as usize);
        // Decodes back with the app's own decoder.
        let audio = crate::decode_file(&path).unwrap();
        assert_eq!(audio.frames(), 4_800);
        assert!((audio.samples[2] + audio.samples[3]).abs() < 1e-3, "left and right mirror");
        let _ = std::fs::remove_file(path);
    }
}
