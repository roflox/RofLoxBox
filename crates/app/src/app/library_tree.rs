//! The library sidebar: one tree with the collection and every opened
//! rekordbox export (USB sticks are found and dropped as they come and go).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText, ScrollArea, Ui};

use super::{DeckhandApp, Focus, Source};
use crate::library::{UsbLibrary, eject, find_exports};
use crate::usb_export::{ExportRequest, UsbExport};
use crate::usb_import::{ImportRequest, UsbImport, default_destination, playlist_track_ids};
use crate::widgets::disclosure;
use deckhand_formats::pdb::{PlaylistKind, PlaylistNode};

/// How often mounted drives are checked for exports.
const USB_CHECK_EVERY: Duration = Duration::from_secs(2);

/// A place in the tree the browse knob can select.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TreeEntry {
    /// The collection: everything (`None`) or a playlist.
    Collection(Option<u32>),
    /// An export: all its tracks (`None`) or a playlist (index).
    Usb(PathBuf, Option<usize>),
    /// A folder in the explorer.
    Folder(PathBuf),
}

/// A drive being ejected in the background.
pub(super) struct Ejecting {
    root: PathBuf,
    done: mpsc::Receiver<anyhow::Result<()>>,
}

/// An export being imported into the collection.
pub(super) struct Importing {
    root: PathBuf,
    /// The collection folder its playlists go into.
    folder: String,
    /// Rebuild that folder (whole export) or add to it (one playlist).
    replace: bool,
    job: UsbImport,
}

/// Tracks no playlist uses, waiting for the user to confirm their removal.
pub(super) struct Cleanup {
    root: PathBuf,
    ids: Vec<u32>,
    bytes: u64,
}

/// Unused tracks being removed: (export root, removed tracks and bytes).
pub(super) struct Cleaning {
    root: PathBuf,
    done: mpsc::Receiver<anyhow::Result<(usize, u64)>>,
}

/// A collection playlist being exported to a stick.
pub(super) struct Exporting {
    root: PathBuf,
    playlist: String,
    job: UsbExport,
}

/// A change to an export's playlists.
pub(super) enum UsbEdit {
    Create {
        parent: Option<u32>,
        folder: bool,
    },
    Rename(u32, String),
    Delete(u32),
    AddTracks(u32, Vec<u32>),
    /// Playlist, position (from 0).
    RemoveEntry(u32, usize),
}

/// Sidebar state that is not per export.
pub(super) struct LibraryTree {
    pub(super) collection_open: bool,
    /// A stick playlist being renamed: (export root, playlist id, name).
    renaming: Option<(PathBuf, u32, String)>,
    checked: Instant,
    ejecting: Option<Ejecting>,
    importing: Option<Importing>,
    exporting: Option<Exporting>,
    cleanup: Option<Cleanup>,
    cleaning: Option<Cleaning>,
}

impl Default for LibraryTree {
    fn default() -> Self {
        Self {
            collection_open: true,
            renaming: None,
            checked: Instant::now(),
            ejecting: None,
            importing: None,
            exporting: None,
            cleanup: None,
            cleaning: None,
        }
    }
}

impl DeckhandApp {
    /// The export selected in the tree.
    pub(super) fn usb(&self) -> Option<&UsbLibrary> {
        match &self.source {
            Source::Usb(root) => self.libraries.iter().find(|l| &l.root == root),
            Source::Collection | Source::Explorer => None,
        }
    }

    fn usb_mut(&mut self, root: &Path) -> Option<&mut UsbLibrary> {
        self.libraries.iter_mut().find(|l| l.root == root)
    }

    /// Opens an export (once) and, with `select`, shows it.
    pub(super) fn open_usb(&mut self, root: PathBuf, select: bool) {
        if !self.libraries.iter().any(|l| l.root == root) {
            match UsbLibrary::open(&root) {
                Ok(library) => {
                    self.libraries.push(library);
                    self.library_error = None;
                }
                Err(err) => {
                    self.library_error = Some(format!("{err:#}"));
                    return;
                }
            }
        }
        if select {
            self.set_source(Source::Usb(root));
        }
    }

