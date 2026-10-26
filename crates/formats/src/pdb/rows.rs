//! Typed rows. Offsets are relative to the start of each row.
//!
//! Reference fields use `None` where the file stores ID 0 ("not set").

use super::{RowRef, TableType, string};
use crate::error::Result;

/// A row type that lives in one table.
pub trait FromRow: Sized {
    const TABLE: TableType;
    fn from_row(row: &RowRef<'_>) -> Result<Self>;
}

/// Subtype bit meaning a row stores a 16-bit offset to its name instead of an 8-bit one.
const SUBTYPE_FAR_NAME: u16 = 0x04;

fn reference(id: u32) -> Option<u32> {
    (id != 0).then_some(id)
}

/// Reads a name whose offset is 8-bit, or 16-bit when the row subtype says so.
fn near_or_far_name(row: &RowRef<'_>, near_at: usize, far_at: usize) -> Result<String> {
    let offset = if row.u16(0x00)? & SUBTYPE_FAR_NAME != 0 {
        usize::from(row.u16(far_at)?)
    } else {
        usize::from(row.u8(near_at)?)
    };
    row.string(offset)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: u32,
    pub title: String,
    pub artist_id: Option<u32>,
    pub composer_id: Option<u32>,
    pub original_artist_id: Option<u32>,
    pub remixer_id: Option<u32>,
    pub album_id: Option<u32>,
    pub genre_id: Option<u32>,
    pub label_id: Option<u32>,
    pub key_id: Option<u32>,
    pub artwork_id: Option<u32>,
    pub color_id: Option<u8>,
    /// Stars, 0 to 5.
    pub rating: u8,
    /// Tempo at the start of the track in BPM × 100.
    pub tempo: u32,
    pub duration_secs: u16,
    pub sample_rate: u32,
    pub sample_depth: u16,
    /// kbit/s.
    pub bitrate: u32,
    pub file_size: u32,
    pub track_number: u32,
    pub disc_number: u16,
    pub year: u16,
    pub play_count: u16,
    pub isrc: String,
    pub mix_name: String,
    pub comment: String,
    pub message: String,
    pub date_added: String,
    pub release_date: String,
    pub analyze_path: String,
    pub analyze_date: String,
    pub filename: String,
    pub file_path: String,
    pub kuvo_public: bool,
    pub autoload_hot_cues: bool,
    pub extras: TrackExtras,
}

/// Track row fields whose meaning is unknown, kept so rows can be written back
/// unchanged. The defaults are the values seen in most rekordbox exports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackExtras {
    pub bitmask: u32,
    /// Varies per track; looks like a random or hashed value.
    pub unknown_id: u32,
    /// Constant within one database, different between databases.
    pub database_pair: (u16, u16),
    pub unknown_56: u16,
    pub unknown_5a: u16,
    pub unknown_5c: u16,
    /// Strings 1–4, 8, 9, 13 and 18 of the row.
    pub strings: [String; 8],
}

/// Indices of [`TrackExtras::strings`] within the 21 strings of a track row.
pub(super) const TRACK_EXTRA_STRINGS: [usize; 8] = [1, 2, 3, 4, 8, 9, 13, 18];

impl Default for TrackExtras {
    fn default() -> Self {
        Self {
            bitmask: 0x000c_0700,
            unknown_id: 0,
            database_pair: (64128, 1511),
            unknown_56: 41,
            unknown_5a: 1,
            unknown_5c: 3,
            strings: ["", "1", "1", "", "", "", "", ""].map(String::from),
        }
    }
}

impl Track {
    pub fn bpm(&self) -> f64 {
        f64::from(self.tempo) / 100.0
    }
}

impl FromRow for Track {
    const TABLE: TableType = TableType::Tracks;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        const STRING_OFFSETS: usize = 0x5e;
        let string_at = |index: usize| -> Result<String> {
            row.string(usize::from(row.u16(STRING_OFFSETS + 2 * index)?))
        };
        let flag_at = |index: usize| -> Result<bool> { Ok(string_at(index)? == "ON") };
        let isrc_offset = usize::from(row.u16(STRING_OFFSETS)?);

