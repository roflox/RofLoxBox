//! Audio for RofLoxBox: decoding tracks and the real-time two-deck mixer.

mod decode;
mod encode;
mod engine;
mod eq;
mod fx;
mod mirror;
mod preview;
mod record;
mod resample;
mod session;
mod stems;
mod stretch;

pub use decode::{
    BandSplit, Frames, TrackAudio, band_waveform, band_waveform_at, decode_file, peak_waveform,
    peak_waveform_at,
};
pub use engine::{
    AudioOptions, Command, CrossfaderCurve, CrossfaderSide, DECKS, Engine, FaderCurve, JogSender,
    OutputInfo, Quality, SAMPLER_SLOTS, channel_gain,
};
pub use encode::{Encoding, time_offset, write_track};
pub use eq::{EQ_MAX_DB, EQ_MIN_DB, EqSettings, Equalizer, Filter};
pub use fx::{BeatFx, BeatFxKind, BeatFxParams, ColorFx, ColorFxKind, FxTarget};
pub use preview::SessionPlayer;
pub use resample::resample;
pub use record::{Recording, write_float_wav_file, write_wav_file};
pub use session::{
    Clash, Clip, Comparison, DeleteOptions, Edit, Move, is_edit, orig_path, Curve, Exported, Files, FxSpan, Grid, Nudge, Outline, PlayEdges, Played, Point, Rendered, SYNC_CHECK_SECS,
    SYNC_DRIFT_BEATS, Session, Setting, Sources, StemLevels, StemSpan, StemsSource, SyncSpan, TrackMove, TrackSpan,
    clashes, fx_spans, lane_value, move_fx, remember_grid, remember_stems, remember_track,
    export_files, remove_fx, restyle_fx, session_path,
};
pub use stems::{BASS, DRUMS, INSTRUMENTS, PARTS, StemLayers, VOCALS, fit_gains};
pub use stretch::Stretcher;
