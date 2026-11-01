//! The real-time mixer and the audio output stream.
//!
//! The audio callback never blocks or allocates: the UI sends [`Command`]s
//! through a lock-free queue, reads deck state from atomics, and receives
//! replaced track buffers back for dropping outside the audio thread.

use std::f32::consts::FRAC_PI_2;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

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
/// The screen guesses the position at most this far past the last buffer.
const MAX_EXTRAPOLATION_SECS: f64 = 0.05;

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
    /// EQ or isolator, and its frequencies, for every channel.
    SetEqSettings(crate::eq::EqSettings),
    /// Jog pitch bend strength (1.0 = a quarter faster per turn per second).
    SetBendSensitivity(f64),
    /// Jog steps the controller sends per turn of a wheel.
    SetJogResolution(f64),
    SetCrossfaderCurve(CrossfaderCurve),
    /// Starts (Some) or ends (None) taking the master into a recording.
    Record(Option<crate::record::RecordTap>),
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
    /// How fast the position moved over the last buffer (track seconds per
    /// second) and when it was published (nanoseconds since [`Shared::epoch`]),
    /// so the screen can tell where the record is between buffers.
    speed: AtomicU64,
    published_nanos: AtomicU64,
    playing: AtomicBool,
    /// The jog is bending the tempo right now.
    bending: AtomicBool,
    peak: AtomicU32,
}

#[derive(Debug)]
struct Shared {
    decks: [DeckMeter; DECKS],
    master_peak: AtomicU32,
    sampler_playing: [AtomicBool; SAMPLER_SLOTS],
    epoch: Instant,
}