    /// Drops an export from the tree; its loaded tracks keep playing.
    fn close_usb(&mut self, root: &Path) {
        self.libraries.retain(|l| l.root != root);
        if matches!(&self.source, Source::Usb(r) if r == root) {
            let next = self.libraries.first().map(|l| l.root.clone());
            self.set_source(next.map_or(Source::Collection, Source::Usb));
        }
    }

    /// Adds exports on newly mounted drives, drops those of removed ones,
    /// and finishes an eject. Call every frame.
    pub(super) fn poll_usb(&mut self, ctx: &egui::Context) {
        self.poll_import(ctx);
        self.poll_export(ctx);
        self.poll_cleaning(ctx);
        if let Some(ejecting) = &self.tree.ejecting
            && let Ok(result) = ejecting.done.try_recv()
        {
            let root = ejecting.root.clone();
            self.tree.ejecting = None;
            match result {
                Ok(()) => {
                    self.status = Some(format!("{} ejected: safe to remove", volume_name(&root)));
                    self.close_usb(&root);
                }
                Err(err) => self.library_error = Some(format!("Could not eject: {err:#}")),
            }
        }
        if self.tree.checked.elapsed() < USB_CHECK_EVERY {
            ctx.request_repaint_after(USB_CHECK_EVERY);
            return;
        }
        self.tree.checked = Instant::now();
        let found = find_exports();
        let gone: Vec<PathBuf> = self
            .libraries
            .iter()
            .filter(|l| l.removable && !found.contains(&l.root))
            .map(|l| l.root.clone())
            .collect();
        for root in gone {
            self.close_usb(&root);
        }
        let ejecting = self.tree.ejecting.as_ref().map(|e| e.root.clone());
        for root in found {
            if Some(&root) != ejecting.as_ref() && !self.libraries.iter().any(|l| l.root == root) {
                // Show a stick plugged in while nothing else is to browse.
                let select = self.libraries.is_empty() && self.collection.tracks.is_empty();
                self.open_usb(root, select);
            }
        }
        ctx.request_repaint_after(USB_CHECK_EVERY);
    }

    /// Ejects a drive in the background; decks keep playing what they have
    /// loaded (the audio is in memory).
    /// Copies an export's music and analysis to this computer and adds its
    /// tracks and playlists (all, or the playlist or folder at row
    /// `playlist`) to the collection, in the background.
    fn start_import(&mut self, root: &Path, playlist: Option<usize>) {
        if self.tree.importing.is_some() {
            self.status = Some("An import is running already.".into());
            return;
        }
        let Some(library) = self.libraries.iter().find(|l| l.root == root) else { return };
        let name = library.name();
        let all = &library.library;
        let (playlists, tracks, replace) = match playlist {
            None => (all.playlists.clone(), all.tracks.clone(), true),
            Some(row) => {
                let Some(node) = nth_node(&all.playlists, row, &mut 0) else { return };
                let mut ids = Vec::new();
                playlist_track_ids(std::slice::from_ref(node), &mut ids);
                let tracks = ids.iter().filter_map(|id| all.track(*id).cloned()).collect();
                (vec![node.clone()], tracks, false)
            }
        };
        let request = ImportRequest {
            root: root.to_path_buf(),
            destination: default_destination().join(&name),
            tracks,
            playlists,
        };
        self.tree.importing = Some(Importing {
            root: root.to_path_buf(),
            folder: format!("USB {name}"),
            replace,
            job: UsbImport::start(request),
        });
    }

    /// Puts a collection playlist on a stick (or in a folder, which gets a
    /// new export), in the background.
    pub(super) fn start_export(&mut self, playlist: u32, root: PathBuf) {
        if self.tree.exporting.is_some() {
            self.status = Some("An export is running already.".into());
            return;
        }
        let Some(node) = self.collection.node(playlist) else { return };
        let tracks =
            node.tracks.iter().filter_map(|id| self.collection.track(*id).cloned()).collect();
        let request = ExportRequest {
            root: root.clone(),
            playlist: node.name.clone(),
            tracks,
            key_notation: self.config.prefs.key_notation,
            analysis: self.config.prefs.analysis,
        };
        let name = node.name.clone();
        self.status = Some(format!("Exporting {name} to {}…", volume_name(&root)));
        self.tree.exporting =
            Some(Exporting { root, playlist: name, job: UsbExport::start(request) });
    }

