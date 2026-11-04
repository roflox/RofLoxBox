//! Importing controller mappings from the `MidiMappings/*.midi.csv` files of
//! local rekordbox installations (rekordbox 7 and older versions installed
//! side by side), plus any such files the user keeps in extra folders.
//!
//! Each row names a function, its MIDI message (`9`/`B` + channel, then the
//! note or controller, in hex), the channel used by decks 1–4 (or a whole
//! message per deck), and the message that lights the matching LED. Column
//! positions are taken from the file's header row. The files are read at run
//! time; deckhand does not include them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::action::{
    Action, DECKS, DeckAction, DeckFeedback, Feedback, GlobalAction, GlobalFeedback, PadMode,
};
use crate::mapping::{Binding, Encoding, Mapping, MessageType, Output, ShiftControl};

/// The result of an import: the mapping and the functions without a deckhand
/// equivalent yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    pub mapping: Mapping,
    pub unsupported: Vec<String>,
}

/// `MidiMappings` directories of rekordbox installations on this computer,
/// newest version first.
pub fn mapping_dirs() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if cfg!(windows) {
        // Installers have used both `Program Files\Pioneer` and `Program Files\rekordbox`.
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(var) {
                let dir = PathBuf::from(dir);
                roots.push(dir.join("Pioneer"));
                roots.push(dir.join("rekordbox"));
                roots.push(dir);
            }
        }
    } else {
        roots.push(PathBuf::from("/Applications"));
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Applications"));
        }
    }
    let mut dirs: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_lowercase().starts_with("rekordbox"))
        })
        .map(|p| {
            if cfg!(windows) {
                p.join("MidiMappings")
            } else {
                p.join("rekordbox.app/Contents/Resources/MidiMappings")
            }
        })
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.reverse();
    dirs
}

/// All mapping files as (controller name, path). Installed rekordbox
/// versions come first (newest wins); files in `extra` folders add
/// controllers no installed version has.
pub fn mapping_files_with(extra: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for dir in mapping_dirs().into_iter().chain(extra.iter().cloned()) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            let Some(name) =
                path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".midi.csv"))
            else {
                continue;
            };
            if !files.iter().any(|(n, _)| n == name) {
                files.push((name.to_owned(), path.clone()));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

pub fn mapping_files() -> Vec<(String, PathBuf)> {
    mapping_files_with(&[])
}

/// A controller name reduced for matching: letters and digits, upper case,
/// without the vendor prefix, and whether it names the unit's MIDI mode.
fn core_name(text: &str) -> (String, bool) {
    let mut name: String =
        text.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    for prefix in ["PIONEERDJ", "PIONEER", "ALPHATHETA"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest.to_owned();
            break;
        }
    }
    let midi = name.ends_with("MIDI");
    if midi {
        name.truncate(name.len() - 4);
    }
    (name, midi)
}

/// The mapping file for a MIDI port, matched by controller name.
pub fn find_for_port_with(port: &str, extra: &[PathBuf]) -> Option<(String, PathBuf)> {
    let (port_name, port_midi) = core_name(port);
    mapping_files_with(extra)
        .into_iter()
        .filter_map(|(name, path)| {
            let (core, midi) = core_name(&name);
            if core.is_empty() || !port_name.contains(&core) {
                return None;
            }
            // Prefer an exact name, the same MIDI-mode flavor, then the longest match.
            let score = (usize::from(core == port_name) * 1000)
                + (usize::from(midi == port_midi) * 100)
                + core.len();
            Some((score, name, path))
        })
        .max_by_key(|(score, ..)| *score)
        .map(|(_, name, path)| (name, path))
}

pub fn find_for_port(port: &str) -> Option<(String, PathBuf)> {
    find_for_port_with(port, &[])
}

pub fn import_file(path: &Path) -> anyhow::Result<Import> {
    let bytes = std::fs::read(path)?;
    // Some older files are not valid UTF-8; their content is ASCII apart from comments.
    let text = String::from_utf8_lossy(&bytes);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".midi.csv"))
        .unwrap_or("controller");
    Ok(import(name, &text))
}

/// A 4-digit hex message: (status, data byte).
fn parse_code(text: &str) -> Option<(u8, u8)> {
    let text = text.trim();
    if text.len() != 4 {
        return None;
    }
    let value = u16::from_str_radix(text, 16).ok()?;
    Some(((value >> 8) as u8, value as u8))
}

/// The message for one deck: a deck column holds either a channel number
/// (replacing the channel of the base message) or a whole message.
fn deck_message(base: Option<(u8, u8)>, column: &str) -> Option<(u8, u8)> {
    let column = column.trim();
    if let Some(code) = parse_code(column) {
        return Some(code);
    }
    let channel: u8 = column.parse().ok()?;
    let (status, number) = base?;
    Some((status & 0xf0 | (channel & 0x0f), number))
}

