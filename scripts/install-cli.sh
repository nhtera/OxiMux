#!/bin/sh
# Install the `oximux` CLI and its relay.
#
#   curl -fsSL https://raw.githubusercontent.com/nhtera/OxiMux/main/scripts/install-cli.sh | sh
#
# What this trusts, and what it cannot
# ------------------------------------
# The release manifest is signed with the maintainer's minisign key, and this
# script carries the public half (RELEASE_PUBKEY below). When `minisign` is on
# PATH the signature is checked before any checksum in the manifest is believed
# — that is the whole point of signing it: the manifest and the artifacts come
# from one GitHub Release, so a checksum alone proves only that the download
# matches what the publisher said, which a stolen publish token rewrites.
#
# When `minisign` is absent the script falls back to the manifest's sha256 over
# TLS, says so in as many words, and continues. That is a real reduction in
# trust and the reason `--require-signature` exists; CI uses it. It is not
# silent, and it is not the default failure mode of a one-line install that
# most people run on a machine with no minisign.
#
# The installed binary does NOT inherit this weakness: it carries the same key
# compiled in and every `oximux update` from here on verifies the signature or
# refuses outright. This is trust-on-first-install, and only that.
#
# POSIX sh on purpose — /bin/sh is dash on Debian and Ubuntu.

set -eu

REPO="nhtera/OxiMux"
# Overridable so the installer itself can be tested against a local fake
# release. Unlike the compiled updater — where the equivalent override is
# debug-build-only, because a release binary must carry no way to repoint its
# own trust chain — this costs nothing: anyone who can set an environment
# variable for this script can equally well edit the script.
BASE_URL="${OXIMUX_INSTALL_BASE_URL:-https://github.com/${REPO}/releases}"
LATEST="${BASE_URL}/latest/download"

# release-pubkey: the base64 body of the minisign .pub file. Managed by
# scripts/gen-release-key.sh; packaging/release-pubkey.txt is the source of
# truth and apps/cli/tests/update_e2e.rs fails the build if they drift.
RELEASE_PUBKEY="RWQ4owMUFazkg7fHezLB688BjTGDGJBBQ4EPLVbLDp8baal1VsMJ71FJ"

DEFAULT_DIR="${HOME}/.local/bin"
INSTALL_DIR="${OXIMUX_INSTALL_DIR:-$DEFAULT_DIR}"
REQUIRE_SIGNATURE=0

die() {
    echo "error: $*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

usage() {
    cat <<EOF
Install the oximux CLI.

  --dir <path>           where to install (default: ${DEFAULT_DIR})
  --require-signature    fail unless the manifest signature is verified
  -h, --help             this

Environment: OXIMUX_INSTALL_DIR is the same as --dir.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --dir) shift; [ $# -gt 0 ] || die "--dir needs a path"; INSTALL_DIR="$1" ;;
        --dir=*) INSTALL_DIR="${1#--dir=}" ;;
        --require-signature) REQUIRE_SIGNATURE=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

need uname
need tar
need mkdir
need awk

# curl or wget, whichever the machine has.
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO "$2" "$1"; }
else
    die "neither curl nor wget is installed"
fi

# sha256sum on Linux, shasum on macOS. Both print "<hex>  <path>".
if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
else
    die "no sha256 tool found (looked for sha256sum and shasum)"
fi

# --- the target triple whose asset belongs on this machine ------------------

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) echo "aarch64-apple-darwin" ;;
                x86_64) echo "x86_64-apple-darwin" ;;
                *) die "unsupported macOS architecture: $arch" ;;
            esac
            ;;
        Linux)
            case "$arch" in
                x86_64|amd64) echo "x86_64-unknown-linux-gnu" ;;
                aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
                *) die "unsupported Linux architecture: $arch" ;;
            esac
            ;;
        *) die "unsupported operating system: $os (Windows: use scripts/install-cli.ps1)" ;;
    esac
}

# --- reading the signed manifest --------------------------------------------

