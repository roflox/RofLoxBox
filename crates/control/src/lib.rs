//! Controller input for deckhand: MIDI ports, mappings from MIDI messages to
//! actions, and learning mappings by moving controls.

mod action;
mod learn;
mod mapping;
mod midi;
pub mod pioneer;
pub mod rekordbox;

pub use action::{
    Action, ActionKind, DECKS, DeckAction, DeckFeedback, Feedback, GlobalAction, GlobalFeedback,
    PadMode,
};
pub use learn::Learner;
pub use mapping::{
    Binding, ControlEvent, Encoding, Mapping, MessageType, Output, ShiftControl, Translator, Value,
};
pub use midi::{MidiInput, MidiMessage, MidiOutput, input_ports, output_ports};
