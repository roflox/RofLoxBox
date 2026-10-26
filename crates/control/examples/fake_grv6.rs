//! Test aid: a virtual MIDI device named like a DDJ-GRV6. It sends a scripted
//! set of silent control changes (no play or cue) and prints the LED and
//! meter messages it receives back.
//!
//!     cargo run -p deckhand-control --example fake_grv6 -- 8
//!
//! The argument is how many seconds to stay connected; a second argument
//! `cues` runs the cue script (memory cue calls, gated hot cue) instead of
//! the mixer script. Deck 1's channel fader is pulled down first, so any
//! playback stays silent.

use std::time::Duration;

use midir::os::unix::{VirtualInput, VirtualOutput};

fn main() -> anyhow::Result<()> {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8);
    let output = midir::MidiOutput::new("fake-grv6")?;
    let mut device =
        output.create_virtual("DDJ-GRV6 (virtual)").map_err(|e| anyhow::anyhow!("{e}"))?;
    let input = midir::MidiInput::new("fake-grv6")?;
    let _leds = input
        .create_virtual("DDJ-GRV6 (virtual)", |_, bytes, _| println!("LED {bytes:02X?}"), ())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("virtual DDJ-GRV6 ready");

    // Give the app time to start, find the port and import the mapping.
    std::thread::sleep(Duration::from_secs(seconds / 2));
    let cues = std::env::args().nth(2).as_deref() == Some("cues");
    let cue_script: &[(&str, [u8; 3])] = &[
        ("deck 1 channel fader MSB down", [0xb0, 0x13, 0x00]),
        ("deck 1 channel fader LSB", [0xb0, 0x33, 0x00]),
        ("deck 1 memory call next", [0x90, 0x53, 0x7f]),
        ("deck 1 memory call next release", [0x90, 0x53, 0x00]),
        ("deck 1 memory call next (the loop)", [0x90, 0x53, 0x7f]),
        ("deck 1 memory call next release", [0x90, 0x53, 0x00]),
        ("deck 1 memory call previous", [0x90, 0x51, 0x7f]),
        ("deck 1 memory call previous release", [0x90, 0x51, 0x00]),
        ("deck 1 gate mode (test binding)", [0x90, 0x70, 0x7f]),
        ("deck 1 gate mode release", [0x90, 0x70, 0x00]),
        ("deck 1 hot cue A press", [0x97, 0x00, 0x7f]),
    ];
    let mixer_script: &[(&str, [u8; 3])] = &[
        ("deck 1 quantize off", [0x90, 0x35, 0x7f]),
        ("deck 1 quantize release", [0x90, 0x35, 0x00]),
        ("deck 2 vinyl off", [0x91, 0x17, 0x7f]),
        ("deck 2 vinyl release", [0x91, 0x17, 0x00]),
        ("deck 2 channel fader MSB to 25%", [0xb1, 0x13, 0x20]),
        ("deck 2 channel fader LSB", [0xb1, 0x33, 0x00]),
        ("CH1 filter MSB to low-pass", [0xb6, 0x17, 0x10]),
        ("CH1 filter LSB", [0xb6, 0x37, 0x00]),
        ("crossfader MSB fully right", [0xb6, 0x1f, 0x7f]),
        ("crossfader LSB", [0xb6, 0x3f, 0x7f]),
        ("deck 1 headphone cue on", [0x90, 0x54, 0x7f]),
        ("deck 1 headphone cue release", [0x90, 0x54, 0x00]),
        ("deck 2 EQ low MSB fully down (kill)", [0xb1, 0x0f, 0x00]),
        ("deck 2 EQ low LSB", [0xb1, 0x2f, 0x00]),
    ];
    let script = if cues { cue_script } else { mixer_script };
    for (what, message) in script {
        println!("SEND {what}: {message:02X?}");
        device.send(message)?;
        std::thread::sleep(Duration::from_millis(60));
    }
    if cues {
        // Hold the gated hot cue for a moment, then release it.
        std::thread::sleep(Duration::from_millis(400));
        println!("SEND deck 1 hot cue A release");
        device.send(&[0x97, 0x00, 0x00])?;
    }
    for _ in 0..10 {
        // Shift + browse turned left: zoom the waveforms out.
        device.send(&[0xb6, 0x64, 0x7f])?;
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_secs(seconds - seconds / 2));
    Ok(())
}
