//! The `oximux` command line tool this app ships, and how OxiMux's terminals
//! find it.
//!
//! # Where it lives
//!
//! - **macOS:** the binary is `Contents/MacOS/oximux-cli`, beside the relay it
//!   finds as a sibling, and `Contents/Resources/bin/oximux` links to it. That
//!   directory is the one put on PATH: `Contents/MacOS` would also expose the
//!   app binary itself as `oximux`.
//! - **Windows:** `oximux-cli.exe` ships at the top of the install, the only
//!   shape an already-installed updater can deliver (its swap creates no
//!   folders), and this module copies it to `<install>\bin\oximux.exe` at boot.
//!   `bin\` is what goes on PATH, for the same reason as on macOS.
//! - **Elsewhere, and in dev builds:** nothing. `cargo run` has no bundle; a
//!   debug build may point [`DEV_OVERRIDE_ENV`] at a directory to try it.
//!
//! # Who gets it
//!
//! Only interactive terminal shells (`terminal_view::spawn_settings`), never
//! the app's own process PATH: agents the app spawns inherit that, and desktop
//! agents carry no per-session credential yet, so a CLI on their PATH would act
//! with the operator's full scope. The two variables exported to a shell are
//! outputs only — an OxiMux started from an OxiMux pane drops them at boot
//! ([`scrub_inherited`]) rather than adopting another build's CLI.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The directory a shell puts first on PATH, re-applied by the shell
/// integration overlays after the user's rc files ran.
pub const BIN_DIR_ENV: &str = "OXIMUX_CLI_BIN_DIR";
/// The bundled CLI's absolute path, for a login sub-shell (an agent's tool
/// shell) that rebuilds PATH from the user's profile.
pub const CLI_ENV: &str = "OXIMUX_CLI";
/// Debug builds only: a directory holding an `oximux` to treat as bundled.
pub const DEV_OVERRIDE_ENV: &str = "OXIMUX_DEV_CLI_BIN_DIR";

/// The bundled CLI's directory, resolved (and on Windows, refreshed) once.
pub fn bin_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(resolve).as_deref()
}

fn resolve() -> Option<PathBuf> {
    if cfg!(debug_assertions)
        && let Some(dir) = std::env::var_os(DEV_OVERRIDE_ENV).map(PathBuf::from)
    {
        return usable_dir(&dir).then_some(dir);
    }
    let exe = std::env::current_exe().ok()?;
    if cfg!(target_os = "macos") {
        macos_bin_dir(&std::fs::canonicalize(&exe).unwrap_or(exe))
    } else {
        windows_bin_dir(&exe)
    }
}

/// The install's `bin\`, refreshed. Not from a cargo build tree — the
/// updater's own test for "is this an install" — so `cargo run` neither grows a
/// `bin\` in `target\debug` nor puts a build's CLI on its terminals' PATH.
#[cfg(windows)]
fn windows_bin_dir(exe: &Path) -> Option<PathBuf> {
    let install = oximux_auto_update::windows::install::install_dir_of(exe)?;
    match refresh_windows_copy(&install) {
        Ok(dir) => dir,
        Err(err) => {
            tracing::warn!(%err, "could not refresh the bundled oximux command; terminals start without it");
            None
        }
    }
}

#[cfg(not(windows))]
fn windows_bin_dir(_exe: &Path) -> Option<PathBuf> {
    None
}

/// `…/X.app/Contents/Resources/bin` for an app binary in `…/X.app/Contents/MacOS`
/// whose bundle carries the CLI link — unless the bundle runs from somewhere it
/// will not stay: a quarantine mirror (App Translocation) or a mounted disk
/// image, where a later image mounted under the same `/Volumes` name would
/// supply whatever `oximux` it pleased. A disk image is told from an app kept
/// on an external drive by being read-only — the same tests the updater makes.
fn macos_bin_dir(exe: &Path) -> Option<PathBuf> {
    use oximux_auto_update::bundle::{bundle_root_of, dir_is_writable, is_translocated};
    let bundle = bundle_root_of(exe)?;
    if is_translocated(&bundle) {
        return None;
    }
    if bundle.starts_with("/Volumes") && !bundle.parent().is_some_and(dir_is_writable) {
        return None;
    }
    let dir = bundle.join("Contents").join("Resources").join("bin");
    usable_dir(&dir).then_some(dir)
}

