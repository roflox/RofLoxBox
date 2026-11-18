//! RofLoxBox's own music collection: tracks found in folders on this
//! computer, their tags, and playlists organised in folders.
//!
//! The collection is a JSON file. It is rewritten whole (through a temporary
//! file, so a crash never leaves half a file) whenever something changes.

mod collection;
mod rekordbox_xml;
mod scan;
pub mod serato;

pub use collection::{
    Collection, Cue, CueKind, Derived, EditedField, ImportSummary, Node, NodeKind, Place,
    TempoMarker, Track, TrackEdit,
};
pub use rekordbox_xml::{XmlLibrary, XmlNode, XmlTrack, path_from_location};
pub use scan::{
    AUDIO_EXTENSIONS, SKIP_MARKER, Scan, ScannedFile, embedded_artwork, file_stamp, read_file,
};

use std::path::{Path, PathBuf};

/// Where RofLoxBox keeps its settings and collection:
/// `~/Library/Application Support/RofLoxBox` (`%APPDATA%\RofLoxBox` on
/// Windows), or `ROFLOXBOX_DATA_DIR` when set (tests use it to leave the
/// user's files alone). A folder of the app's name from before it was
/// renamed is moved here the first time.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ROFLOXBOX_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let parent = if cfg!(windows) {
        std::env::var_os("APPDATA").map_or_else(|| PathBuf::from("."), PathBuf::from)
    } else {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
        home.join("Library/Application Support")
    };
    let dir = parent.join("RofLoxBox");
    static MOVED: std::sync::Once = std::sync::Once::new();
    MOVED.call_once(|| move_old_data_dir(&parent.join(OLD_DATA_DIR), &dir));
    dir
}

/// The settings folder's name before the app was renamed.
const OLD_DATA_DIR: &str = "deckhand";

/// Moves the settings folder from before the rename to `new` (unless there
/// is one there already). The old name is left as a link to the new one,
/// so what refers to files in it by path (rekordbox's library, stems made
/// into tracks) still finds them; the collection's own paths are changed.
fn move_old_data_dir(old: &Path, new: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(old) else { return };
    if !meta.is_dir() || new.exists() || std::fs::rename(old, new).is_err() {
        return;
    }
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(new, old);
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(old)
        .arg(new)
        .output();
    let collection = new.join("collection.json");
    let Ok(text) = std::fs::read_to_string(&collection) else { return };
    // As the paths are written in the JSON (backslashes escaped).
    let escaped = |p: &Path| {
        serde_json::to_string(&p.to_string_lossy()).map(|s| s.trim_matches('"').to_owned())
    };
    let (Ok(from), Ok(to)) = (escaped(old), escaped(new)) else { return };
    let separator = if cfg!(windows) { "\\\\" } else { "/" };
    let changed = text.replace(&format!("{from}{separator}"), &format!("{to}{separator}"));
    if changed != text {
        let temp = collection.with_extension("json.moving");
        if std::fs::write(&temp, changed).is_ok() {
            let _ = std::fs::rename(&temp, &collection);
        }
    }
}

/// The collection file in [`data_dir`].
pub fn collection_path() -> PathBuf {
    data_dir().join("collection.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_settings_folder_moves_with_its_paths() {
        let root = std::env::temp_dir().join(format!("rofloxbox-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (old, new) = (root.join(OLD_DATA_DIR), root.join("RofLoxBox"));
        std::fs::create_dir_all(old.join("Stems")).unwrap();
        std::fs::write(old.join("Stems/a.wav"), b"").unwrap();
        let track = old.join("Stems").join("a.wav");
        let json = serde_json::json!({ "tracks": [{ "location": track }] }).to_string();
        std::fs::write(old.join("collection.json"), json).unwrap();

        move_old_data_dir(&old, &new);
        let text = std::fs::read_to_string(new.join("collection.json")).unwrap();
        let moved = serde_json::json!(new.join("Stems").join("a.wav")).to_string();
        assert!(text.contains(moved.trim_matches('"')), "{text}");
        assert!(new.join("Stems/a.wav").is_file());
        #[cfg(unix)]
        assert!(old.join("Stems/a.wav").is_file(), "the old name still leads there");
        // Done once: the link left behind isn't moved again.
        move_old_data_dir(&old, &new);
        assert!(new.join("collection.json").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
