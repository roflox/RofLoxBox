use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use deckhand_formats::anlz::{AnlzFile, CueKind, Section};
use deckhand_formats::pdb::{
    Database, DatabaseWriter, Library, PlaylistKind, PlaylistNode, Tag, TagTrack,
};
use deckhand_formats::settings::{Settings, SettingsFile};

#[derive(Parser)]
#[command(version, about = "Inspect Pioneer DJ USB exports")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read `export.pdb` / `exportExt.pdb` databases.
    Pdb {
        #[command(subcommand)]
        command: PdbCommand,
    },
    /// Summarize an analysis file (`ANLZ*.DAT`, `.EXT`, `.2EX`).
    Anlz {
        file: PathBuf,
        /// Print every beat of the beat grid.
        #[arg(long)]
        beats: bool,
    },
    /// Show a `*SETTING.DAT` file.
    Settings { file: PathBuf },
    /// Work with a whole USB/SD export.
    Usb {
        #[command(subcommand)]
        command: UsbCommand,
    },
    /// deckhand's own collection (the one the app shows).
    Collection {
        /// Collection file; defaults to the app's.
        #[arg(long)]
        file: Option<PathBuf>,
        #[command(subcommand)]
        command: CollectionCommand,
    },
}

#[derive(Subcommand)]
enum CollectionCommand {
    /// Add folders or files (folders are remembered for rescans).
    Add { paths: Vec<PathBuf> },
    /// List the tracks.
    List,
    /// Import a rekordbox XML export (File → Export Collection in xml format).
    ImportXml { file: PathBuf },
}

#[derive(Subcommand)]
enum UsbCommand {
    /// Copy an export, rewriting every database, analysis and settings file
    /// with deckhand's writers (audio and other files are copied as they are).
    /// Use it to test the writers on a player.
    Rewrite {
        /// Root of the source medium (the directory containing `PIONEER`).
        source: PathBuf,
        /// Empty or missing destination directory.
        destination: PathBuf,
    },
}

#[derive(Subcommand)]
enum PdbCommand {
    /// Show the header, tables, page and row counts.
    Info { file: PathBuf },
    /// List tracks.
    Tracks { file: PathBuf },
    /// Show the playlist tree with its tracks.
    Playlists { file: PathBuf },
    /// List My Tag categories and tags from an `exportExt.pdb`.
    Tags { file: PathBuf },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Pdb { command } => match command {
            PdbCommand::Info { file } => info(&open(&file)?),
            PdbCommand::Tracks { file } => tracks(&library(&file)?),
            PdbCommand::Playlists { file } => playlists(&library(&file)?),
            PdbCommand::Tags { file } => tags(&open(&file)?),
        },
        Command::Anlz { file, beats } => anlz(
            &AnlzFile::open(&file).with_context(|| format!("reading {}", file.display()))?,
            beats,
        ),
        Command::Usb { command: UsbCommand::Rewrite { source, destination } } => {
            usb_rewrite(&source, &destination)
        }
        Command::Collection { file, command } => {
            collection(&file.unwrap_or_else(deckhand_library::collection_path), command)
        }
        Command::Settings { file } => settings(
            &SettingsFile::open(&file).with_context(|| format!("reading {}", file.display()))?,
        ),
    }
}

