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
    #[serde(default)]
    pub prefs: Preferences,
    /// Deck, mixer and FX settings as they were when deckhand last ran.
    #[serde(default)]
    pub session: Session,
    /// Deck controls and where they are, for two and for four decks.
    #[serde(default)]
    pub layout_two: crate::layout::DeckLayout,
    #[serde(default = "crate::layout::DeckLayout::four_decks")]
    pub layout_four: crate::layout::DeckLayout,
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
            hot_cue_gate: false,
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
    /// No loading onto a playing deck.
    pub load_lock: bool,
    /// No jumping by clicking the overview of a playing deck.
    pub needle_lock: bool,
    /// No calling memory cues on a playing deck.
    pub memory_call_lock: bool,
    /// A loaded track starts at its earliest memory or hot cue.
    pub start_at_first_cue: bool,
    /// Loading a track that plays on another deck picks it up there, in time.
    pub instant_doubles: bool,
    /// Auto Cue per deck: the cue goes to the first sound louder than the level.
    pub auto_cue: [bool; 4],
    pub auto_cue_level_db: f32,
    /// Vinyl Speed Adjust, seconds: starting up, and stopping (brake).
    pub vinyl_start: f32,
    pub vinyl_brake: f32,
    /// Jog wheel scratch speed, 1.0 = one turn moves 1.8 s.
    pub scratch_sensitivity: f32,
    pub crossfader_curve: CrossfaderCurveSetting,
    pub fader_curve: FaderCurveSetting,
    /// Isolator: the EQ knobs cut fully; EQ: down to −26 dB.
    pub isolator: bool,
    pub limiter: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            load_lock: true,
            needle_lock: true,
            memory_call_lock: true,
            start_at_first_cue: true,
            instant_doubles: false,
            auto_cue: [false; 4],
            auto_cue_level_db: -60.0,
            vinyl_start: 0.0,
            vinyl_brake: 0.0,
            scratch_sensitivity: 1.0,
            crossfader_curve: CrossfaderCurveSetting::Standard,
            fader_curve: FaderCurveSetting::Standard,
            isolator: true,
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
            prefs: Preferences::default(),
            session: Session::default(),
            layout_two: crate::layout::DeckLayout::default(),
            layout_four: crate::layout::DeckLayout::four_decks(),
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
        config.layout_two.repair();
        config.layout_four.repair();
        config
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
