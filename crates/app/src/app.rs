//! The main window.

mod collection_view;
mod deck_view;
mod explorer_view;
mod history;
mod hotplug;
mod keyboard;
mod library_tree;
mod mix_editor;
mod recording;
mod selection;
mod session;
mod status_bar;
mod stems_view;

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
use crate::deck::{AT_CUE, BEAT_JUMPS, Deck, DeckPrefs, HOT_CUES, display_key, key_sync_shift};
use crate::effects::Effects;
use crate::library::find_exports;
use crate::prefs_view::{PrefsTab, appearance_prefs, deck_prefs, mixer_prefs};
use collection_view::CollectionView;
use deckhand_library::Collection;

/// Which library the browser shows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// deckhand's own collection.
    Collection,
    /// An opened rekordbox export, by its root.
    Usb(PathBuf),
    /// A folder of the computer (the explorer).
    Explorer,
}
use crate::waveform::{
    HotCueMark, Markers, WaveformStyle, detail_time, draw_detail, overview_time,
};
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
/// Mixer sizes: a channel strip's narrowest width, the gap between strips,
/// and the master/phones column with its gap.
const MIXER_STRIP: f32 = 50.0;
const MIXER_SPACING: f32 = 4.0;
const MIXER_MASTER: f32 = 70.0;
/// The parts in the master/phones column.
const MIXER_MASTER_PARTS: [crate::parts::MixerPart; 4] = [
    crate::parts::MixerPart::MasterLevel,
    crate::parts::MixerPart::MasterMeter,
    crate::parts::MixerPart::PhonesMix,
    crate::parts::MixerPart::PhonesLevel,
];
/// How often synced decks are checked for phase drift (the same as when
/// an edited session is synced again).
const DRIFT_CHECK: Duration =
    Duration::from_millis((deckhand_audio::SYNC_CHECK_SECS * 1000.0) as u64);
/// Phase error (in beats) that sync corrects.
const DRIFT_LIMIT: f64 = deckhand_audio::SYNC_DRIFT_BEATS;
/// Auto master: below this gain (about −30 dB) the master has left the mix…
const OUT_OF_MIX: f32 = 0.03;
/// …and a deck must be above this one (about −20 dB) to take over, so a
/// fade halfway does not pass MASTER back and forth.
const IN_MIX: f32 = 0.1;

