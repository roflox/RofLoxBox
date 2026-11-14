//! A deck's controls, drawn element by element in the user's layout. In
//! layout-editing mode the elements can be dragged to other rows, into the
//! side column, or onto "Hide".

use deckhand_audio::Engine;
use deckhand_control::{DeckAction, PadMode, Value};
use eframe::egui::{self, Color32, RichText, Sense, Slider, Stroke, Ui, UiBuilder, vec2};

use super::{
    ACTIVE, CUE_COLOR, DECK_COLORS, DeckhandApp, PLAY_COLOR, WARN, beats_label, hot_cue_marks,
    overview_time, pad_face, pad_mode_short, pad_parameter_caption, toggle, toggle_in,
};
use crate::widgets::{RoundIcon, round_button};

/// Accent colours of lit buttons.
const RED: Color32 = Color32::from_rgb(230, 60, 50);
use crate::deck::{Deck, TempoRange, display_key, format_key};
use crate::effects::{Effects, FX_BEATS, PadFxSlot, beat_label};
use crate::layout::{DeckElement, DeckLayout, Place, Section};
use crate::library::format_time;
use crate::waveform::{Markers, draw_overview};

/// What the controls asked for; applied once the deck is drawn.
#[derive(Default)]
struct DeckOut {
    play: bool,
    eject: bool,
    toggle_sync: bool,
    make_master_pressed: bool,
    key_sync: bool,
    pad_events: Vec<(DeckAction, bool)>,
    /// The track's title started being dragged.
    drag_track: bool,
    /// A Pad FX pad set to something else: (slot 0–15, what it does now).
    pad_fx_edits: Vec<(usize, PadFxSlot)>,
    status: Option<String>,
    /// Separate the track into stems now.
    separate: bool,
}

/// The parts of the app one deck's controls work on.
struct Parts<'a> {
    deck: &'a mut Deck,
    engine: &'a mut Engine,
    effects: &'a Effects,
    cue_held: &'a mut bool,
    pad_held: &'a mut [Option<DeckAction>; 8],
    index: usize,
    compact: bool,
    is_master: bool,
    needle_lock: bool,
    /// Track End Warning, seconds (0 = off).
    end_warning: f64,
    /// Eject/Load Lock: a playing deck keeps its track.
    load_lock: bool,
    tempo_ranges: Vec<TempoRange>,
    /// A stem model is set up.
    stem_model: bool,
    /// Stems on the decks are on in Preferences.
    live_stems: bool,
    /// The pads are in the layout: GRID edits the grid in their place.
    pads_shown: bool,
    /// The CUE and Play buttons' size when they fill a column (the side
    /// column's width); their usual size otherwise.
    round_size: Option<f32>,
    out: DeckOut,
}

/// A layout change from a drop: the element and where it goes (`None` hides it).
type Move = Option<(Section, Option<Place>)>;

/// The space between the main area and the side column.
const SIDE_SPACE: f32 = 10.0;
/// The side column's width (tempo, key).
const SIDE_WIDTH: f32 = 128.0;
/// The space between controls in a section.
const GAP: f32 = 4.0;
/// The space between sections.
const SECTION_GAP: f32 = 8.0;
/// A control's height (a button row).
const ROW_HEIGHT: f32 = 28.0;
/// The width of a small button ("16", "◀", "×2"); wider ones are a
/// multiple of it, so the same control is the same size everywhere.
const UNIT: f32 = 34.0;

impl DeckhandApp {
    /// One deck's controls; `mirror` (a deck in the right column) puts the
    /// side column on its left.
    pub(super) fn deck_ui(&mut self, ui: &mut Ui, index: usize, compact: bool, mirror: bool) {
        if self.engine.is_none() {
            ui.label("No audio output");
            return;
        }
        let four = self.config.four_decks;
        let layout = self.config.deck_layout(four).clone();
        let editing = self.editing_layout;
        let has_side = editing || layout.side.iter().any(|s| layout.section_shown(*s));
        let main_width = if has_side {
            (ui.available_width() - SIDE_WIDTH - SIDE_SPACE).max(220.0)
        } else {
            ui.available_width()
        };
        let mut moved: Move = None;
        let stem_model = self.stem_model().is_some();
        let Some(engine) = self.engine.as_mut() else { return };
        let mut parts = Parts {
            deck: &mut self.decks[index],
            engine,
            effects: &self.effects,
            cue_held: &mut self.cue_held[index],
            pad_held: &mut self.pad_held[index],
            index,
            compact,
            is_master: self.master == Some(index),
            needle_lock: self.config.prefs.needle_lock,
            end_warning: f64::from(self.config.prefs.track_end_warning),
            load_lock: self.config.prefs.load_lock,
            tempo_ranges: self.config.prefs.tempo_ranges.clone(),
            stem_model,
            live_stems: self.config.prefs.live_stems,
            pads_shown: layout.is_shown(DeckElement::Pads),
            round_size: None,
            out: DeckOut::default(),
        };
        let area = ui.horizontal_top(|ui| {
            // The side column outside: on the right of a left deck, on the
            // left of a right one, so the two decks mirror each other.
            for main in if mirror { [false, true] } else { [true, false] } {
                if main {
                    ui.allocate_ui_with_layout(
                        vec2(main_width, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(main_width);
                            ui.spacing_mut().item_spacing.y = SECTION_GAP;
                            for (r, row) in layout.main.iter().enumerate() {
                                let row: Vec<Section> =
                                    row.iter().copied().filter(|s| editing || layout.section_shown(*s)).collect();
                                if row.is_empty() {
                                    continue;
                                }
                                // Sections side by side; one that no longer fits
                                // goes to the next line whole.
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = vec2(SECTION_GAP, SECTION_GAP);
                                    for &section in &row {
                                        keep_together(ui, (index, section), |ui| {
                                            place_section(ui, section, &layout, &mut parts, editing, &mut moved);
                                        });
                                    }
                                    if editing {
                                        drop_zone(ui, "+", 22.0, Some(Place::RowEnd(r)), &mut moved);
                                    }
                                });
                            }
                            if editing {
                                ui.horizontal(|ui| {
                                    drop_zone(ui, "New row", 120.0, Some(Place::NewRow), &mut moved);
                                    drop_zone(ui, "Hide", 80.0, None, &mut moved);
                                });
                            }
                        },
                    );
                } else if has_side {
                    if !mirror {
                        ui.add_space(SIDE_SPACE);
                    }
                    ui.allocate_ui_with_layout(
                        vec2(SIDE_WIDTH, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(SIDE_WIDTH);
                            ui.spacing_mut().item_spacing.y = SECTION_GAP;
                            // The column by the mixer is drawn mirrored on a
                            // right deck: everything in it left for right.
                            set_mirrored(ui, mirror);
                            set_in_side(ui, true);
                            for &section in layout.side.iter().filter(|s| editing || layout.section_shown(**s)) {
                                if mirror {
                                    // Snapped to the far side, as the left deck's
                                    // are to theirs (mirrored).
                                    against_far_side(ui, (index, "side", section), |ui| {
                                        place_section(ui, section, &layout, &mut parts, editing, &mut moved);
                                    });
                                } else {
                                    place_section(ui, section, &layout, &mut parts, editing, &mut moved);
                                }
                            }
                            set_mirrored(ui, false);
                            set_in_side(ui, false);
                            if editing {
                                drop_zone(ui, "+", SIDE_WIDTH - 4.0, Some(Place::SideEnd), &mut moved);
                            }
                        },
                    );
                    if mirror {
                        ui.add_space(SIDE_SPACE);
                    }
                }
            }
        });
        let area = area.response.rect;

        let out = parts.out;
        let ctx = ui.ctx().clone();
        // Tracks dragged from a list or another deck load onto the deck they are dropped on.
        self.deck_drop_target(ui, area, ui.id().with(("track drop", index)), index);
        if out.drag_track {
            self.start_deck_drag(&ctx, index);
        }
        for (slot, pad) in out.pad_fx_edits {
            self.set_pad_fx(slot, pad);
        }
        for (action, pressed) in out.pad_events {
            self.apply_deck_event(index, action, Value::Pressed(pressed), &ctx);
        }
        if out.eject {
            self.eject_deck(index);
        }
        if out.play {
            self.play_pause(index);
        }
        if out.toggle_sync {
            self.toggle_sync(index);
        }
        if out.make_master_pressed {
            self.make_master(index);
        }
        if out.key_sync {
            self.key_sync(index);
        }
        if out.status.is_some() {
            self.status = out.status;
        }
        if out.separate {
            self.separate_deck(index);
        }
        if let Some((section, place)) = moved {
            let layout = self.config.deck_layout_mut(four);
            match place {
                Some(place) => layout.move_to(section, place),
                None => layout.set_section_shown(section, false),
            }
            self.config.save();
        }
    }
}

