#!/usr/bin/env bash
# Builds GitDragon.app and a .dmg from an existing release binary. CI only (needs macOS: hdiutil, codesign).
# Usage: packaging/macos/build-dmg.sh <version> [path/to/gitdragon] [out-dir]
set -euo pipefail

VERSION="${1:?version required}"
BIN="${2:-target/release/gitdragon}"
OUT="${3:-dist}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ARCH="$(uname -m)"   # arm64 or x86_64
APP="$OUT/GitDragon.app"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/gitdragon"
cp "$(dirname "$BIN")/gitdragon-mcp" "$APP/Contents/MacOS/gitdragon-mcp"   # next to the app, where Settings > AI finds it
sed "s/@VERSION@/$VERSION/g" "$HERE/Info.plist" > "$APP/Contents/Info.plist"
# TODO(icon): generate gitdragon.icns from the logo (iconutil) and copy it to Contents/Resources.
[ -f "$HERE/gitdragon.icns" ] && cp "$HERE/gitdragon.icns" "$APP/Contents/Resources/"

# --- Signing + notarisation hook (disabled until an Apple Developer ID exists; see docs/release.md).
# Without it the app is ad-hoc signed, which Apple Silicon requires to run at all; Gatekeeper still warns.
if [ -n "${APPLE_SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGN_IDENTITY" "$APP"
else
  codesign --force --sign - "$APP"
fi

DMG="$OUT/GitDragon-$VERSION-macos-$ARCH.dmg"
rm -f "$DMG"
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "GitDragon" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"

if [ -n "${APPLE_SIGN_IDENTITY:-}" ] && [ -n "${APPLE_NOTARY_PROFILE:-}" ]; then
  codesign --force --timestamp --sign "$APPLE_SIGN_IDENTITY" "$DMG"
  xcrun notarytool submit "$DMG" --keychain-profile "$APPLE_NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG"
fi
echo "$DMG"
