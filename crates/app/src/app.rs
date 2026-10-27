//! The main window.

mod collection_view;
mod deck_view;
mod hotplug;
mod recording;
mod session;
mod status_bar;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use deckhand_audio::{
    AudioOptions, BeatFxKind, ColorFxKind, Command, CrossfaderSide, DECKS, Engine, FxTarget,
};
use deckhand_control::rekordbox;
use deckhand_control::{
    Action, ActionKind, ControlEvent, DeckAction, DeckFeedback, Feedback, GlobalAction,
    GlobalFeedback, PadMode, Value, input_ports,
};
use eframe::egui::{
    self, Align, Color32, Layout, RichText, ScrollArea, Sense, Slider, Stroke, Ui, vec2,
};

use crate::browser::{Row, TableOptions, sort_rows, track_table};
use crate::config::{AudioQuality, Config};
use crate::controller::Controller;
use crate::deck::{AT_CUE, BEAT_JUMPS, Deck, DeckPrefs, HOT_CUES, key_sync_shift};
use crate::effects::{Effects, pad_fx_label};
use crate::library::{UsbLibrary, find_exports};
use crate::prefs_view::{PrefsTab, deck_prefs, mixer_prefs};
use collection_view::CollectionView;
use deckhand_library::Collection;

/// Which library the browser shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// deckhand's own collection.
    Collection,
    /// The opened rekordbox USB export.
    Usb,
}
use crate::waveform::{HotCueMark, Markers, WaveformStyle, draw_detail, overview_time};
use crate::widgets::{crossfader, fader, knob, meter};

const DECK_COLORS: [Color32; DECKS] = [
    Color32::from_rgb(60, 140, 255),
    Color32::from_rgb(255, 150, 40),
    Color32::from_rgb(80, 210, 160),
    Color32::from_rgb(230, 90, 200),
];
const CUE_COLOR: Color32 = Color32::from_rgb(255, 170, 0);
const PLAY_COLOR: Color32 = Color32::from_rgb(40, 190, 90);
const ACTIVE: Color32 = Color32::from_rgb(60, 120, 220);
const WARN: Color32 = Color32::from_rgb(255, 120, 90);
/// How often synced decks are checked for phase drift.
const DRIFT_CHECK: Duration = Duration::from_millis(500);
/// Phase error (in beats) that sync corrects.
const DRIFT_LIMIT: f64 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Playlists,
    Tracks,
}

pub struct DeckhandApp {
    config: Config,
    engine: Option<Engine>,
    engine_error: Option<String>,
    /// Jog steps per turn the engine was last told (None: not since it started).
    jog_resolution: Option<u32>,
    decks: Vec<Deck>,
    master: Option<usize>,
    last_drift_check: Instant,
    crossfader: f32,
    master_volume: f32,
    headphone_mix: f32,
    headphone_volume: f32,
    library: Option<UsbLibrary>,
    library_error: Option<String>,
    exports: Vec<PathBuf>,
    source: Source,
    collection: Collection,
    collection_view: CollectionView,
    playlist: Option<usize>,
    search: String,
    selected: Option<u32>,
    focus: Focus,
    controller: Controller,
    show_controller: bool,
    show_prefs: bool,
    prefs_tab: PrefsTab,
    effects: Effects,
    status_bar: status_bar::StatusBar,
    /// Deck elements can be dragged around.
    editing_layout: bool,
    moved_window: bool,
    hotplug: hotplug::Hotplug,
    /// Each deck's jog bend at the last frame, to notice it ending.
    was_bending: [bool; DECKS],
    /// USB export folders closed in the sidebar (by row).
    usb_collapsed: std::collections::HashSet<usize>,
    /// F12: window and screen details for troubleshooting.
    show_diagnostics: bool,
    /// The master being recorded.
    recording: Option<deckhand_audio::Recording>,
    /// The deck each side of the controller drives (left 1/3, right 2/4).
    active_decks: [usize; 2],
    /// Position of the selected row in the list, telling duplicate tracks
    /// apart (`None`: the first copy).
    selected_position: Option<usize>,
    /// The playlist sidebar scrolls to the selected playlist (browse knob).
    reveal_playlist: bool,
    /// The list scrolls to the selected track on the next frame (set when the
    /// controller's browse knob moves the selection).
    reveal_selected: bool,
    /// When the remembered settings were last compared and saved.
    session_checked: Instant,
    /// For flashing LEDs.
    started: Instant,
    /// Sample rates and buffer range of the device shown in the Audio window.
    audio_caps: Option<AudioCaps>,
    action_filter: String,
    import_choice: Option<usize>,
    cue_held: [bool; DECKS],
    /// Hot cue pads held with the mouse, for gate mode.
    pad_held: [[Option<DeckAction>; HOT_CUES]; DECKS],
    seconds_visible: f64,
    status: Option<String>,
    screenshot: Option<Screenshot>,
}

/// Developer aid: `DECKHAND_SCREENSHOT=out.png` loads tracks, saves an image
/// of the window once they are ready, and quits.
struct Screenshot {
    path: PathBuf,
    requested: bool,
    /// Earliest time to take it (`DECKHAND_SCREENSHOT_DELAY`, seconds).
    not_before: Instant,
}

