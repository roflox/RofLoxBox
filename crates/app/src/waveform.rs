//! Waveforms: taken from rekordbox's analysis files when present, otherwise
//! computed from the audio, and drawn into egui with cue markers and the
//! deck's EQ applied.

use deckhand_formats::anlz::{AnlzFile, Waveform as AnlzWaveform, WaveformKind};
use eframe::egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, Vec2, pos2, vec2};

/// Columns per second in rekordbox's detail waveforms.
pub const COLUMNS_PER_SECOND: f64 = 150.0;
const OVERVIEW_COLUMNS: usize = 800;

const BLUE: Color32 = Color32::from_rgb(40, 120, 230);
const LOW: Color32 = Color32::from_rgb(32, 83, 217);
const MID: Color32 = Color32::from_rgb(242, 170, 60);
const LOW_MID: Color32 = Color32::from_rgb(169, 107, 39);
const HIGH: Color32 = Color32::from_rgb(255, 255, 255);
const MEMORY_CUE: Color32 = Color32::from_rgb(230, 50, 50);
const CUE_POINT: Color32 = Color32::from_rgb(255, 170, 0);

/// How the scrolling waveform is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum WaveformStyle {
    /// rekordbox's RGB waveform (low ≈ red, mid ≈ green, high ≈ blue).
    #[default]
    Rgb,
    /// Low, mid and high bands drawn over each other (CDJ-3000 style).
    ThreeBand,
}

#[derive(Debug, Clone, Copy)]
pub struct Column {
    /// 0.0–1.0
    pub height: f32,
    pub color: Color32,
}

#[derive(Debug, Clone, Default)]
pub struct WaveformData {
    /// 150 columns per second.
    pub detail: Vec<Column>,
    /// The detail columns carry real colours (rekordbox's colour waveform,
    /// or colours worked out from the bands).
    pub colored: bool,
    /// Low, mid, high heights, 150 per second.
    pub bands: Vec<[f32; 3]>,
    pub overview: Vec<Column>,
}

impl WaveformData {
    /// From the analysis files: colored detail (PWV5) or monochrome (PWV3)
    /// from `.EXT`, three bands (PWV7) from `.2EX`.
    pub fn from_analysis(ext: Option<&AnlzFile>, two_ex: Option<&AnlzFile>) -> Option<Self> {
        let (detail, colored) = match ext {
            Some(ext) => match ext.waveform(WaveformKind::ColorDetail) {
                Some(color) => (color_columns(color), true),
                None => (mono_columns(ext.waveform(WaveformKind::Detail)?), false),
            },
            None => return None,
        };
        let bands = two_ex
            .and_then(|f| f.waveform(WaveformKind::ThreeBandDetail))
            .map(band_columns)
            .unwrap_or_default();
        (!detail.is_empty()).then(|| Self { colored, ..Self::with_detail(detail, bands) })
    }

    /// Gives a waveform without colours (from peaks, or rekordbox's
    /// monochrome one) colours from its bands, like rekordbox's RGB view:
    /// bass red, mids green, highs blue.
    pub fn color_from_bands(&mut self) {
        if self.colored || self.bands.is_empty() {
            return;
        }
        for (i, column) in self.detail.iter_mut().enumerate() {
            if let Some(&bands) = self.bands.get(i.min(self.bands.len() - 1)) {
                column.color = band_color(bands);
            }
        }
        *self = Self {
            colored: true,
            ..Self::with_detail(std::mem::take(&mut self.detail), std::mem::take(&mut self.bands))
        };
    }

    /// From peak levels at 150 per second (see `deckhand_audio::peak_waveform`).
    pub fn from_peaks(peaks: &[f32], bands: Vec<[f32; 3]>) -> Self {
        let max = peaks.iter().copied().fold(0.0f32, f32::max).max(1e-6);
        Self::with_detail(
            peaks.iter().map(|&p| Column { height: (p / max).sqrt(), color: BLUE }).collect(),
            bands,
        )
    }

    fn with_detail(detail: Vec<Column>, bands: Vec<[f32; 3]>) -> Self {
        let step = detail.len().div_ceil(OVERVIEW_COLUMNS).max(1);
        let overview = detail
            .chunks(step)
            .map(|chunk| {
                *chunk.iter().max_by(|a, b| a.height.total_cmp(&b.height)).expect("non-empty chunk")
            })
            .collect();
        Self { detail, colored: false, bands, overview }
    }

