//! Separating tracks into stems with a model the user picks, and making new
//! tracks from some of them (an acapella, an instrumental, …).
//!
//! A model is a folder in `models/` with a `model.toml`: its name, the stems
//! it makes and the command that makes them, for example
//!
//! ```toml
//! name = "Demucs (htdemucs)"
//! stems = ["drums", "bass", "other", "vocals"]
//! command = ["demucs", "-n", "htdemucs", "--float32", "--clip-mode", "none", "-o", "{output}", "{input}"]
//! install = "pipx install demucs && pipx inject demucs numpy soundfile"   # shown when the command is missing
//! ```
//!
//! `{input}` is the track as a WAV file, `{output}` a folder for the results
//! and `{model}` the model's own folder. Any tool works: the stem files are
//! found in `{output}` by name (`vocals.wav`, `Song_(Vocals)_model.flac`), or
//! by a pattern per stem under `[files]`. Separations are kept in a cache,
//! per track and model.
//!
//! The model is given deckhand's own decoding of the track, so its stems
//! start on the same sample as the original plays here and copied cues land
//! where they were.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// On Windows, runs a command-line tool without opening a console window
/// for it (its output is read through pipes anyway).
pub fn no_window(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

pub const MODEL_FILE: &str = "model.toml";
/// Audio files a model's results may come as.
const AUDIO: [&str; 7] = ["wav", "flac", "aif", "aiff", "mp3", "ogg", "m4a"];

#[derive(Debug, Clone, Deserialize)]
struct ModelFile {
    name: String,
    stems: Vec<String>,
    command: Vec<String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    install: String,
    #[serde(default)]
    python: Option<PathBuf>,
}

/// A separation model the user added.
#[derive(Debug, Clone, PartialEq)]
pub struct StemModel {
    /// Its folder's name; the preferences remember the chosen one by it.
    pub id: String,
    pub name: String,
    pub stems: Vec<String>,
    command: Vec<String>,
    files: BTreeMap<String, String>,
    /// How to install its tool, for when it is missing.
    install: String,
    /// For Demucs: the Python it is installed in, to run the fast helper
    /// with; found by itself when unset.
    pub python: Option<PathBuf>,
    dir: PathBuf,
}

pub fn models_dir() -> PathBuf {
    deckhand_library::data_dir().join("models")
}

fn cache_dir() -> PathBuf {
    deckhand_library::data_dir().join("stem-cache")
}

/// The most the stem cache keeps; past it, the stems of the tracks used
/// least recently go (they are separated again when needed).
const CACHE_LIMIT: u64 = 2 << 30;

/// Marks a track's cached stems as just used.
fn touch(dir: &Path) {
    let _ = std::fs::write(dir.join(".used"), b"");
}

/// Removes the least recently used tracks' stems until the cache fits
/// `limit` bytes.
fn trim_cache(cache: &Path, limit: u64) {
    let size = |dir: &Path| -> u64 {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(std::fs::Metadata::is_file)
            .map(|m| m.len())
            .sum()
    };
    let mut tracks: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(cache)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|model| std::fs::read_dir(model.path()).into_iter().flatten().flatten())
        .map(|entry| entry.path())
        .filter(|dir| dir.is_dir() && dir.extension().is_none())
        .map(|dir| {
            let used = std::fs::metadata(dir.join(".used"))
                .or_else(|_| std::fs::metadata(&dir))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (used, size(&dir), dir)
        })
        .collect();
    // Newest first: they stay.
    tracks.sort_by_key(|track| std::cmp::Reverse(track.0));
    let mut total = 0;
    for (_, bytes, dir) in tracks {
        total += bytes;
        if total > limit {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Where made tracks go unless Preferences say otherwise.
pub fn default_output_dir() -> PathBuf {
    deckhand_library::data_dir().join("Stems")
}

/// The models in `dir`, by name, and what could not be read.
pub fn load_models(dir: &Path) -> (Vec<StemModel>, Vec<String>) {
    let (mut models, mut errors) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(dir) else { return (models, errors) };
    for entry in entries.flatten() {
        let folder = entry.path();
        let file = folder.join(MODEL_FILE);
        if !file.is_file() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        match read_model(&file) {
            Ok(m) => models.push(StemModel {
                id,
                name: m.name,
                stems: m.stems,
                command: m.command,
                files: m.files,
                install: m.install,
                python: m.python,
                dir: folder,
            }),
            Err(err) => errors.push(format!("{id}: {err:#}")),
        }
    }
    models.sort_by_key(|m| m.name.to_lowercase());
    (models, errors)
}

fn read_model(file: &Path) -> Result<ModelFile> {
    let text = std::fs::read_to_string(file)?;
    let model: ModelFile = toml::from_str(&text)?;
    if model.stems.is_empty() {
        bail!("no stems listed");
    }
    if model.command.is_empty() {
        bail!("no command");
    }
    Ok(model)
}

/// A ready-made model description for a separation tool.
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// How to install the tool.
    pub install: &'static str,
    toml: &'static str,
}

pub const PRESETS: [Preset; 4] = [
    Preset {
        id: "demucs-htdemucs",
        name: "Demucs htdemucs (4 stems)",
        install: "pipx install demucs && pipx inject demucs numpy soundfile",
        toml: r#"name = "Demucs htdemucs"
install = "pipx install demucs && pipx inject demucs numpy soundfile"
stems = ["drums", "bass", "other", "vocals"]
command = ["demucs", "-n", "htdemucs", "-o", "{output}", "--filename", "{stem}.{ext}", "{input}"]
"#,
    },
    Preset {
        id: "demucs-htdemucs-ft",
        name: "Demucs htdemucs_ft (4 stems, better, 4× slower)",
        install: "pipx install demucs && pipx inject demucs numpy soundfile",
        toml: r#"name = "Demucs htdemucs_ft"
install = "pipx install demucs && pipx inject demucs numpy soundfile"
stems = ["drums", "bass", "other", "vocals"]
command = ["demucs", "-n", "htdemucs_ft", "--float32", "--clip-mode", "none", "-o", "{output}", "--filename", "{stem}.{ext}", "{input}"]
"#,
    },
    Preset {
        id: "demucs-htdemucs-6s",
        name: "Demucs htdemucs_6s (6 stems with guitar and piano)",
        install: "pipx install demucs && pipx inject demucs numpy soundfile",
        toml: r#"name = "Demucs htdemucs_6s"
install = "pipx install demucs && pipx inject demucs numpy soundfile"
stems = ["drums", "bass", "other", "vocals", "guitar", "piano"]
command = ["demucs", "-n", "htdemucs_6s", "--float32", "--clip-mode", "none", "-o", "{output}", "--filename", "{stem}.{ext}", "{input}"]
"#,
    },
    Preset {
        id: "audio-separator-bs-roformer",
        name: "BS-Roformer via audio-separator (vocals, instrumental)",
        install: "pipx install \"audio-separator[cpu]\"",
        toml: r#"name = "BS-Roformer (audio-separator)"
install = "pipx install \"audio-separator[cpu]\""
stems = ["vocals", "instrumental"]
command = ["audio-separator", "{input}", "--model_filename", "model_bs_roformer_ep_317_sdr_12.9755.ckpt", "--output_dir", "{output}", "--output_format", "WAV"]
"#,
    },
];

/// Writes `preset` into the models folder `dir` (a model already there stays).
pub fn add_preset(dir: &Path, preset: &Preset) -> Result<()> {
    let folder = dir.join(preset.id);
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let file = folder.join(MODEL_FILE);
    if !file.exists() {
        std::fs::write(&file, preset.toml)
            .with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(())
}

/// What to make from a track's stems.
#[derive(Debug, Clone, PartialEq)]
pub struct Recipe {
    /// In the menu.
    pub label: String,
    /// After the title: "Song (Acapella)".
    pub suffix: String,
    /// The stems mixed into the new track.
    pub keep: Vec<String>,
}

fn is_vocals(stem: &str) -> bool {
    stem.eq_ignore_ascii_case("vocals") || stem.eq_ignore_ascii_case("vocal")
}

fn capitalized(stem: &str) -> String {
    let mut chars = stem.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// What can be made with `model`: acapella and instrumental with a vocals
/// stem, without drums with a drums stem, and each other stem alone.
pub fn recipes(model: &StemModel) -> Vec<Recipe> {
    let stems = &model.stems;
    let but = |skip: &dyn Fn(&str) -> bool| -> Vec<String> {
        stems.iter().filter(|s| !skip(s)).cloned().collect()
    };
    let mut all = Vec::new();
    let mut add = |label: String, suffix: String, keep: Vec<String>| {
        if !keep.is_empty()
            && keep.len() < stems.len()
            && !all.iter().any(|r: &Recipe| r.keep == keep)
        {
            all.push(Recipe { label, suffix, keep });
        }
    };
    if let Some(vocals) = stems.iter().find(|s| is_vocals(s)) {
        add("Make acapella".into(), "Acapella".into(), vec![vocals.clone()]);
        add("Make instrumental".into(), "Instrumental".into(), but(&|s| is_vocals(s)));
    }
    if stems.iter().any(|s| s.eq_ignore_ascii_case("drums")) {
        add("Without drums".into(), "No Drums".into(), but(&|s| s.eq_ignore_ascii_case("drums")));
    }
    for stem in stems {
        let name = capitalized(stem);
        add(format!("{name} only"), name, vec![stem.clone()]);
    }
    all
}

/// Where a made track's file goes: `folder/Artist/Title (Suffix).wav`.
pub fn output_path(folder: &Path, artist: &str, title: &str, suffix: &str) -> PathBuf {
    let clean = |text: &str| -> String {
        let cleaned: String = text
            .chars()
            .map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c })
            .collect();
        cleaned.trim().trim_matches('.').to_owned()
    };
    let title = clean(title);
    let title = if title.is_empty() { "Untitled".to_owned() } else { title };
    let artist = clean(artist);
    let folder = if artist.is_empty() { folder.to_path_buf() } else { folder.join(artist) };
    folder.join(format!("{title} ({suffix}).wav"))
}

/// One track to separate and what to make of it.
#[derive(Debug, Clone)]
pub struct Job {
    pub track: u32,
    pub title: String,
    pub input: PathBuf,
    pub model: StemModel,
    pub makes: Vec<(Recipe, PathBuf)>,
}

/// Separating a deck's track to play its stems live.
#[derive(Debug, Clone)]
pub struct LiveJob {
    pub deck: usize,
    pub title: String,
    pub input: PathBuf,
    pub model: StemModel,
    /// The track as the deck decoded it.
    pub audio: Arc<deckhand_audio::TrackAudio>,
    /// The deck's position in seconds (`f64` bits), kept up to date by the
    /// app: the pieces around it go first. [`CANCELLED`] stops the job.
    pub playhead: Arc<AtomicU64>,
}

/// A [`LiveJob::playhead`] value: the deck has another track now.
pub const CANCELLED: u64 = u64::MAX;

#[derive(Debug)]
pub enum Event {
    /// The job started; `percent` of the model's run is done.
    Progress {
        title: String,
        percent: Option<u32>,
        /// The deck the stems are for, when played live.
        deck: Option<usize>,
    },
    /// Stems for a deck: the track's file and its parts.
    Live {
        deck: usize,
        input: PathBuf,
        stems: Arc<deckhand_audio::StemLayers>,
    },
    Made {
        track: u32,
        recipe: Recipe,
        path: PathBuf,
        model: String,
    },
    Failed {
        title: String,
        error: String,
        deck: Option<usize>,
    },
}

/// Separates queued tracks one after another, on a thread of its own.
pub struct Separator {
    jobs: Sender<(u64, Job)>,
    pub events: Receiver<Event>,
    /// Jobs submitted before the current generation are dropped.
    generation: Arc<AtomicU64>,
    pending: Arc<AtomicUsize>,
    running: Arc<Mutex<Option<Child>>>,
}

impl Default for Separator {
    fn default() -> Self {
        let (jobs, queue) = channel::<(u64, Job)>();
        let (events_tx, events) = channel();
        let generation = Arc::new(AtomicU64::new(0));
        let pending = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(Mutex::new(None));
        let (current, left, child) = (generation.clone(), pending.clone(), running.clone());
        std::thread::Builder::new()
            .name("stems".into())
            .spawn(move || {
                for (generation, job) in queue {
                    if generation == current.load(Ordering::Relaxed) {
                        run(job, &events_tx, &child, &cache_dir());
                    }
                    left.fetch_sub(1, Ordering::Relaxed);
                }
            })
            .expect("starting the stems thread");
        Self { jobs, events, generation, pending, running }
    }
}

impl Separator {
    pub fn submit(&self, job: Job) {
        self.pending.fetch_add(1, Ordering::Relaxed);
        if self.jobs.send((self.generation.load(Ordering::Relaxed), job)).is_err() {
            self.pending.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Jobs waiting or running.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Stops the running separation and drops the waiting ones.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut running) = self.running.lock()
            && let Some(child) = running.as_mut()
        {
            let _ = child.kill();
        }
    }
}

fn run(job: Job, events: &Sender<Event>, running: &Mutex<Option<Child>>, cache: &Path) {
    let (track, title) = (job.track, job.title.clone());
    let progress = |percent| {
        let _ = events.send(Event::Progress { title: title.clone(), percent, deck: None });
    };
    progress(None);
    let stems = match separate(&job.model, &job.input, cache, running, &progress) {
        Ok(stems) => {
            if let Some(dir) = stems.values().next().and_then(|f| f.parent()) {
                touch(dir);
            }
            trim_cache(cache, CACHE_LIMIT);
            stems
        }
        Err(err) => {
            let _ = events.send(Event::Failed { title, error: format!("{err:#}"), deck: None });
            return;
        }
    };
    for (recipe, path) in job.makes {
        let files: Vec<&Path> =
            recipe.keep.iter().filter_map(|s| stems.get(s)).map(|p| p.as_path()).collect();
        let made = if files.len() == recipe.keep.len() {
            mix(&files, &path)
        } else {
            Err(anyhow!("the model did not make all of {}", recipe.keep.join(", ")))
        };
        let _ = events.send(match made {
            Ok(()) => Event::Made { track, recipe, path, model: job.model.name.clone() },
            Err(err) => Event::Failed {
                title: format!("{} ({})", job.title, recipe.suffix),
                error: format!("{err:#}"),
                deck: None,
            },
        });
    }
}

/// The vocal, bass and drum stems, ready to play at the track's `rate` and
/// length. A model without one of them leaves that part in the instruments.
/// Each stem gets the level at which all of them add up to `track` (as
/// deckhand decodes it), whatever the model did to their levels.
fn layers(
    stems: &BTreeMap<String, PathBuf>,
    track: &deckhand_audio::TrackAudio,
) -> Result<deckhand_audio::StemLayers> {
    let mut decoded = stems
        .iter()
        .map(|(name, file)| Ok((name.as_str(), deckhand_audio::decode_file(file)?)))
        .collect::<Result<Vec<_>>>()?;
    let all: Vec<&deckhand_audio::TrackAudio> = decoded.iter().map(|(_, audio)| audio).collect();
    let gains = deckhand_audio::fit_gains(track, &all);
    for ((_, audio), gain) in decoded.iter_mut().zip(gains) {
        audio.samples.iter_mut().for_each(|s| *s *= gain);
    }
    let part =
        |is: &dyn Fn(&str) -> bool| decoded.iter().find(|(name, _)| is(name)).map(|(_, a)| a);
    let vocals = part(&|s| is_vocals(s));
    let bass = part(&|s| s.eq_ignore_ascii_case("bass"));
    let drums = part(&|s| s.eq_ignore_ascii_case("drums"));
    if vocals.is_none() && bass.is_none() && drums.is_none() {
        bail!("the model has no vocal, bass or drum stem to play");
    }
    Ok(deckhand_audio::StemLayers::new(vocals, bass, drums, track.sample_rate, track.frames()))
}

/// Demucs turns each stem down on its own when it would clip (and saves
/// 16-bit files), so its stems stop adding up to the track. Unless the
/// command says otherwise, it is asked for float files at their real level,
/// and on Apple Silicon (with `gpu`) to run on the GPU, about three times
/// as fast; elsewhere it finds an Nvidia GPU by itself.
fn model_args(command: &[String], gpu: bool) -> Vec<String> {
    let mut args = command.to_vec();
    let program = command.first().map(Path::new).and_then(Path::file_stem);
    if program.is_some_and(|p| p.eq_ignore_ascii_case("demucs")) {
        let has = |flags: &[&str]| command.iter().any(|a| flags.contains(&a.as_str()));
        if gpu
            && cfg!(all(target_os = "macos", target_arch = "aarch64"))
            && !has(&["-d", "--device"])
        {
            args.splice(1..1, ["-d".to_owned(), "mps".to_owned()]);
        }
        if !has(&["--clip-mode"]) {
            args.splice(1..1, ["--clip-mode".to_owned(), "none".to_owned()]);
        }
        if !has(&["--float32", "--int24", "--mp3", "--flac"]) {
            args.insert(1, "--float32".to_owned());
        }
    }
    args
}

/// Live stems are tracked in blocks this long: a piece is a run of them.
const BLOCK_SECS: f64 = 2.0;
/// Blocks in the piece under the playhead when it has none yet: short, so
/// the stems there play within a couple of seconds.
const FIRST_BLOCKS: usize = 3;
/// Blocks in the other pieces: longer, as each run has its overhead.
const PIECE_BLOCKS: usize = 10;
/// Audio on each side of a piece the model also hears, so it separates the
/// piece's edges as well as the middle.
const CONTEXT_SECS: f64 = 2.0;
/// Pieces meet in a cross-fade this long.
const JOIN_SECS: f64 = 0.25;

/// Separates decks' tracks for live stems, a piece at a time, on a thread
/// of its own: always the piece nearest to a deck's playhead that is still
/// missing, across all decks, so the stems play within seconds and follow
/// jumps. A finished track goes in the cache like any separation. Demucs
/// models run in a helper that keeps the model loaded (see [`warm`]).
///
/// [`warm`]: LiveSeparator::warm
pub struct LiveSeparator {
    jobs: Sender<LiveMessage>,
    pub events: Receiver<Event>,
}

enum LiveMessage {
    Job(LiveJob),
    /// Load this model now, so the first piece doesn't wait for it.
    Warm(StemModel),
    /// Close the helper, freeing its memory.
    Rest,
}

impl Default for LiveSeparator {
    fn default() -> Self {
        let (jobs, queue) = channel::<LiveMessage>();
        let (events_tx, events) = channel();
        std::thread::Builder::new()
            .name("live stems".into())
            .spawn(move || live_loop(&queue, &events_tx, &cache_dir()))
            .expect("starting the live stems thread");
        Self { jobs, events }
    }
}

impl LiveSeparator {
    pub fn submit(&self, job: LiveJob) {
        let _ = self.jobs.send(LiveMessage::Job(job));
    }

    /// Gets the model ready ahead of the first track (for models that run
    /// in a helper; nothing for others).
    pub fn warm(&self, model: StemModel) {
        let _ = self.jobs.send(LiveMessage::Warm(model));
    }

    /// Closes the model's helper (stems turned off).
    pub fn rest(&self) {
        let _ = self.jobs.send(LiveMessage::Rest);
    }
}

/// The Demucs helper ([`demucs_worker.py`]) with a model loaded.
///
/// [`demucs_worker.py`]: https://github.com/roflox/RofLoxBox/blob/main/crates/app/src/stems/demucs_worker.py
struct Helper {
    model: StemModel,
    child: Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}

const HELPER_SCRIPT: &str = include_str!("stems/demucs_worker.py");

impl Helper {
    /// Starts the helper in the Python Demucs is installed in, for a model
    /// whose command runs `demucs`; `None` for other models.
    fn start(model: &StemModel) -> Option<Result<Self>> {
        let (python, name) = demucs_python(model)?;
        Some(Self::spawn(model, &python, &name))
    }

    fn spawn(model: &StemModel, python: &Path, name: &str) -> Result<Self> {
        let script = deckhand_library::data_dir().join("demucs_worker.py");
        if std::fs::read_to_string(&script).ok().as_deref() != Some(HELPER_SCRIPT) {
            std::fs::create_dir_all(deckhand_library::data_dir())?;
            std::fs::write(&script, HELPER_SCRIPT)?;
        }
        let mut child = no_window(&mut Command::new(python))
            .arg("-I")
            .arg(&script)
            .arg(name)
            .current_dir(&model.dir)
            .env("PATH", search_path(&model.dir))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", python.display()))?;
        let input = child.stdin.take().context("helper input")?;
        let output = std::io::BufReader::new(child.stdout.take().context("helper output")?);
        let mut helper = Self { model: model.clone(), child, input, output };
        let ready = helper.answer()?;
        if !ready.get("ready").is_some_and(serde_json::Value::is_array) {
            bail!("the Demucs helper did not start: {ready}");
        }
        Ok(helper)
    }

    /// The helper's next line.
    fn answer(&mut self) -> Result<serde_json::Value> {
        use std::io::BufRead;
        let mut line = String::new();
        if self.output.read_line(&mut line)? == 0 {
            bail!("the Demucs helper stopped");
        }
        let value: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("the Demucs helper said: {line}"))?;
        if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
            bail!("Demucs: {error}");
        }
        Ok(value)
    }

    /// Separates `wav` into stem files in `output`.
    fn separate(&mut self, wav: &Path, output: &Path) -> Result<()> {
        use std::io::Write;
        let request = serde_json::json!({ "input": wav, "output": output });
        writeln!(self.input, "{request}")?;
        self.input.flush()?;
        self.answer().map(|_| ())
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether the model's command runs `demucs`.
pub fn uses_demucs(model: &StemModel) -> bool {
    model
        .command
        .first()
        .and_then(|p| Path::new(p).file_stem())
        .is_some_and(|p| p.eq_ignore_ascii_case("demucs"))
}

/// The Python the fast Demucs helper runs with, for a model whose command
/// runs `demucs`: the one set, else the one found.
pub fn helper_python(model: &StemModel) -> Option<PathBuf> {
    demucs_python(model).map(|(python, _)| python)
}

/// For a model whose command runs `demucs`: the Python it is installed in
/// (the one set, else named by the `demucs` launcher's first line, or next
/// to it) and the Demucs model name (`-n`, else htdemucs).
fn demucs_python(model: &StemModel) -> Option<(PathBuf, String)> {
    if !uses_demucs(model) {
        return None;
    }
    let program = model.command.first()?;
    let name = model
        .command
        .iter()
        .position(|a| a == "-n" || a == "--name")
        .and_then(|i| model.command.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "htdemucs".into());
    if let Some(python) = &model.python {
        return Some((python.clone(), name));
    }
    let launcher = if Path::new(program).is_absolute() {
        Some(PathBuf::from(program))
    } else {
        std::env::split_paths(&search_path(&model.dir))
            .flat_map(|dir| [dir.join(program), dir.join(format!("{program}.exe"))])
            .find(|p| p.is_file())
    }?;
    let first_line = std::fs::read(&launcher).ok().and_then(|bytes| {
        let line = bytes.split(|b| *b == b'\n').next()?.strip_prefix(b"#!")?.to_vec();
        String::from_utf8(line).ok()
    });
    let named =
        first_line.map(|l| PathBuf::from(l.trim())).filter(|p| p.is_absolute() && p.is_file());
    let home =
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let dir = launcher.parent().map(Path::to_path_buf).unwrap_or_default();
    // Beside it in a virtual environment; or, installed with pip on
    // Windows, the launcher is in Python's Scripts folder.
    let beside = [
        dir.join("python"),
        dir.join("python3"),
        dir.join("python.exe"),
        dir.parent().map(|p| p.join("python.exe")).unwrap_or_default(),
    ];
    // pipx on Windows puts a copy of the launcher in its bin folder, and
    // keeps the environment under PIPX_HOME, AppData or the home folder.
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let pipx_homes = std::env::var_os("PIPX_HOME")
        .map(PathBuf::from)
        .into_iter()
        .chain(local.map(|l| l.join("pipx/pipx")))
        .chain(home.iter().flat_map(|h| [h.join(".local/pipx"), h.join("pipx")]));
    let pipx = pipx_homes
        .map(|p| p.join("venvs/demucs"))
        .flat_map(|v| [v.join("bin/python"), v.join("Scripts/python.exe")]);
    let python = named.or_else(|| beside.into_iter().chain(pipx).find(|p| p.is_file()))?;
    Some((python, name))
}

/// A track being separated piece by piece.
struct Live {
    job: LiveJob,
    /// The track as deckhand decodes it.
    track: Arc<deckhand_audio::TrackAudio>,
    layers: Arc<deckhand_audio::StemLayers>,
    /// Every stem of the model, for the cache.
    stems: BTreeMap<String, Vec<f32>>,
    /// Blocks done.
    done: Vec<bool>,
    /// Frames per block.
    block: usize,
    dir: PathBuf,
}

impl Live {
    fn cancelled(&self) -> bool {
        self.job.playhead.load(Ordering::Relaxed) == CANCELLED
    }

    /// The most urgent piece still missing, as (urgency, first block, end
    /// block): the playhead's own block first, then ones ahead by their
    /// distance, ones behind at three times theirs (and a bit).
    fn next_piece(&self) -> Option<(usize, usize, usize)> {
        let seconds = f64::from_bits(self.job.playhead.load(Ordering::Relaxed)).max(0.0);
        let blocks = self.done.len();
        let at =
            ((seconds * f64::from(self.track.sample_rate)) as usize / self.block).min(blocks - 1);
        let ahead = (at..blocks).find(|&b| !self.done[b]).map(|b| (b - at, b));
        // Behind counts one more, so ahead wins a tie: decks play forwards.
        let behind = (0..at).rev().find(|&b| !self.done[b]).map(|b| ((at - b) * 3 + 1, b));
        let (urgency, start) = match (ahead, behind) {
            (Some(a), Some(b)) => a.min(b),
            (a, b) => a.or(b)?,
        };
        let length = if urgency == 0 { FIRST_BLOCKS } else { PIECE_BLOCKS };
        if start >= at {
            // Forwards from there, up to blocks already done.
            let end = (start..(start + length).min(blocks))
                .find(|&b| self.done[b])
                .unwrap_or((start + length).min(blocks));
            Some((urgency, start, end))
        } else {
            // Backwards, ending at the missing block next to what's done.
            let first = (start.saturating_sub(length - 1)..=start)
                .rev()
                .take_while(|&b| !self.done[b])
                .last()
                .unwrap_or(start);
            Some((urgency, first, start + 1))
        }
    }

    /// Separates blocks `first..end` and puts them in place.
    fn separate(
        &mut self,
        (first, end): (usize, usize),
        helper: &mut Option<Helper>,
        running: &Mutex<Option<Child>>,
    ) -> Result<()> {
        let rate = self.track.sample_rate;
        let frames_total = self.track.frames();
        let seconds = |s: f64| (s * f64::from(rate)) as usize;
        let (start, stop) = (first * self.block, (end * self.block).min(frames_total));
        let (from, to) = (
            start.saturating_sub(seconds(CONTEXT_SECS)),
            (stop + seconds(CONTEXT_SECS)).min(frames_total),
        );
        let work = self.dir.with_extension("work").join(format!("piece-{first}"));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
        let piece = deckhand_audio::TrackAudio {
            sample_rate: rate,
            samples: self.track.samples[from * 2..to * 2].to_vec(),
        };
        let wav = work.join("input.wav");
        deckhand_audio::write_wav_file(&wav, rate, &piece.samples)?;
        let output = work.join("out");
        let model = &self.job.model;
        let by_helper = match helper.as_mut().filter(|h| h.model == *model) {
            Some(h) => match h.separate(&wav, &output) {
                Ok(()) => find_stems(&output, &model.stems, &BTreeMap::new()).ok(),
                Err(_) => {
                    // Broken: the next piece starts a new one.
                    *helper = None;
                    None
                }
            },
            None => None,
        };
        let found = match by_helper {
            Some(found) => found,
            None => run_model(model, &wav, &output, &work, running, &|_| {})?,
        };
        let decoded = found
            .iter()
            .map(|(name, file)| Ok((name.clone(), deckhand_audio::decode_file(file)?)))
            .collect::<Result<Vec<_>>>()?;
        let _ = std::fs::remove_dir_all(&work);
        let all: Vec<&deckhand_audio::TrackAudio> = decoded.iter().map(|(_, a)| a).collect();
        let gains = deckhand_audio::fit_gains(&piece, &all);
        let join = seconds(JOIN_SECS);
        let left = first > 0 && self.done[first - 1];
        let right = end < self.done.len() && self.done[end];
        for ((name, audio), gain) in decoded.iter().zip(gains) {
            let part = live_part(name);
            let Some(buffer) = self.stems.get_mut(name) else { continue };
            let step = f64::from(audio.sample_rate) / f64::from(rate);
            for frame in from..to {
                // In the piece, or fading in or out over a ready neighbour.
                let weight = if (start..stop).contains(&frame) {
                    1.0
                } else if left && frame < start && frame + join >= start {
                    1.0 - (start - frame) as f32 / join as f32
                } else if right && frame >= stop && frame < stop + join {
                    1.0 - (frame - stop) as f32 / join as f32
                } else {
                    continue;
                };
                let [l, r] = audio.sample_at((frame - from) as f64 * step, true);
                let new = [l * gain, r * gain];
                let old = [buffer[frame * 2], buffer[frame * 2 + 1]];
                let mixed =
                    [old[0] + (new[0] - old[0]) * weight, old[1] + (new[1] - old[1]) * weight];
                buffer[frame * 2] = mixed[0];
                buffer[frame * 2 + 1] = mixed[1];
                if let Some(part) = part {
                    self.layers.set(part, frame, mixed);
                }
            }
        }
        for block in first..end {
            self.done[block] = true;
            self.layers.set_ready(block);
        }
        Ok(())
    }

    /// Keeps the finished separation, as a whole-track one would be.
    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        for (name, samples) in &self.stems {
            deckhand_audio::write_float_wav_file(
                &self.dir.join(format!("{name}.wav")),
                self.track.sample_rate,
                samples,
            )?;
        }
        let _ = std::fs::remove_dir_all(self.dir.with_extension("work"));
        Ok(())
    }
}

/// The live part a stem plays as (vocals, bass, drums), if any.
fn live_part(stem: &str) -> Option<usize> {
    if is_vocals(stem) {
        Some(deckhand_audio::VOCALS)
    } else if stem.eq_ignore_ascii_case("bass") {
        Some(deckhand_audio::BASS)
    } else if stem.eq_ignore_ascii_case("drums") {
        Some(deckhand_audio::DRUMS)
    } else {
        None
    }
}

fn live_loop(queue: &Receiver<LiveMessage>, events: &Sender<Event>, cache: &Path) {
    let running = Mutex::new(None);
    let mut active: Vec<Live> = Vec::new();
    let mut helper: Option<Helper> = None;
    // Models whose helper would not start: they run as commands.
    let mut no_helper: Vec<StemModel> = Vec::new();
    let mut ensure_helper = |model: &StemModel, helper: &mut Option<Helper>| {
        if helper.as_ref().is_some_and(|h| h.model == *model) || no_helper.contains(model) {
            return;
        }
        *helper = None;
        match Helper::start(model) {
            Some(Ok(started)) => *helper = Some(started),
            Some(Err(_)) | None => no_helper.push(model.clone()),
        }
    };
    loop {
        // Wait for work when idle; take whatever else came meanwhile.
        let mut messages: Vec<LiveMessage> = Vec::new();
        if active.is_empty() {
            match queue.recv() {
                Ok(message) => messages.push(message),
                Err(_) => return,
            }
        }
        messages.extend(queue.try_iter());
        for message in messages {
            match message {
                LiveMessage::Job(job) => start_live(job, events, cache, &mut active),
                LiveMessage::Warm(model) => ensure_helper(&model, &mut helper),
                LiveMessage::Rest => helper = None,
            }
        }
        active.retain(|live| !live.cancelled());
        let next = active
            .iter()
            .enumerate()
            .filter_map(|(i, live)| {
                live.next_piece().map(|(urgency, first, end)| (urgency, i, first, end))
            })
            .min();
        let Some((_, i, first, end)) = next else { continue };
        ensure_helper(&active[i].job.model, &mut helper);
        let live = &mut active[i];
        if let Err(err) = live.separate((first, end), &mut helper, &running) {
            let deck = live.job.deck;
            let _ = events.send(Event::Failed {
                title: live.job.title.clone(),
                error: format!("{err:#}"),
                deck: Some(deck),
            });
            active.remove(i);
            continue;
        }
        if live.done.iter().all(|d| *d) {
            if let Err(err) = live.save() {
                let _ = events.send(Event::Failed {
                    title: live.job.title.clone(),
                    error: format!("keeping the stems: {err:#}"),
                    deck: None,
                });
            }
            touch(&live.dir);
            trim_cache(cache, CACHE_LIMIT);
            active.remove(i);
        }
    }
}

/// Plays the cached stems of a track separated before, else gives the deck
/// silent stems at once (its parts can be switched straight away) and lines
/// the track up for separating piece by piece.
fn start_live(job: LiveJob, events: &Sender<Event>, cache: &Path, active: &mut Vec<Live>) {
    // A deck taking another track drops its earlier job.
    active.retain(|live| live.job.deck != job.deck && !live.cancelled());
    let dir = cache.join(&job.model.id).join(cache_key(&job.input));
    let fail = |err: anyhow::Error| {
        let _ = events.send(Event::Failed {
            title: job.title.clone(),
            error: format!("{err:#}"),
            deck: Some(job.deck),
        });
    };
    if let Ok(stems) = find_stems(&dir, &job.model.stems, &BTreeMap::new()) {
        touch(&dir);
        match layers(&stems, &job.audio) {
            Ok(layers) => {
                let _ = events.send(Event::Live {
                    deck: job.deck,
                    input: job.input.clone(),
                    stems: Arc::new(layers),
                });
            }
            Err(err) => fail(err),
        }
        return;
    }
    let track = job.audio.clone();
    let has = [deckhand_audio::VOCALS, deckhand_audio::BASS, deckhand_audio::DRUMS]
        .map(|part| job.model.stems.iter().any(|s| live_part(s) == Some(part)));
    let block = (BLOCK_SECS * f64::from(track.sample_rate)) as usize;
    let frames = track.frames();
    let layers = Arc::new(deckhand_audio::StemLayers::empty(has, frames, block));
    let _ = events.send(Event::Live {
        deck: job.deck,
        input: job.input.clone(),
        stems: layers.clone(),
    });
    let stems = job.model.stems.iter().map(|s| (s.clone(), vec![0.0; frames * 2])).collect();
    active.push(Live {
        layers,
        done: vec![false; frames.div_ceil(block).max(1)],
        stems,
        block,
        dir,
        track,
        job,
    });
}

/// The cache folder for a file: its path, size and modification time.
fn cache_key(input: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    deckhand_library::file_stamp(input).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The track's stems by name, from the cache or by running the model.
fn separate(
    model: &StemModel,
    input: &Path,
    cache: &Path,
    running: &Mutex<Option<Child>>,
    progress: &dyn Fn(Option<u32>),
) -> Result<BTreeMap<String, PathBuf>> {
    let dir = cache.join(&model.id).join(cache_key(input));
    if let Ok(found) = find_stems(&dir, &model.stems, &BTreeMap::new()) {
        return Ok(found);
    }
    let work = dir.with_extension("work");
    let _ = std::fs::remove_dir_all(&work);
    let output = work.join("out");
    std::fs::create_dir_all(&output).with_context(|| format!("creating {}", output.display()))?;
    let audio = deckhand_audio::decode_file(input)?;
    let wav = work.join("input.wav");
    deckhand_audio::write_wav_file(&wav, audio.sample_rate, &audio.samples)?;
    drop(audio);

    let found = run_model(model, &wav, &output, &work, running, progress)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut stems = BTreeMap::new();
    for (stem, file) in found {
        let ext = file.extension().map_or("wav".into(), |e| e.to_string_lossy().into_owned());
        let to = dir.join(format!("{stem}.{ext}"));
        if std::fs::rename(&file, &to).is_err() {
            std::fs::copy(&file, &to).with_context(|| format!("copying {}", file.display()))?;
        }
        stems.insert(stem, to);
    }
    let _ = std::fs::remove_dir_all(&work);
    Ok(stems)
}

/// Runs the model on `wav`, its results going to `output` (and its log to
/// `work`). Demucs runs on the Mac's GPU when there is one, else (or when
/// that fails) on the processor.
fn run_model(
    model: &StemModel,
    wav: &Path,
    output: &Path,
    work: &Path,
    running: &Mutex<Option<Child>>,
    progress: &dyn Fn(Option<u32>),
) -> Result<BTreeMap<String, PathBuf>> {
    let gpu = model_args(&model.command, true) != model_args(&model.command, false);
    let run = |gpu| match run_model_on(model, wav, output, work, running, progress, gpu) {
        Err(error) if old_demucs(&error) => {
            OLD_DEMUCS.store(true, Ordering::Relaxed);
            run_model_on(model, wav, output, work, running, progress, gpu)
        }
        result => result,
    };
    match run(gpu) {
        Err(_) if gpu => run(false),
        result => result,
    }
}

/// Set once Demucs turns down `--clip-mode none`: the release on PyPI
/// (4.0) only has rescale and clamp. Rescale it is then: it turns a stem
/// down as a whole, which [`layers`] undoes.
static OLD_DEMUCS: AtomicBool = AtomicBool::new(false);

fn old_demucs(error: &anyhow::Error) -> bool {
    let error = error.to_string();
    !OLD_DEMUCS.load(Ordering::Relaxed)
        && error.contains("--clip-mode")
        && error.contains("invalid choice: 'none'")
}

/// `args` with `--clip-mode none` swapped for rescale when Demucs is too
/// old for it.
fn clip_mode_for_this_demucs(mut args: Vec<String>) -> Vec<String> {
    if OLD_DEMUCS.load(Ordering::Relaxed) {
        for i in 1..args.len() {
            if args[i - 1] == "--clip-mode" && args[i] == "none" {
                args[i] = "rescale".into();
            }
        }
    }
    args
}

fn run_model_on(
    model: &StemModel,
    wav: &Path,
    output: &Path,
    work: &Path,
    running: &Mutex<Option<Child>>,
    progress: &dyn Fn(Option<u32>),
    gpu: bool,
) -> Result<BTreeMap<String, PathBuf>> {
    let _ = std::fs::remove_dir_all(output);
    std::fs::create_dir_all(output).with_context(|| format!("creating {}", output.display()))?;
    let fill = |arg: &String| {
        arg.replace("{input}", &wav.to_string_lossy())
            .replace("{output}", &output.to_string_lossy())
            .replace("{model}", &model.dir.to_string_lossy())
    };
    let args: Vec<String> =
        clip_mode_for_this_demucs(model_args(&model.command, gpu)).iter().map(fill).collect();
    let mut child = no_window(&mut Command::new(&args[0]))
        .args(&args[1..])
        .current_dir(&model.dir)
        .env("PATH", search_path(&model.dir))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            let install = if model.install.is_empty() {
                String::new()
            } else {
                format!(" Install it with: {}", model.install)
            };
            format!(
                "{} is not installed or not found.{install} Or put its full path in {}",
                args[0],
                model.dir.join(MODEL_FILE).display()
            )
        })?;
    let log = Arc::new(Mutex::new(String::new()));
    let latest = Arc::new(Mutex::new(None::<u32>));
    let readers: Vec<_> = [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|stream| {
        let (log, latest) = (log.clone(), latest.clone());
        std::thread::spawn(move || read_output(stream, &log, &latest))
    })
    .collect();
    *running.lock().map_err(|_| anyhow!("stems lock"))? = Some(child);
    let status = loop {
        if let Some(status) = running
            .lock()
            .map_err(|_| anyhow!("stems lock"))?
            .as_mut()
            .map(Child::try_wait)
            .transpose()?
            .flatten()
        {
            break status;
        }
        std::thread::sleep(Duration::from_millis(100));
        progress(*latest.lock().map_err(|_| anyhow!("stems lock"))?);
    };
    *running.lock().map_err(|_| anyhow!("stems lock"))? = None;
    for reader in readers {
        let _ = reader.join();
    }
    let log = log.lock().map_err(|_| anyhow!("stems lock"))?.clone();
    let _ = std::fs::write(work.join("log.txt"), &log);
    if !status.success() {
        bail!("{}", failure(&model.name, &status.to_string(), &log, &work.join("log.txt")));
    }
    find_stems(output, &model.stems, &model.files)
}