/// Whether `dir` can go on PATH: absolute, free of PATH separators, and
/// holding an `oximux`.
fn usable_dir(dir: &Path) -> bool {
    let separator = if cfg!(windows) { ';' } else { ':' };
    dir.is_absolute()
        && !dir.to_string_lossy().contains(separator)
        && dir.join(format!("oximux{}", std::env::consts::EXE_SUFFIX)).is_file()
}

/// Bring `<install>\bin\oximux.exe` in line with the shipped
/// `<install>\oximux-cli.exe`, and answer the directory. `Ok(None)` when this
/// install ships no CLI (a build before it did).
///
/// "In line" is size and modified time, which the copy carries over, so an
/// update — which replaces `oximux-cli.exe` — is noticed at the next boot. A
/// copy still running (an `oximux events` left open) cannot be overwritten but
/// can be renamed: it is moved aside under the `.old-` name the updater's own
/// boot sweep already retires once nothing holds it.
#[cfg_attr(not(windows), allow(dead_code, reason = "tested on every platform, used on Windows"))]
fn refresh_windows_copy(install: &Path) -> std::io::Result<Option<PathBuf>> {
    let suffix = std::env::consts::EXE_SUFFIX;
    let shipped = install.join(format!("oximux-cli{suffix}"));
    let Ok(source) = std::fs::metadata(&shipped) else {
        return Ok(None);
    };
    let dir = install.join("bin");
    let copy = dir.join(format!("oximux{suffix}"));
    let current = std::fs::metadata(&copy)
        .is_ok_and(|have| have.len() == source.len() && have.modified().ok() == source.modified().ok());
    if !current {
        std::fs::create_dir_all(&dir)?;
        let fresh = dir.join(format!(".oximux{suffix}.new"));
        std::fs::copy(&shipped, &fresh)?;
        if let Ok(modified) = source.modified() {
            std::fs::File::options().write(true).open(&fresh)?.set_modified(modified)?;
        }
        if copy.exists() {
            std::fs::rename(&copy, oximux_auto_update::release::swap::backup_path(&copy))?;
        }
        std::fs::rename(&fresh, &copy)?;
    }
    Ok(Some(dir))
}

/// Drop what an OxiMux pane exported to this process, if it was started from
/// one: the two variables, and the parent's CLI directory from `PATH`.
///
/// Call before any thread exists — it writes the environment.
pub fn scrub_inherited() {
    let inherited = std::env::var_os(BIN_DIR_ENV);
    // SAFETY: called at the top of `main`, before any other thread exists.
    unsafe {
        std::env::remove_var(BIN_DIR_ENV);
        std::env::remove_var(CLI_ENV);
    }
    if let (Some(dir), Some(path)) = (inherited, std::env::var_os("PATH")) {
        let kept = without(&path, Path::new(&dir));
        // SAFETY: as above.
        unsafe { std::env::set_var("PATH", kept) };
        tracing::info!("dropped an inherited OxiMux command line directory from PATH");
    }
}

/// `path` with every entry equal to `dir` removed.
fn without(path: &OsString, dir: &Path) -> OsString {
    let kept: Vec<PathBuf> = std::env::split_paths(path).filter(|entry| entry != dir).collect();
    std::env::join_paths(kept).unwrap_or_else(|_| path.clone())
}

