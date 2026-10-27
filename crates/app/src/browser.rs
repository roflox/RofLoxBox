//! The track list shared by the collection and USB browsers: columns the
//! user can choose, sorting by clicking a header, ratings.

use std::cmp::Ordering;

use eframe::egui::{self, Color32, RichText, Sense, Ui, vec2};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};

use crate::deck::parse_key;
use crate::library::format_time;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrackColumn {
    /// Position in the playlist (or collection).
    Number,
    Title,
    Artist,
    Album,
    Genre,
    Label,
    Bpm,
    Key,
    Time,
    Year,
    Rating,
    Plays,
    Added,
    Bitrate,
    Comment,
}

impl TrackColumn {
    pub const ALL: [TrackColumn; 15] = [
        Self::Number,
        Self::Title,
        Self::Artist,
        Self::Album,
        Self::Genre,
        Self::Label,
        Self::Bpm,
        Self::Key,
        Self::Time,
        Self::Year,
        Self::Rating,
        Self::Plays,
        Self::Added,
        Self::Bitrate,
        Self::Comment,
    ];

    pub fn default_set() -> Vec<TrackColumn> {
        vec![
            Self::Number,
            Self::Title,
            Self::Artist,
            Self::Bpm,
            Self::Key,
            Self::Time,
            Self::Genre,
            Self::Rating,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Self::Number => "#",
            Self::Title => "Title",
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::Genre => "Genre",
            Self::Label => "Label",
            Self::Bpm => "BPM",
            Self::Key => "Key",
            Self::Time => "Time",
            Self::Year => "Year",
            Self::Rating => "Rating",
            Self::Plays => "Plays",
            Self::Added => "Added",
            Self::Bitrate => "Bitrate",
            Self::Comment => "Comment",
        }
    }

    fn width(self) -> f32 {
        match self {
            Self::Number => 40.0,
            Self::Title => 240.0,
            Self::Artist | Self::Album | Self::Comment => 170.0,
            Self::Genre | Self::Label => 110.0,
            Self::Rating => 74.0,
            Self::Added => 84.0,
            Self::Bpm | Self::Time | Self::Bitrate => 58.0,
            Self::Key | Self::Year | Self::Plays => 46.0,
        }
    }

    /// Numbers start with the highest when first clicked, text with A.
    fn starts_descending(self) -> bool {
        matches!(self, Self::Rating | Self::Plays | Self::Added)
    }
}

/// How the list is sorted; no sort keeps the playlist's own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub column: TrackColumn,
    pub descending: bool,
}

/// One track as the list shows it.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub id: u32,
    /// Position in the playlist shown (a track can be in it more than once).
    pub entry: Option<usize>,
    /// Position in the list as stored (the playlist, or the collection), for
    /// the # column.
    pub position: usize,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub label: String,
    pub bpm: Option<f64>,
    pub key: Option<String>,
    pub duration: f64,
    pub year: Option<u16>,
    /// 0–5 stars.
    pub rating: u8,
    pub plays: u32,
    /// `YYYY-MM-DD`, so text order is date order.
    pub added: String,
    /// kbit/s.
    pub bitrate: Option<u32>,
    pub comment: String,
}

impl Row {
    fn text(&self, column: TrackColumn) -> String {
        match column {
            TrackColumn::Number => (self.position + 1).to_string(),
            TrackColumn::Title => self.title.clone(),
            TrackColumn::Artist => self.artist.clone(),
            TrackColumn::Album => self.album.clone(),
            TrackColumn::Genre => self.genre.clone(),
            TrackColumn::Label => self.label.clone(),
            TrackColumn::Bpm => self.bpm.map_or_else(String::new, |b| format!("{b:.1}")),
            TrackColumn::Key => self.key.clone().unwrap_or_default(),
            TrackColumn::Time => format_time(self.duration),
            TrackColumn::Year => self.year.map_or_else(String::new, |y| y.to_string()),
            TrackColumn::Rating => String::new(),
            TrackColumn::Plays => self.plays.to_string(),
            TrackColumn::Added => self.added.clone(),
            TrackColumn::Bitrate => self.bitrate.map_or_else(String::new, |b| b.to_string()),
            TrackColumn::Comment => self.comment.clone(),
        }
    }

