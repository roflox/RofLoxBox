//! Prints the first track rows of an export as stored.
//!
//! `cargo run -p deckhand-formats --example track_row -- export.pdb [count]`

use deckhand_formats::pdb::{Album, Artist, Database, Track};

fn main() {
    let mut args = std::env::args().skip(1);
    let db = Database::open(args.next().expect("export.pdb")).unwrap();
    let count: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(2);
    let tracks: Vec<Track> = db.read_all().unwrap();
    for track in tracks.iter().take(count) {
        println!("{track:#?}");
    }
    let artists: Vec<Artist> = db.read_all().unwrap();
    let albums: Vec<Album> = db.read_all().unwrap();
    println!("{:?}\n{:?}", artists.first(), albums.first());
    let keys: Vec<deckhand_formats::pdb::Key> = db.read_all().unwrap();
    println!("keys: {:?}", keys.iter().map(|k| k.name.clone()).collect::<Vec<_>>());
}
