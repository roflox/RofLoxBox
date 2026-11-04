//! Mappings: which MIDI message drives which action, stored as TOML.
//!
//! ```toml
//! name = "AlphaTheta GRV6"
//!
//! [[binding]]
//! action = "deck1.play_pause"
//! message = "note"
//! channel = 1
//! number = 11
//! encoding = "button"
//!
//! [[binding]]
//! action = "deck1.volume"
//! message = "cc"
//! channel = 1
//! number = 19
//! encoding = "absolute14"
//! lsb = 51
//! ```

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::action::{Action, Feedback};
use crate::midi::MidiMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Note,
    Cc,
    PitchBend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    /// Note on/off, or a CC that is non-zero while held.
    Button,
    /// One 7-bit CC.
    Absolute7,
    /// A 7-bit MSB CC plus an LSB CC (`lsb`), or a pitch bend.
    Absolute14,
    /// Steps around 64: 65 = +1, 63 = −1.
    RelativeOffset64,
    /// Two's complement steps: 1 = +1, 127 = −1.
    RelativeTwos,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    #[serde(with = "action_name")]
    pub action: Action,
    pub message: MessageType,
    /// 1–16, as MIDI channels are usually numbered.
    pub channel: u8,
    /// Note or controller number; ignored for pitch bend.
    #[serde(default)]
    pub number: u8,
    pub encoding: Encoding,
    /// LSB controller of a 14-bit CC pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsb: Option<u8>,
    /// Reverse an absolute control (for example a tempo fader whose top is 0).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
    /// Applies only while a Shift button is held.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shift: bool,
}

/// A control that works as Shift: while it is held, bindings marked `shift`
/// take over from the normal ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShiftControl {
    pub message: MessageType,
    /// 1–16.
    pub channel: u8,
    pub number: u8,
}