    pub fn has_bands(&self) -> bool {
        !self.bands.is_empty()
    }
}

/// The colour of a column from its low, mid and high levels: the loudest
/// band at full brightness, the others in proportion.
fn band_color([low, mid, high]: [f32; 3]) -> Color32 {
    // Highs measure smaller than lows for the same loudness; lift them.
    let [r, g, b] = [low, mid * 1.3, high * 2.0];
    let peak = r.max(g).max(b);
    if peak <= 1e-6 {
        return Color32::from_gray(60);
    }
    // A steep curve keeps colours saturated, as in rekordbox.
    let channel = |v: f32| (255.0 * (v / peak).powf(2.2)) as u8;
    Color32::from_rgb(channel(r), channel(g), channel(b))
}

/// PWV5: big-endian u16, red/green/blue 3 bits each, then 5 bits of height.
fn color_columns(waveform: &AnlzWaveform) -> Vec<Column> {
    waveform
        .columns()
        .map(|c| {
            let v = u16::from_be_bytes([c[0], c[1]]);
            let channel = |shift: u16| (((v >> shift) & 0x7) as f32 / 7.0 * 255.0) as u8;
            Column {
                height: f32::from((v >> 2) & 0x1f) / 31.0,
                color: Color32::from_rgb(channel(13), channel(10), channel(7)),
            }
        })
        .collect()
}

/// PWV3: 5 bits of height, 3 bits of whiteness.
fn mono_columns(waveform: &AnlzWaveform) -> Vec<Column> {
    waveform
        .columns()
        .map(|c| {
            let whiteness = f32::from(c[0] >> 5) / 7.0;
            let mix =
                |base: u8| (f32::from(base) + (255.0 - f32::from(base)) * whiteness * 0.6) as u8;
            Column {
                height: f32::from(c[0] & 0x1f) / 31.0,
                color: Color32::from_rgb(mix(40), mix(120), mix(230)),
            }
        })
        .collect()
}

/// PWV7: one byte each for low, mid and high.
///
/// The crate-digger notes give the order as mid, high, low; correlating the
/// bytes with bands computed from the audio of both fixture tracks shows
/// low, mid, high (byte 0 ↔ low 0.84/0.95, byte 1 ↔ mid 0.71/0.92,
/// byte 2 ↔ high 0.71/0.75).
fn band_columns(waveform: &AnlzWaveform) -> Vec<[f32; 3]> {
    let max = waveform.data.iter().copied().max().unwrap_or(1).max(1) as f32;
    waveform
        .columns()
        .map(|c| [f32::from(c[0]) / max, f32::from(c[1]) / max, f32::from(c[2]) / max])
        .collect()
}

/// A column of the RGB waveform with the EQ applied: each color channel
/// stands for a band and is scaled by that band's gain.
fn eq_rgb(column: Column, gains: [f32; 3]) -> Column {
    let [r, g, b] = [column.color.r(), column.color.g(), column.color.b()].map(f32::from);
    let sum = r + g + b;
    if sum <= 0.0 || gains == [1.0; 3] {
        return column;
    }
    let scaled = [r * gains[0], g * gains[1], b * gains[2]];
    let height = column.height * (scaled.iter().sum::<f32>() / sum);
    let peak = scaled.iter().copied().fold(0.0f32, f32::max);
    let brightness = r.max(g).max(b);
    let channel = |v: f32| if peak > 0.0 { (v / peak * brightness).min(255.0) as u8 } else { 0 };
    Column {
        height: height.min(1.0),
        color: Color32::from_rgb(channel(scaled[0]), channel(scaled[1]), channel(scaled[2])),
    }
}

/// A hot cue: time, color and letter.
pub type HotCueMark = (f64, Color32, char);

/// Markers drawn over a waveform.
pub struct Markers<'a> {
    /// Beat times in seconds with their bar position (1 = downbeat).
    pub beats: &'a [(f64, u16)],
    pub hot_cues: &'a [HotCueMark],
    pub memory_cues: &'a [f64],
    /// Stored memory loops (start, end).
    pub memory_loops: &'a [(f64, f64)],
    pub cue_point: f64,
    /// The deck's current loop and whether it is active.
    pub active_loop: Option<((f64, f64), bool)>,
    /// Beat grid brightness, 0.0 (off) … 1.5.
    pub grid_strength: f32,
}

