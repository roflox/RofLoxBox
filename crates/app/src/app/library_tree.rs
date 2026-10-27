//! The library sidebar: one tree with the collection and every opened
//! rekordbox export (USB sticks are found and dropped as they come and go).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText, ScrollArea, Ui};

use super::{DeckhandApp, Focus, Source};
use crate::library::{UsbLibrary, eject, find_exports};
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

/// Sidebar state that is not per export.
pub(super) struct LibraryTree {
    pub(super) collection_open: bool,
    checked: Instant,
    ejecting: Option<Ejecting>,
    importing: Option<Importing>,
}

impl Default for LibraryTree {
    fn default() -> Self {
        Self { collection_open: true, checked: Instant::now(), ejecting: None, importing: None }
    }
}

impl DeckhandApp {
    /// The export selected in the tree.
    pub(super) fn usb(&self) -> Option<&UsbLibrary> {
        match &self.source {
            Source::Usb(root) => self.libraries.iter().find(|l| &l.root == root),
            Source::Collection => None,
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

    fn start_eject(&mut self, root: PathBuf) {
        if self.tree.importing.as_ref().is_some_and(|i| i.root == root) {
            self.status = Some("Wait for the import from this stick to finish, then eject.".into());
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
        entries
    }

    /// The tree entry shown in the list.
    pub(super) fn current_entry(&self) -> TreeEntry {
        match &self.source {
            Source::Collection => TreeEntry::Collection(self.collection_view.node),
            Source::Usb(root) => TreeEntry::Usb(root.clone(), self.usb().and_then(|l| l.playlist)),
        }
    }

    pub(super) fn select_entry(&mut self, entry: TreeEntry) {
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
            ui.add_space(6.0);
            if ui
                .small_button("Open export folder…")
                .on_hover_text("A folder holding a rekordbox export (with a PIONEER folder)")
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
            let importing = self.tree.importing.as_ref().is_some_and(|i| i.root == root);
            if !importing
                && ui
                    .small_button("Import")
                    .on_hover_text(
                        "Copy its music and rekordbox's analysis to this computer and add its                          tracks, cues and playlists to the collection",
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
                    .on_hover_text("Eject safely: decks keep playing what they have loaded")
                    .clicked()
                {
                    action = Some(true);
                }
            } else if ui.small_button("×").on_hover_text("Close this export").clicked() {
                action = Some(false);
            }
        });
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
                let response = ui
                    .add(egui::Button::selectable(selected, label).truncate())
                    .on_hover_text(&row.name);
                response.context_menu(|ui| {
                    let what = if folder { "folder" } else { "playlist" };
                    if ui.button(format!("Import this {what} into the collection")).clicked() {
                        import_row = Some(i);
                        ui.close();
                    }
                });
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

/// The name shown for a drive or export folder.
fn volume_name(root: &Path) -> String {
    root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into())
}