mod action_name {
    use super::Action;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(action: &Action, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&action.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Action, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

/// A message the controller understands as display state: a note lights an
/// LED with velocity 127 (0 = off); a CC carries a value (meters).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    #[serde(with = "feedback_name")]
    pub feedback: Feedback,
    pub message: MessageType,
    pub channel: u8,
    pub number: u8,
}

mod feedback_name {
    use super::Feedback;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(feedback: &Feedback, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&feedback.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Feedback, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    pub name: String,
    #[serde(default, rename = "binding")]
    pub bindings: Vec<Binding>,
    #[serde(default, rename = "output")]
    pub outputs: Vec<Output>,
    /// Left-to-right order of the controller's mixer channels (1-based),
    /// for example `[3, 1, 2, 4]`, so the screen can match the hardware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_order: Option<[usize; 4]>,
    /// Shift buttons (one per deck on most controllers).
    #[serde(default, rename = "shift", skip_serializing_if = "Vec::is_empty")]
    pub shift_controls: Vec<ShiftControl>,
    /// Jog steps the controller sends for one full turn of a wheel; when
    /// unset, the known value for the model (see [`Mapping::jog_steps_per_turn`]).
    #[serde(default, rename = "jog_steps_per_turn", skip_serializing_if = "Option::is_none")]
    pub jog_resolution: Option<u32>,
    /// Functions the controller's rekordbox mapping gave controls when this
    /// mapping was made or last brought up to date: the ones it gains later
    /// are added once (see [`Mapping::add_new_functions`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offered: Vec<String>,
}

/// Jog steps per turn of models whose wheels differ from the common 720.
const JOG_STEPS_PER_TURN: &[(&str, u32)] = &[("DDJ-GRV6", 6480)];
/// Jog steps per turn on most Pioneer DDJ controllers.
const DEFAULT_JOG_STEPS_PER_TURN: u32 = 720;

impl Mapping {
    /// How many jog steps make one full turn of a wheel.
    pub fn jog_steps_per_turn(&self) -> u32 {
        self.jog_resolution.filter(|&n| n > 0).unwrap_or_else(|| {
            JOG_STEPS_PER_TURN
                .iter()
                .find(|(model, _)| self.name.eq_ignore_ascii_case(model))
                .map_or(DEFAULT_JOG_STEPS_PER_TURN, |&(_, n)| n)
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Adds `binding`, replacing earlier bindings of the same action and of
    /// the same MIDI control in the same layer (with or without Shift).
    pub fn bind(&mut self, binding: Binding) {
        self.bindings.retain(|b| {
            b.action != binding.action
                && !(b.message == binding.message
                    && b.channel == binding.channel
                    && b.number == binding.number
                    && b.shift == binding.shift)
        });
        self.bindings.push(binding);
    }

    /// Adds what `other` has and this mapping lacks: bindings for functions
    /// with no binding here, on controls not already used here (in the same
    /// Shift layer); lights not mapped yet; Shift buttons. Nothing here is
    /// changed. Returns (bindings, lights) added.
    /// Adds the other mapping's lights for things this one lights nothing
    /// for; returns how many were added.
    pub fn add_missing_outputs(&mut self, other: &Mapping) -> usize {
        let mut added = 0;
        for output in &other.outputs {
            if !self.outputs.iter().any(|o| o.feedback == output.feedback) {
                self.outputs.push(output.clone());
                added += 1;
            }
        }
        added
    }

    pub fn add_missing(&mut self, other: &Mapping) -> (usize, usize) {
        let bindings = self.add_bindings(other.bindings.iter());
        let outputs = self.add_missing_outputs(other);
        for control in &other.shift_controls {
            if !self.shift_controls.contains(control) {
                self.shift_controls.push(*control);
            }
        }
        (bindings, outputs)
    }

    /// Bindings for functions with no binding here, on controls not used
    /// here yet (in the same Shift layer); returns how many were added.
    fn add_bindings<'a>(&mut self, bindings: impl Iterator<Item = &'a Binding>) -> usize {
        let same_control = |a: &Binding, b: &Binding| {
            a.message == b.message
                && a.channel == b.channel
                && (a.number == b.number || a.lsb == Some(b.number) || b.lsb == Some(a.number))
                && a.shift == b.shift
        };
        let mut added = 0;
        for binding in bindings {
            let has_action = self.bindings.iter().any(|b| b.action == binding.action);
            let control_used = self.bindings.iter().any(|b| same_control(b, binding));
            if !has_action && !control_used {
                self.bindings.push(binding.clone());
                added += 1;
            }
        }
        added
    }

    /// Brings a saved mapping up to date with `import`, a fresh conversion of
    /// the controller's rekordbox mapping: functions RofLoxBox learned since
    /// get their controls (where free), and lights it learned to drive are
    /// added. A function offered before and since cleared stays cleared.
    /// Returns whether anything changed.
    pub fn add_new_functions(&mut self, import: &Mapping) -> bool {
        let offered = std::mem::take(&mut self.offered);
        let new = import.bindings.iter().filter(|b| !offered.contains(&b.action.to_string()));
        let added = self.add_bindings(new) + self.add_missing_outputs(import);
        self.offered = offered;
        let offered = import.offered.clone();
        let changed = added > 0 || offered != self.offered;
        self.offered = offered;
        changed
    }

    /// The same mapping under another name.
    pub fn renamed(mut self, name: &str) -> Self {
        self.name = name.to_owned();
        self
    }

    pub fn binding_for(&self, action: Action) -> Option<&Binding> {
        self.bindings.iter().find(|b| b.action == action)
    }
}

/// A control value delivered to an action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Pressed(bool),
    /// 0.0–1.0
    Absolute(f32),
    /// Steps; positive = clockwise / forward.
    Relative(i32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlEvent {
    pub action: Action,
    pub value: Value,
}

/// Turns MIDI messages into control events using a mapping. Keeps the state
/// needed to combine 14-bit controller pairs.
#[derive(Debug, Default)]
pub struct Translator {
    mapping: Mapping,
    /// Last MSB / LSB per (channel, msb controller).
    halves: HashMap<(u8, u8), (u8, u8)>,
    /// Shift buttons held down.
    held_shift: std::collections::HashSet<ShiftControl>,
    /// Actions a held button pressed, by (type, channel 0–15, number).
    pressed: HashMap<(MessageType, u8, u8), Vec<Action>>,
}

impl Translator {
    pub fn new(mapping: Mapping) -> Self {
        Self { mapping, ..Self::default() }
    }

    pub fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    pub fn set_mapping(&mut self, mapping: Mapping) {
        self.mapping = mapping;
        self.halves.clear();
        self.held_shift.clear();
        self.pressed.clear();
    }

    /// Whether a Shift button is held.
    pub fn shift_held(&self) -> bool {
        !self.held_shift.is_empty()
    }

    /// Tracks Shift buttons; returns `true` if `message` came from one.
    pub fn observe_shift(&mut self, message: &MidiMessage) -> bool {
        let (control, down) = match *message {
            MidiMessage::NoteOn { channel, note, .. } => (
                ShiftControl { message: MessageType::Note, channel: channel + 1, number: note },
                true,
            ),
            MidiMessage::NoteOff { channel, note } => (
                ShiftControl { message: MessageType::Note, channel: channel + 1, number: note },
                false,
            ),
            MidiMessage::ControlChange { channel, control, value } => (
                ShiftControl { message: MessageType::Cc, channel: channel + 1, number: control },
                value > 0,
            ),
            MidiMessage::PitchBend { .. } => return false,
        };
        if !self.mapping.shift_controls.contains(&control) {
            return false;
        }
        if down {
            self.held_shift.insert(control);
        } else {
            self.held_shift.remove(&control);
        }
        true
    }

    pub fn translate(&mut self, message: MidiMessage) -> Vec<ControlEvent> {
        if self.observe_shift(&message) {
            return Vec::new();
        }
        let shifted = self.shift_held();
        let mut candidates: Vec<(bool, ControlEvent)> = Vec::new();
        for binding in &self.mapping.bindings {
            let channel = binding.channel.saturating_sub(1);
            let value = match (message, binding.message) {
                (MidiMessage::NoteOn { channel: c, note, .. }, MessageType::Note)
                    if c == channel && note == binding.number =>
                {
                    Value::Pressed(true)
                }
                (MidiMessage::NoteOff { channel: c, note }, MessageType::Note)
                    if c == channel && note == binding.number =>
                {
                    Value::Pressed(false)
                }
                (MidiMessage::PitchBend { channel: c, value }, MessageType::PitchBend)
                    if c == channel =>
                {
                    Value::Absolute(f32::from(value) / 16383.0)
                }
                (MidiMessage::ControlChange { channel: c, control, value }, MessageType::Cc)
                    if c == channel =>
                {
                    let is_msb = control == binding.number;
                    let is_lsb = binding.lsb == Some(control);
                    if !is_msb && !is_lsb {
                        continue;
                    }
                    match binding.encoding {
                        Encoding::Button => Value::Pressed(value > 0),
                        Encoding::Absolute7 => Value::Absolute(f32::from(value) / 127.0),
                        Encoding::Absolute14 => {
                            let halves = self.halves.entry((c, binding.number)).or_insert((0, 0));
                            if is_msb {
                                halves.0 = value;
                            } else {
                                halves.1 = value;
                            }
                            Value::Absolute(
                                f32::from(u16::from(halves.0) << 7 | u16::from(halves.1)) / 16383.0,
                            )
                        }
                        Encoding::RelativeOffset64 => Value::Relative(i32::from(value) - 64),
                        Encoding::RelativeTwos => Value::Relative(if value < 64 {
                            i32::from(value)
                        } else {
                            i32::from(value) - 128
                        }),
                    }
                }
                _ => continue,
            };
            let value = match value {
                Value::Absolute(v) if binding.invert => Value::Absolute(1.0 - v),
                other => other,
            };
            candidates.push((binding.shift, ControlEvent { action: binding.action, value }));
        }
        // With Shift held, Shift bindings win; controls without one keep
        // their normal binding (controllers that send other notes on Shift).
        let use_shift = shifted && candidates.iter().any(|(s, _)| *s);
        let events: Vec<ControlEvent> =
            candidates.into_iter().filter(|(s, _)| *s == use_shift).map(|(_, e)| e).collect();
        // A button releases what it pressed, even if Shift changed meanwhile.
        let key = match message {
            MidiMessage::NoteOn { channel, note, .. } | MidiMessage::NoteOff { channel, note } => {
                Some((MessageType::Note, channel, note))
            }
            MidiMessage::ControlChange { channel, control, .. } => {
                Some((MessageType::Cc, channel, control))
            }
            MidiMessage::PitchBend { .. } => None,
        };
        let Some(key) = key else { return events };
        match events.first().map(|e| e.value) {
            Some(Value::Pressed(true)) => {
                self.pressed.insert(key, events.iter().map(|e| e.action).collect());
                events
            }
            Some(Value::Pressed(false)) => match self.pressed.remove(&key) {
                Some(actions) => actions
                    .into_iter()
                    .map(|action| ControlEvent { action, value: Value::Pressed(false) })
                    .collect(),
                None => events,
            },
            _ => events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{DeckAction, GlobalAction};

    fn mapping() -> Mapping {
        Mapping {
            name: "test".into(),
            bindings: vec![
                Binding {
                    action: Action::Deck(0, DeckAction::PlayPause),
                    message: MessageType::Note,
                    channel: 1,
                    number: 11,
                    encoding: Encoding::Button,
                    lsb: None,
                    invert: false,
                    shift: false,
                },
                Binding {
                    action: Action::Deck(0, DeckAction::Volume),
                    message: MessageType::Cc,
                    channel: 1,
                    number: 19,
                    encoding: Encoding::Absolute14,
                    lsb: Some(51),
                    invert: false,
                    shift: false,
                },
                Binding {
                    action: Action::Deck(1, DeckAction::JogTurn),
                    message: MessageType::Cc,
                    channel: 2,
                    number: 33,
                    encoding: Encoding::RelativeOffset64,
                    lsb: None,
                    invert: false,
                    shift: false,
                },
                Binding {
                    action: Action::Global(GlobalAction::BrowseTurn),
                    message: MessageType::Cc,
                    channel: 7,
                    number: 64,
                    encoding: Encoding::RelativeTwos,
                    lsb: None,
                    invert: false,
                    shift: false,
                },
            ],
            outputs: Vec::new(),
            channel_order: None,
            shift_controls: Vec::new(),
            jog_resolution: None,
            offered: Vec::new(),
        }
    }

    #[test]
    fn translates_messages() {
        let mut t = Translator::new(mapping());
        let event = |events: Vec<ControlEvent>| events.first().map(|e| e.value);
        assert_eq!(
            event(t.translate(MidiMessage::NoteOn { channel: 0, note: 11, velocity: 127 })),
            Some(Value::Pressed(true))
        );
        assert_eq!(
            event(t.translate(MidiMessage::NoteOff { channel: 0, note: 11 })),
            Some(Value::Pressed(false))
        );
        assert!(
            t.translate(MidiMessage::NoteOn { channel: 1, note: 11, velocity: 127 }).is_empty()
        );

        t.translate(MidiMessage::ControlChange { channel: 0, control: 19, value: 127 });
        let full = t.translate(MidiMessage::ControlChange { channel: 0, control: 51, value: 127 });
        assert_eq!(event(full), Some(Value::Absolute(1.0)));

        assert_eq!(
            event(t.translate(MidiMessage::ControlChange { channel: 1, control: 33, value: 62 })),
            Some(Value::Relative(-2))
        );
        assert_eq!(
            event(t.translate(MidiMessage::ControlChange { channel: 6, control: 64, value: 127 })),
            Some(Value::Relative(-1))
        );
    }

    #[test]
    fn toml_round_trip() {
        let text = toml::to_string_pretty(&mapping()).unwrap();
        assert!(text.contains("action = \"deck1.play_pause\""));
        assert_eq!(toml::from_str::<Mapping>(&text).unwrap(), mapping());
    }

    #[test]
    fn shift_combinations() {
        use crate::action::DeckAction as D;
        let note = |action, number, shift| Binding {
            action: Action::Deck(0, action),
            message: MessageType::Note,
            channel: 1,
            number,
            encoding: Encoding::Button,
            lsb: None,
            invert: false,
            shift,
        };
        let mut mapping = Mapping {
            shift_controls: vec![ShiftControl {
                message: MessageType::Note,
                channel: 1,
                number: 0x3F,
            }],
            ..Mapping::default()
        };
        mapping.bind(note(D::PlayPause, 11, false));
        mapping.bind(note(D::JumpToStart, 11, true));
        mapping.bind(note(D::Cue, 12, false));
        assert_eq!(mapping.bindings.len(), 3, "both layers of one control are kept");
        let mut t = Translator::new(mapping);
        let on = |n| MidiMessage::NoteOn { channel: 0, note: n, velocity: 127 };
        let off = |n| MidiMessage::NoteOff { channel: 0, note: n };
        let actions = |events: Vec<ControlEvent>| {
            events.into_iter().map(|e| (e.action, e.value)).collect::<Vec<_>>()
        };
        let a = |d| Action::Deck(0, d);

        assert_eq!(actions(t.translate(on(11))), [(a(D::PlayPause), Value::Pressed(true))]);
        assert_eq!(actions(t.translate(off(11))), [(a(D::PlayPause), Value::Pressed(false))]);
        // Shift itself sends nothing.
        assert!(t.translate(on(0x3F)).is_empty());
        assert_eq!(actions(t.translate(on(11))), [(a(D::JumpToStart), Value::Pressed(true))]);
        // Without a Shift binding the normal one still works.
        assert_eq!(actions(t.translate(on(12))), [(a(D::Cue), Value::Pressed(true))]);
        // Letting go of Shift first still releases the shifted action.
        t.translate(off(0x3F));
        assert_eq!(actions(t.translate(off(11))), [(a(D::JumpToStart), Value::Pressed(false))]);
    }

    #[test]
    fn add_missing_keeps_existing_bindings() {
        use crate::action::DeckAction as D;
        let note = |action, number| Binding {
            action: Action::Deck(0, action),
            message: MessageType::Note,
            channel: 1,
            number,
            encoding: Encoding::Button,
            lsb: None,
            invert: false,
            shift: false,
        };
        // Mine: PLAY learned on note 20 (the rekordbox file uses 11).
        let grv6 = Mapping { name: "DDJ-GRV6".into(), ..Mapping::default() };
        assert_eq!(grv6.jog_steps_per_turn(), 6480);
        assert_eq!(Mapping::default().jog_steps_per_turn(), 720);
        let set = Mapping { jog_resolution: Some(1000), ..grv6 };
        assert_eq!(set.jog_steps_per_turn(), 1000);
        let mut mine = Mapping { bindings: vec![note(D::PlayPause, 20)], ..Mapping::default() };
        let theirs = Mapping {
            bindings: vec![
                note(D::PlayPause, 11), // already bound here: skipped
                note(D::Cue, 20),       // control used here: skipped
                note(D::Sync, 30),      // new: added
            ],
            ..Mapping::default()
        };
        assert_eq!(mine.add_missing(&theirs), (1, 0));
        assert_eq!(mine.bindings.len(), 2);
        assert_eq!(mine.bindings[0], note(D::PlayPause, 20));
        assert_eq!(mine.bindings[1].action, Action::Deck(0, D::Sync));
    }

    #[test]
    fn new_functions_are_added_once() {
        use crate::action::DeckAction as D;
        let note = |action, number| Binding {
            action: Action::Deck(0, action),
            message: MessageType::Note,
            channel: 1,
            number,
            encoding: Encoding::Button,
            lsb: None,
            invert: false,
            shift: false,
        };
        let names = |m: &Mapping| m.bindings.iter().map(|b| b.action.to_string()).collect();
        // Saved when the import had only PLAY and CUE; CUE was cleared since.
        let mut old = Mapping {
            bindings: vec![note(D::PlayPause, 11), note(D::Cue, 12)],
            ..Mapping::default()
        };
        old.offered = names(&old);
        let mut mine = Mapping {
            bindings: vec![note(D::PlayPause, 11)],
            offered: old.offered,
            ..Mapping::default()
        };
        // Now the import knows the stems pads too.
        let mut import = Mapping {
            bindings: vec![note(D::PlayPause, 11), note(D::Cue, 12), note(D::StemPart(0), 16)],
            ..Mapping::default()
        };
        import.offered = names(&import);
        assert!(mine.add_new_functions(&import));
        let actions: Vec<String> = names(&mine);
        assert_eq!(actions, ["deck1.play_pause", "deck1.stem_vocal"]);
        assert!(!mine.add_new_functions(&import));
    }
}