/// Places time (`along`, later = larger) and amplitude (`across`) on the
/// screen, horizontally (time left to right) or vertically (time bottom to
/// top, so upcoming music comes from above).
#[derive(Debug, Clone, Copy)]
struct Axes {
    rect: Rect,
    vertical: bool,
}

impl Axes {
    fn length(self) -> f32 {
        if self.vertical { self.rect.height() } else { self.rect.width() }
    }

    fn breadth(self) -> f32 {
        if self.vertical { self.rect.width() } else { self.rect.height() }
    }

    fn at(self, along: f32, across: f32) -> Pos2 {
        if self.vertical {
            pos2(self.rect.left() + across, self.rect.bottom() - along)
        } else {
            pos2(self.rect.left() + along, self.rect.top() + across)
        }
    }

    /// Unit vector pointing from the `across = 0` edge into the waveform.
    fn inward(self) -> Vec2 {
        if self.vertical { vec2(1.0, 0.0) } else { vec2(0.0, 1.0) }
    }

    fn line(self, painter: &Painter, along: f32, from: f32, to: f32, stroke: Stroke) {
        painter.line_segment([self.at(along, from), self.at(along, to)], stroke);
    }

    fn span(self, from: f32, to: f32) -> Rect {
        Rect::from_two_pos(self.at(from, 0.0), self.at(to, self.breadth()))
    }
}

/// Beat grid over the waveform: phrase starts (every 4 bars) thickest, bar
/// starts strong, single beats as a soft line with bright edge ticks. Lines
/// get a dark outline so they stay visible over bright waveforms; single
/// beats are left out when they would be too close together.
fn draw_beat_grid(
    painter: &Painter,
    axes: Axes,
    beats: &[(f64, u16)],
    along_of: impl Fn(f64) -> f32,
    strength: f32,
) {
    if strength <= 0.0 || beats.len() < 2 {
        return;
    }
    let alpha = |a: f32| (a * strength).clamp(0.0, 255.0) as u8;
    let spacing = (along_of(beats[1].0) - along_of(beats[0].0)).abs();
    let show_beats = spacing >= 6.0;
    let show_bars = spacing * 4.0 >= 6.0;
    let (length, breadth) = (axes.length(), axes.breadth());
    let mut bar = 0usize;
    for &(t, position) in beats {
        let downbeat = position == 1;
        if downbeat {
            bar += 1;
        }
        let a = along_of(t);
        if a < -2.0 || a > length + 2.0 {
            continue;
        }
        if downbeat && show_bars {
            let phrase = bar % 4 == 1;
            let width: f32 = if phrase { 3.0 } else { 2.0 };
            axes.line(
                painter,
                a,
                0.0,
                breadth,
                Stroke::new(width + 2.0, Color32::from_black_alpha(alpha(150.0))),
            );
            let white = Color32::from_white_alpha(alpha(if phrase { 255.0 } else { 220.0 }));
            axes.line(painter, a, 0.0, breadth, Stroke::new(width, white));
        } else if !downbeat && show_beats {
            axes.line(
                painter,
                a,
                0.0,
                breadth,
                Stroke::new(1.0_f32, Color32::from_white_alpha(alpha(55.0))),
            );
            let tick = 7.0;
            for (from, to) in [(0.0, tick), (breadth - tick, breadth)] {
                axes.line(
                    painter,
                    a,
                    from,
                    to,
                    Stroke::new(3.0_f32, Color32::from_black_alpha(alpha(150.0))),
                );
                axes.line(
                    painter,
                    a,
                    from,
                    to,
                    Stroke::new(1.5_f32, Color32::from_white_alpha(alpha(230.0))),
                );
            }
        }
    }
}

