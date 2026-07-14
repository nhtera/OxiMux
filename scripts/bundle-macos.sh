#!/usr/bin/env bash
#
# Build OxiMux.app for local use. Signs the bundle ad-hoc by default so
# UNUserNotificationCenter (desktop notifications) has a sealed identity;
# set OXIMUX_CODESIGN_IDENTITY (or the legacy OXIMUX_SIGN_ID, or pass
# --sign <identity>) to a real "Apple Development: …" / "Developer ID
# Application: …" identity for a stable TCC identity across rebuilds and
# to make notification authorization grants stick. No notarization.
# Output: dist/OxiMux.app
#
# Usage:
#   ./scripts/bundle-macos.sh                 # release bundle (default)
#   ./scripts/bundle-macos.sh debug           # debug bundle (faster build)
#   ./scripts/bundle-macos.sh --debug-fast    # refresh binary only (~200 ms)
#   ./scripts/bundle-macos.sh --sign "Apple Development: you@example.com (TEAMID)" debug
#   OXIMUX_CODESIGN_IDENTITY="Apple Development: …" ./scripts/bundle-macos.sh
#
# Ad-hoc ("-") is fine for exercising the UI, but `UNUserNotificationCenter`
# silently drops the one-time authorization grant for ad-hoc-signed
# bundles on some macOS versions — banners never appear even after
# accepting the permission prompt. Sign with a real identity to test
# notifications end-to-end:
#
#   1. `security find-identity -v -p codesigning` — if an "Apple
#      Development: …" or "Developer ID Application: …" identity is
#      listed, use its NAME as the --sign value (not the SHA-1 hash —
#      signature verification matches the certificate name against the
#      sealed bundle's Authority chain, which never shows hashes).
#   2. No usable identity? Create a one-time local self-signed
#      codesigning cert (never leaves this machine, no Apple account
#      needed):
#        a. Open Keychain Access → Certificate Assistant → Create a
#           Certificate…
#        b. Name: e.g. "OxiMux Local Dev"; Identity Type: Self Signed
#           Root; Certificate Type: Code Signing; check "Let me
#           override defaults" only if you need a longer validity.
#        c. Create, then in Keychain Access find the cert, expand it,
#           double-click the private key, and under Access Control
#           allow /usr/bin/codesign (or "Always Allow" when prompted
#           the first time you sign with it) — otherwise every codesign
#           invocation blocks on a keychain GUI prompt.
#        d. Re-run `security find-identity -v -p codesigning` to
#           confirm it is now "valid" and pass its name via --sign.
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

# Pull --sign/--sign=<identity> out of the argument list up front so the
# rest of the script's positional parsing (profile / --debug-fast) is
# untouched. Left in ARGS, everything else passes through unchanged.
SIGN_FLAG=""
ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign)
            if [[ $# -lt 2 ]]; then
                echo "error: --sign requires an identity argument" >&2
                exit 2
            fi
            SIGN_FLAG="$2"
            shift 2
            ;;
        --sign=*)
            SIGN_FLAG="${1#--sign=}"
            shift
            ;;
        *)
            ARGS+=("$1")
            shift
            ;;
    esac
done
set -- ${ARGS[@]+"${ARGS[@]}"}

# Seal the bundle so notification delivery has a code identity. Ad-hoc
# ("-") works for the local dev loop; macOS keys notification permission
# to the bundle id, so the grant survives ad-hoc rebuilds — but ad-hoc
# bundles never actually get the UNUserNotificationCenter grant (see the
# header note above). A real identity, via --sign, OXIMUX_CODESIGN_IDENTITY,
# or the legacy OXIMUX_SIGN_ID, gives a stable cdhash and a working
# notification grant. Nested binaries first, then the bundle seal — this
# is deliberately the opposite of `--deep` (deprecated/unreliable for
# app bundles): sign the relay, then the main binary, then reseal the
# app's resource envelope last so it captures both.
sign_bundle() {
    local sign_id="${SIGN_FLAG:-${OXIMUX_CODESIGN_IDENTITY:-${OXIMUX_SIGN_ID:--}}}"
    codesign --force -s "$sign_id" "$APP_DIR/Contents/MacOS/oximux-relay"
    codesign --force -s "$sign_id" "$APP_DIR/Contents/MacOS/oximux"
    codesign --force -s "$sign_id" "$APP_DIR"
    echo "==> Signed $APP_DIR (identity: $sign_id)"
    # Only the opt-in real-identity path pays for verification, so the
    # default ad-hoc dev loop is untouched (same commands, same output).
    if [[ "$sign_id" != "-" ]]; then
        verify_signature "$sign_id"
    fi
}

# Fail loudly rather than silently shipping a mis-signed bundle: a real
# identity was requested but not applied would otherwise surface much
# later as "notifications still don't work" with no clue why. Only
# called when a real identity was requested (see sign_bundle above).
verify_signature() {
    local sign_id="$1"
    local report
    if ! report="$(codesign --display --verbose=4 "$APP_DIR" 2>&1)"; then
        echo "error: codesign -dv failed on $APP_DIR after signing:" >&2
        echo "$report" >&2
        exit 1
    fi
    echo "==> codesign -dv $APP_DIR"
    echo "$report" | sed 's/^/    /'
    if ! grep -qF "Authority=$sign_id" <<<"$report"; then
        echo "error: requested signing identity '$sign_id' but the sealed" >&2
        echo "       bundle's Authority chain does not show it — treat this" >&2
        echo "       as a failed sign (see codesign -dv output above)." >&2
        exit 1
    fi
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