    /// Asks to remove the tracks no playlist on the stick uses.
    fn ask_cleanup(&mut self, root: &Path) {
        let Some(library) = self.libraries.iter().find(|l| l.root == root) else { return };
        let used: std::collections::HashSet<u32> = library
            .playlists
            .iter()
            .filter_map(|r| r.track_ids.as_ref())
            .flatten()
            .copied()
            .collect();
        let unused: Vec<&deckhand_formats::pdb::LibraryTrack> =
            library.library.tracks.iter().filter(|t| !used.contains(&t.track.id)).collect();
        if unused.is_empty() {
            self.status = Some(format!("Every track on {} is in a playlist.", library.name()));
            return;
        }
        let bytes = unused.iter().map(|t| u64::from(t.track.file_size)).sum();
        let ids = unused.iter().map(|t| t.track.id).collect();
        self.tree.cleanup = Some(Cleanup { root: root.to_path_buf(), ids, bytes });
    }

    /// The confirmation for removing unused tracks.
    pub(super) fn cleanup_dialog(&mut self, ctx: &egui::Context) {
        let Some(cleanup) = &self.tree.cleanup else { return };
        let name = volume_name(&cleanup.root);
        let (mut confirm, mut cancel) = (false, false);
        egui::Window::new(format!("Remove unused tracks from {name}?"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} track(s) ({:.1} GB) are not in any playlist on this stick.",
                    cleanup.ids.len(),
                    cleanup.bytes as f64 / 1e9
                ));
                ui.label(
                    "Their music and analysis files are deleted from the stick. This cannot be undone; the stick's database is backed up first.",
                );
                ui.horizontal(|ui| {
                    let delete = egui::Button::new(RichText::new("Delete").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(170, 40, 40));
                    confirm = ui.add(delete).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if cancel {
            self.tree.cleanup = None;
        }
        if confirm && let Some(cleanup) = self.tree.cleanup.take() {
            let (tx, done) = mpsc::channel();
            let root = cleanup.root.clone();
            std::thread::spawn(move || {
                let _ = tx.send(remove_unused(&root, &cleanup.ids));
            });
            self.status = Some(format!("Removing unused tracks from {name}…"));
            self.tree.cleaning = Some(Cleaning { root: cleanup.root, done });
        }
    }

    /// Finishes removing unused tracks: the stick's library is read again.
    fn poll_cleaning(&mut self, ctx: &egui::Context) {
        let Some(cleaning) = &self.tree.cleaning else { return };
        let Ok(result) = cleaning.done.try_recv() else {
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        };
        let root = cleaning.root.clone();
        self.tree.cleaning = None;
        let open = self.libraries.iter().find(|l| l.root == root).map(|l| l.open);
        self.libraries.retain(|l| l.root != root);
        self.open_usb(root.clone(), false);
        if let Some(open) = open
            && let Some(library) = self.usb_mut(&root)
        {
            library.open = open;
        }
        self.status = Some(match result {
            Ok((tracks, bytes)) => format!(
                "Removed {tracks} unused track(s) from {}, {:.1} GB freed.",
                volume_name(&root),
                bytes as f64 / 1e9
            ),
            Err(err) => format!("Removing unused tracks: {err:#}"),
        });
    }

    /// Finishes an export: the stick's library is read again.
    fn poll_export(&mut self, ctx: &egui::Context) {
        let Some(exporting) = &self.tree.exporting else { return };
        let Some(result) = exporting.job.finished() else {
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        };
        let Some(exporting) = self.tree.exporting.take() else { return };
        let root = exporting.root;
        match result {
            Ok(done) => {
                self.libraries.retain(|l| l.root != root);
                self.open_usb(root.clone(), false);
                let one_library =
                    self.libraries.iter().any(|l| l.root == root && l.has_one_library());
                let mut text = format!(
                    "Exported {} to {}: {} track(s) copied{}.",
                    exporting.playlist,
                    volume_name(&root),
                    done.added,
                    if done.replaced { ", playlist replaced" } else { "" },
                );
                if done.missing > 0 {
                    text += &format!(" {} track(s) were missing on this computer.", done.missing);
                }
                if one_library {
                    text += " Its OneLibrary still has the old playlists: in rekordbox, right-click the stick and choose Convert to OneLibrary.";
                }
                self.status = Some(text);
            }
            Err(err) => self.status = Some(format!("Export stopped: {err:#}")),
        }
    }

    /// Merges a finished import into the collection.
    fn poll_import(&mut self, ctx: &egui::Context) {
        let Some(importing) = &self.tree.importing else { return };
        let Some(result) = importing.job.finished() else {
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        };
        let Some(importing) = self.tree.importing.take() else { return };
        match result {
            Ok(imported) => {
                let summary = self.collection.import_into(
                    imported.library,
                    &importing.folder,
                    importing.replace,
                );
                let missing = if imported.missing > 0 {
                    format!(" {} track(s) were not on the stick.", imported.missing)
                } else {
                    String::new()
                };
                self.status = Some(format!(
                    "Imported {} new and {} known track(s) and {} playlist(s) into the collection folder {}; the music is in {}.{missing}",
                    summary.added,
                    summary.updated,
                    summary.playlists,
                    importing.folder,
                    default_destination().display(),
                ));
                self.tree.collection_open = true;
            }
            Err(err) => self.status = Some(format!("Import stopped: {err:#}")),
        }
    }

    /// Changes the playlists of an export in its `export.pdb` (backed up
    /// before the first change) and reads it again.
    pub(super) fn edit_usb(&mut self, root: &Path, edit: UsbEdit) {
        let Some(library) = self.libraries.iter().find(|l| l.root == root) else { return };
        let pdb = library.pdb_path();
        let one_library = library.has_one_library();
        let selected = library.playlist.and_then(|i| library.playlists.get(i)).map(|r| r.id);
        let closed: Vec<u32> = library
            .collapsed
            .iter()
            .filter_map(|&i| library.playlists.get(i))
            .map(|r| r.id)
            .collect();
        let result = deckhand_formats::pdb::edit_playlists(&pdb, |t| match edit {
            UsbEdit::Create { parent, folder } => {
                let name = if folder { "New folder" } else { "New playlist" };
                Some((t.create(name, folder, parent), name))
            }
            UsbEdit::Rename(id, name) => {
                t.rename(id, &name);
                None
            }
            UsbEdit::Delete(id) => {
                t.delete(id);
                None
            }
            UsbEdit::AddTracks(id, tracks) => {
                t.add_tracks(id, &tracks);
                None
            }
            UsbEdit::RemoveEntry(id, position) => {
                t.remove_entry(id, position);
                None
            }
        });
        let created = match result {
            Ok(created) => created,
            Err(err) => {
                self.library_error = Some(format!("Changing the stick's playlists: {err:#}"));
                return;
            }
        };
        // Read it again, keeping how the section looked.
        match UsbLibrary::open(root) {
            Ok(mut fresh) => {
                if let Some(old) = self.libraries.iter_mut().find(|l| l.root == root) {
                    fresh.open = old.open;
                    fresh.playlist =
                        selected.and_then(|id| fresh.playlists.iter().position(|r| r.id == id));
                    fresh.collapsed = (0..fresh.playlists.len())
                        .filter(|&i| closed.contains(&fresh.playlists[i].id))
                        .collect();
                    *old = fresh;
                }
            }
            Err(err) => self.library_error = Some(format!("{err:#}")),
        }
        if let Some((id, name)) = created {
            self.tree.renaming = Some((root.to_path_buf(), id, name.to_owned()));
        }
        if one_library {
            self.status = Some(
                "Changed the stick's device library. Its OneLibrary still has the old playlists: \
                 in rekordbox, right-click the stick and choose Convert to OneLibrary."
                    .into(),
            );
        }
    }

    fn start_eject(&mut self, root: PathBuf) {
        if self.tree.importing.as_ref().is_some_and(|i| i.root == root)
            || self.tree.exporting.as_ref().is_some_and(|e| e.root == root)
        {
            self.status = Some("Wait for the import or export to finish, then eject.".into());
            return;
        }
        if self.decks.iter().any(|d| d.loading_title().is_some()) {
            self.status = Some("Wait for the track to finish loading, then eject.".into());
            return;
        }
        let (tx, done) = mpsc::channel();
        let path = root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(eject(&path));
        });
        self.status = Some(format!("Ejecting {}…", volume_name(&root)));
        self.tree.ejecting = Some(Ejecting { root, done });
    }

    /// Everything the browse knob steps through, in tree order: playlists
    /// (and "all tracks") of open sections; folders are skipped.
    pub(super) fn tree_entries(&self) -> Vec<TreeEntry> {
        let mut entries = Vec::new();
        if self.tree.collection_open {
            entries.extend(self.collection_browse_entries().into_iter().map(TreeEntry::Collection));
        }
        for library in self.libraries.iter().filter(|l| l.open) {
            entries.push(TreeEntry::Usb(library.root.clone(), None));
            let mut hidden_below: Option<usize> = None;
            for (i, row) in library.playlists.iter().enumerate() {
                if hidden_below.is_some_and(|d| row.depth > d) {
                    continue;
                }
                hidden_below = None;
                match &row.track_ids {
                    None if library.collapsed.contains(&i) => hidden_below = Some(row.depth),
                    None => {}
                    Some(_) => entries.push(TreeEntry::Usb(library.root.clone(), Some(i))),
                }
            }
        }
        entries.extend(self.explorer_folders().into_iter().map(TreeEntry::Folder));
        entries
    }

    /// The tree entry shown in the list.
    pub(super) fn current_entry(&self) -> TreeEntry {
        match &self.source {
            Source::Collection => TreeEntry::Collection(self.collection_view.node),
            Source::Usb(root) => TreeEntry::Usb(root.clone(), self.usb().and_then(|l| l.playlist)),
            Source::Explorer => {
                self.explorer.folder.clone().map_or(TreeEntry::Collection(None), TreeEntry::Folder)
            }
        }
    }

    pub(super) fn select_entry(&mut self, entry: TreeEntry) {
        self.marked.clear();
        match entry {
            TreeEntry::Collection(node) => {
                self.set_source(Source::Collection);
                self.collection_view.node = node;
            }
            TreeEntry::Usb(root, playlist) => {
                if let Some(library) = self.usb_mut(&root) {
                    library.playlist = playlist;
                }
                self.set_source(Source::Usb(root));
            }
            TreeEntry::Folder(folder) => self.open_explorer_folder(folder),
        }
    }

    /// The sidebar: the collection, then each export.
    pub(super) fn library_sidebar(&mut self, ui: &mut Ui) {
        if let Some(err) = &self.library_error {
            ui.colored_label(super::WARN, err);
        }
        ScrollArea::vertical().id_salt("library tree").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_width(230.0);
            self.collection_section(ui);
            let roots: Vec<PathBuf> = self.libraries.iter().map(|l| l.root.clone()).collect();
            for root in roots {
                ui.add_space(4.0);
                self.usb_section(ui, &root);
            }
            ui.add_space(4.0);
            self.explorer_section(ui);
            ui.add_space(6.0);
            if ui
                .small_button("Open export folder…")
                .on_hover_text("A folder with a PIONEER folder")
                .clicked()
                && let Some(root) = rfd::FileDialog::new()
                    .set_title("Choose the root of a rekordbox USB export")
                    .pick_folder()
            {
                self.open_usb(root, true);
            }
        });
    }

    fn collection_section(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if disclosure(ui, self.tree.collection_open).clicked() {
                self.tree.collection_open = !self.tree.collection_open;
            }
            ui.label(RichText::new("COLLECTION").strong());
            ui.menu_button("…", |ui| self.collection_tools(ui))
                .response
                .on_hover_text("Add music, import, new playlist");
        });
        self.collection_status(ui);
        if !self.tree.collection_open {
            return;
        }
        let selected = self.source == Source::Collection && self.collection_view.node.is_none();
        let all = format!("All tracks ({})", self.collection.tracks.len());
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            let response = ui.selectable_label(selected, all);
            if selected && self.reveal_playlist {
                response.scroll_to_me(Some(egui::Align::Center));
            }
            if response.clicked() {
                self.select_entry(TreeEntry::Collection(None));
                self.focus = Focus::Playlists;
            }
        });
        self.collection_tree_rows(ui);
    }

    fn usb_section(&mut self, ui: &mut Ui, root: &Path) {
        let Some(library) = self.libraries.iter().find(|l| l.root == root) else { return };
        let (open, removable, name) = (library.open, library.removable, library.name());
        let mut import: Option<Option<usize>> = None;
        let mut header_edit = None;
        let mut cleanup = false;
        let ejecting = self.tree.ejecting.as_ref().is_some_and(|e| e.root == root);
        let mut toggle_open = false;
        let mut action = None;
        ui.horizontal(|ui| {
            if disclosure(ui, open).clicked() {
                toggle_open = true;
            }
            let kind = if removable { "USB" } else { "EXPORT" };
            ui.label(RichText::new(format!("{kind}  {name}")).strong())
                .on_hover_text(root.display().to_string());
            ui.menu_button("…", |ui| {
                if ui.button("New playlist").clicked() {
                    header_edit = Some(UsbEdit::Create { parent: None, folder: false });
                    ui.close();
                }
                if ui.button("New folder").clicked() {
                    header_edit = Some(UsbEdit::Create { parent: None, folder: true });
                    ui.close();
                }
                ui.separator();
                if ui.button("Remove tracks not in any playlist…").clicked() {
                    cleanup = true;
                    ui.close();
                }
            })
            .response
            .on_hover_text("New playlist or folder on the stick");
            let importing = self.tree.importing.as_ref().is_some_and(|i| i.root == root);
            if !importing
                && ui
                    .small_button("Import")
                    .on_hover_text(
                        "Copy tracks, cues and playlists to this computer",
                    )
                    .clicked()
            {
                import = Some(None);
            }
            if ejecting {
                ui.spinner();
            } else if removable {
                if ui
                    .small_button("Eject")
                    .on_hover_text("Eject; loaded decks keep playing")
                    .clicked()
                {
                    action = Some(true);
                }
            } else if ui.small_button("×").on_hover_text("Close this export").clicked() {
                action = Some(false);
            }
        });
        if let Some(exporting) = self.tree.exporting.as_ref().filter(|e| e.root == root) {
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.spinner();
                let done = exporting.job.done.load(std::sync::atomic::Ordering::Relaxed);
                ui.label(
                    RichText::new(format!("Exporting {done}/{}", exporting.job.total)).small(),
                );
                cancel = ui.small_button("Cancel").clicked();
            });
            if cancel {
                exporting.job.cancel();
            }
        }
        if let Some(importing) = self.tree.importing.as_ref().filter(|i| i.root == root) {
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.spinner();
                let done = importing.job.done.load(std::sync::atomic::Ordering::Relaxed);
                ui.label(
                    RichText::new(format!("Importing {done}/{}", importing.job.total)).small(),
                );
                cancel = ui.small_button("Cancel").clicked();
            });
            if cancel {
                importing.job.cancel();
            }
        }
        if let Some(playlist) = import {
            self.start_import(root, playlist);
        }
        if cleanup {
            self.ask_cleanup(root);
        }
        if let Some(edit) = header_edit {
            self.edit_usb(root, edit);
            return;
        }
        match action {
            Some(true) => self.start_eject(root.to_path_buf()),
            Some(false) => self.close_usb(root),
            None => {}
        }
        if toggle_open && let Some(library) = self.usb_mut(root) {
            library.open = !library.open;
        }
        if !open || action.is_some() {
            return;
        }
        let Some(library) = self.libraries.iter().find(|l| l.root == root) else { return };
        let shown = matches!(&self.source, Source::Usb(r) if r == root);
        let current = library.playlist.filter(|_| shown);
        let mut clicked: Option<Option<usize>> = None;
        let mut toggle_folder = None;
        let mut import_row = None;
        let mut edit = None;
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            let selected = shown && current.is_none();
            let response = ui.selectable_label(
                selected,
                format!("All tracks ({})", library.library.tracks.len()),
            );
            if selected && self.reveal_playlist {
                response.scroll_to_me(Some(egui::Align::Center));
            }
            if response.clicked() {
                clicked = Some(None);
            }
        });
        // Rows inside closed folders are skipped.
        let mut hidden_below: Option<usize> = None;
        for (i, row) in library.playlists.iter().enumerate() {
            if hidden_below.is_some_and(|d| row.depth > d) {
                continue;
            }
            hidden_below = None;
            let folder = row.track_ids.is_none();
            let folder_open = !library.collapsed.contains(&i);
            if folder && !folder_open {
                hidden_below = Some(row.depth);
            }
            ui.horizontal(|ui| {
                ui.add_space(14.0 + row.depth as f32 * 14.0);
                if folder {
                    if disclosure(ui, folder_open).clicked() {
                        toggle_folder = Some(i);
                    }
                } else {
                    ui.add_space(14.0 + ui.spacing().item_spacing.x);
                }
                let label = match &row.track_ids {
                    Some(ids) => format!("♫ {} ({})", row.name, ids.len()),
                    None => row.name.clone(),
                };
                let selected = current == Some(i);
                if let Some((_, _, name)) =
                    self.tree.renaming.as_mut().filter(|(r, id, _)| r == root && *id == row.id)
                {
                    let field = ui.add(egui::TextEdit::singleline(name).desired_width(150.0));
                    field.request_focus();
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.tree.renaming = None;
                    } else if field.lost_focus() {
                        let name = name.trim().to_owned();
                        if !name.is_empty() && name != row.name {
                            edit = Some(UsbEdit::Rename(row.id, name));
                        }
                        self.tree.renaming = None;
                    }
                    return;
                }
                let response = ui
                    .add(egui::Button::selectable(selected, label).truncate())
                    .on_hover_text(&row.name);
                response.context_menu(|ui| {
                    let what = if folder { "folder" } else { "playlist" };
                    if folder {
                        if ui.button("New playlist here").clicked() {
                            edit = Some(UsbEdit::Create { parent: Some(row.id), folder: false });
                            ui.close();
                        }
                        if ui.button("New folder here").clicked() {
                            edit = Some(UsbEdit::Create { parent: Some(row.id), folder: true });
                            ui.close();
                        }
                    }
                    if ui.button("Rename").clicked() {
                        self.tree.renaming = Some((root.to_path_buf(), row.id, row.name.clone()));
                        ui.close();
                    }
                    let delete =
                        if folder { "Delete folder and its playlists" } else { "Delete playlist" };
                    if ui.button(delete).clicked() {
                        edit = Some(UsbEdit::Delete(row.id));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(format!("Import this {what} into the collection")).clicked() {
                        import_row = Some(i);
                        ui.close();
                    }
                });
                // Tracks of this stick dropped on one of its playlists.
                if !folder && response.dnd_hover_payload::<crate::browser::TrackDrag>().is_some() {
                    ui.painter().rect_stroke(
                        response.rect,
                        3.0,
                        egui::Stroke::new(2.0_f32, ui.visuals().selection.bg_fill),
                        egui::StrokeKind::Inside,
                    );
                }
                if !folder
                    && let Some(drag) = response.dnd_release_payload::<crate::browser::TrackDrag>()
                {
                    match &*drag {
                        crate::browser::TrackDrag::Usb(from, ids) if from.as_path() == root => {
                            edit = Some(UsbEdit::AddTracks(row.id, ids.clone()));
                        }
                        _ => {
                            self.status = Some(
                                "Only tracks of this stick can be dropped on its playlists \
                                 (export a collection playlist to put tracks on it)"
                                    .into(),
                            );
                        }
                    }
                }
                if selected && self.reveal_playlist {
                    response.scroll_to_me(Some(egui::Align::Center));
                }
                if response.clicked() {
                    if folder {
                        toggle_folder = Some(i);
                    } else {
                        clicked = Some(Some(i));
                    }
                }
            });
        }
        if let Some(row) = import_row {
            self.start_import(root, Some(row));
        }
        if let Some(edit) = edit {
            self.edit_usb(root, edit);
            return;
        }
        if let Some(i) = toggle_folder
            && let Some(library) = self.usb_mut(root)
            && !library.collapsed.remove(&i)
        {
            library.collapsed.insert(i);
        }
        if let Some(playlist) = clicked {
            self.select_entry(TreeEntry::Usb(root.to_path_buf(), playlist));
            self.focus = Focus::Playlists;
        }
    }
}

