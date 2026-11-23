//! Converting collection tracks (format, bitrate, sample rate): what to
//! convert to and what becomes of the originals, the conversion's results
//! taken into the collection, and its settings on the Library page.

use std::path::PathBuf;

use eframe::egui::{self, RichText, Ui};
use rofloxbox_library::XmlTrack;

use super::RofLoxBoxApp;
use crate::convert::{Conversion, Format, Settings};

impl RofLoxBoxApp {
    fn originals_dir(&self) -> PathBuf {
        self.config
            .prefs
            .originals_dir
            .clone()
            .unwrap_or_else(crate::convert::default_originals_dir)
    }

    /// "Convert N tracks": to what, and the originals kept or replaced.
    pub(super) fn convert_dialog(&mut self, ctx: &egui::Context) {
        let Some((ids, mut replace, mut remember)) = self.collection_view.converting.clone() else {
            return;
        };
        let originals = self.originals_dir();
        let asked = self.config.prefs.convert_replace.is_none();
        let mut settings = self.config.prefs.convert_settings;
        let (mut go, mut cancel) = (false, false);
        let in_rekordbox = ids
            .iter()
            .filter_map(|&id| self.collection.track(id))
            .filter(|t| t.analysis.is_some())
            .count();
        egui::Window::new(format!("Convert {} track(s)", ids.len()))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                format_choice(ui, &mut settings);
                ui.add_space(6.0);
                ui.label(RichText::new("The originals").strong());
                if asked {
                    ui.radio_value(
                        &mut replace,
                        false,
                        "Keep them: the converted files are added as new tracks",
                    );
                    ui.radio_value(&mut replace, true, "Replace them in the collection")
                        .on_hover_text(format!(
                            "The converted file takes the track's place (cues, grid, playlists \
                             and all); the original is moved to {}",
                            originals.display()
                        ));
                    ui.checkbox(&mut remember, "Don't ask again");
                } else {
                    let what = if replace {
                        format!("replaced; moved to {}", originals.display())
                    } else {
                        "kept; the converted files are added as new tracks".into()
                    };
                    ui.label(format!("They're {what}."));
                    ui.label(RichText::new("Change it in Preferences → Library.").small().weak());
                }
                if replace {
                    ui.label(
                        RichText::new(format!(
                            "Originals go to {} (RofLoxBox never adds them back).",
                            originals.display()
                        ))
                        .small()
                        .weak(),
                    );
                    if in_rekordbox > 0 {
                        ui.label(
                            RichText::new(format!(
                                "{in_rekordbox} of them are in rekordbox: there they move to the \
                                 converted file with the next save into rekordbox (cues, grid \
                                 and playlists stay)."
                            ))
                            .small()
                            .weak(),
                        );
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    go = ui.button(RichText::new("Convert").strong()).clicked();
                    cancel = ui.button("Cancel").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Escape));
                });
            });
        if settings != self.config.prefs.convert_settings {
            self.config.prefs.convert_settings = settings;
            self.config.save();
        }
        if cancel {
            self.collection_view.converting = None;
            return;
        }
        self.collection_view.converting = Some((ids.clone(), replace, remember));
        if !go {
            return;
        }
        self.collection_view.converting = None;
        if asked && remember {
            self.config.prefs.convert_replace = Some(replace);
            self.config.save();
        }
        if self.collection_view.conversion.is_some() {
            self.status = Some("A conversion is running already".into());
            return;
        }
        let tracks: Vec<(u32, PathBuf)> = ids
            .iter()
            .filter_map(|&id| self.collection.track(id))
            .filter(|t| t.path.is_file())
            .map(|t| (t.id, t.path.clone()))
            .collect();
        self.status =
            Some(format!("Converting {} track(s) to {}…", tracks.len(), settings.label()));
        self.collection_view.conversion =
            Some(Conversion::start(tracks, settings, replace));
    }

    /// Takes converted tracks into the collection: beside their originals,
    /// or in their place (the originals moved away). Call every frame.
    pub(super) fn poll_conversion(&mut self, ctx: &egui::Context) {
        let Some(conversion) = self.collection_view.conversion.as_mut() else { return };
        let mut results = Vec::new();
        while let Ok(converted) = conversion.results.try_recv() {
            conversion.received += 1;
            results.push((conversion.received, converted));
        }
        let (total, replace, label) =
            (conversion.total, conversion.replace, conversion.settings.label());
        let finished = conversion.is_finished() && conversion.results.try_recv().is_err();
        let stopped = conversion.cancelled();
        let done = conversion.received;
        for (n, converted) in results {
            let title =
                self.collection.track(converted.id).map_or_else(String::new, |t| t.title.clone());
            let line = match converted.result {
                Err(err) => {
                    self.log(format!("Convert {n}/{total}: {title} — {err}"), true);
                    continue;
                }
                Ok((new, offset)) if replace => {
                    match self.replace_with(converted.id, &new, offset) {
                        Ok(moved) => {
                            let to = moved.display();
                            format!("{title} — now {label}; the original is in {to}")
                        }
                        Err(err) => {
                            let line = format!(
                                "Convert {n}/{total}: {title} — converted, not replaced: {err:#}"
                            );
                            self.log(line, true);
                            continue;
                        }
                    }
                }
                Ok((new, offset)) => {
                    self.add_converted(converted.id, &new, offset);
                    format!("{title} — {label} added: {}", new.display())
                }
            };
            self.log(format!("Convert {n}/{total}: {line}"), false);
        }
        if finished {
            self.collection_view.conversion = None;
            let how = if stopped { " (stopped)" } else { "" };
            let message =
                format!("Converted {done} of {total} track(s) to {label}{how} — see the Log");
            self.status = Some(message);
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }

    /// A converted file as a track of its own, with the original's tags,
    /// cues and grid.
    fn add_converted(&mut self, original: u32, new: &std::path::Path, offset: f64) {
        let Some(track) = self.collection.track(original) else { return };
        let mut xml = XmlTrack::from_track(track);
        crate::convert::shift_times(&mut xml.cues, &mut xml.tempo, offset);
        // The analysis files are the original file's: its waveform would
        // be off by the offset too. Worked out from the new file instead
        // (the grid is kept in the collection).
        if !xml.tempo.is_empty() {
            xml.analysis = None;
        }
        let key = new.to_string_lossy().into_owned();
        (xml.id, xml.location) = (key.clone(), key);
        xml.file.path = new.to_path_buf();
        if let Ok(read) = rofloxbox_library::read_file(new) {
            (xml.file.bitrate, xml.file.sample_rate, xml.file.duration) =
                (read.bitrate, read.sample_rate, read.duration);
            (xml.file.file_size, xml.file.modified, xml.file.has_artwork) =
                (read.file_size, read.modified, read.has_artwork);
        }
        self.collection.add_tracks(vec![xml]);
    }

    /// The converted file takes the track's place under the original's
    /// name; the original goes to the originals folder. Returns where.
    fn replace_with(
        &mut self,
        id: u32,
        new: &std::path::Path,
        offset: f64,
    ) -> anyhow::Result<PathBuf> {
        let Some(original) = self.collection.track(id).map(|t| t.path.clone()) else {
            anyhow::bail!("the track is no longer in the collection");
        };
        let moved = crate::convert::move_original(&original, &self.originals_dir())?;
        let dir = original.parent().unwrap_or(std::path::Path::new("."));
        let stem =
            original.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let extension =
            new.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let place = crate::convert::free_path(dir, &stem, &extension);
        crate::convert::move_file(new, &place)?;
        let read = rofloxbox_library::read_file(&place).ok();
        self.collection.update_track(id, |t| {
            // rekordbox's entry follows on the next save into it (from the
            // file it has: the first original, if replaced twice before).
            if t.rekordbox_path.is_none() {
                t.rekordbox_path = Some(t.path.clone());
            }
            t.path = place.clone();
            // Cues and grid stay on the music (also in the library saved to,
            // on the next save), and the original's analysis goes.
            crate::convert::shift_times(&mut t.cues, &mut t.tempo, offset);
            if !t.tempo.is_empty() {
                t.analysis = None;
            }
            if let Some(read) = &read {
                (t.bitrate, t.sample_rate, t.has_artwork) =
                    (read.bitrate, read.sample_rate, read.has_artwork);
                (t.file_size, t.modified) = (read.file_size, read.modified);
            }
        });
        Ok(moved)
    }

    /// The Library page's part on converted tracks.
    pub(super) fn convert_prefs(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        ui.add_space(10.0);
        ui.label(RichText::new("Converting tracks").strong().size(15.0));
        ui.horizontal(|ui| {
            ui.label("Replaced originals go to:");
            ui.label(RichText::new(self.originals_dir().display().to_string()).small());
            if ui.small_button("Choose…").clicked()
                && let Some(path) =
                    rfd::FileDialog::new().set_title("Folder for replaced originals").pick_folder()
            {
                self.config.prefs.originals_dir = Some(path);
                changed = true;
            }
        });
        if let Some(replace) = self.config.prefs.convert_replace {
            let what = if replace { "replaced" } else { "kept beside the converted files" };
            ui.horizontal(|ui| {
                ui.label(format!("Originals are {what}."));
                if ui.button("Ask again").clicked() {
                    self.config.prefs.convert_replace = None;
                    changed = true;
                }
            });
        }
        changed
    }
}

