//! The real-time mixer and the audio output stream.
//!
//! The audio callback never blocks or allocates: the UI sends [`Command`]s
//! through a lock-free queue, reads deck state from atomics, and receives
//! replaced track buffers back for dropping outside the audio thread.

use std::f32::consts::FRAC_PI_2;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SizedSample, StreamConfig, SupportedBufferSize};

use crate::decode::TrackAudio;
use crate::eq::Equalizer;
use crate::fx::{BeatFx, BeatFxParams, ColorFx, ColorFxKind, FxTarget};
use crate::stretch::Stretcher;

pub const DECKS: usize = 4;
/// Sampler slots: four banks of sixteen.
pub const SAMPLER_SLOTS: usize = 64;
const QUEUE: usize = 1024;
/// How quickly a held or gliding record catches up with the jog, and how
/// quickly its speed follows, in seconds. Together they turn the jog's
/// steps, which arrive in bursts, into smooth motion (damping ≈ 0.7).
const JOG_FOLLOW_SECS: f64 = 0.015;
const JOG_SMOOTH_SECS: f64 = 0.008;
/// Fastest scratch, times normal speed.
const MAX_SCRATCH_SPEED: f64 = 8.0;

/// How samples between the source frames are computed when the speed or
/// sample rate differs from the original.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Quality {
    /// Linear interpolation: cheapest, slightly dull highs and some aliasing.
    Standard,
    /// Cubic (Hermite) interpolation.
    #[default]
    High,
}

/// How to open the audio output; `None` fields use the device's defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioOptions {
    pub device: Option<String>,
    pub sample_rate: Option<u32>,
    pub buffer_frames: Option<u32>,
    pub quality: Quality,
}

/// Which side of the crossfader a channel is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossfaderSide {
    A,
    /// Not affected by the crossfader.
    Thru,
    B,
}

#[derive(Debug)]
pub enum Command {
    Load {
        deck: usize,
        audio: Arc<TrackAudio>,
    },
    Unload {
        deck: usize,
    },
    SetPlaying {
        deck: usize,
        playing: bool,
    },
    /// Jump to an absolute position.
    Seek {
        deck: usize,
        seconds: f64,
    },
    /// Move by a relative amount (jog wheel while paused, beat jump).
    Shift {
        deck: usize,
        seconds: f64,
    },
    /// Playback speed, 1.0 = original tempo.
    SetTempo {
        deck: usize,
        ratio: f64,
    },
    /// Temporary speed change on top of the tempo (jog wheel while playing).
    SetNudge {
        deck: usize,
        amount: f64,
    },
    /// Master Tempo: keep the key when the tempo changes.
    SetKeyLock {
        deck: usize,
        on: bool,
    },
    /// Hand on the record (vinyl jog top): playback follows [`Command::JogMove`]
    /// instead of the tempo until released.
    Hold {
        deck: usize,
        held: bool,
    },
    /// Moves the record by this much while it is held, or while the deck is
    /// paused. Playback glides there, so single jog steps do not click.
    JogMove {
        deck: usize,
        seconds: f64,
    },
    SetQuality(Quality),
    /// Slip mode: after scratching, a loop or a held hot cue, playback
    /// continues where the track would have been.
    SetSlip {
        deck: usize,
        on: bool,
    },
    /// Holds the slip timeline while a hot cue pad is held.
    SlipHold {
        deck: usize,
        held: bool,
    },
    /// Changes the key without changing the tempo, in semitones.
    SetKeyShift {
        deck: usize,
        semitones: f64,
    },
    /// Adds the master mix to the headphones.
    SetMasterCue(bool),
    /// The Color FX type of every channel knob.
    SetColorFx(ColorFxKind),
    /// With Color FX off the channel knobs do nothing.
    SetColorFxOn(bool),
    SetBeatFx(BeatFxParams),
    SamplerLoad {
        slot: usize,
        audio: Arc<TrackAudio>,
    },
    /// Plays the slot from its start (again, if it is playing).
    SamplerPlay {
        slot: usize,
    },
    SamplerStop {
        slot: usize,
    },
    SetSamplerVolume(f32),
    SetCrossfaderCurve(CrossfaderCurve),
    SetFaderCurve(FaderCurve),
    /// Off: the master is only clipped at full scale.
    SetLimiter(bool),
    /// Vinyl Speed Adjust: how long starting and stopping (brake) take, in
    /// seconds; 0 is instant.
    SetVinylSpeed {
        deck: usize,
        start: f64,
        brake: f64,
    },
    /// Loop between two positions in seconds; playback wraps at the end.
    SetLoop {
        deck: usize,
        start: f64,
        end: f64,
    },
    ClearLoop {
        deck: usize,
    },
    /// Channel fader, 0.0–1.0.
    SetVolume {
        deck: usize,
        level: f32,
    },
    /// Gain before the EQ, linear.
    SetTrim {
        deck: usize,
        gain: f32,
    },
    /// EQ band 0 = low, 1 = mid, 2 = high, in dB.
    SetEq {
        deck: usize,
        band: usize,
        db: f32,
    },
    /// The channel's Color FX knob, −1.0 … 1.0 (with the Filter type:
    /// low-pass left, high-pass right).
    SetFilter {
        deck: usize,
        position: f32,
    },
    SetCrossfaderAssign {
        deck: usize,
        side: CrossfaderSide,
    },
    SetHeadphoneCue {
        deck: usize,
        on: bool,
    },
    /// −1.0 = full A side, 1.0 = full B side.
    SetCrossfader(f32),
    SetMasterVolume(f32),
    /// 0.0 = only cued channels, 1.0 = only master.
    SetHeadphoneMix(f32),
    SetHeadphoneVolume(f32),
}