/// Why a model failed: the cause first (the tool's last real line, such as
/// Python's exception), then the end of its output and where the log is.
fn failure(model: &str, status: &str, log: &str, log_file: &Path) -> String {
    let lines: Vec<&str> =
        log.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    // Python underlines the failing code with ^ and ~ after the line.
    let cause = lines
        .iter()
        .rev()
        .find(|l| !l.trim().chars().all(|c| matches!(c, '^' | '~' | ' ')))
        .map_or("no output", |l| l.trim());
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    format!(
        "{model} failed: {cause}\n\nExit {status}. The end of its output:\n{tail}\n\nThe whole log: {}",
        log_file.display()
    )
}

/// Keeps the tool's output, and the last percentage it printed.
fn read_output(mut stream: Box<dyn Read + Send>, log: &Mutex<String>, latest: &Mutex<Option<u32>>) {
    let mut buffer = [0u8; 4096];
    while let Ok(n) = stream.read(&mut buffer) {
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buffer[..n]);
        if let Some(percent) = text.split(['\r', '\n']).filter_map(percent).next_back()
            && let Ok(mut latest) = latest.lock()
        {
            *latest = Some(percent);
        }
        if let Ok(mut log) = log.lock() {
            // Progress bars redraw with \r; keep only the last state.
            log.push_str(&text.replace('\r', "\n"));
        }
    }
}

