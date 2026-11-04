//! Importing a rekordbox USB export into the collection: the music and
//! rekordbox's analysis files are copied to this computer (the stick will be
//! unplugged), and tracks, beat grids, cues and playlists come along.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use anyhow::{Context, Result};
use deckhand_formats::anlz::{AnlzFile, CueKind as AnlzCueKind, CueListKind};
use deckhand_formats::pdb::{LibraryTrack, PlaylistKind, PlaylistNode};
use deckhand_library::{
    Cue, CueKind, ScannedFile, TempoMarker, XmlLibrary, XmlNode, XmlTrack, file_stamp,
};

/// What to import from an export.
pub struct ImportRequest {
    /// The export's root (holding `PIONEER` and the music).
    pub root: PathBuf,
    /// Where its files are copied to.
    pub destination: PathBuf,
    pub tracks: Vec<LibraryTrack>,
    /// Playlists and folders to bring along (all of them, or one).
    pub playlists: Vec<PlaylistNode>,
}

/// An import running in the background.
pub struct UsbImport {
    pub total: usize,
    pub done: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    result: mpsc::Receiver<Result<Imported>>,
}

/// The result: what to merge into the collection.
pub struct Imported {
    pub library: XmlLibrary,
    /// Tracks whose audio file was not on the stick.
    pub missing: usize,
}

impl UsbImport {
    pub fn start(request: ImportRequest) -> Self {
        let total = request.tracks.len();
        let done = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, result) = mpsc::channel();
        let (counter, stop) = (done.clone(), cancel.clone());
        std::thread::spawn(move || {
            let _ = tx.send(run(&request, &counter, &stop));
        });
        Self { total, done, cancel, result }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The result once the work is finished.
    pub fn finished(&self) -> Option<Result<Imported>> {
        self.result.try_recv().ok()
    }
}

/// Runs an import in this thread (tests).
#[cfg(test)]
pub fn run_for_tests(request: &ImportRequest) -> Result<Imported> {
    run(request, &AtomicUsize::new(0), &AtomicBool::new(false))
}

fn run(request: &ImportRequest, done: &AtomicUsize, cancel: &AtomicBool) -> Result<Imported> {
    let mut tracks = Vec::new();
    let mut missing = 0;
    for track in &request.tracks {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        match import_track(request, track)? {
            Some(xml) => tracks.push(xml),
            None => missing += 1,
        }
        done.fetch_add(1, Ordering::Relaxed);
    }
    let playlists = request.playlists.iter().map(playlist_node).collect();
    Ok(Imported { library: XmlLibrary { tracks, playlists }, missing })
}

