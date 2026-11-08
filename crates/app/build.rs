//! The version, 0.<number of commits>.0 like scripts/version.sh (with
//! uncommitted changes: 0.<commits + 1>.0-dev), as `ROFLOXBOX_VERSION`. Windows: puts the app icon into the .exe, for
//! Explorer, shortcuts and the taskbar. Needs a resource compiler (Visual
//! Studio's rc.exe, or llvm-rc); without one the .exe builds without it.

use std::path::Path;
use std::process::Command;

fn main() {
    version();
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
