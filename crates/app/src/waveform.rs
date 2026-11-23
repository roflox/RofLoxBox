//! Waveforms: taken from rekordbox's analysis files when present, otherwise
//! computed from the audio, and drawn into egui with cue markers and the
//! deck's EQ applied.

use rofloxbox_formats::anlz::{AnlzFile, Waveform as AnlzWaveform, WaveformKind};
use eframe::egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, Vec2, pos2, vec2};

/// Columns per second in rekordbox's detail waveforms.
pub const COLUMNS_PER_SECOND: f64 = 150.0;
/// Columns per second of a High detail waveform.
pub const HIGH_COLUMNS_PER_SECOND: u32 = 600;
const OVERVIEW_COLUMNS: usize = 800;

/// How finely the decks' waveforms are worked out when a track loads
/// (follows the drawing: [`WaveformDrawing::detail`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum WaveformDetail {
    /// rekordbox's resolution: 150 columns a second, its own colours when
    /// it analysed the track.
    #[default]
    Standard,
    /// Worked out from the audio at 600 columns a second: sharper when
    /// zoomed in, and a finer overview.
    High,
}

const BLUE: Color32 = Color32::from_rgb(40, 120, 230);
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

/// How the RGB waveform is drawn (needs High detail; otherwise Simple).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum WaveformDrawing {
    /// The signal's top and bottom, with a lighter core, colour held most of
    /// the way out and soft tips; own colour mix, smoothed outline.
    #[default]
    Soft,
    /// The signal's top and bottom, plain colours, fading to half at the
    /// tips.
    Classic,
    /// One solid bar a column, the same above and below.
    Simple,
    /// Simple bars at rekordbox's own detail (150 columns a second, its
    /// colours): the lightest to load and to draw.
    Simplest,
}

impl WaveformDrawing {
    pub const ALL: [Self; 4] = [Self::Soft, Self::Classic, Self::Simple, Self::Simplest];

    pub fn label(self) -> &'static str {
        match self {
            Self::Soft => "Soft",
            Self::Classic => "Classic",
            Self::Simple => "Simple",
            Self::Simplest => "Simplest",
        }
    }

    /// What it is, for Preferences.
    pub fn description(self) -> &'static str {
        match self {
            Self::Soft => "The waveform's top and bottom, a lighter core, soft tips",
            Self::Classic => "The waveform's top and bottom in plain colours",
            Self::Simple => "One solid bar a column",
            Self::Simplest => "Simple bars at rekordbox's own detail: the lightest to load and draw",
        }
    }

    /// How finely tracks are worked out for it when they load: rekordbox's
    /// resolution for Simplest, four times finer for the rest.
    pub fn detail(self) -> WaveformDetail {
        match self {
            Self::Simplest => WaveformDetail::Standard,
            _ => WaveformDetail::High,
        }
    }

    /// The drawing it is done with (Simplest draws as Simple does).
    pub fn shape(self) -> Self {
        match self {
            Self::Simplest => Self::Simple,
            other => other,
        }
    }
}

/// One column of the RGB waveform's shape and colours (High detail).
#[derive(Debug, Clone, Copy)]
pub struct RgbShape {
    /// How far the signal goes above and below the middle, 1.0 = the
    /// loudest point of the track (linear).
    pub top: f32,
    pub bottom: f32,
    /// The pastel colours (from the colour detail waveform's formula).
    pub pastel: Color32,
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
    /// Columns a second in `detail` and `bands` (0 means 150).
    pub per_second: f64,
    /// The RGB waveform's shape and colours, one per detail column (High
    /// detail; empty otherwise).
    pub rgb: Vec<RgbShape>,
}

impl WaveformData {
    /// Columns a second in `detail` and `bands`.
    pub fn columns_per_second(&self) -> f64 {
        if self.per_second > 0.0 { self.per_second } else { COLUMNS_PER_SECOND }
    }

