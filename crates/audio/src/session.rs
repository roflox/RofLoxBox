//! Sessions: a recording's performance, not just its sound.
//!
//! Next to the WAV, a recording keeps everything the mixer was told: where
//! it stood when recording started ([`Snapshot`]), then every command, jog
//! movement and audio buffer size, at the frame it took effect. Replaying
//! that through the same mixer, offline, plays the set again as it was.
//!
//! A session file has one JSON object per line: a `start` line with the
//! snapshot, then `block`, `command` and `jog` lines, and `end`. Tracks are
//! named by their file, stems by where deckhand's stem cache keeps them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::decode::TrackAudio;
use crate::engine::{Command, CrossfaderSide, Offline, Snapshot};
use crate::fx::{BeatFxKind, BeatFxParams, FxTarget};
use crate::stems::StemLayers;

const VERSION: u32 = 1;
/// Events the audio thread can log before the writer takes them.
const QUEUE: usize = 1 << 15;

/// Where a track's stems are: the stem files deckhand keeps for `track`,
/// in `dir`, made by a model with these `stems`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StemsSource {
    pub track: PathBuf,
    pub dir: PathBuf,
    pub stems: Vec<String>,
}

/// Tracks and stems handed to the engine, by what they were made from,
/// so a session can name them.
static TRACKS: Mutex<Vec<(Weak<TrackAudio>, PathBuf)>> = Mutex::new(Vec::new());
static STEMS: Mutex<Vec<(Weak<StemLayers>, StemsSource)>> = Mutex::new(Vec::new());
static GRIDS: Mutex<Vec<(Weak<TrackAudio>, Grid)>> = Mutex::new(Vec::new());

/// How far (in beats) a synced deck may drift from the master's beat
/// before sync puts it back, and how often it looks.
pub const SYNC_DRIFT_BEATS: f64 = 0.02;
pub const SYNC_CHECK_SECS: f64 = 0.5;

/// A track's beats, for sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grid {
    pub bpm: f64,
    /// When each beat is, in seconds.
    pub beats: Vec<f64>,
    /// Each beat's place in its bar (1 starts one), if known.
    #[serde(default)]
    pub bars: Vec<u16>,
}

impl Grid {
    fn beat_length(&self) -> f64 {
        if self.bpm > 0.0 { 60.0 / self.bpm } else { 0.5 }
    }

    /// Position in beats (fractional) at `seconds`.
    pub fn beat_at(&self, seconds: f64) -> f64 {
        let beats = &self.beats;
        if beats.len() < 2 {
            return seconds / self.beat_length();
        }
        let i = beats.partition_point(|&t| t <= seconds);
        let (a, b) = match i {
            0 => (0, 1),
            i if i >= beats.len() => (beats.len() - 2, beats.len() - 1),
            i => (i - 1, i),
        };
        a as f64 + (seconds - beats[a]) / (beats[b] - beats[a])
    }

    /// Where beat `beat` (fractional) is, in seconds.
    pub fn time_of_beat(&self, beat: f64) -> f64 {
        let beats = &self.beats;
        if beats.len() < 2 {
            return beat * self.beat_length();
        }
        let a = (beat.floor().max(0.0) as usize).min(beats.len() - 2);
        beats[a] + (beat - a as f64) * (beats[a + 1] - beats[a])
    }
}

/// Notes that `audio` was decoded from `path`. Call before it goes to the
/// engine, or sessions cannot name it.
pub fn remember_track(audio: &Arc<TrackAudio>, path: &Path) {
    remember(&TRACKS, audio, path.to_path_buf());
}

/// Notes `audio`'s beat grid, so sessions can redo sync.
pub fn remember_grid(audio: &Arc<TrackAudio>, grid: Grid) {
    remember(&GRIDS, audio, grid);
}

/// Notes where `stems` come from (see [`remember_track`]).
pub fn remember_stems(stems: &Arc<StemLayers>, source: StemsSource) {
    remember(&STEMS, stems, source);
}

fn remember<T, S>(list: &Mutex<Vec<(Weak<T>, S)>>, item: &Arc<T>, source: S) {
    let mut list = list.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(weak, _)| weak.strong_count() > 0);
    list.push((Arc::downgrade(item), source));
}

fn source_of<T, S: Clone>(list: &Mutex<Vec<(Weak<T>, S)>>, item: &Arc<T>) -> Option<S> {
    let list = list.lock().unwrap_or_else(|e| e.into_inner());
    list.iter()
        .find(|(weak, _)| std::ptr::eq(weak.as_ptr(), Arc::as_ptr(item)))
        .map(|(_, s)| s.clone())
}

/// Opens what a session names, for replaying it.
pub trait Sources {
    fn track(&mut self, path: &Path) -> Result<Arc<TrackAudio>>;
    /// `track` is the track as [`Sources::track`] opened it.
    fn stems(&mut self, source: &StemsSource, track: &TrackAudio) -> Result<Arc<StemLayers>>;
}

/// Decodes the tracks; has no stems.
pub struct Files;

impl Sources for Files {
    fn track(&mut self, path: &Path) -> Result<Arc<TrackAudio>> {
        Ok(Arc::new(crate::decode::decode_file(path)?))
    }

    fn stems(&mut self, source: &StemsSource, _: &TrackAudio) -> Result<Arc<StemLayers>> {
        if StemLayers::saved_in(&source.dir) {
            return Ok(Arc::new(StemLayers::load(&source.dir)?));
        }
        bail!("stems from the stem cache can only be opened by the app")
    }
}

/// The sources a session is being read with, each opened once.
struct Opened {
    sources: Box<dyn Sources>,
    tracks: HashMap<PathBuf, Arc<TrackAudio>>,
    stems: HashMap<StemsSource, Arc<StemLayers>>,
    warnings: Vec<String>,
}

thread_local! {
    static OPENED: RefCell<Option<Opened>> = const { RefCell::new(None) };
    /// The session writer's: where it keeps the stems a recording used.
    static KEPT: RefCell<HashMap<StemsSource, StemsSource>> = RefCell::new(HashMap::new());
}

/// A track's audio, by its file.
fn track_at(path: &Path) -> Option<Arc<TrackAudio>> {
    let list = TRACKS.lock().unwrap_or_else(|e| e.into_inner());
    list.iter().find(|(_, p)| p == path).and_then(|(weak, _)| weak.upgrade())
}

/// Keeps the stems a recording's decks use in its folder (`stems/` next
/// to the session), each as the deck had it when it let go of them.
struct Keeper {
    dir: PathBuf,
    decks: [Option<(Arc<StemLayers>, PathBuf, u32)>; crate::engine::DECKS],
    saving: Vec<(PathBuf, JoinHandle<Result<()>>)>,
}

impl Keeper {
    fn new(dir: PathBuf) -> Self {
        KEPT.with_borrow_mut(HashMap::clear);
        Self { dir, decks: Default::default(), saving: Vec::new() }
    }

    /// A deck takes `stems`: they will be kept, and the session names
    /// them where.
    fn take(&mut self, deck: usize, stems: &Arc<StemLayers>) {
        let Some(source) = source_of(&STEMS, stems) else { return };
        let kept = KEPT.with_borrow_mut(|kept| {
            if let Some(kept) = kept.get(&source) {
                return kept.clone();
            }
            let name = source.track.file_stem().unwrap_or_default().to_string_lossy();
            let taken = |dir: &PathBuf| kept.values().any(|k| &k.dir == dir);
            let dir = (1..)
                .map(|n| match n {
                    1 => self.dir.join(name.as_ref()),
                    n => self.dir.join(format!("{name} {n}")),
                })
                .find(|dir| !taken(dir))
                .unwrap_or_default();
            let new = StemsSource { dir, ..source.clone() };
            kept.insert(source.clone(), new.clone());
            new
        });
        let rate = track_at(&source.track).map_or(44_100, |audio| audio.sample_rate);
        if let Some(deck) = self.decks.get_mut(deck) {
            *deck = Some((stems.clone(), kept.dir, rate));
        }
    }

    /// The deck lets go of its stems: they are written, as they are now.
    fn let_go(&mut self, deck: usize) {
        let Some((stems, dir, rate)) = self.decks.get_mut(deck).and_then(Option::take) else {
            return;
        };
        // The same stems kept before: that first, then these over it.
        if let Some(i) = self.saving.iter().position(|(d, _)| *d == dir) {
            let (_, earlier) = self.saving.remove(i);
            let _ = earlier.join();
        }
        let to = dir.clone();
        self.saving.push((dir, std::thread::spawn(move || stems.save(&to, rate))));
    }

    fn finish(mut self) -> Result<()> {
        for deck in 0..self.decks.len() {
            self.let_go(deck);
        }
        for (dir, saving) in self.saving {
            saving
                .join()
                .map_err(|_| anyhow!("keeping stems stopped unexpectedly"))?
                .with_context(|| format!("keeping the stems in {}", dir.display()))?;
        }
        Ok(())
    }
}

fn open_track(path: &Path) -> Result<Arc<TrackAudio>> {
    OPENED.with_borrow_mut(|opened| {
        let opened = opened.as_mut().ok_or_else(|| anyhow!("no sources to open tracks with"))?;
        if let Some(audio) = opened.tracks.get(path) {
            return Ok(audio.clone());
        }
        let audio =
            opened.sources.track(path).with_context(|| format!("opening {}", path.display()))?;
        remember_track(&audio, path);
        opened.tracks.insert(path.to_path_buf(), audio.clone());
        Ok(audio)
    })
}

/// A track's stems. Stems that are gone become stems not separated yet,
/// with a warning: the whole track plays where its parts were turned up or
/// down, and the deck limits its peaks as with stems (as it did live while
/// they were being separated).
fn open_stems(source: &StemsSource) -> Result<Option<Arc<StemLayers>>> {
    let track = open_track(&source.track)?;
    OPENED.with_borrow_mut(|opened| {
        let opened = opened.as_mut().ok_or_else(|| anyhow!("no sources to open stems with"))?;
        if let Some(stems) = opened.stems.get(source) {
            return Ok(Some(stems.clone()));
        }
        match opened.sources.stems(source, &track) {
            Ok(stems) => {
                remember_stems(&stems, source.clone());
                opened.stems.insert(source.clone(), stems.clone());
                Ok(Some(stems))
            }
            Err(err) => {
                let name = source.track.file_name().unwrap_or_default().to_string_lossy();
                let warning = format!("no stems for {name}: {err:#}");
                if !opened.warnings.contains(&warning) {
                    opened.warnings.push(warning);
                }
                let frames = track.frames();
                let stems = Arc::new(StemLayers::empty([false; 3], frames, frames));
                remember_stems(&stems, source.clone());
                opened.stems.insert(source.clone(), stems.clone());
                Ok(Some(stems))
            }
        }
    })
}

/// Serde for the tracks in commands and snapshots: their file, empty for
/// one that did not come from a file.
pub(crate) mod track {
    use super::*;

    pub fn serialize<S: serde::Serializer>(
        audio: &Arc<TrackAudio>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        source_of(&TRACKS, audio).unwrap_or_default().serialize(s)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Arc<TrackAudio>, D::Error> {
        open(PathBuf::deserialize(d)?)
    }

    pub(super) fn open<E: serde::de::Error>(path: PathBuf) -> Result<Arc<TrackAudio>, E> {
        if path.as_os_str().is_empty() {
            return Err(E::custom("a track that did not come from a file"));
        }
        open_track(&path).map_err(|e| E::custom(format!("{e:#}")))
    }
}

pub(crate) mod track_opt {
    use super::*;

    pub fn serialize<S: serde::Serializer>(
        audio: &Option<Arc<TrackAudio>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        audio.as_ref().map(|a| source_of(&TRACKS, a).unwrap_or_default()).serialize(s)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Arc<TrackAudio>>, D::Error> {
        Option::<PathBuf>::deserialize(d)?.map(super::track::open).transpose()
    }
}

/// Serde for stems: where they are kept.
pub(crate) mod stems {
    use super::*;
    use serde::de::Error;

    pub fn serialize<S: serde::Serializer>(
        stems: &Option<Arc<StemLayers>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        // Kept with the recording, where the writer keeps them.
        let source = stems.as_ref().and_then(|stems| source_of(&STEMS, stems));
        source.map(|s| KEPT.with_borrow(|kept| kept.get(&s).cloned().unwrap_or(s))).serialize(s)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Arc<StemLayers>>, D::Error> {
        match Option::<StemsSource>::deserialize(d)? {
            None => Ok(None),
            Some(source) => open_stems(&source).map_err(|e| D::Error::custom(format!("{e:#}"))),
        }
    }
}

/// Serde for arrays longer than serde's own (32).
pub(crate) mod array {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, T: Serialize, const N: usize>(
        items: &[T; N],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_seq(items)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>, const N: usize>(
        d: D,
    ) -> Result<[T; N], D::Error> {
        let items = Vec::<T>::deserialize(d)?;
        let len = items.len();
        items.try_into().map_err(|_| D::Error::invalid_length(len, &"a full array"))
    }
}

/// What the audio thread logs, at the frame it happened.
pub(crate) enum Logged {
    /// From here on, buffers this many frames long.
    Block(u32),
    Command(Command),
    Jog(u8, i16),
    End,
}

/// The audio thread's end of a session being written.
pub(crate) struct SessionTap {
    events: rtrb::Producer<(u64, Logged)>,
    snapshot: rtrb::Producer<Box<Snapshot>>,
    /// Filled in place when recording starts, so the audio thread does not
    /// allocate.
    spare: Option<Box<Snapshot>>,
    block: u32,
    lost: Arc<AtomicU64>,
}

impl SessionTap {
    /// Takes the mixer's state at the first recorded frame.
    pub(crate) fn start(&mut self, fill: impl FnOnce(&mut Snapshot)) {
        if let Some(mut snapshot) = self.spare.take() {
            fill(&mut snapshot);
            let _ = self.snapshot.push(snapshot);
        }
    }

    pub(crate) fn command(&mut self, at: u64, command: Command) {
        self.log(at, Logged::Command(command));
    }

    pub(crate) fn jog(&mut self, at: u64, deck: u8, steps: i16) {
        self.log(at, Logged::Jog(deck, steps));
    }

    /// A buffer of `frames` starts at `at`.
    pub(crate) fn block(&mut self, at: u64, frames: usize) {
        let frames = frames as u32;
        if frames != self.block {
            self.block = frames;
            self.log(at, Logged::Block(frames));
        }
    }

    pub(crate) fn end(&mut self, at: u64) {
        self.log(at, Logged::End);
    }