    fn compare(&self, other: &Row, column: TrackColumn) -> Ordering {
        let text = |a: &str, b: &str| {
            // Empty values go last; letters compare without case.
            match (a.is_empty(), b.is_empty()) {
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ => a.to_lowercase().cmp(&b.to_lowercase()),
            }
        };
        let number = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => a.total_cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        match column {
            TrackColumn::Number => self.position.cmp(&other.position),
            TrackColumn::Bpm => number(self.bpm, other.bpm),
            TrackColumn::Key => number(self.camelot(), other.camelot()),
            TrackColumn::Time => self.duration.total_cmp(&other.duration),
            TrackColumn::Year => number(self.year.map(f64::from), other.year.map(f64::from)),
            TrackColumn::Rating => self.rating.cmp(&other.rating),
            TrackColumn::Plays => self.plays.cmp(&other.plays),
            TrackColumn::Bitrate => {
                number(self.bitrate.map(f64::from), other.bitrate.map(f64::from))
            }
            _ => text(&self.text(column), &other.text(column)),
        }
    }

    /// Camelot wheel position (1A, 1B, 2A … 12B), so keys that mix well
    /// sort next to each other.
    fn camelot(&self) -> Option<f64> {
        let (pitch, minor) = parse_key(self.key.as_deref()?)?;
        // 8A = A minor (9), 8B = C major (0); each number is a fifth up.
        let base = if minor { 9 } else { 0 };
        let number = ((pitch - base) * 7).rem_euclid(12) + 8;
        let number = (number - 1) % 12 + 1;
        Some(f64::from(number) * 2.0 + if minor { 0.0 } else { 1.0 })
    }
}

/// Sorts rows in place (stable, so equal rows keep the playlist order).
pub fn sort_rows(rows: &mut [Row], sort: Option<Sort>) {
    let Some(sort) = sort else { return };
    rows.sort_by(|a, b| {
        let order = a.compare(b, sort.column);
        if sort.descending { order.reverse() } else { order }
    });
}

/// What happened in the list this frame.
#[derive(Default)]
pub struct TableOutput {
    /// (track, position in the list).
    pub clicked: Option<(u32, usize)>,
    pub double_clicked: Option<u32>,
    /// (track, stars) set by clicking the rating.
    pub rated: Option<(u32, u8)>,
    /// The sort or the shown columns changed (worth saving).
    pub layout_changed: bool,
}

pub struct TableOptions<'a> {
    pub selected: Option<u32>,
    /// The selected row's position, for duplicates (`None`: first copy).
    pub selected_position: Option<usize>,
    /// Scroll so the selected row is visible (after the controller moved it).
    pub reveal_selected: bool,
    /// Track loaded on each deck, for the `[1]` markers.
    pub loaded: &'a [Option<u32>],
    /// Rows can be dragged (onto playlists); the payload is the track id.
    pub draggable: bool,
    pub editable_rating: bool,
}

