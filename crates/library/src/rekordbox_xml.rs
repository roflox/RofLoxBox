//! Reading rekordbox's XML export (File → Export Collection in xml format):
//! tracks with their tags, beat grids and cues, and the playlist tree.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::collection::{Cue, CueKind, TempoMarker};
use crate::scan::ScannedFile;

/// A track as rekordbox exported it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XmlTrack {
    /// rekordbox's `TrackID`, used by the playlists.
    pub id: String,
    /// `Location` as written (a `file://` URL).
    pub location: String,
    pub file: ScannedFile,
    /// 0–5 stars.
    pub rating: u8,
    /// rekordbox colour label 1–8.
    pub color: Option<u8>,
    pub play_count: u32,
    /// Unix seconds.
    pub added: Option<u64>,
    pub tempo: Vec<TempoMarker>,
    pub cues: Vec<Cue>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum XmlNode {
    Folder {
        name: String,
        children: Vec<XmlNode>,
    },
    /// Track keys: `TrackID`s, or locations when `by_location`.
    Playlist {
        name: String,
        keys: Vec<String>,
        by_location: bool,
    },
}

#[derive(Debug, Default)]
pub struct XmlLibrary {
    pub tracks: Vec<XmlTrack>,
    /// The children of rekordbox's ROOT folder.
    pub playlists: Vec<XmlNode>,
}

impl XmlLibrary {
    pub fn open(path: &Path) -> Result<Self> {
        let text = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("reading {}", path.display()))
    }

    pub fn parse(xml: &[u8]) -> Result<Self> {
        let mut reader = Reader::from_reader(xml);
        reader.config_mut().trim_text(true);
        let mut library = XmlLibrary::default();
        let mut buf = Vec::new();
        let mut in_collection = false;
        let mut track: Option<XmlTrack> = None;
        // Open NODE elements of the playlist tree, innermost last.
        let mut stack: Vec<XmlNode> = Vec::new();
        loop {
            let event = reader.read_event_into(&mut buf)?;
            let (element, empty) = match &event {
                Event::Start(e) => (Some(e.clone()), false),
                Event::Empty(e) => (Some(e.clone()), true),
                Event::End(e) => {
                    match e.name().into_inner() {
                        "COLLECTION" => in_collection = false,
                        "TRACK" if in_collection => library.tracks.extend(track.take()),
                        "NODE" => close_node(&mut stack, &mut library.playlists),
                        _ => {}
                    }
                    buf.clear();
                    continue;
                }
                Event::Eof => break,
                _ => {
                    buf.clear();
                    continue;
                }
            };
            let e = element.expect("start or empty element");
            let attrs = attributes(&e)?;
            let get = |name: &str| attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
            match e.name().into_inner() {
                "COLLECTION" => in_collection = !empty,
                "TRACK" if in_collection => {
                    let parsed = parse_track(&get);
                    if empty {
                        library.tracks.push(parsed);
                    } else {
                        track = Some(parsed);
                    }
                }
                "TEMPO" => {
                    if let Some(t) = track.as_mut() {
                        let num = |n: &str| get(n).and_then(|v| v.parse::<f64>().ok());
                        if let (Some(start), Some(bpm)) = (num("Inizio"), num("Bpm")) {
                            let beat = get("Battito").and_then(|b| b.parse().ok()).unwrap_or(1);
                            t.tempo.push(TempoMarker { start, bpm, beat });
                        }
                    }
                }
                "POSITION_MARK" => {
                    if let Some(t) = track.as_mut()
                        && let Some(cue) = parse_cue(&get)
                    {
                        t.cues.push(cue);
                    }
                }
                "NODE" => {
                    let name = get("Name").unwrap_or_default().to_owned();
                    let node = if get("Type") == Some("1") {
                        XmlNode::Playlist {
                            name,
                            keys: Vec::new(),
                            by_location: get("KeyType") == Some("1"),
                        }
                    } else {
                        XmlNode::Folder { name, children: Vec::new() }
                    };
                    stack.push(node);
                    if empty {
                        close_node(&mut stack, &mut library.playlists);
                    }
                }
                "TRACK" => {
                    if let (Some(XmlNode::Playlist { keys, .. }), Some(key)) =
                        (stack.last_mut(), get("Key"))
                    {
                        keys.push(key.to_owned());
                    }
                }
                _ => {}
            }
            buf.clear();
        }
        if library.tracks.is_empty() && library.playlists.is_empty() {
            bail!("no rekordbox collection found");
        }
        Ok(library)
    }
}