fn collection(file: &Path, command: CollectionCommand) -> Result<()> {
    let mut collection = deckhand_library::Collection::open(file)?;
    match command {
        CollectionCommand::Add { paths } => {
            for path in paths.iter().filter(|p| p.is_dir()) {
                collection.add_folder(&std::fs::canonicalize(path)?);
            }
            let paths: Vec<PathBuf> =
                paths.iter().map(std::fs::canonicalize).collect::<std::io::Result<_>>()?;
            let scan = deckhand_library::Scan::start(paths, collection.known_files());
            let files = loop {
                if let Some(files) = scan.finished() {
                    break files;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            };
            let (added, updated) = collection.merge(files);
            collection.save()?;
            println!(
                "{added} added, {updated} updated, {} in {}",
                collection.tracks.len(),
                file.display()
            );
        }
        CollectionCommand::ImportXml { file: xml } => {
            let s = collection.import_rekordbox(deckhand_library::XmlLibrary::open(&xml)?);
            collection.save()?;
            println!(
                "{} added, {} updated, {} playlists, {} files not found; {} tracks in {}",
                s.added,
                s.updated,
                s.playlists,
                s.missing,
                collection.tracks.len(),
                file.display()
            );
        }
        CollectionCommand::List => {
            for t in &collection.tracks {
                let bpm = t.bpm.map_or_else(String::new, |b| format!("{b:.1}"));
                println!(
                    "{:5}  {} — {}  {bpm}  {}",
                    t.id,
                    t.artist,
                    t.title,
                    t.key.as_deref().unwrap_or("")
                );
            }
        }
    }
    Ok(())
}

fn open(file: &PathBuf) -> Result<Database> {
    Database::open(file).with_context(|| format!("reading {}", file.display()))
}

fn library(file: &PathBuf) -> Result<Library> {
    Library::from_database(&open(file)?).with_context(|| format!("reading {}", file.display()))
}

fn info(db: &Database) -> Result<()> {
    println!(
        "schema {:?}, page size {}, {} pages, sequence {}, next unused page {}",
        db.schema(),
        db.page_size(),
        db.page_count(),
        db.sequence(),
        db.next_unused_page()
    );
    println!("{:<18} {:>6} {:>6} {:>6} {:>6}", "table", "first", "last", "pages", "rows");
    for table in db.tables() {
        let pages = db.table_pages(table)?;
        let mut rows = 0;
        for page in &pages {
            rows += page.rows()?.len();
        }
        println!(
            "{:<18} {:>6} {:>6} {:>6} {:>6}",
            format!("{:?}", table.table_type),
            table.first_page,
            table.last_page,
            pages.len(),
            rows
        );
    }
    Ok(())
}

fn tracks(library: &Library) -> Result<()> {
    for t in &library.tracks {
        let track = &t.track;
        println!(
            "{:>5}  {:>6.2} bpm  {:>4}s  {:<4} {} — {}  [{}]",
            track.id,
            track.bpm(),
            track.duration_secs,
            t.key.as_deref().unwrap_or("-"),
            t.artist.as_deref().unwrap_or("?"),
            track.title,
            track.file_path
        );
    }
    Ok(())
}

fn playlists(library: &Library) -> Result<()> {
    fn print(library: &Library, nodes: &[PlaylistNode], depth: usize) {
        for node in nodes {
            let indent = "  ".repeat(depth);
            match &node.kind {
                PlaylistKind::Folder(children) => {
                    println!("{indent}{}/", node.name);
                    print(library, children, depth + 1);
                }
                PlaylistKind::Playlist(track_ids) => {
                    println!("{indent}{} ({} tracks)", node.name, track_ids.len());
                    for id in track_ids {
                        let title =
                            library.track(*id).map_or("<missing track>", |t| &t.track.title);
                        println!("{indent}  - {title}");
                    }
                }
            }
        }
    }
    print(library, &library.playlists, 0);
    for list in &library.history {
        println!("history {} ({} tracks)", list.name, list.track_ids.len());
    }
    Ok(())
}

fn tags(db: &Database) -> Result<()> {
    let tags = db.read_all::<Tag>()?;
    let tag_tracks = db.read_all::<TagTrack>()?;
    let mut categories: Vec<&Tag> = tags.iter().filter(|tag| tag.is_category).collect();
    categories.sort_by_key(|category| category.position);
    for category in categories {
        println!("{}", category.name);
        let mut members: Vec<&Tag> =
            tags.iter().filter(|tag| tag.category_id == Some(category.id)).collect();
        members.sort_by_key(|tag| tag.position);
        for tag in members {
            let count = tag_tracks.iter().filter(|link| link.tag_id == tag.id).count();
            println!("  {} ({count} tracks)", tag.name);
        }
    }
    Ok(())
}

fn anlz(file: &AnlzFile, all_beats: bool) -> Result<()> {
    for section in &file.sections {
        let tag = String::from_utf8_lossy(&section.tag()).into_owned();
        match section {
            Section::Path(path) => println!("{tag}  path {path}"),
            Section::BeatGrid(grid) => {
                let first = grid.beats.first();
                println!(
                    "{tag}  {} beats, first at {} ms, {:.2} bpm",
                    grid.beats.len(),
                    first.map_or(0, |b| b.time_ms),
                    first.map_or(0.0, |b| b.bpm())
                );
                if all_beats {
                    for beat in &grid.beats {
                        println!(
                            "      {:>8} ms  beat {}  {:.2} bpm",
                            beat.time_ms,
                            beat.bar_position,
                            beat.bpm()
                        );
                    }
                }
            }
            Section::ExtendedBeatGrid(grid) => println!(
                "{tag}  {} beats, first at {} ms, last at {} ms",
                grid.beat_count, grid.first_beat.time_ms, grid.last_beat.time_ms
            ),
            Section::Cues(list) => {
                println!("{tag}  {:?} cues: {}", list.kind, list.cues.len());
                for cue in &list.cues {
                    let name = cue
                        .hot_cue_letter()
                        .map_or_else(|| "memory".to_owned(), |c| format!("hot {c}"));
                    let what = match (cue.kind, cue.loop_end_ms) {
                        (CueKind::Loop, Some(end)) => format!("loop {}–{end} ms", cue.time_ms),
                        _ => format!("cue at {} ms", cue.time_ms),
                    };
                    let comment =
                        cue.comment.as_deref().map(|c| format!("  \"{c}\"")).unwrap_or_default();
                    let color = cue
                        .hot_cue_color
                        .map(|c| format!("  #{:02x}{:02x}{:02x}", c.rgb[0], c.rgb[1], c.rgb[2]))
                        .unwrap_or_default();
                    println!("      {name:<8} {what}{comment}{color}");
                }
            }
            Section::Vbr(index) => println!("{tag}  VBR index, {} entries", index.len()),
            Section::Waveform(waveform) => println!(
                "{tag}  {:?} waveform, {} columns × {} bytes",
                waveform.kind,
                waveform.column_count(),
                waveform.entry_len
            ),
            Section::SongStructure(s) => {
                println!(
                    "{tag}  {:?} mood, {} phrases, ends at beat {}{}",
                    s.mood,
                    s.phrases.len(),
                    s.end_beat,
                    if s.masked { " (was masked)" } else { "" }
                );
                for phrase in &s.phrases {
                    println!("      beat {:>4}  {}", phrase.beat, phrase.label(s.mood));
                }
            }
            Section::Unknown { bytes, .. } => println!("{tag}  unknown, {} bytes", bytes.len()),
        }
    }
    Ok(())
}

fn settings(file: &SettingsFile) -> Result<()> {
    println!("{:?} settings: {} / {} {}", file.kind, file.brand, file.software, file.version);
    println!(
        "checksum {:#06x} ({})",
        file.checksum,
        if file.checksum_is_valid() { "valid" } else { "INVALID" }
    );
    match file.settings() {
        Some(Settings::Player(s)) => println!("{s:#?}"),
        Some(Settings::Player2(s)) => println!("{s:#?}"),
        Some(Settings::Device(s)) => println!("{s:#?}"),
        Some(Settings::Mixer(s)) => println!("{s:#?}"),
        None => println!("unrecognized data block"),
    }
    println!("raw data:");
    for (row, chunk) in file.data.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        println!("  {:04x}  {}", row * 16, hex.join(" "));
    }
    Ok(())
}

