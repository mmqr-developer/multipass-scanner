# Build and install Multi-pass Scanner.
#
#   make                  build the release binary (as your own user)
#   make test             run the unit tests
#
# Linux:
#   sudo make install     install into /usr/local
#   sudo make uninstall   remove it again
#   make check-macos      type-check the macOS scanner backend from Linux
#                         (needs: rustup target add aarch64-apple-darwin)
#   make check-windows    type-check the Windows-only code from Linux
#                         (needs: rustup target add x86_64-pc-windows-msvc)
#   PREFIX and DESTDIR work as usual (e.g. make install PREFIX=$HOME/.local).
#
# macOS:
#   make app              make "Multi-pass Scanner.app" in target/release
#   make scan-test        try the scanner without the app: five small scans,
#                         checked by the program, pictures in scan-test/
#
# Windows has no make: use packaging\windows\build.ps1 and install.ps1.

PREFIX  ?= /usr/local
DESTDIR ?=
BIN     := target/release/multipass-scanner
APPDIR  := $(DESTDIR)$(PREFIX)/share/applications
ICONDIR := $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps

all: $(BIN)

$(BIN): Cargo.toml $(wildcard src/*.rs) crates/flatbed/Cargo.toml $(wildcard crates/flatbed/src/*.rs)
	cargo build --release

# The scanner backend alone: no OpenCV, no GUI.
scan-test:
	cargo run --release -p flatbed --example scan-test -- scan-test

# The macOS backend cannot be built on Linux, but it can be type-checked.
check-macos:
	cargo check -p flatbed --all-targets --target aarch64-apple-darwin

# Likewise the Windows backend, and the app's few Windows-only lines.
check-windows:
	cargo check -p flatbed --all-targets --target x86_64-pc-windows-msvc
	packaging/windows/check-app.sh

ifeq ($(shell uname -s),Darwin)

app: $(BIN)
	packaging/macos/bundle.sh

install uninstall:
	@echo "On macOS: run 'make app', then copy the app from target/release to /Applications."; exit 1

else

# `install` only copies: run `make` first as your own user, so that cargo does
# not run as root.
install:
	@test -x $(BIN) || { echo "run 'make' first (as your own user), then 'sudo make install'"; exit 1; }
	install -D -m 0755 $(BIN) $(DESTDIR)$(PREFIX)/bin/multipass-scanner
	install -d $(APPDIR) $(ICONDIR)
	sed 's|/usr/local/bin/|$(PREFIX)/bin/|g' packaging/linux/multipass-scanner.desktop > $(APPDIR)/multipass-scanner.desktop
	chmod 0644 $(APPDIR)/multipass-scanner.desktop
	install -m 0644 packaging/multipass-scanner.svg $(ICONDIR)/multipass-scanner.svg
	-[ -z "$(DESTDIR)" ] && update-desktop-database -q $(PREFIX)/share/applications 2>/dev/null || true
	-[ -z "$(DESTDIR)" ] && gtk-update-icon-cache -q -t $(PREFIX)/share/icons/hicolor 2>/dev/null || true

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/multipass-scanner
	rm -f $(APPDIR)/multipass-scanner.desktop
	rm -f $(ICONDIR)/multipass-scanner.svg
	-[ -z "$(DESTDIR)" ] && update-desktop-database -q $(PREFIX)/share/applications 2>/dev/null || true

endif

test:
	cargo test --release --workspace

.PHONY: all install uninstall test app scan-test check-macos check-windows
