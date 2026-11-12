#![allow(clippy::unwrap_used)] // tests: helpers outside #[test] functions too
use std::path::{Path, PathBuf};

use deckhand_formats::anlz::{
    AnlzFile, CueKind, CueListKind, CueStatus, HotCueColor, Mood, Section, WaveformKind,
};
use deckhand_formats::pdb::Library;

fn export_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/complete_export/demo_tracks")
}

fn demo_track_1(extension: &str) -> AnlzFile {
    let path =
        export_root().join("PIONEER/USBANLZ/P016/0000875E/ANLZ0000").with_extension(extension);
    AnlzFile::open(path).unwrap()
}

#[test]
fn dat_file() {
    let file = demo_track_1("DAT");
    assert_eq!(file.path(), Some("/Contents/Loopmasters/UnknownAlbum/Demo Track 1.mp3"));

    let grid = file.beat_grid().unwrap();
    assert_eq!(grid.beats.len(), 368);
    assert_eq!(grid.beats[0].time_ms, 25);
    assert!(grid.beats.iter().all(|beat| beat.bpm() == 128.0));
    assert!(grid.beats.windows(2).all(|pair| pair[0].time_ms < pair[1].time_ms));
    assert!(grid.beats.iter().all(|beat| (1..=4).contains(&beat.bar_position)));

    assert_eq!(file.waveform(WaveformKind::Preview).unwrap().column_count(), 400);
    assert_eq!(file.waveform(WaveformKind::TinyPreview).unwrap().column_count(), 100);
    assert!(file.sections.iter().any(|s| matches!(s, Section::Vbr(index) if index.len() == 401)));

    let kinds: Vec<CueListKind> = file.cue_lists().map(|list| list.kind).collect();
    assert_eq!(kinds, [CueListKind::Hot, CueListKind::Memory]);
    assert!(file.cue_lists().all(|list| list.cues.is_empty() && !list.extended));
}

#[test]
fn ext_file() {
    let file = demo_track_1("EXT");
    let detail = file.waveform(WaveformKind::Detail).unwrap();
    assert_eq!((detail.column_count(), detail.entry_len), (25866, 1));
    let color = file.waveform(WaveformKind::ColorDetail).unwrap();
    assert_eq!((color.column_count(), color.entry_len), (25866, 2));
    let preview = file.waveform(WaveformKind::ColorPreview).unwrap();
    assert_eq!((preview.column_count(), preview.entry_len), (1200, 6));
    assert_eq!(file.cue_lists().filter(|list| list.extended).count(), 2);

    let structure = file.song_structure().unwrap();
    assert!(structure.masked);
    assert_eq!(structure.mood, Mood::Mid);
    assert_eq!(structure.end_beat, 353);
    assert_eq!(structure.phrases.len(), 19);
    for (i, phrase) in structure.phrases.iter().enumerate() {
        assert_eq!(usize::from(phrase.index), i + 1);
        assert!(phrase.beat < structure.end_beat);
        assert_ne!(phrase.label(structure.mood), "Unknown");
    }
    assert!(structure.phrases.windows(2).all(|pair| pair[0].beat < pair[1].beat));
}

#[test]
fn two_ex_file() {
    let file = demo_track_1("2EX");
    let preview = file.waveform(WaveformKind::ThreeBandPreview).unwrap();
    assert_eq!((preview.column_count(), preview.entry_len), (1200, 3));
    let detail = file.waveform(WaveformKind::ThreeBandDetail).unwrap();
    assert_eq!((detail.column_count(), detail.entry_len), (25866, 3));
}

/// Every track's analysis files exist where `export.pdb` says, and name that track.
#[test]
fn analysis_files_match_database() {
    let root = export_root();
    let library = Library::open(root.join("PIONEER/rekordbox/export.pdb")).unwrap();
    for t in &library.tracks {
        let dat = root.join(t.track.analyze_path.trim_start_matches('/'));
        for extension in ["DAT", "EXT", "2EX"] {
            let file = AnlzFile::open(dat.with_extension(extension)).unwrap();
            assert_eq!(file.path(), Some(t.track.file_path.as_str()), "{}", dat.display());
        }
        let beats = AnlzFile::open(&dat).unwrap().beat_grid().unwrap().beats.len() as f64;
        let expected = f64::from(t.track.duration_secs) * t.track.bpm() / 60.0;
        assert!(
            (beats - expected).abs() < 4.0,
            "{}: {beats} beats, expected ~{expected}",
            t.track.title
        );
    }
}