impl DeckhandApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::widgets::apply_theme(&cc.egui_ctx);
        let mut config = Config::load();
        if let Some(root) = std::env::var_os("DECKHAND_LIBRARY") {
            config.library_root = Some(root.into());
        }
        if std::env::var_os("DECKHAND_FOUR_DECKS").is_some() {
            config.four_decks = true;
        }
        if std::env::var_os("DECKHAND_VERTICAL").is_some() {
            config.vertical_waveforms = true;
        }
        if std::env::var_os("DECKHAND_HIDE_MIXER").is_some() {
            config.show_mixer = false;
        }
        let pad_mode = std::env::var("DECKHAND_PAD_MODE").ok().and_then(|name| {
            PadMode::ALL
                .into_iter()
                .find(|m| m.label().eq_ignore_ascii_case(&name.replace('_', " ")))
        });
        if std::env::var_os("DECKHAND_THREE_BAND").is_some() {
            config.waveform_style = WaveformStyle::ThreeBand;
        }
        let mut app = Self {
            engine: None,
            engine_error: None,
            jog_resolution: None,
            decks: (0..DECKS).map(Deck::new).collect(),
            master: None,
            last_drift_check: Instant::now(),
            crossfader: 0.0,
            master_volume: 0.8,
            headphone_mix: 0.0,
            headphone_volume: 0.8,
            library: None,
            library_error: None,
            exports: find_exports(),
            source: Source::Usb,
            collection: Collection::default(),
            collection_view: CollectionView::default(),
            playlist: None,
            search: String::new(),
            selected: None,
            focus: Focus::Tracks,
            controller: Controller::default(),
            show_controller: std::env::var_os("DECKHAND_SHOW_CONTROLLER").is_some(),
            show_prefs: std::env::var_os("DECKHAND_SHOW_AUDIO").is_some()
                || std::env::var_os("DECKHAND_SHOW_PREFS").is_some(),
            prefs_tab: if std::env::var_os("DECKHAND_SHOW_MIXER_PREFS").is_some() {
                PrefsTab::Mixer
            } else if std::env::var_os("DECKHAND_SHOW_AUDIO").is_some() {
                PrefsTab::Audio
            } else {
                PrefsTab::default()
            },
            effects: Effects::default(),
            status_bar: status_bar::StatusBar::default(),
            editing_layout: std::env::var_os("DECKHAND_EDIT_LAYOUT").is_some(),
            reveal_selected: false,
            reveal_playlist: false,
            selected_position: None,
            active_decks: [0, 1],
            recording: None,
            moved_window: false,
            usb_collapsed: Default::default(),
            hotplug: Default::default(),
            was_bending: [false; DECKS],
            show_diagnostics: std::env::var_os("DECKHAND_DIAGNOSTICS").is_some(),
            session_checked: Instant::now(),
            started: Instant::now(),
            audio_caps: None,
            action_filter: String::new(),
            import_choice: None,
            cue_held: [false; DECKS],
            pad_held: [[None; HOT_CUES]; DECKS],
            seconds_visible: 8.0,
            status: None,
            screenshot: std::env::var_os("DECKHAND_SCREENSHOT").map(|p| {
                let delay = std::env::var("DECKHAND_SCREENSHOT_DELAY")
                    .ok()
                    .and_then(|d| d.parse().ok())
                    .unwrap_or(0);
                Screenshot {
                    path: p.into(),
                    requested: false,
                    not_before: Instant::now() + Duration::from_secs(delay),
                }
            }),
            config,
        };
        app.restore_session();
        // Test aid: record into this folder from the start.
        let record_to = std::env::var_os("DECKHAND_RECORD").map(PathBuf::from);
        if let Some(dir) = &record_to {
            app.config.recordings_dir = Some(dir.clone());
        }
        app.effects.load_samples(&crate::config::samples_dir());
        if let Some(mode) = pad_mode {
            for deck in &mut app.decks {
                deck.set_pad_mode(mode);
            }
        }
        app.start_engine(app.config.audio_device.clone());
        match Collection::open(&deckhand_library::collection_path()) {
            Ok(collection) => app.collection = collection,
            Err(err) => app.collection_view.error = Some(format!("{err:#}")),
        }
        let prefer_collection = app.config.use_collection;
        let root = app
            .config
            .library_root
            .clone()
            .filter(|r| r.join("PIONEER").is_dir())
            .or_else(|| app.exports.first().cloned());
        if let Some(root) = root {
            app.open_library(root);
        }
        // Start in the collection when it has music, or when there is no export.
        if !app.collection.tracks.is_empty() && prefer_collection || app.library.is_none() {
            app.set_source(Source::Collection);
        }
        let only = std::env::var("DECKHAND_PORT_FILTER").ok();
        let ports: Vec<String> = input_ports()
            .into_iter()
            .filter(|p| only.as_ref().is_none_or(|f| p.contains(f.as_str())))
            .collect();
        // Tests can force a port (for example a virtual test device).
        let forced = std::env::var("DECKHAND_MIDI_PORT").ok();
        let port = forced
            .clone()
            .or_else(|| app.config.midi_port.clone().filter(|p| ports.contains(p)))
            .or_else(|| {
                ports
                    .iter()
                    .find(|p| {
                        rekordbox::find_for_port_with(p, &[crate::config::extra_mapping_dir()])
                            .is_some()
                            || ["GRV", "DDJ", "AlphaTheta", "XDJ"].iter().any(|k| p.contains(k))
                    })
                    .cloned()
            });
        app.note_ports(ports);
        if let Some(port) = port {
            if forced.is_some() {
                app.controller.open(&port, &cc.egui_ctx);
            } else {
                app.connect_controller(&port, &cc.egui_ctx);
            }
        }
        if record_to.is_some() {
            app.toggle_recording();
        }
        if app.screenshot.is_some() {
            let ids = app.visible_track_ids();
            let decks = if app.config.four_decks { DECKS } else { 2 };
            for (deck, id) in ids.into_iter().cycle().take(decks).enumerate() {
                app.selected = Some(id);
                app.load_selected(deck, &cc.egui_ctx);
            }
        }
        app
    }

    fn deck_count(&self) -> usize {
        if self.config.four_decks { DECKS } else { 2 }
    }

    /// (Re)starts the audio output with the configured options; loaded
    /// tracks carry on where they were.
    fn start_engine(&mut self, device: Option<String>) {
        self.finish_recording();
        let state: Vec<(f64, bool)> = (0..DECKS)
            .map(|d| {
                self.engine.as_ref().map_or((0.0, false), |e| (e.position_secs(d), e.is_playing(d)))
            })
            .collect();
        self.engine = None;
        let mut options = AudioOptions {
            device: device.clone(),
            sample_rate: self.config.sample_rate,
            buffer_frames: self.config.buffer_frames,
            quality: self.config.audio_quality.engine(),
        };
        let mut result = Engine::start(&options);
        if result.is_err() && (options.sample_rate.is_some() || options.buffer_frames.is_some()) {
            // The device may not take the chosen rate or buffer size.
            (options.sample_rate, options.buffer_frames) = (None, None);
            if let Ok(engine) = Engine::start(&options) {
                self.status = Some(
                    "The sample rate or buffer size is not supported; using the device's defaults"
                        .into(),
                );
                (self.config.sample_rate, self.config.buffer_frames) = (None, None);
                result = Ok(engine);
            }
        }
        match result {
            Ok(mut engine) => {
                for (deck, &(position, playing)) in self.decks.iter_mut().zip(&state) {
                    deck.apply_mixer(&mut engine);
                    deck.apply_tempo(&mut engine);
                    deck.restore(&mut engine, position, playing);
                }
                self.effects.apply_all(&mut engine);
                self.controller.set_jog_sender(engine.take_jog_sender());
                self.jog_resolution = None;
                self.engine = Some(engine);
                self.apply_preferences();
                let Some(mut engine) = self.engine.take() else { return };
                engine.send(Command::SetCrossfader(self.crossfader));
                engine.send(Command::SetMasterVolume(self.master_volume));
                engine.send(Command::SetHeadphoneMix(self.headphone_mix));
                engine.send(Command::SetHeadphoneVolume(self.headphone_volume));
                self.engine = Some(engine);
                self.engine_error = None;
                self.config.audio_device = device;
                self.config.save();
            }
            Err(err) => self.engine_error = Some(format!("{err:#}")),
        }
    }

    fn open_library(&mut self, root: PathBuf) {
        match UsbLibrary::open(&root) {
            Ok(library) => {
                self.library = Some(library);
                self.library_error = None;
                self.playlist = None;
                self.selected = None;
                self.config.library_root = Some(root);
                self.set_source(Source::Usb);
            }
            Err(err) => self.library_error = Some(format!("{err:#}")),
        }
    }

    fn set_source(&mut self, source: Source) {
        if self.source != source {
            self.selected = None;
        }
        self.source = source;
        self.config.use_collection = source == Source::Collection;
        self.config.save();
    }

    fn load_selected(&mut self, deck: usize, ctx: &egui::Context) {
        if self.source == Source::Collection {
            if let Some(id) = self.selected {
                self.load_from_collection(id, deck, ctx);
            }
            return;
        }
        if self.load_blocked(deck) {
            return;
        }
        let Some(id) = self.selected else { return };
        self.mark_double(deck, id, false);
        let Some(library) = &self.library else { return };
        let Some(track) = library.library.track(id) else { return };
        self.decks[deck].start_load(track, library, ctx);
    }

    /// Eject/Load Lock: a playing deck keeps its track.
    fn load_blocked(&mut self, deck: usize) -> bool {
        let playing = self.engine.as_ref().is_some_and(|e| e.is_playing(deck));
        if playing && self.config.prefs.load_lock {
            self.status = Some(format!(
                "Deck {} is playing — pause it before loading (or turn off Load Lock in Preferences)",
                deck + 1
            ));
        }
        playing && self.config.prefs.load_lock
    }

    /// Instant Doubles: loading a track that plays on another deck makes
    /// this deck pick it up there once loaded.
    fn mark_double(&mut self, deck: usize, id: u32, from_collection: bool) {
        self.decks[deck].double_of = None;
        if !self.config.prefs.instant_doubles {
            return;
        }
        let Some(engine) = &self.engine else { return };
        self.decks[deck].double_of = (0..self.deck_count()).find(|&d| {
            d != deck
                && engine.is_playing(d)
                && self.decks[d]
                    .track
                    .as_ref()
                    .is_some_and(|t| t.id == id && t.from_collection == from_collection)
        });
    }

    /// Finishes Instant Doubles for decks whose track just loaded.
    fn finish_doubles(&mut self) {
        let Some(engine) = self.engine.as_mut() else { return };
        for deck in 0..DECKS {
            let Some(source) = self.decks[deck].double_of else { continue };
            if self.decks[deck].loading_title().is_some() {
                continue;
            }
            self.decks[deck].double_of = None;
            let (fader, range, key_lock) = {
                let s = &self.decks[source];
                (s.tempo_fader, s.tempo_range, s.key_lock)
            };
            let d = &mut self.decks[deck];
            d.tempo_range = range;
            d.set_tempo_fader(fader, engine);
            if d.key_lock != key_lock {
                d.toggle_key_lock(engine);
            }
            engine.send(Command::Seek { deck, seconds: engine.position_secs(source) });
            engine.send(Command::SetPlaying { deck, playing: engine.is_playing(source) });
        }
    }

    /// Hands the Preferences to the decks and the engine.
    /// Tells the decks and the engine how many steps the connected
    /// controller's jog sends per turn, so a turn moves the same distance on
    /// every controller.
    fn sync_jog_resolution(&mut self) {
        let steps = self.controller.mapping().jog_steps_per_turn();
        for deck in &mut self.decks {
            deck.jog_steps_per_turn = f64::from(steps);
        }
        if let Some(engine) = self.engine.as_mut()
            && self.jog_resolution != Some(steps)
        {
            engine.send(Command::SetJogResolution(f64::from(steps)));
            self.jog_resolution = Some(steps);
        }
    }

    fn apply_preferences(&mut self) {
        let p = self.config.prefs.clone();
        for (index, deck) in self.decks.iter_mut().enumerate() {
            deck.prefs = DeckPrefs {
                memory_call_lock: p.memory_call_lock,
                call_resizes_loop: p.call_resizes_loop,
                gate_while_playing: p.gate_while_playing,
                start_at_first_cue: p.start_at_first_cue,
                auto_cue_db: p.auto_cue[index].then_some(p.auto_cue_level_db),
                isolator: p.isolator,
                scratch_sensitivity: f64::from(p.scratch_sensitivity),
                bend_sensitivity: f64::from(p.bend_sensitivity),
                vinyl_start: f64::from(p.vinyl_start),
                vinyl_brake: f64::from(p.vinyl_brake),
            };
        }
        let Some(engine) = self.engine.as_mut() else { return };
        for deck in &self.decks {
            deck.apply_mixer(engine);
        }
        engine.send(Command::SetEqSettings(p.eq_settings()));
        engine.send(Command::SetBendSensitivity(f64::from(p.bend_sensitivity)));
        engine.send(Command::SetCrossfaderCurve(p.crossfader_curve.engine()));
        engine.send(Command::SetFaderCurve(p.fader_curve.engine()));
        engine.send(Command::SetLimiter(p.limiter));
    }

    /// Track ids in the order the list shows them.
    fn visible_track_ids(&self) -> Vec<u32> {
        self.visible_rows().into_iter().map(|(id, _)| id).collect()
    }

    /// (track, position in the list) in the order the list shows them; the
    /// position tells duplicates apart.
    fn visible_rows(&self) -> Vec<(u32, usize)> {
        let rows = match self.source {
            Source::Collection => self.collection_rows(),
            Source::Usb => self.usb_rows(),
        };
        rows.into_iter().map(|r| (r.id, r.position)).collect()
    }

    // Sync

    /// The deck others follow: the chosen master if loaded, otherwise a
    /// playing deck that is not itself synced.
    fn master_deck(&self) -> Option<usize> {
        let engine = self.engine.as_ref()?;
        self.master.filter(|&m| self.decks[m].track.is_some()).or_else(|| {
            (0..self.deck_count()).find(|&d| {
                self.decks[d].track.is_some() && !self.decks[d].sync && engine.is_playing(d)
            })
        })
    }

    fn toggle_sync(&mut self, deck: usize) {
        let Some(engine) = self.engine.as_mut() else { return };
        if self.decks[deck].sync {
            self.decks[deck].sync_off(engine);
            return;
        }
        if self.decks[deck].track.is_none() {
            return;
        }
        match self.master_deck().filter(|&m| m != deck) {
            Some(master) => {
                self.master.get_or_insert(master);
                self.decks[deck].sync = true;
                (self.decks[deck].phase_offset, self.decks[deck].rephase) = (0.0, false);
                self.follow_master(deck, true);
            }
            // Like rekordbox: with no other deck to follow, SYNC still turns
            // on and this deck becomes the master the others follow.
            None => {
                self.decks[deck].sync = true;
                self.master.get_or_insert(deck);
            }
        }
    }

    /// Matches a synced deck's tempo to the master and, if `align`, its beat phase.
    fn follow_master(&mut self, deck: usize, align: bool) {
        let Some(master) = self.master_deck().filter(|&m| m != deck) else { return };
        let Some(engine) = self.engine.as_mut() else { return };
        let (Some(master_bpm), Some(master_track)) =
            (self.decks[master].current_bpm(), self.decks[master].track.as_ref())
        else {
            return;
        };
        let master_beat = master_track.beat_at(engine.position_secs(master));
        let target = &mut self.decks[deck];
        let Some(track) = target.track.as_ref() else { return };
        if track.bpm <= 0.0 {
            return;
        }
        let ratio = master_bpm / track.bpm;
        if target.sync_ratio.is_none_or(|r| (r - ratio).abs() > 1e-6) {
            target.sync_ratio = Some(ratio);
            target.apply_tempo(engine);
        }
        // While the DJ nudges, leave the phase alone; afterwards keep the
        // offset they chose (the grid can be off, so they get the last word).
        if align && engine.is_playing(master) && !target.nudging() && !engine.is_bending(deck) {
            let beat = track.beat_at(engine.position_secs(deck));
            let mut phase = beat.fract() - master_beat.fract();
            phase -= phase.round();
            if std::mem::take(&mut target.rephase) {
                target.phase_offset = phase;
                return;
            }
            let mut error = phase - target.phase_offset;
            error -= error.round();
            if error.abs() > DRIFT_LIMIT {
                let seconds = track.time_of_beat(beat - error) - track.time_of_beat(beat);
                engine.send(Command::Shift { deck, seconds });
            }
        }
    }

    fn update_sync(&mut self) {
        // A jog bend (handled in the audio engine) that just ended leaves
        // the phase where the DJ put it.
        if let Some(engine) = &self.engine {
            for deck in 0..DECKS {
                let bending = engine.is_bending(deck);
                if self.was_bending[deck] && !bending {
                    self.decks[deck].rephase = true;
                }
                self.was_bending[deck] = bending;
            }
        }
        let check_drift = self.last_drift_check.elapsed() >= DRIFT_CHECK;
        if check_drift {
            self.last_drift_check = Instant::now();
        }
        for deck in 0..DECKS {
            if self.decks[deck].sync {
                let playing = self.engine.as_ref().is_some_and(|e| e.is_playing(deck));
                self.follow_master(deck, check_drift && playing);
            }
        }
    }

    fn play_pause(&mut self, deck: usize) {
        let Some(engine) = self.engine.as_mut() else { return };
        let was_playing = engine.is_playing(deck);
        self.decks[deck].toggle_play(engine);
        if !was_playing && self.decks[deck].sync {
            self.follow_master(deck, true);
        }
    }

    // Controller

    fn apply_event(&mut self, event: ControlEvent, ctx: &egui::Context) {
        match event.action {
            Action::Deck(d, action) if d < DECKS => {
                self.apply_deck_event(d, action, event.value, ctx)
            }
            Action::Deck(..) => {}
            Action::Global(action) => self.apply_global_event(action, event.value),
        }
    }

    fn apply_deck_event(
        &mut self,
        d: usize,
        action: DeckAction,
        value: Value,
        ctx: &egui::Context,
    ) {
        use DeckAction as A;
        let pressed = matches!(value, Value::Pressed(true));
        // The deck a control layer is on becomes that side's active deck;
        // mixer channel controls do not switch layers.
        let mixer = matches!(
            action,
            A::Volume
                | A::Trim
                | A::EqLow
                | A::EqMid
                | A::EqHigh
                | A::Filter
                | A::HeadphoneCue
                | A::CrossfaderAssignA
                | A::CrossfaderAssignThru
                | A::CrossfaderAssignB
                | A::FaderStartPlay
                | A::FaderStartCue
        );
        if !mixer && (action != A::Select || pressed) {
            self.active_decks[d % 2] = d;
        }
        if action == A::Select {
            return;
        }
        match (action, value) {
            (A::PlayPause, _) if pressed => return self.play_pause(d),
            (A::Sync, _) if pressed => return self.toggle_sync(d),
            (A::SyncMaster, _) if pressed => {
                self.master = if self.master == Some(d) { None } else { Some(d) };
                return;
            }
            (A::Load, _) if pressed => return self.load_selected(d, ctx),
            (A::KeySync, _) if pressed => return self.key_sync(d),
            (A::TapBpm, _) if pressed => {
                if let Some(bpm) = self.decks[d].tap_bpm() {
                    self.status = Some(format!("Deck {}: BPM set to {bpm:.2} by tapping", d + 1));
                }
                return;
            }
            (A::PadMode(mode), _) if pressed => return self.decks[d].set_pad_mode(mode),
            (A::SamplerStop(n), _) if pressed => return self.sampler_shift_pad(d, n),
            (A::SamplerBankDown, _) if pressed => return self.effects.change_bank(-1),
            (A::SamplerBankUp, _) if pressed => return self.effects.change_bank(1),
            (A::PadFx(slot), Value::Pressed(true)) => {
                let slot = (slot + 8 * self.decks[d].pad_fx_page) % 16;
                self.effects.pad = Some((d, slot));
                return;
            }
            (A::PadFx(_), Value::Pressed(false)) => {
                if self.effects.pad.is_some_and(|(deck, _)| deck == d) {
                    self.effects.pad = None;
                }
                return;
            }
            _ => {}
        }
        let Some(engine) = self.engine.as_mut() else { return };
        let deck = &mut self.decks[d];
        match (action, value) {
            (A::Cue, Value::Pressed(true)) => deck.cue_pressed(engine),
            (A::Cue, Value::Pressed(false)) => deck.cue_released(engine),
            (A::JumpToStart, _) if pressed => deck.jump_to_start(engine),
            (A::HotCue(n), Value::Pressed(true)) => {
                deck.hot_cue_pressed(usize::from(n), false, engine)
            }
            (A::HotCue(n) | A::HotCueGate(n), Value::Pressed(false)) => {
                deck.hot_cue_released(usize::from(n), engine);
            }
            (A::HotCueGate(n), Value::Pressed(true)) => {
                deck.hot_cue_pressed(usize::from(n), true, engine)
            }
            (A::HotCueGateMode, _) if pressed => deck.hot_cue_gate = !deck.hot_cue_gate,
            (A::HotCueDelete(n), _) if pressed => deck.clear_hot_cue(usize::from(n)),
            (A::MemoryPrev, _) if pressed => deck.memory_call(false, engine),
            (A::MemoryNext, _) if pressed => deck.memory_call(true, engine),
            (A::MemorySet, _) if pressed => deck.memory_set(engine),
            (A::MemoryDelete, _) if pressed => deck.memory_delete(),
            (A::LoopMoveBack, _) if pressed => deck.move_loop(-1.0, engine),
            (A::LoopMoveForward, _) if pressed => deck.move_loop(1.0, engine),
            (A::JogSearch, Value::Relative(steps)) => deck.jog_search(steps, engine),
            (A::CrossfaderAssignA | A::CrossfaderAssignThru | A::CrossfaderAssignB, _)
                if pressed =>
            {
                deck.side = match action {
                    A::CrossfaderAssignA => CrossfaderSide::A,
                    A::CrossfaderAssignB => CrossfaderSide::B,
                    _ => CrossfaderSide::Thru,
                };
                deck.apply_mixer(engine);
            }
            (A::TempoFader, Value::Absolute(v)) => deck.set_tempo_fader(v * 2.0 - 1.0, engine),
            (A::TempoRangeCycle, _) if pressed => {
                let next = deck.tempo_range.next();
                deck.set_tempo_range(next, engine);
            }
            (A::Quantize, _) if pressed => deck.quantize = !deck.quantize,
            (A::Vinyl, _) if pressed => deck.vinyl = !deck.vinyl,
            (A::KeyLock, _) if pressed => deck.toggle_key_lock(engine),
            (A::JogTouch, Value::Pressed(touched)) => deck.jog_touch(touched, engine),
            (A::JogTurn, Value::Relative(steps)) => deck.jog(steps, engine),
            (A::JogScratch, Value::Relative(steps)) => deck.jog_scratch(steps, engine),
            (A::LoopIn, _) if pressed => deck.loop_in(engine),
            (A::LoopOut, _) if pressed => deck.loop_out(engine),
            (A::ReloopExit, _) if pressed => deck.reloop_exit(engine),
            (A::AutoLoop, _) if pressed => deck.beat_loop(4.0, engine),
            (A::LoopHalf, _) if pressed => deck.call_during_loop(false, engine),
            (A::LoopDouble, _) if pressed => deck.call_during_loop(true, engine),
            (A::LoopResizeHalf, _) if pressed => deck.resize_loop(0.5, engine),
            (A::LoopResizeDouble, _) if pressed => deck.resize_loop(2.0, engine),
            (A::BeatJump(n), _) if pressed => deck.pad_beat_jump(usize::from(n), engine),
            (A::BeatJumpBack, _) if pressed => deck.beat_jump(-deck.jump_size, engine),
            (A::BeatJumpForward, _) if pressed => deck.beat_jump(deck.jump_size, engine),
            (A::BeatJumpRangeHalf, _) if pressed => deck.change_jump_range(0.5),
            (A::BeatJumpRangeDouble, _) if pressed => deck.change_jump_range(2.0),
            (A::BeatLoop(n), _) if pressed => {
                deck.beat_loop(deck.pad_loop_beats(usize::from(n)), engine)
            }
            (A::Volume, Value::Absolute(v)) => {
                deck.volume = v;
                deck.apply_mixer(engine);
            }
            (A::Trim, Value::Absolute(v)) => {
                deck.trim = v;
                deck.apply_mixer(engine);
            }
            (A::EqLow | A::EqMid | A::EqHigh, Value::Absolute(v)) => {
                let band = match action {
                    A::EqLow => 0,
                    A::EqMid => 1,
                    _ => 2,
                };
                deck.eq[band] = v;
                deck.apply_mixer(engine);
            }
            (A::Filter, Value::Absolute(v)) => {
                let position = v * 2.0 - 1.0;
                deck.filter = if position.abs() < 0.03 { 0.0 } else { position };
                deck.apply_mixer(engine);
            }
            (A::HeadphoneCue, _) if pressed => {
                deck.headphone_cue = !deck.headphone_cue;
                deck.apply_mixer(engine);
            }
            (A::Slip, _) if pressed => deck.toggle_slip(engine),
            (A::KeyReset, _) if pressed => deck.set_key_shift(0, engine),
            (A::KeyUp, _) if pressed => deck.set_key_shift(deck.key_shift + 1, engine),
            (A::KeyDown, _) if pressed => deck.set_key_shift(deck.key_shift - 1, engine),
            (A::KeyShift(n), _) if pressed => deck.key_shift_pad(usize::from(n), engine),
            (A::BeatJumpRangeSixteenth, _) if pressed => deck.change_jump_range(1.0 / 16.0),
            (A::BeatJumpRangeSixteen, _) if pressed => deck.change_jump_range(16.0),
            (A::BeatLoopSizesHalf, _) if pressed => deck.change_loop_scale(0.5),
            (A::BeatLoopSizesDouble, _) if pressed => deck.change_loop_scale(2.0),
            (A::PadFxPageDown | A::PadFxPageUp, _) if pressed => deck.pad_fx_page ^= 1,
            (A::KeyShiftPageDown, _) if pressed => deck.key_shift_page(-1),
            (A::KeyShiftPageUp, _) if pressed => deck.key_shift_page(1),
            (A::Keyboard(n), _) if pressed => deck.keyboard_pad(usize::from(n), engine),
            (A::KeyboardDown, _) if pressed => deck.keyboard_page(-1),
            (A::KeyboardUp, _) if pressed => deck.keyboard_page(1),
            (A::ActiveLoop, _) if pressed => deck.toggle_active_loop(engine),
            (A::FaderStartPlay, _) if pressed => deck.fader_start(true, engine),
            (A::FaderStartCue, _) if pressed => deck.fader_start(false, engine),
            (A::SamplerPlay(n), _) if pressed => {
                engine.send(Command::SamplerPlay { slot: self.effects.sampler_slot(d, n) });
            }
            _ => {}
        }
    }

    /// Shift + sampler pad: stops the slot while it plays, otherwise loads the
    /// selected track into it.
    fn sampler_shift_pad(&mut self, d: usize, pad: u8) {
        let slot = self.effects.sampler_slot(d, pad);
        let Some(engine) = self.engine.as_mut() else { return };
        if engine.sampler_playing(slot) {
            engine.send(Command::SamplerStop { slot });
            return;
        }
        let Some(id) = self.selected else { return };
        let (path, title) = match self.source {
            Source::Collection => {
                let Some(track) = self.collection.track(id) else { return };
                (track.path.clone(), track.title.clone())
            }
            Source::Usb => {
                let Some(library) = &self.library else { return };
                let Some(track) = library.library.track(id) else { return };
                (library.resolve(&track.track.file_path), track.track.title.clone())
            }
        };
        self.effects.load_into(slot, path);
        self.status = Some(format!("Loading {title} into sampler slot {}", slot + 1));
    }

    /// The BPM Beat FX follow: the master deck's, else the first playing deck's.
    fn fx_bpm(&self) -> f64 {
        let playing = |d: usize| self.engine.as_ref().is_some_and(|e| e.is_playing(d));
        self.master
            .into_iter()
            .chain((0..DECKS).filter(|&d| playing(d)))
            .find_map(|d| self.decks[d].current_bpm())
            .filter(|bpm| *bpm > 20.0)
            .unwrap_or(120.0)
    }

    /// Shifts the deck's key to match the master deck (or another loaded deck).
    fn key_sync(&mut self, d: usize) {
        let Some(engine) = self.engine.as_mut() else { return };
        if self.decks[d].key_synced {
            self.decks[d].set_key_shift(0, engine);
            return;
        }
        let other = self
            .master
            .filter(|&m| m != d)
            .or_else(|| (0..DECKS).find(|&o| o != d && engine.is_playing(o)))
            .or_else(|| (0..DECKS).find(|&o| o != d && self.decks[o].key().is_some()));
        let (Some(target), Some((own, minor))) =
            (other.and_then(|o| self.decks[o].heard_key()), self.decks[d].key())
        else {
            self.status = Some("Key sync needs both tracks' keys".into());
            return;
        };
        let deck = &mut self.decks[d];
        let tempo =
            if deck.key_lock { 0 } else { (12.0 * deck.tempo_ratio().log2()).round() as i32 };
        let shift = key_sync_shift(((own + tempo).rem_euclid(12), minor), target);
        deck.set_key_shift(shift, engine);
        deck.key_synced = true;
    }

    fn apply_global_event(&mut self, action: GlobalAction, value: Value) {
        match (action, value) {
            (GlobalAction::BrowseTurn, Value::Relative(steps)) => self.browse(steps),
            // Pressing the browse knob (rekordbox's Forward on most
            // controllers) goes into the playlist, or back out to the playlists.
            (GlobalAction::BrowseForward | GlobalAction::BrowsePress, Value::Pressed(true)) => {
                self.focus = match self.focus {
                    Focus::Playlists => Focus::Tracks,
                    Focus::Tracks => Focus::Playlists,
                };
            }
            (GlobalAction::BrowseBack, Value::Pressed(true)) => self.focus = Focus::Playlists,
            (GlobalAction::BrowsePageUp, Value::Pressed(true)) => self.browse(-10),
            (GlobalAction::BrowsePageDown, Value::Pressed(true)) => self.browse(10),
            (GlobalAction::BrowseTop, Value::Pressed(true)) => self.browse(-100_000),
            (GlobalAction::BrowseBottom, Value::Pressed(true)) => self.browse(100_000),
            (GlobalAction::SwitchFocus, Value::Pressed(true)) => {
                self.focus = match self.focus {
                    Focus::Tracks => Focus::Playlists,
                    Focus::Playlists => Focus::Tracks,
                };
            }
            (GlobalAction::Zoom, Value::Relative(steps)) => {
                self.seconds_visible =
                    (self.seconds_visible * 1.15f64.powi(-steps)).clamp(2.0, 30.0);
            }
            _ => {}
        }
        let Some(engine) = self.engine.as_mut() else { return };
        let pressed = matches!(value, Value::Pressed(true));
        let fx = &mut self.effects;
        match (action, value) {
            (GlobalAction::MasterCue, _) if pressed => fx.toggle_master_cue(engine),
            (GlobalAction::ColorFxOn, _) if pressed => fx.toggle_color(engine),
            (GlobalAction::ColorFxSelect, _) if pressed => fx.next_color(engine),
            (GlobalAction::FxDepth, Value::Absolute(v)) => fx.depth = v,
            (GlobalAction::FxOn, _) if pressed => fx.toggle_on(),
            (GlobalAction::FxRelease, _) if pressed => fx.toggle_release(),
            (GlobalAction::FxBeatDown, _) if pressed => fx.change_beat(-1),
            (GlobalAction::FxBeatUp, _) if pressed => fx.change_beat(1),
            (GlobalAction::FxAutoBeat, _) if pressed => fx.tapped_bpm = None,
            (GlobalAction::FxTap, _) if pressed => fx.tap(),
            (GlobalAction::FxType(n), _) if pressed => {
                fx.kind = BeatFxKind::ALL[usize::from(n) % BeatFxKind::ALL.len()];
            }
            (GlobalAction::FxAssign(n), _) if pressed => {
                fx.target = match n {
                    0..=3 => FxTarget::Channel(usize::from(n)),
                    4 => FxTarget::Master,
                    _ => FxTarget::Sampler,
                };
            }
            (
                GlobalAction::CrossfaderStartPlayA
                | GlobalAction::CrossfaderStartPlayB
                | GlobalAction::CrossfaderStartCueA
                | GlobalAction::CrossfaderStartCueB,
                _,
            ) if pressed => {
                use GlobalAction as G;
                let side = match action {
                    G::CrossfaderStartPlayA | G::CrossfaderStartCueA => CrossfaderSide::A,
                    _ => CrossfaderSide::B,
                };
                let play = matches!(action, G::CrossfaderStartPlayA | G::CrossfaderStartPlayB);
                for deck in self.decks.iter_mut().filter(|d| d.side == side) {
                    deck.fader_start(play, engine);
                }
            }
            (GlobalAction::Crossfader, Value::Absolute(v)) => {
                self.crossfader = v * 2.0 - 1.0;
                engine.send(Command::SetCrossfader(self.crossfader));
            }
            (GlobalAction::MasterVolume, Value::Absolute(v)) => {
                self.master_volume = v;
                engine.send(Command::SetMasterVolume(v));
            }
            (GlobalAction::HeadphoneMix, Value::Absolute(v)) => {
                self.headphone_mix = v;
                engine.send(Command::SetHeadphoneMix(v));
            }
            (GlobalAction::HeadphoneVolume, Value::Absolute(v)) => {
                self.headphone_volume = v;
                engine.send(Command::SetHeadphoneVolume(v));
            }
            _ => {}
        }
    }

    /// The browse knob moves through playlists or tracks, whichever has focus.
    fn browse(&mut self, steps: i32) {
        self.reveal_selected = true;
        self.reveal_playlist = true;
        let step = |current: Option<usize>, len: usize| -> Option<usize> {
            if len == 0 {
                return None;
            }
            Some(match current {
                Some(i) => (i as i64 + i64::from(steps)).clamp(0, len as i64 - 1) as usize,
                None => 0,
            })
        };
        match self.focus {
            Focus::Tracks => {
                let rows = self.visible_rows();
                // The selected row itself, or the first copy of the track.
                let current = self.selected.and_then(|s| {
                    rows.iter()
                        .position(|&r| r == (s, self.selected_position.unwrap_or(usize::MAX)))
                        .or_else(|| rows.iter().position(|&(id, _)| id == s))
                });
                if let Some(next) = step(current, rows.len()) {
                    (self.selected, self.selected_position) =
                        (Some(rows[next].0), Some(rows[next].1));
                }
            }
            Focus::Playlists if self.source == Source::Collection => {
                let entries = self.collection_browse_entries();
                let current = entries.iter().position(|&e| e == self.collection_view.node);
                if let Some(next) = step(current, entries.len()) {
                    self.collection_view.node = entries[next];
                }
            }
            Focus::Playlists => {
                let Some(library) = &self.library else { return };
                // Position 0 is "All tracks"; folders, and playlists inside
                // closed folders, are skipped.
                let mut entries: Vec<Option<usize>> = vec![None];
                let mut hidden_below: Option<usize> = None;
                for (i, row) in library.playlists.iter().enumerate() {
                    if hidden_below.is_some_and(|d| row.depth > d) {
                        continue;
                    }
                    hidden_below = None;
                    match &row.track_ids {
                        None if self.usb_collapsed.contains(&i) => hidden_below = Some(row.depth),
                        None => {}
                        Some(_) => entries.push(Some(i)),
                    }
                }
                let current = entries.iter().position(|&e| e == self.playlist);
                if let Some(next) = step(current, entries.len()) {
                    self.playlist = entries[next];
                }
            }
        }
    }

    /// LED and meter values for the controller.
    fn feedback_values(&self) -> HashMap<Feedback, u8> {
        let mut values = HashMap::new();
        let Some(engine) = &self.engine else { return values };
        let on = |b: bool| if b { 127 } else { 0 };
        // Flashing LEDs: on for 250 ms, off for 250 ms.
        let flash = (self.started.elapsed().as_millis() / 250).is_multiple_of(2);
        for (d, deck) in self.decks.iter().enumerate() {
            let loaded = deck.track.is_some();
            let playing = engine.is_playing(d);
            let mut set = |what, value| {
                values.insert(Feedback::Deck(d, what), value);
            };
            set(DeckFeedback::Playing, on(playing));
            set(
                DeckFeedback::AtCue,
                on(loaded && !playing && (engine.position_secs(d) - deck.cue_point).abs() < AT_CUE),
            );
            for n in 0..HOT_CUES {
                let has = deck.track.as_ref().is_some_and(|t| t.hot_cues[n].is_some());
                set(DeckFeedback::HotCueSet(n as u8), on(has));
            }
            set(DeckFeedback::Sync, on(deck.sync));
            set(DeckFeedback::SyncMaster, on(self.master == Some(d)));
            set(DeckFeedback::Quantize, on(deck.quantize));
            set(DeckFeedback::Vinyl, on(deck.vinyl));
            // Slip flashes while normal playback runs on in the background.
            let slipping = deck.slip
                && playing
                && (deck.is_scratching() || deck.loop_active || deck.slip_held());
            set(DeckFeedback::Slip, on(deck.slip && (!slipping || flash)));
            set(DeckFeedback::KeySync, on(deck.key_synced));
            for pad in 0..8u8 {
                let slot = self.effects.sampler_slot(d, pad);
                let lit = if engine.sampler_playing(slot) {
                    flash
                } else {
                    self.effects.samples[slot].is_some()
                };
                set(DeckFeedback::SamplerPlaying(pad), on(lit));
                let semis = i32::from(pad);
                set(DeckFeedback::PadLit(PadMode::BeatJump, pad), on(loaded));
                let looping = deck
                    .active_loop_beats()
                    .is_some_and(|b| (b - deck.pad_loop_beats(usize::from(pad))).abs() < 1e-6);
                set(
                    DeckFeedback::PadLit(PadMode::BeatLoop, pad),
                    on(loaded && (!looping || flash)),
                );
                let current = deck.key_shift == deck.key_shift_base + semis;
                set(
                    DeckFeedback::PadLit(PadMode::KeyShift, pad),
                    on(loaded && (!current || flash)),
                );
                let keyboard = match deck.keyboard_cue {
                    None => {
                        deck.track.as_ref().is_some_and(|t| t.hot_cues[usize::from(pad)].is_some())
                    }
                    Some(_) => deck.key_shift != deck.keyboard_base + semis || flash,
                };
                set(DeckFeedback::PadLit(PadMode::Keyboard, pad), on(loaded && keyboard));
            }
            for slot in 0..16u8 {
                let held = self.effects.pad == Some((d, slot));
                set(DeckFeedback::PadLit(PadMode::PadFx, slot), on(loaded && (!held || flash)));
            }
            for mode in PadMode::ALL {
                set(DeckFeedback::PadMode(mode), on(deck.pad_mode == mode));
            }
            set(DeckFeedback::KeyLock, on(deck.key_lock));
            set(DeckFeedback::LoopActive, on(deck.loop_active));
            set(DeckFeedback::HeadphoneCue, on(deck.headphone_cue));
            set(DeckFeedback::Loaded, on(loaded));
            set(DeckFeedback::Level, (engine.deck_peak(d).clamp(0.0, 1.0) * 127.0) as u8);
        }
        let fx = &self.effects;
        values.insert(Feedback::Global(GlobalFeedback::FxOn), on(fx.on));
        values.insert(Feedback::Global(GlobalFeedback::FxRelease), on(fx.release));
        values.insert(Feedback::Global(GlobalFeedback::ColorFxOn), on(fx.color_on));
        values.insert(Feedback::Global(GlobalFeedback::MasterCue), on(fx.master_cue));
        values.insert(Feedback::Global(GlobalFeedback::FxBeat), fx.beat_indicator());
        values
    }
}

