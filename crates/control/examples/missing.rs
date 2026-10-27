//! Shows what "Add missing only" would add to a saved mapping, without
//! changing it.
//!
//! `cargo run -p deckhand-control --example missing -- MAPPING.toml DDJ-GRV6`

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(file), Some(name)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: missing MAPPING.toml CONTROLLER");
    };
    let mut mapping = deckhand_control::Mapping::load(file.as_ref())?;
    let (_, path) = deckhand_control::rekordbox::mapping_files()
        .into_iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| anyhow::anyhow!("no rekordbox mapping named {name}"))?;
    let import = deckhand_control::rekordbox::import_file(&path)?;
    let before = mapping.bindings.len();
    let lights_before = mapping.outputs.len();
    let (bindings, lights) = mapping.add_missing(&import.mapping);
    println!("would add {bindings} controls and {lights} lights");
    for b in &mapping.bindings[before..] {
        println!("  {} ({:?} ch{} #{:02X})", b.action, b.message, b.channel, b.number);
    }
    for o in &mapping.outputs[lights_before..] {
        println!("  light {:?} ({:?} ch{} #{:02X})", o.feedback, o.message, o.channel, o.number);
    }
    Ok(())
}
