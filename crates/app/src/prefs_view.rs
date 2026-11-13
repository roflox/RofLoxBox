//! The Deck and Mixer pages of the Preferences window.

use eframe::egui::{self, RichText, Ui};

use crate::config::{CrossfaderCurveSetting, FaderCurveSetting, Preferences, Theme};
use crate::deck::TempoRange;
use crate::widgets::help;

/// Pages of the Preferences window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrefsTab {
    #[default]
    Deck,
    Mixer,
    Keyboard,
    Stems,
    Library,
    Appearance,
    Audio,
}

impl PrefsTab {
    pub const ALL: [PrefsTab; 7] = [
        PrefsTab::Deck,
        PrefsTab::Mixer,
        PrefsTab::Keyboard,
        PrefsTab::Stems,
        PrefsTab::Library,
        PrefsTab::Appearance,
        PrefsTab::Audio,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PrefsTab::Deck => "Deck",
            PrefsTab::Mixer => "Mixer",
            PrefsTab::Keyboard => "Keyboard",
            PrefsTab::Stems => "Stems",
            PrefsTab::Library => "Library",
            PrefsTab::Appearance => "Appearance",
            PrefsTab::Audio => "Audio",
        }
    }
}

/// A titled group; `help` (if any) sits behind a "?" after the title.
fn section(ui: &mut Ui, title: &str, help: &str, body: impl FnOnce(&mut Ui)) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong().size(15.0));
        if !help.is_empty() {
            crate::widgets::help(ui, help);
        }
    });
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

/// Dark, light or the system's, and the text size; returns whether
/// anything changed.
pub fn appearance_prefs(ui: &mut Ui, p: &mut Preferences) -> bool {
    let before = (p.theme, p.text_scale);
    section(ui, "Theme", "Waveforms stay dark.", |ui| {
        ui.radio_value(&mut p.theme, Theme::Dark, "Dark");
        ui.radio_value(&mut p.theme, Theme::Light, "Light");
        ui.radio_value(&mut p.theme, Theme::System, "System")
            .on_hover_text("Follows the system");
    });
    section(ui, "Text size", "Lists, menus and windows; not deck labels.", |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Slider::new(&mut p.text_scale, 50..=200).suffix(" %").step_by(5.0));
            if ui.add_enabled(p.text_scale != 100, egui::Button::new("Reset")).clicked() {
                p.text_scale = 100;
            }
        });
    });
    (p.theme, p.text_scale) != before
}

