#!/bin/sh
# Parley installer for Linux.
#
#   curl -fsSL https://meetparley.app/install.sh | sh
#
# Downloads the latest Parley AppImage from GitHub releases, checks its
# SHA-256, installs it as ~/.local/bin/parley and adds Parley to your
# application menu. No root needed. Run it again to update.
#
#   curl -fsSL https://meetparley.app/install.sh | sh -s -- --uninstall
#                                       remove Parley (keeps your meetings and settings)
#   PARLEY_VERSION=0.5.0 sh install.sh  install a specific version
#   PARLEY_VARIANT=cpu sh install.sh    pick the build: vulkan (GPU) or cpu;
#                                       default is vulkan if a Vulkan driver is present
#   PARLEY_BIN_DIR=/opt/bin sh install.sh
#
# Source: https://github.com/Hankanman/Parley/blob/main/site/install.sh

set -eu

REPO="Hankanman/Parley"
APP_ID="io.github.hankanman.Parley"
BIN_DIR="${PARLEY_BIN_DIR:-$HOME/.local/bin}"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
DESKTOP_FILE="$DATA_HOME/applications/$APP_ID.desktop"
ICON_FILE="$DATA_HOME/icons/hicolor/512x512/apps/$APP_ID.png"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

fetch() { # fetch URL [OUTPUT]
    if command -v curl >/dev/null 2>&1; then
        if [ $# -eq 2 ]; then curl -fL --progress-bar -o "$2" "$1"; else curl -fsSL "$1"; fi
    elif command -v wget >/dev/null 2>&1; then
        if [ $# -eq 2 ]; then wget -q --show-progress -O "$2" "$1"; else wget -qO- "$1"; fi
    else
        die "curl or wget is required"
    fi
}

refresh_menus() {
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$DATA_HOME/applications" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$DATA_HOME/icons/hicolor" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN_DIR/parley" "$DESKTOP_FILE" "$ICON_FILE"
    refresh_menus
    say "Parley removed. Your meetings, models and settings are still in $DATA_HOME/$APP_ID"
    exit 0
fi

[ "$(uname -s)" = "Linux" ] || die "Parley is Linux-only."
[ "$(uname -m)" = "x86_64" ] || die "Parley is only built for x86_64 (this machine is $(uname -m))."

# The AppImages are built on Ubuntu 24.04: they need glibc 2.39 or newer.
GLIBC="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
if [ -n "$GLIBC" ]; then
    major="${GLIBC%%.*}"; minor="${GLIBC#*.}"; minor="${minor%%.*}"
    if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 39 ]; }; then
        die "Parley needs glibc 2.39 or newer (Ubuntu 24.04, Debian 13, Fedora 40 or later); this system has $GLIBC."
    fi
fi

if [ -n "${PARLEY_VERSION:-}" ]; then
    API="https://api.github.com/repos/$REPO/releases/tags/v${PARLEY_VERSION#v}"
else
    API="https://api.github.com/repos/$REPO/releases/latest"
fi
say "Looking up the release"
RELEASE="$(fetch "$API")" || die "couldn't reach GitHub ($API)"
asset_url() { # asset_url <file-name regex>
    printf '%s' "$RELEASE" | grep -o "\"browser_download_url\": *\"[^\"]*/$1\"" | head -n1 | sed 's/.*"\(https[^"]*\)"/\1/'
}
CPU_URL="$(asset_url 'Parley-[0-9][^/"]*[0-9]-x86_64\.AppImage')"
VULKAN_URL="$(asset_url 'Parley-[0-9][^/"]*-vulkan-x86_64\.AppImage')"

