//! Reading a Serato DJ library, as its formats are publicly described
//! (Mixxx, the serato-tags write-ups): `_Serato_/database V2` lists the
//! tracks with their tags, `_Serato_/Subcrates/*.crate` the crates (a
//! crate inside another is named `Parent%%Child`). Hot cues, saved loops
//! and beat grids aren't in the library but in each audio file's tags
//! ("Serato Markers2", "Serato BeatGrid").
//!
//! Both library files are a run of fields: a four-letter tag, a big-endian
//! length, then the value, which the tag's first letter types: `o` holds
//! more fields, `t` and `p` are UTF-16 (big-endian) text, `u` a 32-bit
//! number, `s` a 16-bit one, `b` a byte.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;

use crate::collection::{Cue, CueKind, TempoMarker};
use crate::rekordbox_xml::{XmlLibrary, XmlNode, XmlTrack};
use crate::scan::{ScannedFile, file_stamp, read_file};

/// Serato's libraries on this computer: in the Music folder and at the top
/// of each drive (Serato keeps one per drive for the music on it).
pub fn find() -> Vec<PathBuf> {
    let mut places = Vec::new();
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    if let Some(home) = home {
        places.push(PathBuf::from(home).join("Music"));
    }
    if cfg!(windows) {
        places.extend((b'C'..=b'Z').map(|d| PathBuf::from(format!("{}:\\", d as char))));
    } else if let Ok(volumes) = std::fs::read_dir("/Volumes") {
        places.extend(volumes.flatten().map(|v| v.path()));
    }
    places
        .into_iter()
        .map(|p| p.join("_Serato_"))
        .filter(|p| p.join("database V2").is_file())
        .collect()
}

/// The `_Serato_` folder `picked` is, or holds.
pub fn library_in(picked: &Path) -> Option<PathBuf> {
    [picked.to_path_buf(), picked.join("_Serato_")]
        .into_iter()
        .find(|p| p.join("database V2").is_file())
}

/// Reads the library in `serato` (a `_Serato_` folder): its tracks, with
/// their cues and grids from the files that are there, and its crates.
pub fn open(serato: &Path) -> Result<XmlLibrary> {
    let database = serato.join("database V2");
    let bytes =
        std::fs::read(&database).with_context(|| format!("reading {}", database.display()))?;
    let root = volume_root(serato);
    let mut tracks = Vec::new();
    let mut known = HashMap::new();
    for (tag, value) in fields(&bytes)? {
        if &tag != b"otrk" {
            continue;
        }
        let Some(track) = database_track(value, &root) else { continue };
        known.insert(track.id.clone(), tracks.len());
        tracks.push(track);
    }

    // Crates by name (`A%%B` is B inside A), sorted so parents come first.
    let mut crates: BTreeMap<Vec<String>, Vec<String>> = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(serato.join("Subcrates")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("crate") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(found) = fields(&bytes) else { continue };
            let mut keys = Vec::new();
            for (tag, value) in found {
                if &tag != b"otrk" {
                    continue;
                }
                let Some(file) = fields(value)
                    .ok()
                    .and_then(|f| f.into_iter().find(|(t, _)| t == b"ptrk"))
                    .map(|(_, v)| utf16(v))
                else {
                    continue;
                };
                let path = resolve(&root, &file);
                let key = path.to_string_lossy().into_owned();
                // A crate's track that the database doesn't list.
                if !known.contains_key(&key) {
                    known.insert(key.clone(), tracks.len());
                    tracks.push(file_track(path));
                }
                keys.push(key);
            }
            let name = stem.split("%%").map(str::to_owned).collect();
            crates.insert(name, keys);
        }
    }
    if tracks.is_empty() && crates.is_empty() {
        bail!("{} lists no tracks", database.display());
    }
    // Cues and grids from the files themselves.
    for track in &mut tracks {
        if track.file.path.is_file() {
            let (cues, tempo) = markers(&track.file.path);
            (track.cues, track.tempo) = (cues, tempo);
        }
    }
    Ok(XmlLibrary { tracks, playlists: crate_tree(&crates, &[]) })
}