enum Target {
    Deck(DeckAction),
    Global(GlobalAction),
    /// A per-channel control that rekordbox lists as global (CH1–CH4 knobs).
    Channel(usize, DeckAction),
}

/// Pad rows named only by mode (`PAD3_PadMode1`) describe their function in
/// the comment; the meaning of each mode differs between controllers.
fn pad_by_comment(comment: &str, pad: u8) -> Option<DeckAction> {
    let comment = comment.trim().to_ascii_uppercase();
    if comment.starts_with("DELETE HOT CUE") {
        Some(DeckAction::HotCueDelete(pad))
    } else if comment.starts_with("HOT CUE") {
        Some(DeckAction::HotCue(pad))
    } else if comment.starts_with("BEAT JUMP") {
        Some(DeckAction::BeatJump(pad))
    } else {
        None
    }
}

fn is_mode_pad(name: &str) -> bool {
    name.starts_with("PAD") && name.contains("_PadMode")
}

fn target(name: &str, number: u8, comment: &str) -> Option<Target> {
    use DeckAction as D;
    use GlobalAction as G;
    let pad = number & 0x07;
    Some(match name {
        "PlayPause" => Target::Deck(D::PlayPause),
        "Cue" => Target::Deck(D::Cue),
        "JumpToTrackStart" => Target::Deck(D::JumpToStart),
        "JogScratch" => Target::Deck(D::JogScratch),
        "JogPitchBend" | "WheelPitchBend" => Target::Deck(D::JogTurn),
        "WheelSearch" | "JogSearch" => Target::Deck(D::JogSearch),
        "JogTouch" => Target::Deck(D::JogTouch),
        "TempoSlider" => Target::Deck(D::TempoFader),
        "MasterTempo" => Target::Deck(D::KeyLock),
        "TempoRange" => Target::Deck(D::TempoRangeCycle),
        "Sync" => Target::Deck(D::Sync),
        "Master" => Target::Deck(D::SyncMaster),
        "Quantize" => Target::Deck(D::Quantize),
        "Slip" => Target::Deck(D::Slip),
        "Vinyl" => Target::Deck(D::Vinyl),
        "ActivePartVocal" => Target::Deck(D::StemPart(0)),
        "ActivePartInst" => Target::Deck(D::StemPart(1)),
        "ActivePartBass" => Target::Deck(D::StemPart(2)),
        "ActivePartDrums" => Target::Deck(D::StemPart(3)),
        "PartIsolatorVocal" => Target::Deck(D::PartLevel(0)),
        "PartIsolatorInst" => Target::Deck(D::PartLevel(1)),
        "PartIsolatorDrums" => Target::Deck(D::PartLevel(2)),
        "PartIsolator" => Target::Deck(D::PartIsolator),
        "LoopIn" => Target::Deck(D::LoopIn),
        "LoopOut" => Target::Deck(D::LoopOut),
        "ReloopExit" => Target::Deck(D::ReloopExit),
        "4BeatLoopLongPress" | "4BeatLoop" | "AutoLoop" => Target::Deck(D::AutoLoop),
        "LoopHalf" => Target::Deck(D::LoopHalf),
        "LoopDouble" => Target::Deck(D::LoopDouble),
        "LoopMoveLeft" => Target::Deck(D::LoopMoveBack),
        "LoopMoveRight" => Target::Deck(D::LoopMoveForward),
        // Fixed beat-loop buttons of older controllers (DDJ-SB and similar).
        "BeatLoop1_4" => Target::Deck(D::BeatLoop(0)),
        "BeatLoop1_2" => Target::Deck(D::BeatLoop(1)),
        "BeatLoop1" => Target::Deck(D::BeatLoop(2)),
        "BeatLoop2" => Target::Deck(D::BeatLoop(3)),
        "BeatLoop4" => Target::Deck(D::BeatLoop(4)),
        "BeatLoop8" => Target::Deck(D::BeatLoop(5)),
        "Parameter1Left_HotCue" => Target::Deck(D::MemoryPrev),
        "Parameter1Right_HotCue" => Target::Deck(D::MemoryNext),
        "Parameter1Left_BeatJump" => Target::Deck(D::BeatJumpRangeHalf),
        "Parameter1Right_BeatJump" => Target::Deck(D::BeatJumpRangeDouble),
        "Memory" => Target::Deck(D::MemorySet),
        "Delete" => Target::Deck(D::MemoryDelete),
        "Load" => Target::Deck(D::Load),
        "ChannelFader" => Target::Deck(D::Volume),
        "Gain" => Target::Deck(D::Trim),
        "EQHigh" => Target::Deck(D::EqHigh),
        "EQMid" => Target::Deck(D::EqMid),
        "EQLow" => Target::Deck(D::EqLow),
        "HeadphoneCue" => Target::Deck(D::HeadphoneCue),
        "CrossfaderAssign.A" => Target::Deck(D::CrossfaderAssignA),
        "CrossfaderAssign.Thru" => Target::Deck(D::CrossfaderAssignThru),
        "CrossfaderAssign.B" => Target::Deck(D::CrossfaderAssignB),
        "CrossFader" => Target::Global(G::Crossfader),
        "MasterLevel" => Target::Global(G::MasterVolume),
        "HeadphonesMix" => Target::Global(G::HeadphoneMix),
        "HeadphonesVolume" => Target::Global(G::HeadphoneVolume),
        "Browse" => Target::Global(G::BrowseTurn),
        "Forward" => Target::Global(G::BrowseForward),
        "Back" => Target::Global(G::BrowseBack),
        "Zoom" => Target::Global(G::Zoom),
        "BrowseJumpUp" => Target::Global(G::BrowsePageUp),
        "BrowseJumpDown" => Target::Global(G::BrowsePageDown),
        "BrowseJumpTop" => Target::Global(G::BrowseTop),
        "BrowseJumpBottom" => Target::Global(G::BrowseBottom),
        "SwitchActiveWindow" => Target::Global(G::SwitchFocus),
        "MasterCue" => Target::Global(G::MasterCue),
        "CFXOn" => Target::Global(G::ColorFxOn),
        "CFXSelect" => Target::Global(G::ColorFxSelect),
        "FX1-1" => Target::Global(G::FxDepth),
        "FX1-1On" => Target::Global(G::FxOn),
        "FX1ReleaseFXOn" => Target::Global(G::FxRelease),
        "FX1BeatDown" => Target::Global(G::FxBeatDown),
        "FX1BeatUp" => Target::Global(G::FxBeatUp),
        "FX1AutoBeat" => Target::Global(G::FxAutoBeat),
        "FX1TapBeat" => Target::Global(G::FxTap),
        "FX1Assign.MASTER" => Target::Global(G::FxAssign(4)),
        "FX1Assign.SAMPLER" => Target::Global(G::FxAssign(5)),
        "CrossFaderStartPlay.A" => Target::Global(G::CrossfaderStartPlayA),
        "CrossFaderStartPlay.B" => Target::Global(G::CrossfaderStartPlayB),
        "CrossFaderStartCue.A" => Target::Global(G::CrossfaderStartCueA),
        "CrossFaderStartCue.B" => Target::Global(G::CrossfaderStartCueB),
        "TapBpm" => Target::Deck(D::TapBpm),
        "KeySync" => Target::Deck(D::KeySync),
        "KeyReset" => Target::Deck(D::KeyReset),
        "Parameter1Left_Sampler" => Target::Deck(D::SamplerBankDown),
        "Parameter1Right_Sampler" => Target::Deck(D::SamplerBankUp),
        "Parameter1Left_PadFx" => Target::Deck(D::PadFxPageDown),
        "Parameter1Right_PadFx" => Target::Deck(D::PadFxPageUp),
        "Parameter2Left_BeatJump" => Target::Deck(D::BeatJumpRangeSixteenth),
        "Parameter2Right_BeatJump" => Target::Deck(D::BeatJumpRangeSixteen),
        "Parameter1Left_KeyShift" => Target::Deck(D::KeyShiftPageDown),
        "Parameter1Right_KeyShift" => Target::Deck(D::KeyShiftPageUp),
        "Parameter1Left_Keyboard" => Target::Deck(D::KeyboardDown),
        "Parameter1Right_Keyboard" => Target::Deck(D::KeyboardUp),
        "ActiveLoop" => Target::Deck(D::ActiveLoop),
        // DECK button, and the controller reporting which deck is active.
        "Deck" | "DeckState" => Target::Deck(D::Select),
        "ChannelFaderStartPlay" => Target::Deck(D::FaderStartPlay),
        "ChannelFaderStartCue" => Target::Deck(D::FaderStartCue),
        _ if pad_mode(name).is_some() => Target::Deck(D::PadMode(pad_mode(name)?)),
        _ => {
            if let Some(kind) = name.strip_prefix("FX1-1Select.") {
                return BEAT_FX_NAMES
                    .iter()
                    .position(|&n| n == kind)
                    .map(|i| Target::Global(G::FxType(i as u8)));
            }
            if let Some(ch) = name.strip_prefix("FX1Assign.CH").and_then(|n| n.parse::<u8>().ok()) {
                return (1..=DECKS as u8)
                    .contains(&ch)
                    .then_some(Target::Global(G::FxAssign(ch - 1)));
            }
            if let Some(ch) =
                name.strip_prefix("CFXParameterCH").and_then(|n| n.parse::<usize>().ok())
            {
                return (1..=DECKS).contains(&ch).then_some(Target::Channel(ch - 1, D::Filter));
            }
            if !name.starts_with("PAD") {
                return None;
            }
            if is_mode_pad(name) {
                return pad_by_comment(comment, pad).map(Target::Deck);
            }
            if let Some(slot) = pad_fx_slot(name) {
                Target::Deck(D::PadFx(slot))
            } else if name.ends_with("_HotCue") {
                Target::Deck(D::HotCue(pad))
            } else if name.ends_with("_HotCue+Shift") {
                Target::Deck(D::HotCueDelete(pad))
            } else if name.ends_with("_BeatJump") {
                Target::Deck(D::BeatJump(pad))
            } else if name.ends_with("_BeatLoop") {
                Target::Deck(D::BeatLoop(pad))
            } else if name.ends_with("_KeyShift") {
                Target::Deck(D::KeyShift(pad))
            } else if name.ends_with("_Keyboard") {
                Target::Deck(D::Keyboard(pad))
            } else if name.ends_with("_Keyboard+Shift") && pad < 6 {
                // Shift + pads 1–6 in Keyboard mode delete hot cues.
                Target::Deck(D::HotCueDelete(pad))
            } else if name.ends_with("_Sampler") {
                Target::Deck(D::SamplerPlay(pad))
            } else if name.ends_with("_Sampler+Shift") {
                Target::Deck(D::SamplerStop(pad))
            } else {
                return None;
            }
        }
    })
}

