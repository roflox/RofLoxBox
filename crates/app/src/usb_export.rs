//! Exporting collection playlists to a USB stick in rekordbox's device
//! library format: the music goes to `/Contents/<artist>/<album>/`, the
//! analysis (path, beat grid, waveforms, cues) to `/PIONEER/USBANLZ/`, and
//! tracks and the playlist are added to `/PIONEER/rekordbox/export.pdb`,
//! keeping everything already there.
//!
//! Tracks imported from a stick take rekordbox's own analysis along (with
//! the collection's cues and, when edited, beat grid); others get one worked
//! out from the audio, without the sections only rekordbox's analysis
//! produces (VBR index, colour overview, phrases).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use anyhow::{Context, Result};
use rofloxbox_formats::anlz::{
    AnlzFile, Beat, BeatGrid, Cue as AnlzCue, CueKind as AnlzCueKind, CueList, CueListKind,
    CueStatus, HotCueColor, Section, Waveform, WaveformKind,
};
use rofloxbox_formats::pdb::{
    Album, Artist, Database, DatabaseWriter, Genre, Key, Label, PlaylistTables, Track, TrackExtras,
    backup_path,
};
use rofloxbox_library::{CueKind, Track as CollectionTrack};

use crate::deck::{AnalysisMethod, KeyNotation, format_key, parse_key};

/// A playlist to put on a stick.
pub struct ExportRequest {
    /// The stick's root (where `PIONEER` and `Contents` go).
    pub root: PathBuf,
    pub playlist: String,
    /// A playlist on the stick (its id) the tracks are added to the end of,
    /// rather than the top-level `playlist` they make (or replace).
    pub add_to: Option<u32>,
    /// The playlist's tracks in order (a track may repeat).
    pub tracks: Vec<CollectionTrack>,
    /// For key names the stick does not have yet.
    pub key_notation: KeyNotation,
    /// How tracks without rekordbox's analysis are analysed.
    pub analysis: AnalysisMethod,
}

/// An export running in the background.
pub struct UsbExport {
    pub total: usize,
    pub done: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    result: mpsc::Receiver<Result<Exported>>,
}

pub struct Exported {
    /// Tracks copied to the stick now (others were there already).
    pub added: usize,
    /// Tracks whose audio file was missing.
    pub missing: usize,
    /// The playlist existed and was replaced.
    pub replaced: bool,
}

