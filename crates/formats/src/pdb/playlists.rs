//! Editing the playlists of an existing export: creating, renaming and
//! deleting playlists and folders, and adding or removing their tracks. The
//! database is rewritten with every other row kept.
//!
//! Numbering follows rekordbox's exports: playlist ids count up, the sort
//! order counts from 0 among the playlists of one folder, and the entries of a
//! playlist are numbered from 1.

use std::path::{Path, PathBuf};

use super::{
    Database, DatabaseWriter, HistoryEntry, PlaylistEntry, PlaylistTreeNode, TagTrack, Track,
};
use crate::error::Result;

/// The playlist tree and playlist entry tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistTables {
    pub nodes: Vec<PlaylistTreeNode>,
    pub entries: Vec<PlaylistEntry>,
}

impl PlaylistTables {
    pub fn read(db: &Database) -> Result<Self> {
        Ok(Self { nodes: db.read_all()?, entries: db.read_all()? })
    }

    fn children(&self, parent: Option<u32>) -> impl Iterator<Item = &PlaylistTreeNode> {
        self.nodes.iter().filter(move |n| n.parent_id == parent)
    }

    /// Adds a playlist or folder at the end of `parent` (`None`: the top
    /// level); returns its id.
    pub fn create(&mut self, name: &str, folder: bool, parent: Option<u32>) -> u32 {
        let id = self.nodes.iter().map(|n| n.id).max().unwrap_or(0) + 1;
        let sort_order = self.children(parent).map(|n| n.sort_order + 1).max().unwrap_or(0);
        self.nodes.push(PlaylistTreeNode {
            id,
            parent_id: parent,
            sort_order,
            is_folder: folder,
            name: name.to_owned(),
        });
        id
    }

    pub fn rename(&mut self, id: u32, name: &str) {
        if let Some(node) = self.nodes.iter_mut().find(|n| n.id == id) {
            node.name = name.to_owned();
        }
    }

    /// Removes a playlist, or a folder with everything in it.
    pub fn delete(&mut self, id: u32) {
        let Some(parent) = self.nodes.iter().find(|n| n.id == id).map(|n| n.parent_id) else {
            return;
        };
        let mut gone = vec![id];
        let mut i = 0;
        while i < gone.len() {
            let below: Vec<u32> = self.children(Some(gone[i])).map(|n| n.id).collect();
            gone.extend(below);
            i += 1;
        }
        self.nodes.retain(|n| !gone.contains(&n.id));
        self.entries.retain(|e| !gone.contains(&e.playlist_id));
        // Close the gap in the sort order.
        let mut siblings: Vec<usize> =
            (0..self.nodes.len()).filter(|&i| self.nodes[i].parent_id == parent).collect();
        siblings.sort_by_key(|&i| self.nodes[i].sort_order);
        for (order, i) in siblings.into_iter().enumerate() {
            self.nodes[i].sort_order = order as u32;
        }
    }

    /// The tracks of a playlist in order.
    pub fn tracks(&self, playlist: u32) -> Vec<u32> {
        let mut entries: Vec<&PlaylistEntry> =
            self.entries.iter().filter(|e| e.playlist_id == playlist).collect();
        entries.sort_by_key(|e| e.entry_index);
        entries.iter().map(|e| e.track_id).collect()
    }

    /// Replaces a playlist's tracks (numbered 1, 2, … in this order).
    pub fn set_tracks(&mut self, playlist: u32, tracks: &[u32]) {
        self.entries.retain(|e| e.playlist_id != playlist);
        self.entries.extend(tracks.iter().enumerate().map(|(i, &track_id)| PlaylistEntry {
            entry_index: i as u32 + 1,
            track_id,
            playlist_id: playlist,
        }));
    }

    /// Appends tracks to a playlist.
    pub fn add_tracks(&mut self, playlist: u32, tracks: &[u32]) {
        let mut all = self.tracks(playlist);
        all.extend_from_slice(tracks);
        self.set_tracks(playlist, &all);
    }

    /// Removes the entry at `position` (from 0) of a playlist.
    pub fn remove_entry(&mut self, playlist: u32, position: usize) {
        let mut all = self.tracks(playlist);
        if position < all.len() {
            all.remove(position);
            self.set_tracks(playlist, &all);
        }
    }
}

/// The backup of an export's database made before its first edit.
pub fn backup_path(pdb: &Path) -> PathBuf {
    pdb.with_extension("pdb.rofloxbox-backup")
}

