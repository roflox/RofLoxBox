//! Audio for deckhand: decoding tracks and the real-time two-deck mixer.

mod decode;
mod engine;
mod eq;
mod fx;
mod record;
mod stems;
mod stretch;

pub use decode::{Frames, TrackAudio, band_waveform, decode_file, peak_waveform};
pub use engine::{
    AudioOptions, Command, CrossfaderCurve, CrossfaderSide, DECKS, Engine, FaderCurve, JogSender,
    OutputInfo, Quality, SAMPLER_SLOTS, channel_gain,
};
pub use eq::{EQ_MAX_DB, EQ_MIN_DB, EqSettings, Equalizer, Filter};
pub use fx::{BeatFx, BeatFxKind, BeatFxParams, ColorFx, ColorFxKind, FxTarget};
pub use record::{Recording, write_float_wav_file, write_wav_file};
pub use stems::{BASS, DRUMS, INSTRUMENTS, PARTS, StemLayers, VOCALS, fit_gains};
pub use stretch::Stretcher;
