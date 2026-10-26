//! Writing DeviceSQL databases.
//!
//! The layout copies a freshly written rekordbox export:
//!
//! - page 0: file header and table directory;
//! - pages `2i + 1` and `2i + 2` for table `i`: an index page, then either the
//!   table's first data page or an all-zero placeholder if the table is empty;
//! - further data pages are appended after those;
//! - the last data page of a table links to an unused page number past the
//!   end of the file, which the directory lists as the table's empty candidate.
//!
//! Index pages are written without entries, as rekordbox does in new exports.
//!
//! Sequence numbers and empty-candidate numbers follow rekordbox's write
//! order: the last non-empty table first, then the others in table order.
//! The file ends at the last index or data page. With this, rebuilding the
//! empty fixture exports from their rows reproduces them byte for byte.

use super::encode::EncodedRow;
use super::{
    Album, Artist, Artwork, Color, ColumnEntry, Database, FILE_HEADER_LEN, Genre, HistoryEntry,
    HistoryPlaylist, Key, Label, PAGE_HEADER_LEN, PlaylistEntry, PlaylistTreeNode, ROW_GROUP_LEN,
    ROWS_PER_GROUP, RowRef, Schema, TableType, Tag, TagTrack, ToRow, Track,
};
use crate::error::Result;

const PAGE_SIZE: usize = 4096;
const HEAP_LEN: usize = PAGE_SIZE - PAGE_HEADER_LEN;
const INDEX_SLOTS: usize = 1004;
const INDEX_SLOT_EMPTY: u32 = 0x1fff_fff8;
const NO_PAGE: u32 = 0x03ff_ffff;
const FLAGS_DATA: u8 = 0x24;
const FLAGS_INDEX: u8 = 0x64;
/// File header value of unknown meaning; 5 in every export seen.
const HEADER_UNKNOWN: u32 = 5;

/// Collects rows per table and lays them out into a database file.
#[derive(Debug, Clone)]
pub struct DatabaseWriter {
    schema: Schema,
    /// Rows per table, indexed by the table's raw type.
    tables: Vec<Vec<EncodedRow>>,
}

impl DatabaseWriter {
    /// An empty database with every table of `schema`.
    pub fn new(schema: Schema) -> Self {
        let count = match schema {
            Schema::Export => 20,
            Schema::ExportExt => 9,
        };
        Self { schema, tables: vec![Vec::new(); count] }
    }

    /// A writer holding every row of `db`. Rows of known types are decoded
    /// and re-encoded; rows of other tables are copied byte for byte.
    pub fn from_database(db: &Database) -> Result<Self> {
        fn reencode<T: ToRow>(row: &RowRef<'_>) -> Result<EncodedRow> {
            Ok(T::from_row(row)?.to_row())
        }
        let mut writer = Self::new(db.schema());
        for table in db.tables() {
            let encode: Option<fn(&RowRef<'_>) -> Result<EncodedRow>> = match table.table_type {
                TableType::Tracks => Some(reencode::<Track>),
                TableType::Genres => Some(reencode::<Genre>),
                TableType::Artists => Some(reencode::<Artist>),
                TableType::Albums => Some(reencode::<Album>),
                TableType::Labels => Some(reencode::<Label>),
                TableType::Keys => Some(reencode::<Key>),
                TableType::Colors => Some(reencode::<Color>),
                TableType::PlaylistTree => Some(reencode::<PlaylistTreeNode>),
                TableType::PlaylistEntries => Some(reencode::<PlaylistEntry>),
                TableType::HistoryPlaylists => Some(reencode::<HistoryPlaylist>),
                TableType::HistoryEntries => Some(reencode::<HistoryEntry>),
                TableType::Artwork => Some(reencode::<Artwork>),
                TableType::Columns => Some(reencode::<ColumnEntry>),
                TableType::Tags => Some(reencode::<Tag>),
                TableType::TagTracks => Some(reencode::<TagTrack>),
                TableType::History | TableType::Unknown(_) => None,
            };
            let rows = db.table_rows(table)?;
            let raw = db.raw_rows(table)?;
            for (row, bytes) in rows.iter().zip(raw) {
                let encoded = match encode {
                    Some(encode) => encode(row)?,
                    None => EncodedRow::raw(bytes),
                };
                writer.push_encoded(table.table_type, encoded);
            }
        }
        Ok(writer)
    }

