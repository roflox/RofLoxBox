//! Converting collection tracks to another format, bitrate or sample rate,
//! in the background, without other programs: read with symphonia, written
//! by RofLoxBox (WAV, AIFF), flacenc (FLAC) or LAME (MP3), tags and cover
//! picture copied with lofty. The converted file goes next to the original
//! under a name of its own; what becomes of the original (kept, or
//! replaced in the collection and moved away) is decided after.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rofloxbox_audio::Encoding;
use serde::{Deserialize, Serialize};

/// What tracks are converted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    /// MP3 at this many kbit/s (constant).
    Mp3(u32),
    /// No longer offered (it needed ffmpeg): kept so settings that chose it
    /// still read; converted as MP3.
    Aac(u32),
    Wav,
    Aiff,
    Flac,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Mp3(_) | Format::Aac(_) => "mp3",
            Format::Wav => "wav",
            Format::Aiff => "aiff",
            Format::Flac => "flac",
        }
    }
}

/// Everything a conversion is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub format: Format,
    /// Hz; `None` keeps the track's (MP3: at most 48 kHz).
    pub sample_rate: Option<u32>,
    /// 16 or 24 for WAV, AIFF and FLAC.
    pub bits: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self { format: Format::Mp3(320), sample_rate: None, bits: 16 }
    }
}

impl Settings {
    /// A short name for file names and messages: "MP3 320", "AIFF 16-bit 44.1 kHz".
    pub fn label(&self) -> String {
        let mut label = match self.format {
            Format::Mp3(kbps) | Format::Aac(kbps) => format!("MP3 {kbps}"),
            Format::Wav => format!("WAV {}-bit", self.bits),
            Format::Aiff => format!("AIFF {}-bit", self.bits),
            Format::Flac => format!("FLAC {}-bit", self.bits),
        };
        if let Some(rate) = self.sample_rate {
            label.push_str(&format!(" {:.1} kHz", f64::from(rate) / 1000.0));
        }
        label
    }

    fn encoding(&self) -> Encoding {
        let bits = if self.bits >= 24 { 24 } else { 16 };
        match self.format {
            Format::Mp3(kbps) | Format::Aac(kbps) => Encoding::Mp3(kbps),
            Format::Wav => Encoding::Wav(bits),
            Format::Aiff => Encoding::Aiff(bits),
            Format::Flac => Encoding::Flac(bits),
        }
    }
}

/// A file name not taken yet: `stem.ext`, else `stem 2.ext`, …
pub fn free_path(dir: &Path, stem: &str, extension: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem} {n}.{extension}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Converts `input` into a new file next to it, "<name> (<settings>).<ext>",
/// with its tags and cover picture. Returns it, and how much later (seconds)
/// the music sits in it: an MP3's encoder delay, which cues and the beat
/// grid have to move by to stay on the music.
fn convert(input: &Path, settings: &Settings) -> Result<(PathBuf, f64)> {
    let dir = input.parent().context("the file has no folder")?;
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let extension = settings.format.extension();
    let output = free_path(dir, &format!("{stem} ({})", settings.label()), extension);
    // Written under a name of its own until done, so a stopped one leaves
    // nothing that looks finished.
    let partial = output.with_extension(format!("partial.{extension}"));
    let written = (|| -> Result<f64> {
        let audio = rofloxbox_audio::decode_file(input)?;
        rofloxbox_audio::write_track(&audio, &partial, settings.encoding(), settings.sample_rate)?;
        copy_tags(input, &partial);
        let converted = rofloxbox_audio::decode_file(&partial)?;
        Ok(rofloxbox_audio::time_offset(&audio, &converted))
    })();
    let offset = match written {
        Ok(offset) => offset,
        Err(err) => {
            let _ = std::fs::remove_file(&partial);
            return Err(err);
        }
    };
    std::fs::rename(&partial, &output).context("naming the converted file")?;
    Ok((output, offset))
}

/// The tags (title, artist, … and the cover picture) of `from`, written
/// into `to` in the kind of tag its format takes. Best effort: a file
/// without tags, or one lofty can't read, leaves `to` without them.
fn copy_tags(from: &Path, to: &Path) {
    use lofty::prelude::*;
    let Ok(source) = lofty::read_from_path(from) else { return };
    let Some(mut tag) = source.primary_tag().or_else(|| source.first_tag()).cloned() else {
        return;
    };
    let Some(kind) = lofty::file::FileType::from_path(to) else { return };
    tag.re_map(kind.primary_tag_type());
    let _ = tag.save_to_path(to, lofty::config::WriteOptions::default());
}

/// One track converted, or not.
pub struct Converted {
    pub id: u32,
    /// The new file and how much later the music sits in it (seconds).
    pub result: Result<(PathBuf, f64), String>,
}

/// Moves cues, loops and the beat grid `by` seconds, to stay on the music
/// of a converted file.
pub fn shift_times(
    cues: &mut [rofloxbox_library::Cue],
    tempo: &mut [rofloxbox_library::TempoMarker],
    by: f64,
) {
    for cue in cues {
        cue.start += by;
        if let Some(end) = &mut cue.end {
            *end += by;
        }
    }
    for marker in tempo {
        marker.start += by;
    }
}

/// Conversions running in the background.
pub struct Conversion {
    pub total: usize,
    pub settings: Settings,
    /// Replace the originals in the collection once converted.
    pub replace: bool,
    cancelled: Arc<AtomicBool>,
    pub results: Receiver<Converted>,
    working: Arc<Mutex<Vec<u32>>>,
    /// Results taken in so far.
    pub received: usize,
    running: Arc<AtomicUsize>,
}

