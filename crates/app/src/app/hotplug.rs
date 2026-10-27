//! Noticing controllers being plugged in and out while the app runs.

use std::time::{Duration, Instant};

use deckhand_control::input_ports;
use eframe::egui;

use super::DeckhandApp;
use crate::config::mapping_path;

/// How often the MIDI ports are listed.
const CHECK_EVERY: Duration = Duration::from_secs(2);

/// Ports that look like DJ controllers come first when several appear.
fn looks_like_controller(port: &str) -> bool {
    ["GRV", "DDJ", "AlphaTheta", "Pioneer", "XDJ", "DJM", "CDJ"].iter().any(|k| port.contains(k))
}

#[derive(Default)]
pub struct Hotplug {
    /// Ports present at the last check.
    known: Vec<String>,
    checked: Option<Instant>,
    /// A port the user disconnected by hand: not reconnected until it is
    /// unplugged and plugged in again.
    declined: Option<String>,
}

impl DeckhandApp {
    /// Connects a controller: quietly if it has a saved mapping, otherwise
    /// (a new controller) with the Controller window open to check it.
    pub(super) fn connect_controller(&mut self, port: &str, ctx: &egui::Context) {
        let known = mapping_path(port).is_file();
        self.controller.open(port, ctx);
        if self.controller.port().is_none() {
            return;
        }
        self.config.midi_port = Some(port.to_owned());
        self.config.save();
        if known {
            self.status = Some(format!("Controller connected: {port}"));
        } else {
            self.show_controller = true;
            self.status = Some(format!(
                "New controller: {port} — check its mapping in the Controller window"
            ));
        }
    }

    /// The user disconnected the controller by hand.
    pub(super) fn decline_controller(&mut self) {
        self.hotplug.declined = self.controller.port().map(str::to_owned);
        self.controller.close();
    }

    /// Notices controllers being plugged in or out; call every frame.
    pub(super) fn check_controllers(&mut self, ctx: &egui::Context) {
        if std::env::var_os("DECKHAND_MIDI_PORT").is_some() {
            return; // tests pick their port
        }
        // Test aid: only ports whose name contains this count.
        let only = std::env::var("DECKHAND_PORT_FILTER").ok();
        if self.hotplug.checked.is_some_and(|t| t.elapsed() < CHECK_EVERY) {
            return;
        }
        self.hotplug.checked = Some(Instant::now());
        ctx.request_repaint_after(CHECK_EVERY);
        let ports: Vec<String> = input_ports()
            .into_iter()
            .filter(|p| only.as_ref().is_none_or(|f| p.contains(f.as_str())))
            .collect();
        if let Some(port) = self.controller.port().map(str::to_owned)
            && !ports.contains(&port)
        {
            self.controller.close();
            self.status = Some(format!("Controller disconnected: {port}"));
        }
        if self.hotplug.declined.as_ref().is_some_and(|d| !ports.contains(d)) {
            self.hotplug.declined = None;
        }
        let mut new: Vec<String> = ports
            .iter()
            .filter(|p| {
                !self.hotplug.known.contains(p) && self.hotplug.declined.as_ref() != Some(*p)
            })
            .cloned()
            .collect();
        new.sort_by_key(|p| !looks_like_controller(p));
        self.hotplug.known = ports;
        if self.controller.port().is_none()
            && let Some(port) = new.first().cloned()
        {
            self.connect_controller(&port, ctx);
        }
    }

    /// Remembers the ports present at start-up, so only later ones count as
    /// newly plugged in.
    pub(super) fn note_ports(&mut self, ports: Vec<String>) {
        self.hotplug.known = ports;
        self.hotplug.checked = Some(Instant::now());
    }
}
