#!/usr/bin/env bash
# Package the built vibe-desktop binary for Linux or macOS.
# Usage: package-unix.sh <binary-path> <os: linux|macos> <version> <out-file>
set -euo pipefail

BIN="$1"
OS="$2"
VERSION="$3"
OUT="$4"

PACKAGING="$(cd "$(dirname "$0")/../packaging" && pwd)"

case "$OS" in
  linux)
    STAGE="$(mktemp -d)/vibe-desktop-${VERSION}-linux-x86_64"
    mkdir -p "$STAGE/bin" "$STAGE/share/applications" "$STAGE/share/icons/hicolor/512x512/apps"
    cp "$BIN" "$STAGE/bin/vibe-desktop"
    cp "$PACKAGING/vibe-desktop.desktop" "$STAGE/share/applications/"
    cp "$PACKAGING/icon.png" "$STAGE/share/icons/hicolor/512x512/apps/vibe-desktop.png"
    tar -C "$(dirname "$STAGE")" -czf "$OUT" "$(basename "$STAGE")"
    ;;
  macos)
    STAGE="$(mktemp -d)/Vibe Desktop.app/Contents"
    mkdir -p "$STAGE/MacOS" "$STAGE/Resources"
    cp "$BIN" "$STAGE/MacOS/vibe-desktop"
    sed "s/VIBE_DESKTOP_VERSION/${VERSION}/" "$PACKAGING/Info.plist" > "$STAGE/Info.plist"
    # Build the icns from icon.png (macOS-only tools — run in the macOS job).
    ICONSET="$(mktemp -d)/icon.iconset"
    for size in 16 32 128 256 512; do
      sips -z "$size" "$size" "$PACKAGING/icon.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
      dbl=$((size * 2))
      sips -z "$dbl" "$dbl" "$PACKAGING/icon.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "$ICONSET" -o "$STAGE/Resources/icon.icns"
    ditto -c -k --sequesterRsrc "$(dirname "$STAGE")" "$OUT"
    ;;
  *)
    echo "unknown os: $OS" >&2
    exit 1
    ;;
esac
echo "wrote $OUT"
