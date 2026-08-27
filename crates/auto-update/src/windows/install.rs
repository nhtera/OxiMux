//! Self-inspection on Windows: is this process an updatable install, and where
//! are the files it would have to replace?
//!
//! The macOS counterpart ([`crate::bundle`]) asks a second question — who
//! signed this — and pins updates to that identity. There is no equivalent
//! here, because the Windows artifacts are not Authenticode-signed; the
//! trust root is the minisign signature over the release manifest, checked
//! against a key compiled into this binary. See [`super`] for what that does
//! and does not buy.

use std::path::{Path, PathBuf};

use crate::bundle::UnsupportedReason;

/// The executable the installer places, and the one this process is.
pub const APP_EXE: &str = "oximux.exe";

/// An install the updater may act on.
///
/// Captured ONCE at boot and kept for the process lifetime, for the same
/// reason macOS captures its pin at boot: after a staged update lands, the
/// path holds different files, and a config re-derived at quit would describe
/// the thing it is supposed to be checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    /// The directory the installer owns — `oximux.exe` and every DLL, helper
    /// binary, and bundled tool beside it.
    pub install_dir: PathBuf,
}

/// Classify `exe` (normally `std::env::current_exe()`) as updatable or not.
///
/// Does a write probe, so call it off the UI thread.
pub fn eligibility(exe: &Path) -> Result<InstalledApp, UnsupportedReason> {
    let install_dir = install_dir_of(exe).ok_or(UnsupportedReason::NotABundle)?;

    // The swap renames the staging directory's contents into this one, and a
    // rename is only atomic within a filesystem — so the *parent* has to be
    // writable too, not just the install directory itself.
    let parent = install_dir
        .parent()
        .ok_or(UnsupportedReason::RootNotWritable)?;
    if !dir_is_writable(&install_dir) || !dir_is_writable(parent) {
        return Err(UnsupportedReason::RootNotWritable);
    }

    Ok(InstalledApp { install_dir })
}

/// The directory an installed `oximux.exe` lives in, or `None` when this is a
/// development build.
///
/// "Development build" is decided by the cargo layout around the executable —
/// `…/target/debug/oximux.exe`, or `…/target/<triple>/release/oximux.exe` —
/// rather than by `debug_assertions`, because `cargo build --release` produces
/// a release binary sitting in exactly the tree an update must never overwrite.
/// Replacing it would delete a developer's build output and leave cargo
/// convinced it was still up to date.
pub fn install_dir_of(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    if in_cargo_target_dir(dir) {
        return None;
    }
    Some(dir.to_path_buf())
}

/// `<…>/target/{debug,release}` or `<…>/target/<triple>/{debug,release}`.
fn in_cargo_target_dir(dir: &Path) -> bool {
    let profile = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if profile != "debug" && profile != "release" {
        return false;
    }
    let Some(above) = dir.parent() else {
        return false;
    };
    // Directly under `target/`, or one triple directory below it.
    let is_target = |p: &Path| p.file_name().and_then(|n| n.to_str()) == Some("target");
    is_target(above) || above.parent().is_some_and(is_target)
}

/// Probed by creating a file. Windows ACLs do not reduce to permission bits,
/// and the case that matters — an install someone moved into `Program Files`
/// by hand — refuses writes for reasons no metadata read reports.
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".oximux-write-probe-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    if ok {
        let _ = std::fs::remove_file(&probe);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_installed_exe_resolves_to_its_directory() {
        let dir = install_dir_of(Path::new(
            r"C:\Users\dev\AppData\Local\Programs\OxiMux\oximux.exe",
        ));
        assert_eq!(
            dir,
            Some(PathBuf::from(r"C:\Users\dev\AppData\Local\Programs\OxiMux"))
        );
    }

    /// The one that matters. `cargo build --release` is not a debug build and
    /// carries no marker of its own — only the path says so, and an updater
    /// that missed this would overwrite a developer's build output.
    #[test]
    fn a_cargo_build_is_never_an_installable_target() {
        for exe in [
            r"D:\Projects\OxiMux\target\debug\oximux.exe",
            r"D:\Projects\OxiMux\target\release\oximux.exe",
            r"D:\Projects\OxiMux\target\x86_64-pc-windows-msvc\release\oximux.exe",
            "/home/dev/oximux/target/debug/oximux",
        ] {
            assert_eq!(install_dir_of(Path::new(exe)), None, "{exe}");
        }
    }

    /// …but a real install may perfectly well sit in a folder called
    /// `release`, and refusing that would disable updates for it.
    #[test]
    fn a_directory_merely_named_release_is_still_an_install() {
        let exe = Path::new(r"C:\Tools\oximux\release\oximux.exe");
        assert_eq!(
            install_dir_of(exe),
            Some(PathBuf::from(r"C:\Tools\oximux\release"))
        );
    }
}