/// A tempo step of − and + by the BPM: 0.02 % of the original tempo.
const TEMPO_NUDGE_STEP: f64 = 0.0002;

/// rekordbox's jog display: a ring (its mark turns with the music, once
/// per 1.8 s as a record does) round the BPM in big, the tempo in % and the
/// range, and − + for tiny tempo steps.
#[allow(clippy::too_many_arguments)]
fn tempo_display(
    ui: &mut Ui,
    deck: &mut Deck,
    engine: &mut Engine,
    playing: bool,
    position: f64,
    index: usize,
    ranges: &[TempoRange],
    status: &mut Option<String>,
) {
    let size = ui.available_width().min(140.0);
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    // Double-click: type the BPM.
    let edit_id = egui::Id::new(("tempo edit", index));
    let mut editing: Option<String> = ui.data(|d| d.get_temp(edit_id));
    if response.double_clicked() && deck.track.is_some() {
        editing = Some(deck.current_bpm().map(|b| format!("{b:.2}")).unwrap_or_default());
    }
    let response = response.on_hover_text("Double-click to type the BPM");
    let painter = ui.painter_at(rect.expand(2.0));
    let center = rect.center();
    let radius = size / 2.0 - 2.0;
    let color = DECK_COLORS[index % DECK_COLORS.len()];
    painter.circle_stroke(center, radius, Stroke::new(3.0_f32, Color32::from_gray(70)));
    painter.circle_stroke(center, radius - 5.0, Stroke::new(2.0_f32, color));
    // The mark, turning as a record at 33⅓ does (one turn in 1.8 s).
    let turn = (position / 1.8).fract() as f32 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
    let mark = center + vec2(turn.cos(), turn.sin()) * (radius - 1.0);
    painter.circle_filled(mark, 4.0, if playing { Color32::WHITE } else { Color32::from_gray(150) });
    let weak = ui.visuals().weak_text_color();
    let strong = ui.visuals().strong_text_color();
    painter.text(center - vec2(0.0, size * 0.25), egui::Align2::CENTER_CENTER, "BPM", egui::FontId::proportional(10.0), weak);
    match deck.current_bpm() {
        _ if editing.is_some() => {}
        Some(bpm) => {
            let whole = format!("{}", bpm.trunc() as i64);
            let decimals = format!(".{:02}", ((bpm.fract() * 100.0).round() as i64).min(99));
            let big = ui.painter().layout_no_wrap(whole, egui::FontId::proportional(size * 0.2), strong);
            let small = ui.painter().layout_no_wrap(decimals, egui::FontId::proportional(size * 0.11), strong);
            let width = big.size().x + small.size().x;
            let left = center.x - width / 2.0;
            let baseline = center.y - size * 0.02;
            painter.galley(egui::pos2(left, baseline - big.size().y / 2.0), big.clone(), strong);
            painter.galley(
                egui::pos2(left + big.size().x, baseline + big.size().y / 2.0 - small.size().y - 2.0),
                small,
                strong,
            );
        }
        None => {
            painter.text(center, egui::Align2::CENTER_CENTER, "—", egui::FontId::proportional(size * 0.18), weak);
        }
    }
    let percent = (deck.tempo_ratio() - 1.0) * 100.0;
    let line = center.y + size * 0.15;
    // The tempo left of the range (right of it when mirrored).
    let (first, second) = if mirrored(ui) {
        (egui::Align2::LEFT_CENTER, egui::Align2::RIGHT_CENTER)
    } else {
        (egui::Align2::RIGHT_CENTER, egui::Align2::LEFT_CENTER)
    };
    let away = if mirrored(ui) { 4.0 } else { -4.0 };
    painter.text(
        egui::pos2(center.x + away, line),
        first,
        format!("{percent:+.2}%"),
        egui::FontId::proportional(11.0),
        ui.visuals().text_color(),
    );
    painter.text(
        egui::pos2(center.x - away, line),
        second,
        deck.tempo_range.label(),
        egui::FontId::proportional(11.0),
        Color32::from_rgb(110, 200, 110),
    );
    // − + under them, inside the ring.
    let button = vec2(size * 0.18, size * 0.14);
    let y = center.y + size * 0.31;
    let step = (TEMPO_NUDGE_STEP / deck.tempo_range.fraction()) as f32;
    let hover = if deck.tempo_fader_active() { "Tempo −0.02 %" } else { "Follows the master (SYNC)" };
    // + on the left when mirrored.
    let side = if mirrored(ui) { -1.0 } else { 1.0 };
    let minus = egui::Rect::from_center_size(egui::pos2(center.x - side * button.x * 0.65, y), button);
    if ui.put(minus, egui::Button::new("−")).on_hover_text(hover).clicked() {
        deck.set_tempo_fader(deck.tempo_fader - step, engine);
    }
    let hover = if deck.tempo_fader_active() { "Tempo +0.02 %" } else { "Follows the master (SYNC)" };
    let plus = egui::Rect::from_center_size(egui::pos2(center.x + side * button.x * 0.65, y), button);
    if ui.put(plus, egui::Button::new("+")).on_hover_text(hover).clicked() {
        deck.set_tempo_fader(deck.tempo_fader + step, engine);
    }
    // The BPM being typed, where it shows: Enter sets it, Esc (or clicking
    // away) cancels.
    if let Some(mut text) = editing {
        let field = egui::Rect::from_center_size(center - vec2(0.0, size * 0.02), vec2(size * 0.62, size * 0.2));
        let edit = ui.put(
            field,
            egui::TextEdit::singleline(&mut text)
                .font(egui::FontId::proportional(size * 0.14))
                .horizontal_align(egui::Align::Center),
        );
        if !edit.has_focus() && !edit.lost_focus() {
            edit.request_focus();
        }
        let (enter, escape) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
        if edit.lost_focus() || escape {
            if enter && !escape {
                match text.trim().replace(',', ".").parse::<f64>() {
                    Ok(bpm) => {
                        if let Err(why) = deck.set_bpm(bpm, ranges, engine) {
                            *status = Some(format!("Deck {}: {why}", index + 1));
                        }
                    }
                    Err(_) => *status = Some(format!("Deck {}: \"{}\" is not a BPM", index + 1, text.trim())),
                }
            }
            ui.data_mut(|d| d.remove::<String>(edit_id));
        } else {
            ui.data_mut(|d| d.insert_temp(edit_id, text));
        }
    }
    let _ = response;
}

/// Inside a section's panel, around its controls.
const PANEL_MARGIN: f32 = 8.0;

/// A section of a deck's controls: a panel with its name, controls a row
/// tall and spaced alike.
fn panel(ui: &mut Ui, title: &str, contents: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(Stroke::new(1.0_f32, Color32::from_gray(48)))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(PANEL_MARGIN as i8, 6))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(GAP, GAP);
            ui.spacing_mut().interact_size.y = ROW_HEIGHT;
            // Its name above its controls, whatever the row around it.
            ui.vertical(|ui| {
                if !title.is_empty() {
                    ui.label(RichText::new(title).small().weak());
                }
                contents(ui);
            });
        });
}

/// Draws `contents` (a section) whole: when it no longer fits on the line
/// (as wide as it was last frame, or measured unseen the first time), it
/// starts the next one, never half on each.
fn keep_together(ui: &mut Ui, key: impl std::hash::Hash, mut contents: impl FnMut(&mut Ui)) {
    let id = egui::Id::new(("keep together", key));
    let width = ui.data(|d| d.get_temp::<f32>(id)).unwrap_or_else(|| {
        let builder = UiBuilder::new()
            .id_salt(("measure", id))
            .max_rect(ui.max_rect())
            .layout(egui::Layout::left_to_right(egui::Align::Min))
            .sizing_pass()
            .invisible();
        let mut measure = ui.new_child(builder);
        contents(&mut measure);
        measure.min_rect().width()
    });
    let at_line_start = ui.cursor().min.x <= ui.max_rect().left() + 1.0;
    if !at_line_start && ui.max_rect().right() - ui.cursor().min.x < width {
        ui.end_row();
    }
    let rect = ui.scope(|ui| contents(ui)).response.rect;
    ui.data_mut(|d| d.insert_temp(id, rect.width()));
}

