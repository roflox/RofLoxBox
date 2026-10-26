//! Renders a track at another tempo with Master Tempo on, to a WAV file, for
//! listening to the time-stretcher without the app.
//!
//! `cargo run --release -p deckhand-audio --example stretch -- IN OUT.wav TEMPO [SECONDS]`

use std::io::Write;
use std::time::Instant;

use anyhow::{Context, Result};
use deckhand_audio::{Stretcher, decode_file};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output, tempo, rest @ ..] = args.as_slice() else {
        anyhow::bail!("usage: stretch IN OUT.wav TEMPO [SECONDS]");
    };
    let tempo: f64 = tempo.parse().context("TEMPO, e.g. 1.08")?;
    let seconds: f64 = rest.first().map_or(Ok(30.0), |s| s.parse()).context("SECONDS")?;
    let audio = decode_file(input.as_ref())?;
    let rate = audio.sample_rate;
    let frames = (seconds * f64::from(rate)) as usize;

    let started = Instant::now();
    let mut stretcher = Stretcher::new(rate);
    let mut position = 0.0;
    let mut samples = Vec::with_capacity(frames * 2);
    for _ in 0..frames {
        samples.extend(stretcher.next(&audio, position, 1.0, true));
        position += tempo;
    }
    let took = started.elapsed().as_secs_f64();
    println!("{seconds} s of audio in {took:.3} s ({:.0}× real time)", seconds / took);

    let mut file = std::io::BufWriter::new(std::fs::File::create(output)?);
    let data_len = (samples.len() * 2) as u32;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_len).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?; // PCM
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&(rate * 4).to_le_bytes())?;
    file.write_all(&4u16.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        file.write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
    }
    Ok(())
}
