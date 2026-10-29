//! Settings remembered between runs, in
//! `~/Library/Application Support/deckhand/config.toml` (`%APPDATA%\deckhand`
//! on Windows).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub library_root: Option<PathBuf>,
    pub audio_device: Option<String>,
    pub midi_port: Option<String>,
    #[serde(default)]
    pub four_decks: bool,
    #[serde(default)]
    pub waveform_style: crate::waveform::WaveformStyle,
    #[serde(default = "default_grid_strength")]
    pub grid_strength: f32,
    /// Mixer channel order in 4-deck mode when no controller says otherwise.
    #[serde(default = "default_channel_order")]
    pub channel_order: [usize; 4],
    /// Scrolling waveforms run top to bottom between decks and mixer.
    #[serde(default)]
    pub vertical_waveforms: bool,
    #[serde(default = "yes")]
    pub show_waveforms: bool,
    #[serde(default = "yes")]
    pub show_mixer: bool,
    #[serde(default = "yes")]
    pub show_library: bool,
    #[serde(default = "yes")]
    pub show_fx: bool,
    /// The browser showed deckhand's collection (rather than a USB export).
    #[serde(default = "yes")]
    pub use_collection: bool,
    /// Track list columns, left to right.
    #[serde(default = "crate::browser::TrackColumn::default_set")]
    pub columns: Vec<crate::browser::TrackColumn>,
    #[serde(default)]
    pub sort: Option<crate::browser::Sort>,
    /// The # column was added to saved column sets once (later removals stick).
    #[serde(default)]
    pub number_column_added: bool,
    #[serde(default)]
    pub prefs: Preferences,
    /// Collection folders shown closed in the sidebar.
    #[serde(default)]
    pub collapsed_folders: Vec<u32>,
    /// Where recordings go; `None`: Music/RofLoxBox Recordings.
    #[serde(default)]
    pub recordings_dir: Option<PathBuf>,
    /// Deck, mixer and FX settings as they were when deckhand last ran.
    #[serde(default)]
    pub session: Session,
    /// Deck controls and where they are, for two and for four decks.
    #[serde(default)]
    pub layout_two: crate::layout::DeckLayout,
    #[serde(default = "crate::layout::DeckLayout::four_decks")]
    pub layout_four: crate::layout::DeckLayout,
    /// Where the decks, vertical waveforms and mixer go.
    #[serde(default)]
    pub screen_layout: crate::screen_layout::ScreenLayout,
    /// Mixer and FX strip parts not shown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_mixer_parts: Vec<crate::parts::MixerPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_fx_parts: Vec<crate::parts::FxPart>,
    /// Deck layouts saved under a name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layout_presets: Vec<crate::layout::LayoutPreset>,
    /// Output sample rate; `None` uses the device's current rate.
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// Audio buffer size in frames; `None` uses the device's default.
    #[serde(default)]
    pub buffer_frames: Option<u32>,
    #[serde(default)]
    pub audio_quality: AudioQuality,
}

/// What the decks, mixer and FX were set to, restored at start-up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub decks: Vec<DeckSession>,
    pub crossfader: f32,
    pub master_volume: Option<f32>,
    pub headphone_mix: f32,
    pub headphone_volume: Option<f32>,
    /// Seconds of waveform shown.
    pub zoom: Option<f64>,
    pub fx: FxSession,
}

/// Crossfader side of a channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    #[default]
    A,
    Thru,
    B,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeckSession {
    pub side: Option<Side>,
    pub tempo_range: Option<crate::deck::TempoRange>,
    pub quantize: bool,
    pub vinyl: bool,
    pub key_lock: bool,
    pub slip: bool,
    /// Hot cues from pause play while held (GATE). Saved under a new name:
    /// it used to default to off.
    #[serde(rename = "hot_cue_hold")]
    pub hot_cue_gate: bool,
    pub pad_mode: deckhand_control::PadMode,
    pub jump_size: f64,
    pub jump_range: f64,
    pub loop_scale: f64,
    pub volume: f32,
    pub trim: f32,
    pub eq: [f32; 3],
    pub filter: f32,
    pub headphone_cue: bool,
}

