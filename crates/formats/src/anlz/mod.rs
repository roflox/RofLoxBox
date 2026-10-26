//! Track analysis files: `PIONEER/USBANLZ/Pxxx/xxxxxxxx/ANLZ0000.DAT`, `.EXT`
//! and `.2EX`.
//!
//! All values are big-endian. A file starts with a `PMAI` header and
//! continues with tagged sections. Every section starts with its four-byte
//! tag, the length of its header and its total length; type-specific fields
//! fill the rest of the header and the payload follows it.

mod cues;
mod song_structure;
mod write;

pub use cues::{Cue, CueKind, CueList, CueListKind, CueStatus, HotCueColor};
pub use song_structure::{Mood, Phrase, SongStructure};

use std::path::Path;

use crate::bytes::{slice, u16_be_at, u32_be_at};
use crate::error::{Error, Result};

const FILE_MAGIC: &[u8; 4] = b"PMAI";
const SECTION_HEADER_LEN: usize = 12;

/// A parsed analysis file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnlzFile {
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Section {
    /// `PQTZ`
    BeatGrid(BeatGrid),
    /// `PQT2`: beat grid summary in `.EXT` files (no public documentation;
    /// see [`ExtendedBeatGrid`]).
    ExtendedBeatGrid(ExtendedBeatGrid),
    /// `PCOB` (basic) or `PCO2` (extended, with comments and colors)
    Cues(CueList),
    /// `PPTH`: path of the audio file on the medium.
    Path(String),
    /// `PVBR`: byte offsets into the audio file at evenly spaced times, for
    /// seeking in variable bit-rate files. Files seen so far hold 401 values;
    /// the meaning of the last one is not known yet.
    Vbr(Vec<u32>),
    /// `PWAV`, `PWV2` … `PWV7`
    Waveform(Waveform),
    /// `PSSI`: phrases.
    SongStructure(SongStructure),
    /// A section this crate does not interpret, kept verbatim.
    Unknown { tag: [u8; 4], bytes: Vec<u8> },
}

impl AnlzFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.get(..4) != Some(FILE_MAGIC) {
            return Err(Error::InvalidHeader("missing PMAI magic"));
        }
        let header_len = u32_be_at(data, 4)? as usize;
        if header_len < 12 || header_len > data.len() {
            return Err(Error::InvalidHeader("file header length out of range"));
        }

        let mut sections = Vec::new();
        let mut offset = header_len;
        while offset + SECTION_HEADER_LEN <= data.len() {
            let raw = RawSection::at(data, offset)?;
            offset += raw.bytes.len();
            sections.push(raw.parse()?);
        }
        Ok(Self { sections })
    }

    pub fn beat_grid(&self) -> Option<&BeatGrid> {
        self.sections.iter().find_map(|s| match s {
            Section::BeatGrid(grid) => Some(grid),
            _ => None,
        })
    }

    pub fn path(&self) -> Option<&str> {
        self.sections.iter().find_map(|s| match s {
            Section::Path(path) => Some(path.as_str()),
            _ => None,
        })
    }

    pub fn cue_lists(&self) -> impl Iterator<Item = &CueList> {
        self.sections.iter().filter_map(|s| match s {
            Section::Cues(list) => Some(list),
            _ => None,
        })
    }

    pub fn waveform(&self, kind: WaveformKind) -> Option<&Waveform> {
        self.sections.iter().find_map(|s| match s {
            Section::Waveform(waveform) if waveform.kind == kind => Some(waveform),
            _ => None,
        })
    }

    pub fn song_structure(&self) -> Option<&SongStructure> {
        self.sections.iter().find_map(|s| match s {
            Section::SongStructure(structure) => Some(structure),
            _ => None,
        })
    }
}