/// Beat FX type names as in `FX1-1Select.<NAME>`, in `FxType` order.
const BEAT_FX_NAMES: [&str; 14] = [
    "DELAY",
    "ECHO",
    "LOWCUTECHO",
    "SPIRAL",
    "HELIX",
    "REVERB",
    "FLANGER",
    "PHASER",
    "FILTER",
    "TRANS",
    "PITCH",
    "ROLL",
    "MOBIUSSAW",
    "MOBIUSTRI",
];

/// Pad-mode buttons, named the same in every mapping file.
fn pad_mode(name: &str) -> Option<PadMode> {
    Some(match name {
        "HotCue" => PadMode::HotCue,
        "Keyboard" => PadMode::Keyboard,
        "Stems" => PadMode::Stems,
        "PadFx1" | "PadFx" => PadMode::PadFx,
        "BeatJump" => PadMode::BeatJump,
        "BeatLoop" => PadMode::BeatLoop,
        "Sampler" => PadMode::Sampler,
        "KeyShift" => PadMode::KeyShift,
        _ => return None,
    })
}

/// `PAD9_PadFx1` → slot 8.
fn pad_fx_slot(name: &str) -> Option<u8> {
    let (pad, rest) = name.strip_prefix("PAD")?.split_once('_')?;
    rest.starts_with("PadFx").then_some(())?;
    let pad: u8 = pad.parse().ok()?;
    (1..=16).contains(&pad).then_some(pad - 1)
}