/// The crates under `parent` as nodes: a crate with crates in it becomes a
/// folder (with a playlist of its own tracks first, when it has any).
fn crate_tree(crates: &BTreeMap<Vec<String>, Vec<String>>, parent: &[String]) -> Vec<XmlNode> {
    let mut nodes = Vec::new();
    for (name, keys) in crates {
        if name.len() != parent.len() + 1 || !name.starts_with(parent) {
            continue;
        }
        let own = name.last().cloned().unwrap_or_default();
        let playlist = |keys: &Vec<String>| XmlNode::Playlist {
            name: own.clone(),
            keys: keys.clone(),
            by_location: false,
            id: None,
        };
        let children = crate_tree(crates, name);
        if children.is_empty() {
            nodes.push(playlist(keys));
        } else {
            let mut inside = Vec::new();
            if !keys.is_empty() {
                inside.push(playlist(keys));
            }
            inside.extend(children);
            nodes.push(XmlNode::Folder { name: own, children: inside, id: None });
        }
    }
    // A crate inside one that has no file of its own (`A%%B` without `A`).
    let mut missing: Vec<&String> = crates
        .keys()
        .filter(|n| n.len() > parent.len() + 1 && n.starts_with(parent))
        .map(|n| &n[parent.len()])
        .filter(|first| {
            let mut own = parent.to_vec();
            own.push((*first).clone());
            !crates.contains_key(&own)
        })
        .collect();
    missing.dedup();
    for first in missing {
        let mut own = parent.to_vec();
        own.push(first.clone());
        nodes.push(XmlNode::Folder { name: first.clone(), children: crate_tree(crates, &own), id: None });
    }
    nodes
}

/// One `otrk` of the database: the file's tags as Serato has them; for
/// files that are here, what the file itself says fills the rest.
fn database_track(value: &[u8], root: &Path) -> Option<XmlTrack> {
    let mut text: HashMap<[u8; 4], String> = HashMap::new();
    let mut numbers: HashMap<[u8; 4], u32> = HashMap::new();
    for (tag, value) in fields(value).ok()? {
        match tag[0] {
            b't' | b'p' => {
                text.insert(tag, utf16(value).trim().to_owned());
            }
            b'u' if value.len() == 4 => {
                numbers.insert(tag, u32::from_be_bytes(value.try_into().ok()?));
            }
            _ => {}
        }
    }
    let path = resolve(root, text.get(b"pfil")?);
    let get = |tag: &[u8; 4]| text.get(tag).filter(|v| !v.is_empty()).cloned();
    let mut track = file_track(path);
    let file = &mut track.file;
    if let Some(title) = get(b"tsng") {
        file.title = title;
    }
    for (tag, field) in [
        (b"tart", &mut file.artist),
        (b"talb", &mut file.album),
        (b"tgen", &mut file.genre),
        (b"tlbl", &mut file.label),
        (b"tcom", &mut file.comment),
    ] {
        if let Some(value) = get(tag) {
            *field = value;
        }
    }
    if let Some(bpm) = get(b"tbpm").and_then(|b| b.replace(',', ".").parse::<f64>().ok()) {
        file.bpm = Some(bpm).filter(|b| (20.0..=400.0).contains(b)).or(file.bpm);
    }
    file.key = get(b"tkey").or(file.key.take());
    if file.year.is_none() {
        file.year = get(b"ttyr").and_then(|y| y.get(..4)?.parse().ok());
    }
    if file.duration <= 0.0 {
        file.duration = get(b"tlen").map_or(0.0, |l| duration(&l));
    }
    track.added = numbers.get(b"uadd").map(|&s| u64::from(s)).filter(|&s| s > 0);
    Some(track)
}

