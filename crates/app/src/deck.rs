//! One deck: the loaded track, its cue points, loops, tempo and transport.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use deckhand_audio::{
    Command, CrossfaderSide, DRUMS, Engine, PARTS, StemLayers, TrackAudio, VOCALS, band_waveform,
    decode_file, peak_waveform,
};
use deckhand_control::PadMode;
use deckhand_formats::anlz::{
    AnlzFile, CueKind, CueListKind, CueStatus, Section, Waveform, WaveformKind,
};
use deckhand_formats::pdb::LibraryTrack;
use eframe::egui::{self, Color32};

use crate::library::UsbLibrary;
use crate::waveform::WaveformData;

pub const HOT_CUES: usize = 8;
const HOT_CUE_DEFAULT: Color32 = Color32::from_rgb(40, 200, 80);

/// Audio moved by one turn of the jog, paused or scratching: like a
/// 33⅓ rpm record, 1.8 s.
const JOG_SECONDS_PER_TURN: f64 = 1.8;
/// Speed change per jog step while playing, and how long it lasts.
/// Pitch bend when spinning one turn per second, at sensitivity 1.
const BEND_PER_TURN_PER_SECOND: f64 = 0.25;
/// The bend stops this long after the last jog movement.
const NUDGE_HOLD: Duration = Duration::from_millis(35);
/// Jog speed is measured over this much recent movement.
const JOG_WINDOW: Duration = Duration::from_millis(50);
/// Jog search movement per step.
const SEARCH_SECONDS_PER_STEP: f64 = 0.1;
/// Positions this close to the cue point count as "at the cue".
pub const AT_CUE: f64 = 0.02;
/// Beat-jump pads: back/forward pairs.
pub const BEAT_JUMPS: [f64; 8] = [-1.0, 1.0, -2.0, 2.0, -4.0, 4.0, -8.0, 8.0];
/// Beat-loop pad lengths in beats.
pub const BEAT_LOOPS: [f64; 8] = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

/// A tempo fader range: ±`percent` %, where 100 is WIDE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TempoRange(pub u8);

impl TempoRange {
    pub const SIX: TempoRange = TempoRange(6);
    pub const TEN: TempoRange = TempoRange(10);
    pub const SIXTEEN: TempoRange = TempoRange(16);
    pub const WIDE: TempoRange = TempoRange(100);
    /// The ranges TEMPO RANGE steps through unless Preferences say otherwise.
    pub const DEFAULTS: [TempoRange; 4] = [Self::SIX, Self::TEN, Self::SIXTEEN, Self::WIDE];

    /// A range of ±`percent` % (1–100).
    pub fn percent(percent: u8) -> Self {
        Self(percent.clamp(1, 100))
    }

    pub fn fraction(self) -> f64 {
        f64::from(self.0.clamp(1, 100)) / 100.0
    }

    pub fn label(self) -> String {
        if self.0 >= 100 { "WIDE".to_owned() } else { format!("±{}%", self.0) }
    }

    /// The next of `ranges` (in order, back to the first); a range not in
    /// the list goes to the first one above it.
    pub fn next(self, ranges: &[TempoRange]) -> Self {
        match ranges.iter().position(|&r| r == self) {
            Some(i) => ranges[(i + 1) % ranges.len()],
            None => ranges
                .iter()
                .copied()
                .find(|&r| r > self)
                .or(ranges.first().copied())
                .unwrap_or(self),
        }
    }

    /// The one of `ranges` closest to this one.
    pub fn nearest(self, ranges: &[TempoRange]) -> Self {
        ranges
            .iter()
            .copied()
            .min_by_key(|r| (i16::from(r.0) - i16::from(self.0)).abs())
            .unwrap_or(self)
    }
}

