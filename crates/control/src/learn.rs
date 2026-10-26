//! Learning a binding by moving a control.

use crate::action::{Action, ActionKind};
use crate::mapping::{Binding, Encoding, MessageType};
use crate::midi::MidiMessage;

/// Watches incoming messages until it can tell which control the user moved
/// and how it encodes its values.
#[derive(Debug, Clone)]
pub struct Learner {
    action: Action,
    controls: Vec<(u8, u8, u8)>,
}

/// Messages from a moved fader or wheel before deciding on its encoding.
const SAMPLES: usize = 8;

impl Learner {
    pub fn new(action: Action) -> Self {
        Self { action, controls: Vec::new() }
    }

    pub fn action(&self) -> Action {
        self.action
    }

    /// Feeds one message; returns the binding once it is clear.
    pub fn feed(&mut self, message: MidiMessage) -> Option<Binding> {
        let binding = |message, channel: u8, number, encoding, lsb| Binding {
            action: self.action,
            message,
            channel: channel + 1,
            number,
            encoding,
            lsb,
            invert: false,
            shift: false,
        };
        match (self.action.kind(), message) {
            (ActionKind::Button, MidiMessage::NoteOn { channel, note, .. }) => {
                Some(binding(MessageType::Note, channel, note, Encoding::Button, None))
            }
            (ActionKind::Button, MidiMessage::ControlChange { channel, control, value })
                if value > 0 =>
            {
                Some(binding(MessageType::Cc, channel, control, Encoding::Button, None))
            }
            (ActionKind::Absolute, MidiMessage::PitchBend { channel, .. }) => {
                Some(binding(MessageType::PitchBend, channel, 0, Encoding::Absolute14, None))
            }
            (
                ActionKind::Absolute | ActionKind::Relative,
                MidiMessage::ControlChange { channel, control, value },
            ) => {
                self.controls.push((channel, control, value));
                if self.controls.len() < SAMPLES {
                    return None;
                }
                let (channel, msb) = self.dominant_control();
                if self.action.kind() == ActionKind::Absolute {
                    // Pioneer-style 14-bit pairs send the LSB on controller + 32.
                    let lsb = msb + 0x20;
                    let has_lsb = self.controls.iter().any(|&(c, n, _)| c == channel && n == lsb);
                    let (encoding, lsb) = if has_lsb {
                        (Encoding::Absolute14, Some(lsb))
                    } else {
                        (Encoding::Absolute7, None)
                    };
                    Some(binding(MessageType::Cc, channel, msb, encoding, lsb))
                } else {
                    let values: Vec<u8> = self
                        .controls
                        .iter()
                        .filter(|&&(c, n, _)| c == channel && n == msb)
                        .map(|&(_, _, v)| v)
                        .collect();
                    let near_64 = values.iter().filter(|&&v| (48..=80).contains(&v)).count();
                    let encoding = if near_64 * 2 >= values.len() {
                        Encoding::RelativeOffset64
                    } else {
                        Encoding::RelativeTwos
                    };
                    Some(binding(MessageType::Cc, channel, msb, encoding, None))
                }
            }
            _ => None,
        }
    }

    /// The (channel, controller) that sent the most messages, preferring the
    /// lower number of a 14-bit pair.
    fn dominant_control(&self) -> (u8, u8) {
        let mut counts: Vec<((u8, u8), usize)> = Vec::new();
        for &(channel, control, _) in &self.controls {
            match counts.iter_mut().find(|(key, _)| *key == (channel, control)) {
                Some((_, count)) => *count += 1,
                None => counts.push(((channel, control), 1)),
            }
        }
        counts.sort_by_key(|&((_, control), count)| (std::cmp::Reverse(count), control));
        let (channel, control) = counts[0].0;
        // If the winner is the LSB half of a pair, report its MSB.
        if control >= 0x20 && counts.iter().any(|&((c, n), _)| c == channel && n == control - 0x20)
        {
            (channel, control - 0x20)
        } else {
            (channel, control)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckAction;

    #[test]
    fn learns_button() {
        let mut learner = Learner::new(Action::Deck(0, DeckAction::Cue));
        let binding =
            learner.feed(MidiMessage::NoteOn { channel: 0, note: 12, velocity: 127 }).unwrap();
        assert_eq!((binding.message, binding.channel, binding.number), (MessageType::Note, 1, 12));
    }

    #[test]
    fn learns_14_bit_fader() {
        let mut learner = Learner::new(Action::Deck(1, DeckAction::Volume));
        let mut result = None;
        for value in 0..8 {
            result = result.or(learner.feed(MidiMessage::ControlChange {
                channel: 1,
                control: 0x13,
                value,
            }));
            result = result.or(learner.feed(MidiMessage::ControlChange {
                channel: 1,
                control: 0x33,
                value,
            }));
        }
        let binding = result.unwrap();
        assert_eq!(
            (binding.channel, binding.number, binding.encoding, binding.lsb),
            (2, 0x13, Encoding::Absolute14, Some(0x33))
        );
    }

    #[test]
    fn learns_jog_encoding() {
        let mut learner = Learner::new(Action::Deck(0, DeckAction::JogTurn));
        let mut result = None;
        for value in [65, 66, 65, 63, 62, 65, 66, 67] {
            result = result.or(learner.feed(MidiMessage::ControlChange {
                channel: 0,
                control: 0x21,
                value,
            }));
        }
        assert_eq!(result.unwrap().encoding, Encoding::RelativeOffset64);
    }
}
