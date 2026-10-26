//! Option values of the settings files.
//!
//! Every value was observed in a fixture where only that option was changed,
//! except where a doc comment says *inferred*: those are defaults (the one
//! option no fixture changes to) or gaps in an evenly spaced sequence, such as
//! the 6 dB steps of [`AutoCueLevel`].

/// Defines an option enum stored as one byte, with `Unknown` for other bytes.
macro_rules! option_byte {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $byte:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
            Unknown(u8),
        }

        impl $name {
            pub fn from_byte(byte: u8) -> Self {
                match byte {
                    $($byte => Self::$variant,)+
                    other => Self::Unknown(other),
                }
            }

            pub fn to_byte(self) -> u8 {
                match self {
                    $(Self::$variant => $byte,)+
                    Self::Unknown(byte) => byte,
                }
            }
        }
    };
}

option_byte!(OnOff { Off = 0x80, On = 0x81 });

option_byte!(
    /// Brightness of rings and slot lights on players.
    Illumination {
        Off = 0x80,
        Dark = 0x81,
        /// Inferred (default).
        Bright = 0x82,
    }
);

option_byte!(
    /// Threshold for setting the automatic cue at the start of a track.
    AutoCueLevel {
        Minus36Db = 0x80,
        Minus42Db = 0x81,
        /// Inferred.
        Minus48Db = 0x82,
        /// Inferred.
        Minus54Db = 0x83,
        /// Inferred.
        Minus60Db = 0x84,
        /// Inferred.
        Minus66Db = 0x85,
        Minus72Db = 0x86,
        Minus78Db = 0x87,
        /// Inferred (default): cue at the first memory cue.
        Memory = 0x88,
    }
);

option_byte!(
    /// Player menu language. Only the values seen in fixtures are named.
    Language {
        /// Inferred (default).
        English = 0x81,
        French = 0x82,
        Greek = 0x91,
        Turkish = 0x92,
    }
);

option_byte!(QuantizeBeatValue {
    /// Inferred (default).
    One = 0x80,
    Half = 0x81,
    Quarter = 0x82,
    Eighth = 0x83,
});

option_byte!(HotCueAutoload {
    Off = 0x80,
    /// Inferred (default).
    On = 0x81,
    /// Follow the per-track setting from rekordbox.
    RekordboxSetting = 0x82,
});

option_byte!(TimeMode {
    Elapsed = 0x80,
    /// Inferred (default).
    Remain = 0x81,
});

option_byte!(JogMode {
    Cdj = 0x80,
    /// Inferred (default).
    Vinyl = 0x81,
});

option_byte!(TempoRange {
    Six = 0x80,
    /// Inferred (default): ±10 %, the only range between ±6 % and ±16 %.
    Ten = 0x81,
    Sixteen = 0x82,
    Wide = 0x83,
});

option_byte!(PhaseMeter {
    /// Inferred (default).
    Type1 = 0x80,
    Type2 = 0x81,
});

option_byte!(VinylSpeedAdjust {
    TouchAndRelease = 0x80,
    /// Inferred (default).
    Touch = 0x81,
    Release = 0x82,
});

option_byte!(JogDisplayMode {
    /// Inferred (default).
    Auto = 0x80,
    Info = 0x81,
    Simple = 0x82,
    Artwork = 0x83,
});

option_byte!(WaveformDivisions {
    TimeScale = 0x80,
    /// Inferred (default).
    Phrase = 0x81,
});

option_byte!(
    /// What the player's waveform area shows.
    WaveformArea {
        /// Inferred (default).
        Waveform = 0x80,
        PhaseMeter = 0x81,
    }
);

option_byte!(
    /// Beat jump distance. The values double from ½ to 64 beats.
    BeatJumpBeatValue {
        Half = 0x80,
        One = 0x81,
        /// Inferred.
        Two = 0x82,
        /// Inferred.
        Four = 0x83,
        /// Inferred.
        Eight = 0x84,
        /// Inferred (default).
        Sixteen = 0x85,
        /// Inferred.
        ThirtyTwo = 0x86,
        SixtyFour = 0x87,
    }
);

option_byte!(OverviewWaveformType {
    /// Inferred (default).
    Half = 0x01,
    Full = 0x02,
});

option_byte!(WaveformColor {
    /// Inferred (default).
    Blue = 0x01,
    Rgb = 0x03,
    ThreeBand = 0x04,
});

option_byte!(KeyDisplayFormat {
    /// Inferred (default).
    Classic = 0x01,
    Alphanumeric = 0x02,
});

option_byte!(WaveformCurrentPosition {
    /// Inferred (default).
    Center = 0x01,
    Left = 0x02,
});

option_byte!(ChannelFaderCurve {
    SteepTop = 0x80,
    /// Inferred (default).
    Linear = 0x81,
    SteepBottom = 0x82,
});

option_byte!(
    /// Channel fader curve for long-throw faders.
    LongChannelFaderCurve {
        /// Inferred (default).
        Exponential = 0x80,
        /// Named "exp2" in the fixtures.
        Exponential2 = 0x81,
        Linear = 0x82,
    }
);

option_byte!(CrossfaderCurve {
    ConstantPower = 0x80,
    SlowCut = 0x81,
    /// Inferred (default).
    FastCut = 0x82,
});

option_byte!(HeadphonesMix {
    /// Inferred (default).
    PostEq = 0x80,
    PreEq = 0x81,
});

option_byte!(HeadphonesMonitoring {
    /// Inferred (default).
    Stereo = 0x80,
    MonoSplit = 0x81,
});

option_byte!(TalkOverMode {
    /// Inferred (default).
    Advanced = 0x80,
    Normal = 0x81,
});

option_byte!(TalkOverLevel {
    Minus24Db = 0x80,
    /// Inferred (default).
    Minus18Db = 0x81,
    Minus12Db = 0x82,
    Minus6Db = 0x83,
});

option_byte!(MidiButtonType {
    /// Inferred (default).
    Toggle = 0x80,
    Trigger = 0x81,
});

/// Mixer display brightness: white, or a level from 1 to 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayBrightness {
    White,
    Level(u8),
    Unknown(u8),
}

impl DisplayBrightness {
    pub fn from_byte(byte: u8) -> Self {
        match byte {
            0x80 => Self::White,
            0x81..=0x85 => Self::Level(byte - 0x80),
            other => Self::Unknown(other),
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            Self::White => 0x80,
            Self::Level(level) => 0x80 + level,
            Self::Unknown(byte) => byte,
        }
    }
}