    fn log(&mut self, at: u64, event: Logged) {
        if self.events.push((at, event)).is_err() {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// An event as a [`Line`], borrowed (for backups).
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum LineRef<'a> {
    Block { at: u64, frames: u32 },
    Command { at: u64, command: &'a Command },
    Jog { at: u64, deck: u8, steps: i16 },
    End { at: u64 },
}

/// Which deck an event is for, if one.
fn deck_of(event: &Logged) -> Option<usize> {
    use Command as C;
    match event {
        Logged::Jog(deck, _) => Some(usize::from(*deck)),
        Logged::Command(
            C::Load { deck, .. }
            | C::Unload { deck }
            | C::SetPlaying { deck, .. }
            | C::Seek { deck, .. }
            | C::Shift { deck, .. }
            | C::SetTempo { deck, .. }
            | C::SetNudge { deck, .. }
            | C::SetKeyLock { deck, .. }
            | C::Hold { deck, .. }
            | C::JogMove { deck, .. }
            | C::SetSlip { deck, .. }
            | C::SlipHold { deck, .. }
            | C::SetKeyShift { deck, .. }
            | C::SetStems { deck, .. }
            | C::SetStemLevels { deck, .. }
            | C::SetVinylSpeed { deck, .. }
            | C::SetLoop { deck, .. }
            | C::ClearLoop { deck }
            | C::SetVolume { deck, .. }
            | C::SetTrim { deck, .. }
            | C::SetEq { deck, .. }
            | C::SetFilter { deck, .. }
            | C::SetColorAmount { deck, .. }
            | C::SetCrossfaderAssign { deck, .. }
            | C::SetHeadphoneCue { deck, .. }
            | C::SyncTempo { deck, .. }
            | C::SyncShift { deck, .. }
            | C::SyncOffset { deck, .. }
            | C::MoveTrack { deck, .. }
            | C::Gate { deck, .. },
        ) => Some(*deck),
        _ => None,
    }
}

/// One line of a session file.
#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Line {
    Start {
        version: u32,
        rate: u32,
        snapshot: Box<Snapshot>,
    },
    Block {
        at: u64,
        frames: u32,
    },
    Command {
        at: u64,
        command: Command,
    },
    Jog {
        at: u64,
        deck: u8,
        steps: i16,
    },
    End {
        at: u64,
    },
    /// Events the writer could not keep up with: the replay will differ.
    Lost {
        count: u64,
    },
    /// A track's beat grid.
    Beats {
        path: PathBuf,
        grid: Grid,
    },
    /// A lane as edited, with its curves.
    Lane {
        setting: Setting,
        points: Vec<Point>,
    },
}

/// Starts writing a session to `path`; returns the audio thread's end and
/// the writer.
pub(crate) fn start(path: &Path, rate: u32) -> Result<(SessionTap, JoinHandle<Result<()>>)> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let (events, events_rx) = rtrb::RingBuffer::new(QUEUE);
    let (snapshot, snapshot_rx) = rtrb::RingBuffer::new(1);
    let lost = Arc::new(AtomicU64::new(0));
    let tap = SessionTap {
        events,
        snapshot,
        spare: Some(crate::engine::blank_snapshot(rate)),
        block: 0,
        lost: lost.clone(),
    };
    let writer = std::thread::Builder::new().name("session".into()).spawn({
        let stems = path.parent().unwrap_or(Path::new(".")).join("stems");
        move || write(file, rate, snapshot_rx, events_rx, &lost, stems)
    })?;
    Ok((tap, writer))
}

fn write(
    file: File,
    rate: u32,
    mut snapshot: rtrb::Consumer<Box<Snapshot>>,
    mut events: rtrb::Consumer<(u64, Logged)>,
    lost: &AtomicU64,
    stems: PathBuf,
) -> Result<()> {
    let mut out = BufWriter::with_capacity(1 << 16, file);
    let mut keeper = Keeper::new(stems);
    let line = |out: &mut BufWriter<File>, line: Line| -> Result<()> {
        serde_json::to_writer(&mut *out, &line)?;
        out.write_all(b"\n")?;
        Ok(())
    };
    let snapshot = loop {
        if let Ok(snapshot) = snapshot.pop() {
            break snapshot;
        }
        if snapshot.is_abandoned() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // Each track's beat grid, once, before it plays.
    let mut grids = std::collections::HashSet::new();
    let mut grid = |out: &mut BufWriter<File>, audio: &Arc<TrackAudio>| -> Result<()> {
        if let (Some(path), Some(grid)) = (source_of(&TRACKS, audio), source_of(&GRIDS, audio))
            && grids.insert(path.clone())
        {
            line(out, Line::Beats { path, grid })?;
        }
        Ok(())
    };
    let decks: Vec<_> =
        (0..crate::engine::DECKS).filter_map(|d| snapshot.deck(d).0.cloned()).collect();
    for deck in 0..crate::engine::DECKS {
        if let Some(stems) = snapshot.stems(deck) {
            keeper.take(deck, stems);
        }
    }
    line(&mut out, Line::Start { version: VERSION, rate, snapshot })?;
    for audio in &decks {
        grid(&mut out, audio)?;
    }
    let mut repeats = Repeats::default();
    loop {
        let Ok((at, event)) = events.pop() else {
            if events.is_abandoned() {
                break;
            }
            out.flush()?;
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let next = match event {
            Logged::Block(frames) => Line::Block { at, frames },
            Logged::Command(command) => {
                if repeats.unchanged(&command) {
                    continue;
                }
                match &command {
                    Command::Load { deck, audio } => {
                        grid(&mut out, audio)?;
                        keeper.let_go(*deck);
                    }
                    Command::SetStems { deck, stems } => {
                        keeper.let_go(*deck);
                        if let Some(stems) = stems {
                            keeper.take(*deck, stems);
                        }
                    }
                    _ => {}
                }
                Line::Command { at, command }
            }
            Logged::Jog(deck, steps) => Line::Jog { at, deck, steps },
            Logged::End => Line::End { at },
        };
        line(&mut out, next)?;
    }
    let count = lost.load(Ordering::Relaxed);
    if count > 0 {
        line(&mut out, Line::Lost { count })?;
    }
    out.flush()?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    keeper.finish()
}

/// The app sends a deck's settings again whenever one of them changes;
/// those that did not change are left out of the session.
#[derive(Default)]
struct Repeats {
    last: HashMap<(std::mem::Discriminant<Command>, usize, usize), String>,
}

/// Commands for no deck.
const ALL: usize = usize::MAX;

impl Repeats {
    fn unchanged(&mut self, command: &Command) -> bool {
        if let Command::Load { deck, .. } = *command {
            // Loading resets the deck's nudge, stem levels and more.
            self.last.retain(|&(_, d, _), _| d != deck);
            return false;
        }
        let Some((deck, part)) = Self::setting(command) else { return false };
        let Ok(text) = serde_json::to_string(command) else { return false };
        // Sync's tempo and the fader's set the same thing.
        let kind = match command {
            Command::SyncTempo { .. } => {
                std::mem::discriminant(&Command::SetTempo { deck: 0, ratio: 1.0 })
            }
            _ => std::mem::discriminant(command),
        };
        let key = (kind, deck, part);
        if self.last.get(&key) == Some(&text) {
            return true;
        }
        self.last.insert(key, text);
        false
    }

    /// Commands that only set something, so repeating one changes nothing:
    /// which deck (or [`ALL`]) and part.
    fn setting(command: &Command) -> Option<(usize, usize)> {
        use Command as C;
        Some(match *command {
            C::SetTempo { deck, .. }
            | C::SyncTempo { deck, .. }
            | C::SyncOffset { deck, .. }
            | C::SetNudge { deck, .. }
            | C::SetKeyLock { deck, .. }
            | C::SetSlip { deck, .. }
            | C::SlipHold { deck, .. }
            | C::SetKeyShift { deck, .. }
            | C::SetStemLevels { deck, .. }
            | C::SetVinylSpeed { deck, .. }
            | C::SetVolume { deck, .. }
            | C::SetTrim { deck, .. }
            | C::SetFilter { deck, .. }
            | C::SetCrossfaderAssign { deck, .. }
            | C::SetHeadphoneCue { deck, .. } => (deck, 0),
            C::SetEq { deck, band, .. } => (deck, band),
            C::SetColorAmount { deck, kind, .. } => (deck, kind.index()),
            C::SetQuality(_)
            | C::SetMasterCue(_)
            | C::SetColorFx(_)
            | C::SetColorFxOn(_)
            | C::SetBeatFx(_)
            | C::SetBeatFxDepth(_)
            | C::SetSamplerVolume(_)
            | C::SetEqSettings(_)
            | C::SetBendSensitivity(_)
            | C::SetJogResolution(_)
            | C::SetCrossfaderCurve(_)
            | C::SetFaderCurve(_)
            | C::SetLimiter(_)
            | C::SetCrossfader(_)
            | C::SetMasterVolume(_)
            | C::SetHeadphoneMix(_)
            | C::SetHeadphoneVolume(_)
            | C::SyncState { .. } => (ALL, 0),
            _ => return None,
        })
    }
}

/// A knob or fader a session can be edited by: one lane of automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Setting {
    Volume(usize),
    Trim(usize),
    /// Band 0 = low, 1 = mid, 2 = high.
    Eq(usize, usize),
    /// The channel's Color FX knob as played live (sessions are given
    /// [`Setting::ColorFx`] lanes instead when edited).
    Filter(usize),
    /// One Color FX type's amount on a channel.
    ColorFx(usize, crate::fx::ColorFxKind),
    Tempo(usize),
    Crossfader,
    Master,
    /// The Beat FX depth (wet/dry), shown inside the effects' blocks.
    BeatFxDepth,
}

impl Setting {
    /// Every setting of `decks` decks, in the order they are shown.
    pub fn all(decks: usize) -> Vec<Self> {
        let mut all = Vec::new();
        for d in 0..decks {
            all.extend([
                Self::Volume(d),
                Self::Trim(d),
                Self::Eq(d, 2),
                Self::Eq(d, 1),
                Self::Eq(d, 0),
            ]);
            all.extend(crate::fx::ColorFxKind::ALL.map(|kind| Self::ColorFx(d, kind)));
            all.push(Self::Tempo(d));
        }
        all.extend([Self::Crossfader, Self::Master]);
        all
    }

    /// The setting a command sets, and to what.
    pub fn of(command: &Command) -> Option<(Self, f64)> {
        Some(match *command {
            Command::SetVolume { deck, level } => (Self::Volume(deck), f64::from(level)),
            Command::SetTrim { deck, gain } => (Self::Trim(deck), f64::from(gain)),
            Command::SetEq { deck, band, db } => (Self::Eq(deck, band.min(2)), f64::from(db)),
            Command::SetFilter { deck, position } => (Self::Filter(deck), f64::from(position)),
            Command::SetColorAmount { deck, kind, position } => {
                (Self::ColorFx(deck, kind), f64::from(position))
            }
            Command::SetTempo { deck, ratio } | Command::SyncTempo { deck, ratio } => {
                (Self::Tempo(deck), ratio)
            }
            Command::SetCrossfader(position) => (Self::Crossfader, f64::from(position)),
            Command::SetMasterVolume(level) => (Self::Master, f64::from(level)),
            Command::SetBeatFxDepth(depth) => (Self::BeatFxDepth, f64::from(depth)),
            _ => return None,
        })
    }

    /// The command that sets it to `value`.
    pub fn command(self, value: f64) -> Command {
        let v = value as f32;
        match self {
            Self::Volume(deck) => Command::SetVolume { deck, level: v },
            Self::Trim(deck) => Command::SetTrim { deck, gain: v },
            Self::Eq(deck, band) => Command::SetEq { deck, band, db: v },
            Self::Filter(deck) => Command::SetFilter { deck, position: v },
            Self::ColorFx(deck, kind) => Command::SetColorAmount { deck, kind, position: v },
            Self::Tempo(deck) => Command::SetTempo { deck, ratio: value },
            Self::Crossfader => Command::SetCrossfader(v),
            Self::Master => Command::SetMasterVolume(v),
            Self::BeatFxDepth => Command::SetBeatFxDepth(v),
        }
    }

    pub fn deck(self) -> Option<usize> {
        match self {
            Self::Volume(d)
            | Self::Trim(d)
            | Self::Eq(d, _)
            | Self::Filter(d)
            | Self::ColorFx(d, _)
            | Self::Tempo(d) => Some(d),
            Self::Crossfader | Self::Master | Self::BeatFxDepth => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Volume(_) => "Volume",
            Self::Trim(_) => "Trim",
            Self::Eq(_, 2) => "EQ high",
            Self::Eq(_, 1) => "EQ mid",
            Self::Eq(_, _) => "EQ low",
            Self::Filter(_) => "Color FX",
            Self::ColorFx(_, kind) => match kind {
                crate::fx::ColorFxKind::Filter => "Filter intensity",
                crate::fx::ColorFxKind::Space => "Space intensity",
                crate::fx::ColorFxKind::DubEcho => "Dub Echo intensity",
                crate::fx::ColorFxKind::Crush => "Crush intensity",
                crate::fx::ColorFxKind::Noise => "Noise intensity",
                crate::fx::ColorFxKind::Pitch => "Pitch intensity",
            },
            Self::Tempo(_) => "Tempo",
            Self::Crossfader => "Crossfader",
            Self::Master => "Master",
            Self::BeatFxDepth => "Beat FX depth",
        }
    }

    /// The knob's travel, bottom to top.
    pub fn range(self) -> (f64, f64) {
        match self {
            Self::Volume(_) | Self::Master | Self::BeatFxDepth => (0.0, 1.0),
            Self::Trim(_) => (0.0, 4.0),
            Self::Eq(..) => (f64::from(crate::eq::EQ_MIN_DB), f64::from(crate::eq::EQ_MAX_DB)),
            Self::Filter(_) | Self::ColorFx(..) | Self::Crossfader => (-1.0, 1.0),
            Self::Tempo(_) => (0.84, 1.16),
        }
    }

    /// Where the knob rests.
    pub fn neutral(self) -> f64 {
        match self {
            Self::Volume(_) | Self::Master | Self::Trim(_) | Self::Tempo(_) => 1.0,
            Self::Eq(..) | Self::Filter(_) | Self::ColorFx(..) | Self::Crossfader => 0.0,
            Self::BeatFxDepth => 0.5,
        }
    }

    /// A value as the knob shows it.
    pub fn display(self, value: f64) -> String {
        match self {
            Self::Volume(_) | Self::Master | Self::BeatFxDepth => format!("{:.0}%", value * 100.0),
            Self::Trim(_) => format!("{:+.1} dB", 20.0 * value.max(1e-6).log10()),
            Self::Eq(..) => format!("{value:+.1} dB"),
            Self::Filter(_) | Self::Crossfader => format!("{value:+.2}"),
            Self::ColorFx(..) => format!("{:+.0}%", value * 100.0),
            Self::Tempo(_) => format!("{:+.2}%", (value - 1.0) * 100.0),
        }
    }
}

/// A setting's value from a moment on; `at` is in frames from the start
/// of the recording.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub at: u64,
    pub value: f64,
    /// How it goes on to the next point.
    #[serde(default)]
    pub curve: Curve,
}

impl Point {
    pub fn step(at: u64, value: f64) -> Self {
        Self { at, value, curve: Curve::Step }
    }
}

/// How a lane goes from one point to the next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Curve {
    /// Holds the value, then jumps (as knobs are recorded).
    #[default]
    Step,
    /// A straight line.
    Line,
    /// Eases out of one value and into the next (an S-curve).
    Smooth,
}

impl Curve {
    /// How far along the way (0–1) it is `x` (0–1) of the way.
    pub fn ease(self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Self::Step => 0.0,
            Self::Line => x,
            Self::Smooth => x * x * (3.0 - 2.0 * x),
        }
    }
}

/// A lane's value at `at` (frames from the start), curves followed.
pub fn lane_value(points: &[Point], at: u64) -> f64 {
    let index = points.partition_point(|p| p.at <= at);
    let Some(point) = index.checked_sub(1).and_then(|i| points.get(i)) else {
        return points.first().map_or(0.0, |p| p.value);
    };
    match points.get(index) {
        Some(next) if point.curve != Curve::Step && next.at > point.at => {
            let x = (at - point.at) as f64 / (next.at - point.at) as f64;
            point.value + (next.value - point.value) * point.curve.ease(x)
        }
        _ => point.value,
    }
}

/// A stretch with the Beat FX on, in frames from the start.
#[derive(Debug, Clone, PartialEq)]
pub struct FxSpan {
    pub from: u64,
    pub to: u64,
    pub kind: BeatFxKind,
    pub target: FxTarget,
    /// The effect's beat length (beat × multiplier) in seconds.
    pub beat_secs: f32,
    /// Release FX: the sound cut, the echo playing out.
    pub release: bool,
    /// The depth knob along it.
    pub depth: Vec<(u64, f32)>,
}

impl FxSpan {
    /// Whether `params` are this effect's (another type, target, beat or
    /// Release FX makes another block).
    fn same_effect(&self, params: &BeatFxParams) -> bool {
        (self.kind, self.target, self.release) == (params.kind, params.target, params.release)
            && self.beat_secs == params.beat_secs
    }
}

fn fx_on(params: &BeatFxParams) -> bool {
    params.on || params.release
}

/// The stretches with the Beat FX on, from its changes (as
/// [`Session::beat_fx`] gives them) over `length` frames.
pub fn fx_spans(changes: &[(u64, BeatFxParams)], length: u64) -> Vec<FxSpan> {
    let mut spans = Vec::new();
    let mut open: Option<FxSpan> = None;
    let start = |at: u64, params: &BeatFxParams| FxSpan {
        from: at,
        to: at,
        kind: params.kind,
        target: params.target,
        beat_secs: params.beat_secs,
        release: params.release,
        depth: vec![(at, params.depth)],
    };
    for &(at, params) in changes {
        match (open.as_mut(), fx_on(&params)) {
            (None, true) => open = Some(start(at, &params)),
            // Another effect right after: a block of its own.
            (Some(span), true) if !span.same_effect(&params) => {
                spans.extend(open.take().map(|span| FxSpan { to: at, ..span }));
                open = Some(start(at, &params));
            }
            (Some(span), true) => {
                if span.depth.last().is_none_or(|d| d.1 != params.depth) {
                    span.depth.push((at, params.depth));
                }
            }
            (Some(_), false) => {
                spans.extend(open.take().map(|span| FxSpan { to: at, ..span }));
            }
            (None, false) => {}
        }
    }
    spans.extend(open.map(|span| FxSpan { to: length, ..span }));
    spans
}

/// The changes with `span` moved to `from`–`to` (stretched to fit): what
/// it did goes along, what was there before gives way.
pub fn move_fx(
    changes: &[(u64, BeatFxParams)],
    span: &FxSpan,
    from: u64,
    to: u64,
) -> Vec<(u64, BeatFxParams)> {
    let length = (span.to - span.from).max(1) as f64;
    let place = |at: u64| from + ((at - span.from) as f64 / length * (to - from) as f64) as u64;
    let own = |&(at, params): &(u64, BeatFxParams)| span_owns(span, at, &params);
    let mut moved: Vec<_> =
        changes.iter().filter(|c| !own(c) && !(c.0 >= from && c.0 <= to)).copied().collect();
    let mine: Vec<(u64, BeatFxParams)> =
        changes.iter().filter(|c| own(c)).map(|&(at, p)| (place(at), p)).collect();
    // A block that ran into the next one has no end of its own: it ends
    // where it was moved to.
    if let Some(&(_, last)) = mine.last()
        && fx_on(&last)
    {
        moved.push((to, BeatFxParams { on: false, release: false, ..last }));
    }
    moved.extend(mine);
    moved.sort_by_key(|(at, _)| *at);
    moved
}

/// Whether a change is `span`'s own: in it, or its end (not the next
/// block starting where it ends).
fn span_owns(span: &FxSpan, at: u64, params: &BeatFxParams) -> bool {
    at >= span.from && (at < span.to || at == span.to && !fx_on(params))
}

/// The changes with `span`'s effect type, what it is on or its beat length
/// changed, also while its echoes ring out after it.
pub fn restyle_fx(
    changes: &[(u64, BeatFxParams)],
    span: &FxSpan,
    kind: Option<BeatFxKind>,
    target: Option<FxTarget>,
    beat_secs: Option<f32>,
) -> Vec<(u64, BeatFxParams)> {
    let next_on = changes
        .iter()
        .find(|(at, params)| *at >= span.to && fx_on(params))
        .map_or(u64::MAX, |(at, _)| *at);
    changes
        .iter()
        .map(|&(at, mut params)| {
            if at >= span.from && at < next_on {
                params.kind = kind.unwrap_or(params.kind);
                params.target = target.unwrap_or(params.target);
                params.beat_secs = beat_secs.unwrap_or(params.beat_secs);
            }
            (at, params)
        })
        .collect()
}

/// The changes with the effect off through `span`.
pub fn remove_fx(changes: &[(u64, BeatFxParams)], span: &FxSpan) -> Vec<(u64, BeatFxParams)> {
    changes
        .iter()
        .map(|&(at, mut params)| {
            if span_owns(span, at, &params) {
                (params.on, params.release) = (false, false);
            }
            (at, params)
        })
        .collect()
}

/// A track moved in the editor: from `at` (frames from the start) on,
/// `deck`'s track plays `later` seconds later.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackMove {
    pub deck: usize,
    pub at: u64,
    pub later: f64,
}

/// A track on a deck in a session, in frames from the start.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackSpan {
    pub deck: usize,
    /// Empty for a track that did not come from a file.
    pub path: PathBuf,
    pub from: u64,
    pub to: u64,
    /// When it was playing (the deck's play button).
    pub playing: Vec<(u64, u64)>,
}

/// A recorded session, read back with its tracks.
pub struct Session {
    rate: u32,
    snapshot: Box<Snapshot>,
    events: Vec<(u64, Logged)>,
    end: Option<u64>,
    lost: u64,
    warnings: Vec<String>,
    /// What it plays, as opened.
    tracks: HashMap<PathBuf, Arc<TrackAudio>>,
    stems: HashMap<StemsSource, Arc<StemLayers>>,
    /// The tracks' beat grids, by file.
    grids: HashMap<PathBuf, Grid>,
    /// Lanes as edited, with their curves (the commands follow them).
    curves: std::collections::BTreeMap<Setting, Vec<Point>>,
}

impl Session {
    /// Reads a session, opening its tracks and stems with `sources`.
    pub fn read(path: &Path, sources: impl Sources + 'static) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let opened = Opened {
            sources: Box::new(sources),
            tracks: HashMap::new(),
            stems: HashMap::new(),
            warnings: Vec::new(),
        };
        OPENED.set(Some(opened));
        let read = Self::parse(BufReader::new(file));
        let opened = OPENED.take();
        let mut session = read.with_context(|| format!("reading {}", path.display()))?;
        if let Some(opened) = opened {
            (session.warnings, session.tracks, session.stems) =
                (opened.warnings, opened.tracks, opened.stems);
        }
        Ok(session)
    }