/// Whether the controls being drawn are in a right deck's column by the
/// mixer, drawn mirrored (left for right).
fn mirrored(ui: &Ui) -> bool {
    ui.data(|d| d.get_temp::<bool>(egui::Id::new("deck mirrored"))).unwrap_or(false)
}

fn set_mirrored(ui: &Ui, on: bool) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("deck mirrored"), on));
}

/// Whether the controls being drawn are in the side column.
fn in_side(ui: &Ui) -> bool {
    ui.data(|d| d.get_temp::<bool>(egui::Id::new("deck side"))).unwrap_or(false)
}

fn set_in_side(ui: &Ui, on: bool) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("deck side"), on));
}

/// Draws `contents` against the right edge: as much space before it as it
/// leaves (its width as last frame).
fn against_far_side(ui: &mut Ui, key: impl std::hash::Hash, contents: impl FnOnce(&mut Ui)) {
    let id = egui::Id::new(("far side", key));
    let width: Option<f32> = ui.data(|d| d.get_temp(id));
    ui.horizontal(|ui| {
        if let Some(width) = width {
            ui.add_space((ui.available_width() - width).max(0.0));
        }
        let rect = ui.scope(contents).response.rect;
        ui.data_mut(|d| d.insert_temp(id, rect.width()));
    });
}

/// Parts of a control side by side, each `weight` units wide and a row
/// tall, filled by `part` (buttons fill their part).
fn split(ui: &mut Ui, weights: &[f32], mut part: impl FnMut(&mut Ui, usize)) {
    let mut order: Vec<usize> = (0..weights.len()).collect();
    if mirrored(ui) {
        order.reverse();
    }
    ui.horizontal(|ui| {
        for i in order {
            let weight = &weights[i];
            let size = vec2(UNIT * weight + GAP * (weight - 1.0).max(0.0), ROW_HEIGHT);
            let layout = egui::Layout::centered_and_justified(egui::Direction::LeftToRight);
            ui.allocate_ui_with_layout(size, layout, |ui| part(ui, i));
        }
    });
}

/// Parts one under another, each a row tall and as wide as there is room.
fn stack(ui: &mut Ui, rows: usize, mut part: impl FnMut(&mut Ui, usize)) {
    ui.vertical(|ui| {
        for i in 0..rows {
            let size = vec2(ui.available_width(), ROW_HEIGHT);
            let layout = egui::Layout::centered_and_justified(egui::Direction::LeftToRight);
            ui.allocate_ui_with_layout(size, layout, |ui| part(ui, i));
        }
    });
}

/// Draws a section's shown controls, line by line: the track's as they
/// are, the others in a panel (narrow ones as a column of wide buttons).
fn draw_section(ui: &mut Ui, section: Section, layout: &DeckLayout, p: &mut Parts<'_>) {
    let lines: Vec<Vec<DeckElement>> = section
        .lines()
        .iter()
        .map(|line| line.iter().copied().filter(|e| layout.is_shown(*e)).collect::<Vec<_>>())
        .filter(|line| !line.is_empty())
        .collect();
    if section == Section::Track {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            for line in &lines {
                ui.horizontal_wrapped(|ui| {
                    for &element in line {
                        draw_element(ui, element, p);
                    }
                });
            }
        });
        return;
    }
    // In the side column, CUE and Play fill it, as wide as its other panels.
    p.round_size = None;
    let fill_column = section == Section::Play && in_side(ui);
    if fill_column {
        let inner = SIDE_WIDTH - 2.0 * PANEL_MARGIN;
        let across = if layout.is_stacked(section) { 1.0 } else { 2.0 };
        p.round_size = Some((inner - GAP * (across - 1.0)) / across);
    }
    panel(ui, section.title(), |ui| {
        if fill_column {
            ui.set_width(SIDE_WIDTH - 2.0 * PANEL_MARGIN);
        }
        if layout.is_stacked(section) {
            // One on the other: two lines tall.
            ui.vertical(|ui| {
                for &element in lines.iter().flatten() {
                    draw_element(ui, element, p);
                }
            });
        } else if section.narrow() {
            ui.set_width(SIDE_WIDTH - 2.0 * PANEL_MARGIN);
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Center), |ui| {
                for &element in lines.iter().flatten() {
                    draw_element(ui, element, p);
                }
            });
        } else {
            // CUE then Play on every deck, mirrored or not.
            let flip = mirrored(ui) && section != Section::Play;
            for line in &lines {
                ui.horizontal(|ui| {
                    let mut line = line.clone();
                    if flip {
                        line.reverse();
                    }
                    for element in line {
                        draw_element(ui, element, p);
                    }
                });
            }
        }
    });
}

/// Draws a section; while editing, as a box to drag (whole) onto another
/// section (in front of it, or behind it on its right half) or a drop zone.
fn place_section(
    ui: &mut Ui,
    section: Section,
    layout: &DeckLayout,
    p: &mut Parts<'_>,
    editing: bool,
    moved: &mut Move,
) {
    if !editing {
        draw_section(ui, section, layout, p);
        return;
    }
    let id = egui::Id::new(("deck section", p.index, section));
    let response = ui
        .dnd_drag_source(id, section, |ui| {
            egui::Frame::new()
                .stroke(Stroke::new(1.0_f32, Color32::from_gray(110)))
                .corner_radius(6)
                .inner_margin(2.0)
                .show(ui, |ui| {
                    ui.add_enabled_ui(false, |ui| draw_section(ui, section, layout, p));
                });
        })
        .response
        .on_hover_text(format!("{} — drag to move", section.label()));
    // While dragging, nothing counts as hovered; use the pointer itself.
    let pointer = ui.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));
    let after = pointer.is_some_and(|pos| pos.x > response.rect.center().x);
    if response.dnd_hover_payload::<Section>().is_some() {
        // Show where it would land.
        let x = if after { response.rect.right() } else { response.rect.left() };
        ui.painter().vline(x, response.rect.y_range(), Stroke::new(3.0_f32, ACTIVE));
    }
    if let Some(dragged) = response.dnd_release_payload::<Section>() {
        *moved = Some((*dragged, Some(Place::Next { to: section, after })));
    }
}

/// A target for dropping sections: the end of a row, a new row, the side
/// column, or `None` to hide them.
fn drop_zone(ui: &mut Ui, text: &str, width: f32, place: Option<Place>, moved: &mut Move) {
    let (rect, response) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
    let hovered = response.dnd_hover_payload::<Section>().is_some();
    let color = if hovered { ACTIVE } else { Color32::from_gray(70) };
    ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0_f32, color), egui::StrokeKind::Inside);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(11.0),
        color,
    );
    if let Some(dragged) = response.dnd_release_payload::<Section>() {
        *moved = Some((*dragged, place));
    }
}

/// Track End Warning for a deck `remaining` seconds from the end: `None`
/// when off or not near the end, else whether the blink is lit now. Once a
/// second, twice a second in the last 10.
fn end_warning(ui: &Ui, warning: f64, remaining: f64, playing: bool) -> Option<bool> {
    if warning <= 0.0 || !playing || remaining > warning || remaining <= 0.0 {
        return None;
    }
    let rate = if remaining <= 10.0 { 2.0 } else { 1.0 };
    Some((ui.input(|i| i.time) * rate).fract() < 0.5)
}

