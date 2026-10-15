# Building and packaging

## Build and install

Build dependencies (Debian/Ubuntu names): `build-essential pkg-config libgtk-4-dev libudev-dev
libkmod-dev libdisplay-info-dev libv4l-dev libwayland-dev libxcb-randr0-dev clang libclang-dev
gettext`, plus a stable Rust toolchain. `libkmod` and `libdisplay-info` are loaded with `dlopen` at
run time and `libudisks2` is reached over D-Bus, so each is optional on the target machine.

```sh
make                                   # release build of the whole workspace
make check                             # cargo test --workspace
sudo make install PREFIX=/usr          # or: make install DESTDIR=$PWD/pkg PREFIX=/usr
```

`make install` puts these in place:

| File | Destination |
|---|---|
| GUI (`qldm-gtk`) | `$PREFIX/bin/qlinuxdevicemanager` |
| `qldm-camera-helper` | `$PREFIX/bin/` — **must stay next to the GUI binary**, which looks for it there |
| Desktop entry (id `io.github.birdie-github.QLinuxDeviceManager`, unchanged) | `share/applications/` |
| hicolor icons 16–512 px and the scalable SVG | `share/icons/hicolor/…/apps/` |
| `project.json` | `share/QLinuxDeviceManager/` |
| Message catalogues (`po/<lang>.po` → `.mo`) | `share/locale/<lang>/LC_MESSAGES/qlinuxdevicemanager.mo` |

The catalogue directory defaults to `/usr/share/locale`; build with `QLDM_LOCALE_DIR=<dir>` (the
Makefile does this from `LOCALEDIR`) to bake in another one, or set it at run time to test an
uninstalled tree. Name, version and repository come from `project.json` through
`crates/qldm-core/build.rs` (`qldm_core::app`), as CMake did.

## Translations

English is the source language: each `tr("…")` literal is its own msgid, translated with glibc
`dgettext` (no extra crate or library). `po/update-pot.sh` regenerates `po/qlinuxdevicemanager.pot`
with `xgettext`; a translator copies it to `po/<lang>.po`, and `make install` compiles every
`po/*.po`. Only string literals passed directly to `tr` are extracted, so keep the literal at the
call site. Placeholders are `%1`, `%2`, … as in the earlier C++ version. No translations beyond English
are shipped yet.

## Continuous integration

`.github/workflows/rust.yml` runs `cargo fmt --check`, `cargo clippy --workspace --all-targets
-D warnings`, the unit and integration tests, the GTK smoke test under `xvfb`, `desktop-file-validate`,
a check that the `.pot` is current, and a staged `make install`. The earlier Qt AppImage workflow was removed with the C++ code.

## Flatpak (manifest sketch, not built or tested)

Use the GNOME runtime (`org.gnome.Platform`, `org.gnome.Sdk` with the rust-stable extension). The
application reads `/sys`, `/proc`, `/dev` and the system bus, so:

```yaml
app-id: io.github.birdie-github.QLinuxDeviceManager
runtime: org.gnome.Platform
runtime-version: '48'
sdk: org.gnome.Sdk
sdk-extensions: [org.freedesktop.Sdk.Extension.rust-stable]
command: qlinuxdevicemanager
finish-args:
  - --share=ipc
  - --socket=wayland
  - --socket=fallback-x11
  - --device=all                         # /dev/video*, /dev/dri for camera and display queries
  - --system-talk-name=org.freedesktop.UDisks2
  - --filesystem=host-os:ro              # /usr/lib kernel modules and the host's libkmod data
modules:
  - name: qlinuxdevicemanager
    buildsystem: simple
    build-commands: [make install PREFIX=/app]
```

Inside the sandbox `/sys` and `/proc` are those of the host kernel but processes, mounts and
network namespaces are the sandbox's, and `libkmod`/`libdisplay-info` must be bundled. Treat this
as a starting point: it has not been built.

## AppImage (replaces the Qt recipe)

Stage with `make install PREFIX=/usr DESTDIR=$PWD/AppDir`, then run `linuxdeploy` with
`--appdir AppDir --desktop-file … --icon-file … --plugin gtk --output appimage`
(the `linuxdeploy-plugin-gtk` bundles GTK 4 and its schemas). Bundle `libkmod.so.2` and
`libdisplay-info.so.*` with `--library` since they are `dlopen`ed and invisible to ELF scanning.
This has not been run; build on the oldest distribution you intend to support.
