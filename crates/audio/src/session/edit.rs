//! A recording as edited: clips on each deck, lanes of automation, Beat FX
//! and markers, as a DAW keeps them.
//!
//! The recording (a [`Session`] file) is what the mixer was told, and is
//! never changed. Opening it to edit turns it into an [`Edit`]: each time a
//! deck played a stretch of its track becomes a [`Clip`] that says which
//! track, from where in it, and when in the set. What the DJ did to the
//! record while it played (jog, scratches, nudges, key changes) stays in
//! that clip as its [`Move`]s, so it sounds as it was, and goes with it
//! when the clip is cut. Jumps (hot cues, beat jumps) and loops start a
//! clip of their own.
//!
//! To be heard, an edit is made into commands again ([`Edit::to_session`])
//! and played through the same mixer as the recording: each clip loads its
//! track, goes to its place and plays, and nothing else on the deck is
//! there to get in its way.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::{
    Grid, JUMP_SPLIT_SECS, Logged, OPENED, Opened, PLACES_FOUND, Point, Run, STEMS, Session,
    Setting, Sources, StemLevels, StemsSource, TAIL_SECS, TRACKS, copy_value, copy_with, deck_of,
    lane_value, open_stems, open_track, source_of,
};
use crate::decode::TrackAudio;
use crate::engine::{Command, DECKS, Offline, Snapshot};
use crate::fx::BeatFxParams;
use crate::stems::{PARTS, StemLayers};

/// What the first line of an edit's file says it is.
const FORMAT: &str = "deckhand-edit";
const VERSION: u32 = 1;
/// A scratch let go of and grabbed again this soon (seconds) is one.
const SCRATCH_GAP_SECS: f64 = 0.5;
/// Stem levels as recorded.
const ALL_STEMS: [f32; PARTS] = [1.0; PARTS];

/// Whether the file at `path` is an edit (not a recording).
pub fn is_edit(path: &Path) -> bool {
    let mut head = [0u8; 256];
    let Ok(mut file) = File::open(path) else { return false };
    let n = file.read(&mut head).unwrap_or(0);
    String::from_utf8_lossy(&head[..n]).contains(FORMAT)
}

/// Where a recording is kept once its file holds an edit of it.
pub fn orig_path(session: &Path) -> PathBuf {
    let mut name = session.as_os_str().to_owned();
    name.push(".orig");
    PathBuf::from(name)
}

/// Something the DJ did to the record while a clip played, which plays
/// along with the clip and goes with it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "move", rename_all = "snake_case")]
pub enum Move {
    /// The jog wheel turned (steps of its resolution).
    Jog {
        steps: i16,
    },
    Hold {
        held: bool,
    },
    Settle,
    JogMove {
        seconds: f64,
    },
    Nudge {
        amount: f64,
    },
    KeyLock {
        on: bool,
    },
    KeyShift {
        semitones: f64,
    },
    Slip {
        on: bool,
    },
    SlipHold {
        held: bool,
    },
    VinylSpeed {
        start: f64,
        brake: f64,
    },
    SyncShift {
        seconds: f64,
    },
    SyncOffset {
        beats: f64,
    },
    /// A jump too soon after the clip began to be a clip of its own (the
    /// needle dragged).
    Seek {
        seconds: f64,
    },
    Shift {
        seconds: f64,
    },
    /// Play or stop, a loop set or let go of: in a clip's opening, as
    /// recorded.
    Play {
        playing: bool,
    },
    Loop {
        start: f64,
        end: f64,
    },
    ClearLoop,
}

impl Move {
    /// The move a deck's command is, with its deck.
    fn of(command: &Command) -> Option<(usize, Self)> {
        use Command as C;
        Some(match *command {
            C::Hold { deck, held } => (deck, Self::Hold { held }),
            C::Settle { deck } => (deck, Self::Settle),
            C::JogMove { deck, seconds } => (deck, Self::JogMove { seconds }),
            C::SetNudge { deck, amount } => (deck, Self::Nudge { amount }),
            C::SetKeyLock { deck, on } => (deck, Self::KeyLock { on }),
            C::SetKeyShift { deck, semitones } => (deck, Self::KeyShift { semitones }),
            C::SetSlip { deck, on } => (deck, Self::Slip { on }),
            C::SlipHold { deck, held } => (deck, Self::SlipHold { held }),
            C::SetVinylSpeed { deck, start, brake } => (deck, Self::VinylSpeed { start, brake }),
            C::SyncShift { deck, seconds } => (deck, Self::SyncShift { seconds }),
            C::SyncOffset { deck, beats } => (deck, Self::SyncOffset { beats }),
            C::Seek { deck, seconds } => (deck, Self::Seek { seconds }),
            C::Shift { deck, seconds } => (deck, Self::Shift { seconds }),
            C::MoveTrack { deck, later } => (deck, Self::Shift { seconds: -later }),
            C::SetPlaying { deck, playing } => (deck, Self::Play { playing }),
            C::SetLoop { deck, start, end } => (deck, Self::Loop { start, end }),
            C::ClearLoop { deck } => (deck, Self::ClearLoop),
            _ => return None,
        })
    }

    fn logged(self, deck: usize) -> Logged {
        use Command as C;
        Logged::Command(match self {
            Self::Jog { steps } => return Logged::Jog(deck as u8, steps),
            Self::Hold { held } => C::Hold { deck, held },
            Self::Settle => C::Settle { deck },
            Self::JogMove { seconds } => C::JogMove { deck, seconds },
            Self::Nudge { amount } => C::SetNudge { deck, amount },
            Self::KeyLock { on } => C::SetKeyLock { deck, on },
            Self::KeyShift { semitones } => C::SetKeyShift { deck, semitones },
            Self::Slip { on } => C::SetSlip { deck, on },
            Self::SlipHold { held } => C::SlipHold { deck, held },
            Self::VinylSpeed { start, brake } => C::SetVinylSpeed { deck, start, brake },
            Self::SyncShift { seconds } => C::SyncShift { deck, seconds },
            Self::SyncOffset { beats } => C::SyncOffset { deck, beats },
            Self::Seek { seconds } => C::Seek { deck, seconds },
            Self::Shift { seconds } => C::Shift { deck, seconds },
            Self::Play { playing } => C::SetPlaying { deck, playing },
            Self::Loop { start, end } => C::SetLoop { deck, start, end },
            Self::ClearLoop => C::ClearLoop { deck },
        })
    }

    /// The deck's settings a clip starts with, after this move.
    fn settle_into(self, clip: &mut Clip) {
        match self {
            Self::KeyLock { on } => clip.key_lock = on,
            Self::KeyShift { semitones } => clip.key_shift = semitones,
            Self::Slip { on } => clip.slip = on,
            Self::VinylSpeed { start, brake } => clip.vinyl = Some((start, brake)),
            _ => {}
        }
    }
}

/// A stretch of a track played on a deck: from `source` seconds into
/// `track`, at `at` frames into the set, for `length` frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub deck: usize,
    pub track: PathBuf,
    #[serde(default)]
    pub stems: Option<StemsSource>,
    pub at: u64,
    pub length: u64,
    /// Where in the track it starts (seconds).
    pub source: f64,
    #[serde(default)]
    pub key_lock: bool,
    #[serde(default)]
    pub key_shift: f64,
    #[serde(default)]
    pub slip: bool,
    /// Vinyl Speed Adjust: start and brake times.
    #[serde(default)]
    pub vinyl: Option<(f64, f64)>,
    /// Looping between these places in the track (seconds) from the start.
    #[serde(default)]
    pub looped: Option<(f64, f64)>,
    /// What was done to the record while it played, in frames from the
    /// clip's start.
    #[serde(default)]
    pub moves: Vec<(u64, Move)>,
    /// Its stem levels (frames from its start), as recorded until changed.
    #[serde(default)]
    pub stem_levels: Vec<StemLevels>,
    /// It ends with the deck stopped (as the DJ stopped it, braking as
    /// set), not cut.
    #[serde(default)]
    pub brake: bool,
    /// Seconds its audio was moved later in the set (earlier if negative)
    /// inside it, by the editor: `source` has it already.
    #[serde(default)]
    pub later: f64,
    /// The DJ's hand was on the record (scratching): a clip of its own,
    /// so the clips around it play straight.
    #[serde(default)]
    pub scratch: bool,
    /// It starts where the clip before it left the record (split from
    /// it): played straight on from there, not sent to `source`.
    #[serde(default)]
    pub continues: bool,
    /// The end (frames) of the clip on its deck it continues from: only
    /// while that one still ends there does it play straight on.
    #[serde(default)]
    pub continues_from: Option<u64>,
    /// What the deck was told the moment it began, in order, as recorded
    /// (the jump, the play): done again just so while the clip carries on
    /// from the one before, so an untouched set plays exactly as it was.
    /// An edit to its start makes it begin anew instead.
    #[serde(default)]
    pub opening: Vec<Move>,
    /// Where in the track the deck was just before its opening.
    #[serde(default)]
    pub opening_from: f64,
}

impl Clip {
    pub fn end(&self) -> u64 {
        self.at + self.length
    }

    /// Its stem levels `offset` frames in.
    fn levels_at(&self, offset: u64) -> [f32; PARTS] {
        self.stem_levels.iter().take_while(|l| l.0 <= offset).last().map_or(ALL_STEMS, |l| l.1)
    }

