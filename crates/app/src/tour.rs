//! Guided tours: steps written as TOML (`crates/app/tours/*.toml`, or
//! extra ones in the data folder's `tours/`), each pointing at a UI
//! element by name. Widgets become targets with `.tour(Target::…)`.
//!
//! ```toml
//! id = "first-start"
//! name = "First start"
//! auto = true                      # runs once by itself
//!
//! [[step]]
//! target = "preferences"           # a `Target`, in kebab-case
//! title = "Your rekordbox library"
//! text = "Shown as a paragraph. A blank line starts another."
//! warn = "Shown in orange, with ⚠."
//! commands = ["brew install pipx"] # each with a Copy button
//! when = ["rekordbox-write-off", { not-prefs-tab = "stems" }]  # else skipped
//! wait = ["performance", "prefs-closed"]  # hidden until all hold
//! action = { label = "Open Library settings", open-prefs = "library" }
//! click-advances = true            # clicking the target = Next (default)
//! ```

use std::collections::HashMap;

use eframe::egui::{self, Color32, Id, Rect, RichText};
use serde::Deserialize;

use crate::prefs_view::PrefsTab;

/// The tours that ship with the app.
const BUILT_IN: &[(&str, &str)] = &[
    ("first-start.toml", include_str!("../tours/first-start.toml")),
    ("stems.toml", include_str!("../tours/stems.toml")),
    ("edit.toml", include_str!("../tours/edit.toml")),
];

/// A UI element a tour step can point at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Target {
    ModePerformance,
    ModeEdit,
    AudioDevice,
    Preferences,
    Log,
    Record,
    Marker,
    DeckCount,
    ViewMenu,
    Controller,
    Stems,
    LibrarySearch,
    RekordboxWrite,
    PrefsDeck,
    PrefsMixer,
    PrefsKeyboard,
    PrefsStems,
    PrefsLibrary,
    PrefsAppearance,
    PrefsAudio,
    StemModels,
    StemPresets,
    LiveStems,
    EditOpen,
    EditSave,
    EditRender,
    EditPlay,
    EditMarkers,
    EditTools,
    EditUndo,
    EditLanes,
    EditRuler,
    EditSnap,
}

impl Target {
    /// The name tour files use.
    pub fn name(self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
    }

    pub fn prefs_tab(tab: PrefsTab) -> Self {
        match tab {
            PrefsTab::Deck => Self::PrefsDeck,
            PrefsTab::Mixer => Self::PrefsMixer,
            PrefsTab::Keyboard => Self::PrefsKeyboard,
            PrefsTab::Stems => Self::PrefsStems,
            PrefsTab::Library => Self::PrefsLibrary,
            PrefsTab::Appearance => Self::PrefsAppearance,
            PrefsTab::Audio => Self::PrefsAudio,
        }
    }
}

/// App state a step can depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Condition {
    Performance,
    Edit,
    PrefsOpen,
    PrefsClosed,
    Recording,
    NotRecording,
    TwoDecks,
    FourDecks,
    RekordboxWriteOn,
    RekordboxWriteOff,
    MacOs,
    Windows,
    Linux,
    /// A session is open in EDIT.
    SessionOpen,
    NoSession,
    /// The open session has recording markers.
    HasMarkers,
    /// That Preferences page is showing (or not).
    PrefsTab(PrefsTab),
    NotPrefsTab(PrefsTab),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tour {
    pub id: String,
    pub name: String,
    /// Runs by itself once (until finished or skipped).
    #[serde(default)]
    pub auto: bool,
    #[serde(rename = "step")]
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Step {
    pub target: Target,
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub warn: Option<String>,
    /// Shell commands, each with a Copy button.
    #[serde(default)]
    pub commands: Vec<String>,
    /// All must hold, else the step is skipped.
    #[serde(default)]
    pub when: Vec<Condition>,
    /// All must hold, else the step waits, hidden.
    #[serde(default)]
    pub wait: Vec<Condition>,
    #[serde(default)]
    pub action: Option<Action>,
    #[serde(default = "yes")]
    pub click_advances: bool,
}

fn yes() -> bool {
    true
}

/// An extra button on a step; it also moves on to the next step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Action {
    pub label: String,
    #[serde(default)]
    pub open_prefs: Option<PrefsTab>,
}

/// Parses one tour file.
pub fn parse(text: &str) -> Result<Tour, String> {
    let tour: Tour = toml::from_str(text).map_err(|e| e.to_string())?;
    if tour.steps.is_empty() {
        return Err("no [[step]]".into());
    }
    Ok(tour)
}

/// The built-in tours plus those in `dir` (a file with a built-in's id
/// replaces it), and the problems with files that did not load.
pub fn load(dir: &std::path::Path) -> (Vec<Tour>, Vec<String>) {
    let mut tours = Vec::new();
    let mut errors = Vec::new();
    for (name, text) in BUILT_IN {
        match parse(text) {
            Ok(t) => tours.push(t),
            Err(e) => errors.push(format!("Tour {name}: {e}")),
        }
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    for path in files {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| parse(&t)) {
            Ok(t) => {
                tours.retain(|o| o.id != t.id);
                tours.push(t);
            }
            Err(e) => errors.push(format!("Tour {name}: {e}")),
        }
    }
    (tours, errors)
}