/// Format, quality and sample rate.
fn format_choice(ui: &mut Ui, settings: &mut Settings) {
    ui.label(RichText::new("Convert to").strong());
    let kind = |f: Format| std::mem::discriminant(&f);
    ui.horizontal_wrapped(|ui| {
        for (format, name) in [
            (Format::Mp3(320), "MP3"),
            (Format::Aiff, "AIFF"),
            (Format::Wav, "WAV"),
            (Format::Flac, "FLAC"),
        ] {
            if ui.selectable_label(kind(settings.format) == kind(format), name).clicked()
                && kind(settings.format) != kind(format)
            {
                settings.format = format;
            }
        }
    });
    match &mut settings.format {
        Format::Mp3(kbps) | Format::Aac(kbps) => {
            ui.horizontal(|ui| {
                ui.label("Bitrate");
                for rate in [128, 192, 256, 320] {
                    ui.selectable_value(kbps, rate, format!("{rate} kbit/s"));
                }
            });
        }
        _ => {
            ui.horizontal(|ui| {
                ui.label("Bit depth");
                ui.selectable_value(&mut settings.bits, 16, "16-bit");
                ui.selectable_value(&mut settings.bits, 24, "24-bit");
            });
        }
    }
    ui.horizontal(|ui| {
        ui.label("Sample rate");
        ui.selectable_value(&mut settings.sample_rate, None, "Keep");
        ui.selectable_value(&mut settings.sample_rate, Some(44_100), "44.1 kHz");
        ui.selectable_value(&mut settings.sample_rate, Some(48_000), "48 kHz");
    });
    let mp3 = matches!(settings.format, Format::Mp3(_) | Format::Aac(_));
    if mp3 && settings.sample_rate.is_none() {
        ui.label(
            RichText::new("MP3 holds at most 48 kHz: higher rates become 44.1 or 48 kHz.")
                .small()
                .weak(),
        );
    }
    ui.label(
        RichText::new(
            "For every CDJ: MP3 320, or AIFF/WAV 16-bit at 44.1 kHz. Many players can't play \
             above 48 kHz or 24-bit, and older ones no FLAC.",
        )
        .small()
        .weak(),
    );
}