/// Deck state the audio thread publishes for the UI.
#[derive(Debug, Default)]
struct DeckMeter {
    position_secs: AtomicU64,
    playing: AtomicBool,
    peak: AtomicU32,
}

#[derive(Debug)]
struct Shared {
    decks: [DeckMeter; DECKS],
    master_peak: AtomicU32,
    sampler_playing: [AtomicBool; SAMPLER_SLOTS],
}

// Arrays this long have no derived `Default`.
impl Default for Shared {
    fn default() -> Self {
        Self {
            decks: Default::default(),
            master_peak: AtomicU32::default(),
            sampler_playing: std::array::from_fn(|_| AtomicBool::default()),
        }
    }
}

fn store_f64(slot: &AtomicU64, value: f64) {
    slot.store(value.to_bits(), Ordering::Relaxed);
}

fn store_f32(slot: &AtomicU32, value: f32) {
    slot.store(value.to_bits(), Ordering::Relaxed);
}

fn load_f32(slot: &AtomicU32) -> f32 {
    f32::from_bits(slot.load(Ordering::Relaxed))
}

/// The output device in use.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Frames per audio callback when a buffer size was set, else the
    /// device's default.
    pub buffer_frames: Option<u32>,
}

impl OutputInfo {
    /// Output latency of one buffer, in milliseconds.
    pub fn latency_ms(&self) -> Option<f64> {
        self.buffer_frames.map(|f| f64::from(f) * 1000.0 / f64::from(self.sample_rate))
    }

    /// Whether outputs 3–4 can carry a separate headphone mix.
    pub fn has_headphone_output(&self) -> bool {
        self.channels >= 4
    }
}

pub struct Engine {
    commands: rtrb::Producer<Command>,
    trash: rtrb::Consumer<Arc<TrackAudio>>,
    shared: Arc<Shared>,
    info: OutputInfo,
    _stream: cpal::Stream,
}

impl Engine {
    /// Names of the available output devices.
    pub fn output_devices() -> Vec<String> {
        let host = cpal::default_host();
        host.output_devices()
            .map(|devices| {
                devices.filter_map(|d| d.description().ok().map(|d| d.name().to_owned())).collect()
            })
            .unwrap_or_default()
    }

    fn find_device(name: Option<&str>) -> Result<cpal::Device> {
        let host = cpal::default_host();
        match name {
            Some(name) => host
                .output_devices()?
                .find(|d| d.description().is_ok_and(|desc| desc.name() == name))
                .ok_or_else(|| anyhow!("audio device {name} not found")),
            None => host.default_output_device().ok_or_else(|| anyhow!("no audio output device")),
        }
    }

    /// Sample rates a device can play, and its buffer size range in frames
    /// if it reports one.
    pub fn device_capabilities(device: Option<&str>) -> (Vec<u32>, Option<(u32, u32)>) {
        let Ok(configs) = Self::find_device(device).and_then(|d| Ok(d.supported_output_configs()?))
        else {
            return (Vec::new(), None);
        };
        let configs: Vec<_> = configs.collect();
        let rates = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000]
            .into_iter()
            .filter(|&r| configs.iter().any(|c| c.try_with_sample_rate(r).is_some()))
            .collect();
        let buffers = configs.iter().find_map(|c| match *c.buffer_size() {
            SupportedBufferSize::Range { min, max } => Some((min, max)),
            SupportedBufferSize::Unknown => None,
        });
        (rates, buffers)
    }

    /// Opens the output with as many channels as the device offers, so that
    /// a controller's headphone output can be used.
    pub fn start(options: &AudioOptions) -> Result<Self> {
        let device = Self::find_device(options.device.as_deref())?;
        let name =
            device.description().map(|d| d.name().to_owned()).unwrap_or_else(|_| "unknown".into());
        let default =
            device.default_output_config().context("reading the device's output format")?;
        let rate = options.sample_rate.unwrap_or(default.sample_rate());
        let usable = |f: SampleFormat| {
            matches!(f, SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32)
        };
        let best = device
            .supported_output_configs()?
            .filter(|c| usable(c.sample_format()))
            .filter_map(|c| c.try_with_sample_rate(rate))
            // Most channels first, then the device's own sample format.
            .max_by_key(|c| (c.channels(), c.sample_format() == default.sample_format()))
            .unwrap_or(default);
        let format = best.sample_format();
        let buffer_frames = options.buffer_frames.map(|frames| match *best.buffer_size() {
            SupportedBufferSize::Range { min, max } => frames.clamp(min, max),
            SupportedBufferSize::Unknown => frames,
        });
        let mut config: StreamConfig = best.into();
        if let Some(frames) = buffer_frames {
            config.buffer_size = BufferSize::Fixed(frames);
        }
        let info = OutputInfo {
            device: name,
            sample_rate: config.sample_rate,
            channels: config.channels,
            buffer_frames,
        };

        let (mut commands, command_rx) = rtrb::RingBuffer::new(QUEUE);
        let (trash_tx, trash) = rtrb::RingBuffer::new(QUEUE);
        let shared = Arc::new(Shared::default());
        let mixer = Mixer::new(info.sample_rate, command_rx, trash_tx, shared.clone());
        let _ = commands.push(Command::SetQuality(options.quality));
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, mixer)?,
            SampleFormat::I16 => build::<i16>(&device, config, mixer)?,
            SampleFormat::I32 => build::<i32>(&device, config, mixer)?,
            other => return Err(anyhow!("unsupported sample format {other}")),
        };
        stream.play()?;
        Ok(Self { commands, trash, shared, info, _stream: stream })
    }

    pub fn info(&self) -> &OutputInfo {
        &self.info
    }

    /// Queues a command; returns `false` if the queue is full.
    pub fn send(&mut self, command: Command) -> bool {
        self.commands.push(command).is_ok()
    }

    pub fn position_secs(&self, deck: usize) -> f64 {
        f64::from_bits(self.shared.decks[deck].position_secs.load(Ordering::Relaxed))
    }

    pub fn is_playing(&self, deck: usize) -> bool {
        self.shared.decks[deck].playing.load(Ordering::Relaxed)
    }

    /// Recent peak level of a deck after its fader, 0.0–1.0+.
    pub fn deck_peak(&self, deck: usize) -> f32 {
        load_f32(&self.shared.decks[deck].peak)
    }

    pub fn sampler_playing(&self, slot: usize) -> bool {
        self.shared.sampler_playing.get(slot).is_some_and(|p| p.load(Ordering::Relaxed))
    }

    pub fn master_peak(&self) -> f32 {
        load_f32(&self.shared.master_peak)
    }

    /// Drops track buffers the audio thread has replaced. Call regularly.
    pub fn collect_garbage(&mut self) {
        while self.trash.pop().is_ok() {}
    }
}

