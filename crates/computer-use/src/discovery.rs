//! Locating the `cua-driver` executable.
//!
//! OxiMux does not ship or download the driver — the user installs it, which
//! puts a notarized `CuaDriver.app` in `/Applications` and a symlink on `PATH`.
//! Preferring the bundle over the symlink matters for verification: the grants
//! and the signature belong to the bundle, and reporting the real path keeps
//! diagnostics honest about what was actually checked.

use std::path::{Path, PathBuf};

use crate::Error;

/// Overrides the search entirely. Exists for tests and for a developer running
/// a locally built driver; not surfaced in settings.
pub const PATH_OVERRIDE_ENV: &str = "OXIMUX_CUA_DRIVER";

/// Executable path inside the installed app bundle.
const BUNDLE_SUFFIX: &str = "CuaDriver.app/Contents/MacOS/cua-driver";

/// Bare executable name, for the `PATH` sweep.
const EXE_NAME: &str = "cua-driver";

/// Find the driver, or explain what was searched.
///
/// Order is deliberate: an explicit override wins, then the two standard
/// install locations, then `PATH`. `PATH` is last because the installer puts a
/// *symlink* there pointing back into the bundle — reaching it first would
/// work, but every later step would be reasoning about the link rather than
/// the signed binary.
pub fn locate() -> Result<PathBuf, Error> {
    let mut searched = Vec::new();

    if let Some(raw) = std::env::var_os(PATH_OVERRIDE_ENV) {
        let path = PathBuf::from(raw);
        // An override that does not exist is a mistake worth reporting rather
        // than silently falling through to a different driver than the one the
        // developer asked for.
        return if is_executable(&path) {
            Ok(resolve(&path))
        } else {
            Err(Error::NotFound {
                searched: vec![format!("${PATH_OVERRIDE_ENV}={}", path.display())],
            })
        };
    }

    for base in install_roots() {
        let candidate = base.join(BUNDLE_SUFFIX);
        if is_executable(&candidate) {
            return Ok(resolve(&candidate));
        }
        searched.push(candidate.display().to_string());
    }

    if let Some(found) = search_path() {
        return Ok(resolve(&found));
    }
    searched.push(format!("$PATH ({EXE_NAME})"));

    Err(Error::NotFound { searched })
}

/// Directories an installed `CuaDriver.app` can live in. Shared with the
/// in-app installer (`install`), which writes to the first writable root —
/// one list, so discovery and installation can never drift apart.
pub(crate) fn install_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    roots
}

/// First `PATH` entry holding an executable `cua-driver`.
fn search_path() -> Option<PathBuf> {
    let raw = std::env::var_os("PATH")?;
    std::env::split_paths(&raw)
        .map(|dir| dir.join(EXE_NAME))
        .find(|candidate| is_executable(candidate))
}

/// Follow symlinks so the caller holds the real binary. Falls back to the
/// original path when canonicalization fails — a working driver behind an
/// unreadable link is still better than refusing to run.
fn resolve(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
    use std::fs;

    #[cfg(unix)]
    fn write_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, "#!/bin/sh\n").expect("write");
        let mut perms = fs::metadata(path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod");
    }

    #[test]
    fn a_plain_file_is_not_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cua-driver");
        fs::write(&path, "not runnable").expect("write");
        assert!(!is_executable(&path));
    }

    #[cfg(unix)]
    #[test]
    fn an_executable_bit_is_required() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cua-driver");
        write_executable(&path);
        assert!(is_executable(&path));
    }

    #[test]
    fn a_missing_file_is_not_executable() {
        assert!(!is_executable(Path::new("/nonexistent/cua-driver")));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_follows_a_symlink_to_the_real_binary() {
        // Mirrors the real install: `~/.local/bin/cua-driver` is a symlink into
        // the app bundle, and verification must run against the bundle.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real-cua-driver");
        write_executable(&real);
        let link = dir.path().join("linked-cua-driver");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert_eq!(
            resolve(&link),
            real.canonicalize().expect("canonicalize real")
        );
    }

    #[test]
    fn install_roots_lead_with_the_system_applications_dir() {
        assert_eq!(install_roots()[0], PathBuf::from("/Applications"));
    }
}
