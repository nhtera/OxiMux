#!/usr/bin/env bash
#
# Package dist/OxiMux.app into a styled, signed (and optionally notarized)
# DMG: dist/OxiMux-<version>-macos-<arch>.dmg
#
# The DMG opens as the classic drag-to-install window: the app icon on the
# left, an Applications alias on the right, a dashed arrow between them
# (assets/dmg-background.tiff — regenerate with
# `swift scripts/generate-dmg-background.swift`; the icon coordinates below
# and the arrow position in that script are one layout and must move
# together).
#
# Usage:
#   ./scripts/make-dmg.sh --sign "Developer ID Application: … (TEAMID)"
#   ./scripts/make-dmg.sh --sign "…" --notarize      # submit + staple + gate
#
# Expects dist/OxiMux.app to already exist and be signed (hardened runtime
# for a notarized release): run
#   ./scripts/bundle-macos.sh --hardened --sign "Developer ID Application: …"
# first. This script deliberately does NOT rebuild — packaging a stale bundle
# is caught by the version check against Cargo.toml below.
#
# Notarization credentials, in order of preference:
#   1. App Store Connect API key via env (the CI path):
#        NOTARY_KEY_PATH  — path to the AuthKey_<id>.p8 file
#        NOTARY_KEY_ID    — the key id
#        NOTARY_ISSUER_ID — the issuer id
#   2. Keychain profile "$OXIMUX_NOTARY_PROFILE" (default oximux-notary),
#      created once with `xcrun notarytool store-credentials` (the local path,
#      same profile bundle-macos.sh --notarize uses).
#
# Note the split from bundle-macos.sh --notarize: that flow notarizes the
# BARE .app (zip transport) for direct-zip distribution. For a DMG release,
# skip it — submit the DMG once here; notarization covers the app inside it
# and the ticket staples to the DMG, which is what users actually download.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

APP_DIR="dist/OxiMux.app"
BACKGROUND="assets/dmg-background.tiff"
NOTARY_PROFILE="${OXIMUX_NOTARY_PROFILE:-oximux-notary}"

SIGN_ID="${OXIMUX_CODESIGN_IDENTITY:-}"
NOTARIZE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign)
            [[ $# -ge 2 ]] || { echo "error: --sign requires an identity" >&2; exit 2; }
            SIGN_ID="$2"; shift 2 ;;
        --sign=*)
            SIGN_ID="${1#--sign=}"; shift ;;
        --notarize)
            NOTARIZE=1; shift ;;
        *)
            echo "error: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

# ---- preflight: fail before the expensive steps -------------------------

[[ -d "$APP_DIR" ]] || {
    echo "error: $APP_DIR not found. Build it first:" >&2
    echo "       ./scripts/bundle-macos.sh --hardened --sign \"Developer ID Application: …\"" >&2
    exit 2
}
[[ -f "$BACKGROUND" ]] || {
    echo "error: $BACKGROUND missing. Regenerate it:" >&2
    echo "       swift scripts/generate-dmg-background.swift" >&2
    exit 2
}
command -v create-dmg >/dev/null || {
    echo "error: create-dmg not installed (brew install create-dmg)" >&2
    exit 2
}
[[ -n "$SIGN_ID" ]] || {
    echo "error: a signing identity is required (--sign or OXIMUX_CODESIGN_IDENTITY):" >&2
    echo "       an unsigned DMG around a signed app still trips Gatekeeper heuristics" >&2
    exit 2
}
if [[ "$NOTARIZE" -eq 1 && "$SIGN_ID" != "Developer ID Application:"* ]]; then
    echo "error: notarization requires a 'Developer ID Application' identity; got '$SIGN_ID'" >&2
    exit 2
fi

# The bundle's version, and a guard that it matches the workspace: packaging
# yesterday's dist/ under today's tag is the silent failure mode of a script
# that doesn't rebuild.
VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_DIR/Contents/Info.plist")"
CARGO_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [[ "$VERSION" != "$CARGO_VERSION" ]]; then
    echo "error: $APP_DIR is v$VERSION but Cargo.toml says v$CARGO_VERSION — stale bundle?" >&2
    echo "       rebuild with ./scripts/bundle-macos.sh --hardened --sign \"…\"" >&2
    exit 2