impl UsbExport {
    pub fn start(request: ExportRequest) -> Self {
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

    pub fn finished(&self) -> Option<Result<Exported>> {
        self.result.try_recv().ok()
    }
}

/// The rows of the tables an export adds to.
struct Rows {
    tracks: Vec<Track>,
    artists: Vec<Artist>,
    albums: Vec<Album>,
    genres: Vec<Genre>,
    labels: Vec<Label>,
    keys: Vec<Key>,
    playlists: PlaylistTables,
}

/// Runs an export in this thread (tests).
#[cfg(test)]
pub fn run_for_tests(request: &ExportRequest) -> Result<Exported> {
    run(request, &AtomicUsize::new(0), &AtomicBool::new(false))
}

fn run(request: &ExportRequest, done: &AtomicUsize, cancel: &AtomicBool) -> Result<Exported> {
    let pdb = request.root.join("PIONEER/rekordbox/export.pdb");
    let existing = if pdb.is_file() { Some(Database::open(&pdb)?) } else { None };
    let mut rows = match &existing {
        Some(db) => Rows {
            tracks: db.read_all()?,
            artists: db.read_all()?,
            albums: db.read_all()?,
            genres: db.read_all()?,
            labels: db.read_all()?,
            keys: db.read_all()?,
            playlists: PlaylistTables::read(db)?,
        },
        None => Rows {
            tracks: Vec::new(),
            artists: Vec::new(),
            albums: Vec::new(),
            genres: Vec::new(),
            labels: Vec::new(),
            keys: Vec::new(),
            playlists: PlaylistTables { nodes: Vec::new(), entries: Vec::new() },
        },
    };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    // A value rekordbox keeps the same in every track row of one database.
    let database_pair = rows
        .tracks
        .first()
        .map_or(TrackExtras::default().database_pair, |t| t.extras.database_pair);

    let mut on_stick: HashMap<u32, u32> = HashMap::new();
    let (mut added, mut missing) = (0, 0);
    let mut entries = Vec::new();
    for track in &request.tracks {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        let id = match on_stick.get(&track.id) {
            Some(&id) => Some(id),
            None => {
                let id = put_track(request, track, &mut rows, &today, database_pair, &mut added)?;
                if let Some(id) = id {
                    on_stick.insert(track.id, id);
                } else {
                    missing += 1;
                }
                id
            }
        };
        entries.extend(id);
        done.fetch_add(1, Ordering::Relaxed);
    }

    let mut existing_playlist = None;
    match request.add_to.filter(|id| rows.playlists.nodes.iter().any(|n| n.id == *id && !n.is_folder)) {
        // Added after the playlist's own tracks.
        Some(playlist) => rows.playlists.add_tracks(playlist, &entries),
        // The playlist: replaced if one of that name is at the top level.
        None => {
            existing_playlist = rows
                .playlists
                .nodes
                .iter()
                .find(|n| n.parent_id.is_none() && !n.is_folder && n.name == request.playlist)
                .map(|n| n.id);
            let playlist = existing_playlist
                .unwrap_or_else(|| rows.playlists.create(&request.playlist, false, None));
            rows.playlists.set_tracks(playlist, &entries);
        }
    }

    let mut writer = match &existing {
        Some(db) => DatabaseWriter::from_database(db)?,
        None => DatabaseWriter::new_export(&today),
    };
    writer.replace_rows(&rows.tracks);
    writer.replace_rows(&rows.artists);
    writer.replace_rows(&rows.albums);
    writer.replace_rows(&rows.genres);
    writer.replace_rows(&rows.labels);
    writer.replace_rows(&rows.keys);
    writer.replace_rows(&rows.playlists.nodes);
    writer.replace_rows(&rows.playlists.entries);
    let bytes = writer.to_bytes();
    std::fs::create_dir_all(pdb.parent().expect("has a parent"))?;
    if existing.is_some() {
        let backup = backup_path(&pdb);
        if !backup.exists() {
            std::fs::copy(&pdb, &backup)?;
        }
    } else {
        let ext = pdb.with_file_name("exportExt.pdb");
        if !ext.exists() {
            std::fs::write(&ext, DatabaseWriter::new_export_ext().to_bytes())?;
        }
    }
    let temporary = pdb.with_extension("pdb.rofloxbox-new");
    std::fs::write(&temporary, &bytes)?;
    std::fs::rename(&temporary, &pdb)?;
    Ok(Exported { added, missing, replaced: existing_playlist.is_some() })
}

/// Copies a track to the stick (unless it is there already) and returns
/// its id there; `None` when its audio file is missing.
fn put_track(
    request: &ExportRequest,
    track: &CollectionTrack,
    rows: &mut Rows,
    today: &str,
    database_pair: (u16, u16),
    added: &mut usize,
) -> Result<Option<u32>> {
    if !track.path.is_file() {
        return Ok(None);
    }
    let filename = track
        .path
        .file_name()
        .map_or_else(|| "track".to_owned(), |n| safe_name(&n.to_string_lossy()));
    let folder = |value: &str, unknown: &str| {
        if value.trim().is_empty() { unknown.to_owned() } else { safe_name(value) }
    };
    let file_path = format!(
        "/Contents/{}/{}/{filename}",
        folder(&track.artist, "UnknownArtist"),
        folder(&track.album, "UnknownAlbum")
    );
    if let Some(known) = rows.tracks.iter().find(|t| t.file_path == file_path) {
        return Ok(Some(known.id));
    }

    let destination = below(&request.root, &file_path);
    copy_file(&track.path, &destination)?;
    let analyze_path = unused_analysis_path(&file_path, &rows.tracks);
    write_analysis(&request.root, &analyze_path, &file_path, track, request.analysis)?;

    let id = rows.tracks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    let artist_id = name_row(&mut rows.artists, &track.artist, |id, name| Artist { id, name });
    let album_id = (!track.album.trim().is_empty()).then(|| {
        match rows.albums.iter().find(|a| a.name == track.album && a.artist_id == artist_id) {
            Some(album) => album.id,
            None => {
                let id = rows.albums.iter().map(|a| a.id).max().unwrap_or(0) + 1;
                rows.albums.push(Album { id, artist_id, name: track.album.clone() });
                id
            }
        }
    });
    let genre_id = name_row(&mut rows.genres, &track.genre, |id, name| Genre { id, name });
    let label_id = name_row(&mut rows.labels, &track.label, |id, name| Label { id, name });
    let key_id = track.key.as_deref().and_then(parse_key).map(|key| {
        match rows.keys.iter().find(|k| parse_key(&k.name) == Some(key)) {
            Some(known) => known.id,
            None => {
                let id = rows.keys.iter().map(|k| k.id).max().unwrap_or(0) + 1;
                rows.keys.push(Key { id, name: format_key(key, request.key_notation) });
                id
            }
        }
    });
    let bpm = track.bpm.or_else(|| track.tempo.first().map(|m| m.bpm)).unwrap_or(0.0);
    let added_date = chrono::DateTime::from_timestamp(track.added as i64, 0)
        .map_or_else(|| today.to_owned(), |d| d.format("%Y-%m-%d").to_string());
    rows.tracks.push(Track {
        id,
        title: track.title.clone(),
        artist_id,
        composer_id: None,
        original_artist_id: None,
        remixer_id: None,
        album_id,
        genre_id,
        label_id,
        key_id,
        artwork_id: None,
        color_id: track.color.filter(|c| (1..=8).contains(c)),
        rating: track.rating.min(5),
        tempo: (bpm * 100.0).round() as u32,
        duration_secs: track.duration.round().clamp(0.0, f64::from(u16::MAX)) as u16,
        sample_rate: track.sample_rate.unwrap_or(44_100),
        sample_depth: 16,
        bitrate: track.bitrate.unwrap_or(0),
        file_size: std::fs::metadata(&destination)
            .map_or(0, |m| m.len().min(u64::from(u32::MAX)) as u32),
        track_number: track.track_number.unwrap_or(0),
        disc_number: 0,
        year: track.year.unwrap_or(0),
        play_count: track.play_count.min(u32::from(u16::MAX)) as u16,
        isrc: String::new(),
        mix_name: String::new(),
        comment: track.comment.clone(),
        message: String::new(),
        date_added: added_date,
        release_date: String::new(),
        analyze_path,
        analyze_date: today.to_owned(),
        filename,
        file_path: file_path.clone(),
        kuvo_public: true,
        autoload_hot_cues: true,
        extras: TrackExtras {
            unknown_id: hash(&file_path),
            database_pair,
            ..TrackExtras::default()
        },
    });
    *added += 1;
    Ok(Some(id))
}

/// The id of the row naming `name`, added if missing; `None` for no name.
fn name_row<T>(rows: &mut Vec<T>, name: &str, make: impl Fn(u32, String) -> T) -> Option<u32>
where
    T: NamedRow,
{
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if let Some(row) = rows.iter().find(|r| r.name() == name) {
        return Some(row.id());
    }
    let id = rows.iter().map(NamedRow::id).max().unwrap_or(0) + 1;
    rows.push(make(id, name.to_owned()));
    Some(id)
}

trait NamedRow {
    fn id(&self) -> u32;
    fn name(&self) -> &str;
}

macro_rules! named_row {
    ($($row:ty),*) => {$(
        impl NamedRow for $row {
            fn id(&self) -> u32 {
                self.id
            }
            fn name(&self) -> &str {
                &self.name
            }
        }
    )*};
}
named_row!(Artist, Genre, Label);

/// A path component the FAT and exFAT file systems of sticks accept.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').to_owned();
    if cleaned.is_empty() { "_".to_owned() } else { cleaned }
}