/// A track known only by its file: its tags when it is here.
fn file_track(path: PathBuf) -> XmlTrack {
    let file = read_file(&path).unwrap_or_else(|_| {
        let (file_size, modified) = file_stamp(&path);
        ScannedFile {
            title: path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
            path: path.clone(),
            file_size,
            modified,
            ..ScannedFile::default()
        }
    });
    XmlTrack {
        id: path.to_string_lossy().into_owned(),
        location: path.to_string_lossy().into_owned(),
        file,
        // Serato has no ratings: the collection's stay.
        rating: None,
        ..XmlTrack::default()
    }
}

/// `"05:12.34"` (or `"312"`) as seconds.
fn duration(text: &str) -> f64 {
    text.split(':').fold(0.0, |total, part| total * 60.0 + part.trim().parse::<f64>().unwrap_or(0.0))
}

/// Where the paths of the library in `serato` start: the top of the drive
/// it is on (Serato writes them from there, without it).
fn volume_root(serato: &Path) -> PathBuf {
    let mut components = serato.components();
    match components.next() {
        Some(std::path::Component::Prefix(prefix)) => {
            PathBuf::from(prefix.as_os_str()).join(std::path::MAIN_SEPARATOR_STR)
        }
        _ => {
            // macOS drives are under /Volumes; anything else is on `/`.
            let parts: Vec<_> = serato.iter().take(3).collect();
            if parts.len() == 3 && parts[1] == "Volumes" {
                parts.iter().collect()
            } else {
                PathBuf::from("/")
            }
        }
    }
}

/// A library path (`Users/me/Music/a.mp3`, with `/` or `\`) under `root`.
fn resolve(root: &Path, file: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    path.extend(file.split(['/', '\\']).filter(|p| !p.is_empty()));
    path
}

/// The fields of a library file: (tag, value).
fn fields(mut bytes: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    while bytes.len() >= 8 {
        let tag: [u8; 4] = bytes[..4].try_into()?;
        let len = u32::from_be_bytes(bytes[4..8].try_into()?) as usize;
        let Some(value) = bytes.get(8..8 + len) else {
            bail!("a Serato library field runs past the end of the file");
        };
        out.push((tag, value));
        bytes = &bytes[8 + len..];
    }
    Ok(out)
}

fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_owned()
}

/// A file's Serato cues and loops, and its beat grid.
fn markers(path: &Path) -> (Vec<Cue>, Vec<TempoMarker>) {
    let (markers, grid) = serato_tags(path).unwrap_or_default();
    let cues = markers.map(|m| parse_markers(&m)).unwrap_or_default();
    let tempo = grid.and_then(|g| parse_beat_grid(&g)).unwrap_or_default();
    (cues, tempo)
}

