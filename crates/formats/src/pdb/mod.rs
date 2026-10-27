//! DeviceSQL databases: `PIONEER/rekordbox/export.pdb` and `exportExt.pdb`.
//!
//! A database is a sequence of fixed-size pages. Page 0 holds the file header
//! and the table directory. Every table is a linked list of pages; each data
//! page stores rows in a heap that grows forward from the page header, and a
//! row index that grows backward from the end of the page in groups of 16.

mod defaults;
mod encode;
mod library;
mod playlists;
mod rows;
mod string;
mod write;

pub use encode::{EncodedRow, ToRow};
pub use library::{HistoryList, Library, LibraryTrack, PlaylistKind, PlaylistNode};
pub use playlists::{PlaylistTables, backup_path, edit_playlists, remove_tracks, write_safely};
pub use rows::{
    Album, Artist, Artwork, Color, ColumnEntry, FromRow, Genre, HistoryEntry, HistoryPlaylist, Key,
    Label, PlaylistEntry, PlaylistTreeNode, Tag, TagTrack, Track, TrackExtras,
};
pub use write::DatabaseWriter;

use std::path::Path;

use crate::bytes::{u8_at, u16_at, u24_at, u32_at};
use crate::error::{Error, Result};

const FILE_HEADER_LEN: usize = 0x1c;
const TABLE_ENTRY_LEN: usize = 0x10;
const PAGE_HEADER_LEN: usize = 0x28;
const ROW_GROUP_LEN: usize = 0x24;
const ROWS_PER_GROUP: usize = 16;
const PAGE_FLAG_NOT_DATA: u8 = 0x40;

/// The two database schemas share a layout but number their tables differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schema {
    /// `export.pdb`
    Export,
    /// `exportExt.pdb`
    ExportExt,
}

impl Schema {
    /// Guesses the schema from a file name (`exportExt.pdb` → [`Schema::ExportExt`]).
    pub fn from_path(path: &Path) -> Self {
        let is_ext = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.to_ascii_lowercase().starts_with("exportext"));
        if is_ext { Schema::ExportExt } else { Schema::Export }
    }
}

/// Kind of rows stored in a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableType {
    Tracks,
    Genres,
    Artists,
    Albums,
    Labels,
    Keys,
    Colors,
    PlaylistTree,
    PlaylistEntries,
    HistoryPlaylists,
    HistoryEntries,
    Artwork,
    Columns,
    History,
    /// `exportExt.pdb` only.
    Tags,
    /// `exportExt.pdb` only.
    TagTracks,
    Unknown(u32),
}

impl TableType {
    /// The number this table type is stored under in `schema`.
    pub fn to_raw(self, schema: Schema) -> u32 {
        (0..32).find(|&raw| TableType::from_raw(schema, raw) == self).unwrap_or(match self {
            TableType::Unknown(raw) => raw,
            _ => u32::MAX,
        })
    }

    fn from_raw(schema: Schema, raw: u32) -> Self {
        match (schema, raw) {
            (Schema::Export, 0) => TableType::Tracks,
            (Schema::Export, 1) => TableType::Genres,
            (Schema::Export, 2) => TableType::Artists,
            (Schema::Export, 3) => TableType::Albums,
            (Schema::Export, 4) => TableType::Labels,
            (Schema::Export, 5) => TableType::Keys,
            (Schema::Export, 6) => TableType::Colors,
            (Schema::Export, 7) => TableType::PlaylistTree,
            (Schema::Export, 8) => TableType::PlaylistEntries,
            (Schema::Export, 11) => TableType::HistoryPlaylists,
            (Schema::Export, 12) => TableType::HistoryEntries,
            (Schema::Export, 13) => TableType::Artwork,
            (Schema::Export, 16) => TableType::Columns,
            (Schema::Export, 19) => TableType::History,
            (Schema::ExportExt, 3) => TableType::Tags,
            (Schema::ExportExt, 4) => TableType::TagTracks,
            (_, raw) => TableType::Unknown(raw),
        }
    }
}

/// An entry of the table directory in the file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Table {
    pub table_type: TableType,
    pub raw_type: u32,
    pub empty_candidate: u32,
    pub first_page: u32,
    pub last_page: u32,
}

