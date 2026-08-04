//! Build-time facts the binary reports and its self-update needs.
//!
//! Four values, none of which the source can know on its own: the target
//! triple this binary was built for (which asset in a release manifest is
//! *its* asset), the release channel, the commit it came from, and the
//! maintainer public key its updater verifies manifests against.
//!
//! The public key is read from a committed file rather than written here as a
//! literal, because the two shell installers must carry the same key and a
//! rotation that updates one place and not the others would leave an
//! installer trusting a retired key. One file, three readers, one test that
//! asserts they agree.

use std::path::PathBuf;

fn main() {
    // TARGET is the triple cargo built for — the cross-compile-correct answer,
    // where anything derived from the host would name the wrong asset.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=OXIMUX_TARGET={target}");

    // Release builds are `stable` unless the release workflow says otherwise;
    // a developer's own build says `dev` so `oximux version` never claims to
    // be something a user could have downloaded.
    println!("cargo:rerun-if-env-changed=OXIMUX_BUILD_CHANNEL");
    let channel = std::env::var("OXIMUX_BUILD_CHANNEL").unwrap_or_else(|_| {
        match std::env::var("PROFILE").as_deref() {
            Ok("release") => "stable".into(),
            _ => "dev".into(),
        }
    });
    println!("cargo:rustc-env=OXIMUX_BUILD_CHANNEL={channel}");

    println!("cargo:rustc-env=OXIMUX_GIT_SHA={}", git_sha());

    let key_file = workspace_root().join("packaging/release-pubkey.txt");
    println!("cargo:rerun-if-changed={}", key_file.display());
    println!("cargo:rustc-env=OXIMUX_RELEASE_PUBKEY={}", release_pubkey(&key_file));
}

/// `apps/cli` → the workspace root. Two levels up, not a search: a search
/// could silently pick up a different checkout's key file.
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
/// reads as `UNSET`, which the updater treats as "this build cannot verify
/// anything" and refuses on — never as "verification is optional".
fn release_pubkey(path: &std::path::Path) -> String {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return "UNSET".into();
    };
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .next_back()
        .unwrap_or("UNSET")
        .to_string()
}

/// The commit, when there is one to read. A tarball build (no `.git`, no git
/// binary) reports `unknown` rather than failing the build — provenance is
/// nice to print, not a reason to be unbuildable.
fn git_sha() -> String {
    // Not `rerun-if-changed=.git/HEAD`: that re-runs the script on every
    // branch switch for a string that only decorates `oximux version`.
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(workspace_root())
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if sha.is_empty() { "unknown".into() } else { sha }
        }
        _ => "unknown".into(),
    }
}