    fn parse(file: impl BufRead) -> Result<Self> {
        let mut lines = file.lines().enumerate();
        let (rate, snapshot) = match lines.next() {
            Some((_, line)) => match serde_json::from_str(&line?)? {
                Line::Start { version, rate, snapshot } => {
                    if version > VERSION {
                        bail!("made by a newer version of the app");
                    }
                    (rate, snapshot)
                }
                _ => bail!("not a session: it does not start with one"),
            },
            None => bail!("the session is empty"),
        };
        let mut session = Self {
            rate,
            snapshot,
            events: Vec::new(),
            end: None,
            lost: 0,
            warnings: Vec::new(),
            tracks: HashMap::new(),
            stems: HashMap::new(),
            grids: HashMap::new(),
            curves: Default::default(),
        };
        for (index, line) in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let line: Line =
                serde_json::from_str(&line).with_context(|| format!("line {}", index + 1))?;
            let (at, event) = match line {
                Line::Start { .. } => bail!("line {}: a second start", index + 1),
                Line::Block { at, frames } => (at, Logged::Block(frames)),
                Line::Command { at, command } => (at, Logged::Command(command)),
                Line::Jog { at, deck, steps } => (at, Logged::Jog(deck, steps)),
                Line::End { at } => {
                    session.end = Some(at);
                    (at, Logged::End)
                }
                Line::Lost { count } => {
                    session.lost += count;
                    continue;
                }
                Line::Beats { path, grid } => {
                    session.grids.insert(path, grid);
                    continue;
                }
                Line::Lane { setting, points } => {
                    session.curves.insert(setting, points);
                    continue;
                }
            };
            session.events.push((at, event));
        }
        Ok(session)
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Frames from start to end, unless the recording never ended.
    pub fn frames(&self) -> Option<u64> {
        self.end.map(|end| end.saturating_sub(self.snapshot.clock))
    }

    /// Events lost while recording; the replay will not match then.
    pub fn lost_events(&self) -> u64 {
        self.lost
    }

    /// What could not be opened as it was (stems), so plays differently.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// How long it is in frames: to its end, or its last event.
    pub fn length(&self) -> u64 {
        self.frames().unwrap_or_else(|| {
            self.events.last().map_or(0, |(at, _)| at.saturating_sub(self.snapshot.clock))
        })
    }