fn usb_rewrite(source: &Path, destination: &Path) -> Result<()> {
    anyhow::ensure!(
        source.join("PIONEER").is_dir(),
        "{} has no PIONEER directory",
        source.display()
    );
    if destination.exists() {
        anyhow::ensure!(
            std::fs::read_dir(destination)?.next().is_none(),
            "{} is not empty",
            destination.display()
        );
    }
    let (mut rewritten, mut copied) = (0, 0);
    let mut pending = vec![source.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let target = destination.join(path.strip_prefix(source)?);
            if path.is_dir() {
                std::fs::create_dir_all(&target)?;
                pending.push(path);
                continue;
            }
            let name =
                path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_uppercase();
            let context = || format!("rewriting {}", path.display());
            let bytes = if name.ends_with(".PDB") && name.starts_with("EXPORT") {
                Some(
                    DatabaseWriter::from_database(&Database::open(&path).with_context(context)?)?
                        .to_bytes(),
                )
            } else if name.starts_with("ANLZ") {
                Some(AnlzFile::open(&path).with_context(context)?.to_bytes())
            } else if name.ends_with("SETTING.DAT") || name.ends_with("SETTING2.DAT") {
                Some(SettingsFile::open(&path).with_context(context)?.to_bytes())
            } else {
                None
            };
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match bytes {
                Some(bytes) => {
                    std::fs::write(&target, bytes)?;
                    rewritten += 1;
                }
                None => {
                    std::fs::copy(&path, &target)?;
                    copied += 1;
                }
            }
        }
    }
    println!("rewrote {rewritten} files, copied {copied} files to {}", destination.display());
    Ok(())
}
