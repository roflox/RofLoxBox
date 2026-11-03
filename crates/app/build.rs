//! Windows: puts the app icon into the .exe, for Explorer, shortcuts and
//! the taskbar. Needs a resource compiler (Visual Studio's rc.exe, or
//! llvm-rc); without one the .exe builds without it.

use std::path::Path;

fn main() {
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
