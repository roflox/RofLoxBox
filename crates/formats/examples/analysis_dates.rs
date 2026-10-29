//! Prints each track's file name, analysis date and date added.
//!
//! `cargo run -p deckhand-formats --example analysis_dates -- export.pdb`

use deckhand_formats::pdb::{Database, Track};

fn main() {
    let db = Database::open(std::env::args().nth(1).expect("export.pdb")).unwrap();
    let tracks: Vec<Track> = db.read_all().unwrap();
    for t in tracks {
        println!("{}\t{}\t{}", t.filename, t.analyze_date, t.date_added);
    }
}
