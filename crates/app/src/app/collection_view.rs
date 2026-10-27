//! The browser for deckhand's own collection: folders of playlists on the
//! left, tracks on the right, adding music by folder, file or drag and drop.

use std::path::PathBuf;

use deckhand_library::{NodeKind, Scan};
use eframe::egui::{self, Align, Layout, RichText, ScrollArea, Stroke, Ui, vec2};

use super::{ACTIVE, DeckhandApp, Focus, WARN};
use crate::browser::{Row, TableOptions, sort_rows, track_table};

/// Browser state for the collection.
#[derive(Default)]
pub struct CollectionView {
    /// The playlist shown; `None` is the whole collection.
    pub node: Option<u32>,
    pub scan: Option<Scan>,
    /// A playlist or folder being renamed, with the text typed so far.
    renaming: Option<(u32, String)>,
    pub error: Option<String>,
    /// A track whose info is being edited: (id, title, artist, album, genre,
    /// label, comment).
    editing: Option<(u32, [String; 6])>,
    /// A track (and playlist) waiting for "add it again?" to be answered.
    pending_duplicate: Option<(u32, u32)>,
    /// A rekordbox XML file being read.
    xml: Option<std::sync::mpsc::Receiver<anyhow::Result<deckhand_library::XmlLibrary>>>,
}

