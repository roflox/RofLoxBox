//! EDIT mode: a recorded set as a little DAW. The tracks on each deck over
//! time, and a lane of automation for each knob and fader, to change and
//! play into a new WAV.

use std::collections::{BTreeMap, HashMap};
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

/// The range marked on the ruler.
const RANGE: Color32 = Color32::from_rgb(90, 160, 255);
/// The playhead.
const PLAY: Color32 = Color32::from_rgb(255, 200, 60);
/// Width of the names left of the timeline.
const LABELS: f32 = 150.0;
const RULER: f32 = 22.0;
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
type Opening = (String, Receiver<Result<(PathBuf, Session)>>);

/// EDIT mode's state: the session being edited, or being opened.
#[derive(Default)]
pub(super) struct EditView {
    editor: Option<MixEditor>,
    opening: Option<Opening>,
    /// `DECKHAND_EDIT_SESSION` was opened (a test aid).
    opened_at_start: bool,
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
    /// The shape of new points, and of selected ones when chosen.
    shape: Curve,
    /// When each deck was synced to which (from the recording).
    sync: Vec<SyncSpan>,
    /// Synced decks follow tempo edits (sync is done again).
    follow: bool,
    resync: Option<Receiver<Result<usize>>>,
    /// The tracks moved, one being dragged, and the one ← and → move.
    moves: Vec<TrackMove>,
    move_drag: Option<MoveDrag>,
    chosen_move: Option<(usize, u64)>,
    /// Each track's BPM, by file.
    bpms: HashMap<PathBuf, f64>,
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
    /// Where the pointer is on the timeline, for the values.
    hover: Option<u64>,
    /// What each deck played (levels and beats), being worked out again
    /// after edits.
    played: Vec<deckhand_audio::Played>,
    playing_out: Option<Receiver<Result<Vec<deckhand_audio::Played>>>>,
    played_stale: bool,
    /// Groups folded away: a deck's lanes, the mixer's (`None`), the
    /// effects ([`EFFECTS`]).
    collapsed: std::collections::HashSet<Option<usize>>,
    /// A move shown before the waveforms catch up, and the working outs
    /// started (the one running is the last).
    preview: Option<Preview>,
    jobs: u64,
    /// Where two decks' low end went into the mix together, and whether
    /// to show it.
    clashes: Vec<deckhand_audio::Clash>,
    show_clashes: bool,
    /// Each deck's Master Tempo: on or off from when.
    key_locks: Vec<Vec<(u64, bool)>>,
    clip_drag: Option<ClipDrag>,
    /// The pieces heard of each clip (by deck and where it starts), and an
    /// edge being dragged.
    pieces: HashMap<(usize, u64), Vec<(u64, u64)>>,
    edge_drag: Option<EdgeDrag>,
    /// A message for the status line.
    note: Option<String>,
}