    /// High detail: the RGB waveform (see
    /// `rofloxbox_analysis::waveform::rgb_columns_at`) at `per_second`
    /// columns a second, with bands for the 3-band view and an overview as
    /// fine. Without `columns`, shaped and coloured from the peaks and bands.
    pub fn high_detail(
        peaks: &[f32],
        bands: Vec<[f32; 3]>,
        columns: &[rofloxbox_analysis::waveform::RgbColumn],
        per_second: u32,
    ) -> Self {
        let factor = f64::from(per_second) / COLUMNS_PER_SECOND;
        let overview_columns = (OVERVIEW_COLUMNS as f64 * factor.min(3.0)) as usize;
        let mut data = Self::from_peaks(peaks, bands);
        // Bars and the overview: shaped by the bands, in our own colours
        // (from the bands).
        data.shape_from_bands();
        data.overview = overview(&data.detail, overview_columns);
        data.per_second = f64::from(per_second);
        let loudest = columns.iter().map(|c| c.top.max(c.bottom)).fold(1e-6f32, f32::max);
        data.rgb = columns
            .iter()
            .map(|c| {
                let rgb = |[r, g, b]: [u8; 3]| Color32::from_rgb(r, g, b);
                RgbShape { top: c.top / loudest, bottom: c.bottom / loudest, pastel: rgb(c.classic) }
            })
            .collect();
        data
    }

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

    /// Shapes the waveform from its bands, like rekordbox's RGB view. The
    /// heights follow the bands' level: rekordbox's own colour waveform
    /// (PWV5) has spiky heights that drop to almost nothing in loud
    /// passages, which drew as black gaps the overview did not have. A
    /// waveform without colours (from peaks, or rekordbox's monochrome one)
    /// also gets them from the bands: bass red, mids green, highs blue.
    pub fn shape_from_bands(&mut self) {
        if self.bands.is_empty() {
            return;
        }
        let level = |&[low, mid, high]: &[f32; 3]| (low * low + mid * mid + high * high).sqrt();
        let loudest = self.bands.iter().map(level).fold(0.0f32, f32::max).max(1e-6);
        let last = self.bands.len() - 1;
        for (i, column) in self.detail.iter_mut().enumerate() {
            let bands = &self.bands[i.min(last)];
            column.height = level(bands) / loudest;
            if !self.colored {
                column.color = band_color(*bands);
            }
        }
        *self = Self {
            colored: true,
            ..Self::with_detail(std::mem::take(&mut self.detail), std::mem::take(&mut self.bands))
        };
    }

    /// From peak levels at 150 per second (see `rofloxbox_audio::peak_waveform`).
    pub fn from_peaks(peaks: &[f32], bands: Vec<[f32; 3]>) -> Self {
        let max = peaks.iter().copied().fold(0.0f32, f32::max).max(1e-6);
        Self::with_detail(
            peaks.iter().map(|&p| Column { height: (p / max).sqrt(), color: BLUE }).collect(),
            bands,
        )
    }

    fn with_detail(detail: Vec<Column>, bands: Vec<[f32; 3]>) -> Self {
        let overview = overview(&detail, OVERVIEW_COLUMNS);
        Self {
            detail,
            colored: false,
            bands,
            overview,
            per_second: COLUMNS_PER_SECOND,
            rgb: Vec::new(),
        }
    }

    pub fn has_bands(&self) -> bool {
        !self.bands.is_empty()
    }
}

/// About `columns` columns for the whole track, each the loudest of its
/// stretch.
fn overview(detail: &[Column], columns: usize) -> Vec<Column> {
    let step = detail.len().div_ceil(columns.max(1)).max(1);
    detail
        .chunks(step)
        .map(|chunk| {
            *chunk.iter().max_by(|a, b| a.height.total_cmp(&b.height)).expect("non-empty chunk")
        })
        .collect()
}

