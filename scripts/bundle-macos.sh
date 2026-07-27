#!/usr/bin/env bash
#
# Build OxiMux.app for local use. Signs the bundle ad-hoc by default so
# UNUserNotificationCenter (desktop notifications) has a sealed identity;
# set OXIMUX_CODESIGN_IDENTITY (or the legacy OXIMUX_SIGN_ID, or pass
# --sign <identity>) to a real "Apple Development: …" / "Developer ID
# Application: …" identity for a stable TCC identity across rebuilds and
# to make notification authorization grants stick. Pass --notarize to
# sign hardened and submit to Apple.
# Output: dist/OxiMux.app
#
# Usage:
#   ./scripts/bundle-macos.sh                 # release bundle (default)
#   ./scripts/bundle-macos.sh debug           # debug bundle (faster build)
#   ./scripts/bundle-macos.sh --debug-fast    # refresh binary only (~200 ms)
#   ./scripts/bundle-macos.sh --sign "Apple Development: you@example.com (TEAMID)" debug
#   OXIMUX_CODESIGN_IDENTITY="Apple Development: …" ./scripts/bundle-macos.sh
#   ./scripts/bundle-macos.sh --hardened --sign "Developer ID Application: … (TEAMID)"
#   ./scripts/bundle-macos.sh --notarize --sign "Developer ID Application: … (TEAMID)"
#
# --hardened signs with the hardened runtime, a secure timestamp, and
# assets/OxiMux.entitlements — everything notarization requires, without
# submitting. Run the app after this before notarizing: the hardened runtime is
# what breaks an app, and finding that here costs seconds rather than a full
# submission round trip.
#
# --notarize does all of the above, then submits, staples the ticket into the
# bundle, and reports Gatekeeper's verdict. It needs a one-time keychain
# profile (see notarize_bundle below) and a "Developer ID Application"
# certificate — an "Apple Development" identity, which is all a free Apple
# account issues, is refused up front rather than at submission.
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
#
# oximux-screen-gate ships the same way. It is the PreToolUse hook that
# decides an agent's screen-control calls, spawned once per tool call by
# the agent's own CLI. Missing from the bundle, the hook command fails to
# run and the chat drives the screen with nothing enforcing it.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

APP_DIR="dist/OxiMux.app"
ENTITLEMENTS="assets/OxiMux.entitlements"
# Keychain profile name for notarytool credentials; override if you keep more
# than one Apple account on this machine.
NOTARY_PROFILE="${OXIMUX_NOTARY_PROFILE:-oximux-notary}"

# Pull --sign/--sign=<identity> out of the argument list up front so the
# rest of the script's positional parsing (profile / --debug-fast) is
# untouched. Left in ARGS, everything else passes through unchanged.
SIGN_FLAG=""
# Hardened runtime + secure timestamp + entitlements. Required for
# notarization, and the part most likely to break the app at runtime — so it
# is reachable on its own, without a submission, to keep that failure 30
# seconds away instead of a round trip away.
HARDENED=0
NOTARIZE=0
ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --hardened)
            HARDENED=1
            shift
            ;;
        --notarize)
            HARDENED=1
            NOTARIZE=1
            shift
            ;;
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

