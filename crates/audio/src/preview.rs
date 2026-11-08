//! Listening to a session (EDIT mode): the mixer plays it offline, ahead
//! of time, into a queue that an output stream of its own plays from.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::engine::Engine;
use crate::session::Session;

/// How much is played ahead of what is heard, in seconds.
const AHEAD_SECS: f64 = 0.5;

/// A session playing from a moment on. Dropping it stops it.
pub struct SessionPlayer {
    shared: Arc<Shared>,
    _stream: cpal::Stream,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    /// Frames from the session's start heard so far.
    heard: AtomicU64,
    /// The mixer has caught up with where playing starts.
    ready: AtomicBool,
    /// Everything has been played into the queue.
    rendered: AtomicBool,
    /// …and heard.
    drained: AtomicBool,
    error: Mutex<Option<String>>,
}

impl SessionPlayer {
    /// Plays `session` from `from` (frames from its start) on `device`
    /// (the system's default if `None`). The mixer plays everything before
    /// `from` too, unheard, so it can take a moment to start.
    pub fn start(session: Session, from: u64, device: Option<&str>) -> Result<Self> {
        let rate = session.sample_rate();
        let device = Engine::find_device(device)?;
        let default =
            device.default_output_config().context("reading the device's output format")?;
        let usable = |f: SampleFormat| {
            matches!(f, SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32)
        };
        // At the session's rate when the device can; else resampled.
        let config = device
            .supported_output_configs()?
            .filter(|c| usable(c.sample_format()) && c.channels() >= 2)
            .filter_map(|c| c.try_with_sample_rate(rate))
            .max_by_key(|c| c.sample_format() == default.sample_format())
            .unwrap_or(default);
        let format = config.sample_format();
        let config: StreamConfig = config.into();
        let (producer, consumer) =
            rtrb::RingBuffer::<[f32; 2]>::new((AHEAD_SECS * f64::from(rate)) as usize);
        let shared = Arc::new(Shared::default());
        shared.heard.store(from, Ordering::Relaxed);
        let output = Output {
            queue: consumer,
            shared: shared.clone(),
            step: f64::from(rate) / f64::from(config.sample_rate),
            phase: 0.0,
            frames: [[0.0; 2]; 2],
            channels: usize::from(config.channels),
        };
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, output)?,
            SampleFormat::I16 => build::<i16>(&device, config, output)?,
            SampleFormat::I32 => build::<i32>(&device, config, output)?,
            other => bail!("unsupported sample format {other}"),
        };
        let mixing = shared.clone();
        std::thread::Builder::new()
            .name("session player".into())
            .spawn(move || {
                if let Err(err) = play_into(&session, from, producer, &mixing)
                    && !mixing.stop.load(Ordering::Relaxed)
                {
                    *mixing.error.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("{err:#}"));
                }
                mixing.rendered.store(true, Ordering::Relaxed);
            })
            .context("starting the session player")?;
        stream.play()?;
        Ok(Self { shared, _stream: stream })
    }

    /// Where it is, in frames from the session's start.
    pub fn position(&self) -> u64 {
        self.shared.heard.load(Ordering::Relaxed)
    }

    /// Playing has caught up with where it started.
    pub fn ready(&self) -> bool {
        self.shared.ready.load(Ordering::Relaxed)
    }

    /// Everything has been heard (or it failed).
    pub fn finished(&self) -> bool {
        let rendered = self.shared.rendered.load(Ordering::Relaxed);
        rendered && (!self.ready() || self.shared.drained.load(Ordering::Relaxed))
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().ok()?.clone()
    }
}

impl Drop for SessionPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Plays the session into the queue, dropping what comes before `from`.
fn play_into(
    session: &Session,
    from: u64,
    mut queue: rtrb::Producer<[f32; 2]>,
    shared: &Shared,
) -> Result<()> {
    let mut clock = 0u64;
    let stopped = || anyhow!("stopped");
    session.render(|buffer| {
        if shared.stop.load(Ordering::Relaxed) {
            return Err(stopped());
        }
        let frames = buffer.len() as u64 / 2;
        let skip = from.saturating_sub(clock).min(frames) as usize;
        clock += frames;
        if skip as u64 == frames {
            return Ok(());
        }
        shared.ready.store(true, Ordering::Relaxed);
        for frame in buffer[skip * 2..].chunks_exact(2) {
            let mut frame = [frame[0], frame[1]];
            loop {
                match queue.push(frame) {
                    Ok(()) => break,
                    Err(rtrb::PushError::Full(back)) => {
                        frame = back;
                        if shared.stop.load(Ordering::Relaxed) {
                            return Err(stopped());
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// The audio callback's side: takes frames from the queue, resampling when
/// the device runs at another rate.
struct Output {
    queue: rtrb::Consumer<[f32; 2]>,
    shared: Arc<Shared>,
    /// Session frames per device frame.
    step: f64,
    /// How far between `frames[0]` and `frames[1]`.
    phase: f64,
    frames: [[f32; 2]; 2],
    channels: usize,
}

impl Output {
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T]) {
        let silence = T::from_sample(0.0);
        if !self.shared.ready.load(Ordering::Relaxed) {
            data.fill(silence);
            return;
        }
        for frame in data.chunks_mut(self.channels) {
            self.phase += self.step;
            while self.phase >= 1.0 {
                self.phase -= 1.0;
                match self.queue.pop() {
                    Ok(next) => {
                        self.frames = [self.frames[1], next];
                        self.shared.heard.fetch_add(1, Ordering::Relaxed);
                    }
                    // Not ready yet, or the end: silence.
                    Err(_) => {
                        self.frames = [self.frames[1], [0.0; 2]];
                        if self.shared.rendered.load(Ordering::Relaxed) {
                            self.shared.drained.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
            let t = self.phase as f32;
            let [a, b] = self.frames;
            let out = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = match channel {
                    0 | 1 => T::from_sample(out[channel]),
                    _ => silence,
                };
            }
        }
    }
}

fn build<T>(device: &cpal::Device, config: StreamConfig, mut output: Output) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], _| output.fill(data),
        |err| eprintln!("session player stream error: {err}"),
        None,
    )?)
}
