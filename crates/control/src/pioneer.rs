//! Pioneer DJ / AlphaTheta controller SysEx.
//!
//! These controllers report the current position of every fader and knob
//! when they receive a status request, `F0 00 40 05 00 00 <model> 00 03 01
//! F7`, with the two-byte model ID below. The positions arrive as ordinary
//! MIDI messages, so the mapping handles them like moved controls. The model
//! IDs are the ones rekordbox sends to each unit (DDJ-400's matches the
//! public Mixxx mapping).

/// (model name as in the MIDI port name, model ID).
const MODELS: &[(&str, [u8; 2])] = &[
    ("DDJ-200", [0x02, 0x0A]),
    ("DDJ-400", [0x02, 0x06]),
    ("DDJ-800", [0x02, 0x09]),
    ("DDJ-1000", [0x02, 0x00]),
    ("DDJ-FLX2", [0x04, 0x0D]),
    ("DDJ-FLX4", [0x04, 0x05]),
    ("DDJ-FLX10", [0x04, 0x01]),
    ("DDJ-GRV6", [0x04, 0x0C]),
    ("DDJ-REV5", [0x04, 0x06]),
    ("DDJ-RB", [0x00, 0x0E]),
    ("DDJ-RR", [0x00, 0x0D]),
    ("DDJ-RX", [0x00, 0x02]),
    ("DDJ-RZ", [0x00, 0x04]),
    ("DDJ-RZX", [0x00, 0x14]),
    ("DDJ-WeGO4", [0x01, 0x00]),
    ("XDJ-AN", [0x06, 0x08]),
    ("XDJ-AZ", [0x04, 0x0B]),
    ("XDJ-RX", [0x03, 0x24]),
    ("XDJ-RX3", [0x03, 0x0D]),
    ("XDJ-XZ", [0x02, 0x0C]),
    ("OPUS-QUAD", [0x04, 0x02]),
    ("OMNIS-DUO", [0x04, 0x08]),
];

/// Letters and digits only, upper case.
fn squash(text: &str) -> String {
    text.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect()
}

/// The message asking the controller on `port` for all control positions,
/// if it is a known model.
pub fn status_request(port: &str) -> Option<Vec<u8>> {
    let port = squash(port);
    // The longest matching name wins, so "DDJ-RZX" is not taken for "DDJ-RZ".
    let (_, [hi, lo]) = MODELS
        .iter()
        .filter(|(name, _)| port.contains(&squash(name)))
        .max_by_key(|(name, _)| name.len())?;
    Some(vec![0xF0, 0x00, 0x40, 0x05, 0x00, 0x00, *hi, *lo, 0x00, 0x03, 0x01, 0xF7])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_model_in_port_names() {
        let grv6 = status_request("DDJ-GRV6").unwrap();
        assert_eq!(grv6, [0xF0, 0x00, 0x40, 0x05, 0x00, 0x00, 0x04, 0x0C, 0x00, 0x03, 0x01, 0xF7]);
        assert_eq!(status_request("PIONEER DDJ-RZX MIDI").unwrap()[6..8], [0x00, 0x14]);
        assert_eq!(status_request("DDJ-RZ").unwrap()[6..8], [0x00, 0x04]);
        assert_eq!(status_request("XDJ-RX3").unwrap()[6..8], [0x03, 0x0D]);
        assert_eq!(status_request("Launchpad Mini"), None);
    }
}
