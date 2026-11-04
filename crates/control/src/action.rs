//! What controls can do, and what controllers can show.

use std::fmt;
use std::str::FromStr;

pub const DECKS: usize = 4;

/// How a control delivers values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Press and release.
    Button,
    /// A position, 0.0–1.0 (faders, knobs).
    Absolute,
    /// Movement steps (jog wheels, encoders).
    Relative,
}

/// What the eight performance pads of a deck do, as on rekordbox.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum PadMode {
    #[default]
    HotCue,
    Keyboard,
    Stems,
    PadFx,
    BeatJump,
    BeatLoop,
    Sampler,
    KeyShift,
}

impl PadMode {
    pub const ALL: [PadMode; 8] = [
        Self::HotCue,
        Self::Keyboard,
        Self::Stems,
        Self::PadFx,
        Self::BeatJump,
        Self::BeatLoop,
        Self::Sampler,
        Self::KeyShift,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::HotCue => "HOT CUE",
            Self::Keyboard => "KEYBOARD",
            Self::Stems => "STEMS",
            Self::PadFx => "PAD FX",
            Self::BeatJump => "BEAT JUMP",
            Self::BeatLoop => "BEAT LOOP",
            Self::Sampler => "SAMPLER",
            Self::KeyShift => "KEY SHIFT",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::HotCue => "hot_cue",
            Self::Keyboard => "keyboard",
            Self::Stems => "stems",
            Self::PadFx => "pad_fx",
            Self::BeatJump => "beat_jump",
            Self::BeatLoop => "beat_loop",
            Self::Sampler => "sampler",
            Self::KeyShift => "key_shift",
        }
    }

    /// The mode's parameter ◀ / ▶ buttons (rekordbox's `Parameter1/2`),
    /// without and with Shift.
    pub fn parameter_action(self, up: bool, shift: bool) -> Option<DeckAction> {
        use DeckAction as D;
        Some(match (self, shift, up) {
            (Self::HotCue, false, false) => D::MemoryPrev,
            (Self::HotCue, false, true) => D::MemoryNext,
            (Self::Keyboard, _, false) => D::KeyboardDown,
            (Self::Keyboard, _, true) => D::KeyboardUp,
            (Self::PadFx, _, false) => D::PadFxPageDown,
            (Self::PadFx, _, true) => D::PadFxPageUp,
            (Self::BeatJump, false, false) => D::BeatJumpRangeHalf,
            (Self::BeatJump, false, true) => D::BeatJumpRangeDouble,
            (Self::BeatJump, true, false) => D::BeatJumpRangeSixteenth,
            (Self::BeatJump, true, true) => D::BeatJumpRangeSixteen,
            (Self::BeatLoop, _, false) => D::BeatLoopSizesHalf,
            (Self::BeatLoop, _, true) => D::BeatLoopSizesDouble,
            (Self::Sampler, false, false) => D::SamplerBankDown,
            (Self::Sampler, false, true) => D::SamplerBankUp,
            (Self::KeyShift, _, false) => D::KeyShiftPageDown,
            (Self::KeyShift, _, true) => D::KeyShiftPageUp,
            _ => return None,
        })
    }

    /// What pad 0–7 does in this mode, with or without Shift (rekordbox's
    /// layout for the DDJ series). The controller sends these actions
    /// itself; the on-screen pads use this.
    pub fn pad_action(self, pad: u8, shift: bool) -> Option<DeckAction> {
        use DeckAction as D;
        let pad = pad % 8;
        Some(match (self, shift) {
            (Self::HotCue, false) => D::HotCue(pad),
            (Self::HotCue, true) => D::HotCueDelete(pad),
            (Self::Keyboard, false) => D::Keyboard(pad),
            (Self::Keyboard, true) => match pad {
                6 => D::KeyboardDown,
                7 => D::KeyboardUp,
                _ => D::HotCueDelete(pad),
            },
            (Self::Stems, false) if pad < 4 => D::StemPart(pad),
            (Self::Stems, _) => return None,
            (Self::PadFx, _) => D::PadFx(pad + if shift { 8 } else { 0 }),
            (Self::BeatJump, false) => D::BeatJump(pad),
            (Self::BeatJump, true) => match pad {
                6 => D::BeatJumpRangeHalf,
                7 => D::BeatJumpRangeDouble,
                _ => return None,
            },
            (Self::BeatLoop, false) => D::BeatLoop(pad),
            (Self::BeatLoop, true) => return None,
            (Self::Sampler, false) => D::SamplerPlay(pad),
            (Self::Sampler, true) => D::SamplerStop(pad),
            (Self::KeyShift, false) => D::KeyShift(pad),
            (Self::KeyShift, true) => match pad {
                6 => D::KeyShiftPageDown,
                7 => D::KeyShiftPageUp,
                _ => return None,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckAction {
    PlayPause,
    Cue,
    JumpToStart,
    /// Hot cue A = 0 … H = 7.
    HotCue(u8),
    HotCueDelete(u8),
    /// Hot cue that plays only while held, whatever the gate mode.
    HotCueGate(u8),
    /// Switches all hot cue pads of the deck between trigger and gate.
    HotCueGateMode,
    MemoryPrev,
    MemoryNext,
    MemorySet,
    MemoryDelete,
    /// Moves the active loop back or forward by its own length.
    LoopMoveBack,
    LoopMoveForward,
    /// Jog wheel with Shift on many controllers: fast search.
    JogSearch,
    CrossfaderAssignA,
    CrossfaderAssignThru,
    CrossfaderAssignB,
    Load,
    TempoFader,
    TempoRangeCycle,
    KeyLock,
    Sync,
    SyncMaster,
    Quantize,
    Vinyl,
    Slip,
    JogTouch,
    /// Jog wheel side: bends the tempo.
    JogTurn,
    /// Jog wheel top: scratches in vinyl mode, bends otherwise.
    JogScratch,
    LoopIn,
    LoopOut,
    ReloopExit,
    /// Four-beat loop from the current position.
    AutoLoop,
    /// Pioneer controllers send these from the CALL buttons during loop
    /// playback; what they do follows the "CALL during loop" preference.
    LoopHalf,
    LoopDouble,
    /// Always halve / double the active loop (for a button of its own).
    LoopResizeHalf,
    LoopResizeDouble,
    /// Beat-jump pads 0–7: back/forward pairs of 1, 2, 4 and 8 beats,
    /// scaled by the deck's jump range.
    BeatJump(u8),
    /// Jump back / forward by the deck's beat jump size.
    BeatJumpBack,
    BeatJumpForward,
    /// Halve / double the beat-jump pads' range.
    BeatJumpRangeHalf,
    BeatJumpRangeDouble,
    /// Beat-loop pads 0–7: ¼, ½, 1, 2, 4, 8, 16, 32 beats.
    BeatLoop(u8),
    Volume,
    Trim,
    EqLow,
    EqMid,
    EqHigh,
    /// The channel's Color FX knob (with the Filter type: low-pass left of
    /// center, high-pass right of center).
    Filter,
    HeadphoneCue,
    /// Tapping sets the track's BPM and beat grid.
    TapBpm,
    /// Shift the key to match the master deck's.
    KeySync,
    KeyReset,
    KeyUp,
    KeyDown,
    /// Key-shift pads 0–7: ±0 … +7 semitones from the current page.
    KeyShift(u8),
    KeyShiftPageDown,
    KeyShiftPageUp,
    /// Keyboard pads 0–7: the first press picks the hot cue, then each pad
    /// plays it ±0 … +7 semitones from the current page.
    Keyboard(u8),
    KeyboardDown,
    KeyboardUp,
    /// Turns the stored loop on or off without jumping.
    ActiveLoop,
    /// Channel fader start (sent by the controller): play / back to cue.
    FaderStartPlay,
    FaderStartCue,
    /// Pad FX pads 0–15, momentary.
    PadFx(u8),
    /// Sampler pads 0–7 (slots 1–8 on decks 1/3, 9–16 on decks 2/4).
    SamplerPlay(u8),
    /// Stops the slot, or loads the selected track into it when it is silent.
    SamplerStop(u8),
    PadMode(PadMode),
    /// Sampler bank 1–4 down / up.
    SamplerBankDown,
    SamplerBankUp,
    /// Pad FX pads show effects A–H or I–P.
    PadFxPageDown,
    PadFxPageUp,
    /// Beat-jump pads' amounts ×1/16 / ×16.
    BeatJumpRangeSixteenth,
    BeatJumpRangeSixteen,
    /// Beat-loop pads' lengths halved / doubled.
    BeatLoopSizesHalf,
    BeatLoopSizesDouble,
    /// The controller's DECK button: this deck is the one its side controls.
    Select,
    /// Turns a stem part off or on: 0 vocals, 1 instruments, 2 bass, 3 drums.
    StemPart(u8),
    /// Part Isolator: the EQ knobs set the parts' levels instead.
    PartIsolator,
    /// A Part Isolator knob (controllers that send other controls for the
    /// EQ knobs while it is on): 0 vocals, 1 instruments, 2 drums.
    PartLevel(u8),
}

/// The stem parts, as rekordbox names them on the controllers.
pub const STEM_PARTS: [&str; 4] = ["VOCAL", "INST", "BASS", "DRUMS"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlobalAction {
    Crossfader,
    MasterVolume,
    HeadphoneMix,
    HeadphoneVolume,
    BrowseTurn,
    BrowsePress,
    BrowseBack,
    BrowseForward,
    Zoom,
    BrowsePageUp,
    BrowsePageDown,
    BrowseTop,
    BrowseBottom,
    /// Moves between the playlist tree and the track list.
    SwitchFocus,
    /// Adds the master mix to the headphones.
    MasterCue,
    ColorFxOn,
    /// Steps through the Color FX types.
    ColorFxSelect,
    FxDepth,
    FxOn,
    /// Release FX: cuts the effect's source and lets the echo play out.
    FxRelease,
    FxBeatDown,
    FxBeatUp,
    /// Back to the master deck's BPM after tapping.
    FxAutoBeat,
    FxTap,
    /// Beat FX type 0–13 (Delay … Mobius Tri).
    FxType(u8),
    /// Beat FX on channel 0–3, 4 = master, 5 = sampler.
    FxAssign(u8),
    CrossfaderStartPlayA,
    CrossfaderStartPlayB,
    CrossfaderStartCueA,
    CrossfaderStartCueB,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Deck 0–3.
    Deck(usize, DeckAction),
    Global(GlobalAction),
}

impl DeckAction {
    pub fn all() -> Vec<DeckAction> {
        use DeckAction::*;
        let mut all = vec![PlayPause, Cue, JumpToStart];
        all.extend((0..8).map(HotCue));
        all.extend((0..8).map(HotCueDelete));
        all.extend((0..8).map(HotCueGate));
        all.extend([
            HotCueGateMode,
            MemoryPrev,
            MemoryNext,
            MemorySet,
            MemoryDelete,
            LoopMoveBack,
            LoopMoveForward,
            JogSearch,
        ]);
        all.extend([CrossfaderAssignA, CrossfaderAssignThru, CrossfaderAssignB]);
        all.extend([
            Load,
            TempoFader,
            TempoRangeCycle,
            KeyLock,
            Sync,
            SyncMaster,
            Quantize,
            Vinyl,
            Slip,
            JogTouch,
            JogTurn,
            JogScratch,
            LoopIn,
            LoopOut,
            ReloopExit,
            AutoLoop,
            LoopHalf,
            LoopDouble,
            LoopResizeHalf,
            LoopResizeDouble,
        ]);
        all.extend((0..8).map(BeatJump));
        all.extend([BeatJumpBack, BeatJumpForward, BeatJumpRangeHalf, BeatJumpRangeDouble]);
        all.extend((0..8).map(BeatLoop));
        all.extend([Volume, Trim, EqLow, EqMid, EqHigh, Filter, HeadphoneCue]);
        all.extend([TapBpm, KeySync, KeyReset, KeyUp, KeyDown]);
        all.extend((0..8).map(KeyShift));
        all.extend([KeyShiftPageDown, KeyShiftPageUp]);
        all.extend((0..8).map(Keyboard));
        all.extend([KeyboardDown, KeyboardUp, ActiveLoop, FaderStartPlay, FaderStartCue]);
        all.extend((0..16).map(PadFx));
        all.extend((0..8).map(SamplerPlay));
        all.extend((0..8).map(SamplerStop));
        all.extend(self::PadMode::ALL.map(PadMode));
        all.extend([
            SamplerBankDown,
            SamplerBankUp,
            PadFxPageDown,
            PadFxPageUp,
            BeatJumpRangeSixteenth,
            BeatJumpRangeSixteen,
            BeatLoopSizesHalf,
            BeatLoopSizesDouble,
            Select,
            PartIsolator,
        ]);
        all.extend((0..4).map(StemPart));
        all.extend((0..3).map(PartLevel));
        all
    }

    fn name(self) -> String {
        use DeckAction::*;
        let letter = |n: u8| char::from(b'a' + n);
        match self {
            PlayPause => "play_pause".into(),
            Cue => "cue".into(),
            JumpToStart => "jump_to_start".into(),
            HotCue(n) => format!("hot_cue_{}", letter(n)),
            HotCueDelete(n) => format!("hot_cue_{}_delete", letter(n)),
            HotCueGate(n) => format!("hot_cue_{}_gate", letter(n)),
            HotCueGateMode => "hot_cue_gate_mode".into(),
            MemoryPrev => "memory_prev".into(),
            MemoryNext => "memory_next".into(),
            MemorySet => "memory_set".into(),
            MemoryDelete => "memory_delete".into(),
            LoopMoveBack => "loop_move_back".into(),
            LoopMoveForward => "loop_move_forward".into(),
            JogSearch => "jog_search".into(),
            CrossfaderAssignA => "crossfader_assign_a".into(),
            CrossfaderAssignThru => "crossfader_assign_thru".into(),
            CrossfaderAssignB => "crossfader_assign_b".into(),
            Load => "load".into(),
            TempoFader => "tempo".into(),
            TempoRangeCycle => "tempo_range".into(),
            KeyLock => "key_lock".into(),
            Sync => "sync".into(),
            SyncMaster => "sync_master".into(),
            Quantize => "quantize".into(),
            Vinyl => "vinyl".into(),
            Slip => "slip".into(),
            JogTouch => "jog_touch".into(),
            JogTurn => "jog_turn".into(),
            JogScratch => "jog_scratch".into(),
            LoopIn => "loop_in".into(),
            LoopOut => "loop_out".into(),
            ReloopExit => "reloop_exit".into(),
            AutoLoop => "auto_loop".into(),
            LoopHalf => "loop_half".into(),
            LoopDouble => "loop_double".into(),
            LoopResizeHalf => "loop_resize_half".into(),
            LoopResizeDouble => "loop_resize_double".into(),
            BeatJump(n) => format!("beat_jump_{}", n + 1),
            BeatJumpBack => "beat_jump_back".into(),
            BeatJumpForward => "beat_jump_forward".into(),
            BeatJumpRangeHalf => "beat_jump_range_half".into(),
            BeatJumpRangeDouble => "beat_jump_range_double".into(),
            BeatLoop(n) => format!("beat_loop_{}", n + 1),
            Volume => "volume".into(),
            Trim => "trim".into(),
            EqLow => "eq_low".into(),
            EqMid => "eq_mid".into(),
            EqHigh => "eq_high".into(),
            Filter => "filter".into(),
            HeadphoneCue => "headphone_cue".into(),
            TapBpm => "tap_bpm".into(),
            KeySync => "key_sync".into(),
            KeyReset => "key_reset".into(),
            KeyUp => "key_up".into(),
            KeyDown => "key_down".into(),
            KeyShift(n) => format!("key_shift_{}", n + 1),
            KeyShiftPageDown => "key_shift_page_down".into(),
            KeyShiftPageUp => "key_shift_page_up".into(),
            SamplerBankDown => "sampler_bank_down".into(),
            SamplerBankUp => "sampler_bank_up".into(),
            PadFxPageDown => "pad_fx_page_down".into(),
            PadFxPageUp => "pad_fx_page_up".into(),
            BeatJumpRangeSixteenth => "beat_jump_range_sixteenth".into(),
            BeatJumpRangeSixteen => "beat_jump_range_sixteen".into(),
            BeatLoopSizesHalf => "beat_loop_sizes_half".into(),
            BeatLoopSizesDouble => "beat_loop_sizes_double".into(),
            Select => "deck_select".into(),
            StemPart(n) => format!("stem_{}", STEM_PARTS[usize::from(n % 4)].to_lowercase()),
            PartIsolator => "part_isolator".into(),
            PartLevel(n) => {
                format!("part_isolator_{}", ["vocal", "inst", "drums"][usize::from(n % 3)])
            }
            PadMode(mode) => format!("pad_mode_{}", mode.name()),
            Keyboard(n) => format!("keyboard_{}", n + 1),
            KeyboardDown => "keyboard_down".into(),
            KeyboardUp => "keyboard_up".into(),
            ActiveLoop => "loop_active".into(),
            FaderStartPlay => "fader_start_play".into(),
            FaderStartCue => "fader_start_cue".into(),
            PadFx(n) => format!("pad_fx_{}", n + 1),
            SamplerPlay(n) => format!("sampler_{}", n + 1),
            SamplerStop(n) => format!("sampler_{}_stop", n + 1),
        }
    }

    pub fn kind(self) -> ActionKind {
        use DeckAction::*;
        match self {
            TempoFader | Volume | Trim | EqLow | EqMid | EqHigh | Filter | PartLevel(_) => {
                ActionKind::Absolute
            }
            JogTurn | JogScratch | JogSearch => ActionKind::Relative,
            _ => ActionKind::Button,
        }
    }
}

impl GlobalAction {
    pub fn all() -> Vec<GlobalAction> {
        use GlobalAction::*;
        let mut all = vec![
            Crossfader,
            MasterVolume,
            HeadphoneMix,
            HeadphoneVolume,
            BrowseTurn,
            BrowsePress,
            BrowseBack,
            BrowseForward,
            Zoom,
            BrowsePageUp,
            BrowsePageDown,
            BrowseTop,
            BrowseBottom,
            SwitchFocus,
            MasterCue,
            ColorFxOn,
            ColorFxSelect,
            FxDepth,
            FxOn,
            FxRelease,
            FxBeatDown,
            FxBeatUp,
            FxAutoBeat,
            FxTap,
        ];
        all.extend((0..14).map(FxType));
        all.extend((0..6).map(FxAssign));
        all.extend([
            CrossfaderStartPlayA,
            CrossfaderStartPlayB,
            CrossfaderStartCueA,
            CrossfaderStartCueB,
        ]);
        all
    }

    fn name(self) -> String {
        use GlobalAction::*;
        match self {
            Crossfader => "crossfader".into(),
            MasterVolume => "master_volume".into(),
            HeadphoneMix => "headphone_mix".into(),
            HeadphoneVolume => "headphone_volume".into(),
            BrowseTurn => "browse_turn".into(),
            BrowsePress => "browse_press".into(),
            BrowseBack => "browse_back".into(),
            BrowseForward => "browse_forward".into(),
            Zoom => "zoom".into(),
            BrowsePageUp => "browse_page_up".into(),
            BrowsePageDown => "browse_page_down".into(),
            BrowseTop => "browse_top".into(),
            BrowseBottom => "browse_bottom".into(),
            SwitchFocus => "switch_focus".into(),
            MasterCue => "master_cue".into(),
            ColorFxOn => "color_fx_on".into(),
            ColorFxSelect => "color_fx_select".into(),
            FxDepth => "fx_depth".into(),
            FxOn => "fx_on".into(),
            FxRelease => "fx_release".into(),
            FxBeatDown => "fx_beat_down".into(),
            FxBeatUp => "fx_beat_up".into(),
            FxAutoBeat => "fx_auto_beat".into(),
            FxTap => "fx_tap".into(),
            FxType(n) => format!("fx_type_{}", n + 1),
            FxAssign(n @ 0..=3) => format!("fx_assign_ch{}", n + 1),
            FxAssign(4) => "fx_assign_master".into(),
            FxAssign(_) => "fx_assign_sampler".into(),
            CrossfaderStartPlayA => "crossfader_start_play_a".into(),
            CrossfaderStartPlayB => "crossfader_start_play_b".into(),
            CrossfaderStartCueA => "crossfader_start_cue_a".into(),
            CrossfaderStartCueB => "crossfader_start_cue_b".into(),
        }
    }

    pub fn kind(self) -> ActionKind {
        use GlobalAction::*;
        match self {
            BrowseTurn | Zoom => ActionKind::Relative,
            Crossfader | MasterVolume | HeadphoneMix | HeadphoneVolume | FxDepth => {
                ActionKind::Absolute
            }
            _ => ActionKind::Button,
        }
    }
}

impl Action {
    /// Every action, deck by deck, then the global ones.
    pub fn all() -> Vec<Action> {
        (0..DECKS)
            .flat_map(|deck| DeckAction::all().into_iter().map(move |a| Action::Deck(deck, a)))
            .chain(GlobalAction::all().into_iter().map(Action::Global))
            .collect()
    }

    pub fn kind(self) -> ActionKind {
        match self {
            Action::Deck(_, action) => action.kind(),
            Action::Global(action) => action.kind(),
        }
    }
}

/// Written as `deck1.play_pause`, `deck2.hot_cue_c`, `crossfader`, ….
impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Deck(deck, action) => write!(f, "deck{}.{}", deck + 1, action.name()),
            Action::Global(action) => f.write_str(&action.name()),
        }
    }
}

impl FromStr for Action {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        Action::all()
            .into_iter()
            .find(|action| action.to_string() == text)
            .ok_or_else(|| format!("unknown action {text:?}"))
    }
}

/// State a controller can display (LEDs, meters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckFeedback {
    Playing,
    /// Lit while paused at the cue point.
    AtCue,
    HotCueSet(u8),
    Sync,
    SyncMaster,
    Quantize,
    Vinyl,
    Slip,
    KeyLock,
    LoopActive,
    HeadphoneCue,
    Loaded,
    /// Channel level, sent as a value 0–127.
    Level,
    /// Lit while the key is shifted to match (key sync).
    KeySync,
    /// Sampler pad: lit when its slot is loaded, flashing while it plays.
    SamplerPlaying(u8),
    /// Pad-mode button lit while that mode is selected.
    PadMode(PadMode),
    /// A pad of that mode, lit when it can be used (flashing when active).
    PadLit(PadMode, u8),
    /// A stem part is playing (0 vocals … 3 drums).
    StemOn(u8),
    PartIsolator,
}