/// FNV-1a, for analysis folder names and a per-track value.
fn hash(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

/// `/PIONEER/USBANLZ/Pxxx/xxxxxxxx/ANLZ0000.DAT`, like rekordbox's, named
/// from the track's path and not used by another track.
fn unused_analysis_path(file_path: &str, tracks: &[Track]) -> String {
    let mut h = hash(file_path);
    loop {
        let path = format!("/PIONEER/USBANLZ/P{:03X}/{h:08X}/ANLZ0000.DAT", h >> 20);
        if !tracks.iter().any(|t| t.analyze_path == path) {
            return path;
        }
        h = h.wrapping_add(1);
    }
}

fn below(root: &Path, export_path: &str) -> PathBuf {
    root.join(export_path.trim_start_matches('/'))
}

fn copy_file(from: &Path, to: &Path) -> Result<()> {
    let size = |p: &Path| std::fs::metadata(p).ok().map(|m| m.len());
    if to.is_file() && size(from) == size(to) {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    Ok(())
}

/// Writes the track's `.DAT` and `.EXT` (and `.2EX` when rekordbox made one)
/// at `analyze_path` below `root` (a stick, or rekordbox's `share` folder).
pub(crate) fn write_analysis(
    root: &Path,
    analyze_path: &str,
    file_path: &str,
    track: &CollectionTrack,
    method: AnalysisMethod,
) -> Result<()> {
    let dat_path = below(root, analyze_path);
    if let Some(parent) = dat_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let source = track.analysis.as_ref().filter(|p| p.is_file());
    let (dat, ext, two_ex) = match source {
        Some(source) => {
            let dat = AnlzFile::open(source)?;
            let ext = AnlzFile::open(source.with_extension("EXT")).ok();
            let two_ex = AnlzFile::open(source.with_extension("2EX")).ok();
            rekordbox_analysis(dat, ext, two_ex, file_path, track)
        }
        None => {
            let audio = rofloxbox_audio::decode_file(&track.path)
                .with_context(|| format!("reading {}", track.path.display()))?;
            let (dat, ext) = own_analysis(&audio, file_path, track, method);
            (dat, Some(ext), Some(three_band_analysis(&audio, file_path)))
        }
    };
    std::fs::write(&dat_path, dat.to_bytes())?;
    if let Some(ext) = ext {
        std::fs::write(dat_path.with_extension("EXT"), ext.to_bytes())?;
    }
    if let Some(two_ex) = two_ex {
        std::fs::write(dat_path.with_extension("2EX"), two_ex.to_bytes())?;
    }
    Ok(())
}

/// rekordbox's analysis with the new path, the collection's cues and, when
/// it differs from rekordbox's, the collection's beat grid.
fn rekordbox_analysis(
    mut dat: AnlzFile,
    ext: Option<AnlzFile>,
    two_ex: Option<AnlzFile>,
    file_path: &str,
    track: &CollectionTrack,
) -> (AnlzFile, Option<AnlzFile>, Option<AnlzFile>) {
    // The grid is rewritten only when it was edited in RofLoxBox: compared
    // by tempo markers (the collection keeps the length in whole seconds,
    // so the last beat may differ).
    let grid_changed = match dat.beat_grid() {
        Some(original) => !same_markers(&crate::usb_import::grid_markers(original), &track.tempo),
        None => !track.tempo.is_empty(),
    };
    let grid = collection_grid(track);
    let original_cues: Vec<AnlzCue> = ext
        .iter()
        .chain([&dat])
        .flat_map(AnlzFile::cue_lists)
        .flat_map(|l| l.cues.clone())
        .collect();
    let set_path = |file: &mut AnlzFile| {
        for section in &mut file.sections {
            if let Section::Path(path) = section {
                *path = file_path.to_owned();
            }
        }
    };
    // The basic memory lists: the collection's, with its active loop.
    let basic = |file: &mut AnlzFile| {
        for section in &mut file.sections {
            if let Section::Cues(list) = section
                && !list.extended
                && list.kind == CueListKind::Memory
            {
                list.cues = basic_memory(&track.cues, &list.cues);
            }
        }
    };
    set_path(&mut dat);
    basic(&mut dat);
    if grid_changed {
        replace_grid(&mut dat, grid);
    }
    let ext = ext.map(|mut ext| {
        set_path(&mut ext);
        basic(&mut ext);
        if grid_changed {
            // Its summary of the old grid would no longer match.
            ext.sections.retain(|s| !matches!(s, Section::ExtendedBeatGrid(_)));
        }
        let (hot, memory) = collection_cues(track, &original_cues);
        let mut put = Some((hot, memory));
        let mut sections = Vec::with_capacity(ext.sections.len());
        for section in ext.sections {
            match section {
                Section::Cues(list) if list.extended => {
                    // Both extended lists go where the first one was.
                    if let Some((hot, memory)) = put.take() {
                        sections.push(Section::Cues(hot));
                        sections.push(Section::Cues(memory));
                    }
                }
                other => sections.push(other),
            }
        }
        if let Some((hot, memory)) = put {
            sections.push(Section::Cues(hot));
            sections.push(Section::Cues(memory));
        }
        AnlzFile { sections }
    });
    let two_ex = two_ex.map(|mut f| {
        set_path(&mut f);
        f
    });
    (dat, ext, two_ex)
}

pub(crate) fn replace_grid(file: &mut AnlzFile, grid: BeatGrid) {
    if let Some(section) = file.sections.iter_mut().find(|s| matches!(s, Section::BeatGrid(_))) {
        *section = Section::BeatGrid(grid);
    } else {
        let at =
            file.sections.iter().position(|s| matches!(s, Section::Path(_))).map_or(0, |i| i + 1);
        file.sections.insert(at, Section::BeatGrid(grid));
    }
}

pub(crate) fn same_markers(
    a: &[rofloxbox_library::TempoMarker],
    b: &[rofloxbox_library::TempoMarker],
) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            (x.start - y.start).abs() < 0.002 && (x.bpm - y.bpm).abs() < 0.005 && x.beat == y.beat
        })
}

