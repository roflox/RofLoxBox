//! deckhand's own music collection: tracks found in folders on this
//! computer, their tags, and playlists organised in folders.
//!
//! The collection is a JSON file. It is rewritten whole (through a temporary
//! file, so a crash never leaves half a file) whenever something changes.

mod collection;
mod rekordbox_xml;
mod scan;

pub use collection::{Collection, Cue, CueKind, ImportSummary, Node, NodeKind, TempoMarker, Track};
pub use rekordbox_xml::{XmlLibrary, XmlNode, XmlTrack, path_from_location};
pub use scan::{AUDIO_EXTENSIONS, Scan, ScannedFile, read_file};

use std::path::PathBuf;

/// Where deckhand keeps its settings and collection:
/// `~/Library/Application Support/deckhand` (`%APPDATA%\deckhand` on
/// Windows), or `DECKHAND_DATA_DIR` when set (tests use it to leave the
/// user's files alone).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DECKHAND_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        let appdata = std::env::var_os("APPDATA").map_or_else(|| PathBuf::from("."), PathBuf::from);
        return appdata.join("deckhand");
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Library/Application Support/deckhand")
}

/// The collection file in [`data_dir`].
pub fn collection_path() -> PathBuf {
    data_dir().join("collection.json")
}
