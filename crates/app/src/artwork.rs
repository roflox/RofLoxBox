//! Small cover pictures for the track lists, read and shrunk in the
//! background and kept as textures.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

/// Where a track's picture is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Cover {
    /// In the audio file's tags.
    Embedded(PathBuf),
    /// A picture file of its own (a rekordbox export's artwork).
    File(PathBuf),
}

/// Pixels across a thumbnail (sharp at 2× on an 18-point icon).
const SIZE: u32 = 40;
/// Thumbnails kept; past that, the cache starts again.
const KEEP: usize = 3000;
/// Requests waiting; the newest go first, the oldest are dropped
/// (scrolled past).
const WAITING: usize = 200;

struct Covers {
    textures: HashMap<Cover, Option<TextureHandle>>,
    asked: HashSet<Cover>,
    queue: Arc<(Mutex<Vec<Cover>>, Condvar)>,
    done: Receiver<(Cover, Option<ColorImage>)>,
}

fn covers(ctx: &egui::Context) -> &'static Mutex<Covers> {
    static COVERS: OnceLock<Mutex<Covers>> = OnceLock::new();
    COVERS.get_or_init(|| {
        let queue: Arc<(Mutex<Vec<Cover>>, Condvar)> = Arc::default();
        let (tx, done) = channel();
        for n in 0..2 {
            let (queue, tx, ctx) = (queue.clone(), tx.clone(), ctx.clone());
            let _ = std::thread::Builder::new()
                .name(format!("covers {n}"))
                .spawn(move || work(&queue, &tx, &ctx));
        }
        Mutex::new(Covers { textures: HashMap::new(), asked: HashSet::new(), queue, done })
    })
}

fn work(
    queue: &(Mutex<Vec<Cover>>, Condvar),
    done: &Sender<(Cover, Option<ColorImage>)>,
    ctx: &egui::Context,
) {
    loop {
        let cover = {
            let mut waiting = queue.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(cover) = waiting.pop() {
                    break cover;
                }
                waiting = queue.1.wait(waiting).unwrap_or_else(|e| e.into_inner());
            }
        };
        let image = thumbnail(&cover);
        if done.send((cover, image)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn thumbnail(cover: &Cover) -> Option<ColorImage> {
    let bytes = match cover {
        Cover::Embedded(path) => deckhand_library::embedded_artwork(path)?,
        Cover::File(path) => std::fs::read(path).ok()?,
    };
    let image = image::load_from_memory(&bytes).ok()?.thumbnail(SIZE, SIZE).to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, image.as_raw()))
}

/// The cover's thumbnail, once it has been read (asked for the first time,
/// it is read in the background and the list repaints when it is in).
pub fn thumbnail_texture(ctx: &egui::Context, cover: &Cover) -> Option<TextureHandle> {
    let mut covers = covers(ctx).lock().ok()?;
    while let Ok((done, image)) = covers.done.try_recv() {
        covers.asked.remove(&done);
        if covers.textures.len() >= KEEP {
            covers.textures.clear();
        }
        let texture = image.map(|image| {
            ctx.load_texture(format!("cover {done:?}"), image, TextureOptions::LINEAR)
        });
        covers.textures.insert(done, texture);
    }
    if let Some(texture) = covers.textures.get(cover) {
        return texture.clone();
    }
    if covers.asked.insert(cover.clone()) {
        let mut waiting = covers.queue.0.lock().unwrap_or_else(|e| e.into_inner());
        waiting.push(cover.clone());
        if waiting.len() > WAITING {
            let extra = waiting.len() - WAITING;
            let dropped: Vec<Cover> = waiting.drain(..extra).collect();
            drop(waiting);
            for cover in dropped {
                covers.asked.remove(&cover);
            }
        } else {
            drop(waiting);
        }
        covers.queue.1.notify_one();
    }
    None
}