    /// Takes off its first `cut` frames: the moves there are gone (their
    /// settings kept), it starts at `source`.
    fn cut_front(&mut self, cut: u64, source: f64) {
        let cut = cut.min(self.length);
        let levels = self.levels_at(cut);
        let mut moves = Vec::with_capacity(self.moves.len());
        for (off, m) in std::mem::take(&mut self.moves) {
            if off < cut {
                m.settle_into(self);
            } else {
                moves.push((off - cut, m));
            }
        }
        self.moves = moves;
        let mut stem_levels = vec![(0, levels)];
        stem_levels.extend(self.stem_levels.iter().filter(|l| l.0 > cut).map(|l| (l.0 - cut, l.1)));
        stem_levels.dedup_by(|b, a| a.1 == b.1);
        self.stem_levels = if stem_levels == [(0, ALL_STEMS)] { Vec::new() } else { stem_levels };
        self.at += cut;
        self.length -= cut;
        self.source = source;
        self.continues = false;
        self.continues_from = None;
        self.opening.clear();
    }

    /// Lets it start `more` frames sooner, `source` there: its moves stay
    /// where they are in the set.
    fn grow_front(&mut self, more: u64, source: f64) {
        for m in &mut self.moves {
            m.0 += more;
        }
        for l in self.stem_levels.iter_mut().skip(1) {
            l.0 += more;
        }
        self.at -= more;
        self.length += more;
        self.source = source;
        self.continues = false;
        self.continues_from = None;
        self.opening.clear();
    }

    /// Ends it after `length` frames, cut there.
    fn cut_back(&mut self, length: u64) {
        if length < self.length {
            self.length = length;
            self.moves.retain(|m| m.0 < length);
            self.stem_levels.retain(|l| l.0 == 0 || l.0 < length);
            self.brake = false;
        }
    }

    /// The next clip `next` carries straight on from this one (a clip split
    /// in two): no need to load the track again between them.
    fn carries_on(&self, next: &Clip) -> bool {
        self.deck == next.deck
            && self.end() == next.at
            && self.track == next.track
            && self.stems == next.stems
            && !self.brake
    }
}

/// What a deletion takes along with the audio (its moves always go).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteOptions {
    /// The deck's EQ and Color FX there.
    pub eq_filter: bool,
    /// The deck's volume and trim there.
    pub volume: bool,
    /// The Beat FX there.
    pub beat_fx: bool,
}

/// An edit's file.
#[derive(Serialize)]
struct SavedRef<'a> {
    format: &'a str,
    version: u32,
    rate: u32,
    block: u32,
    length: u64,
    recording: &'a Option<PathBuf>,
    snapshot: &'a Snapshot,
    clips: &'a [Clip],
    lanes: Vec<(&'a Setting, &'a Vec<Point>)>,
    beat_fx: &'a [(u64, BeatFxParams)],
    markers: &'a [u64],
    grids: BTreeMap<&'a PathBuf, &'a Grid>,
    others: &'a [(u64, serde_json::Value)],
}

#[derive(Deserialize)]
struct Saved {
    format: String,
    version: u32,
    rate: u32,
    block: u32,
    length: u64,
    #[serde(default)]
    recording: Option<PathBuf>,
    snapshot: Box<Snapshot>,
    clips: Vec<Clip>,
    #[serde(default)]
    lanes: Vec<(Setting, Vec<Point>)>,
    #[serde(default)]
    beat_fx: Vec<(u64, BeatFxParams)>,
    #[serde(default)]
    markers: Vec<u64>,
    #[serde(default)]
    grids: HashMap<PathBuf, Grid>,
    #[serde(default)]
    others: Vec<(u64, serde_json::Value)>,
}

/// A recording as edited (see the module's notes). Times are in frames
/// from the start of the set.
#[derive(Clone)]
pub struct Edit {
    pub rate: u32,
    /// The mixer's buffer length: everything happens on a buffer start.
    pub block: u32,
    pub length: u64,
    /// The recording's file, to start again from.
    pub recording: Option<PathBuf>,
    /// Sorted by deck, then time; on a deck they do not overlap.
    pub clips: Vec<Clip>,
    pub lanes: BTreeMap<Setting, Vec<Point>>,
    /// The Beat FX unit: as it starts, then every change.
    pub beat_fx: Vec<(u64, BeatFxParams)>,
    pub markers: Vec<u64>,
    pub grids: HashMap<PathBuf, Grid>,
    /// The rest the mixer was told (settings, sampler), as recorded.
    others: Vec<(u64, serde_json::Value)>,
    /// The mixer as recording started (its settings; the decks are emptied
    /// at once).
    snapshot: Arc<Snapshot>,
    tracks: HashMap<PathBuf, Arc<TrackAudio>>,
    stems: HashMap<StemsSource, Arc<StemLayers>>,
    warnings: Vec<String>,
}

/// Where decks are in their tracks (and their tempo) at given moments,
/// after what is done then: `want` (clock, deck) is sorted.
struct Places {
    want: Vec<(u64, usize)>,
    found: Vec<(f64, f64)>,
    /// Where they were just before what is done then, instead.
    before_events: bool,
}

impl Run for Places {
    fn positions_only(&self) -> bool {
        true
    }

    fn before(&mut self, clock: u64, mixer: &mut Offline) -> Result<()> {
        // What is done now is still waiting to be taken in until settled.
        if !self.before_events && self.want.get(self.found.len()).is_some_and(|w| w.0 <= clock) {
            mixer.settle();
        }
        while let Some(&(at, deck)) = self.want.get(self.found.len())
            && at <= clock
        {
            let view = mixer.deck(deck);
            self.found.push((view.seconds, view.tempo));
        }
        if self.found.len() == self.want.len() {
            bail!(PLACES_FOUND);
        }
        Ok(())
    }
}

/// Finds `want` (frames from the start, deck) in `session`: where each
/// deck is in its track then, and its tempo. Sorted as asked.
fn places(session: &Session, want: &[(u64, usize)]) -> Result<Vec<(f64, f64)>> {
    find_places(session, want, false)
}

/// [`places`], or where the decks were just before what is done then.
fn find_places(session: &Session, want: &[(u64, usize)], before_events: bool) -> Result<Vec<(f64, f64)>> {
    let start = session.snapshot.clock;
    let mut order: Vec<usize> = (0..want.len()).collect();
    order.sort_by_key(|&i| want[i].0);
    let mut places = Places {
        want: order.iter().map(|&i| (start + want[i].0, want[i].1)).collect(),
        found: Vec::new(),
        before_events,
    };
    match session.run(session.events.iter(), &mut places) {
        Err(err) if err.to_string() != PLACES_FOUND => return Err(err),
        _ => {}
    }
    if places.found.len() < want.len() {
        bail!("past the end of the set");
    }
    let mut found = vec![(0.0, 1.0); want.len()];
    for (k, &i) in order.iter().enumerate() {
        found[i] = places.found[k];
    }
    Ok(found)
}

/// A deck as the recording is read through, for [`Edit::from_session`].
#[derive(Default)]
struct Deck {
    track: Option<PathBuf>,
    stems: Option<StemsSource>,
    levels: [f32; PARTS],
    key_lock: bool,
    key_shift: f64,
    slip: bool,
    vinyl: Option<(f64, f64)>,
    looped: Option<(f64, f64)>,
    playing: bool,
    gate: bool,
    open: Option<Clip>,
    /// The hand is on the record.
    scratching: bool,
    /// When the hand let go, while the scratch clip may still go on (it
    /// grabs again soon).
    released: Option<u64>,
    /// What the deck was told at the moment being read, in order.
    frame: (u64, Vec<Move>),
    /// Stopped then by the DJ and not touched since: played again, it
    /// carries on from there.
    resume: Option<u64>,
}

impl Deck {
    /// Notes `m`, told at `at`; a clip that began then has it in its
    /// opening.
    fn note(&mut self, at: u64, m: Move) {
        if self.frame.0 != at {
            self.frame = (at, Vec::new());
        }
        self.frame.1.push(m);
        if let Some(clip) = &mut self.open
            && clip.at == at
        {
            clip.opening = self.frame.1.clone();
        }
    }

    fn heard(&self) -> bool {
        self.playing && self.gate && self.track.is_some()
    }

    fn open(&mut self, deck: usize, at: u64) {
        let Some(track) = self.track.clone() else { return };
        let stem_levels =
            if self.levels == ALL_STEMS { Vec::new() } else { vec![(0, self.levels)] };
        self.open = Some(Clip {
            deck,
            track,
            stems: self.stems.clone(),
            at,
            length: 0,
            source: 0.0,
            key_lock: self.key_lock,
            key_shift: self.key_shift,
            slip: self.slip,
            vinyl: self.vinyl,
            looped: self.looped,
            moves: Vec::new(),
            stem_levels,
            brake: false,
            later: 0.0,
            scratch: self.scratching,
            continues: self.resume.is_some(),
            continues_from: self.resume,
            opening: if self.frame.0 == at { self.frame.1.clone() } else { Vec::new() },
            opening_from: 0.0,
        });
    }

    /// A scratch let go of before `at` and not grabbed again within `gap`
    /// ends there: the clip after it plays straight.
    fn settle_release(&mut self, at: u64, gap: u64, clips: &mut Vec<Clip>) {
        if let Some(released) = self.released
            && at > released + gap
        {
            self.released = None;
            self.scratching = false;
            self.split_at(released, clips);
        }
    }

