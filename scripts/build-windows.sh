#!/bin/sh
# Cross-compiles deckhand for 64-bit Windows (10/11) from macOS or Linux.
# Needs `cargo install cargo-xwin` and `rustup target add x86_64-pc-windows-msvc`.
# cargo-xwin downloads the Windows SDK and CRT on first use.
# The result is target/dist/RofLoxBox-windows-x64.zip (RofLoxBox.exe inside).
set -eu
cd "$(dirname "$0")/.."
TARGET=x86_64-pc-windows-msvc
eval "$(cargo xwin env --target "$TARGET")"
# Its empty RUSTFLAGS would override the per-target flags below.
unset RUSTFLAGS
# Link with the active toolchain's own rust-lld rather than whichever
# lld-link cargo-xwin found, which can belong to a broken toolchain.
HOST=$(rustc -vV | sed -n 's/^host: //p')
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="$(rustc --print sysroot)/lib/rustlib/$HOST/bin/rust-lld"
# A static C runtime so the .exe runs without the Visual C++ Redistributable.
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="$CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS -C target-feature=+crt-static"
cargo build --release -p deckhand-app --target "$TARGET" "$@"
mkdir -p target/dist
rm -f target/dist/RofLoxBox-windows-x64.zip target/dist/RofLoxBox.exe
cp "target/$TARGET/release/deckhand-app.exe" target/dist/RofLoxBox.exe
(cd target/dist && zip -q RofLoxBox-windows-x64.zip RofLoxBox.exe && rm RofLoxBox.exe)
echo "target/dist/RofLoxBox-windows-x64.zip"
