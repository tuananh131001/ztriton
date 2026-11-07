# ztriton — build & install the macOS app bundle.
#
#   make            build release binary
#   make app        build dist/ztriton.app
#   make install    install to ~/Applications + `ztriton` CLI link
#   make uninstall  remove app and CLI link (keeps Keychain login/device id)
#   make bench      CPU/memory benchmark of the release build while playing
#                   (needs a signed-in session); results in proof/perf/
#   make test       unit tests + headless e2e tests; live-server tests read
#                   JELLYFIN_URL / JELLYFIN_USER / JELLYFIN_PASSWORD from the
#                   environment or the git-ignored .env (cp .env.example .env)
#
# Keychain access is tied to the signer's Team ID, or for ad-hoc code to the
# per-build cdhash, so unsigned rebuilds re-prompt for the login password.
# With an "Apple Development" identity (free Apple ID, Xcode › Settings ›
# Accounts), `cargo run`/`cargo test` (runner in .cargo/config.toml) and
# `make app` sign with it, and one "Always Allow" survives rebuilds.
# SIGN_IDENTITY / ZTRITON_SIGN_IDENTITY override the identity.
#
# The app itself never reads .env: its login is entered on the sign-in
# screen and kept in the macOS Keychain (service dev.ztriton.player).
#
# Overrides: make install APP_DIR=/Applications BIN_DIR=/usr/local/bin

APP_NAME   := ztriton
BUNDLE_ID  := dev.ztriton.player
VERSION    := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

APP_DIR    ?= $(HOME)/Applications
BIN_DIR    ?= $(HOME)/.local/bin
CONFIG_DIR ?= $(HOME)/Library/Application Support/ztriton

SIGN_IDENTITY ?= $(shell security find-identity -v -p codesigning | awk -F'"' '/"Apple Development: /{print $$2; exit}')

TARGET_BIN := target/release/$(APP_NAME)
BUNDLE     := dist/$(APP_NAME).app

.PHONY: all build app install uninstall test bench check-deps clean

all: build

check-deps:
	@command -v cargo >/dev/null || { echo "error: cargo not found (mise install / rustup)"; exit 1; }
	@{ test -e /opt/homebrew/lib/libmpv.dylib || test -e /usr/local/lib/libmpv.dylib || test -n "$$MPV_LIB_DIR"; } \
		|| { echo "error: libmpv not found — run: brew install mpv"; exit 1; }

build: check-deps
	cargo build --release

app: build
	@rm -rf "$(BUNDLE)"
	@mkdir -p "$(BUNDLE)/Contents/MacOS" "$(BUNDLE)/Contents/Resources"
	@cp "$(TARGET_BIN)" "$(BUNDLE)/Contents/MacOS/$(APP_NAME)"
	@printf '%s\n' \
	  '<?xml version="1.0" encoding="UTF-8"?>' \
	  '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
	  '<plist version="1.0"><dict>' \
	  '  <key>CFBundleName</key><string>$(APP_NAME)</string>' \
	  '  <key>CFBundleDisplayName</key><string>ztriton</string>' \
	  '  <key>CFBundleIdentifier</key><string>$(BUNDLE_ID)</string>' \
	  '  <key>CFBundleExecutable</key><string>$(APP_NAME)</string>' \
	  '  <key>CFBundlePackageType</key><string>APPL</string>' \
	  '  <key>CFBundleVersion</key><string>$(VERSION)</string>' \
	  '  <key>CFBundleShortVersionString</key><string>$(VERSION)</string>' \
	  '  <key>LSMinimumSystemVersion</key><string>13.0</string>' \
	  '  <key>LSApplicationCategoryType</key><string>public.app-category.entertainment</string>' \
	  '  <key>NSHighResolutionCapable</key><true/>' \
	  '  <key>NSLocalNetworkUsageDescription</key><string>ztriton streams video from your Jellyfin server.</string>' \
	  '</dict></plist>' > "$(BUNDLE)/Contents/Info.plist"
	@if [ -n "$(SIGN_IDENTITY)" ]; then \
		codesign --force --sign "$(SIGN_IDENTITY)" --identifier $(BUNDLE_ID) "$(BUNDLE)" && echo "signed with $(SIGN_IDENTITY)"; \
	else \
		codesign --force --sign - "$(BUNDLE)" >/dev/null 2>&1 && echo "ad-hoc signed (add an Apple Development identity to stop Keychain prompts after rebuilds)" \
			|| echo "warning: codesign failed (app still runs locally)"; \
	fi
	@plutil -lint "$(BUNDLE)/Contents/Info.plist" >/dev/null
	@echo "built $(BUNDLE) (v$(VERSION))"

install: app
	@mkdir -p "$(APP_DIR)" "$(BIN_DIR)" "$(CONFIG_DIR)"
	@rm -rf "$(APP_DIR)/$(APP_NAME).app"
	@cp -R "$(BUNDLE)" "$(APP_DIR)/"
	@ln -sf "$(APP_DIR)/$(APP_NAME).app/Contents/MacOS/$(APP_NAME)" "$(BIN_DIR)/$(APP_NAME)"
	@echo "installed $(APP_DIR)/$(APP_NAME).app"
	@echo "CLI: $(BIN_DIR)/$(APP_NAME)  (open from Spotlight/Finder, or: open '$(APP_DIR)/$(APP_NAME).app')"

uninstall:
	rm -rf "$(APP_DIR)/$(APP_NAME).app"
	rm -f "$(BIN_DIR)/$(APP_NAME)"
	@echo "device id kept in: $(CONFIG_DIR); saved login kept in Keychain (Sign out in the app, or: security delete-generic-password -s $(BUNDLE_ID) -a session)"

test: check-deps
	cargo test --release -- --nocapture --test-threads=1

bench: build
	scripts/bench.sh $(TARGET_BIN)
	MODE=idle scripts/bench.sh $(TARGET_BIN)

clean:
	cargo clean
	rm -rf dist
