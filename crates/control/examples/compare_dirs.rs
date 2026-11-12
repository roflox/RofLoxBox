//! Imports every mapping file in two folders and compares them per controller.
use std::path::Path;

fn summary(path: &Path) -> String {
    match deckhand_control::rekordbox::import_file(path) {
        Ok(i) => format!(
            "{:>4} controls {:>3} lights",
            i.mapping.bindings.len(),
            i.mapping.outputs.len()
        ),
        Err(err) => format!("ERROR {err:#}"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let (a, b) = (Path::new(&args[1]), Path::new(&args[2]));
    let mut names: Vec<String> = std::fs::read_dir(a)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".midi.csv"))
        .collect();
    names.sort();
    for name in names {
        let (left, right) = (
            summary(&a.join(&name)),
            if b.join(&name).exists() { summary(&b.join(&name)) } else { "—".into() },
        );
        let mark = if left == right { " " } else { "≠" };
        println!("{mark} {:<28} rb6 {left:<26} rb7 {right}", name.trim_end_matches(".midi.csv"));
    }
    Ok(())
}