/// The colour of a column from its low, mid and high levels: the loudest
/// band at full brightness, the others in proportion.
fn band_color([low, mid, high]: [f32; 3]) -> Color32 {
    // Highs measure smaller than the rest for the same loudness; lift
    // them as much.
    let [r, g, b] = [low, mid, high * 1.35];
    let peak = r.max(g).max(b);
    if peak <= 1e-6 {
        return Color32::from_gray(60);
    }
    // A curve keeps the colours saturated while the other bands still
    // tint it (a kick red with some of its click, hats blue).
    let channel = |v: f32| (255.0 * (v / peak).powf(1.6)) as u8;
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
    /// Cue marker size on the scrolling waveform (1.0 = normal).
    pub cue_size: f32,
    /// Slip: where the track would be, running on while the record is
    /// scratched, held or looping.
    pub slip: Option<f64>,
}

/// The slip needle: where playback returns to when slipping ends.
const SLIP_NEEDLE: Color32 = Color32::from_rgb(120, 200, 255);

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

/// The track time under `pointer` in a detail waveform drawn by
/// [`draw_detail`] at `position` (the playhead, in the middle).
pub fn detail_time(
    rect: Rect,
    vertical: bool,
    pointer: Pos2,
    position: f64,
    seconds_visible: f64,
) -> f64 {
    let axes = Axes { rect, vertical };
    let along = if vertical { rect.bottom() - pointer.y } else { pointer.x - rect.left() };
    let from_centre = f64::from(along - axes.length() / 2.0);
    position + from_centre / f64::from(axes.length()) * seconds_visible
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
    drawing: WaveformDrawing,
    look: &crate::waveform_look::WaveformLook,
    gains: [f32; 3],
    markers: &Markers<'_>,
) {
    painter.rect_filled(rect, 0.0, look.background());
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
    // The deck's loop: bright orange with solid in/out lines while it loops,
    // a grey outline when it is set but off.
    let mut loop_edges = None;
    if let Some((range, active)) = markers.active_loop {
        let color = if active {
            Color32::from_rgba_unmultiplied(255, 150, 0, 75)
        } else {
            Color32::from_white_alpha(14)
        };
        shade(range, color);
        loop_edges = Some((range, active));
    }

    let use_bands = style == WaveformStyle::ThreeBand && data.has_bands();
    let mut mesh = eframe::egui::Mesh::default();
    // RGB pixel columns: where along, extent up and down, colour.
    let mut rgb: Vec<(f32, f32, f32, Color32)> = Vec::with_capacity(length as usize);
    let drawing = drawing.shape();
    let shaped = !use_bands && drawing != WaveformDrawing::Simple && !data.rgb.is_empty();
    // Without the shape (Standard detail) every drawing is Simple bars.
    let drawing = if use_bands || shaped { drawing } else { WaveformDrawing::Simple };
    let stops = look.stops(drawing);
    let columns = if use_bands { data.bands.len() } else { data.detail.len() };
    let per_second = data.columns_per_second();
    for step in 0..length as usize {
        let t = start + step as f64 / px_per_sec;
        if t < 0.0 {
            continue;
        }
        // Every column under this pixel counts, the loudest showing (as in
        // the overview), so zoomed out nothing falls between pixels.
        let first = (t * per_second) as usize;
        if first >= columns {
            break;
        }
        let end = (((t + 1.0 / px_per_sec) * per_second) as usize).clamp(first + 1, columns);
        let a = step as f32;
        let mut bar = |height: f32, color: Color32, shaded: bool| {
            let h = height.min(1.0) * half;
            if h > 0.3 {
                if shaded {
                    gradient_column(&mut mesh, axes, a, mid, h, h, color, stops);
                } else {
                    column_strip(&mut mesh, axes, a, &[(mid - h, color), (mid + h, color)]);
                }
            }
        };
        if use_bands {
            let [low, mid_band, high] = data.bands[first..end]
                .iter()
                .fold([0.0f32; 3], |m, b| [m[0].max(b[0]), m[1].max(b[1]), m[2].max(b[2])]);
            let (low, mid_band, high) = (low * gains[0], mid_band * gains[1], high * gains[2]);
            // Larger of low/mid first, their overlap in brown, highs on top.
            if low >= mid_band {
                bar(low, look.three_band(0), false);
            } else {
                bar(mid_band, look.three_band(1), false);
            }
            bar(low.min(mid_band), look.three_band(2), false);
            bar(high * 0.6, look.three_band(3), false);
        } else if !shaped {
            let index = (first..end)
                .max_by(|&a, &b| data.detail[a].height.total_cmp(&data.detail[b].height))
                .expect("non-empty range");
            let mut column = data.detail[index];
            if look.pastel(drawing)
                && let Some(shape) = data.rgb.get(index)
            {
                column.color = shape.pastel;
            }
            let column = eq_rgb(column, gains);
            bar(column.height, look.band_colours(drawing, column.color), true);
        } else {
            // The signal's top and bottom (the loudest of the pixel's
            // columns each way), in the colour of its biggest column.
            let from = first.min(data.rgb.len() - 1);
            let shapes = &data.rgb[from..end.min(data.rgb.len())];
            let Some((offset, biggest)) = shapes
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| (a.top + a.bottom).total_cmp(&(b.top + b.bottom)))
            else {
                continue;
            };
            let top = shapes.iter().map(|c| c.top).fold(0.0, f32::max);
            let bottom = shapes.iter().map(|c| c.bottom).fold(0.0, f32::max);
            let soft = drawing == WaveformDrawing::Soft;
            let color = if look.pastel(drawing) {
                biggest.pastel
            } else {
                data.detail.get(from + offset).map_or(biggest.pastel, |c| c.color)
            };
            let color = look.band_colours(drawing, color);
            let color = if soft { look.shade(color) } else { color };
            let height = biggest.top.max(1e-6);
            let eq = eq_rgb(Column { height, color }, gains);
            let scale = eq.height / height;
            let size = |v: f32| {
                let v = (v * scale).min(1.0);
                if soft { v.powf(look.height_curve) } else { v }
            };
            let (up, down) = (size(top) * half, size(bottom) * half);
            if soft {
                rgb.push((a, up, down, eq.color));
            } else if up + down > 0.3 {
                gradient_column(&mut mesh, axes, a, mid, up, down, eq.color, stops);
            }
        }
    }
    // Soft: each pixel column blended a little with its neighbours (never
    // made smaller), for a smoother outline.
    let blend = |i: usize, of: fn(&(f32, f32, f32, Color32)) -> f32| {
        if !look.smooth_outline {
            return of(&rgb[i]);
        }
        let at = |j: usize| rgb.get(j).map_or(of(&rgb[i]), of);
        let around = 0.5 * of(&rgb[i]) + 0.25 * (at(i.wrapping_sub(1)) + at(i + 1));
        of(&rgb[i]).max(around)
    };
    for (i, &(a, _, _, full)) in rgb.iter().enumerate() {
        let (up, down) = (blend(i, |c| c.1), blend(i, |c| c.2));
        if up + down > 0.3 {
            gradient_column(&mut mesh, axes, a, mid, up, down, full, stops);
        }
    }
    painter.add(Shape::mesh(mesh));

    draw_beat_grid(painter, axes, markers.beats, along_of, markers.grid_strength);
    if let Some(((start, end), active)) = loop_edges {
        let (color, width) = if active {
            (Color32::from_rgb(255, 160, 0), 2.5_f32)
        } else {
            (Color32::from_gray(120), 1.0_f32)
        };
        for t in [start, end] {
            let a = along_of(t);
            if (0.0..=length).contains(&a) {
                axes.line(painter, a, 0.0, breadth, Stroke::new(width, color));
            }
        }
        if active {
            let a = along_of(start).max(2.0);
            if a < length - 30.0 {
                let pos = axes.at(a + 3.0, breadth - 2.0);
                painter.text(pos, Align2::LEFT_BOTTOM, "LOOP", FontId::proportional(11.0), color);
            }
        }
    }

    // Cue markers stand out over a bright waveform: lines with a dark
    // edge, and big tabs and triangles at the edges.
    let size = (breadth * 0.22).clamp(12.0, 22.0) * markers.cue_size.clamp(0.5, 3.0);
    // Never taller than about half the waveform.
    let size = size.min(breadth * 0.5).max(8.0);
    let line = markers.cue_size.clamp(1.0, 2.0);
    let visible = |a: f32| (-size * 1.5..=length + size).contains(&a);
    let inward = axes.inward();
    let marker_line = |a: f32, width: f32, color: Color32| {
        axes.line(painter, a, 0.0, breadth, Stroke::new(width + 2.0, Color32::from_black_alpha(170)));
        axes.line(painter, a, 0.0, breadth, Stroke::new(width, color));
    };
    for &t in markers.memory_cues {
        let a = along_of(t);
        if visible(a) {
            marker_line(a, 1.5 * line, MEMORY_CUE);
            pointer(painter, axes.at(a, 0.0), size * 0.7, inward, MEMORY_CUE);
        }
    }
    let a = along_of(markers.cue_point);
    if visible(a) {
        marker_line(a, 1.5 * line, CUE_POINT);
        pointer(painter, axes.at(a, breadth), size * 0.7, -inward, CUE_POINT);
    }
    for &(t, color, letter) in markers.hot_cues {
        let a = along_of(t);
        if visible(a) {
            marker_line(a, 2.0 * line, color);
            let corner = axes.at(a, 0.0);
            // The flag sits on the later side of the cue line.
            let corner = if vertical { corner - vec2(0.0, size + 1.0) } else { corner };
            flag(painter, corner, letter, color, size);
        }
    }
    if let Some(t) = markers.slip {
        let a = along_of(t);
        if (0.0..=length).contains(&a) {
            axes.line(painter, a, 0.0, breadth, Stroke::new(2.0_f32, SLIP_NEEDLE));
            let pos = axes.at(a + 3.0, 2.0);
            let font = FontId::proportional(10.0);
            painter.text(pos, Align2::LEFT_TOP, "SLIP", font, SLIP_NEEDLE);
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
    if let Some(((a, b), active)) = markers.active_loop {
        let span = Rect::from_x_y_ranges(x_of(a)..=x_of(b).max(x_of(a) + 2.0), rect.y_range());
        if active {
            painter.rect_filled(span, 0.0, Color32::from_rgba_unmultiplied(255, 150, 0, 90));
        }
        let color = if active { Color32::from_rgb(255, 160, 0) } else { Color32::from_gray(130) };
        painter.rect_stroke(
            span,
            0.0,
            Stroke::new(1.5_f32, color),
            eframe::egui::StrokeKind::Inside,
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
    let size = (rect.height() * 0.3).clamp(10.0, 14.0);
    for &t in markers.memory_cues {
        triangle(painter, pos2(x_of(t), rect.bottom()), size * 0.55, false, MEMORY_CUE);
    }
    triangle(painter, pos2(x_of(markers.cue_point), rect.bottom()), size * 0.55, false, CUE_POINT);
    for &(t, color, letter) in markers.hot_cues {
        let x = x_of(t);
        let line = [pos2(x, rect.top()), pos2(x, rect.bottom())];
        painter.line_segment(line, Stroke::new(3.0_f32, Color32::from_black_alpha(150)));
        painter.line_segment(line, Stroke::new(1.0_f32, color));
        flag(painter, pos2(x, rect.top()), letter, color, size);
    }
    if let Some(t) = markers.slip {
        let x = x_of(t);
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.5_f32, SLIP_NEEDLE),
        );
    }
    let x = rect.left() + played * rect.width();
    painter.line_segment(
        [pos2(x, rect.top()), pos2(x, rect.bottom())],
        Stroke::new(2.0_f32, Color32::WHITE),
    );
}

/// A column `up` above and `down` below `mid`, shaded by `stops` from the
/// middle out to both tips.
#[allow(clippy::too_many_arguments)]
fn gradient_column(
    mesh: &mut eframe::egui::Mesh,
    axes: Axes,
    along: f32,
    mid: f32,
    up: f32,
    down: f32,
    color: Color32,
    stops: &[crate::waveform_look::GradientStop],
) {
    let mut points = [(0.0f32, Color32::TRANSPARENT); 2 * MAX_STOPS - 1];
    let stops = &stops[..stops.len().min(MAX_STOPS)];
    let n = stops.len();
    if n == 0 {
        return;
    }
    for (k, stop) in stops.iter().enumerate() {
        let c = stop.color(color);
        points[n - 1 - k] = (mid - up * stop.at, c);
        points[n - 1 + k] = (mid + down * stop.at, c);
    }
    column_strip(mesh, axes, along, &points[..2 * n - 1]);
}

/// Most gradient stops drawn.
const MAX_STOPS: usize = 8;

/// A one-point-wide strip at `along` through `stops` (across, colour),
/// shaded between them; neighbouring pieces share their vertices.
fn column_strip(mesh: &mut eframe::egui::Mesh, axes: Axes, along: f32, stops: &[(f32, Color32)]) {
    let first = mesh.vertices.len() as u32;
    for &(across, color) in stops {
        mesh.colored_vertex(axes.at(along, across), color);
        mesh.colored_vertex(axes.at(along + 1.0, across), color);
    }
    for k in 0..stops.len().saturating_sub(1) as u32 {
        let i = first + 2 * k;
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 2, i + 3);
    }
}