impl eframe::App for DeckhandApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // winit's window teardown can trip an AppKit exception on recent macOS;
        // nothing needs cleaning up, so leave directly.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.save_session(true);
            self.finish_recording();
            self.config.save();
            std::process::exit(0);
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.collect_garbage();
            for deck in &mut self.decks {
                deck.poll(engine);
            }
        }
        self.sync_jog_resolution();
        for event in self.controller.poll() {
            self.apply_event(event, ctx);
        }
        self.save_track_edits();
        self.poll_collection(ctx);
        self.check_controllers(ctx);
        self.save_session(false);
        self.finish_doubles();
        self.update_sync();
        let bpm = self.fx_bpm();
        if let Some(engine) = self.engine.as_mut() {
            self.effects.update(engine, bpm);
        }
        let values = self.feedback_values();
        self.controller.send_feedback(|f| values.get(&f).copied().unwrap_or(0));

        let busy = self
            .engine
            .as_ref()
            .is_some_and(|e| (0..DECKS).any(|d| e.is_playing(d) || e.is_moving(d)))
            || self.decks.iter().any(|d| d.loading_title().is_some() || d.is_scratching());
        if busy {
            ctx.request_repaint();
        } else if self.controller.has_output() {
            // Keep meters and LEDs fresh at a low rate.
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        self.top_bar(ctx);
        if self.config.show_waveforms {
            let vertical = self.config.vertical_waveforms;
            egui::TopBottomPanel::top("waveforms").show(ctx, |ui| {
                if !vertical {
                    self.waveforms(ui);
                }
                self.waveform_toolbar(ui);
            });
        }
        if self.config.show_fx {
            egui::TopBottomPanel::top("fx").show(ctx, |ui| self.fx_panel(ui));
        }
        self.update_status();
        if ctx.input(|i| i.key_pressed(egui::Key::F12)) {
            self.show_diagnostics = !self.show_diagnostics;
        }
        if self.show_diagnostics {
            // Window and screen details, drawn as a panel (not a floating
            // layer) so it shows even when floating things do not.
            let info = ctx.input(|i| {
                let v = i.viewport();
                format!(
                    "viewport {:?} | content {:?} | points/pixel {:.2} (native {:?}) | monitor {:?} | outer {:?} | inner {:?} | fullscreen {:?} | maximized {:?} | focused {:?}",
                    i.viewport_rect(),
                    i.content_rect(),
                    i.pixels_per_point,
                    v.native_pixels_per_point,
                    v.monitor_size,
                    v.outer_rect,
                    v.inner_rect,
                    v.fullscreen,
                    v.maximized,
                    v.focused,
                )
            });
            let layers = ctx
                .memory(|m| m.layer_ids().filter(|l| l.order != egui::Order::Background).count());
            let bending: Vec<bool> =
                (0..DECKS).map(|d| self.engine.as_ref().is_some_and(|e| e.is_bending(d))).collect();
            let info = format!("{info} | jog bending {bending:?}");
            egui::TopBottomPanel::bottom("diagnostics").show(ctx, |ui| {
                ui.label(
                    RichText::new(format!("{info} | floating layers {layers}")).small().monospace(),
                );
            });
        }
        self.status_bar(ctx);
        if self.config.show_library {
            egui::TopBottomPanel::bottom("library")
                .resizable(true)
                .default_height(if self.config.four_decks { 200.0 } else { 300.0 })
                .show(ctx, |ui| self.library_ui(ui, ctx));
            self.reveal_playlist = false;
        }
        egui::CentralPanel::default().show(ctx, |ui| self.central(ui));
        if self.show_controller {
            self.controller_window(ctx);
        }
        self.duplicate_dialog(ctx);
        self.edit_dialog(ctx);
        if self.show_prefs {
            self.prefs_window(ctx);
        }
        // Test aid: move the window to "x,y" two seconds after start (to
        // try another monitor while running).
        if let Ok(target) = std::env::var("DECKHAND_MOVE_TO")
            && self.started.elapsed() > Duration::from_secs(2)
            && !self.moved_window
            && let Some((x, y)) = target.split_once(',')
            && let (Ok(x), Ok(y)) = (x.trim().parse::<f32>(), y.trim().parse::<f32>())
        {
            self.moved_window = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
        }
        self.handle_screenshot(ctx);
    }
}