/// The track list. `context` fills a row's right-click menu.
pub fn track_table(
    ui: &mut Ui,
    rows: &[Row],
    columns: &mut Vec<TrackColumn>,
    sort: &mut Option<Sort>,
    options: &TableOptions<'_>,
    context: &mut dyn FnMut(&mut Ui, &Row),
) -> TableOutput {
    let mut output = TableOutput::default();
    if columns.is_empty() {
        columns.push(TrackColumn::Title);
    }
    let shown = columns.clone();
    let mut table = TableBuilder::new(ui)
        .striped(true)
        .sense(if options.draggable { Sense::click_and_drag() } else { Sense::click() })
        .resizable(true);
    for (i, column) in shown.iter().enumerate() {
        table = table.column(if i == 0 {
            Column::initial(column.width()).at_least(column.width().min(120.0)).clip(true)
        } else if i == shown.len() - 1 {
            Column::remainder().at_least(column.width().min(60.0)).clip(true)
        } else {
            Column::initial(column.width()).at_least(30.0).clip(true)
        });
    }
    // The selected row; with duplicates, the copy at the selected position.
    let selected_row = options.selected.and_then(|s| {
        rows.iter()
            .position(|r| r.id == s && Some(r.position) == options.selected_position)
            .or_else(|| rows.iter().position(|r| r.id == s))
    });
    if let Some(row) = selected_row.filter(|_| options.reveal_selected) {
        table = table.scroll_to_row(row, None);
    }
    table
        .header(20.0, |mut header| {
            for &column in &shown {
                header.col(|ui| {
                    let direction = sort.filter(|s| s.column == column).map(|s| s.descending);
                    let response = ui
                        .horizontal(|ui| {
                            let label = ui.add(
                                egui::Label::new(RichText::new(column.label()).strong())
                                    .sense(Sense::click_and_drag()),
                            );
                            if let Some(descending) = direction {
                                sort_arrow(ui, descending);
                            }
                            label
                        })
                        .inner
                        .on_hover_text("Click to sort; again to reverse; a third time for playlist order. Drag to move the column. Right-click to choose columns.");
                    // Dragging a header onto another moves it there (in front
                    // of it, or behind it on its right half).
                    response.dnd_set_drag_payload(column);
                    let cell = ui.max_rect();
                    let target = ui.interact(cell, ui.id().with(("column drop", column)), Sense::hover());
                    let pointer = ui.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));
                    let after = pointer.is_some_and(|p| p.x > cell.center().x);
                    if target.dnd_hover_payload::<TrackColumn>().is_some_and(|d| *d != column) {
                        let x = if after { cell.right() } else { cell.left() };
                        ui.painter().vline(x, cell.y_range(), egui::Stroke::new(2.0_f32, ui.visuals().selection.bg_fill));
                    }
                    if let Some(dragged) = target.dnd_release_payload::<TrackColumn>()
                        && *dragged != column
                    {
                        columns.retain(|c| c != &*dragged);
                        let at = columns.iter().position(|c| *c == column).map_or(columns.len(), |i| i + usize::from(after));
                        columns.insert(at, *dragged);
                        output.layout_changed = true;
                    }
                    if response.clicked() {
                        // Unsorted → first direction → reversed → unsorted.
                        *sort = match *sort {
                            Some(s) if s.column == column && s.descending != column.starts_descending() => None,
                            Some(s) if s.column == column => Some(Sort { column, descending: !s.descending }),
                            _ => Some(Sort { column, descending: column.starts_descending() }),
                        };
                        output.layout_changed = true;
                    }
                    response.context_menu(|ui| {
                        ui.label(RichText::new("Columns").small().weak());
                        for candidate in TrackColumn::ALL {
                            let mut on = columns.contains(&candidate);
                            if ui.checkbox(&mut on, candidate.label()).changed() {
                                if on {
                                    columns.push(candidate);
                                } else if columns.len() > 1 {
                                    columns.retain(|c| *c != candidate);
                                }
                                output.layout_changed = true;
                            }
                        }
                        if ui.button("Reset columns").clicked() {
                            *columns = TrackColumn::default_set();
                            output.layout_changed = true;
                            ui.close();
                        }
                    });
                });
            }
        })
        .body(|body| {
            body.rows(20.0, rows.len(), |mut table_row| {
                let row = &rows[table_row.index()];
                table_row.set_selected(selected_row == Some(table_row.index()));
                for &column in &shown {
                    table_row.col(|ui| {
                        if column == TrackColumn::Rating {
                            if let Some(stars) = rating(ui, row.rating, options.editable_rating) {
                                output.rated = Some((row.id, stars));
                            }
                            return;
                        }
                        let mut text = row.text(column);
                        if column == TrackColumn::Title {
                            let marker: String = (0..options.loaded.len())
                                .filter(|&d| options.loaded[d] == Some(row.id))
                                .map(|d| format!("[{}] ", d + 1))
                                .collect();
                            text = marker + &text;
                        }
                        ui.add(egui::Label::new(text).truncate());
                    });
                }
                let response = table_row.response();
                if options.draggable {
                    response.dnd_set_drag_payload(row.id);
                }
                if response.clicked() || response.drag_started() {
                    output.clicked = Some((row.id, row.position));
                }
                if response.double_clicked() {
                    output.double_clicked = Some(row.id);
                }
                response.context_menu(|ui| context(ui, row));
            });
        });
    output
}

