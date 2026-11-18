//! EDIT: a deck's stems as clips under its track, like the Beat FX: each
//! clip is a stretch with parts turned off (or down). As in a DAW: click
//! picks a clip, double-click opens it (which parts play), drag moves it or
//! its edges, right-click removes it. Double-click or right-click where the
//! deck has stems adds one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;

use rofloxbox_audio::{PARTS, StemLayers, StemLevels, StemSpan, StemsSource, TrackSpan};
use rofloxbox_control::STEM_PARTS;
use eframe::egui::{self, Color32, Pos2, Rect, RichText, Stroke, Ui, pos2, vec2};

use super::{DECK_COLORS, RofLoxBoxApp, Edit, FxGrip, GRAB, LABELS, MixEditor, pack, value_tag};
use crate::app::deck_view::STEM_COLORS;

/// The height of a deck's stems row.
pub(super) const STEM_ROW: f32 = 46.0;
/// Every part as recorded.
const ALL: [f32; PARTS] = [1.0; PARTS];
/// The clip's name strip, above its parts.
const TITLE: f32 = 13.0;
/// What a new stem clip turns off at first: the vocals.
pub(super) const FIRST_STEMS: [f32; PARTS] = [0.0, 1.0, 1.0, 1.0];
/// Ready-made clips: name and levels.
const PRESETS: [(&str, [f32; PARTS]); 6] = [
    ("No vocals", [0.0, 1.0, 1.0, 1.0]),
    ("No drums", [1.0, 1.0, 1.0, 0.0]),
    ("No bass", [1.0, 1.0, 0.0, 1.0]),
    ("No bass, no drums", [1.0, 1.0, 0.0, 0.0]),
    ("Vocals only", [1.0, 0.0, 0.0, 0.0]),
    ("Drums only", [0.0, 0.0, 0.0, 1.0]),
];
/// How long a new clip is, in beats (8 bars), up to the next one.
const NEW_BEATS: f64 = 32.0;

/// A stretch of a deck with its parts not all as recorded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct StemBlock {
    pub from: u64,
    pub to: u64,
    pub levels: [f32; PARTS],
}

/// A stem clip being dragged by a part of it, and where it is now.
pub(super) struct StemDrag {
    deck: usize,
    index: usize,
    grip: FxGrip,
    /// Where the pointer took hold.
    at: u64,
    from: u64,
    to: u64,
}

/// A stem clip's dialog, open: the deck, a moment in the clip, and where
/// it was double-clicked.
#[derive(Clone, Copy)]
pub(super) struct StemOpen {
    deck: usize,
    at: u64,
    pos: Pos2,
}

/// What the stems row's right-click menu is for: adding a clip on a deck
/// at a moment.
#[derive(Clone, Copy)]
pub(super) enum StemMenu {
    Add(usize, u64),
}

fn all_on(levels: &[f32; PARTS]) -> bool {
    levels.iter().all(|l| (l - 1.0).abs() < 1e-4)
}

fn is_off(level: f32) -> bool {
    level < 1e-3
}

/// The stretches of `changes` (as [`rofloxbox_audio::Session::stem_levels`]
/// gives them) with a part not as recorded.
pub(super) fn stem_blocks(changes: &[StemLevels], length: u64) -> Vec<StemBlock> {
    let mut blocks = Vec::new();
    for (i, &(from, levels)) in changes.iter().enumerate() {
        let to = changes.get(i + 1).map_or(length, |c| c.0).min(length);
        if !all_on(&levels) && to > from {
            blocks.push(StemBlock { from, to, levels });
        }
    }
    blocks
}

fn levels_at(changes: &[StemLevels], at: u64) -> [f32; PARTS] {
    changes.iter().take_while(|c| c.0 <= at).last().map_or(ALL, |c| c.1)
}

/// `changes` with `from`–`to` set to `levels`; after it, as it was.
pub(super) fn paint(
    changes: &[StemLevels],
    from: u64,
    to: u64,
    levels: [f32; PARTS],
) -> Vec<StemLevels> {
    let after = levels_at(changes, to);
    let mut all: Vec<StemLevels> =
        changes.iter().copied().filter(|c| c.0 < from || c.0 > to).collect();
    all.extend([(from, levels), (to, after)]);
    all.sort_by_key(|c| c.0);
    // One change a moment (the last put there), and none that changes
    // nothing.
    let mut tidy: Vec<StemLevels> = Vec::with_capacity(all.len());
    for change in all {
        match tidy.last_mut() {
            Some(last) if last.0 == change.0 => last.1 = change.1,
            Some(last) if last.1 == change.1 => {}
            _ => tidy.push(change),
        }
    }
    if tidy.first().is_none_or(|c| c.0 > 0) {
        tidy.insert(0, (0, ALL));
    }
    tidy
}

