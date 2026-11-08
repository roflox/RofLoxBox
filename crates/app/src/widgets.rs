//! Mixer controls in the style of a DJ mixer: rotary knobs, faders with a
//! cap, segmented level meters.

use std::f32::consts::PI;
use std::ops::RangeInclusive;

use eframe::egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Rect, Response, Sense, Shape, Stroke,
    StrokeKind, Ui, Vec2, pos2, vec2,
};

const BODY: Color32 = Color32::from_rgb(34, 34, 38);
const RIM: Color32 = Color32::from_rgb(70, 70, 76);
const TRACK: Color32 = Color32::from_rgb(55, 55, 60);
const ARC: Color32 = Color32::from_rgb(90, 170, 255);
/// Knobs turn through 270°, from lower left to lower right.
const SWEEP: f32 = 1.5 * PI;
const START: f32 = 0.75 * PI;

/// A rotary knob. Drag up/down to turn (hold Shift for fine control);
/// double-click returns to `default`. With `bipolar`, the value arc starts
/// at the center (EQ, filter) instead of the minimum.
pub fn knob(
    ui: &mut Ui,
    value: &mut f32,
    range: RangeInclusive<f32>,
    default: f32,
    label: &str,
    bipolar: bool,
    diameter: f32,
) -> Response {
    let size = vec2(diameter + 6.0, diameter + 14.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let (min, max) = (*range.start(), *range.end());
    if response.dragged() {
        let fine = ui.input(|i| i.modifiers.shift);
        let per_pixel = (max - min) / if fine { 600.0 } else { 150.0 };
        let new = (*value - response.drag_delta().y * per_pixel).clamp(min, max);
        if new != *value {
            *value = new;
            response.mark_changed();
        }
    }
    if response.double_clicked() && *value != default {
        *value = default;
        response.mark_changed();
    }
    let response = response.on_hover_cursor(CursorIcon::ResizeVertical);

    let painter = ui.painter_at(rect.expand(2.0));
    let center = pos2(rect.center().x, rect.top() + diameter / 2.0 + 2.0);
    let radius = diameter / 2.0;
    let angle_of = |v: f32| START + (v - min) / (max - min) * SWEEP;
    let arc = |from: f32, to: f32, r: f32, stroke: Stroke| {
        let steps = ((to - from).abs() / 0.08).ceil().max(2.0) as usize;
        let points: Vec<_> = (0..=steps)
            .map(|i| {
                let a = from + (to - from) * i as f32 / steps as f32;
                center + vec2(a.cos(), a.sin()) * r
            })
            .collect();
        painter.add(Shape::line(points, stroke));
    };

    arc(START, START + SWEEP, radius + 2.5, Stroke::new(2.0_f32, TRACK));
    let origin = if bipolar { angle_of((min + max) / 2.0) } else { START };
    let current = angle_of(*value);
    if (current - origin).abs() > 0.01 {
        arc(origin.min(current), origin.max(current), radius + 2.5, Stroke::new(2.5_f32, ARC));
    }
    painter.circle(center, radius - 1.0, BODY, Stroke::new(1.5_f32, RIM));
    let tip = center + vec2(current.cos(), current.sin()) * (radius - 3.0);
    painter.line_segment(
        [center + vec2(current.cos(), current.sin()) * (radius * 0.25), tip],
        Stroke::new(2.5_f32, Color32::WHITE),
    );
    painter.text(
        pos2(rect.center().x, rect.bottom() - 1.0),
        Align2::CENTER_BOTTOM,
        label,
        FontId::proportional(9.5),
        ui.visuals().text_color(),
    );
    response
}

/// A vertical fader with a cap, 0.0 (bottom) … 1.0 (top).
pub fn fader(ui: &mut Ui, value: &mut f32, height: f32) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(26.0, height), Sense::click_and_drag());
    let cap = vec2(24.0, 12.0);
    let travel = rect.height() - cap.y;
    if let Some(pointer) = response.interact_pointer_pos()
        && (response.dragged() || response.clicked())
    {
        let new = (1.0 - (pointer.y - rect.top() - cap.y / 2.0) / travel).clamp(0.0, 1.0);
        if new != *value {
            *value = new;
            response.mark_changed();
        }
    }
    let painter = ui.painter_at(rect.expand(1.0));
    let slot = Rect::from_center_size(rect.center(), vec2(4.0, travel));
    painter.rect_filled(slot, 2.0, Color32::from_gray(12));
    for i in 0..=10 {
        let y = slot.top() + slot.height() * i as f32 / 10.0;
        painter.line_segment(
            [pos2(rect.left(), y), pos2(rect.left() + 5.0, y)],
            Stroke::new(1.0_f32, Color32::from_gray(70)),
        );
    }
    let y = rect.top() + (1.0 - *value) * travel;
    let cap_rect = Rect::from_min_size(pos2(rect.center().x - cap.x / 2.0, y), cap);
    painter.rect_filled(cap_rect, CornerRadius::same(2), Color32::from_gray(200));
    painter.rect_stroke(
        cap_rect,
        CornerRadius::same(2),
        Stroke::new(1.0_f32, Color32::from_gray(90)),
        StrokeKind::Inside,
    );
    painter.line_segment(
        [
            pos2(cap_rect.left() + 2.0, cap_rect.center().y),
            pos2(cap_rect.right() - 2.0, cap_rect.center().y),
        ],
        Stroke::new(1.5_f32, Color32::from_gray(40)),
    );
    response.on_hover_cursor(CursorIcon::ResizeVertical)
}

