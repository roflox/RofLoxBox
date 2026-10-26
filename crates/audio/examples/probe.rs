//! Decodes the given files and opens the default output device (silently).
fn main() -> anyhow::Result<()> {
    for path in std::env::args().skip(1) {
        let start = std::time::Instant::now();
        let audio = deckhand_audio::decode_file(path.as_ref())?;
        println!(
            "{path}: {} Hz, {:.1} s, decoded in {:?}",
            audio.sample_rate,
            audio.duration_secs(),
            start.elapsed()
        );
    }
    println!("output devices: {:?}", deckhand_audio::Engine::output_devices());
    let engine = deckhand_audio::Engine::start(&Default::default())?;
    println!("opened: {:?}", engine.info());
    Ok(())
}
