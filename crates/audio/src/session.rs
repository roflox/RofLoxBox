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
    /// The folder the session file is in: stems kept with a recording are
    /// found there after it was renamed or moved.
    folder: Option<PathBuf>,
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
        // Not on this computer: exported along with the session.
        let exported = (!path.exists())
            .then(|| opened.folder.as_deref().and_then(|f| exported_track(f, path)))
            .flatten();
        let audio = opened
            .sources
            .track(exported.as_deref().unwrap_or(path))
            .with_context(|| format!("opening {}", path.display()))?;
        remember_track(&audio, path);
        opened.tracks.insert(path.to_path_buf(), audio.clone());
        Ok(audio)
    })
}

/// The folder in an exported session's folder its tracks are copied to,
/// and the file there naming each track's copy by its original path.
const EXPORTED_TRACKS: &str = "tracks";
const EXPORTED_INDEX: &str = "index.json";

/// The copy of `path` exported with the session in `folder`, if there.
fn exported_track(folder: &Path, path: &Path) -> Option<PathBuf> {
    let tracks = folder.join(EXPORTED_TRACKS);
    let index = std::fs::read(tracks.join(EXPORTED_INDEX)).ok()?;
    let index: HashMap<PathBuf, String> = serde_json::from_slice(&index).ok()?;
    let copy = tracks.join(index.get(path)?);
    copy.exists().then_some(copy)
}

/// A track's outline, to draw its clips from (as a DAW does): the
/// loudest sample, either channel, in every `step` frames, and in every
/// `step * COARSE` (to draw zoomed out).
#[derive(Debug, Clone, Default)]
pub struct Outline {
    pub rate: u32,
    pub step: usize,
    pub fine: Vec<f32>,
    pub coarse: Vec<f32>,
}

impl Outline {
    pub const COARSE: usize = 32;

    fn of(audio: &TrackAudio, step: usize) -> Self {
        let step = step.max(1);
        let fine: Vec<f32> = audio
            .samples
            .chunks(step * 2)
            .map(|c| c.iter().fold(0f32, |m, s| m.max(s.abs())))
            .collect();
        let coarse =
            fine.chunks(Self::COARSE).map(|c| c.iter().copied().fold(0f32, f32::max)).collect();
        Self { rate: audio.sample_rate, step, fine, coarse }
    }

    /// The loudest between `from` and `to` seconds into the track.
    pub fn peak(&self, from: f64, to: f64) -> f32 {
        let (from, to) = (from.min(to), from.max(to));
        let per = f64::from(self.rate) / self.step as f64;
        let (a, b) = ((from * per).floor().max(0.0) as usize, (to * per).ceil().max(0.0) as usize);
        let (list, a, b) = if b - a > 4 * Self::COARSE {
            (&self.coarse, a / Self::COARSE, b.div_ceil(Self::COARSE))
        } else {
            (&self.fine, a, b.max(a + 1))
        };
        list.get(a.min(list.len())..b.min(list.len()))
            .map_or(0.0, |s| s.iter().copied().fold(0f32, f32::max))
    }
}

/// What [`export_files`] did.
#[derive(Debug, Default)]
pub struct Exported {
    pub tracks: usize,
    pub bytes: u64,
    /// Tracks not found to copy.
    pub missing: Vec<PathBuf>,
}

/// Copies `tracks` (the files a session plays) into `dir`'s `tracks/`
/// folder, and the stems kept with the recording (`stems`, the `stems/`
/// folder beside it) into `dir/stems`: with the session written into
/// `dir`, it opens on another computer, finding them there.
pub fn export_files(dir: &Path, tracks: &[PathBuf], stems: Option<&Path>) -> Result<Exported> {
    let folder = dir.join(EXPORTED_TRACKS);
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let mut exported = Exported::default();
    let mut index: HashMap<PathBuf, String> = HashMap::new();
    for track in tracks {
        if !track.is_file() {
            exported.missing.push(track.clone());
            continue;
        }
        let stem = track.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let extension = track.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        // Tracks with the same name from different folders: numbered.
        let name = (1..)
            .map(|n| if n == 1 { format!("{stem}{extension}") } else { format!("{stem} {n}{extension}") })
            .find(|name| !index.values().any(|taken| taken == name))
            .unwrap_or_default();
        let to = folder.join(&name);
        exported.bytes += std::fs::copy(track, &to)
            .with_context(|| format!("copying {} to {}", track.display(), to.display()))?;
        exported.tracks += 1;
        index.insert(track.clone(), name);
    }
    let written = serde_json::to_vec_pretty(&index)?;
    std::fs::write(folder.join(EXPORTED_INDEX), written)?;
    if let Some(stems) = stems.filter(|s| s.is_dir()) {
        exported.bytes += copy_dir(stems, &dir.join("stems"))?;
    }
    Ok(exported)
}

/// Copies the folder `from` (with what is in it) to `to`; returns the bytes.
fn copy_dir(from: &Path, to: &Path) -> Result<u64> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let mut bytes = 0;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            bytes += copy_dir(&entry.path(), &target)?;
        } else {
            bytes += std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(bytes)
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
        // Kept with a recording whose folder was renamed or moved since:
        // its stems/ folder beside the session.
        let moved = opened.folder.as_ref().and_then(|folder| {
            let name = source.dir.file_name()?;
            let kept_here = source.dir.parent()?.file_name()? == "stems";
            let here = folder.join("stems").join(name);
            (kept_here && !source.dir.exists() && StemLayers::saved_in(&here))
                .then(|| StemsSource { dir: here, ..source.clone() })
        });
        match opened.sources.stems(moved.as_ref().unwrap_or(source), &track) {
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

/// How far a play's edges can go (frames from the start, inclusive):
/// see [`Session::play_edges`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayEdges {
    pub start: (u64, u64),
    pub end: (u64, u64),
}

/// The deck's place in its track changes other than by playing: a jump,
/// a scratch, a loop.
fn is_jump(event: &Logged) -> bool {
    matches!(
        event,
        Logged::Jog(..)
            | Logged::Command(
                Command::Seek { .. }
                    | Command::Shift { .. }
                    | Command::Hold { .. }
                    | Command::JogMove { .. }
                    | Command::SetLoop { .. }
            )
    )
}

/// A nudge: `deck` sped up or slowed down a moment (the jog's edge, the
/// pitch bend buttons) from `from` to `to` (frames from the start), which
/// left its track `shift` seconds ahead (or behind, negative).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Nudge {
    pub deck: usize,
    pub from: u64,
    pub to: u64,
    pub shift: f64,
}

