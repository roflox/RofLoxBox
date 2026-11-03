#!/bin/sh
# Builds RofLoxBox.app for Apple Silicon Macs (macOS 11+) and packs it into
# target/dist/RofLoxBox-<version>-macos.dmg (drag the app to Applications)
# and a .zip. With --universal it also runs on Intel Macs (needs
# `rustup target add x86_64-apple-darwin`).
#
# Signing, when these are set (otherwise ad hoc: another Mac then has to
# open it once with right-click → Open):
#   MACOS_SIGN_IDENTITY  "Developer ID Application: Name (TEAMID)", a
#                        certificate in the keychain (Apple Developer Program)
#   MACOS_NOTARY_PROFILE a notarytool keychain profile, made once with
#                        `xcrun notarytool store-credentials`; the .dmg is
#                        then notarized by Apple and stapled.
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
DIST=target/dist
APP=$DIST/RofLoxBox.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# shellcheck disable=SC2086
lipo -create -output "$APP/Contents/MacOS/RofLoxBox" $BINARIES
cp packaging/icons/RofLoxBox.icns "$APP/Contents/Resources/"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>RofLoxBox</string>
    <key>CFBundleDisplayName</key><string>RofLoxBox</string>
    <key>CFBundleIdentifier</key><string>app.rofloxbox</string>
    <key>CFBundleExecutable</key><string>RofLoxBox</string>
    <key>CFBundleIconFile</key><string>RofLoxBox</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
    # Hardened runtime and a secure timestamp: what notarization asks for.
    codesign --force --options runtime --timestamp --sign "$MACOS_SIGN_IDENTITY" "$APP"
else
    echo "MACOS_SIGN_IDENTITY not set: signing ad hoc (not notarizable)"
    codesign --force --sign - "$APP"
fi
codesign --verify --strict "$APP"

# The disk image: the app and a link to Applications to drag it onto.
DMG=$DIST/RofLoxBox-$VERSION-macos.dmg
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "RofLoxBox $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$DMG" >/dev/null
if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
    codesign --force --timestamp --sign "$MACOS_SIGN_IDENTITY" "$DMG"
    if [ -n "${MACOS_NOTARY_PROFILE:-}" ]; then
        xcrun notarytool submit "$DMG" --keychain-profile "$MACOS_NOTARY_PROFILE" --wait
        xcrun stapler staple "$DMG"
        xcrun stapler staple "$APP"
    else
        echo "MACOS_NOTARY_PROFILE not set: signed, but not notarized"
    fi
fi
rm -f "$DIST/RofLoxBox-macos.zip"
ditto -c -k --keepParent "$APP" "$DIST/RofLoxBox-$VERSION-macos.zip"
echo "$DMG"
