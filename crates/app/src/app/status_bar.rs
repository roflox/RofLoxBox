//! The status line at the bottom of the window: the latest message on one
//! line, expandable to the full text and the recent history.

use std::time::Instant;

use eframe::egui::{self, Align, Layout, RichText, ScrollArea, Sense};

use super::{DeckhandApp, WARN};

/// Messages kept for the expanded view.
const HISTORY: usize = 30;

struct Message {
    text: String,
    error: bool,
    at: Instant,
}

#[derive(Default)]
pub struct StatusBar {
    history: Vec<Message>,
    /// Messages shown last frame, so each is recorded once.
    active: Vec<String>,
    open: bool,
    dismissed: bool,
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
                bar.history.push(Message { text: text.clone(), error: *error, at: Instant::now() });
                bar.dismissed = false;
            }
        }
        let excess = bar.history.len().saturating_sub(HISTORY);
        bar.history.drain(..excess);
        bar.active = current.into_iter().map(|(t, _)| t).collect();
    }

    pub(super) fn status_bar(&mut self, ctx: &egui::Context) {
        let bar = &self.status_bar;
        let Some(latest) = bar.history.last() else { return };
        if bar.dismissed && !bar.open {
            return;
        }
        let (text, error) = (latest.text.clone(), latest.error);
        let mut toggle = false;
        let mut dismiss = false;
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let open = self.status_bar.open;
            // × on the right; the message fills the rest of the line, from the left.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                dismiss =
                    ui.small_button("×").on_hover_text("Hide until the next message").clicked();
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let color = if error { WARN } else { ui.visuals().text_color() };
                    let marker = if open { "−" } else { "+" };
                    let line = text.lines().next().unwrap_or_default();
                    let response = ui
                        .add(
                            egui::Label::new(
                                RichText::new(format!("{marker} {line}")).color(color),
                            )
                            .truncate()
                            .sense(Sense::click()),
                        )
                        .on_hover_text("Click for the full message and earlier ones");
                    toggle = response.clicked();
                });
            });
            if self.status_bar.open {
                ui.separator();
                ScrollArea::vertical().max_height(160.0).stick_to_bottom(true).show(ui, |ui| {
                    for message in &self.status_bar.history {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(ago(message.at)).small().weak().monospace());
                            let color =
                                if message.error { WARN } else { ui.visuals().text_color() };
                            ui.label(RichText::new(&message.text).color(color));
                        });
                    }
                });
            }
        });
        if toggle {
            self.status_bar.open = !self.status_bar.open;
        }
        if dismiss {
            self.status_bar.open = false;
            self.status_bar.dismissed = true;
            self.status = None;
        }
    }
}

/// `12s`, `3m`, `2h` ago.
fn ago(at: Instant) -> String {
    let secs = at.elapsed().as_secs();
    match secs {
        0..60 => format!("{secs:>3}s"),
        60..3600 => format!("{:>3}m", secs / 60),
        _ => format!("{:>3}h", secs / 3600),
    }
}