impl DeckhandApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(crate::APP_NAME);
                ui.separator();
                let current = match self.source {
                    Source::Collection => "Collection".to_owned(),
                    Source::Usb => self
                        .library
                        .as_ref()
                        .map_or_else(|| "No library".to_owned(), UsbLibrary::name),
                };
                let mut open = None;
                let mut collection = false;
                egui::ComboBox::from_id_salt("library").selected_text(current).show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.source == Source::Collection, "Collection")
                        .clicked()
                    {
                        collection = true;
                    }
                    ui.separator();
                    for export in &self.exports {
                        if ui.selectable_label(false, export.display().to_string()).clicked() {
                            open = Some(export.clone());
                        }
                    }
                    if ui.button("Choose folder…").clicked() {
                        open = rfd::FileDialog::new()
                            .set_title("Choose the root of a rekordbox USB export")
                            .pick_folder();
                    }
                });
                if ui.button("⟳").on_hover_text("Look for USB exports again").clicked() {
                    self.exports = find_exports();
                }
                if let Some(root) = open {
                    self.open_library(root);
                }
                if collection {
                    self.set_source(Source::Collection);
                }
                ui.separator();

                let device = self
                    .engine
                    .as_ref()
                    .map_or_else(|| "No audio".to_owned(), |e| e.info().device.clone());
                let mut chosen = None;
                egui::ComboBox::from_id_salt("audio").selected_text(device).show_ui(ui, |ui| {
                    for name in Engine::output_devices() {
                        if ui.selectable_label(false, &name).clicked() {
                            chosen = Some(name);
                        }
                    }
                });
                if let Some(name) = chosen {
                    self.start_engine(Some(name));
                }
                if let Some(engine) = &self.engine {
                    let info = engine.info();
                    ui.label(format!("{} Hz · {} ch", info.sample_rate, info.channels));
                }
                ui.toggle_value(&mut self.show_prefs, "⚙ Preferences");
                ui.separator();
                self.record_button(ui);
                ui.separator();
                let mut four = self.config.four_decks;
                ui.selectable_value(&mut four, false, "2 decks");
                ui.selectable_value(&mut four, true, "4 decks");
                if four != self.config.four_decks {
                    self.config.four_decks = four;
                    self.config.save();
                }
                ui.separator();
                ui.menu_button("View", |ui| {
                    let before = (
                        self.config.vertical_waveforms,
                        self.config.show_waveforms,
                        self.config.show_mixer,
                        self.config.show_library,
                        self.config.show_fx,
                    );
                    ui.label(RichText::new("Waveforms").small().weak());
                    ui.radio_value(&mut self.config.vertical_waveforms, false, "Horizontal (top)");
                    ui.radio_value(
                        &mut self.config.vertical_waveforms,
                        true,
                        "Vertical (between decks)",
                    );
                    ui.separator();
                    ui.checkbox(&mut self.config.show_waveforms, "Waveforms");
                    ui.checkbox(&mut self.config.show_mixer, "Mixer");
                    ui.checkbox(&mut self.config.show_library, "Library");
                    ui.checkbox(&mut self.config.show_fx, "FX and sampler");
                    ui.separator();
                    let four = self.config.four_decks;
                    let which = if four { "4-deck" } else { "2-deck" };
                    ui.menu_button(format!("Deck elements ({which} layout)"), |ui| {
                        let layout = self.config.deck_layout_mut(four);
                        let mut changed = false;
                        for element in crate::layout::DeckElement::ALL {
                            let mut shown = layout.is_shown(element);
                            if ui.checkbox(&mut shown, element.label()).changed() {
                                layout.set_shown(element, shown);
                                changed = true;
                            }
                        }
                        if changed {
                            self.config.save();
                        }
                    });
                    if ui
                        .checkbox(&mut self.editing_layout, "Edit deck layout (drag elements)")
                        .changed()
                    {
                        ui.close();
                    }
                    if ui.button("Reset deck layout").clicked() {
                        *self.config.deck_layout_mut(four) = if four {
                            crate::layout::DeckLayout::four_decks()
                        } else {
                            crate::layout::DeckLayout::default()
                        };
                        self.config.save();
                        ui.close();
                    }
                    let after = (
                        self.config.vertical_waveforms,
                        self.config.show_waveforms,
                        self.config.show_mixer,
                        self.config.show_library,
                        self.config.show_fx,
                    );
                    if before != after {
                        self.config.save();
                    }
                });
                ui.separator();
                let label = self
                    .controller
                    .port()
                    .map_or_else(|| "🎛 Controller".to_owned(), |p| format!("🎛 {p}"));
                ui.toggle_value(&mut self.show_controller, label);
            });
        });
    }

    /// Horizontal waveform strip: one row per deck, in mixer channel order.
    fn waveforms(&mut self, ui: &mut Ui) {
        let count = self.deck_count();
        let height = if count > 2 { 52.0 } else { 84.0 };
        for index in self.mixer_channels() {
            self.deck_waveform(ui, index, vec2(ui.available_width(), height), false);
        }
    }

    /// One deck's scrolling waveform; dragging it moves through the track.
    fn deck_waveform(&mut self, ui: &mut Ui, index: usize, size: egui::Vec2, vertical: bool) {
        // The zoom is real time: a deck playing faster shows more of its
        // track, so beats of synced decks line up across the whole width.
        let visible = self.seconds_visible * self.decks[index].tempo_ratio().max(0.05);
        let Some(engine) = self.engine.as_mut() else { return };
        let deck = &self.decks[index];
        let (rect, response) = ui.allocate_exact_size(size, Sense::drag());
        let painter = ui.painter_at(rect);
        match &deck.track {
            Some(track) => {
                let position = engine.display_position_secs(index);
                let hot_cues = hot_cue_marks(track);
                let markers = Markers {
                    beats: &track.beats,
                    hot_cues: &hot_cues,
                    memory_cues: &track.memory_cues,
                    memory_loops: &track.memory_loops,
                    cue_point: deck.cue_point,
                    active_loop: deck.loop_region.map(|r| (r, deck.loop_active)),
                    grid_strength: self.config.grid_strength,
                };
                draw_detail(
                    &painter,
                    rect,
                    vertical,
                    &track.waveform,
                    position,
                    visible,
                    self.config.waveform_style,
                    if self.config.prefs.eq_waveforms { deck.eq_gains() } else { [1.0; 3] },
                    &markers,
                );
                if response.dragged() {
                    // Dragging the music along: left moves forward horizontally;
                    // vertically, upcoming music is above, so down moves forward.
                    let delta = response.drag_delta();
                    let (moved, length) =
                        if vertical { (delta.y, rect.height()) } else { (-delta.x, rect.width()) };
                    let seconds = f64::from(moved) / f64::from(length) * visible;
                    engine.send(Command::Shift { deck: index, seconds });
                }
            }
            None => {
                painter.rect_filled(rect, 0.0, Color32::from_gray(12));
            }
        }
        painter.text(
            rect.left_top() + vec2(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            format!("{}", index + 1),
            egui::FontId::proportional(14.0),
            DECK_COLORS[index],
        );
    }

    fn waveform_toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("Zoom");
            ui.add(
                Slider::new(&mut self.seconds_visible, 2.0..=30.0).logarithmic(true).suffix(" s"),
            );
            ui.separator();
            let mut style = self.config.waveform_style;
            ui.selectable_value(&mut style, WaveformStyle::Rgb, "RGB");
            ui.selectable_value(&mut style, WaveformStyle::ThreeBand, "3-Band");
            if style != self.config.waveform_style {
                self.config.waveform_style = style;
                self.config.save();
            }
            ui.separator();
            ui.label("Grid");
            let grid = ui
                .add(Slider::new(&mut self.config.grid_strength, 0.0..=1.5).show_value(false))
                .on_hover_text("Beat grid brightness; all the way left hides it");
            if grid.drag_stopped() || (grid.changed() && !grid.dragged()) {
                self.config.save();
            }
            if self.config.prefs.eq_waveforms {
                ui.label(RichText::new("waveforms follow each channel's EQ").small().weak());
            }
        });
    }

    /// Decks, vertical waveforms and mixer side by side, leaving out hidden parts.
    fn central(&mut self, ui: &mut Ui) {
        if self.editing_layout {
            ui.horizontal(|ui| {
                ui.colored_label(
                    CUE_COLOR,
                    "Editing the deck layout: drag an element onto another (left or right half), onto + at the end of a row or the side column, onto New row, or onto Hide.",
                );
                if ui.button("Done").clicked() {
                    self.editing_layout = false;
                }
            });
        }
        let four = self.config.four_decks;
        let vertical = self.config.vertical_waveforms && self.config.show_waveforms;
        let show_mixer = self.config.show_mixer;
        let (left, right): (Vec<usize>, Vec<usize>) =
            if four { (vec![0, 2], vec![1, 3]) } else { (vec![0], vec![1]) };
        // Vertical waveforms follow the mixer's channel order, half on each side.
        let channels = self.mixer_channels();
        let (left_waves, right_waves) = channels.split_at(channels.len() / 2);
        let (left_waves, right_waves) = (left_waves.to_vec(), right_waves.to_vec());
        const GAP: f32 = 8.0;
        let wave_width = if vertical { 56.0 * left.len() as f32 } else { 0.0 };
        let mixer_width = if show_mixer { if four { 310.0 } else { 210.0 } } else { 0.0 };
        let height = ui.available_height();
        // Gaps between the five columns, plus a margin so the last column is never clipped.
        let deck_width =
            ((ui.available_width() - 2.0 * wave_width - mixer_width - 6.0 * GAP - 16.0) / 2.0)
                .max(240.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            self.deck_column(ui, &left, deck_width, height);
            if vertical {
                self.wave_column(ui, &left_waves, wave_width, height);
            }
            if show_mixer {
                ui.allocate_ui_with_layout(
                    vec2(mixer_width, height),
                    Layout::top_down(Align::Center),
                    |ui| {
                        ui.set_width(mixer_width);
                        self.mixer_ui(ui);
                    },
                );
            }
            if vertical {
                self.wave_column(ui, &right_waves, wave_width, height);
            }
            self.deck_column(ui, &right, deck_width, height);
        });
    }

    fn deck_column(&mut self, ui: &mut Ui, decks: &[usize], width: f32, height: f32) {
        let compact = decks.len() > 1;
        ui.allocate_ui_with_layout(vec2(width, height), Layout::top_down(Align::Min), |ui| {
            ui.set_width(width);
            ScrollArea::vertical().id_salt(("decks", decks[0])).show(ui, |ui| {
                for &deck in decks {
                    // With four decks, the one each controller side drives is framed.
                    let active = self.config.four_decks && self.active_decks[deck % 2] == deck;
                    let stroke = if active {
                        Stroke::new(2.0_f32, DECK_COLORS[deck])
                    } else {
                        Stroke::new(2.0_f32, Color32::TRANSPARENT)
                    };
                    egui::Frame::new()
                        .stroke(stroke)
                        .corner_radius(4)
                        .inner_margin(3.0)
                        .show(ui, |ui| self.deck_ui(ui, deck, compact));
                    if compact {
                        ui.separator();
                    }
                }
            });
        });
    }

    fn wave_column(&mut self, ui: &mut Ui, decks: &[usize], width: f32, height: f32) {
        ui.allocate_ui_with_layout(vec2(width, height), Layout::left_to_right(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let each = (width - 4.0 * (decks.len() as f32 - 1.0)) / decks.len() as f32;
            for &deck in decks {
                self.deck_waveform(ui, deck, vec2(each, height), true);
            }
        });
    }

    /// Mixer channels left to right: 1 2 with two decks; with four, the
    /// connected controller's order or the chosen default.
    fn mixer_channels(&self) -> Vec<usize> {
        if !self.config.four_decks {
            return vec![0, 1];
        }
        let order = self.controller.channel_order().unwrap_or(self.config.channel_order);
        order.iter().map(|&ch| ch.clamp(1, DECKS) - 1).collect()
    }

    fn mixer_ui(&mut self, ui: &mut Ui) {
        // Five knobs are stacked per channel; they shrink when the window is
        // short so the fader and crossfader still fit.
        let room = ui.available_height() - 125.0 - 90.0;
        let channel_knob = (room / 5.0 - 16.0).clamp(16.0, 24.0);
        const KNOB: f32 = 24.0;
        const ORDERS: [[usize; 4]; 2] = [[1, 2, 3, 4], [3, 1, 2, 4]];
        let channels = self.mixer_channels();
        let four = self.config.four_decks;
        let current_order = self.controller.channel_order().unwrap_or(self.config.channel_order);
        let mut new_order = None;
        let Some(engine) = self.engine.as_mut() else { return };
        // Faders take whatever height the knobs, buttons and crossfader leave.
        let fader_height =
            (ui.available_height() - 5.0 * (channel_knob + 16.0) - 125.0).clamp(60.0, 180.0);
        let panel = egui::Frame::new()
            .fill(Color32::from_rgb(22, 22, 25))
            .corner_radius(6)
            .inner_margin(6.0);
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            panel.show(ui, |ui| {
                if four {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Channel order").small().weak());
                        for order in ORDERS {
                            let label = order.map(|c| c.to_string()).join(" ");
                            if ui
                                .selectable_label(
                                    current_order == order,
                                    RichText::new(label).small(),
                                )
                                .clicked()
                            {
                                new_order = Some(order);
                            }
                        }
                    });
                }
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing = vec2(4.0, 2.0);
                    for &index in &channels {
                        let deck = &mut self.decks[index];
                        ui.vertical(|ui| {
                            ui.set_width(KNOB + 26.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(format!("{}", index + 1))
                                        .strong()
                                        .color(DECK_COLORS[index]),
                                );
                                let mut changed = false;
                                let [low, mid, high] = &mut deck.eq;
                                // One column per channel, top to bottom, like a DJ mixer.
                                changed |= knob(
                                    ui,
                                    &mut deck.trim,
                                    0.0..=1.0,
                                    0.5,
                                    "TRIM",
                                    true,
                                    channel_knob,
                                )
                                .changed();
                                changed |= knob(ui, high, 0.0..=1.0, 0.5, "HI", true, channel_knob)
                                    .changed();
                                changed |= knob(ui, mid, 0.0..=1.0, 0.5, "MID", true, channel_knob)
                                    .changed();
                                changed |= knob(ui, low, 0.0..=1.0, 0.5, "LOW", true, channel_knob)
                                    .changed();
                                changed |= knob(
                                    ui,
                                    &mut deck.filter,
                                    -1.0..=1.0,
                                    0.0,
                                    "FILTER",
                                    true,
                                    channel_knob,
                                )
                                .changed();
                                let cue_fill = if deck.headphone_cue {
                                    CUE_COLOR
                                } else {
                                    Color32::from_gray(48)
                                };
                                let cue = egui::Button::new(RichText::new("CUE").small().strong())
                                    .fill(cue_fill)
                                    .min_size(vec2(44.0, 18.0));
                                if ui.add(cue).on_hover_text("Headphone cue").clicked() {
                                    deck.headphone_cue = !deck.headphone_cue;
                                    changed = true;
                                }
                                ui.horizontal(|ui| {
                                    ui.add_space(10.0);
                                    ui.spacing_mut().item_spacing.x = 3.0;
                                    changed |= fader(ui, &mut deck.volume, fader_height).changed();
                                    meter(ui, engine.deck_peak(index), vec2(6.0, fader_height));
                                });
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    ui.spacing_mut().item_spacing.x = 1.0;
                                    for (side, label) in [
                                        (CrossfaderSide::A, "A"),
                                        (CrossfaderSide::Thru, "T"),
                                        (CrossfaderSide::B, "B"),
                                    ] {
                                        let text = RichText::new(label).small();
                                        if ui
                                            .selectable_label(deck.side == side, text)
                                            .on_hover_text("Crossfader assign")
                                            .clicked()
                                        {
                                            deck.side = side;
                                            changed = true;
                                        }
                                    }
                                });
                                if changed {
                                    deck.apply_mixer(engine);
                                }
                            });
                        });
                    }
                    // A separator here would stretch to the full height and push the crossfader out.
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.set_width(64.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("MASTER").small().strong());
                            if knob(
                                ui,
                                &mut self.master_volume,
                                0.0..=1.0,
                                0.8,
                                "LEVEL",
                                false,
                                KNOB + 4.0,
                            )
                            .changed()
                            {
                                engine.send(Command::SetMasterVolume(self.master_volume));
                            }
                            ui.horizontal(|ui| {
                                ui.add_space(20.0);
                                ui.spacing_mut().item_spacing.x = 3.0;
                                let peak = engine.master_peak();
                                meter(ui, peak, vec2(7.0, fader_height));
                                meter(ui, peak, vec2(7.0, fader_height));
                            });
                            ui.label(RichText::new("PHONES").small().strong());
                            if engine.info().has_headphone_output() {
                                if knob(
                                    ui,
                                    &mut self.headphone_mix,
                                    0.0..=1.0,
                                    0.0,
                                    "CUE/MST",
                                    false,
                                    KNOB,
                                )
                                .changed()
                                {
                                    engine.send(Command::SetHeadphoneMix(self.headphone_mix));
                                }
                                if knob(
                                    ui,
                                    &mut self.headphone_volume,
                                    0.0..=1.0,
                                    0.8,
                                    "LEVEL",
                                    false,
                                    KNOB,
                                )
                                .changed()
                                {
                                    engine.send(Command::SetHeadphoneVolume(self.headphone_volume));
                                }
                            } else {
                                ui.label(RichText::new("needs a 4-channel output").small().weak());
                            }
                        });
                    });
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("A").small());
                    let width = (ui.available_width() - 30.0).clamp(120.0, 220.0);
                    if crossfader(ui, &mut self.crossfader, width).changed() {
                        engine.send(Command::SetCrossfader(self.crossfader));
                    }
                    ui.label(RichText::new("B").small());
                });
            });
        });
        if let Some(order) = new_order {
            if self.controller.port().is_some() {
                self.controller.set_channel_order(order);
            } else {
                self.config.channel_order = order;
                self.config.save();
            }
        }
    }

    /// Beat FX, Color FX, Master Cue and the sampler, in one strip.
    fn fx_panel(&mut self, ui: &mut Ui) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        let fx = &mut self.effects;
        let decks = if self.config.four_decks { DECKS } else { 2 };
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().slider_width = 64.0;
            ui.label(RichText::new("BEAT FX").small().strong());
            egui::ComboBox::from_id_salt("beat fx")
                .selected_text(fx.kind.label())
                .width(92.0)
                .show_ui(ui, |ui| {
                    for kind in BeatFxKind::ALL {
                        ui.selectable_value(&mut fx.kind, kind, kind.label());
                    }
                });
            if ui.small_button("◀").on_hover_text("Shorter beat").clicked() {
                fx.change_beat(-1);
            }
            ui.label(RichText::new(format!("{:>4}", fx.beat_label())).monospace());
            if ui.small_button("▶").on_hover_text("Longer beat").clicked() {
                fx.change_beat(1);
            }
            let tap = match fx.tapped_bpm {
                Some(bpm) => format!("TAP {bpm:.0}"),
                None => "TAP".to_owned(),
            };
            let response = toggle(ui, fx.tapped_bpm.is_some(), &tap).on_hover_text(
                "Tap the effect's tempo; right-click to follow the master deck again",
            );
            if response.clicked() {
                fx.tap();
            }
            if response.secondary_clicked() {
                fx.tapped_bpm = None;
            }
            ui.add(
                Slider::new(&mut fx.depth, 0.0..=1.0)
                    .show_value(false)
                    .text(RichText::new("DEPTH").small()),
            );
            let target_label = |t: FxTarget| match t {
                FxTarget::Channel(d) => format!("CH{}", d + 1),
                FxTarget::Master => "MASTER".to_owned(),
                FxTarget::Sampler => "SAMPLER".to_owned(),
            };
            egui::ComboBox::from_id_salt("fx target")
                .selected_text(target_label(fx.target))
                .width(70.0)
                .show_ui(ui, |ui| {
                    for target in (0..decks)
                        .map(FxTarget::Channel)
                        .chain([FxTarget::Master, FxTarget::Sampler])
                    {
                        ui.selectable_value(&mut fx.target, target, target_label(target));
                    }
                });
            if toggle(ui, fx.on, "ON").clicked() {
                fx.toggle_on();
            }
            if toggle(ui, fx.release, "RELEASE")
                .on_hover_text("Release FX: cut the source and let the echo play out")
                .clicked()
            {
                fx.toggle_release();
            }
            ui.label("?").on_hover_text(
                (0..16)
                    .map(|s| format!("Pad FX {}: {}", s + 1, pad_fx_label(s)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            ui.separator();

            ui.label(RichText::new("COLOR FX").small().strong());
            let mut color = fx.color;
            egui::ComboBox::from_id_salt("color fx")
                .selected_text(color.label())
                .width(72.0)
                .show_ui(ui, |ui| {
                    for kind in ColorFxKind::ALL {
                        ui.selectable_value(&mut color, kind, kind.label());
                    }
                });
            if color != fx.color {
                fx.set_color(color, engine);
            }
            if toggle(ui, fx.color_on, "ON")
                .on_hover_text("Color FX on the channel knobs")
                .clicked()
            {
                fx.toggle_color(engine);
            }
            if toggle(ui, fx.master_cue, "MASTER CUE")
                .on_hover_text("Hear the master mix in the headphones")
                .clicked()
            {
                fx.toggle_master_cue(engine);
            }
            ui.separator();

            ui.label(RichText::new("SAMPLER").small().strong());
            ui.spacing_mut().item_spacing.x = 3.0;
            if ui.small_button("◀").on_hover_text("Previous bank").clicked() {
                fx.change_bank(-1);
            }
            ui.label(RichText::new(format!("BANK {}", fx.bank + 1)).small().monospace());
            if ui.small_button("▶").on_hover_text("Next bank").clicked() {
                fx.change_bank(1);
            }
            let first = fx.bank * crate::effects::BANK_SLOTS;
            for slot in first..first + crate::effects::BANK_SLOTS {
                let name = fx.samples[slot].as_ref().map(|s| s.name.clone());
                let label = (slot - first + 1).to_string();
                let playing = engine.sampler_playing(slot);
                let response =
                    crate::widgets::lit_button_sized(ui, &label, playing, ACTIVE, 11.0, 6.0)
                        .on_hover_text(
                            name.as_deref()
                                .unwrap_or("Empty: put audio files in the samples folder"),
                        );
                if response.clicked() {
                    engine.send(Command::SamplerPlay { slot });
                }
                if response.secondary_clicked() {
                    engine.send(Command::SamplerStop { slot });
                }
            }
            if ui
                .small_button("…")
                .on_hover_text("Open the samples folder; files load at start-up")
                .clicked()
            {
                let dir = crate::config::samples_dir();
                let _ = std::fs::create_dir_all(&dir);
                let opener = if cfg!(windows) { "explorer" } else { "open" };
                let _ = std::process::Command::new(opener).arg(&dir).spawn();
            }
        });
    }

    fn library_ui(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        if self.source == Source::Collection {
            return self.collection_ui(ui, ctx);
        }
        let Some(library) = &self.library else {
            ui.centered_and_justified(|ui| {
                ui.label("Plug in a rekordbox USB export, or choose its folder in the bar above.");
            });
            return;
        };
        let mut load: Option<(u32, usize)> = None;
        let mut double_clicked = None;
        let deck_count = self.deck_count();
        let rows = self.usb_rows();
        let loaded: Vec<Option<u32>> = self
            .decks
            .iter()
            .map(|d| d.track.as_ref().filter(|t| !t.from_collection).map(|t| t.id))
            .collect();
        let free = self.free_deck(&loaded);
        let focus_stroke =
            |focused: bool| if focused { Stroke::new(1.0_f32, ACTIVE) } else { Stroke::NONE };
        ui.horizontal_top(|ui| {
            // The sidebar sits in a horizontal row; give it its own top-down layout.
            ui.allocate_ui_with_layout(
                vec2(240.0, ui.available_height()),
                Layout::top_down(Align::Min),
                |ui| {
                    egui::Frame::new().stroke(focus_stroke(self.focus == Focus::Playlists)).show(
                        ui,
                        |ui| {
                            ScrollArea::vertical()
                                .id_salt("playlists")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.set_width(230.0);
                                    if ui
                                        .selectable_label(
                                            self.playlist.is_none(),
                                            format!(
                                                "All tracks ({})",
                                                library.library.tracks.len()
                                            ),
                                        )
                                        .clicked()
                                    {
                                        self.playlist = None;
                                        self.focus = Focus::Playlists;
                                    }
                                    // USB folders can be closed; rows inside are skipped.
                                    let mut hidden_below: Option<usize> = None;
                                    let mut toggle = None;
                                    for (i, row) in library.playlists.iter().enumerate() {
                                        if hidden_below.is_some_and(|d| row.depth > d) {
                                            continue;
                                        }
                                        hidden_below = None;
                                        let folder = row.track_ids.is_none();
                                        let open = !self.usb_collapsed.contains(&i);
                                        if folder && !open {
                                            hidden_below = Some(row.depth);
                                        }
                                        ui.horizontal(|ui| {
                                            ui.add_space(row.depth as f32 * 14.0);
                                            if folder {
                                                if crate::widgets::disclosure(ui, open).clicked() {
                                                    toggle = Some(i);
                                                }
                                            } else {
                                                ui.add_space(14.0 + ui.spacing().item_spacing.x);
                                            }
                                            let label = match &row.track_ids {
                                                Some(ids) => {
                                                    format!("♫ {} ({})", row.name, ids.len())
                                                }
                                                None => row.name.clone(),
                                            };
                                            let button = egui::Button::selectable(
                                                self.playlist == Some(i),
                                                label,
                                            )
                                            .truncate();
                                            let response = ui.add(button).on_hover_text(&row.name);
                                            if self.playlist == Some(i) && self.reveal_playlist {
                                                response.scroll_to_me(Some(egui::Align::Center));
                                            }
                                            if response.clicked() {
                                                if folder {
                                                    toggle = Some(i);
                                                } else {
                                                    self.playlist = Some(i);
                                                    self.focus = Focus::Playlists;
                                                }
                                            }
                                        });
                                    }
                                    if let Some(i) = toggle
                                        && !self.usb_collapsed.remove(&i)
                                    {
                                        self.usb_collapsed.insert(i);
                                    }
                                });
                        },
                    );
                },
            );
            ui.separator();
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("Search title, artist, album, genre")
                            .desired_width(320.0),
                    );
                    if let Some(id) = self.selected {
                        ui.separator();
                        for deck in 0..deck_count {
                            if ui.button(format!("Load to deck {}", deck + 1)).clicked() {
                                load = Some((id, deck));
                            }
                        }
                    }
                });
                let options = TableOptions {
                    selected: self.selected,
                    selected_position: self.selected_position,
                    reveal_selected: std::mem::take(&mut self.reveal_selected),
                    loaded: &loaded,
                    draggable: false,
                    editable_rating: false,
                };
                let mut context_load = None;
                let output = egui::Frame::new()
                    .stroke(focus_stroke(self.focus == Focus::Tracks))
                    .show(ui, |ui| {
                        track_table(
                            ui,
                            &rows,
                            &mut self.config.columns,
                            &mut self.config.sort,
                            &options,
                            &mut |ui, row| {
                                for deck in 0..deck_count {
                                    if ui.button(format!("Load to deck {}", deck + 1)).clicked() {
                                        context_load = Some((row.id, deck));
                                        ui.close();
                                    }
                                }
                            },
                        )
                    })
                    .inner;
                if output.layout_changed {
                    self.config.save();
                }
                if let Some((id, position)) = output.clicked {
                    (self.selected, self.selected_position) = (Some(id), Some(position));
                    self.focus = Focus::Tracks;
                }
                if let Some(id) = output.double_clicked {
                    double_clicked = Some(id);
                }
                if context_load.is_some() {
                    load = context_load;
                }
            });
        });
        if let (Some(id), Some(deck)) = (double_clicked, free) {
            load = Some((id, deck));
        }
        if let Some((id, deck)) = load {
            self.selected = Some(id);
            self.load_selected(deck, ctx);
        }
    }

    /// The USB export's tracks for the list: playlist, search and sort applied.
    fn usb_rows(&self) -> Vec<Row> {
        let Some(library) = &self.library else { return Vec::new() };
        let mut rows: Vec<Row> = library
            .numbered_tracks(self.playlist, &self.search)
            .into_iter()
            .map(|(position, t)| Row {
                id: t.track.id,
                entry: None,
                position,
                title: t.track.title.clone(),
                artist: t.artist.clone().unwrap_or_default(),
                album: t.album.clone().unwrap_or_default(),
                genre: t.genre.clone().unwrap_or_default(),
                label: t.label.clone().unwrap_or_default(),
                bpm: Some(t.track.bpm()).filter(|b| *b > 0.0),
                key: t.key.clone(),
                duration: f64::from(t.track.duration_secs),
                year: Some(t.track.year).filter(|y| *y > 0),
                rating: t.track.rating.min(5),
                plays: u32::from(t.track.play_count),
                added: t.track.date_added.clone(),
                bitrate: Some(t.track.bitrate).filter(|b| *b > 0),
                comment: t.track.comment.clone(),
            })
            .collect();
        sort_rows(&mut rows, self.config.sort);
        rows
    }

    /// A deck to load into by double-click: an empty one, else one not playing.
    fn free_deck(&self, loaded: &[Option<u32>]) -> Option<usize> {
        let count = self.deck_count();
        (0..count).find(|&d| loaded[d].is_none() && self.decks[d].track.is_none()).or_else(|| {
            (0..count).find(|&d| !self.engine.as_ref().is_some_and(|e| e.is_playing(d)))
        })
    }

    fn prefs_window(&mut self, ctx: &egui::Context) {
        const BUFFERS: [u32; 8] = [32, 64, 128, 256, 512, 1024, 2048, 4096];
        let device = self.config.audio_device.clone();
        if self.audio_caps.as_ref().is_none_or(|(d, ..)| *d != device) {
            let (rates, buffers) = Engine::device_capabilities(device.as_deref());
            self.audio_caps = Some((device.clone(), rates, buffers));
        }
        let Some((_, rates, buffer_range)) = self.audio_caps.clone() else { return };
        let info = self.engine.as_ref().map(|e| e.info().clone());
        let rate = info.as_ref().map_or(48_000, |i| i.sample_rate);
        let mut open = self.show_prefs;
        let mut restart = false;
        let mut prefs_changed = false;
        let decks = self.deck_count();
        egui::Window::new("Preferences").open(&mut open).default_width(560.0).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for tab in PrefsTab::ALL {
                        ui.selectable_value(
                            &mut self.prefs_tab,
                            tab,
                            RichText::new(tab.label()).strong(),
                        );
                    }
                });
                ui.separator();
                match self.prefs_tab {
                    PrefsTab::Deck => {
                        prefs_changed |= deck_prefs(ui, &mut self.config.prefs, decks);
                        return;
                    }
                    PrefsTab::Mixer => {
                        prefs_changed |= mixer_prefs(ui, &mut self.config.prefs);
                        return;
                    }
                    PrefsTab::Audio => {}
                }
                egui::Grid::new("audio settings").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Output");
                let current = info.as_ref().map_or("No audio".to_owned(), |i| i.device.clone());
                egui::ComboBox::from_id_salt("audio device").selected_text(current).width(240.0).show_ui(ui, |ui| {
                    for name in Engine::output_devices() {
                        if ui.selectable_label(device.as_deref() == Some(&name), &name).clicked() {
                            self.config.audio_device = Some(name);
                            restart = true;
                        }
                    }
                });
                ui.end_row();

                ui.label("Sample rate");
                let label = |r: Option<u32>| r.map_or("Device default".to_owned(), |r| format!("{:.1} kHz", f64::from(r) / 1000.0));
                egui::ComboBox::from_id_salt("sample rate").selected_text(label(self.config.sample_rate)).show_ui(ui, |ui| {
                    for option in std::iter::once(None).chain(rates.iter().copied().map(Some)) {
                        if ui.selectable_label(self.config.sample_rate == option, label(option)).clicked() {
                            self.config.sample_rate = option;
                            restart = true;
                        }
                    }
                });
                ui.end_row();

                ui.label("Buffer size");
                let label = |frames: Option<u32>| {
                    frames.map_or("Device default".to_owned(), |f| {
                        format!("{f} samples ({:.1} ms)", f64::from(f) * 1000.0 / f64::from(rate))
                    })
                };
                egui::ComboBox::from_id_salt("buffer").selected_text(label(self.config.buffer_frames)).width(180.0).show_ui(ui, |ui| {
                    let fits = |f: &u32| buffer_range.is_none_or(|(min, max)| (min..=max).contains(f));
                    for option in std::iter::once(None).chain(BUFFERS.into_iter().filter(fits).map(Some)) {
                        if ui.selectable_label(self.config.buffer_frames == option, label(option)).clicked() {
                            self.config.buffer_frames = option;
                            restart = true;
                        }
                    }
                });
                ui.end_row();

                ui.label("Quality");
                ui.horizontal(|ui| {
                    let mut quality = self.config.audio_quality;
                    ui.selectable_value(&mut quality, AudioQuality::Standard, "Standard")
                        .on_hover_text("Linear interpolation: least CPU");
                    ui.selectable_value(&mut quality, AudioQuality::High, "High")
                        .on_hover_text("Cubic interpolation: cleaner highs when the tempo or sample rate differs");
                    if quality != self.config.audio_quality {
                        self.config.audio_quality = quality;
                        self.config.save();
                        if let Some(engine) = self.engine.as_mut() {
                            engine.send(Command::SetQuality(quality.engine()));
                        }
                    }
                });
                ui.end_row();
            });
                self.recording_prefs(ui);
                ui.separator();
                if let Some(info) = &info {
                    let buffer =
                        info.latency_ms().map_or("device default buffer".to_owned(), |ms| {
                            format!(
                                "{} samples = {ms:.1} ms",
                                info.buffer_frames.unwrap_or_default()
                            )
                        });
                    ui.label(format!(
                        "Running: {} Hz · {} channels · {buffer}",
                        info.sample_rate, info.channels
                    ));
                }
                ui.label(
                RichText::new("Smaller buffers make the controller respond sooner; raise the buffer if you hear crackles or dropouts.")
                    .small()
                    .weak(),
            );
            },
        );
        self.show_prefs = open;
        if prefs_changed {
            self.config.save();
            self.apply_preferences();
        }
        if restart {
            self.start_engine(self.config.audio_device.clone());
        }
    }

    fn controller_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_controller;
        egui::Window::new("Controller").open(&mut open).default_width(560.0).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let current = self.controller.port().unwrap_or("Not connected").to_owned();
                    let mut chosen = None;
                    egui::ComboBox::from_id_salt("midi")
                        .selected_text(current)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for port in input_ports() {
                                if ui.selectable_label(false, &port).clicked() {
                                    chosen = Some(port);
                                }
                            }
                        });
                    if let Some(port) = chosen {
                        self.connect_controller(&port, ctx);
                    }
                    if self.controller.port().is_some() && ui.button("Disconnect").clicked() {
                        self.decline_controller();
                    }
                    if self.controller.port().is_some() {
                        ui.label(if self.controller.has_output() {
                            "LEDs: on"
                        } else {
                            "LEDs: no output port"
                        });
                    }
                });
                if let Some(err) = &self.controller.error {
                    ui.colored_label(WARN, err);
                }
                if self.controller.port().is_none() {
                    ui.label("Connect the controller by USB, then pick its MIDI port above.");
                    return;
                }

                ui.separator();
                ui.label(RichText::new("Automatic mapping").strong());
                let extra = vec![crate::config::extra_mapping_dir()];
                let files = rekordbox::mapping_files_with(&extra);
                if files.is_empty() {
                    ui.label("No rekordbox installation found; map controls with Learn below.");
                } else {
                    ui.horizontal(|ui| {
                    let suggested = self
                        .controller
                        .port()
                        .and_then(|p| rekordbox::find_for_port_with(p, &extra))
                        .and_then(|(name, _)| files.iter().position(|(n, _)| *n == name));
                    let choice = self.import_choice.or(suggested);
                    let text = choice.map_or("Choose controller…", |i| files[i].0.as_str());
                    egui::ComboBox::from_id_salt("import").selected_text(text).width(220.0).show_ui(ui, |ui| {
                        for (i, (name, _)) in files.iter().enumerate() {
                            if ui.selectable_label(choice == Some(i), name).clicked() {
                                self.import_choice = Some(i);
                            }
                        }
                    });
                    if let Some(i) = choice
                        && ui.button("Import from rekordbox").on_hover_text("Replace the mapping with rekordbox's mapping for this controller").clicked()
                    {
                        self.controller.import_from(&files[i].1);
                    }
                    if let Some(i) = choice
                        && ui
                            .button("Add missing only")
                            .on_hover_text("Keep your mapping; add only functions and lights it does not have yet (on controls not used already)")
                            .clicked()
                    {
                        self.controller.import_missing_from(&files[i].1);
                    }
                });
                    ui.label(RichText::new("Reads the mapping file of your installed rekordbox; nothing is copied into RofLoxBox.").small().weak());
                }
                ui.horizontal(|ui| {
                if ui
                    .button("Import mapping file…")
                    .on_hover_text("Use a rekordbox .midi.csv mapping, e.g. from rekordbox 6 or another computer")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new().add_filter("rekordbox mapping", &["csv"]).pick_file()
                {
                    self.controller.import_from(&path);
                }
                if ui
                    .button("Open mappings folder")
                    .on_hover_text("Put rekordbox .midi.csv files here (e.g. from rekordbox 6) to use them automatically")
                    .clicked()
                {
                    let dir = crate::config::extra_mapping_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    let opener = if cfg!(windows) { "explorer" } else { "open" };
                    let _ = std::process::Command::new(opener).arg(&dir).spawn();
                }
            });
                if let Some(notice) = &self.controller.notice {
                    ui.label(RichText::new(notice).small());
                }

                ui.separator();
                ui.horizontal(|ui| {
                let count = self.controller.mapping().shift_controls.len();
                ui.label(format!("Shift buttons: {}", if count == 0 { "none".to_owned() } else { count.to_string() }))
                    .on_hover_text("While a Shift button is held, functions marked Shift take over their control");
                if ui.small_button("Learn Shift button").clicked() {
                    self.controller.learn_shift();
                }
                if count > 0 && ui.small_button("Clear").clicked() {
                    self.controller.clear_shift_controls();
                }
            });
                match self.controller.learning() {
                    Some(action) => {
                        ui.horizontal(|ui| {
                        ui.colored_label(
                            CUE_COLOR,
                            format!("Learning {action}: press or move that control (hold Shift for a Shift combination)…"),
                        );
                        if ui.button("Cancel").clicked() {
                            self.controller.cancel_learn();
                        }
                    });
                    }
                    None if self.controller.learning_shift() => {
                        ui.horizontal(|ui| {
                            ui.colored_label(CUE_COLOR, "Press the button to use as Shift…");
                            if ui.button("Cancel").clicked() {
                                self.controller.cancel_learn();
                            }
                        });
                    }
                    None => {
                        ui.label("To change a control: press Learn next to a function, then press or move it on the controller. Hold a Shift button while doing so to make a Shift combination.");
                    }
                }
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.action_filter)
                            .hint_text("Filter functions, e.g. deck1 or loop")
                            .desired_width(260.0),
                    );
                    ui.label(format!(
                        "{} controls, {} lights mapped",
                        self.controller.mapping().bindings.len(),
                        self.controller.mapping().outputs.len()
                    ));
                });
                let filter = self.action_filter.to_lowercase();
                ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    egui::Grid::new("bindings").striped(true).num_columns(5).show(ui, |ui| {
                        for action in
                            Action::all().into_iter().filter(|a| a.to_string().contains(&filter))
                        {
                            ui.label(action.to_string());
                            let bindings: Vec<_> = self
                                .controller
                                .mapping()
                                .bindings
                                .iter()
                                .filter(|b| b.action == action)
                                .cloned()
                                .collect();
                            if bindings.is_empty() {
                                ui.weak("—");
                            } else {
                                let text: Vec<String> = bindings
                                    .iter()
                                    .map(|b| {
                                        let shift = if b.shift { "Shift + " } else { "" };
                                        format!(
                                            "{shift}{:?} ch{} #{:02X}",
                                            b.message, b.channel, b.number
                                        )
                                    })
                                    .collect();
                                ui.monospace(text.join(", "));
                            }
                            ui.horizontal(|ui| {
                                if ui.small_button("Learn").clicked() {
                                    self.controller.learn(action);
                                }
                                if !bindings.is_empty()
                                    && ui
                                        .small_button("×")
                                        .on_hover_text("Remove the binding")
                                        .clicked()
                                {
                                    self.controller.unbind(action);
                                }
                            });
                            if let (Some(b), ActionKind::Absolute) =
                                (bindings.first(), action.kind())
                            {
                                let mut invert = b.invert;
                                if ui.checkbox(&mut invert, "invert").changed() {
                                    self.controller.set_inverted(action, invert);
                                }
                            } else {
                                ui.label("");
                            }
                            if let Some(b) = bindings.first() {
                                let mut shift = b.shift;
                                if ui
                                    .checkbox(&mut shift, "Shift")
                                    .on_hover_text("Only while a Shift button is held")
                                    .changed()
                                {
                                    self.controller.set_shifted(action, shift);
                                }
                            } else {
                                ui.label("");
                            }
                            ui.end_row();
                        }
                    });
                });
                ui.collapsing("MIDI monitor", |ui| {
                    for line in &self.controller.log {
                        ui.monospace(line);
                    }
                });
            },
        );
        self.show_controller = open;
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = self.screenshot.as_mut() else { return };
        let ready = self.decks.iter().all(|d| d.loading_title().is_none());
        if ready && !shot.requested && Instant::now() >= shot.not_before {
            shot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            if let Err(err) = save_png(&shot.path, &image) {
                eprintln!("screenshot: {err:#}");
            }
            self.finish_recording();
            std::process::exit(0);
        }
        ctx.request_repaint();
    }
}

