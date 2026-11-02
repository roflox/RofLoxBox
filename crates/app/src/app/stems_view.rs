//! Making tracks from stems (Make → acapella, instrumental, …) and the Stems
//! page of Preferences.

use std::path::PathBuf;

use deckhand_library::Derived;
use eframe::egui::{self, RichText, Ui};

use super::{DeckhandApp, WARN};
use crate::stems::{self, Event, Job, Recipe, StemModel};

impl DeckhandApp {
    /// The chosen model, else the first one there is.
    pub(super) fn stem_model(&self) -> Option<&StemModel> {
        let models = &self.stem_models.0;
        models.iter().find(|m| m.id == self.config.prefs.stem_model).or(models.first())
    }

    fn stems_folder(&self) -> PathBuf {
        self.config.prefs.stems_folder.clone().unwrap_or_else(stems::default_output_dir)
    }

    /// Queues the tracks for separation and making `recipe` of them.
    pub(super) fn make_stems(&mut self, ids: &[u32], recipe: &Recipe) {
        let Some(model) = self.stem_model().cloned() else { return };
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

    /// Takes finished separations into the collection and shows progress.
    pub(super) fn poll_stems(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.stems.events.try_recv() {
            match event {
                Event::Progress { title, percent } => {
                    let done = percent.map(|p| format!(": {p}%")).unwrap_or_default();
                    let waiting = self.stems.pending().saturating_sub(1);
                    let more = if waiting > 0 {
                        format!(" ({waiting} more waiting)")
                    } else {
                        String::new()
                    };
                    self.status = Some(format!("Separating {title}{done}{more}"));
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
                Event::Failed { title, error } => {
                    self.collection_view.error =
                        Some(format!("Could not separate {title}: {error}"));
                }
            }
        }
        if self.stems.pending() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    /// The Stems page of Preferences; returns whether a setting changed.
    pub(super) fn stems_prefs(&mut self, ui: &mut Ui) -> bool {
        let before = (self.config.prefs.stem_model.clone(), self.config.prefs.stems_folder.clone());
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
        (self.config.prefs.stem_model.clone(), self.config.prefs.stems_folder.clone()) != before
    }
}