/// What an output row shows: a deck's state or a mixer-wide one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Shows {
    Deck(DeckFeedback),
    Global(GlobalFeedback),
}

fn feedback(name: &str, number: u8, comment: &str) -> Option<Shows> {
    match name {
        "FX1-1On" => return Some(Shows::Global(GlobalFeedback::FxOn)),
        "FX1ReleaseFXOn" => return Some(Shows::Global(GlobalFeedback::FxRelease)),
        "CFXOn" => return Some(Shows::Global(GlobalFeedback::ColorFxOn)),
        "MasterCue" => return Some(Shows::Global(GlobalFeedback::MasterCue)),
        "FX1BeatMultiplier" => return Some(Shows::Global(GlobalFeedback::FxBeat)),
        _ => {}
    }
    deck_feedback(name, number, comment).map(Shows::Deck)
}

fn deck_feedback(name: &str, number: u8, comment: &str) -> Option<DeckFeedback> {
    use DeckFeedback as F;
    Some(match name {
        "KeySync" => F::KeySync,
        _ if pad_mode(name).is_some() => F::PadMode(pad_mode(name)?),
        _ if pad_fx_slot(name).is_some() => F::PadLit(PadMode::PadFx, pad_fx_slot(name)?),
        _ if name.starts_with("PAD") && name.ends_with("_BeatJump") => {
            F::PadLit(PadMode::BeatJump, number & 0x07)
        }
        _ if name.starts_with("PAD") && name.ends_with("_BeatLoop") => {
            F::PadLit(PadMode::BeatLoop, number & 0x07)
        }
        _ if name.starts_with("PAD") && name.ends_with("_KeyShift") => {
            F::PadLit(PadMode::KeyShift, number & 0x07)
        }
        _ if name.starts_with("PAD") && name.ends_with("_Keyboard") => {
            F::PadLit(PadMode::Keyboard, number & 0x07)
        }
        "PlayPause" => F::Playing,
        "Cue" => F::AtCue,
        "Sync" => F::Sync,
        "Master" => F::SyncMaster,
        "Quantize" => F::Quantize,
        "Slip" => F::Slip,
        "Vinyl" => F::Vinyl,
        "ActivePartVocal" => F::StemOn(0),
        "ActivePartInst" => F::StemOn(1),
        "ActivePartBass" => F::StemOn(2),
        "ActivePartDrums" => F::StemOn(3),
        "PartIsolator" => F::PartIsolator,
        "MasterTempo" => F::KeyLock,
        "HeadphoneCue" => F::HeadphoneCue,
        "LoopIn" | "LoopOut" | "GetLoopActive" => F::LoopActive,
        "ChannelLevel" => F::Level,
        "LoadedIndicator" => F::Loaded,
        _ if name.starts_with("PAD") && name.ends_with("_HotCue") => F::HotCueSet(number & 0x07),
        _ if name.starts_with("PAD") && name.ends_with("_Sampler") => {
            F::SamplerPlaying(number & 0x07)
        }
        _ if is_mode_pad(name)
            && pad_by_comment(comment, number & 0x07)
                == Some(DeckAction::HotCue(number & 0x07)) =>
        {
            F::HotCueSet(number & 0x07)
        }
        _ => return None,
    })
}

