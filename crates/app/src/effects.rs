//! The mixer's effects as the app sees them: Beat FX settings and tapping,
//! momentary Pad FX, Color FX, Master Cue and the sampler slots.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use deckhand_audio::{
    BeatFxKind, BeatFxParams, ColorFxKind, Command, Engine, FxTarget, SAMPLER_SLOTS, TrackAudio,
    decode_file,
};

/// Sampler slots per bank.
pub const BANK_SLOTS: usize = 16;

/// Beat values the ◀ / ▶ buttons step through.
pub const FX_BEATS: [f64; 11] =
    [1.0 / 16.0, 0.125, 0.25, 0.5, 0.75, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
/// What Pioneer controllers' beat indicators expect for each of [`FX_BEATS`]
/// (the controller lights or flashes the matching LED itself).
const FX_BEAT_INDICATOR: [u8; 11] = [1, 2, 3, 4, 33, 5, 6, 7, 8, 9, 10];
const DEFAULT_BEAT: usize = 5;

/// Pad FX pads: A–H and, on the second page or with Shift, I–P.
pub const PAD_FX_SLOTS: usize = 16;

/// What one Pad FX pad runs while held (set in Preferences → Pad FX).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PadFxSlot {
    #[serde(with = "fx_kind")]
    pub effect: BeatFxKind,
    /// Beats, one of [`FX_BEATS`].
    pub beats: f64,
    /// 0.0–1.0.
    pub depth: f32,
}

impl PadFxSlot {
    pub fn label(self) -> String {
        format!("{} {}", self.effect.label(), beat_label(self.beats))
    }

    /// A short pad caption, like `ROLL 1/4`.
    pub fn short(self) -> String {
        let name = self.effect.label().split(' ').next_back().unwrap_or_default();
        format!("{name}\n{}", beat_label(self.beats))
    }

    /// The factory pads.
    pub fn defaults() -> Vec<PadFxSlot> {
        PAD_FX.iter().map(|&(effect, beats, depth)| PadFxSlot { effect, beats, depth }).collect()
    }
}

/// Effects saved by name in the settings.
mod fx_kind {
    use deckhand_audio::BeatFxKind;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(kind: &BeatFxKind, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(kind.label())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BeatFxKind, D::Error> {
        let name = String::deserialize(d)?;
        BeatFxKind::ALL
            .into_iter()
            .find(|k| k.label().eq_ignore_ascii_case(&name))
            .ok_or_else(|| D::Error::custom(format!("unknown effect {name}")))
    }
}

/// Factory Pad FX: (effect, beats, depth).
const PAD_FX: [(BeatFxKind, f64, f32); PAD_FX_SLOTS] = [
    (BeatFxKind::Roll, 0.25, 1.0),
    (BeatFxKind::Roll, 0.125, 1.0),
    (BeatFxKind::Roll, 1.0 / 16.0, 1.0),
    (BeatFxKind::Trans, 0.25, 1.0),
    (BeatFxKind::Echo, 0.5, 0.7),
    (BeatFxKind::Echo, 0.75, 0.7),
    (BeatFxKind::Reverb, 1.0, 0.8),
    (BeatFxKind::Pitch, 1.0, 0.5),
    (BeatFxKind::Roll, 0.5, 1.0),
    (BeatFxKind::Roll, 1.0, 1.0),
    (BeatFxKind::Spiral, 0.5, 0.7),
    (BeatFxKind::Trans, 0.125, 1.0),
    (BeatFxKind::LowCutEcho, 1.0, 0.7),
    (BeatFxKind::Helix, 0.5, 0.8),
    (BeatFxKind::Flanger, 4.0, 0.8),
    (BeatFxKind::Phaser, 4.0, 0.8),
];

pub fn beat_label(beats: f64) -> String {
    match beats {
        b if b < 1.0 && (1.0 / b).fract() == 0.0 => format!("1/{}", (1.0 / b) as u32),
        b if b < 1.0 => format!("{}/4", (b * 4.0) as u32),
        b => format!("{b}"),
    }
}

/// A loaded sampler slot.
pub struct Sample {
    pub name: String,
    audio: Arc<TrackAudio>,
}

type Loaded = Vec<(usize, String, Arc<TrackAudio>)>;

pub struct Effects {
    pub kind: BeatFxKind,
    pub on: bool,
    pub release: bool,
    pub depth: f32,
    /// Index into [`FX_BEATS`].
    pub beat: usize,
    pub target: FxTarget,
    /// BPM set with the FX tap button; `None` follows the master deck.
    pub tapped_bpm: Option<f64>,
    taps: Vec<Instant>,
    /// A held Pad FX pad: (deck, slot).
    pub pad: Option<(usize, u8)>,
    /// What each Pad FX pad does.
    pub pad_slots: [PadFxSlot; PAD_FX_SLOTS],
    pub color: ColorFxKind,
    pub color_on: bool,
    pub master_cue: bool,
    /// Sampler bank 0–3 (sixteen slots each).
    pub bank: usize,
    pub samples: [Option<Sample>; SAMPLER_SLOTS],
    loading: Vec<Receiver<Loaded>>,
    sent: Option<BeatFxParams>,
}

impl Default for Effects {
    fn default() -> Self {
        Self {
            kind: BeatFxKind::Echo,
            on: false,
            release: false,
            depth: 0.5,
            beat: DEFAULT_BEAT,
            target: FxTarget::Master,
            tapped_bpm: None,
            taps: Vec::new(),
            pad: None,
            pad_slots: std::array::from_fn(|i| PadFxSlot::defaults()[i]),
            color: ColorFxKind::Filter,
            color_on: true,
            master_cue: false,
            bank: 0,
            samples: std::array::from_fn(|_| None),
            loading: Vec::new(),
            sent: None,
        }
    }
}

impl Effects {
    pub fn pad_slot(&self, slot: usize) -> PadFxSlot {
        self.pad_slots[slot % PAD_FX_SLOTS]
    }