    pub fn push<T: ToRow>(&mut self, row: &T) {
        self.push_encoded(T::TABLE, row.to_row());
    }

    pub fn push_encoded(&mut self, table: TableType, row: EncodedRow) {
        let raw = table.to_raw(self.schema) as usize;
        assert!(raw < self.tables.len(), "{table:?} is not a table of {:?}", self.schema);
        assert!(row.len() + 2 + 4 <= HEAP_LEN, "row of {} bytes does not fit on a page", row.len());
        self.tables[raw].push(row);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let table_count = self.tables.len();
        // Split each table's rows into pages.
        let paged: Vec<Vec<&[EncodedRow]>> =
            self.tables.iter().map(|rows| split_pages(rows)).collect();

        // Assign page numbers: the first data page of table i is 2i + 2, the rest follow.
        let mut next_page = 2 * table_count as u32 + 1;
        let mut page_numbers: Vec<Vec<u32>> = Vec::with_capacity(table_count);
        for (i, pages) in paged.iter().enumerate() {
            let mut numbers = Vec::with_capacity(pages.len());
            for p in 0..pages.len() {
                numbers.push(if p == 0 {
                    2 * i as u32 + 2
                } else {
                    next_page += 1;
                    next_page - 1
                });
            }
            page_numbers.push(numbers);
        }
        let last_written = page_numbers
            .iter()
            .flatten()
            .copied()
            .chain([2 * table_count as u32 - 1])
            .max()
            .expect("at least one page");

        // Write order: the last non-empty table, then the other non-empty tables.
        let non_empty: Vec<usize> =
            (0..table_count).filter(|&i| !page_numbers[i].is_empty()).collect();
        let write_order: Vec<usize> = non_empty
            .last()
            .copied()
            .into_iter()
            .chain(non_empty.iter().copied().take(non_empty.len().saturating_sub(1)))
            .collect();

        // Empty candidates: the placeholder page for empty tables, a fresh number otherwise.
        let mut candidates: Vec<u32> = (0..table_count).map(|i| 2 * i as u32 + 2).collect();
        let mut sequences: Vec<Vec<u32>> = page_numbers.iter().map(|n| vec![1; n.len()]).collect();
        let mut sequence = 0;
        for &i in &write_order {
            candidates[i] = next_page;
            next_page += 1;
            for seq in &mut sequences[i] {
                sequence += 1;
                *seq = sequence;
            }
        }

        let mut file = vec![0u8; (last_written as usize + 1) * PAGE_SIZE];

        // File header and table directory.
        put_u32(&mut file, 0x04, PAGE_SIZE as u32);
        put_u32(&mut file, 0x08, table_count as u32);
        put_u32(&mut file, 0x0c, next_page);
        put_u32(&mut file, 0x10, HEADER_UNKNOWN);
        put_u32(&mut file, 0x14, sequence + 4);
        for i in 0..table_count {
            let entry = FILE_HEADER_LEN + i * 16;
            let index_page = 2 * i as u32 + 1;
            put_u32(&mut file, entry, i as u32);
            put_u32(&mut file, entry + 4, candidates[i]);
            put_u32(&mut file, entry + 8, index_page);
            put_u32(&mut file, entry + 12, page_numbers[i].last().copied().unwrap_or(index_page));
        }

        for i in 0..table_count {
            let raw_type = i as u32;
            let index_page = 2 * i as u32 + 1;
            let first_data = page_numbers[i].first().copied();
            write_index_page(page_mut(&mut file, index_page), index_page, raw_type, first_data);

            for (p, rows) in paged[i].iter().enumerate() {
                let number = page_numbers[i][p];
                let next = page_numbers[i].get(p + 1).copied().unwrap_or(candidates[i]);
                let page = page_mut(&mut file, number);
                write_data_page(page, number, raw_type, next, sequences[i][p], rows);
            }
        }
        file
    }
}

