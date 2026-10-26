//! The Deck and Mixer pages of the Preferences window.

use eframe::egui::{self, RichText, Ui};

use crate::config::{CrossfaderCurveSetting, FaderCurveSetting, Preferences};

/// Pages of the Preferences window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PrefsTab {
    #[default]
    Deck,
    Mixer,
    Audio,
}

impl PrefsTab {
    pub const ALL: [PrefsTab; 3] = [PrefsTab::Deck, PrefsTab::Mixer, PrefsTab::Audio];

    pub fn label(self) -> &'static str {
        match self {
            PrefsTab::Deck => "Deck",
            PrefsTab::Mixer => "Mixer",
            PrefsTab::Audio => "Audio",
        }
    }
}

fn section(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui)) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong().size(15.0));
    ui.indent(title, body);
}

/// Lock / Unlock radio buttons; returns whether it changed.
fn lock(ui: &mut Ui, locked: &mut bool) -> bool {
    let before = *locked;
    ui.horizontal(|ui| {
        ui.radio_value(locked, true, "Lock");
        ui.radio_value(locked, false, "Unlock");
    });
    *locked != before
}

/// Deck settings; returns whether anything changed.
pub fn deck_prefs(ui: &mut Ui, p: &mut Preferences, decks: usize) -> bool {
    let before = p.clone();
    egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
        section(ui, "Eject/Load Lock", |ui| {
            lock(ui, &mut p.load_lock);
            ui.label(RichText::new("Locked: no loading onto a playing deck.").small().weak());
        });
        section(ui, "Needle Lock", |ui| {
            lock(ui, &mut p.needle_lock);
            ui.label(RichText::new("Locked: clicking the overview of a playing deck does not jump.").small().weak());
        });
        section(ui, "Memory Cue Call Lock", |ui| {
            lock(ui, &mut p.memory_call_lock);
            ui.label(RichText::new("Locked: memory cues are not called on a playing deck (loops still halve/double).").small().weak());
        });
        section(ui, "Load", |ui| {
            ui.checkbox(&mut p.start_at_first_cue, "Playback starts at Memory/Hot Cue nearest to the beginning of the track");
            ui.checkbox(&mut p.instant_doubles, "Instant Doubles")
                .on_hover_text("Loading a track that plays on another deck continues from the same position, at the same tempo");
        });
        section(ui, "Auto Cue", |ui| {
            ui.horizontal(|ui| {
                ui.label("Auto Cue Level");
                egui::ComboBox::from_id_salt("auto cue level")
                    .selected_text(format!("{:.0} dB", p.auto_cue_level_db))
                    .show_ui(ui, |ui| {
                        for db in [-36.0, -42.0, -48.0, -54.0, -60.0, -66.0, -72.0, -78.0] {
                            ui.selectable_value(&mut p.auto_cue_level_db, db, format!("{db:.0} dB"));
                        }
                    });
            });
            ui.horizontal_wrapped(|ui| {
                for deck in 0..decks {
                    ui.checkbox(&mut p.auto_cue[deck], format!("Deck {}", deck + 1));
                }
            });
            ui.label(RichText::new("On load, the cue point goes to where the music starts (louder than the level).").small().weak());
        });
        section(ui, "Vinyl Speed Adjust", |ui| {
            ui.add(egui::Slider::new(&mut p.vinyl_start, 0.0..=1.0).suffix(" s").text("Touch/Release (start up)"));
            ui.add(egui::Slider::new(&mut p.vinyl_brake, 0.0..=3.0).suffix(" s").text("Brake (stop)"));
            ui.label(RichText::new("0 s starts and stops instantly; longer feels like a turntable motor.").small().weak());
        });
        section(ui, "Jog", |ui| {
            ui.add(egui::Slider::new(&mut p.scratch_sensitivity, 0.25..=3.0).text("Scratch sensitivity"));
        });
        ui.add_space(8.0);
        if ui.button("Reset to defaults").clicked() {
            let defaults = Preferences::default();
            p.load_lock = defaults.load_lock;
            p.needle_lock = defaults.needle_lock;
            p.memory_call_lock = defaults.memory_call_lock;
            p.start_at_first_cue = defaults.start_at_first_cue;
            p.instant_doubles = defaults.instant_doubles;
            p.auto_cue = defaults.auto_cue;
            p.auto_cue_level_db = defaults.auto_cue_level_db;
            p.vinyl_start = defaults.vinyl_start;
            p.vinyl_brake = defaults.vinyl_brake;
            p.scratch_sensitivity = defaults.scratch_sensitivity;
        }
    });
    *p != before
}

/// Mixer settings; returns whether anything changed.
pub fn mixer_prefs(ui: &mut Ui, p: &mut Preferences) -> bool {
    let before = p.clone();
    section(ui, "Crossfader Curve", |ui| {
        for curve in CrossfaderCurveSetting::ALL {
            ui.radio_value(&mut p.crossfader_curve, curve, curve.label());
        }
    });
    section(ui, "Channel Fader Curve", |ui| {
        for curve in FaderCurveSetting::ALL {
            ui.radio_value(&mut p.fader_curve, curve, curve.label());
        }
    });
    section(ui, "EQ", |ui| {
        ui.radio_value(&mut p.isolator, true, "Isolator (knob fully left cuts the band)");
        ui.radio_value(&mut p.isolator, false, "EQ (down to −26 dB)");
    });
    section(ui, "Master Limiter", |ui| {
        ui.checkbox(&mut p.limiter, "Soften peaks before they clip");
    });
    ui.add_space(8.0);
    if ui.button("Reset to defaults").clicked() {
        let defaults = Preferences::default();
        p.crossfader_curve = defaults.crossfader_curve;
        p.fader_curve = defaults.fader_curve;
        p.isolator = defaults.isolator;
        p.limiter = defaults.limiter;
    }
    *p != before
}