impl Default for DeckSession {
    fn default() -> Self {
        Self {
            side: None,
            tempo_range: None,
            quantize: true,
            vinyl: true,
            key_lock: false,
            slip: false,
            hot_cue_gate: true,
            pad_mode: deckhand_control::PadMode::default(),
            jump_size: 4.0,
            jump_range: 1.0,
            loop_scale: 1.0,
            volume: 1.0,
            trim: 0.5,
            eq: [0.5; 3],
            filter: 0.0,
            headphone_cue: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FxSession {
    /// Index into `BeatFxKind::ALL`.
    pub kind: Option<usize>,
    pub beat: Option<usize>,
    pub depth: Option<f32>,
    /// `ch1`–`ch4`, `master` or `sampler`.
    pub target: Option<String>,
    /// Index into `ColorFxKind::ALL`.
    pub color: Option<usize>,
    pub color_on: Option<bool>,
}

/// How decks and the mixer behave (Preferences window), like rekordbox's
/// controller preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// How keys are written on the decks and in the lists.
    pub key_notation: crate::deck::KeyNotation,
    /// How tracks without rekordbox's analysis are analysed.
    pub analysis: crate::deck::AnalysisMethod,
    /// A loaded track without a beat grid or key gets them detected.
    pub analyze_on_load: bool,
    /// No loading onto a playing deck.
    pub load_lock: bool,
    /// No jumping by clicking the overview of a playing deck.
    pub needle_lock: bool,
    /// No calling memory cues on a playing deck.
    pub memory_call_lock: bool,
    /// CALL ◀ ▶ during loop playback halves / doubles the loop (Pioneer's
    /// way) instead of moving between memory cues and loops.
    pub call_resizes_loop: bool,
    /// GATE hot cues also stop on release while the deck is playing.
    pub gate_while_playing: bool,
    /// A loaded track starts at its earliest memory or hot cue.
    pub start_at_first_cue: bool,
    /// Loading a track that plays on another deck picks it up there, in time.
    pub instant_doubles: bool,
    /// MASTER passes to another playing deck when the master leaves the mix
    /// (paused, ejected, or faded out with its channel fader or crossfader).
    pub auto_master: bool,
    /// The tempo ranges TEMPO RANGE steps through and the decks offer, in
    /// order (at least one).
    pub tempo_ranges: Vec<crate::deck::TempoRange>,
    /// Auto Cue per deck: the cue goes to the first sound louder than the level.
    pub auto_cue: [bool; 4],
    pub auto_cue_level_db: f32,
    /// Vinyl Speed Adjust, seconds: starting up, and stopping (brake).
    pub vinyl_start: f32,
    pub vinyl_brake: f32,
    /// Jog wheel scratch speed, 1.0 = one turn moves 1.8 s.
    pub scratch_sensitivity: f32,
    /// Jog pitch bend strength.
    pub bend_sensitivity: f32,
    pub crossfader_curve: CrossfaderCurveSetting,
    pub fader_curve: FaderCurveSetting,
    /// Isolator: the EQ knobs cut fully; EQ: down to −26 dB.
    pub isolator: bool,
    /// Waveforms shrink the bands the EQ turns down.
    pub eq_waveforms: bool,
    /// EQ mode: low shelf, mid peak and high shelf in Hz.
    pub eq_low_hz: f32,
    pub eq_mid_hz: f32,
    pub eq_high_hz: f32,
    /// Isolator mode: low/mid and mid/high split points in Hz.
    pub iso_low_hz: f32,
    pub iso_high_hz: f32,
    pub limiter: bool,
}

impl Preferences {
    pub fn eq_settings(&self) -> deckhand_audio::EqSettings {
        deckhand_audio::EqSettings {
            isolator: self.isolator,
            low_hz: self.eq_low_hz,
            mid_hz: self.eq_mid_hz,
            high_hz: self.eq_high_hz,
            split_low_hz: self.iso_low_hz,
            split_high_hz: self.iso_high_hz,
        }
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            load_lock: true,
            needle_lock: true,
            memory_call_lock: true,
            key_notation: Default::default(),
            analysis: Default::default(),
            analyze_on_load: true,
            call_resizes_loop: false,
            gate_while_playing: false,
            start_at_first_cue: true,
            auto_master: true,
            tempo_ranges: crate::deck::TempoRange::DEFAULTS.to_vec(),
            instant_doubles: false,
            auto_cue: [false; 4],
            auto_cue_level_db: -60.0,
            vinyl_start: 0.0,
            vinyl_brake: 0.0,
            scratch_sensitivity: 1.0,
            bend_sensitivity: 1.0,
            crossfader_curve: CrossfaderCurveSetting::Standard,
            fader_curve: FaderCurveSetting::Standard,
            isolator: true,
            eq_waveforms: true,
            eq_low_hz: 70.0,
            eq_mid_hz: 1_000.0,
            eq_high_hz: 13_000.0,
            iso_low_hz: 250.0,
            iso_high_hz: 4_000.0,
            limiter: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrossfaderCurveSetting {
    Smooth,
    #[default]
    Standard,
    Sharp,
}

impl CrossfaderCurveSetting {
    pub const ALL: [Self; 3] = [Self::Smooth, Self::Standard, Self::Sharp];

    pub fn label(self) -> &'static str {
        match self {
            Self::Smooth => "Smooth (long blend)",
            Self::Standard => "Standard",
            Self::Sharp => "Sharp (scratch)",
        }
    }

    pub fn engine(self) -> deckhand_audio::CrossfaderCurve {
        match self {
            Self::Smooth => deckhand_audio::CrossfaderCurve::Smooth,
            Self::Standard => deckhand_audio::CrossfaderCurve::Standard,
            Self::Sharp => deckhand_audio::CrossfaderCurve::Sharp,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FaderCurveSetting {
    Slow,
    #[default]
    Standard,
    Fast,
}

impl FaderCurveSetting {
    pub const ALL: [Self; 3] = [Self::Slow, Self::Standard, Self::Fast];

    pub fn label(self) -> &'static str {
        match self {
            Self::Slow => "Slow rise",
            Self::Standard => "Standard",
            Self::Fast => "Fast rise",
        }
    }

    pub fn engine(self) -> deckhand_audio::FaderCurve {
        match self {
            Self::Slow => deckhand_audio::FaderCurve::Slow,
            Self::Standard => deckhand_audio::FaderCurve::Standard,
            Self::Fast => deckhand_audio::FaderCurve::Fast,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioQuality {
    Standard,
    #[default]
    High,
}

impl AudioQuality {
    pub fn engine(self) -> deckhand_audio::Quality {
        match self {
            Self::Standard => deckhand_audio::Quality::Standard,
            Self::High => deckhand_audio::Quality::High,
        }
    }
}

fn yes() -> bool {
    true
}

fn default_channel_order() -> [usize; 4] {
    [1, 2, 3, 4]
}

fn default_grid_strength() -> f32 {
    1.0
}

impl Default for Config {
    fn default() -> Self {
        Self {
            library_root: None,
            audio_device: None,
            midi_port: None,
            four_decks: false,
            waveform_style: crate::waveform::WaveformStyle::default(),
            grid_strength: default_grid_strength(),
            channel_order: default_channel_order(),
            vertical_waveforms: false,
            show_waveforms: true,
            show_mixer: true,
            show_library: true,
            show_fx: true,
            use_collection: true,
            columns: crate::browser::TrackColumn::default_set(),
            sort: None,
            number_column_added: true,
            prefs: Preferences::default(),
            recordings_dir: None,
            collapsed_folders: Vec::new(),
            session: Session::default(),
            layout_two: crate::layout::DeckLayout::default(),
            layout_four: crate::layout::DeckLayout::four_decks(),
            layout_presets: Vec::new(),
            hidden_mixer_parts: Vec::new(),
            hidden_fx_parts: Vec::new(),
            screen_layout: Default::default(),
            sample_rate: None,
            buffer_frames: None,
            audio_quality: AudioQuality::default(),
        }
    }
}

pub fn data_dir() -> PathBuf {
    deckhand_library::data_dir()
}

/// Extra rekordbox `.midi.csv` mapping files (for example from rekordbox 6),
/// used for controllers the installed rekordbox has no mapping for.
pub fn extra_mapping_dir() -> PathBuf {
    data_dir().join("rekordbox-mappings")
}

/// Audio files for the sampler slots (the first eight, by name).
pub fn samples_dir() -> PathBuf {
    data_dir().join("samples")
}

/// Where the mapping for a MIDI port is stored.
pub fn mapping_path(port: &str) -> PathBuf {
    let file: String =
        port.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    data_dir().join("mappings").join(format!("{file}.toml"))
}

impl Config {
    pub fn load() -> Self {
        let mut config: Config = std::fs::read_to_string(data_dir().join("config.toml"))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        if !config.number_column_added {
            config.number_column_added = true;
            if !config.columns.contains(&crate::browser::TrackColumn::Number) {
                config.columns.insert(0, crate::browser::TrackColumn::Number);
            }
        }
        config.layout_two.repair();
        config.layout_four.repair();
        config.screen_layout.repair();
        config
    }

    /// The View menu's settings besides the deck elements.
    pub fn view(&self) -> crate::layout::ViewSettings {
        crate::layout::ViewSettings {
            vertical_waveforms: self.vertical_waveforms,
            show_waveforms: self.show_waveforms,
            show_mixer: self.show_mixer,
            show_library: self.show_library,
            show_fx: self.show_fx,
            screen_layout: self.screen_layout.clone(),
            hidden_mixer_parts: self.hidden_mixer_parts.clone(),
            hidden_fx_parts: self.hidden_fx_parts.clone(),
        }
    }

    pub fn set_view(&mut self, view: crate::layout::ViewSettings) {
        self.vertical_waveforms = view.vertical_waveforms;
        self.show_waveforms = view.show_waveforms;
        self.show_mixer = view.show_mixer;
        self.show_library = view.show_library;
        self.show_fx = view.show_fx;
        self.screen_layout = view.screen_layout;
        self.screen_layout.repair();
        self.hidden_mixer_parts = view.hidden_mixer_parts;
        self.hidden_fx_parts = view.hidden_fx_parts;
    }

    pub fn deck_layout(&self, four: bool) -> &crate::layout::DeckLayout {
        if four { &self.layout_four } else { &self.layout_two }
    }

    pub fn deck_layout_mut(&mut self, four: bool) -> &mut crate::layout::DeckLayout {
        if four { &mut self.layout_four } else { &mut self.layout_two }
    }

    pub fn save(&self) {
        // Screenshot runs (a developer aid) must not change the user's settings.
        if std::env::var_os("DECKHAND_SCREENSHOT").is_some() {
            return;
        }
        let dir = data_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(dir.join("config.toml"), text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{DeckLayout, LayoutPreset};

    #[test]
    fn layout_presets_are_saved_and_read_back() {
        use crate::layout::ViewSettings;
        let mut config = Config::default();
        let view = ViewSettings {
            vertical_waveforms: true,
            show_mixer: false,
            hidden_fx_parts: vec![crate::parts::FxPart::Sampler],
            ..ViewSettings::default()
        };
        config.layout_presets.push(LayoutPreset {
            name: "Club".into(),
            layout: DeckLayout::everything(),
            view: Some(view.clone()),
        });
        let text = toml::to_string_pretty(&config).unwrap();
        let read: Config = toml::from_str(&text).unwrap();
        assert_eq!(read.layout_presets, config.layout_presets);
        assert_eq!(read.layout_two, DeckLayout::default());
        // Applying the view and reading it back gives the same view.
        let mut applied = Config::default();
        applied.set_view(view.clone());
        assert!(applied.view().matches(&view));
        // Presets saved before view presets existed have no view.
        let old = "[[layout_presets]]\nname = \"Old\"\n[layout_presets.layout]\nmain = []\nside = []\nhidden = []\n";
        let read: Config = toml::from_str(old).unwrap();
        assert_eq!(read.layout_presets[0].view, None);
    }
}
