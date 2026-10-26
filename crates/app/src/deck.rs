//! One deck: the loaded track, its cue points, loops, tempo and transport.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use deckhand_audio::{
    Command, CrossfaderSide, Engine, TrackAudio, band_waveform, decode_file, peak_waveform,
};
use deckhand_control::PadMode;
use deckhand_formats::anlz::{AnlzFile, CueKind, CueListKind};
use deckhand_formats::pdb::LibraryTrack;
use eframe::egui::{self, Color32};

use crate::library::UsbLibrary;
use crate::waveform::WaveformData;

pub const HOT_CUES: usize = 8;
const HOT_CUE_DEFAULT: Color32 = Color32::from_rgb(40, 200, 80);

/// Jog movement while paused, per step.
const JOG_SECONDS_PER_STEP: f64 = 1.0 / 150.0;
/// Speed change per jog step while playing, and how long it lasts.
const NUDGE_PER_STEP: f64 = 0.01;
const NUDGE_HOLD: Duration = Duration::from_millis(80);
/// Audio moved per jog step while scratching: one turn of a 33⅓ rpm record
/// is 1.8 s; assumes 1024 steps per turn.
const SCRATCH_SECONDS_PER_STEP: f64 = 1.8 / 1024.0;
/// Jog search movement per step.
const SEARCH_SECONDS_PER_STEP: f64 = 0.1;
/// Positions this close to the cue point count as "at the cue".
pub const AT_CUE: f64 = 0.02;
/// Beat-jump pads: back/forward pairs.
pub const BEAT_JUMPS: [f64; 8] = [-1.0, 1.0, -2.0, 2.0, -4.0, 4.0, -8.0, 8.0];
/// Beat-loop pad lengths in beats.
pub const BEAT_LOOPS: [f64; 8] = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TempoRange {
    Six,
    Ten,
    Sixteen,
    Wide,
}

impl TempoRange {
    pub const ALL: [TempoRange; 4] =
        [TempoRange::Six, TempoRange::Ten, TempoRange::Sixteen, TempoRange::Wide];