/// A triangle at `tip`, pointing down (at the top edge) or up (at the bottom edge).
fn triangle(painter: &Painter, tip: Pos2, size: f32, pointing_down: bool, color: Color32) {
    pointer(painter, tip, size, vec2(0.0, if pointing_down { 1.0 } else { -1.0 }), color);
}

/// A triangle with its base on `base` pointing along `direction` (a unit vector).
fn pointer(painter: &Painter, base: Pos2, size: f32, direction: Vec2, color: Color32) {
    let across = vec2(-direction.y, direction.x) * size * 0.7;
    let points = vec![base - across, base + across, base + direction * size];
    let edge = Stroke::new(1.0_f32, Color32::from_black_alpha(170));
    painter.add(Shape::convex_polygon(points, color, edge));
}

/// A hot cue flag: a colored tab with the cue letter, hanging right of the line.
fn flag(painter: &Painter, top_left: Pos2, letter: char, color: Color32, size: f32) {
    let rect = Rect::from_min_size(top_left, vec2(size, size + 1.0));
    painter.rect_filled(rect.expand(1.0), 3.0, Color32::from_black_alpha(170));
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
        FontId::proportional(size * 0.8),
        text,
    );
}

/// The track time under a horizontal pointer position.
pub fn overview_time(rect: Rect, pointer: Pos2, duration: f64) -> f64 {
    f64::from(((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0)) * duration
}

#[cfg(test)]
mod tests {

    /// How colourful a track's High detail waveform is (`ROFLOXBOX_TRACK`):
    /// the share of columns that are one band alone (a channel over 90 % of
    /// the colour), and the average of each channel.
    /// `cargo test -p rofloxbox-app --release waveform_colours -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn waveform_colours() {
        let path = std::env::var("ROFLOXBOX_TRACK").unwrap();
        let audio = rofloxbox_audio::decode_file(std::path::Path::new(&path)).unwrap();
        let data = WaveformData::high_detail(
            &rofloxbox_audio::peak_waveform_at(&audio, HIGH_COLUMNS_PER_SECOND),
            rofloxbox_audio::band_waveform_at(&audio, HIGH_COLUMNS_PER_SECOND),
            &rofloxbox_analysis::waveform::rgb_columns_at(
                &audio.to_f32(),
                audio.sample_rate,
                HIGH_COLUMNS_PER_SECOND,
            ),
            HIGH_COLUMNS_PER_SECOND,
        );
        let raw = rofloxbox_audio::band_waveform_at(&audio, HIGH_COLUMNS_PER_SECOND);
        let n = raw.len() as f32;
        let avg = |k: usize| raw.iter().map(|b| b[k]).sum::<f32>() / n;
        println!("bands mean low {:.2} mid {:.2} high {:.2}", avg(0), avg(1), avg(2));
        // A picture of 8 s from a minute in (`ROFLOXBOX_STRIP`: where, .ppm).
        if let Ok(out) = std::env::var("ROFLOXBOX_STRIP") {
            let (w, h) = (1600usize, 120usize);
            let from = 60 * HIGH_COLUMNS_PER_SECOND as usize;
            let mut img = vec![20u8; w * h * 3];
            for x in 0..w {
                let Some(c) = data.detail.get(from + x * 3) else { break };
                let half = (c.height * (h / 2) as f32) as usize;
                for y in h / 2 - half..h / 2 + half {
                    img[(y * w + x) * 3..][..3].copy_from_slice(&[c.color.r(), c.color.g(), c.color.b()]);
                }
            }
            let mut file = format!("P6 {w} {h} 255\n").into_bytes();
            file.extend(img);
            std::fs::write(out, file).unwrap();
        }
        let loud: Vec<_> = data.detail.iter().filter(|c| c.height > 0.3).collect();
        let one = loud
            .iter()
            .filter(|c| {
                let [r, g, b] = [c.color.r(), c.color.g(), c.color.b()].map(f32::from);
                r.max(g).max(b) > 0.9 * (r + g + b)
            })
            .count();
        let mean = |f: fn(&Column) -> u8| loud.iter().map(|c| f32::from(f(c))).sum::<f32>() / loud.len() as f32;
        println!(
            "one band alone: {:.0} %, mean r {:.0} g {:.0} b {:.0}",
            100.0 * one as f32 / loud.len() as f32,
            mean(|c| c.color.r()),
            mean(|c| c.color.g()),
            mean(|c| c.color.b())
        );
    }

    use super::*;

    #[test]
    fn clicks_map_to_track_time() {
        // 800 points showing 8 s around 60 s: 100 points a second.
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 100.0));
        assert!((detail_time(rect, false, pos2(400.0, 50.0), 60.0, 8.0) - 60.0).abs() < 1e-9);
        assert!((detail_time(rect, false, pos2(500.0, 50.0), 60.0, 8.0) - 61.0).abs() < 1e-9);
        assert!((detail_time(rect, false, pos2(0.0, 50.0), 60.0, 8.0) - 56.0).abs() < 1e-9);
        // Vertical: upcoming music is above the middle.
        let tall = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 800.0));
        assert!((detail_time(tall, true, pos2(50.0, 300.0), 60.0, 8.0) - 61.0).abs() < 1e-9);
    }

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
        let mut data =
            WaveformData::from_peaks(&[0.5, 1.0], vec![[1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]);
        data.shape_from_bands();
        assert!(data.colored && data.detail[0].color.r() > data.detail[0].color.b());
    }

    #[test]
    fn heights_follow_the_bands() {
        // rekordbox's heights dip to nothing where the bands stay loud.
        let mut data = WaveformData::from_peaks(&[1.0, 0.0, 1.0], vec![[0.8, 0.6, 0.0]; 3]);
        data.colored = true;
        let color = data.detail[1].color;
        data.shape_from_bands();
        assert!(data.detail.iter().all(|c| (c.height - 1.0).abs() < 1e-6));
        assert_eq!(data.detail[1].color, color, "rekordbox's colours stay");
    }
}