    /// Ends the clip at `at` and carries on in a new one from there (with
    /// what was done to the record from then on).
    fn split_at(&mut self, at: u64, clips: &mut Vec<Clip>) {
        let Some(mut clip) = self.open.take() else { return };
        if at <= clip.at {
            self.open = Some(clip);
            return;
        }
        let off = at - clip.at;
        let mut next = clip.clone();
        (next.at, next.length, next.source, next.scratch) = (at, 0, 0.0, self.scratching);
        next.continues = true;
        next.continues_from = Some(at);
        next.opening = Vec::new();
        let levels = clip.levels_at(off);
        next.moves = Vec::new();
        for &(o, m) in &clip.moves {
            if o < off {
                m.settle_into(&mut next);
            } else {
                next.moves.push((o - off, m));
            }
        }
        next.stem_levels = std::iter::once((0, levels))
            .chain(clip.stem_levels.iter().filter(|l| l.0 > off).map(|l| (l.0 - off, l.1)))
            .collect();
        if next.stem_levels == [(0, ALL_STEMS)] {
            next.stem_levels.clear();
        }
        clip.moves.retain(|m| m.0 < off);
        clip.stem_levels.retain(|l| l.0 < off);
        (clip.length, clip.brake) = (off, false);
        clips.push(clip);
        self.open = Some(next);
    }

    fn close(&mut self, at: u64, brake: bool, clips: &mut Vec<Clip>) {
        self.settle_release(at, 0, clips);
        if let Some(mut clip) = self.open.take() {
            clip.length = at.saturating_sub(clip.at);
            clip.brake = brake;
            let length = clip.length;
            clip.moves.retain(|m| m.0 < length);
            if clip.length > 0 {
                self.resume = brake.then_some(at);
                clips.push(clip);
            }
        }
    }

    /// A clip of its own from `at` on, if it plays (in place of one that
    /// only just began).
    fn restart(&mut self, deck: usize, at: u64, clips: &mut Vec<Clip>) {
        let heard = self.heard();
        let (stems, looped) = (self.stems.clone(), self.looped);
        match &mut self.open {
            Some(clip) if clip.at == at => {
                (clip.stems, clip.looped, clip.scratch) = (stems, looped, self.scratching)
            }
            Some(_) => {
                self.close(at, false, clips);
                self.open(deck, at);
            }
            None if heard => self.open(deck, at),
            None => {}
        }
    }
}

impl Edit {
    /// The recording `session` as clips and lanes, to edit.
    pub fn from_session(session: &Session) -> Result<Self> {
        let mut session = session.duplicate()?;
        session.split_color_fx();
        let s = &session;
        let start = s.snapshot.clock;
        let length = s.length();
        let rate = s.rate;
        let block = s
            .events
            .iter()
            .find_map(|(_, e)| match e {
                Logged::Block(n) => Some(*n),
                _ => None,
            })
            .ok_or_else(|| anyhow!("the session has no buffer size"))?;
        let path_of = |audio: &Arc<TrackAudio>| {
            s.tracks
                .iter()
                .find(|(_, a)| Arc::ptr_eq(a, audio))
                .map(|(p, _)| p.clone())
                .or_else(|| source_of(&TRACKS, audio))
                .filter(|p| !p.as_os_str().is_empty())
        };
        let stems_of = |stems: &Arc<StemLayers>| {
            s.stems
                .iter()
                .find(|(_, x)| Arc::ptr_eq(x, stems))
                .map(|(source, _)| source.clone())
                .or_else(|| source_of(&STEMS, stems))
        };
        let jump_split = (JUMP_SPLIT_SECS * f64::from(rate)) as u64;
        let scratch_gap = (SCRATCH_GAP_SECS * f64::from(rate)) as u64;
        let mut decks: Vec<Deck> = (0..DECKS)
            .map(|d| {
                let (audio, playing) = s.snapshot.deck(d);
                // As it was when recording began: a loop going, a key
                // shifted, a nudge held (a new load would forget them).
                let (looped, (start, brake), slip, key_shift, nudge) = s.snapshot.deck_play(d);
                let mut deck = Deck {
                    track: audio.and_then(path_of),
                    stems: s.snapshot.stems(d).and_then(stems_of),
                    levels: s.snapshot.stem_levels(d),
                    key_lock: s.snapshot.key_lock(d),
                    key_shift,
                    slip,
                    vinyl: (start != 0.0 || brake != 0.0).then_some((start, brake)),
                    looped,
                    playing,
                    gate: true,
                    ..Deck::default()
                };
                if deck.heard() {
                    deck.open(d, 0);
                    // Playing on from before recording began.
                    if let Some(clip) = &mut deck.open {
                        (clip.continues, clip.continues_from) = (true, Some(0));
                    }
                    if nudge != 0.0
                        && let Some(clip) = &mut deck.open
                    {
                        clip.moves.push((0, Move::Nudge { amount: nudge }));
                    }
                }
                deck
            })
            .collect();
        let mut clips = Vec::new();
        let mut others = Vec::new();
        for (at, event) in &s.events {
            let at = at.saturating_sub(start).min(length);
            let command = match event {
                Logged::Jog(d, steps) => {
                    if let Some(deck) = decks.get_mut(usize::from(*d)) {
                        if deck.open.is_none() {
                            deck.resume = None;
                        }
                        deck.note(at, Move::Jog { steps: *steps });
                    }
                    if let Some(clip) = decks.get_mut(usize::from(*d)).and_then(|d| d.open.as_mut())
                    {
                        clip.moves.push((at - clip.at, Move::Jog { steps: *steps }));
                    }
                    continue;
                }
                Logged::Command(command) => command,
                Logged::Block(_) | Logged::End => continue,
            };
            // Lanes, Beat FX and markers are read as a whole below.
            if Setting::of(command).is_some()
                || matches!(command, Command::SetBeatFx(_) | Command::Marker | Command::Record(_))
            {
                continue;
            }
            let Some(d) = deck_of(event).filter(|&d| d < DECKS) else {
                others.push((at, serde_json::to_value(command)?));
                continue;
            };
            let deck = &mut decks[d];
            deck.settle_release(at, scratch_gap, &mut clips);
            let noted = Move::of(command).map(|(_, m)| m);
            // Touched while stopped: played again, it starts anew.
            if deck.open.is_none()
                && !matches!(
                    command,
                    Command::SetPlaying { playing: true, .. } | Command::Gate { open: true, .. }
                )
            {
                deck.resume = None;
            }
            match command {
                Command::Hold { held, .. } if deck.open.is_some() => {
                    if *held {
                        let again = deck.released.take().is_some();
                        if !again && !deck.scratching {
                            deck.scratching = true;
                            deck.restart(d, at, &mut clips);
                        }
                        deck.scratching = true;
                    } else if deck.scratching {
                        deck.released = Some(at);
                    }
                    if let Some(clip) = &mut deck.open {
                        clip.moves.push((at - clip.at, Move::Hold { held: *held }));
                    }
                }
                Command::Load { audio, .. } => {
                    deck.close(at, false, &mut clips);
                    (deck.track, deck.stems, deck.levels) = (path_of(audio), None, ALL_STEMS);
                    (deck.looped, deck.playing, deck.gate) = (None, false, true);
                }
                Command::Unload { .. } => {
                    deck.close(at, false, &mut clips);
                    deck.track = None;
                }
                Command::SetPlaying { playing, .. } => {
                    deck.playing = *playing;
                    if *playing && deck.open.is_none() && deck.heard() {
                        deck.open(d, at);
                    } else if *playing && let Some(clip) = &mut deck.open {
                        // Played again while playing: it may have stopped at
                        // its track's end since; done again as recorded.
                        if clip.at < at {
                            clip.moves.push((at - clip.at, Move::Play { playing: true }));
                        }
                    } else if !*playing {
                        deck.close(at, true, &mut clips);
                    }
                }
                Command::Gate { open, .. } => {
                    deck.gate = *open;
                    if *open && deck.open.is_none() && deck.heard() {
                        deck.open(d, at);
                    } else if !*open {
                        deck.close(at, false, &mut clips);
                    }
                }
                Command::SetLoop { start, end, .. } => {
                    deck.looped = (end > start).then_some((*start, *end));
                    deck.restart(d, at, &mut clips);
                }
                Command::ClearLoop { .. } => {
                    deck.looped = None;
                    deck.restart(d, at, &mut clips);
                }
                Command::SetStems { stems, .. } => {
                    deck.stems = stems.as_ref().and_then(stems_of);
                    deck.restart(d, at, &mut clips);
                }
                Command::SetStemLevels { levels, .. } => {
                    deck.levels = *levels;
                    if let Some(clip) = &mut deck.open {
                        let off = at - clip.at;
                        clip.stem_levels.retain(|l| l.0 != off);
                        clip.stem_levels.push((off, *levels));
                    }
                }
                command => {
                    let Some((_, m)) = Move::of(command) else {
                        others.push((at, serde_json::to_value(command)?));
                        continue;
                    };
                    let jump = matches!(m, Move::Seek { .. } | Move::Shift { .. });
                    match &mut deck.open {
                        // A jump: a clip of its own (not the needle dragged).
                        Some(clip) if jump && at >= clip.at + jump_split => {
                            deck.restart(d, at, &mut clips);
                        }
                        Some(clip) => clip.moves.push((at - clip.at, m)),
                        None => {}
                    }
                    let mut settings = Clip {
                        key_lock: deck.key_lock,
                        key_shift: deck.key_shift,
                        slip: deck.slip,
                        vinyl: deck.vinyl,
                        ..Clip::empty()
                    };
                    m.settle_into(&mut settings);
                    (deck.key_lock, deck.key_shift, deck.slip, deck.vinyl) =
                        (settings.key_lock, settings.key_shift, settings.slip, settings.vinyl);
                }
            }
            if let Some(m) = noted {
                decks[d].note(at, m);
            }
        }
        for deck in &mut decks {
            deck.close(length, false, &mut clips);
        }
        clips.sort_by_key(|c| (c.deck, c.at));
        // Where each clip starts in its track, as the recording plays.
        let want: Vec<(u64, usize)> = clips.iter().map(|c| (c.at, c.deck)).collect();
        for (clip, (seconds, _)) in clips.iter_mut().zip(places(s, &want)?) {
            clip.source = seconds;
        }
        // And where the deck was as the moment it began came (its opening
        // is done from there).
        for (clip, (seconds, _)) in clips.iter_mut().zip(find_places(s, &want, true)?) {
            clip.opening_from = seconds;
        }
        let mut lanes = BTreeMap::new();
        for setting in Setting::all(DECKS).into_iter().chain([Setting::BeatFxDepth]) {
            lanes.insert(setting, s.lane(setting));
        }
        Ok(Self {
            rate,
            block,
            length,
            recording: None,
            clips,
            lanes,
            beat_fx: s.beat_fx(),
            markers: s.markers(),
            grids: s.grids.clone(),
            others,
            snapshot: Arc::new(s.copy(&*s.snapshot)?),
            tracks: s.tracks.clone(),
            stems: s.stems.clone(),
            warnings: s.warnings.clone(),
        })
    }

