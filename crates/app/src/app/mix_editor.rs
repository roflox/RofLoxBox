//! EDIT mode: a recorded set as a little DAW. The tracks on each deck over
//! time, and a lane of automation for each knob and fader, to change and
//! play into a new WAV.

use crate::tour::Tag as _;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use deckhand_audio::{
    BeatFxKind, BeatFxParams, ColorFxKind, Curve, FxTarget, Point, Session, Setting, SyncSpan,
    TrackMove, TrackSpan, fx_spans, lane_value, move_fx, remove_fx, restyle_fx,
};
use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};

use super::{DECK_COLORS, DeckhandApp, WARN};

/// Bass clashes.
const CLASH: Color32 = Color32::from_rgb(235, 60, 60);
/// The Beat FX beats, as the mixer offers them.
const BEATS: [(f64, &str); 11] = [
    (1.0 / 16.0, "1/16"),
    (0.125, "1/8"),
    (0.25, "1/4"),
    (0.5, "1/2"),
    (0.75, "3/4"),
    (1.0, "1"),
    (2.0, "2"),
    (4.0, "4"),
    (8.0, "8"),
    (16.0, "16"),
    (32.0, "32"),
];

/// How far past its end the timeline reaches at least (seconds).
const ROOM_SECS: f64 = 120.0;
/// The master grid line a drag snapped to.
const SNAP: Color32 = Color32::from_rgb(120, 220, 255);
/// The range marked on the ruler.
const RANGE: Color32 = Color32::from_rgb(90, 160, 255);
/// The playhead.
const PLAY: Color32 = Color32::from_rgb(255, 200, 60);
/// Markers set while recording.
const MARKER: Color32 = Color32::from_rgb(255, 70, 160);
/// Width of the names left of the timeline.
const LABELS: f32 = 150.0;
const RULER: f32 = 34.0;
/// The time labels' part of the ruler; the master grid's bars below.
const TIME_STRIP: f32 = 17.0;
const DECK_ROW: f32 = 26.0;
/// A deck's row with its waveform.
const TRACK_ROW: f32 = 52.0;
const LANE_ROW: f32 = 56.0;
const FX_ROW: f32 = 56.0;
/// How far apart (frames) dragged points stay: saved points move to the
/// nearest buffer start, and two on one would become one.
const POINT_GAP: u64 = 2048;
/// How close (points) the pointer has to be to take hold of a point.
const GRAB: f32 = 7.0;

/// Which screen the app shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Performance,
    Edit,
}

/// A session being opened: its name, and then it with its file.
type Opening = (String, Receiver<Result<(PathBuf, Session, Outlines)>>);

/// Each track's outline, to draw its clips from.
type Outlines = HashMap<PathBuf, deckhand_audio::Outline>;

/// Frames a track's outline takes the loudest of, drawn zoomed in.
const OUTLINE_STEP: usize = 128;

/// EDIT mode's state: the session being edited, or being opened.
#[derive(Default)]
pub(super) struct EditView {
    editor: Option<MixEditor>,
    opening: Option<Opening>,
    /// `DECKHAND_EDIT_SESSION` was opened (a test aid).
    opened_at_start: bool,
}

impl EditView {
    pub(super) fn session_open(&self) -> bool {
        self.editor.is_some()
    }

    /// The open session has recording markers.
    pub(super) fn has_markers(&self) -> bool {
        self.editor.as_ref().is_some_and(|e| !e.markers.is_empty())
    }
}

/// A session open for editing.
struct MixEditor {
    path: PathBuf,
    name: String,
    session: Arc<Mutex<Session>>,
    rate: f64,
    length: u64,
    tracks: Vec<TrackSpan>,
    /// Every setting's points, as the session has them.
    lanes: BTreeMap<Setting, Vec<Point>>,
    shown: Vec<Setting>,
    /// The seconds on view: from, and how many.
    view_from: f64,
    view_secs: f64,
    selection: Option<(Setting, u64, u64)>,
    drag: Option<Drag>,
    undo: Vec<Change>,
    redo: Vec<Change>,
    edited: bool,
    render: Option<Render>,
    /// Copying the session with its songs somewhere: how it went, and
    /// where.
    export: Option<Receiver<(anyhow::Result<String>, PathBuf)>>,
    /// The shape of new points, and of selected ones when chosen.
    shape: Curve,
    /// When each deck was synced to which (from the recording).
    sync: Vec<SyncSpan>,
    /// Synced decks follow tempo edits (sync is done again).
    follow: bool,
    /// The view pages along with the playhead while playing.
    follow_playhead: bool,
    resync: Option<Receiver<Result<usize>>>,
    /// The tracks moved, one being dragged, and the one ← and → move.
    moves: Vec<TrackMove>,
    move_drag: Option<MoveDrag>,
    chosen_move: Option<(usize, u64)>,
    /// Each track's BPM, by file.
    bpms: HashMap<PathBuf, f64>,
    /// Each track's hot cues and memory cues (from the library), drawn on
    /// its clips where the deck passed them.
    cues: HashMap<PathBuf, Vec<ClipCue>>,
    /// Each track's outline: clips are drawn from it, as in a DAW, so they
    /// move, resize and stretch with the tempo at once.
    outlines: Outlines,
    /// Where in its track each play began (deck, its start): from the
    /// replay, kept along as clips move and resize.
    anchors: HashMap<(usize, u64), f64>,
    /// The plays (deck, from, to) the replay's places are good for: drawn
    /// from where the deck really was (jog, nudges, jumps); others, and a
    /// deck whose tempo changed since, by the tempo lane.
    replayed: HashSet<(usize, u64, u64)>,
    /// Each nudge in the set (jog edge, pitch bend): marked on its clip,
    /// right-click one to take it out.
    nudges: Vec<deckhand_audio::Nudge>,
    /// Markers set while recording (frames from the start).
    markers: Vec<u64>,
    /// The Beat FX row and the Color FX overview are shown.
    show_beat_fx: bool,
    show_color_fx: bool,
    /// The lanes as recorded, to go back to.
    original: BTreeMap<Setting, Vec<Point>>,
    /// Listening: the player, where playing starts, and the output device.
    player: Option<deckhand_audio::SessionPlayer>,
    cursor: u64,
    device: Option<String>,
    /// Changed while playing: play again from where it is, with the change.
    replay_edit: bool,
    /// The Beat FX's changes, the block chosen and one being dragged.
    fx: Vec<(u64, BeatFxParams)>,
    chosen_fx: Option<usize>,
    fx_drag: Option<FxDrag>,
    /// The Add Beat FX dialog, when open; and what was last added (the
    /// dialog starts with it).
    adding_fx: Option<AddFx>,
    last_fx: AddFx,
    /// A Beat FX depth point being dragged.
    depth_drag: Option<DepthDrag>,
    /// The tool, and the stretches selected with it: of a deck's clips, of
    /// the Beat FX (and where the drag started).
    tool: Tool,
    /// The keys of the tools, as Preferences name them.
    pub(super) tool_keys: [String; 2],
    clip_selection: Option<(usize, u64, u64)>,
    /// More picked with Cmd/Ctrl-click: plays, pieces or whole clips.
    picked: Vec<Picked>,
    fx_selection: Option<(u64, u64)>,
    range_from: u64,
    /// The selected part of a clip being dragged (Normal tool).
    sel_drag: Option<SelDrag>,
    /// The timeline is being moved with the middle button.
    panning: bool,
    /// A stretch of the timeline marked on the ruler (Select tool): what
    /// Render plays, and later more (loops). Where its drag started.
    range: Option<(u64, u64)>,
    range_start: Option<u64>,
    /// A side of the range being dragged (right side: true).
    range_side: Option<bool>,
    /// A point's value being typed.
    point_edit: Option<PointEdit>,
    /// Where the pointer is on the timeline, for the values.
    hover: Option<u64>,
    /// What each deck played (levels and beats), being worked out again
    /// after edits.
    played: Vec<deckhand_audio::Played>,
    playing_out: Option<Receiver<Result<(Vec<deckhand_audio::Played>, Vec<MixLevel>)>>>,
    /// The mix's level after each buffer (start, peak, RMS).
    mix: Vec<MixLevel>,
    /// The master tempo at each master beat (BPM).
    tempo: Vec<(u64, f64)>,
    played_stale: bool,
    /// Only the mix changed (EQ, faders, FX…): the bright "heard" layer and
    /// the bass clashes are out of date, but working them out again (a
    /// whole replay) waits until asked for or the clashes are shown.
    heard_stale: bool,
    /// Groups folded away: a deck's lanes, the mixer's (`None`), the
    /// effects ([`EFFECTS`]).
    collapsed: std::collections::HashSet<Option<usize>>,
    /// A move shown before the waveforms catch up, and the working outs
    /// started (the one running is the last).
    preview: Option<Preview>,
    jobs: u64,
    /// Edits that moved audio, and how many the running replay has seen:
    /// an older replay's result would draw things where they were.
    moved_edits: u64,
    job_edits: u64,
    /// Where two decks' low end went into the mix together, and whether
    /// to show it.
    clashes: Vec<deckhand_audio::Clash>,
    show_clashes: bool,
    /// The master beat grid (see [`master_grid`]) and its usual beat
    /// length in frames; shown across the rows, and snapped to.
    grid: Vec<GridBeat>,
    grid_beat: u64,
    snap: bool,
    /// Where a drag snapped onto the master grid this frame.
    snap_line: Option<u64>,
    /// Each deck's Master Tempo: on or off from when.
    key_locks: Vec<Vec<(u64, bool)>>,
    clip_drag: Option<ClipDrag>,
    /// The pieces heard of each clip (by deck and where it starts), and an
    /// edge being dragged.
    pieces: HashMap<(usize, u64), Vec<(u64, u64)>>,
    edge_drag: Option<EdgeDrag>,
    /// A play's edge being dragged.
    play_edge: Option<PlayEdgeDrag>,
    /// A message for the status line.
    note: Option<String>,
}

/// The Master group in [`MixEditor::collapsed`].
const MASTER: Option<usize> = Some(usize::MAX - 1);

/// A buffer of the mix: where it starts, its peak and RMS.
type MixLevel = (u64, f32, f32);

/// The effects' group in [`MixEditor::collapsed`].
const EFFECTS: Option<usize> = Some(usize::MAX);

/// One beat of the master grid: where, whether a bar starts there, the
/// deck it is taken from, and its bar's number (from 1; 0 before the
/// first bar).
#[derive(Debug, Clone, Copy, PartialEq)]
struct GridBeat {
    at: u64,
    bar: bool,
    deck: usize,
    number: u32,
}

/// A deck this loud in the mix (about −30 dB) can lead the master grid.
const LEAD_LEVEL: f32 = 0.03;

/// The set's master beat grid, worked out from what was played: the deck
/// recorded as sync master leads where there is one, else the deck leading
/// keeps leading while heard, else the loudest heard deck with a beat grid.
/// Gaps (nothing leading, or a deck without a grid) go on at the last
/// beat's pace. Also the usual beat length (frames), 0 without beats.
fn master_grid(
    played: &[deckhand_audio::Played],
    sync: &[SyncSpan],
    rate: f64,
    length: u64,
) -> (Vec<GridBeat>, u64) {
    let beats: Vec<Vec<(u64, bool)>> = played
        .iter()
        .map(|p| {
            let mut b = p.beats.clone();
            b.sort_by_key(|x| x.0);
            b
        })
        .collect();
    let step = ((rate * 0.25) as u64).max(1);
    let near = (rate * 2.0) as u64;
    let heard = |d: usize, t: u64| {
        let l = &played[d].levels;
        let i = l.partition_point(|x| x.0 <= t).saturating_sub(1);
        let j = l.partition_point(|x| x.0 < t + step).max(i + 1).min(l.len());
        l.get(i..j).map_or(0.0, |s| s.iter().map(|x| x.2).fold(0.0, f32::max))
    };
    let has_beats = |d: usize, t: u64| {
        let b = &beats[d];
        let i = b.partition_point(|x| x.0 + near < t);
        b.get(i).is_some_and(|x| x.0 <= t + near)
    };
    let leads = |d: usize, t: u64| heard(d, t) >= LEAD_LEVEL && has_beats(d, t);
    // Who leads, from when.
    let mut segments: Vec<(u64, usize)> = Vec::new();
    let mut leader: Option<usize> = None;
    let mut t = 0;
    while t < length {
        let recorded = sync
            .iter()
            .find(|s| s.from <= t && t < s.to)
            .map(|s| s.master)
            .filter(|&m| m < played.len() && leads(m, t));
        let next = recorded.or(leader.filter(|&d| leads(d, t))).or_else(|| {
            (0..played.len())
                .filter(|&d| leads(d, t))
                .max_by(|&a, &b| heard(a, t).total_cmp(&heard(b, t)))
        });
        if let Some(d) = next {
            if leader != Some(d) {
                segments.push((t, d));
            }
            leader = Some(d);
        }
        t += step;
    }
    // Their beats (`None`: a bar or not is worked out by counting).
    let mut taken: Vec<(u64, Option<bool>, usize)> = Vec::new();
    for (i, &(from, deck)) in segments.iter().enumerate() {
        let to = segments.get(i + 1).map_or(length, |s| s.0);
        let b = &beats[deck];
        let first = b.partition_point(|x| x.0 < from);
        for &(at, bar) in b[first..].iter().take_while(|x| x.0 < to) {
            // Not right on top of the one before (where the lead changes).
            match taken.as_slice() {
                [.., (a, ..), (z, _, led)] if at <= *z || *led != deck && (at - z) * 2 < z - a => {
                    continue;
                }
                [.., (z, ..)] if at <= *z => continue,
                _ => {}
            }
            taken.push((at, Some(bar), deck));
        }
    }
    let mut filled: Vec<(u64, Option<bool>, usize)> = Vec::with_capacity(taken.len());
    for (i, &(at, bar, deck)) in taken.iter().enumerate() {
        filled.push((at, bar, deck));
        let interval = match (i.checked_sub(1).map(|p| taken[p].0), taken.get(i + 1)) {
            (Some(before), _) => at - before,
            (None, Some(next)) => next.0 - at,
            (None, None) => 0,
        };
        if interval == 0 {
            continue;
        }
        let until = taken.get(i + 1).map_or(length, |n| n.0);
        if until - at > interval * 3 / 2 {
            let mut next = at + interval;
            while next + interval / 2 < until {
                filled.push((next, None, deck));
                next += interval;
            }
        }
    }
    let (mut since, mut number) = (0u32, 0u32);
    let grid: Vec<GridBeat> = filled
        .into_iter()
        .map(|(at, bar, deck)| {
            since += 1;
            let bar = bar.unwrap_or(number > 0 && since > 4);
            if bar {
                since = 1;
                number += 1;
            }
            GridBeat { at, bar, deck, number }
        })
        .collect();
    let mut intervals: Vec<u64> = grid.windows(2).map(|w| w[1].at - w[0].at).collect();
    intervals.sort_unstable();
    let beat = intervals.get(intervals.len() / 2).copied().unwrap_or(0);
    (grid, beat)
}

/// The master tempo at each beat of `grid` (from the second): over the
/// last two bars, so the beats' placement doesn't make it jitter, and a
/// lead change's odd beat doesn't count.
fn grid_tempo(grid: &[GridBeat], rate: f64) -> Vec<(u64, f64)> {
    let mut tempo = Vec::with_capacity(grid.len());
    for i in 1..grid.len() {
        // Back to 8 beats, as long as one deck leads.
        let mut k = 1;
        while k < 8 && i > k && grid[i - k - 1].deck == grid[i].deck {
            k += 1;
        }
        // The first beat after a lead change: the gap between two decks'
        // grids, not a tempo.
        if k == 1 && i > 1 && grid[i - 1].deck != grid[i].deck {
            continue;
        }
        let span = grid[i].at - grid[i - k].at;
        if span > 0 {
            let bpm = 60.0 * rate * k as f64 / span as f64;
            tempo.push((grid[i - 1].at, (bpm * 100.0).round() / 100.0));
        }
    }
    tempo
}

/// Changes kept to undo and redo.
const HISTORY: usize = 100;

/// A change in the history: its name, and the state to go back (or
/// forward) to.
struct Change {
    label: String,
    edit: Edit,
}

/// A state to go to: one lane, one track move, the Beat FX, or (for clip
/// moves, which change much) every event, compressed.
enum Edit {
    Lane(Setting, Vec<Point>),
    Move { deck: usize, at: u64, later: f64 },
    Fx(Vec<(u64, BeatFxParams)>),
    Events(Vec<u8>),
}

fn pack(session: &Session) -> Option<Vec<u8>> {
    use std::io::Write as _;
    let mut out = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    out.write_all(&session.backup().ok()?).ok()?;
    out.finish().ok()
}

fn unpack(packed: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(packed).read_to_end(&mut out).ok()?;
    Some(out)
}

/// A piece's edge being dragged: the clip, which piece and edge, and the
/// pieces as they are while dragging.
struct EdgeDrag {
    span: TrackSpan,
    index: usize,
    right: bool,
    pieces: Vec<(u64, u64)>,
}

/// A play's start or end being dragged: the play as it was, the part of
/// it heard then (a cut may end it sooner), as it is now, how far it can
/// go, and the edge.
struct PlayEdgeDrag {
    deck: usize,
    play: (u64, u64),
    heard: (u64, u64),
    now: (u64, u64),
    edges: deckhand_audio::PlayEdges,
    right: bool,
}

/// An edge under the pointer: a piece's (clip, which piece, right edge)
/// or a play's (the play, right edge).
enum EdgeHit {
    Piece(TrackSpan, usize, bool),
    Play((u64, u64), bool),
}

/// Clip colours, given to tracks in the order they come in the set so
/// clips next to each other differ.
const CLIP_COLORS: [Color32; 10] = [
    Color32::from_rgb(70, 140, 245),
    Color32::from_rgb(245, 155, 60),
    Color32::from_rgb(70, 195, 115),
    Color32::from_rgb(190, 100, 235),
    Color32::from_rgb(240, 95, 150),
    Color32::from_rgb(60, 195, 205),
    Color32::from_rgb(225, 200, 65),
    Color32::from_rgb(135, 150, 255),
    Color32::from_rgb(250, 120, 85),
    Color32::from_rgb(150, 200, 85),
];

/// A clip being dragged by its title: its deck, where it was loaded and
/// taken off, and where the drag started.
struct ClipDrag {
    deck: usize,
    from: u64,
    to: u64,
    x: f32,
}

/// A track being moved: from where, by how much before and now, and where
/// the drag started.
struct MoveDrag {
    deck: usize,
    at: u64,
    base: f64,
    x: f32,
    later: f64,
    /// Where the track's time on the deck ends.
    to: u64,
}

/// A track moved on screen ahead of the waveforms being worked out again:
/// `deck`'s audio from `at` to `to` shows `frames` later.
#[derive(Clone, Copy)]
struct Preview {
    deck: usize,
    at: u64,
    to: u64,
    frames: i64,
    /// The whole clip moves (not only the audio in it).
    whole: bool,
    /// Kept until a working out started after this one is in.
    until_job: u64,
}

/// A Beat FX block being dragged by a part of it, and where it is now.
struct FxDrag {
    index: usize,
    grip: FxGrip,
    /// Where the pointer took hold.
    at: u64,
    from: u64,
    to: u64,
}

/// A point's value being typed: the lane, the point's moment, the text,
/// where the dialog is, and the unit typed in.
struct PointEdit {
    setting: Setting,
    at: u64,
    text: String,
    pos: Pos2,
    unit: Unit,
    /// A clip edge's BPM handle: its points are pinned as the value is set.
    handle: Option<TempoHandle>,
}

/// A hot cue (A = 0 … H = 7) or memory cue of a track: where, and its
/// colour.
#[derive(Clone, Copy)]
struct ClipCue {
    seconds: f64,
    hot: Option<u8>,
    color: Color32,
}

/// The cues the library has for each of `paths`: by the file, else (a copy
/// on a USB stick, an exported session) by its name.
fn library_cues<'a>(
    collection: &deckhand_library::Collection,
    paths: impl Iterator<Item = &'a PathBuf>,
) -> HashMap<PathBuf, Vec<ClipCue>> {
    let mut found = HashMap::new();
    for path in paths {
        if found.contains_key(path) {
            continue;
        }
        let name = path.file_name();
        let track = collection
            .tracks
            .iter()
            .find(|t| t.path == *path)
            .or_else(|| collection.tracks.iter().find(|t| name.is_some() && t.path.file_name() == name));
        let Some(track) = track else { continue };
        let cues: Vec<ClipCue> = track
            .cues
            .iter()
            .filter(|c| c.end.is_none())
            .map(|c| match c.kind {
                deckhand_library::CueKind::Hot(n) => ClipCue {
                    seconds: c.start,
                    hot: Some(n),
                    color: c.color.map_or(crate::deck::HOT_CUE_DEFAULT, |[r, g, b]| {
                        Color32::from_rgb(r, g, b)
                    }),
                },
                deckhand_library::CueKind::Memory => {
                    ClipCue { seconds: c.start, hot: None, color: Color32::from_rgb(230, 50, 50) }
                }
            })
            .collect();
        found.insert(path.clone(), cues);
    }
    found
}

/// A clip edge's BPM handle on its deck's tempo lane: the clip, where on
/// screen, whether it is its end, and the BPM there (the track's known).
#[derive(Clone, Copy)]
struct TempoHandle {
    from: u64,
    to: u64,
    end: bool,
    pos: Pos2,
    bpm: Option<f64>,
}

/// What a typed value means.
#[derive(Clone, Copy, PartialEq)]
enum Unit {
    /// The track's BPM there (tempo, the track's own BPM known).
    Bpm(f64),
    /// Percent off the track's tempo.
    TempoPercent,
    /// Percent of the travel (faders, depth); −100…100 for knobs that
    /// go both ways (Color FX, crossfader).
    Percent,
    /// Decibels: gain for trim, the EQ's own for EQ.
    TrimDb,
    EqDb,
}

impl Unit {
    fn of(setting: Setting, bpm: Option<f64>) -> Self {
        match setting {
            Setting::Tempo(_) => bpm.map_or(Self::TempoPercent, Self::Bpm),
            Setting::Trim(_) => Self::TrimDb,
            Setting::Eq(..) => Self::EqDb,
            _ => Self::Percent,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Bpm(_) => "BPM",
            Self::TempoPercent | Self::Percent => "%",
            Self::TrimDb | Self::EqDb => "dB",
        }
    }

    /// The stored value as typed.
    fn show(self, value: f64) -> String {
        match self {
            Self::Bpm(bpm) => format!("{:.2}", bpm * value),
            Self::TempoPercent => format!("{:+.2}", (value - 1.0) * 100.0),
            Self::Percent => format!("{:.1}", value * 100.0),
            Self::TrimDb => format!("{:+.1}", 20.0 * value.max(1e-6).log10()),
            Self::EqDb => format!("{value:+.1}"),
        }
    }

    /// The typed text as the stored value.
    fn read(self, text: &str) -> Option<f64> {
        let typed: f64 = text.trim().trim_end_matches('%').replace(',', ".").parse().ok()?;
        Some(match self {
            Self::Bpm(bpm) => typed / bpm,
            Self::TempoPercent => 1.0 + typed / 100.0,
            Self::Percent => typed / 100.0,
            Self::TrimDb => 10f64.powf(typed / 20.0),
            Self::EqDb => typed,
        })
    }
}

/// The Add Beat FX dialog: where the effect starts, where the dialog is,
/// and the choices.
#[derive(Clone, Copy)]
struct AddFx {
    at: u64,
    pos: Pos2,
    kind: BeatFxKind,
    target: FxTarget,
    beats: u32,
    depth: f32,
    /// The effect's beat (1/4, 1, 2…), as on the mixer.
    beat: f64,
}

/// A Beat FX depth point being dragged: which point of the depth lane, the
/// block it is in, and the lane as it is now.
struct DepthDrag {
    index: usize,
    block: Rect,
    points: Vec<Point>,
    /// An effect's edge handle: up and down only.
    edge: bool,
    /// The effect's start and end handle: points between stay between.
    limits: (u64, u64),
}

/// A depth handle in a Beat FX block: where, its lane point (none for an
/// edge without one yet), the block, the moment, and whether it is an edge.
#[derive(Clone, Copy)]
struct Handle {
    pos: Pos2,
    index: Option<usize>,
    block: usize,
    at: u64,
    edge: bool,
}

/// The editing tool: Normal moves and edits; Select drags stretches to
/// delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tool {
    Normal,
    Select,
}

/// Something selected on a deck: a stretch of a clip (a play, a piece), or
/// a whole clip (`whole`: from its load to where it was taken off).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Picked {
    deck: usize,
    from: u64,
    to: u64,
    whole: bool,
}

/// The selected part of a deck's clip being dragged: the deck, where the
/// drag started, and how far it has gone (frames, kept inside the clip).
struct SelDrag {
    deck: usize,
    x: f32,
    frames: i64,
}

/// Which kind of selection is made (one at a time).
#[derive(Clone, Copy)]
enum Selected {
    Lane,
    Clip,
    Fx,
}

#[derive(Clone, Copy)]
enum FxGrip {
    Start,
    End,
    Whole,
}

/// "MST", "1"…"4" or "SP", as on the mixer.
fn target_label(target: FxTarget) -> String {
    match target {
        FxTarget::Master => "MST".into(),
        FxTarget::Sampler => "SP".into(),
        FxTarget::Channel(d) => (d + 1).to_string(),
    }
}

enum Drag {
    /// A point being moved; the lane as it was, and its scale then (kept
    /// while dragging, so the point doesn't run off as the scale changes).
    /// `pinned`: a clip edge's BPM handle, which only goes up and down.
    Point { setting: Setting, index: usize, before: Vec<Point>, range: (f64, f64), pinned: bool },
    /// A stretch being selected from here.
    Select { setting: Setting, from: u64 },
}

/// The edited set being played into a WAV in the background.
struct Render {
    done_frames: Arc<AtomicU64>,
    /// Frames it plays through, for the progress.
    total: u64,
    done: Receiver<Result<String>>,
}

impl EditView {
    /// Stops listening (leaving EDIT mode).
    pub(super) fn stop_playing(&mut self) {
        if let Some(editor) = self.editor.as_mut() {
            editor.player = None;
        }
    }
}

impl MixEditor {
    fn new(path: PathBuf, session: Session) -> Self {
        let rate = f64::from(session.sample_rate());
        let length = session.length();
        let tracks = session.tracks();
        let sync = session.sync_spans();
        let bpms = tracks
            .iter()
            .filter_map(|t| Some((t.path.clone(), session.grid(&t.path)?.bpm)))
            .filter(|(_, bpm)| *bpm > 0.0)
            .collect();
        let (moves, fx) = (session.moves(), session.beat_fx());
        let nudges = session.nudges();
        let markers = session.markers();
        let pieces = tracks.iter().map(|t| ((t.deck, t.from), session.pieces(t))).collect();
        let key_locks = (0..deckhand_audio::DECKS).map(|d| session.key_lock(d)).collect();
        let decks = tracks.iter().map(|t| t.deck + 1).max().unwrap_or(0);
        let lanes: BTreeMap<_, _> = Setting::all(deckhand_audio::DECKS)
            .into_iter()
            .chain([Setting::BeatFxDepth])
            .map(|s| (s, session.lane(s)))
            .collect();
        // The lanes that move, else the decks' faders.
        let mut shown: Vec<Setting> = lanes
            .iter()
            .filter(|(s, points)| points.len() > 1 && **s != Setting::BeatFxDepth)
            .map(|(s, _)| *s)
            .collect();
        if shown.is_empty() {
            shown = (0..decks).map(Setting::Volume).collect();
        }
        let original = lanes.clone();
        shown.sort_by_key(lane_order);
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        // Test aid: "from,seconds" on view.
        let view = std::env::var("DECKHAND_EDIT_VIEW").ok().and_then(|v| {
            let (from, secs) = v.split_once(',')?;
            Some((from.trim().parse().ok()?, secs.trim().parse().ok()?))
        });
        let (view_from, view_secs) = view.unwrap_or((0.0, (length as f64 / rate).max(1.0)));
        Self {
            path,
            name,
            session: Arc::new(Mutex::new(session)),
            rate,
            length,
            tracks,
            lanes,
            shown,
            view_from,
            view_secs,
            selection: None,
            drag: None,
            undo: Vec::new(),
            redo: Vec::new(),
            edited: false,
            render: None,
            export: None,
            shape: Curve::Line,
            sync,
            follow: true,
            follow_playhead: false,
            resync: None,
            moves,
            move_drag: None,
            chosen_move: None,
            bpms,
            cues: HashMap::new(),
            outlines: HashMap::new(),
            anchors: HashMap::new(),
            replayed: HashSet::new(),
            nudges,
            markers,
            show_beat_fx: true,
            show_color_fx: true,
            original,
            player: None,
            cursor: 0,
            device: None,
            replay_edit: false,
            fx,
            chosen_fx: None,
            adding_fx: None,
            last_fx: AddFx {
                at: 0,
                pos: Pos2::ZERO,
                kind: BeatFxKind::Echo,
                target: FxTarget::Master,
                beats: 8,
                depth: 0.5,
                beat: 1.0,
            },
            depth_drag: None,
            tool: Tool::Normal,
            tool_keys: Default::default(),
            clip_selection: None,
            picked: Vec::new(),
            fx_selection: None,
            range_from: 0,
            sel_drag: None,
            panning: false,
            range: None,
            range_start: None,
            range_side: None,
            point_edit: None,
            fx_drag: None,
            hover: None,
            played: Vec::new(),
            playing_out: None,
            played_stale: true,
            heard_stale: false,
            moved_edits: 0,
            job_edits: 0,
            mix: Vec::new(),
            tempo: Vec::new(),
            collapsed: Default::default(),
            preview: None,
            jobs: 0,
            clashes: Vec::new(),
            show_clashes: false,
            grid: Vec::new(),
            grid_beat: 0,
            snap: true,
            snap_line: None,
            key_locks,
            clip_drag: None,
            pieces,
            edge_drag: None,
            play_edge: None,
            note: None,
        }
    }

