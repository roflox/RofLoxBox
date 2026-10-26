//! Cue lists: `PCOB` (all players) and `PCO2` (nxs2 and later, adds
//! comments, colors and quantized loop lengths).

use super::{RawSection, utf16be};
use crate::bytes::{slice, u8_at, u16_be_at, u32_be_at};
use crate::error::Result;

/// Entry fields before the optional comment and color in a `PCP2` entry.
const EXTENDED_FIXED_LEN: usize = 0x2c;
const NO_LOOP: u32 = u32::MAX;

type EntryParser = fn(&[u8]) -> Result<Cue>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueList {
    pub kind: CueListKind,
    /// Read from a `PCO2` section.
    pub extended: bool,
    /// `PCOB` header value of unknown meaning ("memory count" in the
    /// crate-digger notes); `0xffffffff` in every file seen so far.
    pub memory_count: u32,
    pub cues: Vec<Cue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CueListKind {
    Memory,
    Hot,
    Unknown(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    /// 1 = A, 2 = B, …; `None` for memory cues.
    pub hot_cue: Option<u32>,
    pub kind: CueKind,
    pub time_ms: u32,
    /// Where a loop jumps back to `time_ms`.
    pub loop_end_ms: Option<u32>,
    /// Only stored in `PCOB` entries.
    pub status: Option<CueStatus>,
    /// Memory cue / loop color, referencing the colors table (`PCO2` only).
    pub color_id: Option<u8>,
    /// Loop length in beats as numerator / denominator, if quantized (`PCO2` only).
    pub loop_beats: Option<(u16, u16)>,
    pub comment: Option<String>,
    /// Hot cue display color (`PCO2` only).
    pub hot_cue_color: Option<HotCueColor>,
    /// `PCPT` ordering fields of unknown exact meaning (previous, next).
    pub order: Option<(u16, u16)>,
    /// Bytes after the color of a `PCP2` entry; meaning unknown.
    pub trailer: Vec<u8>,
}

impl Cue {
    /// Hot cue letter as shown on the player: `A` for 1, `B` for 2, ….
    pub fn hot_cue_letter(&self) -> Option<char> {
        let index = self.hot_cue?.checked_sub(1)?;
        char::from_u32(u32::from(b'A') + index).filter(char::is_ascii_uppercase)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CueKind {
    Cue,
    Loop,
    Unknown(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CueStatus {
    Disabled,
    Enabled,
    ActiveLoop,
    Unknown(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotCueColor {
    /// Index into rekordbox's hot cue color palette.
    pub code: u8,
    pub rgb: [u8; 3],
}

pub(super) fn parse(section: &RawSection<'_>, extended: bool) -> Result<CueList> {
    let kind = match section.u32(0x0c)? {
        0 => CueListKind::Memory,
        1 => CueListKind::Hot,
        other => CueListKind::Unknown(other),
    };
    let count = section.u16(if extended { 0x10 } else { 0x12 })?;
    let memory_count = if extended { 0 } else { section.u32(0x14)? };
    let entry_magic: &[u8; 4] = if extended { b"PCP2" } else { b"PCPT" };
    let parse_entry: EntryParser = if extended { extended_entry } else { basic_entry };

    let bytes = section.bytes();
    let mut cues = Vec::with_capacity(count.into());
    let mut offset = section.header_len();
    for _ in 0..count {
        if slice(bytes, offset, 4)? != entry_magic {
            return Err(section.invalid("cue entry has the wrong magic"));
        }
        let entry_len = u32_be_at(bytes, offset + 8)? as usize;
        if entry_len < 12 {
            return Err(section.invalid("cue entry length too small"));
        }
        cues.push(parse_entry(slice(bytes, offset, entry_len)?)?);
        offset += entry_len;
    }
    Ok(CueList { kind, extended, memory_count, cues })
}

fn cue_kind(raw: u8) -> CueKind {
    match raw {
        1 => CueKind::Cue,
        2 => CueKind::Loop,
        other => CueKind::Unknown(other),
    }
}

fn loop_end(kind: CueKind, raw: u32) -> Option<u32> {
    (kind == CueKind::Loop && raw != NO_LOOP).then_some(raw)
}

/// A `PCPT` entry.
fn basic_entry(entry: &[u8]) -> Result<Cue> {
    let kind = cue_kind(u8_at(entry, 0x1c)?);
    Ok(Cue {
        hot_cue: Some(u32_be_at(entry, 0x0c)?).filter(|&n| n != 0),
        status: Some(match u32_be_at(entry, 0x10)? {
            0 => CueStatus::Disabled,
            1 => CueStatus::Enabled,
            4 => CueStatus::ActiveLoop,
            other => CueStatus::Unknown(other),
        }),
        kind,
        time_ms: u32_be_at(entry, 0x20)?,
        loop_end_ms: loop_end(kind, u32_be_at(entry, 0x24)?),
        color_id: None,
        loop_beats: None,
        comment: None,
        hot_cue_color: None,
        order: Some((u16_be_at(entry, 0x18)?, u16_be_at(entry, 0x1a)?)),
        trailer: Vec::new(),
    })
}

/// A `PCP2` entry.
fn extended_entry(entry: &[u8]) -> Result<Cue> {
    let kind = cue_kind(u8_at(entry, 0x10)?);
    let numerator = u16_be_at(entry, 0x24)?;
    let denominator = u16_be_at(entry, 0x26)?;

    let (comment, color_at) = if entry.len() >= EXTENDED_FIXED_LEN {
        let comment_len = u32_be_at(entry, 0x28)? as usize;
        let text = utf16be(slice(entry, EXTENDED_FIXED_LEN, comment_len)?);
        // A comment field holding only the terminating NUL reads as `Some("")`.
        ((comment_len > 0).then_some(text), EXTENDED_FIXED_LEN + comment_len)
    } else {
        (None, entry.len())
    };
    let color = slice(entry, color_at, 4).ok();
    let hot_cue_color = color
        .map(|c| HotCueColor { code: c[0], rgb: [c[1], c[2], c[3]] })
        .filter(|color| color.code != 0 || color.rgb != [0; 3]);
    let trailer = entry.get(color_at + 4..).unwrap_or_default().to_vec();

    Ok(Cue {
        hot_cue: Some(u32_be_at(entry, 0x0c)?).filter(|&n| n != 0),
        kind,
        time_ms: u32_be_at(entry, 0x14)?,
        loop_end_ms: loop_end(kind, u32_be_at(entry, 0x18)?),
        status: None,
        color_id: Some(u8_at(entry, 0x1c)?).filter(|&id| id != 0),
        loop_beats: (numerator != 0 && denominator != 0).then_some((numerator, denominator)),
        comment,
        hot_cue_color,
        order: None,
        trailer,
    })
}