    /// Reads an edit, opening its tracks and stems with `sources`.
    pub fn read(path: &Path, sources: impl Sources + 'static) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let opened = Opened {
            sources: Box::new(sources),
            tracks: HashMap::new(),
            stems: HashMap::new(),
            warnings: Vec::new(),
            folder: path.parent().map(Path::to_path_buf),
        };
        OPENED.set(Some(opened));
        let read = (|| -> Result<Saved> {
            let saved: Saved = serde_json::from_reader(BufReader::new(file))?;
            if saved.format != FORMAT {
                bail!("not an edit");
            }
            if saved.version > VERSION {
                bail!("made by a newer version of the app");
            }
            for clip in &saved.clips {
                open_track(&clip.track)?;
                if let Some(stems) = &clip.stems {
                    open_stems(stems)?;
                }
            }
            Ok(saved)
        })();
        let opened = OPENED.take();
        let saved = read.with_context(|| format!("reading {}", path.display()))?;
        let opened = opened.ok_or_else(|| anyhow!("the tracks were not kept"))?;
        Ok(Self {
            rate: saved.rate,
            block: saved.block,
            length: saved.length,
            recording: saved.recording,
            clips: saved.clips,
            lanes: saved.lanes.into_iter().collect(),
            beat_fx: saved.beat_fx,
            markers: saved.markers,
            grids: saved.grids,
            others: saved.others,
            snapshot: Arc::from(saved.snapshot),
            tracks: opened.tracks,
            stems: opened.stems,
            warnings: opened.warnings,
        })
    }

    /// Writes the edit to `path` (by way of a file beside it, so a failed
    /// write leaves the old one).
    pub fn write(&self, path: &Path) -> Result<()> {
        let saved = SavedRef {
            format: FORMAT,
            version: VERSION,
            rate: self.rate,
            block: self.block,
            length: self.length,
            recording: &self.recording,
            snapshot: &self.snapshot,
            clips: &self.clips,
            lanes: self.lanes.iter().collect(),
            beat_fx: &self.beat_fx,
            markers: &self.markers,
            grids: self.grids.iter().collect(),
            others: &self.others,
        };
        let mut name = path.as_os_str().to_owned();
        name.push(".saving");
        let temp = PathBuf::from(name);
        let file = File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
        let mut out = BufWriter::with_capacity(1 << 16, file);
        serde_json::to_writer_pretty(&mut out, &saved)?;
        out.write_all(b"\n")?;
        out.flush()?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(&temp, path).with_context(|| format!("saving {}", path.display()))?;
        Ok(())
    }

    /// What could not be opened as it was (stems), so plays differently.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn track_audio(&self, path: &Path) -> Option<Arc<TrackAudio>> {
        self.tracks.get(path).cloned()
    }

    /// `at` moved to the nearest buffer start.
    pub fn snap(&self, at: u64) -> u64 {
        let block = u64::from(self.block.max(1));
        (at + block / 2) / block * block
    }

    /// The clips on `deck`, by index into [`Edit::clips`].
    pub fn on_deck(&self, deck: usize) -> impl Iterator<Item = (usize, &Clip)> {
        self.clips.iter().enumerate().filter(move |(_, c)| c.deck == deck)
    }

    /// The clip on `deck` playing at `at`.
    pub fn clip_at(&self, deck: usize, at: u64) -> Option<usize> {
        self.on_deck(deck).find(|(_, c)| c.at <= at && at < c.end()).map(|(i, _)| i)
    }

    /// The tempo lane of `deck`.
    fn tempo(&self, deck: usize) -> &[Point] {
        self.lanes.get(&Setting::Tempo(deck)).map_or(&[], Vec::as_slice)
    }

    /// Track seconds `deck` plays from `from` to `to`, at its tempo.
    fn played_secs(&self, deck: usize, from: u64, to: u64) -> f64 {
        let lane = self.tempo(deck);
        let block = u64::from(self.block.max(1));
        let mut secs = 0.0;
        let mut at = from;
        while at < to {
            let step = (to - at).min(block);
            let tempo = if lane.is_empty() { 1.0 } else { lane_value(lane, at) };
            secs += tempo * step as f64 / f64::from(self.rate);
            at += step;
        }
        secs
    }

    /// Where `deck` is in its track at `at` (after what is done then), as
    /// the edit plays.
    pub fn place(&self, deck: usize, at: u64) -> Result<f64> {
        Ok(places(&self.to_session()?, &[(at, deck)])?[0].0)
    }

    /// How far clip `i`'s start can go: back to the end of the clip before
    /// it on its deck and the track's start, on to its end.
    pub fn start_limits(&self, i: usize) -> (u64, u64) {
        let clip = &self.clips[i];
        let before = self
            .on_deck(clip.deck)
            .filter(|(_, c)| c.end() <= clip.at)
            .map(|(_, c)| c.end())
            .max()
            .unwrap_or(0);
        // The track's start: as far back as it has played since.
        let rate = f64::from(self.rate);
        let tempo = lane_value(self.tempo(clip.deck), clip.at).max(0.01);
        let back = (clip.source.max(0.0) / tempo * rate) as u64;
        let earliest = before.max(clip.at.saturating_sub(back));
        let block = u64::from(self.block.max(1));
        (earliest.div_ceil(block) * block, clip.end().saturating_sub(block).max(clip.at))
    }

    /// Moves clip `i`'s start to `at` (within [`Edit::start_limits`]): it
    /// plays more of its track, or less; what it did to the record there
    /// stays where it was in the set. Returns where it starts now.
    pub fn trim_start(&mut self, i: usize, at: u64) -> Result<u64> {
        let (earliest, latest) = self.start_limits(i);
        let at = self.snap(at).clamp(earliest, latest);
        let clip = &self.clips[i];
        if at < clip.at {
            let source = clip.source - self.played_secs(clip.deck, at, clip.at);
            let more = clip.at - at;
            self.clips[i].grow_front(more, source.max(0.0));
        } else if at > clip.at {
            let source = self.place(clip.deck, at)?;
            let cut = at - clip.at;
            self.clips[i].cut_front(cut, source);
        }
        Ok(at)
    }

    /// Moves clip `i`'s end to `end`: not past the next clip on its deck.
    /// Returns where it ends now.
    pub fn trim_end(&mut self, i: usize, end: u64) -> u64 {
        let clip = &self.clips[i];
        let block = u64::from(self.block.max(1));
        let after = self
            .on_deck(clip.deck)
            .filter(|(_, c)| c.at >= clip.end())
            .map(|(_, c)| c.at)
            .min()
            .unwrap_or(u64::MAX);
        let end = self.snap(end).clamp(clip.at + block, after.max(clip.at + block));
        let clip = &mut self.clips[i];
        if end < clip.end() {
            clip.cut_back(end - clip.at);
        } else {
            clip.length = end - clip.at;
            clip.brake = false;
        }
        self.fit_length();
        end
    }

    /// Splits the clip on `deck` at `at` in two (sounding the same). Returns
    /// the second's index.
    pub fn split(&mut self, deck: usize, at: u64) -> Result<usize> {
        let at = self.snap(at);
        let i = self
            .clip_at(deck, at)
            .filter(|&i| self.clips[i].at < at)
            .ok_or_else(|| anyhow!("no clip on deck {} there", deck + 1))?;
        let source = self.place(deck, at)?;
        let mut second = self.clips[i].clone();
        second.cut_front(at - second.at, source);
        second.continues = true;
        second.continues_from = Some(at);
        let first = at - self.clips[i].at;
        self.clips[i].cut_back(first);
        self.clips.insert(i + 1, second);
        Ok(i + 1)
    }

    /// Moves clip `i` by `by` frames (not before the start): it is laid
    /// over what is where it lands, which is cut away. Returns where it
    /// starts now.
    pub fn move_clip(&mut self, i: usize, by: i64) -> Result<u64> {
        let clip = &self.clips[i];
        Ok(self.shift_range(clip.deck, clip.at, clip.end(), by)?.0)
    }

    /// Moves `deck`'s audio from `from` to `to` by `by` frames, splitting
    /// the clips there: laid over what is where it lands. Returns where it
    /// is now.
    pub fn shift_range(&mut self, deck: usize, from: u64, to: u64, by: i64) -> Result<(u64, u64)> {
        let (from, to) = (self.snap(from), self.snap(to));
        let new_from = self.snap((from as i64 + by).max(0) as u64);
        if from >= to || new_from == from {
            return Ok((from, to));
        }
        for at in [from, to] {
            if self.clip_at(deck, at).is_some_and(|i| self.clips[i].at < at) {
                self.split(deck, at)?;
            }
        }
        let before = self.clips.clone();
        let (moving, rest): (Vec<Clip>, Vec<Clip>) = std::mem::take(&mut self.clips)
            .into_iter()
            .partition(|c| c.deck == deck && from <= c.at && c.end() <= to);
        self.clips = rest;
        let new_to = new_from + (to - from);
        if let Err(err) = self.delete(deck, new_from, new_to, DeleteOptions::default()) {
            self.clips = before;
            return Err(err);
        }
        for mut clip in moving {
            clip.at = clip.at - from + new_from;
            clip.continues = false;
            clip.continues_from = None;
            clip.opening.clear();
            self.clips.push(clip);
        }
        self.sort();
        self.fit_length();
        Ok((new_from, new_to))
    }

    fn sort(&mut self) {
        self.clips.sort_by_key(|c| (c.deck, c.at));
    }

    /// Deletes `deck`'s audio from `from` to `to`, with what was done to
    /// the record there, and what `options` say.
    pub fn delete(
        &mut self,
        deck: usize,
        from: u64,
        to: u64,
        options: DeleteOptions,
    ) -> Result<()> {
        let (from, to) = (self.snap(from), self.snap(to));
        if from >= to {
            return Ok(());
        }
        for at in [to, from] {
            if self.clip_at(deck, at).is_some_and(|i| self.clips[i].at < at) {
                self.split(deck, at)?;
            }
        }
        self.clips.retain(|c| !(c.deck == deck && from <= c.at && c.end() <= to));
        let mut settings = Vec::new();
        if options.eq_filter {
            settings.extend((0..3).map(|band| Setting::Eq(deck, band)));
            settings.push(Setting::Filter(deck));
            settings.extend(crate::fx::ColorFxKind::ALL.map(|kind| Setting::ColorFx(deck, kind)));
        }
        if options.volume {
            settings.extend([Setting::Volume(deck), Setting::Trim(deck)]);
        }
        for setting in settings {
            if let Some(points) = self.lanes.get_mut(&setting) {
                clear_range(points, from, to);
            }
        }
        if options.beat_fx {
            let after = self.beat_fx.iter().take_while(|c| c.0 <= to).last().map(|c| c.1);
            let before = self.beat_fx.iter().take_while(|c| c.0 < from).last().map(|c| c.1);
            self.beat_fx.retain(|c| !(from <= c.0 && c.0 <= to));
            if let (Some(after), Some(before)) = (after, before)
                && after != before
            {
                self.beat_fx.push((to, after));
                self.beat_fx.sort_by_key(|c| c.0);
            }
        }
        Ok(())
    }

    /// The set goes on past its last clip, with a little to ring out.
    fn fit_length(&mut self) {
        let last = self.clips.iter().map(Clip::end).max().unwrap_or(0);
        let tail = (TAIL_SECS * f64::from(self.rate)) as u64;
        if last + tail > self.length {
            let block = u64::from(self.block.max(1));
            self.length = (last + tail).div_ceil(block) * block;
        }
    }

    /// `deck` as recording began, as a clip ending there (stopped unless
    /// it was playing), if it held a track.
    fn as_recording_began(&self, deck: usize) -> Option<Clip> {
        let (audio, playing) = self.snapshot.deck(deck);
        let audio = audio?;
        let track = self
            .tracks
            .iter()
            .find(|(_, a)| Arc::ptr_eq(a, audio))
            .map(|(p, _)| p.clone())
            .or_else(|| source_of(&TRACKS, audio))?;
        let stems = self.snapshot.stems(deck).and_then(|stems| {
            self.stems
                .iter()
                .find(|(_, x)| Arc::ptr_eq(x, stems))
                .map(|(source, _)| source.clone())
                .or_else(|| source_of(&STEMS, stems))
        });
        Some(Clip { deck, track, stems, brake: !playing, ..Clip::empty() })
    }

    /// A clip's end: stopped as the DJ did (braking as set), or cut.
    fn end_clip(&self, clip: &Clip, events: &mut Vec<(u64, u8, Logged)>) {
        let deck = clip.deck;
        let end = clip.end();
        if clip.brake {
            events.push((end, 1, Logged::Command(Command::SetPlaying { deck, playing: false })));
        } else {
            events.push((end, 1, Logged::Command(Command::Gate { deck, open: false })));
            events.push((end, 1, Logged::Command(Command::Hold { deck, held: false })));
            events.push((end, 1, Logged::Command(Command::SetNudge { deck, amount: 0.0 })));
        }
    }

    /// The edit as commands to the mixer, to play it (render it, find
    /// where the decks are) as a recording is.
    pub fn to_session(&self) -> Result<Session> {
        let start = self.snapshot.clock;
        let block = u64::from(self.block.max(1));
        let length = self.length.max(self.clips.iter().map(Clip::end).max().unwrap_or(0));
        // (frames from the start, which first at one moment, the event)
        let mut events: Vec<(u64, u8, Logged)> = vec![(0, 0, Logged::Block(self.block))];
        // The decks as recording began: one keeps its track for its first
        // clip of it (as the DJ did); else it is emptied, or cut if it
        // played on where its first clip doesn't.
        let before: Vec<Option<Clip>> = (0..DECKS).map(|d| self.as_recording_began(d)).collect();
        for (deck, was) in before.iter().enumerate() {
            let first = self.clips.iter().find(|c| c.deck == deck);
            match (was, first) {
                (Some(was), Some(first)) if was.track == first.track && was.stems == first.stems => {
                    if !was.brake && !was.carries_on(first) {
                        events.push((0, 1, Logged::Command(Command::Gate { deck, open: false })));
                    }
                }
                _ => events.push((0, 1, Logged::Command(Command::Unload { deck }))),
            }
        }
        for (setting, points) in &self.lanes {
            let neutral_tempo = matches!(setting, Setting::Tempo(_)) && points.is_empty();
            if neutral_tempo {
                continue;
            }
            for (k, point) in points.iter().enumerate() {
                let at = point.at.min(length);
                events.push((at, 3, Logged::Command(setting.command(point.value))));
                if let Some(next) = points.get(k + 1)
                    && point.curve != super::Curve::Step
                {
                    let mut t = (at / block + 1) * block;
                    while t < next.at.min(length) {
                        events.push((
                            t,
                            3,
                            Logged::Command(setting.command(lane_value(points, t))),
                        ));
                        t += block;
                    }
                }
            }
        }
        let first_fx = self.snapshot.beat_fx();
        let mut last_fx = first_fx;
        for &(at, params) in &self.beat_fx {
            if params != last_fx || at == 0 && params != first_fx {
                last_fx = params;
                events.push((at, 3, Logged::Command(Command::SetBeatFx(params))));
            }
        }
        for &at in &self.markers {
            events.push((at, 3, Logged::Command(Command::Marker)));
        }
        for (at, command) in &self.others {
            let command: Command = copy_value(&self.tracks, &self.stems, command.clone())?;
            events.push((*at, 3, Logged::Command(command)));
        }
        for (i, clip) in self.clips.iter().enumerate() {
            let deck = clip.deck;
            let command = |c: Command| Logged::Command(c);
            let previous = self.clips[..i]
                .iter()
                .rev()
                .find(|c| c.deck == deck)
                .or(before[deck].as_ref());
            let next = self.clips.get(i + 1).filter(|c| c.deck == deck);
            let carried = previous.is_some_and(|p| p.carries_on(clip));
            // Its track still on the deck from the clip before (stopped or
            // cut since): not loaded again, as the DJ didn't.
            let same_load =
                previous.is_some_and(|p| p.track == clip.track && p.stems == clip.stems);
            let at = clip.at;
            if same_load && !carried && previous.is_some_and(|p| !p.brake) {
                events.push((at, 2, command(Command::Gate { deck, open: true })));
            }
            if !carried && !same_load {
                let audio = self
                    .tracks
                    .get(&clip.track)
                    .cloned()
                    .ok_or_else(|| anyhow!("{} is not open", clip.track.display()))?;
                events.push((at, 2, command(Command::Load { deck, audio })));
                let stems = match &clip.stems {
                    Some(source) => Some(self.stems.get(source).cloned().ok_or_else(|| {
                        anyhow!("stems for {} are not open", clip.track.display())
                    })?),
                    None => None,
                };
                if stems.is_some() {
                    events.push((at, 2, command(Command::SetStems { deck, stems })));
                }
            }
            // Carrying on from the clip before, as recorded: what began it,
            // just as it was told.
            // Or played again where the DJ stopped it, untouched since: the
            // deck carries on as it was.
            let resumes = clip.continues
                && previous.is_some_and(|p| {
                    p.track == clip.track
                        && p.stems == clip.stems
                        && clip.continues_from == Some(p.end())
                });
            if resumes || carried && !clip.opening.is_empty() {
                events.extend(clip.opening.iter().map(|m| (at, 2, m.logged(deck))));
                for &(off, levels) in
                    clip.stem_levels.iter().filter(|l| l.0 > 0 && l.0 < clip.length)
                {
                    events.push((at + off, 4, command(Command::SetStemLevels { deck, levels })));
                }
                // (Those at its start are in its opening, when it has one.)
                let opened = !clip.opening.is_empty();
                for &(off, m) in clip.moves.iter().filter(|m| (!opened || m.0 > 0) && m.0 < clip.length)
                {
                    events.push((at + off, 4, m.logged(deck)));
                }
                if !next.is_some_and(|n| clip.carries_on(n)) {
                    self.end_clip(clip, &mut events);
                }
                continue;
            }
            let mut setup = vec![
                Command::SetKeyLock { deck, on: clip.key_lock },
                Command::SetKeyShift { deck, semitones: clip.key_shift },
                Command::SetSlip { deck, on: clip.slip },
                Command::SetStemLevels { deck, levels: clip.levels_at(0) },
            ];
            // Begun as recorded: from where the deck was, what it was told
            // then, just so.
            let opened = !clip.opening.is_empty();
            if opened {
                setup.push(Command::Seek { deck, seconds: clip.opening_from });
            } else if !(carried && clip.continues) {
                setup.push(Command::Seek { deck, seconds: clip.source });
            }
            if let Some((start, brake)) = clip.vinyl {
                setup.push(Command::SetVinylSpeed { deck, start, brake });
            }
            let loops = clip.opening.iter().any(|m| matches!(m, Move::Loop { .. } | Move::ClearLoop));
            if !(opened && loops) {
                setup.push(match clip.looped {
                    Some((start, end)) => Command::SetLoop { deck, start, end },
                    None => Command::ClearLoop { deck },
                });
            }
            if carried {
                setup.push(Command::Gate { deck, open: true });
            }
            let plays = clip.opening.iter().any(|m| matches!(m, Move::Play { playing: true }));
            if !(opened && plays) {
                setup.push(Command::SetPlaying { deck, playing: true });
            }
            events.extend(setup.into_iter().map(|c| (at, 2, command(c))));
            events.extend(clip.opening.iter().map(|m| (at, 2, m.logged(deck))));
            if !self.lanes.contains_key(&Setting::Tempo(deck)) {
                events.push((at, 3, command(Command::SetTempo { deck, ratio: 1.0 })));
            }
            for &(off, levels) in clip.stem_levels.iter().filter(|l| l.0 > 0 && l.0 < clip.length) {
                events.push((at + off, 4, command(Command::SetStemLevels { deck, levels })));
            }
            // (Those at its start are in its opening, when it has one.)
            for &(off, m) in clip.moves.iter().filter(|m| (!opened || m.0 > 0) && m.0 < clip.length) {
                events.push((at + off, 4, m.logged(deck)));
            }
            // Its end: stopped as the DJ did, or cut (unless the next carries
            // on from it).
            if !next.is_some_and(|n| clip.carries_on(n)) {
                self.end_clip(clip, &mut events);
            }
        }
        events.push((length, 9, Logged::End));
        events.sort_by_key(|(at, order, _)| (*at, *order));
        Ok(Session {
            rate: self.rate,
            snapshot: Box::new(copy_with(&self.tracks, &self.stems, &*self.snapshot)?),
            events: events.into_iter().map(|(at, _, e)| (start + at, e)).collect(),
            end: Some(start + length),
            lost: 0,
            warnings: self.warnings.clone(),
            tracks: self.tracks.clone(),
            stems: self.stems.clone(),
            grids: self.grids.clone(),
            curves: self.lanes.clone().into_iter().collect(),
            edit: None,
        })
    }
}