/// A horizontal crossfader, −1.0 (left) … 1.0 (right). Double-click centers it.
pub fn crossfader(ui: &mut Ui, value: &mut f32, width: f32) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(width, 26.0), Sense::click_and_drag());
    let cap = vec2(14.0, 24.0);
    let travel = rect.width() - cap.x;
    if let Some(pointer) = response.interact_pointer_pos()
        && response.dragged()
    {
        let new = ((pointer.x - rect.left() - cap.x / 2.0) / travel * 2.0 - 1.0).clamp(-1.0, 1.0);
        if new != *value {
            *value = new;
            response.mark_changed();
        }
    }
    if response.double_clicked() && *value != 0.0 {
        *value = 0.0;
        response.mark_changed();
    }
    let painter = ui.painter_at(rect.expand(1.0));
    let slot = Rect::from_center_size(rect.center(), vec2(travel, 4.0));
    painter.rect_filled(slot, 2.0, Color32::from_gray(12));
    painter.line_segment(
        [pos2(rect.center().x, rect.top()), pos2(rect.center().x, rect.top() + 4.0)],
        Stroke::new(1.0_f32, Color32::from_gray(90)),
    );
    let x = rect.left() + (*value + 1.0) / 2.0 * travel;
    let cap_rect = Rect::from_min_size(pos2(x, rect.center().y - cap.y / 2.0), cap);
    painter.rect_filled(cap_rect, CornerRadius::same(2), Color32::from_gray(200));
    painter.line_segment(
        [
            pos2(cap_rect.center().x, cap_rect.top() + 2.0),
            pos2(cap_rect.center().x, cap_rect.bottom() - 2.0),
        ],
        Stroke::new(1.5_f32, Color32::from_gray(40)),
    );
    response.on_hover_cursor(CursorIcon::ResizeHorizontal)
}

/// A segmented level meter (green, yellow, red), `level` 0.0–1.0+.
pub fn meter(ui: &mut Ui, level: f32, size: Vec2) {
    const SEGMENTS: usize = 15;
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let gap = 1.5;
    let h = (rect.height() - gap * (SEGMENTS as f32 - 1.0)) / SEGMENTS as f32;
    let lit = (level.clamp(0.0, 1.0) * SEGMENTS as f32).round() as usize;
    for i in 0..SEGMENTS {
        let bottom = rect.bottom() - i as f32 * (h + gap);
        let segment = Rect::from_min_max(pos2(rect.left(), bottom - h), pos2(rect.right(), bottom));
        let color = match i {
            i if i >= SEGMENTS - 2 => Color32::from_rgb(235, 50, 40),
            i if i >= SEGMENTS - 5 => Color32::from_rgb(240, 200, 40),
            _ => Color32::from_rgb(60, 210, 90),
        };
        painter.rect_filled(segment, 1.0, if i < lit { color } else { color.gamma_multiply(0.15) });
    }
}