/// The raw "Serato Markers2" and "Serato BeatGrid" of a file: ID3 GEOB
/// frames (MP3, AIFF, WAV), or Vorbis comments and MP4 items holding the
/// same, wrapped (FLAC, Ogg, M4A).
fn serato_tags(path: &Path) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>)> {
    use lofty::config::ParseOptions;
    use lofty::file::{AudioFile, FileType};
    use lofty::id3::v2::{Frame, FrameFlags, GeneralEncapsulatedObject, Id3v2Tag};

    let geob = |tag: Option<&Id3v2Tag>, name: &str| -> Option<Vec<u8>> {
        tag?.into_iter().find_map(|frame| match frame {
            Frame::Binary(binary) if frame.id_str() == "GEOB" => {
                let object = GeneralEncapsulatedObject::parse(&binary.data, FrameFlags::default()).ok()?;
                (object.descriptor.as_deref() == Some(name)).then_some(object.data)
            }
            _ => None,
        })
    };
    let wrapped = |text: Option<&str>| -> Option<Vec<u8>> { unwrap(&decode(text?.as_bytes())?) };
    let options = ParseOptions::new().read_properties(false);
    let mut file = std::fs::File::open(path)?;
    let Some(kind) = FileType::from_path(path) else { return Ok((None, None)) };
    Ok(match kind {
        FileType::Mpeg => {
            let f = lofty::mpeg::MpegFile::read_from(&mut file, options)?;
            (geob(f.id3v2(), "Serato Markers2"), geob(f.id3v2(), "Serato BeatGrid"))
        }
        FileType::Aiff => {
            let f = lofty::iff::aiff::AiffFile::read_from(&mut file, options)?;
            (geob(f.id3v2(), "Serato Markers2"), geob(f.id3v2(), "Serato BeatGrid"))
        }
        FileType::Wav => {
            let f = lofty::iff::wav::WavFile::read_from(&mut file, options)?;
            (geob(f.id3v2(), "Serato Markers2"), geob(f.id3v2(), "Serato BeatGrid"))
        }
        FileType::Flac => {
            let f = lofty::flac::FlacFile::read_from(&mut file, options)?;
            let tag = f.vorbis_comments();
            (
                wrapped(tag.and_then(|t| t.get("SERATO_MARKERS_V2"))),
                wrapped(tag.and_then(|t| t.get("SERATO_BEATGRID"))),
            )
        }
        FileType::Vorbis => {
            let f = lofty::ogg::VorbisFile::read_from(&mut file, options)?;
            let tag = Some(f.vorbis_comments());
            (
                wrapped(tag.and_then(|t| t.get("SERATO_MARKERS_V2"))),
                wrapped(tag.and_then(|t| t.get("SERATO_BEATGRID"))),
            )
        }
        FileType::Mp4 => {
            use lofty::mp4::{AtomData, AtomIdent};
            let f = lofty::mp4::Mp4File::read_from(&mut file, options)?;
            let item = |name: &str| {
                let ident = AtomIdent::Freeform { mean: "com.serato.dj".into(), name: name.into() };
                f.ilst()?.get(&ident)?.data().find_map(|d| match d {
                    AtomData::UTF8(text) => wrapped(Some(text)),
                    AtomData::Unknown { data, .. } => unwrap(&decode(data)?),
                    _ => None,
                })
            };
            (item("markersv2"), item("beatgrid"))
        }
        _ => (None, None),
    })
}

/// Base64 as Serato writes it: in lines, often without its padding.
fn decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut clean: Vec<u8> =
        text.iter().copied().filter(|c| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/').collect();
    // A lone character past whole groups carries no whole byte.
    if clean.len() % 4 == 1 {
        clean.pop();
    }
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::RequireNone)
            .with_decode_allow_trailing_bits(true),
    );
    engine.decode(&clean).ok()
}

/// What a Vorbis comment or MP4 item holds: the GEOB's fields in a row
/// (type, file name and description, each ending in a NUL) then its data.
fn unwrap(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut rest = bytes;
    if rest.starts_with(b"application/octet-stream\0") {
        for _ in 0..3 {
            let end = rest.iter().position(|&b| b == 0)?;
            rest = &rest[end + 1..];
        }
    }
    Some(rest.to_vec())
}