/// State of the mixer-wide sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlobalFeedback {
    FxOn,
    FxRelease,
    ColorFxOn,
    MasterCue,
    /// Beat FX beat length, as the value that lights the controller's beat
    /// indicator for it.
    FxBeat,
}

impl GlobalFeedback {
    const ALL: [GlobalFeedback; 5] =
        [Self::FxOn, Self::FxRelease, Self::ColorFxOn, Self::MasterCue, Self::FxBeat];

    fn name(self) -> &'static str {
        match self {
            Self::FxOn => "fx_on",
            Self::FxRelease => "fx_release",
            Self::ColorFxOn => "color_fx_on",
            Self::MasterCue => "master_cue",
            Self::FxBeat => "fx_beat",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feedback {
    Deck(usize, DeckFeedback),
    Global(GlobalFeedback),
}

impl DeckFeedback {
    fn all() -> Vec<DeckFeedback> {
        use DeckFeedback::*;
        let mut all = vec![Playing, AtCue];
        all.extend((0..8).map(HotCueSet));
        all.extend([
            Sync,
            SyncMaster,
            Quantize,
            Vinyl,
            Slip,
            KeyLock,
            LoopActive,
            HeadphoneCue,
            Loaded,
            Level,
            KeySync,
        ]);
        all.extend((0..8).map(SamplerPlaying));
        all.extend((0..4).map(StemOn));
        all.push(PartIsolator);
        all.extend(self::PadMode::ALL.map(PadMode));
        for mode in [
            self::PadMode::Keyboard,
            self::PadMode::PadFx,
            self::PadMode::BeatJump,
            self::PadMode::BeatLoop,
            self::PadMode::KeyShift,
        ] {
            all.extend(
                (0..if mode == self::PadMode::PadFx { 16 } else { 8 }).map(|n| PadLit(mode, n)),
            );
        }
        all
    }

    fn name(self) -> String {
        use DeckFeedback::*;
        match self {
            Playing => "playing".into(),
            AtCue => "at_cue".into(),
            HotCueSet(n) => format!("hot_cue_{}_set", char::from(b'a' + n)),
            Sync => "sync".into(),
            SyncMaster => "sync_master".into(),
            Quantize => "quantize".into(),
            Vinyl => "vinyl".into(),
            Slip => "slip".into(),
            KeyLock => "key_lock".into(),
            LoopActive => "loop_active".into(),
            HeadphoneCue => "headphone_cue".into(),
            Loaded => "loaded".into(),
            Level => "level".into(),
            KeySync => "key_sync".into(),
            SamplerPlaying(n) => format!("sampler_{}_playing", n + 1),
            PadMode(mode) => format!("pad_mode_{}", mode.name()),
            PadLit(mode, n) => format!("{}_pad_{}", mode.name(), n + 1),
            StemOn(n) => format!("stem_{}_on", STEM_PARTS[usize::from(n % 4)].to_lowercase()),
            PartIsolator => "part_isolator".into(),
        }
    }
}

impl fmt::Display for Feedback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Feedback::Deck(deck, what) => write!(f, "deck{}.{}", deck + 1, what.name()),
            Feedback::Global(what) => f.write_str(what.name()),
        }
    }
}

