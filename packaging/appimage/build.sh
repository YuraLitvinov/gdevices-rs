#!/bin/bash
# Runs on Ubuntu 24.04 (GTK 4.14, glibc 2.39), as root in a container or with sudo on a runner,
# and packages the application as an AppImage in $OUT.
#   env: SRC (checkout, default /src)  OUT (default $SRC/dist)
set -euo pipefail
SRC=${SRC:-/src}; OUT=${OUT:-$SRC/dist}
export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p "$OUT"

SUDO=""; [ "$(id -u)" = 0 ] || SUDO=sudo
echo "::group::system packages"
$SUDO apt-get update -qq
DEBIAN_FRONTEND=noninteractive $SUDO apt-get install -y --no-install-recommends \
    build-essential pkg-config python3 git ca-certificates curl wget file patchelf make \
    gettext desktop-file-utils libgtk-4-dev libudev-dev libkmod-dev libdisplay-info-dev libv4l-dev \
    libwayland-dev libxcb-randr0-dev clang libclang-dev
echo "::endgroup::"

echo "::group::Rust"
command -v cargo >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
echo "::endgroup::"

echo "::group::application and AppDir"
cd "$SRC"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$SRC/target-appimage}"
APPDIR=$OUT/AppDir; rm -rf "$APPDIR"
make install PREFIX=/usr DESTDIR="$APPDIR" TARGET_DIR="$CARGO_TARGET_DIR/release"
echo "::endgroup::"

echo "::group::AppImage"
tools=$(mktemp -d)
wget -q -O "$tools/linuxdeploy" https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
wget -q -O "$tools/linuxdeploy-plugin-gtk.sh" https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh
chmod +x "$tools"/*
export PATH="$tools:$PATH" APPIMAGE_EXTRACT_AND_RUN=1 DEPLOY_GTK_VERSION=4 NO_STRIP=1
VERSION=$(python3 -c 'import json;print(json.load(open("project.json"))["release"]["version"])')
export LDAI_OUTPUT="$OUT/QLinuxDeviceManager-$VERSION-Linux-x86_64.AppImage"
ID=io.github.birdie-github.QLinuxDeviceManager
# libkmod and libdisplay-info are dlopen()ed, which ELF scanning cannot see.
"$tools/linuxdeploy" --appdir "$APPDIR" \
    --desktop-file "$APPDIR/usr/share/applications/$ID.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/512x512/apps/$ID.png" \
    --library /usr/lib/x86_64-linux-gnu/libkmod.so.2 \
    --library "$(ls /usr/lib/x86_64-linux-gnu/libdisplay-info.so.[0-9]* | head -1)" \
    --plugin gtk --output appimage
test -f "$LDAI_OUTPUT"
echo "::endgroup::"
echo "built $LDAI_OUTPUT"