# One field of one target's asset. The manifest is compact single-line JSON;
# newlines are stripped first so a re-formatted-but-still-signed manifest is
# read the same way. Only ever run on bytes whose signature already verified.
manifest_field() {
    _file="$1"; _target="$2"; _field="$3"
    tr -d '\n\r\t ' < "$_file" | awk -v target="$_target" -v field="$_field" '
        {
            key = "\"" target "\":{"
            i = index($0, key)
            if (i == 0) { exit 1 }
            rest = substr($0, i + length(key))
            j = index(rest, "}")
            if (j == 0) { exit 1 }
            obj = substr(rest, 1, j - 1)
            fkey = "\"" field "\":"
            k = index(obj, fkey)
            if (k == 0) { exit 1 }
            val = substr(obj, k + length(fkey))
            if (substr(val, 1, 1) == "\"") {
                val = substr(val, 2)
                end = index(val, "\"")
            } else {
                end = index(val, ",")
                if (end == 0) { end = length(val) + 1 }
            }
            print substr(val, 1, end - 1)
        }'
}

# The top-level version, same normalisation.
manifest_version() {
    tr -d '\n\r\t ' < "$1" | awk '
        {
            k = index($0, "\"version\":\"")
            if (k == 0) { exit 1 }
            val = substr($0, k + length("\"version\":\""))
            print substr(val, 1, index(val, "\"") - 1)
        }'
}

TARGET="$(detect_target)"

TMP="$(mktemp -d 2>/dev/null || mktemp -d -t oximux)"
# Set once the staging directory inside the install dir exists; cleaned up on
# every exit path so a failed install leaves nothing behind that a later run
# could mistake for a verified binary.
STAGE=""
cleanup() { rm -rf "$TMP"; [ -n "$STAGE" ] && rm -rf "$STAGE"; return 0; }
trap cleanup EXIT INT TERM

echo "oximux: installing for ${TARGET}"

fetch "${LATEST}/manifest.json" "${TMP}/manifest.json" \
    || die "could not download the release manifest — is there a published release?"

# --- the signature, before anything in the manifest is believed -------------

if [ "$RELEASE_PUBKEY" = "UNSET" ]; then
    if [ "$REQUIRE_SIGNATURE" -eq 1 ]; then
        die "this installer carries no release key, so --require-signature cannot be satisfied"
    fi
    echo "oximux: WARNING — this installer carries no release key; falling back to checksum-only trust." >&2
elif command -v minisign >/dev/null 2>&1; then
    fetch "${LATEST}/manifest.json.minisig" "${TMP}/manifest.json.minisig" \
        || die "the release has no manifest signature"
    printf 'untrusted comment: oximux release key\n%s\n' "$RELEASE_PUBKEY" > "${TMP}/release.pub"
    minisign -V -p "${TMP}/release.pub" -x "${TMP}/manifest.json.minisig" \
        -m "${TMP}/manifest.json" >/dev/null \
        || die "the release manifest signature did NOT verify. Do not retry blindly — this is what a tampered release looks like. Check https://github.com/${REPO}/releases"
    echo "oximux: manifest signature verified"
elif [ "$REQUIRE_SIGNATURE" -eq 1 ]; then
    die "minisign is not installed and --require-signature was given (brew install minisign / apt install minisign)"
else
    echo "oximux: WARNING — minisign is not installed, so the release signature was NOT checked." >&2
    echo "oximux:           Falling back to the manifest's sha256 over TLS. Install minisign and" >&2
    echo "oximux:           re-run with --require-signature for the full check." >&2
fi

# --- the asset ---------------------------------------------------------------

VERSION="$(manifest_version "${TMP}/manifest.json")" \
    || die "the release manifest carries no version"
ARCHIVE="$(manifest_field "${TMP}/manifest.json" "$TARGET" archive)" \
    || die "this release has no build for ${TARGET}"
WANT_SHA="$(manifest_field "${TMP}/manifest.json" "$TARGET" sha256)" \
    || die "the release manifest carries no checksum for ${TARGET}"

