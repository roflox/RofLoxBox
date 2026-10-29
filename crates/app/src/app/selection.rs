//! Selecting tracks in the lists (click, Cmd/Ctrl-click, Shift-click,
//! select all, arrow keys) and dragging them onto playlists and decks.

use std::path::PathBuf;

use eframe::egui::{self, Key, Modifiers};

use super::{DeckhandApp, Focus, Source};
use crate::browser::TrackDrag;

impl DeckhandApp {
    /// A row clicked: alone, or with Cmd/Ctrl added to (or taken out of)
    /// the selection, or with Shift the rows from the current one to it.
    pub(super) fn click_track(&mut self, id: u32, position: usize, modifiers: Modifiers) {
        let rows = self.visible_rows();
        if modifiers.shift
            && let Some(anchor) = self.selected
        {
            let at = |target: (u32, usize)| rows.iter().position(|&r| r == target);
            let from = self
                .selected_position
                .and_then(|p| at((anchor, p)))
                .or_else(|| rows.iter().position(|r| r.0 == anchor));
            if let (Some(from), Some(to)) = (from, at((id, position))) {
                let (a, b) = (from.min(to), from.max(to));
                self.marked = rows[a..=b].iter().map(|r| r.0).collect();
                // The anchor stays where the range started.
                self.focus = Focus::Tracks;
                return;
            }
        }
        if modifiers.command {
            if self.marked.is_empty()
                && let Some(current) = self.selected
            {
                self.marked.insert(current);
            }
            if !self.marked.remove(&id) {
                self.marked.insert(id);
            }
        } else {
            self.marked.clear();
        }
        (self.selected, self.selected_position) = (Some(id), Some(position));
        self.focus = Focus::Tracks;
    }

    /// The selected tracks in list order (each once).
    pub(super) fn selected_ids(&self) -> Vec<u32> {
        if self.marked.is_empty() {
            return self.selected.into_iter().collect();
        }
        let mut seen = std::collections::HashSet::new();
        self.visible_rows()
            .into_iter()
            .map(|r| r.0)
            .filter(|id| self.marked.contains(id) && seen.insert(*id))
            .collect()
    }

    /// Every track of the list shown.
    pub(super) fn select_all(&mut self) {
        let rows = self.visible_rows();
        self.marked = rows.iter().map(|r| r.0).collect();
        if let Some(&(id, position)) = rows.first() {
            (self.selected, self.selected_position) = (Some(id), Some(position));
        }
        self.focus = Focus::Tracks;
    }

    /// A row started being dragged: it carries the selection when the row
    /// is part of it, otherwise just itself (which becomes the selection).
    pub(super) fn start_track_drag(&mut self, ctx: &egui::Context, id: u32, position: usize) {
        if !self.marked.contains(&id) && self.selected != Some(id) {
            self.click_track(id, position, Modifiers::NONE);
        }
        let ids = if self.marked.contains(&id) { self.selected_ids() } else { vec![id] };
        let drag = match &self.source {
            Source::Collection => TrackDrag::Collection(ids),
            Source::Usb(root) => TrackDrag::Usb(root.clone(), ids),
            Source::Explorer => TrackDrag::Files(
                ids.iter()
                    .filter_map(|&i| self.explorer.files_get(i).map(|f| f.path.clone()))
                    .collect(),
            ),
        };
        egui::DragAndDrop::set_payload(ctx, drag);
    }

