#!/usr/bin/env bash
#
# Fetch the pinned, checksum-verified iOS Simulator helper for bundling into
# OxiMux.app. The Simulator panel spawns `oximux-sim-helper`, a stdio-only
# child that streams and drives one simulator.
#
# The helper is NOT built here and none of its source lives in this repo. It
# is built and released by our fork of serve-sim (Apache-2.0):
#   https://github.com/nhtera/serve-sim  (branch `oximux`, see oximux/README.md)
# To use a locally built fork instead, point the OXIMUX_SIM_HELPER env override
# at it (read by the simulator crate); this script is not involved.
#
# Output: target/bundle-tools/oximux-sim-helper (+ LICENSE, + .version stamp)
#
# The sha256 is pinned HERE, not read from the release's own .sha256 asset:
# a replaced release asset must fail the build, not re-pin itself.
#
# Caching: if the output exists and the stamp matches the pinned version, this
# is a no-op, so offline rebuilds keep working once fetched. A checksum
# mismatch is always fatal.
#
# arm64 only: the OxiMux DMG is arm64-only and the helper release matches. On
# any other host this warns and skips (exit 0) so an Intel dev bundle still
# builds; the Simulator panel reports the missing helper as "unavailable".
#
# The fetched binary is never executed here: the pinned sha256 already fixes
# its exact bytes, and running a freshly extracted binary during a build can
# hang on a Mac whose XProtect scan is stuck.
set -euo pipefail

cd "$(dirname "$0")/.."

SIM_HELPER_VERSION="0.1.0"
SIM_HELPER_SHA256="aef556918cb6fabc03aab96649cdc4579541e3a0bc410242905a75807e4b6b01"
REPO="nhtera/serve-sim"

NAME="oximux-sim-helper-${SIM_HELPER_VERSION}-macos-arm64"
URL="https://github.com/${REPO}/releases/download/helper-v${SIM_HELPER_VERSION}/${NAME}.tar.gz"

OUT_DIR="target/bundle-tools"
OUT_BIN="$OUT_DIR/oximux-sim-helper"
OUT_LICENSE="$OUT_DIR/oximux-sim-helper.LICENSE"
STAMP="$OUT_DIR/oximux-sim-helper.version"
WANT="oximux-sim-helper ${SIM_HELPER_VERSION} [arm64] ${SIM_HELPER_SHA256}"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "==> Simulator helper is macOS-only; skipping on $(uname -s)"
    exit 0
fi
if [[ "$(uname -m)" != "arm64" ]]; then
    echo "warning: the simulator helper is released for arm64 only; skipping on $(uname -m)" >&2
    exit 0
fi

if [[ -x "$OUT_BIN" && -f "$OUT_LICENSE" && -f "$STAMP" && "$(cat "$STAMP")" == "$WANT" ]]; then
    echo "==> Simulator helper up to date ($SIM_HELPER_VERSION), skipping fetch"
    exit 0
fi

mkdir -p "$OUT_DIR"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/oximux-sim-helper.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

echo "==> Fetching ${NAME}.tar.gz"
curl -fsSL --retry 3 -o "$WORK/${NAME}.tar.gz" "$URL"

# Pinned checksum: "<hex>  <file>" for shasum -c, run where the file sits.
echo "${SIM_HELPER_SHA256}  ${NAME}.tar.gz" > "$WORK/${NAME}.tar.gz.sha256"
(cd "$WORK" && shasum -a 256 -c "${NAME}.tar.gz.sha256")

tar -xzf "$WORK/${NAME}.tar.gz" -C "$WORK"
for f in oximux-sim-helper LICENSE; do
    if [[ ! -f "$WORK/$NAME/$f" ]]; then
        echo "error: ${NAME}.tar.gz did not contain $NAME/$f — release layout changed?" >&2
        exit 1
    fi
done

cp -f "$WORK/$NAME/oximux-sim-helper" "$OUT_BIN"
cp -f "$WORK/$NAME/LICENSE" "$OUT_LICENSE"
chmod 755 "$OUT_BIN"
echo "$WANT" > "$STAMP"
echo "==> $OUT_BIN ready ($SIM_HELPER_VERSION)"