/// The collection's beat grid as rekordbox stores it: every beat.
pub(crate) fn collection_grid(track: &CollectionTrack) -> BeatGrid {
    let mut beats = Vec::new();
    for (i, marker) in track.tempo.iter().enumerate() {
        if marker.bpm <= 0.0 {
            continue;
        }
        let end = track.tempo.get(i + 1).map_or(track.duration, |next| next.start);
        let step = 60.0 / marker.bpm;
        let mut n = 0u32;
        loop {
            let time = marker.start + f64::from(n) * step;
            if time >= end - 1e-6 {
                break;
            }
            beats.push(Beat {
                bar_position: ((u32::from(marker.beat.clamp(1, 4)) - 1 + n) % 4 + 1) as u16,
                tempo: (marker.bpm * 100.0).round() as u16,
                time_ms: (time * 1000.0).round().max(0.0) as u32,
            });
            n += 1;
        }
    }
    BeatGrid { beats }
}

/// The collection's hot cues and memory cues/loops as extended cue lists;
/// cues rekordbox analysed keep their stored details (color codes and all).
fn collection_cues(track: &CollectionTrack, original: &[AnlzCue]) -> (CueList, CueList) {
    let mut hot = Vec::new();
    let mut memory = Vec::new();
    for cue in &track.cues {
        let time_ms = (cue.start * 1000.0).round().max(0.0) as u32;
        let loop_end_ms = cue.end.map(|end| (end * 1000.0).round().max(0.0) as u32);
        let hot_cue = match cue.kind {
            CueKind::Hot(n) => Some(u32::from(n) + 1),
            CueKind::Memory => None,
        };
        let known = original.iter().find(|o| {
            o.hot_cue == hot_cue
                && o.time_ms.abs_diff(time_ms) <= 1
                && match (o.loop_end_ms, loop_end_ms) {
                    (Some(a), Some(b)) => a.abs_diff(b) <= 1,
                    (a, b) => a.is_none() && b.is_none(),
                }
        });
        let entry = known.cloned().unwrap_or_else(|| AnlzCue {
            hot_cue,
            kind: if loop_end_ms.is_some() { AnlzCueKind::Loop } else { AnlzCueKind::Cue },
            time_ms,
            loop_end_ms,
            status: None,
            color_id: None,
            loop_beats: None,
            comment: Some(cue.name.clone()),
            hot_cue_color: hot_cue.and(cue.color).map(|rgb| HotCueColor { code: 0, rgb }),
            order: None,
            trailer: Vec::new(),
        });
        if hot_cue.is_some() { hot.push(entry) } else { memory.push(entry) }
    }
    hot.sort_by_key(|c| c.hot_cue);
    memory.sort_by_key(|c| c.time_ms);
    let list = |kind, cues| CueList { kind, extended: true, memory_count: 0, cues };
    (list(CueListKind::Hot, hot), list(CueListKind::Memory, memory))
}

