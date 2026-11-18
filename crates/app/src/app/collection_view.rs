//! The browser for RofLoxBox's own collection: folders of playlists on the
//! left, tracks on the right, adding music by folder, file or drag and drop.

use crate::tour::Tag as _;
use std::path::PathBuf;

use rofloxbox_library::{CueKind, NodeKind, Place, Scan};
use eframe::egui::{self, RichText, Stroke, Ui};

use super::{RofLoxBoxApp, Focus, WARN};
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
    xml: Option<std::sync::mpsc::Receiver<anyhow::Result<rofloxbox_library::XmlLibrary>>>,
    /// Serato's libraries found on this computer, and when they were looked
    /// for (again every few seconds while the menu shows them).
    serato_found: Option<(std::time::Instant, Vec<PathBuf>)>,
    /// A Serato library being read.
    serato: Option<std::sync::mpsc::Receiver<anyhow::Result<rofloxbox_library::XmlLibrary>>>,
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
    /// Quitting once the save into rekordbox running now is done (it runs
    /// in the background, so the window keeps answering meanwhile).
    quit_after_save: bool,
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
    /// Playlists and folders picked by Cmd/Ctrl- or Shift-clicking (or one
    /// clicked), to move or delete together.
    picked: Vec<u32>,
    /// Where a Shift-click range starts.
    pick_anchor: Option<u32>,
    /// The tree's rows shown last frame, in order (for Shift-click ranges).
    shown_rows: Vec<u32>,
    /// Playlists and folders waiting for "delete them?", and whether to ask
    /// again next time.
    confirm_delete: Option<(Vec<u32>, bool)>,
    /// Tracks waiting for "remove them from the collection?".
    confirm_forget: Option<Vec<u32>>,
    /// "Convert these tracks" being asked: the tracks, whether to replace
    /// the originals, and whether to remember that.
    pub(super) converting: Option<(Vec<u32>, bool, bool)>,
    /// Tracks being converted.
    pub(super) conversion: Option<crate::convert::Conversion>,
}

/// Playlists and folders being dragged in the collection's tree.
pub struct PlaylistDrag(pub Vec<u32>);

