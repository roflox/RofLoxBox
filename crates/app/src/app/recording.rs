//! Recording the master output: the REC button and where files go.

use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};

use super::{DeckhandApp, WARN};

/// Default folder: `Music/RofLoxBox Recordings` in the home folder.
pub fn default_recordings_dir() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Music").join(format!("{} Recordings", crate::APP_NAME))
}

impl DeckhandApp {
    fn recordings_dir(&self) -> PathBuf {
        self.config.recordings_dir.clone().unwrap_or_else(default_recordings_dir)
    }

    pub(super) fn toggle_recording(&mut self) {
        let dir = self.recordings_dir();
        let Some(engine) = self.engine.as_mut() else {
            self.status = Some("No audio output to record".into());
            return;
        };
        match self.recording.take() {
            Some(recording) => match engine.stop_recording(recording) {
                Ok(path) => self.status = Some(format!("Recording saved: {}", path.display())),
                Err(err) => self.status = Some(format!("Recording failed: {err:#}")),
            },
            None => {
                let name = format!(
                    "{} {}.wav",
                    crate::APP_NAME,
                    chrono::Local::now().format("%Y-%m-%d %H-%M-%S")
                );
                let path = dir.join(name);
                match engine.start_recording(&path) {
                    Ok(recording) => {
                        self.status = Some(format!("Recording to {}", path.display()));
                        self.recording = Some(recording);
                    }
                    Err(err) => self.status = Some(format!("Could not start recording: {err:#}")),
                }
            }
        }
    }

    /// Finishes a recording before the audio output goes away.
    pub(super) fn finish_recording(&mut self) {
        if self.recording.is_some() {
            self.toggle_recording();
        }
    }

    /// The REC button with the time recorded.
    pub(super) fn record_button(&mut self, ui: &mut Ui) {
        let recording = self.recording.as_ref();
        let label = match recording {
            Some(r) => {
                let secs = r.duration().as_secs();
                format!("REC {}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
            }
            None => "REC".to_owned(),
        };
        let red = egui::Color32::from_rgb(230, 50, 50);
        let mut response = crate::widgets::lit_button(ui, &label, recording.is_some(), red);
        let dropped = recording.map_or(0, |r| r.dropped_frames());
        if dropped > 0 {
            ui.label(RichText::new(format!("{dropped} frames lost")).small().color(WARN))
                .on_hover_text("The disk could not keep up; try another folder or drive");
        }
        response = response.on_hover_text(match recording {
            Some(r) => format!("Recording the master to {}; click to stop", r.path().display()),
            None => format!("Record the master output to {}", self.recordings_dir().display()),
        });
        if response.clicked() {
            self.toggle_recording();
        }
        if self.recording.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    /// The recording part of the Audio preferences.
    pub(super) fn recording_prefs(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new("Recording").strong().size(15.0));
        ui.horizontal(|ui| {
            ui.label("Folder");
            ui.label(RichText::new(self.recordings_dir().display().to_string()).monospace());
        });
        ui.horizontal(|ui| {
            if ui.button("Change…").clicked()
                && let Some(dir) =
                    rfd::FileDialog::new().set_title("Folder for recordings").pick_folder()
            {
                self.config.recordings_dir = Some(dir);
                self.config.save();
            }
            if ui.button("Open folder").clicked() {
                let dir = self.recordings_dir();
                let _ = std::fs::create_dir_all(&dir);
                let opener = if cfg!(windows) { "explorer" } else { "open" };
                let _ = std::process::Command::new(opener).arg(&dir).spawn();
            }
            if self.config.recordings_dir.is_some() && ui.button("Default").clicked() {
                self.config.recordings_dir = None;
                self.config.save();
            }
        });
        ui.label(
            RichText::new(
                "The master output as you hear it, as a 24-bit WAV at the output's sample rate.",
            )
            .small()
            .weak(),
        );
    }
}