fn draw_element(ui: &mut Ui, element: DeckElement, p: &mut Parts<'_>) {
    use DeckElement as E;
    let compact = p.compact;
    let index = p.index;
    let deck = &mut *p.deck;
    let engine = &mut *p.engine;
    let playing = engine.is_playing(index);
    let position = engine.position_secs(index);
    let big = if compact { vec2(60.0, 32.0) } else { vec2(76.0, 44.0) };
    match element {
        E::Header => {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("DECK {}", index + 1))
                            .strong()
                            .color(DECK_COLORS[index]),
                    );
                    if deck.sync {
                        ui.label(RichText::new("SYNC").small().strong().color(ACTIVE));
                    }
                    if p.is_master {
                        ui.label(RichText::new("MASTER").small().strong().color(CUE_COLOR));
                    }
                    if let Some(title) = deck.loading_title() {
                        ui.spinner();
                        ui.label(format!("Loading {title}…"));
                    }
                    if deck.track.is_some() {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let locked = p.load_lock && p.engine.is_playing(index);
                            let hint = if locked {
                                "Eject/Load Lock: stop the deck first"
                            } else {
                                "Take the track off this deck"
                            };
                            let eject = ui
                                .add_enabled(!locked, egui::Button::new(RichText::new("EJECT").small()))
                                .on_hover_text(hint)
                                .on_disabled_hover_text(hint);
                            p.out.eject = eject.clicked();
                        });
                    }
                });
                if let Some(err) = &deck.error {
                    ui.colored_label(WARN, err);
                }
            });
        }
        E::Title => match &deck.track {
            Some(track) => {
                // Dragging the title takes the track to a playlist or another deck.
                let response = if compact {
                    ui.label(RichText::new(format!("{} — {}", track.title, track.artist)).strong())
                } else {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&track.title).size(18.0).strong());
                        ui.label(&track.artist);
                    })
                    .response
                };
                let response = response
                    .interact(Sense::drag())
                    .on_hover_cursor(egui::CursorIcon::Grab)
                    .on_hover_text("Drag to a playlist, deck or channel");
                p.out.drag_track |= response.drag_started();
            }
            // As tall as a loaded one's, so every deck is the same size.
            None if compact => {
                ui.label(RichText::new("Empty — double-click a track in the library").weak());
            }
            None => {
                ui.vertical(|ui| {
                    ui.label(RichText::new("Empty").size(18.0).weak());
                    ui.label(RichText::new("Double-click a track in the library").weak());
                });
            }
        },
        E::Info => {
            let size = if compact { 20.0 } else { 26.0 };
            let Some(track) = &deck.track else {
                // Placeholders, as tall as the values.
                ui.label(RichText::new("---.--").size(size).monospace().weak());
                ui.label(RichText::new("BPM").weak());
                return;
            };
            ui.label(
                RichText::new(format!("{:.2}", deck.current_bpm().unwrap_or(0.0)))
                    .size(size)
                    .monospace(),
            );
            ui.label("BPM");
            if deck.is_analysing() {
                ui.spinner().on_hover_text("Detecting the beat grid and key");
            }
            ui.separator();
            ui.label(
                RichText::new(format!("{:+.2}%", (deck.tempo_ratio() - 1.0) * 100.0)).monospace(),
            );
            if let Some(heard) = deck.heard_key() {
                ui.separator();
                // The key heard now; the shift in semitones when there is one.
                let shifted = deck.key() != Some(heard);
                let mut text = format_key(heard, deck.prefs.key_notation);
                if deck.key_shift != 0 {
                    text += &format!(" {:+}", deck.key_shift);
                }
                let color = if deck.key_synced {
                    ACTIVE
                } else if shifted {
                    WARN
                } else {
                    ui.visuals().text_color()
                };
                ui.label(RichText::new(text).monospace().color(color));
            } else if let Some(key) = &track.key {
                ui.separator();
                ui.label(RichText::new(display_key(key, deck.prefs.key_notation)).monospace());
            }
            let mut new_gain = None;
            if deck.prefs.auto_gain {
                ui.separator();
                let mut gain = track.gain_db;
                let response = ui
                    .add(
                        egui::DragValue::new(&mut gain)
                            .range(crate::deck::TRACK_GAIN_DB)
                            .speed(0.05)
                            .fixed_decimals(1)
                            .custom_formatter(|v, _| format!("{v:+.1} dB")),
                    )
                    .on_hover_text("Track gain: drag; right-click to re-measure");
                let mut measured = None;
                response.context_menu(|ui| {
                    if ui.button("Measure again").clicked() {
                        measured = deck.measured_gain_db();
                        ui.close();
                    }
                    if ui.button("0 dB").clicked() {
                        measured = Some(0.0);
                        ui.close();
                    }
                });
                new_gain = measured.or(response.changed().then_some(gain));
            }
            ui.separator();
            let size = if compact { 15.0 } else { 20.0 };
            ui.label(RichText::new(format_time(position)).size(size).monospace());
            let remaining =
                RichText::new(format!("-{}", format_time(track.duration - position))).size(size).monospace();
            ui.label(match end_warning(ui, p.end_warning, track.duration - position, playing) {
                Some(true) => remaining.color(RED),
                _ => remaining.weak(),
            });
            // Bar counter, and bars until the next memory cue, like rekordbox.
            ui.separator();
            let (bar, beat) = track.bar_and_beat(position);
            ui.label(RichText::new(format!("{bar}.{beat}")).size(size).monospace())
                .on_hover_text("Bar and beat");
            if let Some(beats) = track.beats_to_next_memory(position) {
                let soon = beats <= 16;
                let color = if soon { CUE_COLOR } else { ui.visuals().weak_text_color() };
                ui.label(
                    RichText::new(format!("{} Bars", crate::deck::bars_label(beats)))
                        .size(size * 0.8)
                        .monospace()
                        .color(color),
                )
                .on_hover_text("Bars.beats to the next memory cue");
            }
            if let Some(db) = new_gain {
                deck.set_track_gain(db, engine);
            }
        }
        E::Overview => {
            let height = if compact { 28.0 } else { 44.0 };
            let width = ui.available_width().max(160.0);
            // Below it, with stems, which parts of the track have them (room
            // kept without, so every deck is the same size).
            let strip_height = if compact { 3.0 } else { 4.0 };
            let below = strip_height + 2.0;
            let Some(track) = &deck.track else {
                let (whole, _) = ui.allocate_exact_size(vec2(width, height + below), Sense::hover());
                let rect = egui::Rect::from_min_size(whole.min, vec2(width, height));
                ui.painter().rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
                return;
            };
            let strip = match &deck.stems {
                crate::deck::Stems::Ready(layers) => Some((layers.clone(), strip_height)),
                _ => None,
            };
            let (whole, response) = ui.allocate_exact_size(vec2(width, height + below), Sense::click_and_drag());
            let rect = egui::Rect::from_min_size(whole.min, vec2(width, height));
            let hot_cues = hot_cue_marks(track);
            let markers = Markers {
                beats: &[],
                hot_cues: &hot_cues,
                memory_cues: &track.memory_cues,
                memory_loops: &track.memory_loops,
                cue_point: deck.cue_point,
                active_loop: deck.loop_region.map(|r| (r, deck.loop_active)),
                grid_strength: 0.0,
            };
            let shown = engine.display_position_secs(index);
            draw_overview(&ui.painter_at(rect), rect, &track.waveform, shown, track.duration, &markers);
            if let Some((layers, h)) = &strip {
                let under = egui::Rect::from_min_size(rect.left_bottom() + vec2(0.0, 2.0), vec2(width, *h));
                stems_strip(ui, under, layers);
            }
            if end_warning(ui, p.end_warning, track.duration - shown, playing) == Some(true) {
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 2.0, RED.gamma_multiply(0.3));
                painter.rect_stroke(rect, 2.0, Stroke::new(2.0_f32, RED), egui::StrokeKind::Inside);
            }
            if let Some(pointer) =
                response.interact_pointer_pos().filter(|_| response.clicked() || response.dragged())
            {
                // Needle Lock: no jumping around a playing track.
                if !(p.needle_lock && playing) {
                    let seconds = overview_time(rect, pointer, track.duration);
                    deck.seek(seconds, engine);
                }
            }
        }
        E::Cue => {
            // Lit while held, or paused at the cue point (like a CDJ).
            let at_cue = deck.track.is_some() && !playing && (position - deck.cue_point).abs() < crate::deck::AT_CUE;
            let size = p.round_size.unwrap_or(big.y);
            let cue = round_button(ui, RoundIcon::Text("CUE"), CUE_COLOR, at_cue || *p.cue_held, size);
            let held = cue.is_pointer_button_down_on();
            if held && !*p.cue_held {
                deck.cue_pressed(engine);
            } else if !held && *p.cue_held {
                deck.cue_released(engine);
            }
            *p.cue_held = held;
        }
        E::Play => {
            let icon = if playing { RoundIcon::Pause } else { RoundIcon::Play };
            let size = p.round_size.unwrap_or(big.y);
            if round_button(ui, icon, PLAY_COLOR, playing, size).clicked() {
                p.out.play = true;
            }
        }
        E::Sync => p.out.toggle_sync = toggle(ui, deck.sync, "BEAT SYNC").clicked(),
        E::Master => p.out.make_master_pressed = toggle_in(ui, p.is_master, "MASTER", CUE_COLOR).clicked(),
        E::Quantize => {
            if toggle_in(ui, deck.quantize, "Q", RED)
                .on_hover_text("Snap cues and loops to the beat")
                .clicked()
            {
                deck.quantize = !deck.quantize;
            }
        }
        E::Vinyl => {
            if toggle(ui, deck.vinyl, "VINYL")
                .on_hover_text("Jog top scratches")
                .clicked()
            {
                deck.vinyl = !deck.vinyl;
            }
        }
        E::Slip => {
            if toggle_in(ui, deck.slip, "SLIP", RED)
                .on_hover_text("Return where the track would be")
                .clicked()
            {
                deck.toggle_slip(engine);
            }
        }
        E::LoopInOut => {
            let looping = deck.loop_active;
            split(ui, &[1.2, 1.2, 1.8], |ui, i| match i {
                0 => {
                    if toggle_in(ui, deck.loop_in.is_some() && !looping, "IN", CUE_COLOR).clicked() {
                        deck.loop_in(engine);
                    }
                }
                1 => {
                    if toggle_in(ui, looping, "OUT", CUE_COLOR).clicked() {
                        deck.loop_out(engine);
                    }
                }
                _ => {
                    let label = if looping { "EXIT" } else { "RELOOP" };
                    if toggle_in(ui, looping, label, CUE_COLOR).clicked() {
                        deck.reloop_exit(engine);
                    }
                }
            });
        }
        E::LoopResize => {
            split(ui, &[1.0, 1.0], |ui, i| {
                let (label, factor, hover) =
                    if i == 0 { ("½", 0.5, "Halve the loop") } else { ("×2", 2.0, "Double the loop") };
                if ui.button(label).on_hover_text(hover).clicked() {
                    deck.resize_loop(factor, engine);
                }
            });
        }
        E::BeatLoops => {
            const BEATS: [f64; 4] = [1.0, 4.0, 8.0, 16.0];
            split(ui, &[1.0; 4], |ui, i| {
                {
                    let beats = BEATS[i];
                    if ui.button(format!("{beats}")).on_hover_text(format!("{beats}-beat loop")).clicked() {
                        deck.beat_loop(beats, engine);
                    }
                }
            });
        }
        E::BeatJump => {
            // JUMP ◀ − 4 beats + ▶: the arrows jump, − and + set how far.
            let size = deck.jump_size;
            split(ui, &[1.0, 1.0, 2.2, 1.0, 1.0], |ui, i| match i + 1 {
                1 => {
                    if ui.button("◀").on_hover_text("Jump back").clicked() {
                        deck.beat_jump(-deck.jump_size, engine);
                    }
                }
                2 => {
                    let shorter = ui.add_enabled(size > 1.0 / 8.0, egui::Button::new("−"));
                    if shorter.on_hover_text("Shorter jumps (½)").clicked() {
                        deck.change_jump_size(0.5);
                    }
                }
                3 => {
                    let unit = if size == 1.0 { "beat" } else { "beats" };
                    ui.label(RichText::new(format!("{} {unit}", beats_label(size))).monospace())
                        .on_hover_text("How far ◀ ▶ jump");
                }
                4 => {
                    let longer = ui.add_enabled(size < 64.0, egui::Button::new("+"));
                    if longer.on_hover_text("Longer jumps (×2)").clicked() {
                        deck.change_jump_size(2.0);
                    }
                }
                _ => {
                    if ui.button("▶").on_hover_text("Jump forward").clicked() {
                        deck.beat_jump(deck.jump_size, engine);
                    }
                }
            });
        }
        E::Gate => {
            if toggle(ui, deck.hot_cue_gate, "GATE")
                .on_hover_text("Hot cues play only while held")
                .clicked()
            {
                deck.hot_cue_gate = !deck.hot_cue_gate;
            }
        }
        E::MemoryCall => {
            // ◀ MEMORY ▶, as on a CDJ.
            split(ui, &[1.0, 1.0], |ui, i| match i {
                0 => {
                    if ui.button("◀").on_hover_text("Previous memory cue").clicked() {
                        deck.memory_call(false, engine);
                    }
                }
                _ => {
                    if ui.button("▶").on_hover_text("Next memory cue").clicked() {
                        deck.memory_call(true, engine);
                    }
                }
            });
        }
        E::MemorySet => {
            split(ui, &[1.2, 1.8], |ui, i| {
                if i == 0 {
                    let set = ui.button("SET");
                    if set.on_hover_text("Save the cue or loop as a memory cue").clicked() {
                        deck.memory_set(engine);
                    }
                } else {
                    let delete = ui.button("DELETE");
                    if delete.on_hover_text("Delete the memory cue here").clicked() {
                        deck.memory_delete();
                    }
                }
            });
        }
        E::PadModes => {
            // Pad modes like rekordbox; the controller's mode buttons switch them too.
            let weight = if compact { 1.0 } else { 1.3 };
            split(ui, &[weight; 8], |ui, i| {
                let mode = PadMode::ALL[i];
                let label = RichText::new(pad_mode_short(mode)).small();
                let lit = deck.pad_mode == mode && !deck.grid_edit;
                if ui.selectable_label(lit, label).on_hover_text(mode.label()).clicked() {
                    deck.grid_edit = false;
                    deck.set_pad_mode(mode);
                }
            });
        }
        E::PadShift if deck.grid_edit => {}
        E::PadShift => {
            // SHIFT works like the controller's: latch it on screen or hold
            // the keyboard's Shift key. (The pads' ◀ ▶ are under them.)
            if toggle_in(ui, deck.screen_shift, "SHIFT", CUE_COLOR)
                .on_hover_text("The pads' second functions (or hold Shift)")
                .clicked()
            {
                deck.screen_shift = !deck.screen_shift;
            }
        }
        E::Pads if deck.grid_edit => {
            let pads = PadBox::new(ui, compact);
            pads.show(ui, |ui| grid_editor(ui, deck, position, index, compact, &mut p.out));
        }
        E::Pads if deck.pad_mode == PadMode::PadFx => {
            let shift_held = deck.screen_shift || ui.input(|i| i.modifiers.shift);
            pad_fx_area(ui, deck, p.effects, p.pad_held, &mut p.out, shift_held, index, compact);
        }
        E::Pads if deck.pad_mode == PadMode::Stems => {
            stems_pads(ui, deck, engine, &mut p.out, p.stem_model, p.live_stems, compact);
        }
        E::Pads => {
            let shift_held = deck.screen_shift || ui.input(|i| i.modifiers.shift);
            let pads = PadBox::new(ui, compact);
            pads.show(ui, |ui| {
                for row in [0..4, 4..8] {
                    ui.horizontal(|ui| {
                        for slot in row {
                            let face = pad_face(deck, slot, shift_held, p.effects, engine, index);
                            let text = RichText::new(face.label)
                                .strong()
                                .size(if compact { 11.0 } else { 13.0 });
                            // Held pads must keep counting as held past egui's click time.
                            let button = egui::Button::new(text)
                                .fill(face.fill)
                                .min_size(pads.pad)
                                .sense(Sense::click_and_drag());
                            let response = ui
                                .add_enabled(deck.track.is_some() || deck.pad_mode == PadMode::Sampler, button)
                                .on_hover_text(face.hover);
                            let held = &mut p.pad_held[slot];
                            hold_pad(ui, &response, deck.pad_mode, slot, shift_held, held, &mut p.out);
                        }
                    });
                }
                // Under the pads, as the controller's ◀ ▶: what they do
                // (the jump distances, loop lengths, sampler bank, pages).
                let mode = deck.pad_mode;
                let (down, up) =
                    (mode.parameter_action(false, shift_held), mode.parameter_action(true, shift_held));
                if (down.is_some() || up.is_some()) && mode != PadMode::HotCue {
                    let caption = pad_parameter_caption(deck, p.effects, shift_held);
                    let (less, more) = match mode {
                        PadMode::BeatJump if shift_held => ("Shorter jumps (÷16)", "Longer jumps (×16)"),
                        PadMode::BeatJump => ("Shorter jumps (½); Shift: ÷16", "Longer jumps (×2); Shift: ×16"),
                        PadMode::BeatLoop => ("Shorter loops (½)", "Longer loops (×2)"),
                        _ => ("Previous", "Next"),
                    };
                    ui.horizontal(|ui| {
                        let side = (pads.size.x - 3.0 * PAD_GAP) / 4.0;
                        let arrow = |ui: &mut Ui, label: &str, action: Option<DeckAction>, hover: &str| {
                            let button = egui::Button::new(RichText::new(label).strong()).min_size(vec2(side, 0.0));
                            ui.add_enabled(action.is_some(), button).on_hover_text(hover).clicked()
                        };
                        if arrow(ui, "<", down, less) && let Some(action) = down {
                            p.out.pad_events.extend([(action, true), (action, false)]);
                        }
                        ui.add_sized(
                            vec2(2.0 * side + PAD_GAP, ui.spacing().interact_size.y),
                            egui::Label::new(RichText::new(caption).small().monospace()),
                        );
                        if arrow(ui, ">", up, more) && let Some(action) = up {
                            p.out.pad_events.extend([(action, true), (action, false)]);
                        }
                    });
                }
            });
        }
        E::TempoRange => {
            {
                let mut range = deck.tempo_range;
                egui::ComboBox::from_id_salt(("range", index))
                    .selected_text(range.label())
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for &option in &p.tempo_ranges {
                            ui.selectable_value(&mut range, option, option.label());
                        }
                    });
                if range != deck.tempo_range {
                    deck.set_tempo_range(range, engine);
                }
            }
        }
        E::MasterTempo => {
            if toggle_in(ui, deck.key_lock, "MT", RED)
                .on_hover_text("Keep the key at any tempo")
                .clicked()
            {
                deck.toggle_key_lock(engine);
            }
        }
        E::KeyShift => {
            {
                split(ui, &[1.0, 1.0, 1.0], |ui, i| match i {
                    0 => {
                        if ui.button("−").on_hover_text("Key down a semitone").clicked() {
                            deck.set_key_shift(deck.key_shift - 1, engine);
                        }
                    }
                    1 => {
                        let shift = ui.button(format!("{:+}", deck.key_shift));
                        if shift.on_hover_text("Semitones; click to reset").clicked() {
                            deck.set_key_shift(0, engine);
                        }
                    }
                    _ => {
                        if ui.button("+").on_hover_text("Key up a semitone").clicked() {
                            deck.set_key_shift(deck.key_shift + 1, engine);
                        }
                    }
                });
            }
        }
        E::KeySyncTap => {
            stack(ui, 2, |ui, row| {
                if row == 0 {
                    p.out.key_sync = toggle(ui, deck.key_synced, "KEY SYNC")
                        .on_hover_text("Match the master's key")
                        .clicked();
                } else if ui.button("TAP").on_hover_text("Tap the BPM").clicked()
                    && let Some(bpm) = deck.tap_bpm()
                {
                    p.out.status = Some(format!("Deck {}: BPM set to {bpm:.2} by tapping", index + 1));
                }
            });
        }
        E::GridEdit => {
            if toggle(ui, deck.grid_edit, "GRID")
                .on_hover_text("Edit the beat grid")
                .clicked()
            {
                deck.grid_edit = !deck.grid_edit;
            }
            // Without the pads on screen, the editor opens under the button.
            if deck.grid_edit && !p.pads_shown {
                ui.vertical(|ui| grid_editor(ui, deck, position, index, compact, &mut p.out));
            }
        }
        E::TempoFader => {
            // Like on a CDJ, pulling the fader down makes the track faster.
            let mut fader = -deck.tempo_fader;
            ui.spacing_mut().slider_width = if compact { 110.0 } else { 170.0 };
            let slider = ui.add_enabled(
                deck.tempo_fader_active(),
                Slider::new(&mut fader, -1.0..=1.0).vertical().show_value(false),
            );
            if slider.changed() {
                deck.set_tempo_fader(-fader, engine);
            }
            slider.on_disabled_hover_text("Follows the master (SYNC)");
        }
        E::TempoDisplay => {
            let ranges = p.tempo_ranges.clone();
            tempo_display(ui, deck, engine, playing, position, index, &ranges, &mut p.out.status);
        }
        E::TempoReset => {
            if ui.button("RESET").on_hover_text("Tempo back to ±0").clicked() {
                deck.reset_tempo(engine);
            }
        }
        E::Nudge => {
            stack(ui, 2, |ui, row| {
                if row == 0 {
                    caption(ui, "NUDGE");
                    return;
                }
                split(ui, &[1.0, 1.0], |ui, i| {
                    let (label, steps, hover) =
                        if i == 0 { ("−", -3, "Nudge slower (hold)") } else { ("+", 3, "Nudge faster (hold)") };
                    let button = ui.add(egui::Button::new(label).sense(Sense::click_and_drag()));
                    if button.on_hover_text(hover).is_pointer_button_down_on() {
                        deck.jog(steps, engine);
                    }
                });
            });
        }
    }
}