// ---- Targets ----

#[derive(Clone, Copy)]
struct Seen {
    rect: Rect,
    pass: u64,
    clicked: bool,
}

#[derive(Clone, Default)]
struct Targets(HashMap<Target, Seen>);

fn targets_id() -> Id {
    Id::new("tour targets")
}

/// Makes a widget a tour target.
pub trait Tag {
    fn tour(self, target: Target) -> Self;
}

impl Tag for egui::Response {
    fn tour(self, target: Target) -> Self {
        let seen = Seen { rect: self.rect, pass: self.ctx.cumulative_pass_nr(), clicked: self.clicked() };
        self.ctx.data_mut(|d| {
            d.get_temp_mut_or_default::<Targets>(targets_id()).0.insert(target, seen);
        });
        self
    }
}

/// Where `target` was drawn in this pass or the one before, and whether it
/// was clicked in this one.
fn find(ctx: &egui::Context, target: Target) -> Option<(Rect, bool)> {
    let pass = ctx.cumulative_pass_nr();
    let seen = ctx.data(|d| d.get_temp::<Targets>(targets_id()))?.0.get(&target).copied()?;
    (seen.pass + 1 >= pass).then_some((seen.rect, seen.clicked && seen.pass == pass))
}

/// Test aid (`DECKHAND_TARGETS`): outlines and names every live target.
pub fn show_targets(ctx: &egui::Context) {
    let pass = ctx.cumulative_pass_nr();
    let Some(targets) = ctx.data(|d| d.get_temp::<Targets>(targets_id())) else { return };
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Debug, Id::new("tour target names")));
    let color = Color32::from_rgb(0, 220, 255);
    for (target, seen) in targets.0 {
        if seen.pass + 1 < pass {
            continue;
        }
        painter.rect_stroke(seen.rect, 2.0, egui::Stroke::new(1.0_f32, color), egui::StrokeKind::Outside);
        painter.text(
            seen.rect.left_bottom() + egui::vec2(0.0, 2.0),
            egui::Align2::LEFT_TOP,
            target.name(),
            egui::FontId::monospace(10.0),
            color,
        );
    }
}

// ---- Running ----

/// A tour in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Running {
    pub tour: usize,
    pub step: usize,
}

/// What happened to the shown step this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Still showing, waiting or hidden.
    Showing,
    /// Moved on; the action of the step left, if one was picked.
    Next(Option<PrefsTab>),
    /// Past the last step, or skipped.
    Finished,
}

/// The target of the step `run` is on, or the first after it that applies:
/// the part of the app to bring up for it.
pub fn upcoming(tours: &[Tour], run: Running, holds: impl Fn(Condition) -> bool) -> Option<Target> {
    let tour = tours.get(run.tour)?;
    tour.steps[run.step.min(tour.steps.len())..]
        .iter()
        .find(|s| s.when.iter().all(|&c| holds(c)))
        .map(|s| s.target)
}

/// Shows the current step of `run`, skipping steps whose `when` fails.
pub fn show(
    ctx: &egui::Context,
    tours: &[Tour],
    run: &mut Running,
    holds: impl Fn(Condition) -> bool,
    accent: Color32,
    warn: Color32,
) -> Outcome {
    let Some(tour) = tours.get(run.tour) else { return Outcome::Finished };
    let applies = |s: &Step| s.when.iter().all(|&c| holds(c));
    while tour.steps.get(run.step).is_some_and(|s| !applies(s)) {
        run.step += 1;
    }
    let Some(step) = tour.steps.get(run.step) else { return Outcome::Finished };
    if !step.wait.iter().all(|&c| holds(c)) {
        return Outcome::Showing;
    }
    let Some((rect, clicked)) = find(ctx, step.target) else { return Outcome::Showing };
    if clicked && step.click_advances {
        run.step += 1;
        return Outcome::Next(None);
    }
    // Number among the steps that apply now.
    let shown: Vec<usize> = (0..tour.steps.len()).filter(|&i| applies(&tour.steps[i])).collect();
    let place = shown.iter().position(|&i| i == run.step).unwrap_or(0);
    let last = place + 1 == shown.len();
    let counter = (shown.len() > 1).then(|| format!("{}/{}", place + 1, shown.len()));
    let choice = spotlight(ctx, rect, step, counter.as_deref(), last, accent, warn);
    match choice {
        None => Outcome::Showing,
        Some(Choice::Skip) => Outcome::Finished,
        Some(Choice::Next) => {
            run.step += 1;
            Outcome::Next(None)
        }
        Some(Choice::Action) => {
            run.step += 1;
            Outcome::Next(step.action.as_ref().and_then(|a| a.open_prefs))
        }
    }
}

