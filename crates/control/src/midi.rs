//! MIDI input ports.

use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::{Result, anyhow};

/// A channel voice message (channel 0–15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMessage {
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        note: u8,
    },
    ControlChange {
        channel: u8,
        control: u8,
        value: u8,
    },
    /// 14-bit value, 8192 = center.
    PitchBend {
        channel: u8,
        value: u16,
    },
}

impl MidiMessage {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let (&status, data) = bytes.split_first()?;
        let channel = status & 0x0f;
        let d0 = data.first().copied().unwrap_or(0) & 0x7f;
        let d1 = data.get(1).copied().unwrap_or(0) & 0x7f;
        Some(match status & 0xf0 {
            0x90 if d1 > 0 => MidiMessage::NoteOn { channel, note: d0, velocity: d1 },
            0x90 | 0x80 => MidiMessage::NoteOff { channel, note: d0 },
            0xb0 => MidiMessage::ControlChange { channel, control: d0, value: d1 },
            0xe0 => MidiMessage::PitchBend { channel, value: u16::from(d1) << 7 | u16::from(d0) },
            _ => return None,
        })
    }
}

/// Names of the MIDI input ports present now.
pub fn input_ports() -> Vec<String> {
    let Ok(input) = midir::MidiInput::new("deckhand") else { return Vec::new() };
    input.ports().iter().filter_map(|port| input.port_name(port).ok()).collect()
}

/// An open MIDI input. Messages arrive on [`MidiInput::messages`]; `wake`
/// runs after each message so a UI can redraw immediately.
/// Runs on the MIDI thread for every message before the UI sees it;
/// returning `true` takes the message (the UI never gets it). Used for jog
/// wheels, which must not wait for the next screen refresh.
pub type FastPath = Box<dyn FnMut(&MidiMessage) -> bool + Send>;

pub struct MidiInput {
    port: String,
    fast: std::sync::Arc<std::sync::Mutex<Option<FastPath>>>,
    messages: Receiver<MidiMessage>,
    _connection: midir::MidiInputConnection<Sender<MidiMessage>>,
}

impl MidiInput {
    pub fn open(port_name: &str, wake: impl Fn() + Send + 'static) -> Result<Self> {
        let mut input = midir::MidiInput::new("deckhand")?;
        input.ignore(midir::Ignore::All);
        let port = input
            .ports()
            .into_iter()
            .find(|port| input.port_name(port).is_ok_and(|name| name == port_name))
            .ok_or_else(|| anyhow!("MIDI port {port_name} not found"))?;
        let (tx, messages) = channel();
        let fast: std::sync::Arc<std::sync::Mutex<Option<FastPath>>> = Default::default();
        let hook = fast.clone();
        let connection = input
            .connect(
                &port,
                "deckhand-in",
                move |_, bytes, tx: &mut Sender<MidiMessage>| {
                    if let Some(message) = MidiMessage::parse(bytes) {
                        let taken = hook
                            .lock()
                            .ok()
                            .and_then(|mut f| f.as_mut().map(|f| f(&message)))
                            .unwrap_or(false);
                        if !taken {
                            let _ = tx.send(message);
                            wake();
                        }
                    }
                },
                tx,
            )
            .map_err(|err| anyhow!("connecting to {port_name}: {err}"))?;
        Ok(Self { port: port_name.to_owned(), fast, messages, _connection: connection })
    }

    /// Installs (or with `None` removes) the [`FastPath`].
    pub fn set_fast_path(&self, fast: Option<FastPath>) {
        if let Ok(mut slot) = self.fast.lock() {
            *slot = fast;
        }
    }

    pub fn port(&self) -> &str {
        &self.port
    }

    pub fn messages(&self) -> impl Iterator<Item = MidiMessage> + '_ {
        self.messages.try_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_messages() {
        assert_eq!(
            MidiMessage::parse(&[0x91, 0x0b, 0x7f]),
            Some(MidiMessage::NoteOn { channel: 1, note: 0x0b, velocity: 0x7f })
        );
        assert_eq!(
            MidiMessage::parse(&[0x90, 0x0b, 0x00]),
            Some(MidiMessage::NoteOff { channel: 0, note: 0x0b })
        );
        assert_eq!(
            MidiMessage::parse(&[0xb6, 0x1f, 0x40]),
            Some(MidiMessage::ControlChange { channel: 6, control: 0x1f, value: 0x40 })
        );
        assert_eq!(
            MidiMessage::parse(&[0xe0, 0x00, 0x40]),
            Some(MidiMessage::PitchBend { channel: 0, value: 8192 })
        );
        assert_eq!(MidiMessage::parse(&[0xf8]), None);
    }
}

/// Names of the MIDI output ports present now.
pub fn output_ports() -> Vec<String> {
    let Ok(output) = midir::MidiOutput::new("deckhand") else { return Vec::new() };
    output.ports().iter().filter_map(|port| output.port_name(port).ok()).collect()
}

/// An open MIDI output, for LEDs and meters.
pub struct MidiOutput {
    connection: midir::MidiOutputConnection,
}

impl MidiOutput {
    /// Opens the output port with `name`, or failing that, the first one
    /// whose name contains it (inputs and outputs are often named alike).
    pub fn open(name: &str) -> Result<Self> {
        let output = midir::MidiOutput::new("deckhand")?;
        let ports = output.ports();
        let port = ports
            .iter()
            .find(|p| output.port_name(p).is_ok_and(|n| n == name))
            .or_else(|| {
                ports.iter().find(|p| {
                    output.port_name(p).is_ok_and(|n| n.contains(name) || name.contains(&n))
                })
            })
            .ok_or_else(|| anyhow!("no MIDI output for {name}"))?
            .clone();
        let connection = output
            .connect(&port, "deckhand-out")
            .map_err(|err| anyhow!("connecting to {name}: {err}"))?;
        Ok(Self { connection })
    }

    pub fn send(&mut self, bytes: &[u8]) {
        let _ = self.connection.send(bytes);
    }
}
