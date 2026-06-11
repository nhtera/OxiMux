#!/usr/bin/env bash
#
# Build OxiMux.app for local use. Signs the bundle ad-hoc by default so
# UNUserNotificationCenter (desktop notifications) has a sealed identity;
# set OXIMUX_SIGN_ID to a real "Developer ID Application: …" identity for
# a stable TCC identity across rebuilds. No notarization.
# Output: dist/OxiMux.app
#
# Usage:
#   ./scripts/bundle-macos.sh                 # release bundle (default)
#   ./scripts/bundle-macos.sh debug           # debug bundle (faster build)
#   ./scripts/bundle-macos.sh --debug-fast    # refresh binary only (~200 ms)
#   OXIMUX_SIGN_ID="Developer ID Application: …" ./scripts/bundle-macos.sh
#
# --debug-fast: assumes an existing dist/OxiMux.app and a fresh
# `cargo build -p oximux-app`. Copies target/debug/oximux into the
# bundle without rebuilding the cargo target, regenerating Info.plist,
# or recopying assets. Use it for the inner UI-iteration loop; rerun
# the full bundle whenever Info.plist or assets change.
#
# The PTY relay daemon (oximux-relay) is bundled as a sibling of the
# main binary. The app resolves it via current_exe()'s parent dir, and
# without it every PTY falls back to an in-process backend that dies on
# quit — so terminal scrollback/sessions never survive a relaunch.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

APP_DIR="dist/OxiMux.app"

# Seal the bundle so notification delivery has a code identity. Ad-hoc
# ("-") works for the local dev loop; macOS keys notification permission
# to the bundle id, so the grant survives ad-hoc rebuilds. A real
# identity via OXIMUX_SIGN_ID gives a stable cdhash too (useful when ad-
# hoc behaves inconsistently across TCC resets). Nested binaries first,
# then the bundle seal.
sign_bundle() {
    local sign_id="${OXIMUX_SIGN_ID:--}"
    codesign --force -s "$sign_id" "$APP_DIR/Contents/MacOS/oximux-relay"
    codesign --force -s "$sign_id" "$APP_DIR/Contents/MacOS/oximux"
    codesign --force -s "$sign_id" "$APP_DIR"
    echo "==> Signed $APP_DIR (identity: $sign_id)"
}

# Fast path: refresh the bundled binary in place. Fail loudly if there
# is no existing bundle to refresh — implicit `mkdir` would mask a
# missing full-bundle step and surface as a launch failure later.
if [[ "${1:-}" == "--debug-fast" ]]; then
    if [[ ! -d "$APP_DIR" ]]; then
        echo "error: --debug-fast requires an existing $APP_DIR." >&2
        echo "       run ./scripts/bundle-macos.sh debug first." >&2
        exit 2
    fi
    if [[ ! -f "target/debug/oximux" ]]; then
        echo "error: --debug-fast expects target/debug/oximux." >&2
        echo "       run cargo build -p oximux-app first." >&2
        exit 2
    fi
    if [[ ! -f "target/debug/oximux-relay" ]]; then
        echo "error: --debug-fast expects target/debug/oximux-relay." >&2
        echo "       run cargo build -p oximux-relay first." >&2
        exit 2
    fi
    cp -f "target/debug/oximux" "$APP_DIR/Contents/MacOS/oximux"
    cp -f "target/debug/oximux-relay" "$APP_DIR/Contents/MacOS/oximux-relay"
    # Keep Info.plist + app icon in sync too, so a fast refresh produces a
    # complete bundle (correct menu-bar name + Dock icon), not just binaries.
    cp -f "assets/Info.plist" "$APP_DIR/Contents/Info.plist"
    if [[ -f "assets/AppIcon.icns" ]]; then
        mkdir -p "$APP_DIR/Contents/Resources"
        cp -f "assets/AppIcon.icns" "$APP_DIR/Contents/Resources/AppIcon.icns"
    fi
    sign_bundle
    echo "==> Refreshed $APP_DIR/Contents/{MacOS,Info.plist,Resources} from target/debug"
    exit 0
fi

PROFILE="${1:-release}"
if [[ "$PROFILE" != "release" && "$PROFILE" != "debug" ]]; then
    echo "error: profile must be 'release' or 'debug', got '$PROFILE'" >&2
    exit 2
fi

if [[ "$PROFILE" == "release" ]]; then
    CARGO_FLAGS=(--release)
    TARGET_SUBDIR="release"
else
    CARGO_FLAGS=()
    TARGET_SUBDIR="debug"
fi

echo "==> Building oximux + oximux-relay ($PROFILE)"
# `${CARGO_FLAGS[@]+...}` guards the expansion so an empty array (debug
# profile) doesn't trip `set -u` ("unbound variable") on bash < 4.4.
cargo build -p oximux-app --bin oximux ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}
cargo build -p oximux-relay --bin oximux-relay ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}

echo "==> Assembling $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

cp "target/$TARGET_SUBDIR/oximux" "$APP_DIR/Contents/MacOS/oximux"
cp "target/$TARGET_SUBDIR/oximux-relay" "$APP_DIR/Contents/MacOS/oximux-relay"
cp "assets/Info.plist" "$APP_DIR/Contents/Info.plist"

# App icon — charcoal rounded tile + terminal prompt glyph (matches the
# in-app welcome-view brand). Regenerate from assets/AppIcon.icns.
if [[ -f "assets/AppIcon.icns" ]]; then
    cp "assets/AppIcon.icns" "$APP_DIR/Contents/Resources/AppIcon.icns"
fi

sign_bundle

echo "==> $APP_DIR ready ($(du -sh "$APP_DIR" | cut -f1))"
echo "    open $APP_DIR    # to launch"
