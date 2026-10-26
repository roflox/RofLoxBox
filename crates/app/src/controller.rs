//! The connected controller: MIDI input and output, its mapping, learn mode
//! and LED feedback.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use deckhand_control::{
    Action, ControlEvent, Feedback, Learner, Mapping, MessageType, MidiInput, MidiMessage,
    MidiOutput, ShiftControl, Translator,
};
use deckhand_control::{pioneer, rekordbox};
use eframe::egui;

use crate::config::mapping_path;

const LOG_LINES: usize = 12;

#[derive(Default)]
pub struct Controller {
    input: Option<MidiInput>,
    output: Option<MidiOutput>,
    translator: Translator,
    learner: Option<Learner>,
    /// The next button pressed becomes a Shift button.
    learning_shift: bool,
    /// Last value sent per (status, number), so only changes are sent.
    sent: HashMap<(u8, u8), u8>,
    pub log: VecDeque<String>,
    pub error: Option<String>,
    pub notice: Option<String>,
}

impl Controller {
    pub fn port(&self) -> Option<&str> {
        self.input.as_ref().map(MidiInput::port)
    }

    pub fn has_output(&self) -> bool {
        self.output.is_some()
    }

    pub fn mapping(&self) -> &Mapping {
        self.translator.mapping()
    }

