//! Prints the jog bindings a rekordbox mapping file imports to.
//!
//! `cargo run -p deckhand-control --example jog_bindings -- DDJ-GRV6`

fn main() {
    let wanted = std::env::args().nth(1).expect("controller name");
    let (_, path) = deckhand_control::rekordbox::mapping_files()
        .into_iter()
        .find(|(n, _)| *n == wanted)
        .expect("no such file");
    let import = deckhand_control::rekordbox::import_file(&path).unwrap();
    for b in import.mapping.bindings.iter().filter(|b| b.action.to_string().contains("jog")) {
        println!("{} ch{} #{:02X} {:?}", b.action, b.channel, b.number, b.message);
    }
}