/// Saved as the percentage; older settings wrote the names Six, Ten,
/// Sixteen and Wide.
impl serde::Serialize for TempoRange {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for TempoRange {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Saved {
            Percent(u8),
            Name(String),
        }
        Ok(match Saved::deserialize(deserializer)? {
            Saved::Percent(p) => Self::percent(p),
            Saved::Name(name) => match name.as_str() {
                "Six" => Self::SIX,
                "Sixteen" => Self::SIXTEEN,
                "Wide" => Self::WIDE,
                _ => Self::TEN,
            },
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HotCue {
    pub seconds: f64,
    pub color: Color32,
}

/// Everything known about the track on a deck.
pub struct LoadedTrack {
    pub id: u32,
    /// The audio file.
    pub path: PathBuf,
    /// From deckhand's collection rather than a USB export.
    pub from_collection: bool,
    /// The export it came from (USB tracks).
    pub usb_root: Option<PathBuf>,
    /// Auto Cue: where the first sound starts, when it was looked for.
    pub first_sound: Option<f64>,
    /// A memory loop rekordbox marked active: it loops when playback gets there.
    pub active_loop: Option<(f64, f64)>,
    pub title: String,
    pub artist: String,
    pub key: Option<String>,
    pub bpm: f64,
    pub duration: f64,
    pub waveform: WaveformData,
    /// Beat times with their bar position.
    pub beats: Vec<(f64, u16)>,
    pub hot_cues: [Option<HotCue>; HOT_CUES],
    pub memory_cues: Vec<f64>,
    /// Stored loops (start, end) from the memory cue list.
    pub memory_loops: Vec<(f64, f64)>,
    /// Track gain in dB (Auto Gain), from the collection or the loudness.
    pub gain_db: f32,
}

impl LoadedTrack {
    fn beat_length(&self) -> f64 {
        if self.bpm > 0.0 { 60.0 / self.bpm } else { 0.5 }
    }

    /// Position in beats (fractional) at `seconds`, from the beat grid, or
    /// from the BPM if there is none.
    pub fn beat_at(&self, seconds: f64) -> f64 {
        let beats = &self.beats;
        if beats.len() < 2 {
            return seconds / self.beat_length();
        }
        let i = beats.partition_point(|&(t, _)| t <= seconds);
        let (a, b) = match i {
            0 => (0, 1),
            i if i >= beats.len() => (beats.len() - 2, beats.len() - 1),
            i => (i - 1, i),
        };
        let (ta, tb) = (beats[a].0, beats[b].0);
        a as f64 + (seconds - ta) / (tb - ta)
    }

    /// Inverse of [`LoadedTrack::beat_at`].
    pub fn time_of_beat(&self, beat: f64) -> f64 {
        let beats = &self.beats;
        if beats.len() < 2 {
            return beat * self.beat_length();
        }
        let a = (beat.floor().max(0.0) as usize).min(beats.len() - 2);
        let (ta, tb) = (beats[a].0, beats[a + 1].0);
        ta + (beat - a as f64) * (tb - ta)
    }

    pub fn nearest_beat_time(&self, seconds: f64) -> f64 {
        self.time_of_beat(self.beat_at(seconds).round())
    }

    /// The bar (counted from 1 at the first downbeat) and the beat in it
    /// (1–4) at `seconds`, like rekordbox's bar counter.
    pub fn bar_and_beat(&self, seconds: f64) -> (i64, u16) {
        let index = self.beat_at(seconds).floor() as i64;
        let beats = &self.beats;
        if beats.len() < 2 {
            return (index.div_euclid(4) + 1, (index.rem_euclid(4) + 1) as u16);
        }
        // Beats before the first downbeat belong to bar 0 (a pickup).
        let first_downbeat = beats.iter().position(|b| b.1 == 1).unwrap_or(0) as i64;
        let in_grid = index.clamp(0, beats.len() as i64 - 1) as usize;
        let beat = if index < 0 { 1 } else { beats[in_grid].1.clamp(1, 4) };
        let bar = (index - first_downbeat).div_euclid(4) + 1;
        (bar, beat)
    }

    /// Whole beats from `seconds` to the next memory cue or loop ahead.
    pub fn beats_to_next_memory(&self, seconds: f64) -> Option<i64> {
        let now = self.beat_at(seconds);
        self.memory_cues
            .iter()
            .copied()
            .chain(self.memory_loops.iter().map(|l| l.0))
            .filter(|&t| t > seconds + 0.01)
            .map(|t| (self.beat_at(t) - now).ceil() as i64)
            .min()
    }

    /// Whether the grid keeps one tempo (every beat within 5 ms of a
    /// straight line), which the grid edits need. No grid counts as one.
    pub fn grid_is_straight(&self) -> bool {
        let n = self.beats.len();
        if n < 3 {
            return true;
        }
        // Least-squares line through (index, time).
        let mean_i = (n - 1) as f64 / 2.0;
        let mean_t = self.beats.iter().map(|b| b.0).sum::<f64>() / n as f64;
        let (mut num, mut den) = (0.0, 0.0);
        for (i, b) in self.beats.iter().enumerate() {
            num += (i as f64 - mean_i) * (b.0 - mean_t);
            den += (i as f64 - mean_i).powi(2);
        }
        let slope = num / den;
        self.beats
            .iter()
            .enumerate()
            .all(|(i, b)| (mean_t + (i as f64 - mean_i) * slope - b.0).abs() < 0.005)
    }

    /// The grid's tempo: the track's BPM, or when it has none (0), the
    /// average beat length of the grid.
    pub fn grid_bpm(&self) -> f64 {
        match (self.bpm > 0.0, self.beats.first(), self.beats.last()) {
            (false, Some(a), Some(b)) if self.beats.len() >= 2 && b.0 > a.0 => {
                60.0 * (self.beats.len() - 1) as f64 / (b.0 - a.0)
            }
            _ => self.bpm,
        }
    }

    /// The first beat and its number in the bar; without a grid, the start.
    fn grid_anchor(&self) -> (f64, u16) {
        self.beats.first().map_or((0.0, 1), |&(t, n)| (t, n.clamp(1, 4)))
    }

    /// Replaces the grid with a straight one at `bpm` that has beat
    /// `number` (1–4) at `anchor`, from the start of the track to its end.
    fn set_straight_grid(&mut self, anchor: f64, number: u16, bpm: f64) {
        let beat = 60.0 / bpm;
        // Beats before the anchor, back to the start.
        let before = (anchor / beat + 1e-9).floor() as i64;
        let start = anchor - before as f64 * beat;
        let first = i64::from(number.clamp(1, 4)) - 1 - before;
        self.beats = (0..)
            .map(|i: i64| (start + i as f64 * beat, ((first + i).rem_euclid(4) + 1) as u16))
            .take_while(|b| b.0 < self.duration)
            .collect();
        self.bpm = bpm;
    }
}

/// Beats as rekordbox counts them down: `bars.beats`, e.g. 9 beats = "2.1".
pub fn bars_label(beats: i64) -> String {
    format!("{}.{}", beats / 4, beats % 4)
}

type LoadResult = Result<(LoadedTrack, Arc<TrackAudio>)>;

pub struct Deck {
    pub index: usize,
    pub track: Option<LoadedTrack>,
    /// The decoded audio, kept to reload it when the audio output restarts.
    audio: Option<Arc<TrackAudio>>,
    loading: Option<(String, Receiver<LoadResult>)>,
    /// Beat grid and key being detected for the loaded track.
    analysing: Option<Receiver<Detected>>,
    /// Detection filled in the grid or key; the collection should keep it.
    pub analysed: bool,
    pub error: Option<String>,
    pub cue_point: f64,
    previewing: bool,
    /// −1.0 (slowest) … 1.0 (fastest).
    pub tempo_fader: f32,
    pub tempo_range: TempoRange,
    /// Tempo set by sync, overriding the fader.
    pub sync_ratio: Option<f64>,
    /// This deck is the master the synced decks follow (kept up to date by
    /// the app): its own tempo fader stays in charge, SYNC or not.
    pub is_master: bool,
    pub sync: bool,
    pub quantize: bool,
    pub vinyl: bool,
    /// Master Tempo: the key stays the same when the tempo changes.
    pub key_lock: bool,
    pub loop_in: Option<f64>,
    pub loop_region: Option<(f64, f64)>,
    pub loop_active: bool,
    jog_touched: bool,
    nudge_until: Option<Instant>,
    /// Jog steps since the last frame, and the smoothed spin speed
    /// (steps per second) the pitch bend follows.
    nudge_steps: std::collections::VecDeque<(Instant, f64)>,
    nudge_velocity: f64,
    nudge_sent: f64,
    last_poll: Instant,
    /// With SYNC: how far (in beats) this deck sits from the master's beat
    /// on purpose, after the DJ nudged it; drift correction keeps it.
    pub phase_offset: f64,
    /// A nudge or scratch just ended: take the current phase as the offset.
    pub rephase: bool,
    /// The waveform is held by the mouse, stopping the deck.
    grabbed: bool,
    pub volume: f32,
    pub trim: f32,
    /// Knob positions, 0.5 = flat.
    pub eq: [f32; 3],
    /// −1.0 … 1.0, 0 = off.
    pub filter: f32,
    pub side: CrossfaderSide,
    pub headphone_cue: bool,
    /// Hot cues play only while held, then return and pause.
    pub hot_cue_gate: bool,
    /// Beats moved by the ◀ / ▶ beat jump.
    pub jump_size: f64,
    /// Multiplier for the beat-jump pads (1 = 1, 2, 4 and 8 beats).
    pub jump_range: f64,
    /// The hot cue currently held in gate mode.
    gate_held: Option<usize>,
    pub slip: bool,
    /// Hot cue pad held while slip mode is on: playback returns on release.
    slip_pad: Option<usize>,
    /// Key shift in semitones.
    pub key_shift: i32,
    /// The key shift was set by key sync.
    pub key_synced: bool,
    pub pad_mode: PadMode,
    /// Live stems of the loaded track.
    pub stems: Stems,
    /// Which stem parts play: vocals, instruments, bass, drums.
    pub stem_on: [bool; PARTS],
    /// Part Isolator: the EQ knobs set the vocal, instrument (with bass) and
    /// drum levels; 0.5 is the full level, as for the EQ.
    pub part_isolator: bool,
    pub isolator: [f32; 3],
    /// The loaded track's cues or beat grid changed and should be saved.
    pub edited: bool,
    /// The beat grid itself changed (saved as one tempo marker).
    pub grid_edited: bool,
    /// The beat-grid edit controls are open.
    pub grid_edit: bool,
    pub prefs: DeckPrefs,
    /// Jog steps the controller sends per turn of a wheel.
    pub jog_steps_per_turn: f64,
    /// Instant Doubles: once loaded, pick up where this other deck plays.
    pub double_of: Option<usize>,
    /// The on-screen SHIFT button is latched.
    pub screen_shift: bool,
    /// Pad FX pads show effects A–H (0) or I–P (1).
    pub pad_fx_page: u8,
    /// The Pad FX pads' settings are open in place of the pads (the cog).
    pub pad_fx_settings: bool,
    /// Beat-loop pad lengths are `BEAT_LOOPS` times this.
    pub loop_scale: f64,
    /// Keyboard mode: the hot cue the pads play, once picked.
    pub keyboard_cue: Option<usize>,
    /// Keyboard pads: semitones of pad 1 (pages of eight).
    pub keyboard_base: i32,
    /// Key Shift pads: semitones of pad 1 (pages of eight).
    pub key_shift_base: i32,
    taps: Vec<Instant>,
}

impl Deck {
    pub fn new(index: usize) -> Self {
        Self {
            index,
            track: None,
            audio: None,
            loading: None,
            analysing: None,
            analysed: false,
            error: None,
            cue_point: 0.0,
            previewing: false,
            tempo_fader: 0.0,
            tempo_range: TempoRange::TEN,
            sync_ratio: None,
            is_master: false,
            sync: false,
            quantize: true,
            vinyl: true,
            key_lock: false,
            loop_in: None,
            loop_region: None,
            loop_active: false,
            jog_touched: false,
            nudge_until: None,
            nudge_steps: Default::default(),
            nudge_velocity: 0.0,
            nudge_sent: 0.0,
            last_poll: Instant::now(),
            phase_offset: 0.0,
            rephase: false,
            grabbed: false,
            volume: 1.0,
            trim: 0.5,
            eq: [0.5; 3],
            filter: 0.0,
            side: if index % 2 == 0 { CrossfaderSide::A } else { CrossfaderSide::B },
            headphone_cue: false,
            hot_cue_gate: true,
            jump_size: 4.0,
            jump_range: 1.0,
            gate_held: None,
            slip: false,
            slip_pad: None,
            key_shift: 0,
            key_synced: false,
            pad_mode: PadMode::HotCue,
            stems: Stems::None,
            stem_on: [true; PARTS],
            part_isolator: false,
            isolator: [0.5; 3],
            edited: false,
            grid_edited: false,
            grid_edit: false,
            prefs: DeckPrefs::default(),
            jog_steps_per_turn: 720.0,
            double_of: None,
            screen_shift: false,
            pad_fx_page: 0,
            pad_fx_settings: false,
            loop_scale: 1.0,
            keyboard_cue: None,
            keyboard_base: 0,
            key_shift_base: 0,
            taps: Vec::new(),
        }
    }

    pub fn loading_title(&self) -> Option<&str> {
        self.loading.as_ref().map(|(title, _)| title.as_str())
    }

    pub fn tempo_ratio(&self) -> f64 {
        self.sync_ratio.unwrap_or(1.0 + f64::from(self.tempo_fader) * self.tempo_range.fraction())
    }

    pub fn current_bpm(&self) -> Option<f64> {
        self.track.as_ref().map(|t| t.bpm * self.tempo_ratio())
    }

    /// EQ knobs as display gains for the waveform: 1.0 flat, 0.0 killed.
    pub fn eq_gains(&self) -> [f32; 3] {
        self.eq.map(|knob| {
            let db = eq_db(knob, self.prefs.isolator);
            if db <= deckhand_audio::EQ_MIN_DB { 0.0 } else { 10f32.powf(db / 20.0).min(1.6) }
        })
    }

    pub fn is_scratching(&self) -> bool {
        self.jog_touched && self.vinyl
    }

    /// Beat grid and key detection is running for the loaded track.
    pub fn is_analysing(&self) -> bool {
        self.analysing.is_some()
    }

    /// Takes the track off the deck. Settings like tempo range or SYNC stay.
    pub fn eject(&mut self, engine: &mut Engine) {
        engine.send(Command::Unload { deck: self.index });
        self.track = None;
        self.audio = None;
        self.stems = Stems::None;
        self.loading = None;
        (self.analysing, self.analysed) = (None, false);
        self.error = None;
        self.edited = false;
        self.grid_edited = false;
        self.double_of = None;
        self.cue_point = 0.0;
        (self.loop_in, self.loop_region, self.loop_active) = (None, None, false);
        (self.gate_held, self.slip_pad) = (None, None);
    }

    /// Starts decoding a track and reading its analysis in the background.
    pub fn start_load(&mut self, track: &LibraryTrack, library: &UsbLibrary, ctx: &egui::Context) {
        let request = LoadRequest {
            id: track.track.id,
            from_collection: false,
            usb_root: Some(library.root.clone()),
            title: track.track.title.clone(),
            artist: track.artist.clone().unwrap_or_default(),
            key: track.key.clone(),
            bpm: track.track.bpm(),
            audio_path: library.resolve(&track.track.file_path),
            analysis_path: (!track.track.analyze_path.is_empty())
                .then(|| library.resolve(&track.track.analyze_path)),
            known: None,
            auto_cue_db: self.prefs.auto_cue_db,
            analysis: self.prefs.analysis,
        };
        self.start_request(request, ctx);
    }

    /// Loads a track of deckhand's collection.
    pub fn start_load_file(&mut self, track: &deckhand_library::Track, ctx: &egui::Context) {
        let request = LoadRequest {
            id: track.id,
            from_collection: true,
            usb_root: None,
            title: track.title.clone(),
            artist: track.artist.clone(),
            key: track.key.clone(),
            // Imported tracks can have tempo markers without a BPM of their own.
            bpm: track.bpm.or_else(|| track.tempo.first().map(|m| m.bpm)).unwrap_or(0.0),
            audio_path: track.path.clone(),
            analysis_path: track.analysis.clone(),
            known: Some(Known::from_track(track)),
            auto_cue_db: self.prefs.auto_cue_db,
            analysis: self.prefs.analysis,
        };
        self.start_request(request, ctx);
    }

    /// Loads an audio file that is in no library (from the explorer); `id`
    /// tells it apart on the decks.
    pub fn start_load_path(
        &mut self,
        id: u32,
        file: &deckhand_library::ScannedFile,
        ctx: &egui::Context,
    ) {
        let title = if file.title.is_empty() {
            file.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            file.title.clone()
        };
        let request = LoadRequest {
            id,
            from_collection: false,
            usb_root: None,
            title,
            artist: file.artist.clone(),
            key: file.key.clone(),
            bpm: file.bpm.unwrap_or(0.0),
            audio_path: file.path.clone(),
            analysis_path: None,
            known: None,
            auto_cue_db: self.prefs.auto_cue_db,
            analysis: self.prefs.analysis,
        };
        self.start_request(request, ctx);
    }

    fn start_request(&mut self, request: LoadRequest, ctx: &egui::Context) {
        let (tx, rx) = channel();
        self.loading = Some((request.title.clone(), rx));
        (self.analysing, self.analysed) = (None, false);
        self.error = None;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(request.load());
            ctx.request_repaint();
        });
    }

    /// Hands a finished load to the engine and runs time-based behavior.
    /// Call every frame.
    pub fn poll(&mut self, engine: &mut Engine) {
        if let Some((_, rx)) = &self.loading
            && let Ok(result) = rx.try_recv()
        {
            self.loading = None;
            match result {
                Ok((track, audio)) => {
                    self.audio = Some(audio.clone());
                    engine.send(Command::Load { deck: self.index, audio });
                    // New track, new stems; the Part Isolator's levels stay.
                    (self.stems, self.stem_on) = (Stems::None, [true; PARTS]);
                    self.apply_stems(engine);
                    // Auto Cue, else the earliest memory or hot cue, else the start.
                    let earliest_cue = track
                        .memory_cues
                        .iter()
                        .copied()
                        .chain(track.hot_cues.iter().flatten().map(|c| c.seconds))
                        .min_by(f64::total_cmp);
                    self.cue_point = track
                        .first_sound
                        .or(earliest_cue.filter(|_| self.prefs.start_at_first_cue))
                        .unwrap_or(0.0);
                    engine.send(Command::Seek { deck: self.index, seconds: self.cue_point });
                    (self.loop_in, self.loop_region, self.loop_active) = (None, None, false);
                    if let Some((start, end)) = track.active_loop {
                        self.loop_in = Some(start);
                        self.loop_region = Some((start, end));
                        self.loop_active = true;
                        engine.send(Command::SetLoop { deck: self.index, start, end });
                    }
                    if self.prefs.analyze_on_load
                        && let Some(audio) = &self.audio
                    {
                        self.analysing = detect_missing(&track, audio.clone());
                    }
                    self.track = Some(track);
                    (self.edited, self.grid_edited) = (false, false);
                    self.apply_mixer(engine);
                    self.phase_offset = 0.0;
                    self.apply_tempo(engine);
                }
                Err(err) => self.error = Some(format!("{err:#}")),
            }
        }
        if let Some(rx) = &self.analysing
            && let Ok(found) = rx.try_recv()
        {
            self.analysing = None;
            if let Some(track) = self.track.as_mut() {
                // Only what is still missing: a grid edited meanwhile stays.
                if let Some(grid) = found.grid.filter(|_| track.beats.is_empty()) {
                    track.set_straight_grid(
                        grid.first_beat,
                        u16::from(grid.first_number),
                        grid.bpm,
                    );
                    self.analysed = true;
                }
                if let Some(key) = found.key.filter(|_| track.key.is_none()) {
                    track.key = Some(key.camelot());
                    self.analysed = true;
                }
            }
        }
        // Pitch bend follows how fast the jog spins, smoothed over frames.
        let now = Instant::now();
        let dt = now.duration_since(self.last_poll).as_secs_f64().clamp(1e-3, 0.1);
        self.last_poll = now;
        if self.nudge_until.is_some() {
            // Spin speed over the last moments: steady while spinning, zero
            // shortly after the wheel stops.
            while self.nudge_steps.front().is_some_and(|(t, _)| now.duration_since(*t) > JOG_WINDOW)
            {
                self.nudge_steps.pop_front();
            }
            let speed =
                self.nudge_steps.iter().map(|(_, s)| s).sum::<f64>() / JOG_WINDOW.as_secs_f64();
            let spinning = !self.nudge_steps.is_empty();
            let target = if spinning { speed } else { 0.0 };
            // Quick to follow the hand, quicker still to stop with it.
            let settle = if spinning { 0.025 } else { 0.012 };
            self.nudge_velocity += (target - self.nudge_velocity) * (dt / settle).min(1.0);
            let bend = (self.nudge_velocity / self.jog_steps_per_turn
                * BEND_PER_TURN_PER_SECOND
                * self.prefs.bend_sensitivity)
                .clamp(-0.6, 0.6);
            if !spinning && bend.abs() < 0.002 {
                (self.nudge_until, self.nudge_velocity, self.nudge_sent) = (None, 0.0, 0.0);
                self.rephase = true;
                engine.send(Command::SetNudge { deck: self.index, amount: 0.0 });
            } else if (bend - self.nudge_sent).abs() > 0.0005 {
                self.nudge_sent = bend;
                engine.send(Command::SetNudge { deck: self.index, amount: bend });
            }
        }
    }

    fn quantized(&self, seconds: f64) -> f64 {
        match (&self.track, self.quantize) {
            (Some(track), true) => track.nearest_beat_time(seconds),
            _ => seconds,
        }
    }

    pub fn toggle_play(&mut self, engine: &mut Engine) {
        if self.track.is_none() {
            return;
        }
        // PLAY while CUE or a gated hot cue is held: keep playing after the
        // release, like a CDJ.
        let held = std::mem::take(&mut self.previewing) | self.gate_held.take().is_some();
        let playing = held || !engine.is_playing(self.index);
        engine.send(Command::SetPlaying { deck: self.index, playing });
    }

    pub fn cue_pressed(&mut self, engine: &mut Engine) {
        if self.track.is_none() {
            return;
        }
        let position = engine.position_secs(self.index);
        if engine.is_playing(self.index) {
            engine.send(Command::SetPlaying { deck: self.index, playing: false });
            engine.send(Command::Seek { deck: self.index, seconds: self.cue_point });
        } else if (position - self.cue_point).abs() > AT_CUE {
            self.cue_point = self.quantized(position);
            engine.send(Command::Seek { deck: self.index, seconds: self.cue_point });
        } else {
            self.previewing = true;
            engine.send(Command::SetPlaying { deck: self.index, playing: true });
        }
    }

    pub fn cue_released(&mut self, engine: &mut Engine) {
        if std::mem::take(&mut self.previewing) {
            engine.send(Command::SetPlaying { deck: self.index, playing: false });
            engine.send(Command::Seek { deck: self.index, seconds: self.cue_point });
        }
    }

    pub fn jump_to_start(&mut self, engine: &mut Engine) {
        engine.send(Command::Seek { deck: self.index, seconds: 0.0 });
    }

    /// A hot cue pad was pressed: in gate mode (or with `force_gate`) the
    /// cue plays until the pad is released; otherwise it jumps and plays.
    pub fn hot_cue_pressed(&mut self, slot: usize, force_gate: bool, engine: &mut Engine) {
        let is_set = self.track.as_ref().is_some_and(|t| t.hot_cues[slot].is_some());
        if is_set && self.slip && engine.is_playing(self.index) {
            // Slip: the cue plays while held, then the track carries on
            // where it would have been.
            self.slip_pad = Some(slot);
            engine.send(Command::SlipHold { deck: self.index, held: true });
        } else if is_set
            && (force_gate
                || (self.hot_cue_gate
                    && (!engine.is_playing(self.index) || self.prefs.gate_while_playing)))
        {
            // Gate: plays while held, then back to the cue and paused. Like
            // rekordbox, a playing deck just jumps (unless set otherwise).
            self.gate_held = Some(slot);
        }
        self.hot_cue(slot, engine);
    }

    /// A hot cue pad was released: ends a gated or slipping hot cue.
    pub fn hot_cue_released(&mut self, slot: usize, engine: &mut Engine) {
        if self.slip_pad == Some(slot) {
            self.slip_pad = None;
            engine.send(Command::SlipHold { deck: self.index, held: false });
            return;
        }
        if self.gate_held != Some(slot) {
            return;
        }
        self.gate_held = None;
        if let Some(cue) = self.track.as_ref().and_then(|t| t.hot_cues[slot]) {
            engine.send(Command::SetPlaying { deck: self.index, playing: false });
            engine.send(Command::Seek { deck: self.index, seconds: cue.seconds });
        }
    }

    // Memory cues

    /// Memory cues and loops by start time.
    fn memory_points(&self) -> Vec<(f64, Option<f64>)> {
        let Some(track) = &self.track else { return Vec::new() };
        let mut points: Vec<(f64, Option<f64>)> = track
            .memory_cues
            .iter()
            .map(|&t| (t, None))
            .chain(track.memory_loops.iter().map(|&(s, e)| (s, Some(e))))
            .collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        points
    }

    /// Calls the previous (`forward = false`) or next memory cue or loop
    /// relative to the cue point (paused) or the position (playing). During
    /// loop playback it halves / doubles the loop instead when the
    /// preference asks for Pioneer's way.
    pub fn memory_call(&mut self, forward: bool, engine: &mut Engine) {
        if self.loop_active && self.prefs.call_resizes_loop {
            self.resize_loop(if forward { 2.0 } else { 0.5 }, engine);
            return;
        }
        if self.prefs.memory_call_lock && engine.is_playing(self.index) {
            return;
        }
        let reference = if engine.is_playing(self.index) {
            engine.position_secs(self.index)
        } else {
            self.cue_point
        };
        let points = self.memory_points();
        let target = if forward {
            points.into_iter().find(|p| p.0 > reference + AT_CUE)
        } else {
            points.into_iter().rev().find(|p| p.0 < reference - AT_CUE)
        };
        let Some((start, end)) = target else { return };
        self.cue_point = start;
        engine.send(Command::Seek { deck: self.index, seconds: start });
        match end {
            Some(end) => {
                self.loop_in = Some(start);
                self.set_loop(start, end, engine);
            }
            // A plain memory cue leaves any active loop.
            None if self.loop_active => {
                self.loop_active = false;
                engine.send(Command::ClearLoop { deck: self.index });
            }
            None => {}
        }
    }

    /// Stores the active loop, or the cue point (paused) or position (playing).
    pub fn memory_set(&mut self, engine: &mut Engine) {
        let playing = engine.is_playing(self.index);
        let position = self.quantized(engine.position_secs(self.index));
        let (region, active, cue_point) = (self.loop_region, self.loop_active, self.cue_point);
        let Some(track) = self.track.as_mut() else { return };
        match region.filter(|_| active) {
            Some(range) => {
                if !track.memory_loops.iter().any(|l| (l.0 - range.0).abs() < AT_CUE) {
                    track.memory_loops.push(range);
                    self.edited = true;
                }
            }
            None => {
                let t = if playing { position } else { cue_point };
                if !track.memory_cues.iter().any(|m| (m - t).abs() < AT_CUE) {
                    track.memory_cues.push(t);
                    self.edited = true;
                    track.memory_cues.sort_by(f64::total_cmp);
                }
            }
        }
    }

    /// Deletes the memory cue or loop at the cue point.
    pub fn memory_delete(&mut self) {
        let t = self.cue_point;
        if let Some(track) = self.track.as_mut() {
            track.memory_cues.retain(|m| (m - t).abs() >= AT_CUE);
            self.edited = true;
            track.memory_loops.retain(|l| (l.0 - t).abs() >= AT_CUE);
        }
    }

    /// Jumps to a hot cue and plays, or stores one at the current position.
    pub fn hot_cue(&mut self, slot: usize, engine: &mut Engine) {
        let position = self.quantized(engine.position_secs(self.index));
        let Some(track) = self.track.as_mut() else { return };
        match track.hot_cues[slot] {
            Some(cue) => {
                // A hot cue leaves an active loop, wherever the cue is; the
                // loop stays set for RELOOP.
                if self.loop_active {
                    self.loop_active = false;
                    engine.send(Command::ClearLoop { deck: self.index });
                }
                engine.send(Command::Seek { deck: self.index, seconds: cue.seconds });
                engine.send(Command::SetPlaying { deck: self.index, playing: true });
            }
            None => {
                track.hot_cues[slot] = Some(HotCue { seconds: position, color: HOT_CUE_DEFAULT });
                self.edited = true;
            }
        }
    }

    pub fn clear_hot_cue(&mut self, slot: usize) {
        if let Some(track) = self.track.as_mut() {
            track.hot_cues[slot] = None;
            self.edited = true;
        }
    }

    pub fn seek(&mut self, seconds: f64, engine: &mut Engine) {
        engine.send(Command::Seek { deck: self.index, seconds });
    }

    /// Jumps by a number of beats along the beat grid.
    /// Jumps by a number of beats along the beat grid, keeping the position
    /// within the beat. Like rekordbox, an active loop moves along with the
    /// jump instead of being left behind.
    pub fn beat_jump(&mut self, beats: f64, engine: &mut Engine) {
        let Some(track) = &self.track else { return };
        let position = engine.position_secs(self.index);
        let active_loop = self.loop_region.filter(|_| self.loop_active);
        let Some((target, moved_loop)) = plan_beat_jump(track, position, active_loop, beats) else {
            return;
        };
        if let Some((start, end)) = moved_loop {
            self.loop_in = Some(start);
            self.set_loop(start, end, engine);
        }
        engine.send(Command::Seek { deck: self.index, seconds: target });
    }

    /// A beat-jump pad: back/forward pairs scaled by the jump range.
    pub fn pad_beat_jump(&mut self, pad: usize, engine: &mut Engine) {
        self.beat_jump(BEAT_JUMPS[pad % BEAT_JUMPS.len()] * self.jump_range, engine);
    }

    /// Scales the beat-jump pads' amounts (×½, ×2, ×1/16, ×16).
    pub fn change_jump_range(&mut self, factor: f64) {
        self.jump_range = (self.jump_range * factor).clamp(1.0 / 32.0, 64.0);
    }

    /// Length in beats of beat-loop pad 0–7.
    pub fn pad_loop_beats(&self, pad: usize) -> f64 {
        BEAT_LOOPS[pad % BEAT_LOOPS.len()] * self.loop_scale
    }

    /// Halves (`0.5`) or doubles (`2.0`) the beat-loop pads' lengths.
    pub fn change_loop_scale(&mut self, factor: f64) {
        self.loop_scale = (self.loop_scale * factor).clamp(1.0 / 16.0, 16.0);
    }

    /// Halves or doubles the size of the ◀ / ▶ beat jump.
    pub fn change_jump_size(&mut self, factor: f64) {
        self.jump_size = (self.jump_size * factor).clamp(1.0 / 8.0, 64.0);
    }

    // Loops

    fn set_loop(&mut self, start: f64, end: f64, engine: &mut Engine) {
        if end <= start {
            return;
        }
        self.loop_region = Some((start, end));
        self.loop_active = true;
        engine.send(Command::SetLoop { deck: self.index, start, end });
    }

    pub fn loop_in(&mut self, engine: &mut Engine) {
        if self.track.is_some() {
            self.loop_in = Some(self.quantized(engine.position_secs(self.index)));
        }
    }

    pub fn loop_out(&mut self, engine: &mut Engine) {
        let end = self.quantized(engine.position_secs(self.index));
        if let Some(start) = self.loop_in {
            self.set_loop(start, end, engine);
        }
    }

    /// Without a loop of its own, RELOOP and Active Loop use the memory loop
    /// playback is in, or else the next one ahead.
    fn adopt_memory_loop(&mut self, engine: &Engine) {
        if self.loop_region.is_some() {
            return;
        }
        let Some(track) = &self.track else { return };
        let position = engine.position_secs(self.index);
        let inside = track.memory_loops.iter().find(|(s, e)| (*s..*e).contains(&position));
        let ahead = track
            .memory_loops
            .iter()
            .filter(|(s, _)| *s > position)
            .min_by(|a, b| a.0.total_cmp(&b.0));
        self.loop_region = inside.or(ahead).copied();
    }

    pub fn reloop_exit(&mut self, engine: &mut Engine) {
        self.adopt_memory_loop(engine);
        if self.loop_active {
            self.loop_active = false;
            engine.send(Command::ClearLoop { deck: self.index });
        } else if let Some((start, end)) = self.loop_region {
            self.set_loop(start, end, engine);
            let position = engine.position_secs(self.index);
            if position > end || position < start {
                engine.send(Command::Seek { deck: self.index, seconds: start });
            }
        }
    }

    /// A loop of `beats` from the current beat; pressing the same size again exits.
    pub fn beat_loop(&mut self, beats: f64, engine: &mut Engine) {
        let Some(track) = &self.track else { return };
        let position = engine.position_secs(self.index);
        if self.loop_active
            && let Some((start, end)) = self.loop_region
        {
            let current = track.beat_at(end) - track.beat_at(start);
            if (current - beats).abs() < 1e-6 {
                self.loop_active = false;
                engine.send(Command::ClearLoop { deck: self.index });
                return;
            }
        }
        let beat = track.beat_at(position);
        let start_beat = if self.quantize { (beat * 4.0).floor() / 4.0 } else { beat };
        let start = track.time_of_beat(start_beat);
        let end = track.time_of_beat(start_beat + beats);
        self.loop_in = Some(start);
        self.set_loop(start, end, engine);
    }

    /// Moves the loop back (`-1`) or forward (`1`) by its own length.
    pub fn move_loop(&mut self, direction: f64, engine: &mut Engine) {
        if let Some((start, end)) = self.loop_region {
            let shift = (end - start) * direction.signum();
            if start + shift >= 0.0 {
                self.loop_in = Some(start + shift);
                self.set_loop(start + shift, end + shift, engine);
                engine.send(Command::Shift { deck: self.index, seconds: shift });
            }
        }
    }

    /// Fast search through the track (jog with Shift on many controllers).
    pub fn jog_search(&mut self, steps: i32, engine: &mut Engine) {
        engine.send(Command::Shift {
            deck: self.index,
            seconds: f64::from(steps) * SEARCH_SECONDS_PER_STEP,
        });
    }

    /// The loop ½ / ×2 message a Pioneer controller sends from its CALL
    /// buttons during loop playback: CALL as the preference says.
    pub fn call_during_loop(&mut self, forward: bool, engine: &mut Engine) {
        if self.prefs.call_resizes_loop {
            self.resize_loop(if forward { 2.0 } else { 0.5 }, engine);
        } else {
            self.memory_call(forward, engine);
        }
    }

    pub fn resize_loop(&mut self, factor: f64, engine: &mut Engine) {
        if let Some((start, end)) = self.loop_region {
            let new_end = start + (end - start) * factor;
            if new_end - start > 0.01 {
                self.set_loop(start, new_end, engine);
            }
        }
    }

    // Jog wheel

    /// Jog side or (outside vinyl scratching) top: bends the tempo while
    /// playing, moves through the track while paused.
    pub fn jog(&mut self, steps: i32, engine: &mut Engine) {
        if engine.is_playing(self.index) {
            // Bends while spinning; poll() turns the speed into a bend.
            self.nudge_steps.push_back((Instant::now(), f64::from(steps)));
            self.nudge_until = Some(Instant::now() + NUDGE_HOLD);
        } else {
            engine.send(Command::JogMove {
                deck: self.index,
                seconds: f64::from(steps) * JOG_SECONDS_PER_TURN / self.jog_steps_per_turn,
            });
        }
    }

    /// Jog top movement.
    pub fn jog_scratch(&mut self, steps: i32, engine: &mut Engine) {
        if self.is_scratching() {
            engine.send(Command::JogMove {
                deck: self.index,
                seconds: f64::from(steps) * JOG_SECONDS_PER_TURN / self.jog_steps_per_turn
                    * self.prefs.scratch_sensitivity,
            });
        } else {
            self.jog(steps, engine);
        }
    }

    /// Touching the jog top in vinyl mode holds the record.
    /// The jog or nudge buttons are moving the deck right now.
    pub fn nudging(&self) -> bool {
        self.nudge_until.is_some() || self.jog_touched || self.grabbed
    }

    /// Taking hold of a running waveform stops the deck, like a hand on the
    /// record; it plays on when let go. Paused decks just move.
    pub fn grab_waveform(&mut self, engine: &mut Engine) {
        if engine.is_playing(self.index) {
            self.grabbed = true;
            engine.send(Command::Hold { deck: self.index, held: true });
        }
    }

    /// The waveform dragged by this many seconds of music.
    pub fn drag_waveform(&mut self, seconds: f64, engine: &mut Engine) {
        let deck = self.index;
        engine.send(if self.grabbed {
            Command::JogMove { deck, seconds }
        } else {
            Command::Shift { deck, seconds }
        });
    }

    pub fn release_waveform(&mut self, engine: &mut Engine) {
        if std::mem::take(&mut self.grabbed) {
            self.rephase = true;
            engine.send(Command::Hold { deck: self.index, held: false });
        }
    }

    pub fn jog_touch(&mut self, touched: bool, engine: &mut Engine) {
        if self.jog_touched && !touched {
            self.rephase = true;
        }
        self.jog_touched = touched;
        if self.vinyl {
            engine.send(Command::Hold { deck: self.index, held: touched });
        }
    }

    // Tempo

    pub fn set_tempo_fader(&mut self, value: f32, engine: &mut Engine) {
        let moved = value.clamp(-1.0, 1.0) - self.tempo_fader;
        self.tempo_fader += moved;
        if self.sync && !self.is_master {
            // A follower's tempo comes from the master.
            return;
        }
        // A master still playing at the tempo sync gave it (it took over
        // from another deck) moves from there, so taking over never jumps.
        self.sync_ratio = moved_ratio(self.sync_ratio, moved, self.tempo_range);
        self.apply_tempo(engine);
    }

    /// Tempo back to ±0 (the master's too, with the synced decks following).
    pub fn reset_tempo(&mut self, engine: &mut Engine) {
        if self.is_master || !self.sync {
            self.sync_ratio = None;
        }
        self.set_tempo_fader(0.0, engine);
    }

    /// Whether the tempo fader does anything now (SYNC on a follower: no).
    pub fn tempo_fader_active(&self) -> bool {
        !self.sync || self.is_master
    }

    pub fn toggle_slip(&mut self, engine: &mut Engine) {
        self.slip = !self.slip;
        engine.send(Command::SetSlip { deck: self.index, on: self.slip });
    }

    // Key

    pub fn set_key_shift(&mut self, semitones: i32, engine: &mut Engine) {
        self.key_shift = semitones.clamp(-12, 12);
        self.key_synced = false;
        engine
            .send(Command::SetKeyShift { deck: self.index, semitones: f64::from(self.key_shift) });
    }

    /// Selects a pad mode; entering Keyboard mode picks its hot cue anew.
    pub fn set_pad_mode(&mut self, mode: PadMode) {
        if mode == PadMode::Keyboard {
            self.keyboard_cue = None;
        }
        self.pad_mode = mode;
    }

    /// Key Shift pads set the key to ±0 … +7 semitones from the page.
    pub fn key_shift_pad(&mut self, pad: usize, engine: &mut Engine) {
        self.set_key_shift(self.key_shift_base + (pad % 8) as i32, engine);
    }

    /// Shift + pad 7 / 8 in Key Shift mode: the pads' pitches go down / up a page.
    pub fn key_shift_page(&mut self, direction: i32) {
        self.key_shift_base = (self.key_shift_base + 8 * direction.signum()).clamp(-16, 8);
    }

    /// Keyboard mode. The first pad pressed picks its hot cue (setting one at
    /// the playback position if it is empty); after that each pad plays that
    /// hot cue ±0 … +7 semitones from the page.
    pub fn keyboard_pad(&mut self, pad: usize, engine: &mut Engine) {
        let pad = pad % 8;
        let position = self.quantized(engine.position_secs(self.index));
        let Some(track) = self.track.as_mut() else { return };
        let Some(slot) = self.keyboard_cue else {
            if track.hot_cues[pad].is_none() {
                track.hot_cues[pad] = Some(HotCue { seconds: position, color: HOT_CUE_DEFAULT });
                self.edited = true;
            }
            self.keyboard_cue = Some(pad);
            return;
        };
        let Some(cue) = track.hot_cues[slot] else {
            self.keyboard_cue = None;
            return;
        };
        self.set_key_shift(self.keyboard_base + pad as i32, engine);
        engine.send(Command::Seek { deck: self.index, seconds: cue.seconds });
        engine.send(Command::SetPlaying { deck: self.index, playing: true });
    }

    /// Shift + pad 7 / 8 in Keyboard mode: the pads' pitches go down / up a page.
    pub fn keyboard_page(&mut self, direction: i32) {
        self.keyboard_base = (self.keyboard_base + 8 * direction.signum()).clamp(-16, 8);
    }

    /// The track's key as (pitch class, minor), from names like `Fm`, `C#`,
    /// `Bbm` or Camelot `8A`.
    pub fn key(&self) -> Option<(i32, bool)> {
        parse_key(self.track.as_ref()?.key.as_deref()?)
    }

    /// The key heard now: the track's key moved by the key shift and, without
    /// Master Tempo, by the tempo change.
    pub fn heard_key(&self) -> Option<(i32, bool)> {
        let (pitch, minor) = self.key()?;
        let tempo = if self.key_lock { 0.0 } else { 12.0 * self.tempo_ratio().log2() };
        let shift = (f64::from(self.key_shift) + tempo).round() as i32;
        Some(((pitch + shift).rem_euclid(12), minor))
    }

    /// Tapping four or more times sets the BPM and a straight beat grid that
    /// keeps the first beat where it was.
    pub fn tap_bpm(&mut self) -> Option<f64> {
        let now = Instant::now();
        if self.taps.last().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(2)) {
            self.taps.clear();
        }
        self.taps.push(now);
        if self.taps.len() > 8 {
            self.taps.remove(0);
        }
        if self.taps.len() < 4 {
            return None;
        }
        let span = now.duration_since(self.taps[0]).as_secs_f64();
        // Taps run at the deck's current speed; store the track's own BPM.
        let bpm = 60.0 * (self.taps.len() - 1) as f64 / span / self.tempo_ratio();
        let track = self.track.as_mut()?;
        let (anchor, number) = track.grid_anchor();
        track.set_straight_grid(anchor, number, bpm);
        (self.edited, self.grid_edited) = (true, true);
        Some(bpm)
    }

