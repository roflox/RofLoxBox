//! How the scrolling waveforms look, as set in Preferences: the gradient of
//! each drawing method, the soft method's colour turn and height curve, and
//! the fixed colours (3-band, background).

use eframe::egui::{self, Color32, Ui};
use serde::{Deserialize, Serialize};

use crate::waveform::WaveformDrawing;

/// One stop of a column's gradient, from the middle (`at` 0) to the tip
/// (`at` 1): the column's colour blended `amount` of the way to `tint`,
/// at `alpha` opacity.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GradientStop {
    pub at: f32,
    pub tint: [u8; 3],
    pub amount: f32,
    pub alpha: f32,
}

impl GradientStop {
    const fn plain(at: f32, alpha: f32) -> Self {
        Self { at, tint: [255; 3], amount: 0.0, alpha }
    }

    /// The colour of this stop for a column of `color`.
    pub fn color(&self, color: Color32) -> Color32 {
        let mix = |c: u8, t: u8| {
            (f32::from(c) + (f32::from(t) - f32::from(c)) * self.amount.clamp(0.0, 1.0)) as u8
        };
        let [r, g, b] = self.tint;
        Color32::from_rgba_unmultiplied(
            mix(color.r(), r),
            mix(color.g(), g),
            mix(color.b(), b),
            (self.alpha.clamp(0.0, 1.0) * 255.0) as u8,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaveformLook {
    /// Each drawing in the pastel colours (from the colour detail
    /// waveform's formula) instead of our own (from the bands).
    pub soft_pastel: bool,
    pub classic_pastel: bool,
    pub simple_pastel: bool,
    /// Each drawing's colours for lows, mids and highs: a column's red,
    /// green and blue share become these.
    pub soft_bands: [[u8; 3]; 3],
    pub classic_bands: [[u8; 3]; 3],
    pub simple_bands: [[u8; 3]; 3],
    pub soft: Vec<GradientStop>,
    pub classic: Vec<GradientStop>,
    pub simple: Vec<GradientStop>,
    /// Soft: hues turned by this share of the colour wheel.
    pub hue_turn: f32,
    /// Soft: saturation multiplied by this.
    pub saturation: f32,
    /// Soft: heights as level^this (1.0 = linear).
    pub height_curve: f32,
    /// Soft: each pixel column blended a little with its neighbours.
    pub smooth_outline: bool,
    pub background: [u8; 3],
    /// 3-band: lows, mids, where lows and mids overlap, highs.
    pub three_band: [[u8; 3]; 4],
}

impl Default for WaveformLook {
    fn default() -> Self {
        Self {
            soft_pastel: false,
            classic_pastel: true,
            simple_pastel: false,
            soft_bands: PLAIN_BANDS,
            classic_bands: PLAIN_BANDS,
            simple_bands: PLAIN_BANDS,
            soft: default_stops(WaveformDrawing::Soft),
            classic: default_stops(WaveformDrawing::Classic),
            simple: default_stops(WaveformDrawing::Simple),
            hue_turn: 0.0,
            saturation: 1.0,
            height_curve: 0.8,
            smooth_outline: true,
            background: [12; 3],
            three_band: DEFAULT_THREE_BAND,
        }
    }
}

/// Lows red, mids green, highs blue: colours as they are.
const PLAIN_BANDS: [[u8; 3]; 3] = [[255, 0, 0], [0, 255, 0], [0, 0, 255]];

const DEFAULT_THREE_BAND: [[u8; 3]; 4] =
    [[32, 83, 217], [242, 170, 60], [169, 107, 39], [255, 255, 255]];

fn default_stops(drawing: WaveformDrawing) -> Vec<GradientStop> {
    match drawing {
        WaveformDrawing::Soft => vec![
            GradientStop { at: 0.0, tint: [255; 3], amount: 0.18, alpha: 1.0 },
            GradientStop::plain(0.15, 1.0),
            GradientStop::plain(0.6, 1.0),
            GradientStop::plain(1.0, 0.45),
        ],
        WaveformDrawing::Classic => vec![GradientStop::plain(0.0, 1.0), GradientStop::plain(1.0, 0.5)],
        WaveformDrawing::Simple | WaveformDrawing::Simplest => vec![GradientStop::plain(0.0, 1.0), GradientStop::plain(1.0, 1.0)],
    }
}

impl WaveformLook {
    pub fn stops(&self, drawing: WaveformDrawing) -> &[GradientStop] {
        match drawing {
            WaveformDrawing::Soft => &self.soft,
            WaveformDrawing::Classic => &self.classic,
            WaveformDrawing::Simple | WaveformDrawing::Simplest => &self.simple,
        }
    }

    pub fn pastel(&self, drawing: WaveformDrawing) -> bool {
        match drawing {
            WaveformDrawing::Soft => self.soft_pastel,
            WaveformDrawing::Classic => self.classic_pastel,
            WaveformDrawing::Simple | WaveformDrawing::Simplest => self.simple_pastel,
        }
    }

    fn pastel_mut(&mut self, drawing: WaveformDrawing) -> &mut bool {
        match drawing {
            WaveformDrawing::Soft => &mut self.soft_pastel,
            WaveformDrawing::Classic => &mut self.classic_pastel,
            WaveformDrawing::Simple | WaveformDrawing::Simplest => &mut self.simple_pastel,
        }
    }

    fn bands_mut(&mut self, drawing: WaveformDrawing) -> &mut [[u8; 3]; 3] {
        match drawing {
            WaveformDrawing::Soft => &mut self.soft_bands,
            WaveformDrawing::Classic => &mut self.classic_bands,
            WaveformDrawing::Simple | WaveformDrawing::Simplest => &mut self.simple_bands,
        }
    }

    /// A column colour in `drawing`'s band colours: its red share becomes
    /// the lows' colour, green the mids', blue the highs'.
    pub fn band_colours(&self, drawing: WaveformDrawing, color: Color32) -> Color32 {
        let bands = match drawing {
            WaveformDrawing::Soft => &self.soft_bands,
            WaveformDrawing::Classic => &self.classic_bands,
            WaveformDrawing::Simple | WaveformDrawing::Simplest => &self.simple_bands,
        };
        if *bands == PLAIN_BANDS {
            return color;
        }
        let shares = [color.r(), color.g(), color.b()].map(|c| f32::from(c) / 255.0);
        let channel = |k: usize| {
            let sum: f32 = (0..3).map(|band| shares[band] * f32::from(bands[band][k])).sum();
            sum.min(255.0) as u8
        };
        Color32::from_rgba_unmultiplied(channel(0), channel(1), channel(2), color.a())
    }

    fn stops_mut(&mut self, drawing: WaveformDrawing) -> &mut Vec<GradientStop> {
        match drawing {
            WaveformDrawing::Soft => &mut self.soft,
            WaveformDrawing::Classic => &mut self.classic,
            WaveformDrawing::Simple | WaveformDrawing::Simplest => &mut self.simple,
        }
    }

    /// Soft's shade of a column colour: hue turned, saturation scaled
    /// (in plain RGB, cheap enough to do for every pixel column).
    pub fn shade(&self, color: Color32) -> Color32 {
        if self.hue_turn == 0.0 && self.saturation == 1.0 {
            return color;
        }
        let [r, g, b] = [color.r(), color.g(), color.b()].map(|c| f32::from(c) / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        let mut h = if delta <= 0.0 {
            0.0
        } else if max == r {
            ((g - b) / delta).rem_euclid(6.0)
        } else if max == g {
            (b - r) / delta + 2.0
        } else {
            (r - g) / delta + 4.0
        } / 6.0;
        h = (h + self.hue_turn).rem_euclid(1.0);
        let s = if max > 0.0 { (delta / max * self.saturation).clamp(0.0, 1.0) } else { 0.0 };
        let v = max;
        let c = v * s;
        let x = c * (1.0 - ((h * 6.0).rem_euclid(2.0) - 1.0).abs());
        let (r, g, b) = match (h * 6.0) as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = v - c;
        let to = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        Color32::from_rgb(to(r), to(g), to(b))
    }

    pub fn background(&self) -> Color32 {
        let [r, g, b] = self.background;
        Color32::from_rgb(r, g, b)
    }

    pub fn three_band(&self, which: usize) -> Color32 {
        let [r, g, b] = self.three_band[which.min(3)];
        Color32::from_rgb(r, g, b)
    }
}

fn color_button(ui: &mut Ui, rgb: &mut [u8; 3]) -> bool {
    ui.color_edit_button_srgb(rgb).changed()
}

#[cfg(test)]
#[test]
fn band_colours_remix_the_channels() {
    let mut look = WaveformLook::default();
    let c = Color32::from_rgb(200, 100, 50);
    assert_eq!(look.band_colours(WaveformDrawing::Classic, c), c);
    // Lows orange instead of red: the red share turns orange.
    look.classic_bands[0] = [255, 128, 0];
    let mixed = look.band_colours(WaveformDrawing::Classic, Color32::from_rgb(255, 0, 0));
    assert_eq!(mixed, Color32::from_rgb(255, 128, 0));
}

/// The gradient editor for one drawing method.
fn gradient_editor(ui: &mut Ui, look: &mut WaveformLook, drawing: WaveformDrawing) {
    ui.horizontal(|ui| {
        // Simplest draws with Simple's gradient.
        let title = match drawing {
            WaveformDrawing::Simple => "Simple and Simplest",
            other => other.label(),
        };
        ui.label(egui::RichText::new(title).strong());
        let pastel_default = WaveformLook::default().pastel(drawing);
        let changed = *look.stops(drawing) != default_stops(drawing)
            || *look.bands_mut(drawing) != PLAIN_BANDS
            || look.pastel(drawing) != pastel_default;
        if ui.add_enabled(changed, egui::Button::new("Reset")).clicked() {
            *look.stops_mut(drawing) = default_stops(drawing);
            *look.bands_mut(drawing) = PLAIN_BANDS;
            *look.pastel_mut(drawing) = pastel_default;
        }
    });
    ui.horizontal(|ui| {
        ui.label("Colours");
        let pastel = look.pastel_mut(drawing);
        ui.selectable_value(pastel, false, "Own").on_hover_text("Each band against the loudest: strong reds, greens, blues");
        ui.selectable_value(pastel, true, "Pastel").on_hover_text("Softer, lighter mixes (needs High detail)");
        ui.separator();
        for (name, rgb) in ["Lows", "Mids", "Highs"].into_iter().zip(look.bands_mut(drawing).iter_mut()) {
            ui.label(name);
            color_button(ui, rgb);
        }
    });
    let stops = look.stops_mut(drawing);
    let last = stops.len().saturating_sub(1);
    egui::Grid::new(("gradient", drawing.label())).num_columns(5).spacing([10.0, 4.0]).show(
        ui,
        |ui| {
            ui.label(egui::RichText::new("Where").small().weak());
            ui.label(egui::RichText::new("Blend to").small().weak());
            ui.label(egui::RichText::new("Amount").small().weak());
            ui.label(egui::RichText::new("Opacity").small().weak());
            ui.end_row();
            for i in 0..stops.len() {
                let (low, high) = (
                    if i == 0 { 0.0 } else { stops[i - 1].at },
                    if i == last { 1.0 } else { stops[i + 1].at },
                );
                let stop = &mut stops[i];
                if i == 0 || i == last {
                    ui.label(if i == 0 { "middle" } else { "tip" });
                } else {
                    ui.add(egui::Slider::new(&mut stop.at, low..=high).fixed_decimals(2));
                }
                // The colour only counts with an amount.
                ui.scope(|ui| {
                    if stop.amount <= 0.0 {
                        ui.multiply_opacity(0.35);
                    }
                    color_button(ui, &mut stop.tint);
                });
                ui.add(egui::Slider::new(&mut stop.amount, 0.0..=1.0).fixed_decimals(2));
                ui.add(egui::Slider::new(&mut stop.alpha, 0.0..=1.0).fixed_decimals(2));
                ui.end_row();
            }
        },
    );
}

/// The Preferences part on how the waveforms look: each drawing's
/// gradient (Simplest shares Simple's) and colours.
pub fn look_prefs(ui: &mut Ui, look: &mut WaveformLook) {
    ui.label("Each column is shaded from the middle out to its tips.");
    for drawing in WaveformDrawing::ALL.into_iter().filter(|d| *d == d.shape()) {
        ui.add_space(6.0);
        gradient_editor(ui, look, drawing);
        if drawing == WaveformDrawing::Soft {
            ui.indent("soft extras", |ui| {
                let defaults = WaveformLook::default();
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut look.hue_turn, -0.15..=0.15).text("Hue turn"));
                    ui.add(egui::Slider::new(&mut look.saturation, 0.0..=1.5).text("Saturation"));
                });
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut look.height_curve, 0.4..=1.5).text("Height curve"))
                        .on_hover_text("Below 1 quiet parts stand taller; 1 is linear");
                    ui.checkbox(&mut look.smooth_outline, "Smooth outline");
                });
                let changed = look.hue_turn != defaults.hue_turn
                    || look.saturation != defaults.saturation
                    || look.height_curve != defaults.height_curve
                    || look.smooth_outline != defaults.smooth_outline;
                if ui.add_enabled(changed, egui::Button::new("Reset these")).clicked() {
                    look.hue_turn = defaults.hue_turn;
                    look.saturation = defaults.saturation;
                    look.height_curve = defaults.height_curve;
                    look.smooth_outline = defaults.smooth_outline;
                }
            });
        }
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Colours").strong());
        let changed = look.background != [12; 3] || look.three_band != DEFAULT_THREE_BAND;
        if ui.add_enabled(changed, egui::Button::new("Reset")).clicked() {
            look.background = [12; 3];
            look.three_band = DEFAULT_THREE_BAND;
        }
    });
    ui.horizontal(|ui| {
        ui.label("Background");
        color_button(ui, &mut look.background);
        ui.separator();
        ui.label("3-Band:");
        for (name, rgb) in ["lows", "mids", "both", "highs"].into_iter().zip(look.three_band.iter_mut()) {
            ui.label(name);
            color_button(ui, rgb);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shade_turns_hue_and_keeps_grey() {
        let look = WaveformLook { hue_turn: -0.025, saturation: 0.88, ..WaveformLook::default() };
        let grey = Color32::from_rgb(100, 100, 100);
        assert_eq!(look.shade(grey), grey);
        // Pure blue turned a little toward cyan (negative turn), less saturated.
        let blue = look.shade(Color32::from_rgb(0, 0, 255));
        assert!(blue.b() == 255 && blue.g() > blue.r(), "{blue:?}");
        let plain = WaveformLook { hue_turn: 0.0, saturation: 1.0, ..look };
        assert_eq!(plain.shade(Color32::from_rgb(10, 200, 30)), Color32::from_rgb(10, 200, 30));
    }

    #[test]
    fn stops_blend_toward_their_tint() {
        let stop = GradientStop { at: 0.0, tint: [255; 3], amount: 0.5, alpha: 0.5 };
        let c = stop.color(Color32::from_rgb(0, 100, 200));
        assert_eq!(c, Color32::from_rgba_unmultiplied(127, 177, 227, 127));
    }
}