/// How far past its end an edit can take a clip that loops (frames).
const OPEN_END: u64 = u64::MAX / 4;

/// The undoable part of an edit (the clips, lanes and effects).
#[derive(Serialize, Deserialize)]
struct Backup {
    length: u64,
    clips: Vec<Clip>,
    lanes: Vec<(Setting, Vec<Point>)>,
    beat_fx: Vec<(u64, BeatFxParams)>,
    markers: Vec<u64>,
}

/// What the editor asks of a session, answered by the clips.
impl Edit {
    /// The clips as the editor draws them: each a span of its own,
    /// played throughout.
    pub(crate) fn spans(&self) -> Vec<super::TrackSpan> {
        self.clips
            .iter()
            .map(|c| super::TrackSpan {
                deck: c.deck,
                path: c.track.clone(),
                from: c.at,
                to: c.end(),
                playing: vec![(c.at, c.end())],
                scratch: c.scratch,
            })
            .collect()
    }

    /// The clip on `deck` from `from` to `to`, or else one starting at
    /// `from`.
    fn find(&self, deck: usize, from: u64, to: u64) -> Option<usize> {
        self.on_deck(deck)
            .find(|(_, c)| c.at == from && c.end() == to)
            .or_else(|| self.on_deck(deck).find(|(_, c)| c.at == from))
            .map(|(i, _)| i)
    }

