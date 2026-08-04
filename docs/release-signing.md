# Release signing

`oximux update` replaces the binaries on a user's machine. What makes that safe
is one signature, and this document is how that signature is created, stored,
and — if it ever has to be — replaced.

## Why the manifest is signed at all

The release manifest carries a sha256 for every archive. On its own that proves
nothing useful: the manifest and the archives are attached to the same GitHub
Release, so anything able to rewrite one can rewrite the other. A stolen publish
token defeats checksum-only trust completely.

The signature is checked against a key **compiled into the binary**, which a
publish token cannot reach. That is the whole trust chain, and it is why the
order in `apps/cli/src/update/mod.rs` is not negotiable:

```
minisign signature over manifest.json   ← the only independent trust root
  └─ manifest parsed (never before)
       └─ version strictly greater than the running one
            └─ archive fetched, sha256 checked against the signed manifest
                 └─ extracted, platform gate, paired swap
```

A build with no key compiled in refuses to update at all. It never falls back to
checksum-only trust — that fallback is the exact thing the signature exists to
replace.

## The three files that carry the public key

| File | Read by |
|---|---|
| `packaging/release-pubkey.txt` | `apps/cli/build.rs`, which compiles it into the binary |
| `scripts/install-cli.sh` | the POSIX bootstrap installer |
| `scripts/install-cli.ps1` | the Windows bootstrap installer |

They must be byte-identical. `packaging_key_parity` in
`apps/cli/tests/update_e2e.rs` fails the build if they drift, because a stale
installer trusting a retired key is a silent hole and a stale
`release-pubkey.txt` is a fleet that cannot update.

Never edit the three by hand. `scripts/gen-release-key.sh` writes all three and
verifies its own work.

## Minting the key (once)

```bash
scripts/gen-release-key.sh
```

It refuses to run if a key already exists — rotation is a separate, deliberate
act (below). It prints the secret key once and writes it nowhere in the repo.

Then, in this order:

1. Copy the secret into the repository secret **`MINISIGN_SECRET_KEY`**
   (Settings → Secrets and variables → Actions).
2. Shred the local copy: `rm -P <path printed by the script>`.
3. Commit the three public-key files **together**. A partial commit is what the
   parity test exists to catch.

The key is generated unencrypted (`minisign -G -W`). minisign reads a passphrase
from a terminal and a CI runner has none; the secret's protection is GitHub's
secret storage, not a passphrase. Treat the secret accordingly: it is not
something to keep in a password manager's "shared" vault, a chat message, or a
backup that syncs.

## What the release workflow does with it

`release-manifest` in `.github/workflows/release.yml`:

1. Refuses to run if `MINISIGN_SECRET_KEY` is unset, or if
   `packaging/release-pubkey.txt` still says `UNSET`. An unsigned manifest is
   not a degraded release; it is a release nobody can update to.
2. Builds `manifest.json` from the archives every `release-cli` matrix leg
   produced.
3. Signs it.
4. **Verifies the signature against the committed public key** before uploading.
   If the CI secret and the repo's public key have drifted, this fails the build
   instead of publishing a release every client will reject.
5. Uploads the archives first and the manifest last, so there is no window where
   a verified manifest points at archives that are not attached yet.

## Rotation

Rotating means **every already-installed CLI stops accepting updates.** Those
users must reinstall by hand. There is no key-transition mechanism — adding one
would mean trusting the old key to introduce the new one, which is precisely
the trust the rotation is trying to withdraw.

So rotate only when the secret is known or suspected to be exposed.

```bash
scripts/gen-release-key.sh --rotate
```

Then: update `MINISIGN_SECRET_KEY`, commit the three files, cut a release, and
tell users to reinstall via the bootstrap installer.

### If the secret leaks

Assume anything signed with it is untrusted from the moment of exposure.

1. Rotate immediately (above).
2. Delete or unpublish any release whose manifest was signed after the exposure
   and that you did not produce.
3. Publish a new release with the new key.
4. Say so publicly, with the affected version range. Users who updated in the
   window cannot tell from their machine whether they got a genuine build; only
   you can tell them what to look for.

Note what rotation does **not** do: it cannot reach a machine that already
installed a malicious binary. Rotation closes the door; it does not undo what
came through it.

## Bootstrap installers, and their weaker guarantee

The installers run before there is any verified binary, so they are
trust-on-first-install by construction.

Both verify the manifest signature when `minisign` is on PATH, and otherwise
fall back to the manifest's sha256 over TLS — loudly, on stderr, never
silently. `--require-signature` (`-RequireSignature` on Windows) turns that
fallback into a failure.

This is a real reduction in trust for that one operation. It is the default
because the alternative is a one-line install that fails on most machines, and
because the binary it installs does **not** inherit the weakness: it carries the
key compiled in and every update from then on is verified or refused.

Windows has no Ed25519 verifier in the box — .NET exposes none — so on Windows
there is no way to check the signature without minisign installed.

## Homebrew

`scripts/gen-homebrew-formula.sh` generates the formula from the signed
manifest, and the release workflow attaches `oximux.rb` to the release.
Publishing it to a tap is a manual copy for now.

Homebrew has no notion of the minisign signature. What it inherits is that the
digests in the formula were signed at release time rather than recomputed later
from whatever the URL currently serves.

`oximux update` refuses to touch a Homebrew-managed install — it detects the
Cellar path and says `brew upgrade oximux` instead. Two things owning one set of
files is a state neither can reason about.

## The macOS team pin

On macOS the updater additionally requires that a binary signed by a real
Developer ID team is only ever replaced by a binary from the **same** team.

This is additive to the signature, never a substitute, and it covers the one
case a minisign signature cannot: a misused release key. Its cost is bounded —
an ad-hoc signature (what every local `cargo build` produces) pins nothing and
passes, because pinning to it would be a gate any attacker satisfies while
breaking every developer's own build.

It only means anything if the released CLI binaries are actually Developer
ID-signed, which is why `release-cli` signs them when the certificate secrets
are configured. Without those secrets the release still builds, and that gate
is a no-op.