/// The beat-grid editor, shown in the pads' box while GRID is on.
fn grid_editor(
    ui: &mut Ui,
    deck: &mut Deck,
    position: f64,
    index: usize,
    compact: bool,
    out: &mut DeckOut,
) {
    let Some(track) = &deck.track else {
        ui.label(RichText::new("Load a track to edit its beat grid").weak());
        return;
    };
    let (straight, old_bpm) = (track.grid_is_straight(), track.grid_bpm());
    let size = if compact { 12.0 } else { 14.0 };
    let width = ui.available_width();
    let row_height = if compact { 24.0 } else { 30.0 };
    let wide =
        |ui: &mut Ui, n: f32| vec2((ui.available_width() - (n - 1.0) * PAD_GAP) / n, row_height);
    let mut edited = false;
    ui.add_enabled_ui(straight, |ui| {
        ui.set_width(width);
        ui.horizontal(|ui| {
            // The main one: the grid's beat 1 goes where the needle is.
            let renumber = wide(ui, 4.0);
            let start = egui::Button::new(RichText::new("GRID STARTS HERE").strong().size(size))
                .fill(CUE_COLOR.gamma_multiply(0.35))
                .min_size(vec2(ui.available_width() - renumber.x - PAD_GAP, row_height));
            if ui
                .add(start)
                .on_hover_text("Beat 1 here")
                .clicked()
            {
                edited |= deck.downbeat_here(position);
            }
            if ui
                .add(egui::Button::new("1 ↻").min_size(renumber))
                .on_hover_text("Nearest beat starts the bar")
                .clicked()
            {
                edited |= deck.renumber_here(position);
            }
        });
        // Moving the grid: 1, 10 and 100 ms a click.
        ui.horizontal(|ui| {
            let w = wide(ui, 6.0);
            for (label, ms) in [("◀◀◀", -100), ("◀◀", -10), ("◀", -1), ("▶", 1), ("▶▶", 10), ("▶▶▶", 100_i32)] {
                let way = if ms < 0 { "earlier" } else { "later" };
                if ui
                    .add(egui::Button::new(label).min_size(w))
                    .on_hover_text(format!("Move the grid {} ms {way}", ms.abs()))
                    .clicked()
                {
                    edited |= deck.shift_grid(f64::from(ms) / 1000.0);
                }
            }
        });
        let mut bpm = old_bpm;
        let mut changed = false;
        ui.horizontal(|ui| {
            let w = wide(ui, 5.0);
            if ui.add(egui::Button::new("−").min_size(w)).on_hover_text("BPM −0.01").clicked() {
                (bpm, changed) = (bpm - 0.01, true);
            }
            // Only a drag or typing counts: a BPM the field merely
            // shows (even one outside its range) is no edit.
            let field = ui
                .add_sized(
                    w,
                    egui::DragValue::new(&mut bpm)
                        .speed(0.01)
                        .fixed_decimals(2)
                        .range(20.0..=999.0)
                        .clamp_existing_to_range(false),
                )
                .on_hover_text("BPM: drag or type");
            changed |= field.changed() && (field.dragged() || field.has_focus() || field.lost_focus());
            if ui.add(egui::Button::new("+").min_size(w)).on_hover_text("BPM +0.01").clicked() {
                (bpm, changed) = (bpm + 0.01, true);
            }
            if ui.add(egui::Button::new("÷2").min_size(w)).on_hover_text("Halve the BPM").clicked() {
                (bpm, changed) = (bpm / 2.0, true);
            }
            if ui.add(egui::Button::new("×2").min_size(w)).on_hover_text("Double the BPM").clicked() {
                (bpm, changed) = (bpm * 2.0, true);
            }
        });
        // Kept to two decimals, like the display.
        if changed && (bpm - old_bpm).abs() > 1e-9 {
            edited |= deck.set_grid_bpm((bpm * 100.0).round() / 100.0);
        }
    })
    .response
    .on_disabled_hover_text("Variable-tempo grids can't be edited yet");
    if edited {
        let bpm = deck.track.as_ref().map_or(0.0, |t| t.bpm);
        out.status = Some(format!("Deck {}: beat grid edited ({bpm:.2} BPM)", index + 1));
    }
}

