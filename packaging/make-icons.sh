#!/bin/sh
# Renders packaging/icon.svg into the icon files the builds use (kept in the
# repository, so building needs no image tools): icons/RofLoxBox.icns
# (macOS), icons/RofLoxBox.ico (Windows), icons/RofLoxBox-{256,512}.png
# (Linux and the window icon). Needs rsvg-convert and ImageMagick
# (`brew install librsvg imagemagick`); the .icns also needs macOS.
set -eu
cd "$(dirname "$0")"
mkdir -p icons
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
png() { rsvg-convert -w "$1" -h "$1" icon.svg -o "$2"; }
png 256 icons/RofLoxBox-256.png
png 512 icons/RofLoxBox-512.png
for size in 16 24 32 48 64 128 256; do
    png "$size" "$WORK/ico-$size.png"
done
magick "$WORK"/ico-16.png "$WORK"/ico-24.png "$WORK"/ico-32.png "$WORK"/ico-48.png \
    "$WORK"/ico-64.png "$WORK"/ico-128.png "$WORK"/ico-256.png icons/RofLoxBox.ico
if command -v iconutil >/dev/null; then
    SET="$WORK/RofLoxBox.iconset"
    mkdir -p "$SET"
    for size in 16 32 128 256 512; do
        png "$size" "$SET/icon_${size}x${size}.png"
        png $((size * 2)) "$SET/icon_${size}x${size}@2x.png"
    done
    iconutil -c icns "$SET" -o icons/RofLoxBox.icns
fi
ls -l icons