    /// Applies a grid edit to a straight grid: `edit` gives the new anchor
    /// time, its beat number and the BPM. Returns false (and changes
    /// nothing) without a track or when the grid changes tempo.
    fn edit_grid(&mut self, edit: impl FnOnce(&LoadedTrack) -> Option<(f64, u16, f64)>) -> bool {
        let Some(track) = self.track.as_mut().filter(|t| t.grid_is_straight()) else {
            return false;
        };
        let Some((anchor, number, bpm)) = edit(track).filter(|e| e.2.is_finite() && e.2 > 0.0)
        else {
            return false;
        };
        track.set_straight_grid(anchor, number, bpm.clamp(20.0, 999.0));
        (self.edited, self.grid_edited) = (true, true);
        true
    }

    /// Moves the beat grid later (positive) or earlier by `seconds`.
    pub fn shift_grid(&mut self, seconds: f64) -> bool {
        self.edit_grid(|t| {
            let (anchor, number) = t.grid_anchor();
            let bpm = t.grid_bpm();
            (bpm > 0.0).then_some((anchor + seconds, number, bpm))
        })
    }

    /// Sets the track's BPM, keeping the first beat where it is.
    pub fn set_grid_bpm(&mut self, bpm: f64) -> bool {
        self.edit_grid(|t| {
            let (anchor, number) = t.grid_anchor();
            Some((anchor, number, bpm))
        })
    }