impl Section {
    /// The four-byte tag this section is stored under.
    pub fn tag(&self) -> [u8; 4] {
        match self {
            Section::BeatGrid(_) => *b"PQTZ",
            Section::ExtendedBeatGrid(_) => *b"PQT2",
            Section::Cues(list) if list.extended => *b"PCO2",
            Section::Cues(_) => *b"PCOB",
            Section::Path(_) => *b"PPTH",
            Section::Vbr(_) => *b"PVBR",
            Section::Waveform(waveform) => waveform.kind.tag(),
            Section::SongStructure(_) => *b"PSSI",
            Section::Unknown { tag, .. } => *tag,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeatGrid {
    pub beats: Vec<Beat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Beat {
    /// Position within the bar, 1 being the downbeat.
    pub bar_position: u16,
    /// BPM × 100 at this beat.
    pub tempo: u16,
    /// Milliseconds from the start of the track at normal pitch.
    pub time_ms: u32,
}

impl Beat {
    pub fn bpm(&self) -> f64 {
        f64::from(self.tempo) / 100.0
    }
}

/// `PQT2`. Found by our own analysis of fixture files: the header repeats the
/// first and last beat of the `PQTZ` grid and its beat count, and the payload
/// holds two bytes per beat whose meaning is not known yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedBeatGrid {
    pub first_beat: Beat,
    pub last_beat: Beat,
    pub beat_count: u32,
    /// Header value of unknown meaning (offset 0x2c).
    pub unknown: u32,
    /// One undecoded value per beat.
    pub entries: Vec<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaveformKind {
    /// `PWAV`: monochrome preview, 400 columns.
    Preview,
    /// `PWV2`: tiny monochrome preview for older players.
    TinyPreview,
    /// `PWV3`: monochrome scrolling detail, one column per 1/150 s.
    Detail,
    /// `PWV4`: color preview, 6 bytes per column.
    ColorPreview,
    /// `PWV5`: color scrolling detail, 2 bytes per column.
    ColorDetail,
    /// `PWV6`: 3-band preview (CDJ-3000), 3 bytes per column.
    ThreeBandPreview,
    /// `PWV7`: 3-band scrolling detail (CDJ-3000), 3 bytes per column.
    ThreeBandDetail,
}

impl WaveformKind {
    pub const ALL: [WaveformKind; 7] = [
        WaveformKind::Preview,
        WaveformKind::TinyPreview,
        WaveformKind::Detail,
        WaveformKind::ColorPreview,
        WaveformKind::ColorDetail,
        WaveformKind::ThreeBandPreview,
        WaveformKind::ThreeBandDetail,
    ];

    pub fn tag(self) -> [u8; 4] {
        match self {
            WaveformKind::Preview => *b"PWAV",
            WaveformKind::TinyPreview => *b"PWV2",
            WaveformKind::Detail => *b"PWV3",
            WaveformKind::ColorPreview => *b"PWV4",
            WaveformKind::ColorDetail => *b"PWV5",
            WaveformKind::ThreeBandPreview => *b"PWV6",
            WaveformKind::ThreeBandDetail => *b"PWV7",
        }
    }

    fn from_tag(tag: &[u8; 4]) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| &kind.tag() == tag)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waveform {
    pub kind: WaveformKind,
    /// Bytes per column.
    pub entry_len: usize,
    pub data: Vec<u8>,
}

impl Waveform {
    /// The columns of the waveform, `entry_len` bytes each.
    pub fn columns(&self) -> impl Iterator<Item = &[u8]> {
        self.data.chunks_exact(self.entry_len.max(1))
    }

    pub fn column_count(&self) -> usize {
        self.data.len() / self.entry_len.max(1)
    }
}

/// One section's bytes, header included.
pub(crate) struct RawSection<'a> {
    tag: [u8; 4],
    bytes: &'a [u8],
    header_len: usize,
    /// Offset of the section in the file, for error messages.
    offset: usize,
}

impl<'a> RawSection<'a> {
    fn at(data: &'a [u8], offset: usize) -> Result<Self> {
        let tag: [u8; 4] = slice(data, offset, 4)?.try_into().expect("4-byte slice");
        let header_len = u32_be_at(data, offset + 4)? as usize;
        let total_len = u32_be_at(data, offset + 8)? as usize;
        let raw = Self { tag, bytes: &[], header_len, offset };
        if header_len < SECTION_HEADER_LEN || total_len < header_len {
            return Err(raw.invalid("header or total length out of range"));
        }
        let bytes = slice(data, offset, total_len)?;
        Ok(Self { bytes, ..raw })
    }

    fn parse(&self) -> Result<Section> {
        if let Some(kind) = WaveformKind::from_tag(&self.tag) {
            return self.waveform(kind).map(Section::Waveform);
        }
        match &self.tag {
            b"PQTZ" => self.beat_grid().map(Section::BeatGrid),
            b"PQT2" => self.extended_beat_grid().map(Section::ExtendedBeatGrid),
            b"PCOB" => cues::parse(self, false).map(Section::Cues),
            b"PCO2" => cues::parse(self, true).map(Section::Cues),
            b"PPTH" => self.path().map(Section::Path),
            b"PVBR" => Ok(Section::Vbr(
                self.payload()
                    .chunks_exact(4)
                    .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
                    .collect(),
            )),
            b"PSSI" => song_structure::parse(self).map(Section::SongStructure),
            _ => Ok(Section::Unknown { tag: self.tag, bytes: self.bytes.to_vec() }),
        }
    }

    pub(crate) fn invalid(&self, reason: &'static str) -> Error {
        Error::InvalidSection {
            section: String::from_utf8_lossy(&self.tag).into_owned(),
            offset: self.offset,
            reason,
        }
    }

    pub(crate) fn payload(&self) -> &'a [u8] {
        &self.bytes[self.header_len..]
    }

    pub(crate) fn header_len(&self) -> usize {
        self.header_len
    }

    pub(crate) fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub(crate) fn u16(&self, at: usize) -> Result<u16> {
        u16_be_at(self.bytes, at)
    }

    pub(crate) fn u32(&self, at: usize) -> Result<u32> {
        u32_be_at(self.bytes, at)
    }

    fn beat_grid(&self) -> Result<BeatGrid> {
        let count = self.u32(0x14)? as usize;
        let payload = self.payload();
        if payload.len() / 8 < count {
            return Err(self.invalid("beat count exceeds section"));
        }
        let beats = payload
            .chunks_exact(8)
            .take(count)
            .map(|b| Beat {
                bar_position: u16::from_be_bytes([b[0], b[1]]),
                tempo: u16::from_be_bytes([b[2], b[3]]),
                time_ms: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            })
            .collect();
        Ok(BeatGrid { beats })
    }

    fn beat_at(&self, at: usize) -> Result<Beat> {
        Ok(Beat {
            bar_position: self.u16(at)?,
            tempo: self.u16(at + 2)?,
            time_ms: self.u32(at + 4)?,
        })
    }

    fn extended_beat_grid(&self) -> Result<ExtendedBeatGrid> {
        Ok(ExtendedBeatGrid {
            first_beat: self.beat_at(0x18)?,
            last_beat: self.beat_at(0x20)?,
            beat_count: self.u32(0x28)?,
            unknown: self.u32(0x2c)?,
            entries: self
                .payload()
                .chunks_exact(2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                .collect(),
        })
    }

    fn path(&self) -> Result<String> {
        let len = (self.u32(0x0c)? as usize).min(self.payload().len());
        Ok(utf16be(&self.payload()[..len]))
    }

    fn waveform(&self, kind: WaveformKind) -> Result<Waveform> {
        let payload = self.payload();
        let (entry_len, len) = match kind {
            // These two store the byte length of the data instead of entry size and count.
            WaveformKind::Preview | WaveformKind::TinyPreview => (1, self.u32(0x0c)? as usize),
            _ => {
                let entry_len = self.u32(0x0c)? as usize;
                let count = self.u32(0x10)? as usize;
                let len =
                    entry_len.checked_mul(count).ok_or_else(|| self.invalid("size overflow"))?;
                (entry_len, len)
            }
        };
        let data = payload.get(..len).ok_or_else(|| self.invalid("waveform exceeds section"))?;
        Ok(Waveform { kind, entry_len, data: data.to_vec() })
    }
}

/// Decodes UTF-16BE text, dropping the trailing NUL rekordbox writes.
pub(crate) fn utf16be(bytes: &[u8]) -> String {
    let units: Vec<u16> =
        bytes.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
    let text = String::from_utf16_lossy(&units);
    text.trim_end_matches('\0').to_owned()
}