        Ok(Self {
            sample_rate: row.u32(0x08)?,
            composer_id: reference(row.u32(0x0c)?),
            file_size: row.u32(0x10)?,
            artwork_id: reference(row.u32(0x1c)?),
            key_id: reference(row.u32(0x20)?),
            original_artist_id: reference(row.u32(0x24)?),
            label_id: reference(row.u32(0x28)?),
            remixer_id: reference(row.u32(0x2c)?),
            bitrate: row.u32(0x30)?,
            track_number: row.u32(0x34)?,
            tempo: row.u32(0x38)?,
            genre_id: reference(row.u32(0x3c)?),
            album_id: reference(row.u32(0x40)?),
            artist_id: reference(row.u32(0x44)?),
            id: row.u32(0x48)?,
            disc_number: row.u16(0x4c)?,
            play_count: row.u16(0x4e)?,
            year: row.u16(0x50)?,
            sample_depth: row.u16(0x52)?,
            duration_secs: row.u16(0x54)?,
            color_id: Some(row.u8(0x58)?).filter(|&id| id != 0),
            rating: row.u8(0x59)?,
            isrc: string::read_isrc(row.page, row.offset + isrc_offset)?,
            message: string_at(5)?,
            kuvo_public: flag_at(6)?,
            autoload_hot_cues: flag_at(7)?,
            date_added: string_at(10)?,
            release_date: string_at(11)?,
            mix_name: string_at(12)?,
            analyze_path: string_at(14)?,
            analyze_date: string_at(15)?,
            comment: string_at(16)?,
            title: string_at(17)?,
            filename: string_at(19)?,
            file_path: string_at(20)?,
            extras: TrackExtras {
                bitmask: row.u32(0x04)?,
                unknown_id: row.u32(0x14)?,
                database_pair: (row.u16(0x18)?, row.u16(0x1a)?),
                unknown_56: row.u16(0x56)?,
                unknown_5a: row.u16(0x5a)?,
                unknown_5c: row.u16(0x5c)?,
                strings: {
                    let mut strings: [String; 8] = Default::default();
                    for (slot, &index) in strings.iter_mut().zip(&TRACK_EXTRA_STRINGS) {
                        *slot = string_at(index)?;
                    }
                    strings
                },
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artist {
    pub id: u32,
    pub name: String,
}

impl FromRow for Artist {
    const TABLE: TableType = TableType::Artists;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        Ok(Self { id: row.u32(0x04)?, name: near_or_far_name(row, 0x09, 0x0a)? })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    pub id: u32,
    pub artist_id: Option<u32>,
    pub name: String,
}

impl FromRow for Album {
    const TABLE: TableType = TableType::Albums;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        Ok(Self {
            artist_id: reference(row.u32(0x08)?),
            id: row.u32(0x0c)?,
            name: near_or_far_name(row, 0x15, 0x16)?,
        })
    }
}

/// Generates the row type for the tables that hold an ID followed by a name.
macro_rules! id_name_row {
    ($(#[$meta:meta])* $name:ident, $field:ident, $table:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            pub id: u32,
            pub $field: String,
        }

        impl FromRow for $name {
            const TABLE: TableType = $table;

            fn from_row(row: &RowRef<'_>) -> Result<Self> {
                Ok(Self { id: row.u32(0x00)?, $field: row.string(0x04)? })
            }
        }
    };
}

id_name_row!(Genre, name, TableType::Genres);
id_name_row!(Label, name, TableType::Labels);
id_name_row!(
    /// Path of an artwork image on the medium.
    Artwork,
    path,
    TableType::Artwork
);
id_name_row!(
    /// A history playlist recorded by a player, such as "HISTORY 001".
    HistoryPlaylist,
    name,
    TableType::HistoryPlaylists
);

/// A musical key as displayed by the player, such as "Am" or "8A".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub id: u32,
    pub name: String,
}

impl FromRow for Key {
    const TABLE: TableType = TableType::Keys;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        Ok(Self { id: row.u32(0x00)?, name: row.string(0x08)? })
    }
}

/// A track color label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Color {
    pub id: u8,
    pub name: String,
}

