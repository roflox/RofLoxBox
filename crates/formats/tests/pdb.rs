use std::collections::BTreeMap;
use std::path::PathBuf;

use deckhand_formats::pdb::{
    Color, Database, Key, Library, PlaylistEntry, PlaylistKind, PlaylistNode, PlaylistTreeNode,
    Schema, TableType, Tag,
};

fn fixture(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path)
}

const DEMO: &str = "complete_export/demo_tracks/PIONEER/rekordbox/export.pdb";
const DEMO_EXT: &str = "complete_export/demo_tracks/PIONEER/rekordbox/exportExt.pdb";
const EMPTY: &str = "complete_export/empty/PIONEER/rekordbox/export.pdb";
const LARGE: &str = "pdb/num_rows/export.pdb";

#[test]
fn demo_export_tracks() {
    let library = Library::open(fixture(DEMO)).unwrap();
    assert_eq!(library.tracks.len(), 2);

    let first = library.track(1).unwrap();
    assert_eq!(first.track.title, "Demo Track 1");
    assert_eq!(first.artist.as_deref(), Some("Loopmasters"));
    assert_eq!(first.key.as_deref(), Some("Fm"));
    assert_eq!(first.album, None);
    assert_eq!(first.track.bpm(), 128.0);
    assert_eq!(first.track.duration_secs, 172);
    assert_eq!(first.track.filename, "Demo Track 1.mp3");
    assert_eq!(first.track.file_path, "/Contents/Loopmasters/UnknownAlbum/Demo Track 1.mp3");
    assert_eq!(first.track.analyze_path, "/PIONEER/USBANLZ/P016/0000875E/ANLZ0000.DAT");

    let second = library.track(2).unwrap();
    assert_eq!(second.track.title, "Demo Track 2");
    assert_eq!(second.track.bpm(), 120.0);
    assert_eq!(second.track.duration_secs, 128);

    assert!(library.playlists.is_empty());
}

#[test]
fn demo_export_lookup_tables() {
    let db = Database::open(fixture(DEMO)).unwrap();
    assert_eq!(db.schema(), Schema::Export);
    assert_eq!(db.page_size(), 4096);

    let colors: Vec<String> = db.read_all::<Color>().unwrap().into_iter().map(|c| c.name).collect();
    assert_eq!(colors, ["Pink", "Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple"]);
    assert_eq!(db.read_all::<Key>().unwrap().len(), 5);
}

#[test]
fn demo_export_ext_tags() {
    let db = Database::open(fixture(DEMO_EXT)).unwrap();
    assert_eq!(db.schema(), Schema::ExportExt);
    let tags = db.read_all::<Tag>().unwrap();
    assert_eq!(tags.len(), 28);
    let mut categories: Vec<&Tag> = tags.iter().filter(|t| t.is_category).collect();
    categories.sort_by_key(|c| c.position);
    let names: Vec<&str> = categories.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Genre", "Components", "Situation", "Untitled Column"]);
    assert!(tags.iter().any(|t| t.name == "Peak Time" && !t.is_category));
}

#[test]
fn empty_export() {
    let library = Library::open(fixture(EMPTY)).unwrap();
    assert!(library.tracks.is_empty());
    assert!(library.playlists.is_empty());
}

#[test]
fn large_export_playlists_resolve() {
    let library = Library::open(fixture(LARGE)).unwrap();
    assert_eq!(library.tracks.len(), 3886);

    fn walk<'a>(nodes: &'a [PlaylistNode], out: &mut Vec<&'a [u32]>) {
        for node in nodes {
            match &node.kind {
                PlaylistKind::Folder(children) => walk(children, out),
                PlaylistKind::Playlist(ids) => out.push(ids),
            }
        }
    }
    let mut lists = Vec::new();
    walk(&library.playlists, &mut lists);
    assert!(!lists.is_empty());
    for ids in lists {
        for id in ids {
            assert!(library.track(*id).is_some(), "playlist references missing track {id}");
        }
    }

    // Positions within each playlist run 1..=n without gaps or duplicates.
    let db = Database::open(fixture(LARGE)).unwrap();
    let mut positions: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for entry in db.read_all::<PlaylistEntry>().unwrap() {
        positions.entry(entry.playlist_id).or_default().push(entry.entry_index);
    }
    for (playlist, mut indices) in positions {
        indices.sort_unstable();
        assert!(indices.iter().copied().eq(1..=indices.len() as u32), "playlist {playlist}");
    }

    assert_eq!(library.history.len(), 1);
    assert_eq!(library.history[0].track_ids.len(), 73);
}

