//! The self-update trust root, checked against the files that carry it.
//!
//! The update *pipeline* is tested in-process in `src/update/tests.rs`, where
//! the fetcher seam lets a tampered release be built by hand. What cannot be
//! tested there is the thing this file exists for: the release key is written
//! into three separate files, read by three different programs, and a rotation
//! that updates one and not the others produces an installation whose updater
//! trusts a key nothing signs with — or worse, an installer that still trusts a
//! retired one.
//!
//! `scripts/gen-release-key.sh` writes all three and verifies its own work.
//! This is the durable guard behind it, so a hand-edit cannot drift either.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // apps/cli → the workspace root. Two levels up, not a search: a search
    // could pick up a different checkout entirely.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("apps/cli sits two levels below the workspace root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = workspace_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} must be readable: {err}", path.display()))
}

/// The last non-comment, non-blank line — the same rule `apps/cli/build.rs`
/// applies when it compiles the key in. Duplicated deliberately: if build.rs
/// changes how it reads the file, this test must be made to agree on purpose
/// rather than inherit the change and keep passing.
fn key_from_packaging(raw: &str) -> &str {
    raw.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with('#'))
        .expect("packaging/release-pubkey.txt carries a key line")
}

/// The value of the one line starting with `prefix`, stripped of `quote` at
/// both ends.
fn key_from_installer(raw: &str, prefix: &str, quote: char) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line starting with {prefix:?} — did the installer change shape?"));
    line[prefix.len()..]
        .trim()
        .trim_matches(quote)
        .to_string()
}

fn packaging_key() -> String {
    key_from_packaging(&read("packaging/release-pubkey.txt")).to_string()
}

/// The three readers of the release key must agree, always.
///
/// A drift here is not a cosmetic inconsistency: whichever file is stale makes
/// its reader trust the wrong key, and the failure surfaces to a user as an
/// update that cannot be verified — or, in the retired-key direction, as one
/// that verifies when it should not.
#[test]
fn packaging_key_parity() {
    let packaging = packaging_key();
    let shell = key_from_installer(&read("scripts/install-cli.sh"), "RELEASE_PUBKEY=", '"');
    let powershell =
        key_from_installer(&read("scripts/install-cli.ps1"), "$ReleasePublicKey =", '\'');

    assert_eq!(
        packaging, shell,
        "scripts/install-cli.sh carries a different release key than \
         packaging/release-pubkey.txt — run scripts/gen-release-key.sh, or fix the stale one"
    );
    assert_eq!(
        packaging, powershell,
        "scripts/install-cli.ps1 carries a different release key than \
         packaging/release-pubkey.txt — run scripts/gen-release-key.sh, or fix the stale one"
    );
}

/// Whatever the key is, it must be either the explicit "no key" sentinel or
/// something minisign would actually accept. The failure this catches is a
/// truncated or half-pasted key: 40 characters of valid base64 is not a key,
/// but it is not obviously *not* one either, and every check downstream would
/// report it as a signature failure rather than as the paste error it is.
#[test]
fn the_release_key_is_either_unset_or_a_real_minisign_key() {
    let key = packaging_key();
    if key == "UNSET" {
        return;
    }
    assert!(
        key.starts_with("RW"),
        "a minisign public key body starts with RW (Ed25519); found {key:?}"
    );
    // 2-byte algorithm id + 8-byte key id + 32-byte key = 42 bytes → 56 base64
    // characters, no padding.
    assert_eq!(key.len(), 56, "a minisign public key body is 56 base64 characters; found {key:?}");
    assert!(
        key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='),
        "the release key is not base64: {key:?}"
    );
}

/// The installers are the bootstrap, so they are what a user runs before there
/// is any verified binary to run. Both must keep the escape hatch that makes
/// the weaker trust path a deliberate choice rather than a silent default.
#[test]
fn both_installers_can_be_made_to_require_a_signature() {
    let shell = read("scripts/install-cli.sh");
    assert!(shell.contains("--require-signature"), "install-cli.sh lost --require-signature");
    assert!(
        shell.contains("minisign -V"),
        "install-cli.sh no longer verifies the manifest signature at all"
    );

    let powershell = read("scripts/install-cli.ps1");
    assert!(
        powershell.contains("RequireSignature"),
        "install-cli.ps1 lost -RequireSignature"
    );
    assert!(
        powershell.contains("-V -p"),
        "install-cli.ps1 no longer verifies the manifest signature at all"
    );
}
