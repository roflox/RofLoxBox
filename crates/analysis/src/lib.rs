//! Track analysis for deckhand: waveforms, beat grid and key worked out
//! from the audio, in the forms rekordbox stores in its analysis files.
//!
//! The waveforms match the ones in rekordbox's analysis files, checked
//! against real exports. Beat grid and key detection are our own.

pub mod beats;
pub mod key;
pub mod preview;
mod spectrum;
pub mod waveform;
