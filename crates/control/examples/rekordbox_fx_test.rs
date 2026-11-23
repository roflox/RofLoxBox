//! Test aid: plays rekordbox's Beat FX, one after another, over the 14-pass
//! test song on deck 1, through a virtual MIDI device of its own
//! (`rofloxbox-fx-test`), so rekordbox's output (recorded through BlackHole)
//! can be measured against RofLoxBox's effects.
//!
//!     cargo run -p rofloxbox-control --example rekordbox_fx_test -- [depth%] [beat]
//!
//! `depth%` is the LEVEL/DEPTH knob (default 100); `beat` the steps up from
//! the shortest beat (default: rekordbox's 1 beat, found by its BEAT readout).
//! Before running: the test song loaded on deck 1 (stopped at its start),
//! deck 1's channel up and flat, and rekordbox's output going to BlackHole.
//!
//! The device's mapping is written to rekordbox's own MidiMappings folder
//! (a file of its own; nothing else there is touched): rekordbox reads it
//! when the device appears, so start rekordbox after (or restart it once).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use midir::os::unix::{VirtualInput, VirtualOutput};

const NAME: &str = "rofloxbox-fx-test";

/// rekordbox's mapping for the device: the same messages as a DDJ-GRV6's
/// for the few things the test needs (and the BEAT readout and FX ON lamp
/// sent back).
const MAPPING: &str = "@file,1,rofloxbox-fx-test
#name,function,type,input,deck1,deck2,deck3,deck4,output,deck1,deck2,deck3,deck4,option,comment
PlayPause,PlayPause,Button,900B,0,1,2,3,900B,0,1,2,3,Fast;Priority=50,Play/Pause
Rec,,Button,9667,,,,,9667,,,,,,Recording
ChannelFader,ChannelFader,KnobSliderHiRes,B013,0,1,2,3,,,,,,,CH Fader
Gain,Gain,KnobSliderHiRes,B004,0,1,2,3,,,,,,Fast,Gain
EQHigh,EQHigh,KnobSliderHiRes,B007,0,1,2,3,,,,,,Fast,EQ High
EQMid,EQMid,KnobSliderHiRes,B00B,0,1,2,3,,,,,,Fast,EQ Mid
EQLow,EQLow,KnobSliderHiRes,B00F,0,1,2,3,,,,,,Fast,EQ Low
CFXParameterCH1,CFXParameterCH1,KnobSliderHiRes,B617,,,,,,,,,,Fast,CH1 CFX Depth
CrossFader,CrossFader,KnobSliderHiRes,B61F,,,,,,,,,,Fast,Crossfader
TempoSlider,TempoSlider,KnobSliderHiRes,B000,0,1,2,3,,,,,,Fast,Tempo Control
MasterTempo,MasterTempo,Button,901A,0,1,2,3,901A,0,1,2,3,,MasterTempo On/Off
Sync,Sync,Button,9058,0,1,2,3,9058,0,1,2,3,Blink=600,Sync On/Off
FX1Assign.CH1,,Button,9410,,,,,,,,,,,FX1 to channel 1
FX1-1,FX1-1,KnobSliderHiRes,B402,,,,,,,,,,,FX1-1 Effect Depth
FX1-1On,FX1-1On,Button,9447,,,,,9447,,,,,,FX1-1 Effect On/Off
FX1BeatDown,FX1BeatDown,Button,944A,,,,,,,,,,,FX1 Effect beat down
FX1BeatUp,FX1BeatUp,Button,944B,,,,,,,,,,,FX1 Effect beat up
FX1BeatMultiplier,FX1BeatMultiplier,Indicator,,,,,,B464,,,,,,FX1 BeatMultiplier No
FX1-1Select.DELAY,DELAY,Button,9420,,,,,,,,,,,FX select
FX1-1Select.ECHO,ECHO,Button,9421,,,,,,,,,,,FX select
FX1-1Select.LOWCUTECHO,LOWCUTECHO,Button,9422,,,,,,,,,,,FX select
FX1-1Select.SPIRAL,SPIRAL,Button,9423,,,,,,,,,,,FX select
FX1-1Select.HELIX,HELIX,Button,9424,,,,,,,,,,,FX select
FX1-1Select.REVERB,REVERB,Button,9425,,,,,,,,,,,FX select
FX1-1Select.FLANGER,FLANGER,Button,9426,,,,,,,,,,,FX select
FX1-1Select.PHASER,PHASER,Button,9427,,,,,,,,,,,FX select
FX1-1Select.FILTER,FILTER,Button,9428,,,,,,,,,,,FX select
FX1-1Select.TRANS,TRANS,Button,9429,,,,,,,,,,,FX select
FX1-1Select.PITCH,PITCH,Button,942A,,,,,,,,,,,FX select
FX1-1Select.ROLL,ROLL,Button,942B,,,,,,,,,,,FX select
FX1-1Select.MOBIUSSAW,MOBIUSSAW,Button,942C,,,,,,,,,,,FX select
FX1-1Select.MOBIUSTRI,MOBIUSTRI,Button,942D,,,,,,,,,,,FX select
";

