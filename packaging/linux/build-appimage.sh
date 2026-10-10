#!/usr/bin/env bash
# Builds an AppImage from an existing release binary. CI only (Linux, needs curl + FUSE-less extraction).
# Usage: packaging/linux/build-appimage.sh <version> [path/to/gitdragon] [out-dir]
set -euo pipefail

VERSION="${1:?version required}"
BIN="${2:-target/release/gitdragon}"
OUT="${3:-dist}"
HERE="$(cd "$(dirname "$0")" && pwd)"
APPDIR="$(mktemp -d)/GitDragon.AppDir"

install -Dm755 "$BIN" "$APPDIR/usr/bin/gitdragon"
install -Dm755 "$(dirname "$BIN")/gitdragon-mcp" "$APPDIR/usr/bin/gitdragon-mcp"
install -Dm644 "$HERE/gitdragon.desktop" "$APPDIR/usr/share/applications/gitdragon.desktop"
cp "$HERE/gitdragon.desktop" "$APPDIR/gitdragon.desktop"
# TODO(icon): AppImage requires packaging/linux/gitdragon.png (256x256); generate it from the logo SVG.
[ -f "$HERE/gitdragon.png" ] || { echo "missing packaging/linux/gitdragon.png (AppImage needs an icon)" >&2; exit 1; }
cp "$HERE/gitdragon.png" "$APPDIR/gitdragon.png"
ln -s usr/bin/gitdragon "$APPDIR/AppRun"

# TODO(supply chain): pin appimagetool to a release tag and verify its SHA-256 here before running it.
TOOL="$(mktemp -d)/appimagetool"
curl -fsSL -o "$TOOL" "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"
chmod +x "$TOOL"

mkdir -p "$OUT"
ARCH=x86_64 "$TOOL" --appimage-extract-and-run "$APPDIR" "$OUT/GitDragon-$VERSION-linux-x86_64.AppImage"
