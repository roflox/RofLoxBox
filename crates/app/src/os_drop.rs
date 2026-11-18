//! Files dragged in from the computer's file manager (Finder, Explorer).
//! While they are over the window the system reports no pointer movement,
//! so the pointer is read from the system and fed to egui, with the files
//! as an ordinary track drag: decks, waveforms and playlists take them as
//! they take tracks dragged inside the app. Dropped anywhere else, they go
//! into the collection.

use std::path::PathBuf;

use eframe::egui;

use crate::browser::TrackDrag;

/// Files over the window or dropped on it.
#[derive(Default)]
pub struct OsDrag {
    /// Over the window, being dragged.
    over: bool,
    /// Dropped this frame: given to what's under the pointer, or else to
    /// the collection (see [`OsDrag::unclaimed`]).
    dropped: Option<Vec<PathBuf>>,
}

impl OsDrag {
    /// Before a frame: files over the window move the pointer and are the
    /// drag; dropped, they are let go of there.
    pub fn feed(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let dropped: Vec<PathBuf> =
            std::mem::take(&mut raw.dropped_files).into_iter().filter_map(|f| f.path).collect();
        let hovered: Vec<PathBuf> =
            raw.hovered_files.iter().filter_map(|f| f.path.clone()).collect();
        let pointer = pointer(ctx, raw);
        if !dropped.is_empty() {
            egui::DragAndDrop::set_payload(ctx, TrackDrag::Files(dropped.clone()));
            if let Some(pos) = pointer {
                raw.events.push(egui::Event::PointerMoved(pos));
                raw.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: raw.modifiers,
                });
            }
            self.over = false;
            self.dropped = Some(dropped);
            return;
        }
        if !hovered.is_empty() {
            if !egui::DragAndDrop::has_payload_of_type::<TrackDrag>(ctx) {
                egui::DragAndDrop::set_payload(ctx, TrackDrag::Files(hovered));
            }
            if let Some(pos) = pointer {
                raw.events.push(egui::Event::PointerMoved(pos));
            }
            self.over = true;
            // No events come while they are dragged: look again soon.
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        } else if std::mem::take(&mut self.over) {
            // Dragged out again.
            egui::DragAndDrop::clear_payload(ctx);
            raw.events.push(egui::Event::PointerGone);
        }
    }

    /// After a frame: files dropped where nothing took them.
    pub fn unclaimed(&mut self, ctx: &egui::Context) -> Option<Vec<PathBuf>> {
        let dropped = self.dropped.take()?;
        let taken = !egui::DragAndDrop::has_payload_of_type::<TrackDrag>(ctx);
        egui::DragAndDrop::clear_payload(ctx);
        (!taken).then_some(dropped)
    }
}

/// The audio files of `paths`: files as they are, folders' (with their
/// subfolders') in name order.
pub fn audio_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> =
            std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
        entries.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        for path in entries {
            if path.is_dir() {
                walk(&path, out);
            } else if is_audio(&path) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    for path in paths {
        if path.is_dir() {
            walk(path, &mut out);
        } else if is_audio(path) {
            out.push(path.clone());
        }
    }
    out
}

fn is_audio(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| rofloxbox_library::AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
}

/// The pointer in the window, in points, as the system has it.
fn pointer(ctx: &egui::Context, raw: &egui::RawInput) -> Option<egui::Pos2> {
    let viewport = raw.viewports.get(&raw.viewport_id)?;
    let inner = viewport.inner_rect?;
    let native = viewport.native_pixels_per_point?;
    let screen = screen_pointer_px(native)?;
    Some(egui::pos2(screen.x, screen.y) / ctx.pixels_per_point() - inner.min.to_vec2())
}

/// The pointer on the screen in physical pixels, as the window's position
/// is given (`native`: the window's pixels per point).
#[cfg(target_os = "macos")]
fn screen_pointer_px(native: f32) -> Option<egui::Pos2> {
    use objc2_app_kit::{NSEvent, NSScreen};
    let mtm = objc2::MainThreadMarker::new()?;
    // From the bottom of the main screen (the first), in points.
    let height = NSScreen::screens(mtm).iter().next()?.frame().size.height;
    let at = NSEvent::mouseLocation();
    Some(egui::pos2(at.x as f32, (height - at.y) as f32) * native)
}

#[cfg(windows)]
fn screen_pointer_px(_native: f32) -> Option<egui::Pos2> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut at = POINT { x: 0, y: 0 };
    // SAFETY: writes the position into `at`.
    (unsafe { GetCursorPos(&mut at) } != 0).then(|| egui::pos2(at.x as f32, at.y as f32))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn screen_pointer_px(_native: f32) -> Option<egui::Pos2> {
    None
}

/// Developer aid: `ROFLOXBOX_DRAG="x1,y1,x2,y2[,delay s]"` drags the mouse
/// from one point (in points) to the other once the app has run `delay`
/// seconds (3 by default), as a hand would: press, move in steps, let go.
pub struct ScriptedDrag {
    from: egui::Pos2,
    to: egui::Pos2,
    start: std::time::Instant,
    step: u32,
}

impl ScriptedDrag {
    pub fn from_env() -> Option<Self> {
        let text = std::env::var("ROFLOXBOX_DRAG").ok()?;
        let n: Vec<f32> = text.split(',').filter_map(|v| v.trim().parse().ok()).collect();
        let delay = n.get(4).copied().unwrap_or(3.0);
        Some(Self {
            from: egui::pos2(*n.first()?, *n.get(1)?),
            to: egui::pos2(*n.get(2)?, *n.get(3)?),
            start: std::time::Instant::now() + std::time::Duration::from_secs_f32(delay),
            step: 0,
        })
    }

    /// Before a frame: the next bit of the drag. `false` once it's done.
    pub fn feed(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) -> bool {
        ctx.request_repaint();
        if std::time::Instant::now() < self.start {
            return true;
        }
        const MOVES: u32 = 30;
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let events = &mut raw.events;
        match self.step {
            0 => events.push(egui::Event::PointerMoved(self.from)),
            1 => events.push(button(self.from, true)),
            n if n < 2 + MOVES => {
                let t = (n - 1) as f32 / MOVES as f32;
                events.push(egui::Event::PointerMoved(self.from + (self.to - self.from) * t));
            }
            n if n < 2 + MOVES + 5 => events.push(egui::Event::PointerMoved(self.to)),
            n if n == 2 + MOVES + 5 => events.push(button(self.to, false)),
            _ => return false,
        }
        self.step += 1;
        true
    }
}
