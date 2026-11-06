# ztriton — build & install the macOS app bundle.
#
#   make            build release binary
#   make app        build dist/ztriton.app
#   make install    install to ~/Applications + `ztriton` CLI link
#   make uninstall  remove app and CLI link (keeps Keychain login/device id)
#   make test       unit tests + headless e2e tests; live-server tests need
#                   JELLYFIN_URL / JELLYFIN_USER / JELLYFIN_PASSWORD in the shell env
#
# Credentials are entered in the app's login screen and kept in the macOS
# Keychain (service dev.ztriton.player) — no .env file is read.
#
# Overrides: make install APP_DIR=/Applications BIN_DIR=/usr/local/bin

APP_NAME   := ztriton
BUNDLE_ID  := dev.ztriton.player
VERSION    := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

APP_DIR    ?= $(HOME)/Applications
BIN_DIR    ?= $(HOME)/.local/bin
CONFIG_DIR ?= $(HOME)/Library/Application Support/ztriton

TARGET_BIN := target/release/$(APP_NAME)
BUNDLE     := dist/$(APP_NAME).app

.PHONY: all build app install uninstall test check-deps clean

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
	@codesign --force --sign - "$(BUNDLE)" >/dev/null 2>&1 && echo "ad-hoc signed" || echo "warning: codesign failed (app still runs locally)"
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

clean:
	cargo clean
	rm -rf dist