/// Every data page must hold exactly as many present rows as its header declares.
#[test]
fn present_rows_match_page_headers() {
    for path in [DEMO, DEMO_EXT, EMPTY, LARGE] {
        let db = Database::open(fixture(path)).unwrap();
        for table in db.tables() {
            for page in db.table_pages(table).unwrap() {
                if page.is_data_page() {
                    let rows = page.rows().unwrap().len();
                    assert_eq!(rows, usize::from(page.num_rows), "{path}: page {}", page.index);
                }
            }
        }
    }
}

/// Row counts per page and playlist names must agree with rekordcrate, an
/// independent implementation.
///
/// rekordcrate 0.3.0 undercounts rows on pages with more than 255 row slots:
/// on those pages its count disagrees with the page header, while ours matches
/// it (see `present_rows_match_page_headers`), so they are skipped here.
#[test]
fn rows_match_rekordcrate() {
    use binrw::BinRead;
    use rekordcrate::pdb::{Header, PageType, Row};

    for path in [DEMO, EMPTY, LARGE] {
        let db = Database::open(fixture(path)).unwrap();
        let mut file = std::fs::File::open(fixture(path)).unwrap();
        let header = Header::read(&mut file).unwrap();
        assert_eq!(header.tables.len(), db.tables().len());

        for (theirs, ours) in header.tables.iter().zip(db.tables()) {
            if matches!(theirs.page_type, PageType::Unknown(_)) {
                continue;
            }
            let their_pages = header
                .read_pages(
                    &mut file,
                    binrw::Endian::NATIVE,
                    (&theirs.first_page, &theirs.last_page),
                )
                .unwrap();
            let our_pages = db.table_pages(ours).unwrap();
            assert_eq!(our_pages.len(), their_pages.len(), "{path}: {:?}", ours.table_type);

            let mut their_names: BTreeMap<u32, String> = BTreeMap::new();
            for (our_page, their_page) in our_pages.iter().zip(their_pages) {
                let their_rows: Vec<Row> = their_page
                    .row_groups
                    .into_iter()
                    .flat_map(|group| group.present_rows().collect::<Vec<_>>())
                    .collect();
                if our_page.num_row_offsets <= 255 {
                    assert_eq!(
                        our_page.rows().unwrap().len(),
                        their_rows.len(),
                        "{path}: {:?} page {}",
                        ours.table_type,
                        our_page.index
                    );
                }
                for row in their_rows {
                    if let Row::PlaylistTreeNode(node) = row {
                        their_names.insert(node.id.0, node.name.into_string().unwrap());
                    }
                }
            }

            if ours.table_type == TableType::PlaylistTree {
                let our_names: BTreeMap<u32, String> = db
                    .read_all::<PlaylistTreeNode>()
                    .unwrap()
                    .into_iter()
                    .map(|node| (node.id, node.name))
                    .collect();
                assert_eq!(our_names, their_names, "{path}");
            }
        }
    }
}

/// Corrupted or truncated input must produce errors, never panics.
#[test]
fn corrupt_input_does_not_panic() {
    let original = std::fs::read(fixture(DEMO)).unwrap();
    let parse = |bytes: Vec<u8>| {
        if let Ok(db) = Database::from_bytes(bytes, Schema::Export) {
            let _ = Library::from_database(&db);
        }
    };

    for len in (0..original.len()).step_by(997) {
        parse(original[..len].to_vec());
    }
    // Deterministic pseudo-random byte flips across the whole file.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    for _ in 0..300 {
        let mut bytes = original.clone();
        for _ in 0..16 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let at = (state as usize) % bytes.len();
            bytes[at] = (state >> 32) as u8;
        }
        parse(bytes);
    }
}