    pub fn fraction(self) -> f64 {
        match self {
            TempoRange::Six => 0.06,
            TempoRange::Ten => 0.10,
            TempoRange::Sixteen => 0.16,
            TempoRange::Wide => 1.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TempoRange::Six => "±6%",
            TempoRange::Ten => "±10%",
            TempoRange::Sixteen => "±16%",
            TempoRange::Wide => "WIDE",
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&r| r == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
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
    /// From deckhand's collection rather than a USB export.
    pub from_collection: bool,
    /// Auto Cue: where the first sound starts, when it was looked for.
    pub first_sound: Option<f64>,
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
}

type LoadResult = Result<(LoadedTrack, Arc<TrackAudio>)>;

pub struct Deck {
    pub index: usize,
    pub track: Option<LoadedTrack>,
    /// The decoded audio, kept to reload it when the audio output restarts.
    audio: Option<Arc<TrackAudio>>,
    loading: Option<(String, Receiver<LoadResult>)>,
    pub error: Option<String>,
    pub cue_point: f64,
    previewing: bool,
    /// −1.0 (slowest) … 1.0 (fastest).
    pub tempo_fader: f32,
    pub tempo_range: TempoRange,
    /// Tempo set by sync, overriding the fader.
    pub sync_ratio: Option<f64>,
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
    pub prefs: DeckPrefs,
    /// Instant Doubles: once loaded, pick up where this other deck plays.
    pub double_of: Option<usize>,
    /// The on-screen SHIFT button is latched.
    pub screen_shift: bool,
    /// Pad FX pads show effects A–H (0) or I–P (1).
    pub pad_fx_page: u8,
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
            error: None,
            cue_point: 0.0,
            previewing: false,
            tempo_fader: 0.0,
            tempo_range: TempoRange::Ten,
            sync_ratio: None,
            sync: false,
            quantize: true,
            vinyl: true,
            key_lock: false,
            loop_in: None,
            loop_region: None,
            loop_active: false,
            jog_touched: false,
            nudge_until: None,
            volume: 1.0,
            trim: 0.5,
            eq: [0.5; 3],
            filter: 0.0,
            side: if index % 2 == 0 { CrossfaderSide::A } else { CrossfaderSide::B },
            headphone_cue: false,
            hot_cue_gate: false,
            jump_size: 4.0,
            jump_range: 1.0,
            gate_held: None,
            slip: false,
            slip_pad: None,
            key_shift: 0,
            key_synced: false,
            pad_mode: PadMode::HotCue,
            prefs: DeckPrefs::default(),
            double_of: None,
            screen_shift: false,
            pad_fx_page: 0,
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

    /// Starts decoding a track and reading its analysis in the background.
    pub fn start_load(&mut self, track: &LibraryTrack, library: &UsbLibrary, ctx: &egui::Context) {
        let request = LoadRequest {
            id: track.track.id,
            from_collection: false,
            title: track.track.title.clone(),
            artist: track.artist.clone().unwrap_or_default(),
            key: track.key.clone(),
            bpm: track.track.bpm(),
            audio_path: library.resolve(&track.track.file_path),
            analysis_path: (!track.track.analyze_path.is_empty())
                .then(|| library.resolve(&track.track.analyze_path)),
            known: None,
            auto_cue_db: self.prefs.auto_cue_db,
        };
        self.start_request(request, ctx);
    }

    /// Loads a track of deckhand's collection.
    pub fn start_load_file(&mut self, track: &deckhand_library::Track, ctx: &egui::Context) {
        let request = LoadRequest {
            id: track.id,
            from_collection: true,
            title: track.title.clone(),
            artist: track.artist.clone(),
            key: track.key.clone(),
            bpm: track.bpm.unwrap_or(0.0),
            audio_path: track.path.clone(),
            analysis_path: None,
            known: Some(Known::from_track(track)),
            auto_cue_db: self.prefs.auto_cue_db,
        };
        self.start_request(request, ctx);
    }

    fn start_request(&mut self, request: LoadRequest, ctx: &egui::Context) {
        let (tx, rx) = channel();
        self.loading = Some((request.title.clone(), rx));
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
                    self.track = Some(track);
                    self.apply_tempo(engine);
                }
                Err(err) => self.error = Some(format!("{err:#}")),
            }
        }
        if self.nudge_until.is_some_and(|until| Instant::now() >= until) {
            self.nudge_until = None;
            engine.send(Command::SetNudge { deck: self.index, amount: 0.0 });
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
        let playing = !engine.is_playing(self.index);
        self.previewing = false;
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
        } else if is_set && (self.hot_cue_gate || force_gate) {
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
    /// relative to the cue point (paused) or the position (playing).
    pub fn memory_call(&mut self, forward: bool, engine: &mut Engine) {
        // During loop playback the CALL buttons halve / double the loop.
        if self.loop_active {
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
                }
            }
            None => {
                let t = if playing { position } else { cue_point };
                if !track.memory_cues.iter().any(|m| (m - t).abs() < AT_CUE) {
                    track.memory_cues.push(t);
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
            track.memory_loops.retain(|l| (l.0 - t).abs() >= AT_CUE);
        }
    }

    /// Jumps to a hot cue and plays, or stores one at the current position.
    pub fn hot_cue(&mut self, slot: usize, engine: &mut Engine) {
        let position = self.quantized(engine.position_secs(self.index));
        let Some(track) = self.track.as_mut() else { return };
        match track.hot_cues[slot] {
            Some(cue) => {
                engine.send(Command::Seek { deck: self.index, seconds: cue.seconds });
                engine.send(Command::SetPlaying { deck: self.index, playing: true });
            }
            None => {
                track.hot_cues[slot] = Some(HotCue { seconds: position, color: HOT_CUE_DEFAULT })
            }
        }
    }

    pub fn clear_hot_cue(&mut self, slot: usize) {
        if let Some(track) = self.track.as_mut() {
            track.hot_cues[slot] = None;
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

    pub fn reloop_exit(&mut self, engine: &mut Engine) {
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
            engine.send(Command::SetNudge {
                deck: self.index,
                amount: f64::from(steps) * NUDGE_PER_STEP,
            });
            self.nudge_until = Some(Instant::now() + NUDGE_HOLD);
        } else {
            engine.send(Command::JogMove {
                deck: self.index,
                seconds: f64::from(steps) * JOG_SECONDS_PER_STEP,
            });
        }
    }

    /// Jog top movement.
    pub fn jog_scratch(&mut self, steps: i32, engine: &mut Engine) {
        if self.is_scratching() {
            engine.send(Command::JogMove {
                deck: self.index,
                seconds: f64::from(steps)
                    * SCRATCH_SECONDS_PER_STEP
                    * self.prefs.scratch_sensitivity,
            });
        } else {
            self.jog(steps, engine);
        }
    }

    /// Touching the jog top in vinyl mode holds the record.
    pub fn jog_touch(&mut self, touched: bool, engine: &mut Engine) {
        self.jog_touched = touched;
        if self.vinyl {
            engine.send(Command::Hold { deck: self.index, held: touched });
        }
    }

    // Tempo

    pub fn set_tempo_fader(&mut self, value: f32, engine: &mut Engine) {
        self.tempo_fader = value.clamp(-1.0, 1.0);
        if !self.sync {
            self.apply_tempo(engine);
        }
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
        let first = track.beats.first().map_or(0.0, |b| b.0);
        let beat = 60.0 / bpm;
        let start = first - (first / beat).floor() * beat;
        track.beats = (0..)
            .map(|i| (start + i as f64 * beat, (i % 4) as u16 + 1))
            .take_while(|b| b.0 < track.duration)
            .collect();
        track.bpm = bpm;
        Some(bpm)
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
        if !self.sync {
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
        engine.send(Command::Seek { deck, seconds: position });
        if let (true, Some((start, end))) = (self.loop_active, self.loop_region) {
            engine.send(Command::SetLoop { deck, start, end });
        }
        engine.send(Command::SetPlaying { deck, playing });
    }

    /// Sends all mixer settings of this channel.
    pub fn apply_mixer(&self, engine: &mut Engine) {
        let deck = self.index;
        engine.send(Command::SetVolume { deck, level: self.volume });
        // Trim knob: 0.5 = 0 dB, range ±12 dB.
        let trim_db = (self.trim - 0.5) * 24.0;
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
    pub start_at_first_cue: bool,
    /// Auto Cue level in dB, when Auto Cue is on for the deck.
    pub auto_cue_db: Option<f32>,
    pub isolator: bool,
    pub scratch_sensitivity: f64,
    /// Vinyl Speed Adjust, seconds.
    pub vinyl_start: f64,
    pub vinyl_brake: f64,
}

impl Default for DeckPrefs {
    fn default() -> Self {
        Self {
            memory_call_lock: true,
            start_at_first_cue: true,
            auto_cue_db: None,
            isolator: true,
            scratch_sensitivity: 1.0,
            vinyl_start: 0.0,
            vinyl_brake: 0.0,
        }
    }
}

/// When the first sound louder than `db` starts, in seconds.
fn first_sound(audio: &TrackAudio, db: f32) -> Option<f64> {
    let threshold = 10f32.powf(db / 20.0);
    let index = audio.samples.iter().position(|s| s.abs() > threshold)?;
    Some((index / 2) as f64 / f64::from(audio.sample_rate))
}

/// Grid and cues the collection already has (from a rekordbox import).
struct Known {
    beats: Vec<(f64, u16)>,
    hot_cues: [Option<HotCue>; HOT_CUES],
    memory_cues: Vec<f64>,
    memory_loops: Vec<(f64, f64)>,
}

impl Known {
    fn from_track(track: &deckhand_library::Track) -> Self {
        let mut known = Known {
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
    title: String,
    artist: String,
    key: Option<String>,
    bpm: f64,
    audio_path: PathBuf,
    analysis_path: Option<PathBuf>,
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
        let mut waveform = WaveformData::from_analysis(ext.as_ref(), two_ex.as_ref())
            .unwrap_or_else(|| WaveformData::from_peaks(&peak_waveform(&audio), Vec::new()));
        if !waveform.has_bands() {
            waveform.bands = band_waveform(&audio);
        }
        waveform.color_from_bands();
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
        if let Some(known) = self.known {
            if beats.is_empty() {
                beats = known.beats;
            }
            if hot_cues.iter().all(Option::is_none)
                && memory_cues.is_empty()
                && memory_loops.is_empty()
            {
                (hot_cues, memory_cues, memory_loops) =
                    (known.hot_cues, known.memory_cues, known.memory_loops);
            }
        }

        let track = LoadedTrack {
            id: self.id,
            from_collection: self.from_collection,
            first_sound: self.auto_cue_db.and_then(|db| first_sound(&audio, db)),
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
        };
        Ok((track, Arc::new(audio)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(beats: Vec<(f64, u16)>, bpm: f64) -> LoadedTrack {
        LoadedTrack {
            id: 1,
            from_collection: false,
            first_sound: None,
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

    #[test]
    fn tempo_ranges_cycle() {
        assert_eq!(TempoRange::Wide.next(), TempoRange::Six);
        assert_eq!(TempoRange::Six.next(), TempoRange::Ten);
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
        // Am → Bm: up two; Am → C (its relative major): no change.
        assert_eq!(key_sync_shift((9, true), (11, true)), 2);
        assert_eq!(key_sync_shift((9, true), (0, false)), 0);
        // Never more than a tritone away.
        assert_eq!(key_sync_shift((0, false), (11, false)), -1);
    }
}
