//! Making tracks from stems (Make → acapella, instrumental, …) and the Stems
//! page of Preferences.

use std::path::PathBuf;

use deckhand_library::Derived;
use eframe::egui::{self, RichText, Ui};

use super::{DeckhandApp, WARN};
use crate::deck::Stems;
use crate::stems::{self, Event, Job, LiveJob, Recipe, StemModel};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

impl DeckhandApp {
    /// The chosen model, else the first one there is, with the Python
    /// from Preferences unless its model.toml names one.
    pub(super) fn stem_model(&self) -> Option<StemModel> {
        let models = &self.stem_models.0;
        let mut model = models
            .iter()
            .find(|m| m.id == self.config.prefs.stem_model)
            .or(models.first())?
            .clone();
        if model.python.is_none() {
            model.python = self.config.prefs.demucs_python.clone();
        }
        Some(model)
    }

    fn stems_folder(&self) -> PathBuf {
        self.config.prefs.stems_folder.clone().unwrap_or_else(stems::default_output_dir)
    }

    /// Queues the tracks for separation and making `recipe` of them.
    pub(super) fn make_stems(&mut self, ids: &[u32], recipe: &Recipe) {
        let Some(model) = self.stem_model() else { return };
        let folder = self.stems_folder();
        for &id in ids {
            let Some(track) = self.collection.track(id) else { continue };
            let path = stems::output_path(&folder, &track.artist, &track.title, &recipe.suffix);
            self.stems.submit(Job {
                track: id,
                title: track.title.clone(),
                input: track.path.clone(),
                model: model.clone(),
                makes: vec![(recipe.clone(), path)],
            });
        }
        self.status = Some(format!(
            "{} queued with {} ({} waiting)",
            recipe.label,
            model.name,
            self.stems.pending()
        ));
    }

    /// Separates tracks ahead of time, so they load with their stems.
    pub(super) fn prepare_stems(&mut self, ids: &[u32]) {
        let Some(model) = self.stem_model() else { return };
        for &id in ids {
            let Some(track) = self.collection.track(id) else { continue };
            self.stems.submit(Job {
                track: id,
                title: track.title.clone(),
                input: track.path.clone(),
                model: model.clone(),
                makes: Vec::new(),
            });
        }
        self.status = Some(format!(
            "Getting stems ready with {} ({} waiting)",
            model.name,
            self.stems.pending()
        ));
    }

    /// Starts separating a deck's track for live stems (from the cache when
    /// it was done before).
    pub(super) fn separate_deck(&mut self, d: usize) {
        if !self.config.prefs.live_stems {
            return;
        }
        let Some(model) = self.stem_model() else { return };
        let deck = &mut self.decks[d];
        let (Some(track), Some(audio)) = (&deck.track, deck.audio()) else {
            return;
        };
        self.stem_playheads[d].store(stems::CANCELLED, Ordering::Relaxed);
        let position = self.engine.as_ref().map_or(0.0, |e| e.position_secs(d));
        self.stem_playheads[d] = Arc::new(AtomicU64::new(position.to_bits()));
        self.live_stems.submit(LiveJob {
            deck: d,
            title: track.title.clone(),
            input: track.path.clone(),
            model,
            audio,
            playhead: self.stem_playheads[d].clone(),
        });
        deck.stems = Stems::Separating(None);
    }

    /// Turns stems on the decks on or off. On, loaded tracks get separated
    /// (when tracks are separated on load); off, every deck plays its
    /// track only and the helper quits.
    pub(super) fn set_live_stems(&mut self, on: bool) {
        self.config.prefs.live_stems = on;
        if !on {
            self.stop_live_stems();
        } else if self.config.prefs.stems_on_load {
            for d in 0..self.deck_count() {
                if matches!(self.decks[d].stems, Stems::None | Stems::Failed) {
                    self.separate_deck(d);
                }
            }
        }
    }

