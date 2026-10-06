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
    # AppImage: FHS AppDir (usr/bin + usr/share) with a root AppRun,
    # .desktop and icon. APPIMAGETOOL must point at an appimagetool
    # binary — release.yml downloads appimagetool-x86_64.AppImage.
    APPDIR="$(mktemp -d)/AppDir"
    mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/512x512/apps"
    cp "$BIN" "$APPDIR/usr/bin/vibe-desktop"
    cp "$PACKAGING/vibe-desktop.desktop" "$APPDIR/vibe-desktop.desktop"
    cp "$PACKAGING/vibe-desktop.desktop" "$APPDIR/usr/share/applications/"
    cp "$PACKAGING/icon.png" "$APPDIR/vibe-desktop.png"
    cp "$PACKAGING/icon.png" "$APPDIR/usr/share/icons/hicolor/512x512/apps/vibe-desktop.png"
    ln -s usr/bin/vibe-desktop "$APPDIR/AppRun"
    "${APPIMAGETOOL:?set APPIMAGETOOL to an appimagetool binary}" "$APPDIR" "$OUT"
    ;;
  macos)
    STAGE="$(mktemp -d)/Vibe Desktop.app/Contents"
    mkdir -p "$STAGE/MacOS" "$STAGE/Resources"
    cp "$BIN" "$STAGE/MacOS/vibe-desktop"
    # CFBundle*Version must be numeric — strip a leading v and any
    # non-numeric suffix from the release version.
    BVER="${VERSION#v}"
    BVER="${BVER%%[^0-9.]*}"
    [ -n "$BVER" ] || BVER="0.0.0"
    sed "s/VIBE_DESKTOP_VERSION/${BVER}/" "$PACKAGING/Info.plist" > "$STAGE/Info.plist"
    # Build the icns from icon.png (macOS-only tools — run in the macOS job).
    ICONSET="$(mktemp -d)/icon.iconset"
    mkdir -p "$ICONSET"
    for size in 16 32 128 256 512; do
      sips -z "$size" "$size" "$PACKAGING/icon.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
      dbl=$((size * 2))
      sips -z "$dbl" "$dbl" "$PACKAGING/icon.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "$ICONSET" -o "$STAGE/Resources/icon.icns"
    # The linker-adhoc signature on the raw binary does not cover the
    # bundle's resources — codesign --verify fails with "code has no
    # resources but signature indicates they must be present" and macOS
    # reports the app as damaged. Re-sign the whole bundle adhoc so the
    # signature is coherent (still bypassable via Settings → Open Anyway;
    # real Developer ID + notarization pending certs — see release.yml).
    codesign --force --deep --sign - "$(dirname "$STAGE")"
    codesign --verify --deep --strict "$(dirname "$STAGE")"
    # --keepParent keeps "Vibe Desktop.app" at the zip root — without it
    # ditto flattens the bundle and users get a bare Contents/ folder.
    ditto -c -k --sequesterRsrc --keepParent "$(dirname "$STAGE")" "$OUT"
    ;;
  *)
    echo "unknown os: $OS" >&2
    exit 1
    ;;
esac
echo "wrote $OUT"
