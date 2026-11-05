//! The status line at the bottom of the window: the latest message on one
//! line, cut short with "…". Every message, in full and with its time, is
//! in the log window.

use chrono::{DateTime, Local};
use eframe::egui::{self, Align, Layout, RichText, ScrollArea, Sense, ViewportBuilder, ViewportId};

use super::{DeckhandApp, WARN};

/// Messages kept for the log.
const HISTORY: usize = 1000;

struct Message {
    text: String,
    error: bool,
    at: DateTime<Local>,
}

#[derive(Default)]
pub struct StatusBar {
    history: Vec<Message>,
    /// Messages shown last frame, so each is recorded once.
    active: Vec<String>,
    dismissed: bool,
    /// The log window is open.
    pub log_open: bool,
}

impl DeckhandApp {
    /// Current messages: (text, is an error).
    fn current_messages(&self) -> Vec<(String, bool)> {
        let errors = [
            &self.engine_error,
            &self.library_error,
            &self.collection_view.error,
            &self.controller.error,
        ];
        errors
            .into_iter()
            .flatten()
            .map(|e| (e.clone(), true))
            .chain(self.status.iter().map(|s| (s.clone(), false)))
            .collect()
    }

    /// Records new messages; call once per frame.
    pub(super) fn update_status(&mut self) {
        let current = self.current_messages();
        let bar = &mut self.status_bar;
        for (text, error) in &current {
            if !bar.active.contains(text) {
                bar.history.push(Message { text: text.clone(), error: *error, at: Local::now() });
                bar.dismissed = false;
            }
        }
        let excess = bar.history.len().saturating_sub(HISTORY);
        bar.history.drain(..excess);
        bar.active = current.into_iter().map(|(t, _)| t).collect();
    }

    pub(super) fn status_bar(&mut self, ctx: &egui::Context) {
        if self.status_bar.log_open {
            self.log_window(ctx);
        }
        let bar = &self.status_bar;
        let Some(latest) = bar.history.last() else { return };
        if bar.dismissed {
            return;
        }
        let (text, error) = (latest.text.clone(), latest.error);
        let mut open_log = false;
        let mut dismiss = false;
        egui::TopBottomPanel::bottom("status").resizable(false).show(ctx, |ui| {
            // × on the right; the message fills the rest of the line, from the left.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                dismiss =
                    ui.small_button("×").on_hover_text("Hide until the next message").clicked();
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let color = if error { WARN } else { ui.visuals().text_color() };
                    let first = text.lines().next().unwrap_or_default();
                    let line = if text.lines().nth(1).is_some() {
                        format!("{first} …")
                    } else {
                        first.to_owned()
                    };
                    open_log = ui
                        .add(
                            egui::Label::new(RichText::new(line).color(color))
                                .truncate()
                                .sense(Sense::click()),
                        )
                        .on_hover_text("Click for the log: every message in full")
                        .clicked();
                });
            });
        });
        if open_log {
            self.status_bar.log_open = true;
        }
        if dismiss {
            self.status_bar.dismissed = true;
            self.status = None;
        }
    }

    /// Every message of this session, newest first, in its own window.
    fn log_window(&mut self, ctx: &egui::Context) {
        let builder = ViewportBuilder::default()
            .with_title("RofLoxBox — Log")
            .with_inner_size([640.0, 420.0]);
        let bar = &mut self.status_bar;
        let inside = |ctx: &egui::Context, bar: &mut StatusBar| {
            let mut open = true;
            egui::Window::new("Log")
                .open(&mut open)
                .default_size([560.0, 360.0])
                .show(ctx, |ui| log_contents(ui, bar));
            bar.log_open = open;
        };
        // In full screen macOS makes a new window a tab of the app's, hiding
        // the app behind it: then the log is a window inside the app.
        if ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
            inside(ctx, bar);
            return;
        }
        ctx.show_viewport_immediate(ViewportId::from_hash_of("log"), builder, |ctx, class| {
            if class == egui::ViewportClass::Embedded {
                // No separate windows here: a window inside the app.
                inside(ctx, bar);
            } else {
                egui::CentralPanel::default().show(ctx, |ui| log_contents(ui, bar));
                if ctx.input(|i| i.viewport().close_requested()) {
                    bar.log_open = false;
                }
            }
        });
    }
}

fn log_contents(ui: &mut egui::Ui, bar: &mut StatusBar) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} messages", bar.history.len())).weak());
        if ui.button("Copy all").clicked() {
            let all: Vec<String> = bar
                .history
                .iter()
                .map(|m| format!("{}  {}", m.at.format("%Y-%m-%d %H:%M:%S"), m.text))
                .collect();
            ui.ctx().copy_text(all.join("\n"));
        }
        if ui.button("Clear").clicked() {
            bar.history.clear();
        }
    });
    ui.separator();
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for message in bar.history.iter().rev() {
            ui.horizontal_top(|ui| {
                ui.label(
                    RichText::new(message.at.format("%H:%M:%S").to_string()).weak().monospace(),
                )
                .on_hover_text(message.at.format("%Y-%m-%d %H:%M:%S").to_string());
                let color = if message.error { WARN } else { ui.visuals().text_color() };
                ui.add(
                    egui::Label::new(RichText::new(&message.text).color(color))
                        .wrap()
                        .selectable(true),
                );
            });
            ui.separator();
        }
    });
}