/// Closes the innermost NODE into its parent folder. The outermost one is
/// rekordbox's ROOT folder, whose children become the top level.
fn close_node(stack: &mut Vec<XmlNode>, top: &mut Vec<XmlNode>) {
    let Some(node) = stack.pop() else { return };
    match (stack.last_mut(), node) {
        (Some(XmlNode::Folder { children, .. }), node) => children.push(node),
        (Some(XmlNode::Playlist { .. }), _) => {}
        (None, XmlNode::Folder { children, .. }) => top.extend(children),
        (None, playlist) => top.push(playlist),
    }
}

fn attributes(e: &BytesStart<'_>) -> Result<Vec<(String, String)>> {
    e.attributes()
        .map(|a| {
            let a = a?;
            Ok((
                AsRef::<str>::as_ref(&a.key).to_owned(),
                a.normalized_value(quick_xml::XmlVersion::default())?.into_owned(),
            ))
        })
        .collect()
}

fn parse_track<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> XmlTrack {
    let text = |n: &str| get(n).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
    let number = |n: &str| get(n).and_then(|v| v.trim().parse::<f64>().ok());
    let location = get("Location").unwrap_or_default().to_owned();
    let path = path_from_location(&location);
    let title = text("Name").unwrap_or_else(|| {
        path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned())
    });
    XmlTrack {
        id: get("TrackID").unwrap_or_default().to_owned(),
        file: ScannedFile {
            path,
            title,
            artist: text("Artist").unwrap_or_default(),
            album: text("Album").unwrap_or_default(),
            genre: text("Genre").unwrap_or_default(),
            label: text("Label").unwrap_or_default(),
            comment: text("Comments").unwrap_or_default(),
            year: number("Year").filter(|y| *y > 0.0).map(|y| y as u16),
            track_number: number("TrackNumber").filter(|n| *n > 0.0).map(|n| n as u32),
            duration: number("TotalTime").unwrap_or(0.0),
            bpm: number("AverageBpm").filter(|b| *b > 0.0),
            key: text("Tonality"),
            sample_rate: number("SampleRate").map(|r| r as u32),
            bitrate: number("BitRate").filter(|b| *b > 0.0).map(|b| b as u32),
            has_artwork: false,
            file_size: number("Size").map_or(0, |s| s as u64),
            modified: 0,
        },
        location,
        // 0, 51, 102, 153, 204, 255 → 0–5 stars.
        rating: number("Rating").map_or(0, |r| ((r + 25.0) / 51.0).clamp(0.0, 5.0) as u8),
        color: get("Colour").and_then(color_label),
        play_count: number("PlayCount").map_or(0, |n| n as u32),
        added: get("DateAdded").and_then(unix_date),
        tempo: Vec::new(),
        cues: Vec::new(),
    }
}

fn parse_cue<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> Option<Cue> {
    let number = |n: &str| get(n).and_then(|v| v.trim().parse::<f64>().ok());
    let start = number("Start")?;
    // Type 4 is a loop; Num −1 is a memory cue, 0–7 hot cues A–H.
    let end = (get("Type") == Some("4")).then(|| number("End")).flatten();
    let kind = match number("Num").map(|n| n as i32) {
        Some(n @ 0..=7) => CueKind::Hot(n as u8),
        _ => CueKind::Memory,
    };
    let color = match (number("Red"), number("Green"), number("Blue")) {
        (Some(r), Some(g), Some(b)) => Some([r as u8, g as u8, b as u8]),
        _ => None,
    };
    Some(Cue { kind, start, end, color, name: get("Name").unwrap_or_default().to_owned() })
}

/// `file://localhost/Users/me/a%20b.mp3` → `/Users/me/a b.mp3`;
/// `file://localhost/C:/Music/x.mp3` → `C:/Music/x.mp3`.
pub fn path_from_location(location: &str) -> PathBuf {
    let rest = location
        .strip_prefix("file://localhost")
        .or_else(|| location.strip_prefix("file://"))
        .unwrap_or(location);
    let decoded = percent_decode(rest);
    let bytes = decoded.as_bytes();
    // A Windows drive letter follows the slash: "/C:/…".
    if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' && bytes[1].is_ascii_alphabetic() {
        return PathBuf::from(&decoded[1..]);
    }
    PathBuf::from(decoded)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// rekordbox's colour labels, by the RGB it writes.