/// "No VOCAL", "DRUMS only", "BASS 40%": what a clip does to the parts the
/// stems have.
pub(super) fn describe(levels: &[f32; PARTS], has: &[bool; PARTS]) -> String {
    let parts = (0..PARTS).filter(|&p| has[p]);
    let off: Vec<&str> = parts.clone().filter(|&p| is_off(levels[p])).map(|p| STEM_PARTS[p]).collect();
    let on: Vec<usize> = parts.clone().filter(|&p| !is_off(levels[p])).collect();
    let changed: Vec<String> = on
        .iter()
        .filter(|&&p| (levels[p] - 1.0).abs() >= 1e-3)
        .map(|&p| format!("{} {:.0}%", STEM_PARTS[p], levels[p] * 100.0))
        .collect();
    let mut text = match (off.len(), on.len()) {
        (0, _) => String::new(),
        (_, 1) if changed.is_empty() => return format!("{} only", STEM_PARTS[on[0]]),
        (_, 0) => "All off".to_owned(),
        _ => format!("No {}", off.join(", ")),
    };
    for part in changed {
        if !text.is_empty() {
            text.push_str(", ");
        }
        text.push_str(&part);
    }
    text
}

impl MixEditor {
    /// The stem spans of `deck` and where it has stems at `at`.
    fn stem_span_at(&self, deck: usize, at: u64) -> Option<StemSpan> {
        self.stem_spans.get(deck)?.iter().find(|s| s.from <= at && at < s.to).copied()
    }

    /// The master grid's beat near `at` on screen, if snapping is on.
    fn stem_snap(&self, ui: &Ui, area: Rect, at: u64) -> Option<u64> {
        if !self.snapping(ui) {
            return None;
        }
        let i = self.grid.partition_point(|g| g.at < at);
        [i.checked_sub(1), Some(i)]
            .into_iter()
            .flatten()
            .filter_map(|k| self.grid.get(k))
            .map(|g| g.at)
            .filter(|&g| (self.x_of(area, g) - self.x_of(area, at)).abs() <= GRAB + 1.0)
            .min_by_key(|&g| g.abs_diff(at))
    }