    /// How far clip `i` can play on: to the next clip on its deck and the
    /// end of its track.
    fn end_limit(&self, i: usize) -> u64 {
        let clip = &self.clips[i];
        let next = self
            .on_deck(clip.deck)
            .filter(|(_, c)| c.at >= clip.end())
            .map(|(_, c)| c.at)
            .min()
            .unwrap_or(OPEN_END);
        let track_end = match (&clip.looped, self.tracks.get(&clip.track)) {
            (None, Some(audio)) => {
                let tempo = lane_value(self.tempo(clip.deck), clip.at).max(0.01);
                let left = (audio.duration_secs() - clip.source).max(0.0);
                clip.at + (left / tempo * f64::from(self.rate)) as u64
            }
            _ => OPEN_END,
        };
        next.min(track_end).max(clip.end())
    }

    pub(crate) fn play_edges(&self, deck: usize, from: u64, to: u64) -> Option<super::PlayEdges> {
        let i = self.find(deck, from, to)?;
        let block = u64::from(self.block.max(1));
        Some(super::PlayEdges {
            start: self.start_limits(i),
            end: (self.clips[i].at + block, self.end_limit(i)),
        })
    }

    /// Resizes the clip `from`–`to` on `deck` to `new_from`–`new_to`.
    pub(crate) fn resize(
        &mut self,
        deck: usize,
        (from, to): (u64, u64),
        (new_from, new_to): (u64, u64),
    ) -> Result<(u64, u64)> {
        let i = self.find(deck, from, to).ok_or_else(|| anyhow!("deck {} didn't play there", deck + 1))?;
        if new_from != from {
            self.trim_start(i, new_from)?;
        }
        if new_to != to {
            let end = new_to.min(self.end_limit(i));
            self.trim_end(i, end);
        }
        Ok((self.clips[i].at, self.clips[i].end()))
    }

    /// How far the clip `from`–`to` can move without running into the
    /// clips beside it.
    pub(crate) fn play_limits(&self, deck: usize, from: u64, to: u64) -> Option<(i64, i64)> {
        let i = self.find(deck, from, to)?;
        let clip = &self.clips[i];
        let before = self.on_deck(deck).filter(|(_, c)| c.end() <= clip.at).map(|(_, c)| c.end()).max().unwrap_or(0);
        let after = self.on_deck(deck).filter(|(_, c)| c.at >= clip.end()).map(|(_, c)| c.at).min();
        let later = after.map_or(i64::MAX / 4, |a| (a - clip.end()) as i64);
        Some((-((clip.at - before) as i64), later))
    }

    pub(crate) fn move_play(&mut self, deck: usize, from: u64, to: u64, by: i64) -> (u64, u64) {
        let Some((least, most)) = self.play_limits(deck, from, to) else { return (from, to) };
        let Some(i) = self.find(deck, from, to) else { return (from, to) };
        let length = self.clips[i].length;
        match self.move_clip(i, by.clamp(least, most)) {
            Ok(at) => (at, at + length),
            Err(_) => (from, to),
        }
    }

    /// Moves the clip on `deck` starting at (or playing at) `from`.
    pub(crate) fn move_clip_at(&mut self, deck: usize, from: u64, by: i64) -> Result<u64> {
        let i = self
            .on_deck(deck)
            .find(|(_, c)| c.at == from)
            .map(|(i, _)| i)
            .or_else(|| self.clip_at(deck, from))
            .ok_or_else(|| anyhow!("no clip on deck {} there", deck + 1))?;
        self.move_clip(i, by)
    }

    /// Lets only `pieces` of the clip starting at `from` be heard: the rest
    /// is deleted; pieces past its ends make it longer.
    pub(crate) fn set_pieces(&mut self, deck: usize, from: u64, pieces: &[(u64, u64)]) -> Result<()> {
        let Some(i) = self.on_deck(deck).find(|(_, c)| c.at == from).map(|(i, _)| i) else {
            return Ok(());
        };
        let mut pieces: Vec<(u64, u64)> =
            pieces.iter().map(|&(a, b)| (self.snap(a), self.snap(b))).filter(|p| p.1 > p.0).collect();
        pieces.sort_unstable();
        let (Some(&(first, _)), Some(&(_, last))) = (pieces.first(), pieces.last()) else {
            self.clips.remove(i);
            return Ok(());
        };
        if first < self.clips[i].at {
            self.trim_start(i, first)?;
        }
        if last > self.clips[i].end() {
            let end = last.min(self.end_limit(i));
            self.trim_end(i, end);
        }
        let (at, end) = (self.clips[i].at, self.clips[i].end());
        let mut gaps = Vec::new();
        let mut cut = Vec::new();
        let mut since = at;
        for &(a, b) in &pieces {
            if a > since {
                gaps.push((since, a));
            } else if a == since && a > at {
                cut.push(a);
            }
            since = since.max(b);
        }
        if since < end {
            gaps.push((since, end));
        }
        for (a, b) in gaps {
            self.delete(deck, a, b, DeleteOptions::default())?;
        }
        for a in cut {
            if self.clip_at(deck, a).is_some_and(|i| self.clips[i].at < a) {
                self.split(deck, a)?;
            }
        }
        Ok(())
    }

    /// The clips moved inside (their audio later or sooner), as the
    /// editor shows them.
    pub(crate) fn moves(&self) -> Vec<super::TrackMove> {
        self.clips
            .iter()
            .filter(|c| c.later != 0.0)
            .map(|c| super::TrackMove { deck: c.deck, at: c.at, later: c.later })
            .collect()
    }

    /// Plays `deck`'s track `later` seconds later from `at` (the clip there
    /// is split at `at`) to the end of its clip. Returns where it is.
    pub(crate) fn move_track(&mut self, deck: usize, at: u64, later: f64) -> u64 {
        let at = self.snap(at);
        let Some(mut i) = self.clip_at(deck, at) else { return at };
        if self.clips[i].at < at {
            match self.split(deck, at) {
                Ok(second) => i = second,
                Err(_) => return at,
            }
        }
        let tempo = lane_value(self.tempo(deck), self.clips[i].at).max(0.01);
        let clip = &mut self.clips[i];
        clip.source -= (later - clip.later) * tempo;
        clip.later = later;
        clip.at
    }

