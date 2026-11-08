//! Finding audio files in folders and reading their tags.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use lofty::prelude::*;
use lofty::tag::ItemKey;

use crate::collection::{EditedField, Track, now};

/// File extensions treated as audio (lower case).
pub const AUDIO_EXTENSIONS: [&str; 10] =
    ["mp3", "wav", "aif", "aiff", "flac", "m4a", "mp4", "aac", "ogg", "alac"];

/// A file's tags and properties.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub label: String,
    pub comment: String,
    pub year: Option<u16>,
    pub track_number: Option<u32>,
    pub duration: f64,
    pub bpm: Option<f64>,
    pub key: Option<String>,
    pub sample_rate: Option<u32>,
    pub bitrate: Option<u32>,
    pub has_artwork: bool,
    pub file_size: u64,
    pub modified: u64,
}

impl ScannedFile {
    pub(crate) fn into_track(self, id: u32) -> Track {
        let mut track = Track {
            id,
            path: PathBuf::new(),
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            genre: String::new(),
            label: String::new(),
            comment: String::new(),
            year: None,
            track_number: None,
            duration: 0.0,
            bpm: None,
            key: None,
            sample_rate: None,
            bitrate: None,
            has_artwork: false,
            rating: 0,
            color: None,
            play_count: 0,
            added: now(),
            last_played: None,
            file_size: 0,
            modified: 0,
            cues: Vec::new(),
            tempo: Vec::new(),
            edited: Vec::new(),
            analysis: None,
            derived: None,
            gain_db: None,
        };
        self.apply_to(&mut track);
        track
    }

    /// Refreshes the tag fields of a track, leaving what the user set.
    /// Fields the user edited in deckhand are kept.
    pub(crate) fn apply_to(self, track: &mut Track) {
        let keep = |field: EditedField| track.edited.contains(&field);
        let (title, artist, album) =
            (keep(EditedField::Title), keep(EditedField::Artist), keep(EditedField::Album));
        let (genre, label, comment) =
            (keep(EditedField::Genre), keep(EditedField::Label), keep(EditedField::Comment));
        track.path = self.path;
        if !title {
            track.title = self.title;
        }
        if !artist {
            track.artist = self.artist;
        }
        if !album {
            track.album = self.album;
        }
        if !genre {
            track.genre = self.genre;
        }
        if !label {
            track.label = self.label;
        }
        if !comment {
            track.comment = self.comment;
        }
        track.year = self.year;
        track.track_number = self.track_number;
        track.duration = self.duration;
        track.bpm = self.bpm;
        track.key = self.key;
        track.sample_rate = self.sample_rate;
        track.bitrate = self.bitrate;
        track.has_artwork = self.has_artwork;
        track.file_size = self.file_size;
        track.modified = self.modified;
    }
}

/// A file's size and modification time (Unix seconds), as scans record them.
pub fn file_stamp(path: &Path) -> (u64, u64) {
    std::fs::metadata(path).map_or((0, 0), |m| {
        let modified = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
        (m.len(), modified.map_or(0, |d| d.as_secs()))
    })
}

/// Reads one file's tags. Files without tags get their file name as title.
pub fn read_file(path: &Path) -> Result<ScannedFile> {
    let (file_size, modified) = file_stamp(path);
    let stem = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let tagged = lofty::read_from_path(path)?;
    let properties = tagged.properties();
    let mut file = ScannedFile {
        path: path.to_path_buf(),
        title: stem,
        duration: properties.duration().as_secs_f64(),
        sample_rate: properties.sample_rate(),
        bitrate: properties.audio_bitrate(),
        file_size,
        modified,
        ..ScannedFile::default()
    };
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let text = |value: Option<std::borrow::Cow<'_, str>>| {
            value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
        };
        if let Some(title) = text(tag.title()) {
            file.title = title;
        }
        file.artist = text(tag.artist()).unwrap_or_default();
        file.album = text(tag.album()).unwrap_or_default();
        file.genre = text(tag.genre()).unwrap_or_default();
        let get = |key: ItemKey| tag.get_string(key).map(str::trim).filter(|v| !v.is_empty());
        file.label = get(ItemKey::Label).unwrap_or_default().to_owned();
        file.comment = get(ItemKey::Comment).unwrap_or_default().to_owned();
        file.year = tag.date().map(|d| d.year);
        file.track_number = tag.track();
        file.bpm = get(ItemKey::Bpm)
            .or_else(|| get(ItemKey::IntegerBpm))
            .and_then(|b| b.replace(',', ".").parse::<f64>().ok())
            .filter(|b| (20.0..=400.0).contains(b));
        file.key = get(ItemKey::InitialKey).map(str::to_owned);
        file.has_artwork = !tag.pictures().is_empty();
    }
    Ok(file)
}

/// The picture embedded in an audio file's tags (the front cover when
/// there are several), as stored: JPEG or PNG bytes.
pub fn embedded_artwork(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    let tag = file.primary_tag().or_else(|| file.first_tag())?;
    let pictures = tag.pictures();
    let front = pictures
        .iter()
        .find(|p| p.pic_type() == lofty::picture::PictureType::CoverFront)
        .or_else(|| pictures.first())?;
    Some(front.data().to_vec())
}

/// Audio files below `folder`, recursively, skipping hidden entries.
fn audio_files(folder: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')) {
            continue;
        }
        if path.is_dir() {
            audio_files(&path, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        {
            out.push(path);
        }
    }
}

/// A folder scan running in the background.
pub struct Scan {
    pub found: Arc<AtomicUsize>,
    pub done: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    result: Receiver<Vec<ScannedFile>>,
}

impl Scan {
    /// Scans `paths` (folders or single files). Files whose size and
    /// modification time match `known` are skipped.
    pub fn start(paths: Vec<PathBuf>, known: HashMap<PathBuf, (u64, u64)>) -> Self {
        let (found, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, result) = channel();
        let (f, d, c) = (found.clone(), done.clone(), cancel.clone());
        std::thread::spawn(move || {
            let mut files = Vec::new();
            for path in &paths {
                if path.is_dir() {
                    audio_files(path, &mut files);
                } else {
                    files.push(path.clone());
                }
            }
            files.retain(|p| known.get(p) != Some(&file_stamp(p)));
            f.store(files.len(), Ordering::Relaxed);
            let mut scanned = Vec::new();
            for path in files {
                if c.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(file) = read_file(&path) {
                    scanned.push(file);
                }
                d.fetch_add(1, Ordering::Relaxed);
            }
            let _ = tx.send(scanned);
        });
        Self { found, done, cancel, result }
    }

    /// The scanned files, once the scan has finished.
    pub fn finished(&self) -> Option<Vec<ScannedFile>> {
        self.result.try_recv().ok()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