    /// The tracks on the decks over time.
    pub fn tracks(&self) -> Vec<TrackSpan> {
        let start = self.snapshot.clock;
        let length = self.length();
        let path = |audio: &Arc<TrackAudio>| source_of(&TRACKS, audio).unwrap_or_default();
        let span = |deck, path, from| TrackSpan { deck, path, from, to: from, playing: Vec::new() };
        // Per deck: the track on it, and since when it plays.
        let mut open: Vec<Option<(TrackSpan, Option<u64>)>> = (0..crate::engine::DECKS)
            .map(|deck| {
                let (audio, playing) = self.snapshot.deck(deck);
                audio.map(|audio| (span(deck, path(audio), 0), playing.then_some(0)))
            })
            .collect();
        let mut spans = Vec::new();
        let close = |open: &mut Option<(TrackSpan, Option<u64>)>, at, spans: &mut Vec<_>| {
            if let Some((mut span, playing)) = open.take() {
                span.playing.extend(playing.map(|from| (from, at)));
                span.to = at;
                spans.push(span);
            }
        };
        for (at, event) in &self.events {
            let at = at.saturating_sub(start).min(length);
            let Logged::Command(command) = event else { continue };
            match *command {
                Command::Load { deck, ref audio } if deck < open.len() => {
                    close(&mut open[deck], at, &mut spans);
                    open[deck] = Some((span(deck, path(audio), at), None));
                }
                Command::Unload { deck } if deck < open.len() => {
                    close(&mut open[deck], at, &mut spans);
                }
                Command::SetPlaying { deck, playing } if deck < open.len() => {
                    if let Some((span, since)) = open[deck].as_mut() {
                        match (*since, playing) {
                            (None, true) => *since = Some(at),
                            (Some(from), false) => {
                                span.playing.push((from, at));
                                *since = None;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        for deck in &mut open {
            close(deck, length, &mut spans);
        }
        spans.sort_by_key(|s| (s.deck, s.from));
        spans
    }

    /// A track's beat grid.
    pub fn grid(&self, path: &Path) -> Option<&Grid> {
        self.grids.get(path)
    }

    /// Gives a track a beat grid if the session has none for it (one
    /// recorded before grids were kept).
    pub fn add_grid(&mut self, path: PathBuf, grid: Grid) {
        self.grids.entry(path).or_insert(grid);
    }

    /// Each track's file and length in seconds.
    pub fn track_files(&self) -> Vec<(PathBuf, f64)> {
        self.tracks.iter().map(|(path, audio)| (path.clone(), audio.duration_secs())).collect()
    }

    /// A channel fader at `level` as the gain the mixer gives it (its
    /// curve as recorded).
    pub fn fader_gain(&self, level: f64) -> f64 {
        let (fader, crossfader, _) = self.snapshot.curves();
        let gain =
            crate::engine::channel_gain(level as f32, CrossfaderSide::Thru, 0.0, crossfader, fader);
        f64::from(gain)
    }

    /// The master at `level` as gain.
    pub fn master_gain(&self, level: f64) -> f64 {
        f64::from(crate::engine::master_gain(level as f32))
    }

    /// The Color FX type the channel knobs had when recording started.
    pub fn color_fx(&self) -> crate::fx::ColorFxKind {
        self.snapshot.curves().2
    }

    /// The Beat FX unit over the recording: how it was at the start, then
    /// every change (frames from the start).
    pub fn beat_fx(&self) -> Vec<(u64, BeatFxParams)> {
        let start = self.snapshot.clock;
        let mut changes = vec![(0, self.snapshot.beat_fx())];
        for (at, event) in &self.events {
            if let Logged::Command(Command::SetBeatFx(params)) = event {
                changes.push((at - start, *params));
            }
        }
        changes
    }

    /// Gives each Color FX type of each channel its own lane: the knobs,
    /// the type switch and the on switch as played become every type's
    /// amount over time ([`Command::SetColorAmount`]), which sounds the
    /// same. Returns whether there was anything to change.
    pub fn split_color_fx(&mut self) -> bool {
        use crate::fx::ColorFxKind;
        let played = |event: &Logged| {
            matches!(
                event,
                Logged::Command(
                    Command::SetFilter { .. } | Command::SetColorFx(_) | Command::SetColorFxOn(_)
                )
            )
        };
        if !self.events.iter().any(|(_, event)| played(event)) {
            return false;
        }
        const DECKS: usize = crate::engine::DECKS;
        let snapshot = &self.snapshot;
        let (mut kind, mut on) = (snapshot.color_kind(), snapshot.color_on());
        let mut knobs: [f32; DECKS] = std::array::from_fn(|d| snapshot.color_knob(d));
        let mut amounts: [[f32; 6]; DECKS] = std::array::from_fn(|d| {
            std::array::from_fn(|k| snapshot.color_amount(d, ColorFxKind::ALL[k]))
        });
        let mut events = Vec::with_capacity(self.events.len());
        for (at, event) in std::mem::take(&mut self.events) {
            match event {
                Logged::Command(Command::SetFilter { deck, position }) => {
                    knobs[deck.min(DECKS - 1)] = position.clamp(-1.0, 1.0);
                }
                Logged::Command(Command::SetColorFx(k)) => kind = k,
                Logged::Command(Command::SetColorFxOn(o)) => on = o,
                other => {
                    events.push((at, other));
                    continue;
                }
            }
            for (deck, knob) in knobs.iter().enumerate() {
                for (k, unit) in ColorFxKind::ALL.into_iter().enumerate() {
                    let amount = if on && unit == kind { *knob } else { 0.0 };
                    if amounts[deck][k] != amount {
                        amounts[deck][k] = amount;
                        let command =
                            Command::SetColorAmount { deck, kind: unit, position: amount };
                        events.push((at, Logged::Command(command)));
                    }
                }
            }
        }
        self.events = events;
        self.curves.retain(|setting, _| !matches!(setting, Setting::Filter(_)));
        true
    }

    /// Replaces the Beat FX unit's changes with `changes` (as
    /// [`Session::beat_fx`] gives them), each moved to a buffer start.
    pub fn set_beat_fx(&mut self, changes: &[(u64, BeatFxParams)]) {
        let start = self.snapshot.clock;
        self.events.retain(|(_, event)| !matches!(event, Logged::Command(Command::SetBeatFx(_))));
        let end = self.end.map_or(u64::MAX, |end| end.saturating_sub(start));
        let mut last = self.snapshot.beat_fx();
        for &(at, params) in changes {
            let at = self.snap(at);
            if at < end && params != last {
                last = params;
                self.events.push((start + at, Logged::Command(Command::SetBeatFx(params))));
            }
        }
        self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        self.fit_fx_depth();
    }

    /// Moves `from`–`to` of `span` (frames from the start) by `frames`,
    /// inside the clip: that audio plays at the new place instead of what
    /// was there (laid over it), and the old place is silent. `tempo` is
    /// the deck's there (track seconds a set second). Returns the new
    /// place.
    pub fn move_part(
        &mut self,
        span: &TrackSpan,
        from: u64,
        to: u64,
        frames: i64,
        tempo: f64,
    ) -> (u64, u64) {
        let frames = frames.clamp(
            span.from as i64 - from as i64,
            (span.to as i64 - to as i64).max(span.from as i64 - from as i64),
        );
        let shifted = |at: u64| (at as i64 + frames).max(0) as u64;
        let (new_from, new_to) = (shifted(from), shifted(to));
        if frames == 0 {
            return (from, to);
        }
        let without = |pieces: &[(u64, u64)], a: u64, b: u64| {
            let mut out = Vec::new();
            for &(f, t) in pieces {
                if t <= a || f >= b {
                    out.push((f, t));
                    continue;
                }
                if f < a {
                    out.push((f, a));
                }
                if t > b {
                    out.push((b, t));
                }
            }
            out
        };
        // Heard: the rest without the old and the new place, and the part's
        // own pieces at the new place.
        let pieces = self.pieces(span);
        let moved: Vec<(u64, u64)> = pieces
            .iter()
            .filter(|&&(f, t)| f < to && t > from)
            .map(|&(f, t)| (shifted(f.max(from)), shifted(t.min(to))))
            .collect();
        let mut heard = without(&without(&pieces, from, to), new_from, new_to);
        heard.extend(moved);
        heard.sort_by_key(|p| p.0);
        self.set_pieces(span, &heard);
        // The deck plays the part's audio from the new place, then goes on
        // where it was.
        let later = frames as f64 / f64::from(self.rate) * tempo;
        let moved_by = |session: &Self, at: u64| {
            let at = session.snap(at);
            let deck = span.deck;
            session.moves().iter().find(|m| m.deck == deck && m.at == at).map_or(0.0, |m| m.later)
        };
        let (into, back) = (moved_by(self, new_from), moved_by(self, new_to));
        self.move_track(span.deck, new_from, into + later);
        self.move_track(span.deck, new_to, back - later);
        (new_from, new_to)
    }

    /// The pieces of `span` heard, between the editor's cuts (all of it
    /// when uncut); pieces that touch were split there.
    pub fn pieces(&self, span: &TrackSpan) -> Vec<(u64, u64)> {
        let start = self.snapshot.clock;
        let cuts: Vec<(u64, bool)> = self
            .events
            .iter()
            .filter_map(|(at, event)| match event {
                Logged::Command(Command::Gate { deck, open })
                    if *deck == span.deck && (span.from..span.to).contains(&(at - start)) =>
                {
                    Some((at - start, *open))
                }
                _ => None,
            })
            .collect();
        let mut pieces = Vec::new();
        let (mut from, mut open) = (span.from, true);
        for (at, next) in cuts {
            if open && at > from {
                pieces.push((from, at));
            }
            if next {
                from = at;
            }
            open = next;
        }
        if open && span.to > from {
            pieces.push((from, span.to));
        }
        pieces
    }

    /// Lets only `pieces` of `span` be heard (sorted, not overlapping;
    /// touching ones stay split there).
    pub fn set_pieces(&mut self, span: &TrackSpan, pieces: &[(u64, u64)]) {
        let start = self.snapshot.clock;
        let mut cuts = Vec::new();
        let mut end = span.from;
        for (i, &(from, to)) in pieces.iter().enumerate() {
            if from > end {
                cuts.push((end, false));
                cuts.push((from, true));
            } else if i > 0 {
                cuts.push((from, true));
            }
            end = to;
        }
        if pieces.is_empty() {
            cuts.push((span.from, false));
        } else if end < span.to {
            cuts.push((end, false));
        }
        let deck = span.deck;
        self.events.retain(|(at, event)| {
            !matches!(event, Logged::Command(Command::Gate { deck: d, .. })
                if *d == deck && (span.from..span.to).contains(&(at - start)))
        });
        for (at, open) in cuts {
            let at = self.snap(at).clamp(span.from, span.to.saturating_sub(1));
            self.events.push((start + at, Logged::Command(Command::Gate { deck, open })));
        }
        self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
    }

    /// When `deck`'s Master Tempo (key lock: the key kept when the tempo
    /// changes) was on: how it was at the start, then each change.
    pub fn key_lock(&self, deck: usize) -> Vec<(u64, bool)> {
        let start = self.snapshot.clock;
        let mut changes = vec![(0, self.snapshot.key_lock(deck))];
        for (at, event) in &self.events {
            if let Logged::Command(Command::SetKeyLock { deck: d, on }) = event
                && *d == deck
                && changes.last().is_some_and(|c| c.1 != *on)
            {
                changes.push((at - start, *on));
            }
        }
        changes
    }

    /// The tracks moved in the editor.
    pub fn moves(&self) -> Vec<TrackMove> {
        let start = self.snapshot.clock;
        self.events
            .iter()
            .filter_map(|(at, event)| match event {
                Logged::Command(Command::MoveTrack { deck, later }) => {
                    Some(TrackMove { deck: *deck, at: at - start, later: *later })
                }
                _ => None,
            })
            .collect()
    }

    /// Plays `deck`'s track `later` seconds later (earlier if negative)
    /// from `at` (frames from the start, moved to a buffer start) until the
    /// deck next jumps; replaces a move there, and 0 takes it away.
    /// Returns where it is.
    pub fn move_track(&mut self, deck: usize, at: u64, later: f64) -> u64 {
        let at = self.snap(at);
        let clock = self.snapshot.clock + at;
        self.events.retain(|(t, event)| {
            !(*t == clock
                && matches!(event, Logged::Command(Command::MoveTrack { deck: d, .. }) if *d == deck))
        });
        if later != 0.0 {
            self.events.push((clock, Logged::Command(Command::MoveTrack { deck, later })));
            self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        }
        at
    }

    /// A setting over the recording: where it stood at the start, then
    /// every change.
    pub fn lane(&self, setting: Setting) -> Vec<Point> {
        if let Some(points) = self.curves.get(&setting) {
            return points.clone();
        }
        let start = self.snapshot.clock;
        let mut points = vec![Point::step(0, self.snapshot.setting(setting))];
        for (at, event) in &self.events {
            let Logged::Command(command) = event else { continue };
            let Some((s, value)) = Setting::of(command) else { continue };
            if s != setting || points.last().is_some_and(|p| p.value == value) {
                continue;
            }
            let at = at.saturating_sub(start);
            match points.last_mut() {
                Some(last) if last.at == at => last.value = value,
                _ => points.push(Point::step(at, value)),
            }
        }
        points
    }

    /// Replaces a setting's changes with `points` (as [`Session::lane`]
    /// gives them), each moved to the nearest buffer start; a curve becomes
    /// a change at every buffer start along it.
    pub fn set_lane(&mut self, setting: Setting, points: &[Point]) {
        let start = self.snapshot.clock;
        let is_setting = |event: &Logged| matches!(event, Logged::Command(c) if Setting::of(c).is_some_and(|(s, _)| s == setting));
        self.events.retain(|(_, event)| !is_setting(event));
        let end = self.end.map_or(u64::MAX, |end| end.saturating_sub(start));
        let mut points: Vec<Point> =
            points.iter().map(|p| Point { at: self.snap(p.at), ..*p }).collect();
        points.sort_by_key(|p| p.at);
        // One point a moment: the last one put there.
        points.dedup_by(|later, earlier| {
            let same = later.at == earlier.at;
            if same {
                *earlier = *later;
            }
            same
        });
        if let Some(first) = points.first_mut() {
            first.at = 0;
        }
        let mut last = self.snapshot.setting(setting);
        let mut added = Vec::new();
        let mut set = |at: u64, value: f64| {
            if at < end && value != last {
                last = value;
                added.push((start + at, Logged::Command(setting.command(value))));
            }
        };
        for (i, point) in points.iter().enumerate() {
            set(point.at, point.value);
            let Some(next) = points.get(i + 1).filter(|_| point.curve != Curve::Step) else {
                continue;
            };
            let mut at = self.next_block(point.at);
            while at < next.at {
                set(at, lane_value(&points[i..=i + 1], at));
                at = self.next_block(at);
            }
        }
        self.events.extend(added);
        // Stable: at one moment the order holds, and the end stays last.
        self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        self.curves.insert(setting, points);
        if setting == Setting::BeatFxDepth {
            self.fit_fx_depth();
        }
    }

    /// Gives the Beat FX depth a lane of its own ([`Setting::BeatFxDepth`]):
    /// each depth the Beat FX changes set as played becomes a point of it,
    /// which sounds the same. Returns whether there was anything to do.
    pub fn split_fx_depth(&mut self) -> bool {
        if self.events.iter().any(|(_, e)| matches!(e, Logged::Command(Command::SetBeatFxDepth(_))))
        {
            return false;
        }
        let mut depth = self.snapshot.beat_fx().depth;
        let mut events = Vec::with_capacity(self.events.len());
        let mut changed = false;
        for (at, event) in std::mem::take(&mut self.events) {
            let new = match &event {
                Logged::Command(Command::SetBeatFx(params)) if params.depth != depth => {
                    Some(params.depth)
                }
                _ => None,
            };
            events.push((at, event));
            if let Some(new) = new {
                depth = new;
                changed = true;
                events.push((at, Logged::Command(Command::SetBeatFxDepth(new))));
            }
        }
        self.events = events;
        changed
    }

    /// Every Beat FX change takes the depth its lane has there, so moving,
    /// adding or restyling an effect doesn't undo the lane.
    fn fit_fx_depth(&mut self) {
        if !self
            .events
            .iter()
            .any(|(_, e)| matches!(e, Logged::Command(Command::SetBeatFxDepth(_))))
        {
            return;
        }
        let start = self.snapshot.clock;
        let lane = self.lane(Setting::BeatFxDepth);
        for (at, event) in &mut self.events {
            if let Logged::Command(Command::SetBeatFx(params)) = event {
                params.depth = lane_value(&lane, at.saturating_sub(start)) as f32;
            }
        }
    }

    /// The buffer size in force at `at` (frames from the start), and where
    /// it took effect (on the mixer's clock).
    fn block_at(&self, at: u64) -> (u64, u64) {
        let start = self.snapshot.clock;
        let mut block = (start, 0u64);
        for (t, event) in &self.events {
            if *t > start + at {
                break;
            }
            if let Logged::Block(frames) = event {
                block = (*t, u64::from(*frames));
            }
        }
        block
    }

    /// The buffer start nearest `at` (frames from the start): where a
    /// change can take effect.
    pub fn snap(&self, at: u64) -> u64 {
        let start = self.snapshot.clock;
        let (from, size) = self.block_at(at);
        if size == 0 {
            return at;
        }
        let offset = (start + at).saturating_sub(from);
        (from + (offset + size / 2) / size * size).saturating_sub(start)
    }

    /// The first buffer start after `at`.
    fn next_block(&self, at: u64) -> u64 {
        let start = self.snapshot.clock;
        let (from, size) = self.block_at(at);
        let size = size.max(1);
        let offset = (start + at).saturating_sub(from);
        (from + (offset / size + 1) * size).saturating_sub(start)
    }

    /// The events and lanes as they are, to [`Session::restore`] later.
    pub fn backup(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        serde_json::to_writer(&mut out, &self.curves.iter().collect::<Vec<_>>())?;
        out.push(b'\n');
        for (at, event) in &self.events {
            let at = *at;
            serde_json::to_writer(
                &mut out,
                &match event {
                    Logged::Block(frames) => LineRef::Block { at, frames: *frames },
                    Logged::Command(command) => LineRef::Command { at, command },
                    Logged::Jog(deck, steps) => LineRef::Jog { at, deck: *deck, steps: *steps },
                    Logged::End => LineRef::End { at },
                },
            )?;
            out.push(b'\n');
        }
        Ok(out)
    }

    /// Back to a [`Session::backup`].
    pub fn restore(&mut self, backup: &[u8]) -> Result<()> {
        let opened = Opened {
            sources: Box::new(Files),
            tracks: self.tracks.clone(),
            stems: self.stems.clone(),
            warnings: Vec::new(),
        };
        let previous = OPENED.replace(Some(opened));
        let read = (|| -> Result<_> {
            let mut lines = backup.split(|&b| b == b'\n').filter(|l| !l.is_empty());
            let curves: Vec<(Setting, Vec<Point>)> =
                serde_json::from_slice(lines.next().unwrap_or(b"[]"))?;
            let mut events = Vec::new();
            for line in lines {
                events.push(match serde_json::from_slice(line)? {
                    Line::Block { at, frames } => (at, Logged::Block(frames)),
                    Line::Command { at, command } => (at, Logged::Command(command)),
                    Line::Jog { at, deck, steps } => (at, Logged::Jog(deck, steps)),
                    Line::End { at } => (at, Logged::End),
                    _ => continue,
                });
            }
            Ok((curves, events))
        })();
        OPENED.set(previous);
        let (curves, events) = read?;
        (self.curves, self.events) = (curves.into_iter().collect(), events);
        Ok(())
    }

    /// Moves the clip of `deck`'s track loaded at `from` (frames from the
    /// start) by `by` frames: the track and everything done on that deck
    /// while it was loaded. Where it lands, the clip before it ends sooner,
    /// and what would run into the next clip is cut. Returns where it
    /// starts now.
    pub fn move_clip(&mut self, deck: usize, from: u64, by: i64) -> Result<u64> {
        let start = self.snapshot.clock;
        let spans: Vec<TrackSpan> = self.tracks().into_iter().filter(|s| s.deck == deck).collect();
        let index = spans
            .iter()
            .position(|s| s.from == from)
            .ok_or_else(|| anyhow!("no track on deck {} at that moment", deck + 1))?;
        let loaded_here = self.events.iter().any(|(at, event)| {
            *at == start + from
                && matches!(event, Logged::Command(Command::Load { deck: d, .. }) if *d == deck)
        });
        if !loaded_here {
            bail!("the track was on the deck before recording started, so it cannot move");
        }
        let end = spans[index].to;
        // Not onto the clip before's own start, nor past the session.
        let low = index.checked_sub(1).map_or(0, |i| spans[i].from + 1);
        let landing = (from as i64 + by).clamp(low as i64, self.length() as i64 - 1) as u64;
        let landing = self.snap(landing).max(low);
        let by = landing as i64 - from as i64;
        if by == 0 {
            return Ok(from);
        }
        let next = spans.get(index + 1).map(|s| s.from);
        let (moved, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.events).into_iter().partition(|(at, event)| {
                deck_of(event) == Some(deck) && (start + from..start + end).contains(at)
            });
        let landed = landing..(end as i64 + by).max(0) as u64;
        self.events = rest
            .into_iter()
            // The deck's other clips give way where this one lands.
            .filter(|(at, event)| {
                deck_of(event) != Some(deck)
                    || !landed.contains(&(at - start))
                    || next.is_some_and(|n| at - start >= n)
            })
            .collect();
        self.events.extend(moved.into_iter().filter_map(|(at, event)| {
            let at = (at as i64 + by) as u64;
            // Not into the next clip.
            let cut = next.is_some_and(|n| at - start >= n) && !matches!(event, Logged::End);
            (!cut).then_some((at, event))
        }));
        self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        // The deck's lanes as edited now follow the events.
        self.curves.retain(|setting, _| setting.deck() != Some(deck));
        Ok(landing)
    }

    /// Writes the session, as edited, to `path`.
    pub fn write(&self, path: &Path) -> Result<()> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut out = BufWriter::with_capacity(1 << 16, file);
        let line = |out: &mut BufWriter<File>, line: Line| -> Result<()> {
            serde_json::to_writer(&mut *out, &line)?;
            out.write_all(b"\n")?;
            Ok(())
        };
        let snapshot = Box::new(self.copy(&*self.snapshot)?);
        line(&mut out, Line::Start { version: VERSION, rate: self.rate, snapshot })?;
        for (path, grid) in &self.grids {
            line(&mut out, Line::Beats { path: path.clone(), grid: grid.clone() })?;
        }
        for (setting, points) in &self.curves {
            line(&mut out, Line::Lane { setting: *setting, points: points.clone() })?;
        }
        for (at, event) in &self.events {
            let at = *at;
            line(
                &mut out,
                match event {
                    Logged::Block(frames) => Line::Block { at, frames: *frames },
                    Logged::Command(command) => Line::Command { at, command: self.copy(command)? },
                    Logged::Jog(deck, steps) => Line::Jog { at, deck: *deck, steps: *steps },
                    Logged::End => Line::End { at },
                },
            )?;
        }
        if self.lost > 0 {
            line(&mut out, Line::Lost { count: self.lost })?;
        }
        out.flush()?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        Ok(())
    }

    /// A copy of something holding the session's tracks, through serde
    /// (commands are not `Clone`: one of them holds the recorder).
    fn copy<T: Serialize + serde::de::DeserializeOwned>(&self, item: &T) -> Result<T> {
        let value = serde_json::to_value(item)?;
        let opened = Opened {
            sources: Box::new(Files),
            tracks: self.tracks.clone(),
            stems: self.stems.clone(),
            warnings: Vec::new(),
        };
        let previous = OPENED.replace(Some(opened));
        let copy = serde_json::from_value(value);
        OPENED.set(previous);
        Ok(copy?)
    }

    /// Plays the session through the mixer, handing `out` each buffer of
    /// interleaved stereo; returns the frames played.
    pub fn render(&self, out: impl FnMut(&[f32]) -> Result<()>) -> Result<u64> {
        struct Out<F>(F);
        impl<F: FnMut(&[f32]) -> Result<()>> Run for Out<F> {
            fn after(&mut self, _: u64, buffer: &[f32], _: &mut Offline) -> Result<()> {
                (self.0)(buffer)
            }
        }
        self.run(self.events.iter(), &mut Out(out))
    }

    /// Plays `events` through the mixer from the snapshot, with `run`
    /// along; returns the frames played.
    fn run<'a>(
        &self,
        events: impl Iterator<Item = &'a (u64, Logged)>,
        run: &mut impl Run,
    ) -> Result<u64> {
        let start = self.snapshot.clock;
        let mut mixer = Offline::new(self.rate, &self.snapshot);
        let mut clock = start;
        let mut frames = 0usize;
        let mut buffer = Vec::new();
        let mut events = events.peekable();
        loop {
            while let Some((at, _)) = events.peek()
                && *at <= clock
            {
                let Some((at, event)) = events.next() else { break };
                if *at < clock {
                    bail!("the session is out of step at frame {at}");
                }
                run.event(event);
                match event {
                    Logged::Block(n) => frames = *n as usize,
                    Logged::Command(command) => mixer.command(self.copy(command)?)?,
                    Logged::Jog(deck, steps) => mixer.jog(*deck, *steps)?,
                    Logged::End => return Ok(clock - start),
                }
            }
            if events.peek().is_none() {
                return Ok(clock - start);
            }
            if frames == 0 {
                bail!("the session has no buffer size");
            }
            run.before(clock, &mut mixer)?;
            buffer.resize(frames * 2, 0.0);
            mixer.render(&mut buffer);
            clock += frames as u64;
            run.after(clock, &buffer, &mut mixer)?;
        }
    }

    /// Whether sync was on during the recording (sessions from before it
    /// was recorded have no sync to redo).
    pub fn has_sync(&self) -> bool {
        self.events.iter().any(|(_, event)| {
            matches!(event, Logged::Command(Command::SyncState { synced, .. }) if synced.contains(&true))
        })
    }

    /// When each deck was synced, and to which master.
    pub fn sync_spans(&self) -> Vec<SyncSpan> {
        let start = self.snapshot.clock;
        let length = self.length();
        let mut spans = Vec::new();
        let mut open: [Option<SyncSpan>; crate::engine::DECKS] = Default::default();
        for (at, event) in &self.events {
            let Logged::Command(Command::SyncState { master, synced }) = event else { continue };
            let at = at.saturating_sub(start).min(length);
            for (deck, span) in open.iter_mut().enumerate() {
                let master = master.filter(|&m| m != deck && synced[deck]);
                if span.as_ref().map(|s| Some(s.master)) != Some(master) {
                    spans.extend(span.take().map(|s| SyncSpan { to: at, ..s }));
                    *span = master.map(|master| SyncSpan { deck, master, from: at, to: at });
                }
            }
        }
        spans.extend(open.into_iter().flatten().map(|s| SyncSpan { to: length, ..s }));
        spans.sort_by_key(|s| (s.deck, s.from));
        spans
    }

    /// What each deck played over the set, for drawing: its levels after
    /// every buffer and where its beats fell.
    pub fn played(&self) -> Result<Vec<Played>> {
        let mut playing = Playing {
            grids: &self.grids,
            tracks: std::array::from_fn(|d| {
                self.snapshot.deck(d).0.and_then(|audio| source_of(&TRACKS, audio))
            }),
            start: self.snapshot.clock,
            rate: f64::from(self.rate),
            last: [None; crate::engine::DECKS],
            decks: vec![Played::default(); crate::engine::DECKS],
        };
        self.run(self.events.iter(), &mut playing)?;
        Ok(playing.decks)
    }

    /// A copy to work on while this one is edited.
    pub fn duplicate(&self) -> Result<Self> {
        let events = self
            .events
            .iter()
            .map(|(at, event)| {
                Ok((
                    *at,
                    match event {
                        Logged::Block(frames) => Logged::Block(*frames),
                        Logged::Command(command) => Logged::Command(self.copy(command)?),
                        Logged::Jog(deck, steps) => Logged::Jog(*deck, *steps),
                        Logged::End => Logged::End,
                    },
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            rate: self.rate,
            snapshot: Box::new(self.copy(&*self.snapshot)?),
            events,
            end: self.end,
            lost: self.lost,
            warnings: self.warnings.clone(),
            tracks: self.tracks.clone(),
            stems: self.stems.clone(),
            grids: self.grids.clone(),
            curves: self.curves.clone(),
        })
    }

    /// Does again what sync did live, for the session as edited: synced
    /// decks take the master's tempo, and every [`SYNC_CHECK_SECS`] those
    /// drifting off its beat are put back, keeping the phase the DJ chose.
    /// What sync did while recording is replaced; returns the commands sync
    /// makes now.
    pub fn resync(&mut self) -> Result<usize> {
        let made_by_sync = |event: &Logged| {
            matches!(event, Logged::Command(Command::SyncTempo { .. } | Command::SyncShift { .. }))
        };
        let mut sync = Resync::new(self);
        self.run(self.events.iter().filter(|(_, e)| !made_by_sync(e)), &mut sync)?;
        let added = sync.added;
        let count = added.len();
        self.events.retain(|(_, e)| !made_by_sync(e));
        self.events.extend(added);
        self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        // Tempo lanes as edited keep their points; the rest show sync's.
        Ok(count)
    }

    /// Plays `from`–`to` of the session (frames from the start) into a
    /// 24-bit WAV at `path`. Everything before it is played too, unheard,
    /// so the part sounds as it does in the whole set. `progress` gets the
    /// frames done (of the whole way to `to`). Returns the frames written.
    pub fn render_part_to_wav(
        &self,
        path: &Path,
        from: u64,
        to: u64,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        let rate = self.rate;
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut wav = BufWriter::with_capacity(1 << 16, file);
        crate::record::write_header(&mut wav, rate, 0)?;
        let (mut clock, mut written) = (0u64, 0u64);
        // Played past `to`: stops the mixer there (not a failure).
        const DONE: &str = "the part is played";
        let played = self.render(|buffer| {
            let frames = buffer.len() as u64 / 2;
            let (start, end) = (clock, clock + frames);
            clock = end;
            let (a, b) = (from.clamp(start, end), to.clamp(start, end));
            for &sample in &buffer[((a - start) * 2) as usize..((b - start) * 2) as usize] {
                wav.write_all(&crate::record::to_24_bit(sample).to_le_bytes()[..3])?;
            }
            written += b - a;
            progress(end.min(to));
            if end >= to { Err(anyhow!(DONE)) } else { Ok(()) }
        });
        if let Err(err) = played
            && err.to_string() != DONE
        {
            return Err(err);
        }
        wav.flush()?;
        let mut file = wav.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(0))?;
        crate::record::write_header(&mut file, rate, written)?;
        Ok(written)
    }

    /// Plays the session into a 24-bit WAV at `path` and, if given,
    /// compares it with the `recorded` one. `progress` gets the frames done.
    pub fn render_to_wav(
        &self,
        path: &Path,
        recorded: Option<&Path>,
        mut progress: impl FnMut(u64),
    ) -> Result<Rendered> {
        let rate = self.rate;
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut wav = BufWriter::with_capacity(1 << 16, file);
        crate::record::write_header(&mut wav, rate, 0)?;
        let mut compare = recorded.map(Compare::open).transpose()?;
        let mut done = 0u64;
        let frames = self.render(|buffer| {
            for &s in buffer {
                let v = crate::record::to_24_bit(s);
                wav.write_all(&v.to_le_bytes()[..3])?;
                if let Some(compare) = compare.as_mut() {
                    compare.next(v)?;
                }
            }
            done += buffer.len() as u64 / 2;
            progress(done);
            Ok(())
        })?;
        wav.flush()?;
        let mut file = wav.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(0))?;
        crate::record::write_header(&mut file, rate, frames)?;
        let comparison = compare.map(Compare::finish).transpose()?;
        Ok(Rendered { frames, comparison })
    }
}

/// A deck synced to a master, in frames from the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncSpan {
    pub deck: usize,
    pub master: usize,
    pub from: u64,
    pub to: u64,
}

/// What a deck played over a set (see [`Session::played`]); frames from
/// the start.
#[derive(Debug, Clone, Default)]
pub struct Played {
    /// For each buffer: where it starts, the loudest the track played and
    /// the loudest that reached the mix.
    pub levels: Vec<(u64, f32, f32)>,
    /// For each buffer, the loudest of its low end (below 150 Hz) in the mix.
    pub lows: Vec<f32>,
    /// Where the track's beats fell, and whether each starts a bar.
    pub beats: Vec<(u64, bool)>,
}

/// Where two decks' low end (bass, kick) went into the mix together, in
/// frames from the start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clash {
    pub from: u64,
    pub to: u64,
    pub decks: (usize, usize),
    /// How loud each one's low end was there at most, 1.0 = full scale.
    pub levels: (f32, f32),
}

/// Low end this loud (−18 dB) counts as playing.
const LOW_LEVEL: f32 = 0.125;

/// Where the decks' low end clashed (see [`Played::lows`]): both playing
/// for at least `least` frames, gaps under a fifth of that bridged.
pub fn clashes(played: &[Played], least: u64) -> Vec<Clash> {
    // Each deck's buffers by when they start.
    let lows: Vec<HashMap<u64, f32>> = played
        .iter()
        .map(|p| p.levels.iter().zip(&p.lows).map(|(l, low)| (l.0, *low)).collect())
        .collect();
    let mut found = Vec::new();
    for (a, deck) in played.iter().enumerate() {
        for (b, other) in lows.iter().enumerate().skip(a + 1) {
            let mut open: Option<Clash> = None;
            for (i, &(from, _, _)) in deck.levels.iter().enumerate() {
                let to = deck.levels.get(i + 1).map_or(from, |l| l.0);
                let (la, lb) = (deck.lows[i], other.get(&from).copied().unwrap_or(0.0));
                if la >= LOW_LEVEL && lb >= LOW_LEVEL {
                    match open.as_mut() {
                        Some(c) if from.saturating_sub(c.to) <= least / 5 => {
                            c.to = to;
                            c.levels = (c.levels.0.max(la), c.levels.1.max(lb));
                        }
                        _ => {
                            found.extend(open.take().filter(|c| c.to - c.from >= least));
                            open = Some(Clash { from, to, decks: (a, b), levels: (la, lb) });
                        }
                    }
                }
            }
            found.extend(open.filter(|c| c.to - c.from >= least));
        }
    }
    found.sort_by_key(|c| c.from);
    found
}

/// Follows the decks through a session for [`Session::played`].
struct Playing<'a> {
    grids: &'a HashMap<PathBuf, Grid>,
    tracks: [Option<PathBuf>; crate::engine::DECKS],
    start: u64,
    rate: f64,
    /// Each deck after the last buffer: when (frames from the start), and
    /// where in its track (seconds).
    last: [Option<(u64, f64)>; crate::engine::DECKS],
    decks: Vec<Played>,
}

impl Run for Playing<'_> {
    fn event(&mut self, event: &Logged) {
        if let Logged::Command(Command::Load { deck, audio }) = event
            && *deck < self.tracks.len()
        {
            self.tracks[*deck] = source_of(&TRACKS, audio);
            self.last[*deck] = None;
        }
    }