    /// A deck's stems under its track: where it has them, and the clips.
    pub(super) fn stems_row(&mut self, ui: &Ui, row: Rect, response: &egui::Response, deck: usize) {
        let visuals = ui.visuals().clone();
        let color = DECK_COLORS[deck % DECK_COLORS.len()];
        ui.painter().text(
            pos2(row.left() + 14.0, row.top() + 4.0),
            egui::Align2::LEFT_TOP,
            "Stems",
            egui::FontId::proportional(12.0),
            color,
        );
        let area = Rect::from_min_max(
            pos2(row.left() + LABELS, row.top() + 1.0),
            row.max - vec2(0.0, 1.0),
        );
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 2.0, visuals.extreme_bg_color);
        let spans = self.stem_spans.get(deck).cloned().unwrap_or_default();
        for span in &spans {
            let band = Rect::from_x_y_ranges(
                self.x_of(area, span.from)..=self.x_of(area, span.to),
                area.y_range(),
            );
            painter.rect_filled(band, 0.0, Color32::from_white_alpha(14));
        }
        let changes = self.stems.get(deck).cloned().unwrap_or_default();
        let blocks = stem_blocks(&changes, self.length);
        let has_at = |at: u64| {
            spans.iter().find(|s| s.from <= at && at < s.to).map_or([true; PARTS], |s| s.has)
        };
        let rects: Vec<Rect> = blocks
            .iter()
            .enumerate()
            .map(|(i, block)| {
                let (from, to) = match &self.stem_drag {
                    Some(drag) if drag.deck == deck && drag.index == i => (drag.from, drag.to),
                    _ => (block.from, block.to),
                };
                let left = self.x_of(area, from);
                Rect::from_x_y_ranges(
                    left..=self.x_of(area, to).max(left + 2.0),
                    area.top() + 2.0..=area.bottom() - 2.0,
                )
            })
            .collect();
        let chosen = self.chosen_stem.filter(|c| c.0 == deck).map(|c| c.1);
        for (block, rect) in blocks.iter().zip(&rects) {
            let has = has_at(block.from);
            painter.rect_filled(*rect, 3.0, color.gamma_multiply(0.2));
            let h = (rect.height() - TITLE) / PARTS as f32;
            for part in 0..PARTS {
                let strip = Rect::from_x_y_ranges(
                    rect.left() + 1.0..=rect.right() - 1.0,
                    rect.top() + TITLE + h * part as f32..=rect.top() + TITLE + h * (part + 1) as f32 - 1.0,
                );
                if !has[part] {
                    continue;
                }
                let level = block.levels[part].clamp(0.0, 1.0);
                if is_off(level) {
                    painter.rect_stroke(
                        strip.shrink(0.5),
                        1.0,
                        Stroke::new(1.0_f32, STEM_COLORS[part].gamma_multiply(0.35)),
                        egui::StrokeKind::Inside,
                    );
                } else {
                    painter.rect_filled(strip, 1.0, STEM_COLORS[part].gamma_multiply(0.25 + 0.6 * level));
                }
            }
            let picked = chosen.is_some_and(|at| block.from <= at && at < block.to);
            painter.rect_stroke(
                *rect,
                3.0,
                if picked { Stroke::new(1.5_f32, Color32::WHITE) } else { Stroke::new(1.0_f32, color) },
                egui::StrokeKind::Inside,
            );
            let clip = rect.intersect(area).shrink2(vec2(3.0, 0.0));
            let text = ui.painter().layout_no_wrap(
                describe(&block.levels, &has),
                egui::FontId::proportional(10.5),
                visuals.strong_text_color(),
            );
            ui.painter().with_clip_rect(clip).galley(pos2(clip.left(), rect.top()), text, Color32::WHITE);
        }
        self.play_marks(&painter, area);
        let pointer = response.interact_pointer_pos().or(response.hover_pos());
        let under = |pos: Pos2| rects.iter().position(|r| r.expand2(vec2(4.0, 0.0)).contains(pos));
        let edge_at = |pos: Pos2| {
            rects.iter().enumerate().find_map(|(i, r)| {
                if !r.expand2(vec2(GRAB, 0.0)).contains(pos) || r.width() < 3.0 * GRAB {
                    None
                } else if (pos.x - r.left()).abs() <= GRAB {
                    Some((i, false))
                } else if (pos.x - r.right()).abs() <= GRAB {
                    Some((i, true))
                } else {
                    None
                }
            })
        };
        // What the pointer is on: the clip, and the part under it.
        if let Some(pos) = response.hover_pos()
            && self.stem_drag.is_none()
        {
            match under(pos) {
                Some(i) => {
                    let block = &blocks[i];
                    let text = format!(
                        "{} · double-click: edit · right-click: remove",
                        describe(&block.levels, &has_at(block.from))
                    );
                    value_tag(&painter, area, pos, &text);
                }
                None if self.stem_span_at(deck, self.at_of(area, pos.x)).is_some() => {
                    value_tag(&painter, area, pos, "Double-click: a stem clip");
                }
                None => value_tag(&painter, area, pos, "No stems here"),
            }
        }
        if self.busy() || self.panning {
            return;
        }
        let dragging_edge = self
            .stem_drag
            .as_ref()
            .is_some_and(|d| d.deck == deck && !matches!(d.grip, FxGrip::Whole));
        if response.hover_pos().and_then(edge_at).is_some() || dragging_edge {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        // A click picks a clip (Delete removes it); a double-click opens it.
        if response.clicked()
            && let Some(pos) = pointer
        {
            self.clear_selections();
            if let Some(i) = under(pos) {
                self.chosen_stem = Some((deck, blocks[i].from));
            }
        }
        if response.double_clicked()
            && let Some(pos) = pointer
        {
            match under(pos) {
                Some(i) => {
                    let at = blocks[i].from;
                    self.chosen_stem = Some((deck, at));
                    self.stem_open = Some(StemOpen { deck, at, pos });
                }
                None => {
                    let at = self.at_of(area, pos.x);
                    let at = self.stem_snap(ui, area, at).unwrap_or(at);
                    self.add_stems(deck, at, self.last_stems);
                }
            }
        }
        // Right-click on a clip removes it, as in a DAW; off the clips, a
        // menu of clips to add.
        if response.secondary_clicked()
            && let Some(pos) = pointer
        {
            self.clear_selections();
            match under(pos) {
                Some(i) => {
                    let block = blocks[i];
                    self.set_stems(deck, paint(&changes, block.from, block.to, ALL), "remove");
                    if self.stem_open.is_some_and(|o| o.deck == deck && block.from <= o.at && o.at < block.to) {
                        self.stem_open = None;
                    }
                    self.stem_menu = None;
                }
                None => {
                    let at = self.at_of(area, pos.x);
                    let at = self.stem_snap(ui, area, at).unwrap_or(at);
                    self.stem_menu = self.stem_span_at(deck, at).map(|_| StemMenu::Add(deck, at));
                }
            }
        }
        if let Some(StemMenu::Add(d, at)) = self.stem_menu
            && d == deck
        {
            response.context_menu(|ui| self.stem_add_menu(ui, deck, at, &spans));
        }
        // Dragging a clip, or stretching it by an edge.
        let pressed = ui.input(|i| i.pointer.press_origin()).or(pointer);
        if response.drag_started()
            && let Some(pos) = pressed
            && let Some(i) = under(pos)
        {
            let grip = match edge_at(pos) {
                Some((k, false)) if k == i => FxGrip::Start,
                Some((k, true)) if k == i => FxGrip::End,
                _ => FxGrip::Whole,
            };
            self.clear_selections();
            let block = blocks[i];
            self.chosen_stem = Some((deck, block.from));
            self.stem_drag = Some(StemDrag {
                deck,
                index: i,
                grip,
                at: self.at_of(area, pos.x),
                from: block.from,
                to: block.to,
            });
        }
        let held = self.stem_drag.as_ref().filter(|d| d.deck == deck).map(|d| (d.index, d.at));
        if let Some((index, grabbed)) = held
            && let Some(pos) = pointer
            && let Some(block) = blocks.get(index).copied()
        {
            let at = self.at_of(area, pos.x);
            // Inside the stems it is in, and clear of the clips either side.
            let span = self.stem_span_at(deck, block.from);
            let low = index
                .checked_sub(1)
                .map_or(0, |k| blocks[k].to)
                .max(span.map_or(0, |s| s.from));
            let high = blocks
                .get(index + 1)
                .map_or(self.length, |b| b.from)
                .min(span.map_or(self.length, |s| s.to));
            let length = block.to - block.from;
            let mut snapped = None;
            let Some(grip) = self.stem_drag.as_ref().map(|d| d.grip) else { return };
            let (from, to) = match grip {
                FxGrip::Whole => {
                    let from = (block.from as i64 + at as i64 - grabbed as i64).max(0) as u64;
                    let from = match self.stem_snap(ui, area, from) {
                        Some(g) => {
                            snapped = Some(g);
                            g
                        }
                        None => from,
                    };
                    let from = from.max(low).min(high.saturating_sub(length));
                    (from, from + length)
                }
                FxGrip::Start => {
                    snapped = self.stem_snap(ui, area, at);
                    let high = block.to.saturating_sub(1).max(low);
                    (snapped.unwrap_or(at).clamp(low, high), block.to)
                }
                FxGrip::End => {
                    snapped = self.stem_snap(ui, area, at);
                    let low = block.from + 1;
                    (block.from, snapped.unwrap_or(at).clamp(low, high.max(low)))
                }
            };
            self.snap_line = snapped;
            if let Some(drag) = self.stem_drag.as_mut() {
                (drag.from, drag.to) = (from, to);
            }
        }
        if response.drag_stopped()
            && let Some(drag) = self.stem_drag.take_if(|d| d.deck == deck)
            && let Some(block) = blocks.get(drag.index)
            && (drag.from, drag.to) != (block.from, block.to)
        {
            let cleared = paint(&changes, block.from, block.to, ALL);
            let moved = paint(&cleared, drag.from, drag.to, block.levels);
            let what = if matches!(drag.grip, FxGrip::Whole) { "move" } else { "resize" };
            self.set_stems(deck, moved, what);
            self.chosen_stem = Some((deck, drag.from));
        }
    }