/// Presses a pad while the pointer is down on it and lets go on release.
/// Shift-click or right-click is Shift + pad.
fn hold_pad(
    ui: &Ui,
    response: &egui::Response,
    mode: PadMode,
    slot: usize,
    shift_held: bool,
    held: &mut Option<DeckAction>,
    out: &mut DeckOut,
) {
    let down = |button| ui.input(|i| i.pointer.button_down(button));
    let on = response.is_pointer_button_down_on();
    let primary = on && down(egui::PointerButton::Primary);
    let secondary = on && down(egui::PointerButton::Secondary);
    match (*held, primary || secondary) {
        (None, true) => {
            if let Some(action) = mode.pad_action(slot as u8, secondary || shift_held) {
                out.pad_events.push((action, true));
                *held = Some(action);
            }
        }
        (Some(action), false) => {
            out.pad_events.push((action, false));
            *held = None;
        }
        _ => {}
    }
}

/// A strip under the overview showing which parts of the track have their
/// stems: lit where they play, dim where the deck still plays the original.
fn stems_strip(ui: &Ui, rect: egui::Rect, layers: &deckhand_audio::StemLayers) {
    let painter = ui.painter_at(rect);
    let lit = STEM_COLORS[1];
    painter.rect_filled(rect, 1.0, lit.gamma_multiply(0.25));
    let ready: Vec<bool> = layers.ready_blocks().collect();
    let x = |block: usize| rect.left() + rect.width() * block as f32 / ready.len() as f32;
    let mut start = None;
    for (block, &done) in ready.iter().chain([&false]).enumerate() {
        match (done, start) {
            (true, None) => start = Some(block),
            (false, Some(from)) => {
                let run = egui::Rect::from_x_y_ranges(x(from)..=x(block), rect.y_range());
                painter.rect_filled(run, 0.0, lit);
                start = None;
            }
            _ => {}
        }
    }
    let share = layers.ready_share();
    ui.interact(rect, ui.id().with("stems strip"), Sense::hover()).on_hover_text(if share < 1.0 {
        format!("Stems {:.0}% ready", share * 100.0)
    } else {
        "Stems ready".to_owned()
    });
}