    fn after(&mut self, clock: u64, buffer: &[f32], mixer: &mut Offline) -> Result<()> {
        let at = clock - self.start;
        let from = at - (buffer.len() / 2) as u64;
        for deck in 0..self.decks.len() {
            let (played, heard, low) = mixer.take_peaks(deck);
            let view = mixer.deck(deck);
            if !view.loaded {
                self.last[deck] = None;
                continue;
            }
            self.decks[deck].levels.push((from, played, heard));
            self.decks[deck].lows.push(low);
            let grid = self.tracks[deck].as_ref().and_then(|path| self.grids.get(path));
            // The beats passed, moving forward and not jumping (a cue, a
            // loop's end): placed between the buffers' ends.
            if let (Some(grid), Some((then, was))) = (grid, self.last[deck]) {
                let now = view.seconds;
                let span = (at - then) as f64 / self.rate;
                if now > was && now - was <= span * 8.0 {
                    let first = grid.beats.partition_point(|&b| b <= was);
                    for (i, &beat) in grid.beats.iter().enumerate().skip(first) {
                        if beat > now {
                            break;
                        }
                        let share = (beat - was) / (now - was);
                        let when = then + (share * (at - then) as f64) as u64;
                        self.decks[deck].beats.push((when, grid.bars.get(i) == Some(&1)));
                    }
                }
            }
            self.last[deck] = Some((at, view.seconds));
        }
        Ok(())
    }
}