fn build<T>(device: &cpal::Device, config: StreamConfig, mut mixer: Mixer) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], _| mixer.render(data, channels),
        |err| eprintln!("audio stream error: {err}"),
        None,
    )?)
}

struct DeckState {
    audio: Option<Arc<TrackAudio>>,
    /// Position in source frames.
    position: f64,
    playing: bool,
    tempo: f64,
    nudge: f64,
    key_lock: bool,
    stretcher: Stretcher,
    /// Where the jog wants the record, in source frames, while it is held
    /// or gliding to a paused jog position.
    target: Option<f64>,
    held: bool,
    /// Playback speed in source frames per output frame.
    velocity: f64,
    /// Vinyl Speed Adjust: seconds to start up / brake to a stop.
    start_secs: f64,
    brake_secs: f64,
    /// Slowing down after stop with a brake time.
    braking: bool,
    /// Loop in source frames.
    loop_frames: Option<(f64, f64)>,
    volume: f32,
    trim: f32,
    eq: Equalizer,
    color: ColorFx,
    slip: bool,
    slip_hold: bool,
    was_slipping: bool,
    /// Where the track would be without scratching or looping (slip mode).
    shadow: f64,
    /// Key shift as a frequency ratio.
    key_ratio: f64,
    side: CrossfaderSide,
    cue: bool,
    peak: f32,
}

struct Mixer {
    rate: f64,
    quality: Quality,
    decks: [DeckState; DECKS],
    crossfader: f32,
    master: f32,
    headphone_mix: f32,
    headphone_volume: f32,
    master_peak: f32,
    master_cue: bool,
    crossfader_curve: CrossfaderCurve,
    fader_curve: FaderCurve,
    limiter: bool,
    color_on: bool,
    beat_fx: BeatFx,
    sampler: [SamplerVoice; SAMPLER_SLOTS],
    sampler_volume: f32,
    commands: rtrb::Consumer<Command>,
    trash: rtrb::Producer<Arc<TrackAudio>>,
    shared: Arc<Shared>,
}

/// One sampler slot: plays its sample once from the start.
#[derive(Debug, Default)]
struct SamplerVoice {
    audio: Option<Arc<TrackAudio>>,
    position: f64,
    playing: bool,
}

/// How the crossfader blends the A and B sides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CrossfaderCurve {
    /// A long blend: one side fades in while the other fades out.
    Smooth,
    /// Both sides full up to the center, then fading out.
    #[default]
    Standard,
    /// Both sides full until the very ends (for scratching).
    Sharp,
}

/// How the channel faders rise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FaderCurve {
    /// Quiet for most of the travel, loud at the top.
    Slow,
    #[default]
    Standard,
    /// Loud quickly.
    Fast,
}

/// Gains of the A and B sides.
fn crossfader_gains(position: f32, curve: CrossfaderCurve) -> [f32; 2] {
    let x = position.clamp(-1.0, 1.0);
    match curve {
        CrossfaderCurve::Smooth => {
            // Constant power over the whole travel.
            let t = (x + 1.0) / 2.0 * FRAC_PI_2;
            [t.cos(), t.sin()]
        }
        CrossfaderCurve::Standard => {
            [(x.max(0.0) * FRAC_PI_2).cos(), ((-x).max(0.0) * FRAC_PI_2).cos()]
        }
        CrossfaderCurve::Sharp => {
            let cut = |v: f32| ((v - 0.9).max(0.0) / 0.1 * FRAC_PI_2).cos();
            [cut(x), cut(-x)]
        }
    }
}

/// Channel fader gain.
fn fader_curve_gain(level: f32, curve: FaderCurve) -> f32 {
    let level = level.clamp(0.0, 1.0);
    match curve {
        FaderCurve::Slow => level * level * level,
        FaderCurve::Standard => level * level,
        FaderCurve::Fast => 1.0 - (1.0 - level) * (1.0 - level),
    }
}

/// Master and headphone level curve.
fn fader_gain(level: f32) -> f32 {
    fader_curve_gain(level, FaderCurve::Standard)
}