/// A DeviceSQL database loaded into memory.
#[derive(Debug, Clone)]
pub struct Database {
    data: Vec<u8>,
    schema: Schema,
    page_size: usize,
    next_unused_page: u32,
    sequence: u32,
    tables: Vec<Table>,
}

impl Database {
    /// Reads a database file, choosing the schema from its file name.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Self::from_bytes(std::fs::read(path)?, Schema::from_path(path))
    }

    pub fn from_bytes(data: Vec<u8>, schema: Schema) -> Result<Self> {
        let page_size = u32_at(&data, 0x04)? as usize;
        if !(PAGE_HEADER_LEN + ROW_GROUP_LEN..=1 << 20).contains(&page_size) {
            return Err(Error::InvalidHeader("unsupported page size"));
        }
        let num_tables = u32_at(&data, 0x08)? as usize;
        if num_tables
            .checked_mul(TABLE_ENTRY_LEN)
            .and_then(|len| len.checked_add(FILE_HEADER_LEN))
            .is_none_or(|end| end > page_size)
        {
            return Err(Error::InvalidHeader("table directory does not fit in the first page"));
        }
        let next_unused_page = u32_at(&data, 0x0c)?;
        let sequence = u32_at(&data, 0x14)?;

        let tables = (0..num_tables)
            .map(|i| {
                let base = FILE_HEADER_LEN + i * TABLE_ENTRY_LEN;
                let raw_type = u32_at(&data, base)?;
                Ok(Table {
                    table_type: TableType::from_raw(schema, raw_type),
                    raw_type,
                    empty_candidate: u32_at(&data, base + 0x04)?,
                    first_page: u32_at(&data, base + 0x08)?,
                    last_page: u32_at(&data, base + 0x0c)?,
                })
            })
            .collect::<Result<_>>()?;

        Ok(Self { data, schema, page_size, next_unused_page, sequence, tables })
    }

    pub fn schema(&self) -> Schema {
        self.schema
    }

    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Incremented by rekordbox on every edit of the database.
    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    pub fn next_unused_page(&self) -> u32 {
        self.next_unused_page
    }

    /// Number of whole pages in the file.
    pub fn page_count(&self) -> usize {
        self.data.len() / self.page_size
    }

    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    pub fn table(&self, table_type: TableType) -> Option<&Table> {
        self.tables.iter().find(|table| table.table_type == table_type)
    }

    pub fn page(&self, index: u32) -> Result<Page<'_>> {
        let start = index as usize * self.page_size;
        let buf =
            self.data.get(start..start + self.page_size).ok_or(Error::PageOutOfBounds { index })?;
        let page = Page::parse(buf)?;
        if page.index != index {
            return Err(Error::PageIndexMismatch { expected: index, found: page.index });
        }
        Ok(page)
    }

    /// The pages of a table in chain order, including pages without rows.
    pub fn table_pages(&self, table: &Table) -> Result<Vec<Page<'_>>> {
        let mut pages = Vec::new();
        let mut index = table.first_page;
        loop {
            if pages.len() >= self.page_count() {
                return Err(Error::PageLoop { first_page: table.first_page });
            }
            let page = self.page(index)?;
            // A chain may continue into pages of another table; stop there.
            if page.raw_type != table.raw_type {
                break;
            }
            let next = page.next_page;
            pages.push(page);
            if index == table.last_page {
                break;
            }
            index = next;
        }
        Ok(pages)
    }

    /// Present rows of a table, in page order.
    pub fn table_rows(&self, table: &Table) -> Result<Vec<RowRef<'_>>> {
        let mut rows = Vec::new();
        for page in self.table_pages(table)? {
            rows.extend(page.rows()?);
        }
        Ok(rows)
    }

    /// The bytes of every present row of a table, in page and slot order.
    /// Row lengths are not stored; a row is taken to extend to the next row
    /// in the heap, or to the end of the used heap.
    pub fn raw_rows(&self, table: &Table) -> Result<Vec<Vec<u8>>> {
        let mut rows = Vec::new();
        for page in self.table_pages(table)? {
            rows.extend(page.raw_rows()?);
        }
        Ok(rows)
    }

    /// Parses every row of the table that holds `T`. A missing table yields no rows.
    pub fn read_all<T: FromRow>(&self) -> Result<Vec<T>> {
        let Some(table) = self.table(T::TABLE) else {
            return Ok(Vec::new());
        };
        self.table_rows(table)?.iter().map(T::from_row).collect()
    }
}