/// The playlist or folder at a row of the flattened tree (folders before
/// their contents, as [`UsbLibrary::playlists`] lists them).
fn nth_node<'a>(
    nodes: &'a [PlaylistNode],
    row: usize,
    seen: &mut usize,
) -> Option<&'a PlaylistNode> {
    for node in nodes {
        if *seen == row {
            return Some(node);
        }
        *seen += 1;
        if let PlaylistKind::Folder(children) = &node.kind
            && let Some(found) = nth_node(children, row, seen)
        {
            return Some(found);
        }
    }
    None
}

/// Removes tracks from an export and deletes their files (unless another
/// track still uses them), then folders left empty. Returns the number of
/// tracks and bytes removed.
fn remove_unused(root: &Path, ids: &[u32]) -> anyhow::Result<(usize, u64)> {
    let pdb = root.join("PIONEER/rekordbox/export.pdb");
    let removed = deckhand_formats::pdb::remove_tracks(&pdb, ids)?;
    let kept =
        deckhand_formats::pdb::Database::open(&pdb)?.read_all::<deckhand_formats::pdb::Track>()?;
    let still_used = |path: &str, of: fn(&deckhand_formats::pdb::Track) -> &str| {
        kept.iter().any(|t| of(t) == path)
    };
    let mut bytes = 0;
    let mut folders = Vec::new();
    let mut delete = |file: PathBuf, bytes: &mut u64| {
        if let Ok(meta) = std::fs::metadata(&file)
            && std::fs::remove_file(&file).is_ok()
        {
            *bytes += meta.len();
            if let Some(parent) = file.parent() {
                folders.push(parent.to_path_buf());
            }
        }
    };
    for track in &removed {
        if !still_used(&track.file_path, |t| &t.file_path) {
            delete(root.join(track.file_path.trim_start_matches('/')), &mut bytes);
        }
        if !track.analyze_path.is_empty() && !still_used(&track.analyze_path, |t| &t.analyze_path) {
            let dat = root.join(track.analyze_path.trim_start_matches('/'));
            for extension in ["EXT", "2EX"] {
                delete(dat.with_extension(extension), &mut bytes);
            }
            delete(dat, &mut bytes);
        }
    }
    // Folders left empty go too, up to Contents and USBANLZ.
    let stops = [root.join("Contents"), root.join("PIONEER/USBANLZ"), root.to_path_buf()];
    folders.sort();
    folders.dedup();
    for folder in folders {
        let mut current = Some(folder.as_path());
        while let Some(dir) = current {
            if stops.iter().any(|s| s == dir) || std::fs::remove_dir(dir).is_err() {
                break;
            }
            current = dir.parent();
        }
    }
    Ok((removed.len(), bytes))
}

