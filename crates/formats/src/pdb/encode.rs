//! Encoding rows. Layout rules (string order, alignment, padding) follow what
//! rekordbox writes, as measured on the fixture exports:
//!
//! - short ASCII strings are packed; long ASCII (`0x40`) and UTF-16 (`0x90`)
//!   strings start on a 4-byte boundary within the row;
//! - rows are padded to a multiple of 4 bytes; artist and album rows to
//!   `align4(content + 6)`, track rows to `align4(content) + 52`.

use super::rows::{COLUMN_NAME_END, COLUMN_NAME_START, TRACK_EXTRA_STRINGS};
use super::{
    Album, Artist, Artwork, Color, ColumnEntry, FromRow, Genre, HistoryEntry, HistoryPlaylist, Key,
    Label, PlaylistEntry, PlaylistTreeNode, Tag, TagTrack, Track,
};

const SHORT_ASCII_MAX: usize = 126;

/// A row ready to be placed on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedRow {
    pub(super) bytes: Vec<u8>,
    /// Where the row's `index_shift` goes; it depends on the row's page slot.
    pub(super) index_shift_at: Option<usize>,
}

impl EncodedRow {
    /// A row whose bytes are already complete, for tables this crate does not model.
    pub fn raw(bytes: Vec<u8>) -> Self {
        Self { bytes, index_shift_at: None }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// A row type that can be written into its table.
pub trait ToRow: FromRow {
    fn to_row(&self) -> EncodedRow;
}

/// Encodes a DeviceSQL string. Returns the bytes and whether it is a long
/// (4-byte aligned) string.
pub(super) fn encode_string(text: &str) -> (Vec<u8>, bool) {
    if text.is_ascii() && text.len() <= SHORT_ASCII_MAX {
        let mut out = vec![(((text.len() + 1) << 1) | 1) as u8];
        out.extend(text.as_bytes());
        (out, false)
    } else if text.is_ascii() {
        (long_string(0x40, text.as_bytes()), true)
    } else {
        let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        (long_string(0x90, &utf16), true)
    }
}

fn long_string(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend(((body.len() + 4) as u16).to_le_bytes());
    out.push(0);
    out.extend(body);
    out
}

fn encode_isrc(isrc: &str) -> (Vec<u8>, bool) {
    if isrc.is_empty() {
        return encode_string("");
    }
    let mut body = vec![0x03];
    body.extend(isrc.bytes());
    body.push(0);
    (long_string(0x90, &body), true)
}

fn align4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// Builds a row: a fixed part, then strings appended with their alignment.
struct RowBuilder {
    bytes: Vec<u8>,
}

impl RowBuilder {
    fn new(fixed_len: usize) -> Self {
        Self { bytes: vec![0; fixed_len] }
    }

    fn u8(&mut self, at: usize, value: u8) {
        self.bytes[at] = value;
    }

    fn u16(&mut self, at: usize, value: u16) {
        self.bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, at: usize, value: u32) {
        self.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Appends an encoded string and returns its offset in the row.
    fn push_encoded(&mut self, (encoded, long): (Vec<u8>, bool)) -> usize {
        if long {
            self.bytes.resize(align4(self.bytes.len()), 0);
        }
        let offset = self.bytes.len();
        self.bytes.extend(encoded);
        offset
    }

    fn push_string(&mut self, text: &str) -> usize {
        self.push_encoded(encode_string(text))
    }

    fn finish(mut self, len: usize, index_shift_at: Option<usize>) -> EncodedRow {
        self.bytes.resize(len, 0);
        EncodedRow { bytes: self.bytes, index_shift_at }
    }

    fn finish_aligned(self) -> EncodedRow {
        let len = align4(self.bytes.len());
        self.finish(len, None)
    }
}

/// Artists and albums: a one-byte name offset, with UTF-16 names 4-byte aligned.
fn named_row(
    subtype: u16,
    fixed_len: usize,
    name: &str,
    fill: impl FnOnce(&mut RowBuilder),
) -> EncodedRow {
    let mut row = RowBuilder::new(fixed_len);
    row.u16(0, subtype);
    row.u8(fixed_len - 2, 0x03);
    fill(&mut row);
    let offset = row.push_string(name);
    row.u8(fixed_len - 1, offset as u8);
    let len = align4(row.bytes.len() + 6);
    row.finish(len, Some(2))
}

fn id(reference: Option<u32>) -> u32 {
    reference.unwrap_or(0)
}

impl ToRow for Track {
    fn to_row(&self) -> EncodedRow {
        const STRING_OFFSETS: usize = 0x5e;
        let mut row = RowBuilder::new(0x88);
        let x = &self.extras;
        row.u16(0x00, 0x24);
        row.u32(0x04, x.bitmask);
        row.u32(0x08, self.sample_rate);
        row.u32(0x0c, id(self.composer_id));
        row.u32(0x10, self.file_size);
        row.u32(0x14, x.unknown_id);
        row.u16(0x18, x.database_pair.0);
        row.u16(0x1a, x.database_pair.1);
        row.u32(0x1c, id(self.artwork_id));
        row.u32(0x20, id(self.key_id));
        row.u32(0x24, id(self.original_artist_id));
        row.u32(0x28, id(self.label_id));
        row.u32(0x2c, id(self.remixer_id));
        row.u32(0x30, self.bitrate);
        row.u32(0x34, self.track_number);
        row.u32(0x38, self.tempo);
        row.u32(0x3c, id(self.genre_id));
        row.u32(0x40, id(self.album_id));
        row.u32(0x44, id(self.artist_id));
        row.u32(0x48, self.id);
        row.u16(0x4c, self.disc_number);
        row.u16(0x4e, self.play_count);
        row.u16(0x50, self.year);
        row.u16(0x52, self.sample_depth);
        row.u16(0x54, self.duration_secs);
        row.u16(0x56, x.unknown_56);
        row.u8(0x58, self.color_id.unwrap_or(0));
        row.u8(0x59, self.rating);
        row.u16(0x5a, x.unknown_5a);
        row.u16(0x5c, x.unknown_5c);

        let flag = |on: bool| if on { "ON" } else { "" };
        let mut strings: [&str; 21] = [""; 21];
        for (slot, value) in TRACK_EXTRA_STRINGS.iter().zip(&x.strings) {
            strings[*slot] = value;
        }
        strings[5] = &self.message;
        strings[6] = flag(self.kuvo_public);
        strings[7] = flag(self.autoload_hot_cues);
        strings[10] = &self.date_added;
        strings[11] = &self.release_date;
        strings[12] = &self.mix_name;
        strings[14] = &self.analyze_path;
        strings[15] = &self.analyze_date;
        strings[16] = &self.comment;
        strings[17] = &self.title;
        strings[19] = &self.filename;
        strings[20] = &self.file_path;

        for (index, text) in strings.iter().enumerate() {
            let encoded = if index == 0 { encode_isrc(&self.isrc) } else { encode_string(text) };
            let offset = row.push_encoded(encoded);
            row.u16(STRING_OFFSETS + 2 * index, offset as u16);
        }
        let len = align4(row.bytes.len()) + 52;
        row.finish(len, Some(2))
    }
}

impl ToRow for Artist {
    fn to_row(&self) -> EncodedRow {
        named_row(0x60, 10, &self.name, |row| row.u32(0x04, self.id))
    }
}

impl ToRow for Album {
    fn to_row(&self) -> EncodedRow {
        named_row(0x80, 0x16, &self.name, |row| {
            row.u32(0x08, id(self.artist_id));
            row.u32(0x0c, self.id);
        })
    }
}

fn id_and_string(id: u32, text: &str) -> EncodedRow {
    let mut row = RowBuilder::new(4);
    row.u32(0, id);
    row.push_string(text);
    row.finish_aligned()
}

impl ToRow for Genre {
    fn to_row(&self) -> EncodedRow {
        id_and_string(self.id, &self.name)
    }
}

impl ToRow for Label {
    fn to_row(&self) -> EncodedRow {
        id_and_string(self.id, &self.name)
    }
}

impl ToRow for Artwork {
    fn to_row(&self) -> EncodedRow {
        id_and_string(self.id, &self.path)
    }
}

impl ToRow for HistoryPlaylist {
    fn to_row(&self) -> EncodedRow {
        id_and_string(self.id, &self.name)
    }
}

impl ToRow for Key {
    fn to_row(&self) -> EncodedRow {
        let mut row = RowBuilder::new(8);
        row.u32(0, self.id);
        row.u32(4, self.id);
        row.push_string(&self.name);
        row.finish_aligned()
    }
}

impl ToRow for Color {
    fn to_row(&self) -> EncodedRow {
        let mut row = RowBuilder::new(8);
        row.u8(4, self.id);
        row.u16(5, u16::from(self.id));
        row.push_string(&self.name);
        row.finish_aligned()
    }
}

impl ToRow for PlaylistTreeNode {
    fn to_row(&self) -> EncodedRow {
        let mut row = RowBuilder::new(0x14);
        row.u32(0x00, id(self.parent_id));
        row.u32(0x08, self.sort_order);
        row.u32(0x0c, self.id);
        row.u32(0x10, u32::from(self.is_folder));
        row.push_string(&self.name);
        row.finish_aligned()
    }
}

impl ToRow for PlaylistEntry {
    fn to_row(&self) -> EncodedRow {
        let mut row = RowBuilder::new(12);
        row.u32(0, self.entry_index);
        row.u32(4, self.track_id);
        row.u32(8, self.playlist_id);
        row.finish_aligned()
    }
}

impl ToRow for HistoryEntry {
    fn to_row(&self) -> EncodedRow {
        let mut row = RowBuilder::new(12);
        row.u32(0, self.track_id);
        row.u32(4, self.playlist_id);
        row.u32(8, self.entry_index);
        row.finish_aligned()
    }
}

impl ToRow for ColumnEntry {
    fn to_row(&self) -> EncodedRow {
        let mut row = RowBuilder::new(4);
        row.u16(0, self.id);
        row.u16(2, self.code);
        // Always UTF-16: the markers are not ASCII.
        row.push_string(&format!("{COLUMN_NAME_START}{}{COLUMN_NAME_END}", self.name));
        row.finish_aligned()
    }
}

impl ToRow for Tag {
    fn to_row(&self) -> EncodedRow {
        let mut row = RowBuilder::new(0x1f);
        row.u16(0x00, 0x0680);
        row.u32(0x0c, id(self.category_id));
        row.u32(0x10, self.position);
        row.u32(0x14, self.id);
        row.u32(0x18, if self.is_category { 0x0100_0000 } else { 0 });
        row.u8(0x1c, 0x03);
        let name_at = row.push_string(&self.name);
        let empty_at = row.push_string("");
        row.u8(0x1d, name_at as u8);
        row.u8(0x1e, empty_at as u8);
        let len = align4(row.bytes.len()) + 8;
        row.finish(len, Some(2))
    }
}

impl ToRow for TagTrack {
    fn to_row(&self) -> EncodedRow {
        let mut row = RowBuilder::new(16);
        row.u32(4, self.track_id);
        row.u32(8, self.tag_id);
        row.u32(12, 3);
        row.finish_aligned()
    }
}
