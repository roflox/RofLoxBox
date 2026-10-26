//! A resolved view of an `export.pdb`: tracks with names instead of IDs and
//! the playlist tree with ordered tracks.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::{
    Album, Artist, Artwork, Color, Database, FromRow, Genre, HistoryEntry, HistoryPlaylist, Key,
    Label, PlaylistEntry, PlaylistTreeNode, Track,
};
use crate::error::Result;

#[derive(Debug, Clone, PartialEq)]
pub struct Library {
    pub tracks: Vec<LibraryTrack>,
    /// Root-level playlists and folders, in display order.
    pub playlists: Vec<PlaylistNode>,
    pub history: Vec<HistoryList>,
}

/// A track with its references resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryTrack {
    pub track: Track,
    pub artist: Option<String>,
    pub composer: Option<String>,
    pub original_artist: Option<String>,
    pub remixer: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub label: Option<String>,
    pub key: Option<String>,
    pub color: Option<String>,
    pub artwork_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistNode {
    pub id: u32,
    pub name: String,
    pub kind: PlaylistKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistKind {
    Folder(Vec<PlaylistNode>),
    /// Track IDs in playlist order.
    Playlist(Vec<u32>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryList {
    pub id: u32,
    pub name: String,
    /// Track IDs in play order.
    pub track_ids: Vec<u32>,
}

impl Library {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_database(&Database::open(path)?)
    }

    pub fn from_database(db: &Database) -> Result<Self> {
        let artists = names(db, |a: Artist| (a.id, a.name))?;
        let albums = names(db, |a: Album| (a.id, a.name))?;
        let genres = names(db, |g: Genre| (g.id, g.name))?;
        let labels = names(db, |l: Label| (l.id, l.name))?;
        let keys = names(db, |k: Key| (k.id, k.name))?;
        let colors = names(db, |c: Color| (u32::from(c.id), c.name))?;
        let artwork = names(db, |a: Artwork| (a.id, a.path))?;
        let lookup =
            |map: &HashMap<u32, String>, id: Option<u32>| id.and_then(|id| map.get(&id).cloned());

        let tracks = db
            .read_all::<Track>()?
            .into_iter()
            .map(|track| LibraryTrack {
                artist: lookup(&artists, track.artist_id),
                composer: lookup(&artists, track.composer_id),
                original_artist: lookup(&artists, track.original_artist_id),
                remixer: lookup(&artists, track.remixer_id),
                album: lookup(&albums, track.album_id),
                genre: lookup(&genres, track.genre_id),
                label: lookup(&labels, track.label_id),
                key: lookup(&keys, track.key_id),
                color: lookup(&colors, track.color_id.map(u32::from)),
                artwork_path: lookup(&artwork, track.artwork_id),
                track,
            })
            .collect();

        let mut entries: HashMap<u32, Vec<PlaylistEntry>> = HashMap::new();
        for entry in db.read_all::<PlaylistEntry>()? {
            entries.entry(entry.playlist_id).or_default().push(entry);
        }
        let mut children: HashMap<Option<u32>, Vec<PlaylistTreeNode>> = HashMap::new();
        for node in db.read_all::<PlaylistTreeNode>()? {
            children.entry(node.parent_id).or_default().push(node);
        }
        let playlists = build_tree(None, &mut children, &mut entries, &mut HashSet::new());

        let mut history_entries: HashMap<u32, Vec<HistoryEntry>> = HashMap::new();
        for entry in db.read_all::<HistoryEntry>()? {
            history_entries.entry(entry.playlist_id).or_default().push(entry);
        }
        let history = db
            .read_all::<HistoryPlaylist>()?
            .into_iter()
            .map(|list| {
                let mut list_entries = history_entries.remove(&list.id).unwrap_or_default();
                list_entries.sort_by_key(|entry| entry.entry_index);
                HistoryList {
                    id: list.id,
                    name: list.name,
                    track_ids: list_entries.iter().map(|entry| entry.track_id).collect(),
                }
            })
            .collect();

        Ok(Self { tracks, playlists, history })
    }

    pub fn track(&self, id: u32) -> Option<&LibraryTrack> {
        self.tracks.iter().find(|t| t.track.id == id)
    }
}

fn names<T: FromRow>(
    db: &Database,
    pair: impl Fn(T) -> (u32, String),
) -> Result<HashMap<u32, String>> {
    Ok(db.read_all::<T>()?.into_iter().map(pair).collect())
}

/// Builds the children of `parent`. `visited` stops malformed files whose
/// parent links form a cycle from recursing forever.
fn build_tree(
    parent: Option<u32>,
    children: &mut HashMap<Option<u32>, Vec<PlaylistTreeNode>>,
    entries: &mut HashMap<u32, Vec<PlaylistEntry>>,
    visited: &mut HashSet<u32>,
) -> Vec<PlaylistNode> {
    let mut nodes = children.remove(&parent).unwrap_or_default();
    nodes.sort_by_key(|node| node.sort_order);
    let mut built = Vec::with_capacity(nodes.len());
    for node in nodes {
        if !visited.insert(node.id) {
            continue;
        }
        let kind = if node.is_folder {
            PlaylistKind::Folder(build_tree(Some(node.id), children, entries, visited))
        } else {
            let mut list = entries.remove(&node.id).unwrap_or_default();
            list.sort_by_key(|entry| entry.entry_index);
            PlaylistKind::Playlist(list.iter().map(|entry| entry.track_id).collect())
        };
        built.push(PlaylistNode { id: node.id, name: node.name, kind });
    }
    built
}