    /// Puts a downbeat (beat 1 of a bar) exactly at `position`.
    pub fn downbeat_here(&mut self, position: f64) -> bool {
        self.edit_grid(|t| Some((position, 1, t.grid_bpm())))
    }

    /// Makes the beat nearest `position` beat 1 of its bar; no beat moves.
    pub fn renumber_here(&mut self, position: f64) -> bool {
        self.edit_grid(|t| Some((t.nearest_beat_time(position), 1, t.grid_bpm())))
    }

    /// Fader start: the channel fader opened (play) or closed (back to cue).
    pub fn fader_start(&mut self, play: bool, engine: &mut Engine) {
        if self.track.is_none() {
            return;
        }
        let playing = engine.is_playing(self.index);
        if play && !playing {
            engine.send(Command::SetPlaying { deck: self.index, playing: true });
        } else if !play && playing {
            engine.send(Command::SetPlaying { deck: self.index, playing: false });
            engine.send(Command::Seek { deck: self.index, seconds: self.cue_point });
        }
    }

    /// A hot cue pad is held in slip mode.
    pub fn slip_held(&self) -> bool {
        self.slip_pad.is_some()
    }

    /// Length in beats of the loop playing now.
    pub fn active_loop_beats(&self) -> Option<f64> {
        let (start, end) = self.loop_region.filter(|_| self.loop_active)?;
        let track = self.track.as_ref()?;
        Some(track.beat_at(end) - track.beat_at(start))
    }