/// A beat count as shown on jump buttons: 1/4, 1/2, 1, 2, ….
fn beats_label(beats: f64) -> String {
    if beats >= 1.0 { format!("{beats}") } else { format!("1/{}", (1.0 / beats).round()) }
}

/// Hot cues of a track as waveform markers.
fn hot_cue_marks(track: &crate::deck::LoadedTrack) -> Vec<HotCueMark> {
    track
        .hot_cues
        .iter()
        .enumerate()
        .filter_map(|(i, cue)| cue.map(|c| (c.seconds, c.color, char::from(b'A' + i as u8))))
        .collect()
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, image.size[0] as u32, image.size[1] as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let pixels: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
    encoder.write_header()?.write_image_data(&pixels)?;
    Ok(())
}

/// An output device with its sample rates and buffer size range.
type AudioCaps = (Option<String>, Vec<u32>, Option<(u32, u32)>);

/// What the ◀ ▶ buttons above the pads currently set.
fn pad_parameter_caption(deck: &Deck, fx: &Effects, shift: bool) -> String {
    let beats = crate::effects::beat_label;
    match deck.pad_mode {
        PadMode::HotCue => "MEMORY".into(),
        PadMode::Keyboard => format!("{:+}…{:+}", deck.keyboard_base, deck.keyboard_base + 7),
        PadMode::KeyShift => format!("{:+}…{:+}", deck.key_shift_base, deck.key_shift_base + 7),
        PadMode::PadFx => if deck.pad_fx_page == 0 { "A–H" } else { "I–P" }.into(),
        PadMode::BeatJump => format!(
            "{}…{} beats{}",
            beats(deck.jump_range),
            beats(8.0 * deck.jump_range),
            if shift { " ×16" } else { "" }
        ),
        PadMode::BeatLoop => {
            format!("{}…{}", beats(deck.pad_loop_beats(0)), beats(deck.pad_loop_beats(7)))
        }
        PadMode::Sampler => format!("BANK {}", fx.bank + 1),
        PadMode::Stems => "—".into(),
    }
}