    /// While tracks are dragged: how many, next to the pointer.
    pub(super) fn drag_hint(&self, ctx: &egui::Context) {
        let Some(drag) = egui::DragAndDrop::payload::<TrackDrag>(ctx) else { return };
        let Some(pointer) = ctx.pointer_hover_pos() else { return };
        let count = match &*drag {
            TrackDrag::Collection(ids) | TrackDrag::Usb(_, ids) => ids.len(),
            TrackDrag::Files(paths) => paths.len(),
        };
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        egui::Area::new(egui::Id::new("track drag hint"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pointer + egui::vec2(14.0, 10.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(if count == 1 {
                        "1 track".to_owned()
                    } else {
                        format!("{count} tracks")
                    });
                });
            });
    }

    /// Tracks dropped on a collection playlist.
    pub(super) fn drop_on_playlist(&mut self, drag: &TrackDrag, playlist: u32) {
        match drag {
            TrackDrag::Collection(ids) if ids.len() == 1 => self.request_add(ids[0], playlist),
            TrackDrag::Collection(ids) => self.add_many(ids, playlist),
            TrackDrag::Files(paths) => self.add_files_to_playlist(paths.clone(), playlist),
            TrackDrag::Usb(..) => {
                self.status = Some(
                    "Tracks of a USB export go into playlists once imported into the collection \
                     (right-click the stick)"
                        .into(),
                );
            }
        }
    }

    /// Adds tracks to a playlist, leaving out those already in it.
    pub(super) fn add_many(&mut self, ids: &[u32], playlist: u32) {
        let present: Vec<u32> =
            self.collection.node(playlist).map(|n| n.tracks.clone()).unwrap_or_default();
        let new: Vec<u32> = ids.iter().copied().filter(|id| !present.contains(id)).collect();
        let skipped = ids.len() - new.len();
        self.collection.add_to_playlist(playlist, &new);
        let name = self.collection.node(playlist).map_or_else(String::new, |n| n.name.clone());
        let already =
            if skipped > 0 { format!(" ({skipped} already in it)") } else { String::new() };
        self.status = Some(format!("Added {} track(s) to {name}{already}", new.len()));
    }

    /// Files dropped on a deck: the first one is loaded.
    pub(super) fn drop_on_deck(&mut self, drag: &TrackDrag, deck: usize, ctx: &egui::Context) {
        match drag {
            TrackDrag::Collection(ids) => {
                if let Some(&id) = ids.first() {
                    self.load_from_collection(id, deck, ctx);
                }
            }
            TrackDrag::Files(paths) => {
                if let Some(path) = paths.first().cloned() {
                    self.load_file(path, deck, ctx);
                }
            }
            TrackDrag::Usb(root, ids) => {
                let Some(&id) = ids.first() else { return };
                if self.load_blocked(deck) {
                    return;
                }
                self.mark_double(deck, id, false);
                let Some(library) = self.libraries.iter().find(|l| &l.root == root) else { return };
                let Some(track) = library.library.track(id) else { return };
                self.decks[deck].start_load(track, library, ctx);
            }
        }
    }

    /// Loads an audio file: as its collection track when it is in the
    /// collection, otherwise as a plain file.
    pub(super) fn load_file(&mut self, path: PathBuf, deck: usize, ctx: &egui::Context) {
        if let Some(id) = self.collection.tracks.iter().find(|t| t.path == path).map(|t| t.id) {
            self.load_from_collection(id, deck, ctx);
            return;
        }
        if self.load_blocked(deck) {
            return;
        }
        let file = deckhand_library::read_file(&path).unwrap_or_else(|_| {
            deckhand_library::ScannedFile { path: path.clone(), ..Default::default() }
        });
        let id = super::explorer_view::file_id(&path);
        self.mark_double(deck, id, false);
        self.decks[deck].start_load_path(id, &file, ctx);
    }

    /// Library keys, when no text field is being typed in: arrows browse
    /// (up/down move, left goes to the playlists or folders, right to the
    /// tracks), Page Up/Down and Home/End jump, Cmd/Ctrl+A selects all.
    pub(super) fn library_keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let pressed = |key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, key));
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
            self.select_all();
        }
        let steps = [
            (Key::ArrowUp, -1),
            (Key::ArrowDown, 1),
            (Key::PageUp, -10),
            (Key::PageDown, 10),
            (Key::Home, -100_000),
            (Key::End, 100_000),
        ];
        for (key, steps) in steps {
            if pressed(key) {
                self.browse(steps);
            }
        }
        if pressed(Key::ArrowLeft) {
            self.focus = Focus::Playlists;
        }
        if pressed(Key::ArrowRight) {
            self.focus = Focus::Tracks;
        }
    }
}