    /// Turns the stored loop on or off without moving the playhead.
    pub fn toggle_active_loop(&mut self, engine: &mut Engine) {
        self.adopt_memory_loop(engine);
        if self.loop_active {
            self.loop_active = false;
            engine.send(Command::ClearLoop { deck: self.index });
        } else if let Some((start, end)) = self.loop_region {
            self.set_loop(start, end, engine);
        }
    }

    pub fn toggle_key_lock(&mut self, engine: &mut Engine) {
        self.key_lock = !self.key_lock;
        engine.send(Command::SetKeyLock { deck: self.index, on: self.key_lock });
    }

    pub fn set_tempo_range(&mut self, range: TempoRange, engine: &mut Engine) {
        self.tempo_range = range;
        if self.tempo_fader_active() && self.sync_ratio.is_none() {
            self.apply_tempo(engine);
        }
    }

    pub fn apply_tempo(&self, engine: &mut Engine) {
        engine.send(Command::SetTempo { deck: self.index, ratio: self.tempo_ratio() });
    }

    /// Leaves sync and returns to the fader's tempo.
    pub fn sync_off(&mut self, engine: &mut Engine) {
        self.sync = false;
        self.sync_ratio = None;
        self.apply_tempo(engine);
    }

    /// Puts the loaded track back into a newly started engine where it was.
    pub fn restore(&self, engine: &mut Engine, position: f64, playing: bool) {
        let Some(audio) = &self.audio else { return };
        let deck = self.index;
        engine.send(Command::Load { deck, audio: audio.clone() });
        if let Stems::Ready(stems) = &self.stems {
            engine.send(Command::SetStems { deck, stems: Some(stems.clone()) });
        }
        self.apply_stems(engine);
        engine.send(Command::Seek { deck, seconds: position });
        if let (true, Some((start, end))) = (self.loop_active, self.loop_region) {
            engine.send(Command::SetLoop { deck, start, end });
        }
        engine.send(Command::SetPlaying { deck, playing });
    }

    /// The loaded track's gain, when Auto Gain is on.
    pub fn track_gain_db(&self) -> f32 {
        match &self.track {
            Some(track) if self.prefs.auto_gain => track.gain_db,
            _ => 0.0,
        }
    }

    /// Changes the loaded track's gain (dragged on the deck); collection
    /// tracks keep it.
    pub fn set_track_gain(&mut self, db: f32, engine: &mut Engine) {
        let Some(track) = &mut self.track else { return };
        track.gain_db = db.clamp(*TRACK_GAIN_DB.start(), *TRACK_GAIN_DB.end());
        self.apply_mixer(engine);
    }

    /// The loaded track's gain worked out from its loudness.
    pub fn measured_gain_db(&self) -> Option<f32> {
        self.audio.as_deref().map(auto_gain_db)
    }

    /// The decoded track's sample rate and length in frames.
    /// The track's audio as the deck plays it.
    pub fn audio(&self) -> Option<Arc<TrackAudio>> {
        self.audio.clone()
    }

    /// Takes the separated stems, once they are ready.
    pub fn set_stems(&mut self, stems: Arc<StemLayers>, engine: &mut Engine) {
        engine.send(Command::SetStems { deck: self.index, stems: Some(stems.clone()) });
        self.stems = Stems::Ready(stems);
        self.apply_stems(engine);
    }

    /// Whether the part can be turned off on its own (stems ready, and the
    /// model made that stem).
    pub fn has_part(&self, part: usize) -> bool {
        matches!(&self.stems, Stems::Ready(stems) if stems.has(part))
    }

    /// The parts' levels: on or off, times the Part Isolator's knob.
    pub fn stem_levels(&self) -> [f32; PARTS] {
        // Left of the centre cuts (squared, for an even feel), right of it
        // boosts up to +6 dB.
        let knob = |v: f32| if v <= 0.5 { (v * 2.0).powi(2) } else { 1.0 + (v - 0.5) * 2.0 };
        std::array::from_fn(|part| {
            let on = if self.stem_on[part] || !self.has_part(part) { 1.0 } else { 0.0 };
            let isolator = match part {
                _ if !self.part_isolator => 1.0,
                VOCALS => knob(self.isolator[0]),
                DRUMS => knob(self.isolator[2]),
                _ => knob(self.isolator[1]),
            };
            on * isolator
        })
    }

    pub fn apply_stems(&self, engine: &mut Engine) {
        engine.send(Command::SetStemLevels { deck: self.index, levels: self.stem_levels() });
    }

    /// Turns a part off or on (once the stems are ready).
    pub fn toggle_stem(&mut self, part: usize, engine: &mut Engine) {
        if part < PARTS && self.has_part(part) {
            self.stem_on[part] = !self.stem_on[part];
            self.apply_stems(engine);
        }
    }

    /// Takes the stems away: the deck plays the track as it is.
    pub fn clear_stems(&mut self, engine: &mut Engine) {
        (self.stems, self.stem_on, self.part_isolator) = (Stems::None, [true; PARTS], false);
        engine.send(Command::SetStems { deck: self.index, stems: None });
        self.apply_stems(engine);
    }

    pub fn toggle_part_isolator(&mut self, engine: &mut Engine) {
        self.part_isolator = !self.part_isolator;
        self.apply_stems(engine);
    }

    /// Sends all mixer settings of this channel.
    pub fn apply_mixer(&self, engine: &mut Engine) {
        let deck = self.index;
        engine.send(Command::SetVolume { deck, level: self.volume });
        // Trim knob: 0.5 = 0 dB, range ±12 dB.
        let trim_db = (self.trim - 0.5) * 24.0 + self.track_gain_db();
        engine.send(Command::SetTrim { deck, gain: 10f32.powf(trim_db / 20.0) });
        for (band, &knob) in self.eq.iter().enumerate() {
            engine.send(Command::SetEq { deck, band, db: eq_db(knob, self.prefs.isolator) });
        }
        engine.send(Command::SetFilter { deck, position: self.filter });
        engine.send(Command::SetCrossfaderAssign { deck, side: self.side });
        engine.send(Command::SetHeadphoneCue { deck, on: self.headphone_cue });
        engine.send(Command::SetKeyLock { deck, on: self.key_lock });
        engine.send(Command::SetVinylSpeed {
            deck,
            start: self.prefs.vinyl_start,
            brake: self.prefs.vinyl_brake,
        });
        engine.send(Command::SetSlip { deck, on: self.slip });
        engine.send(Command::SetKeyShift { deck, semitones: f64::from(self.key_shift) });
    }
}

const NOTES: [&str; 12] = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];