/// The collection's memory cues and loops as a basic list's entries, the
/// active loop flagged there (where players and RofLoxBox read it from);
/// entries already in the list keep their details.
fn basic_memory(cues: &[rofloxbox_library::Cue], original: &[AnlzCue]) -> Vec<AnlzCue> {
    let mut cues: Vec<AnlzCue> = cues
        .iter()
        .filter(|c| c.kind == CueKind::Memory)
        .map(|cue| {
            let time_ms = (cue.start * 1000.0).round().max(0.0) as u32;
            let loop_end_ms = cue.end.map(|end| (end * 1000.0).round().max(0.0) as u32);
            let known = original.iter().find(|o| {
                o.hot_cue.is_none()
                    && o.time_ms.abs_diff(time_ms) <= 1
                    && o.loop_end_ms.is_some() == loop_end_ms.is_some()
            });
            let mut entry = known.cloned().unwrap_or_else(|| AnlzCue {
                hot_cue: None,
                kind: if loop_end_ms.is_some() { AnlzCueKind::Loop } else { AnlzCueKind::Cue },
                time_ms,
                loop_end_ms,
                status: None,
                color_id: None,
                loop_beats: None,
                comment: None,
                hot_cue_color: None,
                order: None,
                trailer: Vec::new(),
            });
            entry.loop_end_ms = loop_end_ms;
            let active = cue.active && loop_end_ms.is_some();
            entry.status = Some(if active { CueStatus::ActiveLoop } else { CueStatus::Enabled });
            // Linked again in their new order.
            entry.order = None;
            entry
        })
        .collect();
    cues.sort_by_key(|c| c.time_ms);
    cues
}

/// Writes just the `.2EX` (3-band waveforms) of the analysis at
/// `analyze_path` below `root`, worked out from the track's audio.
pub(crate) fn write_three_band(
    root: &Path,
    analyze_path: &str,
    file_path: &str,
    track: &CollectionTrack,
) -> Result<()> {
    let audio = rofloxbox_audio::decode_file(&track.path)
        .with_context(|| format!("reading {}", track.path.display()))?;
    let two_ex = three_band_analysis(&audio, file_path);
    std::fs::write(below(root, analyze_path).with_extension("2EX"), two_ex.to_bytes())?;
    Ok(())
}

/// The `.2EX`: the 3-band waveforms rekordbox 7 and CDJ-3000s show (lows,
/// mids, highs; 150 columns a second and a 1200-column overview), as
/// rekordbox lays them out: each band's levels 0–127, and in `PWVC` each
/// band's level its peak stands for (the peak is 1.27 of it; never under
/// 80).
fn three_band_analysis(audio: &rofloxbox_audio::TrackAudio, file_path: &str) -> AnlzFile {
    let bands = rofloxbox_audio::band_waveform(audio);
    let peaks: [f32; 3] = std::array::from_fn(|b| {
        bands.iter().map(|c| c[b]).fold(0f32, f32::max).max(1e-6)
    });
    // Levels show louder than amplitudes would (rekordbox's are fuller).
    let level =
        |v: f32, b: usize| ((v / peaks[b]).clamp(0.0, 1.0).powf(0.7) * 127.0).round() as u8;
    let detail: Vec<u8> = bands.iter().flat_map(|c| [0, 1, 2].map(|b| level(c[b], b))).collect();
    const OVERVIEW: usize = 1200;
    let columns = detail.len() / 3;
    let overview: Vec<u8> = (0..OVERVIEW)
        .flat_map(|i| {
            let from = i * columns / OVERVIEW;
            let to = ((i + 1) * columns / OVERVIEW).max(from + 1).min(columns.max(1));
            let band = |b: usize| {
                (from..to).map(|c| detail.get(c * 3 + b).copied().unwrap_or(0)).max().unwrap_or(0)
            };
            [0, 1, 2].map(band)
        })
        .collect();
    let peak_level = |b: usize| -> u16 {
        let top = (0..columns).map(|c| detail[c * 3 + b]).max().unwrap_or(0);
        ((f32::from(top) / 1.27).round() as u16).max(80)
    };
    let mut pwvc = b"PWVC".to_vec();
    pwvc.extend(14u32.to_be_bytes());
    pwvc.extend(20u32.to_be_bytes());
    pwvc.extend([0, 0]);
    for b in 0..3 {
        pwvc.extend(peak_level(b).to_be_bytes());
    }
    let waveform = |kind, data| Section::Waveform(Waveform { kind, entry_len: 3, data });
    AnlzFile {
        sections: vec![
            Section::Path(file_path.to_owned()),
            waveform(WaveformKind::ThreeBandDetail, detail),
            waveform(WaveformKind::ThreeBandPreview, overview),
            Section::Unknown { tag: *b"PWVC", bytes: pwvc },
        ],
    }
}

