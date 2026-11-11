//! Recording the master output: the REC button and where files go.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;

use anyhow::Result;

use eframe::egui::{self, RichText, Ui};

use super::{DECKS, DeckhandApp, WARN};

/// Default folder: `Music/RofLoxBox Recordings` in the home folder.
pub fn default_recordings_dir() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Music").join(format!("{} Recordings", crate::APP_NAME))
}

/// A typed name fit for a file name (none when nothing is left).
fn clean_name(typed: &str) -> Option<String> {
    let name: String = typed
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c })
        .collect();
    let name = name.trim().trim_matches('.').trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// `name`, or "name 2", "name 3"… where a recording has it already.
fn free_name(dir: &Path, name: &str) -> String {
    (1..)
        .map(|n| if n == 1 { name.to_owned() } else { format!("{name} {n}") })
        .find(|n| !dir.join(n).exists())
        .unwrap_or_else(|| name.to_owned())
}

/// Gives a finished recording (its folder, WAV and session) `name`;
/// returns the WAV's new path. Sessions find their kept stems in the
/// renamed folder.
fn rename_recording(wav: &Path, name: &str) -> Result<PathBuf> {
    let folder = wav.parent().ok_or_else(|| anyhow::anyhow!("no folder"))?;
    let old = wav.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    if old == name {
        return Ok(wav.to_path_buf());
    }
    // A folder of its own (named like it): that is renamed too.
    let own_folder = folder.file_name().is_some_and(|f| f.to_string_lossy() == old);
    let parent = folder.parent().ok_or_else(|| anyhow::anyhow!("no folder"))?;
    let name = if own_folder {
        free_name(parent, name)
    } else {
        (1..)
            .map(|n| if n == 1 { name.to_owned() } else { format!("{name} {n}") })
            .find(|n| !folder.join(format!("{n}.wav")).exists())
            .unwrap_or_else(|| name.to_owned())
    };
    for ext in ["wav", "session"] {
        let from = folder.join(format!("{old}.{ext}"));
        if from.exists() {
            std::fs::rename(&from, folder.join(format!("{name}.{ext}")))?;
        }
    }
    if !own_folder {
        return Ok(folder.join(format!("{name}.wav")));
    }
    let renamed = parent.join(&name);
    std::fs::rename(folder, &renamed)?;
    Ok(renamed.join(format!("{name}.wav")))
}

