//! Serializing analysis files. Header constants are the values rekordbox
//! writes; every one is identical across all fixture files.

use super::cues::{Cue, CueKind, CueList, CueListKind, CueStatus};
use super::song_structure::{BODY_START, MASK, Mood, SongStructure};
use super::{AnlzFile, Beat, BeatGrid, ExtendedBeatGrid, Section, Waveform, WaveformKind};

const FILE_HEADER_TAIL: [u8; 16] = [0, 0, 0, 1, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];

impl AnlzFile {
    pub fn to_bytes(&self) -> Vec<u8> {
        let body: Vec<u8> = self.sections.iter().flat_map(Section::to_bytes).collect();
        let mut out = Vec::with_capacity(28 + body.len());
        out.extend(b"PMAI");
        out.extend(28u32.to_be_bytes());
        out.extend((28 + body.len() as u32).to_be_bytes());
        out.extend(FILE_HEADER_TAIL);
        out.extend(body);
        out
    }
}

/// Builds a section from its type-specific header fields and payload.
fn section(tag: [u8; 4], header: &[u8], payload: &[u8]) -> Vec<u8> {
    let header_len = 12 + header.len();
    let mut out = Vec::with_capacity(header_len + payload.len());
    out.extend(tag);
    out.extend((header_len as u32).to_be_bytes());
    out.extend(((header_len + payload.len()) as u32).to_be_bytes());
    out.extend(header);
    out.extend(payload);
    out
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn beat_bytes(beat: &Beat) -> [u8; 8] {
    let mut out = [0; 8];
    out[0..2].copy_from_slice(&beat.bar_position.to_be_bytes());
    out[2..4].copy_from_slice(&beat.tempo.to_be_bytes());
    out[4..8].copy_from_slice(&beat.time_ms.to_be_bytes());
    out
}

fn utf16be_nul(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(u16::to_be_bytes).collect()
}

impl Section {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Section::BeatGrid(grid) => beat_grid(grid),
            Section::ExtendedBeatGrid(grid) => extended_beat_grid(grid),
            Section::Cues(list) if list.extended => extended_cues(list),
            Section::Cues(list) => basic_cues(list),
            Section::Path(path) => {
                let payload = utf16be_nul(path);
                section(*b"PPTH", &(payload.len() as u32).to_be_bytes(), &payload)
            }
            Section::Vbr(index) => section(*b"PVBR", &[0; 4], &be32(index)),
            Section::Waveform(waveform) => waveform_section(waveform),
            Section::SongStructure(structure) => song_structure(structure),
            Section::Unknown { bytes, .. } => bytes.clone(),
        }
    }
}

fn beat_grid(grid: &BeatGrid) -> Vec<u8> {
    let payload: Vec<u8> = grid.beats.iter().flat_map(beat_bytes).collect();
    section(*b"PQTZ", &be32(&[0, 0x0008_0000, grid.beats.len() as u32]), &payload)
}

fn extended_beat_grid(grid: &ExtendedBeatGrid) -> Vec<u8> {
    let mut header = be32(&[0, 0x0100_0002, 0]);
    header.extend(beat_bytes(&grid.first_beat));
    header.extend(beat_bytes(&grid.last_beat));
    header.extend(be32(&[grid.beat_count, grid.unknown, 0, 0]));
    let payload: Vec<u8> = grid.entries.iter().flat_map(|v| v.to_be_bytes()).collect();
    section(*b"PQT2", &header, &payload)
}

fn waveform_section(waveform: &Waveform) -> Vec<u8> {
    let count = waveform.column_count() as u32;
    let entry_len = waveform.entry_len as u32;
    let header = match waveform.kind {
        WaveformKind::Preview | WaveformKind::TinyPreview => {
            be32(&[waveform.data.len() as u32, 0x0001_0000])
        }
        WaveformKind::Detail | WaveformKind::ThreeBandDetail => {
            be32(&[entry_len, count, 0x0096_0000])
        }
        WaveformKind::ColorPreview => be32(&[entry_len, count, 0]),
        WaveformKind::ColorDetail => be32(&[entry_len, count, 0x0096_0305]),
        WaveformKind::ThreeBandPreview => be32(&[entry_len, count]),
    };
    section(waveform.kind.tag(), &header, &waveform.data)
}

fn list_kind(kind: CueListKind) -> u32 {
    match kind {
        CueListKind::Memory => 0,
        CueListKind::Hot => 1,
        CueListKind::Unknown(raw) => raw,
    }
}

fn cue_kind(kind: CueKind) -> u8 {
    match kind {
        CueKind::Cue => 1,
        CueKind::Loop => 2,
        CueKind::Unknown(raw) => raw,
    }
}

fn loop_end(cue: &Cue) -> u32 {
    cue.loop_end_ms.unwrap_or(u32::MAX)
}

