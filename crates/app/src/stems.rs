//! Separating tracks into stems with a model the user picks, and making new
//! tracks from some of them (an acapella, an instrumental, …).
//!
//! A model is a folder in `models/` with a `model.toml`: its name, the stems
//! it makes and the command that makes them, for example
//!
//! ```toml
//! name = "Demucs (htdemucs)"
//! stems = ["drums", "bass", "other", "vocals"]
//! command = ["demucs", "-n", "htdemucs", "-o", "{output}", "{input}"]
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

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
    dir: PathBuf,
}

pub fn models_dir() -> PathBuf {
    deckhand_library::data_dir().join("models")
}

fn cache_dir() -> PathBuf {
    deckhand_library::data_dir().join("stem-cache")
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
command = ["demucs", "-n", "htdemucs_ft", "-o", "{output}", "--filename", "{stem}.{ext}", "{input}"]
"#,
    },
    Preset {
        id: "demucs-htdemucs-6s",
        name: "Demucs htdemucs_6s (6 stems with guitar and piano)",
        install: "pipx install demucs && pipx inject demucs numpy soundfile",
        toml: r#"name = "Demucs htdemucs_6s"
install = "pipx install demucs && pipx inject demucs numpy soundfile"
stems = ["drums", "bass", "other", "vocals", "guitar", "piano"]
command = ["demucs", "-n", "htdemucs_6s", "-o", "{output}", "--filename", "{stem}.{ext}", "{input}"]
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

#[derive(Debug)]
pub enum Event {
    /// The job started; `percent` of the model's run is done.
    Progress {
        title: String,
        percent: Option<u32>,
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
        let _ = events.send(Event::Progress { title: title.clone(), percent });
    };
    progress(None);
    let stems = match separate(&job.model, &job.input, cache, running, &progress) {
        Ok(stems) => stems,
        Err(err) => {
            let _ = events.send(Event::Failed { title, error: format!("{err:#}") });
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
            },
        });
    }
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

    let fill = |arg: &String| {
        arg.replace("{input}", &wav.to_string_lossy())
            .replace("{output}", &output.to_string_lossy())
            .replace("{model}", &model.dir.to_string_lossy())
    };
    let args: Vec<String> = model.command.iter().map(fill).collect();
    let mut child = Command::new(&args[0])
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
    let found = find_stems(&output, &model.stems, &model.files)?;
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
