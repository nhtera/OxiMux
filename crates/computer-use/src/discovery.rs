//! Locating the `cua-driver` executable.
//!
//! OxiMux does not *ship* the driver. It can install one (see
//! [`crate::install`]), and the user may install their own; either way the
//! result is the same layout, because the installer writes where upstream's
//! does. On macOS that is a notarized `CuaDriver.app` plus a symlink on `PATH`;
//! on Windows a junction at `%LOCALAPPDATA%\Programs\Cua\cua-driver\bin`.
//!
//! Preferring the known location over `PATH` matters for verification: on macOS
//! the grants and the signature belong to the bundle rather than to the symlink
//! pointing at it, and on both platforms reporting the real, resolved path
//! keeps diagnostics honest about which bytes were actually checked.

use std::path::{Path, PathBuf};

use crate::Error;

/// Overrides the search entirely. Exists for tests and for a developer running
/// a locally built driver; not surfaced in settings.
pub const PATH_OVERRIDE_ENV: &str = "OXIMUX_CUA_DRIVER";

/// Executable path inside the installed app bundle.
#[cfg(not(windows))]
const BUNDLE_SUFFIX: &str = "CuaDriver.app/Contents/MacOS/cua-driver";

/// Bare executable name, for the `PATH` sweep.
///
/// Carries `.exe` on Windows, and must: [`is_executable`] requires that
/// extension there, so a sweep for the bare name matches nothing at all and
/// discovery reports "not installed" for a driver sitting on `PATH`.
fn exe_name() -> String {
    format!("cua-driver{}", std::env::consts::EXE_SUFFIX)
}

/// Find the driver, or explain what was searched.
///
/// Order is deliberate: an explicit override wins, then the platform's install
/// location, then `PATH`. `PATH` is last because what sits there is a link back
/// into the install — a symlink on macOS, a junction on Windows. Reaching it
/// first would work, but every later step would be reasoning about the link
/// rather than about the bytes that run.
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

    // The known install location, checked before `PATH` for the same reason the
    // bundle is: it is where OxiMux's own installer places, so it is the path
    // whose contents this crate can reason about.
    #[cfg(windows)]
    if let Some(bin) = windows_bin_dir() {
        let candidate = bin.join(exe_name());
        if is_executable(&candidate) {
            return Ok(resolve(&candidate));
        }
        searched.push(candidate.display().to_string());
    }

    // The app-bundle roots are a macOS install layout; Windows has its own,
    // checked just above.
    #[cfg(not(windows))]
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
    searched.push(format!("$PATH ({})", exe_name()));
    // Naming the override in the failure is what makes `PATH`-only discovery
    // workable: it is the supported answer for an install that is not on `PATH`,
    // not just a test hook.
    #[cfg(windows)]
    searched.push(format!("${PATH_OVERRIDE_ENV} (unset)"));

    Err(Error::NotFound { searched })
}

/// The visible bin directory on Windows — a junction pointing at whichever
/// release is active (`%LOCALAPPDATA%\Programs\Cua\cua-driver\bin`).
///
/// Shared with the installer for the same reason the macOS bundle roots are:
/// one function means discovery and installation cannot drift apart. (No
/// intra-doc link to `install_roots` — it is `cfg(not(windows))`, so on the
/// only platform that compiles this the link would not resolve.) Upstream's
/// `CUA_DRIVER_RS_INSTALL_DIR` is honored so a user who moved their install has
/// one install rather than two.
///
/// `resolve()` canonicalizes what it finds here, so callers see the real
/// release directory — `\\?\`-prefixed, and pointing past the junction at the
/// bytes that will actually run.
#[cfg(windows)]
pub(crate) fn windows_bin_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("CUA_DRIVER_RS_INSTALL_DIR") {
        return Some(PathBuf::from(raw));
    }
    std::env::var_os("LOCALAPPDATA")
        .map(|local| PathBuf::from(local).join(r"Programs\Cua\cua-driver\bin"))
}

/// Directories an installed `CuaDriver.app` can live in. Shared with the
/// in-app installer (`install`), which writes to the first writable root —
/// one list, so discovery and installation can never drift apart.
///
/// macOS only: these are bundle roots, and the Windows installer places through
/// [`windows_bin_dir`] instead.
#[cfg(not(windows))]
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
    let name = exe_name();
    std::env::split_paths(&raw)
        .map(|dir| dir.join(&name))
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

/// Windows has no executable bit — what makes a file runnable is its
/// extension.
///
/// Pinned to `.exe` rather than consulting `PATHEXT`, because this is not
/// resolving an arbitrary command: it is confirming that one known binary is
/// what it claims to be. A `cua-driver.bat` or `cua-driver.cmd` sitting where
/// the driver belongs is precisely the substitution this should refuse, and
/// `PATHEXT` would wave both through.
///
/// The previous implementation accepted any regular file, which made
/// `a_plain_file_is_not_executable` fail here for the right reason. It was
/// written as a compiles-everywhere placeholder rather than as policy.
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
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

    /// The Windows half of the same rule: extension decides, and only `.exe`
    /// counts. A batch file named after the driver must not be mistaken for it.
    #[cfg(windows)]
    #[test]
    fn only_an_exe_counts_as_executable_on_windows() {
        let dir = tempfile::tempdir().expect("tempdir");

        let exe = dir.path().join("cua-driver.exe");
        fs::write(&exe, "MZ").expect("write");
        assert!(is_executable(&exe));

        for impostor in ["cua-driver.bat", "cua-driver.cmd", "cua-driver"] {
            let path = dir.path().join(impostor);
            fs::write(&path, "@exit /b 0").expect("write");
            assert!(
                !is_executable(&path),
                "{impostor} must not pass as the driver"
            );
        }

        // Windows paths are case-insensitive, and so is the check.
        let shouty = dir.path().join("CUA-DRIVER.EXE");
        fs::write(&shouty, "MZ").expect("write");
        assert!(is_executable(&shouty));
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

    #[cfg(not(windows))]
    #[test]
    fn install_roots_lead_with_the_system_applications_dir() {
        assert_eq!(install_roots()[0], PathBuf::from("/Applications"));
    }

    /// The Windows counterpart: the installer and discovery must name one
    /// directory, and it must be the one upstream's installer uses too.
    #[cfg(windows)]
    #[test]
    fn the_windows_bin_dir_is_upstreams_own() {
        let bin = windows_bin_dir().expect("%LOCALAPPDATA% is always set on Windows");
        assert!(
            bin.ends_with(r"Programs\Cua\cua-driver\bin"),
            "{}",
            bin.display()
        );
    }

    /// The `PATH` sweep and the executable test have to agree on the name, and
    /// on Windows they only do if the sweep carries `.exe`. When they disagree
    /// the failure is silent — an installed, working driver simply reads as
    /// "not installed" for ever.
    #[test]
    fn the_path_sweep_looks_for_a_name_that_could_pass_as_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let candidate = dir.path().join(exe_name());
        fs::write(&candidate, "MZ").expect("write");

        #[cfg(unix)]
        write_executable(&candidate);

        assert!(
            is_executable(&candidate),
            "{} must satisfy is_executable",
            candidate.display()
        );
    }
}
