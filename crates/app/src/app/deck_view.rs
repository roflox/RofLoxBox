//! A deck's controls, drawn element by element in the user's layout. In
//! layout-editing mode the elements can be dragged to other rows, into the
//! side column, or onto "Hide".

use deckhand_audio::Engine;
use deckhand_control::{DeckAction, PadMode, Value};
use eframe::egui::{self, Color32, RichText, Sense, Slider, Stroke, Ui, vec2};

use super::{
    ACTIVE, CUE_COLOR, DECK_COLORS, DeckhandApp, PLAY_COLOR, WARN, beats_label, hot_cue_marks,
    overview_time, pad_face, pad_mode_short, pad_parameter_caption, toggle, toggle_in,
};
use crate::widgets::{RoundIcon, round_button};

/// Accent colours of lit buttons.
const RED: Color32 = Color32::from_rgb(230, 60, 50);
use crate::deck::{Deck, TempoRange, key_name};
use crate::effects::Effects;
use crate::layout::{DeckElement, Place};
use crate::library::format_time;
use crate::waveform::{Markers, draw_overview};

/// What the controls asked for; applied once the deck is drawn.
#[derive(Default)]
struct DeckOut {
    play: bool,
    toggle_sync: bool,
    toggle_master: bool,
    key_sync: bool,
    pad_events: Vec<(DeckAction, bool)>,
    status: Option<String>,
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
    out: DeckOut,
}

/// A layout change from a drop: the element and where it goes (`None` hides it).
type Move = Option<(DeckElement, Option<Place>)>;

const SIDE_WIDTH: f32 = 84.0;

impl DeckhandApp {
    pub(super) fn deck_ui(&mut self, ui: &mut Ui, index: usize, compact: bool) {
        if self.engine.is_none() {
            ui.label("No audio output");
            return;
        }
        let four = self.config.four_decks;
        let layout = self.config.deck_layout(four).clone();
        let editing = self.editing_layout;
        let shown = |e: &&DeckElement| layout.is_shown(**e);
        let has_side = editing || layout.side.iter().any(|e| layout.is_shown(*e));
        // The row holds the main area, an 8-point space and the side column,
        // with the inherited item spacing between them.
        let gap = ui.spacing().item_spacing.x;
        let main_width = if has_side {
            (ui.available_width() - SIDE_WIDTH - 8.0 - 2.0 * gap - 2.0).max(220.0)
        } else {
            ui.available_width()
        };
        let mut moved: Move = None;
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
            out: DeckOut::default(),
        };
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                vec2(main_width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(main_width);
                    for (r, row) in layout.main.iter().enumerate() {
                        let row: Vec<DeckElement> = row.iter().filter(shown).copied().collect();
                        if row.is_empty() && !editing {
                            continue;
                        }
                        ui.horizontal_wrapped(|ui| {
                            for element in row {
                                place_element(ui, element, &mut parts, editing, &mut moved);
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
            if has_side {
                // A separator here would stretch to the full column height.
                ui.add_space(8.0);
                ui.allocate_ui_with_layout(
                    vec2(SIDE_WIDTH, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(SIDE_WIDTH);
                        for &element in layout.side.iter().filter(shown) {
                            place_element(ui, element, &mut parts, editing, &mut moved);
                        }
                        if editing {
                            drop_zone(ui, "+", SIDE_WIDTH - 4.0, Some(Place::SideEnd), &mut moved);
                        }
                    },
                );
            }
        });

        let out = parts.out;
        let ctx = ui.ctx().clone();
        for (action, pressed) in out.pad_events {
            self.apply_deck_event(index, action, Value::Pressed(pressed), &ctx);
        }
        if out.play {
            self.play_pause(index);
        }
        if out.toggle_sync {
            self.toggle_sync(index);
        }
        if out.toggle_master {
            self.master = if self.master == Some(index) { None } else { Some(index) };
        }
        if out.key_sync {
            self.key_sync(index);
        }
        if out.status.is_some() {
            self.status = out.status;
        }
        if let Some((element, place)) = moved {
            let layout = self.config.deck_layout_mut(four);
            match place {
                Some(place) => layout.move_to(element, place),
                None => layout.set_shown(element, false),
            }
            self.config.save();
        }
    }
}

/// Draws an element; while editing, as a draggable box that elements can be
/// dropped on (in front of it, or behind it on its right half).
fn place_element(
    ui: &mut Ui,
    element: DeckElement,
    p: &mut Parts<'_>,
    editing: bool,
    moved: &mut Move,
) {
    if !editing {
        draw_element(ui, element, p);
        return;
    }
    let id = egui::Id::new(("deck element", p.index, element));
    let response = ui
        .dnd_drag_source(id, element, |ui| {
            egui::Frame::new()
                .stroke(Stroke::new(1.0_f32, Color32::from_gray(90)))
                .inner_margin(2.0)
                .show(ui, |ui| {
                    ui.add_enabled_ui(false, |ui| draw_element(ui, element, p));
                });
        })
        .response
        .on_hover_text(format!("{} — drag to move", element.label()));
    // While dragging, nothing counts as hovered; use the pointer itself.
    let pointer = ui.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));
    let after = pointer.is_some_and(|pos| pos.x > response.rect.center().x);
    if response.dnd_hover_payload::<DeckElement>().is_some() {
        // Show where it would land.
        let x = if after { response.rect.right() } else { response.rect.left() };
        ui.painter().vline(x, response.rect.y_range(), Stroke::new(3.0_f32, ACTIVE));
    }
    if let Some(dragged) = response.dnd_release_payload::<DeckElement>() {
        *moved = Some((*dragged, Some(Place::Next { to: element, after })));
    }
}