/// `.DAT` and `.EXT` worked out from the audio, in rekordbox's section order.
fn own_analysis(
    audio: &rofloxbox_audio::TrackAudio,
    file_path: &str,
    track: &CollectionTrack,
    method: AnalysisMethod,
) -> (AnlzFile, AnlzFile) {
    let peaks = rofloxbox_audio::peak_waveform(audio);
    let bands = rofloxbox_audio::band_waveform(audio);
    // As floats, once, for rekordbox's analyses.
    let samples = audio.to_f32();
    let top = peaks.iter().copied().fold(0f32, f32::max).max(1e-6);
    let level = |i: usize| peaks.get(i).copied().unwrap_or(0.0) / top;
    let highs = |i: usize| bands.get(i).map_or(0.0, |b| b[2]);
    // One byte: 5 bits of height, 3 bits of whiteness (how much high end).
    let mono = |height: f32, high: f32| -> u8 {
        let h = (height.clamp(0.0, 1.0) * 31.0).round() as u8;
        let w = (high.clamp(0.0, 1.0) * 7.0).round() as u8;
        h | (w << 5)
    };
    // Squeezes the 150-per-second columns into `count` columns.
    let squeeze = |count: usize| -> Vec<(f32, f32)> {
        let per = peaks.len().max(1) as f64 / count as f64;
        (0..count)
            .map(|c| {
                let from = (c as f64 * per) as usize;
                let range = from..(((c + 1) as f64 * per) as usize).max(from + 1);
                let height = range.clone().map(level).fold(0f32, f32::max);
                let high = range.clone().map(highs).fold(0f32, f32::max);
                (height, high)
            })
            .collect()
    };
    // The 400-column preview and 100 dots: rekordbox's, or squeezed from
    // the peaks.
    let (preview, tiny): (Vec<u8>, Vec<u8>) = match method {
        AnalysisMethod::Rekordbox => {
            rofloxbox_analysis::preview::preview(&samples, audio.sample_rate)
        }
        AnalysisMethod::RofLoxBox => (
            squeeze(400).into_iter().map(|(h, w)| mono(h, w)).collect(),
            squeeze(100)
                .into_iter()
                .map(|(h, _)| (h.clamp(0.0, 1.0) * 15.0).round() as u8)
                .collect(),
        ),
    };
    // Two bytes: 3 bits each of red (lows), green (mids) and blue (highs),
    // then 5 bits of height — matching rekordbox's own files, or simply
    // from the bands.
    let color: Vec<u16> = match method {
        AnalysisMethod::Rekordbox => {
            rofloxbox_analysis::waveform::color_detail(&samples, audio.sample_rate)
        }
        AnalysisMethod::RofLoxBox => (0..peaks.len())
            .map(|i| {
                let [low, mid, high] = bands.get(i).copied().unwrap_or([0.0; 3]);
                let channel = |v: f32| u16::from((v.clamp(0.0, 1.0) * 7.0).round() as u8);
                let height = u16::from((level(i).clamp(0.0, 1.0) * 31.0).round() as u8);
                channel(low) << 13 | channel(mid) << 10 | channel(high) << 7 | height << 2
            })
            .collect(),
    };
    // The mono detail: rekordbox's, or simply from the peaks with as many
    // columns as the colour one.
    let detail: Vec<u8> = match method {
        AnalysisMethod::Rekordbox => {
            rofloxbox_analysis::waveform::mono_detail(&samples, audio.sample_rate)
        }
        AnalysisMethod::RofLoxBox => (0..color.len()).map(|i| mono(level(i), highs(i))).collect(),
    };
    let color: Vec<u8> = color.iter().flat_map(|v| v.to_be_bytes()).collect();
    let waveform = |kind, entry_len, data| Section::Waveform(Waveform { kind, entry_len, data });
    let (hot, memory) = collection_cues(track, &[]);
    let empty = |kind| {
        Section::Cues(CueList { kind, extended: false, memory_count: u32::MAX, cues: Vec::new() })
    };

    // rekordbox's own files have the VBR index next, all zeros but for some
    // MP3s (401 entries).
    let mut dat = vec![Section::Path(file_path.to_owned()), Section::Vbr(vec![0; 401])];
    let grid = collection_grid(track);
    if !grid.beats.is_empty() {
        dat.push(Section::BeatGrid(grid));
    }
    dat.extend([
        waveform(WaveformKind::Preview, 1, preview),
        waveform(WaveformKind::TinyPreview, 1, tiny),
        empty(CueListKind::Hot),
        Section::Cues(CueList {
            kind: CueListKind::Memory,
            extended: false,
            memory_count: u32::MAX,
            cues: basic_memory(&track.cues, &[]),
        }),
    ]);
    let mut ext = vec![
        Section::Path(file_path.to_owned()),
        waveform(WaveformKind::Detail, 1, detail),
        empty(CueListKind::Hot),
        empty(CueListKind::Memory),
        Section::Cues(hot),
        Section::Cues(memory),
        waveform(WaveformKind::ColorDetail, 2, color),
    ];
    // The colour overview, which only the rekordbox-compatible analysis has.
    if method == AnalysisMethod::Rekordbox {
        let overview =
            rofloxbox_analysis::waveform::color_overview(&samples, audio.sample_rate);
        let data = overview.iter().flatten().map(|&b| b as u8).collect();
        ext.push(waveform(WaveformKind::ColorPreview, 6, data));
    }
    (AnlzFile { sections: dat }, AnlzFile { sections: ext })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The basic memory list holds the collection's memory cues and loops
    /// in order, the active loop flagged, entries rekordbox wrote kept.
    #[test]
    fn basic_memory_list_flags_the_active_loop() {
        let cue = |start: f64, end: Option<f64>, active| rofloxbox_library::Cue {
            kind: CueKind::Memory,
            start,
            end,
            color: None,
            name: String::new(),
            active,
        };
        let cues = [cue(30.0, Some(34.0), true), cue(10.0, None, false), cue(50.0, Some(52.0), false)];
        let original = AnlzCue {
            hot_cue: None,
            kind: AnlzCueKind::Loop,
            time_ms: 30_000,
            loop_end_ms: Some(34_000),
            status: Some(CueStatus::Enabled),
            color_id: Some(3),
            loop_beats: None,
            comment: None,
            hot_cue_color: None,
            order: Some((7, 9)),
            trailer: vec![1, 2],
        };
        let list = basic_memory(&cues, &[original]);
        let got: Vec<(u32, Option<u32>, Option<CueStatus>)> =
            list.iter().map(|c| (c.time_ms, c.loop_end_ms, c.status)).collect();
        assert_eq!(
            got,
            [
                (10_000, None, Some(CueStatus::Enabled)),
                (30_000, Some(34_000), Some(CueStatus::ActiveLoop)),
                (50_000, Some(52_000), Some(CueStatus::Enabled)),
            ]
        );
        assert_eq!((list[1].color_id, list[1].trailer.as_slice(), list[1].order), (Some(3), &[1u8, 2][..], None));
    }

    #[test]
    fn names_fit_usb_file_systems() {
        assert_eq!(safe_name("AC/DC: Live?"), "AC_DC_ Live_");
        assert_eq!(safe_name("  ..."), "_");
        assert_eq!(safe_name("Track."), "Track");
    }

    #[test]
    fn analysis_paths_look_like_rekordbox_ones_and_differ() {
        let first = unused_analysis_path("/Contents/A/B/x.mp3", &[]);
        assert!(first.starts_with("/PIONEER/USBANLZ/P") && first.ends_with("/ANLZ0000.DAT"));
        let parts: Vec<&str> = first.split('/').collect();
        assert_eq!((parts[3].len(), parts[4].len()), (4, 8));
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;

    /// A one-second 44.1 kHz stereo WAV of a quiet tone.
    fn wav(path: &Path, hz: f32) {
        let frames = 44_100u32;
        let mut bytes = Vec::new();
        bytes.extend(b"RIFF");
        bytes.extend((36 + frames * 4).to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(2u16.to_le_bytes());
        bytes.extend(44_100u32.to_le_bytes());
        bytes.extend((44_100u32 * 4).to_le_bytes());
        bytes.extend(4u16.to_le_bytes());
        bytes.extend(16u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend((frames * 4).to_le_bytes());
        for i in 0..frames {
            let v = ((i as f32 * hz * std::f32::consts::TAU / 44_100.0).sin() * 8000.0) as i16;
            bytes.extend(v.to_le_bytes());
            bytes.extend(v.to_le_bytes());
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// Tracks dropped on a stick's playlist go after the ones it has, in
    /// the shared form libraries hand tracks over in.
    #[test]
    fn adds_tracks_to_a_playlist_on_the_stick() {
        let temp = std::env::temp_dir().join(format!("rofloxbox-add-to-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let mut collection = rofloxbox_library::Collection::default();
        let files: Vec<PathBuf> = (0..2).map(|i| temp.join(format!("music/T{i}.wav"))).collect();
        for (i, file) in files.iter().enumerate() {
            wav(file, 220.0 * (i + 1) as f32);
        }
        let scanned = files
            .iter()
            .map(|f| rofloxbox_library::read_file(f).unwrap())
            .collect();
        collection.merge(scanned);
        let stick = temp.join("stick");
        let export = |tracks: Vec<CollectionTrack>, add_to| ExportRequest {
            root: stick.clone(),
            playlist: "Set".into(),
            add_to,
            tracks,
            key_notation: KeyNotation::Alphanumeric,
            analysis: AnalysisMethod::RofLoxBox,
        };
        let first = collection.tracks[0].clone();
        run(&export(vec![first], None), &AtomicUsize::new(0), &AtomicBool::new(false)).unwrap();
        let on_stick = crate::library::UsbLibrary::open(&stick).unwrap();
        let set = on_stick.playlists.iter().find(|p| p.name == "Set").unwrap().id;
        // Handed over and taken back, as a drop does.
        let shared = rofloxbox_library::XmlTrack::from_track(&collection.tracks[1]);
        let added = run(
            &export(vec![shared.to_track(1)], Some(set)),
            &AtomicUsize::new(0),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((added.added, added.replaced), (1, false));
        let on_stick = crate::library::UsbLibrary::open(&stick).unwrap();
        let row = on_stick.playlists.iter().find(|p| p.id == set).unwrap();
        let titles: Vec<String> = row
            .track_ids
            .as_ref()
            .unwrap()
            .iter()
            .map(|id| on_stick.library.track(*id).unwrap().track.title.clone())
            .collect();
        assert_eq!(titles, ["T0", "T1"]);
        assert_eq!(on_stick.playlists.iter().filter(|p| p.track_ids.is_some()).count(), 1);
        let _ = std::fs::remove_dir_all(temp);
    }

    /// Imports the export in `ROFLOXBOX_TEST_EXPORT` into a collection, then
    /// exports it to a new stick (skipped without it).
    #[test]
    fn exports_to_a_new_stick_and_again() {
        let Some(source) = std::env::var_os("ROFLOXBOX_TEST_EXPORT").map(PathBuf::from) else {
            return;
        };
        let temp = std::env::temp_dir().join(format!("rofloxbox-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        // Into a collection.
        let library = crate::library::UsbLibrary::open(&source).unwrap();
        let request = crate::usb_import::ImportRequest {
            root: source,
            destination: temp.join("music"),
            tracks: library.library.tracks.clone(),
            playlists: Vec::new(),
        };
        let imported = crate::usb_import::run_for_tests(&request).unwrap();
        let mut collection = rofloxbox_library::Collection::default();
        collection.import_into(imported.library, "USB test", true);
        let mut tracks = collection.tracks.clone();
        assert!(tracks.len() >= 2);
        // One track with rekordbox's analysis and an extra cue, one without.
        tracks[0].cues.push(rofloxbox_library::Cue {
            kind: CueKind::Hot(7),
            start: 1.5,
            end: None,
            color: Some([10, 20, 30]),
            name: "Added".into(),
            active: false,
        });
        tracks[1].analysis = None;
        let stick = temp.join("stick");
        let request = ExportRequest {
            add_to: None,
            root: stick.clone(),
            playlist: "Exported".into(),
            tracks: vec![tracks[0].clone(), tracks[1].clone(), tracks[0].clone()],
            key_notation: KeyNotation::Alphanumeric,
            analysis: AnalysisMethod::Rekordbox,
        };
        let result = run(&request, &AtomicUsize::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!((result.added, result.missing, result.replaced), (2, 0, false));

        // Read back like a player would.
        let exported = crate::library::UsbLibrary::open(&stick).unwrap();
        assert_eq!(exported.library.tracks.len(), 2);
        let list = exported.playlists.iter().find(|p| p.name == "Exported").unwrap();
        assert_eq!(list.track_ids.as_ref().unwrap().len(), 3, "repeats are kept");
        for t in &exported.library.tracks {
            assert!(exported.resolve(&t.track.file_path).is_file(), "{}", t.track.file_path);
            let dat = AnlzFile::open(exported.resolve(&t.track.analyze_path)).unwrap();
            let ext = AnlzFile::open(exported.resolve(&t.track.analyze_path).with_extension("EXT"))
                .unwrap();
            assert_eq!(dat.path(), Some(t.track.file_path.as_str()));
            assert!(dat.waveform(WaveformKind::Preview).is_some());
            assert!(dat.waveform(WaveformKind::TinyPreview).is_some());
            assert!(ext.waveform(WaveformKind::Detail).is_some());
            assert!(ext.waveform(WaveformKind::ColorDetail).is_some());
            let hot: Vec<_> = ext
                .cue_lists()
                .filter(|l| l.extended && l.kind == CueListKind::Hot)
                .flat_map(|l| l.cues.clone())
                .collect();
            println!(
                "{}: grid {} beats, {} hot cues, sections DAT {:?}",
                t.track.title,
                dat.beat_grid().map_or(0, |g| g.beats.len()),
                hot.len(),
                dat.sections
                    .iter()
                    .map(Section::tag)
                    .map(|t| String::from_utf8_lossy(&t).into_owned())
                    .collect::<Vec<_>>()
            );
            if t.track.title == tracks[0].title {
                assert!(hot.iter().any(|c| c.hot_cue == Some(8) && c.time_ms == 1500));
                // Its grid was not edited: rekordbox's stays, summary and all.
                assert_eq!(dat.beat_grid().map(|g| g.beats.len()), Some(368));
                assert!(ext.sections.iter().any(|s| matches!(s, Section::ExtendedBeatGrid(_))));
            }
        }

        // Exporting again adds nothing and replaces the playlist.
        let again = run(&request, &AtomicUsize::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!((again.added, again.replaced), (0, true));
        let exported = crate::library::UsbLibrary::open(&stick).unwrap();
        assert_eq!(exported.library.tracks.len(), 2);
        assert_eq!(exported.playlists.iter().filter(|p| p.name == "Exported").count(), 1);
        assert!(backup_path(&stick.join("PIONEER/rekordbox/export.pdb")).is_file());
        let _ = std::fs::remove_dir_all(&temp);
    }
}
