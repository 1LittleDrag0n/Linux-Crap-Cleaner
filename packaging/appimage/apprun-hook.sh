#! /usr/bin/env bash
# Start-up environment of the AppImage. build-appimage.sh puts this in place
# of the hook linuxdeploy-plugin-gtk writes, which sets GTK_THEME and forces
# the X11 backend: GTK_THEME replaces libadwaita's own style (the app looks
# different and libadwaita warns about it), and X11 isn't needed for GTK 4.
#
# Here the app runs on Wayland when the session has it, libadwaita follows the
# system's light/dark setting itself, and fonts, icons and cursor come from
# the system's GSettings through the bundled dconf module, as for an installed
# app.
#
# Every variable changed here is listed in LCC_APPIMAGE_ENV, with the system's
# value saved as LCC_HOST_<name>, so the programs the app starts (flatpak,
# apt, systemctl, ...) get the system's values back (lcc_core::host).

export APPDIR="${APPDIR:-"$(dirname "$(realpath "$0")")"}" # Also when extracted

LCC_APPIMAGE_ENV=""
lcc_set() {
    local name="$1" value="$2"
    if [ -n "${!name+set}" ]; then
        export "LCC_HOST_$name=${!name}"
    fi
    export "$name=$value"
    LCC_APPIMAGE_ENV="$LCC_APPIMAGE_ENV:$name"
}

lcc_set GTK_DATA_PREFIX "$APPDIR"
lcc_set GTK_EXE_PREFIX "$APPDIR/usr"
lcc_set GTK_PATH "$APPDIR/usr/lib/gtk-4.0"
lcc_set XDG_DATA_DIRS "$APPDIR/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
lcc_set GSETTINGS_SCHEMA_DIR "$APPDIR/usr/share/glib-2.0/schemas"
lcc_set GIO_MODULE_DIR "$APPDIR/usr/lib/gio/modules"
lcc_set GDK_PIXBUF_MODULE_FILE "$APPDIR/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
export LCC_APPIMAGE_ENV

unset -f lcc_set
