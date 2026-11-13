//! The explorer, like rekordbox's: the computer's folders in the sidebar and
//! a folder's audio files in the list, loadable without adding them to the
//! collection.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use deckhand_library::{AUDIO_EXTENSIONS, ScannedFile, read_file};
use eframe::egui::{self, RichText, Stroke, Ui};

use super::{DeckhandApp, Focus, Source};
use crate::browser::{Row, TableOptions, sort_rows, track_table};
use crate::widgets::disclosure;

#[derive(Default)]
pub struct ExplorerView {
    /// The EXPLORER section is open in the sidebar.
    pub open: bool,
    /// Folders opened in the tree, and the subfolders read for them.
    expanded: HashSet<PathBuf>,
    subfolders: HashMap<PathBuf, Vec<PathBuf>>,
    /// The folder whose files are listed.
    pub folder: Option<PathBuf>,
    /// Its audio files, in name order (row ids are positions here).
    files: Vec<ScannedFile>,
    /// Tags still being read for the folder.
    reading: Option<Receiver<ScannedFile>>,
}

impl ExplorerView {
    pub fn files_get(&self, index: u32) -> Option<&ScannedFile> {
        self.files.get(index as usize)
    }
}

/// Where the tree starts: Music, the home folder and mounted drives.
fn roots() -> Vec<(String, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        if home.join("Music").is_dir() {
            roots.push(("Music".to_owned(), home.join("Music")));
        }
        roots.push(("Home".to_owned(), home));
    }
    if cfg!(target_os = "macos") {
        roots.extend(subfolders(Path::new("/Volumes")).into_iter().map(|p| (name(&p), p)));
    } else if cfg!(windows) {
        roots.extend(
            (b'C'..=b'Z')
                .map(|d| PathBuf::from(format!("{}:\\", d as char)))
                .filter(|p| p.is_dir())
                .map(|p| (p.display().to_string(), p)),
        );
    } else {
        roots.push(("/".to_owned(), PathBuf::from("/")));
    }
    roots
}

fn name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Visible subfolders of `dir`, by name.
fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    found.sort_by_key(|p| name(p).to_lowercase());
    found
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

/// A deck id for a file outside any library: from its path, with the top
/// bit set so it does not look like an export's track id.
pub fn file_id(path: &Path) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    (hasher.finish() as u32) | 0x8000_0000
}

