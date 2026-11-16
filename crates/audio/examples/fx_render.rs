//! Test aid: plays a file through each Beat FX, as deckhand's mixer would,
//! into one WAV per effect, to compare with rekordbox's.
//!
//!     cargo run -p deckhand-audio --example fx_render -- <in.wav> <out dir> [depth 0-1] [beat secs]
//!
//! The effect is on from the start (as the test switches rekordbox's on
//! before playing); `beat secs` defaults to one beat at 120 BPM.

use std::path::PathBuf;

use deckhand_audio::{BeatFx, BeatFxKind, BeatFxParams, FxTarget, decode_file, write_float_wav_file};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let input = PathBuf::from(args.next().expect("an input WAV"));
    let out = PathBuf::from(args.next().expect("an output folder"));
    let depth: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let beat_secs: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0.5);
    std::fs::create_dir_all(&out)?;
    let audio = decode_file(&input)?;
    for kind in BeatFxKind::ALL {
        let mut fx = BeatFx::new(audio.sample_rate);
        fx.set(BeatFxParams {
            kind,
            on: true,
            depth,
            beat_secs,
            target: FxTarget::Master,
            release: false,
        });
        let mut samples = Vec::with_capacity(audio.len());
        for frame in audio.to_f32().chunks_exact(2) {
            let y = fx.process([frame[0], frame[1]]);
            samples.extend_from_slice(&y);
        }
        let name = kind.label().to_lowercase().replace(' ', "-");
        let path = out.join(format!("deckhand-{name}.wav"));
        write_float_wav_file(&path, audio.sample_rate, &samples)?;
        println!("{}", path.display());
    }
    Ok(())
}
