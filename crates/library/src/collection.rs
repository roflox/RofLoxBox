//! The collection model and its storage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::rekordbox_xml::{XmlLibrary, XmlNode};
use crate::scan::ScannedFile;

/// The name of the folder a rekordbox import puts its playlists in.
pub const REKORDBOX_FOLDER: &str = "rekordbox";

/// What a rekordbox import did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ImportSummary {
    pub added: usize,
    pub updated: usize,
    pub playlists: usize,
    /// Tracks whose file was not found on this computer.
    pub missing: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: u32,
    pub path: PathBuf,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub genre: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub comment: String,
    pub year: Option<u16>,
    pub track_number: Option<u32>,
    /// Seconds.
    pub duration: f64,
    pub bpm: Option<f64>,
    /// As written in the tags (`Am`, `8A`, …).
    pub key: Option<String>,
    pub sample_rate: Option<u32>,
    /// kbit/s.
    pub bitrate: Option<u32>,
    #[serde(default)]
    pub has_artwork: bool,
    /// 0–5 stars.
    #[serde(default)]
    pub rating: u8,
    /// rekordbox colour label 1–8.
    pub color: Option<u8>,
    #[serde(default)]
    pub play_count: u32,
    /// Unix seconds.
    pub added: u64,
    pub last_played: Option<u64>,
    /// File size and modification time when the tags were last read.
    pub file_size: u64,
    pub modified: u64,
    /// Hot cues, memory cues and loops.
    #[serde(default)]
    pub cues: Vec<Cue>,
    /// Beat grid: where the tempo starts or changes.
    #[serde(default)]
    pub tempo: Vec<TempoMarker>,
    /// Text fields changed in deckhand; rescans and imports leave them alone.
    /// The audio file is never changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edited: Vec<EditedField>,
    /// rekordbox's analysis of the track (`.DAT`, with `.EXT` and `.2EX`
    /// beside it), kept from a USB import: waveforms, grid and cues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis: Option<PathBuf>,
    /// Made from another track's stems (an acapella, an instrumental, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived: Option<Derived>,
}

/// Where a track made from stems came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Derived {
    /// The original track's id.
    pub original: u32,
    /// The stems it keeps.
    pub stems: Vec<String>,
    /// The separation model's name.
    pub model: String,
}

/// A track field that can be edited in deckhand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditedField {
    Title,
    Artist,
    Album,
    Genre,
    Label,
    Comment,
}

/// New text for a track's fields; `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackEdit {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub label: Option<String>,
    pub comment: Option<String>,
}

/// A beat-grid anchor: from `start` on, beats follow at `bpm`, and the beat
/// at `start` is `beat` (1–4) of its bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TempoMarker {
    /// Seconds.
    pub start: f64,
    pub bpm: f64,
    pub beat: u8,
}

