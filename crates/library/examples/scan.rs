//! Prints the tags deckhand reads from the audio files in a folder.
//!
//! `cargo run -p deckhand-library --example scan -- FOLDER`

use std::collections::HashMap;

fn main() {
    let folder = std::env::args().nth(1).expect("folder");
    let scan = deckhand_library::Scan::start(vec![folder.into()], HashMap::new());
    let files = loop {
        if let Some(files) = scan.finished() {
            break files;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    for f in files {
        println!(
            "{} | {} | {} | {:.1}s | bpm {:?} | key {:?} | {:?} Hz | art {}",
            f.title, f.artist, f.genre, f.duration, f.bpm, f.key, f.sample_rate, f.has_artwork
        );
    }
}