/// `Fm`, `C#`, `Bbm`, `F#min` or Camelot `8A` / `8B` → (pitch class, minor).
pub fn parse_key(text: &str) -> Option<(i32, bool)> {
    let text = text.trim();
    if let Some(number) =
        text.strip_suffix(['A', 'a', 'B', 'b']).and_then(|n| n.parse::<i32>().ok())
        && (1..=12).contains(&number)
    {
        let minor = text.ends_with(['A', 'a']);
        // 8A = A minor, 8B = C major; each step is a fifth.
        let base = if minor { 9 } else { 0 };
        return Some(((base + 7 * (number - 8)).rem_euclid(12), minor));
    }
    let mut chars = text.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    let mut pitch: i32 = match letter {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let rest: String = chars.collect();
    let rest = if let Some(r) = rest.strip_prefix(['#', '♯']) {
        pitch += 1;
        r
    } else if let Some(r) = rest.strip_prefix(['b', '♭']) {
        pitch -= 1;
        r
    } else {
        &rest
    };
    let minor = rest.starts_with('m') && !rest.starts_with("maj");
    Some((pitch.rem_euclid(12), minor))
}

pub fn key_name((pitch, minor): (i32, bool)) -> String {
    format!("{}{}", NOTES[pitch.rem_euclid(12) as usize], if minor { "m" } else { "" })
}

/// How keys are written, like rekordbox's "Key display format".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyNotation {
    /// `Fm`, `Ab`
    Classic,
    /// The Camelot wheel: `4A`, `4B`.
    #[default]
    Alphanumeric,
}

/// Camelot number 1–12 of a key: 8A = A minor, 8B = C major, each step a fifth.
pub fn camelot_number((pitch, minor): (i32, bool)) -> i32 {
    let base = if minor { 9 } else { 0 };
    ((pitch - base) * 7 + 7).rem_euclid(12) + 1
}

pub fn format_key(key: (i32, bool), notation: KeyNotation) -> String {
    match notation {
        KeyNotation::Classic => key_name(key),
        KeyNotation::Alphanumeric => {
            format!("{}{}", camelot_number(key), if key.1 { 'A' } else { 'B' })
        }
    }
}

/// A key as written in tags or an export, rewritten in `notation`; text
/// that is not a key stays as it is.
pub fn display_key(text: &str, notation: KeyNotation) -> String {
    parse_key(text).map_or_else(|| text.to_owned(), |key| format_key(key, notation))
}

/// Semitones (−6 … +5) that bring `own` to `target`, or to its relative
/// major/minor when the modes differ.
pub fn key_sync_shift(own: (i32, bool), target: (i32, bool)) -> i32 {
    let as_major = |(pitch, minor): (i32, bool)| if minor { pitch + 3 } else { pitch };
    let difference = (as_major(target) - as_major(own)).rem_euclid(12);
    if difference > 5 { difference - 12 } else { difference }
}

/// Where a beat jump lands, and where an active loop moves to; `None` if
/// the jump would leave the track.
pub fn plan_beat_jump(
    track: &LoadedTrack,
    position: f64,
    active_loop: Option<(f64, f64)>,
    beats: f64,
) -> Option<(f64, Option<(f64, f64)>)> {
    let shift = |t: f64| track.time_of_beat(track.beat_at(t) + beats);
    let target = shift(position);
    if target < 0.0 || target > track.duration {
        return None;
    }
    Some((target, active_loop.map(|(start, end)| (shift(start), shift(end)))))
}

/// EQ knob position to gain: center flat, right end +6 dB. As an
/// isolator the left end cuts the band fully; as an EQ it stops at −26 dB.
pub fn eq_db(knob: f32, isolator: bool) -> f32 {
    use deckhand_audio::{EQ_MAX_DB, EQ_MIN_DB};
    let knob = knob.clamp(0.0, 1.0);
    if knob >= 0.5 {
        (knob - 0.5) * 2.0 * EQ_MAX_DB
    } else if isolator {
        // Level falls like the knob's travel; the end kills the band.
        if knob <= 0.02 { EQ_MIN_DB } else { (20.0 * (knob / 0.5).log10()).max(EQ_MIN_DB + 0.1) }
    } else {
        // Just above the kill threshold, so the band stays audible.
        ((0.5 - knob) * 2.0 * EQ_MIN_DB).max(EQ_MIN_DB + 0.1)
    }
}

/// Settings from the Preferences window that decks follow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeckPrefs {
    pub memory_call_lock: bool,
    pub key_notation: KeyNotation,
    pub call_resizes_loop: bool,
    /// Gated hot cues also stop on release when the deck was playing.
    pub gate_while_playing: bool,
    pub start_at_first_cue: bool,
    /// Auto Cue level in dB, when Auto Cue is on for the deck.
    pub auto_cue_db: Option<f32>,
    pub isolator: bool,
    pub scratch_sensitivity: f64,
    /// Jog pitch bend strength, 1.0 = a quarter faster per turn per second.
    pub bend_sensitivity: f64,
    /// Vinyl Speed Adjust, seconds.
    pub vinyl_start: f64,
    pub vinyl_brake: f64,
    pub analysis: AnalysisMethod,
    /// Detect a missing beat grid or key once a track is loaded.
    pub analyze_on_load: bool,
    /// Tracks play at their track gain.
    pub auto_gain: bool,
}

/// How tracks without rekordbox's analysis are analysed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnalysisMethod {
    /// Waveforms matching rekordbox's analysis files.
    #[default]
    Rekordbox,
    /// deckhand's own, quicker method.
    Deckhand,
}

impl Default for DeckPrefs {
    fn default() -> Self {
        Self {
            memory_call_lock: true,
            key_notation: KeyNotation::default(),
            call_resizes_loop: false,
            gate_while_playing: false,
            start_at_first_cue: true,
            auto_cue_db: None,
            isolator: true,
            scratch_sensitivity: 1.0,
            bend_sensitivity: 1.0,
            vinyl_start: 0.0,
            vinyl_brake: 0.0,
            analysis: AnalysisMethod::default(),
            analyze_on_load: true,
            auto_gain: true,
        }
    }
}

/// A taken-over tempo (`ratio`, from sync) after the fader moved by
/// `moved`: shifted by as much as the fader would shift its own tempo.
fn moved_ratio(ratio: Option<f64>, moved: f32, range: TempoRange) -> Option<f64> {
    ratio.map(|r| (r + f64::from(moved) * range.fraction()).max(0.05))
}

/// A deck's live stems.
#[derive(Clone, Default)]
pub enum Stems {
    #[default]
    None,
    /// Being separated, with the share done when the model says.
    Separating(Option<u32>),
    Ready(Arc<StemLayers>),
    Failed,
}

/// What detection found for a loaded track.
struct Detected {
    grid: Option<deckhand_analysis::beats::Grid>,
    key: Option<deckhand_analysis::key::Key>,
}

/// Detects the beat grid and/or key `track` lacks, in the background;
/// `None` when it has both.
fn detect_missing(track: &LoadedTrack, audio: Arc<TrackAudio>) -> Option<Receiver<Detected>> {
    let (grid, key) = (track.beats.is_empty(), track.key.is_none());
    if !grid && !key {
        return None;
    }
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        use deckhand_analysis::beats::{DEFAULT_RANGE, beat_grid};
        let found = Detected {
            grid: grid
                .then(|| beat_grid(&audio.samples, audio.sample_rate, DEFAULT_RANGE))
                .flatten(),
            key: key
                .then(|| deckhand_analysis::key::detect_key(&audio.samples, audio.sample_rate))
                .flatten(),
        };
        let _ = tx.send(found);
    });
    Some(rx)
}

/// When the first sound louder than `db` starts, in seconds.
fn first_sound(audio: &TrackAudio, db: f32) -> Option<f64> {
    let threshold = 10f32.powf(db / 20.0);
    let index = audio.samples.iter().position(|s| s.abs() > threshold)?;
    Some((index / 2) as f64 / f64::from(audio.sample_rate))
}

/// Loudness every track is brought to by Auto Gain, in dB of the gated
/// mean square (about LUFS for full-range club music).
const AUTO_GAIN_TARGET_DB: f32 = -9.0;
/// Track gain range, as on rekordbox's decks.
pub const TRACK_GAIN_DB: std::ops::RangeInclusive<f32> = -12.0..=12.0;

/// The gain that brings a track to [`AUTO_GAIN_TARGET_DB`]: the mean square
/// of 400 ms blocks, leaving out silence and blocks 10 dB below the average
/// (the gating of EBU R 128, without its weighting filter).
pub fn auto_gain_db(audio: &TrackAudio) -> f32 {
    let block = (audio.sample_rate as usize * 2 * 2 / 5).max(2);
    let powers: Vec<f64> = audio
        .samples
        .chunks(block)
        .filter(|c| c.len() == block)
        .map(|c| c.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / c.len() as f64)
        .filter(|&p| p > 1e-7)
        .collect();
    if powers.is_empty() {
        return 0.0;
    }
    let mean = powers.iter().sum::<f64>() / powers.len() as f64;
    let loud: Vec<f64> = powers.iter().copied().filter(|&p| p >= mean * 0.1).collect();
    let gated = loud.iter().sum::<f64>() / loud.len().max(1) as f64;
    // The mean square of both channels together: +3 dB for a stereo sum.
    let db = 10.0 * gated.log10() as f32 + 3.0;
    (AUTO_GAIN_TARGET_DB - db).clamp(*TRACK_GAIN_DB.start(), *TRACK_GAIN_DB.end())
}

/// Grid and cues the collection already has (from a rekordbox import).
struct Known {
    gain_db: Option<f32>,
    beats: Vec<(f64, u16)>,
    hot_cues: [Option<HotCue>; HOT_CUES],
    memory_cues: Vec<f64>,
    memory_loops: Vec<(f64, f64)>,
}

impl Known {
    fn from_track(track: &deckhand_library::Track) -> Self {
        let mut known = Known {
            gain_db: track.gain_db,
            beats: track.beats(track.duration.max(1.0) + 1.0),
            hot_cues: [None; HOT_CUES],
            memory_cues: Vec::new(),
            memory_loops: Vec::new(),
        };
        for cue in &track.cues {
            match (cue.kind, cue.end) {
                (deckhand_library::CueKind::Hot(n), _) if usize::from(n) < HOT_CUES => {
                    let color =
                        cue.color.map_or(HOT_CUE_DEFAULT, |[r, g, b]| Color32::from_rgb(r, g, b));
                    known.hot_cues[usize::from(n)] = Some(HotCue { seconds: cue.start, color });
                }
                (deckhand_library::CueKind::Memory, Some(end)) => {
                    known.memory_loops.push((cue.start, end))
                }
                (deckhand_library::CueKind::Memory, None) => known.memory_cues.push(cue.start),
                _ => {}
            }
        }
        known.memory_cues.sort_by(f64::total_cmp);
        known
    }
}