/// The stem parts' colours on the STEMS pads.
pub(super) const STEM_COLORS: [Color32; 4] = [
    Color32::from_rgb(235, 200, 60),
    Color32::from_rgb(90, 200, 120),
    Color32::from_rgb(70, 150, 235),
    Color32::from_rgb(225, 90, 70),
];

/// STEMS pad mode, like the GRV6's: pads 1–4 turn the vocals, instruments,
/// bass and drums off and on; 5 is Part Isolator; 6 shows the separation
/// (or starts it).
fn stems_pads(
    ui: &mut Ui,
    deck: &mut Deck,
    engine: &mut Engine,
    out: &mut DeckOut,
    stem_model: bool,
    live_stems: bool,
    compact: bool,
) {
    use crate::deck::Stems;
    let pads = PadBox::new(ui, compact);
    let size = if compact { 11.0 } else { 13.0 };
    let loaded = deck.track.is_some();
    let ready = matches!(deck.stems, Stems::Ready(_));
    pads.show(ui, |ui| {
        ui.horizontal(|ui| {
            for (part, label) in deckhand_control::STEM_PARTS.iter().enumerate() {
                let color = STEM_COLORS[part];
                let on = deck.stem_on[part] && deck.has_part(part);
                let text = RichText::new(*label).strong().size(size);
                let button = egui::Button::new(if on { text.color(Color32::BLACK) } else { text.color(color) })
                    .fill(if on { color.gamma_multiply(0.85) } else { Color32::from_gray(30) })
                    .min_size(pads.pad);
                let hover = if !ready {
                    "Available once the track is separated (pad 6)".to_owned()
                } else if deck.has_part(part) {
                    format!("Turn the {} off or on", label.to_lowercase())
                } else {
                    "This stem model has no such part; it stays in INST".to_owned()
                };
                if ui.add_enabled(ready && deck.has_part(part), button).on_disabled_hover_text(&hover).on_hover_text(&hover).clicked() {
                    deck.toggle_stem(part, engine);
                }
            }
        });
        ui.horizontal(|ui| {
            let iso = egui::Button::new(RichText::new("ISOLATOR").strong().size(size))
                .fill(if deck.part_isolator { ACTIVE.gamma_multiply(0.6) } else { Color32::from_gray(30) })
                .min_size(pads.pad);
            if ui
                .add_enabled(ready, iso)
                .on_hover_text("HI / MID / LOW: vocal, inst, drums")
                .clicked()
            {
                deck.toggle_part_isolator(engine);
            }
            let (label, hover, enabled) = match &deck.stems {
                Stems::Ready(layers) if layers.ready_share() < 1.0 => (
                    format!("{:.0}%", layers.ready_share() * 100.0),
                    "Separating piece by piece; the parts already work where it's done",
                    false,
                ),
                Stems::Ready(_) => ("READY".to_owned(), "The track is separated", false),
                Stems::Separating(percent) => (
                    percent.map_or("…".to_owned(), |p| format!("{p}%")),
                    "Separating; the track plays as it is meanwhile",
                    false,
                ),
                Stems::Failed => ("RETRY".to_owned(), "Separating failed (see the log); try again", true),
                Stems::None if !live_stems => {
                    ("OFF".to_owned(), "Stems are turned off in Preferences, Stems", false)
                }
                Stems::None if !stem_model => {
                    ("NO MODEL".to_owned(), "Add a stem model in Preferences, Stems", false)
                }
                Stems::None => ("SEPARATE".to_owned(), "Separate the track into stems", true),
            };
            let status = egui::Button::new(RichText::new(label).strong().size(size))
                .fill(Color32::from_gray(30))
                .min_size(pads.pad);
            let response = ui.add_enabled(loaded && enabled, status);
            if response.on_hover_text(hover).on_disabled_hover_text(hover).clicked() {
                out.separate = true;
            }
            for _ in 0..2 {
                ui.add_enabled(false, egui::Button::new("").fill(Color32::from_gray(22)).min_size(pads.pad));
            }
        });
    });
}

/// Effect names in colours by kind, as on rekordbox's Pad FX pads.
fn pad_fx_color(kind: deckhand_audio::BeatFxKind) -> Color32 {
    use deckhand_audio::BeatFxKind as K;
    match kind {
        K::Roll => Color32::from_rgb(90, 205, 235),
        K::Echo | K::Delay | K::LowCutEcho | K::Spiral => Color32::from_rgb(95, 150, 235),
        K::Reverb => Color32::from_rgb(120, 215, 70),
        K::Flanger | K::Phaser | K::Filter | K::Helix => Color32::from_rgb(70, 175, 145),
        K::Trans | K::Pitch | K::MobiusSaw | K::MobiusTri => Color32::from_rgb(185, 125, 235),
    }
}

/// A small name in front of (or above) a group of buttons.
fn caption(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).small().weak());
}

/// The pads' box, the same in every pad mode: as tall as the Pad FX
/// settings need, four pads across and two rows filling it above a row for
/// pages and the cog.
struct PadBox {
    size: egui::Vec2,
    /// A settings row.
    row: f32,
    /// The pads' (or settings') part, above the bottom row.
    body: f32,
    pad: egui::Vec2,
}

