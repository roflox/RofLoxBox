//! Writers: byte-exact reproduction of rekordbox output where possible,
//! semantic round trips everywhere else.
#![allow(clippy::unwrap_used)] // tests: helpers outside #[test] functions too

use std::path::{Path, PathBuf};

use deckhand_formats::anlz::AnlzFile;
use deckhand_formats::pdb::{
    Album, Artist, Artwork, Color, ColumnEntry, Database, DatabaseWriter, FromRow, Genre,
    HistoryEntry, HistoryPlaylist, Key, Label, Library, PlaylistEntry, PlaylistTreeNode, Schema,
    TableType, Tag, TagTrack, Track, TrackExtras,
};
use deckhand_formats::settings::values::TempoRange;
use deckhand_formats::settings::{Settings, SettingsFile, SettingsKind};

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path)
}

fn files_under(dir: &Path, keep: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if keep(path.file_name().unwrap().to_str().unwrap()) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

// Settings

#[test]
fn settings_files_round_trip_byte_exact() {
    let files =
        files_under(&fixture(""), &|n| n.ends_with("SETTING.DAT") || n.ends_with("SETTING2.DAT"));
    assert!(files.len() > 70);
    for path in files {
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            SettingsFile::from_bytes(&original).unwrap().to_bytes(),
            original,
            "{}",
            path.display()
        );
    }
}

#[test]
fn default_settings_match_new_rekordbox_export() {
    for (kind, name) in [
        (SettingsKind::Device, "DEVSETTING.DAT"),
        (SettingsKind::Player, "MYSETTING.DAT"),
        (SettingsKind::Player2, "MYSETTING2.DAT"),
        (SettingsKind::Mixer, "DJMMYSETTING.DAT"),
    ] {
        let expected = std::fs::read(fixture("complete_export/empty/PIONEER").join(name)).unwrap();
        assert_eq!(SettingsFile::with_defaults(kind).to_bytes(), expected, "{name}");
    }
}

#[test]
fn changed_settings_are_written() {
    let mut file = SettingsFile::with_defaults(SettingsKind::Player);
    let Some(Settings::Player(mut player)) = file.settings() else { panic!() };
    player.tempo_range = TempoRange::Sixteen;
    player.lcd_brightness = Some(5);
    file.set_settings(&Settings::Player(player));

    let reread = SettingsFile::from_bytes(&file.to_bytes()).unwrap();
    assert!(reread.checksum_is_valid());
    assert_eq!(reread.settings(), Some(Settings::Player(player)));
    // Same bytes as rekordbox wrote for the ±16 % option alone, except brightness.
    let mut expected =
        SettingsFile::open(fixture("settings/mysetting/temporange_16/MYSETTING.DAT")).unwrap();
    let Some(Settings::Player(mut theirs)) = expected.settings() else { panic!() };
    theirs.lcd_brightness = Some(5);
    expected.set_settings(&Settings::Player(theirs));
    assert_eq!(file.to_bytes(), expected.to_bytes());
}

// Analysis files

#[test]
fn analysis_files_round_trip_byte_exact() {
    let files = files_under(&fixture("complete_export"), &|n| n.starts_with("ANLZ"));
    assert_eq!(files.len(), 6);
    for path in files {
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            AnlzFile::from_bytes(&original).unwrap().to_bytes(),
            original,
            "{}",
            path.display()
        );
    }
}

// Databases

#[test]
fn new_export_matches_new_rekordbox_export() {
    let expected =
        std::fs::read(fixture("complete_export/empty/PIONEER/rekordbox/export.pdb")).unwrap();
    assert_eq!(DatabaseWriter::new_export("2022-02-02").to_bytes(), expected);
}

#[test]
fn new_export_ext_with_tags_matches_rekordbox() {
    let path = fixture("complete_export/empty/PIONEER/rekordbox/exportExt.pdb");
    let mut writer = DatabaseWriter::new_export_ext();
    for tag in Database::open(&path).unwrap().read_all::<Tag>().unwrap() {
        writer.push(&tag);
    }
    assert_eq!(writer.to_bytes(), std::fs::read(path).unwrap());
}

fn rewrite(db: &Database) -> Database {
    Database::from_bytes(DatabaseWriter::from_database(db).unwrap().to_bytes(), db.schema())
        .unwrap()
}