    /// The right-click menu off the stem clips: clips to add there.
    fn stem_add_menu(&mut self, ui: &mut Ui, deck: usize, at: u64, spans: &[StemSpan]) {
        let has = spans.iter().find(|s| s.from <= at && at < s.to).map_or([true; PARTS], |s| s.has);
        ui.label(RichText::new("Add a stem clip").small().weak());
        for (name, levels) in PRESETS {
            // Only those that turn off parts these stems have.
            if (0..PARTS).any(|p| is_off(levels[p]) && !has[p]) {
                continue;
            }
            if ui.button(name).clicked() {
                self.add_stems(deck, at, levels);
                ui.close();
            }
        }
    }

    /// The open stem clip's dialog: which parts play, ready-made sets,
    /// Remove.
    pub(super) fn stem_dialog(&mut self, ctx: &egui::Context) {
        let Some(open) = self.stem_open else { return };
        let changes = self.stems.get(open.deck).cloned().unwrap_or_default();
        let Some(block) =
            stem_blocks(&changes, self.length).into_iter().find(|b| b.from <= open.at && open.at < b.to)
        else {
            self.stem_open = None;
            return;
        };
        let has = self.stem_span_at(open.deck, block.from).map_or([true; PARTS], |s| s.has);
        let time = |at: u64| {
            let secs = at as f64 / self.rate;
            format!("{}:{:05.2}", (secs / 60.0) as u64, secs % 60.0)
        };
        let mut keep = !ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut levels = block.levels;
        let mut remove = false;
        egui::Window::new(format!("Deck {} stem clip", open.deck + 1))
            .id(egui::Id::new("stem clip"))
            .collapsible(false)
            .resizable(false)
            .open(&mut keep)
            .default_pos(open.pos + vec2(8.0, 8.0))
            .show(ctx, |ui| {
                ui.label(RichText::new(format!("{} – {}", time(block.from), time(block.to))).weak());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for part in (0..PARTS).filter(|&p| has[p]) {
                        let on = !is_off(levels[part]);
                        let text = RichText::new(STEM_PARTS[part]).strong().color(if on {
                            Color32::BLACK
                        } else {
                            STEM_COLORS[part]
                        });
                        let button = egui::Button::new(text).fill(if on {
                            STEM_COLORS[part].gamma_multiply(0.85)
                        } else {
                            Color32::from_gray(30)
                        });
                        // The last part off stays off: all on would be no
                        // clip (right-click or Remove takes it away).
                        let last_off = !on && (0..PARTS).filter(|&p| has[p] && is_off(levels[p])).count() == 1;
                        let hover = if last_off {
                            "A stem clip turns at least one part off; Remove takes it away"
                        } else {
                            "Plays in this clip when lit"
                        };
                        if ui.add_enabled(!last_off, button).on_hover_text(hover).on_disabled_hover_text(hover).clicked() {
                            levels[part] = if on { 0.0 } else { 1.0 };
                        }
                    }
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    for (name, preset) in PRESETS {
                        if (0..PARTS).any(|p| is_off(preset[p]) && !has[p]) {
                            continue;
                        }
                        if ui.small_button(name).clicked() {
                            levels = preset;
                        }
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Remove").clicked() {
                        remove = true;
                    }
                    if ui.button("Close").clicked() {
                        self.stem_open = None;
                    }
                });
            });
        if remove {
            self.set_stems(open.deck, paint(&changes, block.from, block.to, ALL), "remove");
            (self.stem_open, self.chosen_stem) = (None, None);
        } else if levels != block.levels {
            let what = describe(&levels, &has);
            self.set_stems(open.deck, paint(&changes, block.from, block.to, levels), &what);
            if !all_on(&levels) {
                self.last_stems = levels;
            } else {
                // Every part back on: the clip is gone.
                (self.stem_open, self.chosen_stem) = (None, None);
            }
        }
        if !keep {
            self.stem_open = None;
        }
    }

    /// A new stem clip from `at` for 8 bars of the deck playing there (or
    /// up to the next clip, or the end of the stems).
    fn add_stems(&mut self, deck: usize, at: u64, levels: [f32; PARTS]) {
        let at = self.session.lock().map_or(at, |s| s.snap(at));
        let Some(span) = self.stem_span_at(deck, at) else { return };
        let changes = self.stems.get(deck).cloned().unwrap_or_default();
        let blocks = stem_blocks(&changes, self.length);
        if blocks.iter().any(|b| b.from <= at && at < b.to) {
            return;
        }
        let beat = self.beat_len(at).unwrap_or(0.5);
        let next = blocks.iter().map(|b| b.from).filter(|&from| from > at).min();
        let to = (at + (beat * NEW_BEATS * self.rate) as u64)
            .min(next.unwrap_or(u64::MAX))
            .min(span.to);
        if to <= at {
            return;
        }
        self.last_stems = levels;
        let has = span.has;
        self.set_stems(deck, paint(&changes, at, to, levels), &format!("add {}", describe(&levels, &has)));
        self.chosen_stem = Some((deck, at));
    }

    /// Hands a deck's new stem levels to the session, keeping the old for
    /// undo.
    pub(super) fn set_stems(&mut self, deck: usize, changes: Vec<StemLevels>, what: &str) {
        let Ok(mut session) = self.session.lock() else { return };
        session.set_stem_levels(deck, &changes);
        let now = session.stem_levels(deck);
        drop(session);
        if self.stems.get(deck) != Some(&now) {
            let before = std::mem::replace(&mut self.stems[deck], now);
            self.record(format!("Deck {} stems: {what}", deck + 1), Edit::Stems(deck, before));
        }
    }

    /// Delete with a stem clip chosen: it goes. Whether there was one.
    pub(super) fn remove_chosen_stems(&mut self) -> bool {
        let Some((deck, at)) = self.chosen_stem.take() else { return false };
        let changes = self.stems.get(deck).cloned().unwrap_or_default();
        let Some(block) = stem_blocks(&changes, self.length).into_iter().find(|b| b.from <= at && at < b.to)
        else {
            return false;
        };
        self.set_stems(deck, paint(&changes, block.from, block.to, ALL), "remove");
        true
    }
}

