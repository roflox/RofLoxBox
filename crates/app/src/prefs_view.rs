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
        section(ui, "Key display format", |ui| {
            use crate::deck::KeyNotation;
            ui.radio_value(&mut p.key_notation, KeyNotation::Alphanumeric, "Alphanumeric (4A, 8B)");
            ui.radio_value(&mut p.key_notation, KeyNotation::Classic, "Classic (Fm, C)");
            ui.label(RichText::new("On the decks and in the track lists, whatever the tags say.").small().weak());
        });
        section(ui, "Analysis", |ui| {
            use crate::deck::AnalysisMethod;
            ui.radio_value(&mut p.analysis, AnalysisMethod::Rekordbox, "rekordbox-compatible");
            ui.radio_value(&mut p.analysis, AnalysisMethod::Deckhand, "RofLoxBox (quicker)");
            ui.label(
                RichText::new(
                    "Waveforms for tracks rekordbox has not analysed, on the decks and on exported \
                     sticks. rekordbox-compatible ones are identical to rekordbox's.",
                )
                .small()
                .weak(),
            );
            ui.checkbox(&mut p.analyze_on_load, "Analyze BPM, beat grid and key on load");
            ui.label(
                RichText::new(
                    "A loaded track without a beat grid or key gets them detected in the \
                     background (RofLoxBox's own detection; rekordbox's grids and keys and \
                     ones you edited are kept). Collection tracks keep the result.",
                )
                .small()
                .weak(),
            );
        });
        section(ui, "Beat Sync master", |ui| {
            ui.checkbox(&mut p.auto_master, "Pass MASTER on when the master deck leaves the mix");
            ui.label(
                RichText::new(
                    "When the master is paused, ejected or faded out (channel fader or \
                     crossfader), the loudest other playing deck becomes the master; synced \
                     decks then follow it.",
                )
                .small()
                .weak(),
            );
        });
        section(ui, "Memory Cue Call Lock", |ui| {
            lock(ui, &mut p.memory_call_lock);
            ui.label(RichText::new("Locked: memory cues are not called on a playing deck.").small().weak());
        });
        section(ui, "CALL ◀ ▶ during loop playback", |ui| {
            ui.radio_value(&mut p.call_resizes_loop, false, "Moves between memory cues and loops");
            ui.radio_value(&mut p.call_resizes_loop, true, "Halves / doubles the loop (Pioneer)");
            ui.label(
                RichText::new("Loop ½ / ×2 can also get buttons of their own: map loop_resize_half / loop_resize_double in the Controller window.")
                    .small()
                    .weak(),
            );
        });
        section(ui, "Hot Cue Gate", |ui| {
            ui.checkbox(&mut p.gate_while_playing, "Gate also while playing");
            ui.label(
                RichText::new("With GATE on, a hot cue pressed on a paused deck plays while held, then returns and pauses. Off (like rekordbox): on a playing deck it just jumps and keeps playing. On: it stops on release there too.")
                    .small()
                    .weak(),
            );
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
            ui.add(egui::Slider::new(&mut p.scratch_sensitivity, 0.25..=3.0).text("Scratch sensitivity"))
                .on_hover_text("Touching the top with VINYL on");
            ui.add(egui::Slider::new(&mut p.bend_sensitivity, 0.25..=4.0).text("Pitch bend sensitivity"))
                .on_hover_text("Spinning the side (or the top with VINYL off): faster spin, bigger bend");
        });
        ui.add_space(8.0);
        if ui.button("Reset to defaults").clicked() {
            let defaults = Preferences::default();
            p.load_lock = defaults.load_lock;
            p.needle_lock = defaults.needle_lock;
            p.memory_call_lock = defaults.memory_call_lock;
            p.key_notation = defaults.key_notation;
            p.analysis = defaults.analysis;
            p.auto_master = defaults.auto_master;
            p.analyze_on_load = defaults.analyze_on_load;
            p.call_resizes_loop = defaults.call_resizes_loop;
            p.gate_while_playing = defaults.gate_while_playing;
            p.start_at_first_cue = defaults.start_at_first_cue;
            p.instant_doubles = defaults.instant_doubles;
            p.auto_cue = defaults.auto_cue;
            p.auto_cue_level_db = defaults.auto_cue_level_db;
            p.vinyl_start = defaults.vinyl_start;
            p.vinyl_brake = defaults.vinyl_brake;
            p.scratch_sensitivity = defaults.scratch_sensitivity;
            p.bend_sensitivity = defaults.bend_sensitivity;
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
    section(ui, "EQ Type", |ui| {
        ui.radio_value(
            &mut p.isolator,
            true,
            "Isolator: splits into 3 bands, fully left removes a band (+6 dB … −∞)",
        );
        ui.radio_value(&mut p.isolator, false, "EQ: shelf / peak filters (+6 dB … −26 dB)");
        let hz =
            |ui: &mut Ui, value: &mut f32, label: &str, range: std::ops::RangeInclusive<f32>| {
                ui.add(egui::Slider::new(value, range).logarithmic(true).suffix(" Hz").text(label));
            };
        if p.isolator {
            hz(ui, &mut p.iso_low_hz, "LOW / MID split", 80.0..=800.0);
            hz(ui, &mut p.iso_high_hz, "MID / HI split", 1_500.0..=10_000.0);
        } else {
            hz(ui, &mut p.eq_low_hz, "LOW", 30.0..=300.0);
            hz(ui, &mut p.eq_mid_hz, "MID", 300.0..=4_000.0);
            hz(ui, &mut p.eq_high_hz, "HI", 3_000.0..=16_000.0);
        }
        ui.label(
            RichText::new("Defaults: EQ 70 Hz / 1 kHz / 13 kHz like Pioneer's DJM-900NXS2; isolator split 250 Hz / 4 kHz.")
                .small()
                .weak(),
        );
    });
    section(ui, "Waveform", |ui| {
        ui.checkbox(&mut p.eq_waveforms, "Waveforms follow the EQ")
            .on_hover_text("Bands turned down with the EQ knobs shrink in the waveforms");
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
        p.eq_low_hz = defaults.eq_low_hz;
        p.eq_mid_hz = defaults.eq_mid_hz;
        p.eq_high_hz = defaults.eq_high_hz;
        p.iso_low_hz = defaults.iso_low_hz;
        p.iso_high_hz = defaults.iso_high_hz;
        p.limiter = defaults.limiter;
        p.eq_waveforms = defaults.eq_waveforms;
    }
    *p != before
}
