#!/usr/bin/env bash
# Builds Linux Crap Cleaner as an AppImage.
#
#   packaging/appimage/build-appimage.sh            build dist/Linux_Crap_Cleaner-<version>-<arch>.AppImage
#   packaging/appimage/build-appimage.sh --appdir   only assemble the AppDir (no downloads)
#
# An AppImage only runs on systems at least as new as the one it was built
# on (glibc). For one that also runs on Ubuntu 24.04, Debian 13, Fedora 40
# and newer, build inside the Ubuntu 24.04 container instead:
#
#   packaging/appimage/build-in-container.sh
#
# Downloads linuxdeploy, its GTK plugin and its AppImage plugin into
# target/appimage-tools on first use.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
APP_ID=io.github.LinuxCrapCleaner
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
APPDIR="$TARGET/AppDir"
TOOLS="$TARGET/appimage-tools"
DIST="$ROOT/dist"
ARCH="$(uname -m)"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n1)"

echo "==> Building release binaries (version $VERSION)"
cargo build --release --locked -p lcc-gui -p lcc-helper --manifest-path "$ROOT/Cargo.toml"

echo "==> Assembling $APPDIR"
rm -rf "$APPDIR"
install -Dm755 "$TARGET/release/linux-crap-cleaner" "$APPDIR/usr/bin/linux-crap-cleaner"
install -Dm755 "$TARGET/release/lcc-helper" "$APPDIR/usr/bin/lcc-helper"
install -Dm644 "$ROOT/data/$APP_ID.desktop" "$APPDIR/usr/share/applications/$APP_ID.desktop"
install -Dm644 "$ROOT/data/$APP_ID.metainfo.xml" "$APPDIR/usr/share/metainfo/$APP_ID.metainfo.xml"
install -Dm644 "$ROOT/data/icons/hicolor/scalable/apps/$APP_ID.svg" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"

if [[ "${1:-}" == "--appdir" ]]; then
    echo "==> AppDir ready (skipped packaging)"
    exit 0
fi

echo "==> Getting linuxdeploy"
mkdir -p "$TOOLS"
fetch() {
    local name="$1" url="$2"
    if [[ ! -x "$TOOLS/$name" ]]; then
        curl -fL --retry 3 -o "$TOOLS/$name.part" "$url"
        mv "$TOOLS/$name.part" "$TOOLS/$name"
        chmod +x "$TOOLS/$name"
    fi
}
fetch "linuxdeploy-$ARCH.AppImage" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$ARCH.AppImage"
fetch "linuxdeploy-plugin-appimage-$ARCH.AppImage" \
    "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-$ARCH.AppImage"
fetch "linuxdeploy-plugin-gtk.sh" \
    "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh"

echo "==> Packaging"
mkdir -p "$DIST"
export PATH="$TOOLS:$PATH"
# Run the tools without FUSE (needed in containers).
export APPIMAGE_EXTRACT_AND_RUN=1
# Bundle GTK 4 (and libadwaita, which the app links to).
export DEPLOY_GTK_VERSION=4
export LINUXDEPLOY_OUTPUT_VERSION="$VERSION"
export LDAI_OUTPUT="$DIST/Linux_Crap_Cleaner-$VERSION-$ARCH.AppImage"

"$TOOLS/linuxdeploy-$ARCH.AppImage" \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/linux-crap-cleaner" \
    --executable "$APPDIR/usr/bin/lcc-helper" \
    --desktop-file "$APPDIR/usr/share/applications/$APP_ID.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg" \
    --plugin gtk \
    --output appimage

echo "==> Built $LDAI_OUTPUT"