/// A track being separated into stems for its clips (asked for from
/// `deck`'s row): the job, the track, the model's stems, the stems as they
/// come in, and them being saved with the recording.
pub(super) struct Separating {
    deck: usize,
    job: usize,
    path: PathBuf,
    model: Vec<String>,
    stems: Option<Arc<StemLayers>>,
    saving: Option<Receiver<anyhow::Result<StemsSource>>>,
    /// Steers the separator; cancelled when this goes (the session closed).
    playhead: Arc<AtomicU64>,
}

impl Drop for Separating {
    fn drop(&mut self) {
        self.playhead.store(crate::stems::CANCELLED, Ordering::Relaxed);
    }
}

impl Separating {
    /// How far along, 0–1.
    fn done(&self) -> f32 {
        self.stems.as_ref().map_or(0.0, |s| s.ready_share())
    }
}

/// A folder of its own for a track's stems in the recording's `stems/`.
fn stems_folder(recording: &Path, track: &Path) -> PathBuf {
    let name = track.file_stem().map_or_else(|| "track".into(), |n| n.to_string_lossy().into_owned());
    let stems = recording.join("stems");
    (1..)
        .map(|n| if n == 1 { stems.join(&name) } else { stems.join(format!("{name} {n}")) })
        .find(|dir| !dir.exists())
        .unwrap_or_else(|| stems.join(&name))
}

