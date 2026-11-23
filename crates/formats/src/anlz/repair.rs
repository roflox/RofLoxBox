//! Repairing the cue lists older RofLoxBox versions wrote, which rekordbox
//! refuses — and with them the whole analysis file: no waveform, beat grid
//! or cues. Only the broken cue sections are written again; every other
//! section is kept byte for byte.
//!
//! What gets repaired:
//! - a memory cue list (`PCOB`) whose header does not name its first entry
//!   (rekordbox reads the list from there);
//! - extended cue entries (`PCP2`) not laid out as rekordbox writes them;
//! - hot cues only in the extended list: players read A–C from the
//!   `.DAT`'s basic list and D on from the `.EXT`'s.

use super::cues::{Cue, CueList, CueListKind, CueStatus};
use super::{AnlzFile, Section};
use crate::bytes::u32_be_at;
use crate::error::Result;

/// Hot cues players read from the `.DAT`'s basic list (A–C); the rest are
/// in the `.EXT`'s.
const DAT_HOT_CUES: u32 = 3;

/// The repaired files; `None` where nothing needed repairing.
#[derive(Debug, Default)]
pub struct Repaired {
    pub dat: Option<Vec<u8>>,
    pub ext: Option<Vec<u8>>,
}

impl Repaired {
    pub fn is_empty(&self) -> bool {
        self.dat.is_none() && self.ext.is_none()
    }
}

/// Repairs the cue lists of an analysis file pair (`.DAT` and its `.EXT`).
pub fn repair_cue_lists(dat: &[u8], ext: Option<&[u8]>) -> Result<Repaired> {
    let ext_file = ext.map(AnlzFile::from_bytes).transpose()?;
    let hot: Vec<Cue> = ext_file
        .iter()
        .flat_map(AnlzFile::cue_lists)
        .filter(|l| l.extended && l.kind == CueListKind::Hot)
        .flat_map(|l| l.cues.clone())
        .collect();
    Ok(Repaired {
        dat: repair_file(dat, &hot, true)?,
        ext: ext.map(|bytes| repair_file(bytes, &hot, false)).transpose()?.flatten(),
    })
}

/// One file with its broken cue sections written again; `None` if none was.
fn repair_file(bytes: &[u8], hot: &[Cue], is_dat: bool) -> Result<Option<Vec<u8>>> {
    let file = AnlzFile::from_bytes(bytes)?;
    let raw = raw_sections(bytes)?;
    if raw.len() != file.sections.len() {
        return Ok(None);
    }
    let mut changed = false;
    let mut body = Vec::with_capacity(bytes.len());
    for (section, range) in file.sections.iter().zip(&raw) {
        let original = &bytes[range.clone()];
        let replacement = match section {
            Section::Cues(list) => repaired_list(list, original, hot, is_dat),
            _ => None,
        };
        match replacement {
            Some(new) => {
                changed = true;
                body.extend(new.to_bytes());
            }
            None => body.extend_from_slice(original),
        }
    }
    if !changed {
        return Ok(None);
    }
    let header_len = u32_be_at(bytes, 4)? as usize;
    let mut out = bytes[..header_len].to_vec();
    out[8..12].copy_from_slice(&((header_len + body.len()) as u32).to_be_bytes());
    out.extend(body);
    Ok(Some(out))
}

/// The list as it should be, if `original` (its bytes) is broken.
fn repaired_list(list: &CueList, original: &[u8], hot: &[Cue], is_dat: bool) -> Option<Section> {
    if list.extended {
        return extended_entries_broken(original).then(|| Section::Cues(list.clone()));
    }
    match list.kind {
        // The writer names the list's first and last entry.
        CueListKind::Memory => memory_header_broken(original).then(|| Section::Cues(list.clone())),
        CueListKind::Hot if list.cues.is_empty() => {
            let cues: Vec<Cue> = hot
                .iter()
                .filter(|c| c.hot_cue.is_some_and(|n| (n <= DAT_HOT_CUES) == is_dat))
                .map(basic_hot_cue)
                .collect();
            (!cues.is_empty()).then(|| Section::Cues(CueList { cues, ..list.clone() }))
        }
        _ => None,
    }
}

/// A hot cue for a basic list, as rekordbox writes them there.
fn basic_hot_cue(cue: &Cue) -> Cue {
    Cue {
        status: Some(CueStatus::Disabled),
        order: Some((0xffff, 0xffff)),
        color_id: None,
        loop_beats: None,
        comment: None,
        hot_cue_color: None,
        trailer: Vec::new(),
        ..cue.clone()
    }
}

/// A memory list whose header names no entry to start from.
fn memory_header_broken(section: &[u8]) -> bool {
    let count = u16::from_be_bytes([section[0x12], section[0x13]]);
    let first = u16::from_be_bytes([section[0x14], section[0x15]]);
    count > 0 && first >= count
}

/// Extended entries without the 1 after the color id, or short of the 40
/// bytes after the color.
fn extended_entries_broken(section: &[u8]) -> bool {
    let header_len = u32::from_be_bytes([section[4], section[5], section[6], section[7]]) as usize;
    let mut at = header_len;
    while at + 12 <= section.len() {
        let len = u32::from_be_bytes([section[at + 8], section[at + 9], section[at + 10], section[at + 11]])
            as usize;
        if len < 12 {
            return false;
        }
        if len < 0x58 || section.get(at + 0x1d) == Some(&0) {
            return true;
        }
        at += len;
    }
    false
}

/// Where each section is in the file.
fn raw_sections(bytes: &[u8]) -> Result<Vec<std::ops::Range<usize>>> {
    let mut at = u32_be_at(bytes, 4)? as usize;
    let mut out = Vec::new();
    while at + 12 <= bytes.len() {
        let len = u32_be_at(bytes, at + 8)? as usize;
        if len == 0 || at + len > bytes.len() {
            break;
        }
        out.push(at..at + len);
        at += len;
    }
    Ok(out)
}