impl DeckhandApp {
    pub(super) fn recordings_dir(&self) -> PathBuf {
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
                Ok(path) => {
                    self.status = Some(format!("Recording saved: {}", path.display()));
                    // Now it gets its name (or keeps the date and time).
                    let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    self.naming = Some((path, name));
                }
                Err(err) => self.status = Some(format!("Recording failed: {err:#}")),
            },
            None => {
                // A folder each: the WAV, its session and the stems used;
                // by the date and time until it is named when it stops.
                let name = format!(
                    "{} {}",
                    crate::APP_NAME,
                    chrono::Local::now().format("%Y-%m-%d %H-%M-%S")
                );
                let name = free_name(&dir, &name);
                let path = dir.join(&name).join(format!("{name}.wav"));
                match engine.start_recording(&path) {
                    Ok(recording) => {
                        self.status = Some(format!("Recording to {}", path.display()));
                        self.recording = Some(recording);
                        // The session starts with how sync stands.
                        (self.sync_sent, self.offsets_sent) = (None, [None; DECKS]);
                    }
                    Err(err) => self.status = Some(format!("Could not start recording: {err:#}")),
                }
            }
        }
    }

    /// The recording just stopped gets its name: Enter (or Save) renames its
    /// folder, WAV and session; Esc (or Keep) leaves the date and time.
    fn naming_dialog(&mut self, ctx: &egui::Context) {
        let Some((_, name)) = self.naming.as_mut() else { return };
        let (mut save, mut keep) = (false, ctx.input(|i| i.key_pressed(egui::Key::Escape)));
        egui::Window::new("Name this recording")
            .id(egui::Id::new("name recording"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                let field = ui.add(egui::TextEdit::singleline(name).desired_width(260.0));
                if !field.has_focus() && !field.lost_focus() {
                    field.request_focus();
                }
                save = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.label(
                    RichText::new("Its folder, WAV and session take the name.").small().weak(),
                );
                ui.horizontal(|ui| {
                    save |= ui.button(RichText::new("Save").strong()).clicked();
                    keep |= ui.button("Keep the date and time").clicked();
                });
            });
        if keep {
            self.naming = None;
        } else if save {
            let (path, name) = self.naming.take().expect("open");
            if let Some(name) = clean_name(&name) {
                self.status = Some(match rename_recording(&path, &name) {
                    Ok(path) => format!("Recording saved: {}", path.display()),
                    Err(err) => format!("Kept its first name: {err:#}"),
                });
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
        // Room on the left for the red dot.
        let mut response =
            crate::widgets::lit_button(ui, &format!("    {label}"), recording.is_some(), red);
        // The red dot: steady when ready, blinking while recording.
        let lit = match recording {
            Some(r) => r.duration().as_millis() % 1000 < 600,
            None => true,
        };
        let dot = egui::pos2(response.rect.left() + 11.0, response.rect.center().y);
        let color = if recording.is_some() && lit {
            egui::Color32::from_rgb(255, 40, 40)
        } else if recording.is_some() {
            red.gamma_multiply(0.35)
        } else {
            red
        };
        ui.painter().circle_filled(dot, 4.5, color);
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
        // The recording going on, else the last one, in the file manager.
        let what = if self.recording.is_some() { "the recording going on" } else { "the last recording" };
        let folder = ui
            .button(super::collection_view::reveal_label())
            .on_hover_text(format!("Opens the folder with {what}"));
        if folder.clicked() {
            let dir = self.recordings_dir();
            let shown = match self.recording.as_ref() {
                Some(r) => Some(r.path().to_path_buf()),
                None => latest_recording(&dir),
            };
            match shown {
                Some(path) => super::collection_view::reveal(&path),
                None => self.status = Some(format!("Nothing recorded yet in {}", dir.display())),
            }
        }
        self.naming_dialog(ui.ctx());
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
                "Each recording gets a folder: the master output as you hear it (a 24-bit WAV at \
                 the output's sample rate), a .session file with everything played (the tracks, \
                 every knob, fader, button and jog movement, to play or edit the set again) and \
                 the stems the decks used, as they had them.",
            )
            .small()
            .weak(),
        );
        ui.horizontal(|ui| {
            let button = ui
                .add_enabled(self.replay.is_none(), egui::Button::new("Replay a session…"))
                .on_hover_text(
                    "Plays a recorded session through the mixer again into a new WAV, and \
                     compares it with the recording",
                );
            if button.clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("Session to replay")
                    .set_directory(self.recordings_dir())
                    .add_filter("Session", &["session"])
                    .pick_file()
            {
                self.replay = Some(Replay::start(path));
            }
            if let Some(replay) = &self.replay {
                ui.spinner();
                ui.label(replay.progress());
            }
        });
    }

    /// Reports a finished replay.
    pub(super) fn poll_replay(&mut self, ctx: &egui::Context) {
        let Some(replay) = &self.replay else { return };
        match replay.done.try_recv() {
            Ok(result) => {
                self.status = Some(match result {
                    Ok(message) => message,
                    Err(err) => format!("Replay failed: {err:#}"),
                });
                self.replay = None;
            }
            Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
        }
    }
}

/// A session being played again into a WAV, in the background.
pub(super) struct Replay {
    name: String,
    /// Frames done, and how many there are.
    done_frames: Arc<AtomicU64>,
    frames: Arc<AtomicU64>,
    done: Receiver<Result<String>>,
}

impl Replay {
    fn start(session: PathBuf) -> Self {
        let name = session.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let (done_frames, frames) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (tx, done) = std::sync::mpsc::channel();
        let (progress, total) = (done_frames.clone(), frames.clone());
        std::thread::spawn(move || {
            let _ = tx.send(replay(&session, &progress, &total));
        });
        Self { name, done_frames, frames, done }
    }

    fn progress(&self) -> String {
        let (done, frames) =
            (self.done_frames.load(Ordering::Relaxed), self.frames.load(Ordering::Relaxed));
        match (done * 100).checked_div(frames) {
            Some(percent) => format!("Replaying {}… {percent}%", self.name),
            None => format!("Replaying {}…", self.name),
        }
    }
}

/// Opens a session's tracks from their files and its stems from the stem
/// cache.
pub(super) struct Sources;

impl deckhand_audio::Sources for Sources {
    fn track(&mut self, path: &Path) -> Result<Arc<deckhand_audio::TrackAudio>> {
        Ok(Arc::new(deckhand_audio::decode_file(path)?))
    }

    fn stems(
        &mut self,
        source: &deckhand_audio::StemsSource,
        track: &deckhand_audio::TrackAudio,
    ) -> Result<Arc<deckhand_audio::StemLayers>> {
        Ok(Arc::new(crate::stems::session_stems(source, track)?))
    }
}

