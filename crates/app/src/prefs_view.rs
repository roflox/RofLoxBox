//! The Deck, Mixer, Analysis, EDIT and View pages of the Preferences window.

use eframe::egui::{self, RichText, Ui};

use crate::config::{CrossfaderCurveSetting, FaderCurveSetting, Preferences, Theme};
use crate::deck::TempoRange;
use crate::widgets::help;

/// Pages of the Preferences window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrefsTab {
    #[default]
    Deck,
    Mixer,
    Analysis,
    Edit,
    Keyboard,
    Stems,
    Library,
    /// Theme, sizes, key display and the View menu.
    #[serde(alias = "appearance")]
    View,
    /// How the waveforms are drawn and look.
    Waveforms,
    Audio,
}

impl PrefsTab {
    pub const ALL: [PrefsTab; 10] = [
        PrefsTab::Deck,
        PrefsTab::Mixer,
        PrefsTab::Analysis,
        PrefsTab::Edit,
        PrefsTab::Keyboard,
        PrefsTab::Stems,
        PrefsTab::Library,
        PrefsTab::View,
        PrefsTab::Waveforms,
        PrefsTab::Audio,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PrefsTab::Deck => "Deck",
            PrefsTab::Mixer => "Mixer",
            PrefsTab::Analysis => "Analysis",
            PrefsTab::Edit => "EDIT",
            PrefsTab::Keyboard => "Keyboard",
            PrefsTab::Stems => "Stems",
            PrefsTab::Library => "Library",
            PrefsTab::View => "View",
            PrefsTab::Waveforms => "Waveforms",
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

/// How things look: theme, sizes, keys; returns whether anything changed.
pub fn view_prefs(ui: &mut Ui, p: &mut Preferences) -> bool {
    let before = p.clone();
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
    section(ui, "Button size", "The decks' buttons and pads: smaller fits more on screen.", |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Slider::new(&mut p.button_scale, 50..=150).suffix(" %").step_by(5.0));
            if ui.add_enabled(p.button_scale != 100, egui::Button::new("Reset")).clicked() {
                p.button_scale = 100;
            }
        });
    });
    section(ui, "Tempo display size", "The ring with the BPM by each deck.", |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Slider::new(&mut p.tempo_display_scale, 50..=200).suffix(" %").step_by(5.0));
            if ui.add_enabled(p.tempo_display_scale != 100, egui::Button::new("Reset")).clicked() {
                p.tempo_display_scale = 100;
            }
        });
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
    *p != before
}

/// How the waveforms are drawn and look; returns whether anything changed.
pub fn waveform_prefs(ui: &mut Ui, p: &mut Preferences, drawing: &mut crate::waveform::WaveformDrawing) -> bool {
    use crate::waveform::WaveformDrawing;
    let (before, drawing_before) = (p.clone(), *drawing);
    section(
        ui,
        "Drawing",
        "How the RGB waveforms are drawn; also picked above the waveforms, next to RGB and \
         3-Band. Simplest uses rekordbox's own detail, the others work tracks out four times \
         finer from the audio; loaded tracks follow a change at once.",
        |ui| {
            for choice in WaveformDrawing::ALL {
                let label = if choice == WaveformDrawing::default() {
                    format!("{} (default)", choice.label())
                } else {
                    choice.label().to_owned()
                };
                ui.radio_value(drawing, choice, label).on_hover_text(choice.description());
            }
        },
    );
    section(ui, "Waveforms", "", |ui| {
        ui.checkbox(&mut p.eq_waveforms, "Waveforms follow the EQ")
            .on_hover_text("Cut bands shrink in the waveforms");
        ui.checkbox(&mut p.scroll_zoom, "Scroll over a waveform to zoom").on_hover_text(
            "Wheel, scroll or pinch",
        );
    });
    section(ui, "Look", "Gradients of each drawing and colours.", |ui| {
        crate::waveform_look::look_prefs(ui, &mut p.waveform_look);
    });
    *p != before || *drawing != drawing_before
}

/// How tracks are analysed; returns whether anything changed.
pub fn analysis_prefs(ui: &mut Ui, p: &mut Preferences) -> bool {
    let before = p.clone();
    section(ui, "Method", "For tracks rekordbox hasn't analysed.", |ui| {
        use crate::deck::AnalysisMethod;
        ui.radio_value(&mut p.analysis, AnalysisMethod::Rekordbox, "rekordbox-compatible");
        ui.radio_value(&mut p.analysis, AnalysisMethod::RofLoxBox, "RofLoxBox (quicker)");
    });
    section(ui, "On load", "", |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut p.analyze_on_load, "Analyze BPM, beat grid and key on load");
            help(ui, "Only for tracks missing them; edited ones are kept.");
        });
    });
    ui.add_space(8.0);
    if ui.button("Reset to defaults").clicked() {
        let defaults = Preferences::default();
        p.analysis = defaults.analysis;
        p.analyze_on_load = defaults.analyze_on_load;
    }
    *p != before
}

