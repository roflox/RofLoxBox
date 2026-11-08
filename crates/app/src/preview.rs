//! The Preview column: each track's whole waveform, small, with its cues.
//!
//! From rekordbox's analysis when the track has it, else worked out from
//! the audio once (in the background, the rows on screen first) and kept
//! in a cache folder.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use deckhand_formats::anlz::{AnlzFile, CueListKind};
use eframe::egui::{self, Color32, Painter, Rect, Stroke, pos2};

use crate::waveform::{Column, WaveformData};

/// Columns across a preview.
const COLUMNS: usize = 400;
/// Previews kept in memory; past that, the cache starts again.
const KEEP: usize = 5000;
/// Requests waiting; the newest go first, the oldest are dropped.
const WAITING: usize = 100;
const MEMORY_CUE: Color32 = Color32::from_rgb(230, 50, 50);

/// Where a track's preview comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    pub audio: PathBuf,
    /// rekordbox's analysis (`.DAT`, `.EXT` beside it).
    pub analysis: Option<PathBuf>,
}

/// A cue on a preview: seconds, colour, and whether it is a hot cue.
pub type Mark = (f64, Color32, bool);

/// A track's preview.
#[derive(Debug, Default)]
pub struct Preview {
    pub columns: Vec<Column>,
    /// Cues from rekordbox's analysis (lists with their own cues use those).
    pub cues: Vec<Mark>,
    /// The track's length the columns cover, in seconds (0 if unknown).
    pub seconds: f64,
}

struct Previews {
    done: HashMap<Source, Option<Arc<Preview>>>,
    asked: HashSet<Source>,
    queue: Arc<(Mutex<Vec<Source>>, Condvar)>,
    results: Receiver<(Source, Option<Preview>)>,
}

fn previews(ctx: &egui::Context) -> &'static Mutex<Previews> {
    static PREVIEWS: OnceLock<Mutex<Previews>> = OnceLock::new();
    PREVIEWS.get_or_init(|| {
        let queue: Arc<(Mutex<Vec<Source>>, Condvar)> = Arc::default();
        let (tx, results) = channel();
        // One thread: decoding is heavy, and the decks come first.
        for n in 0..1 {
            let (queue, tx, ctx) = (queue.clone(), tx.clone(), ctx.clone());
            let _ = std::thread::Builder::new()
                .name(format!("previews {n}"))
                .spawn(move || work(&queue, &tx, &ctx));
        }
        Mutex::new(Previews { done: HashMap::new(), asked: HashSet::new(), queue, results })
    })
}