/// The effects' group in [`MixEditor::collapsed`].
const EFFECTS: Option<usize> = Some(usize::MAX);

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
    Point { setting: Setting, index: usize, before: Vec<Point>, range: (f64, f64) },
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
            shape: Curve::Line,
            sync,
            follow: true,
            resync: None,
            moves,
            move_drag: None,
            chosen_move: None,
            bpms,
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
            fx_selection: None,
            range_from: 0,
            sel_drag: None,
            panning: false,
            range: None,
            range_start: None,
            range_side: None,
            fx_drag: None,
            hover: None,
            played: Vec::new(),
            playing_out: None,
            played_stale: true,
            collapsed: Default::default(),
            preview: None,
            jobs: 0,
            clashes: Vec::new(),
            show_clashes: false,
            key_locks,
            clip_drag: None,
            pieces,
            edge_drag: None,
            note: None,
        }
    }

    fn secs(&self) -> f64 {
        self.length as f64 / self.rate
    }

    fn x_of(&self, rect: Rect, at: u64) -> f32 {
        let t = at as f64 / self.rate;
        rect.left() + ((t - self.view_from) / self.view_secs * f64::from(rect.width())) as f32
    }

    fn at_of(&self, rect: Rect, x: f32) -> u64 {
        let t = self.view_from + f64::from((x - rect.left()) / rect.width()) * self.view_secs;
        ((t.max(0.0) * self.rate) as u64).min(self.length)
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
        self.follow_tempo(setting);
    }

    /// Keeps a change to undo (the last [`HISTORY`]); a new one ends what
    /// could be redone.
    fn record(&mut self, label: String, edit: Edit) {
        self.undo.push(Change { label, edit });
        if self.undo.len() > HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.edited = true;
        self.played_stale = true;
        self.replay_edit = true;
    }

    /// Goes to `edit`'s state; returns the edit that comes back.
    fn apply(&mut self, edit: Edit) -> Option<Edit> {
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
        self.played_stale = true;
        self.replay_edit = true;
        Some(back)
    }

    /// Takes in everything from the session again (after events changed).
    fn refresh(&mut self, session: &Session) {
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

    /// Works out again what the decks played, in the background, on a copy
    /// of the session (edits go on meanwhile).
    fn update_played(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.playing_out {
            match rx.try_recv() {
                Ok(result) => {
                    self.playing_out = None;
                    if let Ok(played) = result {
                        // Half a second of both at least.
                        self.clashes = deckhand_audio::clashes(&played, (self.rate * 0.5) as u64);
                        self.played = played;
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
        if !self.played_stale || self.resync.is_some() {
            return;
        }
        let Some(copy) = self.session.try_lock().ok().and_then(|s| s.duplicate().ok()) else {
            return;
        };
        self.played_stale = false;
        self.jobs += 1;
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(copy.played());
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
        if secs > self.view_from + self.view_secs || secs < self.view_from {
            self.view_from = secs - self.view_secs * 0.05;
            self.clamp_view();
        }
        ctx.request_repaint();
    }

    /// The cursor (where playing starts) and the playhead, down `area`.
    fn play_marks(&self, painter: &egui::Painter, area: Rect) {
        if let Some((from, to)) = self.range {
            let band =
                Rect::from_x_y_ranges(self.x_of(area, from)..=self.x_of(area, to), area.y_range());
            painter.rect_filled(band, 0.0, RANGE.gamma_multiply(0.12));
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
                .on_hover_text("Add or remove lanes");
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
        let secs = (self.view_secs / factor).clamp(0.5, self.secs().max(1.0) * 1.2);
        self.view_from = pivot - (pivot - self.view_from) * secs / self.view_secs;
        self.view_secs = secs;
        self.clamp_view();
    }

    fn clamp_view(&mut self) {
        let max = (self.secs() - self.view_secs * 0.5).max(0.0);
        self.view_from = self.view_from.clamp(-self.view_secs * 0.05, max);
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let playing = self.player.is_some();
        let label = if playing { "⏹ Stop" } else { "▶ Play" };
        if ui
            .add(egui::Button::new(label).selected(playing))
            .on_hover_text("Plays the set as edited from the cursor (Space). Click the ruler to put the cursor somewhere")
            .clicked()
        {
            self.toggle_play();
        }
        let at = self.playhead().unwrap_or(self.cursor) as f64 / self.rate;
        ui.label(RichText::new(format!("{}:{:05.2}", (at / 60.0) as u64, at % 60.0)).monospace());
        if self.player.as_ref().is_some_and(|p| !p.ready()) {
            ui.spinner().on_hover_text("Getting to the cursor…");
        }
        ui.separator();
        for (tool, label, hint) in [
            (Tool::Normal, "Normal", "Move and edit clips, points and effects"),
            (
                Tool::Select,
                "Select",
                "Drag a stretch of a deck's clips, the Beat FX or a lane; Delete removes it",
            ),
        ] {
            let keys = &self.tool_keys;
            let key = match tool {
                Tool::Normal => &keys[0],
                Tool::Select => &keys[1],
            };
            let hint = if key.is_empty() { hint.to_owned() } else { format!("{hint} ({key})") };
            if tool_button(ui, tool, label, self.tool == tool).on_hover_text(hint).clicked() {
                self.tool = tool;
            }
        }
        ui.separator();
        ui.label(RichText::new(&self.name).strong());
        let secs = self.secs() as u64;
        ui.label(RichText::new(format!("{}:{:02}", secs / 60, secs % 60)).weak());
        ui.separator();
        let idle = !self.busy();
        let undo = self.undo.last().map(|c| format!("Undo {} (⌘Z)", c.label));
        if ui
            .add_enabled(idle && undo.is_some(), egui::Button::new("Undo"))
            .on_hover_text(undo.unwrap_or_default())
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
            .on_hover_text("Holds the value from before the selection through it (Delete joins its sides in a straight line instead)")
            .clicked()
        {
            self.flatten();
        }
        if ui
            .add_enabled(selected, egui::Button::new("Ramp"))
            .on_hover_text("One line (or S-curve, with Smooth chosen) through the selection")
            .clicked()
        {
            self.ramp();
        }
        ui.separator();
        for (curve, label, hint) in [
            (Curve::Step, "Step", "Holds, then jumps: as knobs are recorded"),
            (Curve::Line, "Line", "A straight line to the next point"),
            (Curve::Smooth, "Smooth", "Eases out of one value and into the next"),
        ] {
            let hint = format!("{hint}. New points get it; selected ones too");
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
            "Marks where two decks' low end (bass, kick, below 150 Hz) goes into the mix at \
             the same time, as heard: after EQ, faders and crossfader",
        );
        if !self.sync.is_empty() {
            ui.separator();
            ui.checkbox(&mut self.follow, "Synced decks follow").on_hover_text(
                "After a tempo change, sync is done again: synced decks take the master's \
                 tempo and stay on its beat",
            );
        }
        ui.separator();
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
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
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
        self.add_fx_dialog(ui.ctx());
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
                [pos2(x, area.bottom() - 6.0), pos2(x, area.bottom())],
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
        self.play_marks(&painter, area);
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
            let pieces = self.span_pieces(span);
            // Split: the piece a drag would move.
            let hovered = response
                .hover_pos()
                .filter(|p| area.contains(*p) && p.y > area.top() + 16.0 && pieces.len() > 1)
                .map(|p| self.at_of(area, p.x));
            if pieces != [(span.from, span.to)] {
                painter.rect_stroke(
                    block,
                    3.0,
                    Stroke::new(1.0_f32, color.gamma_multiply(0.5)),
                    egui::StrokeKind::Inside,
                );
            }
            for (i, &(from, to)) in pieces.iter().enumerate() {
                let end = if to == span.to { end } else { shifted(to, by) };
                let piece = Rect::from_x_y_ranges(
                    self.x_of(area, shifted(from, by))..=self.x_of(area, end),
                    block.y_range(),
                );
                painter.rect_filled(piece, 3.0, color.gamma_multiply(0.25));
                self.waveform(&painter, area, piece.intersect(area), deck, color);
                if hovered.is_some_and(|at| from <= at && at < to) {
                    painter.rect_stroke(
                        piece,
                        3.0,
                        Stroke::new(1.5_f32, color),
                        egui::StrokeKind::Inside,
                    );
                }
                // Where it was split.
                if i > 0 && pieces[i - 1].1 == from {
                    painter.line_segment(
                        [piece.left_top(), piece.left_bottom()],
                        Stroke::new(1.5_f32, Color32::WHITE.gamma_multiply(0.7)),
                    );
                }
            }
            for &(from, to) in &span.playing {
                let playing = Rect::from_x_y_ranges(
                    self.x_of(area, from)..=self.x_of(area, to),
                    block.bottom() - 4.0..=block.bottom(),
                );
                painter.rect_filled(playing, 0.0, color);
            }
            // Its low end clashing with another deck's.
            if self.show_clashes {
                for clash in self.clashes.iter().filter(|c| c.decks.0 == deck || c.decks.1 == deck)
                {
                    let (from, to) = (clash.from.max(span.from), clash.to.min(span.to));
                    if from < to {
                        let mark = Rect::from_x_y_ranges(
                            self.x_of(area, from)..=self.x_of(area, to),
                            block.y_range(),
                        );
                        painter.rect_filled(mark, 0.0, CLASH.gamma_multiply(0.25));
                    }
                }
            }
            // Synced: a line in the master's colour along the top.
            for sync in self.sync.iter().filter(|s| s.deck == deck) {
                let (from, to) = (sync.from.max(span.from), sync.to.min(span.to));
                if from < to {
                    let line = Rect::from_x_y_ranges(
                        self.x_of(area, from)..=self.x_of(area, to),
                        block.top()..=block.top() + 2.0,
                    );
                    painter.rect_filled(line, 0.0, DECK_COLORS[sync.master % DECK_COLORS.len()]);
                }
            }
            let mut name = span.path.file_stem().map_or_else(
                || "(not from a file)".to_owned(),
                |n| n.to_string_lossy().into_owned(),
            );
            if let Some(bpm) = self.bpms.get(&span.path) {
                name.push_str(&format!("  ·  {bpm:.1} BPM"));
            }
            let text = ui.painter().layout_no_wrap(
                name,
                egui::FontId::proportional(12.0),
                ui.visuals().strong_text_color(),
            );
            let clip = block.intersect(area).shrink2(vec2(4.0, 0.0));
            ui.painter().with_clip_rect(clip).galley(
                pos2(clip.left(), block.top() + 3.0),
                text,
                Color32::WHITE,
            );
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
        if let Some((d, from, to)) = self.clip_selection
            && d == deck
        {
            let selected =
                Rect::from_x_y_ranges(self.x_of(area, from)..=self.x_of(area, to), area.y_range());
            painter.rect_filled(selected, 0.0, ui.visuals().selection.bg_fill.gamma_multiply(0.45));
        }
        if self.busy() || self.panning || pointer.is_none() && self.move_drag.is_none() {
            return;
        }
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
        let selected_x = self
            .clip_selection
            .filter(|s| s.0 == deck)
            .map(|(_, from, to)| (self.x_of(area, from), self.x_of(area, to)));
        let in_selection =
            |pos: Pos2| selected_x.is_some_and(|(left, right)| pos.x >= left && pos.x <= right);
        let origin = ui.input(|i| i.pointer.press_origin()).filter(|p| p.x >= area.left());
        if response.drag_started() {
            match origin {
                Some(pos) if in_selection(pos) && self.selection_clip().is_some() => {
                    self.sel_drag = Some(SelDrag { deck, x: pos.x, frames: 0 });
                }
                _ => self.clear_selections(),
            }
        }
        if (response.clicked() || response.secondary_clicked())
            && pointer.is_some_and(|p| !in_selection(p))
        {
            self.clear_selections();
        }
        if self.sel_drag.as_ref().is_some_and(|d| d.deck == deck) {
            if let Some(pos) = pointer
                && let Some((span, from, to)) = self.selection_clip()
                && let Some(x0) = self.sel_drag.as_ref().map(|d| d.x)
            {
                let secs = f64::from(pos.x - x0) * self.view_secs / f64::from(area.width());
                // Inside the clip: the deck plays this track only there.
                let low = span.from as i64 - from as i64;
                let high = span.to as i64 - to as i64;
                let frames = ((secs * self.rate).round() as i64).clamp(low, high.max(low));
                if let Some(drag) = self.sel_drag.as_mut() {
                    drag.frames = frames;
                }
                let shifted = |at: u64| (at as i64 + frames).max(0) as u64;
                let ghost = Rect::from_x_y_ranges(
                    self.x_of(area, shifted(from))..=self.x_of(area, shifted(to)),
                    area.y_range(),
                );
                painter.rect_filled(ghost, 2.0, color.gamma_multiply(0.45));
                painter.rect_stroke(
                    ghost,
                    2.0,
                    Stroke::new(1.5_f32, Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
                painter.text(
                    pos2(ghost.right() + 6.0, area.top() + 2.0),
                    egui::Align2::LEFT_TOP,
                    format!("{:+.2} s", frames as f64 / self.rate),
                    egui::FontId::monospace(12.0),
                    Color32::WHITE,
                );
            }
            if response.drag_stopped()
                && let Some(drag) = self.sel_drag.take()
            {
                self.move_selection(drag.frames);
            }
            return;
        }
        if response.secondary_clicked()
            && let Some(pos) = pointer
        {
            match near_mark(pos) {
                Some((deck, at)) => self.set_move(deck, at, 0.0),
                None => self.toggle_piece(deck, self.at_of(area, pos.x)),
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
        }
        // The title strip along the top: the whole clip.
        let on_title = |pos: Pos2| pos.y <= area.top() + 16.0;
        if let Some(pos) = response.hover_pos().filter(|p| p.x >= area.left() && on_title(*p)) {
            let over = self.at_of(area, pos.x);
            if self.tracks.iter().any(|t| t.deck == deck && t.from <= over && over < t.to) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        let pressed = ui.input(|i| i.pointer.press_origin()).filter(|p| p.x >= area.left());
        let pressed_edge =
            pressed.filter(|p| !on_title(*p)).and_then(|p| self.edge_at(area, deck, p.x));
        if response.drag_started()
            && let Some((span, index, right)) = pressed_edge
        {
            let pieces = self.span_pieces(&span);
            self.edge_drag = Some(EdgeDrag { span, index, right, pieces });
        } else if response.drag_started()
            && let Some(pos) = pressed
            && on_title(pos)
        {
            let grabbed = self.at_of(area, pos.x);
            let span =
                self.tracks.iter().find(|t| t.deck == deck && t.from <= grabbed && grabbed < t.to);
            if let Some(span) = span {
                self.clip_drag = Some(ClipDrag { deck, from: span.from, to: span.to, x: pos.x });
            }
        } else if response.drag_started()
            && let Some(pos) = pointer
        {
            let grabbed = self.at_of(area, pos.x);
            let span =
                self.tracks.iter().find(|t| t.deck == deck && t.from <= grabbed && grabbed < t.to);
            if let Some(span) = span {
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
            if response.drag_stopped()
                && let Some(drag) = self.edge_drag.take()
            {
                self.set_pieces(&drag.span, drag.pieces, "shrink");
            }
            return;
        }
        if let Some(&ClipDrag { deck: d, from, to, x: x0 }) = self.clip_drag.as_ref()
            && d == deck
        {
            if let Some(pos) = pointer {
                let secs = f64::from(pos.x - x0) * self.view_secs / f64::from(area.width());
                let frames = (secs * self.rate).round() as i64;
                self.preview =
                    Some(Preview { deck, at: from, to, frames, whole: true, until_job: u64::MAX });
                painter.text(
                    pos2(pos.x + 10.0, area.top() + 2.0),
                    egui::Align2::LEFT_TOP,
                    format!("{secs:+.2} s"),
                    egui::FontId::monospace(12.0),
                    Color32::WHITE,
                );
            }
            if response.drag_stopped() {
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
                let later = base + f64::from(pos.x - x0) * per_point * tempo;
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
            if response.drag_stopped()
                && let Some(drag) = self.move_drag.take()
            {
                self.set_move(drag.deck, drag.at, drag.later);
                self.chosen_move = (drag.later != 0.0).then_some((drag.deck, drag.at));
                // Shown moved until the waveforms are worked out with it.
                match self.preview.as_mut() {
                    Some(preview) if drag.later != drag.base => preview.until_job = self.jobs,
                    _ => self.preview = None,
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
        let beats: Vec<(u64, bool)> = played
            .beats
            .iter()
            .map(|&(at, bar)| (moved(at), bar))
            .filter(|&(at, _)| at >= from && at <= to)
            .collect();
        let room = block.width() / beats.len().max(1) as f32;
        // Zoomed out, bars alone; further out, neither (they would only
        // stripe the waveform).
        let bars_only = room < 8.0;
        if room * 4.0 < 16.0 {
            return;
        }
        for &(at, bar) in &beats {
            if bars_only && !bar {
                continue;
            }
            let x = self.x_of(area, at);
            let (alpha, width) = if bar { (0.85, 1.5_f32) } else { (0.35, 1.0) };
            painter.line_segment(
                [pos2(x, block.top() + 16.0), pos2(x, block.bottom() - 5.0)],
                Stroke::new(width, Color32::WHITE.gamma_multiply(alpha)),
            );
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

    /// The piece edge under `x` on `deck`'s row: its clip, which piece and
    /// whether it is the right edge.
    fn edge_at(&self, area: Rect, deck: usize, x: f32) -> Option<(TrackSpan, usize, bool)> {
        self.tracks.iter().filter(|t| t.deck == deck).find_map(|span| {
            self.span_pieces(span).iter().enumerate().find_map(|(i, &(from, to))| {
                if (self.x_of(area, to) - x).abs() <= GRAB {
                    Some((span.clone(), i, true))
                } else if (self.x_of(area, from) - x).abs() <= GRAB {
                    Some((span.clone(), i, false))
                } else {
                    None
                }
            })
        })
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

    /// Splits the piece of `deck`'s clip at `at` in two.
    fn split(&mut self, deck: usize, at: u64) {
        let Some(span) =
            self.tracks.iter().find(|t| t.deck == deck && t.from <= at && at < t.to).cloned()
        else {
            return;
        };
        let mut pieces = self.span_pieces(&span);
        let Some(i) = pieces.iter().position(|&(from, to)| from < at && at < to) else { return };
        let (from, to) = pieces[i];
        pieces.splice(i..=i, [(from, at), (at, to)]);
        self.set_pieces(&span, pieces, "split");
    }

    /// Takes the piece of `deck`'s clip at `at` out, or brings the gap
    /// there back.
    fn toggle_piece(&mut self, deck: usize, at: u64) {
        let Some(span) =
            self.tracks.iter().find(|t| t.deck == deck && t.from <= at && at < t.to).cloned()
        else {
            return;
        };
        let mut pieces = self.span_pieces(&span);
        match pieces.iter().position(|&(from, to)| from <= at && at < to) {
            Some(i) => {
                pieces.remove(i);
                self.set_pieces(&span, pieces, "cut out");
            }
            None => {
                let i = pieces.partition_point(|p| p.1 <= at);
                let from = i.checked_sub(1).map_or(span.from, |i| pieces[i].1);
                let to = pieces.get(i).map_or(span.to, |p| p.0);
                pieces.insert(i, (from, to));
                self.set_pieces(&span, pieces, "bring back");
            }
        }
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
                if let Some(preview) = self.preview.as_mut() {
                    preview.until_job = self.jobs;
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
        let span = self.tracks.iter().find(|t| t.deck == deck && t.from <= at && at < t.to)?;
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
        let near = |pos: Pos2| {
            dots.iter()
                .filter(|d| d.pos.distance(pos) <= GRAB + 2.0)
                .min_by(|a, b| a.pos.distance(pos).total_cmp(&b.pos.distance(pos)))
                .copied()
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
        if response.clicked() || response.secondary_clicked() {
            // One thing at a time: Delete takes the chosen effect away.
            self.clear_selections();
            self.chosen_fx = pointer.and_then(under);
        }
        // Double-click inside an effect: a depth point there (in the shape
        // chosen in the toolbar).
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
            let block = blocks[i];
            let grip = if (pos.x - block.left()).abs() <= GRAB {
                FxGrip::Start
            } else if (pos.x - block.right()).abs() <= GRAB {
                FxGrip::End
            } else {
                FxGrip::Whole
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
        if let Some(drag) = self.fx_drag.as_mut()
            && let Some(at) = pointer_at
            && let Some(span) = spans.get(drag.index)
        {
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
        let Some((deck, from, to)) = self.clip_selection else {
            self.note = Some("Select part of a clip first (Select tool, key 2)".into());
            return;
        };
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

    /// Lets go of every selection.
    fn clear_selections(&mut self) {
        self.chosen_fx = None;
        self.selection = None;
        self.clip_selection = None;
        self.fx_selection = None;
    }

    /// The clip the selected stretch lies in (all of it inside one), and
    /// the stretch.
    fn selection_clip(&self) -> Option<(TrackSpan, u64, u64)> {
        let (deck, from, to) = self.clip_selection?;
        let span = self.tracks.iter().find(|t| t.deck == deck && t.from <= from && to <= t.to)?;
        Some((span.clone(), from, to))
    }

    /// Moves the selected part of a clip by `frames`: it plays there
    /// instead of what was there (laid over it), and where it was is
    /// silent. The selection goes along.
    fn move_selection(&mut self, frames: i64) {
        let Some((span, from, to)) = self.selection_clip() else { return };
        if frames == 0 {
            return;
        }
        let deck = span.deck;
        let tempo = self.value_at(Setting::Tempo(deck), from).max(0.01);
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        let (new_from, new_to) = session.move_part(&span, from, to, frames, tempo);
        self.refresh(&session);
        drop(session);
        self.record(format!("Deck {} move selection", deck + 1), Edit::Events(before));
        self.clip_selection = Some((deck, new_from, new_to));
    }

    /// Clears the selections but one kind.
    fn select_only(&mut self, keep: Selected) {
        self.chosen_fx = None;
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
        if let Some((deck, from, to)) = self.clip_selection.take() {
            let spans: Vec<TrackSpan> = self
                .tracks
                .iter()
                .filter(|t| t.deck == deck && t.from < to && from < t.to)
                .cloned()
                .collect();
            for span in spans {
                let mut pieces = Vec::new();
                for (a, b) in self.span_pieces(&span) {
                    if b <= from || a >= to {
                        pieces.push((a, b));
                        continue;
                    }
                    if a < from {
                        pieces.push((a, from));
                    }
                    if b > to {
                        pieces.push((to, b));
                    }
                }
                self.set_pieces(&span, pieces, "delete");
            }
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
                        "Where both edges start; drag them apart afterwards (e.g. 90% to 30%). \
                         0%: the sound as it is · 100%: only the effect",
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
                .on_hover_text("The sound is cut and the echoes play out")
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
        .on_hover_text("0%: the sound as it is · 50%: half each · 100%: only the effect");
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
                    Some(bpm) => format!("{:.2} BPM {}{how}", bpm * value, setting.display(value)),
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
            && let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(pointer)
        {
            self.drag = Some(match near(pos).filter(|_| self.tool == Tool::Normal) {
                Some(index) => {
                    self.clear_selections();
                    let range = self.range(setting);
                    Drag::Point { setting, index, before: self.lanes[&setting].clone(), range }
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
                Some(Drag::Point { setting: s, index, range: (lo, hi), .. }) if s == setting => {
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
                        at: if index == 0 { 0 } else { at.clamp(low, high.max(low)) },
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
        if response.double_clicked()
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
                        .on_hover_text("The selection goes back to how it was recorded")
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
        "EDIT mode is new and not finished: expect rough edges and changes. Edits are saved as \
         new files; your recordings are never changed, but keep them.",
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
        ui.horizontal(|ui| {
            let opening = self.edit_view.opening.is_some();
            if ui.add_enabled(!opening, egui::Button::new("Open session…")).clicked()
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
            if ui.add_enabled(idle, egui::Button::new("Save edit…")).clicked() {
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
            if ui.add_enabled(idle, egui::Button::new(label)).on_hover_text(hint).clicked() {
                editor.start_render();
            }
            if let Some(render) = &editor.render {
                ui.spinner();
                let done = render.done_frames.load(Ordering::Relaxed);
                ui.label(format!("Rendering… {}%", done * 100 / render.total.max(1)));
            }
            if editor.resync.is_some() {
                ui.spinner();
                ui.label("Syncing again…");
            } else if editor.playing_out.is_some() && editor.played.is_empty() {
                ui.spinner();
                ui.label("Drawing the waveforms…");
            }
            if editor.edited {
                ui.label(RichText::new("edited").small().color(WARN));
            }
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
                    "Every recording keeps a .session file next to its WAV: the tracks, and \
                     every knob, fader, button and jog movement. Open one to change it and \
                     play it into a new WAV.",
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
        editor.timeline(ui);
    }

    fn open_session(&mut self, path: PathBuf, ctx: &egui::Context) {
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Session::read(&path, super::recording::Sources).map(|s| (path, s)));
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
                Ok((path, mut session)) => {
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
                    self.edit_view.editor = Some(MixEditor::new(path, session));
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
        // Deck 1's row, right of the names.
        let (y, from, to) = (60.0, 500.0, 600.0);
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

    /// On `DECKHAND_SESSION`: deck 1's first clip shrunk by its edge, the
    /// second split by a double click and its second piece cut out by a
    /// right click; undone again (read only):
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
        assert_eq!(labels, ["Deck 1 shrink", "Deck 1 split", "Deck 1 cut out"]);
        for _ in 0..3 {
            editor.undo();
        }
        assert_eq!(editor.span_pieces(&spans[0]), [(spans[0].from, spans[0].to)]);
        assert_eq!(editor.span_pieces(&spans[1]), [(spans[1].from, spans[1].to)]);
    }
}