/// "Serato Markers2": version bytes, then base64 of a run of entries
/// (a NUL-ended name, a big-endian length, the data). CUE entries are hot
/// cues, LOOP entries saved loops (memory loops here).
fn parse_markers(data: &[u8]) -> Vec<Cue> {
    let Some(body) = data.get(2..).and_then(decode) else { return Vec::new() };
    let mut rest = body.get(2..).unwrap_or_default();
    let mut cues = Vec::new();
    let be32 = |b: &[u8], at: usize| b.get(at..at + 4).and_then(|x| x.try_into().ok()).map(u32::from_be_bytes);
    let name_at = |b: &[u8], at: usize| {
        let text = b.get(at..).unwrap_or_default();
        let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        String::from_utf8_lossy(&text[..end]).into_owned()
    };
    while let Some(end) = rest.iter().position(|&b| b == 0).filter(|&e| e > 0) {
        let name = &rest[..end];
        let Some(len) = be32(rest, end + 1) else { break };
        let Some(entry) = rest.get(end + 5..end + 5 + len as usize) else { break };
        rest = &rest[end + 5 + len as usize..];
        match name {
            b"CUE" if entry.len() >= 12 => {
                let index = entry[1];
                let Some(at) = be32(entry, 2) else { continue };
                if usize::from(index) < 8 {
                    cues.push(Cue {
                        kind: CueKind::Hot(index),
                        start: f64::from(at) / 1000.0,
                        end: None,
                        color: Some([entry[7], entry[8], entry[9]]),
                        name: name_at(entry, 12),
                        active: false,
                        loop_beats: None,
                    });
                }
            }
            b"LOOP" if entry.len() >= 20 => {
                let (Some(start), Some(stop)) = (be32(entry, 2), be32(entry, 6)) else { continue };
                if stop > start {
                    cues.push(Cue {
                        kind: CueKind::Memory,
                        start: f64::from(start) / 1000.0,
                        end: Some(f64::from(stop) / 1000.0),
                        color: None,
                        name: name_at(entry, 20),
                        active: false,
                        loop_beats: None,
                    });
                }
            }
            _ => {}
        }
    }
    cues
}