/// The EDIT mode's settings; returns whether anything changed.
pub fn edit_prefs(ui: &mut Ui, p: &mut Preferences) -> bool {
    let before = p.clone();
    section(ui, "Playback", "", |ui| {
        ui.checkbox(&mut p.edit_follow_playhead, "Follow playhead")
            .on_hover_text("The view scrolls along while playing");
    });
    section(ui, "Automation", "", |ui| {
        ui.checkbox(&mut p.edit_eq_boost, "EQ can boost above 0 dB")
            .on_hover_text("Otherwise EQ points stop at 0 dB unless you hold Option/Alt");
    });
    if p.edit_delete.is_some() {
        section(ui, "Deleting audio", "What a deck's deleted audio takes along.", |ui| {
            if ui.button("Ask again before deleting audio").clicked() {
                p.edit_delete = None;
            }
        });
    }
    ui.add_space(8.0);
    if ui.button("Reset to defaults").clicked() {
        let defaults = Preferences::default();
        p.edit_follow_playhead = defaults.edit_follow_playhead;
        p.edit_eq_boost = defaults.edit_eq_boost;
        p.edit_delete = defaults.edit_delete;
    }
    *p != before
}

/// Deck settings; returns whether anything changed.
pub fn deck_prefs(ui: &mut Ui, p: &mut Preferences, decks: usize) -> bool {
    let before = p.clone();
    {
        section(ui, "Eject/Load Lock", "Locked: no loading onto a playing deck.", |ui| {
            lock(ui, &mut p.load_lock);
        });
        section(ui, "Load", "", |ui| {
            ui.checkbox(&mut p.start_at_first_cue, "Start at the first cue");
            ui.checkbox(&mut p.instant_doubles, "Instant Doubles")
                .on_hover_text("Loading a playing track picks up where it is");
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
        section(ui, "Tempo ranges", "TEMPO RANGE steps through these. WIDE is ±100 %.", |ui| {
            tempo_ranges(ui, &mut p.tempo_ranges);
        });
        section(ui, "Beat Sync master", "The loudest playing deck takes over.", |ui| {
            ui.checkbox(&mut p.auto_master, "Pass MASTER on when the master deck leaves the mix");
        });
        section(ui, "Memory Cue Call Lock", "Locked: no memory cue calls on a playing deck.", |ui| {
            lock(ui, &mut p.memory_call_lock);
        });
        section(
            ui,
            "Memory points",
            "The most memory cues and loops a track may have; storing more is refused. rekordbox \
             keeps 10 per track. More can lead to unexpected behaviour: rekordbox, its exports \
             and CDJs or other players may drop the extra ones, or not show them all.",
            |ui| {
                ui.add(
                    egui::Slider::new(&mut p.memory_points_limit, 0..=100)
                        .text("per track")
                        .custom_formatter(|v, _| {
                            if v == 0.0 { "No limit".into() } else { format!("{v:.0}") }
                        }),
                );
                let rekordbox = crate::deck::REKORDBOX_MEMORY_POINTS;
                if p.memory_points_limit != rekordbox
                    && ui.button(format!("Back to rekordbox's {rekordbox}")).clicked()
                {
                    p.memory_points_limit = rekordbox;
                }
            },
        );
        section(ui, "CALL ◀ ▶ during loop playback", "Loop ½ / ×2 can also be mapped in Controller.", |ui| {
            ui.radio_value(&mut p.call_resizes_loop, false, "Moves between memory cues and loops");
            ui.radio_value(&mut p.call_resizes_loop, true, "Halves / doubles the loop (Pioneer)");
        });
        section(ui, "Hot Cue Gate", "On: a hot cue on a playing deck also stops on release.", |ui| {
            ui.checkbox(&mut p.gate_while_playing, "Gate also while playing");
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
            p.auto_master = defaults.auto_master;
            p.tempo_ranges = defaults.tempo_ranges.clone();
            p.call_resizes_loop = defaults.call_resizes_loop;
            p.gate_while_playing = defaults.gate_while_playing;
            p.memory_points_limit = defaults.memory_points_limit;
            p.start_at_first_cue = defaults.start_at_first_cue;
            p.instant_doubles = defaults.instant_doubles;
            p.auto_cue = defaults.auto_cue;
            p.auto_cue_level_db = defaults.auto_cue_level_db;
            p.vinyl_start = defaults.vinyl_start;
            p.vinyl_brake = defaults.vinyl_brake;
            p.scratch_sensitivity = defaults.scratch_sensitivity;
            p.bend_sensitivity = defaults.bend_sensitivity;
        }
    }
    *p != before
}

/// Mixer settings; returns whether anything changed.
pub fn mixer_prefs(ui: &mut Ui, p: &mut Preferences) -> bool {
    let before = p.clone();
    section(ui, "Beat FX to step through", "A controller's next / previous FX buttons go through these.", |ui| {
        ui.horizontal_wrapped(|ui| {
            for kind in rofloxbox_audio::BeatFxKind::ALL {
                let mut on = p.beat_fx_cycle.contains(&kind);
                if ui.checkbox(&mut on, kind.label()).changed() {
                    p.beat_fx_cycle.retain(|k| *k != kind);
                    if on {
                        p.beat_fx_cycle.push(kind);
                    }
                }
            }
        });
        if p.beat_fx_cycle.is_empty() {
            ui.label(egui::RichText::new("None chosen: all of them.").small().weak());
        }
    });
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
    section(ui, "Knobs", "Scroll over a knob to turn it; hold Shift for fine steps.", |ui| {
        ui.add(
            egui::Slider::new(&mut p.knob_scroll, 0.0..=4.0)
                .text("Scroll sensitivity")
                .custom_formatter(|v, _| if v == 0.0 { "Off".into() } else { format!("{v:.2}×") }),
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
        p.knob_scroll = defaults.knob_scroll;
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
