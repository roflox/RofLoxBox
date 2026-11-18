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

use crate::action::{Action, Feedback, PadMode};
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

/// Whether two bindings are on the same control.
fn same_control(a: &Binding, b: &Binding) -> bool {
    a.message == b.message && a.channel == b.channel && a.number == b.number
}

/// Where a light is: (message, channel, number).
fn address(output: &Output) -> (MessageType, u8, u8) {
    (output.message, output.channel, output.number)
}

/// A message the controller understands as display state: a note lights an
/// LED with velocity 127 (0 = off); a CC carries a value (meters).
///
/// Several outputs may share one light (a button doing one thing, another
/// with Shift held, and others in other pad modes): the light shows the
/// one that applies now (see [`Mapping::lights`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    #[serde(with = "feedback_name")]
    pub feedback: Feedback,
    pub message: MessageType,
    pub channel: u8,
    pub number: u8,
    /// Shown while a Shift button is held.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shift: bool,
    /// Shown only while its deck's pads are in this mode (for a function
    /// put on a pad of that mode); otherwise the mode its state belongs to,
    /// if any (see [`Feedback::pad_mode`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PadMode>,
}

impl Output {
    /// The deck and pad mode this light is shown in, if only in one.
    fn shown_in(&self) -> Option<(usize, PadMode)> {
        match (self.mode, self.feedback) {
            (Some(mode), Feedback::Deck(deck, _)) => Some((deck, mode)),
            _ => self.feedback.pad_mode(),
        }
    }
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
        let mut mapping: Mapping =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        mapping.upgrade();
        Ok(mapping)
    }

    /// Brings a mapping saved by an older version up to date: LOOP IN / OUT
    /// lights, once "loop active", show their own state now (they flash
    /// while adjusting).
    fn upgrade(&mut self) {
        use crate::action::{DeckAction as D, DeckFeedback as F};
        for output in &mut self.outputs {
            let Feedback::Deck(deck, F::LoopActive) = output.feedback else { continue };
            let button = self.bindings.iter().find(|b| {
                !b.shift
                    && (b.message, b.channel, b.number) == address(output)
                    && matches!(b.action, Action::Deck(d, D::LoopIn | D::LoopOut) if d == deck)
            });
            output.feedback = match button.map(|b| b.action) {
                Some(Action::Deck(_, D::LoopIn)) => Feedback::Deck(deck, F::LoopIn),
                Some(Action::Deck(_, D::LoopOut)) => Feedback::Deck(deck, F::LoopOut),
                _ => continue,
            };
        }
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
    /// Lights follow buttons: the bound button's light shows its new
    /// function in that layer (and in the pad mode the light was showing),
    /// and the lights of buttons the function leaves go dark.
    pub fn bind(&mut self, binding: Binding) {
        if binding.encoding == Encoding::Button {
            let layer = binding.shift;
            let here = self.light_of(&binding);
            let before = self.shown_by(&binding, layer);
            // The light the button had in this layer (else without Shift):
            // its pad mode carries over.
            let unshifted = self.shown_by(&binding, false);
            let replaced = self
                .outputs
                .iter()
                .find(|o| address(o) == here && o.shift == layer && Some(o.feedback) == before)
                .or_else(|| {
                    self.outputs.iter().find(|o| {
                        address(o) == here && !o.shift && Some(o.feedback) == unshifted
                    })
                });
            let mode = replaced.and_then(|o| o.mode.or(o.feedback.pad_mode().map(|(_, m)| m)));
            // The lights of the buttons the function had.
            let left: Vec<(MessageType, u8, u8)> = self
                .bindings
                .iter()
                .filter(|b| {
                    b.action == binding.action && b.shift == layer && !same_control(b, &binding)
                })
                .map(|b| self.light_of(b))
                .collect();
            let shows = binding.action.feedback();
            self.outputs.retain(|o| {
                let shown = Some(o.feedback);
                let gone = o.shift == layer
                    && ((address(o) == here && (shown == before || shown == shows))
                        || (left.contains(&address(o)) && shown == shows)
                        // Shift lights are only ever a bound function's:
                        // the function's old one goes (wherever it was).
                        || (layer && shown == shows));
                !gone
            });
            if let Some(feedback) = shows {
                let (message, channel, number) = here;
                let mode = mode.filter(|_| feedback.pad_mode().is_none());
                let shift = layer;
                self.outputs.push(Output { feedback, message, channel, number, shift, mode });
            }
        }
        self.bindings.retain(|b| {
            b.action != binding.action
                && !(b.message == binding.message
                    && b.channel == binding.channel
                    && b.number == binding.number
                    && b.shift == binding.shift)
        });
        self.bindings.push(binding);
    }

    /// Removes `action`'s bindings; the lights of its buttons go dark.
    pub fn unbind(&mut self, action: Action) {
        let left: Vec<((MessageType, u8, u8), bool)> = self
            .bindings
            .iter()
            .filter(|b| b.action == action && b.encoding == Encoding::Button)
            .map(|b| (self.light_of(b), b.shift))
            .collect();
        if let Some(feedback) = action.feedback() {
            self.outputs
                .retain(|o| !(o.feedback == feedback && left.contains(&(address(o), o.shift))));
        }
        self.bindings.retain(|b| b.action != action);
    }

    /// What the control of `button` shows in a layer now: its function's
    /// state there.
    fn shown_by(&self, button: &Binding, shift: bool) -> Option<Feedback> {
        self.bindings
            .iter()
            .filter(|b| b.shift == shift && same_control(b, button))
            .find_map(|b| b.action.feedback())
    }

    /// The light of a button: where the light of what it does now is (a
    /// controller may light a button by another message than it sends; a
    /// Shift combination lights the button's own light), else the button's
    /// own message, as most controllers have it.
    fn light_of(&self, button: &Binding) -> (MessageType, u8, u8) {
        let own = (button.message, button.channel, button.number);
        let found = |shift: bool| {
            let feedback = self.shown_by(button, shift)?;
            let lights: Vec<_> = self
                .outputs
                .iter()
                .filter(|o| o.feedback == feedback && o.shift == shift)
                .map(address)
                .collect();
            // Buttons whose function shows the same (TAP BPM and LOAD
            // both show "loaded"): a light of theirs isn't this one's.
            let buttons = self
                .bindings
                .iter()
                .filter(|b| b.shift == shift && b.action.feedback() == Some(feedback))
                .count();
            // Several lights show it (one per button doing it): this
            // button's own, if one is; else the only one there is, when
            // this is the only button showing it.
            match lights.as_slice() {
                _ if lights.contains(&own) => Some(own),
                [only] if buttons <= 1 => Some(*only),
                _ => None,
            }
        };
        found(button.shift).or_else(|| found(false)).unwrap_or(own)
    }

    /// What each light shows now, by (message, channel, number): with
    /// Shift held, the Shift layer's function where a light has one; the
    /// function of the deck's current pad mode where a light shows several
    /// modes' states; nothing where none applies (the light goes dark).
    pub fn lights(
        &self,
        shift: bool,
        pad_mode: impl Fn(usize) -> PadMode,
    ) -> Vec<((MessageType, u8, u8), Option<Feedback>)> {
        let mut addresses: Vec<(MessageType, u8, u8)> = Vec::new();
        for output in &self.outputs {
            if !addresses.contains(&address(output)) {
                addresses.push(address(output));
            }
        }
        addresses
            .into_iter()
            .map(|at| {
                let here: Vec<&Output> =
                    self.outputs.iter().filter(|o| address(o) == at).collect();
                let in_mode = |o: &Output| {
                    o.shown_in().is_some_and(|(deck, mode)| pad_mode(deck) == mode)
                };
                // A layer's function for now: the one of the current pad
                // mode, else one of no mode; a light with one function
                // shows it in any mode, unless it was put on a pad of
                // another mode.
                let pick = |shifted: bool| {
                    let candidates: Vec<&Output> =
                        here.iter().copied().filter(|o| o.shift == shifted).collect();
                    match candidates.as_slice() {
                        [only] if only.mode.is_none() || in_mode(only) => Some(only.feedback),
                        _ => candidates
                            .iter()
                            .find(|o| in_mode(o))
                            .or_else(|| candidates.iter().find(|o| o.shown_in().is_none()))
                            .map(|o| o.feedback),
                    }
                };
                // With Shift held, the Shift function, else the plain one.
                // Without, the plain one; a light only Shift functions have
                // still shows them (controllers that send another message
                // with Shift held light that one themselves).
                let shows = if shift || here.iter().all(|o| o.shift) {
                    pick(true).or_else(|| pick(false))
                } else {
                    pick(false)
                };
                (at, shows)
            })
            .collect()
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
                    Value::Absolute(centred(value, 16383))
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
                        Encoding::Absolute7 => Value::Absolute(centred(value.into(), 127)),
                        Encoding::Absolute14 => {
                            let halves = self.halves.entry((c, binding.number)).or_insert((0, 0));
                            if is_msb {
                                halves.0 = value;
                            } else {
                                halves.1 = value;
                            }
                            Value::Absolute(centred(u16::from(halves.0) << 7 | u16::from(halves.1), 16383))
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

/// A control's value `0..=max` as 0–1, its middle (64 of 127, 8192 of
/// 16383) exactly 0.5: a tempo fader or EQ knob at its centre detent is
/// then exactly ±0 (`value / max` would put it a step above).
fn centred(value: u16, max: u16) -> f32 {
    let middle = max / 2 + 1;
    let value = value.min(max);
    if value <= middle {
        f32::from(value) / f32::from(2 * middle)
    } else {
        0.5 + f32::from(value - middle) / f32::from(2 * (max - middle))
    }
}

#[cfg(test)]
mod tests {
    fn button(action: Action, channel: u8, number: u8) -> Binding {
        Binding {
            action,
            message: MessageType::Note,
            channel,
            number,
            encoding: Encoding::Button,
            lsb: None,
            invert: false,
            shift: false,
        }
    }

    fn light(feedback: Feedback, channel: u8, number: u8) -> Output {
        Output { feedback, message: MessageType::Note, channel, number, shift: false, mode: None }
    }

    /// A rebound button lights for its new function, at its own light even
    /// when that is another message than the button sends; the button the
    /// function left goes dark.
    #[test]
    fn lights_follow_rebound_buttons() {
        use crate::action::{DeckAction as D, DeckFeedback as F};
        let deck = |a| Action::Deck(0, a);
        let shows = |f| Feedback::Deck(0, f);
        let mut mapping = Mapping {
            bindings: vec![
                button(deck(D::PlayPause), 1, 0x0b),
                button(deck(D::Cue), 1, 0x0c),
                // A pad lit by another message (channel 8).
                button(deck(D::HotCue(0)), 8, 0x00),
            ],
            outputs: vec![
                light(shows(F::Playing), 1, 0x0b),
                light(shows(F::AtCue), 1, 0x0c),
                light(shows(F::HotCueSet(0)), 9, 0x40),
            ],
            ..Mapping::default()
        };
        // PLAY onto the CUE button.
        mapping.bind(button(deck(D::PlayPause), 1, 0x0c));
        assert_eq!(mapping.outputs, [light(shows(F::HotCueSet(0)), 9, 0x40), light(shows(F::Playing), 1, 0x0c)]);
        // SYNC onto the pad: its light is the pad's (channel 9).
        mapping.bind(button(deck(D::Sync), 8, 0x00));
        let sync = Output { mode: Some(PadMode::HotCue), ..light(shows(F::Sync), 9, 0x40) };
        assert!(mapping.outputs.contains(&sync));
        assert!(!mapping.outputs.iter().any(|o| o.feedback == shows(F::HotCueSet(0))));
        // Gate mode onto hot cue B's pad: the pad shows gate mode.
        mapping.bindings.push(button(deck(D::HotCue(1)), 8, 0x01));
        mapping.outputs.push(light(shows(F::HotCueSet(1)), 9, 0x41));
        mapping.bind(button(deck(D::HotCueGateMode), 8, 0x01));
        let gate = light(shows(F::HotCueGateMode), 9, 0x41);
        assert!(mapping.outputs.contains(&Output { mode: Some(PadMode::HotCue), ..gate }));
        assert!(!mapping.outputs.iter().any(|o| o.feedback == shows(F::HotCueSet(1))));
        // A button that did nothing: lit by its own message.
        mapping.bind(button(deck(D::Slip), 2, 0x20));
        assert!(mapping.outputs.contains(&light(shows(F::Slip), 2, 0x20)));
        // Unbound: its button goes dark.
        mapping.unbind(deck(D::Slip));
        assert!(!mapping.outputs.iter().any(|o| o.feedback == shows(F::Slip)));
        // A fader leaves the lights alone.
        let before = mapping.outputs.clone();
        mapping.bind(Binding {
            encoding: Encoding::Absolute7,
            ..button(deck(D::Volume), 1, 0x0c)
        });
        assert_eq!(mapping.outputs, before);
    }

    /// Shift + QUANTIZE as gate mode: QUANTIZE's light shows gate mode while
    /// Shift is held, quantize otherwise.
    #[test]
    fn shift_functions_light_while_shift_is_held() {
        use crate::action::{DeckAction as D, DeckFeedback as F};
        let deck = |a| Action::Deck(0, a);
        let shows = |f| Feedback::Deck(0, f);
        let mut mapping = Mapping {
            bindings: vec![button(deck(D::Quantize), 1, 0x30)],
            outputs: vec![light(shows(F::Quantize), 1, 0x30)],
            ..Mapping::default()
        };
        mapping.bind(Binding { shift: true, ..button(deck(D::HotCueGateMode), 1, 0x30) });
        let at = (MessageType::Note, 1, 0x30);
        let hot_cue = |_| PadMode::HotCue;
        assert_eq!(mapping.lights(false, hot_cue), [(at, Some(shows(F::Quantize)))]);
        assert_eq!(mapping.lights(true, hot_cue), [(at, Some(shows(F::HotCueGateMode)))]);
        // Unbound: Shift shows quantize again.
        mapping.unbind(deck(D::HotCueGateMode));
        assert_eq!(mapping.lights(true, hot_cue), [(at, Some(shows(F::Quantize)))]);
    }

    /// A Shift combination that sends a note of its own (Shift + QUANTIZE
    /// sends TAP BPM's on the GRV6) lights by that note, not by a light
    /// elsewhere that shows the same state as TAP BPM.
    #[test]
    fn shift_note_lights_itself() {
        use crate::action::{DeckAction as D, DeckFeedback as F};
        let deck = |a| Action::Deck(0, a);
        let shows = |f| Feedback::Deck(0, f);
        let mut mapping = Mapping {
            bindings: vec![
                button(deck(D::Load), 7, 0x46),
                button(deck(D::TapBpm), 1, 0x68),
            ],
            outputs: vec![light(shows(F::Loaded), 16, 0x00)],
            ..Mapping::default()
        };
        // A stale light from before, on the load indicator.
        mapping.outputs.push(Output { shift: true, ..light(shows(F::HotCueGateMode), 16, 0x00) });
        mapping.bind(Binding { shift: true, ..button(deck(D::HotCueGateMode), 1, 0x68) });
        let gate: Vec<_> =
            mapping.outputs.iter().filter(|o| o.feedback == shows(F::HotCueGateMode)).collect();
        assert_eq!(gate, [&Output { shift: true, ..light(shows(F::HotCueGateMode), 1, 0x68) }]);
    }

    /// One pad light for every pad mode (each mode's pad sending its own
    /// note): it shows the current mode's function, with Shift its Shift one.
    #[test]
    fn pad_lights_follow_the_pad_mode() {
        use crate::action::{DeckAction as D, DeckFeedback as F};
        let deck = |a| Action::Deck(0, a);
        let shows = |f| Feedback::Deck(0, f);
        let mut mapping = Mapping {
            bindings: vec![
                button(deck(D::HotCue(0)), 8, 0x00),
                button(deck(D::BeatLoop(0)), 8, 0x60),
                button(deck(D::StemPart(0)), 8, 0x10),
            ],
            outputs: vec![
                light(shows(F::HotCueSet(0)), 8, 0x00),
                light(shows(F::PadLit(PadMode::BeatLoop, 0)), 8, 0x00),
                light(shows(F::StemOn(0)), 8, 0x00),
            ],
            ..Mapping::default()
        };
        let at = (MessageType::Note, 8, 0x00);
        let shown = |m: &Mapping, shift, mode| m.lights(shift, |_| mode)[0];
        assert_eq!(shown(&mapping, false, PadMode::HotCue), (at, Some(shows(F::HotCueSet(0)))));
        assert_eq!(shown(&mapping, false, PadMode::Stems), (at, Some(shows(F::StemOn(0)))));
        assert_eq!(shown(&mapping, false, PadMode::Sampler), (at, None));
        // The beat loop pad made gate mode: lit as gate mode in Beat Loop only.
        mapping.bind(button(deck(D::HotCueGateMode), 8, 0x60));
        let gate = Some(shows(F::HotCueGateMode));
        assert_eq!(shown(&mapping, false, PadMode::BeatLoop), (at, gate));
        assert_eq!(shown(&mapping, false, PadMode::HotCue), (at, Some(shows(F::HotCueSet(0)))));
        // Shift + the stems pad as sync: in Stems mode with Shift held.
        mapping.bind(Binding { shift: true, ..button(deck(D::Sync), 8, 0x10) });
        assert_eq!(shown(&mapping, true, PadMode::Stems), (at, Some(shows(F::Sync))));
        assert_eq!(shown(&mapping, false, PadMode::Stems), (at, Some(shows(F::StemOn(0)))));
        // Shift held where the pad has no Shift function: its plain one.
        assert_eq!(shown(&mapping, true, PadMode::BeatLoop), (at, gate));
        assert_eq!(shown(&mapping, true, PadMode::Sampler), (at, None));
    }

    /// Ends at 0 and 1, the middle exactly 0.5, steady in between.
    #[test]
    fn controls_are_centred() {
        for max in [127u16, 16383] {
            assert_eq!(centred(0, max), 0.0);
            assert_eq!(centred(max, max), 1.0);
            assert_eq!(centred(max / 2 + 1, max), 0.5);
            assert!((0..max).all(|v| centred(v, max) < centred(v + 1, max)));
        }
    }

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