const PAD_GAP: f32 = 3.0;

impl PadBox {
    fn new(ui: &Ui, compact: bool) -> Self {
        let width = ui.available_width().max(200.0);
        // Four settings rows (two columns of four pads), whatever the width,
        // so the pads keep their shape on small screens.
        let row = if compact { 22.0 } else { 26.0 };
        let body = 4.0 * row + 3.0 * PAD_GAP;
        let footer = ui.spacing().interact_size.y;
        Self {
            size: vec2(width, body + PAD_GAP + footer),
            row,
            body,
            pad: vec2((width - 3.0 * PAD_GAP) / 4.0, (body - PAD_GAP) / 2.0),
        }
    }

    /// Draws `contents` in the box, with the pads' spacing.
    fn show(&self, ui: &mut Ui, contents: impl FnOnce(&mut Ui)) {
        ui.allocate_ui_with_layout(self.size, egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_min_size(self.size);
            ui.spacing_mut().item_spacing = vec2(PAD_GAP, PAD_GAP);
            contents(ui);
        });
    }
}

/// Pad FX mode: the pads, or with the cog their settings (rekordbox's Pad
/// FX panel), in the same box: as tall as the settings need, so switching
/// moves nothing. Both end in the same row: pages 1 (A–H) and 2 (I–P), the cog.
#[allow(clippy::too_many_arguments)]
fn pad_fx_area(
    ui: &mut Ui,
    deck: &mut Deck,
    effects: &Effects,
    pad_held: &mut [Option<DeckAction>; 8],
    out: &mut DeckOut,
    shift_held: bool,
    index: usize,
    compact: bool,
) {
    let pads = PadBox::new(ui, compact);
    let PadBox { row, body, pad, .. } = pads;
    pads.show(ui, |ui| {
        let shift = if shift_held { 8 } else { 0 };
        let page = usize::from(deck.pad_fx_page) * 8;
        if deck.pad_fx_settings {
            pad_fx_settings(ui, deck, effects, pad_held, out, shift_held, index, row, body);
        } else {
            for pads in [0..4, 4..8] {
                ui.horizontal(|ui| {
                    for slot in pads {
                        let fx = (slot + shift + page) % 16;
                        let set = effects.pad_slot(fx);
                        let lit = effects.pad == Some((index, fx as u8));
                        let text = RichText::new(set.short())
                            .strong()
                            .size(if compact { 11.0 } else { 13.0 })
                            .color(if lit { Color32::WHITE } else { pad_fx_color(set.effect) });
                        let button = egui::Button::new(text)
                            .fill(if lit {
                                ACTIVE.gamma_multiply(0.6)
                            } else {
                                Color32::from_gray(30)
                            })
                            .min_size(pad)
                            .sense(Sense::click_and_drag());
                        let response =
                            ui.add_enabled(deck.track.is_some(), button).on_hover_text(format!(
                                "Pad {}: hold {}; Shift {}",
                                char::from(b'A' + fx as u8),
                                set.label(),
                                effects.pad_slot((fx + 8) % 16).label(),
                            ));
                        hold_pad(
                            ui,
                            &response,
                            PadMode::PadFx,
                            slot,
                            shift_held,
                            &mut pad_held[slot],
                            out,
                        );
                    }
                });
            }
        }
        ui.horizontal(|ui| {
            for (n, label) in ["1", "2"].into_iter().enumerate() {
                if ui
                    .selectable_label(
                        usize::from(deck.pad_fx_page) == n,
                        RichText::new(label).strong(),
                    )
                    .on_hover_text(if n == 0 { "Pads A–H" } else { "Pads I–P" })
                    .clicked()
                {
                    deck.pad_fx_page = n as u8;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hover = if deck.pad_fx_settings {
                    "Back to the pads"
                } else {
                    "Set what each pad does"
                };
                if ui.selectable_label(deck.pad_fx_settings, "⚙").on_hover_text(hover).clicked() {
                    deck.pad_fx_settings = !deck.pad_fx_settings;
                }
                if deck.pad_fx_settings
                    && ui
                        .small_button("↺")
                        .on_hover_text("Reset these pads")
                        .clicked()
                {
                    let defaults = PadFxSlot::defaults().into_iter().enumerate();
                    out.pad_fx_edits.extend(defaults.skip(page).take(8));
                }
            });
        });
    });
}

/// rekordbox's Pad FX panel: each pad of the page held by its name and set
/// with its effect, beat and level lists, `body` tall.
#[allow(clippy::too_many_arguments)]
fn pad_fx_settings(
    ui: &mut Ui,
    deck: &Deck,
    effects: &Effects,
    pad_held: &mut [Option<DeckAction>; 8],
    out: &mut DeckOut,
    shift_held: bool,
    index: usize,
    row: f32,
    body: f32,
) {
    use deckhand_audio::BeatFxKind;
    const LEVELS: [u32; 11] = [0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
    let page = usize::from(deck.pad_fx_page) * 8;
    let shift = if shift_held { 8 } else { 0 };
    let gap = ui.spacing().item_spacing.x;
    let column_width = (ui.available_width() - gap) / 2.0;
    // Narrow decks get narrower beat and level lists.
    let list = if column_width < 260.0 { 36.0 } else { 52.0 };
    let lists = 18.0 + 2.0 * list + 3.0 * gap + 3.0 * 12.0;
    let name_width = (column_width - lists).max(40.0);
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), body),
        egui::Layout::left_to_right(egui::Align::Min),
        |ui| {
            ui.set_min_height(body);
            for column in [0..4, 4..8] {
                ui.vertical(|ui| {
                    ui.set_width(column_width);
                    for pad in column {
                        let slot = (pad + shift + page) % 16;
                        let mut set = effects.pad_slot(slot);
                        let lit = effects.pad == Some((index, slot as u8));
                        ui.horizontal(|ui| {
                            let fill = if lit {
                                ACTIVE.gamma_multiply(0.6)
                            } else {
                                Color32::from_gray(22)
                            };
                            let name = RichText::new(set.effect.label()).strong().color(if lit {
                                Color32::WHITE
                            } else {
                                pad_fx_color(set.effect)
                            });
                            let button = egui::Button::new(name)
                                .fill(fill)
                                .min_size(vec2(name_width, row))
                                .sense(Sense::click_and_drag());
                            let response = ui
                                .add_enabled(deck.track.is_some(), button)
                                .on_hover_text(format!(
                                    "Pad {}: hold for {}",
                                    char::from(b'A' + slot as u8),
                                    set.label(),
                                ));
                            hold_pad(
                                ui,
                                &response,
                                PadMode::PadFx,
                                pad,
                                shift_held,
                                &mut pad_held[pad],
                                out,
                            );
                            egui::ComboBox::from_id_salt(("pad fx effect", index, slot))
                                .selected_text("")
                                .width(18.0)
                                .show_ui(ui, |ui| {
                                    for kind in BeatFxKind::ALL {
                                        let text =
                                            RichText::new(kind.label()).color(pad_fx_color(kind));
                                        ui.selectable_value(&mut set.effect, kind, text);
                                    }
                                })
                                .response
                                .on_hover_text("Effect");
                            egui::ComboBox::from_id_salt(("pad fx beat", index, slot))
                                .selected_text(beat_label(set.beats))
                                .width(list)
                                .show_ui(ui, |ui| {
                                    for beats in FX_BEATS {
                                        ui.selectable_value(
                                            &mut set.beats,
                                            beats,
                                            beat_label(beats),
                                        );
                                    }
                                })
                                .response
                                .on_hover_text("Beat");
                            let mut level = (set.depth * 100.0).round() as u32;
                            egui::ComboBox::from_id_salt(("pad fx level", index, slot))
                                .selected_text(level.to_string())
                                .width(list)
                                .show_ui(ui, |ui| {
                                    for value in LEVELS {
                                        ui.selectable_value(&mut level, value, value.to_string());
                                    }
                                })
                                .response
                                .on_hover_text("Level/Depth");
                            set.depth = level as f32 / 100.0;
                        });
                        if set != effects.pad_slot(slot) {
                            out.pad_fx_edits.push((slot, set));
                        }
                    }
                });
            }
        },
    );
}