impl Conversion {
    /// Converts `tracks` (id, file), a few at a time (each holds its decoded
    /// audio meanwhile).
    pub fn start(tracks: Vec<(u32, PathBuf)>, settings: Settings, replace: bool) -> Self {
        let total = tracks.len();
        let cancelled = Arc::new(AtomicBool::new(false));
        let working = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(tracks.into_iter()));
        let (tx, results) = channel();
        let threads = std::thread::available_parallelism()
            .map_or(2, |n| (n.get() / 2).clamp(1, 4))
            .min(total.max(1));
        let running = Arc::new(AtomicUsize::new(threads));
        for _ in 0..threads {
            let (queue, cancelled, working, tx, running) =
                (queue.clone(), cancelled.clone(), working.clone(), tx.clone(), running.clone());
            std::thread::spawn(move || {
                while !cancelled.load(Ordering::Relaxed) {
                    let Some((id, path)) = queue.lock().ok().and_then(|mut q| q.next()) else {
                        break;
                    };
                    if let Ok(mut w) = working.lock() {
                        w.push(id);
                    }
                    let result = convert(&path, &settings).map_err(|e| format!("{e:#}"));
                    if let Ok(mut w) = working.lock() {
                        w.retain(|&t| t != id);
                    }
                    if tx.send(Converted { id, result }).is_err() {
                        break;
                    }
                }
                running.fetch_sub(1, Ordering::Relaxed);
            });
        }
        Self { total, settings, replace, cancelled, results, working, received: 0, running }
    }

    /// Stops after the tracks being converted now.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Every thread is done (and every result sent).
    pub fn is_finished(&self) -> bool {
        self.running.load(Ordering::Relaxed) == 0
    }

    /// The tracks being converted right now.
    pub fn working(&self) -> Vec<u32> {
        self.working.lock().map(|w| w.clone()).unwrap_or_default()
    }
}

/// Moves `original` into `folder` (made, and kept out of scans, the first
/// time) under a free name; returns where it went.
pub fn move_original(original: &Path, folder: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
    let marker = folder.join(rofloxbox_library::SKIP_MARKER);
    if !marker.exists() {
        std::fs::write(&marker, "RofLoxBox doesn't add the music in this folder.\n")?;
    }
    let stem = original.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let extension =
        original.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let to = free_path(folder, &stem, &extension);
    move_file(original, &to)?;
    Ok(to)
}

/// Renames, or copies and deletes across drives.
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    std::fs::remove_file(from).with_context(|| format!("removing {}", from.display()))?;
    Ok(())
}

/// The folder replaced originals go to by default: Music/RofLoxBox Originals.
pub fn default_originals_dir() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Music").join("RofLoxBox Originals")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(Settings::default().label(), "MP3 320");
        let aiff = Settings { format: Format::Aiff, sample_rate: Some(44_100), bits: 16 };
        assert_eq!(aiff.label(), "AIFF 16-bit 44.1 kHz");
        assert_eq!(aiff.encoding(), Encoding::Aiff(16));
    }

    #[test]
    fn free_paths_number_on() {
        let dir = std::env::temp_dir().join(format!("rofloxbox-free-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_path(&dir, "a", "mp3"), dir.join("a.mp3"));
        std::fs::write(dir.join("a.mp3"), b"").unwrap();
        assert_eq!(free_path(&dir, "a", "mp3"), dir.join("a 2.mp3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tagged 24-bit 96 kHz WAV to MP3 320: next to it, playable, at
    /// 48 kHz, later by the encoder's delay, with its title; the original then moved to a folder scans
    /// skip.
    #[test]
    fn converts_and_keeps_the_tags() {
        let dir = std::env::temp_dir().join(format!("rofloxbox-convert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("Tone.wav");
        // Noise (a steady tone lines up at every period, so no offset).
        let mut seed = 7u32;
        let tone: Vec<f32> = (0..96_000 * 2)
            .flat_map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let v = (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
                [v, v]
            })
            .collect();
        let audio = rofloxbox_audio::TrackAudio::new(96_000, tone);
        rofloxbox_audio::write_track(&audio, &input, Encoding::Wav(24), None).unwrap();
        {
            use lofty::prelude::*;
            let mut tag = lofty::tag::Tag::new(lofty::tag::TagType::Id3v2);
            tag.set_title("Tone".into());
            tag.set_artist("Someone".into());
            tag.save_to_path(&input, lofty::config::WriteOptions::default()).unwrap();
        }
        let (out, offset) = convert(&input, &Settings::default()).unwrap();
        assert_eq!(out, dir.join("Tone (MP3 320).mp3"));
        // The encoder's delay and the info frame, at 48 kHz.
        assert!((0.04..0.06).contains(&offset), "{offset}");
        let back = rofloxbox_audio::decode_file(&out).unwrap();
        assert_eq!(back.sample_rate, 48_000, "MP3 holds at most 48 kHz");
        let read = rofloxbox_library::read_file(&out).unwrap();
        assert_eq!((read.title.as_str(), read.artist.as_str()), ("Tone", "Someone"));
        let moved = move_original(&input, &dir.join("Originals")).unwrap();
        assert!(moved.is_file() && !input.exists());
        assert!(dir.join("Originals").join(rofloxbox_library::SKIP_MARKER).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