/// Copies a file unless an identical-looking copy is there already.
fn copy_file(from: &Path, to: &Path) -> Result<()> {
    let same = |a: &Path, b: &Path| {
        std::fs::metadata(a).ok().map(|m| m.len()) == std::fs::metadata(b).ok().map(|m| m.len())
    };
    if to.is_file() && same(from, to) {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    Ok(())
}

/// An export path (`/Contents/…`) below a root.
fn below(root: &Path, export_path: &str) -> PathBuf {
    root.join(export_path.trim_start_matches('/'))
}

fn import_track(request: &ImportRequest, track: &LibraryTrack) -> Result<Option<XmlTrack>> {
    let row = &track.track;
    let source = below(&request.root, &row.file_path);
    if !source.is_file() {
        return Ok(None);
    }
    let audio = below(&request.destination, &row.file_path);
    copy_file(&source, &audio)?;

    // rekordbox's analysis: .DAT, and .EXT / .2EX beside it.
    let mut analysis = None;
    let (mut dat, mut ext) = (None, None);
    if !row.analyze_path.is_empty() {
        let source_dat = below(&request.root, &row.analyze_path);
        let copy_dat = below(&request.destination, &row.analyze_path);
        if source_dat.is_file() {
            copy_file(&source_dat, &copy_dat)?;
            for extension in ["EXT", "2EX"] {
                let from = source_dat.with_extension(extension);
                if from.is_file() {
                    copy_file(&from, &copy_dat.with_extension(extension))?;
                }
            }
            dat = AnlzFile::open(&copy_dat).ok();
            ext = AnlzFile::open(copy_dat.with_extension("EXT")).ok();
            analysis = Some(copy_dat);
        }
    }
    let (cues, tempo) = cues_and_grid(dat.as_ref(), ext.as_ref());

    let (file_size, modified) = file_stamp(&audio);
    let text = |value: &Option<String>| value.clone().unwrap_or_default();
    let file = ScannedFile {
        path: audio.clone(),
        title: row.title.clone(),
        artist: text(&track.artist),
        album: text(&track.album),
        genre: text(&track.genre),
        label: text(&track.label),
        comment: row.comment.clone(),
        year: Some(row.year).filter(|y| *y > 0),
        track_number: Some(row.track_number).filter(|n| *n > 0),
        duration: f64::from(row.duration_secs),
        bpm: Some(row.bpm()).filter(|b| *b > 0.0),
        key: track.key.clone(),
        sample_rate: Some(row.sample_rate).filter(|r| *r > 0),
        bitrate: Some(row.bitrate).filter(|b| *b > 0),
        has_artwork: track.artwork_path.is_some(),
        file_size,
        modified,
    };
    Ok(Some(XmlTrack {
        id: row.id.to_string(),
        location: audio.to_string_lossy().into_owned(),
        file,
        rating: row.rating.min(5),
        color: row.color_id,
        play_count: u32::from(row.play_count),
        added: added_seconds(&row.date_added),
        tempo,
        cues,
        analysis,
    }))
}

/// `YYYY-MM-DD` as Unix seconds (midnight UTC).
fn added_seconds(date: &str) -> Option<u64> {
    let date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let seconds = date.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    u64::try_from(seconds).ok()
}

/// Hot cues, memory cues and loops, and the beat grid as tempo markers,
/// from rekordbox's analysis.
fn cues_and_grid(dat: Option<&AnlzFile>, ext: Option<&AnlzFile>) -> (Vec<Cue>, Vec<TempoMarker>) {
    let tempo = dat.and_then(AnlzFile::beat_grid).map(grid_markers).unwrap_or_default();
    // Extended cue lists carry colors and names; fall back to the basic ones.
    let lists: Vec<_> = ext
        .into_iter()
        .flat_map(AnlzFile::cue_lists)
        .filter(|l| l.extended)
        .chain(dat.into_iter().flat_map(AnlzFile::cue_lists))
        .collect();
    let extended = lists.iter().any(|l| l.extended && !l.cues.is_empty());
    let mut cues = Vec::new();
    for list in lists.into_iter().filter(|l| l.extended == extended) {
        for cue in &list.cues {
            let kind = match (list.kind, cue.hot_cue) {
                (CueListKind::Hot, Some(n)) if (1..=8).contains(&n) => CueKind::Hot(n as u8 - 1),
                (CueListKind::Memory, _) => CueKind::Memory,
                _ => continue,
            };
            let end = match (cue.kind, cue.loop_end_ms) {
                (AnlzCueKind::Loop, Some(end)) => Some(f64::from(end) / 1000.0),
                _ => None,
            };
            cues.push(Cue {
                kind,
                start: f64::from(cue.time_ms) / 1000.0,
                end,
                color: cue.hot_cue_color.map(|c| c.rgb),
                name: cue.comment.clone().unwrap_or_default(),
            });
        }
    }
    (cues, tempo)
}

/// A beat grid as tempo markers: where it starts and wherever the tempo
/// changes.
pub fn grid_markers(grid: &deckhand_formats::anlz::BeatGrid) -> Vec<TempoMarker> {
    let mut tempo: Vec<TempoMarker> = Vec::new();
    for beat in &grid.beats {
        if tempo.last().is_none_or(|m| (m.bpm - beat.bpm()).abs() > 1e-6) {
            tempo.push(TempoMarker {
                start: f64::from(beat.time_ms) / 1000.0,
                bpm: beat.bpm(),
                beat: beat.bar_position.clamp(1, 4) as u8,
            });
        }
    }
    tempo
}

fn playlist_node(node: &PlaylistNode) -> XmlNode {
    match &node.kind {
        PlaylistKind::Folder(children) => XmlNode::Folder {
            name: node.name.clone(),
            children: children.iter().map(playlist_node).collect(),
            id: None,
        },
        PlaylistKind::Playlist(ids) => XmlNode::Playlist {
            name: node.name.clone(),
            keys: ids.iter().map(u32::to_string).collect(),
            by_location: false,
            id: None,
        },
    }
}

/// Track IDs used anywhere in these playlists.
pub fn playlist_track_ids(nodes: &[PlaylistNode], out: &mut Vec<u32>) {
    for node in nodes {
        match &node.kind {
            PlaylistKind::Folder(children) => playlist_track_ids(children, out),
            PlaylistKind::Playlist(ids) => {
                for id in ids {
                    if !out.contains(id) {
                        out.push(*id);
                    }
                }
            }
        }
    }
}

/// Where imported music goes by default: `Music/RofLoxBox` in the home folder.
pub fn default_destination() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map_or_else(|| PathBuf::from("."), PathBuf::from).join("Music").join(crate::APP_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Imports the export in `DECKHAND_TEST_EXPORT` (skipped without it).
    #[test]
    fn imports_an_export() {
        let Some(root) = std::env::var_os("DECKHAND_TEST_EXPORT").map(PathBuf::from) else {
            return;
        };
        let library = crate::library::UsbLibrary::open(&root).unwrap();
        let destination =
            std::env::temp_dir().join(format!("deckhand-import-{}", std::process::id()));
        let request = ImportRequest {
            root,
            destination: destination.clone(),
            tracks: library.library.tracks.clone(),
            playlists: library.library.playlists.clone(),
        };
        let imported = run(&request, &AtomicUsize::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!(imported.library.tracks.len(), library.library.tracks.len() - imported.missing);
        for track in &imported.library.tracks {
            assert!(track.file.path.starts_with(&destination) && track.file.path.is_file());
            if let Some(dat) = &track.analysis {
                assert!(dat.is_file());
            }
            println!(
                "{}: {} cues, {} tempo markers, analysis {}",
                track.file.title,
                track.cues.len(),
                track.tempo.len(),
                track.analysis.is_some()
            );
        }
        let mut collection = deckhand_library::Collection::default();
        let summary = collection.import_into(imported.library, "USB test", true);
        assert_eq!(summary.added, library.library.tracks.len() - imported.missing);
        println!("{summary:?}, playlists in collection: {}", collection.tree().len());
        let _ = std::fs::remove_dir_all(&destination);
    }

    #[test]
    fn dates_become_unix_seconds() {
        assert_eq!(added_seconds("2026-10-25"), Some(1_792_886_400));
        assert_eq!(added_seconds(""), None);
    }
}
