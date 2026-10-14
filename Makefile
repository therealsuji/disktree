# disktree: build, check, and install.
#
# `make install` puts the binary, a desktop entry and an icon under PREFIX
# (default: ~/.local), so disktree shows up in the Omarchy launcher and in
# "Open with" for directories. The default needs no root:
#
#   make install                         # ~/.local/bin/disktree
#   sudo make install PREFIX=/usr/local  # system-wide
#
# On macOS it installs disktree.app into ~/Applications instead (APPS to
# change that), plus a `disktree` command in BINDIR that runs the app's
# binary, so both Spotlight and a terminal find it.

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
APPDIR ?= $(PREFIX)/share/applications
ICONDIR ?= $(PREFIX)/share/icons/hicolor/scalable/apps

APPS ?= $(HOME)/Applications
BUNDLE = target/bundle/disktree.app
UNAME := $(shell uname -s)

MANIFEST = Cargo.toml
CARGO ?= cargo
TARGET = target/release/disktree
ICON = assets/disktree.svg
DESKTOP = packaging/disktree.desktop.in

.PHONY: help build run install uninstall bundle lint test ci fmt clean \
	schedule unschedule admin

help:
	@echo "disktree"
	@echo
	@echo "  make build       release build"
	@echo "  make run         build and run, scanning the whole disk"
	@echo "  make install     install to $(PREFIX): binary, desktop entry, icon"
	@echo "                   (macOS: disktree.app into $(APPS))"
	@echo "  make bundle      macOS: build target/bundle/disktree.app and its zip"
	@echo "  make uninstall   remove what install put there"
	@echo "  make schedule    macOS: snapshot the whole disk daily, for growth"
	@echo "  make unschedule  macOS: stop the daily snapshot"
	@echo "  make admin       macOS: open the app as root, to read every folder"
	@echo "  make lint        rustfmt --check and clippy -D warnings"
	@echo "  make test        core and window-harness tests"
	@echo "  make ci          lint, then test"
	@echo "  make fmt         format in place"
	@echo "  make clean       cargo clean"

# Always ask cargo: it is incremental and knows every source file, where a
# make file-target would only compare the binary against the manifest and
# happily install a stale build.
build:
	$(CARGO) build --release

run: build
	$(TARGET)

lint:
	$(CARGO) xtask lint

test:
	$(CARGO) xtask test

ci: lint test

fmt:
	$(CARGO) xtask fmt-fix

ifeq ($(UNAME),Darwin)
# Replaced whole rather than copied over: a stale file left inside a signed
# bundle breaks its signature.
# SIGN names a codesigning identity, such as an Apple Development
# certificate. An ad hoc signature changes with every build, and macOS then
# treats each build as a new app and asks for Full Disk Access again; a real
# identity keeps the grant across rebuilds.
SIGN ?=
install:
	$(CARGO) xtask bundle $(if $(SIGN),--sign "$(SIGN)")
	install -d "$(APPS)" "$(BINDIR)"
	rm -rf "$(APPS)/disktree.app"
	ditto "$(BUNDLE)" "$(APPS)/disktree.app"
	ln -sf "$(APPS)/disktree.app/Contents/MacOS/disktree" "$(BINDIR)/disktree"
	@echo
	@echo "installed:"
	@echo "  $(APPS)/disktree.app"
	@echo "  $(BINDIR)/disktree -> the app's binary"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) \
	    echo; echo "note: $(BINDIR) is not on PATH in this shell";; esac

uninstall: unschedule
	rm -rf "$(APPS)/disktree.app"
	@if [ -L "$(BINDIR)/disktree" ]; then rm -f "$(BINDIR)/disktree"; fi
	@echo "removed"

# The job runs the installed app's own binary, so it can use the app's Full
# Disk Access grant (if macOS gives it one) and reads what the window reads.
AGENT_LABEL = io.github.tobi.disktree.snapshot
AGENT = $(HOME)/Library/LaunchAgents/$(AGENT_LABEL).plist
schedule:
	@test -x "$(APPS)/disktree.app/Contents/MacOS/disktree" || \
	    { echo "run make install first"; exit 1; }
	install -d "$(HOME)/Library/LaunchAgents"
	sed -e 's|@LABEL@|$(AGENT_LABEL)|' \
	    -e 's|@BIN@|$(APPS)/disktree.app/Contents/MacOS/disktree|' \
	    -e 's|@LOG@|$(HOME)/Library/Logs/disktree-snapshot.log|' \
	    packaging/macos/snapshot.plist.in > "$(AGENT)"
	@launchctl bootout gui/$$(id -u) "$(AGENT)" 2>/dev/null || true
	launchctl bootstrap gui/$$(id -u) "$(AGENT)"
	@echo "daily snapshot scheduled: $(AGENT)"

unschedule:
	@launchctl bootout gui/$$(id -u) "$(AGENT)" 2>/dev/null || true
	rm -f "$(AGENT)"

# Full Disk Access does not open folders owned by root or the system's
# service accounts; only root reads those. Root keeps its own settings and
# history under /var/root, so nothing it writes ends up root-owned in your
# home folder, and anything it removes goes to root's Trash.
ROOTRUN = \"$(APPS)/disktree.app/Contents/MacOS/disktree\" / >/dev/null 2>&1 &
admin:
	@test -x "$(APPS)/disktree.app/Contents/MacOS/disktree" || \
	    { echo "run make install first"; exit 1; }
	@osascript -e 'do shell script "HOME=/var/root $(ROOTRUN)" with administrator privileges'
else
install: build
	install -d $(BINDIR) $(APPDIR) $(ICONDIR)
	install -m755 $(TARGET) $(BINDIR)/disktree
	install -m644 $(ICON) $(ICONDIR)/disktree.svg
	VERSION=$$(sed -n 's/^version = "\(.*\)"/\1/p' $(MANIFEST) | head -1) && \
	sed -e 's|@BINDIR@|$(BINDIR)|' -e "s|@VERSION@|$$VERSION|" \
	    $(DESKTOP) > $(APPDIR)/disktree.desktop && \
	chmod 644 $(APPDIR)/disktree.desktop
	@if command -v update-desktop-database >/dev/null 2>&1; then \
	    update-desktop-database $(APPDIR) 2>/dev/null || true; \
	fi
	@echo
	@echo "installed:"
	@echo "  $(BINDIR)/disktree"
	@echo "  $(APPDIR)/disktree.desktop"
	@echo "  $(ICONDIR)/disktree.svg"
	@if command -v desktop-file-validate >/dev/null 2>&1; then \
	    desktop-file-validate $(APPDIR)/disktree.desktop || true; \
	fi
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) \
	    echo; echo "note: $(BINDIR) is not on PATH in this shell";; esac

uninstall:
	rm -f $(BINDIR)/disktree $(APPDIR)/disktree.desktop $(ICONDIR)/disktree.svg
	@if command -v update-desktop-database >/dev/null 2>&1; then \
	    update-desktop-database $(APPDIR) 2>/dev/null || true; \
	fi
	@echo "removed"
endif

bundle:
	$(CARGO) xtask bundle

clean:
	$(CARGO) clean