impl Track {
    /// Every beat of the grid until `duration`: (seconds, position in bar 1–4).
    pub fn beats(&self, duration: f64) -> Vec<(f64, u16)> {
        let mut beats = Vec::new();
        for (i, marker) in self.tempo.iter().enumerate() {
            if marker.bpm <= 0.0 {
                continue;
            }
            let end = self.tempo.get(i + 1).map_or(duration, |next| next.start);
            let step = 60.0 / marker.bpm;
            let mut n = 0u32;
            loop {
                let time = marker.start + f64::from(n) * step;
                if time >= end - 1e-6 {
                    break;
                }
                let position = (u32::from(marker.beat.clamp(1, 4)) - 1 + n) % 4 + 1;
                beats.push((time, position as u16));
                n += 1;
            }
        }
        beats
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CueKind {
    /// Hot cue A = 0 … H = 7.
    Hot(u8),
    Memory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cue {
    pub kind: CueKind,
    /// Seconds.
    pub start: f64,
    /// Loop end in seconds, for loops.
    pub end: Option<f64>,
    /// RGB.
    pub color: Option<[u8; 3]>,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    Folder,
    Playlist,
}

/// A playlist or a folder of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: u32,
    pub name: String,
    pub kind: NodeKind,
    /// `None` at the top level.
    pub parent: Option<u32>,
    /// Playlist entries in order; a track may appear more than once.
    #[serde(default)]
    pub tracks: Vec<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Collection {
    pub tracks: Vec<Track>,
    /// Playlists and folders; siblings keep the order of this list.
    pub nodes: Vec<Node>,
    /// Folders that were added; rescanned to pick up new files.
    pub folders: Vec<PathBuf>,
    next_track: u32,
    next_node: u32,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    dirty: bool,
}

/// Cues, grid, BPM and key: a made track has the original's timing, as the
/// files start on the same sample.
fn copy_timing(source: &Track, track: &mut Track) {
    track.cues = source.cues.clone();
    track.tempo = source.tempo.clone();
    track.bpm = source.bpm;
    track.key = source.key.clone();
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Collection {
    /// Opens the collection stored at `path`, or an empty one if there is
    /// no file yet.
    pub fn open(path: &Path) -> Result<Self> {
        let mut collection: Collection = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Collection::default(),
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        collection.path = path.to_path_buf();
        collection.next_track = collection
            .next_track
            .max(collection.tracks.iter().map(|t| t.id + 1).max().unwrap_or(1));
        collection.next_node =
            collection.next_node.max(collection.nodes.iter().map(|n| n.id + 1).max().unwrap_or(1));
        Ok(collection)
    }

    /// Writes the collection if it changed since it was opened or saved.
    pub fn save(&mut self) -> Result<()> {
        if !self.dirty || self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("saving {}", self.path.display()))?;
        self.dirty = false;
        Ok(())
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn track(&self, id: u32) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    /// Changes a track; marks the collection for saving.
    pub fn update_track(&mut self, id: u32, change: impl FnOnce(&mut Track)) {
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == id) {
            change(track);
            self.dirty = true;
        }
    }

    /// Changes a track's text fields in the collection only (not in the
    /// file) and remembers them as edited.
    pub fn edit_track(&mut self, id: u32, edit: TrackEdit) {
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == id) else { return };
        let fields = [
            (EditedField::Title, edit.title, &mut track.title),
            (EditedField::Artist, edit.artist, &mut track.artist),
            (EditedField::Album, edit.album, &mut track.album),
            (EditedField::Genre, edit.genre, &mut track.genre),
            (EditedField::Label, edit.label, &mut track.label),
            (EditedField::Comment, edit.comment, &mut track.comment),
        ];
        for (field, new, value) in fields {
            if let Some(new) = new.filter(|n| n != value) {
                *value = new;
                if !track.edited.contains(&field) {
                    track.edited.push(field);
                }
            }
        }
        self.dirty = true;
    }

    /// Forgets the edits of a track; the next scan of its file brings the
    /// file's tags back.
    pub fn revert_edits(&mut self, id: u32) {
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == id) {
            track.edited.clear();
            track.modified = 0;
            self.dirty = true;
        }
    }

    /// (size, modified) of every known file, so a scan can skip unchanged ones.
    pub fn known_files(&self) -> HashMap<PathBuf, (u64, u64)> {
        self.tracks.iter().map(|t| (t.path.clone(), (t.file_size, t.modified))).collect()
    }

    /// Adds a folder to rescan later.
    pub fn add_folder(&mut self, folder: &Path) {
        if !self.folders.iter().any(|f| f == folder) {
            self.folders.push(folder.to_path_buf());
            self.dirty = true;
        }
    }

    /// Merges scanned files: new files become tracks, known ones get their
    /// tags refreshed (keeping rating, cues and play counts). Returns
    /// (added, updated).
    pub fn merge(&mut self, files: Vec<ScannedFile>) -> (usize, usize) {
        let by_path: HashMap<PathBuf, usize> =
            self.tracks.iter().enumerate().map(|(i, t)| (t.path.clone(), i)).collect();
        let (mut added, mut updated) = (0, 0);
        for file in files {
            match by_path.get(&file.path) {
                Some(&i) => {
                    file.apply_to(&mut self.tracks[i]);
                    updated += 1;
                }
                None => {
                    let id = self.next_track;
                    self.next_track += 1;
                    self.tracks.push(file.into_track(id));
                    added += 1;
                }
            }
        }
        if added + updated > 0 {
            self.dirty = true;
        }
        (added, updated)
    }

    /// Adds the file made from `original`'s stems as a track that carries
    /// the original's cues, grid, key and tags, titled with `suffix`, right
    /// after the original in each of its playlists. A track already at the
    /// file's path is updated instead. Returns the track's id.
    pub fn add_derived(
        &mut self,
        original: u32,
        file: ScannedFile,
        suffix: &str,
        derived: Derived,
    ) -> Option<u32> {
        let source = self.track(original)?.clone();
        let existing = self.tracks.iter().position(|t| t.path == file.path);
        let id = match existing {
            Some(i) => self.tracks[i].id,
            None => {
                let id = self.next_track;
                self.next_track += 1;
                id
            }
        };
        let mut track = file.into_track(id);
        track.title = format!("{} ({suffix})", source.title);
        track.artist = source.artist.clone();
        track.album = source.album.clone();
        track.genre = source.genre.clone();
        track.label = source.label.clone();
        track.year = source.year;
        track.rating = source.rating;
        track.color = source.color;
        track.comment = if source.comment.is_empty() {
            format!("{suffix} of {}, {}", source.title, derived.model)
        } else {
            source.comment.clone()
        };
        track.derived = Some(derived);
        copy_timing(&source, &mut track);
        match existing {
            Some(i) => {
                // Keep what the made track gathered itself.
                let old = &self.tracks[i];
                (track.added, track.play_count, track.last_played) =
                    (old.added, old.play_count, old.last_played);
                self.tracks[i] = track;
            }
            None => {
                self.tracks.push(track);
                for node in &mut self.nodes {
                    if let Some(at) = node.tracks.iter().position(|&t| t == original) {
                        node.tracks.insert(at + 1, id);
                    }
                }
            }
        }
        self.dirty = true;
        Some(id)
    }

    /// Copies the cues, grid and key of a made track's original to it again.
    /// Returns whether there was an original to copy from.
    pub fn update_from_original(&mut self, id: u32) -> bool {
        let Some(original) = self.track(id).and_then(|t| t.derived.as_ref()).map(|d| d.original)
        else {
            return false;
        };
        let Some(source) = self.track(original).cloned() else { return false };
        self.update_track(id, |t| copy_timing(&source, t));
        true
    }

    /// Tracks whose file is gone.
    pub fn missing(&self) -> Vec<u32> {
        self.tracks.iter().filter(|t| !t.path.is_file()).map(|t| t.id).collect()
    }

    /// Removes tracks from the collection and from every playlist.
    pub fn remove_tracks(&mut self, ids: &[u32]) {
        self.tracks.retain(|t| !ids.contains(&t.id));
        for node in &mut self.nodes {
            node.tracks.retain(|t| !ids.contains(t));
        }
        self.dirty = true;
    }

    /// Imports a rekordbox XML export. Tracks are matched by file path:
    /// known ones get rekordbox's tags, grid, cues, rating and colour; new
    /// ones are added. rekordbox's playlists replace the ones of the last
    /// import, in a top-level folder called `rekordbox`.
    pub fn import_rekordbox(&mut self, library: XmlLibrary) -> ImportSummary {
        self.import_into(library, REKORDBOX_FOLDER, true)
    }

    /// Imports tracks and playlists like [`Collection::import_rekordbox`],
    /// with the playlists in the top-level folder `folder`. With `replace`,
    /// that folder is rebuilt; without, its playlists of the same names are
    /// replaced and the others kept.
    pub fn import_into(
        &mut self,
        library: XmlLibrary,
        folder: &str,
        replace: bool,
    ) -> ImportSummary {
        let mut summary = ImportSummary::default();
        let mut by_path: HashMap<PathBuf, u32> =
            self.tracks.iter().map(|t| (t.path.clone(), t.id)).collect();
        let mut by_key: HashMap<String, u32> = HashMap::new();
        let mut by_location: HashMap<String, u32> = HashMap::new();
        for xml in library.tracks {
            if !xml.file.path.is_file() {
                summary.missing += 1;
            }
            let id = match by_path.get(&xml.file.path) {
                Some(&id) => {
                    summary.updated += 1;
                    id
                }
                None => {
                    let id = self.next_track;
                    self.next_track += 1;
                    self.tracks.push(xml.file.clone().into_track(id));
                    by_path.insert(xml.file.path.clone(), id);
                    summary.added += 1;
                    id
                }
            };
            if let Some(track) = self.tracks.iter_mut().find(|t| t.id == id) {
                let (size, modified) = (track.file_size, track.modified);
                xml.file.apply_to(track);
                // Keep the stamp of the file as last read, so rescans still
                // notice changes; rekordbox does not export one.
                if modified != 0 {
                    (track.file_size, track.modified) = (size, modified);
                }
                track.rating = xml.rating;
                track.color = xml.color.or(track.color);
                track.play_count = track.play_count.max(xml.play_count);
                if let Some(added) = xml.added {
                    track.added = added;
                }
                if !xml.tempo.is_empty() {
                    track.tempo = xml.tempo;
                }
                if !xml.cues.is_empty() {
                    track.cues = xml.cues;
                }
                if xml.analysis.is_some() {
                    track.analysis = xml.analysis;
                }
            }
            by_key.insert(xml.id, id);
            by_location.insert(xml.location, id);
        }

        let existing: Vec<u32> = self
            .nodes
            .iter()
            .filter(|n| n.parent.is_none() && n.kind == NodeKind::Folder && n.name == folder)
            .map(|n| n.id)
            .collect();
        let root = match existing.first() {
            Some(&root) if !replace => {
                // Playlists imported again replace their earlier copies.
                let names: Vec<&str> = library
                    .playlists
                    .iter()
                    .map(|n| match n {
                        XmlNode::Folder { name, .. } | XmlNode::Playlist { name, .. } => {
                            name.as_str()
                        }
                    })
                    .collect();
                let old: Vec<u32> = self
                    .nodes
                    .iter()
                    .filter(|n| n.parent == Some(root) && names.contains(&n.name.as_str()))
                    .map(|n| n.id)
                    .collect();
                for id in old {
                    self.delete(id);
                }
                root
            }
            _ => {
                // Replace the previous import's playlists.
                for id in existing {
                    self.delete(id);
                }
                self.create(folder, NodeKind::Folder, None)
            }
        };
        fn add(
            c: &mut Collection,
            nodes: &[XmlNode],
            parent: u32,
            by_key: &HashMap<String, u32>,
            by_location: &HashMap<String, u32>,
            count: &mut usize,
        ) {
            for node in nodes {
                match node {
                    XmlNode::Folder { name, children } => {
                        let folder = c.create(name, NodeKind::Folder, Some(parent));
                        add(c, children, folder, by_key, by_location, count);
                    }
                    XmlNode::Playlist { name, keys, by_location: located } => {
                        let list = c.create(name, NodeKind::Playlist, Some(parent));
                        let map = if *located { by_location } else { by_key };
                        let ids: Vec<u32> =
                            keys.iter().filter_map(|k| map.get(k).copied()).collect();
                        c.add_to_playlist(list, &ids);
                        *count += 1;
                    }
                }
            }
        }
        add(self, &library.playlists, root, &by_key, &by_location, &mut summary.playlists);
        self.dirty = true;
        summary
    }

    // Playlists

    pub fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    fn node_mut(&mut self, id: u32) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    /// Creates a playlist or folder at the end of `parent` (which must be a
    /// folder, or `None` for the top level).
    pub fn create(&mut self, name: &str, kind: NodeKind, parent: Option<u32>) -> u32 {
        let parent = parent.filter(|p| self.node(*p).is_some_and(|n| n.kind == NodeKind::Folder));
        let id = self.next_node;
        self.next_node += 1;
        self.nodes.push(Node { id, name: name.to_owned(), kind, parent, tracks: Vec::new() });
        self.dirty = true;
        id
    }

    pub fn rename(&mut self, id: u32, name: &str) {
        if let Some(node) = self.node_mut(id) {
            node.name = name.to_owned();
            self.dirty = true;
        }
    }

    /// Deletes a playlist, or a folder with everything in it.
    pub fn delete(&mut self, id: u32) {
        let mut doomed = vec![id];
        let mut i = 0;
        while i < doomed.len() {
            let parent = doomed[i];
            doomed.extend(self.nodes.iter().filter(|n| n.parent == Some(parent)).map(|n| n.id));
            i += 1;
        }
        self.nodes.retain(|n| !doomed.contains(&n.id));
        self.dirty = true;
    }

    /// Appends tracks to a playlist, skipping ones already in it.
    pub fn add_to_playlist(&mut self, id: u32, tracks: &[u32]) -> usize {
        let Some(node) = self.node_mut(id).filter(|n| n.kind == NodeKind::Playlist) else {
            return 0;
        };
        // Duplicates are kept: a track can be in a set twice on purpose.
        node.tracks.extend_from_slice(tracks);
        self.dirty |= !tracks.is_empty();
        tracks.len()
    }

    /// Removes one entry of a playlist, by its position.
    pub fn remove_playlist_entry(&mut self, id: u32, entry: usize) {
        if let Some(node) = self.node_mut(id)
            && entry < node.tracks.len()
        {
            node.tracks.remove(entry);
            self.dirty = true;
        }
    }

    /// Moves a playlist entry to another position.
    pub fn move_in_playlist(&mut self, id: u32, from: usize, to: usize) {
        if let Some(node) = self.node_mut(id)
            && from < node.tracks.len()
        {
            let track = node.tracks.remove(from);
            node.tracks.insert(to.min(node.tracks.len()), track);
            self.dirty = true;
        }
    }

    /// Moves a node into another folder (or the top level), not into itself
    /// or its own children.
    pub fn move_node(&mut self, id: u32, parent: Option<u32>) {
        let mut ancestor = parent;
        while let Some(a) = ancestor {
            if a == id {
                return;
            }
            ancestor = self.node(a).and_then(|n| n.parent);
        }
        if parent.is_some_and(|p| self.node(p).is_none_or(|n| n.kind != NodeKind::Folder)) {
            return;
        }
        if let Some(node) = self.node_mut(id) {
            node.parent = parent;
            self.dirty = true;
        }
    }

    /// The tree in display order: (depth, node).
    pub fn tree(&self) -> Vec<(usize, &Node)> {
        fn walk<'a>(
            c: &'a Collection,
            parent: Option<u32>,
            depth: usize,
            out: &mut Vec<(usize, &'a Node)>,
        ) {
            for node in c.nodes.iter().filter(|n| n.parent == parent) {
                out.push((depth, node));
                if node.kind == NodeKind::Folder {
                    walk(c, Some(node.id), depth + 1, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(self, None, 0, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(path: &str, title: &str) -> ScannedFile {
        ScannedFile { path: PathBuf::from(path), title: title.to_owned(), ..ScannedFile::default() }
    }

    #[test]
    fn merging_keeps_user_data_and_ids() {
        let mut c = Collection::default();
        assert_eq!(c.merge(vec![scanned("/a.mp3", "A"), scanned("/b.mp3", "B")]), (2, 0));
        let a = c.tracks[0].id;
        c.update_track(a, |t| t.rating = 4);
        assert_eq!(c.merge(vec![scanned("/a.mp3", "A (remaster)")]), (0, 1));
        let track = c.track(a).unwrap();
        assert_eq!((track.title.as_str(), track.rating), ("A (remaster)", 4));
        assert_ne!(c.tracks[1].id, a);
    }

    #[test]
    fn made_tracks_carry_the_originals_timing_and_follow_it_in_playlists() {
        let mut c = Collection::default();
        c.merge(vec![scanned("/song.mp3", "Song"), scanned("/other.mp3", "Other")]);
        let (song, other) = (c.tracks[0].id, c.tracks[1].id);
        let cue = Cue {
            kind: CueKind::Hot(2),
            start: 12.5,
            end: Some(16.5),
            color: Some([1, 2, 3]),
            name: "Drop".into(),
        };
        c.update_track(song, |t| {
            t.cues = vec![cue.clone()];
            t.tempo = vec![TempoMarker { start: 0.1, bpm: 124.0, beat: 1 }];
            t.bpm = Some(124.0);
            t.key = Some("8A".into());
            t.artist = "Artist".into();
            t.rating = 4;
        });
        let playlist = c.create("Set", NodeKind::Playlist, None);
        c.add_to_playlist(playlist, &[song, other]);
        let derived =
            || Derived { original: song, stems: vec!["vocals".into()], model: "htdemucs".into() };
        let made = c
            .add_derived(song, scanned("/Song (Acapella).wav", ""), "Acapella", derived())
            .unwrap();
        let track = c.track(made).unwrap();
        assert_eq!(track.title, "Song (Acapella)");
        assert_eq!((track.artist.as_str(), track.rating), ("Artist", 4));
        assert_eq!(track.cues, [cue]);
        assert_eq!((track.bpm, track.key.as_deref()), (Some(124.0), Some("8A")));
        assert_eq!(track.tempo.len(), 1);
        assert_eq!(c.node(playlist).unwrap().tracks, [song, made, other]);
        // Made again: the same track, no duplicate.
        c.update_track(made, |t| t.play_count = 3);
        let again = c
            .add_derived(song, scanned("/Song (Acapella).wav", ""), "Acapella", derived())
            .unwrap();
        assert_eq!(again, made);
        assert_eq!(c.tracks.len(), 3);
        assert_eq!(c.track(made).unwrap().play_count, 3);
        assert_eq!(c.node(playlist).unwrap().tracks, [song, made, other]);
        // The original's cues move; the made track follows when asked.
        c.update_track(song, |t| t.cues[0].start = 20.0);
        assert!(c.update_from_original(made));
        assert_eq!(c.track(made).unwrap().cues[0].start, 20.0);
        assert!(!c.update_from_original(song));
    }

    #[test]
    fn edits_survive_rescans() {
        let mut c = Collection::default();
        c.merge(vec![scanned("/a.mp3", "From tags")]);
        let id = c.tracks[0].id;
        c.edit_track(id, TrackEdit { title: Some("My name".into()), ..TrackEdit::default() });
        c.merge(vec![ScannedFile {
            artist: "New artist".into(),
            ..scanned("/a.mp3", "From tags")
        }]);
        let track = c.track(id).unwrap();
        assert_eq!((track.title.as_str(), track.artist.as_str()), ("My name", "New artist"));
        c.revert_edits(id);
        c.merge(vec![scanned("/a.mp3", "From tags")]);
        assert_eq!(c.track(id).unwrap().title, "From tags");
    }

    #[test]
    fn playlists_nest_and_delete_with_their_contents() {
        let mut c = Collection::default();
        c.merge(vec![scanned("/a.mp3", "A"), scanned("/b.mp3", "B")]);
        let (a, b) = (c.tracks[0].id, c.tracks[1].id);
        let folder = c.create("Sets", NodeKind::Folder, None);
        let list = c.create("Friday", NodeKind::Playlist, Some(folder));
        let top = c.create("Warmup", NodeKind::Playlist, None);
        assert_eq!(c.add_to_playlist(list, &[a, b, a]), 3, "duplicates are kept");
        c.move_in_playlist(list, 1, 0);
        assert_eq!(c.node(list).unwrap().tracks, [b, a, a]);
        c.remove_playlist_entry(list, 2);
        assert_eq!(c.node(list).unwrap().tracks, [b, a], "one entry goes, not every copy");
        let names: Vec<_> = c.tree().iter().map(|(d, n)| (*d, n.name.as_str())).collect();
        assert_eq!(names, [(0, "Sets"), (1, "Friday"), (0, "Warmup")]);
        // A folder cannot go inside itself.
        c.move_node(folder, Some(folder));
        assert_eq!(c.node(folder).unwrap().parent, None);
        c.remove_tracks(&[a]);
        assert_eq!(c.node(list).unwrap().tracks, [b]);
        c.delete(folder);
        assert!(c.node(list).is_none() && c.node(top).is_some());
    }

    #[test]
    fn saves_and_reopens() {
        let dir = std::env::temp_dir().join(format!("deckhand-collection-{}", std::process::id()));
        let path = dir.join("collection.json");
        let mut c = Collection::open(&path).unwrap();
        c.merge(vec![scanned("/a.mp3", "A")]);
        let list = c.create("Set", NodeKind::Playlist, None);
        c.add_to_playlist(list, &[c.tracks[0].id]);
        c.save().unwrap();
        let mut reopened = Collection::open(&path).unwrap();
        assert_eq!(reopened.tracks, c.tracks);
        assert_eq!(reopened.nodes, c.nodes);
        // New ids continue after the stored ones.
        reopened.merge(vec![scanned("/b.mp3", "B")]);
        assert!(reopened.tracks[1].id > reopened.tracks[0].id);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn grid_expands_to_beats_with_bar_positions() {
        let mut c = Collection::default();
        c.merge(vec![scanned("/a.mp3", "A")]);
        let id = c.tracks[0].id;
        c.update_track(id, |t| {
            t.tempo = vec![
                TempoMarker { start: 0.5, bpm: 120.0, beat: 3 },
                TempoMarker { start: 2.0, bpm: 60.0, beat: 2 },
            ]
        });
        let beats = c.track(id).unwrap().beats(4.0);
        assert_eq!(beats, [(0.5, 3), (1.0, 4), (1.5, 1), (2.0, 2), (3.0, 3)]);
    }

    #[test]
    fn rekordbox_import_merges_tracks_and_replaces_its_playlists() {
        let xml = br#"<DJ_PLAYLISTS><COLLECTION>
            <TRACK TrackID="7" Name="A" Location="file://localhost/a.mp3" Rating="255">
              <TEMPO Inizio="0.1" Bpm="128" Metro="4/4" Battito="1"/>
              <POSITION_MARK Type="0" Start="1.5" Num="0"/>
            </TRACK>
            <TRACK TrackID="8" Name="New" Location="file://localhost/new.mp3"/>
          </COLLECTION><PLAYLISTS><NODE Type="0" Name="ROOT">
            <NODE Name="Set" Type="1" KeyType="0"><TRACK Key="8"/><TRACK Key="7"/></NODE>
          </NODE></PLAYLISTS></DJ_PLAYLISTS>"#;
        let mut c = Collection::default();
        c.merge(vec![scanned("/a.mp3", "A (old tags)")]);
        let a = c.tracks[0].id;
        let summary = c.import_rekordbox(XmlLibrary::parse(xml).unwrap());
        assert_eq!((summary.added, summary.updated, summary.playlists), (1, 1, 1));
        let track = c.track(a).unwrap();
        assert_eq!((track.title.as_str(), track.rating, track.cues.len()), ("A", 5, 1));
        assert_eq!(track.tempo[0].bpm, 128.0);
        let set = c.tree().into_iter().find(|(_, n)| n.name == "Set").unwrap().1.clone();
        assert_eq!(set.tracks.len(), 2);
        assert_eq!(set.tracks[1], a);
        // Importing again replaces the playlists instead of adding copies.
        c.import_rekordbox(XmlLibrary::parse(xml).unwrap());
        assert_eq!(c.tree().iter().filter(|(_, n)| n.name == "Set").count(), 1);
        assert_eq!(c.tracks.len(), 2);
    }
}