fn color_label(text: &str) -> Option<u8> {
    let value =
        u32::from_str_radix(text.trim().trim_start_matches("0x").trim_start_matches('#'), 16)
            .ok()?;
    Some(match value {
        0xFF007F => 1, // pink
        0xFF0000 => 2, // red
        0xFFA500 => 3, // orange
        0xFFFF00 => 4, // yellow
        0x00FF00 => 5, // green
        0x25FDE9 => 6, // aqua
        0x0000FF => 7, // blue
        0x660099 => 8, // purple
        _ => return None,
    })
}

/// `2024-01-31` → Unix seconds at midnight UTC.
fn unix_date(text: &str) -> Option<u64> {
    let mut parts = text.trim().split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DJ_PLAYLISTS Version="1.0.0">
  <PRODUCT Name="rekordbox" Version="7.0.0" Company="AlphaTheta"/>
  <COLLECTION Entries="2">
    <TRACK TrackID="11" Name="First &amp; Last" Artist="Someone" Album="LP" Genre="House"
           Kind="MP3 File" Size="1000" TotalTime="300" Year="2020" AverageBpm="124.00"
           DateAdded="2024-01-31" BitRate="320" SampleRate="44100" Comments="nice"
           PlayCount="3" Rating="204" Location="file://localhost/Users/me/Music/a%20b.mp3"
           Tonality="Am" Label="Lbl" Colour="0xFF0000">
      <TEMPO Inizio="0.025" Bpm="124.00" Metro="4/4" Battito="1"/>
      <POSITION_MARK Name="" Type="0" Start="0.025" Num="-1"/>
      <POSITION_MARK Name="drop" Type="0" Start="60.5" Num="1" Red="40" Green="226" Blue="20"/>
      <POSITION_MARK Name="" Type="4" Start="30.0" End="37.742" Num="-1"/>
    </TRACK>
    <TRACK TrackID="12" Name="Second" Location="file://localhost/C:/Music/x.mp3"/>
  </COLLECTION>
  <PLAYLISTS>
    <NODE Type="0" Name="ROOT" Count="2">
      <NODE Type="0" Name="Sets" Count="1">
        <NODE Name="Friday" Type="1" KeyType="0" Entries="2">
          <TRACK Key="12"/>
          <TRACK Key="11"/>
        </NODE>
      </NODE>
      <NODE Name="Empty" Type="1" KeyType="0" Entries="0"/>
    </NODE>
  </PLAYLISTS>
</DJ_PLAYLISTS>"#;

    #[test]
    fn reads_tracks_grids_cues_and_playlists() {
        let lib = XmlLibrary::parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(lib.tracks.len(), 2);
        let t = &lib.tracks[0];
        assert_eq!(t.file.title, "First & Last");
        assert_eq!(t.file.path, PathBuf::from("/Users/me/Music/a b.mp3"));
        assert_eq!((t.file.bpm, t.file.key.as_deref()), (Some(124.0), Some("Am")));
        assert_eq!((t.rating, t.color, t.play_count), (4, Some(2), 3));
        assert_eq!(t.added, Some(1_706_659_200));
        assert_eq!(t.tempo, [TempoMarker { start: 0.025, bpm: 124.0, beat: 1 }]);
        assert_eq!(t.cues.len(), 3);
        assert_eq!(t.cues[1].kind, CueKind::Hot(1));
        assert_eq!(t.cues[1].color, Some([40, 226, 20]));
        assert_eq!(t.cues[2].end, Some(37.742));
        assert_eq!(lib.tracks[1].file.path, PathBuf::from("C:/Music/x.mp3"));
        assert_eq!(
            lib.playlists,
            [
                XmlNode::Folder {
                    name: "Sets".into(),
                    children: vec![XmlNode::Playlist {
                        name: "Friday".into(),
                        keys: vec!["12".into(), "11".into()],
                        by_location: false
                    }]
                },
                XmlNode::Playlist { name: "Empty".into(), keys: vec![], by_location: false },
            ]
        );
    }
}