    fn secs(&self) -> f64 {
        self.length as f64 / self.rate
    }

    /// Seconds the timeline reaches: past the end, to drag clips out
    /// there (the set grows to take them).
    fn room_secs(&self) -> f64 {
        self.secs() + ROOM_SECS.max(self.secs() * 0.25)
    }

    fn x_of(&self, rect: Rect, at: u64) -> f32 {
        let t = at as f64 / self.rate;
        rect.left() + ((t - self.view_from) / self.view_secs * f64::from(rect.width())) as f32
    }

    fn at_of(&self, rect: Rect, x: f32) -> u64 {
        let t = self.view_from + f64::from((x - rect.left()) / rect.width()) * self.view_secs;
        ((t.max(0.0) * self.rate) as u64).min((self.room_secs() * self.rate) as u64)
    }

    /// The lane's range on screen: the knob's travel, wider if it went
    /// further.
    fn range(&self, setting: Setting) -> (f64, f64) {
        if let Some(Drag::Point { setting: s, range, .. }) = &self.drag
            && *s == setting
        {
            return *range;
        }
        let (lo, hi) = setting.range();
        self.lanes[&setting].iter().fold((lo, hi), |(lo, hi), p| (lo.min(p.value), hi.max(p.value)))
    }

    fn y_of(&self, rect: Rect, setting: Setting, value: f64) -> f32 {
        let (lo, hi) = self.range(setting);
        let share = ((value - lo) / (hi - lo).max(1e-9)) as f32;
        rect.bottom() - 3.0 - share * (rect.height() - 6.0)
    }

    fn value_of(&self, rect: Rect, setting: Setting, y: f32) -> f64 {
        let (lo, hi) = self.range(setting);
        let share = f64::from((rect.bottom() - 3.0 - y) / (rect.height() - 6.0));
        (lo + share.clamp(0.0, 1.0) * (hi - lo)).clamp(lo, hi)
    }

    /// The setting's value at a moment.
    fn value_at(&self, setting: Setting, at: u64) -> f64 {
        lane_value(&self.lanes[&setting], at)
    }

    /// Rendering or syncing again: the session is busy.
    fn busy(&self) -> bool {
        self.render.is_some() || self.resync.is_some()
    }

