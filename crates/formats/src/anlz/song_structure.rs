//! `PSSI`: phrase analysis ("song structure").
//!
//! Newer rekordbox versions XOR the section body with a fixed 19-byte
//! pattern, offset by the phrase count.

use super::RawSection;
use crate::bytes::{u8_at, u16_be_at};
use crate::error::Result;

pub(super) const MASK: [u8; 19] = [
    0xcb, 0xe1, 0xee, 0xfa, 0xe5, 0xee, 0xad, 0xee, 0xe9, 0xd2, 0xe9, 0xeb, 0xe1, 0xe9, 0xf3, 0xe8,
    0xe9, 0xf4, 0xe1,
];
/// Start of the (possibly masked) body within the section.
pub(super) const BODY_START: usize = 0x12;
const PHRASES_START: usize = 0x0e;
/// Valid moods are 1–3; a larger value can only come from a masked body.
const MAX_PLAUSIBLE_MOOD: u16 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SongStructure {
    pub mood: Mood,
    /// Beat at which the last phrase ends.
    pub end_beat: u16,
    /// Lighting-mode style bank (0 = default, 1 = cool, … 8 = club 2).
    pub bank: u8,
    pub phrases: Vec<Phrase>,
    /// Body value of unknown meaning after `end_beat` (0x100 in files seen so far).
    pub unknown: u16,
    /// Whether the body was stored XOR-masked.
    pub masked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    High,
    Mid,
    Low,
    Unknown(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phrase {
    /// Phrase number, starting at 1.
    pub index: u16,
    /// Beat at which the phrase starts.
    pub beat: u16,
    /// Phrase kind; its meaning depends on the track mood (see [`Phrase::label`]).
    pub kind: u16,
    /// Variant flags of high-mood phrases.
    pub k1: u8,
    pub k2: u8,
    pub k3: u8,
    /// How many of `extra_beats` a high-mood "Up 3" phrase uses.
    pub b: u8,
    pub extra_beats: [u16; 3],
    /// Beat at which a fill-in starts, if the phrase ends with one.
    pub fill_beat: Option<u16>,
}

impl Phrase {
    /// The phrase name as rekordbox displays it, without variant numbers.
    pub fn label(&self, mood: Mood) -> &'static str {
        match (mood, self.kind) {
            (_, 1) => "Intro",
            (Mood::High, 2) => "Up",
            (Mood::High, 3) => "Down",
            (Mood::High, 5) => "Chorus",
            (Mood::High, 6) => "Outro",
            (Mood::Low, 2..=4) | (Mood::Mid | Mood::Unknown(_), 2) => "Verse 1",
            (Mood::Low, 5..=7) | (Mood::Mid | Mood::Unknown(_), 3) => "Verse 2",
            (Mood::Mid | Mood::Unknown(_), 4) => "Verse 3",
            (Mood::Mid | Mood::Unknown(_), 5) => "Verse 4",
            (Mood::Mid | Mood::Unknown(_), 6) => "Verse 5",
            (Mood::Mid | Mood::Unknown(_), 7) => "Verse 6",
            (Mood::Mid | Mood::Low | Mood::Unknown(_), 8) => "Bridge",
            (Mood::Mid | Mood::Low | Mood::Unknown(_), 9) => "Chorus",
            (Mood::Mid | Mood::Low | Mood::Unknown(_), 10) => "Outro",
            _ => "Unknown",
        }
    }
}

pub(super) fn parse(section: &RawSection<'_>) -> Result<SongStructure> {
    let entry_len = section.u32(0x0c)? as usize;
    let count = section.u16(0x10)?;
    let mut body = section.bytes().get(BODY_START..).unwrap_or_default().to_vec();

    let masked = u16_be_at(&body, 0)? > MAX_PLAUSIBLE_MOOD;
    if masked {
        for (byte, mask) in body.iter_mut().zip(MASK.iter().cycle()) {
            *byte ^= mask.wrapping_add(count as u8);
        }
    }

    let mood = match u16_be_at(&body, 0)? {
        1 => Mood::High,
        2 => Mood::Mid,
        3 => Mood::Low,
        other => Mood::Unknown(other),
    };
    if entry_len < 24 {
        return Err(section.invalid("phrase entries shorter than 24 bytes"));
    }
    let phrases = (0..usize::from(count))
        .map(|i| {
            let at = PHRASES_START + i * entry_len;
            let u16_at = |field: usize| u16_be_at(&body, at + field);
            let u8_at = |field: usize| u8_at(&body, at + field);
            Ok(Phrase {
                index: u16_at(0)?,
                beat: u16_at(2)?,
                kind: u16_at(4)?,
                k1: u8_at(7)?,
                k2: u8_at(9)?,
                b: u8_at(11)?,
                extra_beats: [u16_at(12)?, u16_at(14)?, u16_at(16)?],
                k3: u8_at(19)?,
                fill_beat: if u8_at(21)? != 0 { Some(u16_at(22)?) } else { None },
            })
        })
        .collect::<Result<_>>()?;

    Ok(SongStructure {
        mood,
        end_beat: u16_be_at(&body, 0x08)?,
        unknown: u16_be_at(&body, 0x0a)?,
        bank: u8_at(&body, 0x0c)?,
        phrases,
        masked,
    })
}