/// Gentle limiter: transparent below ±0.8, then saturates towards ±1.
fn limit(sample: f32) -> f32 {
    let x = sample.abs();
    if x <= 0.8 { sample } else { sample.signum() * (0.8 + 0.2 * ((x - 0.8) / 0.2).tanh()) }
}

impl Mixer {
    fn new(
        rate: u32,
        commands: rtrb::Consumer<Command>,
        trash: rtrb::Producer<Arc<TrackAudio>>,
        shared: Arc<Shared>,
    ) -> Self {
        let deck = |index: usize| DeckState {
            audio: None,
            position: 0.0,
            playing: false,
            tempo: 1.0,
            nudge: 0.0,
            key_lock: false,
            stretcher: Stretcher::new(rate),
            target: None,
            held: false,
            velocity: 0.0,
            start_secs: 0.0,
            brake_secs: 0.0,
            braking: false,
            loop_frames: None,
            volume: 1.0,
            trim: 1.0,
            eq: Equalizer::new(rate as f32),
            color: ColorFx::new(rate),
            slip: false,
            slip_hold: false,
            was_slipping: false,
            shadow: 0.0,
            key_ratio: 1.0,
            // Decks 1 and 3 on the left, 2 and 4 on the right.
            side: if index.is_multiple_of(2) { CrossfaderSide::A } else { CrossfaderSide::B },
            cue: false,
            peak: 0.0,
        };
        Self {
            rate: f64::from(rate),
            quality: Quality::default(),
            decks: std::array::from_fn(deck),
            crossfader: 0.0,
            master: 0.8,
            headphone_mix: 0.0,
            headphone_volume: 0.8,
            master_peak: 0.0,
            master_cue: false,
            crossfader_curve: CrossfaderCurve::default(),
            fader_curve: FaderCurve::default(),
            limiter: true,
            color_on: true,
            beat_fx: BeatFx::new(rate),
            sampler: std::array::from_fn(|_| SamplerVoice::default()),
            sampler_volume: 0.8,
            commands,
            trash,
            shared,
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Load { deck, audio } => {
                let d = &mut self.decks[deck];
                if let Some(old) = d.audio.replace(audio) {
                    // Freeing a whole track here could stall the callback.
                    let _ = self.trash.push(old);
                }
                (d.position, d.playing, d.nudge, d.target, d.held, d.loop_frames) =
                    (0.0, false, 0.0, None, false, None);
                (d.velocity, d.braking) = (0.0, false);
            }
            Command::Unload { deck } => {
                let d = &mut self.decks[deck];
                if let Some(old) = d.audio.take() {
                    let _ = self.trash.push(old);
                }
                d.playing = false;
            }
            Command::SetPlaying { deck, playing } => {
                let d = &mut self.decks[deck];
                // Stopping with a brake time lets the record slow down.
                d.braking = !playing && d.playing && d.brake_secs > 0.0 && d.target.is_none();
                d.playing = playing && d.audio.is_some();
                if d.playing && !d.held {
                    d.target = None;
                }
            }
            Command::Seek { deck, seconds } => {
                let d = &mut self.decks[deck];
                if let Some(audio) = &d.audio {
                    d.position =
                        (seconds * f64::from(audio.sample_rate)).clamp(0.0, audio.frames() as f64);
                    d.target = d.held.then_some(d.position);
                }
            }
            Command::Shift { deck, seconds } => {
                let d = &mut self.decks[deck];
                if let Some(audio) = &d.audio {
                    d.position = (d.position + seconds * f64::from(audio.sample_rate))
                        .clamp(0.0, audio.frames() as f64);
                    d.target = d.held.then_some(d.position);
                }
            }
            Command::SetTempo { deck, ratio } => self.decks[deck].tempo = ratio.clamp(0.0, 4.0),
            Command::SetNudge { deck, amount } => self.decks[deck].nudge = amount.clamp(-0.9, 0.9),
            Command::SetKeyLock { deck, on } => self.decks[deck].key_lock = on,
            Command::Hold { deck, held } => {
                let d = &mut self.decks[deck];
                d.held = held && d.audio.is_some();
                if d.held {
                    d.target.get_or_insert(d.position);
                } else if d.playing {
                    // Let go of the record: back to the tempo.
                    d.target = None;
                }
            }
            Command::JogMove { deck, seconds } => {
                let d = &mut self.decks[deck];
                if let Some(audio) = &d.audio
                    && (d.held || !d.playing)
                {
                    let target = d.target.unwrap_or(d.position);
                    let end = audio.frames() as f64;
                    d.target =
                        Some((target + seconds * f64::from(audio.sample_rate)).clamp(0.0, end));
                }
            }
            Command::SetQuality(quality) => self.quality = quality,
            Command::SetLoop { deck, start, end } => {
                let d = &mut self.decks[deck];
                if let Some(audio) = &d.audio {
                    let rate = f64::from(audio.sample_rate);
                    d.loop_frames = (end > start).then_some((start * rate, end * rate));
                }
            }
            Command::ClearLoop { deck } => self.decks[deck].loop_frames = None,
            Command::SetVolume { deck, level } => self.decks[deck].volume = level,
            Command::SetTrim { deck, gain } => self.decks[deck].trim = gain,
            Command::SetEq { deck, band, db } => self.decks[deck].eq.set_gain(band.min(2), db),
            Command::SetFilter { deck, position } => self.decks[deck].color.set_position(position),
            Command::SetSlip { deck, on } => self.decks[deck].slip = on,
            Command::SlipHold { deck, held } => self.decks[deck].slip_hold = held,
            Command::SetKeyShift { deck, semitones } => {
                self.decks[deck].key_ratio = 2f64.powf(semitones.clamp(-24.0, 24.0) / 12.0)
            }
            Command::SetMasterCue(on) => self.master_cue = on,
            Command::SetColorFx(kind) => {
                for deck in &mut self.decks {
                    deck.color.set_kind(kind);
                }
            }
            Command::SetColorFxOn(on) => self.color_on = on,
            Command::SetBeatFx(params) => self.beat_fx.set(params),
            Command::SamplerLoad { slot, audio } => {
                if let Some(voice) = self.sampler.get_mut(slot) {
                    if let Some(old) = voice.audio.replace(audio) {
                        let _ = self.trash.push(old);
                    }
                    voice.playing = false;
                }
            }
            Command::SamplerPlay { slot } => {
                if let Some(voice) = self.sampler.get_mut(slot) {
                    voice.position = 0.0;
                    voice.playing = voice.audio.is_some();
                }
            }
            Command::SamplerStop { slot } => {
                if let Some(voice) = self.sampler.get_mut(slot) {
                    voice.playing = false;
                }
            }
            Command::SetSamplerVolume(level) => self.sampler_volume = level,
            Command::SetCrossfaderCurve(curve) => self.crossfader_curve = curve,
            Command::SetFaderCurve(curve) => self.fader_curve = curve,
            Command::SetLimiter(on) => self.limiter = on,
            Command::SetVinylSpeed { deck, start, brake } => {
                let d = &mut self.decks[deck];
                (d.start_secs, d.brake_secs) = (start.max(0.0), brake.max(0.0));
            }
            Command::SetCrossfaderAssign { deck, side } => self.decks[deck].side = side,
            Command::SetHeadphoneCue { deck, on } => self.decks[deck].cue = on,
            Command::SetCrossfader(position) => self.crossfader = position,
            Command::SetMasterVolume(level) => self.master = level,
            Command::SetHeadphoneMix(mix) => self.headphone_mix = mix.clamp(0.0, 1.0),
            Command::SetHeadphoneVolume(level) => self.headphone_volume = level,
        }
    }