    /// After a tempo change, synced decks follow it: sync is done again
    /// in the background.
    fn follow_tempo(&mut self, setting: Setting) {
        if !matches!(setting, Setting::Tempo(_)) || !self.follow || self.sync.is_empty() {
            return;
        }
        let session = self.session.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = session
                .lock()
                .map_err(|_| anyhow::anyhow!("the session is broken"))
                .and_then(|mut s| s.resync());
            let _ = tx.send(result);
        });
        self.resync = Some(rx);
    }

    /// Takes in what syncing again changed: the decks' tempo lanes.
    fn synced(&mut self) {
        let Ok(session) = self.session.lock() else { return };
        for deck in 0..deckhand_audio::DECKS {
            let setting = Setting::Tempo(deck);
            self.lanes.insert(setting, session.lane(setting));
        }
    }

    /// Hands a lane's new points to the session, keeping the old for undo;
    /// `what` names the change.
    fn commit(&mut self, setting: Setting, before: Vec<Point>, what: &str) {
        let Ok(mut session) = self.session.lock() else { return };
        session.set_lane(setting, &self.lanes[&setting]);
        let lane = session.lane(setting);
        drop(session);
        let changed = lane != before;
        self.lanes.insert(setting, lane);
        if changed {
            let name = match setting.deck() {
                Some(d) => format!("Deck {} {}", d + 1, setting.label()),
                None => setting.label().to_owned(),
            };
            self.record(format!("{name}: {what}"), Edit::Lane(setting, before));
        }
        if let Setting::Tempo(deck) = setting {
            self.replayed.retain(|p| p.0 != deck);
        }
        self.follow_tempo(setting);
    }

    /// Keeps a change to undo (the last [`HISTORY`]); a new one ends what
    /// could be redone.
    fn record(&mut self, label: String, edit: Edit) {
        let edit_kind = Self::mix_only(&edit);
        self.undo.push(Change { label, edit });
        if self.undo.len() > HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.edited = true;
        self.mark_stale(&edit_kind);
        self.replay_edit = true;
    }

    /// Goes to `edit`'s state; returns the edit that comes back.
    fn apply(&mut self, edit: Edit) -> Option<Edit> {
        if let Edit::Lane(Setting::Tempo(deck), _) = &edit {
            let deck = *deck;
            self.replayed.retain(|p| p.0 != deck);
        }
        let edit_kind = Self::mix_only(&edit);
        let shared = self.session.clone();
        let mut session = shared.lock().ok()?;
        let back = match edit {
            Edit::Lane(setting, points) => {
                let before = self.lanes[&setting].clone();
                session.set_lane(setting, &points);
                self.lanes.insert(setting, session.lane(setting));
                drop(session);
                self.follow_tempo(setting);
                Edit::Lane(setting, before)
            }
            Edit::Move { deck, at, later } => {
                let before = session
                    .moves()
                    .iter()
                    .find(|m| m.deck == deck && m.at == at)
                    .map_or(0.0, |m| m.later);
                session.move_track(deck, at, later);
                self.moves = session.moves();
                Edit::Move { deck, at, later: before }
            }
            Edit::Fx(changes) => {
                let before = std::mem::take(&mut self.fx);
                session.set_beat_fx(&changes);
                self.fx = session.beat_fx();
                Edit::Fx(before)
            }
            Edit::Events(packed) => {
                let now = pack(&session)?;
                session.restore(&unpack(&packed)?).ok()?;
                self.refresh(&session);
                Edit::Events(now)
            }
        };
        self.mark_stale(&edit_kind);
        self.replay_edit = true;
        Some(back)
    }

    /// Takes in everything from the session again (after events changed).
    fn refresh(&mut self, session: &Session) {
        self.length = session.length();
        self.nudges = session.nudges();
        self.markers = session.markers();
        self.tracks = session.tracks();
        self.key_locks = (0..deckhand_audio::DECKS).map(|d| session.key_lock(d)).collect();
        self.pieces = self.tracks.iter().map(|t| ((t.deck, t.from), session.pieces(t))).collect();
        self.sync = session.sync_spans();
        self.moves = session.moves();
        self.fx = session.beat_fx();
        for (setting, lane) in &mut self.lanes {
            *lane = session.lane(*setting);
        }
    }

    fn undo(&mut self) {
        let Some(change) = self.undo.pop() else { return };
        if let Some(back) = self.apply(change.edit) {
            self.redo.push(Change { label: change.label, edit: back });
        }
        self.edited = !self.undo.is_empty();
    }

    fn redo(&mut self) {
        let Some(change) = self.redo.pop() else { return };
        if let Some(back) = self.apply(change.edit) {
            self.undo.push(Change { label: change.label, edit: back });
        }
        self.edited = true;
    }

    /// The history: changes that can be redone, then those that can be
    /// undone; a click goes back to just before a change, or forward to
    /// just after it.
    fn history(&mut self, ui: &mut Ui) {
        let (mut undo, mut redo) = (0, 0);
        ui.set_min_width(260.0);
        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
            for (i, change) in self.redo.iter().enumerate() {
                let text = RichText::new(&change.label).weak();
                if ui.selectable_label(false, text).on_hover_text("Redo up to here").clicked() {
                    redo = self.redo.len() - i;
                }
            }
            ui.label(RichText::new("— now —").small().strong());
            for (i, change) in self.undo.iter().enumerate().rev() {
                let clicked = ui
                    .selectable_label(false, &change.label)
                    .on_hover_text("Undo back to before this");
                if clicked.clicked() {
                    undo = self.undo.len() - i;
                }
            }
            if self.undo.is_empty() && self.redo.is_empty() {
                ui.label(RichText::new("No changes yet").weak());
            }
        });
        if undo + redo > 0 {
            ui.close();
        }
        (0..undo).for_each(|_| self.undo());
        (0..redo).for_each(|_| self.redo());
    }

    /// An edit that only changes the mix (not where audio plays): any lane
    /// but tempo, and the Beat FX.
    fn mix_only(edit: &Edit) -> bool {
        match edit {
            Edit::Lane(setting, _) => !matches!(setting, Setting::Tempo(_)),
            Edit::Fx(_) => true,
            Edit::Move { .. } | Edit::Events(_) => false,
        }
    }

    /// After an edit: the waveforms are worked out again when audio moved;
    /// for mix changes only the heard layer is marked out of date.
    fn mark_stale(&mut self, mix_only: &bool) {
        if *mix_only {
            self.heard_stale = true;
        } else {
            self.played_stale = true;
            self.moved_edits += 1;
        }
    }

    /// Works out again what the decks played, in the background, on a copy
    /// of the session (edits go on meanwhile).
    fn update_played(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.playing_out {
            match rx.try_recv() {
                Ok(result) => {
                    self.playing_out = None;
                    let current = self.moved_edits == self.job_edits;
                    if let Some((played, mix)) = result.ok().filter(|_| current) {
                        self.mix = mix;
                        // Half a second of both at least.
                        self.clashes = deckhand_audio::clashes(&played, (self.rate * 0.5) as u64);
                        self.played = played;
                        self.anchors = self.find_anchors();
                        self.replayed = self
                            .tracks
                            .iter()
                            .flat_map(|t| t.playing.iter().map(move |p| (t.deck, p.0, p.1)))
                            .collect();
                        (self.grid, self.grid_beat) =
                            master_grid(&self.played, &self.sync, self.rate, self.length);
                        self.tempo = grid_tempo(&self.grid, self.rate);
                    }
                    if self.preview.is_some_and(|p| self.jobs > p.until_job) {
                        self.preview = None;
                    }
                }
                Err(_) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    return;
                }
            }
        }
        // The clashes on show need the mix as it is now.
        if self.heard_stale && self.show_clashes {
            self.played_stale = true;
        }
        if !self.played_stale || self.resync.is_some() {
            return;
        }
        let Some(copy) = self.session.try_lock().ok().and_then(|s| s.duplicate().ok()) else {
            return;
        };
        self.played_stale = false;
        self.heard_stale = false;
        self.jobs += 1;
        self.job_edits = self.moved_edits;
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(copy.played_and_mix());
            ctx.request_repaint();
        });
        self.playing_out = Some(rx);
    }

    /// Plays the set as it is now from `from` (frames from the start).
    fn play_from(&mut self, from: u64) {
        self.player = None;
        let Ok(session) = self.session.try_lock() else {
            self.note = Some("Busy; play again in a moment".into());
            return;
        };
        let started = session.duplicate().and_then(|copy| {
            deckhand_audio::SessionPlayer::start(
                copy,
                from.min(self.length),
                self.device.as_deref(),
            )
        });
        drop(session);
        match started {
            Ok(player) => self.player = Some(player),
            Err(err) => self.note = Some(format!("Could not play: {err:#}")),
        }
    }

    /// Space: plays from the cursor, or stops (the cursor stays, so
    /// playing again starts at the same place).
    fn toggle_play(&mut self) {
        if self.player.take().is_none() {
            self.play_from(self.cursor);
        }
    }

    /// Where it is playing, in frames from the start.
    fn playhead(&self) -> Option<u64> {
        self.player.as_ref().map(|p| p.position().min(self.length))
    }

    /// Keeps playing going: plays edits made meanwhile, follows the
    /// playhead off the screen, notices the end.
    fn poll_player(&mut self, ctx: &egui::Context) {
        let replay = std::mem::take(&mut self.replay_edit);
        let Some(player) = &self.player else { return };
        if player.finished() {
            if let Some(err) = player.error() {
                self.note = Some(format!("Playing stopped: {err}"));
            }
            self.player = None;
            return;
        }
        let at = player.position().min(self.length);
        if replay && !self.busy() {
            self.play_from(at);
        }
        let secs = at as f64 / self.rate;
        if self.follow_playhead && (secs > self.view_from + self.view_secs || secs < self.view_from) {
            self.view_from = secs - self.view_secs * 0.05;
            self.clamp_view();
        }
        ctx.request_repaint();
    }

    /// The marker under `x` on the ruler, if one.
    fn marker_near(&self, area: Rect, x: f32) -> Option<u64> {
        self.markers
            .iter()
            .copied()
            .filter(|&at| (self.x_of(area, at) + 5.0 - x).abs() <= GRAB + 4.0)
            .min_by(|&a, &b| {
                let off = |at| (self.x_of(area, at) - x).abs();
                off(a).total_cmp(&off(b))
            })
    }

    /// On the ruler: a marker's flag puts the cursor on it; right-click
    /// takes it out.
    fn marker_clicks(&mut self, ui: &Ui, area: Rect, response: &egui::Response) {
        let Some(pos) = response.hover_pos().or(response.interact_pointer_pos()) else { return };
        let Some(at) = self.marker_near(area, pos.x) else { return };
        let number = self.markers.iter().position(|&m| m == at).map_or(0, |i| i + 1);
        let secs = at as f64 / self.rate;
        response.clone().on_hover_text(format!(
            "Marker {number} · {}:{:05.2}\nRight-click: remove",
            (secs / 60.0) as u64,
            secs % 60.0
        ));
        if response.clicked() {
            self.range = None;
            self.cursor = at;
            if self.player.is_some() {
                self.play_from(at);
            }
        }
        if response.secondary_clicked() {
            self.remove_marker(at);
        }
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    /// Puts the cursor on `at` and shows it.
    fn go_to(&mut self, at: u64) {
        self.cursor = at;
        let secs = at as f64 / self.rate;
        if secs < self.view_from || secs > self.view_from + self.view_secs {
            self.view_from = secs - self.view_secs * 0.5;
            self.clamp_view();
        }
        if self.player.is_some() {
            self.play_from(at);
        }
    }

    fn remove_marker(&mut self, at: u64) {
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        if session.remove_markers(at, at) == 0 {
            return;
        }
        self.refresh(&session);
        drop(session);
        self.record("Marker removed".into(), Edit::Events(before));
    }

    /// The cursor (where playing starts) and the playhead, down `area`.
    fn play_marks(&self, painter: &egui::Painter, area: Rect) {
        if let Some((from, to)) = self.range {
            let band =
                Rect::from_x_y_ranges(self.x_of(area, from)..=self.x_of(area, to), area.y_range());
            painter.rect_filled(band, 0.0, RANGE.gamma_multiply(0.12));
        }
        for &at in &self.markers {
            let x = self.x_of(area, at);
            painter.line_segment(
                [pos2(x, area.top()), pos2(x, area.bottom())],
                Stroke::new(1.0_f32, MARKER.gamma_multiply(0.7)),
            );
        }
        let x = self.x_of(area, self.cursor);
        painter.line_segment(
            [pos2(x, area.top()), pos2(x, area.bottom())],
            Stroke::new(1.0_f32, PLAY.gamma_multiply(0.45)),
        );
        if let Some(at) = self.playhead() {
            let x = self.x_of(area, at);
            painter.line_segment(
                [pos2(x, area.top()), pos2(x, area.bottom())],
                Stroke::new(1.5_f32, PLAY),
            );
        }
    }

    /// Puts `from`..`to` of a lane back as it was recorded; after it, the
    /// lane goes on as edited.
    fn reset(&mut self, setting: Setting, from: u64, to: u64) {
        let before = self.lanes[&setting].clone();
        let original = &self.original[&setting];
        let curve_at = |points: &[Point], at: u64| {
            points[points.partition_point(|p| p.at <= at).saturating_sub(1)].curve
        };
        let mut lane: Vec<Point> = before.iter().filter(|p| p.at < from).copied().collect();
        lane.push(Point {
            at: from,
            value: lane_value(original, from),
            curve: curve_at(original, from),
        });
        lane.extend(original.iter().filter(|p| p.at > from && p.at < to).copied());
        if to < self.length {
            lane.push(Point {
                at: to,
                value: lane_value(&before, to),
                curve: curve_at(&before, to),
            });
            lane.extend(before.iter().filter(|p| p.at > to).copied());
        }
        self.lanes.insert(setting, lane);
        let what = if from == 0 && to >= self.length { "reset lane" } else { "reset to original" };
        self.commit(setting, before, what);
    }

    /// The decks whose Color FX were on somewhere.
    fn color_decks(&self) -> Vec<usize> {
        (0..deckhand_audio::DECKS)
            .filter(|&d| {
                ColorFxKind::ALL
                    .iter()
                    .any(|&k| self.lanes[&Setting::ColorFx(d, k)].iter().any(|p| p.value != 0.0))
            })
            .collect()
    }

    /// Where each deck's Color FX were on, and which: a strip a deck, in
    /// its colour, named by type (only to see; they are changed on the
    /// decks' intensity lanes). A click shows that type's lane.
    fn color_overview(&mut self, ui: &Ui, row: Rect, response: &egui::Response, decks: &[usize]) {
        let visuals = ui.visuals().clone();
        ui.painter().text(
            pos2(row.left() + 4.0, row.top() + 4.0),
            egui::Align2::LEFT_TOP,
            "Color FX",
            egui::FontId::proportional(13.0),
            visuals.strong_text_color(),
        );
        let area = Rect::from_min_max(pos2(row.left() + LABELS, row.top() + 1.0), row.max);
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 2.0, visuals.extreme_bg_color);
        if decks.is_empty() {
            painter.text(
                area.left_center() + vec2(6.0, 0.0),
                egui::Align2::LEFT_CENTER,
                "No Color FX in this set",
                egui::FontId::proportional(11.0),
                visuals.weak_text_color(),
            );
            return;
        }
        let mut stretches = Vec::new();
        for (i, &deck) in decks.iter().enumerate() {
            let color = DECK_COLORS[deck % DECK_COLORS.len()];
            let top = area.top() + 3.0 + 14.0 * i as f32;
            for kind in ColorFxKind::ALL {
                let lane = &self.lanes[&Setting::ColorFx(deck, kind)];
                // Stretches where the lane is off zero.
                let mut from = None;
                for (j, point) in lane.iter().enumerate() {
                    let next = lane.get(j + 1).map_or(self.length, |p| p.at);
                    let on = point.value != 0.0
                        || point.curve != Curve::Step
                            && lane.get(j + 1).is_some_and(|n| n.value != 0.0);
                    match (on, from) {
                        (true, None) => from = Some(point.at),
                        (false, Some(start)) => {
                            stretches.push((deck, kind, start, point.at, top, color));
                            from = None;
                        }
                        _ => {}
                    }
                    if j + 1 == lane.len()
                        && let Some(start) = from
                    {
                        stretches.push((deck, kind, start, next, top, color));
                    }
                }
            }
        }
        let mut hits = Vec::new();
        for &(deck, kind, from, to, top, color) in &stretches {
            let block = Rect::from_x_y_ranges(
                self.x_of(area, from)..=self.x_of(area, to).max(self.x_of(area, from) + 2.0),
                top..=top + 12.0,
            );
            painter.rect_filled(block, 2.0, color.gamma_multiply(0.45));
            let text = ui.painter().layout_no_wrap(
                kind.title().to_owned(),
                egui::FontId::proportional(10.0),
                Color32::WHITE,
            );
            let clip = block.intersect(area).shrink2(vec2(2.0, 0.0));
            ui.painter().with_clip_rect(clip).galley(
                pos2(clip.left(), block.top()),
                text,
                Color32::WHITE,
            );
            hits.push((block, deck, kind));
        }
        self.play_marks(&painter, area);
        if let Some(pos) = response.hover_pos()
            && let Some(&(_, deck, kind)) = hits.iter().find(|h| h.0.contains(pos))
        {
            response.clone().on_hover_text_at_pointer(format!(
                "Deck {} {} — click to show its lane",
                deck + 1,
                kind.title()
            ));
        }
        if response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
            && let Some(&(_, deck, kind)) = hits.iter().find(|h| h.0.contains(pos))
        {
            let setting = Setting::ColorFx(deck, kind);
            if !self.shown.contains(&setting) {
                self.shown.push(setting);
                self.shown.sort_by_key(lane_order);
            }
            self.collapsed.remove(&Some(deck));
        }
    }

    /// A small + right under a group's row, opening `menu` (the lanes to
    /// show).
    fn add_button(&mut self, ui: &mut Ui, menu: impl FnOnce(&mut Self, &mut Ui)) {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.menu_button(RichText::new("+").strong(), |ui| menu(self, ui))
                .response
                .on_hover_text("Add or remove lanes")
                .tour(crate::tour::Target::EditLanes);
        });
    }

    /// The lanes of a group (a deck, or the mixer's with `None`) to show or
    /// hide; • marks the ones that move.
    fn lane_menu(&mut self, ui: &mut Ui, group: Option<usize>) {
        for setting in Setting::all(deckhand_audio::DECKS) {
            if setting.deck() != group {
                continue;
            }
            let moves = self.lanes[&setting].len() > 1;
            let mut on = self.shown.contains(&setting);
            let label =
                if moves { format!("{} •", setting.label()) } else { setting.label().into() };
            if ui.checkbox(&mut on, label).changed() {
                if on {
                    self.shown.push(setting);
                    self.shown.sort_by_key(lane_order);
                } else {
                    self.shown.retain(|s| *s != setting);
                }
            }
        }
        ui.label(RichText::new("• it moves in the recording").small().weak());
    }

    /// ← and → move the chosen track's move by a millisecond (a tenth
    /// with ⇧).
    fn nudge_move(&mut self, by: f64) {
        let Some((deck, at)) = self.chosen_move else { return };
        let now = self.moves.iter().find(|m| m.deck == deck && m.at == at).map_or(0.0, |m| m.later);
        let later = ((now + by) * 10_000.0).round() / 10_000.0;
        self.set_move(deck, at, later);
    }

    /// Holds the value from before the selection through it.
    fn flatten(&mut self) {
        self.bridge(Curve::Step);
    }

    /// One curve (Line unless Smooth is chosen) through the selection, from
    /// the value at its start to the value at its end.
    fn ramp(&mut self) {
        self.bridge(if self.shape == Curve::Smooth { Curve::Smooth } else { Curve::Line });
    }

    /// Replaces the selection's points with one going from the value at its
    /// start to the value at its end in `curve`; after it the lane goes on
    /// as it did.
    fn bridge(&mut self, curve: Curve) {
        let what = if curve == Curve::Step { "flatten" } else { "ramp" };
        let Some((setting, from, to)) = self.selection else { return };
        let before = self.lanes[&setting].clone();
        let start = Point { at: from, value: lane_value(&before, from), curve };
        // The point the lane was coming from at `to` goes on the same way.
        let going_on = before[before.partition_point(|p| p.at <= to).saturating_sub(1)].curve;
        let end = Point { at: to, value: lane_value(&before, to), curve: going_on };
        let lane = self.lanes.get_mut(&setting).expect("every setting has a lane");
        lane.retain(|p| p.at < from || p.at > to);
        let index = lane.partition_point(|p| p.at < from);
        lane.splice(index..index, [start, end]);
        self.commit(setting, before, what);
    }

    /// Gives the selected points (or the one the selection starts in) a
    /// shape.
    fn reshape(&mut self, curve: Curve) {
        let Some((setting, from, to)) = self.selection else { return };
        let before = self.lanes[&setting].clone();
        let lane = self.lanes.get_mut(&setting).expect("every setting has a lane");
        let first = lane.partition_point(|p| p.at <= from).saturating_sub(1);
        let last = lane.partition_point(|p| p.at < to).max(first + 1);
        for point in &mut lane[first..last] {
            point.curve = curve;
        }
        self.commit(setting, before, "shape");
    }

    /// Zooms around `x` by `factor` (above 1 zooms in).
    fn zoom(&mut self, rect: Rect, x: f32, factor: f64) {
        let pivot = self.view_from + f64::from((x - rect.left()) / rect.width()) * self.view_secs;
        let secs = (self.view_secs / factor).clamp(0.5, self.room_secs().max(1.0));
        self.view_from = pivot - (pivot - self.view_from) * secs / self.view_secs;
        self.view_secs = secs;
        self.clamp_view();
    }

    fn clamp_view(&mut self) {
        let max = (self.room_secs() - self.view_secs * 0.5).max(0.0);
        self.view_from = self.view_from.clamp(-self.view_secs * 0.05, max);
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let playing = self.player.is_some();
        let label = if playing { "⏹ Stop" } else { "▶ Play" };
        if ui
            .add(egui::Button::new(label).selected(playing))
            .on_hover_text("Play from the cursor (Space)")
            .tour(crate::tour::Target::EditPlay)
            .clicked()
        {
            self.toggle_play();
        }
        let at = self.playhead().unwrap_or(self.cursor) as f64 / self.rate;
        ui.label(RichText::new(format!("{}:{:05.2}", (at / 60.0) as u64, at % 60.0)).monospace());
        if self.player.as_ref().is_some_and(|p| !p.ready()) {
            ui.spinner().on_hover_text("Getting to the cursor…");
        }
        if !self.markers.is_empty() {
            let mut chosen = None;
            ui.menu_button(format!("🚩 {}", self.markers.len()), |ui| {
                for (i, &at) in self.markers.iter().enumerate() {
                    let secs = at as f64 / self.rate;
                    let text = format!("{}  {}:{:05.2}", i + 1, (secs / 60.0) as u64, secs % 60.0);
                    if ui.button(text).clicked() {
                        chosen = Some(at);
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("Go to a marker")
            .tour(crate::tour::Target::EditMarkers);
            if let Some(at) = chosen {
                self.go_to(at);
            }
        }
        ui.separator();
        let mut tools: Option<egui::Response> = None;
        for (tool, label, hint) in [
            (Tool::Normal, "Normal", "Move and edit"),
            (
                Tool::Select,
                "Select",
                "Drag to select; Delete removes",
            ),
        ] {
            let keys = &self.tool_keys;
            let key = match tool {
                Tool::Normal => &keys[0],
                Tool::Select => &keys[1],
            };
            let hint = if key.is_empty() { hint.to_owned() } else { format!("{hint} ({key})") };
            let button = tool_button(ui, tool, label, self.tool == tool).on_hover_text(hint);
            if button.clicked() {
                self.tool = tool;
            }
            tools = Some(match tools {
                Some(t) => t.union(button),
                None => button,
            });
        }
        if let Some(tools) = tools {
            tools.tour(crate::tour::Target::EditTools);
        }
        ui.separator();
        ui.label(RichText::new(&self.name).strong());
        let secs = self.secs() as u64;
        ui.label(RichText::new(format!("{}:{:02}", secs / 60, secs % 60)).weak());
        if ui
            .button(super::collection_view::reveal_label())
            .on_hover_text("Show the folder")
            .clicked()
        {
            super::collection_view::reveal(&self.path);
        }
        if ui
            .add_enabled(self.export.is_none(), egui::Button::new("Export with songs…"))
            .on_hover_text(
                "Copy the session with its songs",
            )
            .clicked()
        {
            self.start_export();
        }
        let picked = self.selected();
        let nudged = picked
            .iter()
            .any(|p| self.nudges.iter().any(|n| n.deck == p.deck && n.from < p.to && p.from <= n.to));
        if nudged
            && ui
                .button("Remove nudges")
                .on_hover_text(
                    "Keeps the beat in step (⌥: don't)",
                )
                .clicked()
        {
            let keep = !ui.input(|i| i.modifiers.alt);
            for p in picked {
                self.remove_nudges(p.deck, p.from, p.to, keep);
            }
        }
        ui.separator();
        let idle = !self.busy();
        let undo = self.undo.last().map(|c| format!("Undo {} (⌘Z)", c.label));
        if ui
            .add_enabled(idle && undo.is_some(), egui::Button::new("Undo"))
            .on_hover_text(undo.unwrap_or_default())
            .tour(crate::tour::Target::EditUndo)
            .clicked()
        {
            self.undo();
        }
        let redo = self.redo.last().map(|c| format!("Redo {} (⇧⌘Z)", c.label));
        if ui
            .add_enabled(idle && redo.is_some(), egui::Button::new("Redo"))
            .on_hover_text(redo.unwrap_or_default())
            .clicked()
        {
            self.redo();
        }
        ui.add_enabled_ui(idle, |ui| ui.menu_button("History", |ui| self.history(ui)));
        let selected = idle && self.selection.is_some();
        if ui
            .add_enabled(selected, egui::Button::new("Flatten"))
            .on_hover_text("Hold the value through the selection")
            .clicked()
        {
            self.flatten();
        }
        if ui
            .add_enabled(selected, egui::Button::new("Ramp"))
            .on_hover_text("Straight line through the selection")
            .clicked()
        {
            self.ramp();
        }
        ui.separator();
        for (curve, label, hint) in [
            (Curve::Step, "Step", "Hold, then jump"),
            (Curve::Line, "Line", "Straight line"),
            (Curve::Smooth, "Smooth", "Ease in and out"),
        ] {
            let hint = format!("{hint} (new and selected points)");
            if ui.selectable_label(self.shape == curve, label).on_hover_text(hint).clicked() {
                self.shape = curve;
                if idle && self.selection.is_some() {
                    self.reshape(curve);
                }
            }
        }
        ui.separator();
        let clashes = if self.show_clashes && !self.played.is_empty() {
            format!("Bass clashes ({})", self.clashes.len())
        } else {
            "Bass clashes".to_owned()
        };
        ui.toggle_value(&mut self.show_clashes, clashes).on_hover_text(
            "Where two decks' bass overlaps",
        );
        if self.heard_stale
            && ui
                .button("⟳ Waveforms")
                .on_hover_text(
                    "Redraw with mix changes",
                )
                .clicked()
        {
            self.played_stale = true;
        }
        if !self.sync.is_empty() {
            ui.separator();
            ui.checkbox(&mut self.follow, "Synced decks follow").on_hover_text(
                "Synced decks follow tempo changes",
            );
        }
        ui.separator();
        ui.toggle_value(&mut self.snap, "Snap")
            .on_hover_text("Snap to the grid (⌘: free, ⇧: fine)")
            .tour(crate::tour::Target::EditSnap);
        ui.toggle_value(&mut self.follow_playhead, "Follow playhead").on_hover_text(
            "Scroll along while playing",
        );
        if ui.button("Fit").on_hover_text("The whole set on view").clicked() {
            (self.view_from, self.view_secs) = (0.0, self.secs().max(1.0));
        }
    }

    /// The timeline: ruler, the decks' tracks, then the lanes.
    fn timeline(&mut self, ui: &mut Ui) {
        let width = ui.available_width();
        // Select: an I-beam over the timeline (rows set their own after).
        if self.tool == Tool::Select
            && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
            && ui.max_rect().contains(pos)
            && pos.x >= ui.max_rect().left() + LABELS
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }
        let (ruler, response) = ui.allocate_exact_size(vec2(width, RULER), Sense::click_and_drag());
        let response = response.tour(crate::tour::Target::EditRuler);
        let area = Rect::from_min_max(pos2(ruler.left() + LABELS, ruler.top()), ruler.max);
        self.ruler(ui, area);
        // A click (or drag) on the ruler puts the cursor there; playing
        // goes on from there.
        let middle = ui.input(|i| i.pointer.button_down(egui::PointerButton::Middle));
        let pressed = ui.input(|i| i.pointer.press_origin()).filter(|p| p.x >= area.left());
        // Either tool: the range's sides can be dragged.
        let side_at = |x: f32| {
            self.range.and_then(|(from, to)| {
                if (self.x_of(area, to) - x).abs() <= GRAB {
                    Some(true)
                } else if (self.x_of(area, from) - x).abs() <= GRAB {
                    Some(false)
                } else {
                    None
                }
            })
        };
        if response.hover_pos().is_some_and(|p| side_at(p.x).is_some()) || self.range_side.is_some()
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if response.drag_started()
            && !middle
            && let Some(side) = pressed.and_then(|p| side_at(p.x))
        {
            self.range_side = Some(side);
        }
        if let Some(right) = self.range_side {
            if let (Some((from, to)), Some(pos)) = (self.range, response.interact_pointer_pos()) {
                let at = self.at_of(area, pos.x);
                self.range =
                    Some(if right { (from, at.max(from + 1)) } else { (at.min(to - 1), to) });
            }
            if response.drag_stopped() {
                self.range_side = None;
            }
        } else if self.tool == Tool::Select && !middle {
            // Select: drag along the ruler to mark a range; a click lets go
            // of it and puts the cursor there.
            if response.drag_started()
                && let Some(pos) = pressed
            {
                let at = self.at_of(area, pos.x);
                self.range_start = Some(at);
                self.range = Some((at, at));
            }
            if response.dragged()
                && let (Some(start), Some(pos)) =
                    (self.range_start, response.interact_pointer_pos())
            {
                let at = self.at_of(area, pos.x);
                self.range = Some((start.min(at), start.max(at)));
            }
            if response.drag_stopped() {
                self.range_start = None;
                if self.range.is_some_and(|(a, b)| b <= a) {
                    self.range = None;
                }
            }
            if response.clicked()
                && let Some(pos) = response.interact_pointer_pos().filter(|p| p.x >= area.left())
            {
                self.range = None;
                self.cursor = self.at_of(area, pos.x);
            }
        } else if let Some(pos) = response.interact_pointer_pos().filter(|p| p.x >= area.left())
            && !middle
            && (response.clicked() || response.dragged())
        {
            self.cursor = self.at_of(area, pos.x);
            if response.clicked() && self.player.is_some() {
                self.play_from(self.cursor);
            }
        }
        self.marker_clicks(ui, area, &response);
        // The range on the ruler, bright, with its times.
        if let Some((from, to)) = self.range {
            let band = Rect::from_x_y_ranges(
                self.x_of(area, from)..=self.x_of(area, to),
                area.top()..=area.bottom(),
            )
            .intersect(area);
            if band.width() > 0.0 {
                let painter = ui.painter_at(area);
                painter.rect_filled(band, 0.0, RANGE.gamma_multiply(0.55));
                let time = |at: u64| {
                    let secs = at as f64 / self.rate;
                    format!("{}:{:05.2}", (secs / 60.0) as u64, secs % 60.0)
                };
                painter.text(
                    band.left_bottom() + vec2(3.0, -2.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("{} – {}", time(from), time(to)),
                    egui::FontId::monospace(10.0),
                    Color32::WHITE,
                );
            }
        }
        if self.show_clashes {
            self.clash_bands(ui, area, &response);
        }
        let over = ui.rect_contains_pointer(ui.max_rect());
        self.hover = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|p| over && p.x >= area.left() && p.x <= area.right())
            .map(|p| self.at_of(area, p.x));
        if over {
            let (zoom, scroll, pointer) =
                ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.hover_pos()));
            let x = pointer.map_or(area.center().x, |p| p.x);
            if zoom != 1.0 {
                self.zoom(area, x, f64::from(zoom));
            }
            if scroll.x != 0.0 {
                self.view_from -= f64::from(scroll.x / area.width()) * self.view_secs;
                self.clamp_view();
            }
        }
        // Middle button held: the view follows the pointer sideways through
        // the set; the rows stay put.
        let (middle, delta) =
            ui.input(|i| (i.pointer.button_down(egui::PointerButton::Middle), i.pointer.delta()));
        let panning = middle && (over || self.panning);
        self.panning = panning;
        if panning {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            if delta.x != 0.0 {
                self.view_from -= f64::from(delta.x / area.width()) * self.view_secs;
                self.clamp_view();
            }
        }
        self.snap_line = self.snap_line.filter(|_| self.dragging_clip());
        let rows = egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            self.master_rows(ui, width);
            ui.add_space(10.0);
            // Each deck: its tracks, then its lanes; the mixer's last.
            let mut decks: Vec<usize> = self.tracks.iter().map(|t| t.deck).collect();
            decks.extend(self.shown.iter().filter_map(|s| s.deck()));
            decks.sort();
            decks.dedup();
            let groups = decks.into_iter().map(Some).chain([None]);
            for (i, deck) in groups.enumerate() {
                let lanes: Vec<Setting> =
                    self.shown.iter().copied().filter(|s| s.deck() == deck).collect();
                if i > 0 {
                    ui.add_space(10.0);
                }
                let height = if deck.is_some() { TRACK_ROW } else { DECK_ROW };
                let (row, response) =
                    ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
                let folded = self.fold(ui, row, &response, deck);
                match deck {
                    Some(deck) => self.deck_row(ui, row, &response, deck),
                    None => {
                        self.group_name(ui, row, "Mixer", ui.visuals().strong_text_color(), folded)
                    }
                }
                if folded {
                    continue;
                }
                self.add_button(ui, |editor, ui| editor.lane_menu(ui, deck));
                for setting in lanes {
                    let (row, response) =
                        ui.allocate_exact_size(vec2(width, LANE_ROW), Sense::click_and_drag());
                    self.lane(ui, row, &response, setting);
                }
            }
            // Effects, apart from the decks and the mixer.
            ui.add_space(10.0);
            let (row, response) = ui.allocate_exact_size(vec2(width, DECK_ROW), Sense::click());
            let folded = self.fold(ui, row, &response, EFFECTS);
            self.group_name(ui, row, "Effects", ui.visuals().strong_text_color(), folded);
            if !folded {
                self.add_button(ui, |editor, ui| {
                    ui.checkbox(&mut editor.show_color_fx, "Color FX (where they were on)");
                    ui.checkbox(&mut editor.show_beat_fx, "Beat FX");
                    ui.label(
                        RichText::new("Color FX are changed per deck: under each deck's +")
                            .small()
                            .weak(),
                    );
                });
                if self.show_color_fx {
                    let decks = self.color_decks();
                    let height = 6.0 + 14.0 * decks.len().max(1) as f32;
                    let (row, response) =
                        ui.allocate_exact_size(vec2(width, height), Sense::click());
                    self.color_overview(ui, row, &response, &decks);
                    ui.add_space(2.0);
                }
                if self.show_beat_fx {
                    let (row, response) =
                        ui.allocate_exact_size(vec2(width, FX_ROW), Sense::click_and_drag());
                    self.fx_row(ui, row, &response);
                }
            }
        });
        self.snap_guide(ui, Rect::from_x_y_ranges(area.x_range(), rows.inner_rect.y_range()));
        self.add_fx_dialog(ui.ctx());
        self.point_edit_dialog(ui.ctx());
    }

    /// A click on a group's name folds its lanes away or back; whether
    /// they are folded.
    fn fold(
        &mut self,
        ui: &Ui,
        row: Rect,
        response: &egui::Response,
        group: Option<usize>,
    ) -> bool {
        let on_name = response.interact_pointer_pos().is_some_and(|p| p.x < row.left() + LABELS);
        if response.clicked() && on_name && !self.collapsed.remove(&group) {
            self.collapsed.insert(group);
        }
        if response.hover_pos().is_some_and(|p| p.x < row.left() + LABELS) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        self.collapsed.contains(&group)
    }

    /// A group's name, with ⏵ when folded and ⏷ when open.
    fn group_name(&self, ui: &Ui, row: Rect, name: &str, color: Color32, folded: bool) {
        let arrow = if folded { "⏵" } else { "⏷" };
        ui.painter().text(
            pos2(row.left() + 4.0, row.center().y),
            egui::Align2::LEFT_CENTER,
            format!("{arrow} {name}"),
            egui::FontId::proportional(13.0),
            color,
        );
    }

    /// The clashes as red bands under the ruler, saying which decks.
    fn clash_bands(&self, ui: &Ui, area: Rect, response: &egui::Response) {
        let painter = ui.painter_at(area);
        let db = |level: f32| format!("{:.0} dB", 20.0 * level.max(1e-6).log10());
        for clash in &self.clashes {
            let band = Rect::from_x_y_ranges(
                self.x_of(area, clash.from)
                    ..=self.x_of(area, clash.to).max(self.x_of(area, clash.from) + 2.0),
                area.bottom() - 6.0..=area.bottom(),
            );
            painter.rect_filled(band, 1.0, CLASH);
            if response.hover_pos().is_some_and(|p| band.expand2(vec2(2.0, 6.0)).contains(p)) {
                let time = |at: u64| {
                    let secs = at as f64 / self.rate;
                    format!("{}:{:04.1}", (secs / 60.0) as u64, secs % 60.0)
                };
                let (a, b) = clash.decks;
                response.clone().on_hover_text_at_pointer(format!(
                    "Deck {} and deck {} low end together, {}–{} ({} / {}): cut one's low EQ",
                    a + 1,
                    b + 1,
                    time(clash.from),
                    time(clash.to),
                    db(clash.levels.0),
                    db(clash.levels.1),
                ));
            }
        }
    }

    fn ruler(&self, ui: &Ui, area: Rect) {
        let painter = ui.painter_at(area);
        let visuals = ui.visuals();
        painter.rect_filled(area, 0.0, visuals.faint_bg_color);
        let per_px = self.view_secs / f64::from(area.width());
        let step = [0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0]
            .into_iter()
            .find(|s| s / per_px >= 70.0)
            .unwrap_or(600.0);
        let mut t = (self.view_from / step).floor().max(0.0) * step;
        while t <= self.view_from + self.view_secs {
            let x = self.x_of(area, (t * self.rate) as u64);
            painter.line_segment(
                [pos2(x, area.top() + TIME_STRIP - 5.0), pos2(x, area.top() + TIME_STRIP)],
                Stroke::new(1.0_f32, visuals.weak_text_color()),
            );
            let label = if step < 1.0 {
                format!("{}:{:04.1}", (t / 60.0) as u64, t % 60.0)
            } else {
                format!("{}:{:02}", (t / 60.0) as u64, (t % 60.0) as u64)
            };
            painter.text(
                pos2(x + 3.0, area.top() + 2.0),
                egui::Align2::LEFT_TOP,
                label,
                egui::FontId::proportional(11.0),
                visuals.weak_text_color(),
            );
            t += step;
        }
        self.grid_ruler(ui, &painter, area);
        self.play_marks(&painter, area);
        // Markers: a flag each, numbered.
        for (i, &at) in self.markers.iter().enumerate() {
            let x = self.x_of(area, at);
            let top = area.top() + 1.0;
            painter.add(Shape::convex_polygon(
                vec![pos2(x, top), pos2(x + 16.0, top + 6.0), pos2(x, top + 12.0)],
                MARKER,
                Stroke::NONE,
            ));
            painter.text(
                pos2(x + 2.0, top + 6.0),
                egui::Align2::LEFT_CENTER,
                (i + 1).to_string(),
                egui::FontId::proportional(9.0),
                Color32::BLACK,
            );
        }
        // The cursor's handle.
        let x = self.x_of(area, self.cursor);
        painter.add(Shape::convex_polygon(
            vec![pos2(x - 5.0, area.top()), pos2(x + 5.0, area.top()), pos2(x, area.top() + 7.0)],
            PLAY,
            Stroke::NONE,
        ));
    }

    /// A deck's tracks. Dragging one moves its audio (from where it starts
    /// playing, or with ⌥ from where it is grabbed); moves show as marks.
    fn deck_row(&mut self, ui: &Ui, row: Rect, response: &egui::Response, deck: usize) {
        let color = DECK_COLORS[deck % DECK_COLORS.len()];
        let folded = self.collapsed.contains(&Some(deck));
        self.group_name(ui, row, &format!("Deck {}", deck + 1), color, folded);
        let area = Rect::from_min_max(pos2(row.left() + LABELS, row.top() + 2.0), row.max);
        let painter = ui.painter_at(area);
        for span in self.tracks.iter().filter(|t| t.deck == deck) {
            let shifted = |at: u64, by: i64| (at as i64 + by).max(0) as u64;
            let (start, end) = match self.preview {
                Some(p) if p.whole && p.deck == deck && p.at == span.from => {
                    (shifted(span.from, p.frames), shifted(span.to, p.frames))
                }
                Some(p) if p.deck == deck && p.to == span.to => {
                    (span.from, shifted(span.to, p.frames).max(span.to))
                }
                _ => (span.from, span.to),
            };
            let block = Rect::from_x_y_ranges(
                self.x_of(area, start)..=self.x_of(area, end),
                area.top()..=area.bottom() - 2.0,
            );
            let by = start as i64 - span.from as i64;
            if by != 0 {
                let was = Rect::from_x_y_ranges(
                    self.x_of(area, span.from)..=self.x_of(area, span.to),
                    area.top()..=area.bottom() - 2.0,
                );
                painter.rect_stroke(
                    was,
                    3.0,
                    Stroke::new(1.0_f32, color.gamma_multiply(0.4)),
                    egui::StrokeKind::Inside,
                );
            }
            let shown = self.visible(span);
            let clip_color = self.clip_color(&span.path);
            let hovered = response
                .hover_pos()
                .filter(|p| area.contains(*p) && p.y > area.top() + 16.0)
                .map(|p| self.at_of(area, p.x));
            // A clip never played is silent: nothing drawn, not even an
            // outline.
            let mut name = span.path.file_stem().map_or_else(
                || "(not from a file)".to_owned(),
                |n| n.to_string_lossy().into_owned(),
            );
            if let Some(bpm) = self.bpms.get(&span.path) {
                name.push_str(&format!("  ·  {bpm:.1} BPM"));
            }
            let mut boxes = Vec::with_capacity(shown.len());
            for (i, &(from, to)) in shown.iter().enumerate() {
                let end = if to == span.to { end } else { shifted(to, by) };
                let left = self.x_of(area, shifted(from, by));
                let piece = Rect::from_x_y_ranges(
                    left..=self.x_of(area, end).max(left + 2.0),
                    block.y_range(),
                );
                boxes.push(piece);
                painter.rect_filled(piece, 3.0, clip_color.gamma_multiply(0.22));
                // Its own audio, then what of it reached the mix.
                let drawn = self.clip_outline(
                    &painter,
                    area,
                    piece.intersect(area),
                    span,
                    (from, to),
                    shifted(from, by) as i64 - from as i64,
                    clip_color,
                );
                self.waveform(&painter, area, piece.intersect(area), deck, clip_color, drawn);
                self.cue_marks(&painter, piece.intersect(area), area, (deck, &span.path), (from, to), by);
                self.nudge_marks(&painter, piece.intersect(area), area, deck, (from, to), by, hovered);
                // Its name on a strip of its own colour.
                let strip = Rect::from_x_y_ranges(piece.x_range(), piece.top()..=piece.top() + 15.0);
                painter.rect_filled(strip, 3.0, clip_color.gamma_multiply(0.55));
                let text = ui.painter().layout_no_wrap(
                    name.clone(),
                    egui::FontId::proportional(12.0),
                    Color32::WHITE,
                );
                let room = strip.intersect(area).shrink2(vec2(4.0, 0.0));
                if room.width() > 8.0 {
                    ui.painter().with_clip_rect(room).galley(
                        pos2(room.left(), strip.top() + 1.0),
                        text,
                        Color32::WHITE,
                    );
                }
                let over = hovered.is_some_and(|at| from <= at && at < to);
                painter.rect_stroke(
                    piece,
                    3.0,
                    Stroke::new(
                        if over { 1.5_f32 } else { 1.0 },
                        clip_color.gamma_multiply(if over { 1.0 } else { 0.7 }),
                    ),
                    egui::StrokeKind::Inside,
                );
                // Where it was split.
                if i > 0 && shown[i - 1].1 == from {
                    painter.line_segment(
                        [piece.left_top(), piece.left_bottom()],
                        Stroke::new(1.5_f32, Color32::WHITE.gamma_multiply(0.7)),
                    );
                }
            }
            // Its low end clashing with another deck's.
            if self.show_clashes {
                for clash in self.clashes.iter().filter(|c| c.decks.0 == deck || c.decks.1 == deck)
                {
                    for b in &boxes {
                        let mark = Rect::from_x_y_ranges(
                            self.x_of(area, clash.from)..=self.x_of(area, clash.to),
                            block.y_range(),
                        )
                        .intersect(*b);
                        if mark.width() > 0.0 {
                            painter.rect_filled(mark, 0.0, CLASH.gamma_multiply(0.25));
                        }
                    }
                }
            }
            // Synced: a line in the master's colour along the top.
            for sync in self.sync.iter().filter(|s| s.deck == deck) {
                for b in &boxes {
                    let line = Rect::from_x_y_ranges(
                        self.x_of(area, sync.from)..=self.x_of(area, sync.to),
                        b.top()..=b.top() + 2.0,
                    )
                    .intersect(*b);
                    if line.width() > 0.0 {
                        painter.rect_filled(line, 0.0, DECK_COLORS[sync.master % DECK_COLORS.len()]);
                    }
                }
            }
        }
        // The moves, as marks with how far.
        let marks: Vec<(TrackMove, f32)> = self
            .moves
            .iter()
            .filter(|m| m.deck == deck)
            .map(|m| (*m, self.x_of(area, m.at)))
            .collect();
        for (m, x) in &marks {
            let chosen = self.chosen_move == Some((m.deck, m.at));
            let mark = if chosen { Color32::WHITE } else { color };
            painter.add(Shape::convex_polygon(
                vec![
                    pos2(*x, area.bottom() - 8.0),
                    pos2(x - 5.0, area.bottom()),
                    pos2(x + 5.0, area.bottom()),
                ],
                mark,
                Stroke::NONE,
            ));
            painter.text(
                pos2(x + 6.0, area.bottom() - 1.0),
                egui::Align2::LEFT_BOTTOM,
                self.describe_move(m.deck, m.at, m.later),
                egui::FontId::monospace(10.0),
                mark,
            );
        }
        self.play_marks(&painter, area);
        let near_mark = |pos: Pos2| {
            marks.iter().find(|(_, x)| (x - pos.x).abs() <= GRAB).map(|(m, _)| (m.deck, m.at))
        };
        let pointer =
            response.interact_pointer_pos().or(response.hover_pos()).filter(|p| p.x >= area.left());
        let mine: Vec<Picked> = self.selected().into_iter().filter(|p| p.deck == deck).collect();
        for p in &mine {
            let selected = Rect::from_x_y_ranges(
                self.x_of(area, p.from)..=self.x_of(area, p.to),
                area.y_range(),
            );
            painter.rect_filled(selected, 0.0, ui.visuals().selection.bg_fill.gamma_multiply(0.45));
        }
        // Being moved: where each lands.
        if let Some(frames) = self.sel_drag.as_ref().map(|d| d.frames) {
            for p in &mine {
                let ghost = Rect::from_x_y_ranges(
                    self.x_of(area, shift(p.from, frames))..=self.x_of(area, shift(p.to, frames)),
                    area.y_range(),
                );
                painter.rect_filled(ghost, 2.0, color.gamma_multiply(0.45));
                painter.rect_stroke(
                    ghost,
                    2.0,
                    Stroke::new(1.5_f32, Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
            }
        }
        // A drag on this deck must always end: let go over the labels, off
        // the window or wherever, it ends here.
        let holding = self.clip_drag.as_ref().is_some_and(|d| d.deck == deck)
            || self.move_drag.as_ref().is_some_and(|d| d.deck == deck)
            || self.sel_drag.as_ref().is_some_and(|d| d.deck == deck)
            || self.edge_drag.as_ref().is_some_and(|d| d.span.deck == deck)
            || self.play_edge.as_ref().is_some_and(|d| d.deck == deck);
        if holding && (self.busy() || self.panning) {
            self.clip_drag = None;
            self.move_drag = None;
            self.sel_drag = None;
            self.edge_drag = None;
            self.play_edge = None;
            self.preview = None;
            return;
        }
        if !holding && (self.busy() || self.panning || pointer.is_none()) {
            return;
        }
        // Let go: also when egui lost track of the drag (the button is up
        // but no stop came).
        let stopped = response.drag_stopped() || holding && !response.dragged();
        // Select: drag a stretch of the deck's clips (Delete cuts it out).
        if self.tool == Tool::Select {
            let pressed = ui.input(|i| i.pointer.press_origin()).filter(|p| p.x >= area.left());
            if response.drag_started()
                && let Some(pos) = pressed
            {
                let at = self.at_of(area, pos.x);
                self.select_only(Selected::Clip);
                self.clip_selection = Some((deck, at, at));
                self.range_from = at;
            }
            if response.dragged()
                && let Some(pos) = pointer
                && self.clip_selection.is_some_and(|s| s.0 == deck)
            {
                let (a, b) = (self.range_from, self.at_of(area, pos.x));
                self.clip_selection = Some((deck, a.min(b), a.max(b)));
            }
            if response.drag_stopped() && self.clip_selection.is_some_and(|s| s.1 >= s.2) {
                self.clip_selection = None;
            }
            if response.clicked() {
                self.clip_selection = None;
            }
            return;
        }
        // Normal tool: dragging the selected part moves just it, laid over
        // what is there; dragging or clicking anything else lets go of the
        // selection and goes on as usual.
        let selected_x: Vec<(f32, f32)> =
            mine.iter().map(|p| (self.x_of(area, p.from), self.x_of(area, p.to))).collect();
        let in_selection =
            |pos: Pos2| selected_x.iter().any(|&(left, right)| pos.x >= left && pos.x <= right);
        let command = ui.input(|i| i.modifiers.command);
        let origin = ui.input(|i| i.pointer.press_origin()).filter(|p| p.x >= area.left());
        let alt = ui.input(|i| i.modifiers.alt);
        if response.drag_started() {
            match origin {
                // A stretch selected (Select), or several things picked:
                // they move together. A clip clicked moves as a clip.
                Some(pos)
                    if in_selection(pos)
                        && !command
                        && (self.tool == Tool::Select || !self.picked.is_empty()) =>
                {
                    self.sel_drag = Some(SelDrag { deck, x: pos.x, frames: 0 });
                }
                _ if command => {}
                _ => self.clear_selections(),
            }
        }
        if (response.clicked() || response.secondary_clicked())
            && !command
            && pointer.is_some_and(|p| !in_selection(p))
        {
            self.clear_selections();
        }
        // Cmd/Ctrl-click: picks a play (or piece), or by its title strip the
        // whole clip, along with what is selected; again lets go of it.
        if response.clicked()
            && command
            && let Some(pos) = pointer
        {
            let at = self.at_of(area, pos.x);
            if let Some(span) = self.clip_at(deck, at).cloned() {
                let item = if pos.y <= area.top() + 16.0 {
                    Picked { deck, from: span.from, to: span.to, whole: true }
                } else {
                    let (from, to) = self
                        .play_at(deck, at)
                        .or_else(|| {
                            self.span_pieces(&span).into_iter().find(|&(f, t)| f <= at && at < t)
                        })
                        .unwrap_or((span.from, span.to));
                    Picked { deck, from, to, whole: false }
                };
                self.toggle_pick(item);
            }
            return;
        }
        if self.sel_drag.as_ref().is_some_and(|d| d.deck == deck) {
            if let Some(pos) = pointer
                && let Some(x0) = self.sel_drag.as_ref().map(|d| d.x)
            {
                let secs = f64::from(pos.x - x0) * self.view_secs / f64::from(area.width());
                // A stretch stays inside its clip (the deck plays the track
                // only there); all go as far as the tightest allows.
                let (low, high) = self.drag_limits();
                let frames = ((secs * self.rate).round() as i64).clamp(low, high);
                if let Some(drag) = self.sel_drag.as_mut() {
                    drag.frames = frames;
                }
                painter.text(
                    pos2(pos.x + 12.0, area.top() + 2.0),
                    egui::Align2::LEFT_TOP,
                    format!("{:+.2} s", frames as f64 / self.rate),
                    egui::FontId::monospace(12.0),
                    Color32::WHITE,
                );
            }
            if stopped
                && let Some(drag) = self.sel_drag.take()
            {
                self.move_selection(drag.frames);
            }
            return;
        }
        // A nudge's mark along the clip's foot: right-click takes it out
        // (⌥: without keeping step).
        let on_nudge = pointer
            .filter(|p| p.y >= area.bottom() - 9.0)
            .and_then(|p| self.nudge_near(area, deck, self.at_of(area, p.x)));
        if response.secondary_clicked()
            && let Some(n) = on_nudge
        {
            let keep = !ui.input(|i| i.modifiers.alt);
            self.remove_nudges(deck, n.from, n.to, keep);
        } else if response.secondary_clicked()
            && let Some(pos) = pointer
        {
            let at = self.at_of(area, pos.x);
            match (near_mark(pos), self.play_at(deck, at)) {
                (Some((deck, at)), _) => self.set_move(deck, at, 0.0),
                // On a play still heard: just that play goes.
                (None, Some((from, to))) if self.heard_at(deck, at) => {
                    self.cut_out(deck, from, to, "cut out play");
                }
                (None, _) => self.bring_back(deck, at),
            }
        }
        if response.double_clicked()
            && let Some(pos) = pointer
        {
            self.split(deck, self.at_of(area, pos.x));
        }
        if response
            .hover_pos()
            .filter(|p| p.x >= area.left())
            .and_then(|p| self.edge_at(area, deck, p.x))
            .is_some()
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if response.clicked()
            && let Some(pos) = pointer
        {
            self.chosen_move = near_mark(pos);
            // A play clicked: selected (Delete cuts it out, dragging it
            // moves it).
            if self.chosen_move.is_none()
                && !command
                && pos.y > area.top() + 16.0
                && let Some((from, to)) = self.play_at(deck, self.at_of(area, pos.x))
            {
                self.select_only(Selected::Clip);
                self.clip_selection = Some((deck, from, to));
            }
            // Its title clicked: the whole clip (Delete deletes it, as in a
            // DAW).
            let whole = self.clip_at(deck, self.at_of(area, pos.x)).map(|span| {
                let shown = self.visible(span);
                (shown.first().map_or(span.from, |v| v.0), shown.last().map_or(span.to, |v| v.1))
            });
            if self.chosen_move.is_none()
                && !command
                && pos.y <= area.top() + 16.0
                && let Some((from, to)) = whole
            {
                self.select_only(Selected::Clip);
                self.clip_selection = Some((deck, from, to));
            }
        }
        // The title strip along the top: the whole clip.
        let on_title = |pos: Pos2| pos.y <= area.top() + 16.0;
        if let Some(pos) = response
            .hover_pos()
            .filter(|p| p.x >= area.left() && (on_title(*p) || !ui.input(|i| i.modifiers.alt)))
        {
            let over = self.at_of(area, pos.x);
            if self.clip_at(deck, over).is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        let pressed = ui.input(|i| i.pointer.press_origin()).filter(|p| p.x >= area.left());
        let pressed_edge =
            pressed.filter(|p| !on_title(*p)).and_then(|p| self.edge_at(area, deck, p.x));
        if response.drag_started()
            && let Some(hit) = pressed_edge
        {
            match hit {
                EdgeHit::Piece(span, index, right) => {
                    let pieces = self.span_pieces(&span);
                    self.edge_drag = Some(EdgeDrag { span, index, right, pieces });
                }
                EdgeHit::Play(play, right) => {
                    // One of several plays: it changes alone.
                    self.separate(deck, play.0);
                    let edges = self.session.lock().ok().and_then(|s| s.play_edges(deck, play.0, play.1));
                    let heard_end = self
                        .tracks
                        .iter()
                        .find(|t| t.deck == deck && t.playing.contains(&play))
                        .and_then(|t| {
                            self.visible(t).into_iter().filter(|v| play.0 <= v.0 && v.1 <= play.1).map(|v| v.1).max()
                        })
                        .unwrap_or(play.1);
                    let heard = (play.0, heard_end);
                    match edges {
                        Some(edges) if right || edges.start != (play.0, play.0) => {
                            self.play_edge =
                                Some(PlayEdgeDrag { deck, play, heard, now: heard, edges, right });
                        }
                        _ => {
                            self.note = Some(
                                "It was already playing when loaded: its start stays".into(),
                            );
                        }
                    }
                }
            }
        } else if response.drag_started()
            && self.sel_drag.is_none()
            && let Some(pos) = pressed
            // By its title or its body, as in a DAW (⌥ on the body: its
            // audio slides inside instead).
            && (on_title(pos)
                || !alt
                    && near_mark(pos).is_none()
                    && self.clip_at(deck, self.at_of(area, pos.x)).is_some())
        {
            let grabbed = self.at_of(area, pos.x);
            // One of several plays: it goes alone.
            self.separate(deck, grabbed);
            if let Some(span) = self.clip_at(deck, grabbed) {
                self.clip_drag = Some(ClipDrag { deck, from: span.from, to: span.to, x: pos.x });
            }
        } else if response.drag_started()
            && let Some(pos) = pointer
        {
            let grabbed = self.at_of(area, pos.x);
            if let Some(span) = self.clip_at(deck, grabbed) {
                let from_here = ui.input(|i| i.modifiers.alt);
                // Split: the piece grabbed and what follows it; else the
                // whole track, from where it starts playing.
                let pieces = self.span_pieces(span);
                let piece =
                    pieces.iter().skip(1).find(|p| p.0 <= grabbed && grabbed < p.1).map(|p| p.0);
                let start =
                    piece.unwrap_or_else(|| span.playing.first().map_or(span.from, |p| p.0));
                let at = match near_mark(pos) {
                    Some((_, at)) => at,
                    None if from_here => grabbed,
                    None => start,
                };
                let at = self.session.lock().map_or(at, |s| s.snap(at));
                let base = self
                    .moves
                    .iter()
                    .find(|m| m.deck == deck && m.at == at)
                    .map_or(0.0, |m| m.later);
                let to = span.to;
                // From where the button went down, not where the drag began.
                let x = ui.input(|i| i.pointer.press_origin()).map_or(pos.x, |p| p.x);
                self.move_drag = Some(MoveDrag { deck, at, base, x, later: base, to });
            }
        }
        if self.play_edge.as_ref().is_some_and(|d| d.deck == deck) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            if let Some(pos) = pointer {
                let mut at = self.at_of(area, pos.x);
                // Onto the master grid when close (⌘: free).
                if self.snapping(ui) {
                    let near = self.grid.get(self.grid.partition_point(|g| g.at < at).saturating_sub(1)..)
                        .into_iter()
                        .flatten()
                        .take(2)
                        .map(|g| g.at)
                        .min_by_key(|g| g.abs_diff(at));
                    if let Some(g) = near
                        && (self.x_of(area, g) - pos.x).abs() <= GRAB + 1.0
                    {
                        at = g;
                        self.snap_line = Some(g);
                    } else {
                        self.snap_line = None;
                    }
                }
                let readout = self.play_edge.as_mut().map(|drag| {
                    let (e, (from, to)) = (drag.edges, drag.now);
                    if drag.right {
                        drag.now.1 = at.clamp(e.end.0.max(from + 1), e.end.1.max(e.end.0));
                        drag.now.1 as i64 - drag.heard.1 as i64
                    } else {
                        drag.now.0 = at.clamp(e.start.0, e.start.1.min(to.saturating_sub(1)));
                        drag.now.0 as i64 - drag.play.0 as i64
                    }
                });
                if let Some(frames) = readout {
                    painter.text(
                        pos2(pos.x + 12.0, area.top() + 2.0),
                        egui::Align2::LEFT_TOP,
                        self.describe_shift(frames),
                        egui::FontId::monospace(12.0),
                        Color32::WHITE,
                    );
                }
            }
            if stopped
                && let Some(drag) = self.play_edge.take()
            {
                self.resize_play(&drag);
            }
            return;
        }
        if self.edge_drag.as_ref().is_some_and(|e| e.span.deck == deck) {
            let at = pointer.map(|p| self.at_of(area, p.x));
            if let (Some(drag), Some(at)) = (self.edge_drag.as_mut(), at) {
                let (span, i) = (&drag.span, drag.index);
                let before = drag.pieces[..i].last().map_or(span.from, |p| p.1);
                let after = drag.pieces.get(i + 1).map_or(span.to, |p| p.0);
                let piece = &mut drag.pieces[i];
                if drag.right {
                    piece.1 = at.clamp(piece.0 + 1, after);
                } else {
                    piece.0 = at.clamp(before, piece.1 - 1);
                }
            }
            if stopped
                && let Some(drag) = self.edge_drag.take()
            {
                self.set_pieces(&drag.span, drag.pieces, "shrink");
            }
            return;
        }
        if let Some(&ClipDrag { deck: d, from, to, x: x0 }) = self.clip_drag.as_ref()
            && d == deck
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            if let Some(pos) = pointer {
                let secs = f64::from(pos.x - x0) * self.view_secs / f64::from(area.width());
                let mut frames = (secs * self.rate).round() as i64;
                // Like a DAW: the clip's beat nearest the grab lands on the
                // master grid (⌘ held: free).
                if self.snapping(ui) {
                    let grab = self.at_of(area, x0);
                    frames = self.snap_shift(area, deck, (from, to), grab, frames, true);
                }
                self.preview =
                    Some(Preview { deck, at: from, to, frames, whole: true, until_job: u64::MAX });
                painter.text(
                    pos2(pos.x + 10.0, area.top() + 2.0),
                    egui::Align2::LEFT_TOP,
                    self.describe_shift(frames),
                    egui::FontId::monospace(12.0),
                    Color32::WHITE,
                );
            }
            if stopped {
                self.clip_drag = None;
                let frames = self.preview.filter(|p| p.whole).map_or(0, |p| p.frames);
                self.move_clip(deck, from, frames);
            }
            return;
        }
        let dragging =
            self.move_drag.as_ref().filter(|d| d.deck == deck).map(|d| (d.at, d.base, d.x, d.to));
        if let Some((at, base, x0, to)) = dragging {
            // Seconds of the track playing in a second of the set.
            let tempo = self.value_at(Setting::Tempo(deck), at).max(0.01);
            if let Some(pos) = pointer {
                // The audio follows the pointer (ten times finer with ⇧).
                let fine = ui.input(|i| i.modifiers.shift);
                let per_point =
                    self.view_secs / f64::from(area.width()) * if fine { 0.1 } else { 1.0 };
                let mut later = base + f64::from(pos.x - x0) * per_point * tempo;
                // Near a master beat, the track's beat sticks to it (not
                // with ⇧, for fine nudges; ⌘: free).
                if !fine && self.snapping(ui) {
                    let raw = ((later - base) / tempo * self.rate).round() as i64;
                    let grab = self.at_of(area, x0);
                    let snapped = self.snap_shift(area, deck, (at, to), grab, raw, false);
                    later = base + snapped as f64 / self.rate * tempo;
                    // Half a beat or more: whole beats, so the track stays
                    // on its beat (a fine fix under that stays free).
                    if let Some(bpm) = self.bpm_at(deck, at).filter(|b| *b > 0.0) {
                        let beat = 60.0 / bpm;
                        let by = later - base;
                        if by.abs() >= beat / 2.0 {
                            later = base + (by / beat).round() * beat;
                        }
                    }
                }
                let later = (later * 10_000.0).round() / 10_000.0;
                if let Some(drag) = self.move_drag.as_mut() {
                    drag.later = later;
                }
                let frames = ((later - base) / tempo * self.rate).round() as i64;
                self.preview =
                    Some(Preview { deck, at, to, frames, whole: false, until_job: u64::MAX });
                let x = self.x_of(area, at);
                painter.line_segment(
                    [pos2(x, area.top()), pos2(x, area.bottom())],
                    Stroke::new(1.0_f32, Color32::WHITE),
                );
                painter.text(
                    pos2(pos.x + 10.0, area.top() + 2.0),
                    egui::Align2::LEFT_TOP,
                    self.describe_move(deck, at, later),
                    egui::FontId::monospace(12.0),
                    Color32::WHITE,
                );
            }
            if stopped
                && let Some(drag) = self.move_drag.take()
            {
                self.set_move(drag.deck, drag.at, drag.later);
                self.chosen_move = (drag.later != 0.0).then_some((drag.deck, drag.at));
                // Drawn where it went right away; the replay refines it.
                if let Some(p) = self.preview.take()
                    && drag.later != drag.base
                {
                    self.shift_played(deck, (p.at, p.to), p.frames, (p.at, p.to));
                }
            }
        }
    }

    /// Where a clip `from`–`to`'s end handle sets the tempo: a little
    /// before its end (its last stretch), so the next clip keeps its own.
    fn tempo_end_at(from: u64, to: u64) -> u64 {
        to.saturating_sub(POINT_GAP).max(from + 1)
    }

    /// The BPM handles at each clip's start and end on `deck`'s tempo lane
    /// (clips wide enough on screen to tell them apart).
    fn tempo_handles(&self, area: Rect, deck: usize) -> Vec<TempoHandle> {
        let setting = Setting::Tempo(deck);
        let mut handles = Vec::new();
        for span in self.tracks.iter().filter(|t| t.deck == deck) {
            for (from, to) in self.visible(span) {
                let (left, right) = (self.x_of(area, from), self.x_of(area, to));
                if right < area.left() || left > area.right() || right - left < 24.0 {
                    continue;
                }
                for (at, x, end) in [(from, left, false), (Self::tempo_end_at(from, to), right, true)] {
                    let value = self.value_at(setting, at);
                    handles.push(TempoHandle {
                        from,
                        to,
                        end,
                        pos: pos2(x, self.y_of(area, setting, value)),
                        bpm: self.bpm_at(deck, at).map(|bpm| bpm * value),
                    });
                }
            }
        }
        handles
    }

    /// Puts points at `handle`'s clip edges in `setting`'s lane (as the lane
    /// stands there now): its start, ramping to its end, and just after it
    /// what follows. Returns the index of the handle's own point.
    fn pin_clip_tempo(&mut self, setting: Setting, handle: TempoHandle) -> usize {
        let (from, to) = (handle.from, handle.to);
        let end = Self::tempo_end_at(from, to);
        let lane = self.lanes.get_mut(&setting).expect("every setting has a lane");
        let now = lane.clone();
        let mut pin = |at: u64, curve: Curve| match lane.iter().position(|p| p.at == at) {
            Some(i) => lane[i].curve = curve,
            None => {
                let i = lane.partition_point(|p| p.at < at);
                lane.insert(i, Point { at, value: lane_value(&now, at), curve });
            }
        };
        pin(from, Curve::Line);
        if end > from {
            pin(end, Curve::Step);
        }
        if to < self.length {
            let after = lane_value(&now, to);
            match lane.iter().position(|p| p.at == to) {
                Some(_) => {}
                None => {
                    let i = lane.partition_point(|p| p.at < to);
                    lane.insert(i, Point { at: to, value: after, curve: Curve::Step });
                }
            }
        }
        let at = if handle.end { end } else { from };
        lane.iter().position(|p| p.at == at).unwrap_or(0)
    }

    /// Where in its track each play began, from the replay's places.
    fn find_anchors(&self) -> HashMap<(usize, u64), f64> {
        let mut anchors = HashMap::new();
        for span in &self.tracks {
            let Some(played) = self.played.get(span.deck) else { continue };
            for &(from, _) in &span.playing {
                let i = played.levels.partition_point(|l| l.0 < from);
                if played.levels.get(i).is_some_and(|l| l.0 == from)
                    && let Some(&(begins, _)) = played.places.get(i)
                {
                    anchors.insert((span.deck, from), begins);
                }
            }
        }
        anchors
    }

    /// Seconds of its track `deck` plays from `a` to `b` (frames; negative
    /// back from `b` before `a`), at its tempo lane's pace.
    fn track_secs(&self, deck: usize, a: u64, b: u64) -> f64 {
        let (low, high, sign) = if b >= a { (a, b, 1.0) } else { (b, a, -1.0) };
        let step = ((0.05 * self.rate) as u64).max(1);
        let setting = Setting::Tempo(deck);
        let (mut t, mut secs) = (low, 0.0);
        while t < high {
            let next = (t + step).min(high);
            secs += (next - t) as f64 / self.rate * self.value_at(setting, t);
            t = next;
        }
        sign * secs
    }

    /// Draws `span`'s clip `from`–`to` (moved `by` frames while dragged)
    /// from its track's own audio, as a DAW does: the outline laid along
    /// the deck's tempo from where the play began in the track, so moving,
    /// resizing and tempo changes show at once. False if it can't be (the
    /// track or where the play began unknown).
    #[allow(clippy::too_many_arguments)]
    fn clip_outline(
        &self,
        painter: &egui::Painter,
        area: Rect,
        block: Rect,
        span: &TrackSpan,
        (from, to): (u64, u64),
        by: i64,
        color: Color32,
    ) -> bool {
        let deck = span.deck;
        let Some(outline) = self.outlines.get(&span.path) else { return false };
        // The play it is part of (as it was, while its edge is dragged).
        let play = match &self.play_edge {
            Some(d) if d.deck == deck && span.playing.contains(&d.play) && d.now.0 <= from && to <= d.now.1.max(d.play.1) => {
                Some(d.play)
            }
            _ => span.playing.iter().copied().find(|p| p.0 <= from && to <= p.1.max(from + 1)),
        };
        let Some(play) = play else { return false };
        let Some(&anchor) = self.anchors.get(&(deck, play.0)) else { return false };
        if block.width() <= 0.0 || block.width().is_nan() {
            return true;
        }
        let (middle, half) = (block.center().y + 4.0, (block.height() - 18.0) / 2.0);
        let back = |t: u64| (t as i64 - by).max(0) as u64;
        // Where the deck really was (the replay's), inside the play as it
        // was replayed, unless its tempo is being changed.
        let tempo_moving = matches!(
            &self.drag,
            Some(Drag::Point { setting: Setting::Tempo(d), .. }) if *d == deck
        );
        let real_ok = !tempo_moving && self.replayed.contains(&(deck, play.0, play.1));
        let played = self.played.get(deck).filter(|p| p.places.len() == p.levels.len());
        let real = |t: u64| -> Option<f64> {
            if !real_ok || t < play.0 || t >= play.1 {
                return None;
            }
            let levels = &played?.levels;
            let j = levels.partition_point(|l| l.0 <= t).checked_sub(1)?;
            let start = levels[j].0;
            let end = levels.get(j + 1).map_or(start, |l| l.0);
            if end <= start {
                return None;
            }
            let (begins, ends) = *played?.places.get(j)?;
            Some(begins + (ends - begins) * (t - start) as f64 / (end - start) as f64)
        };
        let mut x = block.left().floor();
        let mut t = back(self.at_of(area, x));
        let mut secs = real(t).unwrap_or_else(|| anchor + self.track_secs(deck, play.0, t));
        let setting = Setting::Tempo(deck);
        let stroke = Stroke::new(1.0_f32, color.gamma_multiply(0.45));
        while x < block.right() {
            let next = back(self.at_of(area, x + 1.0));
            let pace = self.value_at(setting, t);
            let ahead = real(next)
                .unwrap_or_else(|| secs + next.saturating_sub(t) as f64 / self.rate * pace);
            if (from..to).contains(&t) {
                let h = (outline.peak(secs, ahead).min(1.0) * half).max(0.0);
                if h > 0.3 {
                    painter.line_segment([pos2(x, middle - h), pos2(x, middle + h)], stroke);
                }
            }
            (t, secs) = (next, ahead);
            x += 1.0;
        }
        true
    }

    /// The nudges on `deck`'s clip `from`–`to` (moved `by`): a bar along
    /// its foot, amber where it sped up, blue where it slowed, with how
    /// far it moved the track when the pointer is on one.
    #[allow(clippy::too_many_arguments)]
    fn nudge_marks(
        &self,
        painter: &egui::Painter,
        block: Rect,
        area: Rect,
        deck: usize,
        (from, to): (u64, u64),
        by: i64,
        hovered: Option<u64>,
    ) {
        if block.width() <= 0.0 {
            return;
        }
        let painter = painter.with_clip_rect(block);
        let moved = |t: u64| (t as i64 + by).max(0) as u64;
        for n in self.nudges.iter().filter(|n| n.deck == deck && n.from < to && from <= n.to) {
            let (a, b) = (self.x_of(area, moved(n.from)), self.x_of(area, moved(n.to)));
            let color = if n.shift >= 0.0 {
                Color32::from_rgb(255, 180, 40)
            } else {
                Color32::from_rgb(70, 170, 255)
            };
            let bar = Rect::from_x_y_ranges(a..=b.max(a + 3.0), block.bottom() - 5.0..=block.bottom() - 1.0);
            painter.rect_filled(bar, 1.0, color);
            if hovered.is_some_and(|at| self.nudge_near(area, deck, at).is_some_and(|m| m.from == n.from)) {
                painter.text(
                    pos2(a, block.bottom() - 7.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("nudge {:+.1} ms · right-click: remove", n.shift * 1000.0),
                    egui::FontId::proportional(11.0),
                    color,
                );
            }
        }
    }

    /// The nudge on `deck` at `at` (or within a few pixels of it).
    fn nudge_near(&self, area: Rect, deck: usize, at: u64) -> Option<deckhand_audio::Nudge> {
        let slack = (4.0 / f64::from(area.width()) * self.view_secs * self.rate) as u64;
        self.nudges
            .iter()
            .filter(|n| n.deck == deck && n.from <= at + slack && at <= n.to + slack)
            .min_by_key(|n| n.from.abs_diff(at))
            .copied()
    }

    /// Takes `deck`'s nudges between `from` and `to` out, keeping step (see
    /// [`Session::remove_nudges`]); ⌥ held: without.
    fn remove_nudges(&mut self, deck: usize, from: u64, to: u64, keep_step: bool) {
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        let count = session.remove_nudges(deck, from, to, keep_step);
        if count == 0 {
            self.note = Some("No nudges there".into());
            return;
        }
        self.refresh(&session);
        drop(session);
        let what = if count == 1 { "nudge".to_owned() } else { format!("{count} nudges") };
        let how = if keep_step { "" } else { " (not kept in step)" };
        self.record(format!("Deck {}: {what} removed{how}", deck + 1), Edit::Events(before));
    }

    /// The track's hot cues (lettered, in their colours) and memory cues
    /// (red) on its clip, where the deck passed them between `from` and `to`
    /// (moved `by` frames while being dragged).
    fn cue_marks(
        &self,
        painter: &egui::Painter,
        block: Rect,
        area: Rect,
        (deck, path): (usize, &Path),
        (from, to): (u64, u64),
        by: i64,
    ) {
        let (Some(cues), Some(played)) = (self.cues.get(path), self.played.get(deck)) else {
            return;
        };
        if block.width() <= 0.0 {
            return;
        }
        let painter = painter.with_clip_rect(block);
        let top = block.top() + 16.0;
        for cue in cues {
            for at in played.passes(cue.seconds) {
                if at < from || at >= to {
                    continue;
                }
                let x = self.x_of(area, (at as i64 + by).max(0) as u64);
                painter.line_segment(
                    [pos2(x, top), pos2(x, block.bottom())],
                    Stroke::new(1.0_f32, cue.color.gamma_multiply(0.8)),
                );
                match cue.hot {
                    Some(n) => {
                        let flag = Rect::from_min_size(pos2(x, top), vec2(11.0, 11.0));
                        painter.rect_filled(flag, 1.5, cue.color);
                        painter.text(
                            flag.center(),
                            egui::Align2::CENTER_CENTER,
                            char::from(b'A' + n.min(7)),
                            egui::FontId::monospace(9.0),
                            Color32::BLACK,
                        );
                    }
                    None => {
                        painter.add(Shape::convex_polygon(
                            vec![pos2(x - 4.0, top), pos2(x + 4.0, top), pos2(x, top + 5.0)],
                            cue.color,
                            Stroke::NONE,
                        ));
                    }
                }
            }
        }
    }

    /// What `deck` played inside `block`: dim as the track played, bright
    /// as it reached the mix, with its beats (bars brighter) where they
    /// fell.
    fn waveform(
        &self,
        painter: &egui::Painter,
        area: Rect,
        block: Rect,
        deck: usize,
        color: Color32,
        heard_only: bool,
    ) {
        let Some(played) = self.played.get(deck) else { return };
        // A track off the view has nothing to draw (and its box is inside out).
        if block.width().is_nan() || block.width() <= 0.0 {
            return;
        }
        let (middle, half) = (block.center().y + 4.0, (block.height() - 18.0) / 2.0);
        let levels = &played.levels;
        let preview = self.preview.filter(|p| p.deck == deck);
        let mut x = block.left().floor();
        while x < block.right() {
            let (mut from, mut to) = (self.at_of(area, x), self.at_of(area, x + 1.0));
            // A track being moved shows where it goes, and nothing where
            // it left.
            if let Some(p) = preview {
                let back = |t: u64| (t as i64 - p.frames).max(0) as u64;
                if (p.at..p.to).contains(&back(from)) {
                    (from, to) = (back(from), back(to));
                } else if from >= p.at && from < p.to {
                    x += 1.0;
                    continue;
                }
            }
            let first = levels.partition_point(|l| l.0 < from).saturating_sub(1);
            let (mut loud, mut heard) = (0f32, 0f32);
            for &(at, p, h) in levels.iter().skip(first) {
                if at > to {
                    break;
                }
                (loud, heard) = (loud.max(p), heard.max(h));
            }
            let loud = if heard_only { 0.0 } else { loud };
            for (level, shade) in [(loud, 0.35), (heard, 1.0)] {
                let h = (level.min(1.0) * half).max(if level > 0.0 { 0.5 } else { 0.0 });
                if h > 0.0 {
                    painter.line_segment(
                        [pos2(x, middle - h), pos2(x, middle + h)],
                        Stroke::new(1.0_f32, color.gamma_multiply(shade)),
                    );
                }
            }
            x += 1.0;
        }
        // Beats where there is room to tell them apart (moved along).
        let moved = |at: u64| match preview {
            Some(p) if (p.at..p.to).contains(&at) => (at as i64 + p.frames).max(0) as u64,
            _ => at,
        };
        let (from, to) = (self.at_of(area, block.left()), self.at_of(area, block.right()));
        // Only the beats near the block (sorted by time; a moved track's
        // from where they were).
        let reach = preview.map_or(0, |p| p.frames.unsigned_abs());
        let near = played.beats.partition_point(|b| b.0 + reach < from)
            ..played.beats.partition_point(|b| b.0 <= to + reach);
        let beats: Vec<(u64, bool)> = played.beats[near]
            .iter()
            .map(|&(at, bar)| (moved(at), bar))
            .filter(|&(at, _)| at >= from && at <= to)
            .collect();
        let room = block.width() / beats.len().max(1) as f32;
        // Zoomed out, bars alone; further out, neither (they would only
        // stripe the waveform).
        let bars_only = room < 7.0;
        if room * 4.0 < 7.0 {
            return;
        }
        for &(at, bar) in &beats {
            if bars_only && !bar {
                continue;
            }
            let x = self.x_of(area, at);
            if bar {
                // Downbeats: the whole height, with a notch on top.
                painter.line_segment(
                    [pos2(x, block.top() + 16.0), pos2(x, block.bottom())],
                    Stroke::new(2.0_f32, Color32::WHITE.gamma_multiply(0.9)),
                );
                painter.add(Shape::convex_polygon(
                    vec![
                        pos2(x - 3.5, block.top() + 16.0),
                        pos2(x + 3.5, block.top() + 16.0),
                        pos2(x, block.top() + 21.0),
                    ],
                    Color32::WHITE,
                    Stroke::NONE,
                ));
            } else {
                painter.line_segment(
                    [pos2(x, block.top() + 20.0), pos2(x, block.bottom() - 3.0)],
                    Stroke::new(1.0_f32, Color32::WHITE.gamma_multiply(0.55)),
                );
            }
        }
    }

    /// Moves what `deck` played in `range` by `frames` in the drawings,
    /// keeping it inside `within` (and dropping what was there), so a move
    /// shows at once instead of after the replay.
    fn shift_played(&mut self, deck: usize, range: (u64, u64), frames: i64, within: (u64, u64)) {
        let Some(played) = self.played.get_mut(deck) else { return };
        let moved = |t: u64| range.0 <= t && t < range.1;
        let inside = |t: u64| within.0 <= t && t < within.1;
        let shift = |t: u64| (t as i64 + frames).max(0) as u64;
        let place = |t: u64| {
            if moved(t) {
                Some(shift(t)).filter(|&t| inside(t))
            } else {
                Some(t).filter(|&t| !inside(t))
            }
        };
        let places = played.places.iter().copied().chain(std::iter::repeat((0.0, 0.0)));
        let mut levels: Vec<((u64, f32, f32), (f32, (f64, f64)))> = played
            .levels
            .iter()
            .zip(played.lows.iter().zip(places))
            .filter_map(|(&(at, p, h), (&low, place_in))| {
                place(at).map(|at| ((at, p, h), (low, place_in)))
            })
            .collect();
        levels.sort_by_key(|l| l.0.0);
        let had_places = !played.places.is_empty();
        let (moved_levels, rest): (Vec<_>, Vec<_>) = levels.into_iter().unzip();
        (played.lows, played.places) = rest.into_iter().unzip();
        played.levels = moved_levels;
        if !had_places {
            played.places.clear();
        }
        // What the replay is good for goes along.
        self.replayed = std::mem::take(&mut self.replayed)
            .into_iter()
            .filter_map(|(d, a, b)| {
                if d != deck {
                    return Some((d, a, b));
                }
                match (place(a), place(b.saturating_sub(1))) {
                    (Some(a), Some(b)) => Some((d, a, b + 1)),
                    _ => None,
                }
            })
            .collect();
        // Where its plays began in the track goes along.
        self.anchors = std::mem::take(&mut self.anchors)
            .into_iter()
            .filter_map(|((d, at), secs)| {
                if d != deck {
                    return Some(((d, at), secs));
                }
                place(at).map(|at| ((d, at), secs))
            })
            .collect();
        played.beats = played.beats.iter().filter_map(|&(at, bar)| place(at).map(|at| (at, bar))).collect();
        played.beats.sort_by_key(|b| b.0);
        self.clashes = deckhand_audio::clashes(&self.played, (self.rate * 0.5) as u64);
        (self.grid, self.grid_beat) = master_grid(&self.played, &self.sync, self.rate, self.length);
        self.tempo = grid_tempo(&self.grid, self.rate);
    }

    /// A clip (or its audio) is being dragged.
    fn dragging_clip(&self) -> bool {
        self.clip_drag.is_some()
            || self.move_drag.is_some()
            || self.play_edge.is_some()
            || self.fx_drag.is_some()
    }

    /// Snapping is on and ⌘ isn't held.
    fn snapping(&self, ui: &Ui) -> bool {
        self.snap && !ui.input(|i| i.modifiers.command)
    }

    /// One master beat's width on screen.
    fn beat_px(&self, area: Rect) -> f32 {
        (self.grid_beat as f64 / self.rate / self.view_secs * f64::from(area.width())) as f32
    }

    /// `delta` (frames) for dragging `deck`'s audio in `range`, grabbed at
    /// `grab`, snapped so the clip's beat nearest the grab (a bar, zoomed
    /// out) lands on the master grid's (other decks'). `absolute`: always;
    /// otherwise only within reach of the pointer.
    fn snap_shift(
        &mut self,
        area: Rect,
        deck: usize,
        range: (u64, u64),
        grab: u64,
        delta: i64,
        absolute: bool,
    ) -> i64 {
        let Some(played) = self.played.get(deck) else { return delta };
        let bars = self.beat_px(area) < 12.0;
        let anchor = played
            .beats
            .iter()
            .filter(|b| (!bars || b.1) && range.0 <= b.0 && b.0 < range.1)
            .min_by_key(|b| b.0.abs_diff(grab))
            .map(|b| b.0);
        let Some(anchor) = anchor else { return delta };
        let landing = (anchor as i64 + delta).max(0) as u64;
        let target = self
            .grid
            .iter()
            .filter(|g| g.deck != deck && (!bars || g.bar))
            .min_by_key(|g| g.at.abs_diff(landing))
            .map(|g| g.at);
        let Some(target) = target else { return delta };
        // Nothing to line up with near there.
        let reach = self.grid_beat * if bars { 8 } else { 2 };
        if target.abs_diff(landing) > reach
            || !absolute && (self.x_of(area, target) - self.x_of(area, landing)).abs() > GRAB + 1.0
        {
            return delta;
        }
        self.snap_line = Some(target);
        target as i64 - anchor as i64
    }

    /// "+1.25 s · +2 bars" for a clip moved `frames`.
    fn describe_shift(&self, frames: i64) -> String {
        let secs = format!("{:+.2} s", frames as f64 / self.rate);
        if self.grid_beat == 0 {
            return secs;
        }
        let beats = frames as f64 / self.grid_beat as f64;
        if (beats / 4.0 - (beats / 4.0).round()).abs() < 0.01 {
            format!("{secs} · {:+} bars", (beats / 4.0).round() as i64)
        } else {
            format!("{secs} · {beats:+.2} beats")
        }
    }

    /// Bars the master grid shows a line for: every `n`th, so they stay
    /// apart.
    fn bar_step(&self, area: Rect, least: f32) -> u32 {
        let bar_px = self.beat_px(area) * 4.0;
        [1, 2, 4, 8, 16, 32, 64, 128].into_iter().find(|n| *n as f32 * bar_px >= least).unwrap_or(256)
    }

    /// The master grid on the ruler: bar ticks and numbers, in the colour
    /// of the deck it follows there.
    fn grid_ruler(&self, ui: &Ui, painter: &egui::Painter, area: Rect) {
        if self.grid.is_empty() {
            return;
        }
        let strip = Rect::from_x_y_ranges(area.x_range(), area.top() + TIME_STRIP..=area.bottom());
        painter.rect_filled(strip, 0.0, ui.visuals().extreme_bg_color);
        let (ticks, labels) = (self.bar_step(area, 6.0), self.bar_step(area, 34.0));
        let beats = self.beat_px(area) >= 10.0;
        let (from, to) = (self.at_of(area, area.left()), self.at_of(area, area.right()));
        let first = self.grid.partition_point(|g| g.at < from);
        for g in self.grid[first..].iter().take_while(|g| g.at <= to) {
            let x = self.x_of(area, g.at);
            let color = DECK_COLORS[g.deck % DECK_COLORS.len()];
            let bar = g.bar && g.number > 0;
            if bar && (g.number - 1) % labels == 0 {
                painter.line_segment(
                    [pos2(x, strip.top()), pos2(x, strip.bottom())],
                    Stroke::new(1.5_f32, color),
                );
                painter.text(
                    pos2(x + 3.0, strip.top() + 1.0),
                    egui::Align2::LEFT_TOP,
                    g.number.to_string(),
                    egui::FontId::monospace(10.0),
                    ui.visuals().strong_text_color(),
                );
            } else if bar && (g.number - 1) % ticks == 0 {
                painter.line_segment(
                    [pos2(x, strip.center().y), pos2(x, strip.bottom())],
                    Stroke::new(1.0_f32, color),
                );
            } else if beats && !g.bar {
                painter.line_segment(
                    [pos2(x, strip.bottom() - 4.0), pos2(x, strip.bottom())],
                    Stroke::new(1.0_f32, color.gamma_multiply(0.6)),
                );
            }
        }
    }

    /// The line a drag snapped to, down all the rows.
    fn snap_guide(&self, ui: &Ui, rect: Rect) {
        if let Some(at) = self.snap_line {
            let x = self.x_of(rect, at);
            ui.painter_at(rect).line_segment(
                [pos2(x, rect.top()), pos2(x, rect.bottom())],
                Stroke::new(1.5_f32, SNAP),
            );
        }
    }

    /// The Master group: the set's tempo with its beat grid on it, and how
    /// loud the mix is.
    fn master_rows(&mut self, ui: &mut Ui, width: f32) {
        let (row, response) = ui.allocate_exact_size(vec2(width, DECK_ROW), Sense::click());
        let folded = self.fold(ui, row, &response, MASTER);
        self.group_name(ui, row, "Master", ui.visuals().strong_text_color(), folded);
        if folded {
            return;
        }
        let (row, _) = ui.allocate_exact_size(vec2(width, LANE_ROW), Sense::hover());
        self.tempo_lane(ui, row);
        let (row, _) = ui.allocate_exact_size(vec2(width, LANE_ROW * 0.75), Sense::hover());
        self.loudness_lane(ui, row);
    }

    /// A Master lane's name and the value under the pointer; its area.
    fn master_lane(&self, ui: &Ui, row: Rect, name: &str, value: Option<String>) -> Rect {
        let visuals = ui.visuals();
        ui.painter().text(
            pos2(row.left() + 4.0, row.top() + 4.0),
            egui::Align2::LEFT_TOP,
            name,
            egui::FontId::proportional(13.0),
            visuals.strong_text_color(),
        );
        if let Some(value) = value {
            ui.painter().text(
                pos2(row.left() + 4.0, row.top() + 22.0),
                egui::Align2::LEFT_TOP,
                value,
                egui::FontId::monospace(11.0),
                visuals.text_color(),
            );
        }
        let area =
            Rect::from_min_max(pos2(row.left() + LABELS, row.top() + 1.0), row.max - vec2(0.0, 1.0));
        ui.painter_at(area).rect_filled(area, 2.0, visuals.extreme_bg_color);
        area
    }

    /// The master tempo over the set, and the beat grid it comes from:
    /// bars in the colour of the deck leading there, beats zoomed in.
    fn tempo_lane(&self, ui: &Ui, row: Rect) {
        let bpm_at = |at: u64| {
            let i = self.tempo.partition_point(|t| t.0 <= at);
            self.tempo.get(i.saturating_sub(1)).map(|t| t.1)
        };
        let value = self.hover.or(self.playhead()).and_then(bpm_at).map(|bpm| format!("{bpm:.2} BPM"));
        let area = self.master_lane(ui, row, "Tempo", value);
        let painter = ui.painter_at(area);
        if self.grid.is_empty() {
            painter.text(
                area.left_center() + vec2(8.0, 0.0),
                egui::Align2::LEFT_CENTER,
                "No beat grids played yet",
                egui::FontId::proportional(11.0),
                ui.visuals().weak_text_color(),
            );
            return;
        }
        let (from, to) = (self.at_of(area, area.left()), self.at_of(area, area.right()));
        // The beat grid.
        let every = self.bar_step(area, 10.0);
        let labels = self.bar_step(area, 34.0);
        let beats = self.beat_px(area) >= 8.0;
        let first = self.grid.partition_point(|g| g.at < from);
        for g in self.grid[first..].iter().take_while(|g| g.at <= to) {
            let x = self.x_of(area, g.at);
            let color = DECK_COLORS[g.deck % DECK_COLORS.len()];
            if g.bar && g.number > 0 && (g.number - 1) % every == 0 {
                painter.line_segment(
                    [pos2(x, area.top()), pos2(x, area.bottom())],
                    Stroke::new(1.5_f32, color.gamma_multiply(0.75)),
                );
                if (g.number - 1) % labels == 0 {
                    painter.text(
                        pos2(x + 3.0, area.bottom() - 2.0),
                        egui::Align2::LEFT_BOTTOM,
                        g.number.to_string(),
                        egui::FontId::monospace(9.0),
                        color,
                    );
                }
            } else if beats && !g.bar {
                painter.line_segment(
                    [pos2(x, area.top() + area.height() * 0.3), pos2(x, area.bottom())],
                    Stroke::new(1.0_f32, color.gamma_multiply(0.35)),
                );
            }
        }
        // The tempo line, over a range a little wider than the set's.
        let (low, high) = self
            .tempo
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), t| (l.min(t.1), h.max(t.1)));
        let (low, high) = if high - low < 3.0 {
            let mid = (high + low) / 2.0;
            (mid - 1.5, mid + 1.5)
        } else {
            (low - 0.5, high + 0.5)
        };
        let y = |bpm: f64| {
            area.bottom() - 4.0 - ((bpm - low) / (high - low)) as f32 * (area.height() - 8.0)
        };
        let start = self.tempo.partition_point(|t| t.0 < from).saturating_sub(1);
        let mut line: Vec<Pos2> = Vec::new();
        for &(at, bpm) in self.tempo[start..].iter() {
            let p = pos2(self.x_of(area, at), y(bpm));
            // A step: the tempo holds until the next beat.
            if let Some(last) = line.last().copied() {
                if p.x - last.x < 1.0 && (p.y - last.y).abs() < 1.0 {
                    continue;
                }
                line.push(pos2(p.x, last.y));
            }
            line.push(p);
            if at > to {
                break;
            }
        }
        painter.add(Shape::line(line, Stroke::new(2.0_f32, ui.visuals().strong_text_color())));
        for bpm in [low + 0.5, high - 0.5] {
            painter.text(
                pos2(area.right() - 4.0, y(bpm)),
                egui::Align2::RIGHT_CENTER,
                format!("{bpm:.1}"),
                egui::FontId::monospace(9.0),
                ui.visuals().weak_text_color(),
            );
        }
        if let Some(at) = self.hover {
            let x = self.x_of(area, at);
            painter.line_segment(
                [pos2(x, area.top()), pos2(x, area.bottom())],
                Stroke::new(1.0_f32, ui.visuals().weak_text_color().gamma_multiply(0.5)),
            );
        }
    }

    /// How loud the mix is: RMS filled, peaks as a thin line over it, red
    /// where it reached full scale; −48 dB to 0 dB.
    fn loudness_lane(&self, ui: &Ui, row: Rect) {
        let level_at = |at: u64| {
            let i = self.mix.partition_point(|m| m.0 <= at);
            self.mix.get(i.saturating_sub(1)).copied()
        };
        let db = |v: f32| 20.0 * v.max(1e-6).log10();
        let value = self.hover.or(self.playhead()).and_then(level_at).map(|(_, peak, rms)| {
            format!("{:.1} dB RMS · {:.1} peak", db(rms), db(peak))
        });
        let name = if self.heard_stale { "Loudness (out of date)" } else { "Loudness" };
        let area = self.master_lane(ui, row, name, value);
        let painter = ui.painter_at(area);
        let y = |v: f32| {
            let shown = ((db(v) + 48.0) / 48.0).clamp(0.0, 1.0);
            area.bottom() - shown * (area.height() - 2.0)
        };
        for level in [-6.0f32, -12.0, -24.0] {
            let y = y(10f32.powf(level / 20.0));
            painter.line_segment(
                [pos2(area.left(), y), pos2(area.right(), y)],
                Stroke::new(1.0_f32, ui.visuals().faint_bg_color),
            );
            painter.text(
                pos2(area.right() - 4.0, y),
                egui::Align2::RIGHT_BOTTOM,
                format!("{level:.0}"),
                egui::FontId::monospace(9.0),
                ui.visuals().weak_text_color(),
            );
        }
        let color = ui.visuals().strong_text_color();
        let mut x = area.left().floor();
        while x < area.right() {
            let (from, to) = (self.at_of(area, x), self.at_of(area, x + 1.0));
            let first = self.mix.partition_point(|m| m.0 < from).saturating_sub(1);
            let (mut peak, mut rms) = (0f32, 0f32);
            for &(at, p, r) in self.mix.iter().skip(first) {
                if at > to {
                    break;
                }
                (peak, rms) = (peak.max(p), rms.max(r));
            }
            if peak > 0.0 {
                let clip = peak >= 0.999;
                painter.line_segment(
                    [pos2(x, area.bottom()), pos2(x, y(rms))],
                    Stroke::new(1.0_f32, if clip { CLASH } else { color.gamma_multiply(0.45) }),
                );
                painter.line_segment(
                    [pos2(x, y(rms)), pos2(x, y(peak))],
                    Stroke::new(1.0_f32, if clip { CLASH } else { color.gamma_multiply(0.15) }),
                );
            }
            x += 1.0;
        }
    }

    /// The pieces heard of `span`: as being dragged, else as the session
    /// has them.
    fn span_pieces(&self, span: &TrackSpan) -> Vec<(u64, u64)> {
        match &self.edge_drag {
            Some(drag) if drag.span == *span => drag.pieces.clone(),
            _ => self
                .pieces
                .get(&(span.deck, span.from))
                .cloned()
                .unwrap_or_else(|| vec![(span.from, span.to)]),
        }
    }

    /// The edge under `x` on `deck`'s row of a part shown: a play's start
    /// or end, else a piece's (a cut).
    fn edge_at(&self, area: Rect, deck: usize, x: f32) -> Option<EdgeHit> {
        // Every edge in reach; where two clips touch, the one the pointer
        // is over wins, then the nearest.
        let mut near: Vec<(bool, f32, &TrackSpan, u64, bool)> = Vec::new();
        for span in self.tracks.iter().filter(|t| t.deck == deck) {
            for (from, to) in self.visible(span) {
                for (edge, right) in [(from, false), (to, true)] {
                    let ex = self.x_of(area, edge);
                    let distance = (ex - x).abs();
                    if distance <= GRAB {
                        let inside = if right { x <= ex } else { x >= ex };
                        near.push((!inside, distance, span, edge, right));
                    }
                }
            }
        }
        near.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        let &(_, _, span, edge, right) = near.first()?;
        // An end: the last part of its play heard (what's cut after it
        // gives way as the play goes on).
        let shown = self.visible(span);
        let play = span.playing.iter().find(|p| {
            if right {
                p.0 < edge && edge <= p.1 && !shown.iter().any(|v| edge <= v.0 && v.1 <= p.1)
            } else {
                p.0 == edge
            }
        });
        if let Some(&play) = play {
            return Some(EdgeHit::Play(play, right));
        }
        let pieces = self.span_pieces(span);
        let piece = pieces.iter().position(|p| if right { p.1 == edge } else { p.0 == edge })?;
        Some(EdgeHit::Piece(span.clone(), piece, right))
    }

    /// The parts of `span` shown as clips: where it played (as its edges are
    /// being dragged) and was not cut out. A track loaded but never played
    /// makes no sound and isn't shown.
    fn visible(&self, span: &TrackSpan) -> Vec<(u64, u64)> {
        let pieces = self.span_pieces(span);
        let mut shown = Vec::new();
        for &play in &span.playing {
            let (a, b) = match &self.play_edge {
                // Its end dragged: as heard, to where it ends now.
                Some(d) if d.deck == span.deck && d.play == play && d.right => {
                    for &(c, e) in &pieces {
                        let (from, to) = (play.0.max(c), play.1.min(e).min(d.now.1));
                        if from < to {
                            shown.push((from, to));
                        }
                    }
                    if d.now.1 > d.heard.1 {
                        match shown.iter_mut().find(|s| s.1 == d.heard.1) {
                            Some(last) => last.1 = d.now.1,
                            None => shown.push((d.heard.1, d.now.1)),
                        }
                    }
                    continue;
                }
                Some(d) if d.deck == span.deck && d.play == play => (d.now.0, play.1),
                _ => play,
            };
            for &(c, d) in &pieces {
                let (from, to) = (a.max(c), b.min(d));
                if from < to {
                    shown.push((from, to));
                }
            }
        }
        shown.sort_unstable();
        shown
    }

    /// Makes the plays of the clip on `deck` at `at`, when it has several,
    /// clips of their own (an undo step of its own; sounds the same), so
    /// one can be moved or resized alone.
    fn separate(&mut self, deck: usize, at: u64) {
        let Some(span) = self
            .tracks
            .iter()
            .find(|t| t.deck == deck && t.from <= at && at < t.to && t.playing.len() > 1)
            .cloned()
        else {
            return;
        };
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        match session.split_plays(deck, span.from) {
            Ok(true) => {
                self.refresh(&session);
                drop(session);
                self.record(
                    format!("Deck {}: plays made clips of their own", deck + 1),
                    Edit::Events(before),
                );
            }
            Ok(false) => {}
            Err(err) => self.note = Some(format!("Couldn't separate the plays: {err:#}")),
        }
    }

    /// The clip shown on `deck` at `at`.
    fn clip_at(&self, deck: usize, at: u64) -> Option<&TrackSpan> {
        self.tracks
            .iter()
            .filter(|t| t.deck == deck && t.from <= at && at < t.to)
            .find(|t| self.visible(t).iter().any(|&(from, to)| from <= at && at < to))
    }

    /// The colour of `path`'s clips: by the order tracks come in the set.
    fn clip_color(&self, path: &Path) -> Color32 {
        let mut seen: Vec<&Path> = Vec::new();
        let mut spans: Vec<&TrackSpan> = self.tracks.iter().collect();
        spans.sort_by_key(|t| t.from);
        for t in spans {
            if !seen.contains(&t.path.as_path()) {
                seen.push(&t.path);
            }
        }
        let i = seen.iter().position(|p| *p == path).unwrap_or(0);
        CLIP_COLORS[i % CLIP_COLORS.len()]
    }

    /// Makes a play start or end where its edge was dragged to, keeping
    /// the old for undo.
    fn resize_play(&mut self, drag: &PlayEdgeDrag) {
        if drag.now == drag.heard {
            return;
        }
        let deck = drag.deck;
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        match session.resize_play(deck, drag.play, drag.now) {
            Ok((new_from, _)) => {
                // Where it begins in the track now, until the replay says.
                if new_from != drag.play.0
                    && let Some(&anchor) = self.anchors.get(&(deck, drag.play.0))
                {
                    let moved = anchor + self.track_secs(deck, drag.play.0, new_from);
                    self.anchors.insert((deck, new_from), moved.max(0.0));
                }
                self.refresh(&session);
                drop(session);
                let longer = drag.now.1 - drag.now.0 > drag.heard.1 - drag.heard.0;
                let what = if longer { "longer" } else { "shorter" };
                self.record(format!("Deck {} play {what}", deck + 1), Edit::Events(before));
            }
            Err(err) => self.note = Some(format!("Not changed: {err:#}")),
        }
    }

    /// Lets only `pieces` of `span` be heard, keeping the old for undo.
    fn set_pieces(&mut self, span: &TrackSpan, pieces: Vec<(u64, u64)>, what: &str) {
        if pieces == self.span_pieces(span) {
            return;
        }
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        session.set_pieces(span, &pieces);
        self.refresh(&session);
        drop(session);
        self.record(format!("Deck {} {what}", span.deck + 1), Edit::Events(before));
    }

    /// Splits `deck`'s clip at `at` into two clips of their own, as in a
    /// DAW (each selected, moved and deleted apart).
    fn split(&mut self, deck: usize, at: u64) {
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        match session.split_clip(deck, at) {
            Ok(at) => {
                // Where its second half begins in the track, until the
                // replay says.
                let begun = self
                    .tracks
                    .iter()
                    .find(|t| t.deck == deck && t.from < at && at < t.to)
                    .and_then(|t| t.playing.iter().find(|p| p.0 < at && at <= p.1))
                    .and_then(|p| self.anchors.get(&(deck, p.0)).map(|&a| (p.0, a)));
                if let Some((play, anchor)) = begun {
                    let there = anchor + self.track_secs(deck, play, at);
                    self.anchors.insert((deck, at), there);
                }
                self.refresh(&session);
                drop(session);
                self.record(format!("Deck {} split", deck + 1), Edit::Events(before));
            }
            Err(err) => self.note = Some(format!("Not split: {err:#}")),
        }
    }

    /// Brings back the gap cut in `deck`'s clip at `at`, if a play is in
    /// it. (Where the deck is stopped, nothing is cut: a piece there can
    /// run on into what is heard.)
    fn bring_back(&mut self, deck: usize, at: u64) {
        let Some(span) =
            self.tracks.iter().find(|t| t.deck == deck && t.from <= at && at < t.to).cloned()
        else {
            return;
        };
        let mut pieces = self.span_pieces(&span);
        if pieces.iter().any(|&(from, to)| from <= at && at < to) {
            return;
        }
        let i = pieces.partition_point(|p| p.1 <= at);
        let from = i.checked_sub(1).map_or(span.from, |i| pieces[i].1);
        let to = pieces.get(i).map_or(span.to, |p| p.0);
        if !span.playing.iter().any(|p| p.0 < to && from < p.1) {
            return;
        }
        pieces.insert(i, (from, to));
        self.set_pieces(&span, pieces, "bring back");
    }

    /// Moves the clip of `deck`'s track loaded at `from` by `frames`, with
    /// everything done on the deck while it was loaded.
    fn move_clip(&mut self, deck: usize, from: u64, frames: i64) {
        if frames == 0 {
            self.preview = None;
            return;
        }
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        match session.move_clip(deck, from, frames) {
            Ok(landed) => {
                self.refresh(&session);
                drop(session);
                let secs = (landed as f64 - from as f64) / self.rate;
                self.record(format!("Deck {} clip {secs:+.2} s", deck + 1), Edit::Events(before));
                // Drawn where it landed right away; the replay refines it.
                if let Some(p) = self.preview.take() {
                    let by = landed as i64 - from as i64;
                    let to = p.to;
                    let landing = (from as i64 + by).max(0) as u64;
                    self.shift_played(deck, (from, to), by, (landing, landing + (to - from)));
                }
            }
            Err(err) => {
                self.note = Some(format!("Not moved: {err:#}"));
                self.preview = None;
            }
        }
    }

    /// "+12.0 ms · +0.024 beats" for a move of `deck`'s track at `at`.
    fn describe_move(&self, deck: usize, at: u64, later: f64) -> String {
        let ms = format!("{:+.1} ms", later * 1000.0);
        match self.bpm_at(deck, at) {
            Some(bpm) => format!("{ms} · {:+.3} beats", later * bpm / 60.0),
            None => ms,
        }
    }

    /// Moves `deck`'s track `later` seconds from `at` (0 takes the move
    /// away), keeping the old one for undo.
    fn set_move(&mut self, deck: usize, at: u64, later: f64) {
        let before =
            self.moves.iter().find(|m| m.deck == deck && m.at == at).map_or(0.0, |m| m.later);
        if before == later {
            return;
        }
        let Ok(mut session) = self.session.lock() else { return };
        session.move_track(deck, at, later);
        self.moves = session.moves();
        drop(session);
        let what = if later == 0.0 {
            "back in place".to_owned()
        } else {
            self.describe_move(deck, at, later)
        };
        self.record(
            format!("Deck {} track {what}", deck + 1),
            Edit::Move { deck, at, later: before },
        );
    }

    /// The BPM of the track on `deck` at `at`, at its own tempo.
    fn bpm_at(&self, deck: usize, at: u64) -> Option<f64> {
        let on_deck = || self.tracks.iter().filter(|t| t.deck == deck);
        // The track there; between tracks, the next one loaded (the tempo
        // set there is what it starts at), else the last.
        let span = on_deck()
            .find(|t| t.from <= at && at < t.to)
            .or_else(|| on_deck().filter(|t| t.from > at).min_by_key(|t| t.from))
            .or_else(|| on_deck().filter(|t| t.to <= at).max_by_key(|t| t.to))?;
        self.bpms.get(&span.path).copied()
    }

    /// The Beat FX: a block for each stretch it is on, with its type, what
    /// it is on and its depth. Click one to change it, drag it or its ends
    /// to move it, right-click to take it away.
    fn fx_row(&mut self, ui: &Ui, row: Rect, response: &egui::Response) {
        let visuals = ui.visuals().clone();
        ui.painter().text(
            pos2(row.left() + 4.0, row.top() + 4.0),
            egui::Align2::LEFT_TOP,
            "Beat FX",
            egui::FontId::proportional(13.0),
            visuals.strong_text_color(),
        );
        let area = Rect::from_min_max(
            pos2(row.left() + LABELS, row.top() + 1.0),
            row.max - vec2(0.0, 1.0),
        );
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 2.0, visuals.extreme_bg_color);
        let spans = fx_spans(&self.fx, self.length);
        // The depth lane, as it is while a point is dragged.
        let lane = match &self.depth_drag {
            Some(drag) => drag.points.clone(),
            None => self.lanes[&Setting::BeatFxDepth].clone(),
        };
        let fx_color = |target: FxTarget| match target {
            FxTarget::Channel(d) => DECK_COLORS[d % DECK_COLORS.len()],
            _ => visuals.strong_text_color(),
        };
        let y_in = |block: Rect, depth: f64| {
            block.bottom() - 2.0 - (depth.clamp(0.0, 1.0) as f32) * (block.height() - 16.0)
        };
        let mut blocks = Vec::new();
        // Depth handles: an effect's edges and the points between.
        let mut dots: Vec<Handle> = Vec::new();
        for (i, span) in spans.iter().enumerate() {
            let (from, to) = match &self.fx_drag {
                Some(drag) if drag.index == i => (drag.from, drag.to),
                _ => (span.from, span.to),
            };
            let block = Rect::from_x_y_ranges(
                self.x_of(area, from)..=self.x_of(area, to).max(self.x_of(area, from) + 2.0),
                area.top() + 2.0..=area.bottom() - 2.0,
            );
            blocks.push(block);
            let color = fx_color(span.target);
            painter.rect_filled(block, 3.0, color.gamma_multiply(0.3));
            if self.chosen_fx == Some(i) {
                painter.rect_stroke(
                    block,
                    3.0,
                    Stroke::new(1.5_f32, Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
            }
            // The depth along it, curves and all.
            let shown = block.intersect(area);
            if shown.width() > 0.0 && self.fx_drag.as_ref().is_none_or(|d| d.index != i) {
                let line: Vec<Pos2> = (0..=shown.width() as usize)
                    .map(|k| {
                        let x = shown.left() + k as f32;
                        pos2(x, y_in(block, lane_value(&lane, self.at_of(area, x))))
                    })
                    .collect();
                painter.add(Shape::line(line, Stroke::new(1.5_f32, color)));
                // A handle at each edge (a point there, or where one would
                // go), and the points between.
                let end = self.clip_end(span);
                for (at, edge) in [(span.from, true), (end, true)] {
                    let index = lane.iter().position(|p| p.at == at);
                    let value = index.map_or_else(|| lane_value(&lane, at), |k| lane[k].value);
                    let dot = pos2(self.x_of(area, at), y_in(block, value));
                    painter.circle_filled(dot, 3.5, color);
                    painter.circle_stroke(dot, 3.5, Stroke::new(1.0_f32, Color32::WHITE));
                    dots.push(Handle { pos: dot, index, block: i, at, edge });
                }
                for (k, point) in lane.iter().enumerate() {
                    if point.at > span.from && point.at < end {
                        let dot = pos2(self.x_of(area, point.at), y_in(block, point.value));
                        painter.circle_filled(dot, 2.5, color);
                        dots.push(Handle {
                            pos: dot,
                            index: Some(k),
                            block: i,
                            at: point.at,
                            edge: false,
                        });
                    }
                }
            }
            let label = self.fx_name(span);
            let text = ui.painter().layout_no_wrap(
                label,
                egui::FontId::proportional(11.0),
                visuals.strong_text_color(),
            );
            let clip = block.intersect(area).shrink2(vec2(3.0, 0.0));
            ui.painter().with_clip_rect(clip).galley(
                pos2(clip.left(), block.top() + 1.0),
                text,
                Color32::WHITE,
            );
        }
        if let Some((from, to)) = self.fx_selection {
            let selected =
                Rect::from_x_y_ranges(self.x_of(area, from)..=self.x_of(area, to), area.y_range());
            painter.rect_filled(selected, 0.0, visuals.selection.bg_fill.gamma_multiply(0.45));
        }
        self.play_marks(&painter, area);
        // The depth at the cursor inside an effect (a point's own when on
        // one), with the effect.
        if let Some(pos) = response.hover_pos()
            && self.depth_drag.is_none()
            && self.fx_drag.is_none()
            && let Some(i) = blocks.iter().position(|b| b.contains(pos))
        {
            let on_dot = dots
                .iter()
                .filter(|d| d.pos.distance(pos) <= GRAB + 2.0)
                .min_by(|a, b| a.pos.distance(pos).total_cmp(&b.pos.distance(pos)));
            let depth = match on_dot {
                Some(handle) => lane_value(&lane, handle.at),
                None => lane_value(&lane, self.at_of(area, pos.x)),
            };
            let span = &spans[i];
            let text = format!(
                "{} · {} · depth {:.0}%",
                span.kind.label(),
                target_label(span.target),
                depth * 100.0
            );
            value_tag(&painter, area, pos, &text);
        }
        let pointer = response.interact_pointer_pos().or(response.hover_pos());
        let under = |pos: Pos2| blocks.iter().position(|b| b.expand2(vec2(4.0, 0.0)).contains(pos));
        // An edge's depth dot only right on it: the rest of the edge
        // stretches the effect.
        let near = |pos: Pos2| {
            dots.iter()
                .filter(|d| d.pos.distance(pos) <= if d.edge { 5.0 } else { GRAB + 2.0 })
                .min_by(|a, b| a.pos.distance(pos).total_cmp(&b.pos.distance(pos)))
                .copied()
        };
        // An effect's edge under the pointer (not on its depth dot).
        let edge_at = |pos: Pos2| {
            if near(pos).is_some() {
                return None;
            }
            blocks.iter().enumerate().find_map(|(i, b)| {
                if !b.expand2(vec2(GRAB, 0.0)).contains(pos) {
                    None
                } else if (pos.x - b.left()).abs() <= GRAB {
                    Some((i, false))
                } else if (pos.x - b.right()).abs() <= GRAB {
                    Some((i, true))
                } else {
                    None
                }
            })
        };
        let depth_of = |block: Rect, y: f32| {
            f64::from(((block.bottom() - 2.0 - y) / (block.height() - 16.0)).clamp(0.0, 1.0))
        };
        if self.busy() || self.panning {
            return;
        }
        // Select: drag a stretch (Delete takes the effects off through it).
        if self.tool == Tool::Select {
            let pressed = ui.input(|i| i.pointer.press_origin()).or(pointer);
            if response.drag_started()
                && let Some(pos) = pressed
            {
                let at = self.at_of(area, pos.x);
                self.select_only(Selected::Fx);
                self.fx_selection = Some((at, at));
                self.range_from = at;
            }
            if response.dragged()
                && let Some(pos) = pointer
                && self.fx_selection.is_some()
            {
                let (a, b) = (self.range_from, self.at_of(area, pos.x));
                self.fx_selection = Some((a.min(b), a.max(b)));
            }
            if response.drag_stopped() && self.fx_selection.is_some_and(|(a, b)| a >= b) {
                self.fx_selection = None;
            }
            if response.clicked() {
                self.fx_selection = None;
                self.chosen_fx = pointer.and_then(under);
            }
            // Right click on the selection: another beat or type for just
            // that part (a block of its own), or off.
            if let Some((from, to)) = self.fx_selection {
                response.context_menu(|ui| {
                    ui.label(RichText::new("The selected part").small().weak());
                    let beat = self.beat_len(from).unwrap_or(1.0);
                    ui.menu_button("Beat", |ui| {
                        for (multiple, label) in BEATS {
                            if ui.button(label).clicked() {
                                let secs = (beat * multiple) as f32;
                                self.change_fx_part(
                                    from,
                                    to,
                                    |p| p.beat_secs = secs,
                                    &format!("beat {label}"),
                                );
                                ui.close();
                            }
                        }
                    });
                    ui.menu_button("Type", |ui| {
                        for kind in BeatFxKind::ALL {
                            if ui.button(kind.label()).clicked() {
                                self.change_fx_part(from, to, |p| p.kind = kind, kind.label());
                                ui.close();
                            }
                        }
                    });
                    if ui.button("Remove (Delete)").clicked() {
                        self.delete_selection();
                        ui.close();
                    }
                });
            }
            return;
        }
        if let Some(pos) = response.hover_pos()
            && let Some(handle) = near(pos)
        {
            painter.circle_stroke(handle.pos, 6.0, Stroke::new(1.5_f32, Color32::WHITE));
        }
        // An edge to stretch: a grip on it.
        let stretching = self.fx_drag.as_ref().is_some_and(|d| !matches!(d.grip, FxGrip::Whole));
        let hovered_edge = response.hover_pos().and_then(edge_at);
        if let Some((i, right)) = hovered_edge.filter(|_| self.depth_drag.is_none()) {
            let b = blocks[i];
            let x = if right { b.right() - 2.0 } else { b.left() + 2.0 };
            painter.line_segment(
                [pos2(x, b.top() + 3.0), pos2(x, b.bottom() - 3.0)],
                Stroke::new(3.0_f32, Color32::WHITE.gamma_multiply(0.8)),
            );
        }
        if hovered_edge.is_some() && self.depth_drag.is_none() || stretching {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if response.clicked() || response.secondary_clicked() {
            // One thing at a time: Delete takes the chosen effect away.
            self.clear_selections();
            self.chosen_fx = pointer.and_then(under);
        }
        // Double-click inside an effect: a depth point there (in the shape
        // chosen in the toolbar).
        // Double-click on a depth handle: type its depth.
        if response.double_clicked()
            && let Some(pos) = pointer
            && let Some(handle) = near(pos)
        {
            self.open_point_edit(Setting::BeatFxDepth, handle.at, pos);
        }
        if response.double_clicked()
            && let Some(pos) = pointer
            && near(pos).is_none()
            && let Some(i) = under(pos)
        {
            let at = self.session.lock().map_or(0, |s| s.snap(self.at_of(area, pos.x)));
            let span = &spans[i];
            if at > span.from && at < span.to {
                let setting = Setting::BeatFxDepth;
                let before = self.lanes[&setting].clone();
                let point = Point { at, value: depth_of(blocks[i], pos.y), curve: self.shape };
                let lane = self.lanes.get_mut(&setting).expect("every setting has a lane");
                lane.retain(|p| p.at != at);
                let index = lane.partition_point(|p| p.at < at);
                lane.insert(index, point);
                self.commit(setting, before, "add depth point");
            }
        }
        // Right- or double-click off the effects: the Add Beat FX dialog.
        if (response.secondary_clicked() || response.double_clicked())
            && let Some(pos) = pointer.filter(|p| under(*p).is_none())
        {
            self.adding_fx = Some(AddFx { at: self.at_of(area, pos.x), pos, ..self.last_fx });
        }
        // Right-click on a depth point: gone (the lane's first stays).
        if response.secondary_clicked()
            && let Some(pos) = pointer
            && let Some(index) = near(pos).filter(|d| !d.edge).and_then(|d| d.index)
        {
            let setting = Setting::BeatFxDepth;
            let before = self.lanes[&setting].clone();
            remove_point(self.lanes.get_mut(&setting).expect("every setting has a lane"), index);
            self.commit(setting, before, "delete depth point");
            return;
        }
        // Dragging a depth point: up and down, and along between its
        // neighbours.
        if response.drag_started()
            && let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(pointer)
            && let Some(handle) = near(pos)
        {
            let mut points = self.lanes[&Setting::BeatFxDepth].clone();
            // An edge without a point yet gets one now.
            let index = handle.index.unwrap_or_else(|| {
                let value = lane_value(&points, handle.at);
                let curve =
                    if handle.at == spans[handle.block].from { Curve::Line } else { Curve::Step };
                let k = points.partition_point(|p| p.at < handle.at);
                points.insert(k, Point { at: handle.at, value, curve });
                k
            });
            self.clear_selections();
            let span = &spans[handle.block];
            let limits = (span.from, self.clip_end(span));
            self.depth_drag = Some(DepthDrag {
                index,
                block: blocks[handle.block],
                points,
                edge: handle.edge,
                limits,
            });
            self.chosen_fx = Some(handle.block);
        }
        if self.depth_drag.is_some() {
            if let Some(pos) = pointer {
                let at = self.at_of(area, pos.x);
                if let Some(drag) = self.depth_drag.as_mut() {
                    let i = drag.index;
                    // Apart from its neighbours, and inside its effect.
                    let low = if i == 0 { 0 } else { drag.points[i - 1].at + POINT_GAP };
                    let low = low.max(drag.limits.0 + POINT_GAP);
                    let high = drag
                        .points
                        .get(i + 1)
                        .map_or(u64::MAX, |p| p.at.saturating_sub(POINT_GAP))
                        .min(drag.limits.1.saturating_sub(POINT_GAP));
                    let value = depth_of(drag.block, pos.y);
                    let edge = drag.edge;
                    let point = &mut drag.points[i];
                    point.value = value;
                    // Edges go up and down only; points between, along too.
                    if i > 0 && !edge {
                        point.at = at.clamp(low, high.max(low));
                    }
                    painter.text(
                        pos + vec2(10.0, -4.0),
                        egui::Align2::LEFT_BOTTOM,
                        format!("{:.0}%", value * 100.0),
                        egui::FontId::monospace(11.0),
                        Color32::WHITE,
                    );
                }
            }
            if response.drag_stopped()
                && let Some(drag) = self.depth_drag.take()
            {
                let setting = Setting::BeatFxDepth;
                let before = std::mem::replace(
                    self.lanes.get_mut(&setting).expect("every setting has a lane"),
                    drag.points,
                );
                self.commit(setting, before, "move depth point");
            }
            return;
        }
        // Right click on an effect: its type, what it is on, its depth.
        let chosen = self.chosen_fx.and_then(|i| spans.get(i)).cloned();
        if let Some(span) = &chosen {
            response.context_menu(|ui| self.fx_menu(ui, span));
        }
        // Where the button went down, not where the drag began.
        let pressed = ui.input(|i| i.pointer.press_origin()).or(pointer);
        if response.drag_started()
            && let Some(pos) = pressed
            && let Some(i) = under(pos)
        {
            let grip = match edge_at(pos) {
                Some((k, false)) if k == i => FxGrip::Start,
                Some((k, true)) if k == i => FxGrip::End,
                _ => FxGrip::Whole,
            };
            let span = &spans[i];
            self.clear_selections();
            self.fx_drag = Some(FxDrag {
                index: i,
                grip,
                at: self.at_of(area, pos.x),
                from: span.from,
                to: span.to,
            });
            self.chosen_fx = Some(i);
        }
        let pointer_at = pointer.map(|p| self.at_of(area, p.x));
        // Onto the master grid when close (⌘: free): the edge dragged, or
        // a moved effect's start.
        let snap = self.snapping(ui);
        let grid_near = |at: u64| -> Option<u64> {
            let i = self.grid.partition_point(|g| g.at < at);
            [i.checked_sub(1), Some(i)]
                .into_iter()
                .flatten()
                .filter_map(|k| self.grid.get(k))
                .map(|g| g.at)
                .filter(|&g| (self.x_of(area, g) - self.x_of(area, at)).abs() <= GRAB + 1.0)
                .min_by_key(|&g| g.abs_diff(at))
        };
        let mut snapped_to = None;
        let held = self.fx_drag.as_ref().map(|d| (d.index, d.at, matches!(d.grip, FxGrip::Whole)));
        if let Some((index, grabbed, whole)) = held
            && let Some(at) = pointer_at
            && let Some(span) = spans.get(index)
        {
            let at = match whole {
                true => {
                    let from = (span.from as i64 + at as i64 - grabbed as i64).max(0) as u64;
                    match grid_near(from).filter(|_| snap) {
                        Some(g) => {
                            snapped_to = Some(g);
                            (g as i64 - span.from as i64 + grabbed as i64).max(0) as u64
                        }
                        None => at,
                    }
                }
                false => match grid_near(at).filter(|_| snap) {
                    Some(g) => {
                        snapped_to = Some(g);
                        g
                    }
                    None => at,
                },
            };
            let Some(drag) = self.fx_drag.as_mut() else { return };
            // Not over the stretches either side.
            let low = drag.index.checked_sub(1).map_or(0, |i| spans[i].to + 1);
            let high = spans.get(drag.index + 1).map_or(self.length, |s| s.from.saturating_sub(1));
            let length = span.to - span.from;
            match drag.grip {
                FxGrip::Whole => {
                    let from =
                        (span.from as i64 + at as i64 - drag.at as i64).max(low as i64) as u64;
                    let from = from.min(high.saturating_sub(length));
                    (drag.from, drag.to) = (from, from + length);
                }
                FxGrip::Start => drag.from = at.clamp(low, span.to.saturating_sub(1)),
                FxGrip::End => drag.to = at.clamp(span.from + 1, high),
            }
        }
        if self.fx_drag.is_some() {
            self.snap_line = snapped_to;
        }
        if response.drag_stopped()
            && let Some(drag) = self.fx_drag.take()
            && let Some(span) = spans.get(drag.index)
            && (drag.from, drag.to) != (span.from, span.to)
        {
            let old = (span.from, self.clip_end(span));
            let changes = move_fx(&self.fx, span, drag.from, drag.to);
            self.set_fx(changes, "move");
            // Its depth goes along.
            let new_end = fx_spans(&self.fx, self.length)
                .iter()
                .find(|s| s.from == self.session.lock().map_or(drag.from, |s| s.snap(drag.from)))
                .map_or(drag.to, |s| self.clip_end(s));
            let new_from = self.session.lock().map_or(drag.from, |s| s.snap(drag.from));
            self.move_clip_depth(old, new_from, new_end);
        }
    }

    /// Hands the Beat FX's new changes to the session, keeping the old for
    /// undo.
    fn set_fx(&mut self, changes: Vec<(u64, BeatFxParams)>, what: &str) {
        let Ok(mut session) = self.session.lock() else { return };
        session.set_beat_fx(&changes);
        let now = session.beat_fx();
        drop(session);
        if now != self.fx {
            let before = std::mem::replace(&mut self.fx, now);
            self.record(format!("Beat FX: {what}"), Edit::Fx(before));
        }
    }

    /// A new Beat FX from `add.at` for `add.beats` beats of the deck playing
    /// there (else seconds), or up to the next effect; its edges can then
    /// be dragged.
    fn add_fx(&mut self, add: AddFx) {
        let AddFx { at, kind, target, beats, depth, .. } = add;
        let at = self.session.lock().map_or(at, |s| s.snap(at));
        let spans = fx_spans(&self.fx, self.length);
        if spans.iter().any(|s| s.from <= at && at < s.to) {
            return;
        }
        let at = self.session.lock().map_or(at, |s| s.snap(at));
        let beat = self.beat_len(at).unwrap_or(1.0);
        let next = spans.iter().map(|s| s.from).filter(|&from| from > at).min();
        let to = (at + (beat * f64::from(beats) * self.rate) as u64)
            .min(next.map_or(self.length, |n| n.saturating_sub(1)))
            .min(self.length);
        if to <= at {
            return;
        }
        let beat_secs = (beat * add.beat) as f32;
        let on = BeatFxParams { kind, on: true, release: false, depth, beat_secs, target };
        let off = BeatFxParams { on: false, ..on };
        let mut changes: Vec<(u64, BeatFxParams)> =
            self.fx.iter().copied().filter(|c| c.0 < at || c.0 > to).collect();
        changes.push((at, on));
        changes.push((to, off));
        changes.sort_by_key(|c| c.0);
        self.set_fx(changes, &format!("add {}", kind.label()));
        if let Some(span) = fx_spans(&self.fx, self.length).iter().find(|s| s.from == at) {
            let end = self.clip_end(span);
            self.set_clip_depth(at, end, f64::from(depth));
        }
        self.chosen_fx =
            fx_spans(&self.fx, self.length).iter().position(|s| s.from <= at && at < s.to);
    }

    /// Where an effect's end handle sits: at its end, or just before it
    /// when the next effect starts right there (a moment has one point).
    fn clip_end(&self, span: &deckhand_audio::FxSpan) -> u64 {
        let touching = fx_spans(&self.fx, self.length).iter().any(|s| s.from == span.to);
        let end = if touching { span.to.saturating_sub(4096) } else { span.to };
        // Drawn every frame: never waits for a render holding the session.
        self.session.try_lock().map_or(end, |s| s.snap(end)).max(span.from + 1)
    }

    /// One flat Beat FX depth through an effect: a handle at each edge (the
    /// end one holding after it, for the echoes ringing out).
    fn set_clip_depth(&mut self, from: u64, end: u64, depth: f64) {
        let setting = Setting::BeatFxDepth;
        let before = self.lanes[&setting].clone();
        let mut lane: Vec<Point> = before.iter().filter(|p| p.at < from).copied().collect();
        lane.push(Point { at: from, value: depth, curve: Curve::Line });
        if end > from {
            lane.push(Point { at: end, value: depth, curve: Curve::Step });
        }
        lane.extend(before.iter().filter(|p| p.at > end.max(from)).copied());
        self.lanes.insert(setting, lane);
        self.commit(setting, before, &format!("depth {:.0}%", depth * 100.0));
    }

    /// The depth points of an effect moved or stretched from `old` to
    /// `from`–`to`, along with it.
    fn move_clip_depth(&mut self, old: (u64, u64), from: u64, to: u64) {
        let setting = Setting::BeatFxDepth;
        let before = self.lanes[&setting].clone();
        let length = (old.1 - old.0).max(1) as f64;
        let place = |at: u64| from + ((at - old.0) as f64 / length * (to - from) as f64) as u64;
        let inside = |at: u64| at >= old.0 && at <= old.1;
        let mine: Vec<Point> = before
            .iter()
            .filter(|p| inside(p.at))
            .map(|p| Point { at: place(p.at), ..*p })
            .collect();
        if mine.is_empty() {
            return;
        }
        let mut lane: Vec<Point> = before
            .iter()
            .filter(|p| !inside(p.at) && !(p.at >= from && p.at <= to))
            .copied()
            .collect();
        lane.extend(mine);
        lane.sort_by_key(|p| p.at);
        // The lane starts at the set's start.
        if lane.first().is_none_or(|p| p.at > 0) {
            lane.insert(0, Point::step(0, before[0].value));
        }
        self.lanes.insert(setting, lane);
        self.commit(setting, before, "move depth");
    }

    /// Splits the clips at both edges of the selected stretch, so it is a
    /// piece of its own (to move, cut out or keep); the selection stays.
    fn split_selection(&mut self) {
        let parts: Vec<Picked> = self.selected().into_iter().filter(|p| !p.whole).collect();
        if parts.is_empty() {
            self.note = Some("Select part of a clip first (Select tool, key 2)".into());
            return;
        }
        for p in parts {
            self.split_at_edges(p.deck, p.from, p.to);
        }
    }

    /// Splits `deck`'s clips at `from` and `to`.
    fn split_at_edges(&mut self, deck: usize, from: u64, to: u64) {
        let spans: Vec<TrackSpan> = self
            .tracks
            .iter()
            .filter(|t| t.deck == deck && t.from < to && from < t.to)
            .cloned()
            .collect();
        for span in spans {
            let mut pieces = Vec::new();
            for (a, b) in self.span_pieces(&span) {
                let mut cuts: Vec<u64> =
                    [from, to].into_iter().filter(|&c| a < c && c < b).collect();
                cuts.dedup();
                let mut start = a;
                for cut in cuts {
                    pieces.push((start, cut));
                    start = cut;
                }
                pieces.push((start, b));
            }
            self.set_pieces(&span, pieces, "split");
        }
    }

    /// The time `deck` played (PLAY, or CUE held) at `at`, if it did.
    fn play_at(&self, deck: usize, at: u64) -> Option<(u64, u64)> {
        self.tracks
            .iter()
            .filter(|t| t.deck == deck)
            .flat_map(|t| {
                let pieces = self.span_pieces(t);
                // Ones cut out (deleted) are not there to pick.
                t.playing
                    .iter()
                    .copied()
                    .filter(move |&(f, e)| pieces.iter().any(|&(a, b)| a < e && f < b))
                    .collect::<Vec<_>>()
            })
            .find(|&(from, to)| from <= at && at < to)
    }

    /// Whether `deck`'s clip is heard at `at` (not cut out).
    fn heard_at(&self, deck: usize, at: u64) -> bool {
        self.tracks
            .iter()
            .filter(|t| t.deck == deck && t.from <= at && at < t.to)
            .any(|t| self.span_pieces(t).iter().any(|&(f, e)| f <= at && at < e))
    }

    /// Deletes `from`–`to` of `deck`, as in a DAW: a clip it cuts through
    /// becomes two clips of their own.
    fn cut_out(&mut self, deck: usize, from: u64, to: u64, what: &str) {
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        let after = session.snap(to);
        match session.delete_range(deck, from, to) {
            Ok(()) => {
                // Where the part after it begins in the track, until the
                // replay says.
                let begun = self
                    .tracks
                    .iter()
                    .filter(|t| t.deck == deck)
                    .flat_map(|t| t.playing.iter().copied())
                    .find(|p| p.0 < after && after < p.1)
                    .and_then(|p| self.anchors.get(&(deck, p.0)).map(|&a| (p.0, a)));
                if let Some((play, anchor)) = begun {
                    let there = anchor + self.track_secs(deck, play, after);
                    self.anchors.insert((deck, after), there);
                }
                self.refresh(&session);
                drop(session);
                self.record(format!("Deck {} {what}", deck + 1), Edit::Events(before));
            }
            Err(err) => self.note = Some(format!("Not deleted: {err:#}")),
        }
    }

    /// Lets go of every selection.
    fn clear_selections(&mut self) {
        self.chosen_fx = None;
        self.picked.clear();
        self.selection = None;
        self.clip_selection = None;
        self.fx_selection = None;
    }

    /// Everything selected on the decks: the selection and what was picked
    /// with Cmd/Ctrl-click.
    fn selected(&self) -> Vec<Picked> {
        let mut all: Vec<Picked> = self
            .clip_selection
            .map(|(deck, from, to)| Picked { deck, from, to, whole: false })
            .into_iter()
            .collect();
        for p in &self.picked {
            if !all.iter().any(|q| (q.deck, q.from, q.to) == (p.deck, p.from, p.to)) {
                all.push(*p);
            }
        }
        all
    }

    /// Picks `item` with Cmd/Ctrl-click, or lets go of it if it was.
    fn toggle_pick(&mut self, item: Picked) {
        let same = |deck, from, to| (deck, from, to) == (item.deck, item.from, item.to);
        if self.clip_selection.is_some_and(|(d, f, t)| same(d, f, t)) {
            self.clip_selection = None;
        } else if let Some(i) = self.picked.iter().position(|p| same(p.deck, p.from, p.to)) {
            self.picked.remove(i);
        } else {
            self.picked.push(item);
        }
        (self.selection, self.fx_selection, self.chosen_fx) = (None, None, None);
    }

    /// How far everything selected can move: a stretch stays inside its
    /// clip, a clip inside the set.
    fn drag_limits(&self) -> (i64, i64) {
        let (mut low, mut high) = (i64::MIN, i64::MAX);
        for p in self.selected() {
            let play =
                self.session.try_lock().ok().and_then(|s| s.play_limits(p.deck, p.from, p.to));
            let (lo, hi) = if p.whole {
                (-(p.from as i64), self.length as i64 - p.to as i64)
            } else if let Some(limits) = play {
                // A play: between the plays around it.
                limits
            } else {
                match self
                    .tracks
                    .iter()
                    .find(|t| t.deck == p.deck && t.from <= p.from && p.to <= t.to)
                {
                    Some(span) => (span.from as i64 - p.from as i64, span.to as i64 - p.to as i64),
                    None => (0, 0),
                }
            };
            (low, high) = (low.max(lo), high.min(hi));
        }
        if low > high { (0, 0) } else { (low, high) }
    }

    /// Moves everything selected by `frames`, at once (one undo): stretches
    /// play at their new place laid over what is there, whole clips move
    /// with all done on them. The selection goes along.
    fn move_selection(&mut self, frames: i64) {
        let items = self.selected();
        if frames == 0 || items.is_empty() {
            return;
        }
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        // Whole clips first; stretches inside a moved clip went with it.
        let wholes: Vec<Picked> = items.iter().copied().filter(|p| p.whole).collect();
        let inside_whole = |p: &Picked| {
            wholes.iter().any(|w| w.deck == p.deck && w.from <= p.from && p.to <= w.to)
        };
        let mut moved = Vec::new();
        for w in &wholes {
            if session.move_clip(w.deck, w.from, frames).is_ok() {
                moved.push(Picked { from: shift(w.from, frames), to: shift(w.to, frames), ..*w });
            }
        }
        let mut parts: Vec<Picked> =
            items.iter().copied().filter(|p| !p.whole && !inside_whole(p)).collect();
        // Later ones first when moving later (earlier first otherwise), so
        // one moved doesn't land on one still to move.
        parts.sort_by_key(|p| if frames > 0 { u64::MAX - p.from } else { p.from });
        for p in parts {
            // A whole play moves with what was done in it: it starts where
            // it did in the track (and its box goes along).
            if session.play_limits(p.deck, p.from, p.to).is_some() {
                let (from, to) = session.move_play(p.deck, p.from, p.to, frames);
                moved.push(Picked { from, to, ..p });
                continue;
            }
            let tempo = self.value_at(Setting::Tempo(p.deck), p.from).max(0.01);
            let span = session
                .tracks()
                .into_iter()
                .find(|t| t.deck == p.deck && t.from <= p.from && p.to <= t.to);
            if let Some(span) = span {
                let (from, to) = session.move_part(&span, p.from, p.to, frames, tempo);
                moved.push(Picked { from, to, ..p });
            }
        }
        for p in items.iter().filter(|p| !p.whole && inside_whole(p)) {
            moved.push(Picked { from: shift(p.from, frames), to: shift(p.to, frames), ..*p });
        }
        self.refresh(&session);
        drop(session);
        let what = match moved.as_slice() {
            [one] => format!("Deck {} move selection", one.deck + 1),
            many => format!("Move {} selected", many.len()),
        };
        self.record(what, Edit::Events(before));
        // The selection where it went.
        let first = moved.iter().position(|p| !p.whole);
        self.clip_selection = first.map(|i| (moved[i].deck, moved[i].from, moved[i].to));
        self.picked = moved
            .into_iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != first)
            .map(|(_, p)| p)
            .collect();
    }

    /// Clears the selections but one kind.
    fn select_only(&mut self, keep: Selected) {
        self.chosen_fx = None;
        self.picked.clear();
        if !matches!(keep, Selected::Lane) {
            self.selection = None;
        }
        if !matches!(keep, Selected::Clip) {
            self.clip_selection = None;
        }
        if !matches!(keep, Selected::Fx) {
            self.fx_selection = None;
        }
    }

    /// Delete: cuts the selected stretch out of a deck's clips, takes the
    /// Beat FX off through it, or flattens a lane's selection.
    fn delete_selection(&mut self) {
        if !self.selected().is_empty() {
            for p in self.selected() {
                self.cut_out(p.deck, p.from, p.to, "delete");
            }
            (self.clip_selection, self.picked) = (None, Vec::new());
        } else if let Some((from, to)) = self.fx_selection.take() {
            let params_at = |at: u64| {
                self.fx
                    .iter()
                    .take_while(|c| c.0 <= at)
                    .last()
                    .map_or_else(BeatFxParams::default, |c| c.1)
            };
            let (start, end) = (params_at(from), params_at(to));
            let on = |p: &BeatFxParams| p.on || p.release;
            let mut changes: Vec<(u64, BeatFxParams)> =
                self.fx.iter().copied().filter(|c| c.0 < from || c.0 > to).collect();
            if on(&start) {
                changes.push((from, BeatFxParams { on: false, release: false, ..start }));
            }
            if on(&end) {
                changes.push((to, end));
            }
            changes.sort_by_key(|c| c.0);
            self.set_fx(changes, "delete");
            self.chosen_fx = None;
        } else if self.selection.is_some() {
            // The points in it go; a straight line joins the sides, only
            // across the deleted stretch.
            self.bridge(Curve::Line);
        } else if let Some(i) = self.chosen_fx.take() {
            // The Beat FX clicked on: gone.
            if let Some(span) = fx_spans(&self.fx, self.length).get(i) {
                let changes = remove_fx(&self.fx, span);
                self.set_fx(changes, "remove");
            }
        }
    }

    /// Seconds a beat at `at`: of the track playing there (as sped up or
    /// slowed down), if its BPM is known.
    fn beat_len(&self, at: u64) -> Option<f64> {
        self.tracks
            .iter()
            .find(|t| t.from <= at && at < t.to)
            .and_then(|t| {
                self.bpm_at(t.deck, at).map(|bpm| bpm * self.value_at(Setting::Tempo(t.deck), at))
            })
            .filter(|bpm| *bpm > 0.0)
            .map(|bpm| 60.0 / bpm)
    }

    /// A beat length as the mixer shows it ("1/4", "2"), else in seconds.
    fn beat_name(&self, at: u64, secs: f32) -> String {
        let Some(beat) = self.beat_len(at) else { return format!("{secs:.2} s") };
        let beats = f64::from(secs) / beat;
        BEATS
            .iter()
            .find(|(multiple, _)| (beats / multiple - 1.0).abs() < 0.04)
            .map_or_else(|| format!("{secs:.2} s"), |(_, label)| (*label).to_owned())
    }

    /// An effect's block name: its type, beat (for the ones that have
    /// one), what it is on, and REL for Release FX.
    fn fx_name(&self, span: &deckhand_audio::FxSpan) -> String {
        let mut name = span.kind.label().to_owned();
        if span.kind.uses_beat() {
            name.push(' ');
            name.push_str(&self.beat_name(span.from, span.beat_secs));
        }
        name.push_str(" · ");
        name.push_str(&target_label(span.target));
        if span.release {
            name.push_str(" · REL");
        }
        name
    }

    /// Changes the Beat FX from `from` to `to` only (by `change`): that
    /// part becomes a block of its own, the rest stays as it was.
    fn change_fx_part(
        &mut self,
        from: u64,
        to: u64,
        change: impl Fn(&mut BeatFxParams),
        what: &str,
    ) {
        let at = |t: u64| {
            self.fx
                .iter()
                .take_while(|c| c.0 <= t)
                .last()
                .map_or_else(BeatFxParams::default, |c| c.1)
        };
        let (start, end) = (at(from), at(to));
        let mut changes: Vec<(u64, BeatFxParams)> = self
            .fx
            .iter()
            .map(|&(t, mut params)| {
                if t > from && t < to && (params.on || params.release) {
                    change(&mut params);
                }
                (t, params)
            })
            .filter(|c| c.0 != from && c.0 != to)
            .collect();
        let mut first = start;
        if first.on || first.release {
            change(&mut first);
        }
        changes.push((from, first));
        changes.push((to, end));
        changes.sort_by_key(|c| c.0);
        self.set_fx(changes, what);
    }

    /// Opens the dialog to type the value of `setting`'s point at `at`.
    fn open_point_edit(&mut self, setting: Setting, at: u64, pos: Pos2) {
        let bpm = setting.deck().and_then(|d| self.bpm_at(d, at));
        let unit = Unit::of(setting, bpm);
        let text = unit.show(self.value_at(setting, at));
        self.point_edit = Some(PointEdit { setting, at, text, pos, unit, handle: None });
    }

    /// Opens the dialog to type the BPM at a clip's edge (its handle).
    fn open_handle_edit(&mut self, setting: Setting, handle: TempoHandle, pos: Pos2) {
        let at = if handle.end { Self::tempo_end_at(handle.from, handle.to) } else { handle.from };
        self.open_point_edit(setting, at, pos);
        if let Some(edit) = self.point_edit.as_mut() {
            edit.handle = Some(handle);
        }
    }

    /// The typed value dialog; Enter sets it, Esc closes.
    fn point_edit_dialog(&mut self, ctx: &egui::Context) {
        let Some(edit) = self.point_edit.as_mut() else { return };
        let (mut done, mut cancel) = (false, ctx.input(|i| i.key_pressed(egui::Key::Escape)));
        let label = self.lanes.get(&edit.setting).map_or("", |_| edit.setting.label());
        egui::Window::new(format!("{label} point"))
            .id(egui::Id::new("point value"))
            .collapsible(false)
            .resizable(false)
            .default_pos(edit.pos + vec2(10.0, 10.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let field =
                        ui.add(egui::TextEdit::singleline(&mut edit.text).desired_width(80.0));
                    // Typing straight away.
                    if !field.has_focus() && !field.lost_focus() {
                        field.request_focus();
                    }
                    ui.label(edit.unit.name());
                    done = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                });
                ui.horizontal(|ui| {
                    done |= ui.button(RichText::new("Set").strong()).clicked();
                    cancel |= ui.button("Cancel").clicked();
                });
            });
        if cancel {
            self.point_edit = None;
            return;
        }
        if !done {
            return;
        }
        let Some(edit) = self.point_edit.take() else { return };
        let Some(value) = edit.unit.read(&edit.text) else {
            self.note = Some(format!("Not a number: {}", edit.text));
            return;
        };
        // Within what the mixer takes.
        let value = match edit.setting {
            Setting::Tempo(_) => value.clamp(0.25, 4.0),
            Setting::Eq(..) => value
                .clamp(f64::from(deckhand_audio::EQ_MIN_DB), f64::from(deckhand_audio::EQ_MAX_DB)),
            Setting::Trim(_) => value.clamp(0.0, 4.0),
            s => {
                let (lo, hi) = s.range();
                value.clamp(lo, hi)
            }
        };
        let setting = edit.setting;
        let before = self.lanes[&setting].clone();
        if let Some(handle) = edit.handle {
            self.pin_clip_tempo(setting, handle);
        }
        let lane = self.lanes.get_mut(&setting).expect("every setting has a lane");
        match lane.iter_mut().find(|p| p.at == edit.at) {
            Some(point) => point.value = value,
            // An effect's edge with no point yet.
            None => {
                let k = lane.partition_point(|p| p.at < edit.at);
                lane.insert(k, Point { at: edit.at, value, curve: Curve::Line });
            }
        }
        self.commit(setting, before, &format!("set {}", edit.unit.show(value)));
    }

    /// The Add Beat FX dialog, next to where it was asked for.
    fn add_fx_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut add) = self.adding_fx else { return };
        let (mut done, mut cancel) = (false, ctx.input(|i| i.key_pressed(egui::Key::Escape)));
        let secs = add.at as f64 / self.rate;
        egui::Window::new("Add Beat FX")
            .id(egui::Id::new("add beat fx"))
            .collapsible(false)
            .resizable(false)
            .default_pos(add.pos + vec2(8.0, 8.0))
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!("From {}:{:05.2}", (secs / 60.0) as u64, secs % 60.0))
                        .weak(),
                );
                ui.add_space(4.0);
                egui::Grid::new("add fx kinds").spacing(vec2(4.0, 4.0)).show(ui, |ui| {
                    for (i, kind) in BeatFxKind::ALL.into_iter().enumerate() {
                        ui.selectable_value(&mut add.kind, kind, kind.label());
                        if i % 4 == 3 {
                            ui.end_row();
                        }
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("On");
                    for target in [
                        FxTarget::Channel(0),
                        FxTarget::Channel(1),
                        FxTarget::Channel(2),
                        FxTarget::Channel(3),
                        FxTarget::Sampler,
                        FxTarget::Master,
                    ] {
                        ui.selectable_value(&mut add.target, target, target_label(target));
                    }
                });
                if add.kind.uses_beat() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Beat");
                        for (beat, label) in BEATS {
                            ui.selectable_value(&mut add.beat, beat, label);
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.label("Length");
                    for beats in [1, 2, 4, 8, 16, 32] {
                        ui.selectable_value(&mut add.beats, beats, beats.to_string());
                    }
                    ui.label("beats");
                });
                ui.horizontal(|ui| {
                    ui.label("Depth");
                    ui.add(
                        egui::Slider::new(&mut add.depth, 0.0..=1.0)
                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                    )
                    .on_hover_text(
                        "Start depth for both edges; drag them apart later",
                    );
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    done = ui.button(RichText::new("Add").strong()).clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Enter));
                    cancel |= ui.button("Cancel").clicked();
                });
            });
        if done {
            self.last_fx = add;
            self.adding_fx = None;
            self.add_fx(add);
        } else if cancel {
            self.adding_fx = None;
        } else {
            self.adding_fx = Some(add);
        }
    }

    /// An effect's menu: its type, what it is on, its depth (wet/dry)
    /// through the stretch, or off.
    fn fx_menu(&mut self, ui: &mut Ui, span: &deckhand_audio::FxSpan) {
        ui.menu_button(format!("Type: {}", span.kind.label()), |ui| {
            for kind in BeatFxKind::ALL {
                if ui.selectable_label(kind == span.kind, kind.label()).clicked() {
                    let changes = restyle_fx(&self.fx, span, Some(kind), None, None);
                    self.set_fx(changes, kind.label());
                    ui.close();
                }
            }
        });
        if span.kind.uses_beat() {
            let now = self.beat_name(span.from, span.beat_secs);
            ui.menu_button(format!("Beat: {now}"), |ui| {
                let beat = self.beat_len(span.from).unwrap_or(1.0);
                for (multiple, label) in BEATS {
                    if ui.selectable_label(label == now, label).clicked() {
                        let secs = (beat * multiple) as f32;
                        let changes = restyle_fx(&self.fx, span, None, None, Some(secs));
                        self.set_fx(changes, &format!("beat {label}"));
                        ui.close();
                    }
                }
            });
        }
        ui.menu_button(format!("On: {}", target_label(span.target)), |ui| {
            for target in [
                FxTarget::Channel(0),
                FxTarget::Channel(1),
                FxTarget::Channel(2),
                FxTarget::Channel(3),
                FxTarget::Sampler,
                FxTarget::Master,
            ] {
                let label = match target {
                    FxTarget::Channel(d) => format!("Deck {}", d + 1),
                    FxTarget::Sampler => "Sampler".into(),
                    FxTarget::Master => "Master".into(),
                };
                if ui.selectable_label(target == span.target, label).clicked() {
                    let changes = restyle_fx(&self.fx, span, None, Some(target), None);
                    self.set_fx(changes, &format!("on {}", target_label(target)));
                    ui.close();
                }
            }
        });
        // Release FX: the sound cut, the echoes playing out.
        if span.kind.has_tail() {
            let mut release = span.release;
            if ui
                .checkbox(&mut release, "Release FX")
                .on_hover_text("Cut the sound, let echoes ring")
                .changed()
            {
                let (from, to) = (span.from, span.to);
                self.change_fx_part(
                    from,
                    to,
                    |p| p.release = release,
                    if release { "release" } else { "no release" },
                );
            }
        }
        // One depth through the stretch.
        let now = self.value_at(Setting::BeatFxDepth, span.from) as f32;
        let id = ui.id().with("fx depth");
        let mut depth = ui.data(|d| d.get_temp::<f32>(id)).unwrap_or(now);
        ui.horizontal(|ui| {
            ui.label("Depth");
            let slider = ui.add(
                egui::Slider::new(&mut depth, 0.0..=1.0)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
            );
            ui.data_mut(|d| d.insert_temp(id, depth));
            if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                let end = self.clip_end(span);
                self.set_clip_depth(span.from, end, f64::from(depth));
            }
        })
        .response
        .on_hover_text("0% dry · 100% only the effect");
        ui.separator();
        if ui.button("Remove").clicked() {
            let changes = remove_fx(&self.fx, span);
            self.set_fx(changes, "remove");
            self.chosen_fx = None;
            ui.close();
        }
    }

    /// A value with its real units: the BPM, decibels, the filter's
    /// frequency and the crossfader's side.
    fn describe(&self, setting: Setting, at: u64, value: f64) -> String {
        let db = |gain: f64| {
            if gain <= 1e-6 {
                "−∞ dB".to_owned()
            } else {
                format!("{:+.1} dB", 20.0 * gain.log10())
            }
        };
        let session = self.session.try_lock().ok();
        match setting {
            Setting::Tempo(deck) => {
                // Master Tempo stretches (the key stays); without it the
                // track is sped up or slowed like a record.
                let locked = self.key_locks.get(deck).and_then(|changes| {
                    changes.iter().take_while(|c| c.0 <= at).last().map(|c| c.1)
                });
                let how = match locked {
                    Some(true) => " · MT",
                    _ => "",
                };
                match self.bpm_at(deck, at) {
                    // The track's own BPM is in its title.
                    // Where it lands, not how far it is off the track's own.
                    Some(bpm) => format!("{:.2} BPM{how}", bpm * value),
                    None => format!("{}{how}", setting.display(value)),
                }
            }
            Setting::Volume(_) => match session {
                Some(s) => format!("{:.0}% · {}", value * 100.0, db(s.fader_gain(value))),
                None => setting.display(value),
            },
            Setting::Master => match session {
                Some(s) => format!("{:.0}% · {}", value * 100.0, db(s.master_gain(value))),
                None => setting.display(value),
            },
            Setting::ColorFx(_, ColorFxKind::Filter) => {
                let amount = ((value.abs() - 0.02) / 0.98).max(0.0);
                let hz = |hz: f64| {
                    if hz >= 1000.0 {
                        format!("{:.1} kHz", hz / 1000.0)
                    } else {
                        format!("{hz:.0} Hz")
                    }
                };
                if value.abs() <= 0.02 {
                    "off".to_owned()
                } else if value < 0.0 {
                    format!("low-pass {}", hz(20_000.0 * (60.0f64 / 20_000.0).powf(amount)))
                } else {
                    format!("high-pass {}", hz(20.0 * (8_000.0f64 / 20.0).powf(amount)))
                }
            }
            Setting::ColorFx(_, ColorFxKind::Pitch) if value != 0.0 => {
                format!("{:+.1} semitones", value * 12.0)
            }
            Setting::ColorFx(..) if value == 0.0 => "off".to_owned(),
            Setting::ColorFx(..) => {
                let side = if value < 0.0 { "dark" } else { "bright" };
                format!("{:.0}% · {side}", value.abs() * 100.0)
            }
            Setting::Crossfader if value.abs() < 0.005 => "centre".to_owned(),
            Setting::Crossfader if value < 0.0 => format!("A side {:.0}%", -value * 100.0),
            Setting::Crossfader => format!("B side {:.0}%", value * 100.0),
            Setting::Eq(..) if value <= f64::from(deckhand_audio::EQ_MIN_DB) => "kill".to_owned(),
            _ => setting.display(value),
        }
    }

    fn lane(&mut self, ui: &Ui, row: Rect, response: &egui::Response, setting: Setting) {
        let visuals = ui.visuals().clone();
        // Under its deck's row, so the name alone.
        let label = setting.label();
        let color = setting
            .deck()
            .map_or(visuals.strong_text_color(), |d| DECK_COLORS[d % DECK_COLORS.len()]);
        ui.painter().text(
            pos2(row.left() + 4.0, row.top() + 4.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(13.0),
            color,
        );
        let area = Rect::from_min_max(
            pos2(row.left() + LABELS, row.top() + 1.0),
            row.max - vec2(0.0, 1.0),
        );
        let dragged = match &self.drag {
            Some(Drag::Point { setting: s, index, .. }) if *s == setting => {
                self.lanes[&setting].get(*index).map(|p| (p.at, p.value))
            }
            _ => None,
        };
        let shown = dragged.or_else(|| self.hover.map(|at| (at, self.value_at(setting, at))));
        if let Some((at, value)) = shown {
            ui.painter().text(
                pos2(row.left() + 4.0, row.top() + 22.0),
                egui::Align2::LEFT_TOP,
                self.describe(setting, at, value),
                egui::FontId::monospace(11.0),
                visuals.text_color(),
            );
        }
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 2.0, visuals.extreme_bg_color);
        if let Some(at) = self.hover {
            let x = self.x_of(area, at);
            painter.line_segment(
                [pos2(x, area.top()), pos2(x, area.bottom())],
                Stroke::new(1.0_f32, visuals.weak_text_color().gamma_multiply(0.5)),
            );
        }
        let neutral = self.y_of(area, setting, setting.neutral());
        painter.line_segment(
            [pos2(area.left(), neutral), pos2(area.right(), neutral)],
            Stroke::new(1.0_f32, visuals.faint_bg_color),
        );
        if let Some((s, from, to)) = self.selection
            && s == setting
        {
            let selected =
                Rect::from_x_y_ranges(self.x_of(area, from)..=self.x_of(area, to), area.y_range());
            painter.rect_filled(selected, 0.0, visuals.selection.bg_fill.gamma_multiply(0.5));
        }

        // The line through the points, and the points where there is room
        // to see them.
        let points = &self.lanes[&setting];
        let view = (
            (self.view_from.max(0.0) * self.rate) as u64,
            ((self.view_from + self.view_secs) * self.rate) as u64,
        );
        let first = points.partition_point(|p| p.at < view.0).saturating_sub(1);
        let last = points.partition_point(|p| p.at <= view.1);
        let mut line = Vec::new();
        let mut dots = Vec::new();
        let mut last_dot = f32::NEG_INFINITY;
        for (i, p) in points.iter().enumerate().take(last).skip(first) {
            let (x, y) =
                (self.x_of(area, p.at).max(area.left()), self.y_of(area, setting, p.value));
            if let (Some(&Pos2 { y: before, .. }), Some(previous)) =
                (line.last(), points.get(i.wrapping_sub(1)))
            {
                match previous.curve {
                    Curve::Step => line.push(pos2(x, before)),
                    Curve::Line => {}
                    Curve::Smooth => {
                        let from = self.x_of(area, previous.at).max(area.left());
                        for k in 1..16 {
                            let t = f64::from(k) / 16.0;
                            let at = previous.at + ((p.at - previous.at) as f64 * t) as u64;
                            let value = lane_value(&points[i - 1..=i], at);
                            line.push(pos2(
                                from + (x - from) * t as f32,
                                self.y_of(area, setting, value),
                            ));
                        }
                    }
                }
            }
            line.push(pos2(x, y));
            if x - last_dot >= 5.0 {
                dots.push((i, pos2(x, y)));
                last_dot = x;
            }
        }
        if let Some(&Pos2 { y, .. }) = line.last() {
            let end = self.x_of(area, self.length).min(area.right());
            line.push(pos2(end, y));
        }
        painter.add(Shape::line(line, Stroke::new(1.5_f32, color)));
        self.play_marks(&painter, area);
        let dense = dots.len() as f32 > area.width() / 6.0;
        if !dense {
            for (_, at) in &dots {
                painter.circle_filled(*at, 2.5, color);
            }
        }

        // Each clip's BPM at its start and end, as handles on its deck's
        // tempo lane.
        let handles = match setting {
            Setting::Tempo(deck) => self.tempo_handles(area, deck),
            _ => Vec::new(),
        };
        for h in &handles {
            painter.circle_filled(h.pos, 4.5, color);
            painter.circle_stroke(h.pos, 4.5, Stroke::new(1.0_f32, Color32::WHITE));
            if let Some(bpm) = h.bpm {
                let (anchor, x) = if h.end {
                    (egui::Align2::RIGHT_BOTTOM, h.pos.x - 6.0)
                } else {
                    (egui::Align2::LEFT_BOTTOM, h.pos.x + 6.0)
                };
                painter.text(
                    pos2(x, h.pos.y - 3.0),
                    anchor,
                    format!("{bpm:.2}"),
                    egui::FontId::monospace(10.0),
                    color,
                );
            }
        }
        let handle_near = |pos: Pos2| {
            handles
                .iter()
                .filter(|h| h.pos.distance(pos) <= GRAB + 3.0)
                .min_by(|a, b| a.pos.distance(pos).total_cmp(&b.pos.distance(pos)))
                .copied()
        };

        let editable = !self.busy();
        let pointer = response.interact_pointer_pos().or(response.hover_pos());
        // The point under the pointer: the nearest on screen (every one, not
        // only the dots drawn where points crowd), a little forgiving.
        let spots: Vec<(usize, Pos2)> = {
            let points = &self.lanes[&setting];
            (first..last.min(points.len()))
                .map(|i| {
                    let p = &points[i];
                    let x = self.x_of(area, p.at).max(area.left());
                    (i, pos2(x, self.y_of(area, setting, p.value)))
                })
                .collect()
        };
        let near = |pos: Pos2| {
            spots
                .iter()
                .map(|&(i, at)| (i, at.distance(pos)))
                .filter(|(_, d)| *d <= GRAB + 3.0)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        };
        if let Some(pos) = response.hover_pos()
            && area.contains(pos)
        {
            let at = self.at_of(area, pos.x);
            let value = self.value_at(setting, at);
            let secs = at as f64 / self.rate;
            painter.text(
                pos2(area.right() - 4.0, area.top() + 2.0),
                egui::Align2::RIGHT_TOP,
                format!(
                    "{}:{:05.2}  {}",
                    (secs / 60.0) as u64,
                    secs % 60.0,
                    setting.display(value)
                ),
                egui::FontId::monospace(11.0),
                visuals.weak_text_color(),
            );
            if let Some(i) = near(pos) {
                painter.circle_stroke(
                    spots.iter().find(|d| d.0 == i).map_or(pos, |d| d.1),
                    5.0,
                    Stroke::new(1.5_f32, color),
                );
            }
            // The value at the cursor (a point's own when on one).
            if self.drag.is_none() {
                let (at, value) = match near(pos) {
                    Some(i) => {
                        let p = self.lanes[&setting][i];
                        (p.at, p.value)
                    }
                    None => (at, value),
                };
                value_tag(&painter, area, pos, &self.describe(setting, at, value));
            }
        }
        if !editable || self.panning {
            return;
        }
        if response.drag_started()
            && self.tool == Tool::Normal
            && let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(pointer)
            && let Some(handle) = handle_near(pos)
        {
            // A clip edge's BPM: points pinned at its edges (and just after
            // it, so what follows keeps its tempo), the handle's goes up and
            // down.
            self.clear_selections();
            let range = self.range(setting);
            let before = self.lanes[&setting].clone();
            let index = self.pin_clip_tempo(setting, handle);
            self.drag = Some(Drag::Point { setting, index, before, range, pinned: true });
        } else if response.drag_started()
            && let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(pointer)
        {
            self.drag = Some(match near(pos).filter(|_| self.tool == Tool::Normal) {
                Some(index) => {
                    self.clear_selections();
                    let range = self.range(setting);
                    Drag::Point {
                        setting,
                        index,
                        before: self.lanes[&setting].clone(),
                        range,
                        pinned: false,
                    }
                }
                None => {
                    let from = self.at_of(area, pos.x);
                    self.select_only(Selected::Lane);
                    self.selection = Some((setting, from, from));
                    Drag::Select { setting, from }
                }
            });
        }
        if response.dragged()
            && let Some(pos) = pointer
        {
            match self.drag {
                Some(Drag::Point { setting: s, index, range: (lo, hi), pinned, .. }) if s == setting => {
                    let share = f64::from((area.bottom() - 3.0 - pos.y) / (area.height() - 6.0));
                    let value = lo + share.clamp(0.0, 1.0) * (hi - lo);
                    let at = self.at_of(area, pos.x);
                    let lane = self.lanes.get_mut(&setting).expect("every setting has a lane");
                    // Between its neighbours, apart enough not to land on the
                    // same buffer as one; the first stays at the start.
                    let low = if index == 0 { 0 } else { lane[index - 1].at + POINT_GAP };
                    let high =
                        lane.get(index + 1).map_or(u64::MAX, |p| p.at.saturating_sub(POINT_GAP));
                    lane[index] = Point {
                        curve: lane[index].curve,
                        at: if index == 0 || pinned {
                            lane[index].at
                        } else {
                            at.clamp(low, high.max(low))
                        },
                        value,
                    };
                }
                Some(Drag::Select { setting: s, from }) if s == setting => {
                    let to = self.at_of(area, pos.x);
                    self.selection = Some((setting, from.min(to), from.max(to)));
                }
                _ => {}
            }
        }
        if response.drag_stopped() {
            match self.drag.take() {
                Some(Drag::Point { setting: s, before, .. }) if s == setting => {
                    self.commit(setting, before, "move point")
                }
                Some(Drag::Select { .. }) => {
                    if self.selection.is_some_and(|(_, from, to)| to <= from) {
                        self.selection = None;
                    }
                }
                other => self.drag = other,
            }
        }
        if response.clicked() {
            self.selection = None;
            self.chosen_fx = None;
        }
        // Double-click on a clip edge's BPM handle, or a point: type its
        // value.
        let on_handle = pointer.and_then(|pos| handle_near(pos).map(|h| (h, pos)));
        if response.double_clicked()
            && let Some((handle, pos)) = on_handle
        {
            self.open_handle_edit(setting, handle, pos);
        } else if response.double_clicked()
            && let Some(pos) = pointer
            && let Some(i) = near(pos)
        {
            let at = self.lanes[&setting][i].at;
            self.open_point_edit(setting, at, pos);
        } else if response.double_clicked()
            && let Some(pos) = pointer
            && near(pos).is_none()
        {
            let before = self.lanes[&setting].clone();
            let point = Point {
                at: self.at_of(area, pos.x),
                value: self.value_of(area, setting, pos.y),
                curve: self.shape,
            };
            let lane = self.lanes.get_mut(&setting).expect("every setting has a lane");
            let index = lane.partition_point(|p| p.at <= point.at);
            lane.insert(index, point);
            self.commit(setting, before, "add point");
        }
        let on_point = pointer.and_then(near);
        if response.secondary_clicked()
            && let Some(index) = on_point.filter(|i| *i > 0)
        {
            let before = self.lanes[&setting].clone();
            remove_point(self.lanes.get_mut(&setting).expect("every setting has a lane"), index);
            self.commit(setting, before, "delete point");
        }
        // Right click away from points: back to the recording, and more.
        if on_point.is_none() {
            let selected = self.selection.filter(|(s, ..)| *s == setting);
            let changed = self.lanes[&setting] != self.original[&setting];
            response.context_menu(|ui| {
                if let Some((_, from, to)) = selected {
                    if ui
                        .button("Reset selection to original")
                        .on_hover_text("Back to as recorded")
                        .clicked()
                    {
                        self.reset(setting, from, to);
                        ui.close();
                    }
                    if ui.button("Flatten").clicked() {
                        self.flatten();
                        ui.close();
                    }
                    if ui.button("Ramp").clicked() {
                        self.ramp();
                        ui.close();
                    }
                    ui.separator();
                }
                if ui
                    .add_enabled(changed, egui::Button::new("Reset whole lane to original"))
                    .clicked()
                {
                    self.reset(setting, 0, self.length);
                    ui.close();
                }
                if ui.button("Hide lane").clicked() {
                    self.shown.retain(|s| *s != setting);
                    ui.close();
                }
            });
        }
    }

    /// Plays the edited set into "<name> edit.wav" in the background.
    fn start_render(&mut self) {
        let range = self.range;
        let out = free_path(&self.path, if range.is_some() { "edit part" } else { "edit" });
        let session = self.session.clone();
        let done_frames = Arc::new(AtomicU64::new(0));
        let progress = done_frames.clone();
        let (tx, done) = std::sync::mpsc::channel();
        let total = range.map_or(self.length, |r| r.1);
        std::thread::spawn(move || {
            let result = session
                .lock()
                .map_err(|_| anyhow::anyhow!("the session is broken"))
                .and_then(|s| {
                    let progress = |f| progress.store(f, Ordering::Relaxed);
                    match range {
                        Some((from, to)) => s.render_part_to_wav(&out, from, to, progress),
                        None => s.render_to_wav(&out, None, progress).map(|r| r.frames),
                    }
                })
                .map(|_| {
                    format!(
                        "Rendered into {}",
                        out.file_name().unwrap_or_default().to_string_lossy()
                    )
                });
            let _ = tx.send(result);
        });
        self.render = Some(Render { done_frames, total, done });
    }

    /// Copies the session as edited, with its songs and the stems kept
    /// with it, into a new folder where the user picks, in the background.
    fn start_export(&mut self) {
        let here = self.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let Some(parent) = rfd::FileDialog::new()
            .set_title("Export the session with its songs into")
            .set_directory(&here)
            .pick_folder()
        else {
            return;
        };
        let dir = (1..)
            .map(|n| match n {
                1 => parent.join(format!("{} export", self.name)),
                n => parent.join(format!("{} export {n}", self.name)),
            })
            .find(|d| !d.exists())
            .unwrap_or_else(|| parent.join(format!("{} export", self.name)));
        let session = self.session.clone();
        let file = dir.join(format!("{}.session", self.name));
        let stems = here.join("stems");
        let (tx, rx) = std::sync::mpsc::channel();
        let out = dir.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<String> {
                std::fs::create_dir_all(&out)?;
                let tracks: Vec<PathBuf> = {
                    let session =
                        session.lock().map_err(|_| anyhow::anyhow!("the session is broken"))?;
                    session.write(&file)?;
                    session.track_files().into_iter().map(|(path, _)| path).collect()
                };
                let done = deckhand_audio::export_files(&out, &tracks, Some(&stems))?;
                let mut note = format!(
                    "Exported with {} songs ({:.0} MB) into {}",
                    done.tracks,
                    done.bytes as f64 / 1e6,
                    out.file_name().unwrap_or_default().to_string_lossy()
                );
                if !done.missing.is_empty() {
                    note.push_str(&format!("; {} not found to copy", done.missing.len()));
                }
                Ok(note)
            })();
            let _ = tx.send((result, out));
        });
        self.export = Some(rx);
        self.note = Some("Exporting with songs…".into());
    }

    /// Writes the edited session where the user picks.
    fn save(&mut self) -> Option<String> {
        let dir = self.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let path = rfd::FileDialog::new()
            .set_title("Save the edited session")
            .set_directory(dir)
            .set_file_name(format!("{} edit.session", self.name))
            .add_filter("Session", &["session"])
            .save_file()?;
        let written = self.session.lock().map_err(|_| anyhow::anyhow!("the session is broken"));
        Some(match written.and_then(|s| s.write(&path)) {
            Ok(()) => {
                self.edited = false;
                self.name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                self.path = path;
                format!("Saved {}", self.name)
            }
            Err(err) => format!("Not saved: {err:#}"),
        })
    }
}