fn basic_cues(list: &CueList) -> Vec<u8> {
    let count = list.cues.len();
    let mut header = list_kind(list.kind).to_be_bytes().to_vec();
    header.extend([0, 0]);
    header.extend((count as u16).to_be_bytes());
    header.extend(list.memory_count.to_be_bytes());

    let mut payload = Vec::with_capacity(count * 0x38);
    for (i, cue) in list.cues.iter().enumerate() {
        // Without stored values, link the entries as a list (0xffff marks the ends).
        let (previous, next) = cue.order.unwrap_or((
            if i == 0 { 0xffff } else { i as u16 - 1 },
            if i + 1 == count { 0xffff } else { i as u16 + 1 },
        ));
        let status = match cue.status.unwrap_or(CueStatus::Enabled) {
            CueStatus::Disabled => 0,
            CueStatus::Enabled => 1,
            CueStatus::ActiveLoop => 4,
            CueStatus::Unknown(raw) => raw,
        };
        payload.extend(b"PCPT");
        payload.extend(be32(&[0x1c, 0x38, cue.hot_cue.unwrap_or(0), status, 0x0001_0000]));
        payload.extend(previous.to_be_bytes());
        payload.extend(next.to_be_bytes());
        payload.extend([cue_kind(cue.kind), 0x00, 0x03, 0xe8]);
        payload.extend(be32(&[cue.time_ms, loop_end(cue)]));
        payload.extend([0; 16]);
    }
    section(*b"PCOB", &header, &payload)
}

fn extended_cues(list: &CueList) -> Vec<u8> {
    let mut header = list_kind(list.kind).to_be_bytes().to_vec();
    header.extend((list.cues.len() as u16).to_be_bytes());
    header.extend([0, 0]);

    let mut payload = Vec::new();
    for cue in &list.cues {
        let comment = cue.comment.as_deref().map(utf16be_nul).unwrap_or_default();
        let color = cue.hot_cue_color.map_or([0; 4], |c| [c.code, c.rgb[0], c.rgb[1], c.rgb[2]]);
        let len_entry = 0x2c + comment.len() + 4 + cue.trailer.len();

        payload.extend(b"PCP2");
        payload.extend(be32(&[0x10, len_entry as u32, cue.hot_cue.unwrap_or(0)]));
        payload.extend([cue_kind(cue.kind), 0x00, 0x03, 0xe8]);
        payload.extend(be32(&[cue.time_ms, loop_end(cue)]));
        payload.push(cue.color_id.unwrap_or(0));
        payload.extend([0; 7]);
        let (numerator, denominator) = cue.loop_beats.unwrap_or((0, 0));
        payload.extend(numerator.to_be_bytes());
        payload.extend(denominator.to_be_bytes());
        payload.extend((comment.len() as u32).to_be_bytes());
        payload.extend(comment);
        payload.extend(color);
        payload.extend(&cue.trailer);
    }
    section(*b"PCO2", &header, &payload)
}

fn song_structure(structure: &SongStructure) -> Vec<u8> {
    const ENTRY_LEN: usize = 24;
    let count = structure.phrases.len() as u16;
    let mood: u16 = match structure.mood {
        Mood::High => 1,
        Mood::Mid => 2,
        Mood::Low => 3,
        Mood::Unknown(raw) => raw,
    };

    let mut body = Vec::with_capacity(14 + structure.phrases.len() * ENTRY_LEN);
    body.extend(mood.to_be_bytes());
    body.extend([0; 6]);
    body.extend(structure.end_beat.to_be_bytes());
    body.extend(structure.unknown.to_be_bytes());
    body.extend([structure.bank, 0]);
    for phrase in &structure.phrases {
        let mut entry = [0u8; ENTRY_LEN];
        entry[0..2].copy_from_slice(&phrase.index.to_be_bytes());
        entry[2..4].copy_from_slice(&phrase.beat.to_be_bytes());
        entry[4..6].copy_from_slice(&phrase.kind.to_be_bytes());
        entry[7] = phrase.k1;
        entry[9] = phrase.k2;
        entry[11] = phrase.b;
        for (i, beat) in phrase.extra_beats.iter().enumerate() {
            entry[12 + 2 * i..14 + 2 * i].copy_from_slice(&beat.to_be_bytes());
        }
        entry[19] = phrase.k3;
        if let Some(fill) = phrase.fill_beat {
            entry[21] = 1;
            entry[22..24].copy_from_slice(&fill.to_be_bytes());
        }
        body.extend(entry);
    }
    if structure.masked {
        for (byte, mask) in body.iter_mut().zip(MASK.iter().cycle()) {
            *byte ^= mask.wrapping_add(count as u8);
        }
    }

    // The header ends 14 bytes into the body; split the body accordingly.
    let mut header = (ENTRY_LEN as u32).to_be_bytes().to_vec();
    header.extend(count.to_be_bytes());
    let in_header = 0x20 - BODY_START;
    header.extend(&body[..in_header]);
    section(*b"PSSI", &header, &body[in_header..])
}
