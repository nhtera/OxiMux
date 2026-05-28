#!/usr/bin/env bash
#
# Build OxiMux.app for local use. No signing, no notarization in Phase 0.
# Output: dist/OxiMux.app
#
# Usage:
#   ./scripts/bundle-macos.sh         # release bundle (default)
#   ./scripts/bundle-macos.sh debug   # debug bundle (faster build)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

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

echo "==> Building oximux ($PROFILE)"
# `${CARGO_FLAGS[@]+...}` guards the expansion so an empty array (debug
# profile) doesn't trip `set -u` ("unbound variable") on bash < 4.4.
cargo build -p oximux-app --bin oximux ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}

APP_DIR="dist/OxiMux.app"
echo "==> Assembling $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

cp "target/$TARGET_SUBDIR/oximux" "$APP_DIR/Contents/MacOS/oximux"
cp "assets/Info.plist" "$APP_DIR/Contents/Info.plist"

# Placeholder icon — replace with assets/AppIcon.icns when designed.
if [[ -f "assets/AppIcon.icns" ]]; then
    cp "assets/AppIcon.icns" "$APP_DIR/Contents/Resources/AppIcon.icns"
fi

echo "==> $APP_DIR ready ($(du -sh "$APP_DIR" | cut -f1))"
echo "    open $APP_DIR    # to launch"