/// The effects in the order of the passes, as rekordbox names them.
const EFFECTS: [&str; 14] = [
    "DELAY", "ECHO", "LOW CUT ECHO", "SPIRAL", "HELIX", "REVERB", "FLANGER", "PHASER", "FILTER",
    "TRANS", "PITCH", "ROLL", "MOBIUS SAW", "MOBIUS TRI",
];

/// The test song: 4 s of silence, then 56 s of test sounds, 14 times.
const PASS_SECS: u64 = 60;

fn main() -> anyhow::Result<()> {
    let depth: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(100.0);
    let beat_steps: Option<u8> = std::env::args().nth(2).and_then(|s| s.parse().ok());
    if std::process::Command::new("pgrep").args(["-f", "rofloxbox-app"]).output()?.status.success() {
        eprintln!(
            "RofLoxBox is running: it may take this test device as its controller. Quit it (or \
             keep your GRV6 connected to it) and press Enter to go on."
        );
        std::io::stdin().read_line(&mut String::new())?;
    }
    let home = std::env::var("HOME")?;
    let folder = format!("{home}/Library/Application Support/Pioneer/rekordbox6/MidiMappings");
    let file = format!("{folder}/{NAME}.midi.csv");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(MAPPING) {
        std::fs::write(&file, MAPPING)?;
        println!("wrote {file} (restart rekordbox if it was running, so it reads it)");
    }

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let input = midir::MidiInput::new(NAME)?;
    let _back = input
        .create_virtual(NAME, move |_, bytes, _| drop(tx.send(bytes.to_vec())), ())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let output = midir::MidiOutput::new(NAME)?;
    let mut device = output.create_virtual(NAME).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("virtual device {NAME} ready; waiting for rekordbox (up to 2 min)…");

    let mut shown = Shown::default();
    // rekordbox lights the device up once it has taken it.
    let started = Instant::now();
    while shown.beat.is_none() && shown.fx_on.is_none() && started.elapsed() < Duration::from_secs(120) {
        device.send(&[0x94, 0x10, 0x7F])?; // FX to channel 1 (harmless, asks for a reply)
        device.send(&[0x94, 0x10, 0x00])?;
        shown.take(&rx, Duration::from_secs(2));
    }
    if shown.beat.is_none() && shown.fx_on.is_none() {
        anyhow::bail!("rekordbox didn't answer: is it running, and was it restarted after the mapping was written?");
    }
    println!("rekordbox answers (beat readout {:?}, FX on {:?})", shown.beat, shown.fx_on);

    // Channel 1, the first effect, the depth.
    press(&mut device, 0x10, 4)?;
    press(&mut device, 0x20, 4)?;
    let value = ((depth.clamp(0.0, 100.0) / 100.0) * 16383.0).round() as u16;
    device.send(&[0xB4, 0x02, (value >> 7) as u8])?;
    device.send(&[0xB4, 0x22, (value & 0x7F) as u8])?;
    // The beat: down to the shortest, then up as asked (or to 1 beat).
    for _ in 0..12 {
        press(&mut device, 0x4A, 4)?;
    }
    shown.take(&rx, Duration::from_millis(300));
    let shortest = shown.beat;
    match beat_steps {
        Some(steps) => {
            for _ in 0..steps {
                press(&mut device, 0x4B, 4)?;
            }
        }
        None => {
            // Up until the readout stops changing for "1 beat": rekordbox's
            // readout values aren't documented, so go up 4 steps (1/8, 1/4,
            // 1/2, 3/4 → 1) as on its BEAT buttons.
            for _ in 0..4 {
                press(&mut device, 0x4B, 4)?;
            }
        }
    }
    shown.take(&rx, Duration::from_millis(300));
    println!("beat readout: shortest {shortest:?}, now {:?}; depth {depth}%", shown.beat);
    // FX on (the lamp says whether it is).
    shown.take(&rx, Duration::from_millis(200));
    if shown.fx_on != Some(true) {
        press(&mut device, 0x47, 4)?;
        shown.take(&rx, Duration::from_millis(300));
    }
    println!("FX on: {:?}", shown.fx_on);

    // Channel 1 as the test needs it, whatever the controller's knobs say;
    // deck 1 at its own tempo (120 BPM): no SYNC, no Master Tempo.
    neutral_mixer(&mut device)?;
    shown.take(&rx, Duration::from_millis(300));
    if shown.sync == Some(true) {
        press(&mut device, 0x58, 0)?;
    }
    if shown.master_tempo == Some(true) {
        press(&mut device, 0x1A, 0)?;
    }
    shown.take(&rx, Duration::from_millis(300));
    println!("deck 1: SYNC {:?}, Master Tempo {:?}, tempo 0 %", shown.sync, shown.master_tempo);
    if shown.sync == Some(true) || shown.master_tempo == Some(true) {
        anyhow::bail!("deck 1's SYNC or Master Tempo stayed on");
    }
    // rekordbox records its own master (REC), whatever the audio output
    // (ROFLOXBOX_NO_REC=1: REC is left as it is, armed by hand).
    let handle_rec = std::env::var_os("ROFLOXBOX_NO_REC").is_none();
    if handle_rec && shown.recording != Some(true) {
        press(&mut device, 0x67, 6)?;
        shown.take(&rx, Duration::from_millis(800));
    }
    println!("recording: {:?}", shown.recording);
    if handle_rec && shown.recording != Some(true) {
        anyhow::bail!(
            "rekordbox's REC lamp didn't come on: recording didn't start (is a folder to save \
             recordings in set in its Preferences?)"
        );
    }
    // Play, and the next effect in the silence before each pass. With
    // ROFLOXBOX_DEPTH_SWEEP, the depth also steps 0, 10 … 100 % through each
    // pass (4 s steps after its 4 s gap), for measuring depth curves.
    let sweep = std::env::var_os("ROFLOXBOX_DEPTH_SWEEP").is_some();
    enum Do {
        Select(usize),
        Depth(f64),
    }
    let mut plan: Vec<(Duration, Do)> = Vec::new();
    for k in 0..EFFECTS.len() {
        let pass = Duration::from_secs(PASS_SECS * k as u64);
        if k > 0 {
            plan.push((pass + Duration::from_millis(1500), Do::Select(k)));
        }
        if sweep {
            for j in 0..11u64 {
                plan.push((pass + Duration::from_millis(4100 + j * 4000), Do::Depth(j as f64 / 10.0)));
            }
        }
    }
    plan.sort_by_key(|p| p.0);
    if sweep {
        knob(&mut device, 0xB4, 0x02, 0.0)?;
    }
    press(&mut device, 0x0B, 0)?;
    let t0 = Instant::now();
    println!("playing: pass 1 {}", EFFECTS[0]);
    for (at, what) in plan {
        std::thread::sleep(at.saturating_sub(t0.elapsed()));
        match what {
            Do::Select(k) => {
                neutral_mixer(&mut device)?;
                press(&mut device, 0x20 + k as u8, 4)?;
                println!("{:>6.1}s  pass {} {}", t0.elapsed().as_secs_f64(), k + 1, EFFECTS[k]);
            }
            Do::Depth(depth) => knob(&mut device, 0xB4, 0x02, depth)?,
        }
    }
    let end = Duration::from_secs(PASS_SECS * EFFECTS.len() as u64 + 3);
    std::thread::sleep(end.saturating_sub(t0.elapsed()));
    press(&mut device, 0x0B, 0)?;
    // REC off: rekordbox saves the recording.
    shown.take(&rx, Duration::from_millis(200));
    if handle_rec && shown.recording != Some(false) {
        press(&mut device, 0x67, 6)?;
    }
    // FX off again.
    shown.take(&rx, Duration::from_millis(200));
    if shown.fx_on == Some(true) {
        press(&mut device, 0x47, 4)?;
    }
    println!("done after {:.1}s", t0.elapsed().as_secs_f64());
    Ok(())
}

