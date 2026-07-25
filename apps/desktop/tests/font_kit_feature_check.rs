//! CI guard for the v0.9 failure mode where `gpui_platform`'s `font-kit`
//! feature got silently disabled and the macOS build shipped without any
//! glyph fallback. This test asserts that `font-kit` is present in
//! `Cargo.lock`'s resolved dependency graph — which is only possible when
//! the feature is enabled somewhere.
//!
//! If this test ever fails on macOS, do NOT delete it. The fix is to add
//! `features = ["font-kit"]` back to `gpui_platform` in the workspace
//! `Cargo.toml`.

#![cfg(target_os = "macos")]

use std::path::PathBuf;

#[test]
fn font_kit_is_in_lockfile() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/app -> ../../Cargo.lock
    let lockfile = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("Cargo.lock"))
        .expect("locate workspace root");

    if !lockfile.exists() {
        eprintln!(
            "skipping: Cargo.lock not generated yet at {} \
             (run `cargo generate-lockfile` to populate)",
            lockfile.display()
        );
        return;
    }

    let text = std::fs::read_to_string(&lockfile).expect("read Cargo.lock");
    // Zed forks font-kit and publishes it as `zed-font-kit`. Either name in
    // the lockfile means the feature was resolved. If both are missing,
    // `gpui_platform` was built without `features = ["font-kit"]`.
    let has_font_kit =
        text.contains("name = \"font-kit\"") || text.contains("name = \"zed-font-kit\"");
    assert!(
        has_font_kit,
        "Cargo.lock has neither `font-kit` nor `zed-font-kit` — `gpui_platform` \
         was resolved without `features = [\"font-kit\"]`. This was the v0.9 \
         silent-glyph failure. Re-enable the feature in the root workspace Cargo.toml."
    );
}