/// The name shown for a drive or export folder.
fn volume_name(root: &Path) -> String {
    root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exports the tracks of `DECKHAND_TEST_EXPORT` to a new stick, deletes
    /// the playlist and removes the unused tracks (skipped without it).
    #[test]
    fn unused_tracks_and_their_files_go() {
        let Some(source) = std::env::var_os("DECKHAND_TEST_EXPORT").map(PathBuf::from) else {
            return;
        };
        let temp = std::env::temp_dir().join(format!("deckhand-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let library = UsbLibrary::open(&source).unwrap();
        let files = library
            .library
            .tracks
            .iter()
            .filter_map(|t| deckhand_library::read_file(&library.resolve(&t.track.file_path)).ok())
            .collect();
        let mut collection = deckhand_library::Collection::default();
        collection.merge(files);
        let tracks = collection.tracks.clone();
        let stick = temp.join("stick");
        crate::usb_export::run_for_tests(&crate::usb_export::ExportRequest {
            root: stick.clone(),
            playlist: "Gone".into(),
            tracks,
            key_notation: crate::deck::KeyNotation::Alphanumeric,
            analysis: crate::deck::AnalysisMethod::Rekordbox,
        })
        .unwrap();
        let pdb = stick.join("PIONEER/rekordbox/export.pdb");
        deckhand_formats::pdb::edit_playlists(&pdb, |t| {
            let ids: Vec<u32> = t.nodes.iter().map(|n| n.id).collect();
            for id in ids {
                t.delete(id);
            }
        })
        .unwrap();
        let exported = UsbLibrary::open(&stick).unwrap();
        let ids: Vec<u32> = exported.library.tracks.iter().map(|t| t.track.id).collect();
        let (count, bytes) = remove_unused(&stick, &ids).unwrap();
        assert_eq!(count, ids.len());
        assert!(bytes > 0);
        assert!(UsbLibrary::open(&stick).unwrap().library.tracks.is_empty());
        // Only the empty top folders are left.
        let left = |dir: &str| std::fs::read_dir(stick.join(dir)).map_or(0, |d| d.count());
        assert_eq!((left("Contents"), left("PIONEER/USBANLZ")), (0, 0));
        let _ = std::fs::remove_dir_all(&temp);
    }
}