impl MixEditor {
    /// `deck`'s clips that are heard but have no stems.
    pub(super) fn clips_without_stems(&self, deck: usize) -> Vec<TrackSpan> {
        let spans = self.stem_spans.get(deck).map_or(&[][..], Vec::as_slice);
        self.tracks
            .iter()
            .filter(|t| t.deck == deck && !t.path.as_os_str().is_empty() && !t.playing.is_empty())
            .filter(|t| !spans.iter().any(|s| s.from < t.to && t.from < s.to))
            .cloned()
            .collect()
    }

    /// Under a deck's name: Separate stems for its clips without them, or
    /// how far that has got.
    pub(super) fn stems_button(&mut self, ui: &mut Ui, row: Rect, deck: usize) {
        let rect = Rect::from_min_size(pos2(row.left() + 4.0, row.bottom() - 22.0), vec2(LABELS - 14.0, 18.0));
        let running: Vec<f32> =
            self.separating.iter().filter(|s| s.deck == deck).map(Separating::done).collect();
        if !running.is_empty() {
            let share = running.iter().sum::<f32>() / running.len() as f32;
            let text = match self.separating.iter().any(|s| s.deck == deck && s.saving.is_some()) {
                true => "Saving stems…".to_owned(),
                false => format!("Stems {:.0}%", share * 100.0),
            };
            ui.put(rect, egui::Label::new(RichText::new(text).small().weak()))
                .on_hover_text("Separating; the parts can be turned off once it is done");
            return;
        }
        let missing = self.clips_without_stems(deck);
        if missing.is_empty() {
            return;
        }
        let button = egui::Button::new(RichText::new("Separate stems").small());
        let hover = format!(
            "Separate {} into vocals, instruments, bass and drums (the model in Preferences → Stems), kept with the recording",
            if missing.len() == 1 { "its track".to_owned() } else { format!("its {} tracks", missing.len()) }
        );
        if ui.put(rect, button).on_hover_text(hover).clicked() && !self.stems_wanted.contains(&deck) {
            self.stems_wanted.push(deck);
        }
    }

    /// Hands finished stems to every clip of their track without stems,
    /// keeping the old for undo.
    fn take_stems(&mut self, path: &Path, stems: &Arc<StemLayers>, deck: usize) {
        let shared = self.session.clone();
        let Ok(mut session) = shared.lock() else { return };
        let Some(before) = pack(&session) else { return };
        let spans: Vec<TrackSpan> = (0..rofloxbox_audio::DECKS)
            .flat_map(|d| self.clips_without_stems(d))
            .filter(|t| t.path == path)
            .collect();
        if spans.is_empty() {
            return;
        }
        for span in &spans {
            session.add_stems(span, stems.clone());
        }
        self.refresh(&session);
        drop(session);
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        self.record(format!("Deck {}: stems for {name}", deck + 1), Edit::Events(before));
    }
}