fn encoding(kind: &str) -> Option<(MessageType, Encoding)> {
    Some(match kind {
        "Button" | "Pad" | "JogTouch" => (MessageType::Note, Encoding::Button),
        "KnobSliderHiRes" => (MessageType::Cc, Encoding::Absolute14),
        "KnobSlider" => (MessageType::Cc, Encoding::Absolute7),
        "JogRotate" => (MessageType::Cc, Encoding::RelativeOffset64),
        "Rotary" => (MessageType::Cc, Encoding::RelativeTwos),
        _ => return None,
    })
}

fn message_type(status: u8) -> Option<MessageType> {
    match status & 0xf0 {
        0x90 => Some(MessageType::Note),
        0xb0 => Some(MessageType::Cc),
        _ => None,
    }
}

/// Column positions, from the header row (`#name,function,type,input,deck1…`).
struct Columns {
    name: usize,
    function: usize,
    kind: usize,
    input: usize,
    input_decks: Vec<usize>,
    output: Option<usize>,
    output_decks: Vec<usize>,
    comment: Option<usize>,
}

impl Columns {
    /// The layout of every file seen so far, used when there is no header.
    fn standard() -> Self {
        Self {
            name: 0,
            function: 1,
            kind: 2,
            input: 3,
            input_decks: vec![4, 5, 6, 7],
            output: Some(8),
            output_decks: vec![9, 10, 11, 12],
            comment: Some(14),
        }
    }

    fn from_header(line: &str) -> Option<Self> {
        let names: Vec<String> = line
            .trim_start_matches('#')
            .split(',')
            .map(|c| c.trim().to_ascii_lowercase())
            .collect();
        let find = |wanted: &str| names.iter().position(|n| n == wanted);
        let (input, output) = (find("input")?, find("output"));
        // Deck columns follow `input` (and `output`) and are named deck1, deck2, ….
        let decks_after = |start: usize| -> Vec<usize> {
            (start + 1..names.len()).take_while(|&i| names[i].starts_with("deck")).collect()
        };
        Some(Self {
            name: find("name")?,
            function: find("function").unwrap_or(usize::MAX),
            kind: find("type")?,
            input,
            input_decks: decks_after(input),
            output,
            output_decks: output.map(decks_after).unwrap_or_default(),
            comment: find("comment"),
        })
    }
}

