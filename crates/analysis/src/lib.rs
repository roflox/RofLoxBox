//! Track analysis for deckhand: waveforms, beat grid and key worked out
//! from the audio, in the forms rekordbox stores in its analysis files.
//!
//! The waveform methods follow how rekordbox's analysis behaves, learned by
//! studying it; the code is written from that description. Beat grid and
//! key detection are our own (rekordbox uses a neural network for beats).

pub mod beats;
pub mod key;
pub mod preview;
mod spectrum;
pub mod waveform;