/// A tool's button: its pointer drawn (an arrow for Normal, an I-beam for
/// Select) before its name.
fn tool_button(ui: &mut Ui, tool: Tool, label: &str, on: bool) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    // Its colour comes when painted (hovered, chosen, theme).
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let icon = 12.0;
    let size = vec2(icon + 6.0 + galley.size().x + 12.0, ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let visuals = ui.style().interact_selectable(&response, on);
    if on || response.hovered() {
        ui.painter().rect_filled(rect, 3.0, visuals.weak_bg_fill);
    }
    let color = visuals.text_color();
    let c = pos2(rect.left() + 6.0 + icon / 2.0, rect.center().y);
    match tool {
        // A pointer arrow.
        Tool::Normal => {
            let top = c + vec2(-4.0, -6.0);
            ui.painter().add(Shape::convex_polygon(
                vec![top, top + vec2(0.0, 11.0), top + vec2(3.0, 8.0), top + vec2(7.5, 8.0)],
                color,
                Stroke::NONE,
            ));
            ui.painter().line_segment(
                [top + vec2(3.0, 8.0), top + vec2(5.0, 12.0)],
                Stroke::new(1.6_f32, color),
            );
        }
        // An I-beam.
        Tool::Select => {
            let stroke = Stroke::new(1.4_f32, color);
            let (top, bottom) = (c.y - 6.0, c.y + 6.0);
            ui.painter().line_segment([pos2(c.x, top), pos2(c.x, bottom)], stroke);
            for y in [top, bottom] {
                ui.painter().line_segment([pos2(c.x - 3.0, y), pos2(c.x + 3.0, y)], stroke);
            }
        }
    }
    ui.painter().galley(
        pos2(rect.left() + 6.0 + icon + 6.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    response
}

/// `at` moved by `frames` (not before the start).
fn shift(at: u64, frames: i64) -> u64 {
    (at as i64 + frames).max(0) as u64
}

/// Takes a point out of a lane; the one before it then runs in a straight
/// line to the one after (unless it was a curve), not a step.
fn remove_point(lane: &mut Vec<Point>, index: usize) {
    lane.remove(index);
    if let Some(before) = index.checked_sub(1).and_then(|i| lane.get_mut(i))
        && before.curve == Curve::Step
    {
        before.curve = Curve::Line;
    }
}

/// A value next to the cursor, on a dark tag, kept inside `area`.
fn value_tag(painter: &egui::Painter, area: Rect, pos: Pos2, text: &str) {
    // Above everything, so rows drawn later or the row's edge don't hide it.
    let painter = painter
        .ctx()
        .layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("value tag")));
    let galley =
        painter.layout_no_wrap(text.to_owned(), egui::FontId::monospace(11.0), Color32::WHITE);
    let size = galley.size() + vec2(8.0, 4.0);
    // Up and right of the pointer; flipped where it would leave the area.
    let mut at = pos + vec2(12.0, -size.y - 6.0);
    if at.x + size.x > area.right() {
        at.x = pos.x - size.x - 12.0;
    }
    if at.y < area.top() {
        at.y = pos.y + 10.0;
    }
    let rect = Rect::from_min_size(at, size);
    painter.rect_filled(rect, 3.0, Color32::from_black_alpha(200));
    painter.galley(rect.min + vec2(4.0, 2.0), galley, Color32::WHITE);
}