/// Plays `session` into "<name> replay.wav" beside it, and says how it
/// compares with the recording.
fn replay(session: &Path, progress: &AtomicU64, total: &AtomicU64) -> Result<String> {
    let loaded = deckhand_audio::Session::read(session, Sources)?;
    total.store(loaded.frames().unwrap_or(0), Ordering::Relaxed);
    let (lost, warnings) = (loaded.lost_events(), loaded.warnings().to_vec());
    let recorded = session.with_extension("wav");
    let recorded = recorded.is_file().then_some(recorded);
    let out = super::mix_editor::free_path(session, "replay");
    let rendered = loaded.render_to_wav(&out, recorded.as_deref(), |frames| {
        progress.store(frames, Ordering::Relaxed);
    })?;
    let name = out.file_name().unwrap_or_default().to_string_lossy();
    let mut message = match rendered.comparison {
        None => format!("Replayed into {name}"),
        Some(c) if c.identical(rendered.frames) => {
            format!("Replayed into {name}: identical to the recording, sample for sample")
        }
        Some(c) => {
            let share = c.differing as f64 / (2 * rendered.frames.max(1)) as f64 * 100.0;
            let db = 20.0 * f64::from(c.largest.max(1e-9)).log10();
            format!(
                "Replayed into {name}: {share:.2}% of samples differ from the recording, by up \
                 to {db:.0} dB"
            )
        }
    };
    if lost > 0 {
        message.push_str(&format!("; {lost} events were lost while recording"));
    }
    for warning in warnings {
        message.push_str(&format!("; {warning}"));
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_for_recordings() {
        use super::{clean_name, rename_recording};
        assert_eq!(clean_name("  Friday set: part 1/2 "), Some("Friday set- part 1-2".into()));
        assert_eq!(clean_name(" .. "), None);
        // A recording renamed: folder, WAV and session; a taken name
        // gets a number.
        let root = std::env::temp_dir().join(format!("deckhand-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let old = root.join("RofLoxBox 2026-11-09 20-00-00");
        std::fs::create_dir_all(old.join("stems")).unwrap();
        let wav = old.join("RofLoxBox 2026-11-09 20-00-00.wav");
        std::fs::write(&wav, b"wav").unwrap();
        std::fs::write(wav.with_extension("session"), b"session").unwrap();
        std::fs::create_dir_all(root.join("Friday")).unwrap();
        let renamed = rename_recording(&wav, "Friday").unwrap();
        assert_eq!(renamed, root.join("Friday 2").join("Friday 2.wav"));
        assert!(renamed.exists() && renamed.with_extension("session").exists());
        assert!(root.join("Friday 2").join("stems").is_dir() && !old.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Replays the session `DECKHAND_SESSION` with the app's tracks and
    /// stems into `DECKHAND_REPLAY`, comparing it with its recording:
    /// `cargo test -p deckhand-app --release replays_a_real_session -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn replays_a_real_session() {
        let path = std::path::PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let out = std::path::PathBuf::from(std::env::var("DECKHAND_REPLAY").unwrap());
        let session = deckhand_audio::Session::read(&path, super::Sources).unwrap();
        eprintln!("warnings {:?}", session.warnings());
        let rendered =
            session.render_to_wav(&out, Some(&path.with_extension("wav")), |_| {}).unwrap();
        eprintln!("{} frames: {:?}", rendered.frames, rendered.comparison);
    }
}

#[cfg(test)]
mod played_tests {
    /// `DECKHAND_SESSION`'s beats are in order (read only):
    /// `cargo test -p deckhand-app --release real_played -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_played() {
        let path = std::path::PathBuf::from(std::env::var("DECKHAND_SESSION").unwrap());
        let mut session = deckhand_audio::Session::read(&path, super::Sources).unwrap();
        // The grids as the editor gives them, from the (sandbox's) collection.
        let collection =
            deckhand_library::Collection::open(&deckhand_library::collection_path()).unwrap();
        for (file, duration) in session.track_files() {
            if let Some(track) = collection.tracks.iter().find(|t| t.path == file)
                && let Some(bpm) = track.bpm
            {
                let grid = track.beats(duration);
                let sorted = grid.windows(2).all(|w| w[0].0 <= w[1].0);
                eprintln!("{} bpm {bpm} beats {} sorted {sorted}", file.display(), grid.len());
                let beats = grid.iter().map(|b| b.0).collect();
                let bars = grid.iter().map(|b| b.1).collect();
                session.add_grid(file, deckhand_audio::Grid { bpm, beats, bars });
            }
        }
        let all = session.played().unwrap();
        for clash in deckhand_audio::clashes(&all, 22_050) {
            let t = |at: u64| at as f64 / 44_100.0;
            eprintln!(
                "clash decks {:?} {:.1}–{:.1} s levels {:?}",
                clash.decks,
                t(clash.from),
                t(clash.to),
                clash.levels
            );
        }
        for (deck, played) in all.iter().enumerate() {
            let bad = played.beats.windows(2).position(|w| w[1].0 < w[0].0);
            eprintln!("deck {deck}: {} beats, out of order at {bad:?}", played.beats.len());
            if let Some(i) = bad {
                eprintln!(
                    "{:?}",
                    &played.beats[i.saturating_sub(2)..(i + 3).min(played.beats.len())]
                );
            }
        }
    }
}

/// The newest recording in `dir` (its folder, or for older ones its WAV).
fn latest_recording(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            !name.starts_with('.')
                && (e.path().is_dir() || name.ends_with(".wav") || name.ends_with(".session"))
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}
