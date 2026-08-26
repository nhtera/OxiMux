//! Compiles the release signing key into the crate that verifies with it.
//!
//! The key is read from `packaging/release-pubkey.txt` rather than written here
//! as a literal, because several places must carry the same key and a rotation
//! that updates one and not the others leaves something trusting a retired key.
//! One file, several readers, and tests on both sides that assert they agree —
//! see the header of that file for the full list.

use std::path::PathBuf;

fn main() {
    // TARGET is the triple cargo built for — the cross-compile-correct answer,
    // where anything derived from the host would name the wrong asset in a
    // release manifest.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=OXIMUX_TARGET={target}");

    let key_file = workspace_root().join("packaging/release-pubkey.txt");
    println!("cargo:rerun-if-changed={}", key_file.display());
    println!("cargo:rustc-env=OXIMUX_RELEASE_PUBKEY={}", release_pubkey(&key_file));
}

/// `crates/auto-update` → the workspace root. Two levels up, not a search: a
/// search could silently pick up a different checkout's key file.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or(manifest)
}

/// The last non-comment, non-blank line of the key file. Missing or malformed
/// reads as `UNSET`, which every updater treats as "this build cannot verify
/// anything" and refuses on — never as "verification is optional".
fn release_pubkey(path: &std::path::Path) -> String {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return "UNSET".into();
    };
    raw.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("UNSET")
        .to_string()
}