fn assert_same_rows<T: FromRow + PartialEq + std::fmt::Debug>(
    a: &Database,
    b: &Database,
    path: &str,
) {
    assert_eq!(a.read_all::<T>().unwrap(), b.read_all::<T>().unwrap(), "{path}: {:?}", T::TABLE);
}

/// Rewriting any fixture keeps every row: typed rows decode to the same
/// values, other tables keep their bytes.
#[test]
fn databases_round_trip() {
    for path in [
        "complete_export/empty/PIONEER/rekordbox/export.pdb",
        "complete_export/demo_tracks/PIONEER/rekordbox/export.pdb",
        "complete_export/demo_tracks/PIONEER/rekordbox/exportExt.pdb",
        "pdb/num_rows/export.pdb",
    ] {
        let original = Database::open(fixture(path)).unwrap();
        let written = rewrite(&original);
        assert_same_rows::<Track>(&original, &written, path);
        assert_same_rows::<Artist>(&original, &written, path);
        assert_same_rows::<Album>(&original, &written, path);
        assert_same_rows::<Genre>(&original, &written, path);
        assert_same_rows::<Label>(&original, &written, path);
        assert_same_rows::<Key>(&original, &written, path);
        assert_same_rows::<Color>(&original, &written, path);
        assert_same_rows::<Artwork>(&original, &written, path);
        assert_same_rows::<PlaylistTreeNode>(&original, &written, path);
        assert_same_rows::<PlaylistEntry>(&original, &written, path);
        assert_same_rows::<HistoryPlaylist>(&original, &written, path);
        assert_same_rows::<HistoryEntry>(&original, &written, path);
        assert_same_rows::<ColumnEntry>(&original, &written, path);
        assert_same_rows::<Tag>(&original, &written, path);
        assert_same_rows::<TagTrack>(&original, &written, path);
        for (a, b) in original.tables().iter().zip(written.tables()) {
            assert_eq!(a.table_type, b.table_type);
            if matches!(a.table_type, TableType::History | TableType::Unknown(_)) {
                assert_eq!(original.raw_rows(a).unwrap(), written.raw_rows(b).unwrap(), "{path}");
            }
        }
        if original.schema() == Schema::Export {
            assert_eq!(
                Library::from_database(&original).unwrap(),
                Library::from_database(&written).unwrap()
            );
        }
    }
}

/// Written pages are internally consistent.
#[test]
fn written_pages_are_consistent() {
    let written = rewrite(&Database::open(fixture("pdb/num_rows/export.pdb")).unwrap());
    let mut data_pages = 0;
    for table in written.tables() {
        for page in written.table_pages(table).unwrap() {
            if page.is_data_page() {
                data_pages += 1;
                let rows = usize::from(page.num_rows);
                assert_eq!(page.rows().unwrap().len(), rows);
                assert_eq!(page.num_row_offsets, page.num_rows);
                let index = rows.div_ceil(16) * 4 + rows * 2;
                assert_eq!(
                    usize::from(page.free_size) + usize::from(page.used_size) + index,
                    4096 - 0x28
                );
            }
        }
    }
    assert!(data_pages > 100, "expected a multi-page database, got {data_pages} data pages");
}

/// An independent reader (rekordcrate) agrees with what we write.
#[test]
fn rekordcrate_reads_written_database() {
    use binrw::BinRead;
    use rekordcrate::pdb::{Header, PageType, Row};

    let written =
        DatabaseWriter::from_database(&Database::open(fixture("pdb/num_rows/export.pdb")).unwrap())
            .unwrap()
            .to_bytes();
    let ours = Database::from_bytes(written.clone(), Schema::Export).unwrap();
    let mut cursor = std::io::Cursor::new(written);
    let header = Header::read(&mut cursor).unwrap();
    for (theirs, table) in header.tables.iter().zip(ours.tables()) {
        if matches!(theirs.page_type, PageType::Unknown(_)) {
            continue;
        }
        let their_pages = header
            .read_pages(&mut cursor, binrw::Endian::NATIVE, (&theirs.first_page, &theirs.last_page))
            .unwrap();
        let our_pages = ours.table_pages(table).unwrap();
        assert_eq!(their_pages.len(), our_pages.len(), "{:?}", table.table_type);
        for (their_page, our_page) in their_pages.into_iter().zip(our_pages) {
            // rekordcrate 0.3.0 miscounts pages with more than 255 rows (see tests/pdb.rs).
            if our_page.num_row_offsets > 255 {
                continue;
            }
            let rows: Vec<Row> = their_page
                .row_groups
                .into_iter()
                .flat_map(|g| g.present_rows().collect::<Vec<_>>())
                .collect();
            assert_eq!(rows.len(), our_page.rows().unwrap().len(), "{:?}", table.table_type);
        }
    }
}