impl FromStr for Feedback {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        (0..DECKS)
            .flat_map(|deck| {
                DeckFeedback::all().into_iter().map(move |what| Feedback::Deck(deck, what))
            })
            .chain(GlobalFeedback::ALL.map(Feedback::Global))
            .find(|f| f.to_string() == text)
            .ok_or_else(|| format!("unknown feedback {text:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for action in Action::all() {
            assert_eq!(action.to_string().parse::<Action>(), Ok(action));
        }
        assert_eq!("deck4.hot_cue_c".parse(), Ok(Action::Deck(3, DeckAction::HotCue(2))));
        assert_eq!("deck3.beat_loop_8".parse(), Ok(Action::Deck(2, DeckAction::BeatLoop(7))));
        let feedback = Feedback::Deck(1, DeckFeedback::HotCueSet(3));
        assert_eq!(feedback.to_string().parse(), Ok(feedback));
        assert_eq!("fx_on".parse(), Ok(Feedback::Global(GlobalFeedback::FxOn)));
        assert_eq!("fx_assign_master".parse(), Ok(Action::Global(GlobalAction::FxAssign(4))));
    }

    #[test]
    fn pads_follow_rekordbox_layout() {
        use DeckAction as D;
        // Beat Jump: pads jump back/forward; Shift + 7 / 8 change the amounts.
        assert_eq!(PadMode::BeatJump.pad_action(0, false), Some(D::BeatJump(0)));
        assert_eq!(PadMode::BeatJump.pad_action(6, true), Some(D::BeatJumpRangeHalf));
        assert_eq!(PadMode::BeatJump.pad_action(7, true), Some(D::BeatJumpRangeDouble));
        assert_eq!(PadMode::BeatJump.pad_action(2, true), None);
        // Keyboard: Shift + 1–6 delete hot cues, 7 / 8 change page.
        assert_eq!(PadMode::Keyboard.pad_action(5, true), Some(D::HotCueDelete(5)));
        assert_eq!(PadMode::Keyboard.pad_action(7, true), Some(D::KeyboardUp));
        // Pad FX: Shift reaches effects I–P.
        assert_eq!(PadMode::PadFx.pad_action(0, true), Some(D::PadFx(8)));
        assert_eq!(PadMode::Sampler.pad_action(3, true), Some(D::SamplerStop(3)));
        assert_eq!(PadMode::KeyShift.pad_action(6, true), Some(D::KeyShiftPageDown));
        assert_eq!(
            "deck2.pad_mode_beat_jump".parse(),
            Ok(Action::Deck(1, D::PadMode(PadMode::BeatJump)))
        );
    }

    #[test]
    fn parameter_buttons_per_mode() {
        use DeckAction as D;
        assert_eq!(PadMode::BeatJump.parameter_action(false, false), Some(D::BeatJumpRangeHalf));
        assert_eq!(PadMode::BeatJump.parameter_action(true, true), Some(D::BeatJumpRangeSixteen));
        assert_eq!(PadMode::Sampler.parameter_action(true, false), Some(D::SamplerBankUp));
        assert_eq!(PadMode::BeatLoop.parameter_action(false, false), Some(D::BeatLoopSizesHalf));
        assert_eq!(PadMode::Stems.parameter_action(true, false), None);
    }
}