/// `PQT2` repeats the first and last beat and the beat count of `PQTZ`.
#[test]
fn extended_beat_grid_matches_beat_grid() {
    for dir in ["P016/0000875E", "P053/0001D21F"] {
        let base = export_root().join("PIONEER/USBANLZ").join(dir).join("ANLZ0000");
        let grid = AnlzFile::open(base.with_extension("DAT")).unwrap().beat_grid().unwrap().clone();
        let ext = AnlzFile::open(base.with_extension("EXT")).unwrap();
        let extended = ext
            .sections
            .iter()
            .find_map(|s| match s {
                Section::ExtendedBeatGrid(grid) => Some(grid.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(extended.first_beat, grid.beats[0], "{dir}");
        assert_eq!(extended.last_beat, *grid.beats.last().unwrap(), "{dir}");
        assert_eq!(extended.beat_count as usize, grid.beats.len(), "{dir}");
        assert_eq!(extended.entries.len(), grid.beats.len(), "{dir}");
    }
}

// Hand-built sections, following the crate-digger format description.

fn section(tag: &[u8; 4], header: &[u8], payload: &[u8]) -> Vec<u8> {
    let header_len = 12 + header.len();
    let mut out = tag.to_vec();
    out.extend((header_len as u32).to_be_bytes());
    out.extend(((header_len + payload.len()) as u32).to_be_bytes());
    out.extend(header);
    out.extend(payload);
    out
}

fn file(sections: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = sections.concat();
    let mut out = b"PMAI".to_vec();
    out.extend(28u32.to_be_bytes());
    out.extend((28 + body.len() as u32).to_be_bytes());
    out.extend([0; 16]);
    out.extend(body);
    out
}

fn basic_cue(hot_cue: u32, status: u32, kind: u8, time: u32, loop_time: u32) -> Vec<u8> {
    let mut entry = b"PCPT".to_vec();
    for value in [0x1c, 0x38, hot_cue, status, 0x10000] {
        entry.extend(u32::to_be_bytes(value));
    }
    entry.extend([0xff, 0xff, 0x00, 0x01, kind, 0, 0x03, 0xe8]);
    entry.extend(time.to_be_bytes());
    entry.extend(loop_time.to_be_bytes());
    entry.extend([0; 16]);
    entry
}

fn extended_cue(
    hot_cue: u32,
    kind: u8,
    time: u32,
    loop_time: u32,
    comment: &str,
    color: [u8; 4],
) -> Vec<u8> {
    let comment: Vec<u8> = comment.encode_utf16().chain([0]).flat_map(u16::to_be_bytes).collect();
    let len_entry = 0x2c + comment.len() + 4 + 8;
    let mut entry = b"PCP2".to_vec();
    entry.extend(0x10u32.to_be_bytes());
    entry.extend((len_entry as u32).to_be_bytes());
    entry.extend(hot_cue.to_be_bytes());
    entry.extend([kind, 0, 0x03, 0xe8]);
    entry.extend(time.to_be_bytes());
    entry.extend(loop_time.to_be_bytes());
    entry.extend([5, 0, 0, 0, 0, 0, 0, 0]); // color id 5, padding
    entry.extend([0, 4, 0, 1]); // loop of 4/1 beats
    entry.extend((comment.len() as u32).to_be_bytes());
    entry.extend(comment);
    entry.extend(color);
    entry.extend([0; 8]);
    entry
}

#[test]
fn basic_cue_list() {
    let entries = [basic_cue(1, 1, 1, 1000, u32::MAX), basic_cue(2, 4, 2, 2000, 4000)].concat();
    let mut header = 1u32.to_be_bytes().to_vec(); // hot cues
    header.extend([0, 0, 0, 2, 0, 0, 0, 0]);
    let parsed = AnlzFile::from_bytes(&file(&[section(b"PCOB", &header, &entries)])).unwrap();

    let list = parsed.cue_lists().next().unwrap();
    assert_eq!(list.kind, CueListKind::Hot);
    assert!(!list.extended);
    let [a, b] = &list.cues[..] else { panic!("expected two cues") };
    assert_eq!(a.hot_cue_letter(), Some('A'));
    assert_eq!(
        (a.kind, a.time_ms, a.loop_end_ms, a.status),
        (CueKind::Cue, 1000, None, Some(CueStatus::Enabled))
    );
    assert_eq!(b.hot_cue_letter(), Some('B'));
    assert_eq!((b.kind, b.time_ms, b.loop_end_ms), (CueKind::Loop, 2000, Some(4000)));
    assert_eq!(b.status, Some(CueStatus::ActiveLoop));
    assert_eq!(AnlzFile::from_bytes(&parsed.to_bytes()).unwrap(), parsed);
}

#[test]
fn extended_cue_list() {
    let entries = [
        extended_cue(3, 1, 1500, u32::MAX, "Drop", [0x2a, 0xff, 0x80, 0x00]),
        extended_cue(0, 2, 8000, 9875, "", [0; 4]),
    ]
    .concat();
    let mut header = 0u32.to_be_bytes().to_vec(); // memory cues
    header.extend([0, 2, 0, 0]);
    let parsed = AnlzFile::from_bytes(&file(&[section(b"PCO2", &header, &entries)])).unwrap();

    let list = parsed.cue_lists().next().unwrap();
    assert!(list.extended);
    assert_eq!(list.kind, CueListKind::Memory);
    let [hot, memory] = &list.cues[..] else { panic!("expected two cues") };
    assert_eq!(hot.hot_cue_letter(), Some('C'));
    assert_eq!(hot.comment.as_deref(), Some("Drop"));
    assert_eq!(hot.hot_cue_color, Some(HotCueColor { code: 0x2a, rgb: [0xff, 0x80, 0x00] }));
    assert_eq!(hot.color_id, Some(5));
    assert_eq!(memory.hot_cue, None);
    assert_eq!(
        (memory.kind, memory.loop_end_ms, memory.loop_beats),
        (CueKind::Loop, Some(9875), Some((4, 1)))
    );
    // An empty comment is stored as a lone NUL and kept, so it can be written back.
    assert_eq!((memory.comment.as_deref(), memory.hot_cue_color), (Some(""), None));
    assert_eq!(AnlzFile::from_bytes(&parsed.to_bytes()).unwrap(), parsed);
}

#[test]
fn masked_and_plain_song_structure_agree() {
    const MASK: [u8; 19] = [
        0xcb, 0xe1, 0xee, 0xfa, 0xe5, 0xee, 0xad, 0xee, 0xe9, 0xd2, 0xe9, 0xeb, 0xe1, 0xe9, 0xf3,
        0xe8, 0xe9, 0xf4, 0xe1,
    ];
    let count = 2u16;
    let mut body = vec![0, 3, 0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 2, 0]; // low mood, end beat 64, bank 2
    for (index, beat, kind) in [(1u16, 1u16, 1u16), (2, 33, 9)] {
        let mut entry = [0u8; 24];
        entry[0..2].copy_from_slice(&index.to_be_bytes());
        entry[2..4].copy_from_slice(&beat.to_be_bytes());
        entry[4..6].copy_from_slice(&kind.to_be_bytes());
        body.extend(entry);
    }
    let masked_body: Vec<u8> = body
        .iter()
        .zip(MASK.iter().cycle())
        .map(|(b, m)| b ^ m.wrapping_add(count as u8))
        .collect();

    let parse = |body: &[u8]| {
        let mut header = 24u32.to_be_bytes().to_vec();
        header.extend(count.to_be_bytes());
        // The body starts inside the section header area at offset 0x12.
        let tagged = section(b"PSSI", &header, body);
        AnlzFile::from_bytes(&file(&[tagged])).unwrap().song_structure().unwrap().clone()
    };
    let plain = parse(&body);
    let masked = parse(&masked_body);
    assert!(!plain.masked && masked.masked);
    assert_eq!((plain.mood, plain.end_beat, plain.bank), (Mood::Low, 64, 2));
    assert_eq!(plain.phrases, masked.phrases);
    let labels: Vec<&str> = plain.phrases.iter().map(|p| p.label(plain.mood)).collect();
    assert_eq!(labels, ["Intro", "Chorus"]);
    for structure in [plain, masked] {
        let file = AnlzFile { sections: vec![Section::SongStructure(structure)] };
        assert_eq!(AnlzFile::from_bytes(&file.to_bytes()).unwrap(), file);
    }
}

#[test]
fn unknown_sections_are_kept() {
    let parsed = AnlzFile::from_bytes(&file(&[section(b"PXYZ", &[1, 2], &[3, 4, 5])])).unwrap();
    let Section::Unknown { tag, bytes } = &parsed.sections[0] else { panic!("expected unknown") };
    assert_eq!(tag, b"PXYZ");
    assert_eq!(bytes.len(), 17);
}

#[test]
fn corrupt_input_does_not_panic() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/complete_export/demo_tracks/PIONEER/USBANLZ/P016/0000875E");
    for extension in ["DAT", "EXT", "2EX"] {
        let original = std::fs::read(dir.join("ANLZ0000").with_extension(extension)).unwrap();
        for len in (0..original.len()).step_by(211) {
            let _ = AnlzFile::from_bytes(&original[..len]);
        }
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..300 {
            let mut bytes = original.clone();
            for _ in 0..8 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let at = (state as usize) % bytes.len();
                bytes[at] = (state >> 40) as u8;
            }
            let _ = AnlzFile::from_bytes(&bytes);
        }
    }
}