impl RofLoxBoxApp {
    /// EDIT's stem separations: starts those asked for, takes in what the
    /// separator sends, and saves finished stems with the recording before
    /// the clips get them.
    pub(super) fn poll_stem_jobs(&mut self, ctx: &egui::Context) {
        let Some(editor) = self.edit_view.editor.as_mut() else { return };
        let wanted = std::mem::take(&mut editor.stems_wanted);
        if !wanted.is_empty() {
            match self.stem_model() {
                None => {
                    self.status = Some("No stem model: pick one in Preferences → Stems".into());
                }
                Some(model) => {
                    let separator = self.edit_view.separator.get_or_insert_with(Default::default);
                    let Some(editor) = self.edit_view.editor.as_mut() else { return };
                    for deck in wanted {
                        for span in editor.clips_without_stems(deck) {
                            if editor.separating.iter().any(|s| s.path == span.path) {
                                continue;
                            }
                            let audio = editor.session.lock().ok().and_then(|s| s.track_audio(&span.path));
                            let Some(audio) = audio else { continue };
                            let job = self.edit_view.next_job;
                            self.edit_view.next_job += 1;
                            let playhead = Arc::new(AtomicU64::new(0f64.to_bits()));
                            separator.submit(crate::stems::LiveJob {
                                deck: job,
                                title: span.path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
                                input: span.path.clone(),
                                model: model.clone(),
                                audio,
                                playhead: playhead.clone(),
                            });
                            editor.separating.push(Separating {
                                deck,
                                job,
                                path: span.path.clone(),
                                model: model.stems.clone(),
                                stems: None,
                                saving: None,
                                playhead,
                            });
                        }
                    }
                }
            }
        }
        let Some(editor) = self.edit_view.editor.as_mut() else { return };
        if let Some(separator) = &self.edit_view.separator {
            while let Ok(event) = separator.events.try_recv() {
                match event {
                    crate::stems::Event::Live { deck: job, stems, .. } => {
                        if let Some(s) = editor.separating.iter_mut().find(|s| s.job == job) {
                            s.stems = Some(stems);
                        }
                    }
                    crate::stems::Event::Failed { title, error, deck: Some(job) } => {
                        editor.separating.retain(|s| s.job != job);
                        self.status = Some(format!("Couldn't separate {title}: {error}"));
                    }
                    _ => {}
                }
            }
        }
        if editor.separating.is_empty() {
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        let recording = editor.path.parent().map(Path::to_path_buf).unwrap_or_default();
        // Done: saved with the recording (a thread: a few hundred MB).
        for s in editor.separating.iter_mut().filter(|s| s.saving.is_none() && s.done() >= 1.0) {
            let Some(stems) = s.stems.clone() else { continue };
            let rate = editor.session.lock().ok().and_then(|x| x.track_audio(&s.path)).map_or(44_100, |a| a.sample_rate);
            let source = StemsSource {
                track: s.path.clone(),
                dir: stems_folder(&recording, &s.path),
                stems: s.model.clone(),
            };
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(stems.save(&source.dir, rate).map(|()| source));
            });
            s.saving = Some(rx);
        }
        let mut finished = Vec::new();
        editor.separating.retain(|s| {
            match s.saving.as_ref().map(Receiver::try_recv) {
                Some(Ok(result)) => {
                    finished.push((s.path.clone(), s.stems.clone(), s.deck, result));
                    false
                }
                Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => false,
                _ => true,
            }
        });
        for (path, stems, deck, result) in finished {
            match (result, stems) {
                (Ok(source), Some(stems)) => {
                    // The session names them by the recording's copy now.
                    rofloxbox_audio::remember_stems(&stems, source);
                    editor.take_stems(&path, &stems, deck);
                    self.status = Some(format!(
                        "Stems ready for {}",
                        path.file_stem().unwrap_or_default().to_string_lossy()
                    ));
                }
                (Err(err), _) => self.status = Some(format!("Couldn't keep the stems: {err:#}")),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rofloxbox_audio::Session;
    use std::path::PathBuf;

    /// Clicks on deck 1's stems row (as wide as the window, labels and
    /// all), `times` in a row with `button`; the clip dialog too.
    fn click_row(editor: &mut MixEditor, x: f32, button: egui::PointerButton, times: usize) {
        let ctx = egui::Context::default();
        let row = Rect::from_min_size(pos2(0.0, 100.0), vec2(1200.0, STEM_ROW));
        let pos = pos2(x, row.center().y);
        let press = |pressed| egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        // A few frames first: a new context needs the row laid out before
        // a press can land on it.
        let mut steps = vec![Vec::new(), Vec::new(), vec![egui::Event::PointerMoved(pos)], Vec::new()];
        for _ in 0..times {
            steps.push(vec![press(true)]);
            steps.push(vec![press(false)]);
        }
        steps.push(Vec::new());
        for (i, events) in steps.into_iter().enumerate() {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
                time: Some(i as f64 * 0.05),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::Area::new(egui::Id::new("row")).fixed_pos(Pos2::ZERO).show(ctx, |ui| {
                    let response = ui.allocate_rect(row, egui::Sense::click_and_drag());
                    editor.stems_row(ui, row, &response, 0);
                });
                editor.stem_dialog(ctx);
            });
        }
    }