/// A target for dropping elements: the end of a row, a new row, the side
/// column, or `None` to hide them.
fn drop_zone(ui: &mut Ui, text: &str, width: f32, place: Option<Place>, moved: &mut Move) {
    let (rect, response) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
    let hovered = response.dnd_hover_payload::<DeckElement>().is_some();
    let color = if hovered { ACTIVE } else { Color32::from_gray(70) };
    ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0_f32, color), egui::StrokeKind::Inside);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(11.0),
        color,
    );
    if let Some(dragged) = response.dnd_release_payload::<DeckElement>() {
        *moved = Some((*dragged, place));
    }
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
                });
                if let Some(err) = &deck.error {
                    ui.colored_label(WARN, err);
                }
            });
        }
        E::Title => match &deck.track {
            Some(track) if compact => {
                ui.label(RichText::new(format!("{} — {}", track.title, track.artist)).strong());
            }
            Some(track) => {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&track.title).size(18.0).strong());
                    ui.label(&track.artist);
                });
            }
            None => {
                ui.label(RichText::new("Empty — double-click a track in the library").weak());
            }
        },
        E::Info => {
            let Some(track) = &deck.track else { return };
            let size = if compact { 20.0 } else { 26.0 };
            ui.label(
                RichText::new(format!("{:.2}", deck.current_bpm().unwrap_or(0.0)))
                    .size(size)
                    .monospace(),
            );
            ui.label("BPM");
            ui.separator();
            ui.label(
                RichText::new(format!("{:+.2}%", (deck.tempo_ratio() - 1.0) * 100.0)).monospace(),
            );
            if let Some(heard) = deck.heard_key() {
                ui.separator();
                // The key heard now; the shift in semitones when there is one.
                let shifted = deck.key() != Some(heard);
                let mut text = key_name(heard);
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
                ui.label(RichText::new(key).monospace());
            }
            ui.separator();
            let size = if compact { 15.0 } else { 20.0 };
            ui.label(RichText::new(format_time(position)).size(size).monospace());
            ui.label(
                RichText::new(format!("-{}", format_time(track.duration - position)))
                    .size(size)
                    .monospace()
                    .weak(),
            );
        }
        E::Overview => {
            let Some(track) = &deck.track else { return };
            let height = if compact { 28.0 } else { 44.0 };
            let width = ui.available_width().max(160.0);
            let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
            let hot_cues = hot_cue_marks(track);
            let markers = Markers {
                beats: &[],
                hot_cues: &hot_cues,
                memory_cues: &track.memory_cues,
                memory_loops: &track.memory_loops,
                cue_point: deck.cue_point,
                active_loop: None,
                grid_strength: 0.0,
            };
            draw_overview(&ui.painter_at(rect), rect, &track.waveform, position, track.duration, &markers);
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
            let cue = round_button(ui, RoundIcon::Text("CUE"), CUE_COLOR, at_cue || *p.cue_held, big.y);
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
            if round_button(ui, icon, PLAY_COLOR, playing, big.y).clicked() {
                p.out.play = true;
            }
        }
        E::Sync => p.out.toggle_sync = toggle(ui, deck.sync, "BEAT SYNC").clicked(),
        E::Master => p.out.toggle_master = toggle_in(ui, p.is_master, "MASTER", CUE_COLOR).clicked(),
        E::Quantize => {
            if toggle_in(ui, deck.quantize, "Q", RED)
                .on_hover_text("Quantize: snap cues and loops to the beat")
                .clicked()
            {
                deck.quantize = !deck.quantize;
            }
        }
        E::Vinyl => {
            if toggle(ui, deck.vinyl, "VINYL")
                .on_hover_text("Jog top scratches when on, bends when off")
                .clicked()
            {
                deck.vinyl = !deck.vinyl;
            }
        }
        E::Slip => {
            if toggle_in(ui, deck.slip, "SLIP", RED)
                .on_hover_text("Slip: after scratching, a loop or a held hot cue, the track carries on where it would have been")
                .clicked()
            {
                deck.toggle_slip(engine);
            }
        }
        E::LoopInOut => {
            let looping = deck.loop_active;
            if toggle_in(ui, deck.loop_in.is_some() && !looping, "IN", CUE_COLOR).clicked() {
                deck.loop_in(engine);
            }
            if toggle_in(ui, looping, "OUT", CUE_COLOR).clicked() {
                deck.loop_out(engine);
            }
            let label = if looping { "EXIT" } else { "RELOOP" };
            if toggle_in(ui, looping, label, CUE_COLOR).clicked() {
                deck.reloop_exit(engine);
            }
        }
        E::LoopResize => {
            if ui.button("½").on_hover_text("Halve the loop").clicked() {
                deck.resize_loop(0.5, engine);
            }
            if ui.button("×2").on_hover_text("Double the loop").clicked() {
                deck.resize_loop(2.0, engine);
            }
        }
        E::BeatLoops => {
            for beats in [1.0, 4.0, 8.0, 16.0] {
                if ui.button(format!("{beats}")).on_hover_text(format!("{beats}-beat loop")).clicked() {
                    deck.beat_loop(beats, engine);
                }
            }
        }
        E::BeatJump => {
            if ui.button("◀").on_hover_text("Beat jump back").clicked() {
                deck.beat_jump(-deck.jump_size, engine);
            }
            let response = ui.add(
                egui::Button::new(RichText::new(beats_label(deck.jump_size)).monospace())
                    .min_size(vec2(34.0, 0.0)),
            );
            if response.clicked() {
                deck.change_jump_size(2.0);
            }
            if response.secondary_clicked() {
                deck.change_jump_size(0.5);
            }
            response.on_hover_text("Jump size in beats: click to double, right-click to halve");
            if ui.button("▶").on_hover_text("Beat jump forward").clicked() {
                deck.beat_jump(deck.jump_size, engine);
            }
        }
        E::Gate => {
            if toggle(ui, deck.hot_cue_gate, "GATE")
                .on_hover_text("Gate: hot cues play only while held, then return and pause")
                .clicked()
            {
                deck.hot_cue_gate = !deck.hot_cue_gate;
            }
        }
        E::MemoryCall => {
            if ui.button("◀ MEM").on_hover_text("Call the previous memory cue").clicked() {
                deck.memory_call(false, engine);
            }
            if ui.button("MEM ▶").on_hover_text("Call the next memory cue").clicked() {
                deck.memory_call(true, engine);
            }
        }
        E::MemorySet => {
            if ui
                .button("MEMORY")
                .on_hover_text("Store the cue point (or active loop) as a memory cue")
                .clicked()
            {
                deck.memory_set(engine);
            }
            if ui.button("DELETE").on_hover_text("Delete the memory cue at the cue point").clicked() {
                deck.memory_delete();
            }
        }
        E::PadModes => {
            // Pad modes like rekordbox; the controller's mode buttons switch them too.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for mode in PadMode::ALL {
                    if mode == PadMode::Stems {
                        continue;
                    }
                    let label = if compact { pad_mode_short(mode) } else { mode.label() };
                    if ui.selectable_label(deck.pad_mode == mode, RichText::new(label).small()).clicked() {
                        deck.set_pad_mode(mode);
                    }
                }
            });
        }
        E::PadShift => {
            // SHIFT works like the controller's: latch it on screen or hold
            // the keyboard's Shift key.
            let shift_held = deck.screen_shift || ui.input(|i| i.modifiers.shift);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if toggle_in(ui, deck.screen_shift, "SHIFT", CUE_COLOR)
                    .on_hover_text("Shift for the pads and the ◀ ▶ buttons (or hold the Shift key)")
                    .clicked()
                {
                    deck.screen_shift = !deck.screen_shift;
                }
                let mode = deck.pad_mode;
                let down = mode.parameter_action(false, shift_held);
                if ui.add_enabled(down.is_some(), egui::Button::new("◀").small()).clicked()
                    && let Some(action) = down
                {
                    p.out.pad_events.extend([(action, true), (action, false)]);
                }
                ui.label(
                    RichText::new(pad_parameter_caption(deck, p.effects, shift_held)).small().monospace(),
                );
                let up = mode.parameter_action(true, shift_held);
                if ui.add_enabled(up.is_some(), egui::Button::new("▶").small()).clicked()
                    && let Some(action) = up
                {
                    p.out.pad_events.extend([(action, true), (action, false)]);
                }
            });
        }
        E::Pads => {
            let shift_held = deck.screen_shift || ui.input(|i| i.modifiers.shift);
            let rows: &[std::ops::Range<usize>] = if compact { &[0..8] } else { &[0..4, 4..8] };
            ui.vertical(|ui| {
                for row in rows {
                    ui.horizontal(|ui| {
                        if compact {
                            // Eight pads in one row must fit a narrow four-deck column.
                            ui.spacing_mut().item_spacing.x = 4.0;
                        }
                        for slot in row.clone() {
                            let size = if compact { vec2(27.0, 22.0) } else { vec2(52.0, 30.0) };
                            let face = pad_face(deck, slot, shift_held, p.effects, engine, index);
                            let text = RichText::new(face.label)
                                .strong()
                                .size(if compact { 10.0 } else { 12.0 });
                            let button = egui::Button::new(text).fill(face.fill).min_size(size);
                            let response = ui
                                .add_enabled(deck.track.is_some() || deck.pad_mode == PadMode::Sampler, button)
                                .on_hover_text(face.hover);
                            let down = |button| ui.input(|i| i.pointer.button_down(button));
                            let primary =
                                response.is_pointer_button_down_on() && down(egui::PointerButton::Primary);
                            let secondary =
                                response.is_pointer_button_down_on() && down(egui::PointerButton::Secondary);
                            let held = &mut p.pad_held[slot];
                            match (*held, primary || secondary) {
                                (None, true) => {
                                    // Shift-click or right-click is Shift + pad.
                                    let shift = secondary || shift_held;
                                    if let Some(action) = deck.pad_mode.pad_action(slot as u8, shift) {
                                        p.out.pad_events.push((action, true));
                                        *held = Some(action);
                                    }
                                }
                                (Some(action), false) => {
                                    p.out.pad_events.push((action, false));
                                    *held = None;
                                }
                                _ => {}
                            }
                        }
                    });
                }
            });
        }
        E::TempoRange => {
            ui.vertical(|ui| {
                ui.label(RichText::new("TEMPO").small());
                let mut range = deck.tempo_range;
                egui::ComboBox::from_id_salt(("range", index))
                    .selected_text(range.label())
                    .width(64.0)
                    .show_ui(ui, |ui| {
                        for option in TempoRange::ALL {
                            ui.selectable_value(&mut range, option, option.label());
                        }
                    });
                if range != deck.tempo_range {
                    deck.set_tempo_range(range, engine);
                }
            });
        }
        E::MasterTempo => {
            if toggle_in(ui, deck.key_lock, "MT", RED)
                .on_hover_text("Master Tempo: keep the key when the tempo changes")
                .clicked()
            {
                deck.toggle_key_lock(engine);
            }
        }
        E::KeyShift => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui.small_button("−").on_hover_text("Key down a semitone").clicked() {
                    deck.set_key_shift(deck.key_shift - 1, engine);
                }
                if ui
                    .small_button(format!("{:+}", deck.key_shift))
                    .on_hover_text("Key shift in semitones; click to reset")
                    .clicked()
                {
                    deck.set_key_shift(0, engine);
                }
                if ui.small_button("+").on_hover_text("Key up a semitone").clicked() {
                    deck.set_key_shift(deck.key_shift + 1, engine);
                }
            });
        }
        E::KeySyncTap => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                p.out.key_sync = toggle(ui, deck.key_synced, "KEY")
                    .on_hover_text("Key sync: match the master deck's key")
                    .clicked();
                if ui.button("TAP").on_hover_text("Tap four or more times to set the BPM").clicked()
                    && let Some(bpm) = deck.tap_bpm()
                {
                    p.out.status = Some(format!("Deck {}: BPM set to {bpm:.2} by tapping", index + 1));
                }
            });
        }
        E::TempoFader => {
            // Like on a CDJ, pulling the fader down makes the track faster.
            let mut fader = -deck.tempo_fader;
            ui.spacing_mut().slider_width = if compact { 110.0 } else { 220.0 };
            let slider =
                ui.add_enabled(!deck.sync, Slider::new(&mut fader, -1.0..=1.0).vertical().show_value(false));
            if slider.changed() {
                deck.set_tempo_fader(-fader, engine);
            }
            slider.on_disabled_hover_text("Tempo follows the master while SYNC is on");
        }
        E::TempoReset => {
            if ui.button("Reset").on_hover_text("Tempo back to ±0").clicked() {
                deck.set_tempo_fader(0.0, engine);
            }
        }
        E::Nudge => {
            ui.horizontal(|ui| {
                if ui.button("−").on_hover_text("Nudge slower (hold)").is_pointer_button_down_on() {
                    deck.jog(-3, engine);
                }
                if ui.button("+").on_hover_text("Nudge faster (hold)").is_pointer_button_down_on() {
                    deck.jog(3, engine);
                }
            });
        }
    }
}
