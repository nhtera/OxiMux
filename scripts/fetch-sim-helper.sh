#!/usr/bin/env bash
#
# Fetch the pinned, checksum-verified helpers of the Mobile Emulator panel for
# bundling into OxiMux.app, both from one release:
#   - `oximux-sim-helper`, a stdio-only child that streams and drives one
#     iOS simulator;
#   - `OxiMux Device Capture.app`, which streams a USB iPhone's screen. It is
#     released unsigned: bundle-macos.sh signs it with the camera entitlement
#     alone (assets/device-capture.entitlements).
#
# Neither is built here and none of their source lives in this repo. They are
# built and released by our fork of serve-sim (Apache-2.0):
#   https://github.com/nhtera/serve-sim  (branch `oximux`, see oximux/README.md)
# To use a locally built fork instead, point the OXIMUX_SIM_HELPER /
# OXIMUX_DEVICE_CAPTURE env overrides at it (read by the simulator crate);
# this script is not involved.
#
# Output, in target/bundle-tools/:
#   oximux-sim-helper (+ .LICENSE, + .version stamp)
#   OxiMux Device Capture.app (+ oximux-device-capture.version stamp)
#
# The sha256s are pinned HERE, not read from the release's own .sha256
# assets: a replaced release asset must fail the build, not re-pin itself.
#
# Caching: an output whose stamp matches its pin is not fetched again, so
# offline rebuilds keep working once fetched. A checksum mismatch is always
# fatal.
#
# arm64 only: the OxiMux DMG is arm64-only and the helper release matches. On
# any other host this warns and skips (exit 0) so an Intel dev bundle still
# builds; the panel reports the missing helpers as "unavailable".
#
# The fetched binaries are never executed here: the pinned sha256 already
# fixes their exact bytes, and running a freshly extracted binary during a
# build can hang on a Mac whose XProtect scan is stuck.
set -euo pipefail

cd "$(dirname "$0")/.."

HELPER_VERSION="0.4.0"
SIM_HELPER_SHA256="99123302115a6b303b8d39ba508bd04253b7d40f7485df7dc650366cac754fc4"
CAPTURE_SHA256="02831df2bc8b45553bce7de7b64218a48285f00e16cf90d65a71c3621ee52b99"
REPO="nhtera/serve-sim"
RELEASE="https://github.com/${REPO}/releases/download/helper-v${HELPER_VERSION}"

OUT_DIR="target/bundle-tools"
CAPTURE_APP="OxiMux Device Capture.app"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "==> Simulator helper is macOS-only; skipping on $(uname -s)"
    exit 0
fi
if [[ "$(uname -m)" != "arm64" ]]; then
    echo "warning: the simulator helper is released for arm64 only; skipping on $(uname -m)" >&2
    exit 0
fi

mkdir -p "$OUT_DIR"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/oximux-sim-helper.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# Download `<name>.tar.gz`, check it against `sha`, and unpack it into $WORK.
fetch() {
    local name="$1" sha="$2"
    echo "==> Fetching ${name}.tar.gz"
    curl -fsSL --retry 3 -o "$WORK/${name}.tar.gz" "${RELEASE}/${name}.tar.gz"
    # Pinned checksum: "<hex>  <file>" for shasum -c, run where the file sits.
    echo "${sha}  ${name}.tar.gz" > "$WORK/${name}.tar.gz.sha256"
    (cd "$WORK" && shasum -a 256 -c "${name}.tar.gz.sha256")
    tar -xzf "$WORK/${name}.tar.gz" -C "$WORK"
}

# Fail loudly when the release's layout is not what this script expects.
require() {
    local archive="$1" path="$2"
    if [[ ! -e "$WORK/$path" ]]; then
        echo "error: ${archive}.tar.gz did not contain $path — release layout changed?" >&2
        exit 1
    fi
}

# --- The simulator helper -----------------------------------------------------
name="oximux-sim-helper-${HELPER_VERSION}-macos-arm64"
bin="$OUT_DIR/oximux-sim-helper"
license="$OUT_DIR/oximux-sim-helper.LICENSE"
stamp="$OUT_DIR/oximux-sim-helper.version"
want="oximux-sim-helper ${HELPER_VERSION} [arm64] ${SIM_HELPER_SHA256}"
if [[ -x "$bin" && -f "$license" && -f "$stamp" && "$(cat "$stamp")" == "$want" ]]; then
    echo "==> Simulator helper up to date ($HELPER_VERSION), skipping fetch"
else
    fetch "$name" "$SIM_HELPER_SHA256"
    require "$name" "$name/oximux-sim-helper"
    require "$name" "$name/LICENSE"
    cp -f "$WORK/$name/oximux-sim-helper" "$bin"
    cp -f "$WORK/$name/LICENSE" "$license"
    chmod 755 "$bin"
    echo "$want" > "$stamp"
    echo "==> $bin ready ($HELPER_VERSION)"
fi

# --- The capture app -------------------------------------------------------------
name="oximux-device-capture-${HELPER_VERSION}-macos-arm64"
app="$OUT_DIR/$CAPTURE_APP"
exe="$app/Contents/MacOS/oximux-device-capture"
stamp="$OUT_DIR/oximux-device-capture.version"
want="oximux-device-capture ${HELPER_VERSION} [arm64] ${CAPTURE_SHA256}"
if [[ -x "$exe" && -f "$stamp" && "$(cat "$stamp")" == "$want" ]]; then
    echo "==> Capture app up to date ($HELPER_VERSION), skipping fetch"
else
    fetch "$name" "$CAPTURE_SHA256"
    require "$name" "$name/$CAPTURE_APP/Contents/MacOS/oximux-device-capture"
    require "$name" "$name/$CAPTURE_APP/Contents/Info.plist"
    rm -rf "$app"
    # `ditto` keeps the bundle exactly as released (no signature yet: the
    # bundle step signs it).
    ditto "$WORK/$name/$CAPTURE_APP" "$app"
    chmod 755 "$exe"
    echo "$want" > "$stamp"
    echo "==> $app ready ($HELPER_VERSION)"
fi