/// Edits the playlists of an `export.pdb` in place. The original is copied
/// to [`backup_path`] once (an existing backup is kept), and the new file is
/// written beside it and then moved over it, so a failed write leaves the
/// old database intact.
pub fn edit_playlists<R>(pdb: &Path, edit: impl FnOnce(&mut PlaylistTables) -> R) -> Result<R> {
    let db = Database::open(pdb)?;
    let mut tables = PlaylistTables::read(&db)?;
    let result = edit(&mut tables);
    let mut writer = DatabaseWriter::from_database(&db)?;
    writer.replace_rows(&tables.nodes);
    writer.replace_rows(&tables.entries);
    write_safely(pdb, &writer.to_bytes())?;
    Ok(result)
}

/// Replaces a database file: the original is copied to [`backup_path`] once,
/// and the new one is written beside it and then moved over it.
pub fn write_safely(pdb: &Path, bytes: &[u8]) -> Result<()> {
    let backup = backup_path(pdb);
    if pdb.exists() && !backup.exists() {
        std::fs::copy(pdb, &backup)?;
    }
    let temporary = pdb.with_extension("pdb.rofloxbox-new");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, pdb)?;
    Ok(())
}

/// Removes tracks from an export's database: their rows, playlist and
/// history entries, and their My Tags in the `exportExt.pdb` beside it.
/// Their files are left alone. Returns the removed rows.
pub fn remove_tracks(pdb: &Path, ids: &[u32]) -> Result<Vec<Track>> {
    let db = Database::open(pdb)?;
    let (removed, kept): (Vec<Track>, Vec<Track>) =
        db.read_all::<Track>()?.into_iter().partition(|t| ids.contains(&t.id));
    let mut tables = PlaylistTables::read(&db)?;
    for playlist in tables.nodes.iter().map(|n| n.id).collect::<Vec<_>>() {
        let tracks = tables.tracks(playlist);
        if tracks.iter().any(|t| ids.contains(t)) {
            let left: Vec<u32> = tracks.into_iter().filter(|t| !ids.contains(t)).collect();
            tables.set_tracks(playlist, &left);
        }
    }
    // History lists keep their order, renumbered without the gaps.
    let mut history: Vec<HistoryEntry> = db.read_all()?;
    history.retain(|e| !ids.contains(&e.track_id));
    history.sort_by_key(|e| (e.playlist_id, e.entry_index));
    let mut previous = None;
    let mut index = 0;
    for entry in &mut history {
        if previous != Some(entry.playlist_id) {
            (previous, index) = (Some(entry.playlist_id), 0);
        }
        index += 1;
        entry.entry_index = index;
    }
    let mut writer = DatabaseWriter::from_database(&db)?;
    writer.replace_rows(&kept);
    writer.replace_rows(&tables.entries);
    writer.replace_rows(&history);
    write_safely(pdb, &writer.to_bytes())?;

    let ext = pdb.with_file_name("exportExt.pdb");
    if ext.is_file() {
        let db = Database::open(&ext)?;
        let tags: Vec<TagTrack> = db.read_all()?;
        if tags.iter().any(|t| ids.contains(&t.track_id)) {
            let left: Vec<TagTrack> =
                tags.into_iter().filter(|t| !ids.contains(&t.track_id)).collect();
            let mut writer = DatabaseWriter::from_database(&db)?;
            writer.replace_rows(&left);
            write_safely(&ext, &writer.to_bytes())?;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, parent: Option<u32>, sort_order: u32, folder: bool) -> PlaylistTreeNode {
        PlaylistTreeNode {
            id,
            parent_id: parent,
            sort_order,
            is_folder: folder,
            name: format!("{id}"),
        }
    }

    #[test]
    fn editing_the_tree() {
        let mut t = PlaylistTables {
            nodes: vec![
                node(1, None, 0, false),
                node(2, None, 1, true),
                node(3, Some(2), 0, false),
            ],
            entries: Vec::new(),
        };
        t.set_tracks(1, &[10, 11]);
        t.set_tracks(3, &[12]);
        let new = t.create("New", false, None);
        assert_eq!((new, t.nodes.last().unwrap().sort_order), (4, 2));
        t.add_tracks(new, &[13, 13]);
        assert_eq!(t.tracks(new), [13, 13], "duplicates are kept");
        t.remove_entry(1, 0);
        assert_eq!(t.tracks(1), [11]);
        assert_eq!(t.entries.iter().find(|e| e.playlist_id == 1).unwrap().entry_index, 1);
        t.rename(1, "Renamed");
        assert_eq!(t.nodes[0].name, "Renamed");
        // Deleting the folder takes its playlist and entries along and closes
        // the gap in the order.
        t.delete(2);
        assert!(t.nodes.iter().all(|n| n.id != 2 && n.id != 3));
        assert!(t.entries.iter().all(|e| e.playlist_id != 3));
        let order: Vec<(u32, u32)> = t.nodes.iter().map(|n| (n.id, n.sort_order)).collect();
        assert_eq!(order, [(1, 0), (4, 1)]);
    }
}
