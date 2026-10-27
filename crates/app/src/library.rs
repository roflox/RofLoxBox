//! The library of a USB/SD export: playlists and tracks from `export.pdb`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use deckhand_formats::pdb::{Library, LibraryTrack, PlaylistKind, PlaylistNode};

/// A playlist entry in the sidebar, flattened with its nesting depth.
#[derive(Debug, Clone)]
pub struct PlaylistRow {
    pub name: String,
    pub depth: usize,
    /// `None` for folders.
    pub track_ids: Option<Vec<u32>>,
}

/// An opened export.
pub struct UsbLibrary {
    pub root: PathBuf,
    pub library: Library,
    pub playlists: Vec<PlaylistRow>,
}

impl UsbLibrary {
    /// Opens the export whose `PIONEER` directory is directly below `root`.
    pub fn open(root: &Path) -> Result<Self> {
        let pdb = root.join("PIONEER/rekordbox/export.pdb");
        let library = Library::open(&pdb).with_context(|| format!("reading {}", pdb.display()))?;
        let mut playlists = Vec::new();
        flatten(&library.playlists, 0, &mut playlists);
        Ok(Self { root: root.to_path_buf(), library, playlists })
    }

    /// The name shown for the medium.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map_or_else(|| self.root.display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    /// Tracks of a playlist (or the whole collection for `None`), filtered
    /// by a case-insensitive search over title, artist, album and genre,
    /// each with its position in the list before searching.
    pub fn numbered_tracks(
        &self,
        playlist: Option<usize>,
        search: &str,
    ) -> Vec<(usize, &LibraryTrack)> {
        let needle = search.trim().to_lowercase();
        let matches = |t: &&LibraryTrack| {
            needle.is_empty()
                || [Some(&t.track.title), t.artist.as_ref(), t.album.as_ref(), t.genre.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|field| field.to_lowercase().contains(&needle))
        };
        match playlist.and_then(|i| self.playlists.get(i)).and_then(|p| p.track_ids.as_ref()) {
            Some(ids) => ids
                .iter()
                .enumerate()
                .filter_map(|(i, id)| self.library.track(*id).map(|t| (i, t)))
                .filter(|(_, t)| matches(t))
                .collect(),
            None => self.library.tracks.iter().enumerate().filter(|(_, t)| matches(t)).collect(),
        }
    }

    /// Absolute path of a file referenced from the export (`/Contents/…`).
    pub fn resolve(&self, export_path: &str) -> PathBuf {
        self.root.join(export_path.trim_start_matches('/'))
    }
}

fn flatten(nodes: &[PlaylistNode], depth: usize, out: &mut Vec<PlaylistRow>) {
    for node in nodes {
        match &node.kind {
            PlaylistKind::Folder(children) => {
                out.push(PlaylistRow { name: node.name.clone(), depth, track_ids: None });
                flatten(children, depth + 1, out);
            }
            PlaylistKind::Playlist(ids) => {
                out.push(PlaylistRow {
                    name: node.name.clone(),
                    depth,
                    track_ids: Some(ids.clone()),
                });
            }
        }
    }
}

/// Mounted volumes (drive letters on Windows) that contain a rekordbox export.
pub fn find_exports() -> Vec<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        (b'A'..=b'Z').map(|d| PathBuf::from(format!("{}:\\", d as char))).collect()
    } else {
        let Ok(entries) = std::fs::read_dir("/Volumes") else { return Vec::new() };
        entries.filter_map(|e| e.ok().map(|e| e.path())).collect()
    };
    let mut found: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|p| p.join("PIONEER/rekordbox/export.pdb").is_file())
        .collect();
    found.sort();
    found
}

/// `m:ss` for a duration in seconds.
pub fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}
