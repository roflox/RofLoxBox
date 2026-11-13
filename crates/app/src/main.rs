//! deckhand: a DJ application for rekordbox USB exports.

// Release builds on Windows open no console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// The name shown to users (the code and settings folder keep "deckhand").
pub const APP_NAME: &str = "RofLoxBox";
/// 0.<number of commits>.0 (see build.rs).
pub const VERSION: &str = env!("ROFLOXBOX_VERSION");

mod analyze;
mod app;
mod artwork;
mod browser;
mod config;
mod controller;
mod deck;
mod effects;
mod layout;
mod library;
mod parts;
mod prefs_view;
mod preview;
mod rekordbox_db;
mod rekordbox_save;
mod screen_layout;
mod stems;
mod tour;
mod usb_export;
mod usb_import;
mod waveform;
mod widgets;

/// The app icon for the window, taskbar and Dock (packaging/icon.svg).
fn window_icon() -> eframe::egui::IconData {
    let png = include_bytes!("../../../packaging/icons/RofLoxBox-256.png");
    eframe::icon_data::from_png_bytes(png).unwrap_or_default()
}

fn main() -> eframe::Result {
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 920.0])
        .with_title(format!("{APP_NAME} {VERSION}"))
        .with_icon(window_icon());
    // Test aid: start in full screen.
    if std::env::var_os("DECKHAND_FULLSCREEN").is_some() {
        viewport = viewport.with_fullscreen(true);
    }
    // Test aid: open the window at "x,y" (for example on another monitor).
    if let Some((x, y)) = std::env::var("DECKHAND_WINDOW_POS")
        .ok()
        .and_then(|p| {
            p.split_once(',').map(|(x, y)| (x.trim().parse::<f32>(), y.trim().parse::<f32>()))
        })
        .and_then(|(x, y)| Some((x.ok()?, y.ok()?)))
    {
        viewport = viewport.with_position([x, y]);
    }
    // Metal on macOS: OpenGL there is translated to Metal on the CPU.
    #[cfg(target_os = "macos")]
    let renderer = eframe::Renderer::Wgpu;
    #[cfg(not(target_os = "macos"))]
    let renderer = eframe::Renderer::Glow;
    let options = eframe::NativeOptions { viewport, renderer, ..Default::default() };
    eframe::run_native(APP_NAME, options, Box::new(|cc| Ok(Box::new(app::DeckhandApp::new(cc)))))
}