SIGN_ID="${SIGN_FLAG:-${OXIMUX_CODESIGN_IDENTITY:-${OXIMUX_SIGN_ID:--}}}"


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
    local sign_id="$SIGN_ID"
    local opts=(--force -s "$sign_id")
    if [[ "$HARDENED" -eq 1 ]]; then
        # `--timestamp` contacts Apple's timestamp server, so this needs a
        # network. A bundle signed without one is accepted by codesign and
        # then rejected by notarization, long after the fact.
        opts+=(--options runtime --timestamp --entitlements "$ENTITLEMENTS")
    fi
    # Sign the bundled dylibs (voice-dictation's onnxruntime + sherpa-onnx)
    # before the executables so the nested-first seal order holds. The guard
    # skips the literal glob when no dylibs are present.
    #
    # `-f && ! -L` and not `-e`: `cp -a` above deliberately preserves the
    # versionless symlink (libonnxruntime.dylib -> libonnxruntime.1.17.1.dylib)
    # because the dictation engine links against that name. `-e` follows the
    # link and is therefore true for it, but codesign refuses a symlink with
    # "Operation not permitted", which aborts the whole script under `set -e`.
    # Signing the real file is sufficient; the link resolves to it.
    for dylib in "$APP_DIR/Contents/MacOS"/*.dylib; do
        [[ -f "$dylib" && ! -L "$dylib" ]] && codesign "${opts[@]}" "$dylib"
    done
    codesign "${opts[@]}" "$APP_DIR/Contents/MacOS/oximux-relay"
    codesign "${opts[@]}" "$APP_DIR/Contents/MacOS/oximux-screen-gate"
    codesign "${opts[@]}" "$APP_DIR/Contents/MacOS/oximux"
    codesign "${opts[@]}" "$APP_DIR"
    echo "==> Signed $APP_DIR (identity: $sign_id)"
    # Only the opt-in real-identity path pays for verification, so the
    # default ad-hoc dev loop is untouched (same commands, same output).
    if [[ "$sign_id" != "-" ]]; then
        verify_signature "$sign_id"
    fi
}

# The keychain profile has to exist before a build starts, for the same reason
# the certificate check does: it is a one-time setup step, and finding it
# missing after a release build and a zip wastes the whole run.
require_notary_profile() {
    if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
        echo "error: no notarytool keychain profile named '$NOTARY_PROFILE'." >&2
        echo "       Create it once (it prompts for an app-specific password," >&2
        echo "       which is stored in the keychain and never appears here):" >&2
        echo "" >&2
        echo "         xcrun notarytool store-credentials \"$NOTARY_PROFILE\" \\" >&2
        echo "           --apple-id <your-apple-id> --team-id <TEAMID>" >&2
        echo "" >&2
        echo "       Generate the app-specific password at appleid.apple.com →" >&2
        echo "       Sign-In and Security → App-Specific Passwords. The account" >&2
        echo "       password itself is refused." >&2
        exit 2
    fi
}

# Notarization accepts exactly one certificate type. "Apple Development" —
# which is what a free Apple account issues — signs and launches locally and is
# then rejected at submission, so it is caught here instead of after a build
# and an upload.
require_developer_id() {
    local sign_id="$1"
    if [[ "$sign_id" == "-" ]]; then
        echo "error: --hardened/--notarize needs a real signing identity." >&2
        echo "       Pass --sign \"Developer ID Application: … (TEAMID)\"" >&2
        echo "       or set OXIMUX_CODESIGN_IDENTITY." >&2
        exit 2
    fi
    if [[ "$sign_id" != "Developer ID Application:"* ]]; then
        echo "error: notarization requires a 'Developer ID Application' certificate;" >&2
        echo "       got '$sign_id'." >&2
        echo "       An 'Apple Development' identity (what a free Apple account" >&2
        echo "       issues) signs fine locally and is rejected at submission." >&2
        echo "       Create one in Xcode → Settings → Accounts → Manage" >&2
        echo "       Certificates → + → Developer ID Application. Only the" >&2
        echo "       team's Account Holder can issue it." >&2
        exit 2
    fi
    if [[ ! -f "$ENTITLEMENTS" ]]; then
        echo "error: $ENTITLEMENTS is missing; a hardened build without it" >&2
        echo "       would strip the microphone permission and break dictation" >&2
        echo "       silently." >&2
        exit 2
    fi
}

# Submit the signed bundle and staple the ticket into it.
#
# Credentials come from a keychain profile created once by hand:
#
#   xcrun notarytool store-credentials "$NOTARY_PROFILE" \
#     --apple-id <your-apple-id> --team-id <TEAMID>
#
# It prompts for an app-specific password (appleid.apple.com → Sign-In and
# Security → App-Specific Passwords; the account password is refused). Keeping
# it in the keychain is why no secret appears in this script, in the shell
# history, or in CI logs.
notarize_bundle() {
    local zip="dist/OxiMux-notarize.zip"
    echo "==> Zipping $APP_DIR for submission"
    # `ditto -c -k --keepParent` is the required shape: it preserves the bundle
    # structure and symlinks. A plain `zip` mangles both and the submission is
    # rejected for a malformed bundle.
    rm -f "$zip"
    ditto -c -k --keepParent "$APP_DIR" "$zip"

    echo "==> Submitting to Apple (this blocks until the verdict; usually 1-5 min)"
    if ! xcrun notarytool submit "$zip" --keychain-profile "$NOTARY_PROFILE" --wait; then
        echo "error: notarization failed. For the per-issue detail run:" >&2
        echo "       xcrun notarytool log <submission-id> --keychain-profile $NOTARY_PROFILE" >&2
        echo "       (the submission id is in the output above)" >&2
        exit 1
    fi

    # Staple the .app, not the zip: the zip was only transport. Stapling embeds
    # the ticket so the bundle validates on a machine that is offline or cannot
    # reach Apple.
    echo "==> Stapling the ticket into $APP_DIR"
    xcrun stapler staple "$APP_DIR"
    rm -f "$zip"

    # The verdict that matters is Gatekeeper's, not codesign's — this is what a
    # user's machine actually evaluates on first launch.
    echo "==> Gatekeeper assessment"
    spctl -a -vvv -t install "$APP_DIR" 2>&1 | sed 's/^/    /'
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

# Copy the voice-dictation engine's dynamic dylibs into the bundle and add an
# `@executable_path` rpath so they resolve at launch. sherpa-rs builds
# onnxruntime + the sherpa C-API as *dynamic* libraries under
# `target/<profile>/`; `cargo run`/`cargo test` locate them via an injected
# DYLD_FALLBACK_LIBRARY_PATH, but a launched `.app` has no such path — without
# this the bundle crashes on launch resolving `@rpath/libonnxruntime.*.dylib`.
# `$1` = the target subdir (`debug` / `release`).
bundle_dylibs() {
    local src="target/$1"
    local macos="$APP_DIR/Contents/MacOS"
    local found=0
    shopt -s nullglob
    for dylib in "$src"/libonnxruntime*.dylib "$src"/libsherpa-onnx*.dylib; do
        # `-a` preserves the versionless symlink alongside the real dylib.
        cp -a "$dylib" "$macos/"
        found=1
    done
    shopt -u nullglob
    if [[ "$found" -eq 0 ]]; then
        echo "error: no onnxruntime/sherpa dylibs found in $src." >&2
        echo "       The dictation engine links them dynamically, so the bundle" >&2
        echo "       would crash on launch. Run 'cargo build -p oximux-app' first." >&2
        exit 1
    fi
    # Add the rpath only if absent — install_name_tool errors on a duplicate.
    if ! otool -l "$macos/oximux" | grep -qE '^\s*path @executable_path \(offset'; then
        install_name_tool -add_rpath @executable_path "$macos/oximux"
    fi
    echo "==> Bundled dictation dylibs + @executable_path rpath"
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
    if [[ -f "target/debug/oximux-screen-gate" ]]; then
        cp -f "target/debug/oximux-screen-gate" "$APP_DIR/Contents/MacOS/oximux-screen-gate"
    fi
    # Keep Info.plist + app icon in sync too, so a fast refresh produces a
    # complete bundle (correct menu-bar name + Dock icon), not just binaries.
    cp -f "assets/Info.plist" "$APP_DIR/Contents/Info.plist"
    if [[ -f "assets/AppIcon.icns" ]]; then
        mkdir -p "$APP_DIR/Contents/Resources"
        cp -f "assets/AppIcon.icns" "$APP_DIR/Contents/Resources/AppIcon.icns"
    fi
    # The fresh binary carries no rpath, so re-copy the dylibs + re-add it.
    bundle_dylibs debug
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

# Validate a hardened/notarize request BEFORE building. Both checks are cheap
# and the build is not: "wrong certificate type" discovered after a full
# release build is the same information delivered several minutes later. This
# sits below the function definitions because bash resolves a call at run time,
# not at parse time — hoisting the checks above them fails with
# "command not found" instead of the message they exist to print.
if [[ "$HARDENED" -eq 1 ]]; then
    require_developer_id "$SIGN_ID"
fi
if [[ "$NOTARIZE" -eq 1 ]]; then
    require_notary_profile
fi

echo "==> Building oximux + oximux-relay + oximux-screen-gate ($PROFILE)"
# `${CARGO_FLAGS[@]+...}` guards the expansion so an empty array (debug
# profile) doesn't trip `set -u` ("unbound variable") on bash < 4.4.
cargo build -p oximux-app --bin oximux ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}
cargo build -p oximux-relay --bin oximux-relay ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}
cargo build -p oximux-computer-use --bin oximux-screen-gate ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}

echo "==> Assembling $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

cp "target/$TARGET_SUBDIR/oximux" "$APP_DIR/Contents/MacOS/oximux"
cp "target/$TARGET_SUBDIR/oximux-relay" "$APP_DIR/Contents/MacOS/oximux-relay"
cp "target/$TARGET_SUBDIR/oximux-screen-gate" "$APP_DIR/Contents/MacOS/oximux-screen-gate"
cp "assets/Info.plist" "$APP_DIR/Contents/Info.plist"

# App icon — charcoal rounded tile + terminal prompt glyph (matches the
# in-app welcome-view brand). Regenerate from assets/AppIcon.icns.
if [[ -f "assets/AppIcon.icns" ]]; then
    cp "assets/AppIcon.icns" "$APP_DIR/Contents/Resources/AppIcon.icns"
fi

# Voice-dictation runtime dylibs (onnxruntime + sherpa-onnx) + rpath.
bundle_dylibs "$TARGET_SUBDIR"

sign_bundle

if [[ "$NOTARIZE" -eq 1 ]]; then
    notarize_bundle
fi

echo "==> $APP_DIR ready ($(du -sh "$APP_DIR" | cut -f1))"
echo "    open $APP_DIR    # to launch"