fi

ARCH="$(uname -m)"   # arm64 on Apple silicon
DMG="dist/OxiMux-${VERSION}-macos-${ARCH}.dmg"

# ---- assemble ------------------------------------------------------------

# create-dmg copies the whole source folder into the image, so stage the app
# alone — dist/ also holds zips and build leftovers that must not ship.
STAGING="$(mktemp -d "${TMPDIR:-/tmp}/oximux-dmg.XXXXXX")"
trap 'rm -rf "$STAGING"' EXIT
cp -a "$APP_DIR" "$STAGING/"

echo "==> Building $DMG"
rm -f "$DMG"
# Layout contract with assets/dmg-background.tiff (660x350 = the window's
# 660x400 minus title/status bar; must stay a 72-dpi 1x image — Finder
# ignores dpi tags and draws backdrops at raw pixel size).
# --hdiutil-retries: create-dmg drives Finder via AppleScript and hdiutil,
# both of which are intermittently flaky on CI runners; retries are the
# tool's own supported mitigation.
create-dmg \
    --volname "OxiMux ${VERSION}" \
    --volicon "assets/AppIcon.icns" \
    --background "$BACKGROUND" \
    --window-pos 200 120 \
    --window-size 660 400 \
    --icon-size 128 \
    --icon "OxiMux.app" 165 190 \
    --hide-extension "OxiMux.app" \
    --app-drop-link 495 190 \
    --hdiutil-retries 5 \
    "$DMG" \
    "$STAGING"

# Sign the DMG itself: notarization requires a signed container, and a signed
# outer image keeps Gatekeeper's provenance chain intact end to end.
echo "==> Signing $DMG"
codesign --force --sign "$SIGN_ID" --timestamp "$DMG"

if [[ "$NOTARIZE" -eq 1 ]]; then
    # Pick credentials: API key env (CI) or the local keychain profile.
    NOTARY_ARGS=()
    if [[ -n "${NOTARY_KEY_PATH:-}" && -n "${NOTARY_KEY_ID:-}" && -n "${NOTARY_ISSUER_ID:-}" ]]; then
        NOTARY_ARGS=(--key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID")
        echo "==> Notarizing with App Store Connect API key"
    else
        if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
            echo "error: no notarytool credentials. Either export NOTARY_KEY_PATH/" >&2
            echo "       NOTARY_KEY_ID/NOTARY_ISSUER_ID, or create the keychain" >&2
            echo "       profile once:" >&2
            echo "         xcrun notarytool store-credentials \"$NOTARY_PROFILE\" \\" >&2
            echo "           --apple-id <your-apple-id> --team-id <TEAMID>" >&2
            exit 2
        fi
        NOTARY_ARGS=(--keychain-profile "$NOTARY_PROFILE")
        echo "==> Notarizing with keychain profile '$NOTARY_PROFILE'"
    fi

    echo "==> Submitting $DMG (blocks until the verdict; usually 1-15 min)"
    if ! xcrun notarytool submit "$DMG" "${NOTARY_ARGS[@]}" --wait; then
        echo "error: notarization failed. Per-issue detail:" >&2
        echo "       xcrun notarytool log <submission-id> ${NOTARY_ARGS[*]}" >&2
        exit 1
    fi

    echo "==> Stapling the ticket into $DMG"
    xcrun stapler staple "$DMG"

    # The release gate: both checks must pass or the artifact must not ship.
    # This is what a user's machine evaluates on first open of the download.
    echo "==> Verifying staple"
    xcrun stapler validate "$DMG"
    echo "==> Gatekeeper assessment"
    spctl -a -t open --context context:primary-signature -vv "$DMG" 2>&1 | sed 's/^/    /'
    spctl -a -t open --context context:primary-signature "$DMG"
fi

echo "==> $DMG ready ($(du -h "$DMG" | cut -f1))"
echo "    open \"$DMG\"    # to inspect the install window"