impl DeckhandApp {
    /// Shows a folder's audio files, reading their tags in the background.
    pub(super) fn open_explorer_folder(&mut self, folder: PathBuf) {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_audio(p))
            .collect();
        files.sort_by_key(|p| name(p).to_lowercase());
        let (tx, rx) = channel();
        let to_read = files.clone();
        std::thread::spawn(move || {
            for path in to_read {
                let file = read_file(&path).unwrap_or_else(|_| ScannedFile {
                    title: path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    path: path.clone(),
                    ..ScannedFile::default()
                });
                if tx.send(file).is_err() {
                    break;
                }
            }
        });
        // Until the tags arrive, the file names.
        let view = &mut self.explorer;
        view.files = files
            .into_iter()
            .map(|path| ScannedFile {
                title: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path,
                ..ScannedFile::default()
            })
            .collect();
        view.reading = Some(rx);
        view.folder = Some(folder);
        self.selected = None;
        self.set_source(Source::Explorer);
    }

    /// Takes in tags read so far. Call every frame.
    pub(super) fn poll_explorer(&mut self, ctx: &egui::Context) {
        let view = &mut self.explorer;
        let Some(rx) = &view.reading else { return };
        loop {
            match rx.try_recv() {
                Ok(file) => {
                    if let Some(slot) = view.files.iter_mut().find(|f| f.path == file.path) {
                        *slot = file;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    view.reading = None;
                    break;
                }
            }
        }
    }

    /// The EXPLORER part of the sidebar.
    pub(super) fn explorer_section(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if disclosure(ui, self.explorer.open).clicked() {
                self.explorer.open = !self.explorer.open;
            }
            ui.label(RichText::new("EXPLORER").strong()).on_hover_text(
                "Load files without adding them",
            );
        });
        if !self.explorer.open {
            return;
        }
        for (label, path) in roots() {
            self.folder_row(ui, &path, &label, 1);
        }
    }

    fn folder_row(&mut self, ui: &mut Ui, path: &Path, label: &str, depth: usize) {
        let expanded = self.explorer.expanded.contains(path);
        let selected =
            self.source == Source::Explorer && self.explorer.folder.as_deref() == Some(path);
        ui.horizontal(|ui| {
            ui.add_space(14.0 * depth as f32 - 14.0);
            if disclosure(ui, expanded).clicked() {
                if expanded {
                    self.explorer.expanded.remove(path);
                } else {
                    // Read again on every opening, so new folders show up.
                    self.explorer.subfolders.insert(path.to_path_buf(), subfolders(path));
                    self.explorer.expanded.insert(path.to_path_buf());
                }
            }
            if ui.selectable_label(selected, format!("🗀 {label}")).clicked() {
                self.open_explorer_folder(path.to_path_buf());
                self.focus = Focus::Playlists;
            }
        });
        if !expanded {
            return;
        }
        let children = self.explorer.subfolders.get(path).cloned().unwrap_or_default();
        for child in children {
            self.folder_row(ui, &child, &name(&child), depth + 1);
        }
    }

    /// The folders shown in the explorer tree, top to bottom (for browsing
    /// with the keys and the browse knob).
    pub(super) fn explorer_folders(&self) -> Vec<PathBuf> {
        fn walk(view: &ExplorerView, path: &Path, out: &mut Vec<PathBuf>) {
            out.push(path.to_path_buf());
            if view.expanded.contains(path) {
                for child in view.subfolders.get(path).into_iter().flatten() {
                    walk(view, child, out);
                }
            }
        }
        let mut out = Vec::new();
        if self.explorer.open {
            for (_, root) in roots() {
                walk(&self.explorer, &root, &mut out);
            }
        }
        out
    }

    /// The folder's files as list rows; ids are positions in the folder.
    pub(super) fn explorer_rows(&self) -> Vec<Row> {
        let needle = self.search.trim().to_lowercase();
        let mut rows: Vec<Row> = self
            .explorer
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                needle.is_empty()
                    || [&f.title, &f.artist, &f.album, &f.genre, &f.label]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&needle))
            })
            .map(|(i, f)| Row {
                id: i as u32,
                entry: None,
                position: i,
                title: f.title.clone(),
                artist: f.artist.clone(),
                album: f.album.clone(),
                genre: f.genre.clone(),
                label: f.label.clone(),
                bpm: f.bpm,
                key: f
                    .key
                    .as_deref()
                    .map(|k| crate::deck::display_key(k, self.config.prefs.key_notation)),
                duration: f.duration,
                year: f.year,
                rating: 0,
                plays: 0,
                added: String::new(),
                bitrate: f.bitrate,
                comment: f.comment.clone(),
                artwork: f.has_artwork.then(|| crate::artwork::Cover::Embedded(f.path.clone())),
                preview: Some(crate::preview::Source { audio: f.path.clone(), analysis: None }),
                cues: None,
            })
            .collect();
        sort_rows(&mut rows, self.config.sort);
        rows
    }

    /// Loads a file of the folder shown: as its collection track when the
    /// collection has it (grid, cues), otherwise as a plain file.
    pub(super) fn load_from_explorer(&mut self, index: u32, deck: usize, ctx: &egui::Context) {
        let Some(file) = self.explorer.files.get(index as usize).cloned() else { return };
        // Tags not read yet (just opened folder): read this one now.
        let file = if self.explorer.reading.is_some() {
            read_file(&file.path).unwrap_or(file)
        } else {
            file
        };
        if let Some(id) = self.collection.tracks.iter().find(|t| t.path == file.path).map(|t| t.id)
        {
            self.load_from_collection(id, deck, ctx);
            return;
        }
        if self.load_blocked(deck) {
            return;
        }
        let id = file_id(&file.path);
        self.mark_double(deck, id, false);
        self.decks[deck].start_load_path(id, &file, ctx);
    }

    /// The folder's files: search, load buttons and the list.
    pub(super) fn explorer_list(&mut self, ui: &mut Ui, stroke: Stroke, ctx: &egui::Context) {
        let Some(folder) = self.explorer.folder.clone() else {
            ui.label("Choose a folder in EXPLORER.");
            return;
        };
        let deck_count = self.deck_count();
        let mut load = None;
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search title, artist, album, genre, label")
                    .desired_width(320.0),
            );
            if let Some(id) = self.selected {
                ui.separator();
                for deck in 0..deck_count {
                    if ui.button(format!("Load to deck {}", deck + 1)).clicked() {
                        load = Some((id, deck));
                    }
                }
            }
            ui.separator();
            if ui
                .button("Add folder to collection")
                .on_hover_text("Add with subfolders, kept scanned")
                .clicked()
            {
                self.add_music(vec![folder.clone()]);
            }
            if self.explorer.reading.is_some() {
                ui.spinner();
            }
        });
        ui.label(RichText::new(folder.display().to_string()).small().weak());
        let rows = self.explorer_rows();
        if rows.is_empty() {
            ui.add_space(20.0);
            ui.label("No audio files in this folder.");
        }
        // Deck markers: which row each deck plays.
        let loaded: Vec<Option<u32>> = self
            .decks
            .iter()
            .map(|d| {
                let id =
                    d.track.as_ref().filter(|t| !t.from_collection && t.usb_root.is_none())?.id;
                self.explorer.files.iter().position(|f| file_id(&f.path) == id).map(|i| i as u32)
            })
            .collect();
        let free = self.free_deck(&loaded);
        let options = TableOptions {
            selected: self.selected,
            selected_position: None,
            reveal_selected: std::mem::take(&mut self.reveal_selected),
            loaded: &loaded,
            draggable: true,
            marked: &self.marked,
            editable_rating: false,
            reorder: None,
            compatible_with: self.compatible_key(),
        };
        let selection = self.selected_ids();
        let group = |id: u32| if self.marked.contains(&id) { selection.clone() } else { vec![id] };
        let mut add: Option<Vec<u32>> = None;
        let output = egui::Frame::new()
            .stroke(stroke)
            .show(ui, |ui| {
                track_table(
                    ui,
                    &rows,
                    &mut self.config.columns,
                    &mut self.config.sort,
                    &options,
                    &mut |ui, row| {
                        for deck in 0..deck_count {
                            if ui.button(format!("Load to deck {}", deck + 1)).clicked() {
                                load = Some((row.id, deck));
                                ui.close();
                            }
                        }
                        ui.separator();
                        let ids = group(row.id);
                        let label = match ids.len() {
                            1 => "Add to collection".to_owned(),
                            n => format!("Add {n} tracks to collection"),
                        };
                        if ui.button(label).clicked() {
                            add = Some(ids);
                            ui.close();
                        }
                    },
                )
            })
            .inner;
        if output.layout_changed {
            self.config.save();
        }
        if let Some((id, position)) = output.clicked {
            self.click_track(id, position, output.modifiers);
        }
        if let Some((id, position)) = output.drag_started {
            self.start_track_drag(ui.ctx(), id, position);
        }
        if let (Some(id), Some(deck)) = (output.double_clicked, free) {
            load = Some((id, deck));
        }
        if let Some(indices) = add {
            let paths: Vec<PathBuf> = indices
                .iter()
                .filter_map(|&i| self.explorer.files_get(i).map(|f| f.path.clone()))
                .collect();
            self.add_music(paths);
        }
        if let Some((id, deck)) = load {
            self.selected = Some(id);
            self.load_from_explorer(id, deck, ctx);
        }
    }
}