    /// Connects to a port and loads its saved mapping; without one, imports
    /// the controller's mapping from the local rekordbox installation.
    pub fn open(&mut self, port: &str, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let input = match MidiInput::open(port, move || ctx.request_repaint()) {
            Ok(input) => input,
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                return;
            }
        };
        self.input = Some(input);
        self.output = MidiOutput::open(port).ok();
        self.sent.clear();
        self.error = None;
        match Mapping::load(&mapping_path(port)) {
            Ok(mapping) => self.translator.set_mapping(mapping),
            Err(_) => {
                match rekordbox::find_for_port_with(port, &[crate::config::extra_mapping_dir()]) {
                    Some((_, path)) => self.import_from(&path),
                    None => self
                        .translator
                        .set_mapping(Mapping { name: port.to_owned(), ..Mapping::default() }),
                }
            }
        }
        // Start from where the faders and knobs physically are; the answers
        // arrive as ordinary control messages.
        if let (Some(output), Some(request)) = (self.output.as_mut(), pioneer::status_request(port))
        {
            output.send(&request);
        }
    }

    pub fn close(&mut self) {
        self.input = None;
        self.output = None;
        self.learner = None;
    }

    /// Replaces the mapping with one converted from a rekordbox mapping file.
    pub fn import_from(&mut self, path: &Path) {
        match rekordbox::import_file(path) {
            Ok(mut import) => {
                // A chosen channel order belongs to the controller, not the import.
                import.mapping.channel_order = self.translator.mapping().channel_order;
                self.notice = Some(format!(
                    "Imported {} controls and {} lights from rekordbox's {} mapping. Not available in RofLoxBox yet: {}.",
                    import.mapping.bindings.len(),
                    import.mapping.outputs.len(),
                    import.mapping.name,
                    summarize(&import.unsupported),
                ));
                self.sent.clear();
                self.store(import.mapping);
            }
            Err(err) => self.error = Some(format!("importing {}: {err:#}", path.display())),
        }
    }

    pub fn learning(&self) -> Option<Action> {
        self.learner.as_ref().map(Learner::action)
    }

    pub fn learn(&mut self, action: Action) {
        self.learner = Some(Learner::new(action));
    }

    pub fn cancel_learn(&mut self) {
        self.learner = None;
        self.learning_shift = false;
    }

    pub fn learning_shift(&self) -> bool {
        self.learning_shift
    }

    /// The next button pressed on the controller becomes a Shift button.
    pub fn learn_shift(&mut self) {
        self.learner = None;
        self.learning_shift = true;
    }

    pub fn clear_shift_controls(&mut self) {
        let mut mapping = self.translator.mapping().clone();
        mapping.shift_controls.clear();
        self.store(mapping);
    }

    /// Makes an action's binding apply with Shift held (or without).
    pub fn set_shifted(&mut self, action: Action, shift: bool) {
        let mut mapping = self.translator.mapping().clone();
        for binding in mapping.bindings.iter_mut().filter(|b| b.action == action) {
            binding.shift = shift;
        }
        self.store(mapping);
    }

    pub fn unbind(&mut self, action: Action) {
        let mut mapping = self.translator.mapping().clone();
        mapping.bindings.retain(|b| b.action != action);
        self.store(mapping);
    }

    /// The connected controller's mixer channel order, if one was chosen.
    pub fn channel_order(&self) -> Option<[usize; 4]> {
        self.port().and(self.translator.mapping().channel_order)
    }

    /// Remembers the mixer channel order with the controller's mapping.
    pub fn set_channel_order(&mut self, order: [usize; 4]) {
        let mut mapping = self.translator.mapping().clone();
        mapping.channel_order = Some(order);
        self.store(mapping);
    }

    pub fn set_inverted(&mut self, action: Action, invert: bool) {
        let mut mapping = self.translator.mapping().clone();
        for binding in mapping.bindings.iter_mut().filter(|b| b.action == action) {
            binding.invert = invert;
        }
        self.store(mapping);
    }

    fn store(&mut self, mapping: Mapping) {
        if let Some(port) = self.port()
            && let Err(err) = mapping.save(&mapping_path(port))
        {
            self.error = Some(format!("saving mapping: {err:#}"));
        }
        self.translator.set_mapping(mapping);
    }

    /// Reads pending MIDI messages. While learning, a finished binding is
    /// saved; otherwise messages become control events.
    pub fn poll(&mut self) -> Vec<ControlEvent> {
        let messages: Vec<MidiMessage> =
            self.input.as_ref().map(|i| i.messages().collect()).unwrap_or_default();
        let mut events = Vec::new();
        for message in messages {
            self.log.push_front(format!("{message:?}"));
            self.log.truncate(LOG_LINES);
            if self.learning_shift {
                let control = match message {
                    MidiMessage::NoteOn { channel, note, .. } => Some(ShiftControl {
                        message: MessageType::Note,
                        channel: channel + 1,
                        number: note,
                    }),
                    MidiMessage::ControlChange { channel, control, value } if value > 0 => {
                        Some(ShiftControl {
                            message: MessageType::Cc,
                            channel: channel + 1,
                            number: control,
                        })
                    }
                    _ => None,
                };
                if let Some(control) = control {
                    self.learning_shift = false;
                    let mut mapping = self.translator.mapping().clone();
                    if !mapping.shift_controls.contains(&control) {
                        mapping.shift_controls.push(control);
                    }
                    self.store(mapping);
                }
                continue;
            }
            if let Some(learner) = self.learner.as_mut() {
                // Holding Shift while learning records a Shift combination.
                if self.translator.observe_shift(&message) {
                    continue;
                }
                if let Some(mut binding) = learner.feed(message) {
                    self.learner = None;
                    binding.shift = self.translator.shift_held();
                    let mut mapping = self.translator.mapping().clone();
                    mapping.bind(binding);
                    self.store(mapping);
                }
                continue;
            }
            events.extend(self.translator.translate(message));
        }
        events
    }

    /// Sends LED and meter state; `value` gives 0–127 per feedback item.
    pub fn send_feedback(&mut self, value: impl Fn(Feedback) -> u8) {
        let Some(output) = self.output.as_mut() else { return };
        for out in &self.translator.mapping().outputs {
            let status = match out.message {
                MessageType::Note => 0x90,
                MessageType::Cc => 0xb0,
                MessageType::PitchBend => continue,
            } | (out.channel.saturating_sub(1) & 0x0f);
            let v = value(out.feedback).min(127);
            if self.sent.get(&(status, out.number)) != Some(&v) {
                self.sent.insert((status, out.number), v);
                output.send(&[status, out.number, v]);
            }
        }
    }
}

/// A short, readable list of unsupported rekordbox functions.
fn summarize(names: &[String]) -> String {
    let mut groups: Vec<&str> = Vec::new();
    for name in names {
        // Checked in this order: stem functions also contain "FX".
        let group = if name.contains("Part") || name.contains("Stems") {
            "stems"
        } else if name.starts_with("Drum") {
            "drum swap/roll"
        } else if name.contains("Panel") || name.starts_with("Area") || name.contains("Window") {
            "screen panels"
        } else if name.contains("Sampler") {
            "sampler extras"
        } else if name.starts_with("FX") || name.starts_with("CFX") {
            "beat FX extras"
        } else if name.contains("Keyboard") || name.contains("KeyShift") || name.starts_with("Key")
        {
            "key functions"
        } else if name.starts_with("Browse")
            || name.contains("Preview")
            || name.contains("Related")
            || name.contains("TagList")
            || name == "Discover"
        {
            "browser extras"
        } else if name.starts_with("Deck") || name.starts_with("DualDeck") || name == "Shift" {
            "deck select (handled by the controller)"
        } else {
            "other"
        };
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    if groups.is_empty() { "nothing".into() } else { groups.join(", ") }
}
