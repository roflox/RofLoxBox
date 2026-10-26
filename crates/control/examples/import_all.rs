//! Converts every rekordbox mapping file found on this Mac and prints a
//! summary per controller.
fn main() {
    for (name, path) in deckhand_control::rekordbox::mapping_files() {
        match deckhand_control::rekordbox::import_file(&path) {
            Ok(import) => println!(
                "{name:<28} {:>4} controls {:>4} lights {:>4} unsupported",
                import.mapping.bindings.len(),
                import.mapping.outputs.len(),
                import.unsupported.len()
            ),
            Err(err) => println!("{name:<28} ERROR {err:#}"),
        }
    }
}
