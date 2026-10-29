//! BPM, beat grid and key analysis of collection tracks, in the background.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use deckhand_analysis::beats::{DEFAULT_RANGE, Grid, beat_grid};
use deckhand_analysis::key::{Key, detect_key};

/// What the analysis found for one track.
pub struct Analysed {
    pub id: u32,
    /// Replace what the track has (an explicit re-analysis), or only fill
    /// in what is missing.
    pub overwrite: bool,
    pub grid: Option<Grid>,
    pub key: Option<Key>,
    /// The audio could not be read.
    pub failed: bool,
}

/// A running analysis of some tracks.
pub struct Analysis {
    pub total: usize,
    pub done: Arc<AtomicUsize>,
    cancelled: Arc<AtomicBool>,
    pub results: Receiver<Analysed>,
}

impl Analysis {
    /// Analyses `tracks` (id, file) on a few threads: each decoded track is
    /// held in memory while it is analysed.
    pub fn start(tracks: Vec<(u32, PathBuf)>, overwrite: bool) -> Self {
        let total = tracks.len();
        let done = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, results) = channel();
        let queue = Arc::new(Mutex::new(tracks.into_iter()));
        let threads = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 4));
        for _ in 0..threads {
            let (queue, done, cancelled, tx) =
                (queue.clone(), done.clone(), cancelled.clone(), tx.clone());
            std::thread::spawn(move || work(&queue, &done, &cancelled, &tx, overwrite));
        }
        Self { total, done, cancelled, results }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed) || self.done.load(Ordering::Relaxed) >= self.total
    }
}

fn work(
    queue: &Mutex<std::vec::IntoIter<(u32, PathBuf)>>,
    done: &AtomicUsize,
    cancelled: &AtomicBool,
    tx: &Sender<Analysed>,
    overwrite: bool,
) {
    while !cancelled.load(Ordering::Relaxed) {
        let Some((id, path)) = queue.lock().ok().and_then(|mut q| q.next()) else { break };
        let result = match deckhand_audio::decode_file(&path) {
            Ok(audio) => Analysed {
                id,
                overwrite,
                grid: beat_grid(&audio.samples, audio.sample_rate, DEFAULT_RANGE),
                key: detect_key(&audio.samples, audio.sample_rate),
                failed: false,
            },
            Err(_) => Analysed { id, overwrite, grid: None, key: None, failed: true },
        };
        if cancelled.load(Ordering::Relaxed) || tx.send(result).is_err() {
            break;
        }
        done.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-bit stereo WAV of 40 s: kicks at 128 BPM from 0.2 s over an
    /// A minor chord.
    fn write_track(path: &std::path::Path) {
        let rate = 44_100u32;
        let frames = rate as usize * 40;
        let beat = 60.0 / 128.0;
        let mut data = Vec::with_capacity(frames * 4);
        for i in 0..frames {
            let t = i as f64 / f64::from(rate);
            let since = (t - 0.2).rem_euclid(beat);
            let kick = if t >= 0.2 {
                (-since * 30.0).exp() * (since * 60.0 * std::f64::consts::TAU).sin()
            } else {
                0.0
            };
            let chord: f64 = [45.0, 57.0, 60.0, 64.0]
                .iter()
                .map(|m: &f64| {
                    (std::f64::consts::TAU * 440.0 * 2f64.powf((m - 69.0) / 12.0) * t).sin()
                })
                .sum();
            let v = ((0.5 * kick + 0.08 * chord) * 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    #[test]
    fn analyses_a_file_and_reports_missing_ones() {
        let dir = std::env::temp_dir().join(format!("deckhand-analyze-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("track.wav");
        write_track(&file);
        let analysis = Analysis::start(vec![(7, file), (8, dir.join("missing.wav"))], true);
        let mut results: Vec<Analysed> = (0..2)
            .map(|_| analysis.results.recv_timeout(std::time::Duration::from_secs(60)).unwrap())
            .collect();
        results.sort_by_key(|r| r.id);
        let _ = std::fs::remove_dir_all(&dir);

        let found = &results[0];
        assert!(found.overwrite && !found.failed);
        let grid = found.grid.unwrap();
        assert_eq!(grid.bpm, 128.0);
        assert!((grid.first_beat - 0.2).abs() < 0.015, "{grid:?}");
        assert_eq!(found.key.map(Key::camelot).as_deref(), Some("8A"));
        assert!(results[1].failed);
        // A track counts as done just after its result is sent.
        let waited = std::time::Instant::now();
        while !analysis.is_finished() && waited.elapsed().as_secs() < 5 {
            std::thread::yield_now();
        }
        assert!(analysis.is_finished());
    }
}