/// A rekordbox-style button: flat dark body with a thin outline; when `on`,
/// outline and text light up in `accent`.
pub fn lit_button(ui: &mut Ui, label: &str, on: bool, accent: Color32) -> Response {
    lit_button_sized(ui, label, on, accent, 12.0, 10.0)
}

/// [`lit_button`] with a given text size and horizontal padding, for rows of
/// many small buttons.
pub fn lit_button_sized(
    ui: &mut Ui,
    label: &str,
    on: bool,
    accent: Color32,
    text_size: f32,
    padding: f32,
) -> Response {
    let font = FontId::proportional(text_size);
    let width =
        ui.painter().layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE).size().x;
    let height = text_size + 8.0;
    let (rect, response) =
        ui.allocate_exact_size(vec2((width + padding).max(height), height), Sense::click());
    if ui.is_rect_visible(rect) {
        let enabled = ui.is_enabled();
        let hovered = response.hovered() && enabled;
        let pressed = response.is_pointer_button_down_on() && enabled;
        let visuals = ui.visuals();
        let state = match (pressed, hovered) {
            (true, _) => &visuals.widgets.active,
            (false, true) => &visuals.widgets.hovered,
            _ => &visuals.widgets.inactive,
        };
        let fill = if on { accent.gamma_multiply(0.25) } else { state.bg_fill };
        let outline = if on { accent } else { state.bg_stroke.color };
        let text = if !enabled {
            visuals.weak_text_color()
        } else if on {
            accent.lerp_to_gamma(visuals.strong_text_color(), 0.35)
        } else {
            visuals.text_color()
        };
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(3),
            fill,
            Stroke::new(1.0_f32, outline),
            StrokeKind::Inside,
        );
        painter.text(rect.center(), Align2::CENTER_CENTER, label, font, text);
    }
    response
}

/// What a round transport button shows.
#[derive(Clone, Copy)]
pub enum RoundIcon {
    Text(&'static str),
    Play,
    Pause,
}

/// The round CUE / PLAY buttons of a CDJ: a coloured ring that fills when lit.
/// Senses drags too: egui stops treating a press held longer than 0.8 s as a
/// click, and a held CUE must keep counting as held.
pub fn round_button(
    ui: &mut Ui,
    icon: RoundIcon,
    accent: Color32,
    lit: bool,
    diameter: f32,
) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(vec2(diameter, diameter), Sense::click_and_drag());
    if ui.is_rect_visible(rect) {
        let enabled = ui.is_enabled();
        let hovered = response.hovered() && enabled;
        let center = rect.center();
        let radius = diameter / 2.0 - 1.5;
        let fill = if lit {
            accent.gamma_multiply(0.55)
        } else if hovered {
            ui.visuals().widgets.hovered.bg_fill
        } else {
            ui.visuals().widgets.inactive.bg_fill
        };
        let ring = if !enabled {
            Color32::from_gray(70)
        } else if hovered && !lit {
            accent.lerp_to_gamma(Color32::WHITE, 0.25)
        } else {
            accent
        };
        let mark = if !enabled {
            Color32::from_gray(90)
        } else if lit {
            ui.visuals().strong_text_color()
        } else {
            accent
        };
        let painter = ui.painter();
        painter.circle(center, radius, fill, Stroke::new(2.5_f32, ring));
        let s = radius * 0.42;
        match icon {
            RoundIcon::Text(text) => {
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    text,
                    FontId::proportional(radius * 0.55),
                    mark,
                );
            }
            RoundIcon::Play => {
                let points = vec![
                    pos2(center.x - s * 0.7, center.y - s),
                    pos2(center.x - s * 0.7, center.y + s),
                    pos2(center.x + s, center.y),
                ];
                painter.add(Shape::convex_polygon(points, mark, Stroke::NONE));
            }
            RoundIcon::Pause => {
                let bar = vec2(s * 0.45, s * 1.9);
                for dx in [-s * 0.45, s * 0.45] {
                    painter.rect_filled(
                        Rect::from_center_size(pos2(center.x + dx, center.y), bar),
                        1.0,
                        mark,
                    );
                }
            }
        }
    }
    response
}