struct LoadRequest {
    id: u32,
    known: Option<Known>,
    auto_cue_db: Option<f32>,
    from_collection: bool,
    usb_root: Option<PathBuf>,
    title: String,
    artist: String,
    key: Option<String>,
    bpm: f64,
    audio_path: PathBuf,
    analysis_path: Option<PathBuf>,
    analysis: AnalysisMethod,
}

impl LoadRequest {
    fn load(self) -> LoadResult {
        if !self.audio_path.is_file() {
            let name = self
                .audio_path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            anyhow::bail!("Audio file not found: {name}");
        }
        let audio =
            decode_file(&self.audio_path).with_context(|| format!("loading {}", self.title))?;
        let dat = self.analysis_path.as_ref().and_then(|p| AnlzFile::open(p).ok());
        let ext =
            self.analysis_path.as_ref().and_then(|p| AnlzFile::open(p.with_extension("EXT")).ok());

        let two_ex =
            self.analysis_path.as_ref().and_then(|p| AnlzFile::open(p.with_extension("2EX")).ok());
        // Without rekordbox's analysis: worked out to match its files,
        // or deckhand's quicker way.
        let worked_out = || match self.analysis {
            AnalysisMethod::Rekordbox => {
                let columns =
                    deckhand_analysis::waveform::color_detail(&audio.samples, audio.sample_rate);
                let data = columns.iter().flat_map(|c| c.to_be_bytes()).collect();
                let waveform = Waveform { kind: WaveformKind::ColorDetail, entry_len: 2, data };
                let ext = AnlzFile { sections: vec![Section::Waveform(waveform)] };
                WaveformData::from_analysis(Some(&ext), None)
            }
            AnalysisMethod::Deckhand => None,
        };
        let mut waveform = WaveformData::from_analysis(ext.as_ref(), two_ex.as_ref())
            .or_else(worked_out)
            .unwrap_or_else(|| WaveformData::from_peaks(&peak_waveform(&audio), Vec::new()));
        if !waveform.has_bands() {
            waveform.bands = band_waveform(&audio);
        }
        waveform.shape_from_bands();
        let mut beats: Vec<(f64, u16)> = dat
            .as_ref()
            .and_then(AnlzFile::beat_grid)
            .map(|grid| {
                grid.beats.iter().map(|b| (f64::from(b.time_ms) / 1000.0, b.bar_position)).collect()
            })
            .unwrap_or_default();
        let bpm = if self.bpm > 0.0 {
            self.bpm
        } else {
            dat.as_ref()
                .and_then(AnlzFile::beat_grid)
                .and_then(|g| g.beats.first())
                .map_or(0.0, |b| b.bpm())
        };

        // Extended cue lists carry colors; fall back to the basic ones.
        let mut hot_cues = [None; HOT_CUES];
        let mut memory_cues = Vec::new();
        let mut memory_loops = Vec::new();
        let lists: Vec<_> = ext
            .iter()
            .flat_map(|f| f.cue_lists())
            .filter(|l| l.extended)
            .chain(dat.iter().flat_map(|f| f.cue_lists()))
            .collect();
        let extended = lists.iter().any(|l| l.extended && !l.cues.is_empty());
        for list in lists.into_iter().filter(|l| l.extended == extended) {
            for cue in &list.cues {
                let seconds = f64::from(cue.time_ms) / 1000.0;
                match (list.kind, cue.hot_cue) {
                    (CueListKind::Hot, Some(n)) if (1..=HOT_CUES as u32).contains(&n) => {
                        let color = cue.hot_cue_color.map_or(HOT_CUE_DEFAULT, |c| {
                            Color32::from_rgb(c.rgb[0], c.rgb[1], c.rgb[2])
                        });
                        hot_cues[n as usize - 1] = Some(HotCue { seconds, color });
                    }
                    (CueListKind::Memory, _) => match (cue.kind, cue.loop_end_ms) {
                        (CueKind::Loop, Some(end)) => {
                            memory_loops.push((seconds, f64::from(end) / 1000.0))
                        }
                        _ => memory_cues.push(seconds),
                    },
                    _ => {}
                }
            }
        }
        memory_cues.sort_by(f64::total_cmp);
        // rekordbox's "active loop": flagged in the basic memory cue list.
        let active_loop = dat
            .iter()
            .flat_map(|f| f.cue_lists())
            .filter(|l| !l.extended && l.kind == CueListKind::Memory)
            .flat_map(|l| l.cues.clone())
            .find(|c| c.status == Some(CueStatus::ActiveLoop))
            .and_then(|c| {
                c.loop_end_ms.map(|end| (f64::from(c.time_ms) / 1000.0, f64::from(end) / 1000.0))
            })
            .filter(|(start, end)| end > start);
        let stored_gain = self.known.as_ref().and_then(|k| k.gain_db);
        if let Some(known) = self.known {
            // Collection tracks: the collection's grid and cues (edited in
            // deckhand) win over rekordbox's analysis, which then only gives
            // the waveform. Otherwise the analysis wins.
            let known_cues = known.hot_cues.iter().any(Option::is_some)
                || !known.memory_cues.is_empty()
                || !known.memory_loops.is_empty();
            let no_cues = hot_cues.iter().all(Option::is_none)
                && memory_cues.is_empty()
                && memory_loops.is_empty();
            if beats.is_empty() || (self.from_collection && !known.beats.is_empty()) {
                beats = known.beats;
            }
            if no_cues || (self.from_collection && known_cues) {
                (hot_cues, memory_cues, memory_loops) =
                    (known.hot_cues, known.memory_cues, known.memory_loops);
            }
        }

        let track = LoadedTrack {
            id: self.id,
            path: self.audio_path.clone(),
            from_collection: self.from_collection,
            usb_root: self.usb_root,
            first_sound: self.auto_cue_db.and_then(|db| first_sound(&audio, db)),
            active_loop,
            title: self.title,
            artist: self.artist,
            key: self.key,
            bpm,
            duration: audio.duration_secs(),
            waveform,
            beats,
            hot_cues,
            memory_cues,
            memory_loops,
            gain_db: stored_gain.unwrap_or_else(|| auto_gain_db(&audio)),
        };
        Ok((track, Arc::new(audio)))
    }
}

#[cfg(test)]
mod tests {