/// The scrolling waveform around `position` (seconds), `seconds_visible`
/// long, horizontal or `vertical`, with EQ `gains` (low, mid, high; 1.0 =
/// flat, 0.0 = killed).
#[allow(clippy::too_many_arguments)]
pub fn draw_detail(
    painter: &Painter,
    rect: Rect,
    vertical: bool,
    data: &WaveformData,
    position: f64,
    seconds_visible: f64,
    style: WaveformStyle,
    gains: [f32; 3],
    markers: &Markers<'_>,
) {
    painter.rect_filled(rect, 0.0, Color32::from_gray(12));
    let axes = Axes { rect, vertical };
    let (length, breadth) = (axes.length(), axes.breadth());
    let center = length / 2.0;
    let px_per_sec = f64::from(length) / seconds_visible;
    let mid = breadth / 2.0;
    let half = breadth / 2.0 - 2.0;
    let along_of = |t: f64| center + ((t - position) * px_per_sec) as f32;
    let start = position - seconds_visible / 2.0;

    // Loops behind the waveform.
    let shade = |range: (f64, f64), color: Color32| {
        let (from, to) = (along_of(range.0).max(0.0), along_of(range.1).min(length));
        if to > from {
            painter.rect_filled(axes.span(from, to), 0.0, color);
        }
    };
    for &range in markers.memory_loops {
        shade(range, Color32::from_rgba_unmultiplied(230, 50, 50, 55));
    }
    if let Some((range, active)) = markers.active_loop {
        let color = if active {
            Color32::from_rgba_unmultiplied(255, 170, 0, 40)
        } else {
            Color32::from_white_alpha(10)
        };
        shade(range, color);
    }

    let use_bands = style == WaveformStyle::ThreeBand && data.has_bands();
    for step in 0..length as usize {
        let t = start + step as f64 / px_per_sec;
        if t < 0.0 {
            continue;
        }
        let index = (t * COLUMNS_PER_SECOND) as usize;
        let a = step as f32;
        let bar = |height: f32, color: Color32| {
            let h = height.min(1.0) * half;
            if h > 0.3 {
                axes.line(painter, a, mid - h, mid + h, Stroke::new(1.0_f32, color));
            }
        };
        if use_bands {
            let Some(&[low, mid_band, high]) = data.bands.get(index) else { break };
            let (low, mid_band, high) = (low * gains[0], mid_band * gains[1], high * gains[2]);
            // Larger of low/mid first, their overlap in brown, highs on top.
            if low >= mid_band {
                bar(low, LOW);
            } else {
                bar(mid_band, MID);
            }
            bar(low.min(mid_band), LOW_MID);
            bar(high * 0.6, HIGH);
        } else {
            let Some(&column) = data.detail.get(index) else { break };
            let column = eq_rgb(column, gains);
            bar(column.height, column.color);
        }
    }

    draw_beat_grid(painter, axes, markers.beats, along_of, markers.grid_strength);

    let visible = |a: f32| (-20.0..=length).contains(&a);
    let inward = axes.inward();
    for &t in markers.memory_cues {
        let a = along_of(t);
        if visible(a) {
            axes.line(painter, a, 0.0, breadth, Stroke::new(1.0_f32, MEMORY_CUE));
            pointer(painter, axes.at(a, 0.0), 7.0, inward, MEMORY_CUE);
        }
    }
    let a = along_of(markers.cue_point);
    if visible(a) {
        pointer(painter, axes.at(a, breadth), 7.0, -inward, CUE_POINT);
    }
    for &(t, color, letter) in markers.hot_cues {
        let a = along_of(t);
        if visible(a) {
            axes.line(painter, a, 0.0, breadth, Stroke::new(1.5_f32, color));
            let corner = axes.at(a, 0.0);
            // The flag sits on the later side of the cue line.
            let corner = if vertical { corner - vec2(0.0, 14.0) } else { corner };
            flag(painter, corner, letter, color, 13.0);
        }
    }
    axes.line(painter, center, 0.0, breadth, Stroke::new(2.0_f32, Color32::from_rgb(255, 60, 60)));
}