/// "Serato BeatGrid": version bytes, a marker count, then the markers: each
/// a position in seconds and, but for the last, the beats to the next one
/// (the last has the tempo from there on). Each marker starts a bar.
fn parse_beat_grid(data: &[u8]) -> Option<Vec<TempoMarker>> {
    let count = u32::from_be_bytes(data.get(2..6)?.try_into().ok()?) as usize;
    let at = |i: usize| -> Option<(f32, [u8; 4])> {
        let entry = data.get(6 + i * 8..14 + i * 8)?;
        Some((f32::from_be_bytes(entry[..4].try_into().ok()?), entry[4..].try_into().ok()?))
    };
    let mut markers = Vec::with_capacity(count);
    for i in 0..count {
        let (start, value) = at(i)?;
        let bpm = if i + 1 == count {
            f64::from(f32::from_be_bytes(value))
        } else {
            let beats = f64::from(u32::from_be_bytes(value));
            let next = f64::from(at(i + 1)?.0);
            beats * 60.0 / (next - f64::from(start))
        };
        if bpm.is_finite() && bpm > 0.0 {
            markers.push(TempoMarker { start: f64::from(start), bpm, beat: 1 });
        }
    }
    Some(markers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend((value.len() as u32).to_be_bytes());
        out.extend(value);
        out
    }

    fn text(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_be_bytes).collect()
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rofloxbox-serato-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("_Serato_/Subcrates")).unwrap();
        dir
    }

    /// Tracks with their tags, crates nested by name, and a crate's track
    /// the database doesn't list.
    #[test]
    fn reads_tracks_and_crates() {
        let dir = folder("library");
        let serato = dir.join("_Serato_");
        let mut track = field(b"pfil", &text("Users/me/Music/One.mp3"));
        track.extend(field(b"tsng", &text("One")));
        track.extend(field(b"tart", &text("Someone")));
        track.extend(field(b"tbpm", &text("174.00")));
        track.extend(field(b"tkey", &text("F#m")));
        track.extend(field(b"tlen", &text("05:12.50")));
        track.extend(field(b"uadd", &1_700_000_000u32.to_be_bytes()));
        let mut database = field(b"vrsn", &text("2.0/Serato Scratch LIVE Database"));
        database.extend(field(b"otrk", &track));
        std::fs::write(serato.join("database V2"), database).unwrap();
        let crate_file = |paths: &[&str]| {
            let mut out = field(b"vrsn", &text("1.0/Serato ScratchLive Crate"));
            for p in paths {
                out.extend(field(b"otrk", &field(b"ptrk", &text(p))));
            }
            out
        };
        let subcrates = serato.join("Subcrates");
        std::fs::write(subcrates.join("DnB.crate"), crate_file(&[])).unwrap();
        std::fs::write(subcrates.join("DnB%%Rollers.crate"), crate_file(&["Users/me/Music/One.mp3"]))
            .unwrap();
        std::fs::write(subcrates.join("Warmup.crate"), crate_file(&["Users\\me\\Music\\Two.mp3"]))
            .unwrap();

        let library = open(&serato).unwrap();
        let root = volume_root(&serato);
        let one = &library.tracks[0];
        assert_eq!(one.file.path, root.join("Users").join("me").join("Music").join("One.mp3"));
        assert_eq!((one.file.title.as_str(), one.file.artist.as_str()), ("One", "Someone"));
        assert_eq!((one.file.bpm, one.file.key.as_deref()), (Some(174.0), Some("F#m")));
        assert!((one.file.duration - 312.5).abs() < 1e-9);
        assert_eq!((one.added, one.rating), (Some(1_700_000_000), None));
        assert_eq!(library.tracks.len(), 2, "the crate's other track is added");
        let names: Vec<String> = library
            .playlists
            .iter()
            .map(|n| match n {
                XmlNode::Folder { name, children, .. } => format!("{name}/{}", children.len()),
                XmlNode::Playlist { name, keys, .. } => format!("{name}:{}", keys.len()),
            })
            .collect();
        assert_eq!(names, ["DnB/1", "Warmup:1"]);
        let XmlNode::Folder { children, .. } = &library.playlists[0] else { unreachable!() };
        assert!(matches!(&children[0], XmlNode::Playlist { name, keys, .. }
            if name == "Rollers" && keys == &[one.id.clone()]));
        assert!(library_in(&dir).is_some() && library_in(&serato).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn entry(name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = name.as_bytes().to_vec();
        out.push(0);
        out.extend((data.len() as u32).to_be_bytes());
        out.extend(data);
        out
    }

    /// Hot cues with their colours and names, saved loops, as Serato writes
    /// them: base64 in lines, without padding.
    #[test]
    fn reads_cues_and_loops() {
        let mut cue = vec![0, 2];
        cue.extend(12_345u32.to_be_bytes());
        cue.extend([0, 0xcc, 0x88, 0x00, 0, 0]);
        cue.extend(b"Drop\0");
        let mut looped = vec![0, 0];
        looped.extend(20_000u32.to_be_bytes());
        looped.extend(24_000u32.to_be_bytes());
        looped.extend([0xff; 4]);
        looped.extend([0, 0x27, 0xaa, 0xe1, 0, 0]);
        looped.extend(b"Roll\0");
        let mut body = vec![1, 1];
        body.extend(entry("COLOR", &[0, 0xff, 0xff, 0xff]));
        body.extend(entry("CUE", &cue));
        body.extend(entry("LOOP", &looped));
        body.extend(entry("BPMLOCK", &[0]));
        body.push(0);
        let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&body);
        let lines: Vec<&str> =
            encoded.as_bytes().chunks(72).map(|c| std::str::from_utf8(c).unwrap()).collect();
        let mut data = vec![1, 1];
        data.extend(lines.join("\n").as_bytes());
        let cues = parse_markers(&data);
        assert_eq!(
            cues,
            [
                Cue {
                    kind: CueKind::Hot(2),
                    start: 12.345,
                    end: None,
                    color: Some([0xcc, 0x88, 0x00]),
                    name: "Drop".into(),
                    active: false,
                    loop_beats: None,
                },
                Cue {
                    kind: CueKind::Memory,
                    start: 20.0,
                    end: Some(24.0),
                    color: None,
                    name: "Roll".into(),
                    active: false,
                    loop_beats: None,
                },
            ]
        );
        // Wrapped as in a FLAC's Vorbis comment: the same, once more in base64.
        let mut wrapped = b"application/octet-stream\0\0Serato Markers2\0".to_vec();
        wrapped.extend(&data);
        let comment = base64::engine::general_purpose::STANDARD.encode(&wrapped);
        assert_eq!(parse_markers(&unwrap(&decode(comment.as_bytes()).unwrap()).unwrap()), cues);
    }

    /// A grid that changes tempo: the first stretch's tempo from its beats,
    /// the last one's as written.
    #[test]
    fn reads_beat_grids() {
        let mut data = vec![1, 0];
        data.extend(2u32.to_be_bytes());
        data.extend(0.1f32.to_be_bytes());
        data.extend(64u32.to_be_bytes());
        // 64 beats at 128 BPM later.
        let second = 0.1f32 + 64.0 * 60.0 / 128.0;
        data.extend(second.to_be_bytes());
        data.extend(174.0f32.to_be_bytes());
        data.push(0);
        let grid = parse_beat_grid(&data).unwrap();
        assert_eq!(grid.len(), 2);
        assert!((grid[0].start - 0.1).abs() < 1e-6 && (grid[0].bpm - 128.0).abs() < 1e-3);
        assert!((grid[1].bpm - 174.0).abs() < 1e-9 && grid[1].beat == 1);
    }

    /// An MP3 (`ROFLOXBOX_SERATO_MP3`, changed) given Serato's GEOB frames
    /// reads back its cue and grid.
    #[test]
    #[ignore = "needs an MP3 to change"]
    fn reads_an_mp3s_serato_tags() {
        use lofty::config::WriteOptions;
        use lofty::file::AudioFile;
        use lofty::id3::v2::{Frame, GeneralEncapsulatedObject, Id3v2Tag};
        let path = PathBuf::from(std::env::var("ROFLOXBOX_SERATO_MP3").unwrap());
        let mut cue = vec![0, 0];
        cue.extend(1_500u32.to_be_bytes());
        cue.extend([0, 0xcc, 0, 0, 0, 0]);
        cue.extend(b"Start\0");
        let mut body = vec![1, 1];
        body.extend(entry("CUE", &cue));
        body.push(0);
        let mut written = vec![1, 1];
        written.extend(base64::engine::general_purpose::STANDARD_NO_PAD.encode(&body).as_bytes());
        let mut grid = vec![1, 0];
        grid.extend(1u32.to_be_bytes());
        grid.extend(0.25f32.to_be_bytes());
        grid.extend(172.0f32.to_be_bytes());
        grid.push(0);
        let geob = |name: &str, data: Vec<u8>| {
            let object = GeneralEncapsulatedObject::new(
                lofty::TextEncoding::Latin1,
                Some("application/octet-stream".into()),
                None,
                Some(name.into()),
                data,
            );
            Frame::Binary(lofty::id3::v2::BinaryFrame::new(
                lofty::id3::v2::FrameId::Valid("GEOB".into()),
                object.as_bytes(WriteOptions::default()).unwrap(),
            ))
        };
        let mut file = lofty::mpeg::MpegFile::read_from(
            &mut std::fs::File::open(&path).unwrap(),
            lofty::config::ParseOptions::new(),
        )
        .unwrap();
        let mut tag = file.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
        tag.insert(geob("Serato Markers2", written));
        tag.insert(geob("Serato BeatGrid", grid));
        file.set_id3v2(tag);
        file.save_to_path(&path, WriteOptions::default()).unwrap();
        let (cues, tempo) = markers(&path);
        assert_eq!(cues.len(), 1);
        assert_eq!((cues[0].kind, cues[0].start, cues[0].name.as_str()), (CueKind::Hot(0), 1.5, "Start"));
        assert_eq!(tempo, [TempoMarker { start: 0.25, bpm: 172.0, beat: 1 }]);
    }

    #[test]
    fn paths_start_at_their_drive() {
        if cfg!(windows) {
            assert_eq!(volume_root(Path::new(r"D:\_Serato_")), PathBuf::from(r"D:\"));
        } else {
            assert_eq!(volume_root(Path::new("/Users/me/Music/_Serato_")), PathBuf::from("/"));
            assert_eq!(volume_root(Path::new("/Volumes/USB/_Serato_")), PathBuf::from("/Volumes/USB"));
        }
        assert_eq!(duration("312"), 312.0);
    }
}