/// A screen block dropped on an edge of another.
type ScreenMove = (
    crate::screen_layout::ScreenBlock,
    crate::screen_layout::ScreenBlock,
    crate::screen_layout::Edge,
);

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
    /// The master deck has been heard in the mix since it became master;
    /// only then does leaving the mix pass MASTER on.
    master_heard: Option<usize>,
    last_drift_check: Instant,
    /// The sync state and phase offsets last told to the engine, for
    /// recorded sessions.
    sync_sent: Option<(Option<usize>, [bool; DECKS])>,
    offsets_sent: [Option<f64>; DECKS],
    crossfader: f32,
    master_volume: f32,
    headphone_mix: f32,
    headphone_volume: f32,
    /// Opened rekordbox exports (USB sticks and folders), in tree order.
    libraries: Vec<crate::library::UsbLibrary>,
    library_error: Option<String>,
    tree: library_tree::LibraryTree,
    source: Source,
    collection: Collection,
    collection_view: CollectionView,
    explorer: explorer_view::ExplorerView,
    search: String,
    selected: Option<u32>,
    /// More tracks selected along with `selected` (see `selection`).
    marked: std::collections::HashSet<u32>,
    focus: Focus,
    controller: Controller,
    show_controller: bool,
    show_prefs: bool,
    prefs_tab: PrefsTab,
    effects: Effects,
    status_bar: status_bar::StatusBar,
    /// The deck whose track is being dragged off it.
    deck_drag: Option<usize>,
    /// Deck elements can be dragged around.
    editing_layout: bool,
    /// Name typed for saving the deck layout as a preset.
    preset_name: String,
    /// How the last preset export or import went.
    preset_note: Option<String>,
    moved_window: bool,
    hotplug: hotplug::Hotplug,
    /// Each deck's jog bend at the last frame, to notice it ending.
    was_bending: [bool; DECKS],
    /// F12: window and screen details for troubleshooting.
    show_diagnostics: bool,
    /// The master being recorded.
    recording: Option<deckhand_audio::Recording>,
    /// A session being replayed into a WAV.
    replay: Option<recording::Replay>,
    /// PERFORMANCE or EDIT (a recorded set as a little DAW).
    mode: mix_editor::Mode,
    edit_view: mix_editor::EditView,
    /// The deck each side of the controller drives (left 1/3, right 2/4).
    active_decks: [usize; 2],
    keyboard: keyboard::KeyboardMixer,
    /// Separates tracks into stems in the background.
    stems: crate::stems::Separator,
    /// Separates loaded tracks for live stems, apart from the queue of
    /// tracks being made.
    live_stems: crate::stems::LiveSeparator,
    /// The stem cache's size, and when it was measured (for Preferences).
    stem_cache_size: Option<(Instant, u64)>,
    /// Each deck's position for its live separation (see `LiveJob::playhead`).
    stem_playheads: [std::sync::Arc<std::sync::atomic::AtomicU64>; DECKS],
    /// The model the live separator was last asked to have ready.
    stems_warmed: Option<crate::stems::StemModel>,
    /// The track each deck's stems were asked for.
    stems_for: [Option<PathBuf>; DECKS],
    /// The models in the models folder, and the ones that could not be read.
    stem_models: (Vec<crate::stems::StemModel>, Vec<String>),
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
    /// Playing time per deck, for the history.
    played: [history::Played; DECKS],
    history_tick: Instant,
    /// Hot cue pads held with the mouse, for gate mode.
    pad_held: [[Option<DeckAction>; HOT_CUES]; DECKS],
    seconds_visible: f64,
    status: Option<String>,
    screenshot: Option<Screenshot>,
    /// Test aid: the window asks to close at this time.
    close_at: Option<Instant>,
    /// Test aid (`DECKHAND_AUTOPLAY`): start every loaded deck once its
    /// track is in.
    autoplay: bool,
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
        let mut config = Config::load();
        // Left over from a run that didn't quit normally.
        if !config.prefs.keep_stems {
            crate::stems::clear_cache();
        }
        crate::widgets::apply_theme(&cc.egui_ctx, config.prefs.theme, config.prefs.text_scale);
        let keyboard = keyboard::KeyboardMixer::new(&config.prefs.key_bindings);
        if let Some(root) = std::env::var_os("DECKHAND_LIBRARY") {
            config.library_root = Some(root.into());
        }
        if std::env::var_os("DECKHAND_FOUR_DECKS").is_some() {
            config.four_decks = true;
        }
        if std::env::var_os("DECKHAND_VERTICAL").is_some() {
            config.vertical_waveforms = true;
        }
        // Test aid: a ready-made screen arrangement by number.
        if let Some(layout) = std::env::var("DECKHAND_SCREEN")
            .ok()
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| crate::screen_layout::ScreenLayout::built_in().into_iter().nth(n))
        {
            config.screen_layout = layout.1;
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
            master_heard: None,
            last_drift_check: Instant::now(),
            sync_sent: None,
            offsets_sent: [None; DECKS],
            crossfader: 0.0,
            master_volume: 0.8,
            headphone_mix: 0.0,
            headphone_volume: 0.8,
            libraries: Vec::new(),
            library_error: None,
            tree: Default::default(),
            source: Source::Collection,
            collection: Collection::default(),
            collection_view: CollectionView::default(),
            explorer: Default::default(),
            search: String::new(),
            selected: None,
            marked: Default::default(),
            focus: Focus::Tracks,
            controller: Controller::default(),
            show_controller: std::env::var_os("DECKHAND_SHOW_CONTROLLER").is_some(),
            show_prefs: std::env::var_os("DECKHAND_SHOW_AUDIO").is_some()
                || std::env::var_os("DECKHAND_SHOW_PREFS").is_some()
                || std::env::var_os("DECKHAND_SHOW_LIBRARY_PREFS").is_some(),
            prefs_tab: if std::env::var_os("DECKHAND_SHOW_MIXER_PREFS").is_some() {
                PrefsTab::Mixer
            } else if std::env::var_os("DECKHAND_SHOW_KEYBOARD_PREFS").is_some() {
                PrefsTab::Keyboard
            } else if std::env::var_os("DECKHAND_SHOW_STEMS_PREFS").is_some() {
                PrefsTab::Stems
            } else if std::env::var_os("DECKHAND_SHOW_LIBRARY_PREFS").is_some() {
                PrefsTab::Library
            } else if std::env::var_os("DECKHAND_SHOW_AUDIO").is_some() {
                PrefsTab::Audio
            } else {
                PrefsTab::default()
            },
            effects: Effects::default(),
            status_bar: status_bar::StatusBar::default(),
            deck_drag: None,
            editing_layout: std::env::var_os("DECKHAND_EDIT_LAYOUT").is_some(),
            reveal_selected: false,
            reveal_playlist: false,
            selected_position: None,
            active_decks: [0, 1],
            keyboard,
            stems: Default::default(),
            live_stems: Default::default(),
            stem_cache_size: None,
            stem_playheads: Default::default(),
            stems_warmed: None,
            stems_for: Default::default(),
            stem_models: crate::stems::load_models(&crate::stems::models_dir()),
            recording: None,
            replay: None,
            // Test aid: start in EDIT mode with this session.
            mode: if std::env::var_os("DECKHAND_EDIT_SESSION").is_some() {
                mix_editor::Mode::Edit
            } else {
                mix_editor::Mode::Performance
            },
            edit_view: mix_editor::EditView::default(),
            moved_window: false,
            hotplug: Default::default(),
            was_bending: [false; DECKS],
            show_diagnostics: std::env::var_os("DECKHAND_DIAGNOSTICS").is_some(),
            session_checked: Instant::now(),
            started: Instant::now(),
            audio_caps: None,
            action_filter: String::new(),
            import_choice: None,
            cue_held: [false; DECKS],
            played: Default::default(),
            history_tick: Instant::now(),
            pad_held: [[None; HOT_CUES]; DECKS],
            seconds_visible: keyboard::DEFAULT_SECONDS_VISIBLE,
            status: None,
            close_at: std::env::var("DECKHAND_CLOSE_AFTER")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(|s| Instant::now() + Duration::from_secs(s)),
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
            autoplay: std::env::var_os("DECKHAND_AUTOPLAY").is_some(),
            preset_name: String::new(),
            preset_note: None,
            config,
        };
        app.restore_session();
        // Test aid: record into this folder from the start.
        let record_to = std::env::var_os("DECKHAND_RECORD").map(PathBuf::from);
        if let Some(dir) = &record_to {
            app.config.recordings_dir = Some(dir.clone());
        }
        app.effects.load_samples(&crate::config::samples_dir());
        // Test aid: analyze tracks without a grid or key from the start.
        if std::env::var_os("DECKHAND_ANALYZE").is_some() {
            app.analyze_missing();
        }
        // Test aid: the beat-grid edit controls open.
        if std::env::var_os("DECKHAND_GRID_EDIT").is_some() {
            app.decks.iter_mut().for_each(|d| d.grid_edit = true);
        }
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
        // Every plugged-in export, and the last one used (it may be a folder).
        let last = app.config.library_root.clone().filter(|r| r.join("PIONEER").is_dir());
        for root in find_exports().into_iter().chain(last.clone()) {
            app.open_usb(root, false);
        }
        // Start in the collection when it has music, or when there is no export.
        let usb = last
            .filter(|r| app.libraries.iter().any(|l| &l.root == r))
            .or_else(|| app.libraries.first().map(|l| l.root.clone()));
        match usb {
            Some(root) if !(prefer_collection && !app.collection.tracks.is_empty()) => {
                app.set_source(Source::Usb(root));
            }
            _ => app.set_source(Source::Collection),
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
        // Test aid: this folder shown in the explorer.
        if let Some(folder) = std::env::var_os("DECKHAND_EXPLORER") {
            app.explorer.open = true;
            app.open_explorer_folder(folder.into());
        }
        if app.config.prefs.rekordbox_at_start {
            app.rekordbox_at_start();
        }
        // Test aid: start with the log open.
        app.status_bar.log_open = std::env::var_os("DECKHAND_SHOW_LOG").is_some();
        if app.screenshot.is_some() || app.autoplay {
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

    fn set_source(&mut self, source: Source) {
        if self.source == source {
            return;
        }
        self.selected = None;
        self.marked.clear();
        self.config.use_collection = source == Source::Collection;
        if let Source::Usb(root) = &source {
            self.config.library_root = Some(root.clone());
        }
        self.source = source;
        self.config.save();
    }

    fn load_selected(&mut self, deck: usize, ctx: &egui::Context) {
        if self.source == Source::Collection {
            if let Some(id) = self.selected {
                self.load_from_collection(id, deck, ctx);
            }
            return;
        }
        if self.source == Source::Explorer {
            if let Some(index) = self.selected {
                self.load_from_explorer(index, deck, ctx);
            }
            return;
        }
        if self.load_blocked(deck) {
            return;
        }
        let Some(id) = self.selected else { return };
        self.mark_double(deck, id, false);
        // Borrowing fields one by one, so a deck can be changed meanwhile.
        let Source::Usb(root) = &self.source else { return };
        let Some(library) = self.libraries.iter().find(|l| &l.root == root) else { return };
        let Some(track) = library.library.track(id) else { return };
        self.decks[deck].start_load(track, library, ctx);
    }

    /// Takes a deck's track off, after saving its cue and grid edits.
    fn eject_deck(&mut self, index: usize) {
        if self.load_blocked(index) {
            return;
        }
        self.save_track_edits();
        let Some(engine) = self.engine.as_mut() else { return };
        self.decks[index].eject(engine);
        if self.master == Some(index)
            && !(self.config.prefs.auto_master && self.hand_over_master(index))
        {
            self.master = None;
        }
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

    /// Sets what a Pad FX pad does and keeps it in the settings.
    pub(super) fn set_pad_fx(&mut self, slot: usize, pad: crate::effects::PadFxSlot) {
        let p = &mut self.config.prefs;
        if p.pad_fx.len() < crate::effects::PAD_FX_SLOTS {
            p.pad_fx =
                (0..crate::effects::PAD_FX_SLOTS).map(|i| self.effects.pad_slot(i)).collect();
        }
        p.pad_fx[slot] = pad;
        self.effects.set_pad_slots(&p.pad_fx);
        self.config.save();
    }

    fn apply_preferences(&mut self) {
        if self.config.prefs.tempo_ranges.is_empty() {
            self.config.prefs.tempo_ranges = crate::deck::TempoRange::DEFAULTS.to_vec();
        }
        let p = self.config.prefs.clone();
        self.effects.set_pad_slots(&p.pad_fx);
        // A range taken off the list: stopped decks move to the nearest one;
        // a playing deck keeps its range (and tempo) until changed.
        if let Some(engine) = self.engine.as_mut() {
            for (index, deck) in self.decks.iter_mut().enumerate() {
                if !p.tempo_ranges.contains(&deck.tempo_range) && !engine.is_playing(index) {
                    let nearest = deck.tempo_range.nearest(&p.tempo_ranges);
                    deck.set_tempo_range(nearest, engine);
                }
            }
        }
        for (index, deck) in self.decks.iter_mut().enumerate() {
            deck.prefs = DeckPrefs {
                memory_call_lock: p.memory_call_lock,
                key_notation: p.key_notation,
                call_resizes_loop: p.call_resizes_loop,
                gate_while_playing: p.gate_while_playing,
                start_at_first_cue: p.start_at_first_cue,
                auto_cue_db: p.auto_cue[index].then_some(p.auto_cue_level_db),
                isolator: p.isolator,
                scratch_sensitivity: f64::from(p.scratch_sensitivity),
                bend_sensitivity: f64::from(p.bend_sensitivity),
                vinyl_start: f64::from(p.vinyl_start),
                vinyl_brake: f64::from(p.vinyl_brake),
                analysis: p.analysis,
                analyze_on_load: p.analyze_on_load,
                auto_gain: p.auto_gain,
                waveform_detail: p.waveform_detail,
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

    /// Applying, saving and deleting deck layout presets.
    fn layout_presets_menu(&mut self, ui: &mut Ui, four: bool) {
        use crate::layout::LayoutPreset;
        let current = self.config.deck_layout(four).clone();
        let view = self.config.view();
        let is_current = |preset: &LayoutPreset| {
            preset.layout == current && preset.view.as_ref().is_none_or(|v| v.matches(&view))
        };
        let mut chosen = None;
        for preset in LayoutPreset::built_in() {
            if ui.selectable_label(is_current(&preset), &preset.name).clicked() {
                chosen = Some(preset);
            }
        }
        let mut delete = None;
        if !self.config.layout_presets.is_empty() {
            ui.separator();
        }
        for (i, preset) in self.config.layout_presets.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.selectable_label(is_current(preset), &preset.name).clicked() {
                    chosen = Some(preset.clone());
                }
                if ui.small_button("×").on_hover_text("Delete this preset").clicked() {
                    delete = Some(i);
                }
            });
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.preset_name)
                    .hint_text("Preset name")
                    .desired_width(140.0),
            );
            let name = self.preset_name.trim().to_owned();
            if ui.add_enabled(!name.is_empty(), egui::Button::new("Save current")).clicked() {
                // Saving under an existing name replaces that preset.
                self.config.layout_presets.retain(|p| p.name != name);
                self.config.layout_presets.push(LayoutPreset {
                    name,
                    layout: current.clone(),
                    view: Some(view.clone()),
                });
                self.preset_name.clear();
                self.config.save();
            }
        });
        ui.horizontal(|ui| {
            let any = !self.config.layout_presets.is_empty();
            if ui
                .add_enabled(any, egui::Button::new("Export presets…"))
                .on_hover_text("Saves your presets to a file, to keep or use on another computer")
                .clicked()
            {
                self.preset_note = Some(export_presets(&self.config.layout_presets));
            }
            if ui
                .button("Import presets…")
                .on_hover_text("Adds the presets in a file; ones with the same name are replaced")
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("Import view presets")
                    .add_filter("RofLoxBox presets", &["json"])
                    .pick_file()
            {
                self.preset_note = Some(match import_presets(&path) {
                    Ok(presets) => {
                        let count = presets.len();
                        for preset in presets {
                            self.config.layout_presets.retain(|p| p.name != preset.name);
                            self.config.layout_presets.push(preset);
                        }
                        self.config.save();
                        format!("Imported {count} preset{}", if count == 1 { "" } else { "s" })
                    }
                    Err(err) => format!("Not imported: {err:#}"),
                });
            }
        });
        if let Some(note) = &self.preset_note {
            ui.label(RichText::new(note).small().weak());
        }
        if let Some(i) = delete {
            self.config.layout_presets.remove(i);
            self.config.save();
        }
        if let Some(preset) = chosen {
            *self.config.deck_layout_mut(four) = preset.layout;
            if let Some(view) = preset.view {
                self.config.set_view(view);
            }
            self.config.save();
            ui.close();
        }
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
            Source::Usb(_) => self.usb_rows(),
            Source::Explorer => self.explorer_rows(),
        };
        rows.into_iter().map(|r| (r.id, r.position)).collect()
    }

    // Sync

    /// The deck others follow: the chosen master if loaded, otherwise a
    /// playing deck that is not itself synced (an audible one first).
    fn master_deck(&self) -> Option<usize> {
        let engine = self.engine.as_ref()?;
        self.master.filter(|&m| self.decks[m].track.is_some()).or_else(|| {
            let free = |d: &usize| {
                self.decks[*d].track.is_some() && !self.decks[*d].sync && engine.is_playing(*d)
            };
            let decks = 0..self.deck_count();
            decks
                .clone()
                .filter(free)
                .find(|&d| self.mix_gain(d) > IN_MIX)
                .or_else(|| decks.clone().find(free))
        })
    }

    /// How loud a deck reaches the mix through its channel fader and the
    /// crossfader (0–1).
    fn mix_gain(&self, deck: usize) -> f32 {
        let d = &self.decks[deck];
        let p = &self.config.prefs;
        deckhand_audio::channel_gain(
            d.volume,
            d.side,
            self.crossfader,
            p.crossfader_curve.engine(),
            p.fader_curve.engine(),
        )
    }

    /// Whether a deck is loaded, playing and louder than `threshold` in the mix.
    fn in_mix(&self, deck: usize, threshold: f32) -> bool {
        self.decks[deck].track.is_some()
            && self.engine.as_ref().is_some_and(|e| e.is_playing(deck))
            && self.mix_gain(deck) > threshold
    }

    /// Auto master: when the master deck leaves the mix (paused, or faded
    /// out with its fader or the crossfader), the loudest other deck in the
    /// mix takes over. Without another one, the master stays. Call every
    /// frame.
    fn update_master(&mut self) {
        // The deck synced decks follow is the master, chosen or not (so it
        // shows as master and can hand over).
        if self.master.is_none() && self.decks.iter().any(|d| d.sync) {
            self.master = self.master_deck();
        }
        if !self.config.prefs.auto_master {
            return;
        }
        let Some(current) = self.master else { return };
        let gains = self.playing_gains();
        // A master chosen while silent (a track being prepared) keeps it
        // until it has been heard.
        if gains[current].is_some_and(|g| g > IN_MIX) {
            self.master_heard = Some(current);
        }
        if let Some(next) = next_master(&gains, current, self.master_heard == Some(current)) {
            self.set_master_from(current, next);
        }
    }

    /// Each deck's gain in the mix while it plays a track (`None` otherwise).
    fn playing_gains(&self) -> Vec<Option<f32>> {
        (0..self.deck_count()).map(|d| self.in_mix(d, -1.0).then(|| self.mix_gain(d))).collect()
    }

    /// Makes the loudest deck in the mix (other than `from`) the master.
    fn hand_over_master(&mut self, from: usize) -> bool {
        let Some(next) = loudest_in_mix(&self.playing_gains(), from) else { return false };
        self.set_master_from(from, next);
        true
    }

    /// MASTER pressed: this deck leads from now on. Like on CDJs and in
    /// rekordbox, pressing it on the master changes nothing.
    fn make_master(&mut self, deck: usize) {
        if self.decks[deck].track.is_some() {
            self.master = Some(deck);
        }
    }

    fn set_master_from(&mut self, from: usize, next: usize) {
        self.master = Some(next);
        self.status =
            Some(format!("Deck {} is now the master (deck {} left the mix)", next + 1, from + 1));
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
            engine.send(Command::SyncTempo { deck, ratio: target.tempo_ratio() });
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
                engine.send(Command::SyncShift { deck, seconds });
            }
        }
    }

    fn update_sync(&mut self) {
        self.update_master();
        let master = self.master_deck();
        for (d, deck) in self.decks.iter_mut().enumerate() {
            deck.is_master = master == Some(d);
        }
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
        // For recorded sessions, so an edited one can be synced again: who
        // leads, who follows, and the phase each keeps.
        let synced = std::array::from_fn(|d| self.decks[d].sync);
        let offsets: [f64; DECKS] = std::array::from_fn(|d| self.decks[d].phase_offset);
        let Some(engine) = self.engine.as_mut() else { return };
        if self.sync_sent != Some((master, synced))
            && engine.send(Command::SyncState { master, synced })
        {
            self.sync_sent = Some((master, synced));
        }
        for (deck, offset) in offsets.into_iter().enumerate() {
            if self.offsets_sent[deck] != Some(offset)
                && engine.send(Command::SyncOffset { deck, beats: offset })
            {
                self.offsets_sent[deck] = Some(offset);
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
        // mixer channel controls do not switch layers. Controllers with DECK
        // buttons say so themselves: other messages arriving just after a
        // switch (for the deck left behind) must not switch back.
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
        let deck_buttons = self
            .controller
            .mapping()
            .bindings
            .iter()
            .any(|b| matches!(b.action, Action::Deck(_, A::Select)));
        let switches = if deck_buttons { action == A::Select } else { !mixer };
        if switches && (action != A::Select || pressed) {
            self.active_decks[d % 2] = d;
        }
        if action == A::Select {
            return;
        }
        match (action, value) {
            (A::PlayPause, _) if pressed => return self.play_pause(d),
            (A::Sync, _) if pressed => return self.toggle_sync(d),
            (A::SyncMaster, _) if pressed => return self.make_master(d),
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
                let next = deck.tempo_range.next(&self.config.prefs.tempo_ranges);
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
            (A::StemPart(part), _) if pressed => deck.toggle_stem(usize::from(part), engine),
            (A::PartIsolator, _) if pressed => deck.toggle_part_isolator(engine),
            // Part Isolator: the EQ knobs set the vocal, instrument and drum levels.
            (A::PartLevel(part), Value::Absolute(v)) => {
                deck.isolator[usize::from(part.min(2))] = v;
                deck.apply_stems(engine);
            }
            (A::EqLow | A::EqMid | A::EqHigh, Value::Absolute(v)) if deck.part_isolator => {
                let part = match action {
                    A::EqHigh => 0,
                    A::EqMid => 1,
                    _ => 2,
                };
                deck.isolator[part] = v;
                deck.apply_stems(engine);
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
            Source::Usb(_) => {
                let Some(library) = self.usb() else { return };
                let Some(track) = library.library.track(id) else { return };
                (library.resolve(&track.track.file_path), track.track.title.clone())
            }
            Source::Explorer => {
                let Some(file) = self.explorer.files_get(id) else { return };
                (file.path.clone(), file.title.clone())
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
        self.marked.clear();
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
            Focus::Playlists => {
                // The whole tree: the collection, then every export.
                let mut entries = self.tree_entries();
                let current_entry = self.current_entry();
                let current = entries.iter().position(|e| *e == current_entry);
                if let Some(next) = step(current, entries.len()) {
                    self.select_entry(entries.swap_remove(next));
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
            for part in 0..deckhand_audio::PARTS {
                set(
                    DeckFeedback::StemOn(part as u8),
                    on(deck.has_part(part) && deck.stem_on[part]),
                );
            }
            set(DeckFeedback::PartIsolator, on(deck.part_isolator));
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

impl DeckhandApp {
    /// Saves what is left and leaves.
    pub(super) fn quit(&mut self) -> ! {
        self.save_to_rekordbox_at_quit();
        self.save_session(true);
        self.finish_recording();
        self.config.save();
        if !self.config.prefs.keep_stems {
            crate::stems::clear_cache();
        }
        std::process::exit(0);
    }
}

impl eframe::App for DeckhandApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let performing = self.mode == mix_editor::Mode::Performance;
        if performing {
            self.edit_view.stop_playing();
        }
        // Before anything is drawn, so focused widgets don't take the arrows.
        // Mixing keys only when performing; changing keys in Preferences
        // works in EDIT mode too.
        self.mixing_keys(ctx, performing);
        if performing {
            self.library_keys(ctx);
        }
        // winit's window teardown can trip an AppKit exception on recent macOS;
        // nothing needs cleaning up, so leave directly.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.rekordbox_blocks_quit() {
                // Changes for rekordbox can't be saved while it is open.
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.ask_to_close_rekordbox();
            } else {
                self.quit();
            }
        }
        self.quit_waiting_for_rekordbox(ctx);
        if let Some(at) = self.close_at {
            if Instant::now() >= at {
                self.close_at = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(at - Instant::now());
            }
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.collect_garbage();
            for deck in &mut self.decks {
                deck.poll(engine);
            }
        }
        self.sync_jog_resolution();
        let count = self.deck_count();
        if self.autoplay
            && let Some(engine) = self.engine.as_mut()
        {
            let decks = &mut self.decks[..count];
            if decks.iter().all(|d| d.track.is_some() && d.loading_title().is_none()) {
                for deck in decks {
                    deck.toggle_play(engine);
                }
                self.autoplay = false;
            }
        }
        for event in self.controller.poll() {
            self.apply_event(event, ctx);
        }
        self.save_track_edits();
        self.track_history();
        self.poll_collection(ctx);
        self.poll_stems(ctx);
        self.poll_explorer(ctx);
        self.poll_usb(ctx);
        self.poll_replay(ctx);
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
        let performing = self.mode == mix_editor::Mode::Performance;
        if performing && self.config.show_waveforms {
            let vertical = self.config.vertical_waveforms;
            egui::TopBottomPanel::top("waveforms").show(ctx, |ui| {
                if !vertical {
                    self.waveforms(ui);
                }
                self.waveform_toolbar(ui);
            });
        }
        if performing && self.config.show_fx {
            egui::TopBottomPanel::top("fx").show(ctx, |ui| self.fx_panel(ui));
        }
        self.update_status();
        self.drag_hint(ctx);
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
        if performing && self.config.show_library {
            egui::TopBottomPanel::bottom("library")
                .resizable(true)
                .default_height(if self.config.four_decks { 200.0 } else { 300.0 })
                .show(ctx, |ui| self.library_ui(ui, ctx));
            self.reveal_playlist = false;
        }
        if performing {
            egui::CentralPanel::default().show(ctx, |ui| self.central(ui));
        } else {
            egui::CentralPanel::default().show(ctx, |ui| self.edit_view(ui));
        }
        if self.show_controller {
            self.controller_window(ctx);
        }
        self.duplicate_dialog(ctx);
        self.cleanup_dialog(ctx);
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
                ui.heading(crate::APP_NAME).on_hover_text(format!("Version {}", crate::VERSION));
                ui.separator();
                use mix_editor::Mode;
                ui.selectable_value(&mut self.mode, Mode::Performance, "PERFORMANCE")
                    .on_hover_text("Play: decks, mixer and library");
                ui.selectable_value(&mut self.mode, Mode::Edit, "EDIT")
                    .on_hover_text("Change a recorded set: its tracks and every knob and fader");
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
                ui.toggle_value(&mut self.status_bar.log_open, "📜 Log")
                    .on_hover_text("Every message of this session, with its time");
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
                egui::containers::menu::MenuButton::new("View")
                    .config(
                        egui::containers::menu::MenuConfig::new()
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| {
                        let before = (
                            self.config.vertical_waveforms,
                            self.config.show_waveforms,
                            self.config.show_mixer,
                            self.config.show_library,
                            self.config.show_fx,
                        );
                        ui.label(RichText::new("Waveforms").small().weak());
                        ui.radio_value(
                            &mut self.config.vertical_waveforms,
                            false,
                            "Horizontal (top)",
                        );
                        ui.radio_value(
                            &mut self.config.vertical_waveforms,
                            true,
                            "Vertical (between decks)",
                        );
                        ui.separator();
                        ui.checkbox(&mut self.config.show_waveforms, "Waveforms");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.config.show_mixer, "Mixer");
                            sticky_menu(ui, "parts", |ui| {
                                use crate::parts::{MixerPart, grouped_checkboxes};
                                let hidden = &mut self.config.hidden_mixer_parts;
                                if grouped_checkboxes(
                                    ui,
                                    &MixerPart::GROUPS,
                                    MixerPart::label,
                                    hidden,
                                ) {
                                    self.config.save();
                                }
                            });
                        });
                        ui.checkbox(&mut self.config.show_library, "Library");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.config.show_fx, "FX and sampler");
                            sticky_menu(ui, "parts", |ui| {
                                use crate::parts::{FxPart, grouped_checkboxes};
                                let hidden = &mut self.config.hidden_fx_parts;
                                if grouped_checkboxes(ui, &FxPart::GROUPS, FxPart::label, hidden) {
                                    self.config.save();
                                }
                            });
                        });
                        ui.separator();
                        let four = self.config.four_decks;
                        let which = if four { "4-deck" } else { "2-deck" };
                        sticky_menu(ui, format!("Deck elements ({which} layout)"), |ui| {
                            use crate::layout::DeckElement;
                            let hidden = &mut self.config.deck_layout_mut(four).hidden;
                            let changed = ScrollArea::vertical()
                                .show(ui, |ui| {
                                    crate::parts::grouped_checkboxes(
                                        ui,
                                        &DeckElement::GROUPS,
                                        DeckElement::label,
                                        hidden,
                                    )
                                })
                                .inner;
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
                        sticky_menu(ui, format!("View presets ({which})"), |ui| {
                            self.layout_presets_menu(ui, four);
                        });
                        ui.menu_button("Screen arrangement", |ui| {
                            for (name, layout) in crate::screen_layout::ScreenLayout::built_in() {
                                let current = self.config.screen_layout == layout;
                                if ui.selectable_label(current, name).clicked() {
                                    self.config.screen_layout = layout;
                                    self.config.save();
                                    ui.close();
                                }
                            }
                            ui.label(
                                RichText::new(
                                    "Or move them yourself: Edit layout, then drag their bars.",
                                )
                                .small()
                                .weak(),
                            );
                        });
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
                ui.separator();
                let mut stems = self.config.prefs.live_stems;
                if ui
                    .toggle_value(&mut stems, "STEMS")
                    .on_hover_text(
                        "Stems on the decks. Off: nothing is separated and the separating \
                         helper quits, for more performance",
                    )
                    .changed()
                {
                    self.set_live_stems(stems);
                    self.config.save();
                }
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

    /// One deck's scrolling waveform; dragging it stops the deck and moves
    /// through the track, clicking jumps there.
    fn deck_waveform(&mut self, ui: &mut Ui, index: usize, size: egui::Vec2, vertical: bool) {
        // The zoom is real time: a deck playing faster shows more of its
        // track, so beats of synced decks line up across the whole width.
        let visible = self.seconds_visible * self.decks[index].tempo_ratio().max(0.05);
        let Some(engine) = self.engine.as_mut() else { return };
        let deck = &self.decks[index];
        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
        if self.config.prefs.scroll_zoom && response.hovered() {
            // Scrolling up (or pinching out) shows less, like a wheel notch
            // of the controller's zoom.
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            if scroll != 0.0 || pinch != 1.0 {
                let factor = 1.15f64.powf(-f64::from(scroll) / 50.0) / f64::from(pinch);
                self.seconds_visible = (self.seconds_visible * factor).clamp(2.0, 30.0);
            }
        }
        let painter = ui.painter_at(rect);
        let mut jump = None;
        let mut drag = None;
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
                    drag = Some(f64::from(moved) / f64::from(length) * visible);
                }
                if response.clicked()
                    && let Some(pointer) = response.interact_pointer_pos()
                {
                    let seconds = detail_time(rect, vertical, pointer, position, visible);
                    jump = Some(seconds.clamp(0.0, track.duration));
                }
            }
            None => {
                painter.rect_filled(rect, 0.0, Color32::from_gray(12));
            }
        }
        let deck = &mut self.decks[index];
        if response.drag_started() {
            deck.grab_waveform(engine);
        }
        if let Some(seconds) = drag {
            deck.drag_waveform(seconds, engine);
        }
        if !response.dragged() {
            deck.release_waveform(engine);
        }
        if let Some(seconds) = jump {
            self.decks[index].seek(seconds, engine);
        }
        painter.text(
            rect.left_top() + vec2(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            format!("{}", index + 1),
            egui::FontId::proportional(14.0),
            DECK_COLORS[index],
        );
        self.deck_drop_target(ui, rect, ui.id().with(("waveform drop", index)), index);
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
        use crate::screen_layout::ScreenBlock as B;
        if self.editing_layout {
            ui.horizontal(|ui| {
                ui.colored_label(
                    CUE_COLOR,
                    "Editing the layout: drag a deck element onto another (left or right half), onto + at the end of a row or the side column, onto New row, or onto Hide. Drag the decks, waveforms or mixer by their bar onto an edge of another block.",
                );
                if ui.button("Done").clicked() {
                    self.editing_layout = false;
                }
            });
        }
        let vertical = self.config.vertical_waveforms && self.config.show_waveforms;
        let show_mixer = self.config.show_mixer;
        let shown = |b: B| match b {
            B::LeftWaves | B::RightWaves => vertical,
            B::Mixer => show_mixer,
            B::LeftDecks | B::RightDecks => true,
        };
        let columns: Vec<Vec<B>> = self
            .config
            .screen_layout
            .columns
            .iter()
            .map(|column| column.iter().copied().filter(|&b| shown(b)).collect::<Vec<B>>())
            .filter(|column| !column.is_empty())
            .collect();
        const GAP: f32 = 8.0;
        let side_decks = if self.config.four_decks { 2.0 } else { 1.0 };
        let wave_width = 56.0 * side_decks;
        let mixer_width = self.mixer_width();
        // Columns without decks are as wide as their contents; deck columns
        // share what is left.
        let fixed = |column: &[B]| -> Option<f32> {
            if column.iter().any(|b| matches!(b, B::LeftDecks | B::RightDecks)) {
                return None;
            }
            let waves = column.iter().filter(|b| b.is_waves()).count() as f32;
            let waves_width = waves * wave_width + (waves - 1.0).max(0.0) * 4.0;
            let mixer = if column.contains(&B::Mixer) { mixer_width } else { 0.0 };
            Some(waves_width.max(mixer))
        };
        let fixed_total: f32 = columns.iter().filter_map(|c| fixed(c)).sum();
        let flexible = columns.iter().filter(|c| fixed(c).is_none()).count().max(1) as f32;
        // A margin so the last column is never clipped.
        let gaps = GAP * columns.len() as f32 + 16.0;
        let deck_width = ((ui.available_width() - fixed_total - gaps) / flexible).max(240.0);
        let height = ui.available_height();
        let editing = self.editing_layout;
        let mut moved = None;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for column in &columns {
                let width = fixed(column).unwrap_or(deck_width);
                ui.allocate_ui_with_layout(
                    vec2(width, height),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_width(width);
                        ui.spacing_mut().item_spacing.y = GAP;
                        self.screen_column(ui, column, width, height, editing, &mut moved);
                    },
                );
            }
        });
        if let Some((block, target, edge)) = moved {
            self.config.screen_layout.move_block(block, target, edge);
            self.config.save();
        }
    }

    /// One column of the main screen: its blocks stacked, waveform blocks
    /// next to each other sharing a row, the mixer at its own height.
    fn screen_column(
        &mut self,
        ui: &mut Ui,
        column: &[crate::screen_layout::ScreenBlock],
        width: f32,
        height: f32,
        editing: bool,
        moved: &mut Option<ScreenMove>,
    ) {
        use crate::screen_layout::ScreenBlock as B;
        const GAP: f32 = 8.0;
        let mut groups: Vec<Vec<B>> = Vec::new();
        for &block in column {
            match groups.last_mut() {
                Some(group) if block.is_waves() && group[0].is_waves() => group.push(block),
                _ => groups.push(vec![block]),
            }
        }
        let shared = groups.len() > 1;
        let gaps = GAP * (groups.len() as f32 - 1.0);
        // Sharing a column, the mixer takes what it needs and the rest share
        // what is left (at least a little for the waveforms).
        let mixer_height =
            if shared { (height - gaps - 150.0).clamp(200.0, 470.0) } else { height };
        let has_mixer = column.contains(&B::Mixer);
        let others = groups.iter().filter(|g| g[0] != B::Mixer).count().max(1) as f32;
        let rest = height - gaps - if has_mixer && shared { mixer_height } else { 0.0 };
        let each = rest / others;
        for group in groups {
            let h = if group[0] == B::Mixer { mixer_height } else { each };
            if group.len() == 1 {
                self.screen_block(ui, group[0], vec2(width, h), editing, moved);
                continue;
            }
            let block_width = (width - 4.0 * (group.len() as f32 - 1.0)) / group.len() as f32;
            ui.allocate_ui_with_layout(vec2(width, h), Layout::left_to_right(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for &block in &group {
                    self.screen_block(ui, block, vec2(block_width, h), editing, moved);
                }
            });
        }
    }

    /// Draws a block; while editing, with a bar to drag it by, and taking
    /// other blocks dropped on its edges.
    fn screen_block(
        &mut self,
        ui: &mut Ui,
        block: crate::screen_layout::ScreenBlock,
        size: egui::Vec2,
        editing: bool,
        moved: &mut Option<ScreenMove>,
    ) {
        use crate::screen_layout::{Edge, ScreenBlock as B};
        let channels = self.mixer_channels();
        let (left_waves, right_waves) = channels.split_at(channels.len() / 2);
        let (left_waves, right_waves) = (left_waves.to_vec(), right_waves.to_vec());
        let (left, right): (Vec<usize>, Vec<usize>) =
            if self.config.four_decks { (vec![0, 2], vec![1, 3]) } else { (vec![0], vec![1]) };
        let rect = ui
            .allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
                ui.set_width(size.x);
                let mut height = size.y;
                if editing {
                    const BAR: f32 = 22.0;
                    let id = egui::Id::new(("screen block", block));
                    ui.dnd_drag_source(id, block, |ui| {
                        egui::Frame::new()
                            .fill(Color32::from_gray(45))
                            .corner_radius(3)
                            .inner_margin(egui::Margin::symmetric(6, 2))
                            .show(ui, |ui| {
                                ui.set_width(size.x - 12.0);
                                let text = RichText::new(format!("{} (drag)", block.label()));
                                ui.add(egui::Label::new(text.small()).truncate());
                            });
                    })
                    .response
                    .on_hover_text("Drag onto an edge of another block");
                    height -= BAR + ui.spacing().item_spacing.y;
                }
                match block {
                    B::LeftDecks => self.deck_column(ui, &left, size.x, height),
                    B::RightDecks => self.deck_column(ui, &right, size.x, height),
                    B::LeftWaves => self.wave_column(ui, &left_waves, size.x, height),
                    B::RightWaves => self.wave_column(ui, &right_waves, size.x, height),
                    B::Mixer => {
                        // A wider column (under the waveforms) stretches the mixer.
                        let width = size.x;
                        ui.allocate_ui_with_layout(
                            vec2(size.x, height),
                            Layout::top_down(Align::Center),
                            |ui| {
                                ui.allocate_ui_with_layout(
                                    vec2(width, height),
                                    Layout::top_down(Align::Center),
                                    |ui| {
                                        ui.set_width(width);
                                        self.mixer_ui(ui);
                                    },
                                );
                            },
                        );
                    }
                }
            })
            .response
            .rect;
        if !editing || !egui::DragAndDrop::has_payload_of_type::<B>(ui.ctx()) {
            return;
        }
        let Some(pos) = ui.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos())) else {
            return;
        };
        if !rect.contains(pos) {
            return;
        }
        // The nearest edge says where the dragged block goes.
        let distances = [
            (pos.x - rect.left(), Edge::Left),
            (rect.right() - pos.x, Edge::Right),
            (pos.y - rect.top(), Edge::Above),
            (rect.bottom() - pos.y, Edge::Below),
        ];
        let edge = distances.iter().min_by(|a, b| a.0.total_cmp(&b.0)).map_or(Edge::Right, |d| d.1);
        let stroke = Stroke::new(4.0_f32, ACTIVE);
        let painter = ui.painter();
        match edge {
            Edge::Left => painter.vline(rect.left(), rect.y_range(), stroke),
            Edge::Right => painter.vline(rect.right(), rect.y_range(), stroke),
            Edge::Above => painter.hline(rect.x_range(), rect.top(), stroke),
            Edge::Below => painter.hline(rect.x_range(), rect.bottom(), stroke),
        };
        if ui.input(|i| i.pointer.any_released())
            && let Some(dragged) = egui::DragAndDrop::take_payload::<B>(ui.ctx())
        {
            *moved = Some((*dragged, block, edge));
        }
    }

    /// What the mixer needs across for the parts shown.
    fn mixer_width(&self) -> f32 {
        use crate::parts::MixerPart as P;
        let hidden = &self.config.hidden_mixer_parts;
        let show = |part: P| !hidden.contains(&part);
        let channels = if self.config.four_decks { 4.0 } else { 2.0 };
        let strips = channels * MIXER_STRIP + (channels - 1.0) * MIXER_SPACING;
        let master = if MIXER_MASTER_PARTS.iter().any(|&p| show(p)) {
            MIXER_MASTER + MIXER_SPACING
        } else {
            0.0
        };
        let crossfader = if show(P::Crossfader) { 150.0 } else { 0.0 };
        let order = if self.config.four_decks && show(P::ChannelOrder) { 180.0 } else { 0.0 };
        // The panel's margins on both sides, and a little air.
        (strips + master).max(crossfader).max(order) + 16.0
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
        let order = self.config.channel_order;
        order.iter().map(|&ch| ch.clamp(1, DECKS) - 1).collect()
    }

    fn mixer_ui(&mut self, ui: &mut Ui) {
        use crate::parts::MixerPart as P;
        let hidden = self.config.hidden_mixer_parts.clone();
        let show = |part: P| !hidden.contains(&part);
        // Up to five knobs are stacked per channel; they shrink when the
        // window is short so the fader and crossfader still fit.
        let knobs = [P::Trim, P::High, P::Mid, P::Low, P::Filter].into_iter().filter(|&p| show(p));
        let knobs = knobs.count() as f32;
        let room = ui.available_height() - 125.0 - 90.0;
        let channel_knob = (room / knobs.max(1.0) - 16.0).clamp(16.0, 24.0);
        const KNOB: f32 = 24.0;
        const ORDERS: [[usize; 4]; 2] = [[1, 2, 3, 4], [3, 1, 2, 4]];
        let channels = self.mixer_channels();
        let four = self.config.four_decks;
        let current_order = self.config.channel_order;
        let mut new_order = None;
        // Each channel's column, to drop tracks on.
        let mut strips = Vec::new();
        let Some(engine) = self.engine.as_mut() else { return };
        // Faders take whatever height the knobs, buttons and crossfader leave.
        let fader_height =
            (ui.available_height() - knobs * (channel_knob + 16.0) - 125.0).clamp(60.0, 180.0);
        let panel = egui::Frame::new()
            .fill(if ui.visuals().dark_mode {
                Color32::from_rgb(22, 22, 25)
            } else {
                Color32::from_gray(226)
            })
            .corner_radius(6)
            .inner_margin(6.0);
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            panel.show(ui, |ui| {
                if four && show(P::ChannelOrder) {
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
                    ui.spacing_mut().item_spacing = vec2(MIXER_SPACING, 2.0);
                    // Channel strips share the room the master section leaves,
                    // so hidden parts leave no gap.
                    let master = if MIXER_MASTER_PARTS.iter().any(|&p| show(p)) {
                        MIXER_MASTER + MIXER_SPACING
                    } else {
                        0.0
                    };
                    let count = channels.len() as f32;
                    let strip = ((ui.available_width() - master - (count - 1.0) * MIXER_SPACING)
                        / count)
                        .max(MIXER_STRIP);
                    for &index in &channels {
                        let deck = &mut self.decks[index];
                        let column = ui.vertical(|ui| {
                            ui.set_width(strip);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(format!("{}", index + 1))
                                        .strong()
                                        .color(DECK_COLORS[index]),
                                );
                                let mut changed = false;
                                let [low, mid, high] = &mut deck.eq;
                                // One column per channel, top to bottom, like a DJ mixer.
                                if show(P::Trim) {
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
                                }
                                // Part Isolator: the knobs are the parts' levels.
                                let isolating = deck.part_isolator;
                                let [vocal, inst, drums] = &mut deck.isolator;
                                let knobs = if isolating {
                                    [
                                        (P::High, vocal, "VOCAL"),
                                        (P::Mid, inst, "INST"),
                                        (P::Low, drums, "DRUMS"),
                                    ]
                                } else {
                                    [
                                        (P::High, high, "HI"),
                                        (P::Mid, mid, "MID"),
                                        (P::Low, low, "LOW"),
                                    ]
                                };
                                for (part, value, label) in knobs {
                                    if show(part) {
                                        changed |= knob(
                                            ui,
                                            value,
                                            0.0..=1.0,
                                            0.5,
                                            label,
                                            true,
                                            channel_knob,
                                        )
                                        .changed();
                                    }
                                }
                                if show(P::Filter) {
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
                                }
                                let cue_fill = if deck.headphone_cue {
                                    CUE_COLOR
                                } else {
                                    ui.visuals().widgets.inactive.bg_fill
                                };
                                let cue = egui::Button::new(RichText::new("CUE").small().strong())
                                    .fill(cue_fill)
                                    .min_size(vec2(44.0, 18.0));
                                if show(P::ChannelCue)
                                    && ui.add(cue).on_hover_text("Headphone cue").clicked()
                                {
                                    deck.headphone_cue = !deck.headphone_cue;
                                    changed = true;
                                }
                                if show(P::Fader) || show(P::ChannelMeter) {
                                    ui.horizontal(|ui| {
                                        ui.add_space(10.0);
                                        ui.spacing_mut().item_spacing.x = 3.0;
                                        if show(P::Fader) {
                                            changed |=
                                                fader(ui, &mut deck.volume, fader_height).changed();
                                        }
                                        if show(P::ChannelMeter) {
                                            let peak = engine.deck_peak(index);
                                            meter(ui, peak, vec2(6.0, fader_height));
                                        }
                                    });
                                }
                                if show(P::CrossfaderAssign) {
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
                                }
                                if changed {
                                    deck.apply_mixer(engine);
                                    deck.apply_stems(engine);
                                }
                            });
                        });
                        strips.push((index, column.response.rect));
                    }
                    if !MIXER_MASTER_PARTS.iter().any(|&p| show(p)) {
                        return;
                    }
                    // A separator here would stretch to the full height and push the crossfader out.
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.set_width(MIXER_MASTER - 6.0);
                        ui.vertical_centered(|ui| {
                            if show(P::MasterLevel) || show(P::MasterMeter) {
                                ui.label(RichText::new("MASTER").small().strong());
                            }
                            if show(P::MasterLevel)
                                && knob(
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
                            if show(P::MasterMeter) {
                                ui.horizontal(|ui| {
                                    ui.add_space(20.0);
                                    ui.spacing_mut().item_spacing.x = 3.0;
                                    let peak = engine.master_peak();
                                    meter(ui, peak, vec2(7.0, fader_height));
                                    meter(ui, peak, vec2(7.0, fader_height));
                                });
                            }
                            if !show(P::PhonesMix) && !show(P::PhonesLevel) {
                                return;
                            }
                            ui.label(RichText::new("PHONES").small().strong());
                            if engine.info().has_headphone_output() {
                                if show(P::PhonesMix)
                                    && knob(
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
                                if show(P::PhonesLevel)
                                    && knob(
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
                if !show(P::Crossfader) {
                    return;
                }
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
        for (index, rect) in strips {
            self.deck_drop_target(ui, rect, ui.id().with(("mixer drop", index)), index);
        }
        // One order for the app, with or without a controller.
        if let Some(order) = new_order {
            self.config.channel_order = order;
            self.config.save();
        }
    }

    /// Beat FX, Color FX, Master Cue and the sampler, in one strip.
    fn fx_panel(&mut self, ui: &mut Ui) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        use crate::parts::FxPart;
        let hidden = &self.config.hidden_fx_parts;
        let show = |part: FxPart| !hidden.contains(&part);
        let fx = &mut self.effects;
        let decks = if self.config.four_decks { DECKS } else { 2 };
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().slider_width = 64.0;
            let mut separate = false;
            if show(FxPart::BeatFx) {
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
                        .map(|s| format!("Pad FX {}: {}", s + 1, fx.pad_slot(s).label()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                separate = true;
            }
            if show(FxPart::ColorFx) {
                if separate {
                    ui.separator();
                }
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
                separate = true;
            }
            if show(FxPart::MasterCue) {
                if separate && !show(FxPart::ColorFx) {
                    ui.separator();
                }
                if toggle(ui, fx.master_cue, "MASTER CUE")
                    .on_hover_text("Hear the master mix in the headphones")
                    .clicked()
                {
                    fx.toggle_master_cue(engine);
                }
                separate = true;
            }
            if show(FxPart::Sampler) {
                if separate {
                    ui.separator();
                }
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
            }
        });
    }

    fn library_ui(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let focus_stroke =
            |focused: bool| if focused { Stroke::new(1.0_f32, ACTIVE) } else { Stroke::NONE };
        let mut collection_load = None;
        ui.horizontal_top(|ui| {
            // The sidebar sits in a horizontal row; give it its own top-down layout.
            ui.allocate_ui_with_layout(
                vec2(240.0, ui.available_height()),
                Layout::top_down(Align::Min),
                |ui| {
                    egui::Frame::new()
                        .stroke(focus_stroke(self.focus == Focus::Playlists))
                        .show(ui, |ui| self.library_sidebar(ui));
                },
            );
            ui.separator();
            ui.vertical(|ui| match self.source {
                Source::Collection => {
                    let stroke = focus_stroke(self.focus == Focus::Tracks);
                    collection_load = self.collection_tracks(ui, stroke);
                }
                Source::Usb(_) => self.usb_list(ui, ctx),
                Source::Explorer => {
                    let stroke = focus_stroke(self.focus == Focus::Tracks);
                    self.explorer_list(ui, stroke, ctx);
                }
            });
        });
        if let Some((id, deck)) = collection_load {
            self.selected = Some(id);
            self.load_from_collection(id, deck, ctx);
        }
    }

    /// The selected export's tracks: search, load buttons and the list.
    fn usb_list(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let Some(library) = self.usb() else {
            ui.centered_and_justified(|ui| {
                ui.label("Plug in a USB stick with a rekordbox export, or open an export folder.");
            });
            return;
        };
        let root = library.root.clone();
        // The stick's playlists, for "Add to playlist", and the one shown.
        let stick_playlists: Vec<(u32, String)> = library
            .playlists
            .iter()
            .filter(|r| r.track_ids.is_some())
            .map(|r| (r.id, r.name.clone()))
            .collect();
        let shown_playlist = library.playlist.and_then(|i| library.playlists.get(i)).map(|r| r.id);
        let mut usb_edit = None;
        let mut load: Option<(u32, usize)> = None;
        let mut double_clicked = None;
        let deck_count = self.deck_count();
        let rows = self.usb_rows();
        let loaded: Vec<Option<u32>> = self
            .decks
            .iter()
            .map(|d| d.track.as_ref().filter(|t| t.usb_root.as_ref() == Some(&root)).map(|t| t.id))
            .collect();
        let free = self.free_deck(&loaded);
        let focus_stroke =
            |focused: bool| if focused { Stroke::new(1.0_f32, ACTIVE) } else { Stroke::NONE };
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
            draggable: true,
            marked: &self.marked,
            editable_rating: false,
            reorder: None,
        };
        let selection = self.selected_ids();
        let group = |id: u32| if self.marked.contains(&id) { selection.clone() } else { vec![id] };
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
                        use library_tree::UsbEdit;
                        for deck in 0..deck_count {
                            if ui.button(format!("Load to deck {}", deck + 1)).clicked() {
                                context_load = Some((row.id, deck));
                                ui.close();
                            }
                        }
                        ui.separator();
                        let ids = group(row.id);
                        let many = if ids.len() > 1 {
                            format!(" ({} tracks)", ids.len())
                        } else {
                            String::new()
                        };
                        ui.menu_button(format!("Add to playlist{many}"), |ui| {
                            for (id, name) in &stick_playlists {
                                if ui.button(name).clicked() {
                                    usb_edit = Some(UsbEdit::AddTracks(*id, ids.clone()));
                                    ui.close();
                                }
                            }
                        });
                        if let Some(playlist) = shown_playlist
                            && ui.button("Remove from this playlist").clicked()
                        {
                            usb_edit = Some(UsbEdit::RemoveEntry(playlist, row.position));
                            ui.close();
                        }
                    },
                )
            })
            .inner;
        if output.layout_changed {
            self.config.save();
        }
        if let Some((id, position)) = output.clicked {
            self.click_track(id, position, output.modifiers);
        }
        if let Some((id, position)) = output.drag_started {
            self.start_track_drag(ui.ctx(), id, position);
        }
        if let Some(id) = output.double_clicked {
            double_clicked = Some(id);
        }
        if context_load.is_some() {
            load = context_load;
        }
        if let Some(edit) = usb_edit {
            self.edit_usb(&root, edit);
        }
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
        let Some(library) = self.usb() else { return Vec::new() };
        let mut rows: Vec<Row> = library
            .numbered_tracks(library.playlist, &self.search)
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
                key: t.key.as_deref().map(|k| display_key(k, self.config.prefs.key_notation)),
                duration: f64::from(t.track.duration_secs),
                year: Some(t.track.year).filter(|y| *y > 0),
                rating: t.track.rating.min(5),
                plays: u32::from(t.track.play_count),
                added: t.track.date_added.clone(),
                bitrate: Some(t.track.bitrate).filter(|b| *b > 0),
                comment: t.track.comment.clone(),
                artwork: t.artwork_path.as_deref().map(|p| {
                    crate::artwork::Cover::File(library.root.join(p.trim_start_matches('/')))
                }),
                preview: Some(crate::preview::Source {
                    audio: library.resolve(&t.track.file_path),
                    analysis: (!t.track.analyze_path.is_empty())
                        .then(|| library.resolve(&t.track.analyze_path)),
                }),
                cues: None,
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
                    PrefsTab::Keyboard => {
                        prefs_changed |= self.keyboard.prefs_ui(ui, &mut self.config.prefs);
                        return;
                    }
                    PrefsTab::Stems => {
                        prefs_changed |= self.stems_prefs(ui);
                        return;
                    }
                    PrefsTab::Library => {
                        prefs_changed |= self.library_prefs(ui);
                        return;
                    }
                    PrefsTab::Appearance => {
                        prefs_changed |= appearance_prefs(ui, &mut self.config.prefs);
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
            crate::widgets::apply_theme(ctx, self.config.prefs.theme, self.config.prefs.text_scale);
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

/// Auto master's choice: the deck to pass MASTER to, if any. `gains` holds
/// each deck's gain in the mix while it plays (`None`: paused or empty);
/// `heard`: the master has been heard since it became master.
fn next_master(gains: &[Option<f32>], current: usize, heard: bool) -> Option<usize> {
    let left = gains.get(current).copied().flatten().is_none_or(|g| g <= OUT_OF_MIX);
    if !heard || !left {
        return None;
    }
    loudest_in_mix(gains, current)
}

/// The loudest deck clearly in the mix, other than `except` (the lower
/// number on a tie).
fn loudest_in_mix(gains: &[Option<f32>], except: usize) -> Option<usize> {
    gains
        .iter()
        .enumerate()
        .filter_map(|(d, g)| g.filter(|&g| d != except && g > IN_MIX).map(|g| (d, g)))
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(d, _)| d)
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
        // The deck view draws its own STEMS pads; this is for anything else.
        PadMode::Stems => face(
            deckhand_control::STEM_PARTS.get(pad).map_or("", |p| p).into(),
            idle,
            "Turn a stem part off or on",
        ),
        PadMode::PadFx => {
            let slot = (pad + if shift { 8 } else { 0 } + 8 * usize::from(deck.pad_fx_page)) % 16;
            let held = fx.pad == Some((index, slot as u8));
            face(
                fx.pad_slot(slot).short(),
                if held { ACTIVE } else { Color32::from_rgb(70, 45, 95) },
                &format!("Hold for {}; Shift for the second set", fx.pad_slot(slot).label()),
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
/// A submenu that stays open while its contents are clicked (checkboxes,
/// text fields); clicking outside it closes it.
fn sticky_menu<R>(ui: &mut Ui, label: impl Into<String>, content: impl FnOnce(&mut Ui) -> R) {
    use egui::containers::menu::{MenuConfig, SubMenuButton};
    let config = MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
    SubMenuButton::new(label.into()).config(config).ui(ui, content);
}

fn toggle(ui: &mut Ui, on: bool, label: &str) -> egui::Response {
    crate::widgets::lit_button(ui, label, on, ACTIVE)
}

/// A button lit in its own colour while `on`.
fn toggle_in(ui: &mut Ui, on: bool, label: &str, accent: Color32) -> egui::Response {
    crate::widgets::lit_button(ui, label, on, accent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_passes_to_the_loudest_deck_in_the_mix() {
        // Deck 0 is master and faded out; 1 plays full, 2 plays quietly, 3 is paused.
        let gains = [Some(0.0), Some(1.0), Some(0.5), None];
        assert_eq!(next_master(&gains, 0, true), Some(1));
        // Paused or ejected counts as out of the mix too.
        assert_eq!(next_master(&[None, Some(0.2)], 0, true), Some(1));
    }

    #[test]
    fn master_stays_while_heard_or_without_a_successor() {
        // Still audible (a fade in progress): no change.
        assert_eq!(next_master(&[Some(0.05), Some(1.0)], 0, true), None);
        // Out of the mix, but nobody else is clearly in it.
        assert_eq!(next_master(&[Some(0.0), Some(0.08), None], 0, true), None);
        // Chosen while silent (a track being prepared): kept until heard.
        assert_eq!(next_master(&[Some(0.0), Some(1.0)], 0, false), None);
    }

    #[test]
    fn ties_go_to_the_lower_deck() {
        assert_eq!(loudest_in_mix(&[None, Some(1.0), Some(1.0)], 0), Some(1));
        assert_eq!(loudest_in_mix(&[Some(1.0), Some(1.0)], 0), Some(1));
    }
}

/// A file of view presets, as exported.
#[derive(serde::Serialize, serde::Deserialize)]
struct PresetFile {
    rofloxbox_view_presets: u32,
    presets: Vec<crate::layout::LayoutPreset>,
}

/// Writes `presets` where the user picks; says how it went.
fn export_presets(presets: &[crate::layout::LayoutPreset]) -> String {
    let Some(path) = rfd::FileDialog::new()
        .set_title("Export view presets")
        .set_file_name("RofLoxBox view presets.json")
        .add_filter("RofLoxBox presets", &["json"])
        .save_file()
    else {
        return "Not exported".into();
    };
    let file = PresetFile { rofloxbox_view_presets: 1, presets: presets.to_vec() };
    let written = serde_json::to_string_pretty(&file)
        .map_err(anyhow::Error::from)
        .and_then(|text| Ok(std::fs::write(&path, text)?));
    match written {
        Ok(()) => format!(
            "Exported {} preset{} to {}",
            presets.len(),
            if presets.len() == 1 { "" } else { "s" },
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        Err(err) => format!("Not exported: {err:#}"),
    }
}

/// The presets in an exported file, their layouts repaired for this
/// version.
fn import_presets(path: &std::path::Path) -> anyhow::Result<Vec<crate::layout::LayoutPreset>> {
    let text = std::fs::read_to_string(path)?;
    let file: PresetFile = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("not a RofLoxBox presets file ({e})"))?;
    Ok(file
        .presets
        .into_iter()
        .map(|mut preset| {
            preset.layout.repair();
            preset
        })
        .collect())
}
