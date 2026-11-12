//! Prints the raw playlist tree and the first entries of each playlist.
//!
//! `cargo run -p deckhand-formats --example playlist_rows -- export.pdb`

use deckhand_formats::pdb::{Database, PlaylistEntry, PlaylistTreeNode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("export.pdb");
    let db = Database::open(&path)?;
    let nodes: Vec<PlaylistTreeNode> = db.read_all()?;
    let entries: Vec<PlaylistEntry> = db.read_all()?;
    for n in &nodes {
        let mine: Vec<_> = entries.iter().filter(|e| e.playlist_id == n.id).collect();
        let indexes: Vec<u32> = mine.iter().take(4).map(|e| e.entry_index).collect();
        println!(
            "id {:>3} parent {:?} order {:>3} folder {} {:?}: {} entries, first indexes {:?}",
            n.id,
            n.parent_id,
            n.sort_order,
            n.is_folder,
            n.name,
            mine.len(),
            indexes
        );
    }
    Ok(())
}
