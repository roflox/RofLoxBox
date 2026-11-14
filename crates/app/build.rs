//! The version, 0.<number of commits>.0 like scripts/version.sh (with
//! uncommitted changes: 0.<commits + 1>.0-dev), as `ROFLOXBOX_VERSION`. Windows: puts the app icon into the .exe, for
//! Explorer, shortcuts and the taskbar. Needs a resource compiler (Visual
//! Studio's rc.exe, or llvm-rc); without one the .exe builds without it.

use std::path::Path;
use std::process::Command;

fn main() {
    version();
    check_tours();
    let ico = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/icons/RofLoxBox.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = ico.canonicalize().expect("packaging/icons/RofLoxBox.ico");
    let rc = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("icon.rc");
    let path = ico.display().to_string().replace('\\', "/");
    std::fs::write(&rc, format!("1 ICON \"{path}\"\n")).expect("writing icon.rc");
    if let Err(err) = embed_resource::compile(&rc, embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=no icon in the .exe: {err}");
    }
}

/// A new commit moves the branch's ref (or packed-refs), so the count is
/// worked out again then; a changed source or staging (the index) can make
/// the build -dev or not.
fn version() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git = root.join(".git");
    println!("cargo:rerun-if-changed={}", git.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git.join("packed-refs").display());
    println!("cargo:rerun-if-changed={}", git.join("index").display());
    for watched in ["crates", "scripts", "packaging", "Cargo.toml", "Cargo.lock", "README.md"] {
        println!("cargo:rerun-if-changed={}", root.join(watched).display());
    }
    if let Ok(head) = std::fs::read_to_string(git.join("HEAD"))
        && let Some(branch) = head.trim().strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed={}", git.join(branch).display());
    }
    let count = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    // Changes to tracked files not committed yet: the version to come.
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());
    let version = match count.and_then(|c| c.parse::<u64>().ok()) {
        Some(count) if dirty => format!("0.{}.0-dev", count + 1),
        Some(count) => format!("0.{count}.0"),
        None => env!("CARGO_PKG_VERSION").to_owned(),
    };
    println!("cargo:rustc-env=ROFLOXBOX_VERSION={version}");
}

/// Fails the build when a guided tour (`tours/*.toml`) would not work: a
/// step pointing at a target no widget carries (`.tour(Target::…)`), an
/// unknown condition or Preferences page, a tour not shipped (missing from
/// `tour.rs`'s list), no steps, or two tours with one id.
fn check_tours() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tours = crate_dir.join("tours");
    let src = crate_dir.join("src");
    println!("cargo:rerun-if-changed={}", tours.display());
    println!("cargo:rerun-if-changed={}", src.display());
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let tour_rs = read(&src.join("tour.rs"));
    let prefs_rs = read(&src.join("prefs_view.rs"));
    let targets = enum_variants(&tour_rs, "pub enum Target");
    let conditions = enum_variants(&tour_rs, "pub enum Condition");
    let pages = enum_variants(&prefs_rs, "pub enum PrefsTab");
    // Targets some widget carries: named outside tour.rs; the Preferences
    // tabs all through `Target::prefs_tab`.
    let mut placed = std::collections::HashSet::new();
    let mut files = vec![src.clone()];
    while let Some(path) = files.pop() {
        if path.is_dir() {
            files.extend(std::fs::read_dir(&path).into_iter().flatten().flatten().map(|e| e.path()));
        } else if path.extension().is_some_and(|x| x == "rs") && path.file_name() != Some("tour.rs".as_ref()) {
            let text = read(&path);
            let mut rest = text.as_str();
            while let Some(at) = rest.find("Target::") {
                let before = rest[..at].chars().next_back();
                let name: String = rest[at + 8..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                if before.is_none_or(|c| !(c.is_alphanumeric() || c == '_')) {
                    if name == "prefs_tab" {
                        placed.extend(pages.iter().map(|p| format!("Prefs{p}")));
                    } else {
                        placed.insert(name);
                    }
                }
                rest = &rest[at + 8..];
            }
        }
    }
    let mut problems = Vec::new();
    let mut ids = std::collections::HashMap::new();
    let mut names: Vec<_> = std::fs::read_dir(&tours)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    names.sort();
    for path in names {
        let file = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let mut problem = |what: String| problems.push(format!("tours/{file}: {what}"));
        if !tour_rs.contains(&format!("\"{file}\"")) {
            problem("not in tour.rs's BUILT_IN list, so it would not ship".into());
        }
        let tour: toml::Table = match read(&path).parse() {
            Ok(t) => t,
            Err(e) => {
                problem(e.to_string());
                continue;
            }
        };
        match tour.get("id").and_then(|v| v.as_str()) {
            Some(id) => {
                if let Some(other) = ids.insert(id.to_owned(), file.clone()) {
                    problem(format!("id \"{id}\" is also {other}'s"));
                }
            }
            None => problem("no id".into()),
        }
        if tour.get("name").and_then(|v| v.as_str()).is_none() {
            problem("no name".into());
        }
        let check_conditions = |list: Option<&toml::Value>, key: &str, problem: &mut dyn FnMut(String)| {
            for condition in list.and_then(|v| v.as_array()).into_iter().flatten() {
                let (name, page) = match condition {
                    toml::Value::String(name) => (name.clone(), None),
                    toml::Value::Table(t) => match t.iter().next() {
                        Some((name, page)) => (name.clone(), page.as_str().map(str::to_owned)),
                        None => (String::new(), None),
                    },
                    _ => (String::new(), None),
                };
                if !conditions.contains(&pascal(&name)) {
                    problem(format!("{key}: no condition \"{name}\""));
                }
                if let Some(page) = page
                    && !pages.contains(&pascal(&page))
                {
                    problem(format!("{key}: no Preferences page \"{page}\""));
                }
            }
        };
        check_conditions(tour.get("start-when"), "start-when", &mut problem);
        let steps = tour.get("step").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if steps.is_empty() {
            problem("no [[step]]".into());
        }
        for (i, step) in steps.iter().enumerate() {
            let n = i + 1;
            let target = step.get("target").and_then(|v| v.as_str()).unwrap_or_default();
            let variant = pascal(target);
            if !targets.contains(&variant) {
                problem(format!("step {n}: no target \"{target}\""));
            } else if !placed.contains(&variant) {
                problem(format!("step {n}: no widget carries the target \"{target}\" (.tour(Target::{variant}))"));
            }
            check_conditions(step.get("when"), &format!("step {n} when"), &mut problem);
            check_conditions(step.get("wait"), &format!("step {n} wait"), &mut problem);
            if let Some(page) = step.get("action").and_then(|a| a.get("open-prefs")).and_then(|v| v.as_str())
                && !pages.contains(&pascal(page))
            {
                problem(format!("step {n}: no Preferences page \"{page}\" to open"));
            }
        }
    }
    if !problems.is_empty() {
        panic!("guided tours that would not work:\n  {}", problems.join("\n  "));
    }
}

/// The variants of the enum starting `header` in `source`.
fn enum_variants(source: &str, header: &str) -> Vec<String> {
    let start = source.find(header).unwrap_or_else(|| panic!("no `{header}`"));
    let body = &source[start..];
    let body = &body[body.find('{').expect("enum body") + 1..body.find("\n}").expect("enum end")];
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with('#'))
        .map(|l| l.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>())
        .filter(|name| !name.is_empty())
        .collect()
}

/// "prefs-audio" → "PrefsAudio".
fn pascal(kebab: &str) -> String {
    kebab
        .split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map(|c| c.to_uppercase().chain(chars).collect::<String>()).unwrap_or_default()
        })
        .collect()
}