/// Dark, flat look in the spirit of rekordbox's performance screen, or its
/// light version, or whichever the system uses; text at `text_scale`
/// percent.
pub fn apply_theme(ctx: &eframe::egui::Context, theme: crate::config::Theme, text_scale: u32) {
    use crate::config::Theme;
    use eframe::egui::ThemePreference;
    // Both looks are set; egui picks one, following the system for System.
    ctx.set_visuals_of(eframe::egui::Theme::Dark, visuals(true));
    ctx.set_visuals_of(eframe::egui::Theme::Light, visuals(false));
    ctx.set_theme(match theme {
        Theme::Dark => ThemePreference::Dark,
        Theme::Light => ThemePreference::Light,
        Theme::System => ThemePreference::System,
    });
    let scale = text_scale.clamp(50, 300) as f32 / 100.0;
    let normal = eframe::egui::Style::default().text_styles;
    ctx.all_styles_mut(|style| {
        style.spacing.button_padding = vec2(5.0, 2.0);
        // Labels are not text to select; no I-beam cursor over the library.
        style.interaction.selectable_labels = false;
        style.text_styles = normal
            .iter()
            .map(|(name, font)| (name.clone(), FontId::new(font.size * scale, font.family.clone())))
            .collect();
    });
}

fn visuals(dark: bool) -> eframe::egui::Visuals {
    use eframe::egui::Visuals;
    let gray = Color32::from_gray;
    let corner = CornerRadius::same(3);
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    if dark {
        v.panel_fill = gray(20);
        v.window_fill = gray(26);
        v.extreme_bg_color = gray(12);
        v.faint_bg_color = gray(27);
        v.window_stroke = Stroke::new(1.0_f32, gray(55));
        v.selection.bg_fill = Color32::from_rgb(28, 88, 168);
        v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    } else {
        v.panel_fill = gray(236);
        v.window_fill = gray(246);
        v.extreme_bg_color = gray(252);
        v.faint_bg_color = gray(226);
        v.window_stroke = Stroke::new(1.0_f32, gray(180));
        v.selection.bg_fill = Color32::from_rgb(120, 170, 240);
        v.selection.stroke = Stroke::new(1.0_f32, Color32::BLACK);
    }
    let shades = if dark {
        [(32, 60), (44, 100), (58, 120), (40, 90)]
    } else {
        [(222, 175), (208, 140), (192, 120), (214, 150)]
    };
    let widgets = [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ];
    for (widget, (fill, outline)) in widgets.into_iter().zip(shades) {
        widget.corner_radius = corner;
        widget.bg_fill = gray(fill);
        widget.weak_bg_fill = gray(fill);
        widget.bg_stroke = Stroke::new(1.0_f32, gray(outline));
        widget.expansion = 0.0;
    }
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, gray(if dark { 195 } else { 40 }));
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, gray(if dark { 45 } else { 200 }));
    v.widgets.noninteractive.corner_radius = corner;
    v
}

/// A small triangle for folders: pointing right when closed, down when
/// open. Returns the click response.
pub fn disclosure(ui: &mut Ui, open: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::click());
    let c = rect.center();
    let s = 4.0;
    let points = if open {
        vec![pos2(c.x - s, c.y - s * 0.6), pos2(c.x + s, c.y - s * 0.6), pos2(c.x, c.y + s * 0.8)]
    } else {
        vec![pos2(c.x - s * 0.6, c.y - s), pos2(c.x - s * 0.6, c.y + s), pos2(c.x + s * 0.8, c.y)]
    };
    let visuals = ui.visuals();
    let color = if response.hovered() { visuals.strong_text_color() } else { visuals.text_color() };
    ui.painter().add(Shape::convex_polygon(points, color, Stroke::NONE));
    response
}