/// What runs along with a session through the mixer.
trait Run {
    /// Each event, as the mixer takes it.
    fn event(&mut self, _event: &Logged) {}

    /// Before each buffer, which starts at `clock`: may tell the mixer more.
    fn before(&mut self, _clock: u64, _mixer: &mut Offline) -> Result<()> {
        Ok(())
    }

    /// Each buffer played, which ended at `clock`.
    fn after(&mut self, _clock: u64, _buffer: &[f32], _mixer: &mut Offline) -> Result<()> {
        Ok(())
    }
}

/// Sync as the app does it live, for [`Session::resync`].
struct Resync<'a> {
    grids: &'a HashMap<PathBuf, Grid>,
    /// Each deck's track.
    tracks: [Option<PathBuf>; crate::engine::DECKS],
    master: Option<usize>,
    synced: [bool; crate::engine::DECKS],
    offsets: [f64; crate::engine::DECKS],
    /// Frames between drift checks, and the next one.
    check: u64,
    next_check: u64,
    added: Vec<(u64, Logged)>,
}

impl<'a> Resync<'a> {
    fn new(session: &'a Session) -> Self {
        let tracks = std::array::from_fn(|d| {
            session.snapshot.deck(d).0.and_then(|audio| source_of(&TRACKS, audio))
        });
        Self {
            grids: &session.grids,
            tracks,
            master: None,
            synced: [false; crate::engine::DECKS],
            offsets: [0.0; crate::engine::DECKS],
            check: (SYNC_CHECK_SECS * f64::from(session.rate)) as u64,
            next_check: 0,
            added: Vec::new(),
        }
    }

    fn grid(&self, deck: usize) -> Option<&'a Grid> {
        self.tracks[deck].as_ref().and_then(|path| self.grids.get(path))
    }

    /// Tells the mixer, and keeps it for the session.
    fn send(&mut self, at: u64, mixer: &mut Offline, command: impl Fn() -> Command) -> Result<()> {
        mixer.command(command())?;
        self.added.push((at, Logged::Command(command())));
        Ok(())
    }
}

impl Run for Resync<'_> {
    fn event(&mut self, event: &Logged) {
        match event {
            Logged::Command(Command::SyncState { master, synced }) => {
                (self.master, self.synced) = (*master, *synced);
            }
            Logged::Command(Command::SyncOffset { deck, beats }) => {
                if let Some(offset) = self.offsets.get_mut(*deck) {
                    *offset = *beats;
                }
            }
            Logged::Command(Command::Load { deck, audio }) => {
                if let Some(track) = self.tracks.get_mut(*deck) {
                    *track = source_of(&TRACKS, audio);
                }
            }
            _ => {}
        }
    }

    fn before(&mut self, clock: u64, mixer: &mut Offline) -> Result<()> {
        let align = clock >= self.next_check;
        if align {
            self.next_check = clock + self.check;
        }
        for deck in 0..crate::engine::DECKS {
            let Some(master) = self.master.filter(|&m| m != deck && self.synced[deck]) else {
                continue;
            };
            let (Some(lead), Some(grid)) = (self.grid(master), self.grid(deck)) else { continue };
            let (m, d) = (mixer.deck(master), mixer.deck(deck));
            if !m.loaded || !d.loaded || grid.bpm <= 0.0 {
                continue;
            }
            let ratio = lead.bpm * m.tempo / grid.bpm;
            if (d.tempo - ratio).abs() > 1e-6 {
                self.send(clock, mixer, || Command::SyncTempo { deck, ratio })?;
            }
            // While the DJ nudges, the phase is left alone.
            if align && m.playing && d.playing && !d.handled {
                let beat = grid.beat_at(d.seconds);
                let mut phase = beat.fract() - lead.beat_at(m.seconds).fract();
                phase -= phase.round();
                let mut error = phase - self.offsets[deck];
                error -= error.round();
                if error.abs() > SYNC_DRIFT_BEATS {
                    let seconds = grid.time_of_beat(beat - error) - grid.time_of_beat(beat);
                    self.send(clock, mixer, || Command::SyncShift { deck, seconds })?;
                }
            }
        }
        Ok(())
    }
}

/// A finished replay.
#[derive(Debug, Clone, Copy)]
pub struct Rendered {
    pub frames: u64,
    pub comparison: Option<Comparison>,
}

/// How a replay compares with the recording, sample by sample (24-bit).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Comparison {
    pub recorded_frames: u64,
    /// Samples (left and right counted apart) that differ.
    pub differing: u64,
    /// The largest difference, 1.0 = full scale.
    pub largest: f32,
    /// The first frame that differs.
    pub first: Option<u64>,
}

impl Comparison {
    pub fn identical(&self, frames: u64) -> bool {
        self.differing == 0 && self.recorded_frames == frames
    }
}

/// Reads a recording's samples along with the replay.
struct Compare {
    file: BufReader<File>,
    remaining: u64,
    recorded_frames: u64,
    differing: u64,
    largest: i32,
    /// Samples compared, and the frame of the first that differed.
    compared: u64,
    first: Option<u64>,
}

impl Compare {
    /// Opens a 24-bit stereo WAV as the recorder writes them.
    fn open(path: &Path) -> Result<Self> {
        let mut file = BufReader::with_capacity(
            1 << 16,
            File::open(path).with_context(|| format!("opening {}", path.display()))?,
        );
        let mut header = [0u8; 44];
        file.read_exact(&mut header).with_context(|| format!("reading {}", path.display()))?;
        let u16_at = |i: usize| u16::from_le_bytes([header[i], header[i + 1]]);
        if &header[..4] != b"RIFF"
            || &header[36..40] != b"data"
            || u16_at(20) != 1
            || u16_at(22) != 2
            || u16_at(34) != 24
        {
            bail!("{} is not a recording of this app", path.display());
        }
        let bytes = u32::from_le_bytes([header[40], header[41], header[42], header[43]]);
        let samples = u64::from(bytes) / 3;
        Ok(Self {
            file,
            remaining: samples,
            recorded_frames: samples / 2,
            differing: 0,
            largest: 0,
            compared: 0,
            first: None,
        })
    }

    fn next(&mut self, sample: i32) -> Result<()> {
        self.compared += 1;
        if self.remaining == 0 {
            self.differing += 1;
            self.first.get_or_insert((self.compared - 1) / 2);
            return Ok(());
        }
        self.remaining -= 1;
        let mut bytes = [0u8; 4];
        self.file.read_exact(&mut bytes[1..])?;
        // Sign-extended from 24 bits.
        let recorded = i32::from_le_bytes(bytes) >> 8;
        if recorded != sample {
            self.differing += 1;
            self.first.get_or_insert((self.compared - 1) / 2);
            self.largest = self.largest.max((recorded - sample).abs());
        }
        Ok(())
    }

    fn finish(self) -> Result<Comparison> {
        Ok(Comparison {
            recorded_frames: self.recorded_frames,
            differing: self.differing + self.remaining,
            largest: self.largest as f32 / 8_388_607.0,
            first: self.first.or((self.remaining > 0).then_some(self.compared / 2)),
        })
    }
}

