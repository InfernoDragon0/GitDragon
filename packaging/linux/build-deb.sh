#!/usr/bin/env bash
# Builds a .deb from an existing release binary. CI only (needs dpkg-deb, i.e. a Debian/Ubuntu runner).
# Usage: packaging/linux/build-deb.sh <version> [path/to/gitdragon] [out-dir]
set -euo pipefail

VERSION="${1:?version required}"
BIN="${2:-target/release/gitdragon}"
OUT="${3:-dist}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(mktemp -d)"

install -Dm755 "$BIN" "$ROOT/usr/bin/gitdragon"
install -Dm644 "$HERE/gitdragon.desktop" "$ROOT/usr/share/applications/gitdragon.desktop"
# TODO(icon): install a 256x256 PNG to usr/share/icons/hicolor/256x256/apps/gitdragon.png once one exists.
[ -f "$HERE/gitdragon.png" ] && install -Dm644 "$HERE/gitdragon.png" "$ROOT/usr/share/icons/hicolor/256x256/apps/gitdragon.png"

mkdir -p "$ROOT/DEBIAN"
cat > "$ROOT/DEBIAN/control" <<CONTROL
Package: gitdragon
Version: $VERSION
Section: devel
Priority: optional
Architecture: amd64
Maintainer: InfernoDragon0 <noreply@users.noreply.github.com>
Depends: git (>= 1:2.35), libc6, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libvulkan1, libfontconfig1, libfreetype6, libasound2 | libasound2t64
Recommends: git-lfs
Homepage: https://github.com/InfernoDragon0/GitDragon
Description: Fast, native Git client built with Rust and GPUI
 Commit graph, staging, branches, merge and conflict resolution, pull requests.
CONTROL

mkdir -p "$OUT"
dpkg-deb --root-owner-group --build "$ROOT" "$OUT/gitdragon_${VERSION}_amd64.deb"
rm -rf "$ROOT"
# TODO(signing): sign the .deb / apt repo metadata with a GPG key when a repository is published.