/// Strings in every encoding, and rows that need the long forms, survive a round trip.
#[test]
fn edge_case_rows_round_trip() {
    let long_ascii = "x".repeat(200);
    let track = Track {
        id: 7,
        title: "Ünïcödé — 東京".to_owned(),
        artist_id: Some(3),
        composer_id: None,
        original_artist_id: None,
        remixer_id: None,
        album_id: Some(4),
        genre_id: None,
        label_id: None,
        key_id: None,
        artwork_id: None,
        color_id: Some(2),
        rating: 5,
        tempo: 12_345,
        duration_secs: 300,
        sample_rate: 44_100,
        sample_depth: 16,
        bitrate: 320,
        file_size: 1 << 24,
        track_number: 1,
        disc_number: 1,
        year: 2024,
        play_count: 0,
        isrc: "GBAYE6700149".to_owned(),
        mix_name: String::new(),
        comment: long_ascii.clone(),
        message: String::new(),
        date_added: "2026-10-25".to_owned(),
        release_date: String::new(),
        analyze_path: "/PIONEER/USBANLZ/P000/00000001/ANLZ0000.DAT".to_owned(),
        analyze_date: "2026-10-25".to_owned(),
        filename: "track.flac".to_owned(),
        file_path: "/Contents/Ünïcödé/track.flac".to_owned(),
        kuvo_public: false,
        autoload_hot_cues: true,
        extras: TrackExtras::default(),
    };
    let artist = Artist { id: 3, name: "Röyksopp".to_owned() };
    let album = Album { id: 4, artist_id: Some(3), name: "東京".to_owned() };

    let mut writer = DatabaseWriter::new_export("2026-10-25");
    writer.push(&track);
    writer.push(&artist);
    writer.push(&album);
    // Enough rows to fill several pages and more than one row group per page.
    for id in 10..2_000 {
        writer.push(&Genre { id, name: format!("Genre {id}") });
    }
    let db = Database::from_bytes(writer.to_bytes(), Schema::Export).unwrap();
    assert_eq!(db.read_all::<Track>().unwrap(), [track]);
    assert_eq!(db.read_all::<Artist>().unwrap(), [artist]);
    assert_eq!(db.read_all::<Album>().unwrap(), [album]);
    let genres = db.read_all::<Genre>().unwrap();
    assert_eq!(genres.len(), 1_990);
    assert!(genres.iter().zip(10..).all(|(g, id)| g.id == id && g.name == format!("Genre {id}")));
    assert!(db.table_pages(db.table(TableType::Genres).unwrap()).unwrap().len() > 3);
}