    /// Stops live stems on every deck.
    fn stop_live_stems(&mut self) {
        for (d, playhead) in self.stem_playheads.iter().enumerate() {
            playhead.store(stems::CANCELLED, Ordering::Relaxed);
            if let Some(engine) = self.engine.as_mut() {
                self.decks[d].clear_stems(engine);
            }
        }
    }

    /// Newly loaded tracks get separated, when Preferences say so.
    fn separate_new_tracks(&mut self) {
        for d in 0..self.deck_count() {
            let path = self.decks[d].track.as_ref().map(|t| t.path.clone());
            if path == self.stems_for[d] {
                continue;
            }
            self.stems_for[d] = path.clone();
            // The earlier track's separation stops.
            self.stem_playheads[d].store(stems::CANCELLED, Ordering::Relaxed);
            if path.is_some() && self.config.prefs.stems_on_load && self.stem_model().is_some() {
                self.separate_deck(d);
            }
        }
    }

    /// Takes finished separations to the decks and the collection, and
    /// shows progress.
    pub(super) fn poll_stems(&mut self, ctx: &egui::Context) {
        // With stems on, the model is loaded ahead of the first track, once
        // the decks are done loading (not to slow the start down).
        let wanted = self.stem_model().filter(|_| self.config.prefs.live_stems);
        let settled = self.decks.iter().all(|d| d.loading_title().is_none());
        if settled && wanted != self.stems_warmed {
            self.stems_warmed = wanted.clone();
            match wanted {
                Some(model) => self.live_stems.warm(model),
                None => self.live_stems.rest(),
            }
        }
        self.separate_new_tracks();
        if let Some(engine) = self.engine.as_ref() {
            for (d, playhead) in self.stem_playheads.iter().enumerate() {
                let _ = playhead.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                    (bits != stems::CANCELLED).then(|| engine.position_secs(d).to_bits())
                });
            }
        }
        let events: Vec<Event> =
            self.live_stems.events.try_iter().chain(self.stems.events.try_iter()).collect();
        let current = |decks: &[crate::deck::Deck], deck: usize, input: &PathBuf| {
            decks[deck].track.as_ref().is_some_and(|t| &t.path == input)
        };
        for event in events {
            match event {
                Event::Progress { deck: Some(d), percent, .. } => {
                    if matches!(self.decks[d].stems, Stems::Separating(_)) {
                        self.decks[d].stems = Stems::Separating(percent);
                    }
                }
                Event::Progress { title, percent, deck: None } => {
                    let done = percent.map(|p| format!(": {p}%")).unwrap_or_default();
                    let waiting = self.stems.pending().saturating_sub(1);
                    let more = if waiting > 0 {
                        format!(" ({waiting} more waiting)")
                    } else {
                        String::new()
                    };
                    self.status = Some(format!("Separating {title}{done}{more}"));
                }
                Event::Live { deck, input, stems } => {
                    // A track loaded since then keeps its own.
                    if current(&self.decks, deck, &input)
                        && let Some(engine) = self.engine.as_mut()
                    {
                        self.decks[deck].set_stems(stems, engine);
                    }
                }
                Event::Made { track, recipe, path, model } => {
                    let made = deckhand_library::read_file(&path).and_then(|file| {
                        let derived =
                            Derived { original: track, stems: recipe.keep.clone(), model };
                        self.collection
                            .add_derived(track, file, &recipe.suffix, derived)
                            .ok_or_else(|| anyhow::anyhow!("the original track is gone"))
                    });
                    match made {
                        Ok(id) => {
                            let title = self.collection.track(id).map(|t| t.title.clone());
                            self.status = Some(format!("Made {}", title.unwrap_or_default()));
                        }
                        Err(err) => {
                            self.collection_view.error =
                                Some(format!("Could not add {}: {err:#}", path.display()))
                        }
                    }
                }
                Event::Failed { title, error, deck } => {
                    if let Some(d) = deck {
                        self.decks[d].stems = Stems::Failed;
                    }
                    self.collection_view.error =
                        Some(format!("Could not separate {title}: {error}"));
                }
            }
        }
        let separating = self.decks.iter().any(|d| match &d.stems {
            Stems::Separating(_) => true,
            Stems::Ready(layers) => layers.ready_share() < 1.0,
            _ => false,
        });
        if self.stems.pending() > 0 || separating {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    /// Which Python runs the fast Demucs helper: found by itself, or picked.
    fn demucs_python_prefs(&mut self, ui: &mut Ui) {
        let model = self.stem_model();
        let found = model.as_ref().and_then(stems::helper_python);
        ui.horizontal(|ui| {
            ui.label("Python for Demucs:");
            let (text, color) = match (&found, &self.config.prefs.demucs_python) {
                (Some(python), _) if !python.is_file() => {
                    (format!("{} (not there)", python.display()), Some(WARN))
                }
                (Some(python), Some(_)) => (python.display().to_string(), None),
                (Some(python), None) => (format!("{} (found)", python.display()), None),
                (None, _) if model.as_ref().is_some_and(stems::uses_demucs) => {
                    ("not found".to_owned(), Some(WARN))
                }
                (None, _) => ("not used by this model".to_owned(), None),
            };
            let text = RichText::new(text).small();
            ui.label(match color {
                Some(color) => text.color(color),
                None => text.weak(),
            });
        });
        ui.horizontal(|ui| {
            if ui.button("Choose Python…").clicked()
                && let Some(picked) = rfd::FileDialog::new()
                    .set_title("The Python Demucs is installed in")
                    .pick_file()
            {
                self.config.prefs.demucs_python = Some(picked);
            }
            if ui
                .add_enabled(
                    self.config.prefs.demucs_python.is_some(),
                    egui::Button::new("Find it"),
                )
                .clicked()
            {
                self.config.prefs.demucs_python = None;
            }
        });
        ui.label(
            RichText::new(
                "Demucs models separate live stems fastest in a helper that keeps the model \
                 loaded, run by the Python Demucs is installed in. Without it, each piece \
                 starts Demucs again and takes several times as long. With pipx on Windows it \
                 is usually %LOCALAPPDATA%\\pipx\\pipx\\venvs\\demucs\\Scripts\\python.exe.",
            )
            .small()
            .weak(),
        );
    }

    /// Whether separated stems stay on disk, and how much they take.
    fn stem_cache_prefs(&mut self, ui: &mut Ui) {
        let fresh = self.stem_cache_size.filter(|(at, _)| at.elapsed().as_secs() < 2);
        let bytes = match fresh {
            Some((_, bytes)) => bytes,
            None => {
                let bytes = stems::cache_size();
                self.stem_cache_size = Some((std::time::Instant::now(), bytes));
                bytes
            }
        };
        let separating = self.stems.pending() > 0
            || self.decks.iter().any(|d| match &d.stems {
                Stems::Separating(_) => true,
                Stems::Ready(layers) => layers.ready_share() < 1.0,
                _ => false,
            });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.config.prefs.keep_stems, "Keep stems on disk");
            ui.label(RichText::new(format!("{:.1} GB now", bytes as f64 / 1e9)).small().weak());
            if ui
                .add_enabled(bytes > 0 && !separating, egui::Button::new("Delete now"))
                .on_disabled_hover_text("Not while a track is being separated")
                .clicked()
            {
                stems::clear_cache();
                self.stem_cache_size = None;
            }
        });
        ui.label(
            RichText::new(
                "On, tracks separated before load with their stems at once, also after a \
                 restart (up to 2 GB; the tracks used least recently go first). Off, the \
                 stems are deleted when the app quits.",
            )
            .small()
            .weak(),
        );
    }

    /// The Stems page of Preferences; returns whether a setting changed.
    pub(super) fn stems_prefs(&mut self, ui: &mut Ui) -> bool {
        let p = &self.config.prefs;
        let before = (
            p.stem_model.clone(),
            p.stems_folder.clone(),
            p.stems_on_load,
            p.live_stems,
            p.demucs_python.clone(),
            p.keep_stems,
        );
        let dir = stems::models_dir();
        let chosen = self.stem_model().map(|m| m.id.clone());
        ui.label(RichText::new("Model").strong().size(15.0));
        if self.stem_models.0.is_empty() {
            ui.label("No models yet. Add one of the presets below, or a folder of your own.");
        }
        for model in &self.stem_models.0 {
            ui.horizontal(|ui| {
                if ui.radio(chosen.as_deref() == Some(model.id.as_str()), &model.name).clicked() {
                    self.config.prefs.stem_model = model.id.clone();
                }
                ui.label(RichText::new(model.stems.join(", ")).small().weak());
            });
        }
        for error in &self.stem_models.1 {
            ui.label(RichText::new(format!("Not read: {error}")).small().color(WARN));
        }
        ui.horizontal(|ui| {
            if ui.button("Open models folder").clicked() {
                let _ = std::fs::create_dir_all(&dir);
                super::collection_view::reveal(&dir);
            }
            if ui.button("Reload").clicked() {
                self.stem_models = stems::load_models(&dir);
            }
        });
        ui.label(
            RichText::new(
                "A model is a folder with a model.toml: its name, its stems and the command that \
                 separates a track ({input} into {output}). Any separation tool can be used.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);
        ui.collapsing("Add a preset", |ui| {
            for preset in &stems::PRESETS {
                ui.horizontal(|ui| {
                    let there = self.stem_models.0.iter().any(|m| m.id == preset.id);
                    if ui.add_enabled(!there, egui::Button::new(preset.name)).clicked() {
                        match stems::add_preset(&dir, preset) {
                            Ok(()) => {
                                self.stem_models = stems::load_models(&dir);
                                self.status = Some(format!(
                                    "Added {}; it needs the tool installed: {}",
                                    preset.name, preset.install
                                ));
                            }
                            Err(err) => self.collection_view.error = Some(format!("{err:#}")),
                        }
                    }
                    ui.label(RichText::new(preset.install).small().monospace().weak());
                });
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new("Live stems").strong().size(15.0));
        let mut on = self.config.prefs.live_stems;
        if ui
            .checkbox(&mut on, "Stems on the decks")
            .on_hover_text("Off: decks play tracks only and nothing is separated for them")
            .changed()
        {
            self.set_live_stems(on);
        }
        ui.add_enabled_ui(self.config.prefs.live_stems, |ui| {
            ui.checkbox(&mut self.config.prefs.stems_on_load, "Separate tracks when they load");
            self.demucs_python_prefs(ui);
        });
        ui.label(
            RichText::new(
                "The deck plays the track at once. Its stems are made a piece at a time, \
                 starting where it plays, and each piece plays as soon as it is ready (at once \
                 for tracks separated before). VOCAL, INST, BASS and DRUMS then turn parts \
                 off; with all of them on, the deck plays the original. Separating takes a lot \
                 of processing while it runs; turn stems off to keep the app light. Making \
                 tracks from stems works either way.",
            )
            .small()
            .weak(),
        );
        self.stem_cache_prefs(ui);
        ui.add_space(6.0);
        ui.label(RichText::new("Made tracks").strong().size(15.0));
        let folder = self.stems_folder();
        ui.label(folder.display().to_string());
        ui.horizontal(|ui| {
            if ui.button("Choose folder…").clicked()
                && let Some(picked) = rfd::FileDialog::new()
                    .set_title("Where tracks made from stems go")
                    .pick_folder()
            {
                self.config.prefs.stems_folder = Some(picked);
            }
            if ui
                .add_enabled(self.config.prefs.stems_folder.is_some(), egui::Button::new("Default"))
                .clicked()
            {
                self.config.prefs.stems_folder = None;
            }
        });
        ui.label(
            RichText::new(
                "Saved as 24-bit WAV, which keeps the original's timing exactly, so its cues, \
                 memory cues, loops and beat grid fit the made track.",
            )
            .small()
            .weak(),
        );
        let p = &self.config.prefs;
        (
            p.stem_model.clone(),
            p.stems_folder.clone(),
            p.stems_on_load,
            p.live_stems,
            p.demucs_python.clone(),
            p.keep_stems,
        ) != before
    }
}
