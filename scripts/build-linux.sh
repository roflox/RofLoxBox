#!/bin/sh
# Builds RofLoxBox on 64-bit Linux and packs it as
# target/dist/RofLoxBox-<version>-linux-x86_64.AppImage (when appimagetool
# is on PATH or APPIMAGETOOL points to it) and a .tar.gz holding the program,
# its .desktop file and icon.
# Needs the ALSA headers: `sudo apt install libasound2-dev pkg-config`.
# AppImages are not signed: Linux has no system-wide code signing; the
# release publishes SHA-256 sums instead.
set -eu
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/app/Cargo.toml | head -1)
ARCH=$(uname -m)
cargo build --release -p deckhand-app
DIST=$(pwd)/target/dist
APPDIR=$DIST/RofLoxBox.AppDir
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
    "$APPDIR/usr/share/icons/hicolor/256x256/apps" "$APPDIR/usr/share/icons/hicolor/512x512/apps"
cp target/release/deckhand-app "$APPDIR/usr/bin/rofloxbox"
cp packaging/linux/rofloxbox.desktop "$APPDIR/usr/share/applications/"
cp packaging/linux/rofloxbox.desktop "$APPDIR/"
cp packaging/icons/RofLoxBox-256.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/rofloxbox.png"
cp packaging/icons/RofLoxBox-512.png "$APPDIR/usr/share/icons/hicolor/512x512/apps/rofloxbox.png"
cp packaging/icons/RofLoxBox-256.png "$APPDIR/rofloxbox.png"
ln -sf usr/bin/rofloxbox "$APPDIR/AppRun"

TARBALL=$DIST/RofLoxBox-$VERSION-linux-$ARCH.tar.gz
tar -C "$APPDIR" -czf "$TARBALL" usr
echo "$TARBALL"
TOOL=${APPIMAGETOOL:-$(command -v appimagetool || true)}
if [ -n "$TOOL" ]; then
    IMAGE=$DIST/RofLoxBox-$VERSION-linux-$ARCH.AppImage
    ARCH=$ARCH "$TOOL" --no-appstream "$APPDIR" "$IMAGE"
    echo "$IMAGE"
else
    echo "appimagetool not found: no AppImage"
fi
