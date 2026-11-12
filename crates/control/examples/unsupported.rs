//! Prints the rekordbox functions of one mapping file that deckhand does not map.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wanted = std::env::args().nth(1).expect("controller name");
    let (_, path) = deckhand_control::rekordbox::mapping_files()
        .into_iter()
        .find(|(n, _)| *n == wanted)
        .expect("no such file");
    let import = deckhand_control::rekordbox::import_file(&path)?;
    println!("{}", import.unsupported.join(" "));
    Ok(())
}