fn work(
    queue: &(Mutex<Vec<Source>>, Condvar),
    results: &Sender<(Source, Option<Preview>)>,
    ctx: &egui::Context,
) {
    loop {
        let source = {
            let mut waiting = queue.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(source) = waiting.pop() {
                    break source;
                }
                waiting = queue.1.wait(waiting).unwrap_or_else(|e| e.into_inner());
            }
        };
        let preview = make(&source);
        if results.send((source, preview)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// The track's preview, once it is ready (asked for the first time, it is
/// made in the background and the list repaints when it is in).
pub fn get(ctx: &egui::Context, source: &Source) -> Option<Arc<Preview>> {
    let mut previews = previews(ctx).lock().ok()?;
    while let Ok((done, preview)) = previews.results.try_recv() {
        previews.asked.remove(&done);
        if previews.done.len() >= KEEP {
            previews.done.clear();
        }
        previews.done.insert(done, preview.map(Arc::new));
    }
    if let Some(preview) = previews.done.get(source) {
        return preview.clone();
    }
    if previews.asked.insert(source.clone()) {
        let mut waiting = previews.queue.0.lock().unwrap_or_else(|e| e.into_inner());
        waiting.push(source.clone());
        let dropped: Vec<Source> = if waiting.len() > WAITING {
            let extra = waiting.len() - WAITING;
            waiting.drain(..extra).collect()
        } else {
            Vec::new()
        };
        drop(waiting);
        for source in dropped {
            previews.asked.remove(&source);
        }
        previews.queue.1.notify_one();
    }
    None
}

/// Forgets a track's preview (its audio or analysis changed).
#[allow(dead_code)]
pub fn forget(ctx: &egui::Context, source: &Source) {
    if let Ok(mut previews) = previews(ctx).lock() {
        previews.done.remove(source);
    }
}

fn make(source: &Source) -> Option<Preview> {
    from_analysis(source).or_else(|| from_cache(&source.audio)).or_else(|| {
        let preview = from_audio(&source.audio)?;
        save_cache(&source.audio, &preview);
        Some(preview)
    })
}

/// From rekordbox's colour detail (PWV5) or monochrome one, with the cues
/// of its `.EXT` (or `.DAT`).
fn from_analysis(source: &Source) -> Option<Preview> {
    let dat_path = source.analysis.as_ref()?;
    let dat = AnlzFile::open(dat_path).ok();
    let ext = AnlzFile::open(dat_path.with_extension("EXT")).ok();
    let data = WaveformData::from_analysis(ext.as_ref(), None)?;
    let seconds = data.detail.len() as f64 / data.columns_per_second();
    let mut cues = Vec::new();
    // The .EXT's extended lists carry colours; the .DAT's are the same cues.
    let lists = ext.iter().chain(dat.iter()).find(|f| f.cue_lists().next().is_some());
    for list in lists.into_iter().flat_map(AnlzFile::cue_lists) {
        for cue in &list.cues {
            let t = f64::from(cue.time_ms) / 1000.0;
            match (list.kind, cue.hot_cue) {
                (CueListKind::Hot, Some(n)) if n > 0 => {
                    let color = cue.hot_cue_color.map_or(crate::deck::HOT_CUE_DEFAULT, |c| {
                        Color32::from_rgb(c.rgb[0], c.rgb[1], c.rgb[2])
                    });
                    cues.push((t, color, true));
                }
                (CueListKind::Memory, _) => cues.push((t, MEMORY_CUE, false)),
                _ => {}
            }
        }
    }
    cues.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-3 && a.2 == b.2);
    Some(Preview { columns: shrink(&data.detail), cues, seconds })
}

/// Worked out from the audio, coloured by its bands like the decks'.
fn from_audio(path: &Path) -> Option<Preview> {
    let audio = deckhand_audio::decode_file(path).ok()?;
    let mut data = WaveformData::from_peaks(
        &deckhand_audio::peak_waveform(&audio),
        deckhand_audio::band_waveform(&audio),
    );
    data.shape_from_bands();
    let seconds = audio.frames() as f64 / f64::from(audio.sample_rate);
    Some(Preview { columns: shrink(&data.detail), cues: Vec::new(), seconds })
}

/// [`COLUMNS`] columns, each the loudest of its stretch.
fn shrink(detail: &[Column]) -> Vec<Column> {
    if detail.is_empty() {
        return Vec::new();
    }
    (0..COLUMNS)
        .map(|i| {
            let from = i * detail.len() / COLUMNS;
            let to = ((i + 1) * detail.len() / COLUMNS).max(from + 1).min(detail.len());
            *detail[from..to]
                .iter()
                .max_by(|a, b| a.height.total_cmp(&b.height))
                .unwrap_or(&detail[from.min(detail.len() - 1)])
        })
        .collect()
}

/// The cache file for an audio file: its path, size and modification time.
fn cache_path(audio: &Path) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    audio.hash(&mut hasher);
    deckhand_library::file_stamp(audio).hash(&mut hasher);
    deckhand_library::data_dir().join("previews").join(format!("{:016x}.bin", hasher.finish()))
}

/// The seconds (f64), then height, red, green, blue per column.
fn from_cache(audio: &Path) -> Option<Preview> {
    let bytes = std::fs::read(cache_path(audio)).ok()?;
    let (seconds, columns) = bytes.split_first_chunk::<8>()?;
    let columns = columns
        .chunks_exact(4)
        .map(|c| Column {
            height: f32::from(c[0]) / 255.0,
            color: Color32::from_rgb(c[1], c[2], c[3]),
        })
        .collect();
    Some(Preview { columns, cues: Vec::new(), seconds: f64::from_le_bytes(*seconds) })
}