    /// Takes the pads from the settings; missing ones stay as they come.
    pub fn set_pad_slots(&mut self, slots: &[PadFxSlot]) {
        let defaults = PadFxSlot::defaults();
        self.pad_slots = std::array::from_fn(|i| slots.get(i).copied().unwrap_or(defaults[i]));
    }

    /// What the engine should run at `bpm`: the Beat FX, or a held Pad FX.
    fn params(&self, bpm: f64) -> BeatFxParams {
        let bpm = self.tapped_bpm.unwrap_or(bpm);
        let beat = |beats: f64| (60.0 / bpm * beats) as f32;
        if let Some((deck, slot)) = self.pad {
            let pad = self.pad_slot(usize::from(slot));
            return BeatFxParams {
                kind: pad.effect,
                on: true,
                depth: pad.depth,
                beat_secs: beat(pad.beats),
                target: FxTarget::Channel(deck),
                release: false,
            };
        }
        BeatFxParams {
            kind: self.kind,
            on: self.on,
            depth: self.depth,
            beat_secs: beat(FX_BEATS[self.beat]),
            target: self.target,
            release: self.release,
        }
    }

    /// Sends changed settings (the BPM follows the master deck) and hands
    /// newly decoded samples to the engine. Call every frame.
    pub fn update(&mut self, engine: &mut Engine, bpm: f64) {
        let params = self.params(bpm);
        if self.sent != Some(params) && engine.send(Command::SetBeatFx(params)) {
            self.sent = Some(params);
        }
        let mut done = Vec::new();
        self.loading.retain(|rx| match rx.try_recv() {
            Ok(loaded) => {
                done.extend(loaded);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        for (slot, name, audio) in done {
            engine.send(Command::SamplerLoad { slot, audio: audio.clone() });
            self.samples[slot] = Some(Sample { name, audio });
        }
    }

    /// Sends everything again to a newly started engine.
    pub fn apply_all(&mut self, engine: &mut Engine) {
        self.sent = None;
        engine.send(Command::SetColorFx(self.color));
        engine.send(Command::SetColorFxOn(self.color_on));
        engine.send(Command::SetMasterCue(self.master_cue));
        for (slot, sample) in self.samples.iter().enumerate() {
            if let Some(sample) = sample {
                engine.send(Command::SamplerLoad { slot, audio: sample.audio.clone() });
            }
        }
    }

    /// Sampler pads of decks 1/3 play slots 1–8 of the bank, decks 2/4 slots 9–16.
    pub fn sampler_slot(&self, deck: usize, pad: u8) -> usize {
        self.bank * BANK_SLOTS + usize::from(pad % 8) + if deck % 2 == 1 { 8 } else { 0 }
    }

    pub fn change_bank(&mut self, steps: i32) {
        let banks = (SAMPLER_SLOTS / BANK_SLOTS) as i32;
        self.bank = (self.bank as i32 + steps).rem_euclid(banks) as usize;
    }

    pub fn beat_label(&self) -> String {
        beat_label(FX_BEATS[self.beat])
    }

    /// Value for the controller's beat indicator.
    pub fn beat_indicator(&self) -> u8 {
        FX_BEAT_INDICATOR[self.beat]
    }

    pub fn change_beat(&mut self, steps: i32) {
        self.beat = (self.beat as i32 + steps).clamp(0, FX_BEATS.len() as i32 - 1) as usize;
    }

    /// FX on/off; switching off also ends Release FX.
    pub fn toggle_on(&mut self) {
        self.on = !self.on;
        if !self.on {
            self.release = false;
        }
    }

    pub fn toggle_release(&mut self) {
        self.release = !self.release;
    }

    /// Tapping sets the effect's own BPM.
    pub fn tap(&mut self) {
        let now = Instant::now();
        if self.taps.last().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(2)) {
            self.taps.clear();
        }
        self.taps.push(now);
        if self.taps.len() > 8 {
            self.taps.remove(0);
        }
        if self.taps.len() >= 3 {
            let span = now.duration_since(self.taps[0]).as_secs_f64();
            self.tapped_bpm = Some(60.0 * (self.taps.len() - 1) as f64 / span);
        }
    }

    pub fn set_color(&mut self, kind: ColorFxKind, engine: &mut Engine) {
        self.color = kind;
        engine.send(Command::SetColorFx(kind));
    }

    pub fn next_color(&mut self, engine: &mut Engine) {
        let all = ColorFxKind::ALL;
        let at = all.iter().position(|&k| k == self.color).unwrap_or(0);
        self.set_color(all[(at + 1) % all.len()], engine);
    }

    pub fn toggle_color(&mut self, engine: &mut Engine) {
        self.color_on = !self.color_on;
        engine.send(Command::SetColorFxOn(self.color_on));
    }

    pub fn toggle_master_cue(&mut self, engine: &mut Engine) {
        self.master_cue = !self.master_cue;
        engine.send(Command::SetMasterCue(self.master_cue));
    }

    /// Decodes the first 64 audio files of `dir` into the sampler banks,
    /// in the background.
    pub fn load_samples(&mut self, dir: &Path) {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        files.retain(|p| {
            p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                ["wav", "mp3", "aif", "aiff", "flac", "m4a", "ogg"]
                    .contains(&e.to_ascii_lowercase().as_str())
            })
        });
        files.sort();
        files.truncate(SAMPLER_SLOTS);
        self.load_files(files.into_iter().enumerate().collect());
    }