/// One page of a table.
#[derive(Debug, Clone, Copy)]
pub struct Page<'a> {
    buf: &'a [u8],
    pub index: u32,
    pub raw_type: u32,
    pub next_page: u32,
    pub sequence: u32,
    /// Row index slots ever allocated, including deleted rows.
    pub num_row_offsets: u16,
    /// Rows currently present.
    pub num_rows: u16,
    pub flags: u8,
    pub free_size: u16,
    pub used_size: u16,
}

impl<'a> Page<'a> {
    fn parse(buf: &'a [u8]) -> Result<Self> {
        let row_counts = u24_at(buf, 0x18)?;
        Ok(Self {
            buf,
            index: u32_at(buf, 0x04)?,
            raw_type: u32_at(buf, 0x08)?,
            next_page: u32_at(buf, 0x0c)?,
            sequence: u32_at(buf, 0x10)?,
            num_row_offsets: (row_counts & 0x1fff) as u16,
            num_rows: (row_counts >> 13) as u16,
            flags: u8_at(buf, 0x1b)?,
            free_size: u16_at(buf, 0x1c)?,
            used_size: u16_at(buf, 0x1e)?,
        })
    }

    /// Whether the page carries a row index; other pages must not be parsed for rows.
    pub fn is_data_page(&self) -> bool {
        self.flags & PAGE_FLAG_NOT_DATA == 0
    }

    /// Rows marked present in the row index.
    pub fn rows(&self) -> Result<Vec<RowRef<'a>>> {
        if !self.is_data_page() {
            return Ok(Vec::new());
        }
        let mut rows = Vec::with_capacity(self.num_rows.into());
        for slot in 0..usize::from(self.num_row_offsets) {
            let (group, position) = (slot / ROWS_PER_GROUP, slot % ROWS_PER_GROUP);
            let base = self
                .buf
                .len()
                .checked_sub(group * ROW_GROUP_LEN)
                .ok_or(Error::OutOfBounds { offset: group * ROW_GROUP_LEN, len: ROW_GROUP_LEN })?;
            let index_pos = |back: usize| {
                base.checked_sub(back).ok_or(Error::OutOfBounds { offset: base, len: back })
            };
            let present_flags = u16_at(self.buf, index_pos(4)?)?;
            if present_flags >> position & 1 == 0 {
                continue;
            }
            let row_offset = u16_at(self.buf, index_pos(6 + 2 * position)?)?;
            rows.push(RowRef { page: self.buf, offset: PAGE_HEADER_LEN + usize::from(row_offset) });
        }
        Ok(rows)
    }
}

impl Page<'_> {
    /// The bytes of each present row; see [`Database::raw_rows`].
    pub fn raw_rows(&self) -> Result<Vec<Vec<u8>>> {
        let rows = self.rows()?;
        let mut starts: Vec<usize> = rows.iter().map(|row| row.offset).collect();
        starts.sort_unstable();
        let heap_end = PAGE_HEADER_LEN + usize::from(self.used_size);
        rows.iter()
            .map(|row| {
                let end = starts.iter().copied().find(|&s| s > row.offset).unwrap_or(heap_end);
                crate::bytes::slice(self.buf, row.offset, end.saturating_sub(row.offset))
                    .map(<[u8]>::to_vec)
            })
            .collect()
    }
}

/// The location of one row inside its page. String offsets in a row are
/// relative to the row start, so rows are parsed against the whole page.
#[derive(Debug, Clone, Copy)]
pub struct RowRef<'a> {
    page: &'a [u8],
    offset: usize,
}

impl RowRef<'_> {
    /// Offset of the row from the start of its page.
    pub fn offset(&self) -> usize {
        self.offset
    }

    fn u8(&self, at: usize) -> Result<u8> {
        u8_at(self.page, self.offset + at)
    }

    fn u16(&self, at: usize) -> Result<u16> {
        u16_at(self.page, self.offset + at)
    }

    fn u32(&self, at: usize) -> Result<u32> {
        u32_at(self.page, self.offset + at)
    }

    fn string(&self, at: usize) -> Result<String> {
        string::read(self.page, self.offset + at)
    }
}
