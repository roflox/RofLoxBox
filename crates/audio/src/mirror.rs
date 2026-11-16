//! The master heard on a second output too (the computer's speakers while
//! the controller is the output): the audio thread hands its master frames
//! to a queue that another device's stream plays from. Two devices never
//! run at quite the same rate, so the mirror keeps a little audio queued
//! and plays a touch faster or slower to keep it there.

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::engine::Engine;

/// How much the mirror keeps queued: its delay behind the main output.
const TARGET_SECS: f64 = 0.04;
/// Room in the queue, in seconds.
const QUEUE_SECS: f64 = 0.5;
/// How far the mirror's speed may move from the rate's ratio to keep its
/// queue where it should be (0.5 %: not heard).
const MAX_CORRECTION: f64 = 0.005;

/// The audio thread's end: master frames go in, never blocking.
pub struct MirrorTap {
    queue: rtrb::Producer<[f32; 2]>,
}

impl std::fmt::Debug for MirrorTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MirrorTap")
    }
}

impl MirrorTap {
    /// One master frame (dropped if the mirror has fallen behind).
    pub(crate) fn push(&mut self, frame: [f32; 2]) {
        let _ = self.queue.push(frame);
    }
}

/// The second output playing; dropping it stops it.
pub struct Mirror {
    _stream: cpal::Stream,
    /// The device it plays on.
    pub device: String,
}

impl Mirror {
    /// Opens `device` (the system's default if `None`) to play the master
    /// of a mixer running at `rate`; the tap goes to the mixer.
    pub fn start(device: Option<&str>, rate: u32) -> Result<(Self, MirrorTap)> {
        let device = Engine::find_device(device)?;
        let name =
            device.description().map(|d| d.name().to_owned()).unwrap_or_else(|_| "unknown".into());
        let default =
            device.default_output_config().context("reading the device's output format")?;
        let usable = |f: SampleFormat| matches!(f, SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32);
        // At the mixer's rate when the device can; else resampled.
        let config = device
            .supported_output_configs()?
            .filter(|c| usable(c.sample_format()) && c.channels() >= 2)
            .filter_map(|c| c.try_with_sample_rate(rate))
            .max_by_key(|c| c.sample_format() == default.sample_format())
            .unwrap_or(default);
        let format = config.sample_format();
        let config: StreamConfig = config.into();
        let (producer, consumer) = rtrb::RingBuffer::new((QUEUE_SECS * f64::from(rate)) as usize);
        let output = Output {
            queue: consumer,
            ratio: f64::from(rate) / f64::from(config.sample_rate),
            target: TARGET_SECS * f64::from(rate),
            filling: true,
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
        stream.play()?;
        Ok((Self { _stream: stream, device: name }, MirrorTap { queue: producer }))
    }
}

/// The mirror's audio callback: plays from the queue, resampled, at a speed
/// that keeps [`TARGET_SECS`] queued.
struct Output {
    queue: rtrb::Consumer<[f32; 2]>,
    /// Mixer frames per device frame.
    ratio: f64,
    /// Frames to keep queued.
    target: f64,
    /// Waiting for the queue to fill (at the start, or after running dry).
    filling: bool,
    /// How far between `frames[0]` and `frames[1]`.
    phase: f64,
    frames: [[f32; 2]; 2],
    channels: usize,
}

impl Output {
    /// How fast to play, given how much is queued: a little faster with
    /// more than the target, slower with less.
    fn step(&self, queued: f64) -> f64 {
        let off = ((queued - self.target) / self.target).clamp(-1.0, 1.0);
        self.ratio * (1.0 + off * MAX_CORRECTION)
    }

    fn fill<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T]) {
        let silence = T::from_sample(0.0);
        let queued = self.queue.slots() as f64;
        if self.filling {
            if queued < self.target {
                data.fill(silence);
                return;
            }
            self.filling = false;
        }
        let step = self.step(queued);
        for frame in data.chunks_mut(self.channels) {
            self.phase += step;
            while self.phase >= 1.0 {
                self.phase -= 1.0;
                match self.queue.pop() {
                    Ok(next) => self.frames = [self.frames[1], next],
                    // Ran dry: quiet until there is enough again.
                    Err(_) => {
                        self.filling = true;
                        self.frames = [[0.0; 2]; 2];
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
        |err| eprintln!("mirror stream error: {err}"),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(target: f64) -> (rtrb::Producer<[f32; 2]>, Output) {
        let (producer, consumer) = rtrb::RingBuffer::new(4096);
        let output = Output {
            queue: consumer,
            ratio: 1.0,
            target,
            filling: true,
            phase: 0.0,
            frames: [[0.0; 2]; 2],
            channels: 2,
        };
        (producer, output)
    }

    /// Quiet until the target is queued, then the frames as they came (at
    /// the same rate, the target queued).
    #[test]
    fn waits_for_its_queue_then_plays_through() {
        let (mut tap, mut out) = output(100.0);
        for i in 0..50 {
            tap.push([i as f32, -(i as f32)]).unwrap();
        }
        let mut data = [1.0f32; 20];
        out.fill(&mut data);
        assert!(data.iter().all(|&s| s == 0.0), "not yet");
        for i in 50..200 {
            tap.push([i as f32, -(i as f32)]).unwrap();
        }
        let mut data = [0.0f32; 20];
        out.fill(&mut data);
        // The frames in order, a frame apart (a touch faster: more than the
        // target is queued).
        let left: Vec<f32> = data.iter().step_by(2).copied().collect();
        assert!(left[2..].windows(2).all(|w| (w[1] - w[0] - 1.0).abs() <= 0.01), "{left:?}");
    }

    /// More queued than wanted: it plays a little faster; less: slower.
    #[test]
    fn keeps_its_queue_where_it_should_be() {
        let (_tap, out) = output(100.0);
        assert!(out.step(200.0) > 1.0 && out.step(200.0) <= 1.0 + MAX_CORRECTION);
        assert!(out.step(50.0) < 1.0);
        assert_eq!(out.step(100.0), 1.0);
    }
}