/// A small triangle pointing down (descending) or up.
fn sort_arrow(ui: &mut Ui, descending: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, 9.0), Sense::hover());
    let (top, bottom) = (rect.top() + 1.5, rect.bottom() - 1.5);
    let (left, right, middle) = (rect.left() + 0.5, rect.right() - 0.5, rect.center().x);
    let points = if descending {
        vec![egui::pos2(left, top), egui::pos2(right, top), egui::pos2(middle, bottom)]
    } else {
        vec![egui::pos2(left, bottom), egui::pos2(right, bottom), egui::pos2(middle, top)]
    };
    let color = ui.visuals().strong_text_color();
    ui.painter().add(egui::Shape::convex_polygon(points, color, egui::Stroke::NONE));
}

/// Five dots, filled up to the rating; returns a new rating when clicked
/// (clicking the current one clears it).
fn rating(ui: &mut Ui, stars: u8, editable: bool) -> Option<u8> {
    let size = vec2(64.0, 14.0);
    let sense = if editable { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let painter = ui.painter_at(rect);
    let step = rect.width() / 5.0;
    let hover = response
        .hover_pos()
        .filter(|_| editable)
        .map(|p| ((p.x - rect.left()) / step).floor() as u8 + 1);
    for i in 0..5u8 {
        let center = egui::pos2(rect.left() + step * (f32::from(i) + 0.5), rect.center().y);
        let lit = hover.map_or(i < stars, |h| i < h);
        let color = if lit { Color32::from_rgb(240, 190, 60) } else { Color32::from_gray(70) };
        if lit {
            painter.circle_filled(center, 4.0, color);
        } else {
            painter.circle_stroke(center, 3.5, egui::Stroke::new(1.0_f32, color));
        }
    }
    if response.clicked() {
        let clicked = hover.unwrap_or(0).min(5);
        return Some(if clicked == stars { 0 } else { clicked });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u32, title: &str, bpm: Option<f64>, key: &str) -> Row {
        Row { id, title: title.into(), bpm, key: Some(key.into()), ..Row::default() }
    }

    #[test]
    fn sorts_by_column_and_direction() {
        let mut rows = vec![
            row(1, "beta", Some(128.0), "Am"),
            row(2, "Alpha", None, "C"),
            row(3, "gamma", Some(120.0), "Em"),
        ];
        let ids = |rows: &[Row]| rows.iter().map(|r| r.id).collect::<Vec<_>>();
        sort_rows(&mut rows, Some(Sort { column: TrackColumn::Title, descending: false }));
        assert_eq!(ids(&rows), [2, 1, 3], "case-insensitive");
        sort_rows(&mut rows, Some(Sort { column: TrackColumn::Bpm, descending: false }));
        assert_eq!(ids(&rows), [3, 1, 2], "missing BPM last");
        // Camelot: Am = 8A, C = 8B, Em = 9A.
        sort_rows(&mut rows, Some(Sort { column: TrackColumn::Key, descending: false }));
        assert_eq!(ids(&rows), [1, 2, 3]);
        sort_rows(&mut rows, Some(Sort { column: TrackColumn::Key, descending: true }));
        assert_eq!(ids(&rows), [3, 2, 1]);
    }

    #[test]
    fn camelot_numbers() {
        let c = |k: &str| row(0, "", None, k).camelot().unwrap();
        assert_eq!(c("Am"), 16.0); // 8A
        assert_eq!(c("C"), 17.0); // 8B
        assert_eq!(c("Abm"), 2.0); // 1A
        assert_eq!(c("B"), 3.0); // 1B
        assert_eq!(c("Dbm"), 24.0); // 12A
    }

    #[test]
    fn number_column_restores_list_order() {
        let mut rows: Vec<Row> = [(2, "c"), (0, "b"), (1, "a")]
            .map(|(position, title)| Row { position, title: title.into(), ..Row::default() })
            .into();
        sort_rows(&mut rows, Some(Sort { column: TrackColumn::Number, descending: false }));
        assert_eq!(rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["b", "a", "c"]);
        assert_eq!(rows[0].text(TrackColumn::Number), "1");
    }
}