    fn render<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        while let Ok(command) = self.commands.pop() {
            self.apply(command);
        }
        let xfade = crossfader_gains(self.crossfader, self.crossfader_curve);
        let master_gain = fader_gain(self.master) * 1.25;
        let headphone_gain = fader_gain(self.headphone_volume) * 1.25;

        let fx_target = self.beat_fx.params().target;
        let cubic = self.quality == Quality::High;
        for frame in out.chunks_mut(channels) {
            let mut master = [0f32; 2];
            let mut cue = [0f32; 2];
            // The channel or sampler signal the Beat FX is inserted on.
            let mut fx_in = [0f32; 2];
            for (index, deck) in self.decks.iter_mut().enumerate() {
                let Some(audio) = deck.audio.as_ref() else { continue };
                let step = f64::from(audio.sample_rate) / self.rate;
                // Slip: a shadow position keeps the normal timeline while the
                // record is held, a loop plays or a hot cue is held; playback
                // returns to it afterwards.
                let slipping = deck.slip
                    && deck.playing
                    && (deck.held || deck.loop_frames.is_some() || deck.slip_hold);
                if deck.was_slipping && !slipping && deck.slip && deck.playing {
                    deck.position = deck.shadow;
                }
                if slipping {
                    deck.shadow += deck.tempo * (1.0 + deck.nudge) * step;
                } else {
                    deck.shadow = deck.position;
                }
                deck.was_slipping = slipping;
                if let Some(target) = deck.target {
                    // A damped follower: the speed heads for what would reach
                    // the target, so the record moves smoothly with the jog.
                    let limit = MAX_SCRATCH_SPEED * step;
                    let wanted = ((target - deck.position) / (JOG_FOLLOW_SECS * self.rate))
                        .clamp(-limit, limit);
                    deck.velocity += (wanted - deck.velocity) / (JOG_SMOOTH_SECS * self.rate);
                    let settled =
                        (target - deck.position).abs() < 0.5 && deck.velocity.abs() < 1e-3;
                    if settled && !deck.held {
                        deck.target = None;
                        deck.velocity = 0.0;
                        continue;
                    }
                } else if deck.playing {
                    let speed = deck.tempo * (1.0 + deck.nudge) * step;
                    if deck.start_secs > 0.0 {
                        // Spins up from standstill like a turntable motor.
                        let rise = step / (deck.start_secs * self.rate);
                        deck.velocity = if deck.velocity < speed {
                            (deck.velocity + rise).min(speed)
                        } else {
                            speed
                        };
                    } else {
                        deck.velocity = speed;
                    }
                } else if deck.braking {
                    deck.velocity -= step / (deck.brake_secs * self.rate);
                    if deck.velocity <= 0.0 {
                        (deck.velocity, deck.braking) = (0.0, false);
                        continue;
                    }
                } else {
                    deck.velocity = 0.0;
                    deck.stretcher.reset();
                    continue;
                }
                // Scratching always bends the pitch, like a record.
                let shifted = deck.key_lock || deck.key_ratio != 1.0;
                let raw = if shifted && deck.target.is_none() {
                    // Grains play at the original pitch (Master Tempo) or the
                    // tempo's, times the key shift.
                    let pitch = if deck.key_lock { step } else { deck.velocity };
                    deck.stretcher.next(audio, deck.position, pitch * deck.key_ratio, cubic)
                } else {
                    deck.stretcher.reset();
                    audio.sample_at(deck.position, cubic)
                };
                let sample = [raw[0] * deck.trim, raw[1] * deck.trim];
                let sample = deck.eq.process(sample);
                let sample = if self.color_on { deck.color.process(sample) } else { sample };
                let side_gain = match deck.side {
                    CrossfaderSide::A => xfade[0],
                    CrossfaderSide::B => xfade[1],
                    CrossfaderSide::Thru => 1.0,
                };
                let gain = fader_curve_gain(deck.volume, self.fader_curve) * side_gain;
                let to_fx = fx_target == FxTarget::Channel(index);
                for channel in 0..2 {
                    let post = sample[channel] * gain;
                    if to_fx {
                        fx_in[channel] += post;
                    } else {
                        master[channel] += post;
                    }
                    deck.peak = deck.peak.max(post.abs());
                    if deck.cue {
                        cue[channel] += sample[channel];
                    }
                }
                deck.position += deck.velocity;
                if let Some((start, end)) = deck.loop_frames
                    && deck.velocity > 0.0
                    && deck.position >= end
                {
                    deck.position = start + (deck.position - end) % (end - start);
                }
                deck.position = deck.position.max(0.0);
                if deck.position >= audio.frames() as f64 {
                    deck.position = audio.frames() as f64;
                    deck.playing = false;
                }
            }
            let mut sampler = [0f32; 2];
            for voice in &mut self.sampler {
                let Some(audio) = voice.audio.as_ref().filter(|_| voice.playing) else { continue };
                let s = audio.sample_at(voice.position, cubic);
                sampler = [sampler[0] + s[0], sampler[1] + s[1]];
                voice.position += f64::from(audio.sample_rate) / self.rate;
                voice.playing = voice.position < audio.frames() as f64;
            }
            let sampler = sampler.map(|s| s * fader_gain(self.sampler_volume));
            match fx_target {
                FxTarget::Sampler => fx_in = sampler,
                _ => master = [master[0] + sampler[0], master[1] + sampler[1]],
            }
            if fx_target == FxTarget::Master {
                master = self.beat_fx.process(master);
            } else {
                let wet = self.beat_fx.process(fx_in);
                master = [master[0] + wet[0], master[1] + wet[1]];
            }
            if self.master_cue {
                cue = [cue[0] + master[0], cue[1] + master[1]];
            }
            let limiter = self.limiter;
            let master = master.map(|s| {
                if limiter { limit(s * master_gain) } else { (s * master_gain).clamp(-1.0, 1.0) }
            });
            self.master_peak = self.master_peak.max(master[0].abs()).max(master[1].abs());
            for (channel, slot) in frame.iter_mut().enumerate() {
                let value = match channel {
                    0 | 1 => master[channel],
                    2 | 3 => {
                        let c = channel - 2;
                        let mix =
                            cue[c] * (1.0 - self.headphone_mix) + master[c] * self.headphone_mix;
                        limit(mix * headphone_gain)
                    }
                    _ => 0.0,
                };
                *slot = T::from_sample(value);
            }
        }

