//! The browser for deckhand's own collection: folders of playlists on the
//! left, tracks on the right, adding music by folder, file or drag and drop.

use std::path::PathBuf;

use deckhand_library::{CueKind, NodeKind, Scan};
use eframe::egui::{self, RichText, Stroke, Ui};

use super::{DeckhandApp, Focus, WARN};
use crate::browser::{Row, Sort, TableOptions, TrackColumn, sort_rows, track_table};

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
    /// The key that opened rekordbox's library, to keep in the settings.
    rekordbox_key: Option<std::sync::mpsc::Receiver<(String, i64)>>,
    /// rekordbox's change count of the library being read.
    rekordbox_count: Option<i64>,
    /// Entries of the playlist shown being dragged: (playlist, positions).
    pub dragged_entries: Option<(u32, Vec<usize>)>,
    /// "Back up your library first" is being asked before writing to
    /// rekordbox is allowed.
    confirm_rekordbox_write: bool,
    /// Quitting waits for rekordbox to close, with when it was last looked
    /// for.
    quit_waiting: Option<std::time::Instant>,
    /// Changes being written into rekordbox's library, with the
    /// fingerprint of what is written and whether the save started itself.
    rekordbox_save: Option<(RekordboxSave, u64, bool)>,
    /// When changes go into rekordbox's library on their own.
    rekordbox_due: Option<std::time::Instant>,
    /// The fingerprint of what rekordbox's library has, as far as known.
    rekordbox_print: Option<u64>,
    /// Saving on its own stopped after a failure, until the library is
    /// read again or saved by hand.
    rekordbox_paused: bool,
    /// "rekordbox is open" was said for the changes waiting.
    rekordbox_waiting_said: bool,
    /// BPM, beat grid and key analysis running, with (tracks given a grid
    /// or key, files that could not be read) so far.
    analysis: Option<(crate::analyze::Analysis, usize, usize)>,
    /// Files dropped on a playlist that go into it once the scan reading
    /// them is done: (playlist, files).
    pending_adds: Vec<(u32, Vec<PathBuf>)>,
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

    /// Adds files to a playlist; files not in the collection yet are read
    /// in first and added when that is done.
    pub(super) fn add_files_to_playlist(&mut self, paths: Vec<PathBuf>, playlist: u32) {
        let (known, new): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|p| self.collection.tracks.iter().any(|t| &t.path == p));
        let ids: Vec<u32> = known
            .iter()
            .filter_map(|p| self.collection.tracks.iter().find(|t| &t.path == p).map(|t| t.id))
            .collect();
        if !ids.is_empty() {
            self.add_many(&ids, playlist);
        }
        if !new.is_empty() {
            self.collection_view.pending_adds.push((playlist, new.clone()));
            self.add_music(new);
        }
    }

    /// Rescans every added folder for new and changed files.
    fn rescan(&mut self) {
        let folders = self.collection.folders.clone();
        self.add_music(folders);
    }

    /// Analyzes the tracks that have no beat grid or no key.
    pub(super) fn analyze_missing(&mut self) {
        let ids: Vec<u32> = self
            .collection
            .tracks
            .iter()
            .filter(|t| t.tempo.is_empty() || t.key.is_none())
            .map(|t| t.id)
            .collect();
        self.analyze(ids, false);
    }

    /// Analyzes tracks (BPM, beat grid, key) in the background; `overwrite`
    /// replaces what they have instead of only filling in what is missing.
    fn analyze(&mut self, ids: Vec<u32>, overwrite: bool) {
        if self.collection_view.analysis.is_some() {
            self.status = Some("An analysis is already running".into());
            return;
        }
        let tracks: Vec<(u32, PathBuf)> = ids
            .into_iter()
            .filter_map(|id| self.collection.track(id))
            .filter(|t| t.path.is_file())
            .map(|t| (t.id, t.path.clone()))
            .collect();
        if tracks.is_empty() {
            self.status = Some("Nothing to analyze (or the files are missing)".into());
            return;
        }
        self.collection_view.analysis =
            Some((crate::analyze::Analysis::start(tracks, overwrite), 0, 0));
    }

    /// Stores finished analyses in the collection and on decks holding
    /// the track.
    fn poll_analysis(&mut self, ctx: &egui::Context) {
        use deckhand_library::TempoMarker;
        let Some((analysis, found, failed)) = self.collection_view.analysis.as_mut() else {
            return;
        };
        while let Ok(r) = analysis.results.try_recv() {
            if r.failed {
                *failed += 1;
                continue;
            }
            let mut changed = false;
            self.collection.update_track(r.id, |t| {
                if let Some(g) = r.grid
                    && (r.overwrite || t.tempo.is_empty())
                {
                    t.bpm = Some(g.bpm);
                    t.tempo =
                        vec![TempoMarker { start: g.first_beat, bpm: g.bpm, beat: g.first_number }];
                    changed = true;
                }
                if let Some(k) = r.key
                    && (r.overwrite || t.key.is_none())
                {
                    t.key = Some(k.camelot());
                    changed = true;
                }
            });
            if !changed {
                continue;
            }
            *found += 1;
            let Some(stored) = self.collection.track(r.id) else { continue };
            for deck in &mut self.decks {
                if let Some(loaded) =
                    deck.track.as_mut().filter(|t| t.from_collection && t.id == r.id)
                {
                    loaded.beats = stored.beats(loaded.duration);
                    loaded.bpm = stored.bpm.unwrap_or(loaded.bpm);
                    loaded.key = stored.key.clone();
                    deck.remember_grid();
                }
            }
        }
        if analysis.is_finished() {
            let (done, total) =
                (analysis.done.load(std::sync::atomic::Ordering::Relaxed), analysis.total);
            let unread = if *failed > 0 {
                format!(", {failed} file(s) could not be read")
            } else {
                String::new()
            };
            self.status = Some(format!(
                "Analysis: {found} of {total} track(s) got a grid or key{unread}{}",
                if done < total { " (cancelled)" } else { "" }
            ));
            self.collection_view.analysis = None;
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }

    /// The rekordbox library this computer has (or the one Preferences
    /// name).
    fn rekordbox_db(&self) -> Option<PathBuf> {
        self.config.prefs.rekordbox_db.clone().or_else(crate::rekordbox_db::default_path)
    }

    /// At start: rekordbox's library is read again, unless deckhand has
    /// changes it doesn't have yet (they go in first).
    pub(super) fn rekordbox_at_start(&mut self) {
        if self.collection.rekordbox_unsaved && self.config.prefs.rekordbox_write {
            self.collection_view.rekordbox_due = Some(std::time::Instant::now());
            self.status = Some(
                "rekordbox's library doesn't have deckhand's last changes yet: saving them \
                 instead of reading it"
                    .into(),
            );
        } else {
            self.import_rekordbox_db(false);
        }
    }

    /// Reads rekordbox's library in the background, like an XML export;
    /// `asked`: from a button (else at start, quietly when it isn't there).
    pub(super) fn import_rekordbox_db(&mut self, asked: bool) {
        let Some(path) = self.rekordbox_db().filter(|p| asked || p.is_file()) else {
            if asked {
                self.collection_view.error = Some("No rekordbox library found".into());
            }
            return;
        };
        let saved = self.config.prefs.rekordbox_key.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let (key_tx, key_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let read = crate::rekordbox_db::open_any(&path, &saved).map(|opened| {
                let _ = key_tx.send((opened.key, opened.count));
                opened.library
            });
            let _ = tx.send(read);
        });
        self.collection_view.xml = Some(rx);
        self.collection_view.rekordbox_key = Some(key_rx);
        self.status = Some("Reading the rekordbox library…".into());
    }

    /// The Library page of Preferences; returns whether a setting changed.
    pub(super) fn library_prefs(&mut self, ui: &mut Ui) -> bool {
        let p = &self.config.prefs;
        let before = (
            p.rekordbox_key.clone(),
            p.rekordbox_db.clone(),
            p.rekordbox_at_start,
            p.rekordbox_write,
            p.rekordbox_autosave,
        );
        ui.label(RichText::new("rekordbox library").strong().size(15.0));
        let path = self.rekordbox_db();
        let found = path.as_ref().is_some_and(|p| p.is_file());
        ui.horizontal(|ui| {
            let text = path.as_ref().map_or("not found".to_owned(), |p| p.display().to_string());
            let text = RichText::new(text).small();
            ui.label(if found { text.weak() } else { text.color(WARN) });
        });
        ui.horizontal(|ui| {
            if ui.button("Choose…").clicked()
                && let Some(picked) = rfd::FileDialog::new()
                    .set_title("rekordbox's library (master.db)")
                    .add_filter("rekordbox library", &["db"])
                    .pick_file()
            {
                self.config.prefs.rekordbox_db = Some(picked);
            }
            if ui
                .add_enabled(self.config.prefs.rekordbox_db.is_some(), egui::Button::new("Default"))
                .clicked()
            {
                self.config.prefs.rekordbox_db = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Key:");
            ui.add(
                egui::TextEdit::singleline(&mut self.config.prefs.rekordbox_key)
                    .hint_text("fetched from pyrekordbox")
                    .password(true)
                    .desired_width(320.0),
            );
        });
        ui.label(
            RichText::new(
                "rekordbox encrypts its library. deckhand doesn't come with the key: the first \
                 time, it downloads the one the pyrekordbox project publishes and keeps it here. \
                 Should that ever fail, enter the key yourself.",
            )
            .small()
            .weak(),
        );
        ui.checkbox(&mut self.config.prefs.rekordbox_at_start, "Update from rekordbox at start");
        ui.label(
            RichText::new(
                "rekordbox's tracks, tags, beat grids, hot cues, memory cues, loops, ratings, \
                 colours and playlists come into the collection, with the playlists in the \
                 rekordbox folder (replacing the ones of the last import). Intelligent \
                 playlists are not read.",
            )
            .small()
            .weak(),
        );
        let mut writable = self.config.prefs.rekordbox_write;
        if ui.checkbox(&mut writable, "Write changes back to rekordbox").changed() {
            if writable {
                self.collection_view.confirm_rekordbox_write = true;
            } else {
                self.config.prefs.rekordbox_write = false;
            }
        }
        if self.config.prefs.rekordbox_write {
            ui.indent("rekordbox autosave", |ui| {
                ui.checkbox(&mut self.config.prefs.rekordbox_autosave, "Save automatically")
                    .on_hover_text(
                        "A few seconds after a change, in the background; while rekordbox is \
                         open, once it's closed; at quit, what's left",
                    );
            });
        }
        ui.label(
            RichText::new(
                "Off, rekordbox's files are never changed. On, saving (automatically, or with \
                 \"Save changes to rekordbox\") makes rekordbox's library what deckhand has: \
                 tags, ratings, colours, hot cues, \
                 memory cues, loops and beat grids of its tracks, and the playlists in the \
                 rekordbox folder (made, renamed, moved, reordered or deleted). Tracks put into \
                 those playlists are added to rekordbox. rekordbox has to be closed, and must \
                 not have changed its library since deckhand read it. deckhand keeps a copy of \
                 the library from before each save.",
            )
            .small()
            .weak(),
        );
        if self.collection_view.confirm_rekordbox_write {
            self.confirm_rekordbox_write(ui.ctx());
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    found && self.collection_view.xml.is_none(),
                    egui::Button::new("Read the rekordbox library now"),
                )
                .clicked()
            {
                self.import_rekordbox_db(true);
            }
            self.save_to_rekordbox_button(ui);
        });
        let p = &self.config.prefs;
        (
            p.rekordbox_key.clone(),
            p.rekordbox_db.clone(),
            p.rekordbox_at_start,
            p.rekordbox_write,
            p.rekordbox_autosave,
        ) != before
    }

    /// Asks to back up rekordbox's library before writing to it is allowed.
    fn confirm_rekordbox_write(&mut self, ctx: &egui::Context) {
        let backups = rekordbox_backups();
        let modal = egui::Modal::new(egui::Id::new("confirm rekordbox write")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label(RichText::new("Back up your rekordbox library first").strong().size(16.0));
            ui.add_space(4.0);
            ui.label(
                "deckhand will change rekordbox's own library. Before turning this on, make a \
                 backup in rekordbox: File → Library → Backup Library.",
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "deckhand also keeps a copy of the library from before each save, in {}.",
                    backups.display()
                ))
                .small()
                .weak(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("I have a backup: allow writing").clicked() {
                    self.config.prefs.rekordbox_write = true;
                    self.collection_view.confirm_rekordbox_write = false;
                }
                if ui.button("Cancel").clicked() {
                    self.collection_view.confirm_rekordbox_write = false;
                }
            });
        });
        if modal.should_close() {
            self.collection_view.confirm_rekordbox_write = false;
        }
    }

    /// "Save changes to rekordbox", when writing to it is allowed.
    fn save_to_rekordbox_button(&mut self, ui: &mut Ui) {
        if !self.config.prefs.rekordbox_write {
            return;
        }
        let path = self.rekordbox_db().filter(|p| p.is_file());
        if ui
            .add_enabled(
                path.is_some() && self.collection_view.rekordbox_save.is_none(),
                egui::Button::new("Save changes to rekordbox"),
            )
            .on_hover_text(
                "Writes tags, cues, beat grids and the rekordbox folder's playlists into \
                 rekordbox's library. Close rekordbox first.",
            )
            .clicked()
            && let Some(path) = path
        {
            self.save_to_rekordbox(path, false);
        }
    }

    /// What of the collection rekordbox's library gets, ready to save.
    fn rekordbox_snapshot(&self) -> crate::rekordbox_save::Snapshot {
        crate::rekordbox_save::Snapshot {
            tracks: self.collection.tracks.clone(),
            playlists: self.collection.rekordbox_playlists(),
            read_count: self.collection.rekordbox_count,
        }
    }

    /// A fingerprint of what saving into rekordbox writes, to tell cheaply
    /// whether that changed.
    fn rekordbox_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        for t in &self.collection.tracks {
            (&t.path, &t.title, &t.artist, &t.album, &t.genre, &t.label, &t.comment).hash(&mut h);
            (t.rating, t.color, &t.key, t.year, t.track_number).hash(&mut h);
            for cue in &t.cues {
                let kind = match cue.kind {
                    CueKind::Hot(slot) => slot,
                    CueKind::Memory => u8::MAX,
                };
                (kind, cue.start.to_bits(), cue.end.map(f64::to_bits)).hash(&mut h);
                (cue.color, &cue.name).hash(&mut h);
            }
            for m in &t.tempo {
                (m.start.to_bits(), m.bpm.to_bits(), m.beat).hash(&mut h);
            }
        }
        for n in self.collection.rekordbox_playlists().iter().flatten() {
            (n.id, &n.name, n.kind == NodeKind::Folder, n.parent, &n.tracks).hash(&mut h);
        }
        h.finish()
    }

    /// Notes a change of the collection: when it is one rekordbox's library
    /// would get, saving it on its own is due shortly.
    fn note_rekordbox_change(&mut self) {
        if !self.config.prefs.rekordbox_write || self.collection.rekordbox_count.is_none() {
            return;
        }
        let print = self.rekordbox_fingerprint();
        if Some(print) != self.collection_view.rekordbox_print {
            self.collection.set_rekordbox_unsaved(true);
            // A moment after the last change, so edits come in one save.
            self.collection_view.rekordbox_due =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
        }
    }

    /// Starts saving into rekordbox when it is due (in the background).
    fn auto_save_rekordbox(&mut self, ctx: &egui::Context) {
        let view = &self.collection_view;
        let Some(due) = view.rekordbox_due else { return };
        let prefs = &self.config.prefs;
        if !prefs.rekordbox_write || !prefs.rekordbox_autosave || view.rekordbox_paused {
            return;
        }
        let now = std::time::Instant::now();
        if due > now {
            ctx.request_repaint_after(due - now);
            return;
        }
        // Not while it is being read or saved; not before it was read.
        if view.rekordbox_save.is_some()
            || view.xml.is_some()
            || self.collection.rekordbox_count.is_none()
        {
            return;
        }
        let Some(path) = self.rekordbox_db().filter(|p| p.is_file()) else { return };
        self.collection_view.rekordbox_due = None;
        self.save_to_rekordbox(path, true);
    }

    /// Whether quitting now would leave changes rekordbox's library doesn't
    /// have, as rekordbox is open.
    pub(super) fn rekordbox_blocks_quit(&self) -> bool {
        let prefs = &self.config.prefs;
        let view = &self.collection_view;
        prefs.rekordbox_write
            && prefs.rekordbox_autosave
            && !view.rekordbox_paused
            && (self.collection.rekordbox_unsaved
                || view.rekordbox_due.is_some()
                || view.rekordbox_save.is_some())
            && crate::rekordbox_db::rekordbox_running()
    }

    pub(super) fn ask_to_close_rekordbox(&mut self) {
        self.collection_view.quit_waiting = Some(std::time::Instant::now());
    }

    /// While quitting waits: asks to close rekordbox, and quits (saving)
    /// once it is closed.
    pub(super) fn quit_waiting_for_rekordbox(&mut self, ctx: &egui::Context) {
        let Some(looked) = self.collection_view.quit_waiting else { return };
        let second = std::time::Duration::from_secs(1);
        if looked.elapsed() >= second {
            if !crate::rekordbox_db::rekordbox_running() {
                self.quit();
            }
            self.collection_view.quit_waiting = Some(std::time::Instant::now());
        }
        ctx.request_repaint_after(second);
        let mut quit = false;
        let mut stay = false;
        let modal = egui::Modal::new(egui::Id::new("quit waits for rekordbox")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.label(RichText::new("Unsaved changes for rekordbox").strong().size(16.0));
            ui.add_space(4.0);
            ui.label(
                "rekordbox's library doesn't have your latest changes yet, and they can't be \
                 saved while rekordbox is open. Please close rekordbox: RofLoxBox then saves \
                 them and quits.",
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Waiting for rekordbox to close…").weak());
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                stay = ui.button("Don't quit").clicked();
                quit = ui
                    .button("Quit anyway")
                    .on_hover_text(
                        "The changes stay in RofLoxBox and go into rekordbox's library at its \
                         next start, once rekordbox is closed",
                    )
                    .clicked();
            });
        });
        if quit {
            self.quit();
        }
        if stay || modal.should_close() {
            self.collection_view.quit_waiting = None;
        }
    }

    /// At quit: changes rekordbox doesn't have yet are saved first (while
    /// it is closed), waiting for a save already running.
    pub(super) fn save_to_rekordbox_at_quit(&mut self) {
        if !self.config.prefs.rekordbox_write || !self.config.prefs.rekordbox_autosave {
            return;
        }
        if let Some((rx, print, auto)) = self.collection_view.rekordbox_save.take()
            && let Ok(result) = rx.recv_timeout(std::time::Duration::from_secs(60))
        {
            self.rekordbox_saved(result, print, auto);
        }
        if self.collection_view.rekordbox_due.is_some()
            && !self.collection_view.rekordbox_paused
            && self.collection.rekordbox_count.is_some()
            && !crate::rekordbox_db::rekordbox_running()
            && let Some(path) = self.rekordbox_db().filter(|p| p.is_file())
        {
            self.collection_view.rekordbox_due = None;
            let print = self.rekordbox_fingerprint();
            let result = save_into_rekordbox(
                &path,
                &self.config.prefs.rekordbox_key,
                &self.rekordbox_snapshot(),
            );
            self.rekordbox_saved(result, print, true);
        }
        let _ = self.collection.save();
    }

    /// Writes the collection's changes into rekordbox's library in the
    /// background; `auto`: started by itself, so quiet about it.
    fn save_to_rekordbox(&mut self, path: PathBuf, auto: bool) {
        let snapshot = self.rekordbox_snapshot();
        let print = self.rekordbox_fingerprint();
        let key = self.config.prefs.rekordbox_key.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(save_into_rekordbox(&path, &key, &snapshot));
        });
        self.collection_view.rekordbox_save = Some((rx, print, auto));
        if !auto {
            self.status = Some("Saving changes to rekordbox…".into());
        }
    }

    /// A save into rekordbox finished: `print` is the fingerprint of what
    /// it wrote.
    fn rekordbox_saved(
        &mut self,
        result: anyhow::Result<crate::rekordbox_save::Saved>,
        print: u64,
        auto: bool,
    ) {
        let view = &mut self.collection_view;
        match result {
            Ok(saved) => {
                if !saved.is_empty() {
                    self.collection.saved_to_rekordbox(&saved.ids, saved.count);
                }
                view.rekordbox_print = Some(print);
                view.rekordbox_paused = false;
                view.rekordbox_waiting_said = false;
                // Changes made while it saved are still to come.
                if view.rekordbox_due.is_none() {
                    self.collection.set_rekordbox_unsaved(false);
                }
                let parts: Vec<String> = [
                    (saved.tracks, "track(s) retagged"),
                    (saved.cues, "with new cues"),
                    (saved.grids, "with new beat grids"),
                    (saved.added, "added"),
                    (saved.playlists, "playlist change(s)"),
                ]
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, what)| format!("{n} {what}"))
                .collect();
                if !parts.is_empty() {
                    self.status = Some(format!("Saved to rekordbox: {}", parts.join(", ")));
                } else if !auto {
                    self.status = Some("rekordbox already has these changes".into());
                }
            }
            Err(err) if auto && err.is::<crate::rekordbox_save::RekordboxRunning>() => {
                // Tried again until rekordbox is closed.
                view.rekordbox_due =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
                if !view.rekordbox_waiting_said {
                    view.rekordbox_waiting_said = true;
                    self.status = Some(
                        "rekordbox is open: deckhand's changes go into its library once it's \
                         closed"
                            .into(),
                    );
                }
            }
            Err(err) if auto => {
                view.rekordbox_paused = true;
                view.error = Some(format!(
                    "Saving into rekordbox stopped: {err:#}. Changes stay in deckhand; \
                     \"Save changes to rekordbox\" tries again."
                ));
            }
            Err(err) => view.error = Some(format!("Not saved to rekordbox: {err:#}")),
        }
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
        self.poll_analysis(ctx);
        let finished = self
            .collection_view
            .rekordbox_save
            .as_ref()
            .and_then(|(rx, print, auto)| rx.try_recv().ok().map(|r| (r, *print, *auto)));
        if let Some((result, print, auto)) = finished {
            self.collection_view.rekordbox_save = None;
            self.rekordbox_saved(result, print, auto);
            // What the save changed (new playlists' ids) is no change of
            // the user's.
            let _ = self.collection.save();
        } else if self.collection_view.rekordbox_save.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        if let Some(key) =
            self.collection_view.rekordbox_key.as_ref().and_then(|rx| rx.try_recv().ok())
        {
            self.collection_view.rekordbox_key = None;
            let (key, count) = key;
            self.collection_view.rekordbox_count = Some(count);
            if key != self.config.prefs.rekordbox_key {
                self.config.prefs.rekordbox_key = key;
                self.config.save();
            }
        }
        if let Some(result) = self.collection_view.xml.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.collection_view.xml = None;
            match result {
                Ok(library) => {
                    let s = self.collection.import_rekordbox(library);
                    // Read from rekordbox's own library: what saving into
                    // it compares with.
                    self.collection.rekordbox_count = self.collection_view.rekordbox_count.take();
                    if self.collection.rekordbox_count.is_some() {
                        // The collection is rekordbox's again.
                        self.collection.set_rekordbox_unsaved(false);
                        self.collection_view.rekordbox_paused = false;
                        self.collection_view.rekordbox_print = None;
                    }
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
                    // Dropped files now in the collection go into their playlist.
                    for (playlist, paths) in std::mem::take(&mut view.pending_adds) {
                        let ids: Vec<u32> = paths
                            .iter()
                            .filter_map(|p| {
                                self.collection.tracks.iter().find(|t| &t.path == p).map(|t| t.id)
                            })
                            .collect();
                        self.add_many(&ids, playlist);
                    }
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
        if self.collection.is_dirty() {
            self.note_rekordbox_change();
            if let Err(err) = self.collection.save() {
                self.collection_view.error = Some(format!("{err:#}"));
            }
        }
        self.auto_save_rekordbox(ctx);
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
    }

    /// Adding music, importing and new playlists (the COLLECTION menu).
    pub(super) fn collection_tools(&mut self, ui: &mut Ui) {
        if ui.button("Add a folder of music…").clicked() {
            ui.close();
            if let Some(folder) =
                rfd::FileDialog::new().set_title("Add a folder of music").pick_folder()
            {
                self.add_music(vec![folder]);
            }
        }
        if ui.button("Add files…").clicked() {
            ui.close();
            if let Some(files) = rfd::FileDialog::new()
                .set_title("Add music files")
                .add_filter("Audio", &deckhand_library::AUDIO_EXTENSIONS)
                .pick_files()
            {
                self.add_music(files);
            }
        }
        if ui
            .button("Rescan added folders")
            .on_hover_text("Look for new and changed files in the added folders")
            .clicked()
        {
            ui.close();
            self.rescan();
        }
        if ui
            .button("Import rekordbox library")
            .on_hover_text(
                "Reads rekordbox's own library on this computer (read only): tracks, playlists, \
                 beat grids, cues, ratings and colours. Settings in Preferences → Library.",
            )
            .clicked()
        {
            ui.close();
            self.import_rekordbox_db(true);
        }
        self.save_to_rekordbox_button(ui);
        if ui
            .button("Import rekordbox XML…")
            .on_hover_text(
                "In rekordbox: File → Export Collection in xml format. Brings tracks, playlists, beat grids, hot cues, memory cues, ratings and colours.",
            )
            .clicked()
        {
            ui.close();
            if let Some(file) = rfd::FileDialog::new()
                .set_title("Choose a rekordbox XML export")
                .add_filter("rekordbox XML", &["xml"])
                .pick_file()
            {
                self.import_xml(file);
            }
        }
        if ui
            .button("Analyze tracks without a grid or key")
            .on_hover_text(
                "Works out BPM, beat grid and key where a track has none. Grids from rekordbox \
                 and ones you edited are kept; right-click a track to analyze it again.",
            )
            .clicked()
        {
            ui.close();
            self.analyze_missing();
        }
        ui.separator();
        let parent = self.selected_folder();
        if ui.button("New playlist").clicked() {
            ui.close();
            let id = self.collection.create("New playlist", NodeKind::Playlist, parent);
            self.collection_view.renaming = Some((id, "New playlist".into()));
            self.tree.collection_open = true;
        }
        if ui.button("New folder").clicked() {
            ui.close();
            let id = self.collection.create("New folder", NodeKind::Folder, parent);
            self.collection_view.renaming = Some((id, "New folder".into()));
            self.tree.collection_open = true;
        }
    }

    /// Progress of reading music or an XML file, and errors.
    pub(super) fn collection_status(&mut self, ui: &mut Ui) {
        if self.collection_view.xml.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Reading rekordbox XML…").small());
            });
        }
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
        if let Some((analysis, _, _)) = &self.collection_view.analysis {
            ui.horizontal(|ui| {
                ui.spinner();
                let done = analysis.done.load(std::sync::atomic::Ordering::Relaxed);
                ui.label(RichText::new(format!("Analyzing {done}/{}…", analysis.total)).small());
                if ui.small_button("Cancel").clicked() {
                    analysis.cancel();
                }
            });
        }
        if let Some(err) = &self.collection_view.error {
            ui.colored_label(WARN, err);
        }
    }

    /// The collection's playlists and folders in the sidebar tree.
    pub(super) fn collection_tree_rows(&mut self, ui: &mut Ui) {
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
            self.tree_row(ui, depth + 1, id, &name, kind, count);
        }
    }

    /// Writes hot cues, memory cues/loops and an edited (tapped, shifted…)
    /// beat grid of loaded collection tracks into the collection. Call
    /// every frame.
    pub(super) fn save_track_edits(&mut self) {
        use deckhand_library::{Cue, CueKind, TempoMarker};
        for deck in &mut self.decks {
            // Track gain, measured on load or changed on the deck.
            if let Some(loaded) = deck.track.as_ref().filter(|t| t.from_collection)
                && self
                    .collection
                    .track(loaded.id)
                    .is_some_and(|t| t.gain_db != Some(loaded.gain_db))
            {
                let gain = loaded.gain_db;
                self.collection.update_track(loaded.id, |t| t.gain_db = Some(gain));
            }
            // Detected on load: fills in only what the collection lacks.
            if std::mem::take(&mut deck.analysed)
                && let Some(loaded) = deck.track.as_ref().filter(|t| t.from_collection)
            {
                let first = loaded.beats.first().copied();
                let (bpm, key) = (loaded.bpm, loaded.key.clone());
                self.collection.update_track(loaded.id, |t| {
                    if let Some((start, beat)) = first.filter(|_| t.tempo.is_empty() && bpm > 0.0) {
                        t.bpm = Some(bpm);
                        t.tempo = vec![TempoMarker { start, bpm, beat: beat.clamp(1, 4) as u8 }];
                    }
                    if t.key.is_none() {
                        t.key = key;
                    }
                });
            }
            if !std::mem::take(&mut deck.edited) {
                continue;
            }
            let grid_edited = std::mem::take(&mut deck.grid_edited);
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
            // Grid edits always leave one straight grid: one marker.
            let tapped = (grid_edited && loaded.bpm > 0.0)
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
    pub(super) fn request_add(&mut self, track: u32, playlist: u32) {
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
        let sticks: Vec<(PathBuf, String)> =
            self.libraries.iter().map(|l| (l.root.clone(), l.name())).collect();
        let mut export_to: Option<PathBuf> = None;
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
            let selected =
                self.source == super::Source::Collection && self.collection_view.node == Some(id);
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
                self.select_entry(super::library_tree::TreeEntry::Collection(Some(id)));
                self.focus = Focus::Playlists;
            }
            // Tracks dropped on a playlist are added to it; it is outlined while they are over it.
            if kind == NodeKind::Playlist
                && response.dnd_hover_payload::<crate::browser::TrackDrag>().is_some()
            {
                ui.painter().rect_stroke(
                    response.rect,
                    3.0,
                    egui::Stroke::new(2.0_f32, ui.visuals().selection.bg_fill),
                    egui::StrokeKind::Inside,
                );
            }
            if kind == NodeKind::Playlist
                && let Some(drag) = response.dnd_release_payload::<crate::browser::TrackDrag>()
            {
                self.drop_on_playlist(&drag, id);
            }
            response.context_menu(|ui| {
                if kind == NodeKind::Playlist {
                    ui.menu_button("Export to USB", |ui| {
                        for (root, stick) in &sticks {
                            if ui.button(stick).clicked() {
                                export_to = Some(root.clone());
                                ui.close();
                            }
                        }
                        if !sticks.is_empty() {
                            ui.separator();
                        }
                        if ui
                            .button("Choose folder…")
                            .on_hover_text("A stick or folder without an export gets a new one")
                            .clicked()
                        {
                            ui.close();
                            export_to = rfd::FileDialog::new()
                                .set_title("Choose the USB stick (or a folder) to export to")
                                .pick_folder();
                        }
                    });
                    ui.separator();
                }
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
        if let Some(root) = export_to {
            self.start_export(id, root);
        }
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
                bpm: t.bpm.or_else(|| t.tempo.first().map(|m| m.bpm)),
                key: t
                    .key
                    .as_deref()
                    .map(|k| crate::deck::display_key(k, self.config.prefs.key_notation)),
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
    pub(super) fn collection_tracks(
        &mut self,
        ui: &mut Ui,
        stroke: Stroke,
    ) -> Option<(u32, usize)> {
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
        // Right-clicking a selected track acts on the whole selection.
        let selection = self.selected_ids();
        let marked = self.marked.clone();
        let group = |id: u32| if marked.contains(&id) { selection.clone() } else { vec![id] };
        let stem_model = self.stem_model();
        let stem_recipes = stem_model.as_ref().map(crate::stems::recipes).unwrap_or_default();
        let separating = self.stems.pending();
        let derived: std::collections::HashSet<u32> =
            self.collection.tracks.iter().filter(|t| t.derived.is_some()).map(|t| t.id).collect();
        // In playlist order (by #, or not sorted), entries dragged in the
        // list move there.
        let reorder = match self.config.sort {
            None => Some(false),
            Some(Sort { column: TrackColumn::Number, descending }) => Some(descending),
            Some(_) => None,
        }
        .filter(|_| {
            in_playlist.is_some()
                && self.collection_view.dragged_entries.as_ref().map(|d| d.0) == in_playlist
        });
        let options = TableOptions {
            selected: self.selected,
            selected_position: self.selected_position,
            reveal_selected: std::mem::take(&mut self.reveal_selected),
            loaded: &loaded,
            draggable: true,
            marked: &self.marked,
            editable_rating: true,
            reorder,
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
                        let ids = group(id);
                        let many = if ids.len() > 1 {
                            format!(" ({} tracks)", ids.len())
                        } else {
                            String::new()
                        };
                        ui.menu_button(format!("Add to playlist{many}"), |ui| {
                            if playlists.is_empty() {
                                ui.label("No playlists yet");
                            }
                            for (playlist, name) in &playlists {
                                if ui.button(name).clicked() {
                                    actions.push(TrackAction::AddTo(ids.clone(), *playlist));
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
                        if ui
                            .button(format!("Analyze BPM, grid and key{many}"))
                            .on_hover_text(
                                "Replaces the track's beat grid and key with newly detected ones",
                            )
                            .clicked()
                        {
                            actions.push(TrackAction::Analyze(ids.clone()));
                            ui.close();
                        }
                        ui.menu_button(format!("Make{many}"), |ui| match &stem_model {
                            None => {
                                ui.label(
                                    "No stem model yet: add one on the Stems page of Preferences",
                                );
                            }
                            Some(model) => {
                                ui.label(RichText::new(&model.name).small().weak());
                                if ui
                                    .button("Stems ready for the decks")
                                    .on_hover_text(
                                        "Separates now, in the background: loaded later, the \
                                         tracks have their stems at once",
                                    )
                                    .clicked()
                                {
                                    actions.push(TrackAction::PrepareStems(ids.clone()));
                                    ui.close();
                                }
                                ui.separator();
                                for recipe in &stem_recipes {
                                    if ui.button(&recipe.label).clicked() {
                                        actions
                                            .push(TrackAction::Make(ids.clone(), recipe.clone()));
                                        ui.close();
                                    }
                                }
                                if separating > 0 {
                                    ui.separator();
                                    if ui
                                        .button(format!("Cancel separating ({separating})"))
                                        .clicked()
                                    {
                                        actions.push(TrackAction::CancelStems);
                                        ui.close();
                                    }
                                }
                            }
                        });
                        if derived.contains(&id)
                            && ui
                                .button("Update cues from original")
                                .on_hover_text(
                                    "Copies the original's cues, grid and key to this track again",
                                )
                                .clicked()
                        {
                            actions.push(TrackAction::UpdateFromOriginal(id));
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
            self.click_track(id, position, output.modifiers);
        }
        if let Some((id, position)) = output.drag_started {
            self.start_track_drag(ui.ctx(), id, position);
        }
        if let (Some(before), Some(playlist)) = (output.moved_before, in_playlist)
            && let Some((_, entries)) = self.collection_view.dragged_entries.take()
        {
            self.collection.move_in_playlist(playlist, &entries, before);
            // The moved entries are together now, from `start`; the
            // selected one goes along.
            let start = before - entries.iter().filter(|&&e| e < before).count();
            if let Some(selected) = self.selected_position.filter(|p| entries.contains(p)) {
                let offset = entries.iter().filter(|&&e| e < selected).count();
                self.selected_position = Some(start + offset);
            }
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
                TrackAction::AddTo(ids, playlist) if ids.len() == 1 => {
                    self.request_add(ids[0], playlist)
                }
                TrackAction::AddTo(ids, playlist) => self.add_many(&ids, playlist),
                TrackAction::RemoveFrom(entry, playlist) => {
                    if let Some(entry) = entry {
                        self.collection.remove_playlist_entry(playlist, entry);
                    }
                }
                TrackAction::Forget(id) => self.collection.remove_tracks(&[id]),
                TrackAction::Analyze(ids) => self.analyze(ids, true),
                TrackAction::Make(ids, recipe) => self.make_stems(&ids, &recipe),
                TrackAction::PrepareStems(ids) => self.prepare_stems(&ids),
                TrackAction::CancelStems => {
                    self.stems.cancel();
                    self.status = Some("Stem separation cancelled".into());
                }
                TrackAction::UpdateFromOriginal(id) => {
                    if self.collection.update_from_original(id) {
                        self.status = Some("Cues, grid and key copied from the original".into());
                    } else {
                        self.collection_view.error =
                            Some("The original track is no longer in the collection".into());
                    }
                }
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
    AddTo(Vec<u32>, u32),
    /// (playlist position, playlist).
    RemoveFrom(Option<usize>, u32),
    Forget(u32),
    Edit(u32),
    Analyze(Vec<u32>),
    Make(Vec<u32>, crate::stems::Recipe),
    PrepareStems(Vec<u32>),
    CancelStems,
    UpdateFromOriginal(u32),
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

pub(super) fn deckhand_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn reveal_label() -> &'static str {
    if cfg!(windows) { "Show in Explorer" } else { "Show in Finder" }
}

/// Opens the file manager with the file selected.
pub(super) fn reveal(path: &std::path::Path) {
    let _ = if cfg!(windows) {
        std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn()
    } else {
        std::process::Command::new("open").arg("-R").arg(path).spawn()
    };
}

/// Where copies of rekordbox's library are kept before deckhand changes it.
fn rekordbox_backups() -> PathBuf {
    deckhand_library::data_dir().join("rekordbox-backups")
}

/// A save into rekordbox's library running in the background.
type RekordboxSave = std::sync::mpsc::Receiver<anyhow::Result<crate::rekordbox_save::Saved>>;

/// Saves into rekordbox's library at `path` with the key `saved` (fetched
/// when there is none).
fn save_into_rekordbox(
    path: &std::path::Path,
    saved: &str,
    snapshot: &crate::rekordbox_save::Snapshot,
) -> anyhow::Result<crate::rekordbox_save::Saved> {
    let key = match saved.trim() {
        "" => crate::rekordbox_db::fetch_key()?,
        key => key.to_owned(),
    };
    crate::rekordbox_save::save(path, &key, snapshot, &rekordbox_backups())
}