impl RofLoxBoxApp {
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
        let what = if overwrite { "again" } else { "where missing" };
        self.status = Some(format!(
            "Analyzing BPM, beat grid and key of {} track(s) {what}…",
            tracks.len()
        ));
        self.collection_view.analysis =
            Some((crate::analyze::Analysis::start(tracks, overwrite), 0, 0));
    }

    /// Stores finished analyses in the collection and on decks holding
    /// the track.
    fn poll_analysis(&mut self, ctx: &egui::Context) {
        use rofloxbox_library::TempoMarker;
        let Some((analysis, found, failed)) = self.collection_view.analysis.as_mut() else {
            return;
        };
        let total = analysis.total;
        // One line per track in the log, after the track's changes.
        let mut lines: Vec<(String, bool)> = Vec::new();
        while let Ok(r) = analysis.results.try_recv() {
            analysis.received += 1;
            let done = analysis.received;
            let track = self.collection.track(r.id);
            let title = track.map_or_else(String::new, |t| t.title.clone());
            let progress = format!("Analysis {done}/{total}: {title}");
            if r.failed {
                *failed += 1;
                let path = track.map_or_else(String::new, |t| t.path.display().to_string());
                lines.push((format!("{progress} — couldn't be read ({path})"), true));
                continue;
            }
            let mut found_parts = Vec::new();
            if let Some(g) = &r.grid {
                found_parts.push(format!("{:.2} BPM", g.bpm));
            }
            if let Some(k) = &r.key {
                found_parts.push(k.camelot());
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
            let found_text = if found_parts.is_empty() {
                "nothing found".into()
            } else {
                found_parts.join(", ")
            };
            let kept = if changed { "" } else { " (kept what it had)" };
            lines.push((format!("{progress} — {found_text}{kept}"), false));
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
        let finished = analysis.is_finished();
        for (line, error) in lines {
            self.log(line, error);
        }
        let Some((analysis, found, failed)) = self.collection_view.analysis.as_mut() else {
            return;
        };
        if finished {
            let (done, total) =
                (analysis.done.load(std::sync::atomic::Ordering::Relaxed), analysis.total);
            let unread = if *failed > 0 {
                format!(", {failed} file(s) could not be read")
            } else {
                String::new()
            };
            self.status = Some(format!(
                "Analysis: {found} of {total} track(s) got a grid or key{unread}{}",
                if done < total { " (stopped)" } else { "" }
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

    /// At start: rekordbox's library is read again, unless RofLoxBox has
    /// changes it doesn't have yet (they go in first).
    pub(super) fn rekordbox_at_start(&mut self) {
        if self.collection.rekordbox_unsaved && self.config.prefs.rekordbox_write {
            self.collection_view.rekordbox_due = Some(std::time::Instant::now());
            self.status = Some(
                "rekordbox's library doesn't have RofLoxBox's last changes yet: saving them \
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
            crate::widgets::help(ui, "rekordbox's library is encrypted. The key is downloaded once from pyrekordbox; or enter it yourself.");
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.config.prefs.rekordbox_at_start, "Update from rekordbox at start");
            crate::widgets::help(ui, "Imports tracks, cues, grids and playlists (into the rekordbox folder). Intelligent playlists are skipped.");
        });
        let mut writable = self.config.prefs.rekordbox_write;
        let write = ui.horizontal(|ui| {
            let write = ui
                .checkbox(&mut writable, "Write changes back to rekordbox")
                .tour(crate::tour::Target::RekordboxWrite);
            crate::widgets::help(ui, "On: saving writes your tags, cues, grids and rekordbox-folder playlists into rekordbox's library. A copy is kept before each save.");
            write
        });
        if write.inner.changed() {
            if writable {
                self.collection_view.confirm_rekordbox_write = true;
            } else {
                self.config.prefs.rekordbox_write = false;
            }
        }
        if self.config.prefs.rekordbox_write {
            ui.indent("rekordbox autosave", |ui| {
                ui.checkbox(&mut self.config.prefs.rekordbox_autosave, "Save automatically")
                    .on_hover_text("Shortly after each change");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.config.prefs.rekordbox_wait_closed, "Wait for rekordbox to close");
                    crate::widgets::help(ui, "On: RofLoxBox looks for rekordbox before each save and saves only once it's closed (quitting waits for it too). Off: saves go in whether rekordbox runs or not; rekordbox may not show them until it's restarted, and changes it makes meanwhile stop the save until the library is read again.");
                });
            });
        }
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
        let mut asked_again = self.convert_prefs(ui);
        if asked_again {
            self.config.save();
        }
        if self.config.prefs.delete_playlists_without_asking {
            ui.add_space(10.0);
            ui.label(RichText::new("Playlists").strong().size(15.0));
            ui.label(
                RichText::new("Deleting playlists and folders doesn't ask first (you said so).")
                    .small()
                    .weak(),
            );
            if ui.button("Ask again before deleting playlists").clicked() {
                self.config.prefs.delete_playlists_without_asking = false;
                asked_again = true;
            }
        }
        let p = &self.config.prefs;
        asked_again
            || (
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
                "This changes rekordbox's library. Back it up first: File > Library > Backup Library.",
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "A copy is kept before each save, in {}.",
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
                "Write changes to rekordbox's library",
            )
            .clicked()
            && let Some(path) = path.clone()
        {
            self.save_to_rekordbox(path, false);
        }
        if ui
            .add_enabled(
                path.is_some() && self.collection_view.rekordbox_save.is_none(),
                egui::Button::new("Repair waveforms in rekordbox"),
            )
            .on_hover_text(
                "Goes through the tracks rekordbox has that are in RofLoxBox's collection: those \
                 missing analysis files (saved by an older RofLoxBox, or with files lost) get \
                 them, worked out from the audio, so rekordbox shows their waveforms without \
                 analysing them again. rekordbox's own files aren't changed. It's what every \
                 save does as well; this runs it now.",
            )
            .clicked()
            && let Some(path) = path
        {
            self.save_to_rekordbox(path, false);
        }
    }

    /// What of the collection rekordbox's library gets, ready to save;
    /// `None` until the library was read.
    fn rekordbox_snapshot(&self) -> Option<crate::rekordbox_save::Snapshot> {
        crate::rekordbox_save::Snapshot::of(&self.collection)
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
            && prefs.rekordbox_wait_closed
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
                self.collection_view.quit_waiting = None;
                self.begin_quit(ctx);
                return;
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
                "Close rekordbox to save your changes into it; RofLoxBox then quits.",
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
                        "Saved into rekordbox once it's closed",
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
    /// A save into rekordbox is running.
    pub(super) fn rekordbox_saving(&self) -> bool {
        self.collection_view.rekordbox_save.is_some()
    }

    /// Starts quitting: changes due for rekordbox are saved into it first,
    /// in the background (a window says so), then the app quits.
    pub(super) fn begin_quit(&mut self, ctx: &egui::Context) {
        let prefs = &self.config.prefs;
        let view = &self.collection_view;
        let due = prefs.rekordbox_write
            && prefs.rekordbox_autosave
            && !view.rekordbox_paused
            && view.rekordbox_due.is_some()
            && self.collection.rekordbox_count.is_some()
            && !(prefs.rekordbox_wait_closed && crate::rekordbox_db::rekordbox_running());
        if due
            && view.rekordbox_save.is_none()
            && let Some(path) = self.rekordbox_db().filter(|p| p.is_file())
        {
            self.collection_view.rekordbox_due = None;
            self.save_to_rekordbox(path, true);
        }
        if self.collection_view.rekordbox_save.is_some() {
            self.collection_view.quit_after_save = true;
            ctx.request_repaint();
        } else {
            self.quit();
        }
    }

    /// While quitting waits for a save into rekordbox: says so, and quits
    /// once it's done (or right away, if asked to).
    pub(super) fn quit_after_saving(&mut self, ctx: &egui::Context) {
        if !self.collection_view.quit_after_save {
            return;
        }
        if self.collection_view.rekordbox_save.is_none() {
            self.quit();
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        let (mut now, mut stay) = (false, false);
        egui::Modal::new(egui::Id::new("quit after saving")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(RichText::new("Saving into rekordbox").strong().size(16.0));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Your changes go into rekordbox's library; RofLoxBox then quits.");
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                stay = ui.button("Don't quit").clicked();
                now = ui
                    .button("Quit now")
                    .on_hover_text("The changes are saved into rekordbox next time")
                    .clicked();
            });
        });
        if now {
            // Saved next time: the collection remembers they're due.
            self.collection.set_rekordbox_unsaved(true);
            self.quit();
        }
        if stay {
            self.collection_view.quit_after_save = false;
        }
    }

    /// Writes the collection's changes into rekordbox's library in the
    /// background; `auto`: started by itself, so quiet about it.
    fn save_to_rekordbox(&mut self, path: PathBuf, auto: bool) {
        let print = self.rekordbox_fingerprint();
        let Some(snapshot) = self.rekordbox_snapshot() else {
            let never_read = anyhow::anyhow!("read rekordbox's library into RofLoxBox first");
            self.rekordbox_saved(Err(never_read), print, auto);
            return;
        };
        let key = self.config.prefs.rekordbox_key.clone();
        let wait_closed = self.config.prefs.rekordbox_wait_closed;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(save_into_rekordbox(&path, &key, &snapshot, wait_closed));
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
                    self.collection.moved_in_rekordbox(&saved.moved);
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
                    (saved.moved.len(), "moved to converted files"),
                    (saved.analysed, "given RofLoxBox's waveforms"),
                    (saved.unanalysed, "without waveforms (audio unreadable)"),
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
                        "rekordbox is open: RofLoxBox's changes go into its library once it's \
                         closed"
                            .into(),
                    );
                }
            }
            Err(err) if auto => {
                view.rekordbox_paused = true;
                view.error = Some(format!(
                    "Saving into rekordbox stopped: {err:#}. Changes stay in RofLoxBox; \
                     \"Save changes to rekordbox\" tries again."
                ));
            }
            Err(err) => view.error = Some(format!("Not saved to rekordbox: {err:#}")),
        }
    }

    /// Reads the Serato library in `serato` (a `_Serato_` folder) in the
    /// background; it goes into the Serato folder.
    fn import_serato(&mut self, serato: PathBuf) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(rofloxbox_library::serato::open(&serato));
        });
        self.collection_view.serato = Some(rx);
    }

    /// Reads a rekordbox XML export in the background.
    fn import_xml(&mut self, path: PathBuf) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(rofloxbox_library::XmlLibrary::open(&path));
        });
        self.collection_view.xml = Some(rx);
    }

    /// Merges a finished scan or import and saves. Call every frame.
    pub(super) fn poll_collection(&mut self, ctx: &egui::Context) {
        self.poll_analysis(ctx);
        self.poll_conversion(ctx);
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
        if let Some(result) = self.collection_view.serato.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.collection_view.serato = None;
            match result {
                Ok(library) => {
                    let s = self.collection.import_into(library, "Serato", true);
                    let missing = if s.missing > 0 {
                        format!(", {} file(s) not found on this computer", s.missing)
                    } else {
                        String::new()
                    };
                    self.status = Some(format!(
                        "Serato import: {} added, {} updated, {} crates{missing}",
                        s.added, s.updated, s.playlists
                    ));
                }
                Err(err) => self.collection_view.error = Some(format!("Serato: {err:#}")),
            }
        } else if self.collection_view.serato.is_some() {
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
                .add_filter("Audio", &rofloxbox_library::AUDIO_EXTENSIONS)
                .pick_files()
            {
                self.add_music(files);
            }
        }
        if ui
            .button("Rescan added folders")
            .on_hover_text("Rescan added folders")
            .clicked()
        {
            ui.close();
            self.rescan();
        }
        if ui
            .button("Import rekordbox library")
            .on_hover_text(
                "Read rekordbox's library on this computer",
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
                "rekordbox: File > Export Collection in xml format",
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
        // Serato's libraries found here (the Music folder's, each drive's),
        // or one picked.
        let view = &mut self.collection_view;
        if view.serato_found.as_ref().is_none_or(|(at, _)| at.elapsed().as_secs() >= 5) {
            view.serato_found = Some((std::time::Instant::now(), rofloxbox_library::serato::find()));
        }
        let found_here = view.serato_found.as_ref().map(|(_, f)| f.clone()).unwrap_or_default();
        for found in found_here {
            let place = found.parent().unwrap_or(&found).display().to_string();
            if ui
                .add_enabled(self.collection_view.serato.is_none(), egui::Button::new(format!("Import Serato library ({place})")))
                .on_hover_text("Tracks, crates, hot cues, saved loops and beat grids, into the Serato folder")
                .clicked()
            {
                ui.close();
                self.import_serato(found);
            }
        }
        if ui
            .add_enabled(self.collection_view.serato.is_none(), egui::Button::new("Import Serato library…"))
            .on_hover_text("Choose a _Serato_ folder (or the folder or drive it is in)")
            .clicked()
        {
            ui.close();
            if let Some(picked) =
                rfd::FileDialog::new().set_title("Choose a _Serato_ folder").pick_folder()
            {
                match rofloxbox_library::serato::library_in(&picked) {
                    Some(serato) => self.import_serato(serato),
                    None => {
                        self.collection_view.error =
                            Some(format!("No Serato library in {}", picked.display()))
                    }
                }
            }
        }
        if ui
            .button("Analyze tracks without a grid or key")
            .on_hover_text(
                "Detect missing BPM, grids and keys",
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
            self.new_node(NodeKind::Playlist, parent);
        }
        if ui.button("New folder").clicked() {
            ui.close();
            self.new_node(NodeKind::Folder, parent);
        }
        ui.separator();
        ui.label(RichText::new("Playlists").small().weak());
        let mut by_name = self.config.playlists_by_name;
        ui.radio_value(&mut by_name, false, "In their own order")
            .on_hover_text("As you arranged them by dragging (rekordbox's order too)");
        ui.radio_value(&mut by_name, true, "By name").on_hover_text("Folders first, A to Z");
        if by_name != self.config.playlists_by_name {
            self.config.playlists_by_name = by_name;
            self.config.save();
        }
        if ui.button("Expand all folders").clicked() {
            self.config.collapsed_folders.clear();
            self.config.save();
            ui.close();
        }
        if ui.button("Collapse all folders").clicked() {
            self.config.collapsed_folders = self
                .collection
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Folder)
                .map(|n| n.id)
                .collect();
            self.config.save();
            ui.close();
        }
    }

    /// A new playlist or folder, at the top of `parent` (shown open), with
    /// its name being typed.
    fn new_node(&mut self, kind: NodeKind, parent: Option<u32>) {
        let name = match kind {
            NodeKind::Playlist => "New playlist",
            NodeKind::Folder => "New folder",
        };
        let first = self.collection.tree().into_iter().find(|(_, n)| n.parent == parent);
        let first = first.map(|(_, n)| n.id);
        let id = self.collection.create(name, kind.clone(), parent);
        if let Some(first) = first {
            self.collection.move_nodes(&[id], Place::Before(first));
        }
        // Its folders open, so it shows.
        let mut folder = parent;
        while let Some(f) = folder {
            self.config.collapsed_folders.retain(|c| *c != f);
            folder = self.collection.node(f).and_then(|n| n.parent);
        }
        self.config.save();
        self.tree.collection_open = true;
        if kind == NodeKind::Playlist {
            self.select_entry(super::library_tree::TreeEntry::Collection(Some(id)));
            self.focus = Focus::Playlists;
        }
        (self.collection_view.picked, self.collection_view.pick_anchor) = (vec![id], Some(id));
        self.collection_view.renaming = Some((id, name.into()));
    }

    /// Puts a spacer (a second of silence called "---") into a playlist
    /// before entry `at`, or at its end.
    fn insert_spacer(&mut self, playlist: u32, at: Option<usize>) {
        let path = match spacer_file() {
            Ok(path) => path,
            Err(err) => {
                self.collection_view.error = Some(format!("Making the spacer: {err:#}"));
                return;
            }
        };
        let key = path.to_string_lossy().into_owned();
        let file = rofloxbox_library::ScannedFile {
            path: path.clone(),
            title: SPACER.into(),
            duration: 1.0,
            sample_rate: Some(SPACER_RATE),
            ..Default::default()
        };
        let xml = rofloxbox_library::XmlTrack {
            id: key.clone(),
            location: key,
            file,
            rating: None,
            ..Default::default()
        };
        let (ids, _) = self.collection.add_tracks(vec![xml]);
        let Some(&id) = ids.first() else { return };
        let Some(end) = self.collection.node(playlist).map(|n| n.tracks.len()) else { return };
        self.collection.add_to_playlist(playlist, &[id]);
        if let Some(at) = at.filter(|&at| at < end) {
            self.collection.move_in_playlist(playlist, &[end], at);
        }
        self.status = Some("Spacer inserted".into());
    }

    /// Another copy of each of these entries, right after it.
    fn duplicate_entries(&mut self, playlist: u32, mut entries: Vec<usize>) {
        entries.sort_unstable();
        entries.dedup();
        // From the end, so the positions still to do stay right.
        for &entry in entries.iter().rev() {
            let Some(node) = self.collection.node(playlist) else { return };
            let (Some(&track), end) = (node.tracks.get(entry), node.tracks.len()) else {
                continue;
            };
            self.collection.add_to_playlist(playlist, &[track]);
            self.collection.move_in_playlist(playlist, &[end], entry + 1);
        }
        self.status = Some(format!("Duplicated {} in the playlist", entries.len()));
    }

    /// The playlist entries selected (their positions), in list order.
    fn selected_entries(&self) -> Vec<usize> {
        if self.marked.is_empty() {
            return self.selected_position.into_iter().collect();
        }
        self.visible_rows()
            .into_iter()
            .filter(|(_, position)| self.marked.contains(position))
            .map(|(_, position)| position)
            .collect()
    }

    /// Takes entries (positions) out of a playlist; the selection goes.
    fn remove_entries(&mut self, playlist: u32, mut entries: Vec<usize>) {
        if entries.is_empty() {
            return;
        }
        // From the end, so the positions still left stay right.
        entries.sort_unstable();
        entries.dedup();
        for &entry in entries.iter().rev() {
            self.collection.remove_playlist_entry(playlist, entry);
        }
        self.marked.clear();
        (self.selected, self.selected_position) = (None, None);
        self.status = Some(format!("Removed {} from the playlist", entries.len()));
    }

    /// "Remove these tracks from the collection?"
    pub(super) fn forget_tracks_dialog(&mut self, ctx: &egui::Context) {
        let Some(ids) = &self.collection_view.confirm_forget else { return };
        let question = match ids.as_slice() {
            [] => {
                self.collection_view.confirm_forget = None;
                return;
            }
            [one] => {
                let title = self.collection.track(*one).map_or("", |t| t.title.as_str());
                format!("Remove \"{title}\" from the collection?")
            }
            more => format!("Remove these {} tracks from the collection?", more.len()),
        };
        let mut answer = None;
        egui::Window::new("Remove from collection")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(question);
                ui.label(
                    RichText::new(
                        "They go from every playlist too, with their cues, grids and ratings. \
                         The files stay on the computer.",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Remove").strong()).clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Enter))
                    {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Escape))
                    {
                        answer = Some(false);
                    }
                });
            });
        if let Some(remove) = answer
            && let Some(ids) = self.collection_view.confirm_forget.take()
            && remove
        {
            self.collection.remove_tracks(&ids);
            self.marked.clear();
            (self.selected, self.selected_position) = (None, None);
            self.status = Some(format!("Removed {} track(s) from the collection", ids.len()));
        }
    }

    /// The playlists and folders a click, a key or a menu of `on` acts on:
    /// the picked ones when `on` is one of them (or nothing is given),
    /// otherwise `on` alone.
    fn acted_on(&self, on: Option<u32>) -> Vec<u32> {
        let picked: Vec<u32> = self
            .collection_view
            .picked
            .iter()
            .copied()
            .filter(|&id| self.collection.node(id).is_some())
            .collect();
        match on {
            Some(id) if !picked.contains(&id) => vec![id],
            None if picked.is_empty() => self.collection_view.node.into_iter().collect(),
            _ => picked,
        }
    }

    /// A playlist or folder clicked: alone, or added to (Cmd/Ctrl) or
    /// extended to (Shift) what's picked.
    fn pick_node(&mut self, id: u32, kind: &NodeKind, modifiers: egui::Modifiers) {
        let view = &mut self.collection_view;
        if modifiers.shift
            && let Some(anchor) = view.pick_anchor.or(view.node)
            && let (Some(a), Some(b)) = (
                view.shown_rows.iter().position(|&r| r == anchor),
                view.shown_rows.iter().position(|&r| r == id),
            )
        {
            view.picked = view.shown_rows[a.min(b)..=a.max(b)].to_vec();
            return;
        }
        if modifiers.command {
            if view.picked.is_empty()
                && let Some(shown) = view.node
            {
                view.picked.push(shown);
            }
            if let Some(i) = view.picked.iter().position(|&p| p == id) {
                view.picked.remove(i);
            } else {
                view.picked.push(id);
            }
            view.pick_anchor = Some(id);
            return;
        }
        (view.picked, view.pick_anchor) = (vec![id], Some(id));
        // Clicking a folder opens or closes it (and selects it, for new
        // playlists).
        if *kind == NodeKind::Folder {
            if !self.config.collapsed_folders.contains(&id) {
                self.config.collapsed_folders.push(id);
            } else {
                self.config.collapsed_folders.retain(|f| *f != id);
            }
            self.config.save();
        }
        self.select_entry(super::library_tree::TreeEntry::Collection(Some(id)));
        self.focus = Focus::Playlists;
    }

    /// Deletes playlists and folders, after asking (unless asked not to).
    fn ask_to_delete(&mut self, ids: Vec<u32>) {
        if ids.is_empty() {
            return;
        }
        if self.config.prefs.delete_playlists_without_asking {
            self.delete_nodes(&ids);
        } else {
            self.collection_view.confirm_delete = Some((ids, false));
        }
    }

    fn delete_nodes(&mut self, ids: &[u32]) {
        for &id in ids {
            self.collection.delete(id);
        }
        if self.collection_view.node.is_some_and(|n| self.collection.node(n).is_none()) {
            self.collection_view.node = None;
        }
        self.collection_view.picked.clear();
    }

    /// "Delete these playlists?", with "Don't ask again".
    pub(super) fn delete_playlists_dialog(&mut self, ctx: &egui::Context) {
        let Some((ids, _)) = &self.collection_view.confirm_delete else { return };
        let nodes: Vec<&rofloxbox_library::Node> =
            ids.iter().filter_map(|&id| self.collection.node(id)).collect();
        let question = match nodes.as_slice() {
            [] => {
                self.collection_view.confirm_delete = None;
                return;
            }
            [one] if one.kind == NodeKind::Folder => {
                format!("Delete the folder \"{}\" and everything in it?", one.name)
            }
            [one] => format!("Delete the playlist \"{}\"?", one.name),
            more => format!("Delete these {} playlists and folders?", more.len()),
        };
        let folders = nodes.len() > 1 && nodes.iter().any(|n| n.kind == NodeKind::Folder);
        let mut answer = None;
        egui::Window::new("Delete")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(question);
                if folders {
                    ui.label(RichText::new("Folders go with everything in them.").small().weak());
                }
                ui.label(RichText::new("The tracks stay in the collection.").small().weak());
                if let Some((_, never)) = &mut self.collection_view.confirm_delete {
                    ui.checkbox(never, "Don't ask again");
                }
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Delete").strong()).clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Enter))
                    {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Escape))
                    {
                        answer = Some(false);
                    }
                });
            });
        if let Some(delete) = answer
            && let Some((ids, never)) = self.collection_view.confirm_delete.take()
            && delete
        {
            if never {
                self.config.prefs.delete_playlists_without_asking = true;
                self.config.save();
            }
            self.delete_nodes(&ids);
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
        if self.transferring() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Moving tracks between libraries…").small());
            });
        }
        if self.collection_view.serato.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Reading the Serato library…").small());
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
        if let Some(conversion) = &self.collection_view.conversion {
            let (done, total) = (conversion.received, conversion.total);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Converting {done} of {total}"));
                if ui
                    .button(RichText::new("Stop").color(WARN))
                    .on_hover_text("Stops after the tracks being converted now")
                    .clicked()
                {
                    conversion.cancel();
                }
            });
            ui.add(
                egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                    .desired_width(220.0)
                    .show_percentage(),
            );
            let now: Vec<String> = conversion
                .working()
                .into_iter()
                .filter_map(|id| self.collection.track(id))
                .map(|t| t.title.clone())
                .collect();
            if !now.is_empty() {
                let text = RichText::new(format!("Now: {}", now.join(" · "))).small().weak();
                ui.add(egui::Label::new(text).truncate());
            }
        }
        if let Some((analysis, _, _)) = &self.collection_view.analysis {
            let (done, total) = (analysis.received, analysis.total);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Analyzing {done} of {total}"));
                if ui
                    .button(RichText::new("Stop").color(WARN))
                    .on_hover_text("Stops after the tracks being read now; what's done is kept")
                    .clicked()
                {
                    analysis.cancel();
                }
            });
            ui.add(
                egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                    .desired_width(220.0)
                    .show_percentage(),
            );
            let now: Vec<String> = analysis
                .working()
                .into_iter()
                .filter_map(|id| self.collection.track(id))
                .map(|t| {
                    if t.title.is_empty() { t.path.display().to_string() } else { t.title.clone() }
                })
                .collect();
            if !now.is_empty() {
                let text = RichText::new(format!("Now: {}", now.join(" · "))).small().weak();
                ui.add(egui::Label::new(text).truncate()).on_hover_text(now.join("\n"));
            }
        }
        if let Some(err) = &self.collection_view.error {
            ui.colored_label(WARN, err);
        }
    }

    /// The collection's playlists and folders in the sidebar tree.
    pub(super) fn collection_tree_rows(&mut self, ui: &mut Ui) {
        let tree = if self.config.playlists_by_name {
            self.collection.tree_by_name()
        } else {
            self.collection.tree()
        };
        let songs = self.songs();
        let unique = |tracks: &[u32]| unique_songs(&songs, tracks);
        let rows: Vec<(usize, u32, String, NodeKind, usize)> = tree
            .into_iter()
            .map(|(depth, n)| (depth, n.id, n.name.clone(), n.kind.clone(), unique(&n.tracks)))
            .collect();
        // Rows inside a collapsed folder are skipped.
        let mut hidden_below: Option<usize> = None;
        let mut shown = Vec::new();
        for (depth, id, name, kind, count) in rows {
            if hidden_below.is_some_and(|d| depth > d) {
                continue;
            }
            hidden_below = None;
            if kind == NodeKind::Folder && self.config.collapsed_folders.contains(&id) {
                hidden_below = Some(depth);
            }
            shown.push(id);
            self.tree_row(ui, depth + 1, id, &name, kind, count);
        }
        self.collection_view.shown_rows = shown;
        // Delete (Backspace on a Mac's keyboard) deletes what's picked.
        let delete = ui.input(|i| {
            i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)
        });
        if delete
            && self.focus == Focus::Playlists
            && self.source == super::Source::Collection
            && self.collection_view.renaming.is_none()
            && self.collection_view.confirm_delete.is_none()
            && !ui.ctx().wants_keyboard_input()
        {
            let ids = self.acted_on(None);
            self.ask_to_delete(ids);
        }
        playlist_drag_hint(ui.ctx());
    }

    /// Writes hot cues, memory cues/loops and an edited (tapped, shifted…)
    /// beat grid of loaded collection tracks into the collection. Call
    /// every frame.
    pub(super) fn save_track_edits(&mut self) {
        use rofloxbox_library::{Cue, CueKind, TempoMarker};
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
                    active: false,
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
                    active: end.is_some_and(|end| loaded.is_active_loop((start, end))),
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
                    RichText::new("The audio file isn't changed.")
                        .small()
                        .weak(),
                );
                ui.horizontal(|ui| {
                    save = ui.button("Save").clicked();
                    close = ui.button("Cancel").clicked();
                    if edited {
                        revert = ui
                            .button("Back to the file's tags")
                            .on_hover_text("Re-read the file's tags")
                            .clicked();
                    }
                });
            });
        if save {
            let [title, artist, album, genre, label, comment] = fields.clone();
            self.collection.edit_track(
                id,
                rofloxbox_library::TrackEdit {
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
        let row = ui.horizontal(|ui| {
            ui.add_space(indent);
            if let Some((renaming, text)) = &mut self.collection_view.renaming
                && *renaming == id
            {
                let response = crate::widgets::name_field(ui, text, 150.0);
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
            let shown =
                self.source == super::Source::Collection && self.collection_view.node == Some(id);
            let picked = self.collection_view.picked.contains(&id);
            let response = ui
                .add(egui::Button::selectable(shown || picked, label).truncate())
                .interact(egui::Sense::click_and_drag());
            if shown && self.reveal_playlist {
                response.scroll_to_me(Some(egui::Align::Center));
            }
            if response.clicked() {
                let modifiers = ui.input(|i| i.modifiers);
                self.pick_node(id, &kind, modifiers);
            }
            if response.drag_started() {
                if !picked {
                    (self.collection_view.picked, self.collection_view.pick_anchor) =
                        (vec![id], Some(id));
                }
                let ids = self.acted_on(Some(id));
                egui::DragAndDrop::set_payload(ui.ctx(), PlaylistDrag(ids));
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
                && let Some(drag) =
                    crate::widgets::dropped::<crate::browser::TrackDrag>(&response)
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
                            .on_hover_text("Creates an export if there's none")
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
                        self.new_node(NodeKind::Playlist, Some(id));
                        ui.close();
                    }
                    if ui.button("New folder here").clicked() {
                        self.new_node(NodeKind::Folder, Some(id));
                        ui.close();
                    }
                }
                if ui.button("Rename").clicked() {
                    self.collection_view.renaming = Some((id, name.to_owned()));
                    ui.close();
                }
                let targets = self.acted_on(Some(id));
                let folders: Vec<(u32, String)> = self
                    .collection
                    .tree()
                    .into_iter()
                    .filter(|(_, n)| n.kind == NodeKind::Folder && !targets.contains(&n.id))
                    .map(|(_, n)| (n.id, n.name.clone()))
                    .collect();
                let several = targets.len() > 1;
                let move_to =
                    if several { format!("Move {} to", targets.len()) } else { "Move to".into() };
                ui.menu_button(move_to, |ui| {
                    if ui.button("Top level").clicked() {
                        self.collection.move_nodes(&targets, Place::Into(None));
                        ui.close();
                    }
                    for (folder, folder_name) in folders {
                        if ui.button(folder_name).clicked() {
                            self.collection.move_nodes(&targets, Place::Into(Some(folder)));
                            ui.close();
                        }
                    }
                });
                let what = match (several, &kind) {
                    (true, _) => format!("{} selected", targets.len()),
                    (false, NodeKind::Folder) => "folder and its playlists".into(),
                    (false, NodeKind::Playlist) => "playlist".into(),
                };
                if ui.button(format!("Delete {what}…")).clicked() {
                    self.ask_to_delete(targets.clone());
                    ui.close();
                }
            });
        });
        if let Some(root) = export_to {
            self.start_export(id, root);
        }
        let rect = row.response.rect.with_max_x(ui.max_rect().right());
        self.playlist_drop_target(ui, rect, id, &kind);
    }

    /// "All tracks" clicked: nothing picked any more.
    pub(super) fn clear_picked(&mut self) {
        self.collection_view.picked.clear();
    }

    /// Playlists and folders dropped on "All tracks" go to the top level.
    pub(super) fn top_level_drop_target(&mut self, ui: &Ui, rect: egui::Rect) {
        let row = ui.interact(rect, egui::Id::new("playlist drop top"), egui::Sense::hover());
        let Some(drag) = row.dnd_hover_payload::<PlaylistDrag>() else { return };
        let stroke = Stroke::new(2.0_f32, ui.visuals().selection.bg_fill);
        ui.painter().rect_stroke(rect, 3.0, stroke, egui::StrokeKind::Inside);
        if crate::widgets::dropped::<PlaylistDrag>(&row).is_some() {
            self.collection.move_nodes(&drag.0, Place::Into(None));
        }
    }

    /// Playlists and folders dragged over a row: before it (its upper
    /// part), after it (a playlist's lower half) or into it (a folder's
    /// rest); marked while over it, moved there on release.
    fn playlist_drop_target(&mut self, ui: &Ui, rect: egui::Rect, id: u32, kind: &NodeKind) {
        let row = ui.interact(rect, egui::Id::new(("playlist drop", id)), egui::Sense::hover());
        let Some(pointer) = ui.ctx().pointer_interact_pos() else { return };
        let Some(drag) = row.dnd_hover_payload::<PlaylistDrag>() else { return };
        let part = (pointer.y - rect.top()) / rect.height().max(1.0);
        let place = match kind {
            NodeKind::Folder if part >= 0.3 => Place::Into(Some(id)),
            NodeKind::Playlist if part >= 0.5 => Place::After(id),
            _ => Place::Before(id),
        };
        // By name, the order is the names': only which folder counts.
        let place = match place {
            Place::Before(at) | Place::After(at) if self.config.playlists_by_name => {
                match kind {
                    NodeKind::Folder => Place::Into(Some(id)),
                    NodeKind::Playlist => {
                        Place::Into(self.collection.node(at).and_then(|n| n.parent))
                    }
                }
            }
            place => place,
        };
        let stroke = Stroke::new(2.0_f32, ui.visuals().selection.bg_fill);
        let painter = ui.painter();
        match place {
            Place::Into(_) => {
                painter.rect_stroke(rect, 3.0, stroke, egui::StrokeKind::Inside);
            }
            Place::Before(_) => {
                painter.hline(rect.x_range(), rect.top(), stroke);
            }
            Place::After(_) => {
                painter.hline(rect.x_range(), rect.bottom(), stroke);
            }
        }
        if crate::widgets::dropped::<PlaylistDrag>(&row).is_some() {
            self.collection.move_nodes(&drag.0, place);
            if let Place::Into(Some(folder)) = place {
                self.config.collapsed_folders.retain(|f| *f != folder);
                self.config.save();
            }
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
                artwork: t.has_artwork.then(|| crate::artwork::Cover::Embedded(t.path.clone())),
                preview: Some(crate::preview::Source {
                    audio: t.path.clone(),
                    analysis: t.analysis.clone(),
                }),
                cues: Some(crate::preview::track_marks(t)),
                track_number: t.track_number,
                color: t.color,
                path: t.path.clone(),
                size: t.file_size,
                sample_rate: t.sample_rate,
                hot_cues: Some(
                    t.cues.iter().filter(|c| matches!(c.kind, CueKind::Hot(_))).count(),
                ),
                last_played: t.last_played.map(date).unwrap_or_default(),
                ..Row::default()
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
        let shown = self.shown_decks();
        // The playlist shown: its songs (each once, no spacers) and entries.
        let counts = self
            .collection_view
            .node
            .and_then(|n| self.collection.node(n))
            .filter(|n| n.kind == NodeKind::Playlist)
            .map(|n| (unique_songs(&self.songs(), &n.tracks), n.tracks.len()));
        ui.horizontal(|ui| {
            if let Some((songs, entries)) = counts {
                let text = if songs == entries {
                    format!("{songs} tracks")
                } else {
                    format!("{songs} tracks ({entries} entries)")
                };
                ui.label(RichText::new(text).strong()).on_hover_text(
                    "Each song once: copies of it and spacers aren't counted; \
                     entries are everything in the playlist",
                );
                ui.separator();
            }
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search title, artist, album, genre, label")
                    .desired_width(320.0),
            )
            .tour(crate::tour::Target::LibrarySearch);
            if let Some(id) = self.selected {
                ui.separator();
                for &deck in &shown {
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
        let group = |row: &Row| {
            if marked.contains(&row.position) { selection.clone() } else { vec![row.id] }
        };
        // The playlist entries selected (positions), for removing them.
        let marked_entries = self.selected_entries();
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
            compatible_with: self.compatible_key(),
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
                        // Decks.
                        for &deck in &shown {
                            if ui.button(format!("Load to deck {}", deck + 1)).clicked() {
                                actions.push(TrackAction::Load(id, deck));
                                ui.close();
                            }
                        }
                        ui.separator();
                        // Playlists.
                        let ids = group(row);
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
                        if let Some(playlist) = in_playlist {
                            let entries = if marked.contains(&row.position) {
                                marked_entries.clone()
                            } else {
                                row.entry.into_iter().collect()
                            };
                            let (duplicate, remove) = match entries.len() {
                                1 => (
                                    "Duplicate in playlist".to_owned(),
                                    "Remove from playlist".to_owned(),
                                ),
                                n => (
                                    format!("Duplicate {n} in playlist"),
                                    format!("Remove {n} from playlist"),
                                ),
                            };
                            if ui
                                .button(duplicate)
                                .on_hover_text("Another copy of it, right after it")
                                .clicked()
                            {
                                actions.push(TrackAction::Duplicate(entries.clone(), playlist));
                                ui.close();
                            }
                            if ui
                                .button("Insert spacer")
                                .on_hover_text(SPACER_HINT)
                                .clicked()
                            {
                                actions.push(TrackAction::Spacer(row.entry, playlist));
                                ui.close();
                            }
                            if ui.button(remove).clicked() {
                                actions.push(TrackAction::RemoveFrom(entries, playlist));
                                ui.close();
                            }
                        }
                        ui.separator();
                        // The track itself.
                        if ui.button("Edit track info…").clicked() {
                            actions.push(TrackAction::Edit(id));
                            ui.close();
                        }
                        if ui
                            .button(format!("Analyze BPM, grid and key{many}"))
                            .on_hover_text("Detect grid and key again")
                            .clicked()
                        {
                            actions.push(TrackAction::Analyze(ids.clone()));
                            ui.close();
                        }
                        if ui
                            .button(format!("Convert…{many}"))
                            .on_hover_text("To another format, bitrate or sample rate (ffmpeg)")
                            .clicked()
                        {
                            actions.push(TrackAction::Convert(ids.clone()));
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
                                        "Separate stems now",
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
                                .on_hover_text("Copy cues, grid and key from the original")
                                .clicked()
                        {
                            actions.push(TrackAction::UpdateFromOriginal(id));
                            ui.close();
                        }
                        if let Some(path) = paths.get(&id) {
                            ui.separator();
                            if ui.button(reveal_label()).clicked() {
                                reveal(path);
                                ui.close();
                            }
                        }
                        ui.separator();
                        let forget = match ids.len() {
                            1 => "Remove from collection…".to_owned(),
                            n => format!("Remove {n} tracks from collection…"),
                        };
                        if ui.button(forget).clicked() {
                            actions.push(TrackAction::Forget(ids.clone()));
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
        // Delete (Backspace on a Mac's keyboard): out of the playlist shown,
        // or (all tracks shown) out of the collection, after asking.
        let delete = ui.input(|i| {
            i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)
        });
        if delete
            && self.focus == Focus::Tracks
            && self.collection_view.confirm_forget.is_none()
            && !ui.ctx().wants_keyboard_input()
        {
            match in_playlist {
                Some(playlist) => {
                    actions.push(TrackAction::RemoveFrom(self.selected_entries(), playlist));
                }
                None if !self.selected_ids().is_empty() => {
                    actions.push(TrackAction::Forget(self.selected_ids()));
                }
                None => {}
            }
        }
        for action in actions {
            match action {
                TrackAction::Load(id, deck) => load = Some((id, deck)),
                TrackAction::AddTo(ids, playlist) if ids.len() == 1 => {
                    self.request_add(ids[0], playlist)
                }
                TrackAction::AddTo(ids, playlist) => self.add_many(&ids, playlist),
                TrackAction::RemoveFrom(entries, playlist) => {
                    self.remove_entries(playlist, entries);
                }
                TrackAction::Forget(ids) => self.collection_view.confirm_forget = Some(ids),
                TrackAction::Spacer(at, playlist) => self.insert_spacer(playlist, at),
                TrackAction::Duplicate(entries, playlist) => {
                    self.duplicate_entries(playlist, entries);
                }
                TrackAction::Analyze(ids) => self.analyze(ids, true),
                TrackAction::Convert(ids) => {
                    let replace = self.config.prefs.convert_replace.unwrap_or(false);
                    self.collection_view.converting = Some((ids, replace, false));
                }
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
    /// (playlist positions, playlist).
    RemoveFrom(Vec<usize>, u32),
    /// Another copy of these entries, each right after itself.
    Duplicate(Vec<usize>, u32),
    /// A spacer before this entry (at the end without one) of the playlist.
    Spacer(Option<usize>, u32),
    Forget(Vec<u32>),
    Edit(u32),
    Analyze(Vec<u32>),
    Convert(Vec<u32>),
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

pub(super) fn rofloxbox_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub(super) fn reveal_label() -> &'static str {
    if cfg!(windows) {
        "Show in Explorer"
    } else if cfg!(target_os = "macos") {
        "Show in Finder"
    } else {
        "Show in file manager"
    }
}

/// Opens the file manager with the file (or folder) selected; elsewhere
/// than macOS and Windows, opens the folder it is in.
pub(super) fn reveal(path: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Explorer wants `/select,"C:\a b\c"`: quoted the usual way (the
        // whole argument) it doesn't understand it and opens Documents.
        let _ = std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", windows_path(path)))
            .spawn();
    }
    #[cfg(not(windows))]
    let _ = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg("-R").arg(path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn()
    };
}

/// Opens a folder in the file manager.
pub(crate) fn open_folder(dir: &std::path::Path) {
    let _ = if cfg!(windows) {
        std::process::Command::new("explorer").arg(windows_path(dir)).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(dir).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(dir).spawn()
    };
}

/// `path` as Explorer reads it: backslashes only (a path saved as
/// `C:/Users/…` opens Documents instead).
fn windows_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

#[cfg(test)]
mod reveal_tests {
    #[test]
    fn explorer_gets_backslashes() {
        let path = std::path::Path::new("C:/Users/someone/Music/RofLoxBox Recordings/Set 1/Set 1.session");
        assert_eq!(super::windows_path(path), "C:\\Users\\someone\\Music\\RofLoxBox Recordings\\Set 1\\Set 1.session");
    }
}

/// Where copies of rekordbox's library are kept before RofLoxBox changes it.
fn rekordbox_backups() -> PathBuf {
    rofloxbox_library::data_dir().join("rekordbox-backups")
}

/// A save into rekordbox's library running in the background.
type RekordboxSave = std::sync::mpsc::Receiver<anyhow::Result<crate::rekordbox_save::Saved>>;

/// Saves into rekordbox's library at `path` with the key `saved` (fetched
/// when there is none).
fn save_into_rekordbox(
    path: &std::path::Path,
    saved: &str,
    snapshot: &crate::rekordbox_save::Snapshot,
    wait_closed: bool,
) -> anyhow::Result<crate::rekordbox_save::Saved> {
    let key = match saved.trim() {
        "" => crate::rekordbox_db::fetch_key()?,
        key => key.to_owned(),
    };
    crate::rekordbox_save::save(path, &key, snapshot, &rekordbox_backups(), wait_closed)
}

/// While playlists are dragged: how many, next to the pointer.
fn playlist_drag_hint(ctx: &egui::Context) {
    let Some(drag) = egui::DragAndDrop::payload::<PlaylistDrag>(ctx) else { return };
    let Some(pointer) = ctx.pointer_hover_pos() else { return };
    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    egui::Area::new(egui::Id::new("playlist drag hint"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pointer + egui::vec2(14.0, 10.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| match drag.0.len() {
                1 => ui.label("1 playlist"),
                n => ui.label(format!("{n} playlists and folders")),
            });
        });
}

/// A spacer's title (and its file's name).
const SPACER: &str = "---";
const SPACER_RATE: u32 = 44_100;
const SPACER_HINT: &str = "A second of silence called \"---\", before this track, to mark a \
                           break in the set";

impl RofLoxBoxApp {
    /// Each collection track's song (see [`song_key`]); spacers have none.
    fn songs(&self) -> std::collections::HashMap<u32, String> {
        let spacer = spacer_path();
        self.collection
            .tracks
            .iter()
            .filter(|t| !is_spacer(t, &spacer))
            .map(|t| (t.id, song_key(t)))
            .collect()
    }
}

/// How many songs `tracks` holds: copies of one (in the list, or as other
/// files of the same song) count once, spacers not at all.
fn unique_songs(songs: &std::collections::HashMap<u32, String>, tracks: &[u32]) -> usize {
    let mut seen = std::collections::HashSet::new();
    tracks.iter().filter_map(|t| songs.get(t)).filter(|key| seen.insert(*key)).count()
}

/// What makes two tracks the same song: artist and title (without case or
/// extra spaces); a track without a title is its own.
fn song_key(track: &rofloxbox_library::Track) -> String {
    let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    if track.title.trim().is_empty() {
        format!("file:{}", track.path.display())
    } else {
        format!("{}\u{1f}{}", words(&track.artist), words(&track.title))
    }
}

/// A spacer between tracks of a playlist, not a track: RofLoxBox's own
/// (`spacer`), one titled only with dashes ("---"), or a short silent file
/// made by hand (two seconds at most, its file named "silent…").
fn is_spacer(track: &rofloxbox_library::Track, spacer: &std::path::Path) -> bool {
    spacer_like(&track.title, &track.path, track.duration, spacer)
}

fn spacer_like(
    title: &str,
    path: &std::path::Path,
    duration: f64,
    spacer: &std::path::Path,
) -> bool {
    let title = title.trim();
    let silent_file = duration <= 2.0
        && path
            .file_stem()
            .is_some_and(|s| s.to_string_lossy().to_lowercase().starts_with("silent"));
    path == spacer || (!title.is_empty() && title.chars().all(|c| c == '-')) || silent_file
}

/// Where the spacer's audio is (see [`spacer_file`]).
fn spacer_path() -> PathBuf {
    rofloxbox_library::data_dir().join("Spacer").join(format!("{SPACER}.wav"))
}

/// The spacer's audio, a second of silence, made the first time it's needed
/// in the settings folder.
fn spacer_file() -> anyhow::Result<PathBuf> {
    let path = spacer_path();
    if path.is_file() {
        return Ok(path);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 16-bit stereo PCM WAV.
    let data_len = SPACER_RATE * 2 * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // channels
    wav.extend_from_slice(&SPACER_RATE.to_le_bytes());
    wav.extend_from_slice(&(SPACER_RATE * 4).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&4u16.to_le_bytes()); // bytes per frame
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(44 + data_len as usize, 0);
    std::fs::write(&path, wav)?;
    Ok(path)
}

#[cfg(test)]
mod spacer_tests {
    use super::*;

    #[test]
    fn spacers_are_told_from_tracks() {
        let ours = PathBuf::from("/data/Spacer/---.wav");
        let track = |title: &'static str, path: &'static str, duration: f64| {
            (title, std::path::Path::new(path), duration)
        };
        let is_spacer = |(title, path, duration): (&str, &std::path::Path, f64), ours: &PathBuf| {
            spacer_like(title, path, duration, ours)
        };
        assert!(is_spacer(track("---", "/data/Spacer/---.wav", 1.0), &ours));
        assert!(is_spacer(track("-----", "/music/break.wav", 4.0), &ours));
        assert!(is_spacer(track("", "/music/silent-1.mp3", 1.0), &ours));
        assert!(!is_spacer(track("Silent Night", "/music/silent night.mp3", 180.0), &ours));
        assert!(!is_spacer(track("House 1", "/music/House1.wav", 1.0), &ours));
    }
}