impl DeckhandApp {
    /// Tracks shown for the collection: the chosen playlist or everything,
    /// filtered by the search text.
    /// Tracks shown with their position in the list and whether that list
    /// is a playlist.
    fn collection_entries(&self) -> Vec<(u32, usize, bool)> {
        let needle = self.search.trim().to_lowercase();
        let c = &self.collection;
        let matches = |id: &u32| {
            needle.is_empty()
                || c.track(*id).is_some_and(|t| {
                    [&t.title, &t.artist, &t.album, &t.genre, &t.label]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&needle))
                })
        };
        match self.collection_view.node.and_then(|n| c.node(n)) {
            Some(node) => node
                .tracks
                .iter()
                .enumerate()
                .filter(|(_, id)| matches(id))
                .map(|(i, &id)| (id, i, true))
                .collect(),
            None => c
                .tracks
                .iter()
                .enumerate()
                .filter(|(_, t)| matches(&t.id))
                .map(|(i, t)| (t.id, i, false))
                .collect(),
        }
    }

    /// Playlists the browse knob steps through: `None` (everything) first,
    /// folders skipped.
    pub(super) fn collection_browse_entries(&self) -> Vec<Option<u32>> {
        // Playlists inside closed folders are not visible, so not visited.
        let mut entries = vec![None];
        let mut hidden_below: Option<usize> = None;
        for (depth, node) in self.collection.tree() {
            if hidden_below.is_some_and(|d| depth > d) {
                continue;
            }
            hidden_below = None;
            match node.kind {
                NodeKind::Folder if self.config.collapsed_folders.contains(&node.id) => {
                    hidden_below = Some(depth);
                }
                NodeKind::Folder => {}
                NodeKind::Playlist => entries.push(Some(node.id)),
            }
        }
        entries
    }

    /// Adds folders or files to the collection, reading them in the background.
    pub(super) fn add_music(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        for path in paths.iter().filter(|p| p.is_dir()) {
            self.collection.add_folder(path);
        }
        if let Some(scan) = &self.collection_view.scan {
            scan.cancel();
        }
        self.collection_view.scan = Some(Scan::start(paths, self.collection.known_files()));
    }

    /// Rescans every added folder for new and changed files.
    fn rescan(&mut self) {
        let folders = self.collection.folders.clone();
        self.add_music(folders);
    }

    /// Reads a rekordbox XML export in the background.
    fn import_xml(&mut self, path: PathBuf) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(deckhand_library::XmlLibrary::open(&path));
        });
        self.collection_view.xml = Some(rx);
    }

    /// Merges a finished scan or import and saves. Call every frame.
    pub(super) fn poll_collection(&mut self, ctx: &egui::Context) {
        if let Some(result) = self.collection_view.xml.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.collection_view.xml = None;
            match result {
                Ok(library) => {
                    let s = self.collection.import_rekordbox(library);
                    let missing = if s.missing > 0 {
                        format!(", {} file(s) not found on this computer", s.missing)
                    } else {
                        String::new()
                    };
                    self.status = Some(format!(
                        "rekordbox import: {} added, {} updated, {} playlists{missing}",
                        s.added, s.updated, s.playlists
                    ));
                }
                Err(err) => self.collection_view.error = Some(format!("{err:#}")),
            }
        } else if self.collection_view.xml.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        let view = &mut self.collection_view;
        if let Some(scan) = &view.scan {
            match scan.finished() {
                Some(files) => {
                    let (added, updated) = self.collection.merge(files);
                    self.status =
                        Some(format!("Collection: {added} track(s) added, {updated} updated"));
                    view.scan = None;
                }
                None => ctx.request_repaint_after(std::time::Duration::from_millis(200)),
            }
        }
        // Files dropped on the window go into the collection.
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            self.source = super::Source::Collection;
            self.add_music(dropped);
        }
        if self.collection.is_dirty()
            && let Err(err) = self.collection.save()
        {
            self.collection_view.error = Some(format!("{err:#}"));
        }
    }

    pub(super) fn load_from_collection(&mut self, id: u32, deck: usize, ctx: &egui::Context) {
        if self.load_blocked(deck) {
            return;
        }
        self.mark_double(deck, id, true);
        let Some(track) = self.collection.track(id) else { return };
        if !track.path.is_file() {
            self.status = Some(format!("{} is missing: {}", track.title, track.path.display()));
            return;
        }
        self.decks[deck].start_load_file(track, ctx);
        self.collection.update_track(id, |t| {
            t.play_count += 1;
            t.last_played = Some(deckhand_now());
        });
    }

    pub(super) fn collection_ui(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let focus_stroke =
            |focused: bool| if focused { Stroke::new(1.0_f32, ACTIVE) } else { Stroke::NONE };
        let mut load: Option<(u32, usize)> = None;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                vec2(240.0, ui.available_height()),
                Layout::top_down(Align::Min),
                |ui| {
                    egui::Frame::new()
                        .stroke(focus_stroke(self.focus == Focus::Playlists))
                        .show(ui, |ui| self.collection_sidebar(ui));
                },
            );
            ui.separator();
            ui.vertical(|ui| {
                load = self.collection_tracks(ui, focus_stroke(self.focus == Focus::Tracks));
            });
        });
        if let Some((id, deck)) = load {
            self.selected = Some(id);
            self.load_from_collection(id, deck, ctx);
        }
    }

    fn collection_sidebar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            if ui.small_button("+ Folder of music…").clicked()
                && let Some(folder) =
                    rfd::FileDialog::new().set_title("Add a folder of music").pick_folder()
            {
                self.add_music(vec![folder]);
            }
            if ui.small_button("+ Files…").clicked()
                && let Some(files) = rfd::FileDialog::new()
                    .set_title("Add music files")
                    .add_filter("Audio", &deckhand_library::AUDIO_EXTENSIONS)
                    .pick_files()
            {
                self.add_music(files);
            }
            if ui
                .small_button("⟳")
                .on_hover_text("Rescan the added folders for new and changed files")
                .clicked()
            {
                self.rescan();
            }
            if ui
                .small_button("Import rekordbox XML…")
                .on_hover_text(
                    "In rekordbox: File → Export Collection in xml format. Brings tracks, \
                     playlists, beat grids, hot cues, memory cues, ratings and colours.",
                )
                .clicked()
                && let Some(file) = rfd::FileDialog::new()
                    .set_title("Choose a rekordbox XML export")
                    .add_filter("rekordbox XML", &["xml"])
                    .pick_file()
            {
                self.import_xml(file);
            }
        });
        if self.collection_view.xml.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Reading rekordbox XML…").small());
            });
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let parent = self.selected_folder();
            if ui.small_button("+ Playlist").clicked() {
                let id = self.collection.create("New playlist", NodeKind::Playlist, parent);
                self.collection_view.renaming = Some((id, "New playlist".into()));
            }
            if ui.small_button("+ Folder").clicked() {
                let id = self.collection.create("New folder", NodeKind::Folder, parent);
                self.collection_view.renaming = Some((id, "New folder".into()));
            }
        });
        if let Some(scan) = &self.collection_view.scan {
            ui.horizontal(|ui| {
                ui.spinner();
                let found = scan.found.load(std::sync::atomic::Ordering::Relaxed);
                let done = scan.done.load(std::sync::atomic::Ordering::Relaxed);
                ui.label(RichText::new(format!("Reading {done}/{found}…")).small());
                if ui.small_button("Cancel").clicked() {
                    scan.cancel();
                }
            });
        }
        if let Some(err) = &self.collection_view.error {
            ui.colored_label(WARN, err);
        }
        ui.separator();
        ScrollArea::vertical().id_salt("collection tree").auto_shrink([false, false]).show(
            ui,
            |ui| {
                ui.set_width(230.0);
                let all = format!("Collection ({})", self.collection.tracks.len());
                if ui.selectable_label(self.collection_view.node.is_none(), all).clicked() {
                    self.collection_view.node = None;
                    self.focus = Focus::Playlists;
                }
                let rows: Vec<(usize, u32, String, NodeKind, usize)> = self
                    .collection
                    .tree()
                    .into_iter()
                    .map(|(depth, n)| (depth, n.id, n.name.clone(), n.kind.clone(), n.tracks.len()))
                    .collect();
                // Rows inside a collapsed folder are skipped.
                let mut hidden_below: Option<usize> = None;
                for (depth, id, name, kind, count) in rows {
                    if hidden_below.is_some_and(|d| depth > d) {
                        continue;
                    }
                    hidden_below = None;
                    if kind == NodeKind::Folder && self.config.collapsed_folders.contains(&id) {
                        hidden_below = Some(depth);
                    }
                    self.tree_row(ui, depth, id, &name, kind, count);
                }
            },
        );
    }

    /// Writes hot cues, memory cues/loops and a tapped beat grid of loaded
    /// collection tracks into the collection. Call every frame.
    pub(super) fn save_track_edits(&mut self) {
        use deckhand_library::{Cue, CueKind, TempoMarker};
        for deck in &mut self.decks {
            if !std::mem::take(&mut deck.edited) {
                continue;
            }
            let Some(loaded) = deck.track.as_ref().filter(|t| t.from_collection) else { continue };
            let Some(stored) = self.collection.track(loaded.id) else { continue };
            // Names, colours and hot-loop ends the deck does not track are kept.
            let previous = |kind: CueKind, start: f64| {
                stored
                    .cues
                    .iter()
                    .find(|c| c.kind == kind && (c.start - start).abs() < 0.001)
                    .cloned()
            };
            let mut cues = Vec::new();
            for (n, hot) in loaded.hot_cues.iter().enumerate() {
                let Some(hot) = hot else { continue };
                let kind = CueKind::Hot(n as u8);
                let old = previous(kind, hot.seconds);
                cues.push(Cue {
                    kind,
                    start: hot.seconds,
                    end: old.as_ref().and_then(|c| c.end),
                    color: Some([hot.color.r(), hot.color.g(), hot.color.b()]),
                    name: old.map(|c| c.name).unwrap_or_default(),
                });
            }
            let memory = loaded.memory_cues.iter().map(|&s| (s, None));
            let loops = loaded.memory_loops.iter().map(|&(s, e)| (s, Some(e)));
            for (start, end) in memory.chain(loops) {
                let old = previous(CueKind::Memory, start);
                cues.push(Cue {
                    kind: CueKind::Memory,
                    start,
                    end,
                    color: old.as_ref().and_then(|c| c.color),
                    name: old.map(|c| c.name).unwrap_or_default(),
                });
            }
            // A changed BPM means the grid was tapped: one straight grid.
            let tapped = (stored.bpm != Some(loaded.bpm) && loaded.bpm > 0.0)
                .then(|| loaded.beats.first().map(|&(start, beat)| (start, beat)))
                .flatten();
            let bpm = loaded.bpm;
            self.collection.update_track(loaded.id, |t| {
                t.cues = cues;
                if let Some((start, beat)) = tapped {
                    t.bpm = Some(bpm);
                    t.tempo = vec![TempoMarker { start, bpm, beat: beat.clamp(1, 4) as u8 }];
                }
            });
        }
    }

    /// Adds a track to a playlist, asking first if it is already in it.
    fn request_add(&mut self, track: u32, playlist: u32) {
        let already = self.collection.node(playlist).is_some_and(|n| n.tracks.contains(&track));
        if already {
            self.collection_view.pending_duplicate = Some((track, playlist));
        } else {
            self.collection.add_to_playlist(playlist, &[track]);
            let name = self.collection.node(playlist).map_or_else(String::new, |n| n.name.clone());
            self.status = Some(format!("Added to {name}"));
        }
    }

    /// The track info editor; call every frame.
    pub(super) fn edit_dialog(&mut self, ctx: &egui::Context) {
        const NAMES: [&str; 6] = ["Title", "Artist", "Album", "Genre", "Label", "Comment"];
        let Some((id, fields)) = self.collection_view.editing.as_mut() else { return };
        let id = *id;
        let edited = self.collection.track(id).is_some_and(|t| !t.edited.is_empty());
        let (mut save, mut close, mut revert) = (false, false, false);
        egui::Window::new("Edit track info")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("track fields").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    for (name, value) in NAMES.iter().zip(fields.iter_mut()) {
                        ui.label(*name);
                        ui.add(egui::TextEdit::singleline(value).desired_width(320.0));
                        ui.end_row();
                    }
                });
                ui.label(
                    RichText::new("Changes stay in RofLoxBox; the audio file is not changed, and rescans keep your edits.")
                        .small()
                        .weak(),
                );
                ui.horizontal(|ui| {
                    save = ui.button("Save").clicked();
                    close = ui.button("Cancel").clicked();
                    if edited {
                        revert = ui
                            .button("Back to the file's tags")
                            .on_hover_text("Forget the edits and read the tags from the file again")
                            .clicked();
                    }
                });
            });
        if save {
            let [title, artist, album, genre, label, comment] = fields.clone();
            self.collection.edit_track(
                id,
                deckhand_library::TrackEdit {
                    title: Some(title.trim().to_owned()).filter(|t| !t.is_empty()),
                    artist: Some(artist.trim().to_owned()),
                    album: Some(album.trim().to_owned()),
                    genre: Some(genre.trim().to_owned()),
                    label: Some(label.trim().to_owned()),
                    comment: Some(comment.trim().to_owned()),
                },
            );
            // A loaded copy shows the new name too.
            if let Some(t) = self.collection.track(id) {
                let (title, artist) = (t.title.clone(), t.artist.clone());
                for deck in &mut self.decks {
                    if let Some(loaded) =
                        deck.track.as_mut().filter(|l| l.from_collection && l.id == id)
                    {
                        (loaded.title, loaded.artist) = (title.clone(), artist.clone());
                    }
                }
            }
        }
        if revert {
            self.collection.revert_edits(id);
            if let Some(path) = self.collection.track(id).map(|t| t.path.clone()) {
                self.add_music(vec![path]);
            }
        }
        if save || close || revert {
            self.collection_view.editing = None;
        }
    }

    /// The "already in this playlist" question; call every frame.
    pub(super) fn duplicate_dialog(&mut self, ctx: &egui::Context) {
        let Some((track, playlist)) = self.collection_view.pending_duplicate else { return };
        let title = self.collection.track(track).map_or_else(String::new, |t| t.title.clone());
        let name = self.collection.node(playlist).map_or_else(String::new, |n| n.name.clone());
        let mut answer = None;
        egui::Window::new("Already in the playlist")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!("\"{title}\" is already in \"{name}\". Add it again?"));
                ui.horizontal(|ui| {
                    if ui.button("Add again").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(false);
                    }
                });
            });
        if let Some(add) = answer {
            self.collection_view.pending_duplicate = None;
            if add {
                self.collection.add_to_playlist(playlist, &[track]);
                self.status = Some(format!("Added to {name} again"));
            }
        }
    }

    /// The folder new playlists go into: the selected one, or the selected
    /// playlist's folder.
    fn selected_folder(&self) -> Option<u32> {
        let node = self.collection.node(self.collection_view.node?)?;
        match node.kind {
            NodeKind::Folder => Some(node.id),
            NodeKind::Playlist => node.parent,
        }
    }

    fn tree_row(
        &mut self,
        ui: &mut Ui,
        depth: usize,
        id: u32,
        name: &str,
        kind: NodeKind,
        count: usize,
    ) {
        let indent = depth as f32 * 14.0;
        ui.horizontal(|ui| {
            ui.add_space(indent);
            if let Some((renaming, text)) = &mut self.collection_view.renaming
                && *renaming == id
            {
                let response = ui.text_edit_singleline(text);
                response.request_focus();
                if response.lost_focus() {
                    let name = text.trim().to_owned();
                    if !name.is_empty() {
                        self.collection.rename(id, &name);
                    }
                    self.collection_view.renaming = None;
                }
                return;
            }
            if kind == NodeKind::Folder {
                let open = !self.config.collapsed_folders.contains(&id);
                if crate::widgets::disclosure(ui, open)
                    .on_hover_text("Open or close the folder")
                    .clicked()
                {
                    if open {
                        self.config.collapsed_folders.push(id);
                    } else {
                        self.config.collapsed_folders.retain(|f| *f != id);
                    }
                    self.config.save();
                }
            } else {
                ui.add_space(14.0 + ui.spacing().item_spacing.x);
            }
            let label = match kind {
                NodeKind::Playlist => format!("♫ {name} ({count})"),
                NodeKind::Folder => name.to_owned(),
            };
            let selected = self.collection_view.node == Some(id);
            let response = ui.add(egui::Button::selectable(selected, label).truncate());
            if selected && self.reveal_playlist {
                response.scroll_to_me(Some(egui::Align::Center));
            }
            // Clicking a folder opens or closes it (and selects it, for new
            // playlists).
            if response.clicked() && kind == NodeKind::Folder {
                if !self.config.collapsed_folders.contains(&id) {
                    self.config.collapsed_folders.push(id);
                } else {
                    self.config.collapsed_folders.retain(|f| *f != id);
                }
                self.config.save();
            }
            if response.clicked() {
                self.collection_view.node = Some(id);
                self.focus = Focus::Playlists;
            }
            // Tracks dropped on a playlist are added to it.
            if kind == NodeKind::Playlist
                && let Some(track) = response.dnd_release_payload::<u32>()
            {
                self.request_add(*track, id);
            }
            response.context_menu(|ui| {
                if kind == NodeKind::Folder {
                    if ui.button("New playlist here").clicked() {
                        let new =
                            self.collection.create("New playlist", NodeKind::Playlist, Some(id));
                        self.collection_view.renaming = Some((new, "New playlist".into()));
                        ui.close();
                    }
                    if ui.button("New folder here").clicked() {
                        let new = self.collection.create("New folder", NodeKind::Folder, Some(id));
                        self.collection_view.renaming = Some((new, "New folder".into()));
                        ui.close();
                    }
                }
                if ui.button("Rename").clicked() {
                    self.collection_view.renaming = Some((id, name.to_owned()));
                    ui.close();
                }
                let folders: Vec<(u32, String)> = self
                    .collection
                    .tree()
                    .into_iter()
                    .filter(|(_, n)| n.kind == NodeKind::Folder && n.id != id)
                    .map(|(_, n)| (n.id, n.name.clone()))
                    .collect();
                ui.menu_button("Move to", |ui| {
                    if ui.button("Top level").clicked() {
                        self.collection.move_node(id, None);
                        ui.close();
                    }
                    for (folder, folder_name) in folders {
                        if ui.button(folder_name).clicked() {
                            self.collection.move_node(id, Some(folder));
                            ui.close();
                        }
                    }
                });
                let what =
                    if kind == NodeKind::Folder { "folder and its playlists" } else { "playlist" };
                if ui.button(format!("Delete {what}")).clicked() {
                    self.collection.delete(id);
                    if self.collection_view.node.is_some_and(|n| self.collection.node(n).is_none())
                    {
                        self.collection_view.node = None;
                    }
                    ui.close();
                }
            });
        });
    }

    /// The collection's tracks for the list: playlist, search and sort applied.
    pub(super) fn collection_rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .collection_entries()
            .into_iter()
            .filter_map(|(id, position, playlist)| {
                self.collection.track(id).map(|t| (t, position, playlist))
            })
            .map(|(t, position, playlist)| Row {
                id: t.id,
                entry: playlist.then_some(position),
                position,
                title: t.title.clone(),
                artist: t.artist.clone(),
                album: t.album.clone(),
                genre: t.genre.clone(),
                label: t.label.clone(),
                bpm: t.bpm,
                key: t.key.clone(),
                duration: t.duration,
                year: t.year,
                rating: t.rating,
                plays: t.play_count,
                added: date(t.added),
                bitrate: t.bitrate,
                comment: t.comment.clone(),
            })
            .collect();
        sort_rows(&mut rows, self.config.sort);
        rows
    }

    /// The track list; returns a (track, deck) to load.
    fn collection_tracks(&mut self, ui: &mut Ui, stroke: Stroke) -> Option<(u32, usize)> {
        let mut load = None;
        let deck_count = self.deck_count();
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
        });
        if self.collection.tracks.is_empty() {
            ui.add_space(20.0);
            ui.label("Your collection is empty. Add a folder of music, add files, or drop them on this window.");
            return load;
        }
        let rows = self.collection_rows();
        let in_playlist = self
            .collection_view
            .node
            .filter(|n| self.collection.node(*n).is_some_and(|n| n.kind == NodeKind::Playlist));
        let playlists: Vec<(u32, String)> = self
            .collection
            .tree()
            .into_iter()
            .filter(|(_, n)| n.kind == NodeKind::Playlist)
            .map(|(_, n)| (n.id, n.name.clone()))
            .collect();
        let paths: std::collections::HashMap<u32, PathBuf> = rows
            .iter()
            .filter_map(|r| self.collection.track(r.id).map(|t| (r.id, t.path.clone())))
            .collect();
        let loaded: Vec<Option<u32>> = self
            .decks
            .iter()
            .map(|d| d.track.as_ref().filter(|t| t.from_collection).map(|t| t.id))
            .collect();
        let free = self.free_deck(&loaded);
        let options = TableOptions {
            selected: self.selected,
            selected_position: self.selected_position,
            reveal_selected: std::mem::take(&mut self.reveal_selected),
            loaded: &loaded,
            draggable: true,
            editable_rating: true,
        };
        let mut actions: Vec<TrackAction> = Vec::new();
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
                        let id = row.id;
                        for deck in 0..deck_count {
                            if ui.button(format!("Load to deck {}", deck + 1)).clicked() {
                                actions.push(TrackAction::Load(id, deck));
                                ui.close();
                            }
                        }
                        ui.separator();
                        ui.menu_button("Add to playlist", |ui| {
                            if playlists.is_empty() {
                                ui.label("No playlists yet");
                            }
                            for (playlist, name) in &playlists {
                                if ui.button(name).clicked() {
                                    actions.push(TrackAction::AddTo(id, *playlist));
                                    ui.close();
                                }
                            }
                        });
                        if let Some(playlist) = in_playlist
                            && ui.button("Remove from playlist").clicked()
                        {
                            actions.push(TrackAction::RemoveFrom(row.entry, playlist));
                            ui.close();
                        }
                        if let Some(path) = paths.get(&id)
                            && ui.button(reveal_label()).clicked()
                        {
                            reveal(path);
                            ui.close();
                        }
                        if ui.button("Edit track info…").clicked() {
                            actions.push(TrackAction::Edit(id));
                            ui.close();
                        }
                        if ui.button("Remove from collection").clicked() {
                            actions.push(TrackAction::Forget(id));
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
            (self.selected, self.selected_position) = (Some(id), Some(position));
            self.focus = Focus::Tracks;
        }
        if let (Some(id), Some(deck)) = (output.double_clicked, free) {
            load = Some((id, deck));
        }
        if let Some((id, stars)) = output.rated {
            self.collection.update_track(id, |t| t.rating = stars);
        }
        for action in actions {
            match action {
                TrackAction::Load(id, deck) => load = Some((id, deck)),
                TrackAction::AddTo(id, playlist) => self.request_add(id, playlist),
                TrackAction::RemoveFrom(entry, playlist) => {
                    if let Some(entry) = entry {
                        self.collection.remove_playlist_entry(playlist, entry);
                    }
                }
                TrackAction::Forget(id) => self.collection.remove_tracks(&[id]),
                TrackAction::Edit(id) => {
                    if let Some(t) = self.collection.track(id) {
                        let fields =
                            [&t.title, &t.artist, &t.album, &t.genre, &t.label, &t.comment];
                        self.collection_view.editing = Some((id, fields.map(Clone::clone)));
                    }
                }
            }
        }
        load
    }
}

enum TrackAction {
    Load(u32, usize),
    AddTo(u32, u32),
    /// (playlist position, playlist).
    RemoveFrom(Option<usize>, u32),
    Forget(u32),
    Edit(u32),
}

/// Unix seconds → `YYYY-MM-DD` (UTC).
fn date(unix: u64) -> String {
    if unix == 0 {
        return String::new();
    }
    // Civil date from days (Howard Hinnant's algorithm).
    let days = (unix / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn deckhand_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn reveal_label() -> &'static str {
    if cfg!(windows) { "Show in Explorer" } else { "Show in Finder" }
}

/// Opens the file manager with the file selected.
fn reveal(path: &std::path::Path) {
    let _ = if cfg!(windows) {
        std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn()
    } else {
        std::process::Command::new("open").arg("-R").arg(path).spawn()
    };
}