impl FromRow for Color {
    const TABLE: TableType = TableType::Colors;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        // Color IDs are stored as u16 but tracks reference them with a single byte.
        Ok(Self { id: row.u16(0x05)? as u8, name: row.string(0x08)? })
    }
}

/// A playlist or folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistTreeNode {
    pub id: u32,
    /// `None` at the root level.
    pub parent_id: Option<u32>,
    pub sort_order: u32,
    pub is_folder: bool,
    pub name: String,
}

impl FromRow for PlaylistTreeNode {
    const TABLE: TableType = TableType::PlaylistTree;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        Ok(Self {
            parent_id: reference(row.u32(0x00)?),
            sort_order: row.u32(0x08)?,
            id: row.u32(0x0c)?,
            is_folder: row.u32(0x10)? != 0,
            name: row.string(0x14)?,
        })
    }
}

/// Places a track at a position in a playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaylistEntry {
    pub entry_index: u32,
    pub track_id: u32,
    pub playlist_id: u32,
}

impl FromRow for PlaylistEntry {
    const TABLE: TableType = TableType::PlaylistEntries;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        Ok(Self {
            entry_index: row.u32(0x00)?,
            track_id: row.u32(0x04)?,
            playlist_id: row.u32(0x08)?,
        })
    }
}

/// Places a track at a position in a history playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryEntry {
    pub track_id: u32,
    pub playlist_id: u32,
    pub entry_index: u32,
}

impl FromRow for HistoryEntry {
    const TABLE: TableType = TableType::HistoryEntries;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        Ok(Self {
            track_id: row.u32(0x00)?,
            playlist_id: row.u32(0x04)?,
            entry_index: row.u32(0x08)?,
        })
    }
}

/// A My Tag or a My Tag category (`exportExt.pdb`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: u32,
    /// The category a tag belongs to; `None` for categories.
    pub category_id: Option<u32>,
    /// Position of the tag in its category, or of the category in the category list.
    pub position: u32,
    pub is_category: bool,
    pub name: String,
}

impl FromRow for Tag {
    const TABLE: TableType = TableType::Tags;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        Ok(Self {
            category_id: reference(row.u32(0x0c)?),
            position: row.u32(0x10)?,
            id: row.u32(0x14)?,
            is_category: row.u32(0x18)? != 0,
            name: near_or_far_name(row, 0x1d, 0x1e)?,
        })
    }
}

/// Assigns a My Tag to a track (`exportExt.pdb`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagTrack {
    pub track_id: u32,
    pub tag_id: u32,
}

impl FromRow for TagTrack {
    const TABLE: TableType = TableType::TagTracks;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        Ok(Self { track_id: row.u32(0x04)?, tag_id: row.u32(0x08)? })
    }
}

/// A browse menu entry players show, such as "GENRE" or "BPM".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnEntry {
    pub id: u16,
    /// Menu code of unknown exact meaning (0x80 + …).
    pub code: u16,
    /// The name without the U+FFFA / U+FFFB markers rekordbox wraps it in.
    pub name: String,
}

pub(super) const COLUMN_NAME_START: char = '\u{fffa}';
pub(super) const COLUMN_NAME_END: char = '\u{fffb}';

impl FromRow for ColumnEntry {
    const TABLE: TableType = TableType::Columns;

    fn from_row(row: &RowRef<'_>) -> Result<Self> {
        let raw = row.string(0x04)?;
        let name = raw
            .strip_prefix(COLUMN_NAME_START)
            .and_then(|n| n.strip_suffix(COLUMN_NAME_END))
            .unwrap_or(&raw)
            .to_owned();
        Ok(Self { id: row.u16(0x00)?, code: row.u16(0x02)?, name })
    }
}