enum Choice {
    Action,
    Next,
    Skip,
}

/// Dims all but `target`, with a note about it and an arrow to it.
fn spotlight(
    ctx: &egui::Context,
    target: Rect,
    step: &Step,
    counter: Option<&str>,
    last: bool,
    accent: Color32,
    warn: Color32,
) -> Option<Choice> {
    let screen = ctx.content_rect();
    let hole = target.expand(6.0);
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("hint shade")));
    // Dim everything but the target (four bands around it).
    let shade = Color32::from_black_alpha(170);
    for band in [
        Rect::from_min_max(screen.min, egui::pos2(screen.max.x, hole.min.y)),
        Rect::from_min_max(egui::pos2(screen.min.x, hole.max.y), screen.max),
        Rect::from_min_max(egui::pos2(screen.min.x, hole.min.y), egui::pos2(hole.min.x, hole.max.y)),
        Rect::from_min_max(egui::pos2(hole.max.x, hole.min.y), egui::pos2(screen.max.x, hole.max.y)),
    ] {
        painter.rect_filled(band, 0.0, shade);
    }
    painter.rect_stroke(hole, 6.0, egui::Stroke::new(2.0_f32, accent), egui::StrokeKind::Outside);

    // The note goes below the target, or above when it would not fit.
    let id = Id::new("hint note");
    let width = 360.0;
    let gap = 70.0;
    let height = ctx.memory(|m| m.area_rect(id)).map_or(140.0, |r| r.height());
    let below = hole.max.y + gap + height <= screen.max.y - 8.0;
    let left = (hole.center().x - width * 0.5).clamp(screen.min.x + 16.0, screen.max.x - width - 16.0);
    let top = if below { hole.max.y + gap } else { (hole.min.y - gap - height).max(screen.min.y + 8.0) };
    let mut done = None;
    let note = egui::Area::new(id).order(egui::Order::Tooltip).fixed_pos(egui::pos2(left, top)).show(ctx, |ui| {
        egui::Frame::popup(ui.style()).stroke(egui::Stroke::new(1.5_f32, accent)).show(ui, |ui| {
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&step.title).strong().color(accent));
                if let Some(counter) = counter {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(counter).small().weak());
                    });
                }
            });
            for paragraph in step.text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
                ui.label(paragraph);
            }
            for command in &step.commands {
                crate::widgets::command(ui, command);
            }
            if let Some(line) = &step.warn {
                ui.label(RichText::new(format!("⚠ {}", line.trim())).color(warn));
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if let Some(action) = &step.action
                    && ui.button(&action.label).clicked()
                {
                    done = Some(Choice::Action);
                }
                if ui.button(if last { "Got it" } else { "Next" }).clicked() {
                    done = Some(Choice::Next);
                }
                if !last && ui.button("Skip tour").clicked() {
                    done = Some(Choice::Skip);
                }
            });
        });
    });

    // The arrow from the note to the target.
    let rect = note.response.rect;
    let x = hole.center().x;
    let (from, to) = if below {
        (egui::pos2(x.clamp(rect.left() + 24.0, rect.right() - 24.0), rect.top() - 4.0), egui::pos2(x, hole.max.y + 6.0))
    } else {
        (egui::pos2(x.clamp(rect.left() + 24.0, rect.right() - 24.0), rect.bottom() + 4.0), egui::pos2(x, hole.min.y - 6.0))
    };
    painter.line_segment([from, to], egui::Stroke::new(3.0_f32, accent));
    let dir = (to - from).normalized();
    let side = egui::vec2(-dir.y, dir.x);
    painter.add(egui::Shape::convex_polygon(
        vec![to, to - dir * 14.0 + side * 8.0, to - dir * 14.0 - side * 8.0],
        accent,
        egui::Stroke::NONE,
    ));
    if done.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        done = Some(Choice::Skip);
    }
    ctx.request_repaint();
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_tours_load() {
        for (name, text) in BUILT_IN {
            let tour = parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!tour.id.is_empty(), "{name}");
        }
    }

    #[test]
    fn mistakes_are_reported() {
        let unknown = "id='a'\nname='A'\n[[step]]\ntarget='no-such-button'\ntitle='x'\n";
        assert!(parse(unknown).unwrap_err().contains("no-such-button"));
        let typo = "id='a'\nname='A'\n[[step]]\ntarget='log'\ntitle='x'\ntxet='y'\n";
        assert!(parse(typo).is_err());
        assert!(parse("id='a'\nname='A'\nstep=[]\n").is_err());
    }

    #[test]
    fn target_names_match_the_file_names() {
        assert_eq!(Target::RekordboxWrite.name(), "rekordbox-write");
        let tour = parse("id='a'\nname='A'\n[[step]]\ntarget='rekordbox-write'\ntitle='x'\n").unwrap();
        assert_eq!(tour.steps[0].target, Target::RekordboxWrite);
    }
}
