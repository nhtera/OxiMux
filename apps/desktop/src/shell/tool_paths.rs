//! Locating bundled helper CLI tools.
//!
//! The packaged app carries `rg` beside the main binary — `Contents/MacOS/` in
//! the macOS bundle, the install directory on Windows (see
//! `scripts/fetch-ripgrep.sh` + `bundle-macos.sh`, and their `.ps1`
//! counterparts) — so search works on a machine with no system ripgrep. Dev
//! builds (`cargo run`, tests) have no bundle: they fall back to PATH, which is
//! the pre-bundling behavior.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Program to spawn for ripgrep: the bundled sibling when present, else the
/// bare name for a PATH lookup. Resolved once — callers spawn on every
/// keystroke-ish cadence and a stat per spawn adds nothing (the bundle does
/// not change under a running app).
pub fn rg_program() -> &'static Path {
    static RG: OnceLock<PathBuf> = OnceLock::new();
    RG.get_or_init(|| bundled_sibling("rg").unwrap_or_else(|| PathBuf::from("rg")))
        .as_path()
}

/// A sibling executable of the running binary, if it exists and is executable.
///
/// `name` is the *stem*: `oximux_sibling_binary` adds `EXE_SUFFIX`, so a
/// Windows bundle's `rg.exe` is found. The PATH fallback name is left bare
/// because a PATH lookup on Windows appends the extensions in `PATHEXT` itself.
fn bundled_sibling(name: &str) -> Option<PathBuf> {
    oximux_sibling_binary::locate(name, None)
        .ok()
        .filter(|path| oximux_sibling_binary::is_executable(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sibling_falls_back_to_path_name() {
        // Test binaries live in target/<profile>/deps/ with no `rg` sibling,
        // so this exercises the dev-build fallback branch.
        assert!(bundled_sibling("definitely-not-a-real-tool-name").is_none());
    }

    #[test]
    fn rg_program_is_spawnable_name_or_absolute_path() {
        let p = rg_program();
        // Either the bare PATH name or an absolute sibling path — never empty.
        assert!(p == Path::new("rg") || p.is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rg");
        std::fs::write(&path, "not runnable").expect("write");
        assert!(!oximux_sibling_binary::is_executable(&path));
    }

    /// The bundled sibling is looked for under the platform's executable name.
    /// The literals below are the point: `EXE_SUFFIX` on both sides would pass
    /// even if the suffix were dropped again.
    #[test]
    fn the_sibling_looked_for_carries_the_platform_extension() {
        let name = oximux_sibling_binary::sibling_file_name("rg");
        #[cfg(windows)]
        assert_eq!(name, "rg.exe");
        #[cfg(not(windows))]
        assert_eq!(name, "rg");
    }

    /// A bundle laid out for this platform is found; one laid out for the other
    /// is not. Locks the failure that started this: a Windows bundle shipping
    /// `rg.exe` while the lookup asked for `rg`.
    #[test]
    fn a_sibling_named_for_the_other_platform_is_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wrong = if cfg!(windows) { "rg" } else { "rg.exe" };
        std::fs::write(dir.path().join(wrong), "x").expect("write");
        assert!(!oximux_sibling_binary::is_executable(
            &dir.path().join(oximux_sibling_binary::sibling_file_name("rg"))
        ));
    }
}
