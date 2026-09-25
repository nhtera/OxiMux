//! Locating the helper executables that ship beside the running binary.
//!
//! The packaged app carries its helpers — the PTY relay, `rg`, the simulator
//! helper — as siblings of the main executable: `Contents/MacOS/` in the macOS
//! bundle, the install directory on Windows. Dev builds get the same layout for
//! free, because cargo (and `scripts/build-sim-helper.sh`) put every binary in
//! `target/<profile>/`.
//!
//! # Why a crate
//!
//! The lookup is small but easy to get subtly wrong, and every copy of it got
//! a different part wrong at some point: forgetting `EXE_SUFFIX` makes a
//! Windows bundle miss a perfectly good `rg.exe` and quietly fall back to
//! PATH, and a stale env override that silently falls through to the sibling
//! hides the override being broken. The callers share no other dependency (the
//! desktop app, the relay supervisor, the simulator crate), so one small crate
//! keeps the rule in one place. Callers keep their own *policy* — whether a
//! missing helper is an error or a PATH fallback — on top of [`locate`].

use std::fmt;
use std::path::{Path, PathBuf};

/// Why a helper binary could not be located.
#[derive(Debug)]
pub enum LocateError {
    /// The override variable is set but names nothing. Deliberately not a
    /// fallback to the sibling: an override exists to be obeyed, and falling
    /// through would hide that it is broken.
    OverrideMissing { var: String, path: PathBuf },
    /// No sibling at `path`.
    NotFound { file: String, path: PathBuf, var: Option<String> },
    /// The running binary's own location is unknown.
    NoCurrentExe(String),
}

impl fmt::Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocateError::OverrideMissing { var, path } => {
                write!(f, "{var}={} does not exist", path.display())
            }
            LocateError::NotFound { file, path, var: Some(var) } => write!(
                f,
                "expected {file} binary at {} (override with {var})",
                path.display()
            ),
            LocateError::NotFound { file, path, var: None } => {
                write!(f, "expected {file} binary at {}", path.display())
            }
            LocateError::NoCurrentExe(why) => write!(f, "current_exe: {why}"),
        }
    }
}

impl std::error::Error for LocateError {}

/// A helper's file name on this platform: `stem` plus `EXE_SUFFIX` (`.exe` on
/// Windows, nothing elsewhere).
pub fn sibling_file_name(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

/// The helper `stem`: the path in `env_override` when that variable is set,
/// else the sibling of the running executable. Either must exist.
pub fn locate(stem: &str, env_override: Option<&str>) -> Result<PathBuf, LocateError> {
    if let Some(var) = env_override
        && let Ok(value) = std::env::var(var)
    {
        let path = PathBuf::from(value);
        if !path.exists() {
            return Err(LocateError::OverrideMissing { var: var.to_owned(), path });
        }
        return Ok(path);
    }
    let exe = std::env::current_exe().map_err(|e| LocateError::NoCurrentExe(e.to_string()))?;
    let dir = exe
        .parent()
        .ok_or_else(|| LocateError::NoCurrentExe("current_exe has no parent".into()))?;
    sibling_in(dir, stem, env_override)
}

/// [`locate`]'s sibling half against an explicit directory, so it can be
/// tested without controlling `current_exe()`.
fn sibling_in(dir: &Path, stem: &str, env_override: Option<&str>) -> Result<PathBuf, LocateError> {
    let file = sibling_file_name(stem);
    let path = dir.join(&file);
    if !path.exists() {
        return Err(LocateError::NotFound { file, path, var: env_override.map(str::to_owned) });
    }
    Ok(path)
}

/// Whether `path` is a regular file this process may execute.
#[cfg(unix)]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Whether `path` is a regular file (Windows has no execute bit).
#[cfg(not(unix))]
pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_carries_the_platform_suffix() {
        assert_eq!(sibling_file_name("rg"), format!("rg{}", std::env::consts::EXE_SUFFIX));
    }

    #[test]
    fn sibling_found_in_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(sibling_file_name("helper"));
        std::fs::write(&path, "").expect("write");
        assert_eq!(sibling_in(dir.path(), "helper", None).expect("found"), path);
    }

    #[test]
    fn missing_sibling_names_the_path_and_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = sibling_in(dir.path(), "oximux-relay", Some("OXIMUX_RELAY_BINARY")).unwrap_err();
        let expected = dir.path().join(sibling_file_name("oximux-relay"));
        assert_eq!(
            err.to_string(),
            format!(
                "expected {} binary at {} (override with OXIMUX_RELAY_BINARY)",
                sibling_file_name("oximux-relay"),
                expected.display()
            )
        );
    }

    #[test]
    fn missing_sibling_is_not_found() {
        // Test binaries live in target/<profile>/deps/ with no such sibling.
        assert!(matches!(
            locate("definitely-not-a-real-tool-name", None),
            Err(LocateError::NotFound { var: None, .. })
        ));
    }

    #[test]
    fn override_wins_and_must_exist() {
        // A unique variable name: tests run in parallel in one process.
        let var = "OXIMUX_SIBLING_BINARY_TEST_OVERRIDE";
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("custom");
        std::fs::write(&file, "").expect("write");

        // SAFETY: the variable is unique to this test; nothing else reads it.
        unsafe { std::env::set_var(var, &file) };
        assert_eq!(locate("anything", Some(var)).expect("override"), file);

        let gone = dir.path().join("gone");
        unsafe { std::env::set_var(var, &gone) };
        let err = locate("anything", Some(var)).unwrap_err();
        assert_eq!(err.to_string(), format!("{var}={} does not exist", gone.display()));
        unsafe { std::env::remove_var(var) };
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
