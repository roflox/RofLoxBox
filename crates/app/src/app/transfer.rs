//! Tracks dragged from one library onto a playlist of another: the
//! collection, a USB export, files of the computer (and, to come, Serato
//! and others). The source hands the tracks over in one shared form (the
//! file, its tags, cues and grid: an [`XmlTrack`]), fetching them first
//! when it has to (copied off a stick, read from files); the target then
//! adds them (into the collection, or copied onto a stick with their
//! analysis). Drops within one library don't come here: they only change
//! a playlist.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use rofloxbox_library::XmlTrack;
use eframe::egui;

use super::RofLoxBoxApp;
use crate::browser::TrackDrag;
use crate::usb_export::{ExportRequest, UsbExport};
use crate::usb_import::{ImportRequest, UsbImport, default_destination};

/// A playlist tracks are dropped on.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Target {
    /// A collection playlist.
    Collection(u32),
    /// A playlist of the export at this root.
    Usb(PathBuf, u32),
}

/// Tracks on their way.
pub(super) struct Transfer {
    target: Target,
    stage: Stage,
}

enum Stage {
    /// Being handed over by the source.
    Fetching(Fetch),
    /// Being copied onto a stick.
    Adding(UsbExport),
}

/// The source's side.
enum Fetch {
    /// Copied off a stick (into the music folder RofLoxBox imports to).
    Usb(UsbImport),
    /// Read from files (their tags).
    Files(mpsc::Receiver<Vec<XmlTrack>>),
}

impl RofLoxBoxApp {
    /// Starts moving `drag`'s tracks onto `target`.
    pub(super) fn transfer(&mut self, drag: &TrackDrag, target: Target) {
        if self.tree.transfer.is_some() {
            self.status = Some("Tracks are being moved already; try again once that's done.".into());
            return;
        }
        let fetch = match drag {
            TrackDrag::Collection(ids) => {
                let tracks =
                    ids.iter().filter_map(|id| self.collection.track(*id)).map(XmlTrack::from_track).collect();
                self.deliver(tracks, target);
                return;
            }
            TrackDrag::Files(paths) => {
                let paths = paths.clone();
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(paths.iter().map(|p| file_track(p)).collect());
                });
                Fetch::Files(rx)
            }
            TrackDrag::Usb(root, ids) => {
                let Some(library) = self.libraries.iter().find(|l| &l.root == root) else { return };
                let tracks = ids.iter().filter_map(|id| library.library.track(*id).cloned()).collect();
                Fetch::Usb(UsbImport::start(ImportRequest {
                    root: root.clone(),
                    destination: default_destination().join(library.name()),
                    tracks,
                    playlists: Vec::new(),
                }))
            }
        };
        self.status = Some(format!("Getting {} track(s)…", drag_len(drag)));
        self.tree.transfer = Some(Transfer { target, stage: Stage::Fetching(fetch) });
    }

    /// Adds tracks the source handed over to the target.
    fn deliver(&mut self, tracks: Vec<XmlTrack>, target: Target) {
        if tracks.is_empty() {
            self.status = Some("None of those tracks could be found".into());
            return;
        }
        match target {
            Target::Collection(playlist) => {
                let (ids, summary) = self.collection.add_tracks(tracks);
                self.add_many(&ids, playlist);
                if summary.added > 0 {
                    let added = self.status.take().unwrap_or_default();
                    self.status = Some(format!("{added}; {} new in the collection", summary.added));
                }
            }
            Target::Usb(root, playlist) => {
                let Some(library) = self.libraries.iter().find(|l| l.root == root) else { return };
                let name = library
                    .playlists
                    .iter()
                    .find(|r| r.id == playlist)
                    .map_or_else(|| "the playlist".to_owned(), |r| r.name.clone());
                let request = ExportRequest {
                    root: root.clone(),
                    playlist: name.clone(),
                    add_to: Some(playlist),
                    tracks: tracks.iter().zip(1..).map(|(t, id)| t.to_track(id)).collect(),
                    key_notation: self.config.prefs.key_notation,
                    analysis: self.config.prefs.analysis,
                };
                self.status = Some(format!("Copying {} track(s) to {name}…", tracks.len()));
                self.tree.transfer = Some(Transfer {
                    target: Target::Usb(root, playlist),
                    stage: Stage::Adding(UsbExport::start(request)),
                });
            }
        }
    }

    /// Moves a transfer along: call every frame.
    pub(super) fn poll_transfer(&mut self, ctx: &egui::Context) {
        let Some(transfer) = &self.tree.transfer else { return };
        let fetched = match &transfer.stage {
            Stage::Fetching(Fetch::Files(rx)) => match rx.try_recv() {
                Ok(tracks) => Some(Ok(tracks)),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("reading the files failed"))),
            },
            Stage::Fetching(Fetch::Usb(job)) => {
                job.finished().map(|r| r.map(|imported| imported.library.tracks))
            }
            Stage::Adding(job) => {
                if let Some(result) = job.finished() {
                    let Some(Transfer { target: Target::Usb(root, _), .. }) = self.tree.transfer.take()
                    else {
                        return;
                    };
                    match result {
                        Ok(done) => {
                            // Read again, with the new tracks.
                            self.libraries.retain(|l| l.root != root);
                            self.open_usb(root, false);
                            let missing = if done.missing > 0 {
                                format!(", {} missing on this computer", done.missing)
                            } else {
                                String::new()
                            };
                            self.status = Some(format!("Added to the stick: {} track(s) copied{missing}", done.added));
                        }
                        Err(err) => self.status = Some(format!("Copying to the stick stopped: {err:#}")),
                    }
                    return;
                }
                None
            }
        };
        let Some(fetched) = fetched else {
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        };
        let Some(transfer) = self.tree.transfer.take() else { return };
        match fetched {
            Ok(tracks) => self.deliver(tracks, transfer.target),
            Err(err) => self.status = Some(format!("Getting the tracks stopped: {err:#}")),
        }
    }

    /// Whether tracks are on their way.
    pub(super) fn transferring(&self) -> bool {
        self.tree.transfer.is_some()
    }
}

fn drag_len(drag: &TrackDrag) -> usize {
    match drag {
        TrackDrag::Collection(ids) | TrackDrag::Usb(_, ids) => ids.len(),
        TrackDrag::Files(paths) => paths.len(),
    }
}

/// A file of the computer in the shared form: its tags.
fn file_track(path: &std::path::Path) -> XmlTrack {
    let file = rofloxbox_library::read_file(path)
        .unwrap_or_else(|_| rofloxbox_library::ScannedFile { path: path.to_path_buf(), ..Default::default() });
    let key = path.to_string_lossy().into_owned();
    XmlTrack { id: key.clone(), location: key, file, rating: None, ..XmlTrack::default() }
}