        for (deck, meter) in self.decks.iter_mut().zip(&self.shared.decks) {
            let rate = deck.audio.as_ref().map_or(1.0, |a| f64::from(a.sample_rate));
            store_f64(&meter.position_secs, deck.position / rate);
            meter.playing.store(deck.playing, Ordering::Relaxed);
            // Publish and decay the peak so meters fall back smoothly.
            store_f32(&meter.peak, deck.peak);
            deck.peak *= 0.9;
        }
        store_f32(&self.shared.master_peak, self.master_peak);
        for (voice, playing) in self.sampler.iter().zip(&self.shared.sampler_playing) {
            playing.store(voice.playing, Ordering::Relaxed);
        }
        self.master_peak *= 0.9;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mixer(rate: u32) -> (Mixer, rtrb::Producer<Command>, Arc<Shared>) {
        let (tx, rx) = rtrb::RingBuffer::new(64);
        let (trash_tx, _trash) = rtrb::RingBuffer::new(64);
        let shared = Arc::new(Shared::default());
        (Mixer::new(rate, rx, trash_tx, shared.clone()), tx, shared)
    }

    fn position(shared: &Shared, deck: usize) -> f64 {
        f64::from_bits(shared.decks[deck].position_secs.load(Ordering::Relaxed))
    }

    #[test]
    fn crossfader_is_full_on_both_sides_at_center() {
        assert_eq!(crossfader_gains(0.0, CrossfaderCurve::Standard), [1.0, 1.0]);
        let [a, b] = crossfader_gains(1.0, CrossfaderCurve::Standard);
        assert!(a < 1e-6 && b == 1.0);
        let [a, b] = crossfader_gains(-1.0, CrossfaderCurve::Standard);
        assert!(a == 1.0 && b < 1e-6);
    }