    pub(crate) fn set_lane(&mut self, setting: Setting, points: &[Point]) {
        let mut points: Vec<Point> = points.iter().map(|p| Point { at: self.snap(p.at), ..*p }).collect();
        points.sort_by_key(|p| p.at);
        // One point a moment: the last.
        let mut kept: Vec<Point> = Vec::with_capacity(points.len());
        for p in points {
            match kept.last_mut() {
                Some(last) if last.at == p.at => *last = p,
                _ => kept.push(p),
            }
        }
        self.lanes.insert(setting, kept);
    }

    pub(crate) fn set_beat_fx(&mut self, changes: &[(u64, BeatFxParams)]) {
        let mut changes: Vec<(u64, BeatFxParams)> = changes.iter().map(|&(at, p)| (self.snap(at), p)).collect();
        changes.sort_by_key(|c| c.0);
        changes.dedup_by(|b, a| a.1 == b.1);
        self.beat_fx = changes;
    }

    /// `deck`'s stem levels over the set (see [`Session::stem_levels`]),
    /// shared out to its clips.
    pub(crate) fn set_stem_levels(&mut self, deck: usize, changes: &[StemLevels]) {
        let changes: Vec<StemLevels> = changes.iter().map(|&(at, l)| (self.snap(at), l)).collect();
        for clip in self.clips.iter_mut().filter(|c| c.deck == deck) {
            let first = changes.iter().take_while(|c| c.0 <= clip.at).last().map_or(ALL_STEMS, |c| c.1);
            let mut levels = vec![(0, first)];
            levels.extend(
                changes.iter().filter(|c| clip.at < c.0 && c.0 < clip.end()).map(|&(at, l)| (at - clip.at, l)),
            );
            levels.dedup_by(|b, a| a.1 == b.1);
            clip.stem_levels = if levels == [(0, ALL_STEMS)] { Vec::new() } else { levels };
        }
    }

    /// Gives the clip on `deck` at `at` `stems` (remembered with their
    /// source).
    pub(crate) fn add_stems(&mut self, deck: usize, at: u64, stems: Arc<StemLayers>) {
        let Some(source) = source_of(&STEMS, &stems) else { return };
        let Some(i) = self.on_deck(deck).find(|(_, c)| c.at == at).map(|(i, _)| i).or_else(|| self.clip_at(deck, at))
        else {
            return;
        };
        self.stems.insert(source.clone(), stems);
        self.clips[i].stems = Some(source);
    }

    pub(crate) fn remove_markers(&mut self, from: u64, to: u64) -> usize {
        let before = self.markers.len();
        self.markers.retain(|m| !(from..=to).contains(m));
        before - self.markers.len()
    }

    /// Takes `deck`'s nudges in `gone` out of their clips; with
    /// `keep_step`, what each moved the track stays (the clip starts that
    /// much further on).
    pub(crate) fn remove_nudges(&mut self, gone: &[super::Nudge], keep_step: bool) -> usize {
        for n in gone {
            let Some(i) = self.clip_at(n.deck, n.from) else { continue };
            let clip = &mut self.clips[i];
            let (from, to) = (n.from - clip.at, n.to.saturating_sub(clip.at));
            clip.moves.retain(|&(off, m)| !(matches!(m, Move::Nudge { .. }) && from <= off && off <= to));
            if keep_step {
                clip.source += n.shift;
            }
        }
        gone.len()
    }

    pub(crate) fn backup(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&Backup {
            length: self.length,
            clips: self.clips.clone(),
            lanes: self.lanes.iter().map(|(s, p)| (*s, p.clone())).collect(),
            beat_fx: self.beat_fx.clone(),
            markers: self.markers.clone(),
        })?)
    }

    pub(crate) fn restore(&mut self, backup: &[u8]) -> Result<()> {
        let b: Backup = serde_json::from_slice(backup)?;
        (self.length, self.clips, self.beat_fx, self.markers) = (b.length, b.clips, b.beat_fx, b.markers);
        self.lanes = b.lanes.into_iter().collect();
        Ok(())
    }

    /// Writes the edit to `path`. A recording there is kept first, beside
    /// it as `<name>.session.orig` (once; never written over).
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        if path.exists() && !is_edit(path) {
            let orig = orig_path(path);
            if !orig.exists() {
                std::fs::copy(path, &orig).with_context(|| format!("keeping the recording as {}", orig.display()))?;
            }
        }
        self.write(path)
    }
}

impl Clip {
    fn empty() -> Self {
        Self {
            deck: 0,
            track: PathBuf::new(),
            stems: None,
            at: 0,
            length: 0,
            source: 0.0,
            key_lock: false,
            key_shift: 0.0,
            slip: false,
            vinyl: None,
            looped: None,
            moves: Vec::new(),
            stem_levels: Vec::new(),
            brake: false,
            later: 0.0,
            scratch: false,
            continues: false,
            continues_from: None,
            opening: Vec::new(),
            opening_from: 0.0,
        }
    }
}