fn pad_mode_short(mode: PadMode) -> &'static str {
    match mode {
        PadMode::HotCue => "CUE",
        PadMode::Keyboard => "KBD",
        PadMode::Stems => "STM",
        PadMode::PadFx => "FX",
        PadMode::BeatJump => "JUMP",
        PadMode::BeatLoop => "LOOP",
        PadMode::Sampler => "SMP",
        PadMode::KeyShift => "KEY",
    }
}

/// How an on-screen pad looks in the deck's current pad mode.
struct PadFace {
    label: String,
    fill: Color32,
    hover: String,
}

fn pad_face(
    deck: &Deck,
    pad: usize,
    shift: bool,
    fx: &Effects,
    engine: &Engine,
    index: usize,
) -> PadFace {
    let idle = Color32::from_gray(40);
    let lit = |c: Color32| c.gamma_multiply(0.75);
    let face = |label: String, fill: Color32, hover: &str| PadFace {
        label,
        fill,
        hover: hover.to_owned(),
    };
    let track = deck.track.as_ref();
    match deck.pad_mode {
        PadMode::HotCue => {
            let hot = track.and_then(|t| t.hot_cues[pad]);
            let fill = hot.map_or(idle, |c| lit(c.color));
            face(
                char::from(b'A' + pad as u8).to_string(),
                fill,
                "Hot cue: set or jump; Shift / right-click deletes",
            )
        }
        PadMode::Keyboard if shift && pad >= 6 => face(
            if pad == 6 { "◀ PAGE" } else { "PAGE ▶" }.into(),
            idle,
            "Pitches of the pads down / up a page",
        ),
        PadMode::Keyboard => match deck.keyboard_cue {
            None => {
                let hot = track.and_then(|t| t.hot_cues[pad]);
                face(
                    char::from(b'A' + pad as u8).to_string(),
                    hot.map_or(idle, |c| lit(c.color)),
                    "Pick the hot cue to play (an empty pad sets one here)",
                )
            }
            Some(cue) => {
                let semis = deck.keyboard_base + pad as i32;
                let color = track.and_then(|t| t.hot_cues[cue]).map_or(ACTIVE, |c| c.color);
                let fill =
                    if deck.key_shift == semis { color } else { lit(color).gamma_multiply(0.5) };
                face(
                    format!("{semis:+}"),
                    fill,
                    "Play the hot cue at this pitch; Shift + 7 / 8 changes page",
                )
            }
        },
        PadMode::Stems => face("—".into(), idle, "Stems are not available yet"),
        PadMode::PadFx => {
            let slot = (pad + if shift { 8 } else { 0 } + 8 * usize::from(deck.pad_fx_page)) % 16;
            let held = fx.pad == Some((index, slot as u8));
            face(
                crate::effects::pad_fx_short(slot),
                if held { ACTIVE } else { Color32::from_rgb(70, 45, 95) },
                &format!(
                    "Hold for {}; Shift for the second set",
                    crate::effects::pad_fx_label(slot)
                ),
            )
        }
        PadMode::BeatJump if shift && pad >= 6 => face(
            if pad == 6 { "− BEATS" } else { "+ BEATS" }.into(),
            idle,
            "Fewer / more beats on the jump pads",
        ),
        PadMode::BeatJump => {
            let beats = BEAT_JUMPS[pad].abs() * deck.jump_range;
            let label = crate::effects::beat_label(beats);
            let label =
                if BEAT_JUMPS[pad] < 0.0 { format!("◀ {label}") } else { format!("{label} ▶") };
            face(
                label,
                Color32::from_rgb(40, 70, 100),
                "Jump by this many beats; Shift + 7 / 8 changes the amounts",
            )
        }
        PadMode::BeatLoop => {
            let beats = deck.pad_loop_beats(pad);
            let active = deck.active_loop_beats().is_some_and(|b| (b - beats).abs() < 1e-6);
            face(
                crate::effects::beat_label(beats),
                if active { CUE_COLOR } else { Color32::from_rgb(90, 70, 30) },
                "Loop this many beats; press again to exit",
            )
        }
        PadMode::Sampler => {
            let slot = fx.sampler_slot(index, pad as u8);
            let sample = fx.samples[slot].as_ref();
            let fill = if engine.sampler_playing(slot) {
                ACTIVE
            } else if sample.is_some() {
                Color32::from_rgb(40, 90, 70)
            } else {
                idle
            };
            let hover = sample
                .map_or("Empty: Shift / right-click loads the selected track".to_owned(), |s| {
                    format!("{}; Shift / right-click stops", s.name)
                });
            face((slot + 1).to_string(), fill, &hover)
        }
        PadMode::KeyShift if shift && pad >= 6 => face(
            if pad == 6 { "◀ PAGE" } else { "PAGE ▶" }.into(),
            idle,
            "Pitches of the pads down / up a page",
        ),
        PadMode::KeyShift => {
            let semis = deck.key_shift_base + pad as i32;
            let fill = if deck.key_shift == semis { ACTIVE } else { Color32::from_rgb(60, 50, 90) };
            face(
                format!("{semis:+}"),
                fill,
                "Shift the key to this many semitones; Shift + 7 / 8 changes page",
            )
        }
    }
}

/// A button lit while `on`.
fn toggle(ui: &mut Ui, on: bool, label: &str) -> egui::Response {
    crate::widgets::lit_button(ui, label, on, ACTIVE)
}

/// A button lit in its own colour while `on`.
fn toggle_in(ui: &mut Ui, on: bool, label: &str, accent: Color32) -> egui::Response {
    crate::widgets::lit_button(ui, label, on, accent)
}
