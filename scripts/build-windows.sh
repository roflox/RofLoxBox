#!/bin/sh
# Cross-compiles RofLoxBox for 64-bit Windows (10/11) from macOS or Linux.
# Needs `cargo install cargo-xwin` and `rustup target add x86_64-pc-windows-msvc`.
# cargo-xwin downloads the Windows SDK and CRT on first use.
# Makes target/dist/RofLoxBox-<version>-windows-x64.zip (RofLoxBox.exe
# inside) and, with NSIS (`brew install makensis`), the installer
# target/dist/RofLoxBox-<version>-windows-x64-setup.exe.
#
# Signing, when these are set (needs `brew install osslsigncode`):
#   WINDOWS_SIGN_PFX       a code-signing certificate as a .pfx file
#   WINDOWS_SIGN_PASSWORD  its password
set -eu
cd "$(dirname "$0")/.."
TARGET=x86_64-pc-windows-msvc
VERSION=$(scripts/version.sh)
eval "$(cargo xwin env --target "$TARGET")"
# Its empty RUSTFLAGS would override the per-target flags below.
unset RUSTFLAGS
# Link with the active toolchain's own rust-lld rather than whichever
# lld-link cargo-xwin found, which can belong to a broken toolchain.
HOST=$(rustc -vV | sed -n 's/^host: //p')
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="$(rustc --print sysroot)/lib/rustlib/$HOST/bin/rust-lld"
# A static C runtime so the .exe runs without the Visual C++ Redistributable.
# The C runtime's debug symbols are not shipped; skip the linker's note
# about each of them (LNK4099).
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="$CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS -C target-feature=+crt-static -C link-arg=/ignore:4099"
cargo build --release -p deckhand-app --target "$TARGET" "$@"

DIST=$(pwd)/target/dist
mkdir -p "$DIST"
EXE=$DIST/RofLoxBox.exe
cp "target/$TARGET/release/deckhand-app.exe" "$EXE"

sign() {
    if [ -z "${WINDOWS_SIGN_PFX:-}" ]; then
        return
    fi
    osslsigncode sign -pkcs12 "$WINDOWS_SIGN_PFX" -pass "${WINDOWS_SIGN_PASSWORD:-}" \
        -n RofLoxBox -h sha256 -t http://timestamp.digicert.com -in "$1" -out "$1.signed"
    mv "$1.signed" "$1"
}
[ -n "${WINDOWS_SIGN_PFX:-}" ] || echo "WINDOWS_SIGN_PFX not set: not signing"
sign "$EXE"

ZIP=$DIST/RofLoxBox-$VERSION-windows-x64.zip
rm -f "$ZIP" "$DIST/RofLoxBox-windows-x64.zip"
(cd "$DIST" && zip -q "$(basename "$ZIP")" RofLoxBox.exe)
echo "$ZIP"
if command -v makensis >/dev/null; then
    SETUP=$DIST/RofLoxBox-$VERSION-windows-x64-setup.exe
    makensis -V2 -DVERSION="$VERSION" -DEXE="$EXE" -DOUT="$SETUP" packaging/windows/RofLoxBox.nsi
    sign "$SETUP"
    echo "$SETUP"
else
    echo "makensis not found: no installer (brew install makensis)"
fi
rm -f "$EXE"
