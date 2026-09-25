# Deployment Guide

How an OxiMux release gets built, signed, notarized, and published. The user-facing
artifact is a styled DMG (`OxiMux-<version>-macos-arm64.dmg`) whose Finder window
shows the app icon, an Applications alias, and a drag arrow.

The asset name is load-bearing beyond distribution: the in-app auto-updater
(`crates/auto-update/src/feed.rs`) pins the exact filename
`OxiMux-{version}-macos-arm64.dmg` when resolving a release's download URL.
Renaming the DMG artifact breaks auto-update for every install until the
updater is changed to match.

**The Windows zip is load-bearing the same way**, by a different route. The
Windows updater downloads `OxiMux-<version>-windows-x64.zip` — *not* the
installer `.exe`, which is for humans — and refuses it unless its sha256
matches the signed `manifest.json`. The name is written by
`scripts/bundle-windows.ps1` and read back by the `APP_TRIPLE` map in
`release.yml`'s manifest job. Those two must agree; the job now fails loudly
rather than publishing a manifest with no Windows payload, because that
failure is otherwise silent — every client would simply go on reporting "no
app build for your platform".

## Artifacts & scripts

| Piece | Role |
|---|---|
| `scripts/bundle-macos.sh` | Build + assemble + sign `dist/OxiMux.app` (`--hardened` for release) |
| `scripts/make-dmg.sh` | Package the app into the styled DMG; `--notarize` submits, staples, and Gatekeeper-gates it |
| `scripts/generate-dmg-background.swift` | Regenerates `assets/dmg-background.tiff` (HiDPI, 660x400) |
| `assets/dmg-background.tiff` | DMG window backdrop — its arrow position and `make-dmg.sh`'s icon coordinates are ONE layout; change them together |
| `.github/workflows/release.yml` | CI: tag push → build → sign → DMG → notarize → draft GitHub release |

Notarization is submitted ONCE, on the DMG: Apple recursively notarizes the app
inside, and the ticket staples to the DMG users actually download. The older
`bundle-macos.sh --notarize` path (bare .app via zip transport) remains for
direct-zip distribution only — don't run both for a DMG release.

## Cutting a release

1. Bump `version` in `Cargo.toml` (workspace) and both version keys in
   `assets/Info.plist`; run `cargo check` to refresh `Cargo.lock`.
2. Add the changelog section in `docs/project-changelog.md`.
3. Commit `chore(release): vX.Y.Z`, tag `vX.Y.Z`, push both.
4. The `release` workflow builds, signs, notarizes, and attaches the DMG to a
   **draft** release with generated notes.
5. Curate the release notes, publish the draft.

The workflow hard-fails (in `make-dmg.sh`) on `stapler validate` and
`spctl -a -t open --context context:primary-signature` — an un-notarized DMG
can never reach the release.

Re-run path: `workflow_dispatch` on the tag's ref — Apple's notary service
occasionally stalls; re-running is the fix, not re-tagging. Uploads use
`--clobber`, so re-runs safely replace the asset.

## Local release (fallback when CI is unavailable)

```bash
./scripts/bundle-macos.sh --hardened --sign "Developer ID Application: <name> (<TEAMID>)"
./scripts/make-dmg.sh --sign "Developer ID Application: <name> (<TEAMID>)" --notarize
gh release create vX.Y.Z dist/OxiMux-X.Y.Z-macos-arm64.dmg --title vX.Y.Z --notes-file <notes>
```

Local notarization credentials live in the `oximux-notary` keychain profile
(`xcrun notarytool store-credentials`). CI uses an App Store Connect API key
via env vars instead (`NOTARY_KEY_PATH`/`NOTARY_KEY_ID`/`NOTARY_ISSUER_ID`) —
`make-dmg.sh` prefers the env vars when set.

## One-time CI secrets setup

Repository → Settings → Secrets and variables → Actions:

| Secret | Value |
|---|---|
| `MACOS_CERT_P12_BASE64` | Developer ID Application identity exported from Keychain Access as .p12, then `base64 -i cert.p12 \| pbcopy` |
| `MACOS_CERT_PASSWORD` | password chosen at .p12 export |
| `NOTARY_KEY_P8` | contents of `AuthKey_<KEYID>.p8` (App Store Connect → Users and Access → Integrations → App Store Connect API; Developer role or above) |
| `NOTARY_KEY_ID` | key id from that page |
| `NOTARY_ISSUER_ID` | issuer id from that page |

The signing identity NAME is not a secret — the workflow derives it from the
imported certificate.

Export the .p12 from Keychain Access: My Certificates → right-click
"Developer ID Application: …" → Export, format ".p12", set a password. Both
the certificate and its private key must be present (the expando arrow shows
a key under the cert).

## Gatekeeper facts worth remembering

- Sign inside-out, never `--deep`: dylibs → `oximux-sim-helper` → `rg` →
  `oximux-relay` → `oximux-screen-gate` → `oximux` → the bundle.
  `bundle-macos.sh` encodes this order.
- Hardened runtime (`--options runtime`) and a secure timestamp are notarization requirements for every binary.
  - Our own binaries also get `assets/OxiMux.entitlements`, which is currently mic-only.
  - `oximux-sim-helper` deliberately gets **no** entitlements. It only dlopens Apple-signed simulator frameworks, and the app's grants must not extend to private-framework code.
- "Apple Development" certs sign locally but are REJECTED at notarization;
  only "Developer ID Application" works. Both scripts check this up front.
- Notary wait is usually 1–15 min; occasionally much longer. The service keeps
  processing past a client timeout — check `xcrun notarytool history` before
  resubmitting.

## iOS Simulator helper (`oximux-sim-helper`)

The helper isn't built in this repo. Our fork [nhtera/serve-sim](https://github.com/nhtera/serve-sim), branch `oximux`, builds and releases it. `scripts/fetch-sim-helper.sh` downloads a pinned release into `target/bundle-tools/` and checks it against the sha256 pinned in the script, and `bundle-macos.sh` bundles it.

To ship a new helper version:
1. In the fork: `git fetch upstream && git rebase upstream/main oximux`, then update `oximux/upstream-base`.
2. Still in the fork: bump `helperVersion` in `oximux/Sources/oximux-sim-helper/Version.swift`, run `swift test`, and push.
3. Push the tag `helper-v<version>`. The tag must be on `oximux`. CI publishes an immutable release with a `.sha256` and a build-provenance attestation. Check it with `gh attestation verify <tarball> --repo nhtera/serve-sim`.
4. In OxiMux: set `SIM_HELPER_VERSION` and `SIM_HELPER_SHA256` in `scripts/fetch-sim-helper.sh`, and compute the sha yourself from the downloaded asset. If the stdio protocol changed, update `crates/simulator` in the same PR.

To test against a locally built fork without a release, set `OXIMUX_SIM_HELPER` to the helper's path.