/// EDIT mode is not finished: says so.
fn wip(ui: &mut Ui) {
    ui.label(RichText::new("WORK IN PROGRESS").small().strong().color(WARN)).on_hover_text(
        "EDIT is unfinished. Edits save as new files; recordings stay untouched.",
    );
}

/// Where a lane goes: by deck, then as [`Setting::all`] lists them.
fn lane_order(setting: &Setting) -> usize {
    Setting::all(deckhand_audio::DECKS).iter().position(|s| s == setting).unwrap_or(usize::MAX)
}

/// "<session> <suffix>.wav" next to the session, numbered if taken.
pub(super) fn free_path(session: &Path, suffix: &str) -> PathBuf {
    let stem = session.file_stem().unwrap_or_default().to_string_lossy();
    (1..)
        .map(|n| match n {
            1 => session.with_file_name(format!("{stem} {suffix}.wav")),
            n => session.with_file_name(format!("{stem} {suffix} {n}.wav")),
        })
        .find(|p| !p.exists())
        .unwrap_or_default()
}

impl DeckhandApp {
    /// EDIT mode's screen.
    pub(super) fn edit_view(&mut self, ui: &mut Ui) {
        if !self.edit_view.opened_at_start {
            self.edit_view.opened_at_start = true;
            if let Some(path) = std::env::var_os("DECKHAND_EDIT_SESSION") {
                self.open_session(path.into(), ui.ctx());
            }
        }
        self.poll_edit_view(ui.ctx());
        // Taken before the toolbar, so a focused button doesn't get it too.
        let space = !ui.ctx().wants_keyboard_input()
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space));
        let device = self.engine.as_ref().map(|e| e.info().device.clone());
        if let Some(editor) = self.edit_view.editor.as_mut() {
            editor.device = device;
            editor.poll_player(ui.ctx());
        }
        let mut message = None;
        // Wraps onto more rows when the window is too narrow for it all.
        ui.horizontal_wrapped(|ui| {
            let opening = self.edit_view.opening.is_some();
            if ui
                .add_enabled(!opening, egui::Button::new("Open session…"))
                .tour(crate::tour::Target::EditOpen)
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("Session to edit")
                    .set_directory(self.recordings_dir())
                    .add_filter("Session", &["session"])
                    .pick_file()
            {
                self.open_session(path, ui.ctx());
            }
            if let Some((name, _)) = &self.edit_view.opening {
                ui.spinner();
                ui.label(format!("Opening {name}…"));
            }
            let Some(editor) = self.edit_view.editor.as_mut() else { return };
            let idle = !editor.busy();
            if ui.add_enabled(idle, egui::Button::new("Save edit…")).tour(crate::tour::Target::EditSave).clicked() {
                message = editor.save();
            }
            let (label, hint) = match editor.range {
                Some(_) => (
                    "Render range",
                    "Plays the range marked on the ruler into a new WAV next to the session \
                     (Select tool: drag along the ruler; click it to let go)",
                ),
                None => (
                    "Render",
                    "Plays the edited set into a new WAV next to the session. To render a part, \
                     mark it on the ruler with the Select tool",
                ),
            };
            if ui
                .add_enabled(idle, egui::Button::new(label))
                .on_hover_text(hint)
                .tour(crate::tour::Target::EditRender)
                .clicked()
            {
                editor.start_render();
            }
            // What is going on, in a slot of its own size whether or not
            // there is anything: coming and going, it moves nothing.
            let size = vec2(210.0, ui.spacing().interact_size.y);
            ui.allocate_ui_with_layout(size, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_size(size);
                ui.set_max_size(size);
                let busy = if let Some(render) = &editor.render {
                    let done = render.done_frames.load(Ordering::Relaxed);
                    Some(format!("Rendering… {}%", done * 100 / render.total.max(1)))
                } else if editor.resync.is_some() {
                    Some("Syncing again…".to_owned())
                } else if editor.playing_out.is_some() {
                    Some("Drawing the waveforms…".to_owned())
                } else {
                    None
                };
                if let Some(busy) = busy {
                    ui.add(egui::Spinner::new().size(size.y * 0.7));
                    ui.add(egui::Label::new(RichText::new(busy).small()).truncate());
                } else if editor.edited {
                    ui.label(RichText::new("edited").small().color(WARN));
                }
            });
            ui.separator();
            editor.toolbar(ui);
            ui.separator();
            wip(ui);
        });
        if message.is_some() {
            self.status = message;
        }
        if let Some(note) = self.edit_view.editor.as_mut().and_then(|e| e.note.take()) {
            self.status = Some(note);
        }
        ui.separator();
        let Some(editor) = self.edit_view.editor.as_mut() else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Edit a recorded set").size(18.0).strong());
                wip(ui);
                ui.label(
                    "Open a recording's .session to edit it and render a new WAV.",
                );
            });
            return;
        };
        let typing = ui.ctx().wants_keyboard_input();
        if space {
            editor.toggle_play();
        }
        editor.tool_keys = self.keyboard.tool_labels();
        match self.keyboard.edit_tool(ui.ctx()) {
            Some(0) => editor.tool = Tool::Normal,
            Some(1) => editor.tool = Tool::Select,
            Some(2) if !editor.busy() => editor.split_selection(),
            _ => {}
        }
        let (delete, undo, redo, nudge) = ui.input(|i| {
            let step = if i.modifiers.shift { 0.0001 } else { 0.001 };
            let nudge = if i.key_pressed(egui::Key::ArrowRight) {
                step
            } else if i.key_pressed(egui::Key::ArrowLeft) {
                -step
            } else {
                0.0
            };
            (
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                i.modifiers.command
                    && (i.modifiers.shift && i.key_pressed(egui::Key::Z)
                        || i.key_pressed(egui::Key::Y)),
                nudge,
            )
        });
        if !typing && !editor.busy() {
            if delete {
                editor.delete_selection();
            }
            if undo {
                editor.undo();
            }
            if redo {
                editor.redo();
            }
            if nudge != 0.0 {
                editor.nudge_move(nudge);
            }
        }
        // Follow playhead is a preference, kept between runs.
        editor.follow_playhead = self.config.prefs.edit_follow_playhead;
        editor.timeline(ui);
        if editor.follow_playhead != self.config.prefs.edit_follow_playhead {
            self.config.prefs.edit_follow_playhead = editor.follow_playhead;
            self.config.save();
        }
    }

    fn open_session(&mut self, path: PathBuf, ctx: &egui::Context) {
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let opened = Session::read(&path, super::recording::Sources).map(|s| {
                let outlines = s.outlines(OUTLINE_STEP);
                (path, s, outlines)
            });
            let _ = tx.send(opened);
            ctx.request_repaint();
        });
        self.edit_view.opening = Some((name, rx));
    }

    fn poll_edit_view(&mut self, ctx: &egui::Context) {
        if let Some((_, rx)) = &self.edit_view.opening
            && let Ok(result) = rx.try_recv()
        {
            self.edit_view.opening = None;
            match result {
                Ok((path, mut session, outlines)) => {
                    // Recorded before beat grids were kept: the collection's.
                    for (file, duration) in session.track_files() {
                        if session.grid(&file).is_none()
                            && let Some(track) =
                                self.collection.tracks.iter().find(|t| t.path == file)
                            && let Some(bpm) = track.bpm
                        {
                            let grid = track.beats(duration);
                            let beats = grid.iter().map(|b| b.0).collect();
                            let bars = grid.iter().map(|b| b.1).collect();
                            session.add_grid(file, deckhand_audio::Grid { bpm, beats, bars });
                        }
                    }
                    let warnings = session.warnings().join("; ");
                    if !warnings.is_empty() {
                        self.status = Some(format!("Opened with differences: {warnings}"));
                    }
                    // A lane for each Color FX type of each deck.
                    session.split_color_fx();
                    // And the Beat FX depth one, with curves.
                    session.split_fx_depth();
                    let mut editor = MixEditor::new(path, session);
                    editor.cues = library_cues(&self.collection, editor.tracks.iter().map(|t| &t.path));
                    editor.outlines = outlines;
                    self.edit_view.editor = Some(editor);
                }
                Err(err) => self.status = Some(format!("Could not open the session: {err:#}")),
            }
        }
        let Some(editor) = self.edit_view.editor.as_mut() else { return };
        if let Some(rx) = &editor.resync {
            match rx.try_recv() {
                Ok(result) => {
                    editor.resync = None;
                    match result {
                        Ok(_) => {
                            editor.synced();
                            editor.played_stale = true;
                        }
                        Err(err) => self.status = Some(format!("Not synced again: {err:#}")),
                    }
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        editor.update_played(ctx);
        if let Some(export) = &editor.export {
            match export.try_recv() {
                Ok((result, dir)) => {
                    editor.export = None;
                    editor.note = None;
                    match result {
                        Ok(note) => {
                            self.status = Some(note);
                            super::collection_view::reveal(&dir);
                        }
                        Err(err) => self.status = Some(format!("Export failed: {err:#}")),
                    }
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(200)),
            }
        }
        if let Some(render) = &editor.render {
            match render.done.try_recv() {
                Ok(result) => {
                    self.status = Some(result.unwrap_or_else(|e| format!("Render failed: {e:#}")));
                    editor.render = None;
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(200)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deck heard from `from` to `to` (frames), beats every `beat`
    /// frames from `first`, a bar every 4.
    fn deck(from: u64, to: u64, first: u64, beat: u64) -> deckhand_audio::Played {
        let levels = (from..to).step_by(512).map(|at| (at, 0.5, 0.5)).collect::<Vec<_>>();
        let lows = vec![0.0; levels.len()];
        let beats = (0..).map(|i| (first + i * beat, i % 4 == 0)).take_while(|b| b.0 < to).collect();
        deckhand_audio::Played { levels, lows, beats, places: Vec::new() }
    }

    #[test]
    fn master_grid_follows_the_deck_heard_and_fills_gaps() {
        let rate = 44_100.0;
        let beat = 15_120; // 175 BPM
        // Deck 1 alone for 30 s, then deck 2 alone (on its own grid) after
        // 5 s of nothing.
        let one = deck(0, 1_323_000, 1_000, beat);
        let two = deck(1_543_500, 2_646_000, 1_550_000, beat);
        let (grid, usual) = master_grid(&[one, two], &[], rate, 2_646_000);
        assert_eq!(usual, beat);
        assert!(grid.windows(2).all(|w| w[0].at < w[1].at));
        let first_two = grid.iter().position(|g| g.deck == 1).unwrap();
        assert!(grid[..first_two].iter().all(|g| g.deck == 0));
        // The silence is bridged at deck 1's pace, then deck 2's beats.
        assert_eq!(grid[first_two].at, 1_550_000);
        assert!(grid[..first_two].windows(2).all(|w| w[1].at - w[0].at == beat));
        // Bars counted from 1, every 4 beats while deck 1 leads.
        assert_eq!((grid[0].bar, grid[0].number), (true, 1));
        assert_eq!((grid[4].bar, grid[4].number), (true, 2));
        let tempo = grid_tempo(&grid, rate);
        assert!(tempo.iter().all(|t| (t.1 - 175.0).abs() < 0.01), "{tempo:?}");
    }

    #[test]
    fn typed_values_in_real_units() {
        let bpm = Unit::Bpm(172.0);
        let ratio = bpm.read("174").unwrap();
        assert!((ratio - 174.0 / 172.0).abs() < 1e-12);
        assert_eq!(bpm.show(ratio), "174.00");
        assert!((Unit::TempoPercent.read("+1.5").unwrap() - 1.015).abs() < 1e-12);
        assert!((Unit::TrimDb.read("-6").unwrap() - 0.501_187).abs() < 1e-6);
        assert_eq!(Unit::EqDb.read("-26").unwrap(), -26.0);
        assert_eq!(Unit::Percent.read("45%").unwrap(), 0.45);
        assert_eq!(Unit::Percent.read("12,5").unwrap(), 0.125);
        assert_eq!(Unit::Percent.read("abc"), None);
    }

    /// A clip's start BPM handle on `DECKHAND_SESSION` (in memory): raised,
    /// the clip ramps from it and what follows the clip keeps its tempo:
    /// `cargo test -p deckhand-app clip_bpm_handle -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn clip_bpm_handle() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let session = Session::read(&path, deckhand_audio::Files).unwrap();
        let mut editor = MixEditor::new(path, session);
        let span = editor.tracks.iter().find(|t| !t.playing.is_empty() && t.to < editor.length).unwrap().clone();
        let (from, to) = editor.visible(&span)[0];
        let setting = Setting::Tempo(span.deck);
        let after = editor.value_at(setting, to);
        let before = editor.lanes[&setting].clone();
        let handle = TempoHandle { from, to, end: false, pos: Pos2::ZERO, bpm: None };
        let index = editor.pin_clip_tempo(setting, handle);
        assert_eq!(editor.lanes[&setting][index].at, from);
        let raised = editor.lanes[&setting][index].value * 1.02;
        editor.lanes.get_mut(&setting).unwrap()[index].value = raised;
        editor.commit(setting, before, "move point");
        let now = |at: u64| editor.value_at(setting, at);
        println!("clip {from}..{to}: start {:.4} → mid {:.4} → end {:.4}; after {:.4} (was {after:.4})", now(from), now((from + to) / 2), now(to - 4096), now(to));
        assert!((now(from) - raised).abs() < 1e-6, "the start raised");
        assert!((now(to) - after).abs() < 1e-6, "what follows as it was");
        let session = editor.session.lock().unwrap();
        assert!((session.lane(setting).iter().find(|p| p.at >= to).map_or(after, |p| p.value) - after).abs() < 1e-6);
    }

    /// How much faster than real time `DECKHAND_SESSION` plays offline:
    /// listening from a cursor plays everything before it first (read only).
    /// `cargo test -p deckhand-app --release render_speed -- --ignored --nocapture`
    /// A new effect on `DECKHAND_SESSION` is flat at the depth chosen, with
    /// a handle at each edge, and its depth goes along when it moves:
    /// `cargo test -p deckhand-app new_effect_is_flat -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn new_effect_is_flat() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let mut session = Session::read(&path, super::super::recording::Sources).unwrap();
        session.split_fx_depth();
        let mut editor = MixEditor::new(path, session);
        let at = editor.length / 3;
        let add = AddFx { at, pos: Pos2::ZERO, ..editor.last_fx };
        editor.add_fx(AddFx { depth: 0.75, kind: BeatFxKind::Reverb, ..add });
        let span = fx_spans(&editor.fx, editor.length)
            .into_iter()
            // Its start moved to a buffer start.
            .find(|s| s.from.abs_diff(at) < 4096)
            .expect("the new effect");
        let end = editor.clip_end(&span);
        let lane = editor.lanes[&Setting::BeatFxDepth].clone();
        let edges: Vec<_> =
            lane.iter().filter(|p| p.at == span.from || p.at == end).map(|p| p.value).collect();
        assert_eq!(edges, [0.75, 0.75], "{lane:?}");
        for t in [span.from, (span.from + end) / 2, end] {
            assert!((lane_value(&lane, t) - 0.75).abs() < 1e-9, "flat at {t}");
        }
        // Moved two seconds on: the depth goes with it.
        let by = (2.0 * editor.rate) as u64;
        let old = (span.from, end);
        let changes = move_fx(&editor.fx, &span, span.from + by, span.to + by);
        editor.set_fx(changes, "move");
        editor.move_clip_depth(old, span.from + by, end + by);
        let lane = &editor.lanes[&Setting::BeatFxDepth];
        assert!((lane_value(lane, span.from + by + 10) - 0.75).abs() < 1e-9, "{lane:?}");
        println!("ok: {} depth points", lane.len());
    }

    #[test]
    #[ignore]
    fn render_speed() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let session = Session::read(&path, super::super::recording::Sources).unwrap();
        let started = std::time::Instant::now();
        let frames = session.render(|_| Ok(())).unwrap();
        let secs = frames as f64 / f64::from(session.sample_rate());
        let took = started.elapsed().as_secs_f64();
        println!("{secs:.0} s of set in {took:.1} s: {:.0}× real time", secs / took);
    }

    /// Giving `DECKHAND_SESSION` a lane per Color FX type changes nothing
    /// heard (read only):
    /// `cargo test -p deckhand-app --release color_lanes_sound_the_same -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn color_lanes_sound_the_same() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let mut session = Session::read(&path, super::super::recording::Sources).unwrap();
        let mut before = Vec::new();
        session.render(|b| Ok(before.extend_from_slice(b))).unwrap();
        println!("split: {}", session.split_color_fx());
        println!("depth lane: {}", session.split_fx_depth());
        let mut after = Vec::new();
        session.render(|b| Ok(after.extend_from_slice(b))).unwrap();
        assert_eq!(before.len(), after.len());
        let worst = before.iter().zip(&after).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        println!("largest difference: {worst}");
        assert!(worst < 1e-4, "{worst}");
    }

    /// Dragging a track on `DECKHAND_SESSION` 100 points moves its audio
    /// as far as the pointer went (read only):
    /// `cargo test -p deckhand-app --release dragging_a_track -- --ignored`
    #[test]
    #[ignore]
    fn dragging_a_track_moves_it_with_the_pointer() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let session = Session::read(&path, super::super::recording::Sources).unwrap();
        let mut editor = MixEditor::new(path, session);
        let ctx = egui::Context::default();
        // Deck 1's row, right of the names; ⌥ held: the audio slides (a
        // plain drag would move just the play under the pointer).
        let (y, from, to) = (60.0, 500.0, 600.0);
        let button = |x: f32, pressed| egui::Event::PointerButton {
            pos: pos2(x, y),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::ALT,
        };
        let mut steps =
            vec![vec![egui::Event::PointerMoved(pos2(from, y))], vec![button(from, true)]];
        for i in 1..=10 {
            steps.push(vec![egui::Event::PointerMoved(pos2(
                from + (to - from) * i as f32 / 10.0,
                y,
            ))]);
        }
        steps.push(vec![button(to, false)]);
        steps.push(Vec::new());
        let mut area_width = 0.0;
        for (i, events) in steps.into_iter().enumerate() {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
                time: Some(i as f64 * 0.1),
                modifiers: egui::Modifiers::ALT,
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    area_width = ui.available_width() - LABELS;
                    editor.timeline(ui);
                });
            });
        }
        assert_eq!(editor.moves.len(), 1, "{:?}", editor.moves);
        let moved = editor.moves[0];
        assert_eq!(moved.deck, 0);
        let tempo = editor.value_at(Setting::Tempo(0), moved.at);
        let expected = f64::from(to - from) * editor.view_secs / f64::from(area_width) * tempo;
        assert!((moved.later - expected).abs() < expected * 0.02, "{} vs {expected}", moved.later);
        assert!(editor.preview.is_some_and(|p| p.frames > 0));
        // Undone and done again.
        editor.undo();
        assert!(editor.moves.is_empty());
        editor.redo();
        assert_eq!(editor.moves, [moved]);
        assert_eq!(editor.undo.len(), 1);
        assert!(editor.undo[0].label.starts_with("Deck 1 track +"), "{}", editor.undo[0].label);
    }

    /// Drags `editor`'s timeline from `from` to `to` at height `y`.
    fn drag(editor: &mut MixEditor, y: f32, from: f32, to: f32) -> f32 {
        let ctx = egui::Context::default();
        let button = |x: f32, pressed| egui::Event::PointerButton {
            pos: pos2(x, y),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut steps =
            vec![vec![egui::Event::PointerMoved(pos2(from, y))], vec![button(from, true)]];
        for i in 1..=10 {
            steps.push(vec![egui::Event::PointerMoved(pos2(
                from + (to - from) * i as f32 / 10.0,
                y,
            ))]);
        }
        steps.push(vec![button(to, false)]);
        steps.push(Vec::new());
        let mut area_width = 0.0;
        for (i, events) in steps.into_iter().enumerate() {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
                time: Some(i as f64 * 0.1),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    area_width = ui.available_width() - LABELS;
                    editor.timeline(ui);
                });
            });
        }
        area_width
    }

    /// Dragging the second clip on deck 1 of `DECKHAND_SESSION` by its
    /// title moves it whole; undo puts it back (read only):
    /// `cargo test -p deckhand-app --release dragging_a_clip -- --ignored`
    #[test]
    #[ignore]
    fn dragging_a_clip_by_its_title_moves_it() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let session = Session::read(&path, super::super::recording::Sources).unwrap();
        let mut editor = MixEditor::new(path, session);
        let clips = |e: &MixEditor| {
            e.tracks.iter().filter(|t| t.deck == 0).map(|t| t.from).collect::<Vec<_>>()
        };
        let before = clips(&editor);
        assert_eq!(before.len(), 2);
        // The title strip of deck 1's second clip, about 2:00 in.
        let width = drag(&mut editor, 42.0, 560.0, 660.0);
        let after = clips(&editor);
        let expected = 100.0 * editor.view_secs / f64::from(width) * editor.rate;
        let moved = after[1] as f64 - before[1] as f64;
        assert!((moved - expected).abs() < 1024.0, "{moved} vs {expected}");
        assert!(editor.undo[0].label.starts_with("Deck 1 clip +"), "{}", editor.undo[0].label);
        editor.undo();
        assert_eq!(clips(&editor), before);
        editor.redo();
        assert_eq!(clips(&editor), after);
    }

    /// On `DECKHAND_SESSION`: dragging one of several plays of deck 1's
    /// clip moves just that play; the others stay (read only):
    /// `cargo test -p deckhand-app --release dragging_one_play -- --ignored`
    #[test]
    #[ignore]
    fn dragging_one_play_moves_just_it() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let session = Session::read(&path, super::super::recording::Sources).unwrap();
        let mut editor = MixEditor::new(path, session);
        let Some(span) = editor.tracks.iter().find(|t| t.deck == 0 && t.playing.len() > 1).cloned()
        else {
            println!("no clip with several plays on deck 1 here");
            return;
        };
        let area = Rect::from_min_max(pos2(8.0 + LABELS, 0.0), pos2(1192.0, 10.0));
        // The widest play with room after it (a play stays inside its
        // clip), so the pointer lands on it.
        let room = (2.0 * editor.rate) as u64;
        let Some(&(from, to)) =
            span.playing.iter().filter(|p| p.1 + room < span.to).max_by_key(|p| p.1 - p.0)
        else {
            println!("no play with room to move");
            return;
        };
        let x = (editor.x_of(area, from) + editor.x_of(area, to)) / 2.0;
        drag(&mut editor, 70.0, x, x + 40.0);
        let label = editor.undo.last().map(|c| c.label.clone()).unwrap_or_default();
        assert_eq!(label, "Deck 1 move selection");
        // The deck replays it at its new place and goes back after it: no
        // lasting move of the rest.
        let later: f64 = editor.moves.iter().filter(|m| m.deck == 0).map(|m| m.later).sum();
        assert!(later.abs() < 1e-6, "the rest moved: {:?}", editor.moves);
        let others: Vec<_> = editor
            .tracks
            .iter()
            .find(|t| t.deck == 0 && t.from == span.from)
            .unwrap()
            .playing
            .iter()
            .filter(|p| p.1 <= from || p.0 >= to)
            .copied()
            .collect();
        let before: Vec<_> =
            span.playing.iter().filter(|p| p.1 <= from || p.0 >= to).copied().collect();
        assert_eq!(others, before, "the other plays stay where they were");
    }

    /// On `DECKHAND_SESSION`: two of deck 1's plays picked (as with
    /// Cmd/Ctrl-click) move together, in one undo (read only):
    /// `cargo test -p deckhand-app --release two_plays -- --ignored`
    #[test]
    #[ignore]
    fn two_plays_move_together() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let session = Session::read(&path, super::super::recording::Sources).unwrap();
        let mut editor = MixEditor::new(path, session);
        let room = (3.0 * editor.rate) as u64;
        let Some(span) = editor.tracks.iter().find(|t| t.deck == 0 && t.playing.len() > 2).cloned()
        else {
            println!("no clip with several plays on deck 1 here");
            return;
        };
        let plays: Vec<(u64, u64)> = span
            .playing
            .iter()
            .filter(|p| p.0 > span.from + room && p.1 + room < span.to)
            .take(2)
            .copied()
            .collect();
        if plays.len() < 2 {
            println!("no two plays with room here");
            return;
        }
        for &(from, to) in &plays {
            editor.toggle_pick(Picked { deck: 0, from, to, whole: false });
        }
        assert_eq!(editor.selected().len(), 2);
        // As far as a drag lets them (up to a second): plays stay between
        // their neighbours.
        let by = editor.drag_limits().1.min(editor.rate as i64);
        assert!(by > 0, "no room to move");
        let by = by / 512 * 512;
        editor.move_selection(by);
        assert_eq!(editor.undo.len(), 1);
        assert_eq!(editor.undo[0].label, "Move 2 selected");
        // Both heard a second later, as selected.
        let selected = editor.selected();
        for &(from, to) in &plays {
            let new = (shift(from, by), shift(to, by));
            assert!(
                selected
                    .iter()
                    .any(|p| p.from.abs_diff(new.0) < 2048 && p.to.abs_diff(new.1) < 2048),
                "{new:?} not in {selected:?}"
            );
            let middle = (new.0 + new.1) / 2;
            assert!(editor.heard_at(0, middle), "heard at its new place");
        }
        editor.undo();
        assert_eq!(editor.span_pieces(&span), vec![(span.from, span.to)]);
    }

    /// Clicks on `editor`'s timeline at `pos`: twice for a double click,
    /// with the secondary button for a right click.
    fn click(editor: &mut MixEditor, pos: Pos2, button: egui::PointerButton, times: usize) {
        let ctx = egui::Context::default();
        let press = |pressed| egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut steps = vec![vec![egui::Event::PointerMoved(pos)]];
        for _ in 0..times {
            steps.push(vec![press(true)]);
            steps.push(vec![press(false)]);
        }
        steps.push(Vec::new());
        for (i, events) in steps.into_iter().enumerate() {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
                time: Some(i as f64 * 0.05),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| editor.timeline(ui));
            });
        }
    }

    /// On `DECKHAND_SESSION` (made for the user's 2026-11-05 10-09-47 set,
    /// whose clips sit where the clicks land): deck 1's first clip shrunk
    /// by its edge, the second split by a double click and the play under a
    /// right click cut out; undone again (read only):
    /// `cargo test -p deckhand-app --release shrinking_and_cutting -- --ignored`
    #[test]
    #[ignore]
    fn shrinking_and_cutting_clips() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let session = Session::read(&path, super::super::recording::Sources).unwrap();
        let mut editor = MixEditor::new(path, session);
        let spans: Vec<TrackSpan> = editor.tracks.iter().filter(|t| t.deck == 0).cloned().collect();
        let x_of = |e: &MixEditor, at: u64| {
            let area = Rect::from_min_max(pos2(8.0 + LABELS, 0.0), pos2(1192.0, 10.0));
            e.x_of(area, at)
        };
        // The first clip's right edge, below the title strip.
        let edge = x_of(&editor, spans[0].to);
        drag(&mut editor, 70.0, edge, edge - 80.0);
        let first = editor.span_pieces(&spans[0]);
        assert_eq!(first.len(), 1);
        assert!(first[0].1 < spans[0].to - editor.rate as u64 * 10, "{first:?}");
        assert_eq!(editor.undo.last().unwrap().label, "Deck 1 shrink");

        let middle = x_of(&editor, (spans[1].from + spans[1].to) / 2);
        click(&mut editor, pos2(middle, 70.0), egui::PointerButton::Primary, 2);
        assert_eq!(editor.span_pieces(&spans[1]).len(), 2, "{:?}", editor.span_pieces(&spans[1]));
        click(&mut editor, pos2(middle + 100.0, 70.0), egui::PointerButton::Secondary, 1);
        let second = editor.span_pieces(&spans[1]);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].0, spans[1].from);
        let labels: Vec<_> = editor.undo.iter().map(|c| c.label.as_str()).collect();
        // The right click was on a play: just that play went.
        assert_eq!(labels, ["Deck 1 shrink", "Deck 1 split", "Deck 1 cut out play"]);
        for _ in 0..3 {
            editor.undo();
        }
        assert_eq!(editor.span_pieces(&spans[0]), [(spans[0].from, spans[0].to)]);
        assert_eq!(editor.span_pieces(&spans[1]), [(spans[1].from, spans[1].to)]);
    }
}