/// The session kept next to a recording.
pub fn session_path(recording: &Path) -> PathBuf {
    recording.with_extension("session")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::blank_snapshot;
    use crate::fx::{BeatFxKind, BeatFxParams, FxTarget};

    /// The tests' tracks, by name.
    struct Tracks(HashMap<PathBuf, Arc<TrackAudio>>);

    impl Sources for Tracks {
        fn track(&mut self, path: &Path) -> Result<Arc<TrackAudio>> {
            self.0.get(path).cloned().ok_or_else(|| anyhow!("no track {}", path.display()))
        }

        fn stems(&mut self, source: &StemsSource, track: &TrackAudio) -> Result<Arc<StemLayers>> {
            Files.stems(source, track)
        }
    }

    /// Two tones and a little noise, different for each `seed`.
    fn track(rate: u32, secs: f64, seed: u32) -> Arc<TrackAudio> {
        let mut noise = seed.wrapping_mul(2_654_435_761) | 1;
        let frames = (secs * f64::from(rate)) as usize;
        let samples = (0..frames)
            .flat_map(|i| {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                let t = i as f32 / rate as f32;
                let hz = 110.0 * (1.0 + seed as f32);
                let tone = (t * hz * std::f32::consts::TAU).sin() * 0.4
                    + (t * hz * 3.01 * std::f32::consts::TAU).sin() * 0.2;
                let n = (noise as f32 / u32::MAX as f32 - 0.5) * 0.1;
                [tone + n, tone - n]
            })
            .collect();
        Arc::new(TrackAudio { sample_rate: rate, samples })
    }

    #[test]
    fn a_replay_plays_the_recording_again() {
        let dir = std::env::temp_dir().join(format!("deckhand-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rate = 48_000;
        let tracks: Vec<(PathBuf, Arc<TrackAudio>)> = [(44_100, 0), (48_000, 1), (44_100, 2)]
            .into_iter()
            .map(|(r, seed)| (PathBuf::from(format!("/music/{seed}.wav")), track(r, 6.0, seed)))
            .collect();
        for (path, audio) in &tracks {
            remember_track(audio, path);
        }
        let audio = |i: usize| tracks[i].1.clone();

        let mut live = Offline::new(rate, &blank_snapshot(rate));
        let mut buffer = Vec::new();
        let mut play = |live: &mut Offline, frames: usize, blocks: usize| {
            for _ in 0..blocks {
                buffer.resize(frames * 2, 0.0);
                live.render(&mut buffer);
            }
        };
        let send = |live: &mut Offline, commands: Vec<Command>| {
            for command in commands {
                live.command(command).unwrap();
            }
        };
        // Before recording: a deck playing with Master Tempo, EQ and filter.
        send(
            &mut live,
            vec![
                Command::Load { deck: 0, audio: audio(0) },
                Command::SetPlaying { deck: 0, playing: true },
                Command::SetKeyLock { deck: 0, on: true },
                Command::SetTempo { deck: 0, ratio: 1.04 },
                Command::SetEq { deck: 0, band: 0, db: -10.0 },
                Command::SetFilter { deck: 0, position: -0.4 },
                Command::SetCrossfader(-0.3),
            ],
        );
        play(&mut live, 512, 20);

        let wav = dir.join("set.wav");
        let (recording, tap) = crate::record::Recording::start(&wav, rate).unwrap();
        send(&mut live, vec![Command::Record(Some(tap))]);
        play(&mut live, 512, 10);
        // A second deck comes in scratched, then loops, with a sampler hit
        // and an echo; the buffer size changes on the way.
        send(
            &mut live,
            vec![
                Command::Load { deck: 1, audio: audio(1) },
                Command::Seek { deck: 1, seconds: 1.0 },
                Command::SetPlaying { deck: 1, playing: true },
                Command::SetVolume { deck: 1, level: 0.8 },
                Command::SetSlip { deck: 1, on: true },
                Command::Hold { deck: 1, held: true },
            ],
        );
        for steps in [40, -25, 60, -80, 30] {
            live.jog(1, steps).unwrap();
            play(&mut live, 256, 3);
        }
        send(&mut live, vec![Command::Hold { deck: 1, held: false }]);
        live.jog(1, 12).unwrap();
        play(&mut live, 256, 8);
        live.jog(0, 9).unwrap();
        send(
            &mut live,
            vec![
                Command::SetLoop { deck: 1, start: 2.0, end: 2.25 },
                Command::SamplerLoad { slot: 3, audio: audio(2) },
                Command::SamplerPlay { slot: 3 },
                Command::SetNudge { deck: 0, amount: 0.02 },
                Command::SetBeatFx(BeatFxParams {
                    kind: BeatFxKind::Echo,
                    on: true,
                    depth: 0.7,
                    beat_secs: 0.25,
                    target: FxTarget::Master,
                    release: false,
                }),
            ],
        );
        play(&mut live, 1000, 30);
        send(
            &mut live,
            vec![
                Command::ClearLoop { deck: 1 },
                Command::SetNudge { deck: 0, amount: 0.0 },
                Command::SetCrossfader(0.4),
                Command::SetKeyShift { deck: 1, semitones: 2.0 },
            ],
        );
        play(&mut live, 512, 20);
        // Sent again unchanged (left out of the file), and a nudge that a
        // new track resets, so sending it again matters.
        send(
            &mut live,
            vec![
                Command::SetVolume { deck: 1, level: 0.8 },
                Command::SetCrossfader(0.4),
                Command::SetNudge { deck: 1, amount: 0.01 },
            ],
        );
        play(&mut live, 512, 5);
        send(
            &mut live,
            vec![
                Command::Load { deck: 1, audio: audio(2) },
                Command::SetPlaying { deck: 1, playing: true },
                Command::SetNudge { deck: 1, amount: 0.01 },
                Command::SetVolume { deck: 1, level: 0.8 },
            ],
        );
        play(&mut live, 512, 20);
        send(&mut live, vec![Command::Record(None)]);
        play(&mut live, 512, 1);
        recording.finish().unwrap();

        let map: HashMap<_, _> = tracks.iter().cloned().collect();
        let session = Session::read(&session_path(&wav), Tracks(map.clone())).unwrap();
        assert!(session.warnings().is_empty());
        assert_eq!(session.lost_events(), 0);
        let frames = session.frames().unwrap();
        assert!(frames > 60_000, "{frames}");
        let rendered = session.render_to_wav(&dir.join("replay.wav"), Some(&wav), |_| {}).unwrap();
        assert_eq!(rendered.frames, frames);
        let comparison = rendered.comparison.unwrap();
        assert!(comparison.identical(frames), "{comparison:?}");
        let text = std::fs::read_to_string(session_path(&wav)).unwrap();
        let count = |pattern: &str| text.lines().filter(|l| l.contains(pattern)).count();
        // Sent three times; again after a new track, to be safe.
        assert_eq!(count(r#""SetVolume":{"deck":1"#), 2);
        assert_eq!(count(r#""SetCrossfader""#), 1);
        assert_eq!(count(r#""SetNudge":{"deck":1"#), 2);

        // Without one jog movement it is another performance.
        let text = std::fs::read_to_string(session_path(&wav)).unwrap();
        let jog = text.lines().position(|l| l.contains(r#""event":"jog""#)).unwrap();
        let other: Vec<&str> =
            text.lines().enumerate().filter(|(i, _)| *i != jog).map(|(_, l)| l).collect();
        let changed = dir.join("changed.session");
        std::fs::write(&changed, other.join("\n")).unwrap();
        let session = Session::read(&changed, Tracks(map.clone())).unwrap();
        let rendered = session.render_to_wav(&dir.join("changed.wav"), Some(&wav), |_| {}).unwrap();
        assert!(rendered.comparison.unwrap().differing > 1000);

        // Editing: a lane given back as it is changes nothing, and neither
        // does saving; a flattened crossfader does.
        let mut session = Session::read(&session_path(&wav), Tracks(map.clone())).unwrap();
        let fader = Setting::Crossfader;
        let lane = session.lane(fader);
        assert_eq!(lane.iter().map(|p| p.value as f32).collect::<Vec<_>>(), [-0.3, 0.4]);
        assert_eq!(session.snap(lane[1].at + 100), lane[1].at);
        session.set_lane(fader, &lane);
        let edited = dir.join("edited.session");
        session.write(&edited).unwrap();
        let session = Session::read(&edited, Tracks(map.clone())).unwrap();
        let out = dir.join("edited.wav");
        let rendered = session.render_to_wav(&out, Some(&wav), |_| {}).unwrap();
        assert!(rendered.comparison.unwrap().identical(frames));
        let tracks = session.tracks();
        let names: Vec<_> = tracks.iter().map(|t| (t.deck, t.path.clone())).collect();
        assert_eq!(
            names,
            [
                (0, tracks[0].path.clone()),
                (1, PathBuf::from("/music/1.wav")),
                (1, PathBuf::from("/music/2.wav"))
            ]
        );
        assert_eq!(tracks[0].playing, [(0, frames)]);
        let mut session = session;
        // Deck 1's track was loaded before recording: it stays put.
        assert!(session.move_clip(0, 0, 4096).is_err());
        session.set_lane(fader, &lane[..1]);
        assert_eq!(session.lane(fader).len(), 1);
        let rendered = session.render_to_wav(&out, Some(&wav), |_| {}).unwrap();
        assert!(rendered.comparison.unwrap().differing > 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Records a few seconds from the audio device (nearly silent) and
    /// replays them: `cargo test -p deckhand-audio device -- --ignored`
    #[test]
    #[ignore]
    fn replays_a_recording_from_the_audio_device() {
        let dir = std::env::temp_dir().join(format!("deckhand-device-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (track(44_100, 8.0, 3), track(48_000, 8.0, 4));
        let (pa, pb) = (PathBuf::from("/music/a.wav"), PathBuf::from("/music/b.wav"));
        remember_track(&a, &pa);
        remember_track(&b, &pb);
        let mut engine = crate::Engine::start(&crate::AudioOptions::default()).unwrap();
        let mut jog = engine.take_jog_sender().unwrap();
        let pause = |ms| std::thread::sleep(Duration::from_millis(ms));
        engine.send(Command::SetMasterVolume(0.02));
        engine.send(Command::Load { deck: 0, audio: a.clone() });
        engine.send(Command::SetKeyLock { deck: 0, on: true });
        engine.send(Command::SetPlaying { deck: 0, playing: true });
        pause(300);
        let wav = dir.join("set.wav");
        let recording = engine.start_recording(&wav).unwrap();
        pause(300);
        engine.send(Command::Load { deck: 1, audio: b.clone() });
        engine.send(Command::SetPlaying { deck: 1, playing: true });
        for i in 0..40 {
            jog.send(i % 2, if i % 3 == 0 { -7 } else { 11 });
            engine.send(Command::SetFilter { deck: 0, position: i as f32 / 50.0 - 0.4 });
            engine.send(Command::SetCrossfader(i as f32 / 40.0 - 0.5));
            pause(25);
        }
        engine.send(Command::SetTempo { deck: 1, ratio: 0.97 });
        pause(500);
        engine.stop_recording(recording).unwrap();

        let sources = Tracks([(pa, a), (pb, b)].into_iter().collect());
        let session = Session::read(&session_path(&wav), sources).unwrap();
        let frames = session.frames().unwrap();
        let rendered = session.render_to_wav(&dir.join("replay.wav"), Some(&wav), |_| {}).unwrap();
        let comparison = rendered.comparison.unwrap();
        eprintln!("{frames} frames, {comparison:?}");
        assert!(comparison.identical(frames), "{comparison:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Replays the session `DECKHAND_SESSION` into `DECKHAND_REPLAY` and
    /// compares it with its recording (read only):
    /// `cargo test -p deckhand-audio --release real_session -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_session() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let out = PathBuf::from(std::env::var("DECKHAND_REPLAY").unwrap());
        let started = std::time::Instant::now();
        let session = Session::read(&path, Files).unwrap();
        eprintln!("read in {:?}; warnings {:?}", started.elapsed(), session.warnings());
        let started = std::time::Instant::now();
        let recorded = path.with_extension("wav");
        let rendered = session.render_to_wav(&out, Some(&recorded), |_| {}).unwrap();
        let secs = rendered.frames as f64 / f64::from(session.sample_rate());
        eprintln!("{secs:.0} s rendered in {:?}: {:?}", started.elapsed(), rendered.comparison);
    }

    /// A grid of `bpm` from the start, `secs` long.
    fn grid(bpm: f64, secs: f64) -> Grid {
        let beat = 60.0 / bpm;
        Grid {
            bpm,
            beats: (0..(secs / beat) as usize).map(|i| i as f64 * beat).collect(),
            bars: Vec::new(),
        }
    }

    /// Records `commands` on fresh decks for `secs` seconds; returns the
    /// session's file and its tracks.
    fn record(name: &str, commands: Vec<Command>, secs: f64) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deckhand-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rate = 48_000;
        let mut live = Offline::new(rate, &blank_snapshot(rate));
        let wav = dir.join("set.wav");
        let (recording, tap) = crate::record::Recording::start(&wav, rate).unwrap();
        live.command(Command::Record(Some(tap))).unwrap();
        for command in commands {
            live.command(command).unwrap();
        }
        let mut buffer = vec![0.0; 512 * 2];
        for _ in 0..(secs * f64::from(rate) / 512.0) as usize {
            live.render(&mut buffer);
        }
        live.command(Command::Record(None)).unwrap();
        live.render(&mut buffer);
        recording.finish().unwrap();
        session_path(&wav)
    }

    #[test]
    fn curves_become_changes_along_them() {
        let a = track(48_000, 4.0, 5);
        let path = PathBuf::from("/music/curve.wav");
        remember_track(&a, &path);
        let file = record(
            "curves",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::SetPlaying { deck: 0, playing: true },
            ],
            2.0,
        );
        let tracks = || Tracks([(path.clone(), a.clone())].into_iter().collect());
        let mut session = Session::read(&file, tracks()).unwrap();
        let fade = vec![
            Point { at: 0, value: 1.0, curve: Curve::Step },
            // On buffer starts (512 frames), where changes take effect.
            Point { at: 24_576, value: 1.0, curve: Curve::Smooth },
            Point { at: 71_680, value: 0.0, curve: Curve::Step },
        ];
        let volume = Setting::Volume(0);
        session.set_lane(volume, &fade);
        assert_eq!(session.lane(volume), fade);
        let changes = session
            .events
            .iter()
            .filter(|(_, e)| matches!(e, Logged::Command(Command::SetVolume { .. })))
            .count();
        // A change every buffer along the fade.
        assert!((90..=95).contains(&changes), "{changes}");
        let halfway = lane_value(&session.lane(volume), 48_128);
        assert!((halfway - 0.5).abs() < 1e-9, "{halfway}");
        let edited = file.with_file_name("edited.session");
        session.write(&edited).unwrap();
        let again = Session::read(&edited, tracks()).unwrap();
        assert_eq!(again.lane(volume), fade);
        let (mut first, mut second) = (Vec::new(), Vec::new());
        session
            .render(|b| {
                first.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        again
            .render(|b| {
                second.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        assert_eq!(first, second);
        // Quieter after the fade than before it.
        let level = |s: &[f32]| s.iter().map(|x| x.abs()).sum::<f32>();
        assert!(level(&first[80_000 * 2..90_000 * 2]) < level(&first[..10_000 * 2]) * 0.01);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_moved_track_plays_later() {
        let a = track(48_000, 4.0, 8);
        let path = PathBuf::from("/music/moved.wav");
        remember_track(&a, &path);
        let file = record(
            "move",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::SetPlaying { deck: 0, playing: true },
            ],
            2.0,
        );
        let tracks = || Tracks([(path.clone(), a.clone())].into_iter().collect());
        let mut session = Session::read(&file, tracks()).unwrap();
        let play = |session: &Session| {
            let mut out = Vec::new();
            session
                .render(|b| {
                    out.extend_from_slice(b);
                    Ok(())
                })
                .unwrap();
            out
        };
        let original = play(&session);
        // 10 ms later from half a second on.
        let at = session.move_track(0, 24_000, 0.01);
        assert_eq!(at, 24_064);
        assert_eq!(session.moves(), [TrackMove { deck: 0, at, later: 0.01 }]);
        let moved = play(&session);
        let at = at as usize;
        assert_eq!(moved[..at * 2], original[..at * 2]);
        // From there on, what played 480 frames before.
        let window = (at + 1000) * 2..(at + 40_000) * 2;
        let earlier = window.start - 960..window.end - 960;
        assert_eq!(moved[window], original[earlier]);
        // Moved back, and the move is gone.
        session.move_track(0, at as u64, 0.0);
        assert!(session.moves().is_empty());
        assert_eq!(play(&session), original);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn beat_fx_stretches_move_and_change() {
        use BeatFxKind::{Echo, Reverb};
        let p = |on, kind, depth| BeatFxParams {
            kind,
            on,
            depth,
            beat_secs: 0.5,
            target: FxTarget::Master,
            release: false,
        };
        let changes = vec![
            (0, p(false, Echo, 0.5)),
            (1000, p(true, Echo, 0.5)),
            (1500, p(true, Echo, 0.8)),
            (3000, p(false, Echo, 0.8)),
            (5000, p(true, Reverb, 0.3)),
        ];
        let spans = fx_spans(&changes, 8000);
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].from, spans[0].to, spans[0].kind), (1000, 3000, Echo));
        assert_eq!(spans[0].depth, [(1000, 0.5), (1500, 0.8)]);
        assert_eq!((spans[1].from, spans[1].to, spans[1].kind), (5000, 8000, Reverb));

        let moved = fx_spans(&move_fx(&changes, &spans[0], 2000, 4000), 8000);
        assert_eq!((moved[0].from, moved[0].to), (2000, 4000));
        assert_eq!(moved[0].depth, [(2000, 0.5), (2500, 0.8)]);
        assert_eq!(moved[1], spans[1]);

        let restyled =
            restyle_fx(&changes, &spans[0], Some(Reverb), Some(FxTarget::Channel(1)), None);
        let spans_now = fx_spans(&restyled, 8000);
        assert_eq!((spans_now[0].kind, spans_now[0].target), (Reverb, FxTarget::Channel(1)));
        // Its echoes ring out as the same effect; the next one is untouched.
        assert_eq!(restyled[3].1.kind, Reverb);
        assert_eq!(spans_now[1], spans[1]);

        let removed = fx_spans(&remove_fx(&changes, &spans[0]), 8000);
        assert_eq!(removed, [spans[1].clone()]);
    }

    #[test]
    fn played_shows_levels_and_beats_where_they_fell() {
        let a = track(48_000, 4.0, 9);
        let path = PathBuf::from("/music/beats.wav");
        remember_track(&a, &path);
        let mut beats = grid(120.0, 4.0);
        beats.bars = (0..beats.beats.len()).map(|i| (i % 4 + 1) as u16).collect();
        remember_grid(&a, beats);
        let file = record(
            "played",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::SetPlaying { deck: 0, playing: true },
            ],
            2.2,
        );
        let session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let played = session.duplicate().unwrap().played().unwrap();
        // Beats every half second (24 000 frames), bars every four.
        let deck = &played[0];
        assert_eq!(deck.beats.len(), 4, "{:?}", deck.beats);
        for (i, &(at, bar)) in deck.beats.iter().enumerate() {
            let k = i as u64 + 1;
            assert!(at.abs_diff(k * 24_000) <= 1, "{:?}", deck.beats);
            assert_eq!(bar, k.is_multiple_of(4));
        }
        assert!(deck.levels.iter().all(|&(_, played, heard)| played > 0.0 && heard > 0.0));
        assert!(played[1].levels.is_empty());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_clip_moves_with_what_was_done_to_it() {
        let (a, b) = (track(48_000, 4.0, 10), track(48_000, 4.0, 11));
        let (pa, pb) = (PathBuf::from("/music/clip-a.wav"), PathBuf::from("/music/clip-b.wav"));
        remember_track(&a, &pa);
        remember_track(&b, &pb);
        // Deck 1 plays A, then B from a second on, faded down at 1.5 s.
        let dir = std::env::temp_dir().join(format!("deckhand-clip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rate = 48_000;
        let mut live = Offline::new(rate, &blank_snapshot(rate));
        let wav = dir.join("set.wav");
        let (recording, tap) = crate::record::Recording::start(&wav, rate).unwrap();
        let mut buffer = vec![0.0; 512 * 2];
        let mut play = |live: &mut Offline, blocks: usize| {
            for _ in 0..blocks {
                live.render(&mut buffer);
            }
        };
        live.command(Command::Record(Some(tap))).unwrap();
        live.command(Command::Load { deck: 0, audio: a.clone() }).unwrap();
        live.command(Command::SetPlaying { deck: 0, playing: true }).unwrap();
        play(&mut live, 94);
        live.command(Command::Load { deck: 0, audio: b.clone() }).unwrap();
        live.command(Command::SetPlaying { deck: 0, playing: true }).unwrap();
        play(&mut live, 47);
        live.command(Command::SetVolume { deck: 0, level: 0.3 }).unwrap();
        play(&mut live, 100);
        live.command(Command::Record(None)).unwrap();
        live.render(&mut vec![0.0; 1024]);
        recording.finish().unwrap();

        let tracks = Tracks([(pa, a), (pb, b)].into_iter().collect());
        let mut session = Session::read(&session_path(&wav), tracks).unwrap();
        let spans = session.tracks();
        assert_eq!(spans.iter().map(|s| s.from).collect::<Vec<_>>(), [0, 94 * 512]);
        let backup = session.backup().unwrap();
        let original = played_through(&session);

        // B half a second later: its load and its fade go along, A plays on.
        let landed = session.move_clip(0, 94 * 512, 24_000).unwrap();
        assert_eq!(landed, 141 * 512);
        let spans = session.tracks();
        assert_eq!((spans[0].to, spans[1].from), (landed, landed));
        let volume = session.lane(Setting::Volume(0));
        assert_eq!(volume.last().unwrap().at, (141 + 47) * 512);
        let moved = played_through(&session);
        assert_ne!(moved, original);
        // Undone, as recorded.
        session.restore(&backup).unwrap();
        let again = played_through(&session);
        assert_eq!(again, original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pieces_cut_shrink_and_come_back() {
        let a = track(48_000, 4.0, 12);
        let path = PathBuf::from("/music/pieces.wav");
        remember_track(&a, &path);
        let file = record(
            "pieces",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::SetPlaying { deck: 0, playing: true },
            ],
            2.0,
        );
        let mut session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let span = session.tracks()[0].clone();
        assert_eq!(session.pieces(&span), [(0, span.to)]);
        let original = played_through(&session);

        // Split at 0.5 s, the first piece shrunk to 0.25 s, the last from
        // 1.5 s on taken out: heard 0–0.25 s and 0.5–1.5 s.
        let (quarter, half, later) = (12_288, 24_064, 72_192);
        let pieces = [(0, quarter), (half, later)];
        session.set_pieces(&span, &pieces);
        assert_eq!(session.pieces(&span), pieces);
        let cut = played_through(&session);
        let level = |s: &[f32], from: usize, to: usize| {
            s[from * 2..to * 2].iter().map(|x| x.abs()).fold(0.0f32, f32::max)
        };
        // Faded out within 5 ms (240 frames) of each cut.
        assert!(level(&cut, quarter as usize + 300, half as usize) == 0.0);
        assert!(level(&cut, later as usize + 300, cut.len() / 2) == 0.0);
        assert_eq!(cut[..10_000 * 2], original[..10_000 * 2]);
        assert_eq!(cut[30_000 * 2..60_000 * 2], original[30_000 * 2..60_000 * 2]);
        // Touching pieces stay split; all of it back is the recording.
        session.set_pieces(&span, &[(0, half), (half, span.to)]);
        assert_eq!(session.pieces(&span), [(0, half), (half, span.to)]);
        assert_eq!(played_through(&session), original);
        session.set_pieces(&span, &[(0, span.to)]);
        assert_eq!(played_through(&session), original);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn stems_are_kept_with_the_recording() {
        let rate = 48_000;
        let a = track(rate, 3.0, 13);
        let path = PathBuf::from("/music/stemmed.wav");
        remember_track(&a, &path);
        let vocals = track(rate, 3.0, 14);
        let vocals = TrackAudio {
            sample_rate: rate,
            samples: vocals.samples.iter().map(|s| s * 0.5).collect(),
        };
        let stems = Arc::new(StemLayers::new(Some(&vocals), None, None, rate, a.frames()));
        let cache = StemsSource {
            track: path.clone(),
            dir: PathBuf::from("/nowhere/stem-cache/stemmed"),
            stems: vec!["vocals".into(), "other".into()],
        };
        remember_stems(&stems, cache.clone());
        let file = record(
            "kept",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::SetStems { deck: 0, stems: Some(stems.clone()) },
                Command::SetStemLevels { deck: 0, levels: [0.2, 1.0, 1.0, 1.0] },
                Command::SetPlaying { deck: 0, playing: true },
            ],
            1.5,
        );
        let kept = file.parent().unwrap().join("stems").join("stemmed");
        assert!(StemLayers::saved_in(&kept), "{}", kept.display());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(&kept.display().to_string()) && !text.contains("/nowhere"));

        // Played again from the kept stems: the vocals down as recorded.
        drop(stems);
        let session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        assert!(session.warnings().is_empty(), "{:?}", session.warnings());
        let wav = file.with_extension("wav");
        let rendered = session.render_to_wav(&file.with_file_name("again.wav"), Some(&wav), |_| {});
        let comparison = rendered.unwrap().comparison.unwrap();
        assert_eq!(comparison.differing, 0, "{comparison:?}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// The Beat FX depth gets a lane: sounding the same at first, then
    /// followed by the effect's changes when edited.
    #[test]
    fn beat_fx_depth_lane() {
        let a = track(48_000, 3.0, 0);
        let path = PathBuf::from("/music/fx.wav");
        remember_track(&a, &path);
        let echo = BeatFxParams {
            kind: crate::fx::BeatFxKind::Echo,
            on: true,
            depth: 0.3,
            ..BeatFxParams::default()
        };
        let file = record(
            "depth",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::SetPlaying { deck: 0, playing: true },
                Command::SetBeatFx(echo),
            ],
            2.0,
        );
        let mut session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let render = |s: &Session| {
            let mut out = Vec::new();
            s.render(|b| Ok(out.extend_from_slice(b))).unwrap();
            out
        };
        let before = render(&session);
        assert!(session.split_fx_depth());
        assert_eq!(render(&session), before, "the lane sounds the same");
        let lane = session.lane(Setting::BeatFxDepth);
        assert!(lane.iter().any(|p| (p.value - 0.3).abs() < 1e-6), "{lane:?}");
        // Full depth from a second in: heard, and the effect's own change
        // there takes the lane's depth.
        let half = session.snap(48_000);
        session.set_lane(
            Setting::BeatFxDepth,
            &[Point::step(0, 0.3), Point { at: half, value: 1.0, curve: Curve::Step }],
        );
        let mut changes = session.beat_fx();
        changes.push((half, BeatFxParams { beat_secs: 0.25, ..echo }));
        session.set_beat_fx(&changes);
        let later: Vec<f32> =
            session.beat_fx().iter().filter(|c| c.0 >= half).map(|c| c.1.depth).collect();
        assert_eq!(later, [1.0]);
        assert_ne!(render(&session), before);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Part of a clip moved a second later plays there, laid over what
    /// was there, and leaves silence where it was.
    #[test]
    fn moved_part_plays_at_its_new_place() {
        let a = track(48_000, 4.0, 3);
        let path = PathBuf::from("/music/part.wav");
        remember_track(&a, &path);
        let file = record(
            "part",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::SetPlaying { deck: 0, playing: true },
            ],
            3.5,
        );
        let mut session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let render = |s: &Session| {
            let mut out = Vec::new();
            s.render(|b| Ok(out.extend_from_slice(b))).unwrap();
            out
        };
        let before = render(&session);
        let span = session.tracks()[0].clone();
        // On buffer starts, half a second long, moved a second on.
        let (from, to, by) = (47 * 512, 94 * 512, 94 * 512);
        let (new_from, new_to) = session.move_part(&span, from, to, by, 1.0);
        assert_eq!((new_from, new_to), (from + 94 * 512, to + 94 * 512));
        let after = render(&session);
        // Away from the fades at the edges.
        let margin = 1024;
        let worst = |a: &[f32], a0: u64, b: &[f32], b0: u64, len: u64| {
            (0..len as usize * 2)
                .map(|i| (a[a0 as usize * 2 + i] - b[b0 as usize * 2 + i]).abs())
                .fold(0.0f32, f32::max)
        };
        let len = to - from - 2 * margin;
        let moved = worst(&after, new_from + margin, &before, from + margin, len);
        assert!(moved < 1e-3, "the part plays at its new place: {moved}");
        let old = (from + margin) as usize * 2..(to - margin) as usize * 2;
        let left = after[old].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(left < 1e-3, "silent where it was: {left}");
        // After it, the track goes on as before.
        let rest = worst(&after, new_to + margin, &before, new_to + margin, 24_000);
        assert!(rest < 1e-3, "the rest as before: {rest}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Another beat (or type, target) makes a block of its own; moving or
    /// removing one leaves the one right after it alone.
    #[test]
    fn beat_fx_blocks_split_by_their_settings() {
        let echo = |beat: f32| BeatFxParams {
            kind: crate::fx::BeatFxKind::Echo,
            on: true,
            beat_secs: beat,
            ..BeatFxParams::default()
        };
        let off = BeatFxParams { on: false, ..echo(0.25) };
        let changes = vec![(0, off), (100, echo(0.125)), (200, echo(0.25)), (300, off)];
        let spans = fx_spans(&changes, 1000);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!((spans[0].from, spans[0].to, spans[0].beat_secs), (100, 200, 0.125));
        assert_eq!((spans[1].from, spans[1].to, spans[1].beat_secs), (200, 300, 0.25));

        // The 1/8 one off: the 1/4 one plays on.
        let removed = fx_spans(&remove_fx(&changes, &spans[0]), 1000);
        assert_eq!(removed.len(), 1);
        assert_eq!((removed[0].from, removed[0].to, removed[0].beat_secs), (200, 300, 0.25));

        // The 1/8 one moved later, past the other: both keep their place.
        let moved = fx_spans(&move_fx(&changes, &spans[0], 500, 600), 1000);
        let places: Vec<_> = moved.iter().map(|s| (s.from, s.to, s.beat_secs)).collect();
        assert_eq!(places, [(200, 300, 0.25), (500, 600, 0.125)]);

        // A new beat for the first: only it changes.
        let restyled = fx_spans(&restyle_fx(&changes, &spans[0], None, None, Some(0.5)), 1000);
        let beats: Vec<_> = restyled.iter().map(|s| s.beat_secs).collect();
        assert_eq!(beats, [0.5, 0.25]);
    }

    /// A part rendered alone is the same as that part of the whole.
    #[test]
    fn a_part_renders_as_in_the_whole() {
        let a = track(48_000, 3.0, 2);
        let path = PathBuf::from("/music/whole.wav");
        remember_track(&a, &path);
        let file = record(
            "part-render",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::SetPlaying { deck: 0, playing: true },
            ],
            2.5,
        );
        let session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let mut whole = Vec::new();
        session.render(|b| Ok(whole.extend_from_slice(b))).unwrap();
        let (from, to) = (30_001, 70_003);
        let out = file.with_file_name("part.wav");
        let written = session.render_part_to_wav(&out, from, to, |_| {}).unwrap();
        assert_eq!(written, to - from);
        let part = crate::decode::decode_file(&out).unwrap();
        assert_eq!(part.frames() as u64, to - from);
        let worst = (0..part.samples.len())
            .map(|i| (part.samples[i] - whole[from as usize * 2 + i]).abs())
            .fold(0.0f32, f32::max);
        // 24-bit rounding.
        assert!(worst < 1e-5, "{worst}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn two_basses_at_once_clash_until_one_is_cut() {
        let a = track(48_000, 3.0, 0);
        let path = PathBuf::from("/music/bass.wav");
        remember_track(&a, &path);
        let file = record(
            "clash",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::Load { deck: 1, audio: a.clone() },
                Command::SetPlaying { deck: 0, playing: true },
                Command::SetPlaying { deck: 1, playing: true },
            ],
            2.0,
        );
        let mut session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        // Deck 2's low cut a second in.
        session.set_lane(
            Setting::Eq(1, 0),
            &[Point::step(0, 0.0), Point::step(48_000, f64::from(crate::eq::EQ_MIN_DB))],
        );
        let played = session.played().unwrap();
        let found = clashes(&played, 24_000);
        assert_eq!(found.len(), 1, "{found:?}");
        let clash = found[0];
        assert_eq!(clash.decks, (0, 1));
        assert!(clash.from < 1024, "{clash:?}");
        assert!(clash.to.abs_diff(48_000) < 4096, "{clash:?}");
        // Faded out, deck 2 does not clash at all.
        session.set_lane(Setting::Eq(1, 0), &[Point::step(0, 0.0)]);
        session.set_lane(Setting::Volume(1), &[Point::step(0, 0.0)]);
        assert!(clashes(&session.played().unwrap(), 24_000).is_empty());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Everything a session plays.
    fn played_through(session: &Session) -> Vec<f32> {
        let mut out = Vec::new();
        session
            .render(|b| {
                out.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        out
    }

    /// The decks where a session ends.
    struct Last(Vec<crate::engine::DeckView>);

    impl Run for Last {
        fn after(&mut self, _: u64, _: &[f32], mixer: &mut Offline) -> Result<()> {
            self.0 = (0..2).map(|d| mixer.deck(d)).collect();
            Ok(())
        }
    }

    #[test]
    fn sync_is_done_again_after_a_tempo_edit() {
        let (a, b) = (track(48_000, 8.0, 6), track(48_000, 8.0, 7));
        let (pa, pb) = (PathBuf::from("/music/sync-a.wav"), PathBuf::from("/music/sync-b.wav"));
        remember_track(&a, &pa);
        remember_track(&b, &pb);
        let (ga, gb) = (grid(120.0, 8.0), grid(125.0, 8.0));
        remember_grid(&a, ga.clone());
        remember_grid(&b, gb.clone());
        let file = record(
            "sync",
            vec![
                Command::Load { deck: 0, audio: a.clone() },
                Command::Load { deck: 1, audio: b.clone() },
                // Off the beat by a fifth of one, which sync puts right.
                Command::Seek { deck: 1, seconds: 0.096 },
                Command::SetPlaying { deck: 0, playing: true },
                Command::SetPlaying { deck: 1, playing: true },
                Command::SyncState { master: Some(0), synced: [false, true, false, false] },
                Command::SyncOffset { deck: 1, beats: 0.0 },
                Command::SyncTempo { deck: 1, ratio: 0.96 },
            ],
            3.0,
        );
        let tracks = Tracks([(pa, a), (pb, b)].into_iter().collect());
        let mut session = Session::read(&file, tracks).unwrap();
        assert!(session.has_sync());
        assert_eq!(session.grids.len(), 2);
        assert_eq!(
            session.sync_spans(),
            [SyncSpan { deck: 1, master: 0, from: 0, to: session.length() }]
        );

        // The master speeds up after a second.
        let tempo = Setting::Tempo(0);
        let mut lane = session.lane(tempo);
        lane.push(Point::step(48_000, 1.08));
        session.set_lane(tempo, &lane);
        assert!(session.resync().unwrap() > 0);
        let follower = session.lane(Setting::Tempo(1));
        let last = follower.last().unwrap();
        assert!((last.value - 1.08 * 120.0 / 125.0).abs() < 1e-9, "{follower:?}");
        // A buffer after the master's (snapped to 48 128), as live.
        assert_eq!(last.at, 48_640, "{follower:?}");

        let mut end = Last(Vec::new());
        session.run(session.events.iter(), &mut end).unwrap();
        let (m, d) = (end.0[0], end.0[1]);
        let mut phase = gb.beat_at(d.seconds).fract() - ga.beat_at(m.seconds).fract();
        phase -= phase.round();
        assert!(phase.abs() <= SYNC_DRIFT_BEATS, "{phase}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn an_unknown_track_is_refused() {
        let audio = track(1000, 0.1, 9);
        let line = serde_json::to_string(&Command::Load { deck: 0, audio }).unwrap();
        assert_eq!(line, r#"{"Load":{"deck":0,"audio":""}}"#);
        OPENED.set(None);
        assert!(serde_json::from_str::<Command>(&line).is_err());
    }
}
