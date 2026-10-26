//! deckhand: a DJ application for rekordbox USB exports.

// Release builds on Windows open no console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// The name shown to users (the code and settings folder keep "deckhand").
pub const APP_NAME: &str = "RofLoxBox";

mod app;
mod browser;
mod config;
mod controller;
mod deck;
mod effects;
mod layout;
mod library;
mod prefs_view;
mod waveform;
mod widgets;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 920.0])
            .with_title(APP_NAME),
        ..Default::default()
    };
    eframe::run_native(APP_NAME, options, Box::new(|cc| Ok(Box::new(app::DeckhandApp::new(cc)))))
}