/// The variables an interactive shell's spawn adds for the bundled CLI in
/// `dir`, given the app's own `PATH`.
pub fn shell_env(dir: Option<&Path>, path: Option<OsString>) -> Vec<(String, String)> {
    let Some(dir) = dir else {
        return Vec::new();
    };
    let rest = path.map(|p| without(&p, dir)).unwrap_or_default();
    let mut entries = vec![dir.to_path_buf()];
    entries.extend(std::env::split_paths(&rest));
    let cli = dir.join(format!("oximux{}", std::env::consts::EXE_SUFFIX));
    let mut env = vec![
        (BIN_DIR_ENV.to_string(), dir.to_string_lossy().into_owned()),
        (CLI_ENV.to_string(), cli.to_string_lossy().into_owned()),
    ];
    // Only a PATH that survives as text: the spawn env is strings, and a lossy
    // conversion would corrupt a non-UTF-8 entry the shell would otherwise have
    // inherited intact. Without it the overlays still put the dir first.
    if let Some(joined) = std::env::join_paths(entries).ok().and_then(|p| p.into_string().ok()) {
        env.push(("PATH".to_string(), joined));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake bundle under `root`: `X.app/Contents/{MacOS/oximux, Resources/bin/oximux}`.
    fn bundle(root: &Path, name: &str) -> PathBuf {
        let contents = root.join(name).join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).expect("mkdir");
        std::fs::create_dir_all(contents.join("Resources/bin")).expect("mkdir");
        let exe = format!("oximux{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(contents.join("MacOS/oximux"), "").expect("write");
        std::fs::write(contents.join("Resources/bin").join(&exe), "").expect("write");
        contents.join("MacOS/oximux")
    }

    #[test]
    fn an_installed_bundle_offers_its_bin_dir() {
        let root = tempfile::tempdir().expect("tempdir");
        let exe = bundle(root.path(), "OxiMux.app");
        let dir = macos_bin_dir(&exe).expect("bundled");
        assert!(dir.ends_with("OxiMux.app/Contents/Resources/bin"), "{}", dir.display());
    }

    #[test]
    fn a_bundle_that_will_not_stay_offers_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let translocated = bundle(&root.path().join("AppTranslocation/ABCD/d"), "OxiMux.app");
        assert_eq!(macos_bin_dir(&translocated), None);
        // A disk image's mount point, judged by path alone.
        assert_eq!(macos_bin_dir(Path::new("/Volumes/OxiMux/OxiMux.app/Contents/MacOS/oximux")), None);
    }

    #[test]
    fn a_build_tree_or_a_bundle_without_the_cli_offers_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let exe = bundle(root.path(), "OxiMux.app");
        let cli = format!("Resources/bin/oximux{}", std::env::consts::EXE_SUFFIX);
        std::fs::remove_file(exe.parent().unwrap().parent().unwrap().join(cli)).expect("rm");
        assert_eq!(macos_bin_dir(&exe), None);
        assert_eq!(macos_bin_dir(Path::new("/repo/target/debug/oximux")), None);
    }

    #[test]
    fn the_windows_copy_follows_the_shipped_cli() {
        let install = tempfile::tempdir().expect("tempdir");
        let suffix = std::env::consts::EXE_SUFFIX;
        let shipped = install.path().join(format!("oximux-cli{suffix}"));
        std::fs::write(&shipped, "v1").expect("write");

        let dir = refresh_windows_copy(install.path()).expect("refresh").expect("ships a CLI");
        let copy = dir.join(format!("oximux{suffix}"));
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "v1");
        // Unchanged: left alone (no backup appears).
        refresh_windows_copy(install.path()).expect("refresh");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // An update replaced the shipped CLI: the copy follows, and the old
        // one is set aside for the updater's boot sweep.
        std::fs::write(&shipped, "v2 longer").expect("write");
        refresh_windows_copy(install.path()).expect("refresh");
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "v2 longer");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| oximux_auto_update::release::swap::backup_original(n).is_some()),
            "the old copy is set aside under a name the boot sweep retires: {names:?}"
        );
    }

    #[test]
    fn an_install_without_the_cli_has_no_copy() {
        let install = tempfile::tempdir().expect("tempdir");
        assert_eq!(refresh_windows_copy(install.path()).expect("refresh"), None);
        assert!(!install.path().join("bin").exists());
    }

    #[test]
    fn a_shell_gets_the_dir_first_once_and_the_cli_by_path() {
        let dir = Path::new("/Applications/OxiMux.app/Contents/Resources/bin");
        let path = std::env::join_paths(["/opt/homebrew/bin", dir.to_str().unwrap(), "/usr/bin"]).unwrap();
        let env = shell_env(Some(dir), Some(path));
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&get("PATH")).collect();
        assert_eq!(entries, [dir, Path::new("/opt/homebrew/bin"), Path::new("/usr/bin")]);
        assert_eq!(get(BIN_DIR_ENV), dir.to_string_lossy());
        assert!(get(CLI_ENV).starts_with(&*dir.to_string_lossy()));
        // No bundled CLI: nothing at all.
        assert!(shell_env(None, Some(OsString::from("/usr/bin"))).is_empty());
    }

    #[test]
    fn an_inherited_dir_is_dropped_from_path() {
        let dir = Path::new("/Applications/OxiMux.app/Contents/Resources/bin");
        let path = std::env::join_paths([dir, Path::new("/usr/bin"), dir]).unwrap();
        let kept: Vec<PathBuf> = std::env::split_paths(&without(&path, dir)).collect();
        assert_eq!(kept, [Path::new("/usr/bin")]);
    }
}