fn put_u16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn page_mut(file: &mut [u8], number: u32) -> &mut [u8] {
    let start = number as usize * PAGE_SIZE;
    &mut file[start..start + PAGE_SIZE]
}

fn index_len(rows: usize) -> usize {
    rows.div_ceil(ROWS_PER_GROUP) * 4 + rows * 2
}

/// Greedily fills pages: a row joins the current page while heap and row index fit.
fn split_pages(rows: &[EncodedRow]) -> Vec<&[EncodedRow]> {
    let mut pages = Vec::new();
    let (mut start, mut used) = (0, 0);
    for (i, row) in rows.iter().enumerate() {
        if used + row.len() + index_len(i - start + 1) > HEAP_LEN {
            pages.push(&rows[start..i]);
            (start, used) = (i, 0);
        }
        used += row.len();
    }
    if start < rows.len() {
        pages.push(&rows[start..]);
    }
    pages
}

fn write_page_header(page: &mut [u8], number: u32, raw_type: u32, next: u32, sequence: u32) {
    put_u32(page, 0x04, number);
    put_u32(page, 0x08, raw_type);
    put_u32(page, 0x0c, next);
    put_u32(page, 0x10, sequence);
}

fn write_index_page(page: &mut [u8], number: u32, raw_type: u32, first_data: Option<u32>) {
    write_page_header(page, number, raw_type, number + 1, 1);
    page[0x1b] = FLAGS_INDEX;
    put_u16(page, 0x20, 0x1fff);
    put_u16(page, 0x22, 0x1fff);
    put_u16(page, 0x24, INDEX_SLOTS as u16);

    let body = PAGE_HEADER_LEN;
    put_u32(page, body, number);
    put_u32(page, body + 4, first_data.unwrap_or(NO_PAGE));
    put_u32(page, body + 8, NO_PAGE);
    put_u16(page, body + 0x12, 0x1fff);
    for slot in 0..INDEX_SLOTS {
        put_u32(page, body + 0x14 + 4 * slot, INDEX_SLOT_EMPTY);
    }
}

fn write_data_page(
    page: &mut [u8],
    number: u32,
    raw_type: u32,
    next: u32,
    sequence: u32,
    rows: &[EncodedRow],
) {
    write_page_header(page, number, raw_type, next, sequence);
    let count = rows.len();
    let mut used = 0;
    for (slot, row) in rows.iter().enumerate() {
        let at = PAGE_HEADER_LEN + used;
        page[at..at + row.len()].copy_from_slice(&row.bytes);
        if let Some(shift_at) = row.index_shift_at {
            put_u16(page, at + shift_at, (slot * 0x20) as u16);
        }
        let (group, position) = (slot / ROWS_PER_GROUP, slot % ROWS_PER_GROUP);
        let base = PAGE_SIZE - group * ROW_GROUP_LEN;
        put_u16(page, base - 6 - 2 * position, used as u16);
        used += row.len();
    }
    for group in 0..count.div_ceil(ROWS_PER_GROUP) {
        let rows_in_group = (count - group * ROWS_PER_GROUP).min(ROWS_PER_GROUP);
        let flags = ((1u32 << rows_in_group) - 1) as u16;
        let base = PAGE_SIZE - group * ROW_GROUP_LEN;
        put_u16(page, base - 4, flags);
        put_u16(page, base - 2, flags);
    }

    let counts = count as u32 | (count as u32) << 13;
    page[0x18..0x1b].copy_from_slice(&counts.to_le_bytes()[..3]);
    page[0x1b] = FLAGS_DATA;
    put_u16(page, 0x1c, (HEAP_LEN - used - index_len(count)) as u16);
    put_u16(page, 0x1e, used as u16);
    put_u16(page, 0x20, count as u16);
}