# The Vulkan build runs Whisper on any GPU with a Vulkan driver (NVIDIA, AMD,
# Intel); the CPU build works everywhere. PARLEY_VARIANT=cpu|vulkan overrides.
# True if this machine has an NVIDIA, AMD or Intel GPU with a Vulkan driver
# for it. Mesa installs driver manifests for every vendor (and a software
# renderer), so a manifest on its own proves nothing; match it to the GPU.
has_vulkan() {
    for vendor_file in /sys/class/drm/card*/device/vendor; do
        [ -r "$vendor_file" ] || continue
        case "$(cat "$vendor_file")" in
            0x10de) pattern='nvidia_icd* nouveau_icd*' ;;
            0x1002) pattern='radeon_icd* amd_icd*' ;;
            0x8086) pattern='intel_icd*' ;;
            *) continue ;;
        esac
        for dir in /usr/share/vulkan/icd.d /etc/vulkan/icd.d; do
            for p in $pattern; do
                for icd in "$dir"/$p; do
                    [ -e "$icd" ] && return 0
                done
            done
        done
    done
    return 1
}
case "${PARLEY_VARIANT:-auto}" in
    cpu) URL="$CPU_URL" ;;
    vulkan) URL="$VULKAN_URL" ;;
    auto) if [ -n "$VULKAN_URL" ] && has_vulkan; then URL="$VULKAN_URL"; else URL="${CPU_URL:-$VULKAN_URL}"; fi ;;
    *) die "PARLEY_VARIANT must be cpu or vulkan" ;;
esac
[ -n "$URL" ] || die "no matching Parley AppImage found in that release. See https://github.com/$REPO/releases"
NAME="${URL##*/}"
VERSION="$(printf '%s' "$NAME" | sed 's/^Parley-\([0-9][0-9.]*[0-9]\).*/\1/')"
case "$NAME" in *-vulkan-*) BUILD="Vulkan (GPU)" ;; *) BUILD="CPU" ;; esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM
say "Downloading Parley $VERSION, $BUILD build"
fetch "$URL" "$TMP/$NAME"

if fetch "$URL.sha256" "$TMP/$NAME.sha256" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$TMP" && sha256sum -c --status "$NAME.sha256") || die "checksum mismatch: the download is corrupt or was tampered with"
        say "Checksum verified"
    else
        warn "sha256sum not found; skipping the checksum check"
    fi
else
    warn "this release has no checksum file; skipping the checksum check"
fi
chmod +x "$TMP/$NAME"

# Menu entry and icon, taken from the AppImage itself (extracting needs no FUSE).
(cd "$TMP" && "./$NAME" --appimage-extract "usr/share/icons/hicolor/512x512/apps/$APP_ID.png" >/dev/null 2>&1 \
    && "./$NAME" --appimage-extract "usr/share/applications/$APP_ID.desktop" >/dev/null 2>&1) \
    || warn "couldn't read the menu entry from the AppImage; skipping it"

mkdir -p "$BIN_DIR"
mv -f "$TMP/$NAME" "$BIN_DIR/parley"

if [ -f "$TMP/squashfs-root/usr/share/applications/$APP_ID.desktop" ]; then
    mkdir -p "$(dirname "$DESKTOP_FILE")" "$(dirname "$ICON_FILE")"
    cp "$TMP/squashfs-root/usr/share/icons/hicolor/512x512/apps/$APP_ID.png" "$ICON_FILE"
    sed "s|^Exec=.*|Exec=$BIN_DIR/parley|" "$TMP/squashfs-root/usr/share/applications/$APP_ID.desktop" > "$DESKTOP_FILE"
    refresh_menus
fi

say "Installed Parley $VERSION ($BUILD build) to $BIN_DIR/parley"

# The AppImage runtime has libfuse built in but mounts itself with the
# system's fusermount (FUSE 3 or 2).
if ! command -v fusermount3 >/dev/null 2>&1 && ! command -v fusermount >/dev/null 2>&1; then
    warn "FUSE (fusermount) wasn't found, and AppImages need it to start. Install it with:"
    if command -v apt-get >/dev/null 2>&1; then echo "    sudo apt install fuse3" >&2
    elif command -v dnf >/dev/null 2>&1; then echo "    sudo dnf install fuse3" >&2
    elif command -v pacman >/dev/null 2>&1; then echo "    sudo pacman -S fuse3" >&2
    elif command -v zypper >/dev/null 2>&1; then echo "    sudo zypper install fuse3" >&2
    else echo "    your distribution's fuse3 package" >&2; fi
    echo "    (or run it without FUSE: $BIN_DIR/parley --appimage-extract-and-run)" >&2
fi

case ":$PATH:" in
    *":$BIN_DIR:"*) say "Start it from your application menu, or run: parley" ;;
    *) say "Start it from your application menu, or run: $BIN_DIR/parley"
       warn "$BIN_DIR isn't on your PATH, so plain 'parley' won't work in a terminal yet." ;;
esac