/// The last `NN%` in a line of output.
fn percent(line: &str) -> Option<u32> {
    let at = line.rfind('%')?;
    let digits: String = line[..at]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse::<u32>().ok().filter(|p| *p <= 100)
}

/// PATH with the places separation tools usually land in, as an app
/// started from the Finder gets only the system's.
fn search_path(model_dir: &Path) -> std::ffi::OsString {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut dirs =
        vec![model_dir.join("bin"), model_dir.join("env/bin"), model_dir.join(".venv/bin")];
    dirs.extend(
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".local/bin"),
    ]);
    std::env::join_paths(dirs).unwrap_or_default()
}

/// Each stem's file in `dir`: by its `files` pattern, else the audio file
/// named like the stem, else one with the stem as a word of its name
/// (`Song_(Vocals)_model`), but not `no_vocals`.
fn find_stems(
    dir: &Path,
    stems: &[String],
    patterns: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = Vec::new();
    audio_files(dir, &mut files);
    files.sort();
    let words = |file: &Path| -> Vec<String> {
        let name = file.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        name.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut found = BTreeMap::new();
    let mut missing = Vec::new();
    for stem in stems {
        let lower = stem.to_lowercase();
        let file = match patterns.get(stem) {
            Some(pattern) => {
                Some(dir.join(pattern.replace("{stem}", stem))).filter(|f| f.is_file())
            }
            None => files
                .iter()
                .find(|f| words(f) == [lower.clone()])
                .or_else(|| {
                    files.iter().find(|f| {
                        let w = words(f);
                        w.contains(&lower) && !w.iter().any(|w| w == "no" || w == "without")
                    })
                })
                .cloned(),
        };
        match file {
            Some(file) => {
                found.insert(stem.clone(), file);
            }
            None => missing.push(stem.as_str()),
        }
    }
    if !missing.is_empty() {
        let names: Vec<String> = files
            .iter()
            .filter_map(|f| f.strip_prefix(dir).ok())
            .map(|f| f.display().to_string())
            .collect();
        bail!(
            "no file for {} among the results ({})",
            missing.join(", "),
            if names.is_empty() { "none".to_owned() } else { names.join(", ") }
        );
    }
    Ok(found)
}

fn audio_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            audio_files(&path, out);
        } else if path.extension().is_some_and(|e| AUDIO.iter().any(|a| e.eq_ignore_ascii_case(a)))
        {
            out.push(path);
        }
    }
}

