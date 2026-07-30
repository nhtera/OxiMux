#!/usr/bin/env bash
#
# Fetch a pinned, checksum-verified ripgrep binary for bundling into
# OxiMux.app. The packaged app spawns `rg` for the search panel and Quick
# Open; bundling it means a fresh machine needs no manual install.
#
# ripgrep is MIT/Unlicense dual-licensed — redistribution inside the app
# bundle is permitted (VS Code ships rg the same way).
#
# Output: target/bundle-tools/rg (+ rg.version stamp)
#
# Usage:
#   ./scripts/fetch-ripgrep.sh                 # arch(es) from OXIMUX_RG_ARCHS or host
#   OXIMUX_RG_ARCHS="aarch64 x86_64" ./scripts/fetch-ripgrep.sh   # universal (lipo)
#
# The app is built native-arch (plain `cargo build`), so the default is the
# host arch only. Pass both archs solely for a universal app build.
#
# Caching: if target/bundle-tools/rg exists and rg.version matches the pinned
# version + arch list, this is a no-op — so offline rebuilds keep working once
# the tool has been fetched. A checksum mismatch is always fatal.
set -euo pipefail

cd "$(dirname "$0")/.."

RG_VERSION="15.2.0"
BASE_URL="https://github.com/BurntSushi/ripgrep/releases/download/${RG_VERSION}"

OUT_DIR="target/bundle-tools"
OUT_BIN="$OUT_DIR/rg"
STAMP="$OUT_DIR/rg.version"

# Map uname arch to ripgrep's release-triple arch component.
host_arch() {
    case "$(uname -m)" in
        arm64|aarch64) echo "aarch64" ;;
        x86_64)        echo "x86_64" ;;
        *)
            echo "error: unsupported host arch '$(uname -m)' for bundled rg" >&2
            exit 2
            ;;
    esac
}

ARCHS="${OXIMUX_RG_ARCHS:-$(host_arch)}"
if [[ -z "${ARCHS// /}" ]]; then
    # An explicitly-empty override would otherwise reach lipo with a zero-length
    # array, which macOS bash 3.2 rejects under `set -u`.
    echo "error: OXIMUX_RG_ARCHS is set but empty" >&2
    exit 2
fi
WANT="ripgrep ${RG_VERSION} [${ARCHS}]"

if [[ -x "$OUT_BIN" && -f "$STAMP" && "$(cat "$STAMP")" == "$WANT" ]]; then
    echo "==> Bundled rg up to date ($WANT), skipping fetch"
    exit 0
fi

mkdir -p "$OUT_DIR"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/oximux-rg.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

EXTRACTED=()
for arch in $ARCHS; do
    name="ripgrep-${RG_VERSION}-${arch}-apple-darwin"
    tarball="${name}.tar.gz"
    echo "==> Fetching $tarball"
    curl -fsSL --retry 3 -o "$WORK/$tarball" "$BASE_URL/$tarball"
    curl -fsSL --retry 3 -o "$WORK/$tarball.sha256" "$BASE_URL/$tarball.sha256"

    # The .sha256 asset is "<hex>  <filename>" — shasum -c needs cwd to hold
    # the file under that exact name. A mismatch (truncated download, tampered
    # artifact) must abort the build, not ship a bad search binary.
    (cd "$WORK" && shasum -a 256 -c "$tarball.sha256")

    tar -xzf "$WORK/$tarball" -C "$WORK"
    if [[ ! -f "$WORK/$name/rg" ]]; then
        echo "error: $tarball did not contain $name/rg — release layout changed?" >&2
        exit 1
    fi
    EXTRACTED+=("$WORK/$name/rg")
done

if [[ "${#EXTRACTED[@]}" -eq 1 ]]; then
    cp -f "${EXTRACTED[0]}" "$OUT_BIN"
else
    lipo -create "${EXTRACTED[@]}" -output "$OUT_BIN"
fi
chmod 755 "$OUT_BIN"
echo "$WANT" > "$STAMP"
echo "==> $OUT_BIN ready ($WANT)"
