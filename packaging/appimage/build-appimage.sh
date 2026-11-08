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
        # GitHub's download servers sometimes answer 504 for a minute or two.
        curl -fL --retry 8 --retry-delay 15 --retry-all-errors \
            -o "$TOOLS/$name.part" "$url"
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

echo "==> Bundling GTK"
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
    --plugin gtk

# The GTK plugin's start-up hook forces a GTK theme and X11; ours doesn't.
# AppRun sources the hook by this file name.
echo "==> Installing the start-up hook"
install -m644 "$ROOT/packaging/appimage/apprun-hook.sh" \
    "$APPDIR/apprun-hooks/linuxdeploy-plugin-gtk.sh"

# GSettings through dconf, so GTK uses the system's font, icon theme and
# cursor like an installed app does.
echo "==> Bundling the dconf GSettings backend and desktop schemas"
GIO_MODULES="$(pkg-config --variable=giomoduledir gio-2.0)"
if [[ ! -f "$GIO_MODULES/libdconfsettings.so" ]]; then
    echo "error: $GIO_MODULES/libdconfsettings.so is missing (install dconf-gsettings-backend)" >&2
    exit 1
fi
install -Dm755 "$GIO_MODULES/libdconfsettings.so" "$APPDIR/usr/lib/gio/modules/libdconfsettings.so"
SCHEMAS=/usr/share/glib-2.0/schemas
if ! compgen -G "$SCHEMAS/org.gnome.desktop.interface.gschema.xml" >/dev/null; then
    echo "error: GNOME desktop schemas are missing (install gsettings-desktop-schemas)" >&2
    exit 1
fi
install -m644 "$SCHEMAS"/org.gnome.desktop.*.gschema.xml "$SCHEMAS"/org.gnome.desktop.enums.xml \
    "$APPDIR/usr/share/glib-2.0/schemas/"
glib-compile-schemas "$APPDIR/usr/share/glib-2.0/schemas"

echo "==> Packaging"
"$TOOLS/linuxdeploy-plugin-appimage-$ARCH.AppImage" --appdir "$APPDIR"

echo "==> Built $LDAI_OUTPUT"