    /// Loads one file into a sampler slot, in the background.
    pub fn load_into(&mut self, slot: usize, path: PathBuf) {
        self.load_files(vec![(slot, path)]);
    }

    fn load_files(&mut self, files: Vec<(usize, PathBuf)>) {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let loaded: Loaded = files
                .iter()
                .filter_map(|&(slot, ref path)| {
                    let name = path.file_stem()?.to_string_lossy().into_owned();
                    let audio = decode_file(path).ok()?;
                    Some((slot, name, Arc::new(audio)))
                })
                .collect();
            let _ = tx.send(loaded);
        });
        self.loading.push(rx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beat_labels() {
        let labels: Vec<String> = FX_BEATS.iter().map(|&b| beat_label(b)).collect();
        assert_eq!(labels, ["1/16", "1/8", "1/4", "1/2", "3/4", "1", "2", "4", "8", "16", "32"]);
    }

    #[test]
    fn pad_fx_overrides_the_beat_fx_while_held() {
        let mut fx = Effects { on: true, ..Effects::default() };
        assert_eq!(fx.params(120.0).beat_secs, 0.5);
        fx.pad = Some((2, 0));
        let held = fx.params(120.0);
        assert_eq!(
            (held.kind, held.target, held.beat_secs),
            (BeatFxKind::Roll, FxTarget::Channel(2), 0.125)
        );
        fx.pad = None;
        assert_eq!(fx.params(120.0).kind, BeatFxKind::Echo);
    }
}