/// Nudges this close together (seconds) are one (taps in a row).
const NUDGE_GAP_SECS: f64 = 0.3;

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
            folder: path.parent().map(Path::to_path_buf),
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

    /// Moves the end out past the last thing done, with [`TAIL_SECS`] to
    /// ring out, when an edit put something after it: a set is as long as
    /// what is in it.
    fn stretch_end(&mut self) {
        let start = self.snapshot.clock;
        let Some(i) = self.events.iter().rposition(|(_, e)| matches!(e, Logged::End)) else {
            return;
        };
        let last = self
            .events
            .iter()
            .filter(|(_, e)| !matches!(e, Logged::End | Logged::Block(_)))
            .map(|(at, _)| *at)
            .max()
            .unwrap_or(start);
        if last < self.events[i].0 {
            return;
        }
        let tail = (TAIL_SECS * f64::from(self.rate)) as u64;
        let end = start + self.block_ceil(last - start + tail);
        let (_, event) = self.events.remove(i);
        self.events.push((end, event));
        self.end = Some(end);
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
        let jump_split = (JUMP_SPLIT_SECS * f64::from(self.rate)) as u64;
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
                // A jump while playing (a hot cue, a beat jump): from there
                // it is a play of its own, unless the one before would be a
                // sliver (dragging the needle).
                Command::Seek { deck, .. } if deck < open.len() => {
                    if let Some((span, Some(since))) = open[deck].as_mut()
                        && at >= *since + jump_split
                    {
                        span.playing.push((*since, at));
                        *since = at;
                    }
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

    /// Each track's outline (see [`Outline`]), every `step` frames.
    pub fn outlines(&self, step: usize) -> HashMap<PathBuf, Outline> {
        self.tracks.iter().map(|(path, audio)| (path.clone(), Outline::of(audio, step))).collect()
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
            // Inside the clip, on a buffer start like every event.
            let at = self.snap(at);
            let at = if at >= span.to { self.block_floor(span.to.saturating_sub(1)) } else { at };
            let at = at.max(span.from);
            self.events.push((start + at, Logged::Command(Command::Gate { deck, open })));
        }
        self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        self.drop_deleted();
    }

    /// Takes out the clips nothing of is heard: cut out whole (deleted), or
    /// loaded and never played. What their deck did for them (the load,
    /// plays, jumps, cuts) goes, as if the track had not been loaded there;
    /// the deck stays stopped. Clips the recording began with stay. Returns
    /// how many went.
    pub fn drop_deleted(&mut self) -> usize {
        let start = self.snapshot.clock;
        let spans = self.tracks();
        let mut dropped = 0;
        for (k, span) in spans.iter().enumerate() {
            let deck = span.deck;
            let loaded_here = self.events.iter().any(|(at, e)| {
                *at == start + span.from
                    && matches!(e, Logged::Command(Command::Load { deck: d, .. }) if *d == deck)
            });
            // Played and all cut out, or (split off where the deck stood
            // still) loaded and never played: nothing heard, gone.
            if !self.heard(span).is_empty() || !loaded_here {
                continue;
            }
            let (from, to) = (start + span.from, start + span.to);
            self.events.retain(|(at, e)| !((from..to).contains(at) && gives_way(e, deck, true)));
            // The track before it, which the load stopped, stops there still.
            let before = k.checked_sub(1).map(|i| &spans[i]).filter(|t| t.deck == deck && t.to == span.from);
            if before.is_some_and(|t| t.playing.last().is_some_and(|p| p.1 == span.from)) {
                let i = self.events.partition_point(|(t, _)| *t < from);
                self.events.insert(i, (from, Logged::Command(Command::SetPlaying { deck, playing: false })));
            }
            dropped += 1;
        }
        dropped
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

    /// The buffer start at or before `at`.
    fn block_floor(&self, at: u64) -> u64 {
        let start = self.snapshot.clock;
        let (from, size) = self.block_at(at);
        if size == 0 {
            return at;
        }
        let offset = (start + at).saturating_sub(from);
        (from + offset / size * size).saturating_sub(start)
    }

    /// The buffer start at or after `at`.
    fn block_ceil(&self, at: u64) -> u64 {
        if self.block_floor(at) == at { at } else { self.next_block(at) }
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
            // The stems are open already (by the names the session has).
            folder: None,
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
        if let Some(end) = self.events.iter().rev().find(|(_, e)| matches!(e, Logged::End)) {
            self.end = Some(end.0);
        }
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
        // Not onto the start of the clip heard before it (silent ones give
        // way).
        let low = spans[..index]
            .iter()
            .rev()
            .find(|t| !self.heard(t).is_empty())
            .map_or(0, |t| t.from + 1);
        let landing = (from as i64 + by).max(low as i64) as u64;
        let landing = self.snap(landing);
        let landing = if landing < low { self.block_ceil(low) } else { landing };
        let by = landing as i64 - from as i64;
        if by == 0 {
            return Ok(from);
        }
        let next = spans.get(index + 1).map(|s| s.from);
        let (moved, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.events).into_iter().partition(|(at, event)| {
                deck_of(event) == Some(deck)
                    && (start + from..start + end).contains(at)
                    // The stop of the clip before it (split there) stays.
                    && !(*at == start + from
                        && matches!(event, Logged::Command(Command::SetPlaying { playing: false, .. })))
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
        self.stretch_end();
        Ok(landing)
    }

    /// Plays the session until `places` has them all.
    fn find_places(&self, places: &mut Places) -> Result<()> {
        match self.run(self.events.iter(), places) {
            Err(err) if err.to_string() != PLACES_FOUND => Err(err),
            _ => Ok(()),
        }
    }

    /// Makes each play of the clip on `deck` loaded at `from` a clip of its
    /// own (to move and resize on its own): the track is loaded again where
    /// each play after the first starts, at the place it was, with its stems
    /// and cut as they were, so it sounds the same. False if there was
    /// nothing to split.
    pub fn split_plays(&mut self, deck: usize, from: u64) -> Result<bool> {
        let start = self.snapshot.clock;
        let Some(span) = self.tracks().into_iter().find(|t| t.deck == deck && t.from == from)
        else {
            bail!("no track on deck {} there", deck + 1);
        };
        if span.playing.len() < 2 {
            return Ok(false);
        }
        let starts: Vec<u64> = span.playing[1..].iter().map(|p| start + p.0).collect();
        let mut places = Places { deck, at: starts.clone(), seconds: Vec::new(), tempo: Vec::new() };
        self.find_places(&mut places)?;
        if places.seconds.len() != starts.len() {
            bail!("the plays run past the end of the session");
        }
        let audio = self
            .events
            .iter()
            .rev()
            .find_map(|(at, e)| match e {
                Logged::Command(Command::Load { deck: d, audio }) if *d == deck && *at <= start + from => {
                    Some(audio.clone())
                }
                _ => None,
            })
            .or_else(|| self.snapshot.deck(deck).0.cloned())
            .ok_or_else(|| anyhow!("no track on deck {}", deck + 1))?;
        // What a new load forgets, as it was before `at`.
        let mut added = Vec::new();
        for (&at, &seconds) in starts.iter().zip(&places.seconds) {
            let (stems, levels, gate) = before_load(&self.events, deck, start + from, at);
            // Begun by a jump while the play before it went on: that one ends
            // there (moved apart, it doesn't run on).
            let pressed = self.events.iter().any(|(t, e)| {
                *t == at
                    && matches!(e, Logged::Command(Command::SetPlaying { deck: d, playing: true }) if *d == deck)
            });
            if !pressed {
                added.push((at, Logged::Command(Command::SetPlaying { deck, playing: false })));
            }
            added.push((at, Logged::Command(Command::Load { deck, audio: audio.clone() })));
            for c in stems.into_iter().chain(levels) {
                added.push((at, Logged::Command(self.copy(c)?)));
            }
            if !gate {
                added.push((at, Logged::Command(Command::Gate { deck, open: false })));
            }
            added.push((at, Logged::Command(Command::Seek { deck, seconds })));
            // Begun by a jump while it played: it plays on.
            if !pressed {
                added.push((at, Logged::Command(Command::SetPlaying { deck, playing: true })));
            }
        }
        // In front of what happens at the same moment.
        let mut events = Vec::with_capacity(self.events.len() + added.len());
        let mut added = added.into_iter().peekable();
        for event in std::mem::take(&mut self.events) {
            while added.peek().is_some_and(|a| a.0 <= event.0) {
                events.extend(added.next());
            }
            events.push(event);
        }
        events.extend(added);
        self.events = events;
        Ok(true)
    }

    /// Every nudge in the set, by deck and time.
    pub fn nudges(&self) -> Vec<Nudge> {
        let start = self.snapshot.clock;
        let rate = f64::from(self.rate);
        let lanes: Vec<Vec<Point>> =
            (0..crate::engine::DECKS).map(|d| self.lane(Setting::Tempo(d))).collect();
        // Per deck: the amount now, since when, and the run so far.
        let mut on: Vec<(f64, u64, Option<(u64, f64)>)> = vec![(0.0, 0, None); crate::engine::DECKS];
        let mut found = Vec::new();
        for (at, event) in &self.events {
            let at = at - start;
            let Logged::Command(command) = event else { continue };
            match *command {
                Command::SetNudge { deck, amount } if deck < on.len() => {
                    let (now, since, run) = &mut on[deck];
                    if let Some((_, shift)) = run.as_mut() {
                        *shift += *now * lane_value(&lanes[deck], *since) * (at - *since) as f64 / rate;
                    }
                    let amount = f64::from(amount);
                    match (run.is_some(), amount != 0.0) {
                        (false, true) => *run = Some((at, 0.0)),
                        (true, false) => {
                            if let Some((from, shift)) = run.take() {
                                found.push(Nudge { deck, from, to: at, shift });
                            }
                        }
                        _ => {}
                    }
                    (*now, *since) = (amount, at);
                }
                // A new track starts unnudged.
                Command::Load { deck, .. } if deck < on.len() => on[deck] = (0.0, at, None),
                _ => {}
            }
        }
        found.sort_by_key(|n| (n.deck, n.from));
        // Taps in a row (as the jog's edge is nudged) are one nudge.
        let gap = (NUDGE_GAP_SECS * rate) as u64;
        let mut merged: Vec<Nudge> = Vec::with_capacity(found.len());
        for n in found {
            match merged.last_mut() {
                Some(last) if last.deck == n.deck && n.from <= last.to + gap => {
                    last.to = n.to;
                    last.shift += n.shift;
                }
                _ => merged.push(n),
            }
        }
        merged
    }

    /// Takes `deck`'s nudges between `from` and `to` (frames) out. With
    /// `keep_step`, what each moved the track stays: the play it is in
    /// begins that much further on in the track (shifted as it starts, so
    /// nothing jumps while heard), and after it all is where it was.
    /// Returns how many went.
    pub fn remove_nudges(&mut self, deck: usize, from: u64, to: u64, keep_step: bool) -> usize {
        let start = self.snapshot.clock;
        let gone: Vec<Nudge> = self
            .nudges()
            .into_iter()
            .filter(|n| n.deck == deck && n.from < to.max(from + 1) && from <= n.to)
            .collect();
        if gone.is_empty() {
            return 0;
        }
        let plays: Vec<(u64, u64)> = self
            .tracks()
            .into_iter()
            .filter(|t| t.deck == deck)
            .flat_map(|t| t.playing)
            .collect();
        let mut shifts: Vec<(u64, f64)> = Vec::new();
        for n in &gone {
            let Some(play) = plays.iter().find(|p| p.0 <= n.from && n.from < p.1) else { continue };
            match shifts.iter_mut().find(|s| s.0 == play.0) {
                Some(s) => s.1 += n.shift,
                None => shifts.push((play.0, n.shift)),
            }
        }
        self.events.retain(|(at, e)| {
            let at = at - start;
            !(matches!(e, Logged::Command(Command::SetNudge { deck: d, .. }) if *d == deck)
                && gone.iter().any(|n| n.from <= at && at <= n.to))
        });
        if keep_step {
            for (play, seconds) in shifts {
                // After what begins the play (its cue, its PLAY).
                let clock = start + play;
                let i = self.events.partition_point(|(t, _)| *t <= clock);
                self.events.insert(i, (clock, Logged::Command(Command::Shift { deck, seconds })));
            }
        }
        gone.len()
    }

    /// Deletes `from`–`to` (frames) of `deck`, as in a DAW: a clip it
    /// cuts through is split there, into clips of their own before and
    /// after, and what is between goes.
    pub fn delete_range(&mut self, deck: usize, from: u64, to: u64) -> Result<()> {
        let (from, to) = (self.snap(from), self.snap(to));
        if from >= to {
            return Ok(());
        }
        for at in [to, from] {
            if self.tracks().iter().any(|t| t.deck == deck && t.from < at && at < t.to) {
                self.split_clip(deck, at)?;
            }
        }
        let inside: Vec<TrackSpan> = self
            .tracks()
            .into_iter()
            .filter(|t| t.deck == deck && from <= t.from && t.to <= to)
            .collect();
        for span in inside {
            self.set_pieces(&span, &[]);
        }
        Ok(())
    }

    /// Splits `deck`'s clip at `at` into two clips of their own (to move,
    /// resize and delete apart): the track is loaded again there, where it
    /// was, playing if it was, with its stems and cut as they were, so it
    /// sounds the same. Returns where it was split (a buffer start).
    pub fn split_clip(&mut self, deck: usize, at: u64) -> Result<u64> {
        let start = self.snapshot.clock;
        let at = self.snap(at);
        let Some(span) = self.tracks().into_iter().find(|t| t.deck == deck && t.from < at && at < t.to)
        else {
            bail!("no clip on deck {} there", deck + 1);
        };
        let clock = start + at;
        let mut places = Places { deck, at: vec![clock], seconds: Vec::new(), tempo: Vec::new() };
        self.find_places(&mut places)?;
        let seconds =
            places.seconds.first().copied().ok_or_else(|| anyhow!("past the end of the session"))?;
        let audio = self
            .events
            .iter()
            .rev()
            .find_map(|(t, e)| match e {
                Logged::Command(Command::Load { deck: d, audio }) if *d == deck && *t <= start + span.from => {
                    Some(audio.clone())
                }
                _ => None,
            })
            .or_else(|| self.snapshot.deck(deck).0.cloned())
            .ok_or_else(|| anyhow!("no track on deck {}", deck + 1))?;
        let playing = span.playing.iter().any(|p| p.0 < at && at < p.1);
        let (stems, levels, gate) = before_load(&self.events, deck, start + span.from, clock);
        let mut added = Vec::new();
        // The first ends there (moved apart, it doesn't run on).
        if playing {
            added.push(Logged::Command(Command::SetPlaying { deck, playing: false }));
        }
        added.push(Logged::Command(Command::Load { deck, audio }));
        for c in stems.into_iter().chain(levels) {
            added.push(Logged::Command(self.copy(c)?));
        }
        if !gate {
            added.push(Logged::Command(Command::Gate { deck, open: false }));
        }
        added.push(Logged::Command(Command::Seek { deck, seconds }));
        if playing {
            added.push(Logged::Command(Command::SetPlaying { deck, playing: true }));
        }
        // In front of what happens at that moment.
        let i = self.events.partition_point(|(t, _)| *t < clock);
        self.events.splice(i..i, added.into_iter().map(|e| (clock, e)));
        Ok(at)
    }

    /// What of `span` is heard: where it played and wasn't cut out.
    fn heard(&self, span: &TrackSpan) -> Vec<(u64, u64)> {
        let pieces = self.pieces(span);
        let mut heard = Vec::new();
        for &(a, b) in &span.playing {
            for &(c, d) in &pieces {
                if a.max(c) < b.min(d) {
                    heard.push((a.max(c), b.min(d)));
                }
            }
        }
        heard
    }

    /// Where in its track (seconds) `deck`'s play starting at `from` began,
    /// after what was done at that moment (a jump), and the deck's tempo.
    fn play_place(&self, deck: usize, from: u64) -> Result<(f64, f64)> {
        Ok(self.play_place_and(deck, from, None)?.0)
    }

    /// [`Session::play_place`], and where the deck is (and its tempo) at
    /// `also` (after `from`), from one replay.
    fn play_place_and(
        &self,
        deck: usize,
        from: u64,
        also: Option<u64>,
    ) -> Result<((f64, f64), Option<(f64, f64)>)> {
        let start = self.snapshot.clock;
        let mut at = vec![start + from];
        at.extend(also.filter(|&a| a > from).map(|a| start + a));
        let mut places = Places { deck, at, seconds: Vec::new(), tempo: Vec::new() };
        self.find_places(&mut places)?;
        let (Some(&before), Some(&tempo)) = (places.seconds.first(), places.tempo.first()) else {
            bail!("the play is past the end of the session");
        };
        let there = places.seconds.get(1).copied().zip(places.tempo.get(1).copied());
        let mut seconds = before;
        for (at, event) in &self.events {
            if *at != start + from {
                continue;
            }
            match event {
                Logged::Command(Command::Load { deck: d, .. }) if *d == deck => seconds = 0.0,
                Logged::Command(Command::Seek { deck: d, seconds: s }) if *d == deck => seconds = *s,
                Logged::Command(Command::Shift { deck: d, seconds: s }) if *d == deck => seconds += *s,
                Logged::Command(Command::MoveTrack { deck: d, later }) if *d == deck => seconds -= *later,
                _ => {}
            }
        }
        Ok(((seconds.max(0.0), tempo.max(0.01)), there))
    }

    /// The buffer start nearest `at` inside `low`–`high`; `None` if no
    /// buffer starts there.
    fn snap_within(&self, at: u64, low: u64, high: u64) -> Option<u64> {
        let (low, high) = (self.block_ceil(low), self.block_floor(high));
        (low <= high).then(|| self.snap(at).clamp(low, high))
    }

    /// How far the edges of the play `from`–`to` on `deck` (one of
    /// [`TrackSpan::playing`]) can go, as a clip of the track of its own.
    /// Its start: back to the track's beginning (the audio before the place
    /// it began), but not into what is heard before it on the deck; later,
    /// not past a jump inside it; it stays put when it began with the
    /// recording. Its end: on to the next thing heard on the deck. Whatever
    /// the deck did in between (cue hits, cut out or not, jumps, other
    /// tracks loaded and not heard) gives way. With more
    /// plays in its clip: not into the plays around it nor across a jump.
    pub fn play_edges(&self, deck: usize, from: u64, to: u64) -> Option<PlayEdges> {
        let spans: Vec<TrackSpan> = self.tracks().into_iter().filter(|t| t.deck == deck).collect();
        let k = spans.iter().position(|t| t.from <= from && to <= t.to)?;
        let span = &spans[k];
        let i = span.playing.iter().position(|&p| p == (from, to))?;
        let start = self.snapshot.clock;
        let jumps: Vec<u64> = self
            .events
            .iter()
            .filter(|(_, e)| deck_of(e) == Some(deck) && is_jump(e))
            .map(|(at, _)| at - start)
            .collect();
        let started = self.events.iter().any(|(t, e)| {
            *t == start + from
                && matches!(e, Logged::Command(Command::SetPlaying { deck: d, playing: true }) if *d == deck)
        });
        // Later: up to its end, where it had got to (jog touches, nudges
        // and jumps on the way give way), but not into a loop it set.
        let first_loop = self
            .events
            .iter()
            .filter(|(at, e)| {
                deck_of(e) == Some(deck)
                    && matches!(e, Logged::Command(Command::SetLoop { .. }))
                    && start + from < *at
                    && *at < start + to
            })
            .map(|(at, _)| at - start)
            .min();
        let latest_start = first_loop.unwrap_or(to).saturating_sub(1).max(from);
        let (earliest, latest) = if span.playing.len() == 1 {
            // Where it is heard to, and where the deck is in the track then.
            let heard_end = self
                .heard(span)
                .into_iter()
                .filter(|h| from <= h.0 && h.1 <= to)
                .map(|h| h.1)
                .max()
                .unwrap_or(to);
            let ((place, tempo), there) = self.play_place_and(deck, from, Some(heard_end)).ok()?;
            // Not on past the track's end (silence).
            let track_end = match (self.tracks.get(&span.path), there) {
                (Some(audio), Some((at, pace))) if pace > 0.0 => {
                    let left = (audio.duration_secs() - at).max(0.0);
                    heard_end + (left / pace * f64::from(self.rate)) as u64
                }
                _ => OPEN_END,
            };
            let track_start =
                from.saturating_sub((place / tempo * f64::from(self.rate)).floor() as u64);
            let heard_before = spans[..k]
                .iter()
                .flat_map(|t| self.heard(t))
                .map(|h| h.1)
                .filter(|&end| end <= from)
                .max()
                .unwrap_or(0);
            // On over what isn't heard after it (deleted cue hits, a track
            // loaded and cut): up to the next thing heard on the deck.
            let heard_after = spans[k + 1..]
                .iter()
                .flat_map(|t| self.heard(t))
                .map(|h| h.0)
                .min()
                .unwrap_or(OPEN_END);
            (track_start.max(heard_before).min(from), heard_after.min(track_end).max(to))
        } else {
            let before = i.checked_sub(1).map_or(span.from, |i| span.playing[i].1 + 1);
            let after = span.playing.get(i + 1).map_or(span.to, |p| p.0.saturating_sub(1));
            let earliest =
                jumps.iter().filter(|&&j| before <= j && j < from).max().map_or(before, |&j| j + 1);
            let latest = jumps
                .iter()
                .filter(|&&j| to < j && j <= after)
                .min()
                .map_or(after, |&j| j.saturating_sub(1))
                .max(to);
            (earliest.min(from), latest)
        };
        let start_range = if started { (earliest, latest_start) } else { (from, from) };
        Some(PlayEdges { start: start_range, end: (from + 1, latest) })
    }

    /// Resizes the play `from`–`to` on `deck` to `new_from`–`new_to`
    /// (within [`Session::play_edges`], on buffer starts), as a clip of
    /// the track: sooner, the track is loaded there (stems as they were)
    /// and put where it has to be for the audio already heard to stay put,
    /// and what the deck did in between gives way; later, it starts there
    /// at the place it had reached. Its end: the stop moves (or one is put
    /// in), and what was done after it up to there gives way. Returns where
    /// it is now.
    pub fn resize_play(
        &mut self,
        deck: usize,
        (from, to): (u64, u64),
        (new_from, new_to): (u64, u64),
    ) -> Result<(u64, u64)> {
        let edges = self
            .play_edges(deck, from, to)
            .ok_or_else(|| anyhow!("deck {} didn't play there", deck + 1))?;
        let new_from = self.snap_within(new_from, edges.start.0, edges.start.1).unwrap_or(from);
        let own = self
            .tracks()
            .into_iter()
            .find(|t| t.deck == deck && t.playing.contains(&(from, to)))
            .ok_or_else(|| anyhow!("deck {} didn't play there", deck + 1))?;
        // Where it is heard to (a cut may end it before it stops).
        let heard_end = self
            .heard(&own)
            .into_iter()
            .filter(|h| from <= h.0 && h.1 <= to)
            .map(|h| h.1)
            .max()
            .unwrap_or(to);
        let new_to = if new_to == heard_end {
            heard_end
        } else {
            self.snap_within(new_to, edges.end.0.max(new_from + 1), edges.end.1).unwrap_or(heard_end)
        };
        let start = self.snapshot.clock;
        let mine = |e: &Logged| deck_of(e) == Some(deck);
        let is_play = |e: &Logged, playing: bool| {
            matches!(e, Logged::Command(Command::SetPlaying { deck: d, playing: p }) if *d == deck && *p == playing)
        };
        let is_gate = |e: &Logged| matches!(e, Logged::Command(Command::Gate { deck: d, .. }) if *d == deck);
        let gives_way = |e: &Logged, loads: bool| gives_way(e, deck, loads);
        let (place, tempo) = self.play_place(deck, from)?;
        if new_to > heard_end {
            self.extend_play(deck, &own, heard_end, new_to)?;
        } else if new_to < heard_end {
            match self.events.iter().position(|(at, e)| *at == start + to && is_play(e, false)) {
                Some(i) => self.events[i].0 = start + new_to,
                None => self.events.push((
                    start + new_to,
                    Logged::Command(Command::SetPlaying { deck, playing: false }),
                )),
            }
            self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        }
        if new_from != from {
            let mut seconds = place - (from as f64 - new_from as f64) / f64::from(self.rate) * tempo;
            let mut held = false;
            let mut moved: Vec<Logged> = Vec::new();
            if new_from < from {
                // Loaded again there, as it was when it began.
                let audio = self
                    .events
                    .iter()
                    .rev()
                    .find_map(|(at, e)| match e {
                        Logged::Command(Command::Load { deck: d, audio })
                            if *d == deck && *at <= start + from =>
                        {
                            Some(audio.clone())
                        }
                        _ => None,
                    })
                    .or_else(|| self.snapshot.deck(deck).0.cloned())
                    .ok_or_else(|| anyhow!("no track on deck {}", deck + 1))?;
                let (stems, levels, _) = before_load(&self.events, deck, 0, start + from + 1);
                moved.push(Logged::Command(Command::Load { deck, audio }));
                for c in stems.into_iter().chain(levels) {
                    moved.push(Logged::Command(self.copy(c)?));
                }
                // What was there: gone.
                let starts_it = |at: u64, e: &Logged| {
                    at == start + from
                        && (is_play(e, true)
                            || matches!(
                                e,
                                Logged::Command(
                                    Command::Seek { .. } | Command::Shift { .. } | Command::MoveTrack { .. }
                                )
                            ) && mine(e))
                };
                self.events.retain(|(at, e)| {
                    !((start + new_from <= *at && *at < start + from && gives_way(e, true))
                        || starts_it(*at, e)
                        || *at == start + from && mine(e) && (is_gate(e) || matches!(e, Logged::Command(Command::Load { .. } | Command::SetStems { .. } | Command::SetStemLevels { .. }))))
                });
            } else {
                // Starts later, where the deck had really got to there
                // (whatever the jog and jumps did on the way); what was done
                // before it, now silent, goes.
                let (at, upto) = (start + from, start + new_from);
                let mut places = Places { deck, at: vec![upto], seconds: Vec::new(), tempo: Vec::new() };
                self.find_places(&mut places)?;
                seconds = places
                    .seconds
                    .first()
                    .copied()
                    .ok_or_else(|| anyhow!("past the end of the session"))?;
                // The jog still held there: held from the start.
                for (_, e) in self.events.iter().filter(|(t, e)| at <= *t && *t < upto && mine(e)) {
                    if let Logged::Command(Command::Hold { held: h, .. }) = e {
                        held = *h;
                    }
                }
                self.events.retain(|(t, e)| {
                    !(at <= *t
                        && *t < upto
                        && mine(e)
                        && (is_jump(e)
                            || is_play(e, true)
                            || matches!(e, Logged::Command(Command::MoveTrack { .. }))))
                });
            }
            moved.push(Logged::Command(Command::Seek { deck, seconds: seconds.max(0.0) }));
            moved.push(Logged::Command(Command::SetPlaying { deck, playing: true }));
            if held {
                moved.push(Logged::Command(Command::Hold { deck, held: true }));
            }
            let at = start + new_from;
            let i = self.events.partition_point(|(t, _)| *t < at);
            self.events.splice(i..i, moved.into_iter().map(|e| (at, e)));
        }
        self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        self.stretch_end();
        Ok((new_from, new_to))
    }

    /// Makes `deck`'s track in `own` play on, heard, from `heard_end` to
    /// `new_to` (buffer starts) as if nothing had been done there: what the
    /// deck did in between gives way (jumps, cuts, the stop, cue hits and
    /// tracks loaded after it), and from `new_to` on the deck is as it was.
    fn extend_play(&mut self, deck: usize, own: &TrackSpan, heard_end: u64, new_to: u64) -> Result<()> {
        let start = self.snapshot.clock;
        let spans: Vec<TrackSpan> = self.tracks().into_iter().filter(|t| t.deck == deck).collect();
        let there = spans.iter().find(|t| t.from <= new_to && new_to < t.to);
        // It fades out at `new_to` (as a cut does); a buffer on, when there
        // is room before anything else is heard, the deck goes on as it was.
        let heard_next = spans
            .iter()
            .flat_map(|t| self.heard(t))
            .map(|h| h.0)
            .filter(|&h| h >= new_to)
            .min()
            .unwrap_or(OPEN_END);
        let next_load = spans.iter().map(|t| t.from).filter(|&f| f > new_to).min().unwrap_or(u64::MAX);
        let limit = heard_next.min(next_load).min(there.map_or(u64::MAX, |t| t.to));
        // Another track is loaded (or none) where it ends: that cuts it
        // off at once, so it fades out first.
        let swaps = match there {
            Some(t) => t.from < new_to && t.from != own.from,
            // Unloaded after it (not just the end of the set).
            None => own.to <= new_to && own.to < self.length(),
        };
        let resume = match self.next_block(new_to) {
            r if r < limit && swaps => r,
            _ => new_to,
        };
        let clock = start + resume;
        let place = |s: &Self| -> Result<f64> {
            let mut places = Places { deck, at: vec![clock], seconds: Vec::new(), tempo: Vec::new() };
            s.find_places(&mut places)?;
            places.seconds.first().copied().ok_or_else(|| anyhow!("past the end of the session"))
        };
        let stop = || Logged::Command(Command::SetPlaying { deck, playing: false });
        let mut fade = Vec::new();
        if resume > new_to {
            fade.push(Logged::Command(Command::Gate { deck, open: false }));
        }
        // How the deck goes on from `resume`, as it was.
        let mut after = Vec::new();
        match there {
            Some(t) if t.from < new_to => {
                let playing = t.playing.iter().any(|p| p.0 < resume && resume < p.1);
                let (stems, levels, gate) = before_load(&self.events, deck, start + t.from, clock);
                if t.from == own.from {
                    // Its own track goes on unheard (nothing of its play is
                    // heard after this): no jump, which would click.
                    if !playing {
                        after.push(stop());
                    }
                    if !gate && fade.is_empty() {
                        after.push(Logged::Command(Command::Gate { deck, open: false }));
                    }
                } else {
                    // A track loaded in between: loaded again here.
                    let audio = self
                        .events
                        .iter()
                        .find_map(|(at, e)| match e {
                            Logged::Command(Command::Load { deck: d, audio })
                                if *d == deck && *at == start + t.from =>
                            {
                                Some(audio.clone())
                            }
                            _ => None,
                        })
                        .ok_or_else(|| anyhow!("no track on deck {}", deck + 1))?;
                    after.push(stop());
                    after.push(Logged::Command(Command::Load { deck, audio }));
                    for c in stems.into_iter().chain(levels) {
                        after.push(Logged::Command(self.copy(c)?));
                    }
                    if !gate {
                        after.push(Logged::Command(Command::Gate { deck, open: false }));
                    }
                    after.push(Logged::Command(Command::Seek { deck, seconds: place(self)? }));
                    if playing {
                        after.push(Logged::Command(Command::SetPlaying { deck, playing: true }));
                    }
                }
            }
            // A track loaded right there: it takes over.
            Some(_) => after.push(stop()),
            None => {
                after.push(stop());
                if swaps {
                    after.push(Logged::Command(Command::Unload { deck }));
                }
            }
        }
        self.events.retain(|(at, e)| {
            !(start + heard_end <= *at && *at < clock && gives_way(e, deck, *at >= start + own.to))
        });
        let i = self.events.partition_point(|(t, _)| *t < clock);
        self.events.splice(i..i, after.into_iter().map(|e| (clock, e)));
        let i = self.events.partition_point(|(t, _)| *t < start + new_to);
        self.events.splice(i..i, fade.into_iter().map(|e| (start + new_to, e)));
        Ok(())
    }

    /// How far the play `from`–`to` on `deck` (frames from the start; one
    /// of [`TrackSpan::playing`]) can move: not into the plays before and
    /// after it, nor out of its clip. `None` if it is not a play.
    pub fn play_limits(&self, deck: usize, from: u64, to: u64) -> Option<(i64, i64)> {
        let span =
            self.tracks().into_iter().find(|t| t.deck == deck && t.from <= from && to <= t.to)?;
        let i = span.playing.iter().position(|&p| p == (from, to))?;
        let before = i.checked_sub(1).map_or(span.from, |i| span.playing[i].1 + 1);
        // The deck's last clip: on past the end of the set.
        let last = if span.to >= self.length() { OPEN_END } else { span.to };
        let after = span.playing.get(i + 1).map_or(last, |p| p.0.saturating_sub(1));
        Some((before as i64 - from as i64, after as i64 - to as i64))
    }

    /// Moves the play `from`–`to` on `deck` (one of [`TrackSpan::playing`])
    /// by `by` frames, kept between the plays around it: what was done on
    /// the deck while it played (the play and stop, jumps, knobs, cuts)
    /// moves along, so it starts where it did in the track and sounds the
    /// same. Returns its new place.
    pub fn move_play(&mut self, deck: usize, from: u64, to: u64, by: i64) -> (u64, u64) {
        let Some((low, high)) = self.play_limits(deck, from, to) else { return (from, to) };
        let by = by.clamp(low, high.max(low));
        // On a buffer start, like everything the mixer does.
        let landing = self.snap((from as i64 + by).max(0) as u64);
        let by = landing as i64 - from as i64;
        if by == 0 || !(low..=high).contains(&by) {
            return (from, to);
        }
        let start = self.snapshot.clock;
        // What happens as it stops (the stop, CUE's jump back) goes along;
        // ended by a jump (a hot cue), that jump begins the next play.
        let stops = self.events.iter().any(|(at, e)| {
            *at == start + to
                && matches!(e, Logged::Command(Command::SetPlaying { deck: d, playing: false }) if *d == deck)
        });
        let end = if stops { to + 1 } else { to };
        let (moved, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.events).into_iter().partition(|(at, event)| {
                deck_of(event) == Some(deck)
                    && !matches!(
                        event,
                        Logged::Command(Command::Load { .. } | Command::Unload { .. })
                    )
                    && (start + from..start + end).contains(at)
            });
        self.events = rest;
        self.events.extend(moved.into_iter().map(|(at, event)| ((at as i64 + by) as u64, event)));
        self.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        // The deck's lanes as edited now follow the events.
        self.curves.retain(|setting, _| setting.deck() != Some(deck));
        self.stretch_end();
        (landing, (to as i64 + by) as u64)
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
            // The stems are open already (by the names the session has).
            folder: None,
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
        if run.positions_only() {
            mixer.positions_only();
        }
        let mut clock = start;
        let mut frames = 0usize;
        let mut buffer = Vec::new();
        let mut events = events.peekable();
        loop {
            while let Some((at, _)) = events.peek()
                && *at <= clock
            {
                // One between buffer starts (from an older version's edit)
                // takes effect at the next one.
                let Some((_, event)) = events.next() else { break };
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
        Ok(self.played_and_mix()?.0)
    }

    /// [`Session::played`], and the mix's level after every buffer: where
    /// it starts, its peak and its RMS (1.0 = full scale).
    pub fn played_and_mix(&self) -> Result<(Vec<Played>, Vec<(u64, f32, f32)>)> {
        let mut playing = Playing {
            grids: &self.grids,
            tracks: std::array::from_fn(|d| {
                self.snapshot.deck(d).0.and_then(|audio| source_of(&TRACKS, audio))
            }),
            start: self.snapshot.clock,
            rate: f64::from(self.rate),
            last: [None; crate::engine::DECKS],
            begins: [0.0; crate::engine::DECKS],
            decks: vec![Played::default(); crate::engine::DECKS],
            mix: Vec::new(),
        };
        self.run(self.events.iter(), &mut playing)?;
        Ok((playing.decks, playing.mix))
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
    /// For each buffer (as [`Played::levels`]): where in its track (seconds)
    /// the deck was as it began, after what was done then (a jump), and
    /// as it ended.
    pub places: Vec<(f64, f64)>,
}

impl Played {
    /// Where the deck played through `seconds` of its track, moving on (a
    /// cue point passed, or jumped to and played from): frames from the
    /// start.
    pub fn passes(&self, seconds: f64) -> Vec<u64> {
        let mut at = Vec::new();
        for (i, &(from, to)) in self.places.iter().enumerate() {
            if from <= seconds && seconds < to {
                let Some(&(start, _, _)) = self.levels.get(i) else { break };
                let end = self.levels.get(i + 1).map_or(start, |l| l.0);
                // Where in the buffer, if it is the next one on.
                let share = (seconds - from) / (to - from);
                at.push(start + (share * end.saturating_sub(start) as f64).round() as u64);
            }
        }
        at
    }
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
    /// Where each deck is as the buffer begins (after what was done then).
    begins: [f64; crate::engine::DECKS],
    decks: Vec<Played>,
    /// The mix's level after each buffer: start, peak, RMS.
    mix: Vec<(u64, f32, f32)>,
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

    fn before(&mut self, _clock: u64, mixer: &mut Offline) -> Result<()> {
        // Where they are after what was done now (a cue's jump).
        mixer.settle();
        for (deck, begins) in self.begins.iter_mut().enumerate() {
            *begins = mixer.deck(deck).seconds;
        }
        Ok(())
    }

    fn after(&mut self, clock: u64, buffer: &[f32], mixer: &mut Offline) -> Result<()> {
        let at = clock - self.start;
        let from = at - (buffer.len() / 2) as u64;
        let peak = buffer.iter().fold(0f32, |m, s| m.max(s.abs()));
        let power = buffer.iter().map(|s| s * s).sum::<f32>() / buffer.len().max(1) as f32;
        self.mix.push((from, peak, power.sqrt()));
        for deck in 0..self.decks.len() {
            let (played, heard, low) = mixer.take_peaks(deck);
            let view = mixer.deck(deck);
            if !view.loaded {
                self.last[deck] = None;
                continue;
            }
            self.decks[deck].levels.push((from, played, heard));
            self.decks[deck].lows.push(low);
            self.decks[deck].places.push((self.begins[deck], view.seconds));
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

/// What a set stretched by an edit keeps after the last thing in it
/// (seconds), to ring out.
pub const TAIL_SECS: f64 = 1.0;

/// How far past its end an edit can put a clip (frames): no limit to speak
/// of; the end moves out after it.
const OPEN_END: u64 = u64::MAX / 4;

/// A jump while a deck plays makes a play (a clip) of its own from there,
/// once the play before it is this long (seconds).
pub const JUMP_SPLIT_SECS: f64 = 0.2;

/// What runs along with a session through the mixer.
trait Run {
    /// Only where the decks are is wanted (no sound): the replay is many
    /// times quicker.
    fn positions_only(&self) -> bool {
        false
    }

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

/// What a new load of `deck`'s track forgets, as it was before `at`
/// (loads from `from` on start it afresh): its stems, their levels, and
/// whether it was heard (not cut).
fn before_load(
    events: &[(u64, Logged)],
    deck: usize,
    from: u64,
    at: u64,
) -> (Option<&Command>, Option<&Command>, bool) {
    let (mut stems, mut levels, mut gate) = (None, None, true);
    for (t, e) in events.iter().take_while(|(t, _)| *t < at) {
        let Logged::Command(c) = e else { continue };
        match c {
            Command::Load { deck: d, .. } if *d == deck && *t >= from => {
                (stems, levels, gate) = (None, None, true);
            }
            Command::SetStems { deck: d, .. } if *d == deck => stems = Some(c),
            Command::SetStemLevels { deck: d, .. } if *d == deck => levels = Some(c),
            Command::Gate { deck: d, open } if *d == deck => gate = *open,
            _ => {}
        }
    }
    (stems, levels, gate)
}

/// What `deck` does that gives way to its track laid over it: jumps, cuts,
/// plays and stops, loops, moves; with `loads`, loading and stems too.
fn gives_way(e: &Logged, deck: usize, loads: bool) -> bool {
    deck_of(e) == Some(deck)
        && (is_jump(e)
            || matches!(
                e,
                Logged::Command(
                    Command::Gate { .. }
                        | Command::SetPlaying { .. }
                        | Command::ClearLoop { .. }
                        | Command::MoveTrack { .. }
                )
            )
            || loads
                && matches!(
                    e,
                    Logged::Command(
                        Command::Load { .. }
                            | Command::Unload { .. }
                            | Command::SetStems { .. }
                            | Command::SetStemLevels { .. }
                    )
                ))
}

/// How [`Places`] stops the replay once it has them all.
const PLACES_FOUND: &str = "places found";

/// Where each deck is in its track at given moments, for
/// [`Session::split_plays`].
struct Places {
    deck: usize,
    /// The moments (clock), sorted; the place before what happens then.
    at: Vec<u64>,
    seconds: Vec<f64>,
    /// The deck's tempo there (track seconds a second).
    tempo: Vec<f64>,
}

impl Run for Places {
    fn positions_only(&self) -> bool {
        true
    }

    fn after(&mut self, clock: u64, _buffer: &[f32], mixer: &mut Offline) -> Result<()> {
        while self.seconds.len() < self.at.len() && self.at[self.seconds.len()] <= clock {
            let view = mixer.deck(self.deck);
            self.seconds.push(view.seconds);
            self.tempo.push(view.tempo);
        }
        // All found: no need to play on.
        if self.seconds.len() == self.at.len() {
            bail!(PLACES_FOUND);
        }
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

    /// Prints deck 1's clips, plays, edges and events in a real session
    /// (read only): `DECKHAND_SESSION=… cargo test -p deckhand-audio
    /// real_deck -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_deck() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let deck: usize = std::env::var("DECKHAND_DECK").ok().and_then(|d| d.parse().ok()).unwrap_or(0);
        let session = Session::read(&path, Files).unwrap();
        let start = session.snapshot.clock;
        println!("length {} warnings {:?}", session.length(), session.warnings());
        for n in session.nudges() {
            println!("nudge deck {} {:.2}s–{:.2}s: {:+.1} ms", n.deck + 1, n.from as f64 / f64::from(session.rate), n.to as f64 / f64::from(session.rate), n.shift * 1000.0);
        }
        for span in session.tracks().into_iter().filter(|t| t.deck == deck) {
            println!("clip {:?} {}..{} plays {:?} heard {:?}", span.path.file_name(), span.from, span.to, span.playing, session.heard(&span));
            for &(from, to) in &span.playing {
                println!("  play {from}..{to}: edges {:?}", session.play_edges(deck, from, to));
            }
        }
        for (at, e) in &session.events {
            if deck_of(e) == Some(deck) {
                let text = match e {
                    Logged::Command(c) => serde_json::to_string(c).unwrap_or_default().chars().take(90).collect(),
                    Logged::Jog(_, steps) => format!("jog {steps}"),
                    _ => String::new(),
                };
                println!("{:>9} {text}", at - start);
            }
        }
    }

    /// Moves the start of deck 1's first play 2 s later in a real session,
    /// in memory, and compares what is heard after it (read only).
    #[test]
    #[ignore]
    fn real_shrink() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let mut session = Session::read(&path, Files).unwrap();
        let span = session.tracks().into_iter().find(|t| t.deck == 0 && !t.playing.is_empty()).unwrap();
        let play = span.playing[0];
        let before = rendered(&session);
        let wanted = play.0 + 2 * u64::from(session.rate);
        let edges = session.play_edges(0, play.0, play.1).unwrap();
        println!("play {play:?} edges {edges:?}");
        let now = session.resize_play(0, play, (wanted, play.1)).unwrap();
        println!("now {now:?}");
        let after = rendered(&session);
        let from = now.0 as usize * 2 + 2048;
        let worst = (from..before.len().min(after.len())).map(|i| (before[i] - after[i]).abs()).fold(0f32, f32::max);
        let early = (0..now.0 as usize * 2).map(|i| (before[i] - after[i]).abs()).fold(0f32, f32::max);
        println!("after the new start: worst {worst}; before it (deck 1 gone there): {early}");
        assert!(worst < 1e-3);
    }

    /// Times what an edit costs in a real session (read only).
    #[test]
    #[ignore]
    fn real_timing() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let mut session = Session::read(&path, Files).unwrap();
        println!("{} events", session.events.len());
        let time = |what: &str, f: &mut dyn FnMut()| {
            let t = std::time::Instant::now();
            f();
            println!("{what:>28}: {:?}", t.elapsed());
        };
        time("tracks", &mut || { session.tracks(); });
        let spans = session.tracks();
        time("pieces (all clips)", &mut || { for t in &spans { session.pieces(t); } });
        let settings = Setting::all(crate::engine::DECKS);
        time(&format!("lanes ({})", settings.len()), &mut || { for s in &settings { session.lane(*s); } });
        time("backup (undo step)", &mut || { session.backup().unwrap(); });
        time("duplicate", &mut || { session.duplicate().unwrap(); });
        time("played_and_mix (replay)", &mut || { session.played_and_mix().unwrap(); });
        let span = spans.iter().find(|t| t.deck == 1).unwrap().clone();
        time("move_clip", &mut || { session.move_clip(1, span.from, 48_000).unwrap(); });
    }

    /// Compares where each deck is in its track by its tempo lane (as the
    /// editor draws clips) with where the replay has it (read only).
    #[test]
    #[ignore]
    fn real_drift() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let session = Session::read(&path, Files).unwrap();
        let rate = f64::from(session.rate);
        let (played, _) = session.played_and_mix().unwrap();
        for span in session.tracks() {
            let lane = session.lane(Setting::Tempo(span.deck));
            let grid = session.grid(&span.path).map(|g| g.bpm);
            println!("deck {} {:?} grid {grid:?} tempo lane {:?}", span.deck + 1, span.path.file_name(), lane.iter().take(4).map(|p| (p.at, p.value)).collect::<Vec<_>>());
            let d = &played[span.deck];
            for &(from, to) in &span.playing {
                let i = d.levels.partition_point(|l| l.0 < from);
                let Some(&(anchor, _)) = d.places.get(i) else { continue };
                let (mut t, mut model) = (from, anchor);
                let step = (0.05 * rate) as u64;
                let mut next_print = from;
                while t < to {
                    let j = d.levels.partition_point(|l| l.0 < t);
                    if t >= next_print && let Some(&(real, _)) = d.places.get(j) && d.levels.get(j).is_some_and(|l| l.0 == t) {
                        println!("  {:6.1}s model {:8.3} real {:8.3} drift {:+7.1} ms", t as f64 / rate, model, real, (model - real) * 1000.0);
                        next_print = t + 10 * rate as u64;
                    }
                    let n = (t + step).min(to);
                    model += (n - t) as f64 / rate * lane_value(&lane, t);
                    t = n;
                }
            }
        }
    }

    /// Times grabbing and dropping a clip's edge late in a real session
    /// (read only; in memory).
    #[test]
    #[ignore]
    fn real_resize_timing() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let mut session = Session::read(&path, Files).unwrap();
        let rate = f64::from(session.rate);
        let span = session.tracks().into_iter().filter(|t| !t.playing.is_empty()).max_by_key(|t| t.from).unwrap();
        println!("{} s set; clip at {:.0} s with {} plays", session.length() as f64 / rate, span.from as f64 / rate, span.playing.len());
        println!("{} events", session.events.len());
        struct Nothing;
        impl Run for Nothing {
            fn positions_only(&self) -> bool { true }
        }
        let t = std::time::Instant::now();
        session.run(session.events.iter(), &mut Nothing).unwrap();
        println!("whole positions-only replay: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        for (_, e) in &session.events {
            if let Logged::Command(c) = e { let _ = session.copy(c).unwrap(); }
        }
        println!("copying every command: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        session.split_plays(span.deck, span.from).unwrap();
        println!("split_plays (grab): {:?}", t.elapsed());
        let span = session.tracks().into_iter().filter(|t| t.deck == span.deck && !t.playing.is_empty()).max_by_key(|t| t.from).unwrap();
        let play = span.playing[0];
        let t = std::time::Instant::now();
        let edges = session.play_edges(span.deck, play.0, play.1).unwrap();
        println!("play_edges (grab): {:?}", t.elapsed());
        let t = std::time::Instant::now();
        session.resize_play(span.deck, play, (play.0, (play.1 + 48_000).min(edges.end.1))).unwrap();
        println!("resize_play (drop, longer): {:?}", t.elapsed());
        let t = std::time::Instant::now();
        session.duplicate().unwrap();
        println!("duplicate (redraw copy): {:?}", t.elapsed());
        let t = std::time::Instant::now();
        session.played_and_mix().unwrap();
        println!("played_and_mix (redraw, background): {:?}", t.elapsed());
    }

    /// The quick (positions only) replay puts every deck where the full one
    /// does, in a real session (read only).
    #[test]
    #[ignore]
    fn real_positions_match() {
        struct Every {
            quick: bool,
            seen: Vec<[f64; crate::engine::DECKS]>,
        }
        impl Run for Every {
            fn positions_only(&self) -> bool {
                self.quick
            }
            fn after(&mut self, _: u64, _: &[f32], mixer: &mut Offline) -> Result<()> {
                self.seen.push(std::array::from_fn(|d| mixer.deck(d).seconds));
                Ok(())
            }
        }
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let session = Session::read(&path, Files).unwrap();
        let mut full = Every { quick: false, seen: Vec::new() };
        let mut quick = Every { quick: true, seen: Vec::new() };
        session.run(session.events.iter(), &mut full).unwrap();
        session.run(session.events.iter(), &mut quick).unwrap();
        assert_eq!(full.seen.len(), quick.seen.len());
        let worst = full
            .seen
            .iter()
            .zip(&quick.seen)
            .flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (a - b).abs()))
            .fold(0.0, f64::max);
        println!("{} buffers, worst difference {:.9} s", full.seen.len(), worst);
        let mut shown = 0;
        for (i, (a, b)) in full.seen.iter().zip(&quick.seen).enumerate() {
            for d in 0..crate::engine::DECKS {
                if (a[d] - b[d]).abs() > 1e-6 && shown < 6 {
                    println!("  buffer {i} deck {}: full {:.6} quick {:.6} (before: {:.6} / {:.6})", d + 1, a[d], b[d], full.seen[i.saturating_sub(1)][d], quick.seen[i.saturating_sub(1)][d]);
                    shown += 1;
                }
            }
        }
        assert!(worst < 1e-6);
    }

    /// A grid of `bpm` from the start, `secs` long.
    /// Splits every clip with several plays in a real session and checks
    /// it sounds the same (but for cuts' fades as plays begin):
    /// `DECKHAND_SESSION=… cargo test -p deckhand-audio --release real_split -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_split() {
        let path = PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let mut session = Session::read(&path, Files).unwrap();
        let render = |s: &Session| {
            let mut out = Vec::new();
            s.render(|b| Ok(out.extend_from_slice(b))).unwrap();
            out
        };
        let before = render(&session);
        let mut starts = Vec::new();
        for span in session.tracks().into_iter().filter(|t| t.playing.len() > 1) {
            println!("deck {}: {} plays", span.deck + 1, span.playing.len());
            starts.extend(span.playing[1..].iter().map(|p| p.0));
            session.split_plays(span.deck, span.from).unwrap();
        }
        let after = render(&session);
        assert_eq!(before.len(), after.len());
        let fade = (0.005 * f64::from(session.rate)) as u64 + 1;
        let mut worst = 0f32;
        for i in 0..before.len() {
            let frame = (i / 2) as u64;
            if starts.iter().any(|&s| s <= frame && frame < s + fade) {
                continue;
            }
            worst = worst.max((before[i] - after[i]).abs());
        }
        println!("{} plays split off, worst difference {worst}", starts.len());
        assert!(worst < 1e-4, "{worst}");
    }

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
        let tracks = || Tracks([(path.clone(), a.clone())].into_iter().collect());
        let session = Session::read(&file, tracks()).unwrap();
        assert!(session.warnings().is_empty(), "{:?}", session.warnings());
        let wav = file.with_extension("wav");
        let rendered = session.render_to_wav(&file.with_file_name("again.wav"), Some(&wav), |_| {});
        let comparison = rendered.unwrap().comparison.unwrap();
        assert_eq!(comparison.differing, 0, "{comparison:?}");

        // The recording's folder renamed: its stems are found beside it.
        let folder = file.parent().unwrap().to_path_buf();
        let renamed = folder
            .with_file_name(format!("{} renamed", folder.file_name().unwrap().to_string_lossy()));
        let _ = std::fs::remove_dir_all(&renamed);
        std::fs::rename(&folder, &renamed).unwrap();
        let file = renamed.join(file.file_name().unwrap());
        let session = Session::read(&file, tracks()).unwrap();
        assert!(session.warnings().is_empty(), "{:?}", session.warnings());
        let wav = file.with_extension("wav");
        let rendered = session.render_to_wav(&file.with_file_name("again.wav"), Some(&wav), |_| {});
        let comparison = rendered.unwrap().comparison.unwrap();
        assert_eq!(comparison.differing, 0, "after renaming: {comparison:?}");
        let _ = std::fs::remove_dir_all(&renamed);
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

    /// A short play (a CUE held) moved later: it sounds the same at its new
    /// place, silence where it was, and the play after it untouched.
    #[test]
    fn a_moved_play_sounds_the_same() {
        let a = track(48_000, 6.0, 4);
        let path = PathBuf::from("/music/cued.wav");
        remember_track(&a, &path);
        let file = record("plays", vec![Command::Load { deck: 0, audio: a.clone() }], 5.0);
        let mut session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        // Two presses from the same spot (as CUE does: play, stop, back).
        let start = session.snapshot.clock;
        let block = 512u64;
        let (one, two) = ((20 * block, 60 * block), (300 * block, 340 * block));
        for (from, to) in [one, two] {
            for (at, command) in [
                (from, Command::SetPlaying { deck: 0, playing: true }),
                (to, Command::SetPlaying { deck: 0, playing: false }),
                (to, Command::Seek { deck: 0, seconds: 0.5 }),
            ] {
                session.events.push((start + at, Logged::Command(command)));
            }
        }
        session.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        let render = |s: &Session| {
            let mut out = Vec::new();
            s.render(|b| Ok(out.extend_from_slice(b))).unwrap();
            out
        };
        let before = render(&session);
        let spans = session.tracks();
        assert_eq!(spans[0].playing, vec![one, two]);
        // A second on, well before the next press.
        let (from, to) = session.move_play(0, one.0, one.1, 94 * 512);
        assert_eq!((from, to), (one.0 + 94 * 512, one.1 + 94 * 512));
        assert_eq!(session.tracks()[0].playing, vec![(from, to), two], "the boxes move");
        let after = render(&session);
        let worst = |a: &[f32], a0: u64, b: &[f32], b0: u64, len: u64| {
            (0..len as usize * 2)
                .map(|i| (a[a0 as usize * 2 + i] - b[b0 as usize * 2 + i]).abs())
                .fold(0.0f32, f32::max)
        };
        assert!(worst(&after, from, &before, one.0, one.1 - one.0) < 1e-4, "the same play");
        let old = (one.0 * 2) as usize..(one.1.min(from) * 2) as usize;
        assert!(after[old].iter().all(|s| s.abs() < 1e-4), "silent where it was");
        assert!(
            worst(&after, two.0, &before, two.0, two.1 - two.0) < 1e-4,
            "the next one as it was"
        );
        // Not into the next play.
        let (_, end) = session.move_play(0, from, to, 10 * 48_000);
        assert!(end < two.0, "{end}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// A play made longer at both ends, then shorter: what it played
    /// before stays where it was in the set, the new parts go on from it.
    #[test]
    fn a_resized_play_keeps_its_audio_in_place() {
        let a = track(48_000, 6.0, 4);
        let path = PathBuf::from("/music/resized.wav");
        remember_track(&a, &path);
        let file = record("resize", vec![Command::Load { deck: 0, audio: a.clone() }], 5.0);
        let mut session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let start = session.snapshot.clock;
        let block = 512u64;
        // Cued to 1 s, played from 2 s to 3 s, then a jump back.
        let (from, to) = (188 * block, 281 * block);
        for (at, command) in [
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
            (from, Command::SetPlaying { deck: 0, playing: true }),
            (to, Command::SetPlaying { deck: 0, playing: false }),
            (400 * block, Command::Seek { deck: 0, seconds: 0.0 }),
        ] {
            session.events.push((start + at, Logged::Command(command)));
        }
        session.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        let render = |s: &Session| {
            let mut out = Vec::new();
            s.render(|b| Ok(out.extend_from_slice(b))).unwrap();
            out
        };
        let same = |a: &[f32], b: &[f32], from: u64, to: u64| {
            (from as usize * 2..to as usize * 2).map(|i| (a[i] - b[i]).abs()).fold(0.0f32, f32::max)
        };
        let before = render(&session);
        let edges = session.play_edges(0, from, to).unwrap();
        // Back to the track's beginning (it began 1 s in), on to the clip's
        // end (the jump gives way).
        assert_eq!(edges.start.0, from - 48_000);
        // On to where the 6 s track runs out (it is 1 s + the play in).
        let runs_out = to + ((6.0 - 1.0 - (to - from) as f64 / 48_000.0) * 48_000.0) as u64;
        assert!(edges.end.1.abs_diff(runs_out) <= 1, "{} {runs_out}", edges.end.1);
        // Half a second sooner, half a second longer.
        let (wider_from, wider_to) =
            session.resize_play(0, (from, to), (from - 47 * block, to + 47 * block)).unwrap();
        assert_eq!((wider_from, wider_to), (from - 47 * block, to + 47 * block));
        let playing = |s: &Session| s.tracks().into_iter().flat_map(|t| t.playing).collect::<Vec<_>>();
        assert_eq!(playing(&session), vec![(wider_from, wider_to)]);
        let wider = render(&session);
        assert!(same(&wider, &before, from, to) < 1e-4, "the old part in place");
        // As if it had been played that way: cued half a second earlier,
        // PLAY and stop half a second further out.
        let a = track(48_000, 6.0, 4);
        let path = PathBuf::from("/music/resized-ref.wav");
        remember_track(&a, &path);
        let reference_file =
            record("resize-ref", vec![Command::Load { deck: 0, audio: a.clone() }], 5.0);
        let mut reference =
            Session::read(&reference_file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let start = reference.snapshot.clock;
        let earlier = (47 * block) as f64 / 48_000.0;
        for (at, command) in [
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 - earlier }),
            (wider_from, Command::SetPlaying { deck: 0, playing: true }),
            (wider_to, Command::SetPlaying { deck: 0, playing: false }),
            (400 * block, Command::Seek { deck: 0, seconds: 0.0 }),
        ] {
            reference.events.push((start + at, Logged::Command(command)));
        }
        reference.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        let expected = render(&reference);
        assert!(same(&wider, &expected, 0, wider.len().min(expected.len()) as u64 / 2) < 1e-3);
        let _ = std::fs::remove_dir_all(reference_file.parent().unwrap());
        // Back to its first size: as it was.
        session.resize_play(0, (wider_from, wider_to), (from, to)).unwrap();
        assert_eq!(playing(&session), vec![(from, to)]);
        assert!(same(&render(&session), &before, 0, before.len() as u64 / 2) < 1e-4);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Plays of one track split into clips of their own sound the same;
    /// with the cue hits deleted, the real play made to start sooner plays
    /// the track before it, over where the hits were.
    #[test]
    fn split_plays_sound_the_same_and_take_over_cue_hits() {
        let a = track(48_000, 8.0, 3);
        let path = PathBuf::from("/music/split.wav");
        remember_track(&a, &path);
        let file = record("split", vec![Command::Load { deck: 0, audio: a.clone() }], 6.0);
        let mut session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let start = session.snapshot.clock;
        let block = 512u64;
        // Two CUE hits from 1 s, then the real play from there; a cut in
        // the middle of the second hit, open again before the play.
        let hits = [(40 * block, 50 * block), (80 * block, 90 * block)];
        let play = (150 * block, 400 * block);
        let mut events = vec![(10 * block, Command::Seek { deck: 0, seconds: 1.0 })];
        for (from, to) in hits {
            events.push((from, Command::SetPlaying { deck: 0, playing: true }));
            events.push((to, Command::SetPlaying { deck: 0, playing: false }));
            events.push((to, Command::Seek { deck: 0, seconds: 1.0 }));
        }
        events.push((85 * block, Command::Gate { deck: 0, open: false }));
        events.push((120 * block, Command::Gate { deck: 0, open: true }));
        events.push((play.0, Command::SetPlaying { deck: 0, playing: true }));
        events.push((play.1, Command::SetPlaying { deck: 0, playing: false }));
        for (at, command) in events {
            session.events.push((start + at, Logged::Command(command)));
        }
        session.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        let render = |s: &Session| {
            let mut out = Vec::new();
            s.render(|b| Ok(out.extend_from_slice(b))).unwrap();
            out
        };
        let worst = |a: &[f32], b: &[f32], from: u64, to: u64| {
            (from as usize * 2..(to as usize * 2).min(a.len()).min(b.len()))
                .map(|i| (a[i] - b[i]).abs())
                .fold(0.0f32, f32::max)
        };
        let before = render(&session);
        assert!(session.split_plays(0, 0).unwrap());
        let spans = session.tracks();
        assert_eq!(spans.len(), 3, "one clip a play");
        assert_eq!(spans.iter().map(|s| s.playing.clone()).collect::<Vec<_>>(), [
            vec![hits[0]],
            vec![hits[1]],
            vec![play]
        ]);
        let split = render(&session);
        assert!(worst(&split, &before, 0, before.len() as u64 / 2) < 1e-5, "sounds the same");
        // The hits deleted (cut out), as in the editor.
        for hit in &spans[..2] {
            session.set_pieces(hit, &[]);
        }
        // The play made to start as soon as it can: the track from its
        // beginning (it began 1 s in), over where the hits were.
        let edges = session.play_edges(0, play.0, play.1).unwrap();
        assert_eq!(edges.start.0, play.0 - 48_000, "back to the track's beginning");
        let (from, _) = session.resize_play(0, play, (0, play.1)).unwrap();
        assert!(from >= play.0 - 48_000 && from < play.0 - 48_000 + block, "{from}");
        let heard: Vec<_> = session.tracks().iter().map(|t| session.heard(t)).collect();
        assert_eq!(heard.concat(), vec![(from, play.1)], "just the play, longer");
        let longer = render(&session);
        // As if it had been played from there: the track from its start.
        let a = track(48_000, 8.0, 3);
        let path = PathBuf::from("/music/split-ref.wav");
        remember_track(&a, &path);
        let reference_file = record("split-ref", vec![Command::Load { deck: 0, audio: a.clone() }], 6.0);
        let mut reference =
            Session::read(&reference_file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let start = reference.snapshot.clock;
        let seconds = 1.0 - (play.0 - from) as f64 / 48_000.0;
        for (at, command) in [
            (10 * block, Command::Seek { deck: 0, seconds }),
            (from, Command::SetPlaying { deck: 0, playing: true }),
            (play.1, Command::SetPlaying { deck: 0, playing: false }),
        ] {
            reference.events.push((start + at, Logged::Command(command)));
        }
        reference.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        let expected = render(&reference);
        let bad: Vec<usize> = (0..longer.len().min(expected.len())).filter(|&i| (longer[i] - expected[i]).abs() > 1e-4).map(|i| i / 2).collect();
        assert!(bad.is_empty(), "the track, from earlier: {} frames, {:?}..{:?}, from {from}", bad.len(), bad.first(), bad.last());
        assert!(worst(&longer, &before, play.0, play.1) < 1e-4, "the old part in place");
        let _ = std::fs::remove_dir_all(reference_file.parent().unwrap());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Every event on a buffer start, as the mixer takes them.
    fn on_grid(s: &Session) -> Vec<u64> {
        let mut block = (s.snapshot.clock, 0u64);
        let mut off = Vec::new();
        for (at, event) in &s.events {
            if let Logged::Block(frames) = event {
                block = (*at, u64::from(*frames));
            } else if block.1 > 0 && (at - block.0) % block.1 != 0 {
                off.push(at - s.snapshot.clock);
            }
        }
        off
    }

    /// A session with `events` (frames from the start) on deck 0, which
    /// has an 8 s track loaded, and one as played `reference`.
    fn deck_session(name: &str, events: Vec<(u64, Command)>) -> (Session, PathBuf) {
        let a = track(48_000, 8.0, 3);
        let path = PathBuf::from(format!("/music/{name}.wav"));
        remember_track(&a, &path);
        let file = record(name, vec![Command::Load { deck: 0, audio: a.clone() }], 6.0);
        let mut session = Session::read(&file, Tracks([(path, a)].into_iter().collect())).unwrap();
        let start = session.snapshot.clock;
        for (at, command) in events {
            session.events.push((start + at, Logged::Command(command)));
        }
        session.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        (session, file)
    }

    fn rendered(s: &Session) -> Vec<f32> {
        let mut out = Vec::new();
        s.render(|b| Ok(out.extend_from_slice(b))).unwrap();
        out
    }

    fn worst(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max)
    }

    /// A play made longer runs on over the cue hits deleted after it (each
    /// a clip of its own, hidden): the track on from where it was, not the
    /// hits.
    #[test]
    fn a_longer_play_takes_over_deleted_cue_hits_after_it() {
        let block = 512u64;
        let cue = |at| (at, Command::Seek { deck: 0, seconds: 1.0 });
        let play = (40 * block, 100 * block);
        let mut events = vec![cue(10 * block)];
        for (from, to) in [play, (150 * block, 160 * block), (200 * block, 210 * block)] {
            events.push((from, Command::SetPlaying { deck: 0, playing: true }));
            events.push((to, Command::SetPlaying { deck: 0, playing: false }));
            events.push(cue(to));
        }
        let (mut session, file) = deck_session("longer", events);
        assert!(session.split_plays(0, 0).unwrap());
        for hit in &session.tracks()[1..] {
            session.set_pieces(hit, &[]);
        }
        let edges = session.play_edges(0, play.0, play.1).unwrap();
        // On over the hidden hits, to where the 8 s track runs out.
        let runs_out =
            play.1 + ((8.0 - 1.0 - (play.1 - play.0) as f64 / 48_000.0) * 48_000.0) as u64;
        assert!(edges.end.1.abs_diff(runs_out) <= 1, "{} {runs_out}", edges.end.1);
        let end = 300 * block;
        assert_eq!(session.resize_play(0, play, (play.0, end)).unwrap(), (play.0, end));
        let heard: Vec<_> = session.tracks().iter().flat_map(|t| session.heard(t)).collect();
        assert_eq!(heard, vec![(play.0, end)]);
        assert!(on_grid(&session).is_empty());
        let (reference, reference_file) = deck_session("longer-ref", vec![
            cue(10 * block),
            (play.0, Command::SetPlaying { deck: 0, playing: true }),
            (end, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        assert!(worst(&rendered(&session), &rendered(&reference)) < 1e-4, "the track, played on");
        let _ = std::fs::remove_dir_all(reference_file.parent().unwrap());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// A play whose end was cut out, made longer: the cut part comes back
    /// as the track went on, jumps there giving way.
    #[test]
    fn a_longer_play_brings_back_its_cut_tail() {
        let block = 512u64;
        let play = (40 * block, 300 * block);
        let (mut session, file) = deck_session("tail", vec![
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
            (play.0, Command::SetPlaying { deck: 0, playing: true }),
            (100 * block, Command::Gate { deck: 0, open: false }),
            (150 * block, Command::Seek { deck: 0, seconds: 4.0 }),
            (play.1, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        // The jump made a clip of its own (as the editor shows it).
        assert!(session.split_plays(0, 0).unwrap());
        let first = (play.0, 150 * block);
        assert_eq!(session.tracks()[0].playing, vec![first]);
        let end = 200 * block;
        session.resize_play(0, first, (play.0, end)).unwrap();
        let heard: Vec<_> = session.tracks().iter().flat_map(|t| session.heard(t)).collect();
        assert_eq!(heard, vec![(play.0, end)]);
        let (reference, reference_file) = deck_session("tail-ref", vec![
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
            (play.0, Command::SetPlaying { deck: 0, playing: true }),
            (end, Command::Gate { deck: 0, open: false }),
            (play.1, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        let (got, want) = (rendered(&session), rendered(&reference));
        let bad: Vec<usize> =
            (0..got.len().min(want.len())).filter(|&i| (got[i] - want[i]).abs() > 1e-4).map(|i| i / 2).collect();
        assert!(bad.is_empty(), "{} frames differ, {:?}..{:?}; events {:?}", bad.len(), bad.first(), bad.last(),
            session.events.iter().filter(|(_, e)| deck_of(e) == Some(0)).map(|(at, e)| (at - session.snapshot.clock, match e { Logged::Command(c) => serde_json::to_string(c).unwrap_or_default().chars().take(60).collect::<String>(), _ => String::new() })).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(reference_file.parent().unwrap());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// A play made longer up into a track loaded after it, before that is
    /// heard: the later track is loaded where the play ends now, as it was.
    #[test]
    fn a_longer_play_leaves_the_next_track_as_it_was() {
        let block = 512u64;
        let play = (40 * block, 100 * block);
        let later = (250 * block, 350 * block);
        let events = |end: u64, load: u64| {
            vec![
                (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
                (play.0, Command::SetPlaying { deck: 0, playing: true }),
                (end, Command::SetPlaying { deck: 0, playing: false }),
                (load, Command::Seek { deck: 0, seconds: 2.0 }),
                (later.0, Command::SetPlaying { deck: 0, playing: true }),
                (later.1, Command::SetPlaying { deck: 0, playing: false }),
            ]
        };
        let reload = |s: &mut Session, at: u64| {
            let audio = s.events.iter().find_map(|(_, e)| match e {
                Logged::Command(Command::Load { audio, .. }) => Some(audio.clone()),
                _ => None,
            }).unwrap();
            let start = s.snapshot.clock;
            let i = s.events.partition_point(|(t, _)| *t < start + at);
            s.events.insert(i, (start + at, Logged::Command(Command::Load { deck: 0, audio })));
        };
        let (mut session, file) = deck_session("next", events(play.1, 150 * block));
        reload(&mut session, 150 * block);
        let edges = session.play_edges(0, play.0, play.1).unwrap();
        assert_eq!(edges.end.1, later.0, "up to where the next is heard");
        let end = 200 * block;
        session.resize_play(0, play, (play.0, end)).unwrap();
        assert!(on_grid(&session).is_empty());
        // As if played that way: cut there (faded out), the next track
        // loaded a buffer on.
        let mut played = events(end + block, end + block);
        played.push((end, Command::Gate { deck: 0, open: false }));
        let (mut reference, reference_file) = deck_session("next-ref", played);
        reload(&mut reference, end + block);
        let heard = |s: &Session| s.tracks().iter().flat_map(|t| s.heard(t)).collect::<Vec<_>>();
        assert_eq!(heard(&session), heard(&reference));
        assert!(worst(&rendered(&session), &rendered(&reference)) < 1e-4);
        let _ = std::fs::remove_dir_all(reference_file.parent().unwrap());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Cue hits deleted are gone from the session, not just silenced; a
    /// track the deleted one was loaded over stops where it did.
    #[test]
    fn deleted_clips_are_gone() {
        let block = 512u64;
        let cue = |at| (at, Command::Seek { deck: 0, seconds: 1.0 });
        let play = (40 * block, 100 * block);
        let mut events = vec![cue(10 * block)];
        for (from, to) in [play, (150 * block, 160 * block), (200 * block, 210 * block)] {
            events.push((from, Command::SetPlaying { deck: 0, playing: true }));
            events.push((to, Command::SetPlaying { deck: 0, playing: false }));
            events.push(cue(to));
        }
        let (mut session, file) = deck_session("gone", events);
        session.split_plays(0, 0).unwrap();
        let hits: Vec<TrackSpan> = session.tracks()[1..].to_vec();
        for hit in &hits {
            session.set_pieces(hit, &[]);
        }
        let plays = |s: &Session| {
            s.events.iter().filter(|(_, e)| matches!(e, Logged::Command(Command::SetPlaying { .. }))).count()
        };
        assert_eq!(session.tracks().len(), 1, "one clip left");
        assert_eq!(plays(&session), 2, "the play and its stop");
        let (reference, reference_file) = deck_session("gone-ref", vec![
            cue(10 * block),
            (play.0, Command::SetPlaying { deck: 0, playing: true }),
            (play.1, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        assert!(worst(&rendered(&session), &rendered(&reference)) < 1e-4);

        // Loaded over the play, then deleted: the play stops there.
        let (mut session, file2) = deck_session("over", vec![
            cue(10 * block),
            (play.0, Command::SetPlaying { deck: 0, playing: true }),
        ]);
        let audio = session.events.iter().find_map(|(_, e)| match e {
            Logged::Command(Command::Load { audio, .. }) => Some(audio.clone()),
            _ => None,
        }).unwrap();
        let start = session.snapshot.clock;
        for (at, command) in [
            (play.1, Command::Load { deck: 0, audio }),
            (play.1, Command::SetPlaying { deck: 0, playing: true }),
            (150 * block, Command::SetPlaying { deck: 0, playing: false }),
        ] {
            session.events.push((start + at, Logged::Command(command)));
        }
        session.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        let over = session.tracks()[1].clone();
        session.set_pieces(&over, &[]);
        assert_eq!(session.tracks().len(), 1);
        assert!(worst(&rendered(&session), &rendered(&reference)) < 1e-4, "stops where it was loaded over");
        for f in [file, file2, reference_file] {
            let _ = std::fs::remove_dir_all(f.parent().unwrap());
        }
    }

    /// A play made to start sooner can be made to start later again.
    #[test]
    fn a_sooner_play_shrinks_back() {
        let block = 512u64;
        let play = (40 * block, 300 * block);
        let (mut session, file) = deck_session("back", vec![
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
            (play.0, Command::SetPlaying { deck: 0, playing: true }),
            (play.1, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        let sooner = session.resize_play(0, play, (20 * block, play.1)).unwrap();
        assert_eq!(sooner, (20 * block, play.1));
        let edges = session.play_edges(0, sooner.0, sooner.1);
        eprintln!("{edges:?} tracks {:?}", session.tracks().iter().map(|t| (t.from, t.to, t.playing.clone())).collect::<Vec<_>>());
        let edges = edges.expect("still a play");
        assert!(edges.start.1 > sooner.0, "can start later: {edges:?}");
        let later = session.resize_play(0, sooner, (60 * block, play.1)).unwrap();
        assert_eq!(later, (60 * block, play.1));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// A session exported with its songs opens, and sounds the same, with
    /// the songs gone from where they were.
    #[test]
    fn an_exported_session_finds_its_songs() {
        // A song on disk: a recording of a track playing.
        let t = track(48_000, 3.0, 2);
        remember_track(&t, Path::new("/music/export-src.wav"));
        let made = record("export-song", vec![
            Command::Load { deck: 0, audio: t.clone() },
            Command::SetPlaying { deck: 0, playing: true },
        ], 2.5);
        let song_dir = std::env::temp_dir().join(format!("deckhand-export-music-{}", std::process::id()));
        std::fs::create_dir_all(&song_dir).unwrap();
        let song = song_dir.join("song.wav");
        std::fs::copy(made.with_extension("wav"), &song).unwrap();
        let audio = Arc::new(crate::decode::decode_file(&song).unwrap());
        remember_track(&audio, &song);
        let file = record("export-set", vec![
            Command::Load { deck: 0, audio },
            Command::SetPlaying { deck: 0, playing: true },
        ], 2.0);
        let session = Session::read(&file, Files).unwrap();
        let before = rendered(&session);

        let out = std::env::temp_dir().join(format!("deckhand-exported-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        session.write(&out.join("set.session")).unwrap();
        let tracks: Vec<PathBuf> = session.track_files().into_iter().map(|t| t.0).collect();
        let done = export_files(&out, &tracks, None).unwrap();
        assert_eq!((done.tracks, done.missing.len()), (1, 0));
        drop(session);
        std::fs::remove_dir_all(&song_dir).unwrap();

        let exported = Session::read(&out.join("set.session"), Files).unwrap();
        assert!(exported.warnings().is_empty(), "{:?}", exported.warnings());
        assert!(worst(&rendered(&exported), &before) < 1e-6);
        assert_eq!(exported.track_files()[0].0, song, "named as it was");
        for dir in [out, made.parent().unwrap().to_path_buf(), file.parent().unwrap().to_path_buf()] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A hot cue pressed while playing starts a clip of its own there;
    /// a jump just after the play began (dragging the needle) doesn't.
    /// Separated, it sounds the same.
    #[test]
    fn a_hot_cue_starts_a_clip() {
        let block = 512u64;
        let (mut session, file) = deck_session("hotcue", vec![
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
            (40 * block, Command::SetPlaying { deck: 0, playing: true }),
            (42 * block, Command::Seek { deck: 0, seconds: 1.5 }),
            (150 * block, Command::Seek { deck: 0, seconds: 4.0 }),
            (300 * block, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        assert_eq!(session.tracks()[0].playing, vec![(40 * block, 150 * block), (150 * block, 300 * block)]);
        let before = rendered(&session);
        assert!(session.split_plays(0, 0).unwrap());
        let spans = session.tracks();
        assert_eq!(
            spans.iter().map(|t| t.playing.clone()).collect::<Vec<_>>(),
            [vec![(40 * block, 150 * block)], vec![(150 * block, 300 * block)]]
        );
        assert!(worst(&rendered(&session), &before) < 1e-5, "sounds the same");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// A play made longer past the end of the set: the set grows to take
    /// it, the track playing on.
    #[test]
    fn a_play_runs_on_past_the_end() {
        let block = 512u64;
        let (mut session, file) = deck_session("past", vec![
            (10 * block, Command::Seek { deck: 0, seconds: 0.0 }),
            (40 * block, Command::SetPlaying { deck: 0, playing: true }),
            (100 * block, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        let length = session.length();
        let play = (40 * block, 100 * block);
        let end = session.snap(length + 48_000);
        assert_eq!(session.resize_play(0, play, (play.0, end)).unwrap(), (play.0, end));
        assert!(session.length() >= end + 48_000 - block, "{} for {end}", session.length());
        let heard: Vec<_> = session.tracks().iter().flat_map(|t| session.heard(t)).collect();
        assert_eq!(heard, vec![(play.0, end)]);
        let out = rendered(&session);
        assert_eq!(out.len() as u64, session.length() * 2);
        // The track (a click every so often) still sounding past the old end.
        let past = &out[(length as usize + 1000) * 2..(end as usize - 1000) * 2];
        assert!(past.iter().any(|s| s.abs() > 0.01));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// A cue point is found where the deck played through it, and where a
    /// jump landed on it and played on, not where it jumped away.
    #[test]
    fn cue_passes() {
        let played = Played {
            levels: (0..6).map(|i| (i * 100, 0.5, 0.5)).collect(),
            lows: vec![0.0; 6],
            beats: Vec::new(),
            // Plays 1.0 → 1.2, jumps back to 1.0 (a hot cue) and plays on,
            // then stops at 2.0.
            places: vec![(0.8, 0.9), (0.9, 1.1), (1.1, 1.2), (1.0, 1.1), (2.0, 2.0), (2.0, 2.0)],
        };
        assert_eq!(played.passes(1.0), vec![150, 300]);
        assert!(played.passes(2.0).is_empty(), "stopped there, not played through");
    }

    /// A clip split in two: two clips of their own, sounding the same.
    #[test]
    fn a_split_clip_is_two_clips() {
        let block = 512u64;
        let (mut session, file) = deck_session("split2", vec![
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
            (40 * block, Command::SetPlaying { deck: 0, playing: true }),
            (300 * block, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        let before = rendered(&session);
        assert_eq!(session.split_clip(0, 150 * block + 100).unwrap(), 150 * block);
        let spans = session.tracks();
        assert_eq!(
            spans.iter().map(|t| t.playing.clone()).collect::<Vec<_>>(),
            [vec![(40 * block, 150 * block)], vec![(150 * block, 300 * block)]]
        );
        assert!(worst(&rendered(&session), &before) < 1e-5, "sounds the same");
        // The second moves on its own, the first stays.
        session.move_clip(0, 150 * block, 20 * block as i64).unwrap();
        assert_eq!(session.tracks()[0].playing, vec![(40 * block, 150 * block)]);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// A nudge is found with what it moved the track; taken out keeping
    /// step, all after it sounds as it did.
    #[test]
    fn nudges_come_out_in_step() {
        let block = 512u64;
        let (mut session, file) = deck_session("nudge", vec![
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
            (40 * block, Command::SetPlaying { deck: 0, playing: true }),
            (100 * block, Command::SetNudge { deck: 0, amount: 0.1 }),
            (110 * block, Command::SetNudge { deck: 0, amount: 0.05 }),
            (120 * block, Command::SetNudge { deck: 0, amount: 0.0 }),
            (300 * block, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        let nudges = session.nudges();
        assert_eq!(nudges.len(), 1);
        let n = nudges[0];
        assert_eq!((n.from, n.to), (100 * block, 120 * block));
        let expected = (0.1 + 0.05) * (10 * block) as f64 / 48_000.0;
        assert!((n.shift - expected).abs() < 1e-9, "{} {expected}", n.shift);
        let before = rendered(&session);
        assert_eq!(session.remove_nudges(0, 0, session.length(), true), 1);
        assert!(session.nudges().is_empty());
        let after = rendered(&session);
        let from = (120 * block) as usize * 2;
        let worst = (from..before.len().min(after.len())).map(|i| (before[i] - after[i]).abs()).fold(0f32, f32::max);
        assert!(worst < 1e-3, "in step after it: {worst}");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Deleting the middle of a clip leaves two clips of their own, the
    /// rest sounding as it did.
    #[test]
    fn deleting_the_middle_leaves_two_clips() {
        let block = 512u64;
        let events = vec![
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
            (40 * block, Command::SetPlaying { deck: 0, playing: true }),
            (300 * block, Command::SetPlaying { deck: 0, playing: false }),
        ];
        let (mut session, file) = deck_session("middle", events);
        let before = rendered(&session);
        session.delete_range(0, 100 * block, 200 * block).unwrap();
        let spans = session.tracks();
        let heard: Vec<_> = spans.iter().flat_map(|t| session.heard(t)).collect();
        assert_eq!(heard, vec![(40 * block, 100 * block), (200 * block, 300 * block)]);
        assert_eq!(spans.iter().filter(|t| !t.playing.is_empty()).count(), 2, "two clips");
        let after = rendered(&session);
        let same = |a: u64, b: u64| (a as usize * 2..b as usize * 2).map(|i| (before[i] - after[i]).abs()).fold(0f32, f32::max);
        assert!(same(0, 100 * block) < 1e-5, "before it as it was");
        assert!(same(200 * block, 300 * block) < 1e-5, "after it as it was");
        assert!(after[(110 * block) as usize * 2..(190 * block) as usize * 2].iter().all(|s| s.abs() < 1e-6), "silent between");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// A delete cutting where the deck stood still leaves no silent clips
    /// behind: only what is heard stays in the session.
    #[test]
    fn deleting_leaves_no_silent_clips() {
        let block = 512u64;
        let (mut session, file) = deck_session("silent", vec![
            (10 * block, Command::Seek { deck: 0, seconds: 1.0 }),
            (40 * block, Command::SetPlaying { deck: 0, playing: true }),
            (100 * block, Command::SetPlaying { deck: 0, playing: false }),
            (200 * block, Command::SetPlaying { deck: 0, playing: true }),
            (300 * block, Command::SetPlaying { deck: 0, playing: false }),
        ]);
        // From before the first play to the gap after it (both still).
        session.delete_range(0, 20 * block, 150 * block).unwrap();
        let spans = session.tracks();
        assert!(spans.iter().all(|t| !session.heard(t).is_empty()), "{:?}", spans.iter().map(|t| (t.from, t.to, t.playing.clone())).collect::<Vec<_>>());
        let heard: Vec<_> = spans.iter().flat_map(|t| session.heard(t)).collect();
        assert_eq!(heard, vec![(200 * block, 300 * block)]);
        let loads = session.events.iter().filter(|(_, e)| matches!(e, Logged::Command(Command::Load { .. }))).count();
        assert_eq!(loads, 1, "one load, for the clip left");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Edits keep every event on a buffer start, and a session that has one
    /// off them (from an older version) still plays.
    #[test]
    fn edits_stay_on_buffer_starts() {
        let block = 512u64;
        let (mut session, file) = deck_session("grid", vec![
            (0, Command::SetPlaying { deck: 0, playing: true }),
        ]);
        // A second clip: the track loaded again.
        let audio = session.snapshot.deck(0).0.cloned().or_else(|| session.events.iter().find_map(|(_, e)| match e {
            Logged::Command(Command::Load { audio, .. }) => Some(audio.clone()),
            _ => None,
        })).unwrap();
        let start = session.snapshot.clock;
        session.events.push((start + 200 * block, Logged::Command(Command::Load { deck: 0, audio })));
        session.events.push((start + 200 * block, Logged::Command(Command::SetPlaying { deck: 0, playing: true })));
        session.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        // Up against the clip before it.
        let landed = session.move_clip(0, 200 * block, -(250 * block as i64)).unwrap();
        assert_eq!(landed, block);
        assert!(on_grid(&session).is_empty(), "{:?}", on_grid(&session));
        // A piece ending just before its clip does.
        let span = session.tracks()[0].clone();
        session.set_pieces(&span, &[(0, span.to - 10)]);
        assert!(on_grid(&session).is_empty(), "{:?}", on_grid(&session));
        // One off: played at the next buffer start.
        let start = session.snapshot.clock;
        session.events.push((start + 300 * block + 7, Logged::Command(Command::Gate { deck: 0, open: false })));
        session.events.sort_by_key(|(at, event)| (*at, matches!(event, Logged::End)));
        rendered(&session);
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