fn save_cache(audio: &Path, preview: &Preview) {
    let path = cache_path(audio);
    let mut bytes = preview.seconds.to_le_bytes().to_vec();
    for c in &preview.columns {
        bytes.extend([
            (c.height.clamp(0.0, 1.0) * 255.0) as u8,
            c.color.r(),
            c.color.g(),
            c.color.b(),
        ]);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, bytes);
}

/// Draws a preview into `rect` with `cues` (hot cues as coloured lines with
/// a tab, memory cues as red ticks at the bottom) over `seconds`.
pub fn draw(painter: &Painter, rect: Rect, preview: &Preview, cues: &[Mark], seconds: f64) {
    let n = preview.columns.len();
    if n == 0 {
        return;
    }
    let width = rect.width();
    let per = width / n as f32;
    for (i, column) in preview.columns.iter().enumerate() {
        let x = rect.left() + (i as f32 + 0.5) * per;
        let h = column.height * (rect.height() - 1.0);
        painter.line_segment(
            [pos2(x, rect.bottom() - h), pos2(x, rect.bottom())],
            Stroke::new(per.max(1.0), column.color),
        );
    }
    let seconds = if seconds > 0.0 { seconds } else { preview.seconds };
    if seconds <= 0.0 {
        return;
    }
    let x_of = |t: f64| rect.left() + (t / seconds).clamp(0.0, 1.0) as f32 * width;
    for &(t, color, hot) in cues {
        let x = x_of(t);
        if hot {
            painter.line_segment(
                [pos2(x, rect.top()), pos2(x, rect.bottom())],
                Stroke::new(1.0_f32, color),
            );
            painter.rect_filled(
                Rect::from_min_max(pos2(x, rect.top()), pos2(x + 3.0, rect.top() + 3.0)),
                0.0,
                color,
            );
        } else {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    pos2(x - 2.5, rect.bottom()),
                    pos2(x + 2.5, rect.bottom()),
                    pos2(x, rect.bottom() - 4.0),
                ],
                color,
                Stroke::NONE,
            ));
        }
    }
}

/// A collection track's cues as marks.
pub fn track_marks(track: &deckhand_library::Track) -> Vec<Mark> {
    track
        .cues
        .iter()
        .map(|cue| match cue.kind {
            deckhand_library::CueKind::Hot(_) => {
                let color = cue
                    .color
                    .map_or(crate::deck::HOT_CUE_DEFAULT, |[r, g, b]| Color32::from_rgb(r, g, b));
                (cue.start, color, true)
            }
            deckhand_library::CueKind::Memory => {
                let color = cue.color.map_or(MEMORY_CUE, |[r, g, b]| Color32::from_rgb(r, g, b));
                (cue.start, color, false)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rekordbox export's analysis gives a full preview with its cues.
    #[test]
    fn preview_from_rekordbox_analysis() {
        let dat = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/formats/tests/fixtures/complete_export/demo_tracks/PIONEER/USBANLZ/P016/0000875E/ANLZ0000.DAT");
        let source = Source { audio: PathBuf::from("missing.mp3"), analysis: Some(dat) };
        let preview = from_analysis(&source).expect("a preview");
        assert_eq!(preview.columns.len(), COLUMNS);
        assert!(preview.seconds > 10.0, "{}", preview.seconds);
        assert!(preview.columns.iter().any(|c| c.height > 0.5));
        assert!(preview.cues.iter().all(|c| c.0 >= 0.0 && c.0 <= preview.seconds));
    }

    #[test]
    fn shrink_keeps_the_loudest() {
        let detail: Vec<Column> = (0..1000)
            .map(|i| Column { height: if i == 7 { 1.0 } else { 0.1 }, color: Color32::WHITE })
            .collect();
        let columns = shrink(&detail);
        assert_eq!(columns.len(), COLUMNS);
        // Column 3 covers 7..10.
        assert_eq!(columns[2].height, 0.1);
        assert_eq!(columns[3].height, 1.0);
    }
}