# The name becomes a path component below. The Rust parser refuses these too;
# an installer that skipped the check would be the weaker of the two readers.
case "$ARCHIVE" in
    */*|*'\'*|*..*|-*|'') die "the manifest names an unsafe archive path: ${ARCHIVE}" ;;
esac

echo "oximux: downloading ${VERSION} (${ARCHIVE})"
# Built from the *signed* version and file name rather than read out of the
# manifest as a URL, so a manifest can never name a download host of its own.
fetch "${BASE_URL}/download/v${VERSION}/${ARCHIVE}" "${TMP}/${ARCHIVE}" \
    || die "could not download ${ARCHIVE}"

GOT_SHA="$(sha256_of "${TMP}/${ARCHIVE}")"
# Case-insensitive: hex is hex. tr is POSIX; `${var,,}` is a bashism.
GOT_SHA="$(printf '%s' "$GOT_SHA" | tr 'ABCDEF' 'abcdef')"
WANT_SHA="$(printf '%s' "$WANT_SHA" | tr 'ABCDEF' 'abcdef')"
[ "$GOT_SHA" = "$WANT_SHA" ] \
    || die "checksum mismatch for ${ARCHIVE}: expected ${WANT_SHA}, got ${GOT_SHA}"
echo "oximux: checksum ok"

# --- install ----------------------------------------------------------------

mkdir -p "${TMP}/unpack"
tar -xzf "${TMP}/${ARCHIVE}" -C "${TMP}/unpack" || die "could not unpack ${ARCHIVE}"
for bin in oximux oximux-relay; do
    [ -f "${TMP}/unpack/${bin}" ] || die "${ARCHIVE} does not contain ${bin}"
done

mkdir -p "$INSTALL_DIR" || die "could not create ${INSTALL_DIR}"

# Both binaries move together, or neither does.
#
# The CLI and the relay speak a handshake versioned in lockstep: an install that
# lands one and not the other leaves an installation that cannot talk to itself.
# Doing the two moves in a simple loop would produce exactly that whenever the
# second one fails, so this is the same two-pass swap-with-rollback that
# `oximux update` performs, for the same reason.
#
# Staging INSIDE the install directory is load-bearing: a rename is only atomic
# within one filesystem, and /tmp is very often not the same filesystem as
# ~/.local/bin. The cross-filesystem copy happens here, before anything
# installed is disturbed.
STAGE="${INSTALL_DIR}/.oximux-install-$$"
rm -rf "$STAGE"
mkdir -p "$STAGE" || die "could not stage into ${INSTALL_DIR} — no write access?"
for bin in oximux oximux-relay; do
    cp "${TMP}/unpack/${bin}" "${STAGE}/${bin}" \
        || die "could not stage ${bin} into ${INSTALL_DIR} — no write access?"
    chmod 755 "${STAGE}/${bin}"
done

# Pass 1: vacate the installed names. A running binary cannot be overwritten,
# but its name CAN be vacated — which is why this is "move aside, then move in"
# rather than "move over".
moved=""
for bin in oximux oximux-relay; do
    [ -e "${INSTALL_DIR}/${bin}" ] || continue
    if mv "${INSTALL_DIR}/${bin}" "${STAGE}/${bin}.old"; then
        moved="${moved} ${bin}"
    else
        for b in $moved; do
            mv "${STAGE}/${b}.old" "${INSTALL_DIR}/${b}" 2>/dev/null || true
        done
        die "could not replace ${INSTALL_DIR}/${bin} — is it owned by another account?"
    fi
done

# Pass 2: move the new binaries into the vacated names. Undo pass 2 first on
# failure, so pass 1's undo finds its destinations free.
placed=""
for bin in oximux oximux-relay; do
    if mv "${STAGE}/${bin}" "${INSTALL_DIR}/${bin}"; then
        placed="${placed} ${bin}"
    else
        for b in $placed; do rm -f "${INSTALL_DIR}/${b}"; done
        for b in $moved; do
            mv "${STAGE}/${b}.old" "${INSTALL_DIR}/${b}" 2>/dev/null || true
        done
        die "could not install into ${INSTALL_DIR} — no write access?"
    fi
done

echo "oximux: installed ${VERSION} to ${INSTALL_DIR}"

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo
        echo "${INSTALL_DIR} is not on your PATH. Add it:"
        echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile"
        ;;
esac