    /// Time of each step of loading a track:
    /// `DECKHAND_TEST_TRACK=song.wav cargo test -p deckhand-app load_steps -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn load_steps() {
        let path = PathBuf::from(std::env::var("DECKHAND_TEST_TRACK").unwrap());
        let time = |name: &str, started: std::time::Instant| {
            eprintln!("{name:>14}: {:6.3} s", started.elapsed().as_secs_f64());
        };
        let started = std::time::Instant::now();
        let audio = decode_file(&path).unwrap();
        time("decode", started);
        let started = std::time::Instant::now();
        let _ = deckhand_analysis::waveform::color_detail(&audio.samples, audio.sample_rate);
        time("color waveform", started);
        let started = std::time::Instant::now();
        let _ = band_waveform(&audio);
        time("band waveform", started);
        let started = std::time::Instant::now();
        let _ = peak_waveform(&audio);
        time("peak waveform", started);
        let started = std::time::Instant::now();
        let _ = auto_gain_db(&audio);
        time("auto gain", started);
        let started = std::time::Instant::now();
        let _ = first_sound(&audio, -40.0);
        time("first sound", started);
        eprintln!("{:.0} s of audio", audio.duration_secs());
    }
    use super::*;

    #[test]
    fn auto_gain_follows_loudness() {
        // A full-scale stereo 100 Hz sine is 0 dB here (as in LUFS), so it comes
        // down to the target; silence and a quiet intro do not count.
        let sine = |amp: f32| -> Vec<f32> {
            (0..44_100 * 4)
                .flat_map(|i| {
                    let s = amp * ((i % 441) as f32 / 441.0 * std::f32::consts::TAU).sin();
                    [s, s]
                })
                .collect()
        };
        let audio = TrackAudio { sample_rate: 44_100, samples: sine(1.0) };
        assert!(
            (auto_gain_db(&audio) - AUTO_GAIN_TARGET_DB).abs() < 0.1,
            "{}",
            auto_gain_db(&audio)
        );
        let mut samples = vec![0.0; 44_100 * 4];
        samples.extend(sine(0.01));
        samples.extend(sine(0.5));
        let half = auto_gain_db(&TrackAudio { sample_rate: 44_100, samples });
        assert!((half - (AUTO_GAIN_TARGET_DB + 6.0)).abs() < 0.2, "{half}");
        assert_eq!(
            auto_gain_db(&TrackAudio { sample_rate: 44_100, samples: vec![0.0; 1000] }),
            0.0
        );
    }

    fn track(beats: Vec<(f64, u16)>, bpm: f64) -> LoadedTrack {
        LoadedTrack {
            id: 1,
            path: PathBuf::new(),
            from_collection: false,
            usb_root: None,
            first_sound: None,
            active_loop: None,
            title: String::new(),
            artist: String::new(),
            key: None,
            bpm,
            duration: 300.0,
            waveform: WaveformData::default(),
            beats,
            hot_cues: [None; HOT_CUES],
            memory_cues: Vec::new(),
            memory_loops: Vec::new(),
            gain_db: 0.0,
        }
    }

    #[test]
    fn beat_positions_follow_the_grid() {
        // 120 BPM starting at 0.1 s.
        let grid: Vec<(f64, u16)> =
            (0..100).map(|i| (0.1 + f64::from(i) * 0.5, (i % 4 + 1) as u16)).collect();
        let t = track(grid, 120.0);
        assert!((t.beat_at(0.1) - 0.0).abs() < 1e-9);
        assert!((t.beat_at(1.35) - 2.5).abs() < 1e-9);
        assert!((t.time_of_beat(2.5) - 1.35).abs() < 1e-9);
        assert!((t.nearest_beat_time(1.2) - 1.1).abs() < 1e-9);
        // Before the first beat the grid is extended backwards.
        assert!((t.beat_at(0.0) + 0.2).abs() < 1e-9);
    }

    #[test]
    fn beat_positions_without_grid_use_bpm() {
        let t = track(Vec::new(), 128.0);
        assert!((t.time_of_beat(4.0) - 4.0 * 60.0 / 128.0).abs() < 1e-9);
    }

    #[test]
    fn beat_jump_keeps_phase_and_moves_loops() {
        // 120 BPM grid from 0.1 s: beats every 0.5 s.
        let grid: Vec<(f64, u16)> =
            (0..400).map(|i| (0.1 + f64::from(i) * 0.5, (i % 4 + 1) as u16)).collect();
        let t = track(grid, 120.0);
        // 0.2 s into a beat stays 0.2 s into the beat 4 beats later.
        let (target, moved) = plan_beat_jump(&t, 10.3, None, 4.0).unwrap();
        assert!((target - 12.3).abs() < 1e-9 && moved.is_none(), "{target}");
        let (target, _) = plan_beat_jump(&t, 10.3, None, -1.0).unwrap();
        assert!((target - 9.8).abs() < 1e-9, "{target}");
        // An active 2-beat loop moves along with a 1-beat jump.
        let (target, moved) = plan_beat_jump(&t, 20.35, Some((20.1, 21.1)), 1.0).unwrap();
        assert!((target - 20.85).abs() < 1e-9);
        let (start, end) = moved.unwrap();
        assert!((start - 20.6).abs() < 1e-9 && (end - 21.6).abs() < 1e-9, "{start} {end}");
        // Jumps off either end of the track are refused.
        assert!(plan_beat_jump(&t, 1.0, None, -8.0).is_none());
        assert!(plan_beat_jump(&t, 299.0, None, 8.0).is_none());
    }

    /// A deck holding a 120 BPM track (beats every 0.5 s from 0.1 s, the
    /// first one beat 1).
    fn grid_deck() -> Deck {
        let grid = (0..600).map(|i| (0.1 + f64::from(i) * 0.5, (i % 4 + 1) as u16)).collect();
        let mut deck = Deck::new(0);
        deck.track = Some(track(grid, 120.0));
        deck
    }

    fn first_beats(deck: &Deck) -> Vec<(f64, u16)> {
        let beats = &deck.track.as_ref().unwrap().beats;
        beats[..3].iter().map(|&(t, n)| ((t * 1e6).round() / 1e6, n)).collect()
    }

    #[test]
    fn grid_shift_keeps_bars_and_fills_the_start() {
        let mut deck = grid_deck();
        assert!(deck.shift_grid(0.010));
        assert_eq!(first_beats(&deck), [(0.11, 1), (0.61, 2), (1.11, 3)]);
        assert!(deck.grid_edited && deck.edited);
        // Earlier: the downbeat goes before the start, the track starts on 2.
        assert!(deck.shift_grid(-0.2));
        assert_eq!(first_beats(&deck), [(0.41, 2), (0.91, 3), (1.41, 4)]);
        // Later by more than the gap at the start: a beat 4 comes in before.
        assert!(deck.shift_grid(0.14));
        assert_eq!(first_beats(&deck), [(0.05, 1), (0.55, 2), (1.05, 3)]);
        assert!(deck.shift_grid(0.45));
        assert_eq!(first_beats(&deck), [(0.0, 4), (0.5, 1), (1.0, 2)]);
        assert!(deck.track.as_ref().unwrap().beats.last().unwrap().0 < 300.0);
    }

    #[test]
    fn grid_bpm_keeps_the_first_beat() {
        let mut deck = grid_deck();
        assert!(deck.set_grid_bpm(60.0));
        let track = deck.track.as_ref().unwrap();
        assert_eq!(track.bpm, 60.0);
        assert_eq!(first_beats(&deck), [(0.1, 1), (1.1, 2), (2.1, 3)]);
        assert!(deck.set_grid_bpm(240.0));
        assert_eq!(first_beats(&deck), [(0.1, 1), (0.35, 2), (0.6, 3)]);
    }

    #[test]
    fn downbeat_and_renumbering() {
        let mut deck = grid_deck();
        // A downbeat exactly at 10.3 s: beats move, 10.3 is beat 1.
        assert!(deck.downbeat_here(10.3));
        let track = deck.track.as_ref().unwrap();
        assert_eq!(track.bar_and_beat(10.3).1, 1);
        assert_eq!(first_beats(&deck), [(0.3, 1), (0.8, 2), (1.3, 3)]);
        // Renumbering: the beat nearest 1.75 s (1.8) becomes 1; none move.
        assert!(deck.renumber_here(1.75));
        assert_eq!(first_beats(&deck), [(0.3, 2), (0.8, 3), (1.3, 4)]);
        assert_eq!(deck.track.as_ref().unwrap().bar_and_beat(1.85).1, 1);
    }

    #[test]
    fn grid_edits_use_the_grid_tempo_without_a_bpm() {
        // Collection tracks can have tempo markers but no BPM of their own.
        let mut deck = grid_deck();
        deck.track.as_mut().unwrap().bpm = 0.0;
        assert!((deck.track.as_ref().unwrap().grid_bpm() - 120.0).abs() < 1e-9);
        assert!(deck.shift_grid(0.01));
        assert_eq!(deck.track.as_ref().unwrap().bpm, 120.0);
        assert_eq!(first_beats(&deck), [(0.11, 1), (0.61, 2), (1.11, 3)]);
    }

    #[test]
    fn grids_that_change_tempo_are_left_alone() {
        let mut deck = grid_deck();
        let track = deck.track.as_mut().unwrap();
        // From beat 300 on, 1% slower.
        let from = track.beats[300].0;
        for beat in &mut track.beats[300..] {
            beat.0 += (beat.0 - from) * 0.01;
        }
        assert!(!track.grid_is_straight());
        let before = track.beats.clone();
        assert!(!deck.shift_grid(0.01));
        assert!(!deck.downbeat_here(5.0));
        assert_eq!(deck.track.as_ref().unwrap().beats, before);
        assert!(!deck.grid_edited);
        // Rounded (ms) beat times still count as one tempo.
        let mut deck = grid_deck();
        let track = deck.track.as_mut().unwrap();
        for beat in &mut track.beats {
            beat.0 = (beat.0 * 1000.0 + 0.4).round() / 1000.0;
        }
        assert!(track.grid_is_straight());
    }

    #[test]
    fn a_taken_over_tempo_moves_with_the_fader() {
        // Playing at +3 % from sync; the fader moves up a tenth of its
        // travel on ±10 %: +1 % more, no jump to the fader's own position.
        let ratio = moved_ratio(Some(1.03), 0.1, TempoRange::TEN).unwrap();
        assert!((ratio - 1.04).abs() < 1e-6, "{ratio}");
        assert_eq!(moved_ratio(None, 0.5, TempoRange::TEN), None);
        // Never stopped or reversed by the fader.
        assert_eq!(moved_ratio(Some(0.1), -1.0, TempoRange::WIDE), Some(0.05));
    }

    #[test]
    fn only_a_followers_fader_is_inactive() {
        let mut deck = Deck::new(0);
        assert!(deck.tempo_fader_active());
        deck.sync = true;
        assert!(!deck.tempo_fader_active());
        deck.is_master = true;
        assert!(deck.tempo_fader_active());
    }

    #[test]
    fn tempo_ranges_cycle() {
        let ranges = TempoRange::DEFAULTS;
        assert_eq!(TempoRange::WIDE.next(&ranges), TempoRange::SIX);
        assert_eq!(TempoRange::SIX.next(&ranges), TempoRange::TEN);
        // Not in the list: on to the next one above it.
        assert_eq!(TempoRange::percent(8).next(&ranges), TempoRange::TEN);
        // Just one range: it stays.
        assert_eq!(TempoRange::WIDE.next(&[TempoRange::WIDE]), TempoRange::WIDE);
        assert_eq!(TempoRange::percent(8).nearest(&ranges), TempoRange::SIX);
        assert_eq!(TempoRange::WIDE.label(), "WIDE");
        assert_eq!(TempoRange::percent(25).label(), "±25%");
        assert_eq!(TempoRange::percent(0).fraction(), 0.01);
    }

    #[test]
    fn tempo_ranges_load_old_names_and_percentages() {
        #[derive(serde::Deserialize)]
        struct Saved {
            a: TempoRange,
            b: TempoRange,
            c: TempoRange,
        }
        let saved: Saved = toml::from_str("a = \"Wide\"\nb = \"Six\"\nc = 25").unwrap();
        assert_eq!(
            (saved.a, saved.b, saved.c),
            (TempoRange::WIDE, TempoRange::SIX, TempoRange::percent(25))
        );
    }

    #[test]
    fn eq_knob_curve() {
        use deckhand_audio::{EQ_MAX_DB, EQ_MIN_DB};
        for isolator in [true, false] {
            assert_eq!(eq_db(0.5, isolator), 0.0);
            assert!((eq_db(1.0, isolator) - EQ_MAX_DB).abs() < 1e-6);
        }
        assert_eq!(eq_db(0.0, true), EQ_MIN_DB, "isolator kills");
        assert!(eq_db(0.0, false) > EQ_MIN_DB, "EQ only cuts");
        assert!((eq_db(0.25, true) + 6.02).abs() < 0.01, "half travel = −6 dB");
    }

    #[test]
    fn auto_cue_finds_the_first_sound() {
        let mut samples = vec![0.0001f32; 2000];
        samples.extend([0.5, 0.5]);
        let audio = TrackAudio { sample_rate: 1000, samples };
        assert_eq!(first_sound(&audio, -60.0), Some(1.0));
        assert_eq!(first_sound(&audio, -90.0), Some(0.0), "the noise counts at −90 dB");
    }

    #[test]
    fn keys_parse_and_sync() {
        assert_eq!(parse_key("Fm"), Some((5, true)));
        assert_eq!(parse_key("C#"), Some((1, false)));
        assert_eq!(parse_key("Bbm"), Some((10, true)));
        assert_eq!(parse_key("8A"), Some((9, true)));
        assert_eq!(parse_key("1B"), Some((11, false)));
        assert_eq!(parse_key("12A"), Some((1, true)));
        assert_eq!(key_name((10, true)), "Bbm");
        let both = |text: &str| {
            (display_key(text, KeyNotation::Classic), display_key(text, KeyNotation::Alphanumeric))
        };
        assert_eq!(both("4A"), ("Fm".into(), "4A".into()));
        assert_eq!(both("Fm"), ("Fm".into(), "4A".into()));
        assert_eq!(both("C"), ("C".into(), "8B".into()));
        assert_eq!(both("Am"), ("Am".into(), "8A".into()));
        assert_eq!(both("12A"), ("C#m".into(), "12A".into()));
        assert_eq!(both("1B"), ("B".into(), "1B".into()));
        assert_eq!(both("?"), ("?".into(), "?".into()));
        for number in 1..=12 {
            for letter in ["A", "B"] {
                let camelot = format!("{number}{letter}");
                assert_eq!(display_key(&camelot, KeyNotation::Alphanumeric), camelot);
            }
        }
        // Am → Bm: up two; Am → C (its relative major): no change.
        assert_eq!(key_sync_shift((9, true), (11, true)), 2);
        assert_eq!(key_sync_shift((9, true), (0, false)), 0);
        // Never more than a tritone away.
        assert_eq!(key_sync_shift((0, false), (11, false)), -1);
    }

    #[test]
    fn bar_counter_and_memory_countdown() {
        // 120 BPM, grid starting on beat 4 (a one-beat pickup).
        let beats: Vec<(f64, u16)> =
            (0..40).map(|i| (i as f64 * 0.5, ((i + 3) % 4 + 1) as u16)).collect();
        let mut t = track(beats, 120.0);
        assert_eq!(t.bar_and_beat(0.1), (0, 4), "pickup beat");
        assert_eq!(t.bar_and_beat(0.6), (1, 1));
        assert_eq!(t.bar_and_beat(2.6), (2, 1));
        t.memory_cues = vec![8.5];
        assert_eq!(t.beats_to_next_memory(0.5), Some(16));
        assert_eq!(bars_label(16), "4.0");
        assert_eq!(bars_label(9), "2.1");
        assert_eq!(t.beats_to_next_memory(9.0), None);
    }
}