    #[test]
    fn mixer_plays_at_tempo_and_stops_at_end() {
        let (mut mixer, mut tx, shared) = mixer(48_000);
        let audio = Arc::new(TrackAudio { sample_rate: 48_000, samples: vec![0.5; 48_000 * 2] });
        tx.push(Command::Load { deck: 0, audio }).unwrap();
        tx.push(Command::SetPlaying { deck: 0, playing: true }).unwrap();
        tx.push(Command::SetTempo { deck: 0, ratio: 1.5 }).unwrap();

        let mut out = vec![0f32; 4800 * 2];
        mixer.render(&mut out, 2);
        assert!((position(&shared, 0) - 0.15).abs() < 1e-9, "{}", position(&shared, 0));
        assert!(out.iter().all(|&s| s > 0.0));

        for _ in 0..20 {
            mixer.render(&mut out, 2);
        }
        assert!(!shared.decks[0].playing.load(Ordering::Relaxed));
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn loops_wrap_and_scratch_plays_backwards() {
        let (mut mixer, mut tx, shared) = mixer(1_000);
        let audio = Arc::new(TrackAudio { sample_rate: 1_000, samples: vec![0.1; 10_000 * 2] });
        tx.push(Command::Load { deck: 2, audio }).unwrap();
        tx.push(Command::Seek { deck: 2, seconds: 1.0 }).unwrap();
        tx.push(Command::SetLoop { deck: 2, start: 1.0, end: 1.5 }).unwrap();
        tx.push(Command::SetPlaying { deck: 2, playing: true }).unwrap();

        let mut out = vec![0f32; 1_200 * 2];
        mixer.render(&mut out, 2);
        // 1.2 s of playback inside a 0.5 s loop starting at 1.0 s ends at 1.2 s.
        assert!((position(&shared, 2) - 1.2).abs() < 1e-9, "{}", position(&shared, 2));

        tx.push(Command::ClearLoop { deck: 2 }).unwrap();
        tx.push(Command::SetPlaying { deck: 2, playing: false }).unwrap();
        tx.push(Command::Hold { deck: 2, held: true }).unwrap();
        tx.push(Command::JogMove { deck: 2, seconds: -0.2 }).unwrap();
        let mut out = vec![0f32; 200 * 2];
        mixer.render(&mut out, 2);
        assert!(out.iter().any(|&s| s != 0.0), "scratching must be audible while paused");
        let mut out = vec![0f32; 800 * 2];
        mixer.render(&mut out, 2);
        assert!((position(&shared, 2) - 1.0).abs() < 1e-3, "{}", position(&shared, 2));
    }

    #[test]
    fn jog_steps_become_smooth_motion() {
        let rate = 48_000;
        let (mut mixer, mut tx, shared) = mixer(rate);
        let audio = Arc::new(TrackAudio { sample_rate: rate, samples: vec![0.1; 480_000 * 2] });
        tx.push(Command::Load { deck: 0, audio }).unwrap();
        tx.push(Command::Seek { deck: 0, seconds: 1.0 }).unwrap();
        tx.push(Command::Hold { deck: 0, held: true }).unwrap();
        // Jog steps arriving in bursts once per 16 ms UI frame, about normal speed.
        let mut previous = 1.0;
        let mut speeds = Vec::new();
        for _ in 0..60 {
            tx.push(Command::JogMove { deck: 0, seconds: 0.016 }).unwrap();
            for _ in 0..16 {
                let mut out = vec![0f32; 48 * 2];
                mixer.render(&mut out, 2);
                let now = position(&shared, 0);
                speeds.push((now - previous) / 0.001);
                previous = now;
            }
        }
        // Once moving, the speed stays near 1× instead of jumping per burst.
        for &speed in &speeds[400..] {
            assert!((speed - 1.0).abs() < 0.35, "speed {speed}");
        }
        // Letting go while paused glides to the last jog position and stops.
        tx.push(Command::Hold { deck: 0, held: false }).unwrap();
        let mut out = vec![0f32; 4_800 * 2];
        mixer.render(&mut out, 2);
        assert!((position(&shared, 0) - (1.0 + 60.0 * 0.016)).abs() < 1e-4);
    }

    #[test]
    fn crossfader_assignment_and_thru() {
        let (mut mixer, mut tx, _shared) = mixer(1_000);
        for deck in [0, 3] {
            let audio = Arc::new(TrackAudio { sample_rate: 1_000, samples: vec![0.2; 1_000 * 2] });
            tx.push(Command::Load { deck, audio }).unwrap();
            tx.push(Command::SetPlaying { deck, playing: true }).unwrap();
        }
        // Full left: deck 1 (A) audible, deck 4 (B) silent.
        tx.push(Command::SetCrossfader(-1.0)).unwrap();
        tx.push(Command::SetVolume { deck: 0, level: 0.0 }).unwrap();
        let mut out = vec![0f32; 10 * 2];
        mixer.render(&mut out, 2);
        assert!(out[0].abs() < 1e-6, "deck 4 is on B and must be cut");
        tx.push(Command::SetCrossfaderAssign { deck: 3, side: CrossfaderSide::Thru }).unwrap();
        mixer.render(&mut out, 2);
        assert!(out[0] > 0.0, "deck 4 on thru ignores the crossfader");
    }

    #[test]
    fn headphones_hear_cued_deck_before_fader() {
        let (mut mixer, mut tx, _shared) = mixer(48_000);
        let audio = Arc::new(TrackAudio { sample_rate: 48_000, samples: vec![0.25; 48_000 * 2] });
        tx.push(Command::Load { deck: 1, audio }).unwrap();
        tx.push(Command::SetPlaying { deck: 1, playing: true }).unwrap();
        tx.push(Command::SetVolume { deck: 1, level: 0.0 }).unwrap();
        tx.push(Command::SetHeadphoneCue { deck: 1, on: true }).unwrap();

        let mut out = vec![0f32; 64 * 4];
        mixer.render(&mut out, 4);
        let frame = &out[60 * 4..61 * 4];
        assert_eq!(&frame[..2], &[0.0, 0.0], "fader is down, master must be silent");
        assert!(frame[2] > 0.0 && frame[3] > 0.0, "cue must reach the headphones");
    }
    #[test]
    fn slip_returns_to_where_the_track_would_be() {
        let (mut mixer, mut tx, shared) = mixer(1_000);
        let audio = Arc::new(TrackAudio { sample_rate: 1_000, samples: vec![0.1; 20_000 * 2] });
        tx.push(Command::Load { deck: 0, audio }).unwrap();
        tx.push(Command::SetSlip { deck: 0, on: true }).unwrap();
        tx.push(Command::SetPlaying { deck: 0, playing: true }).unwrap();
        let mut out = vec![0f32; 1_000 * 2];
        mixer.render(&mut out, 2);
        // A half-second loop for two seconds, then exit: playback continues at 3 s.
        tx.push(Command::SetLoop { deck: 0, start: 1.0, end: 1.5 }).unwrap();
        tx.push(Command::Seek { deck: 0, seconds: 1.0 }).unwrap();
        for _ in 0..2 {
            mixer.render(&mut out, 2);
        }
        assert!(position(&shared, 0) < 1.5);
        tx.push(Command::ClearLoop { deck: 0 }).unwrap();
        mixer.render(&mut out[..2], 2);
        assert!((position(&shared, 0) - 3.0).abs() < 0.01, "{}", position(&shared, 0));
    }

    #[test]
    fn sampler_plays_once_and_master_cue_reaches_headphones() {
        let (mut mixer, mut tx, shared) = mixer(1_000);
        let audio = Arc::new(TrackAudio { sample_rate: 1_000, samples: vec![0.5; 100 * 2] });
        tx.push(Command::SamplerLoad { slot: 3, audio }).unwrap();
        tx.push(Command::SamplerPlay { slot: 3 }).unwrap();
        tx.push(Command::SetMasterCue(true)).unwrap();
        let mut out = vec![0f32; 50 * 4];
        mixer.render(&mut out, 4);
        assert!(out[0] > 0.0 && out[2] > 0.0, "sample on master and, with master cue, headphones");
        assert!(shared.sampler_playing[3].load(Ordering::Relaxed));
        let mut out = vec![0f32; 100 * 4];
        mixer.render(&mut out, 4);
        assert_eq!(out[99 * 4], 0.0, "played once");
        assert!(!shared.sampler_playing[3].load(Ordering::Relaxed));
    }

    #[test]
    fn beat_fx_on_a_channel_leaves_the_others_dry() {
        let (mut mixer, mut tx, _shared) = mixer(1_000);
        for deck in [0, 1] {
            let audio = Arc::new(TrackAudio { sample_rate: 1_000, samples: vec![0.2; 10_000 * 2] });
            tx.push(Command::Load { deck, audio }).unwrap();
            tx.push(Command::SetPlaying { deck, playing: true }).unwrap();
        }
        // Trans fully cuts the second half of each beat, on channel 2 only.
        tx.push(Command::SetBeatFx(BeatFxParams {
            kind: crate::fx::BeatFxKind::Trans,
            on: true,
            depth: 1.0,
            beat_secs: 0.1,
            target: FxTarget::Channel(1),
            release: false,
        }))
        .unwrap();
        let mut out = vec![0f32; 100 * 2];
        mixer.render(&mut out, 2);
        let (first, second) = (out[25 * 2], out[75 * 2]);
        assert!(first > second && second > 0.0, "{first} {second}");
    }

    #[test]
    fn crossfader_and_fader_curves() {
        let [a, b] = crossfader_gains(0.0, CrossfaderCurve::Smooth);
        assert!((a - b).abs() < 1e-6 && (a * a + b * b - 1.0).abs() < 1e-5, "constant power");
        assert_eq!(crossfader_gains(0.85, CrossfaderCurve::Sharp)[0], 1.0, "sharp stays full");
        assert!(crossfader_gains(1.0, CrossfaderCurve::Sharp)[0] < 1e-6);
        let half = |c| fader_curve_gain(0.5, c);
        assert!(half(FaderCurve::Slow) < half(FaderCurve::Standard));
        assert!(half(FaderCurve::Standard) < half(FaderCurve::Fast));
    }

    #[test]
    fn vinyl_start_and_brake_take_time() {
        let (mut mixer, mut tx, shared) = mixer(1_000);
        let audio = Arc::new(TrackAudio { sample_rate: 1_000, samples: vec![0.1; 10_000 * 2] });
        tx.push(Command::Load { deck: 0, audio }).unwrap();
        tx.push(Command::SetVinylSpeed { deck: 0, start: 0.5, brake: 1.0 }).unwrap();
        tx.push(Command::SetPlaying { deck: 0, playing: true }).unwrap();
        let mut out = vec![0f32; 500 * 2];
        mixer.render(&mut out, 2);
        // Speeding up linearly over 0.5 s covers a quarter second of audio.
        assert!((position(&shared, 0) - 0.25).abs() < 0.01, "{}", position(&shared, 0));
        mixer.render(&mut out, 2);
        tx.push(Command::SetPlaying { deck: 0, playing: false }).unwrap();
        let before = position(&shared, 0);
        let mut out = vec![0f32; 2_000 * 2];
        mixer.render(&mut out, 2);
        // Braking from full speed over 1 s moves half a second, then stops.
        assert!(
            (position(&shared, 0) - before - 0.5).abs() < 0.01,
            "{}",
            position(&shared, 0) - before
        );
        assert!(!shared.decks[0].playing.load(Ordering::Relaxed));
    }
}