/// The whole track with played part dimmed and cue markers.
pub fn draw_overview(
    painter: &Painter,
    rect: Rect,
    data: &WaveformData,
    position: f64,
    duration: f64,
    markers: &Markers<'_>,
) {
    painter.rect_filled(rect, 2.0, Color32::from_gray(18));
    if duration <= 0.0 || data.overview.is_empty() {
        return;
    }
    let x_of = |t: f64| rect.left() + (t / duration).clamp(0.0, 1.0) as f32 * rect.width();
    for &(a, b) in markers.memory_loops {
        painter.rect_filled(
            Rect::from_x_y_ranges(x_of(a)..=x_of(b).max(x_of(a) + 1.0), rect.y_range()),
            0.0,
            Color32::from_rgba_unmultiplied(230, 50, 50, 50),
        );
    }
    let n = data.overview.len() as f32;
    let played = (position / duration) as f32;
    for (i, column) in data.overview.iter().enumerate() {
        let x = rect.left() + i as f32 / n * rect.width();
        let h = column.height * (rect.height() - 2.0);
        let color =
            if (i as f32 / n) < played { column.color.gamma_multiply(0.45) } else { column.color };
        painter.line_segment(
            [pos2(x, rect.bottom() - h), pos2(x, rect.bottom())],
            Stroke::new(1.0_f32, color),
        );
    }
    for &t in markers.memory_cues {
        triangle(painter, pos2(x_of(t), rect.bottom()), 5.0, false, MEMORY_CUE);
    }
    triangle(painter, pos2(x_of(markers.cue_point), rect.bottom()), 5.0, false, CUE_POINT);
    for &(t, color, letter) in markers.hot_cues {
        let x = x_of(t);
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.0_f32, color.gamma_multiply(0.7)),
        );
        flag(painter, pos2(x, rect.top()), letter, color, 10.0);
    }
    let x = rect.left() + played * rect.width();
    painter.line_segment(
        [pos2(x, rect.top()), pos2(x, rect.bottom())],
        Stroke::new(2.0_f32, Color32::WHITE),
    );
}

/// A triangle at `tip`, pointing down (at the top edge) or up (at the bottom edge).
fn triangle(painter: &Painter, tip: Pos2, size: f32, pointing_down: bool, color: Color32) {
    pointer(painter, tip, size, vec2(0.0, if pointing_down { 1.0 } else { -1.0 }), color);
}

/// A triangle with its base on `base` pointing along `direction` (a unit vector).
fn pointer(painter: &Painter, base: Pos2, size: f32, direction: Vec2, color: Color32) {
    let across = vec2(-direction.y, direction.x) * size * 0.7;
    let points = vec![base - across, base + across, base + direction * size];
    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
}

/// A hot cue flag: a colored tab with the cue letter, hanging right of the line.
fn flag(painter: &Painter, top_left: Pos2, letter: char, color: Color32, size: f32) {
    let rect = Rect::from_min_size(top_left, vec2(size, size + 1.0));
    painter.rect_filled(rect, 2.0, color);
    let text = if color.r() as u16 + color.g() as u16 + color.b() as u16 > 380 {
        Color32::BLACK
    } else {
        Color32::WHITE
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        letter,
        FontId::proportional(size - 2.0),
        text,
    );
}

/// The track time under a horizontal pointer position.
pub fn overview_time(rect: Rect, pointer: Pos2, duration: f64) -> f64 {
    f64::from(((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0)) * duration
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq_scales_rgb_channels() {
        let column = Column { height: 1.0, color: Color32::from_rgb(200, 100, 0) };
        assert_eq!(eq_rgb(column, [1.0; 3]).height, 1.0);
        // Killing the lows removes the red share of the column.
        let killed = eq_rgb(column, [0.0, 1.0, 1.0]);
        assert!((killed.height - 100.0 / 300.0).abs() < 1e-6, "{}", killed.height);
        assert_eq!(killed.color.r(), 0);
        assert!(killed.color.g() > 0);
    }

    #[test]
    fn pwv7_byte_order_is_low_mid_high() {
        let waveform = AnlzWaveform {
            kind: WaveformKind::ThreeBandDetail,
            entry_len: 3,
            data: vec![40, 20, 10],
        };
        assert_eq!(band_columns(&waveform), [[1.0, 0.5, 0.25]]);
    }

    #[test]
    fn band_colors_follow_the_loudest_band() {
        let bass = band_color([1.0, 0.2, 0.05]);
        assert!(bass.r() > 200 && bass.g() < 100 && bass.b() < 100, "{bass:?}");
        let hats = band_color([0.05, 0.1, 0.6]);
        assert!(hats.b() > 200 && hats.r() < 100, "{hats:?}");
        let mut data = WaveformData::from_peaks(&[0.5, 1.0], vec![[1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]);
        data.color_from_bands();
        assert!(data.colored && data.detail[0].color.r() > data.detail[0].color.b());
    }
}