/// Takes a lane's points from `from` to `to` out, keeping its value after
/// `to` as it was.
fn clear_range(points: &mut Vec<Point>, from: u64, to: u64) {
    let after = lane_value(points, to);
    let had_after = points.iter().any(|p| p.at > to);
    points.retain(|p| !(from <= p.at && p.at <= to));
    let before = lane_value(points, to);
    if (after - before).abs() > 1e-9
        || had_after && points.iter().all(|p| p.at < from) && after != before
    {
        points.push(Point::step(to, after));
        points.sort_by_key(|p| p.at);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Tracks, deck_session, rendered, worst};
    use super::*;

    const BLOCK: u64 = 512;

    fn play(at: u64, playing: bool) -> (u64, Command) {
        (at * BLOCK, Command::SetPlaying { deck: 0, playing })
    }

    fn cue(at: u64, seconds: f64) -> (u64, Command) {
        (at * BLOCK, Command::Seek { deck: 0, seconds })
    }

    /// The recording made into an edit plays as it did.
    #[test]
    fn an_edit_plays_as_the_recording() {
        let (session, _) = deck_session(
            "edit-same",
            vec![
                cue(5, 1.0),
                play(10, true),
                (30 * BLOCK, Command::SetNudge { deck: 0, amount: 0.05 }),
                (34 * BLOCK, Command::SetNudge { deck: 0, amount: 0.0 }),
                cue(80, 2.5),
                (120 * BLOCK, Command::SetLoop { deck: 0, start: 3.0, end: 3.25 }),
                (160 * BLOCK, Command::ClearLoop { deck: 0 }),
                (200 * BLOCK, Command::SetTempo { deck: 0, ratio: 1.04 }),
                play(300, false),
                cue(320, 0.5),
                play(330, true),
                play(400, false),
            ],
        );
        let edit = Edit::from_session(&session).unwrap();
        let spans: Vec<(u64, u64)> =
            edit.clips.iter().map(|c| (c.at / BLOCK, c.end() / BLOCK)).collect();
        // A play; a cue jump, a loop, out of it; another play.
        assert_eq!(spans.len(), 5, "{spans:?}");
        assert_eq!(edit.clips[2].looped, Some((3.0, 3.25)));
        assert!(edit.clips[0].moves.iter().any(|m| matches!(m.1, Move::Nudge { .. })));
        let (a, b) = (rendered(&session), rendered(&edit.to_session().unwrap()));
        assert!(a.iter().map(|s| s.abs()).fold(0.0, f32::max) > 0.1, "silent");
        assert_eq!(worst(&a, &b), 0.0);
    }

    /// The Empire case: the start of a set deleted (cue hits, scratching
    /// and all), then the clip after it stretched back over it plays the
    /// track straight on, as it was before it, and nothing deleted.
    #[test]
    fn a_clip_stretched_back_plays_its_track_not_what_was_deleted() {
        let mut events = vec![cue(2, 4.0), play(4, true)];
        // Cue hits and scratching before the drop.
        for k in 0..6u64 {
            events.push(cue(20 + k * 12, 4.0));
            events.push(((26 + k * 12) * BLOCK, Command::Hold { deck: 0, held: true }));
            events.push(((27 + k * 12) * BLOCK, Command::JogMove { deck: 0, seconds: -0.2 }));
            events.push(((29 + k * 12) * BLOCK, Command::Hold { deck: 0, held: false }));
        }
        // The drop: from 1 s into the track, played on.
        events.push(cue(100, 1.0));
        events.push(play(400, false));
        let (session, _) = deck_session("edit-empire", events);
        let mut edit = Edit::from_session(&session).unwrap();
        let drop = 100 * BLOCK;
        edit.delete(0, 0, drop, DeleteOptions::default()).unwrap();
        assert_eq!(edit.clips.len(), 1);
        assert_eq!(edit.clips[0].at, drop);
        // Back to where the track would have been 40 blocks sooner.
        let at = edit.trim_start(0, drop - 40 * BLOCK).unwrap();
        assert_eq!(at, drop - 40 * BLOCK);
        let secs = (40 * BLOCK) as f64 / 48_000.0;
        assert!((edit.clips[0].source - (1.0 - secs)).abs() < 1e-6, "{}", edit.clips[0].source);
        assert!(edit.clips[0].moves.is_empty());

        // As if the deck had played straight on from there.
        let (straight, _) = deck_session(
            "edit-empire-straight",
            vec![
                (at, Command::Seek { deck: 0, seconds: 1.0 - secs }),
                (at, Command::SetPlaying { deck: 0, playing: true }),
                play(400, false),
            ],
        );
        let (a, b) = (rendered(&edit.to_session().unwrap()), rendered(&straight));
        let from = (at * 2) as usize;
        let to = (300 * BLOCK * 2) as usize;
        assert!(worst(&a[from..to], &b[from..to]) < 1e-3, "{}", worst(&a[from..to], &b[from..to]));
        // Nothing heard before it.
        assert!(a[..from].iter().all(|s| s.abs() < 1e-6));
    }

    /// Split in two, a clip sounds the same; its moves go with each half.
    #[test]
    fn a_split_clip_sounds_the_same() {
        let (session, _) = deck_session(
            "edit-split",
            vec![
                cue(5, 1.0),
                play(10, true),
                (50 * BLOCK, Command::SetNudge { deck: 0, amount: 0.05 }),
                (54 * BLOCK, Command::SetNudge { deck: 0, amount: 0.0 }),
                (150 * BLOCK, Command::SetNudge { deck: 0, amount: -0.05 }),
                (154 * BLOCK, Command::SetNudge { deck: 0, amount: 0.0 }),
                play(300, false),
            ],
        );
        let mut edit = Edit::from_session(&session).unwrap();
        let before = rendered(&edit.to_session().unwrap());
        let second = edit.split(0, 100 * BLOCK).unwrap();
        assert_eq!(edit.clips.len(), 2);
        assert_eq!(edit.clips[0].moves.len(), 2);
        assert_eq!(edit.clips[second].moves.len(), 2);
        let after = rendered(&edit.to_session().unwrap());
        assert_eq!(worst(&before, &after), 0.0);
    }

    /// Deleting takes the deck's audio and moves; its lanes stay unless
    /// asked.
    #[test]
    fn deleting_keeps_the_lanes_unless_asked() {
        let (session, _) = deck_session(
            "edit-delete",
            vec![
                cue(5, 1.0),
                play(10, true),
                (60 * BLOCK, Command::SetEq { deck: 0, band: 0, db: -20.0 }),
                (70 * BLOCK, Command::SetNudge { deck: 0, amount: 0.05 }),
                (140 * BLOCK, Command::SetEq { deck: 0, band: 0, db: 0.0 }),
                play(300, false),
            ],
        );
        let mut edit = Edit::from_session(&session).unwrap();
        let mut keeps = edit.clone();
        keeps.delete(0, 50 * BLOCK, 100 * BLOCK, DeleteOptions::default()).unwrap();
        assert_eq!(keeps.clips.len(), 2);
        assert!(keeps.clips.iter().all(|c| c.moves.is_empty()));
        assert_eq!(keeps.lanes[&Setting::Eq(0, 0)].len(), edit.lanes[&Setting::Eq(0, 0)].len());
        edit.delete(
            0,
            50 * BLOCK,
            100 * BLOCK,
            DeleteOptions { eq_filter: true, ..Default::default() },
        )
        .unwrap();
        let eq = &edit.lanes[&Setting::Eq(0, 0)];
        assert!(
            eq.iter().all(|p| !(50 * BLOCK <= p.at && p.at <= 100 * BLOCK) || p.at == 100 * BLOCK)
        );
        // Still cut to -20 dB after the deletion, back up at 140.
        assert!((lane_value(eq, 120 * BLOCK) + 20.0).abs() < 1e-9);
        assert!(lane_value(eq, 150 * BLOCK).abs() < 1e-9);
    }

    /// Saved and read back, an edit plays the same.
    #[test]
    fn an_edit_is_saved_and_read_back() {
        let (session, file) =
            deck_session("edit-save", vec![cue(5, 1.0), play(10, true), play(200, false)]);
        let mut edit = Edit::from_session(&session).unwrap();
        edit.split(0, 100 * BLOCK).unwrap();
        edit.move_clip(1, 20 * BLOCK as i64).unwrap();
        let path = file.with_file_name("edited.session");
        edit.write(&path).unwrap();
        assert!(is_edit(&path));
        assert!(!is_edit(&file));
        let audio = edit.track_audio(&edit.clips[0].track).unwrap();
        let tracks = Tracks([(edit.clips[0].track.clone(), audio)].into_iter().collect());
        let read = Edit::read(&path, tracks).unwrap();
        assert_eq!(read.clips, edit.clips);
        let (a, b) = (rendered(&edit.to_session().unwrap()), rendered(&read.to_session().unwrap()));
        assert_eq!(worst(&a, &b), 0.0);
    }

    /// Through the session, as the editor does it: the start deleted, the
    /// play after it stretched back; saved over the recording, which is
    /// kept beside it.
    #[test]
    fn the_editor_edits_the_clips_and_keeps_the_recording() {
        let mut events = vec![cue(2, 4.0), play(4, true)];
        for k in 0..6u64 {
            events.push(cue(20 + k * 12, 4.0));
            events.push(((26 + k * 12) * BLOCK, Command::Hold { deck: 0, held: true }));
            events.push(((27 + k * 12) * BLOCK, Command::JogMove { deck: 0, seconds: -0.2 }));
            events.push(((29 + k * 12) * BLOCK, Command::Hold { deck: 0, held: false }));
        }
        events.push(cue(100, 1.0));
        events.push(play(400, false));
        let (mut session, file) = deck_session("edit-editor", events);
        session.write(&file).unwrap();
        let recorded = std::fs::read(&file).unwrap();
        session.make_editable(&file).unwrap();
        let drop = 100 * BLOCK;
        session.delete_range(0, 0, drop).unwrap();
        let spans = session.tracks();
        assert_eq!(spans.len(), 1);
        let play = spans[0].playing[0];
        assert_eq!(play.0, drop);
        let edges = session.play_edges(0, play.0, play.1).unwrap();
        assert!(edges.start.0 <= drop - 40 * BLOCK, "{edges:?}");
        let now = session.resize_play(0, play, (drop - 40 * BLOCK, play.1)).unwrap();
        assert_eq!(now, (drop - 40 * BLOCK, play.1));
        // Undo and redo.
        let backup = session.backup().unwrap();
        session.delete_range(0, 0, 300 * BLOCK).unwrap();
        assert!(session.tracks()[0].from >= 300 * BLOCK);
        session.restore(&backup).unwrap();
        assert_eq!(session.tracks()[0].from, drop - 40 * BLOCK);

        session.write(&file).unwrap();
        assert!(is_edit(&file));
        assert_eq!(std::fs::read(orig_path(&file)).unwrap(), recorded);
        // Saved again: the recording kept stays as it was.
        session.write(&file).unwrap();
        assert_eq!(std::fs::read(orig_path(&file)).unwrap(), recorded);
        let audio = session.track_audio(&spans[0].path).unwrap();
        let read = Session::read(&file, Tracks([(spans[0].path.clone(), audio)].into_iter().collect())).unwrap();
        assert!(read.editable());
        assert_eq!(read.tracks(), session.tracks());
        let (a, b) = (rendered(&session), rendered(&read));
        assert_eq!(worst(&a, &b), 0.0);
    }

    /// A scratch (hand on the record, let go, grabbed again soon) is a
    /// clip of its own between straight ones; it all sounds the same.
    #[test]
    fn a_scratch_is_a_clip_of_its_own() {
        let hold = |at: u64, held| (at * BLOCK, Command::Hold { deck: 0, held });
        let jog = |at: u64, seconds| (at * BLOCK, Command::JogMove { deck: 0, seconds });
        let (session, _) = deck_session(
            "edit-scratch",
            vec![
                cue(5, 1.0),
                play(10, true),
                hold(100, true),
                jog(102, -0.1),
                jog(104, 0.1),
                hold(106, false),
                // Grabbed again at once: the same scratch.
                hold(110, true),
                jog(112, -0.1),
                hold(116, false),
                play(300, false),
            ],
        );
        let edit = Edit::from_session(&session).unwrap();
        let shape: Vec<(u64, u64, bool)> =
            edit.clips.iter().map(|c| (c.at / BLOCK, c.end() / BLOCK, c.scratch)).collect();
        let start = |b: u64| b;
        assert_eq!(shape, vec![(start(10), 100, false), (100, 116, true), (116, 300, false)], "{shape:?}");
        assert!(edit.clips[0].moves.is_empty() && edit.clips[2].moves.iter().all(|m| m.0 == 0));
        let (a, b) = (rendered(&session), rendered(&edit.to_session().unwrap()));
        assert_eq!(worst(&a, &b), 0.0);
    }

    /// Untouched, an edit plays the recording sample for sample: cue
    /// stutters (play, stop back to the cue), a sync shift in the moment
    /// play is pressed, a slip hot cue held and let go.
    #[test]
    fn an_untouched_edit_is_exact() {
        let at = |b: u64, c| (b * BLOCK, c);
        let mut events = vec![cue(5, 1.0), (10 * BLOCK, Command::SetSlip { deck: 0, on: true })];
        for k in 0..3u64 {
            events.push(play(20 + k * 10, true));
            events.push(play(25 + k * 10, false));
            events.push(cue(25 + k * 10, 1.0));
        }
        events.push(play(60, true));
        events.push(at(60, Command::SyncShift { deck: 0, seconds: 0.07 }));
        events.push(at(120, Command::SlipHold { deck: 0, held: true }));
        events.push(cue(120, 1.0));
        events.push(play(120, true));
        events.push(at(135, Command::SlipHold { deck: 0, held: false }));
        events.push(play(300, false));
        let (session, _) = deck_session("edit-exact", events);
        let edit = Edit::from_session(&session).unwrap();
        let (a, b) = (rendered(&session), rendered(&edit.to_session().unwrap()));
        assert_eq!(a.len(), b.len());
        assert_eq!(worst(&a, &b), 0.0);
    }
}