/// What rekordbox sends back: the BEAT readout and the FX ON lamp.
#[derive(Default)]
struct Shown {
    beat: Option<u8>,
    fx_on: Option<bool>,
    /// rekordbox's REC lamp.
    recording: Option<bool>,
    /// Deck 1's SYNC and Master Tempo lamps.
    sync: Option<bool>,
    master_tempo: Option<bool>,
}

impl Shown {
    /// Takes in what rekordbox sent, for `wait`.
    fn take(&mut self, rx: &mpsc::Receiver<Vec<u8>>, wait: Duration) {
        let until = Instant::now() + wait;
        while let Ok(bytes) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            match bytes.as_slice() {
                [0xB4, 0x64, v] => self.beat = Some(*v),
                [0x94, 0x47, v] => self.fx_on = Some(*v > 0),
                [0x96, 0x67, v] => self.recording = Some(*v > 0),
                [0x90, 0x58, v] => self.sync = Some(*v > 0),
                [0x90, 0x1A, v] => self.master_tempo = Some(*v > 0),
                _ => {}
            }
        }
    }
}

/// A button pressed and let go.
fn press(device: &mut midir::MidiOutputConnection, note: u8, channel: u8) -> anyhow::Result<()> {
    device.send(&[0x90 | channel, note, 0x7F])?;
    std::thread::sleep(Duration::from_millis(40));
    device.send(&[0x90 | channel, note, 0x00])?;
    std::thread::sleep(Duration::from_millis(120));
    Ok(())
}

/// A 14-bit knob or fader: `value` 0.0-1.0 on `cc` (and its fine part 32 on).
fn knob(device: &mut midir::MidiOutputConnection, status: u8, cc: u8, value: f64) -> anyhow::Result<()> {
    let v = (value.clamp(0.0, 1.0) * 16383.0).round() as u16;
    device.send(&[status, cc, (v >> 7) as u8])?;
    device.send(&[status, cc + 0x20, (v & 0x7F) as u8])?;
    Ok(())
}

/// Channel 1 fader up; its gain, EQ and Color FX knob in the middle; the
/// crossfader in the middle.
fn neutral_mixer(device: &mut midir::MidiOutputConnection) -> anyhow::Result<()> {
    // Deck 1's tempo fader in the middle: 0 %.
    knob(device, 0xB0, 0x00, 0.5)?;
    knob(device, 0xB0, 0x13, 1.0)?;
    for cc in [0x04, 0x07, 0x0B, 0x0F] {
        knob(device, 0xB0, cc, 0.5)?;
    }
    knob(device, 0xB6, 0x17, 0.5)?;
    knob(device, 0xB6, 0x1F, 0.5)?;
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}
