#!/bin/sh
# Builds RofLoxBox.app for Apple Silicon Macs (macOS 11+) and zips it for
# sending: target/dist/RofLoxBox-macos.zip. With --universal it also runs on
# Intel Macs (needs `rustup target add x86_64-apple-darwin`).
# The app is signed ad hoc (no Apple Developer ID), so on another Mac it has
# to be opened once with right-click → Open.
set -eu
cd "$(dirname "$0")/.."
export MACOSX_DEPLOYMENT_TARGET=11.0
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/app/Cargo.toml | head -1)
TARGETS=aarch64-apple-darwin
if [ "${1:-}" = "--universal" ]; then
    TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
fi
BINARIES=""
for target in $TARGETS; do
    cargo build --release -p deckhand-app --target "$target"
    BINARIES="$BINARIES target/$target/release/deckhand-app"
done
APP=target/dist/RofLoxBox.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
# shellcheck disable=SC2086
lipo -create -output "$APP/Contents/MacOS/RofLoxBox" $BINARIES
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>RofLoxBox</string>
    <key>CFBundleDisplayName</key><string>RofLoxBox</string>
    <key>CFBundleIdentifier</key><string>app.rofloxbox</string>
    <key>CFBundleExecutable</key><string>RofLoxBox</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
codesign --force --deep --sign - "$APP"
rm -f target/dist/RofLoxBox-macos.zip
ditto -c -k --keepParent "$APP" target/dist/RofLoxBox-macos.zip
echo "target/dist/RofLoxBox-macos.zip"