    /// On `ROFLOXBOX_SESSION`, a set whose deck 1 had stems: a clip added,
    /// then clicked (picked, unchanged), double-clicked (opened,
    /// unchanged) and right-clicked (gone):
    /// `cargo test -p rofloxbox-app clicking_stem_clips -- --ignored`
    #[test]
    #[ignore]
    fn clicking_stem_clips() {
        let path = PathBuf::from(std::env::var("ROFLOXBOX_SESSION").unwrap());
        let session = Session::read(&path, crate::app::recording::Sources).unwrap();
        let mut editor = MixEditor::new(path, session);
        let span = editor.stem_spans[0][0];
        let before = editor.stems[0].clone();
        let at = span.from + (span.to - span.from) / 4;
        editor.add_stems(0, at, FIRST_STEMS);
        let block = *stem_blocks(&editor.stems[0], editor.length)
            .iter()
            .find(|b| b.from <= at + 4096 && at < b.to)
            .expect("the new clip");
        let added = editor.stems[0].clone();
        let area = Rect::from_min_max(pos2(LABELS, 0.0), pos2(1200.0, 10.0));
        let x = (editor.x_of(area, block.from) + editor.x_of(area, block.to)) / 2.0;
        click_row(&mut editor, x, egui::PointerButton::Primary, 1);
        assert_eq!(editor.stems[0], added, "a click only picks it");
        assert!(editor.chosen_stem.is_some());
        click_row(&mut editor, x, egui::PointerButton::Primary, 2);
        assert_eq!(editor.stems[0], added, "a double-click only opens it");
        assert!(editor.stem_open.is_some());
        click_row(&mut editor, x, egui::PointerButton::Secondary, 1);
        // (Clips already there are only moved to buffer starts.)
        let clips = |c: &[StemLevels]| stem_blocks(c, editor.length).len();
        assert_eq!(clips(&editor.stems[0]), clips(&before), "a right-click removes it");
        assert!(editor.stem_open.is_none());
        let labels: Vec<_> = editor.undo.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Deck 1 stems: add No VOCAL", "Deck 1 stems: remove"]);
    }

    #[test]
    fn painting_stem_clips() {
        let none = vec![(0, ALL)];
        let vocals_off = [0.0, 1.0, 1.0, 1.0];
        let one = paint(&none, 100, 200, vocals_off);
        assert_eq!(one, [(0, ALL), (100, vocals_off), (200, ALL)]);
        assert_eq!(stem_blocks(&one, 1000), [StemBlock { from: 100, to: 200, levels: vocals_off }]);
        // Moved: cleared where it was, put where it goes.
        let moved = paint(&paint(&one, 100, 200, ALL), 300, 400, vocals_off);
        assert_eq!(moved, [(0, ALL), (300, vocals_off), (400, ALL)]);
        // Another right after keeps its own levels; the first's end stays.
        let drums_off = [1.0, 1.0, 1.0, 0.0];
        let two = paint(&one, 200, 250, drums_off);
        assert_eq!(two, [(0, ALL), (100, vocals_off), (200, drums_off), (250, ALL)]);
        assert_eq!(stem_blocks(&two, 1000).len(), 2);
        // A clip running to the end of the set.
        let open = paint(&none, 900, 1000, vocals_off);
        assert_eq!(stem_blocks(&open, 1000), [StemBlock { from: 900, to: 1000, levels: vocals_off }]);
    }

    #[test]
    fn clip_names() {
        let has = [true, true, true, true];
        assert_eq!(describe(&[0.0, 1.0, 1.0, 1.0], &has), "No VOCAL");
        assert_eq!(describe(&[0.0, 0.0, 0.0, 1.0], &has), "DRUMS only");
        assert_eq!(describe(&[1.0, 1.0, 0.0, 0.0], &has), "No BASS, DRUMS");
        assert_eq!(describe(&[0.5, 1.0, 1.0, 1.0], &has), "VOCAL 50%");
        // A model without bass: its level isn't named.
        assert_eq!(describe(&[1.0, 1.0, 0.0, 0.0], &[true, true, false, true]), "No DRUMS");
    }
}
