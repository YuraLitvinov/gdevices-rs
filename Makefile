# Install recipe for the Rust build:  make && sudo make install PREFIX=/usr
# Packagers: make install DESTDIR=$PWD/pkg PREFIX=/usr
PREFIX ?= /usr
DESTDIR ?=
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
LOCALEDIR ?= $(DATADIR)/locale
APP_ID := io.github.birdie-github.QLinuxDeviceManager
EXE := qlinuxdevicemanager
TARGET_DIR ?= target/release
ICON_SIZES := 16 32 48 64 128 256 512
LINGUAS := $(basename $(notdir $(wildcard po/*.po)))

all:
	QLDM_LOCALE_DIR=$(LOCALEDIR) cargo build --release --workspace --locked

check:
	cargo test --workspace --locked

# The camera helper must sit next to the GUI binary: the GUI looks for it there.
install: all
	install -Dm755 $(TARGET_DIR)/qldm-gtk $(DESTDIR)$(BINDIR)/$(EXE)
	install -Dm755 $(TARGET_DIR)/qldm-camera-helper $(DESTDIR)$(BINDIR)/qldm-camera-helper
	install -Dm644 packaging/$(APP_ID).desktop $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	for s in $(ICON_SIZES); do \
	  install -Dm644 resources/external/icons/$${s}x$${s}/QLinuxDeviceManager.png \
	    $(DESTDIR)$(DATADIR)/icons/hicolor/$${s}x$${s}/apps/$(APP_ID).png; done
	install -Dm644 resources/external/icons/QLinuxDeviceManager.svg \
	    $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 project.json $(DESTDIR)$(DATADIR)/QLinuxDeviceManager/project.json
	install -Dm644 LICENSE $(DESTDIR)$(DATADIR)/licenses/QLinuxDeviceManager/LICENSE
	for l in $(LINGUAS); do \
	  install -d $(DESTDIR)$(LOCALEDIR)/$$l/LC_MESSAGES && \
	  msgfmt po/$$l.po -o $(DESTDIR)$(LOCALEDIR)/$$l/LC_MESSAGES/qlinuxdevicemanager.mo; done

validate:
	desktop-file-validate packaging/$(APP_ID).desktop

pot:
	po/update-pot.sh

.PHONY: all check install validate pot