/// Playlist edits on a real export (`DECKHAND_TEST_PDB`, edited as a copy;
/// skipped without it) keep every other row and read back as expected.
#[test]
fn playlist_edits_on_a_real_export() {
    use deckhand_formats::pdb::{Library, PlaylistTables, TableType, edit_playlists};
    let Some(original) = std::env::var_os("DECKHAND_TEST_PDB") else { return };
    let dir = std::env::temp_dir().join(format!("deckhand-pdb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pdb = dir.join("export.pdb");
    std::fs::copy(&original, &pdb).unwrap();
    let before = Database::open(&pdb).unwrap();
    let tables_before = PlaylistTables::read(&before).unwrap();
    let tracks: Vec<u32> = Library::from_database(&before)
        .unwrap()
        .tracks
        .iter()
        .take(3)
        .map(|t| t.track.id)
        .collect();
    let (renamed, deleted) = {
        let mut ids = tables_before.nodes.iter().filter(|n| !n.is_folder).map(|n| n.id);
        (ids.next().unwrap(), ids.next().unwrap())
    };

    let new = edit_playlists(&pdb, |t| {
        let new = t.create("RofLoxBox test", false, None);
        t.add_tracks(new, &tracks);
        t.rename(renamed, "Renamed by RofLoxBox");
        t.delete(deleted);
        new
    })
    .unwrap();
    assert!(deckhand_formats::pdb::backup_path(&pdb).is_file(), "backup made");

    let after = Database::open(&pdb).unwrap();
    // Every table but the playlist ones keeps its rows: the same values for
    // the tables deckhand understands (rekordbox leaves stale bytes after
    // some rows, which are not copied), the same bytes for the others.
    fn same<T: deckhand_formats::pdb::FromRow + PartialEq + std::fmt::Debug>(
        a: &Database,
        b: &Database,
    ) {
        let (x, y): (Vec<T>, Vec<T>) = (a.read_all().unwrap(), b.read_all().unwrap());
        assert_eq!(x.len(), y.len(), "{:?}", T::TABLE);
        for (x, y) in x.iter().zip(&y) {
            assert_eq!(x, y);
        }
    }
    use deckhand_formats::pdb::{
        Album, Artist, Artwork, Color, ColumnEntry, Genre, HistoryEntry, HistoryPlaylist, Key,
        Label, Track,
    };
    same::<Track>(&before, &after);
    same::<Artist>(&before, &after);
    same::<Album>(&before, &after);
    same::<Genre>(&before, &after);
    same::<Label>(&before, &after);
    same::<Key>(&before, &after);
    same::<Color>(&before, &after);
    same::<Artwork>(&before, &after);
    same::<ColumnEntry>(&before, &after);
    same::<HistoryPlaylist>(&before, &after);
    same::<HistoryEntry>(&before, &after);
    for table in before.tables() {
        if matches!(table.table_type, TableType::History | TableType::Unknown(_)) {
            let other = after.table(table.table_type).unwrap();
            assert_eq!(before.raw_rows(table).unwrap(), after.raw_rows(other).unwrap());
        }
    }
    let t = PlaylistTables::read(&after).unwrap();
    assert_eq!(t.tracks(new), tracks);
    assert_eq!(t.nodes.iter().find(|n| n.id == renamed).unwrap().name, "Renamed by RofLoxBox");
    assert!(t.nodes.iter().all(|n| n.id != deleted));
    assert_eq!(t.nodes.len(), tables_before.nodes.len());
    let library = Library::from_database(&after).unwrap();
    assert!(library.playlists.iter().any(|p| p.name == "RofLoxBox test"));

    // rekordcrate reads it too.
    {
        use binrw::BinRead;
        use rekordcrate::pdb::{Header, PageType};
        let mut cursor = std::io::Cursor::new(std::fs::read(&pdb).unwrap());
        let header = Header::read(&mut cursor).unwrap();
        for theirs in &header.tables {
            if matches!(theirs.page_type, PageType::Unknown(_)) {
                continue;
            }
            header
                .read_pages(
                    &mut cursor,
                    binrw::Endian::NATIVE,
                    (&theirs.first_page, &theirs.last_page),
                )
                .unwrap();
        }
    }
    // Tracks no playlist uses go; everything else stays consistent.
    {
        use deckhand_formats::pdb::{Track, remove_tracks};
        // Leave one playlist, so that tracks become unused.
        edit_playlists(&pdb, |t| {
            let ids: Vec<u32> = t.nodes.iter().map(|n| n.id).collect();
            for id in ids.into_iter().skip(1) {
                t.delete(id);
            }
        })
        .unwrap();
        let db = Database::open(&pdb).unwrap();
        let tables = PlaylistTables::read(&db).unwrap();
        let tracks: Vec<Track> = db.read_all().unwrap();
        let unused: Vec<u32> = tracks
            .iter()
            .map(|t| t.id)
            .filter(|id| tables.entries.iter().all(|e| e.track_id != *id))
            .collect();
        let removed = remove_tracks(&pdb, &unused).unwrap();
        assert_eq!(removed.len(), unused.len());
        let db = Database::open(&pdb).unwrap();
        let left: Vec<Track> = db.read_all().unwrap();
        assert_eq!(left.len(), tracks.len() - unused.len());
        let tables = PlaylistTables::read(&db).unwrap();
        assert!(tables.entries.iter().all(|e| left.iter().any(|t| t.id == e.track_id)));
        println!("removed {} of {} tracks", unused.len(), tracks.len());
        assert!(!unused.is_empty());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