// Arrays this long have no derived `Default`.
impl Default for Shared {
    fn default() -> Self {
        Self {
            decks: Default::default(),
            master_peak: AtomicU32::default(),
            sampler_playing: std::array::from_fn(|_| AtomicBool::default()),
            epoch: Instant::now(),
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

/// Jog movement sent straight from the MIDI thread: (deck, steps).
pub type JogEvent = (u8, i16);

/// Sends jog movement to the audio thread without going through the UI.
pub struct JogSender(rtrb::Producer<JogEvent>);

impl JogSender {
    /// Never blocks; drops the event if the queue is full.
    pub fn send(&mut self, deck: usize, steps: i32) {
        let _ = self
            .0
            .push((deck as u8, steps.clamp(-i32::from(i16::MAX), i32::from(i16::MAX)) as i16));
    }
}

/// Jog steps in one turn of the wheel until [`Command::SetJogResolution`]
/// says otherwise: 720 on most Pioneer DDJ controllers.
const DEFAULT_JOG_STEPS_PER_TURN: f64 = 720.0;
/// Pitch bend when spinning one turn per second, at sensitivity 1.
const BEND_PER_TURN_PER_SECOND: f64 = 0.25;
/// Jog speed is measured over this much recent audio time.
const JOG_WINDOW_SECS: f64 = 0.04;
/// Audio moved by one turn of the jog: like a 33⅓ rpm record, 1.8 s.
const JOG_SECONDS_PER_TURN: f64 = 1.8;
/// How far a held or gliding record may trail the jog, in seconds at the
/// fastest scratch speed. Movement beyond that is jumped over, so the track
/// never keeps running after the wheel has stopped.
const MAX_JOG_LAG_SECS: f64 = 0.03;
/// After letting go of a scratched record, the track follows the coasting
/// platter until it has sent no movement for this long.
const COAST_SECS: f64 = 0.06;
/// Recent jog events kept per deck.
const JOG_EVENTS: usize = 64;

pub struct Engine {
    jog: Option<JogSender>,
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
        let (jog_tx, jog_rx) = rtrb::RingBuffer::new(QUEUE);
        let shared = Arc::new(Shared::default());
        let mut mixer = Mixer::new(info.sample_rate, command_rx, trash_tx, shared.clone());
        mixer.jog = Some(jog_rx);
        let _ = commands.push(Command::SetQuality(options.quality));
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, mixer)?,
            SampleFormat::I16 => build::<i16>(&device, config, mixer)?,
            SampleFormat::I32 => build::<i32>(&device, config, mixer)?,
            other => return Err(anyhow!("unsupported sample format {other}")),
        };
        stream.play()?;
        Ok(Self { jog: Some(JogSender(jog_tx)), commands, trash, shared, info, _stream: stream })
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

    /// Where the record is right now, for drawing: the last published
    /// position moved on by its speed since then, so waveforms glide
    /// smoothly however large the audio buffer is.
    pub fn display_position_secs(&self, deck: usize) -> f64 {
        let meter = &self.shared.decks[deck];
        let speed = f64::from_bits(meter.speed.load(Ordering::Relaxed));
        let published = meter.published_nanos.load(Ordering::Relaxed);
        let now = self.shared.epoch.elapsed().as_nanos() as u64;
        let since = now.saturating_sub(published) as f64 * 1e-9;
        self.position_secs(deck) + speed * since.min(MAX_EXTRAPOLATION_SECS)
    }

    /// The record is moving (playing, scratched or gliding to a jog position).
    pub fn is_moving(&self, deck: usize) -> bool {
        f64::from_bits(self.shared.decks[deck].speed.load(Ordering::Relaxed)) != 0.0
    }

    pub fn is_playing(&self, deck: usize) -> bool {
        self.shared.decks[deck].playing.load(Ordering::Relaxed)
    }

    /// Recent peak level of a deck after its fader, 0.0–1.0+.
    pub fn deck_peak(&self, deck: usize) -> f32 {
        load_f32(&self.shared.decks[deck].peak)
    }

    /// Starts recording the master to a 24-bit WAV at `path`.
    pub fn start_recording(&mut self, path: &std::path::Path) -> Result<crate::record::Recording> {
        let (recording, tap) = crate::record::Recording::start(path, self.info.sample_rate)?;
        if self.commands.push(Command::Record(Some(tap))).is_err() {
            return Err(anyhow!("the audio engine is busy; try again"));
        }
        Ok(recording)
    }

    /// Stops recording and finishes the file.
    pub fn stop_recording(
        &mut self,
        recording: crate::record::Recording,
    ) -> Result<std::path::PathBuf> {
        // Retry briefly if the command queue is momentarily full.
        for _ in 0..100 {
            if self.commands.push(Command::Record(None)).is_ok() {
                return recording.finish();
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Err(anyhow!("could not stop the recording"))
    }

    /// The sender for jog movement from the MIDI thread (once per engine).
    pub fn take_jog_sender(&mut self) -> Option<JogSender> {
        self.jog.take()
    }

    /// The jog is moving this deck right now: bending its tempo, or the
    /// released record following the coasting platter.
    pub fn is_bending(&self, deck: usize) -> bool {
        self.shared.decks[deck].bending.load(Ordering::Relaxed)
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
    /// Recent jog movement as (sample clock, steps), and the bend it makes.
    jog_events: [(u64, f32); JOG_EVENTS],
    jog_next: usize,
    jog_bend: f64,
    /// Let go of after scratching, the platter still spinning: playback
    /// follows the jog until it stops.
    coasting: bool,
    /// Sample clock of the last jog movement.
    last_jog: u64,
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
    /// Jog movement from the MIDI thread.
    jog: Option<rtrb::Consumer<JogEvent>>,
    /// Frames rendered so far: the clock jog speed is measured against.
    clock: u64,
    bend_sensitivity: f64,
    jog_steps_per_turn: f64,
    quality: Quality,
    decks: [DeckState; DECKS],
    crossfader: f32,
    master: f32,
    headphone_mix: f32,
    headphone_volume: f32,
    master_peak: f32,
    master_cue: bool,
    recorder: Option<crate::record::RecordTap>,
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

/// How loud a channel reaches the mix from its fader and crossfader
/// (0 = silent, 1 = full), as the engine applies them; trim and EQ aside.
pub fn channel_gain(
    volume: f32,
    side: CrossfaderSide,
    crossfader: f32,
    crossfader_curve: CrossfaderCurve,
    fader_curve: FaderCurve,
) -> f32 {
    let [a, b] = crossfader_gains(crossfader, crossfader_curve);
    let side_gain = match side {
        CrossfaderSide::A => a,
        CrossfaderSide::B => b,
        CrossfaderSide::Thru => 1.0,
    };
    fader_curve_gain(volume, fader_curve) * side_gain
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
            jog_events: [(0, 0.0); JOG_EVENTS],
            jog_next: 0,
            jog_bend: 0.0,
            coasting: false,
            last_jog: 0,
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
            jog: None,
            clock: 0,
            bend_sensitivity: 1.0,
            jog_steps_per_turn: DEFAULT_JOG_STEPS_PER_TURN,
            rate: f64::from(rate),
            quality: Quality::default(),
            decks: std::array::from_fn(deck),
            crossfader: 0.0,
            master: 0.8,
            headphone_mix: 0.0,
            headphone_volume: 0.8,
            master_peak: 0.0,
            master_cue: false,
            recorder: None,
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
                (d.velocity, d.braking, d.coasting) = (0.0, false, false);
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
                if d.playing && !d.held && !d.coasting {
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
                let was_held = d.held;
                d.held = held && d.audio.is_some();
                if d.held {
                    d.coasting = false;
                    d.target.get_or_insert(d.position);
                } else if was_held {
                    // Let go of the record: while the platter still spins on,
                    // the track keeps following it (apply_jog ends this).
                    d.coasting = true;
                    d.last_jog = self.clock;
                }
            }
            Command::JogMove { deck, seconds } => {
                let d = &mut self.decks[deck];
                if let Some(audio) = &d.audio
                    && (d.held || d.coasting || !d.playing)
                {
                    let rate = f64::from(audio.sample_rate);
                    let end = audio.frames() as f64;
                    let target = (d.target.unwrap_or(d.position) + seconds * rate).clamp(0.0, end);
                    let lag = MAX_SCRATCH_SPEED * MAX_JOG_LAG_SECS * rate;
                    d.position = d.position.clamp(target - lag, target + lag);
                    d.target = Some(target);
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
            Command::SetEqSettings(settings) => {
                for deck in &mut self.decks {
                    deck.eq.configure(settings);
                }
            }
            Command::SetBendSensitivity(s) => self.bend_sensitivity = s.clamp(0.05, 10.0),
            Command::SetJogResolution(steps) => {
                self.jog_steps_per_turn =
                    if steps >= 1.0 { steps } else { DEFAULT_JOG_STEPS_PER_TURN };
            }
            Command::SetCrossfaderCurve(curve) => self.crossfader_curve = curve,
            Command::Record(tap) => {
                // Dropping the old tap (an Arc) frees nothing large here: the
                // writer still holds the buffer.
                self.recorder = tap;
            }
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

    /// Takes jog movement from the MIDI thread: on a playing deck it bends
    /// the tempo by how fast the jog spins (measured in audio time, updated
    /// every buffer); on a paused or held deck it moves the record.
    fn apply_jog(&mut self, frames: usize) {
        let now = self.clock;
        self.clock += frames as u64;
        while let Some(Ok((deck, steps))) = self.jog.as_mut().map(rtrb::Consumer::pop) {
            let Some(d) = self.decks.get_mut(usize::from(deck)) else { continue };
            d.last_jog = now;
            if d.playing && !d.held && !d.coasting {
                d.jog_events[d.jog_next] = (now, f32::from(steps));
                d.jog_next = (d.jog_next + 1) % JOG_EVENTS;
            } else {
                let seconds = f64::from(steps) * JOG_SECONDS_PER_TURN / self.jog_steps_per_turn;
                self.apply(Command::JogMove { deck: usize::from(deck), seconds });
            }
        }
        let window = (JOG_WINDOW_SECS * self.rate) as u64;
        let block = frames as f64 / self.rate;
        let coast_end = (COAST_SECS * self.rate) as u64;
        for d in &mut self.decks {
            // The released platter stopped: back to normal playback.
            if d.coasting && now.saturating_sub(d.last_jog) > coast_end {
                d.coasting = false;
                if d.playing {
                    d.target = None;
                }
            }
            let recent =
                d.jog_events.iter().filter(|(t, s)| *s != 0.0 && now.saturating_sub(*t) <= window);
            let (count, steps) =
                recent.fold((0, 0.0f64), |(n, sum), (_, s)| (n + 1, sum + f64::from(*s)));
            let target = if count > 0 {
                steps / JOG_WINDOW_SECS / self.jog_steps_per_turn
                    * BEND_PER_TURN_PER_SECOND
                    * self.bend_sensitivity
            } else {
                0.0
            };
            // Quick to follow the hand, quicker still to stop with it.
            let settle = if count > 0 { 0.012 } else { 0.006 };
            d.jog_bend += (target.clamp(-0.6, 0.6) - d.jog_bend) * (block / settle).min(1.0);
            if count == 0 && d.jog_bend.abs() < 0.001 {
                d.jog_bend = 0.0;
            }
        }
    }

    fn render<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        while let Ok(command) = self.commands.pop() {
            self.apply(command);
        }
        self.apply_jog(out.len() / channels.max(1));
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
                    && (deck.held || deck.coasting || deck.loop_frames.is_some() || deck.slip_hold);
                if deck.was_slipping && !slipping && deck.slip && deck.playing {
                    deck.position = deck.shadow;
                }
                if slipping {
                    deck.shadow += deck.tempo * (1.0 + deck.nudge + deck.jog_bend) * step;
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
                    if settled && !deck.held && !deck.coasting {
                        deck.target = None;
                        deck.velocity = 0.0;
                        continue;
                    }
                } else if deck.playing {
                    let speed = deck.tempo * (1.0 + deck.nudge + deck.jog_bend) * step;
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
            if let Some(recorder) = self.recorder.as_mut() {
                recorder.push(master);
            }
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

        let frames = out.len() / channels.max(1);
        let buffer_secs = frames as f64 / self.rate;
        let now = self.shared.epoch.elapsed().as_nanos() as u64;
        for (deck, meter) in self.decks.iter_mut().zip(&self.shared.decks) {
            let rate = deck.audio.as_ref().map_or(1.0, |a| f64::from(a.sample_rate));
            let position = deck.position / rate;
            let previous = f64::from_bits(meter.position_secs.load(Ordering::Relaxed));
            // Jumps (cues, loops, seeks) are not movement to carry on with.
            let speed = (position - previous) / buffer_secs.max(1e-6);
            let speed = if speed.abs() <= MAX_SCRATCH_SPEED * 1.5 { speed } else { 0.0 };
            store_f64(&meter.speed, speed);
            meter.published_nanos.store(now, Ordering::Relaxed);
            store_f64(&meter.position_secs, position);
            meter.playing.store(deck.playing, Ordering::Relaxed);
            meter.bending.store(deck.jog_bend != 0.0 || deck.coasting, Ordering::Relaxed);
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
    #[test]
    fn channel_gain_follows_fader_and_crossfader() {
        use CrossfaderCurve as X;
        use FaderCurve as F;
        assert_eq!(channel_gain(1.0, CrossfaderSide::Thru, 1.0, X::Standard, F::Standard), 1.0);
        assert_eq!(channel_gain(0.0, CrossfaderSide::A, 0.0, X::Standard, F::Standard), 0.0);
        // Side A with the crossfader all the way to B is cut.
        assert!(channel_gain(1.0, CrossfaderSide::A, 1.0, X::Standard, F::Standard) < 1e-6);
        // Half fader on the standard curve is a quarter.
        assert!(
            (channel_gain(0.5, CrossfaderSide::B, 1.0, X::Standard, F::Standard) - 0.25).abs()
                < 1e-6
        );
    }

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

    #[test]
    fn jog_bend_follows_the_wheel_and_stops_with_it() {
        let rate = 48_000;
        let (mut mixer, mut tx, shared) = mixer(rate);
        let (jog_tx, jog_rx) = rtrb::RingBuffer::new(256);
        mixer.jog = Some(jog_rx);
        let mut jog = JogSender(jog_tx);
        let audio = Arc::new(TrackAudio { sample_rate: rate, samples: vec![0.1; 480_000 * 2] });
        tx.push(Command::Load { deck: 0, audio }).unwrap();
        tx.push(Command::SetPlaying { deck: 0, playing: true }).unwrap();
        let mut out = vec![0f32; 256 * 2];
        mixer.render(&mut out, 2);
        // About 1,000 steps a second (5 per 256-frame block): 1.39 turns a second.
        for _ in 0..40 {
            jog.send(0, 5);
            mixer.render(&mut out, 2);
        }
        let bend = mixer.decks[0].jog_bend;
        let expected =
            5.0 / (256.0 / 48_000.0) / DEFAULT_JOG_STEPS_PER_TURN * BEND_PER_TURN_PER_SECOND;
        assert!((bend - expected).abs() < 0.04, "bend {bend}, expected {expected}");
        assert!(shared.decks[0].bending.load(Ordering::Relaxed));
        // Within ~60 ms of the wheel stopping, the bend is gone.
        for _ in 0..12 {
            mixer.render(&mut out, 2);
        }
        assert_eq!(mixer.decks[0].jog_bend, 0.0);
    }

    #[test]
    fn held_deck_stops_and_only_moves_with_the_drag() {
        let rate = 48_000;
        let (mut mixer, mut tx, shared) = mixer(rate);
        let audio = Arc::new(TrackAudio { sample_rate: rate, samples: vec![0.1; 480_000 * 2] });
        tx.push(Command::Load { deck: 0, audio }).unwrap();
        tx.push(Command::Seek { deck: 0, seconds: 1.0 }).unwrap();
        tx.push(Command::SetPlaying { deck: 0, playing: true }).unwrap();
        let mut out = vec![0f32; 256 * 2];
        mixer.render(&mut out, 2);
        let grabbed_at = position(&shared, 0);
        tx.push(Command::Hold { deck: 0, held: true }).unwrap();
        // Stops at once, a few milliseconds of braking from where it was held.
        for _ in 0..40 {
            mixer.render(&mut out, 2);
        }
        let held_at = position(&shared, 0);
        assert!((held_at - grabbed_at).abs() < 0.03, "{held_at} from {grabbed_at}");
        // And stays there, however long the hand does.
        for _ in 0..200 {
            mixer.render(&mut out, 2);
        }
        assert!((position(&shared, 0) - held_at).abs() < 0.001);
        // Dragged back, it goes there and stays: nothing pulls it forward.
        tx.push(Command::JogMove { deck: 0, seconds: -0.5 }).unwrap();
        for _ in 0..400 {
            mixer.render(&mut out, 2);
        }
        let dragged_to = position(&shared, 0);
        assert!((dragged_to - (held_at - 0.5)).abs() < 0.01, "at {dragged_to}");
        for _ in 0..200 {
            mixer.render(&mut out, 2);
        }
        assert!((position(&shared, 0) - dragged_to).abs() < 0.001);
        // Let go: it plays on from there.
        tx.push(Command::Hold { deck: 0, held: false }).unwrap();
        for _ in 0..40 {
            mixer.render(&mut out, 2);
        }
        let before = position(&shared, 0);
        assert!((before - dragged_to).abs() < 0.3);
        mixer.render(&mut out, 2);
        assert!(position(&shared, 0) > before);
    }

    #[test]
    fn released_record_follows_the_coasting_platter() {
        let rate = 48_000;
        let (mut mixer, mut tx, shared) = mixer(rate);
        let (jog_tx, jog_rx) = rtrb::RingBuffer::new(256);
        mixer.jog = Some(jog_rx);
        let mut jog = JogSender(jog_tx);
        let audio = Arc::new(TrackAudio { sample_rate: rate, samples: vec![0.1; 480_000 * 2] });
        tx.push(Command::Load { deck: 0, audio }).unwrap();
        tx.push(Command::Seek { deck: 0, seconds: 1.0 }).unwrap();
        tx.push(Command::SetPlaying { deck: 0, playing: true }).unwrap();
        tx.push(Command::Hold { deck: 0, held: true }).unwrap();
        let mut out = vec![0f32; 256 * 2];
        mixer.render(&mut out, 2);
        let held_at = position(&shared, 0);
        // Let go while the platter spins on: its movement still moves the record.
        tx.push(Command::Hold { deck: 0, held: false }).unwrap();
        for _ in 0..20 {
            jog.send(0, -4);
            mixer.render(&mut out, 2);
        }
        assert!(mixer.decks[0].coasting);
        assert!(position(&shared, 0) < held_at, "follows the platter backwards");
        assert_eq!(mixer.decks[0].jog_bend, 0.0, "not a pitch bend");
        // The platter stops: playback carries on forwards.
        for _ in 0..40 {
            mixer.render(&mut out, 2);
        }
        assert!(!mixer.decks[0].coasting);
        let before = position(&shared, 0);
        mixer.render(&mut out, 2);
        assert!(position(&shared, 0) > before);
    }

    #[test]
    fn publishes_how_fast_the_record_moves() {
        let rate = 48_000;
        let (mut mixer, mut tx, shared) = mixer(rate);
        let audio = Arc::new(TrackAudio { sample_rate: rate, samples: vec![0.1; 480_000 * 2] });
        tx.push(Command::Load { deck: 0, audio }).unwrap();
        let mut out = vec![0f32; 256 * 2];
        let speed = |shared: &Shared| f64::from_bits(shared.decks[0].speed.load(Ordering::Relaxed));
        mixer.render(&mut out, 2);
        assert_eq!(speed(&shared), 0.0, "paused");
        tx.push(Command::SetPlaying { deck: 0, playing: true }).unwrap();
        mixer.render(&mut out, 2);
        mixer.render(&mut out, 2);
        assert!((speed(&shared) - 1.0).abs() < 1e-6, "speed {}", speed(&shared));
        // A jump is not movement to carry on with.
        tx.push(Command::Seek { deck: 0, seconds: 5.0 }).unwrap();
        mixer.render(&mut out, 2);
        assert_eq!(speed(&shared), 0.0);
    }

    #[test]
    fn record_stops_with_a_wheel_spun_faster_than_it_can_play() {
        let rate = 48_000;
        let (mut mixer, mut tx, shared) = mixer(rate);
        let (jog_tx, jog_rx) = rtrb::RingBuffer::new(256);
        mixer.jog = Some(jog_rx);
        let mut jog = JogSender(jog_tx);
        let audio = Arc::new(TrackAudio { sample_rate: rate, samples: vec![0.1; 4_800_000 * 2] });
        tx.push(Command::Load { deck: 0, audio }).unwrap();
        tx.push(Command::SetJogResolution(6480.0)).unwrap();
        tx.push(Command::Hold { deck: 0, held: true }).unwrap();
        let mut out = vec![0f32; 256 * 2];
        // Ten turns a second for a quarter second: 18 s of record a second,
        // far beyond the fastest scratch.
        for _ in 0..47 {
            jog.send(0, 64 * 54 / 25);
            mixer.render(&mut out, 2);
        }
        let wheel = 47.0 * f64::from(64 * 54 / 25) * 1.8 / 6480.0;
        // Shortly after the wheel stops, so does the record, where the wheel left it.
        for _ in 0..10 {
            mixer.render(&mut out, 2);
        }
        let stopped = position(&shared, 0);
        assert!((stopped - wheel).abs() < 0.01, "at {stopped}, wheel at {wheel}");
        for _ in 0..40 {
            mixer.render(&mut out, 2);
        }
        let settled = position(&shared, 0);
        assert!((settled - wheel).abs() < 1e-4, "settled at {settled}, wheel at {wheel}");
        for _ in 0..40 {
            mixer.render(&mut out, 2);
        }
        let after = position(&shared, 0);
        assert!((after - settled).abs() < 1e-6, "moved on from {settled} to {after}");
    }
}