/// Mixes the stem files into one 24-bit WAV at `out` (written next to it
/// first, so a half-made file never takes its place).
fn mix(files: &[&Path], out: &Path) -> Result<()> {
    let mut rate = None;
    let mut sum: Vec<f32> = Vec::new();
    for file in files {
        let audio = deckhand_audio::decode_file(file)?;
        if rate.is_some_and(|r| r != audio.sample_rate) {
            bail!("the stems have different sample rates");
        }
        rate = Some(audio.sample_rate);
        if sum.len() < audio.samples.len() {
            sum.resize(audio.samples.len(), 0.0);
        }
        for (s, v) in sum.iter_mut().zip(&audio.samples) {
            *s += v;
        }
    }
    let rate = rate.ok_or_else(|| anyhow!("no stems to mix"))?;
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let partial = out.with_extension("wav.part");
    deckhand_audio::write_wav_file(&partial, rate, &sum)?;
    std::fs::rename(&partial, out).with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(stems: &[&str], command: &[&str], dir: &Path) -> StemModel {
        StemModel {
            id: "test".into(),
            name: "Test model".into(),
            stems: stems.iter().map(|s| s.to_string()).collect(),
            command: command.iter().map(|s| s.to_string()).collect(),
            files: BTreeMap::new(),
            install: "pipx install no-such-tool".into(),
            python: None,
            dir: dir.to_path_buf(),
        }
    }

    fn keeps(recipes: &[Recipe]) -> Vec<(&str, Vec<&str>)> {
        recipes
            .iter()
            .map(|r| (r.suffix.as_str(), r.keep.iter().map(String::as_str).collect()))
            .collect()
    }

    #[test]
    fn recipes_follow_the_models_stems() {
        let four = model(&["drums", "bass", "other", "vocals"], &["x"], Path::new("/"));
        assert_eq!(
            keeps(&recipes(&four)),
            [
                ("Acapella", vec!["vocals"]),
                ("Instrumental", vec!["drums", "bass", "other"]),
                ("No Drums", vec!["bass", "other", "vocals"]),
                ("Drums", vec!["drums"]),
                ("Bass", vec!["bass"]),
                ("Other", vec!["other"]),
            ]
        );
        // Two stems: nothing doubles up.
        let two = model(&["vocals", "instrumental"], &["x"], Path::new("/"));
        assert_eq!(
            keeps(&recipes(&two)),
            [("Acapella", vec!["vocals"]), ("Instrumental", vec!["instrumental"])]
        );
    }

    #[test]
    fn stem_files_are_found_by_name() {
        let dir = std::env::temp_dir().join(format!("deckhand-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("htdemucs")).unwrap();
        for name in [
            "htdemucs/drums.wav",
            "Song_(Vocals)_model.flac",
            "Song_(Instrumental)_model.flac",
            "no_bass.wav",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let stems = ["drums".to_owned(), "vocals".to_owned(), "instrumental".to_owned()];
        let found = find_stems(&dir, &stems, &BTreeMap::new()).unwrap();
        assert_eq!(found["drums"], dir.join("htdemucs/drums.wav"));
        assert_eq!(found["vocals"], dir.join("Song_(Vocals)_model.flac"));
        // "no_bass" is not bass.
        let err = find_stems(&dir, &["bass".to_owned()], &BTreeMap::new()).unwrap_err();
        assert!(format!("{err}").contains("no file for bass"), "{err}");
        let patterns = BTreeMap::from([("bass".to_owned(), "no_{stem}.wav".to_owned())]);
        assert!(find_stems(&dir, &["bass".to_owned()], &patterns).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_lead_with_the_cause() {
        let log = "Traceback (most recent call last):\n  File \"x.py\", line 5\n    import numpy\n    ^^^^^^^^^^^^\nModuleNotFoundError: No module named 'numpy'\n\n";
        let text = failure("Demucs", "exit status: 1", log, Path::new("/tmp/log.txt"));
        assert_eq!(
            text.lines().next(),
            Some("Demucs failed: ModuleNotFoundError: No module named 'numpy'")
        );
        assert!(text.ends_with("The whole log: /tmp/log.txt"), "{text}");
        let underlined = "    import numpy\n    ^^^^^^\n";
        let text = failure("Demucs", "exit status: 1", underlined, Path::new("/l"));
        assert!(text.starts_with("Demucs failed: import numpy"), "{text}");
        assert!(failure("D", "1", "", Path::new("/l")).starts_with("D failed: no output"));
    }

    #[test]
    fn cache_keeps_the_latest_tracks() {
        let cache = std::env::temp_dir().join(format!("deckhand-trim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        for (i, name) in ["old", "new"].iter().enumerate() {
            let dir = cache.join("model").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("vocals.wav"), vec![0u8; 1000]).unwrap();
            touch(&dir);
            if i == 0 {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        trim_cache(&cache, 1500);
        assert!(!cache.join("model/old").exists());
        assert!(cache.join("model/new/vocals.wav").exists());
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn progress_is_read_from_bars() {
        assert_eq!(percent(" 45%|████▌     | 52.6/117.0"), Some(45));
        assert_eq!(percent("100%|██████████|"), Some(100));
        assert_eq!(percent("no progress here"), None);
        assert_eq!(percent("250%"), None);
    }

    #[test]
    fn made_files_are_named_after_the_track() {
        let path = output_path(Path::new("/Stems"), "AC/DC", "Back: In Black", "Acapella");
        assert_eq!(path, Path::new("/Stems/AC-DC/Back- In Black (Acapella).wav"));
        assert_eq!(
            output_path(Path::new("/S"), "", "", "Bass"),
            Path::new("/S/Untitled (Bass).wav")
        );
    }

    /// A stand-in model: each stem is a copy of the input, and each run is
    /// counted in `runs` next to it.
    #[cfg(unix)]
    fn fake_model(dir: &Path) -> StemModel {
        let script = r#"echo run >> runs
echo " 50%|#####     |"
cp "$1" "$2/vocals.wav"
mkdir -p "$2/sub" && cp "$1" "$2/sub/Song_(Drums)_fake.wav"
"#;
        std::fs::write(dir.join("separate.sh"), script).unwrap();
        model(&["vocals", "drums"], &["/bin/sh", "separate.sh", "{input}", "{output}"], dir)
    }

    #[cfg(unix)]
    #[test]
    fn live_stems_start_at_the_playhead() {
        let root = std::env::temp_dir().join(format!("deckhand-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let model_dir = root.join("model");
        std::fs::create_dir_all(&model_dir).unwrap();
        let mut model = fake_model(&model_dir);
        // Which piece each run was given.
        let script = std::fs::read_to_string(model_dir.join("separate.sh")).unwrap();
        std::fs::write(model_dir.join("separate.sh"), script.replace("echo run", "echo \"$1\""))
            .unwrap();
        model.id = "fake".into();

        // 50 s.
        let rate = 8_000;
        let input = root.join("song.wav");
        let samples: Vec<f32> =
            (0..rate * 50 * 2).map(|i| ((i / 2) as f32 * 0.01).sin() * 0.5).collect();
        deckhand_audio::write_wav_file(&input, rate as u32, &samples).unwrap();
        let original = deckhand_audio::decode_file(&input).unwrap();
        let (jobs, queue) = channel();
        let (events_tx, events) = channel();
        let cache = root.join("cache");
        let thread_cache = cache.clone();
        let worker = std::thread::spawn(move || live_loop(&queue, &events_tx, &thread_cache));
        jobs.send(LiveMessage::Job(LiveJob {
            deck: 1,
            title: "Song".into(),
            input: input.clone(),
            model,
            audio: Arc::new(original.clone()),
            playhead: Arc::new(AtomicU64::new(30.0_f64.to_bits())),
        }))
        .unwrap();
        drop(jobs);
        let mut layers = None;
        for event in events.iter() {
            match event {
                Event::Live { deck, stems, .. } => {
                    assert_eq!(deck, 1);
                    layers = Some(stems);
                }
                Event::Failed { error, .. } => panic!("{error}"),
                _ => {}
            }
        }
        worker.join().unwrap();
        let layers = layers.expect("stems for the deck");
        assert_eq!(layers.ready_share(), 1.0);

        let runs = std::fs::read_to_string(model_dir.join("runs")).unwrap();
        // Blocks of 2 s; the playhead's at 15: three blocks there first,
        // then onwards, then behind it.
        let order: Vec<usize> = runs
            .lines()
            .map(|l| l.split("piece-").nth(1).unwrap().split('/').next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(order, [15, 18, 5, 0]);

        // The stems are the track again, joins included, and kept.
        for frame in (0..original.frames()).step_by(1_999) {
            let want = original.frame(frame as i64)[0];
            let got = layers.get(deckhand_audio::VOCALS, frame)[0];
            assert!((got - want).abs() < 1e-3, "frame {frame}: {got} vs {want}");
        }
        let kept = find_stems(
            &cache.join("fake").join(cache_key(&input)),
            &["vocals".into(), "drums".into()],
            &BTreeMap::new(),
        )
        .unwrap();
        let vocals = deckhand_audio::decode_file(&kept["vocals"]).unwrap();
        assert_eq!(vocals.samples.len(), original.samples.len());
        assert!(vocals.samples.iter().zip(&original.samples).all(|(v, o)| (v - o).abs() < 1e-4));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn demucs_keeps_full_scale() {
        let strings = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let old = strings(&["demucs", "-n", "htdemucs", "-o", "{output}", "{input}"]);
        assert_eq!(
            model_args(&old, false),
            strings(&[
                "demucs",
                "--float32",
                "--clip-mode",
                "none",
                "-n",
                "htdemucs",
                "-o",
                "{output}",
                "{input}"
            ])
        );
        let own = strings(&[
            "/opt/bin/demucs",
            "-d",
            "cpu",
            "--clip-mode",
            "clamp",
            "--int24",
            "{input}",
        ]);
        assert_eq!(model_args(&own, true), own);
        let other = strings(&["audio-separator", "{input}"]);
        assert_eq!(model_args(&other, true), other);
        let gpu = model_args(&old, true);
        assert_eq!(
            gpu.starts_with(&strings(&["demucs", "--float32", "--clip-mode", "none", "-d", "mps"])),
            cfg!(all(target_os = "macos", target_arch = "aarch64"))
        );
    }

    #[test]
    fn separates_once_and_mixes_what_is_asked() {
        let root = std::env::temp_dir().join(format!("deckhand-stems-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let model_dir = root.join("model");
        std::fs::create_dir_all(&model_dir).unwrap();
        let model = fake_model(&model_dir);

        // One second of a quiet ramp.
        let input = root.join("song.wav");
        let samples: Vec<f32> = (0..48_000 * 2).map(|i| (i % 200) as f32 / 1000.0).collect();
        deckhand_audio::write_wav_file(&input, 48_000, &samples).unwrap();
        let cache = root.join("cache");
        let running = Mutex::new(None);
        let seen = Mutex::new(Vec::new());
        let progress = |p| seen.lock().unwrap().push(p);
        let stems = separate(&model, &input, &cache, &running, &progress).unwrap();
        assert_eq!(stems.keys().collect::<Vec<_>>(), ["drums", "vocals"]);
        assert!(stems["drums"].starts_with(&cache));
        assert!(seen.lock().unwrap().contains(&Some(50)));
        // Again: from the cache, the model does not run.
        let again = separate(&model, &input, &cache, &running, &progress).unwrap();
        assert_eq!(again, stems);
        let runs = std::fs::read_to_string(model_dir.join("runs")).unwrap();
        assert_eq!(runs.lines().count(), 1);

        // Both stems mixed: twice the input, same length and rate.
        let out = root.join("made/Song (Both).wav");
        let files: Vec<&Path> = stems.values().map(PathBuf::as_path).collect();
        mix(&files, &out).unwrap();
        let made = deckhand_audio::decode_file(&out).unwrap();
        let original = deckhand_audio::decode_file(&input).unwrap();
        assert_eq!(made.sample_rate, 48_000);
        assert_eq!(made.samples.len(), original.samples.len());
        for (m, o) in made.samples.iter().zip(&original.samples).step_by(997) {
            assert!((m - 2.0 * o).abs() < 1e-4, "{m} vs {o}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_model_reports_its_output() {
        let root = std::env::temp_dir().join(format!("deckhand-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let input = root.join("song.wav");
        deckhand_audio::write_wav_file(&input, 44_100, &[0.0; 4410 * 2]).unwrap();
        let model =
            model(&["vocals"], &["/bin/sh", "-c", "echo 'CUDA out of memory' >&2; exit 3"], &root);
        let err =
            separate(&model, &input, &root.join("cache"), &Mutex::new(None), &|_| {}).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.starts_with("Test model failed: CUDA out of memory"), "{text}");
        let missing = model_with_command(&root, "no-such-separator-tool");
        let err = separate(&missing, &input, &root.join("cache"), &Mutex::new(None), &|_| {})
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("not installed") && text.contains("pipx install no-such-tool"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn model_with_command(dir: &Path, program: &str) -> StemModel {
        model(&["vocals"], &[program, "{input}"], dir)
    }

    /// How long a deck takes to get stems separated before:
    /// `DECKHAND_TEST_TRACK=song.wav DECKHAND_TEST_STEMS=folder cargo test -p
    /// deckhand-app cached_stems_load -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn cached_stems_load() {
        let track =
            deckhand_audio::decode_file(Path::new(&std::env::var("DECKHAND_TEST_TRACK").unwrap()))
                .unwrap();
        let dir = PathBuf::from(std::env::var("DECKHAND_TEST_STEMS").unwrap());
        let names: Vec<String> = ["drums", "bass", "other", "vocals"].map(String::from).into();
        let stems = find_stems(&dir, &names, &BTreeMap::new()).unwrap();
        let started = std::time::Instant::now();
        let layers = layers(&stems, &track).unwrap();
        eprintln!("cached stems ready in {:.2} s", started.elapsed().as_secs_f64());
        assert!(layers.ready_at(0));
    }

    /// Live (piece by piece) against whole-track separation with a real
    /// model: how soon the first piece plays and how far the results differ,
    /// at the joins and elsewhere. Same variables as `real_model`.
    #[test]
    #[ignore]
    fn real_model_live() {
        let dir = PathBuf::from(std::env::var("DECKHAND_TEST_MODEL").unwrap());
        let track = PathBuf::from(std::env::var("DECKHAND_TEST_TRACK").unwrap());
        let (models, _) = load_models(dir.parent().unwrap());
        let model = models.into_iter().find(|m| m.dir == dir).unwrap();
        let root = std::env::temp_dir().join("deckhand-real-live");
        let _ = std::fs::remove_dir_all(&root);
        let started = std::time::Instant::now();
        let whole =
            separate(&model, &track, &root.join("whole"), &Mutex::new(None), &|_| {}).unwrap();
        eprintln!("whole track: {:.1} s", started.elapsed().as_secs_f64());
        let original = deckhand_audio::decode_file(&track).unwrap();
        let (jobs, queue) = channel();
        let (events_tx, events) = channel();
        let cache = root.join("live");
        let worker = std::thread::spawn(move || live_loop(&queue, &events_tx, &cache));
        // As the app does when it starts: the model loads ahead of the track.
        jobs.send(LiveMessage::Warm(model.clone())).unwrap();
        std::thread::sleep(Duration::from_secs(15));
        let started = std::time::Instant::now();
        let seconds = original.frames() as f64 / f64::from(original.sample_rate);
        jobs.send(LiveMessage::Job(LiveJob {
            deck: 0,
            title: "t".into(),
            input: track.clone(),
            model,
            audio: Arc::new(original.clone()),
            playhead: Arc::new(AtomicU64::new((seconds / 2.0).to_bits())),
        }))
        .unwrap();
        drop(jobs);
        let mut layers = None;
        for event in events.iter() {
            if let Event::Live { stems, .. } = event {
                // Ready where the deck plays.
                let middle = original.frames() as i64 / 2;
                while !stems.ready_at(middle) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                eprintln!(
                    "stems play at the playhead after {:.1} s",
                    started.elapsed().as_secs_f64()
                );
                layers = Some(stems);
            }
        }
        worker.join().unwrap();
        eprintln!("live, all pieces: {:.1} s", started.elapsed().as_secs_f64());
        let layers = layers.unwrap();
        let reference = deckhand_audio::decode_file(&whole["vocals"]).unwrap();
        let piece = (BLOCK_SECS * f64::from(original.sample_rate)) as usize;
        let (mut signal, mut error, mut join_signal, mut join_error) = (0.0, 0.0, 0.0, 0.0);
        for frame in 0..original.frames() {
            let want = f64::from(reference.frame(frame as i64)[0]);
            let got = f64::from(layers.get(deckhand_audio::VOCALS, frame)[0]);
            let near_join = frame % piece < original.sample_rate as usize / 2
                || piece - frame % piece < original.sample_rate as usize / 2;
            let (s, e) = if near_join {
                (&mut join_signal, &mut join_error)
            } else {
                (&mut signal, &mut error)
            };
            *s += want * want;
            *e += (got - want).powi(2);
        }
        let db = |s: f64, e: f64| 10.0 * (s / e.max(1e-12)).log10();
        eprintln!(
            "vocals, live vs whole: {:.1} dB apart away from joins, {:.1} dB within 0.5 s of a block edge",
            db(signal, error),
            db(join_signal, join_error)
        );
    }

    /// With a real separation tool: `DECKHAND_TEST_MODEL=/path/to/models/x
    /// DECKHAND_TEST_TRACK=song.mp3 cargo test -p deckhand-app real_model -- --ignored`
    #[test]
    #[ignore]
    fn real_model() {
        let dir = PathBuf::from(std::env::var("DECKHAND_TEST_MODEL").unwrap());
        let track = PathBuf::from(std::env::var("DECKHAND_TEST_TRACK").unwrap());
        let (models, errors) = load_models(dir.parent().unwrap());
        assert!(errors.is_empty(), "{errors:?}");
        let model = models.into_iter().find(|m| m.dir == dir).unwrap();
        let cache = std::env::temp_dir().join("deckhand-real-model");
        let started = std::time::Instant::now();
        let stems = separate(&model, &track, &cache, &Mutex::new(None), &|p| {
            if let Some(p) = p {
                eprint!("\r{p:3}%");
            }
        })
        .unwrap();
        eprintln!("\nseparated in {:.1} s: {:?}", started.elapsed().as_secs_f64(), stems.keys());
        let original = deckhand_audio::decode_file(&track).unwrap();
        for (stem, file) in &stems {
            let audio = deckhand_audio::decode_file(file).unwrap();
            let seconds =
                |a: &deckhand_audio::TrackAudio| a.frames() as f64 / f64::from(a.sample_rate);
            eprintln!(
                "{stem}: {} Hz, {:.3} s (original {:.3} s)",
                audio.sample_rate,
                seconds(&audio),
                seconds(&original)
            );
        }
        let recipe = recipes(&model).into_iter().find(|r| r.suffix == "Acapella").unwrap();
        let files: Vec<&Path> = recipe.keep.iter().map(|s| stems[s].as_path()).collect();
        let out = cache.join("Acapella.wav");
        mix(&files, &out).unwrap();
        eprintln!("made {}", out.display());
    }
}
