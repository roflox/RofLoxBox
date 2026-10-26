//! Audio for deckhand: decoding tracks and the real-time two-deck mixer.

mod decode;
mod engine;
mod eq;
mod fx;
mod stretch;

pub use decode::{TrackAudio, band_waveform, decode_file, peak_waveform};
pub use engine::{
    AudioOptions, Command, CrossfaderCurve, CrossfaderSide, DECKS, Engine, FaderCurve, OutputInfo,
    Quality, SAMPLER_SLOTS,
};
pub use eq::{EQ_MAX_DB, EQ_MIN_DB, Equalizer, Filter};
pub use fx::{BeatFx, BeatFxKind, BeatFxParams, ColorFx, ColorFxKind, FxTarget};
pub use stretch::Stretcher;
