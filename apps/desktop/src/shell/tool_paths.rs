//! Locating bundled helper CLI tools.
//!
//! The packaged app carries `rg` in `Contents/MacOS/` beside the main binary
//! (see `scripts/fetch-ripgrep.sh` + `bundle-macos.sh`), so search works on a
//! machine with no system ripgrep. Dev builds (`cargo run`, tests) have no
//! bundle — they fall back to PATH, which is the pre-bundling behavior.

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

/// A sibling executable of the running binary (`Contents/MacOS/<name>` in a
/// bundle), if it exists and is executable.
fn bundled_sibling(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join(name);
    is_executable(&candidate).then_some(candidate)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
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
        assert!(!is_executable(&path));
    }
}