/// Converts the text of a rekordbox mapping file.
pub fn import(name: &str, text: &str) -> Import {
    let mut mapping = Mapping { name: name.to_owned(), ..Mapping::default() };
    let mut unsupported: Vec<String> = Vec::new();
    let mut columns = Columns::standard();
    // Feedback kinds already given an output by an unshifted row.
    let mut mapped_feedback: HashMap<Shows, ()> = HashMap::new();

    // Files use \n, \r\n or (older ones) a bare \r between lines.
    for line in text.split(['\r', '\n']) {
        if line.starts_with("#name") || line.starts_with("# name") {
            if let Some(found) = Columns::from_header(line) {
                columns = found;
            }
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        let get = |i: usize| cols.get(i).copied().unwrap_or("").trim();
        let row = get(columns.name);
        if row.is_empty() || row.starts_with('#') || row.starts_with('@') {
            continue;
        }
        // Comments may contain commas; they run to the end of the line.
        let comment = columns
            .comment
            .map(|i| cols.get(i..).map(|c| c.join(",")).unwrap_or_default())
            .unwrap_or_default();
        let (function, kind) = (get(columns.function), get(columns.kind));
        let input = parse_code(get(columns.input));
        let output = columns.output.and_then(|i| parse_code(get(i)));
        let shifted = function.contains("+Shift") || row.ends_with("+Shift");
        let input_deck = |d: usize| columns.input_decks.get(d).map_or("", |&i| get(i));
        let output_deck = |d: usize| columns.output_decks.get(d).map_or("", |&i| get(i));

        // The Shift buttons, one per deck: kept as modifiers so Shift
        // combinations can be learned on top of the imported mapping.
        if row == "Shift" {
            for deck in 0..DECKS {
                if let Some((status, number)) = deck_message(input, input_deck(deck))
                    && let Some(message) = message_type(status)
                {
                    let control = ShiftControl { message, channel: (status & 0x0f) + 1, number };
                    if !mapping.shift_controls.contains(&control) {
                        mapping.shift_controls.push(control);
                    }
                }
            }
            continue;
        }

        // Inputs.
        if let Some((message, encoding)) = encoding(kind) {
            let number = input
                .map(|(_, n)| n)
                .or_else(|| (0..DECKS).find_map(|d| parse_code(input_deck(d))).map(|(_, n)| n));
            match number.and_then(|n| target(row, n, &comment)) {
                Some(target) => {
                    let messages: Vec<(Action, Option<(u8, u8)>)> = match target {
                        Target::Deck(action) => (0..DECKS)
                            .map(|d| (Action::Deck(d, action), deck_message(input, input_deck(d))))
                            .collect(),
                        Target::Global(action) => vec![(Action::Global(action), input)],
                        Target::Channel(deck, action) => vec![(Action::Deck(deck, action), input)],
                    };
                    for (action, code) in messages {
                        let Some((status, number)) = code else { continue };
                        if message_type(status) != Some(message) {
                            continue;
                        }
                        let lsb = (encoding == Encoding::Absolute14).then_some(number + 0x20);
                        let binding = Binding {
                            action,
                            message,
                            channel: (status & 0x0f) + 1,
                            number,
                            encoding,
                            lsb,
                            invert: false,
                            shift: false,
                        };
                        if !mapping.bindings.contains(&binding) {
                            mapping.bindings.push(binding);
                        }
                    }
                }
                None if input.is_some() || (0..DECKS).any(|d| !input_deck(d).is_empty()) => {
                    if !unsupported.iter().any(|u| u == row) {
                        unsupported.push(row.to_owned());
                    }
                }
                None => {}
            }
        }

        // Outputs (LEDs, meters). A shifted row repeating a function that was
        // already mapped (a second PlayPause row) describes another LED state.
        let output_number = output
            .map(|(_, n)| n)
            .or_else(|| (0..DECKS).find_map(|d| parse_code(output_deck(d))).map(|(_, n)| n));
        let Some(what) = output_number.and_then(|n| feedback(row, n, &comment)) else { continue };
        if shifted && mapped_feedback.contains_key(&what) {
            continue;
        }
        let targets: Vec<(Feedback, Option<(u8, u8)>)> = match what {
            Shows::Deck(shown) => (0..DECKS)
                .map(|deck| (Feedback::Deck(deck, shown), deck_message(output, output_deck(deck))))
                .collect(),
            Shows::Global(shown) => vec![(Feedback::Global(shown), output)],
        };
        for (feedback, code) in targets {
            let Some((status, number)) = code else { continue };
            let Some(message) = message_type(status) else { continue };
            let out = Output { feedback, message, channel: (status & 0x0f) + 1, number };
            if !mapping.outputs.contains(&out) {
                mapping.outputs.push(out);
            }
        }
        if !shifted {
            mapped_feedback.insert(what, ());
        }
    }
    mapping.offered = mapping.bindings.iter().map(|b| b.action.to_string()).collect();
    mapping.offered.sort();
    mapping.offered.dedup();
    Import { mapping, unsupported }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rows written in the format of rekordbox's mapping files.
    const SAMPLE: &str = "\
@file,1,TEST
#name,function,type,input,deck1,deck2,deck3,deck4,output,deck1,deck2,deck3,deck4,option,comment
# Deck,,,,,,,,,,,,,,
PlayPause,PlayPause,Button,900B,0,1,2,3,900B,0,1,2,3,Fast,Play/Pause
TempoSlider,TempoSlider,KnobSliderHiRes,B000,0,1,2,3,,,,,,Fast,Tempo
JogScratch,JogScratch,JogRotate,B022,0,1,2,3,,,,,,RO,Scratch
Browse,Browse,Rotary,B640,,,,,,,,,,,Browse
PAD3_HotCue,PAD3.HotCue,Pad,9002,7,9,11,13,9002,7,9,11,13,Fast,HOT CUE3
PAD3_HotCue+Shift,PAD3.HotCue+Shift,Pad,9002,8,10,12,14,9002,8,10,12,14,Fast,Delete
CFXParameterCH2,CFXParameterCH2,KnobSliderHiRes,B618,,,,,,,,,,Fast,CH2 CFX
ChannelLevel,ChannelLevel,Indicator,,,,,,B002,0,1,2,3,RO,Level
LoadedIndicator,LoadedIndicator,Indicator,,,,,,,9F00,9F01,9F02,9F03,RO,Loaded
FX1BeatMultiplier,FX1BeatMultiplier,Indicator,,,,,,B464,,,,,,FX1 BeatMultiplier No
Vinyl,Slip+Shift,Button,9017,0,1,2,3,9017,0,1,2,3,,Vinyl Mode
PlayPause,PlayPause+Shift,Button,9047,0,1,2,3,9047,0,1,2,3,Fast,Play/Pause
DrumSwap1,DrumSwap1,Button,9000,0,1,2,3,9000,0,1,2,3,Fast,Drum
";

    fn find(mapping: &Mapping, action: &str) -> Binding {
        let action: Action = action.parse().unwrap();
        mapping
            .bindings
            .iter()
            .find(|b| b.action == action)
            .cloned()
            .unwrap_or_else(|| panic!("{action} missing"))
    }

    #[test]
    fn imports_rows() {
        let Import { mapping, unsupported } = import("TEST", SAMPLE);
        let play3 = find(&mapping, "deck3.play_pause");
        assert_eq!((play3.message, play3.channel, play3.number), (MessageType::Note, 3, 0x0b));
        let tempo = find(&mapping, "deck2.tempo");
        assert_eq!(
            (tempo.channel, tempo.number, tempo.lsb, tempo.encoding),
            (2, 0, Some(0x20), Encoding::Absolute14)
        );
        assert_eq!(find(&mapping, "deck1.jog_scratch").encoding, Encoding::RelativeOffset64);
        assert_eq!(find(&mapping, "browse_turn").channel, 7);
        let pad = find(&mapping, "deck2.hot_cue_c");
        assert_eq!((pad.channel, pad.number), (10, 2));
        assert_eq!(find(&mapping, "deck2.hot_cue_c_delete").channel, 11);
        let filter = find(&mapping, "deck2.filter");
        assert_eq!((filter.channel, filter.number), (7, 0x18));
        assert_eq!(unsupported, ["DrumSwap1"]);
        let beat = mapping
            .outputs
            .iter()
            .find(|o| o.feedback == Feedback::Global(GlobalFeedback::FxBeat))
            .expect("beat indicator");
        assert_eq!((beat.message, beat.channel, beat.number), (MessageType::Cc, 5, 0x64));

        let output = |text: &str| {
            let feedback: Feedback = text.parse().unwrap();
            mapping.outputs.iter().find(|o| o.feedback == feedback).cloned().unwrap()
        };
        assert_eq!((output("deck4.playing").channel, output("deck4.playing").number), (4, 0x0b));
        assert_eq!(output("deck1.hot_cue_c_set").channel, 8);
        assert_eq!(output("deck3.level").message, MessageType::Cc);
        assert_eq!(output("deck2.vinyl").number, 0x17);
        let playing: Vec<u8> = mapping
            .outputs
            .iter()
            .filter(|o| o.feedback == "deck1.playing".parse().unwrap())
            .map(|o| o.number)
            .collect();
        assert_eq!(playing, [0x0b], "the shifted PlayPause row is another LED state");
        let loaded = output("deck2.loaded");
        assert_eq!((loaded.channel, loaded.number), (16, 0x01));
    }

    /// Older layouts: generic pad names with the function in the comment,
    /// legacy function names, two deck columns, comments containing commas.
    #[test]
    fn imports_older_layouts() {
        let text = "\
#name,function,type,input,deck1,deck2,output,deck1,deck2,option,comment
PAD1_PadMode1,PAD1_PadMode1,Pad,9000,7,9,9000,7,9,Fast,HOT CUE1
PAD1_PadMode1+Shift,PAD1_PadMode1+Shift,Pad,9000,8,10,9000,8,10,Fast,Delete HOT CUE1
PAD2_PadMode3,PAD2_PadMode3,Pad,9021,7,9,9021,7,9,Fast,BEAT JUMP (FWD1)
PAD1_PadMode4,PAD1_PadMode4,Pad,9030,7,9,9030,7,9,Fast,Sample Slot 1 play
BeatLoop4,BeatLoop4,Button,9014,0,1,,,,,4 beat loop
LoopMoveLeft,LoopMoveLeft,Button,9015,0,1,,,,,Move loop, left
MasterLevel,MasterLevel,KnobSliderHiRes,B60D,,,,,,,Master
CrossfaderAssign.B,CrossfaderAssign.B,Button,9070,0,1,,,,,Crossfader assign
";
        let Import { mapping, unsupported } = import("OLD", text);
        assert_eq!(find(&mapping, "deck2.hot_cue_a").channel, 10);
        assert_eq!(find(&mapping, "deck1.hot_cue_a_delete").channel, 9);
        assert_eq!(find(&mapping, "deck1.beat_jump_2").number, 0x21);
        assert_eq!(find(&mapping, "deck2.beat_loop_5").channel, 2);
        find(&mapping, "deck1.loop_move_back");
        find(&mapping, "master_volume");
        find(&mapping, "deck2.crossfader_assign_b");
        assert!(
            mapping.bindings.iter().all(|b| !matches!(b.action, Action::Deck(2 | 3, _))),
            "only two deck columns"
        );
        assert_eq!(unsupported, ["PAD1_PadMode4"]);
        let lit =
            mapping.outputs.iter().any(|o| o.feedback == "deck1.hot_cue_a_set".parse().unwrap());
        assert!(lit, "hot cue pad LEDs from comment-named pads");
    }

    /// rekordbox 6's DJM-S9 file separates lines with a bare carriage return.
    #[test]
    fn imports_classic_mac_line_endings() {
        let text = SAMPLE.replace('\n', "\r");
        let Import { mapping, .. } = import("CR", &text);
        assert_eq!(mapping, import("TEST", SAMPLE).mapping.clone().renamed("CR"));
    }

    #[test]
    fn port_names_match_files_without_vendor_prefix() {
        assert_eq!(core_name("PIONEER DDJ-SX"), ("DDJSX".into(), false));
        assert_eq!(core_name("Pioneer DJ XDJ-RR MIDI"), ("XDJRR".into(), true));
        assert_eq!(core_name("DDJ-GRV6"), ("DDJGRV6".into(), false));
    }

    /// Files in an extra folder (for example from rekordbox 6) are found for
    /// controllers that no installed rekordbox knows.
    #[test]
    fn extra_folder_supplies_missing_controllers() {
        let dir =
            std::env::temp_dir().join(format!("deckhand-extra-mappings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("DDJ-OLDTEST6.midi.csv"), SAMPLE).unwrap();
        let found = find_for_port_with("PIONEER DDJ-OLDTEST6", std::slice::from_ref(&dir));
        assert_eq!(found.as_ref().map(|(n, _)| n.as_str()), Some("DDJ-OLDTEST6"));
        let import = import_file(&found.unwrap().1).unwrap();
        find(&import.mapping, "deck1.play_pause");
        assert!(find_for_port("PIONEER DDJ-OLDTEST6").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Converts the GRV6 file of a local rekordbox installation, if present.
    #[test]
    fn imports_installed_grv6_file() {
        let Some((_, path)) = mapping_files().into_iter().find(|(name, _)| name == "DDJ-GRV6")
        else {
            eprintln!("rekordbox with DDJ-GRV6 mapping not installed; skipping");
            return;
        };
        let Import { mapping, unsupported } = import_file(&path).unwrap();
        for action in [
            "deck1.play_pause",
            "deck4.cue",
            "deck2.jog_scratch",
            "deck1.hot_cue_h",
            "deck3.beat_jump_8",
            "crossfader",
        ] {
            find(&mapping, action);
        }
        for action in
            ["deck2.memory_prev", "deck2.memory_next", "deck4.memory_set", "deck1.memory_delete"]
        {
            find(&mapping, action);
        }
        assert!(mapping.bindings.len() > 200, "{} bindings", mapping.bindings.len());
        assert!(!mapping.outputs.is_empty());
        eprintln!(
            "GRV6: {} bindings, {} outputs, unsupported: {unsupported:?}",
            mapping.bindings.len(),
            mapping.outputs.len()
        );
        assert_eq!(find_for_port("DDJ-GRV6").map(|(n, _)| n).as_deref(), Some("DDJ-GRV6"));
    }

    /// Port matching for the installed files: vendor prefixes and MIDI-mode variants.
    #[test]
    fn installed_files_match_typical_port_names() {
        let files = mapping_files();
        let has = |name: &str| files.iter().any(|(n, _)| n == name);
        if !has("PIONEER DDJ-SX") || !has("Pioneer DJ XDJ-RR MIDI") {
            eprintln!("rekordbox mapping files not installed; skipping");
            return;
        }
        let found = |port: &str| find_for_port(port).map(|(n, _)| n);
        assert_eq!(found("DDJ-SX").as_deref(), Some("PIONEER DDJ-SX"));
        assert_eq!(found("PIONEER DDJ-SX").as_deref(), Some("PIONEER DDJ-SX"));
        assert_eq!(found("XDJ-RR MIDI").as_deref(), Some("Pioneer DJ XDJ-RR MIDI"));
        assert_eq!(found("XDJ-RR").as_deref(), Some("Pioneer DJ XDJ-RR"));
        assert_eq!(found("DDJ-SX2").as_deref(), Some("PIONEER DDJ-SX2"));
    }

    #[test]
    fn shift_rows_become_shift_buttons() {
        let text = "#name,function,type,input,deck1,deck2,deck3,deck4,output,deck1,deck2,deck3,deck4,option,comment\n\
                    Shift,Shift,Button,903F,0,1,2,3,,,,,,RO,Shift\n";
        let import = import("T", text);
        let shifts = &import.mapping.shift_controls;
        assert_eq!(shifts.len(), 4);
        assert_eq!((shifts[1].channel, shifts[1].number), (2, 0x3F));
        assert!(import.unsupported.is_empty());
    }
}