/// Deck settings; returns whether anything changed.
pub fn deck_prefs(ui: &mut Ui, p: &mut Preferences, decks: usize) -> bool {
    let before = p.clone();
    egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
        section(ui, "Eject/Load Lock", "Locked: no loading onto a playing deck.", |ui| {
            lock(ui, &mut p.load_lock);
        });
        section(ui, "Auto Gain", "Evens out loudness. Drag a deck's dB value to change it.", |ui| {
            ui.checkbox(&mut p.auto_gain, "Play tracks at their track gain");
        });
        section(ui, "Track End Warning", "Blinks the deck red this long before the end.", |ui| {
            ui.add(
                egui::Slider::new(&mut p.track_end_warning, 0..=60)
                    .suffix(" s")
                    .custom_formatter(|v, _| if v == 0.0 { "Off".into() } else { format!("{v:.0}") }),
            );
        });
        section(ui, "Needle Lock", "Locked: the small waveform of a playing deck doesn't jump.", |ui| {
            lock(ui, &mut p.needle_lock);
        });
        section(ui, "EDIT", "The view scrolls along while playing.", |ui| {
            ui.checkbox(&mut p.edit_follow_playhead, "Follow playhead");
        });
        section(ui, "Key display format", "On decks and in lists.", |ui| {
            use crate::deck::KeyNotation;
            ui.radio_value(&mut p.key_notation, KeyNotation::Alphanumeric, "Alphanumeric (4A, 8B)");
            ui.radio_value(&mut p.key_notation, KeyNotation::Classic, "Classic (Fm, C)");
            ui.horizontal(|ui| {
                ui.checkbox(&mut p.highlight_compatible_keys, "Highlight compatible keys in the track lists");
                help(ui, "Keys that mix with the master deck's turn green.");
            });
        });
        section(ui, "Waveform detail", "High is sharper when zoomed in. For newly loaded tracks.", |ui| {
            use crate::waveform::WaveformDetail;
            ui.radio_value(&mut p.waveform_detail, WaveformDetail::Standard, "Standard (rekordbox's)");
            ui.radio_value(&mut p.waveform_detail, WaveformDetail::High, "High (4× finer)");
        });
        section(ui, "Analysis", "For tracks rekordbox hasn't analysed.", |ui| {
            use crate::deck::AnalysisMethod;
            ui.radio_value(&mut p.analysis, AnalysisMethod::Rekordbox, "rekordbox-compatible");
            ui.radio_value(&mut p.analysis, AnalysisMethod::Deckhand, "RofLoxBox (quicker)");
            ui.horizontal(|ui| {
                ui.checkbox(&mut p.analyze_on_load, "Analyze BPM, beat grid and key on load");
                help(ui, "Only for tracks missing them; edited ones are kept.");
            });
        });
        section(ui, "Tempo ranges", "TEMPO RANGE steps through these. WIDE is ±100 %.", |ui| {
            tempo_ranges(ui, &mut p.tempo_ranges);
        });
        section(ui, "Beat Sync master", "The loudest playing deck takes over.", |ui| {
            ui.checkbox(&mut p.auto_master, "Pass MASTER on when the master deck leaves the mix");
        });
        section(ui, "Memory Cue Call Lock", "Locked: no memory cue calls on a playing deck.", |ui| {
            lock(ui, &mut p.memory_call_lock);
        });
        section(ui, "CALL ◀ ▶ during loop playback", "Loop ½ / ×2 can also be mapped in Controller.", |ui| {
            ui.radio_value(&mut p.call_resizes_loop, false, "Moves between memory cues and loops");
            ui.radio_value(&mut p.call_resizes_loop, true, "Halves / doubles the loop (Pioneer)");
        });
        section(ui, "Hot Cue Gate", "On: a hot cue on a playing deck also stops on release.", |ui| {
            ui.checkbox(&mut p.gate_while_playing, "Gate also while playing");
        });
        section(ui, "Load", "", |ui| {
            ui.checkbox(&mut p.start_at_first_cue, "Start at the first cue");
            ui.checkbox(&mut p.instant_doubles, "Instant Doubles")
                .on_hover_text("Loading a playing track picks up where it is");
        });
        section(ui, "Auto Cue", "On load, cue where the music starts.", |ui| {
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
        });
        section(ui, "Vinyl Speed Adjust", "0 s is instant.", |ui| {
            ui.add(egui::Slider::new(&mut p.vinyl_start, 0.0..=1.0).suffix(" s").text("Touch/Release (start up)"));
            ui.add(egui::Slider::new(&mut p.vinyl_brake, 0.0..=3.0).suffix(" s").text("Brake (stop)"));
        });
        section(ui, "Jog", "", |ui| {
            ui.add(egui::Slider::new(&mut p.scratch_sensitivity, 0.25..=3.0).text("Scratch sensitivity"))
                .on_hover_text("Jog top, VINYL on");
            ui.add(egui::Slider::new(&mut p.bend_sensitivity, 0.25..=4.0).text("Pitch bend sensitivity"))
                .on_hover_text("Jog side, or top with VINYL off");
        });
        ui.add_space(8.0);
        if ui.button("Reset to defaults").clicked() {
            let defaults = Preferences::default();
            p.load_lock = defaults.load_lock;
            p.needle_lock = defaults.needle_lock;
            p.auto_gain = defaults.auto_gain;
            p.track_end_warning = defaults.track_end_warning;
            p.memory_call_lock = defaults.memory_call_lock;
            p.key_notation = defaults.key_notation;
            p.highlight_compatible_keys = defaults.highlight_compatible_keys;
            p.edit_follow_playhead = defaults.edit_follow_playhead;
            p.analysis = defaults.analysis;
            p.waveform_detail = defaults.waveform_detail;
            p.auto_master = defaults.auto_master;
            p.tempo_ranges = defaults.tempo_ranges.clone();
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
    section(ui, "Crossfader Curve", "", |ui| {
        for curve in CrossfaderCurveSetting::ALL {
            ui.radio_value(&mut p.crossfader_curve, curve, curve.label());
        }
    });
    section(ui, "Channel Fader Curve", "", |ui| {
        for curve in FaderCurveSetting::ALL {
            ui.radio_value(&mut p.fader_curve, curve, curve.label());
        }
    });
    section(ui, "EQ Type", "Defaults as on the DJM-900NXS2.", |ui| {
        ui.radio_value(
            &mut p.isolator,
            true,
            "Isolator (+6 dB … −∞)",
        );
        ui.radio_value(&mut p.isolator, false, "EQ (+6 dB … −26 dB)");
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
    });
    section(ui, "Waveform", "", |ui| {
        ui.checkbox(&mut p.eq_waveforms, "Waveforms follow the EQ")
            .on_hover_text("Cut bands shrink in the waveforms");
        ui.checkbox(&mut p.scroll_zoom, "Scroll over a waveform to zoom").on_hover_text(
            "Wheel, scroll or pinch",
        );
    });
    section(ui, "Master Limiter", "", |ui| {
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
        p.scroll_zoom = defaults.scroll_zoom;
    }
    *p != before
}

/// The tempo ranges to step through: common ones to tick, others to add;
/// the last one left cannot be taken away.
fn tempo_ranges(ui: &mut Ui, ranges: &mut Vec<TempoRange>) {
    let last = ranges.len() == 1;
    let mut delete = None;
    egui::Grid::new("tempo ranges").num_columns(2).striped(true).spacing([24.0, 3.0]).show(
        ui,
        |ui| {
            for &range in ranges.iter() {
                ui.label(range.label());
                let button = ui.add_enabled(!last, egui::Button::new("Delete").small());
                if button.on_disabled_hover_text("At least one range stays").clicked() {
                    delete = Some(range);
                }
                ui.end_row();
            }
        },
    );
    if let Some(range) = delete {
        ranges.retain(|r| *r != range);
    }
    ui.horizontal(|ui| {
        let id = ui.id().with("new tempo range");
        let mut percent: u8 = ui.data(|d| d.get_temp(id)).unwrap_or(12);
        ui.label("Add ±");
        ui.add(egui::DragValue::new(&mut percent).range(1..=100).suffix(" %"));
        ui.data_mut(|d| d.insert_temp(id, percent));
        let range = TempoRange::percent(percent);
        if ui.add_enabled(!ranges.contains(&range), egui::Button::new("Add")).clicked() {
            ranges.push(range);
            ranges.sort();
        }
    });
}
