#!/usr/bin/env bash
# Mint the release signing keypair — once, ever, per key generation.
#
# The public half is written into the three places that must agree:
#
#   packaging/release-pubkey.txt   compiled into the binary by apps/cli/build.rs
#   scripts/install-cli.sh         the POSIX bootstrap installer
#   scripts/install-cli.ps1        the Windows bootstrap installer
#
# `packaging_key_parity` in apps/cli/tests/update_e2e.rs fails the build if they
# drift, which is the whole reason this is a script and not three manual edits.
#
# The SECRET half is printed once and never written into the repository. It
# belongs in the GitHub repository secret MINISIGN_SECRET_KEY, and nowhere else
# that is backed up, synced, or shared. Losing it means the next release cannot
# be signed and every installed CLI stops accepting updates until users
# reinstall; leaking it means an attacker can sign a release that every
# installed CLI will accept. Read docs/release-signing.md before running this.
#
# The key is generated WITHOUT a password (-W). minisign reads a password from
# the terminal, which a CI runner does not have; the secret's protection is
# GitHub's secret storage, not a passphrase.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KEY_FILE="${ROOT}/packaging/release-pubkey.txt"
SH_INSTALLER="${ROOT}/scripts/install-cli.sh"
PS_INSTALLER="${ROOT}/scripts/install-cli.ps1"

ROTATE=0
OUT_DIR=""

usage() {
    cat <<EOF
Mint the OxiMux release signing keypair.

  --rotate           replace an existing key (read docs/release-signing.md first)
  --out-dir <path>   where to write the secret key (default: a mktemp dir)
  -h, --help         this
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --rotate) ROTATE=1 ;;
        --out-dir) shift; OUT_DIR="${1:?--out-dir needs a path}" ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

command -v minisign >/dev/null 2>&1 || {
    echo "error: minisign is required (brew install minisign)" >&2
    exit 1
}

current="$(grep -v '^[[:space:]]*#' "$KEY_FILE" | grep -v '^[[:space:]]*$' | tail -1 || true)"
if [ "$current" != "UNSET" ] && [ "$ROTATE" -eq 0 ]; then
    cat >&2 <<EOF
error: a release key is already minted:

    ${current}

Rotating it means every already-installed CLI will REFUSE updates signed by the
new key — those users must reinstall. Read docs/release-signing.md, then re-run
with --rotate if that is really what you want.
EOF
    exit 1
fi

if [ -n "$OUT_DIR" ]; then
    mkdir -p "$OUT_DIR"
else
    OUT_DIR="$(mktemp -d)"
fi
SECRET="${OUT_DIR}/oximux-release.key"
PUBLIC="${OUT_DIR}/oximux-release.pub"

[ -e "$SECRET" ] && { echo "error: ${SECRET} already exists; refusing to overwrite" >&2; exit 1; }

minisign -G -W -p "$PUBLIC" -s "$SECRET" >/dev/null
chmod 600 "$SECRET"

# A minisign .pub is two lines: an untrusted comment, then the base64 body.
PUBKEY="$(sed -n '2p' "$PUBLIC")"
[ -n "$PUBKEY" ] || { echo "error: minisign produced no public key body" >&2; exit 1; }

# --- write the public half into all three readers ---------------------------
# Temp file + mv throughout: `sed -i` takes an argument on macOS and not on GNU.

replace_line() {
    # $1 = file, $2 = LITERAL line prefix, $3 = whole replacement line
    #
    # The anchor is a literal, matched with index()==1, rather than a regex.
    # `$ReleasePublicKey` needs escaping to survive a regex, and grep's BRE and
    # awk's dynamic regexes disagree about what `\$` means — which fails in the
    # worst way available: grep finds the line, awk does not replace it, and
    # the script reports success having changed nothing.
    file="$1"; prefix="$2"; line="$3"
    grep -qF -- "$prefix" "$file" || {
        echo "error: no line starting with '${prefix}' in ${file} — did its shape change?" >&2
        exit 1
    }
    # awk rather than sed's s///, because a base64 key can contain `/` and `&`,
    # both of which sed would reinterpret.
    awk -v prefix="$prefix" -v repl="$line" '
        !done && index($0, prefix) == 1 { print repl; done = 1; next }
        { print }
    ' "$file" > "${file}.tmp"
    mv "${file}.tmp" "$file"
}

# packaging/release-pubkey.txt: the key is the last non-comment line.
awk -v key="$PUBKEY" '
    /^[[:space:]]*#/ || /^[[:space:]]*$/ { print; next }
    !done { print key; done = 1; next }
' "$KEY_FILE" > "${KEY_FILE}.tmp"
mv "${KEY_FILE}.tmp" "$KEY_FILE"

replace_line "$SH_INSTALLER" 'RELEASE_PUBKEY=' "RELEASE_PUBKEY=\"${PUBKEY}\""
replace_line "$PS_INSTALLER" '$ReleasePublicKey' "\$ReleasePublicKey = '${PUBKEY}'"

chmod +x "$SH_INSTALLER" 2>/dev/null || true

# Read all three back and prove they agree. The parity test in the Rust suite
# is the durable guard, but it only runs later — a rewrite that silently
# changed nothing must fail HERE, while the secret key is still on screen and
# the mistake is one re-run away.
check() {
    got="$1"; where="$2"
    [ "$got" = "$PUBKEY" ] || {
        echo "error: ${where} did not take the new key (got '${got}') — nothing was published; re-run." >&2
        exit 1
    }
}
check "$(grep -v '^[[:space:]]*#' "$KEY_FILE" | grep -v '^[[:space:]]*$' | tail -1)" "$KEY_FILE"
check "$(sed -n 's/^RELEASE_PUBKEY="\(.*\)"$/\1/p' "$SH_INSTALLER")" "$SH_INSTALLER"
check "$(sed -n "s/^\\\$ReleasePublicKey = '\(.*\)'$/\1/p" "$PS_INSTALLER")" "$PS_INSTALLER"

cat <<EOF

Public key written to all three readers:

    ${PUBKEY}

  packaging/release-pubkey.txt
  scripts/install-cli.sh
  scripts/install-cli.ps1

Commit those three together — a partial commit is what the parity test exists
to catch.

NOW DO THIS, then destroy the file:

  1. Copy the secret key:

       cat ${SECRET}

  2. GitHub → Settings → Secrets and variables → Actions → New repository
     secret, named exactly:

       MINISIGN_SECRET_KEY

  3. Shred the local copy:

       rm -P ${SECRET}

The secret is NOT in the repository and must never be. Anyone holding it can
sign a release that every installed oximux will accept.
EOF
