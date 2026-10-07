//! Finding the Android SDK. A GUI launch has no shell `PATH` (and often no
//! `ANDROID_HOME`), so the tools are found by SDK root, never by `PATH`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// An Android SDK with `platform-tools/adb` present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sdk {
    pub root: PathBuf,
}

impl Sdk {
    pub fn adb(&self) -> PathBuf {
        self.root.join("platform-tools").join("adb")
    }

    pub fn emulator(&self) -> PathBuf {
        self.root.join("emulator").join("emulator")
    }

    /// The emulator package is installed (without it: phones only).
    pub fn has_emulator(&self) -> bool {
        self.emulator().is_file()
    }
}

/// Where a standalone `adb` (Homebrew's `android-platform-tools`) links from,
/// Apple silicon first.
pub const STANDALONE_ADB: [&str; 2] = ["/opt/homebrew/bin/adb", "/usr/local/bin/adb"];

/// The first SDK root that has `adb`, in order: the folder chosen in
/// Settings, `ANDROID_HOME`, `ANDROID_SDK_ROOT`, Android Studio's default
/// `~/Library/Android/sdk`, then a standalone `adb` (`standalone`, see
/// [`STANDALONE_ADB`]). A folder the user chose wins over the environment,
/// and any SDK wins over a standalone `adb`: two adb servers of different
/// versions kill each other.
pub fn discover(
    configured: Option<&Path>,
    env: impl Fn(&str) -> Option<OsString>,
    home: Option<&Path>,
    standalone: &[&Path],
) -> Option<Sdk> {
    let candidates = configured
        .map(Path::to_path_buf)
        .into_iter()
        .chain(env("ANDROID_HOME").map(PathBuf::from))
        .chain(env("ANDROID_SDK_ROOT").map(PathBuf::from))
        .chain(home.map(|h| h.join("Library/Android/sdk")));
    candidates
        .map(|root| Sdk { root })
        .find(|sdk| sdk.adb().is_file())
        .or_else(|| standalone.iter().find_map(|adb| standalone_root(adb)).map(|root| Sdk { root }))
}

/// The root a standalone `adb` belongs to: the symlink resolved, and only
/// when it really sits in a `platform-tools` folder (so [`Sdk::adb`] names
/// it). Such an SDK has no emulator: phones only.
fn standalone_root(adb: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(adb).ok().filter(|p| p.is_file())?;
    let tools = real.parent().filter(|d| d.file_name().is_some_and(|n| n == "platform-tools"))?;
    tools.parent().map(Path::to_path_buf)
}

/// [`discover`] against this process's environment.
pub fn discover_here(configured: Option<&Path>) -> Option<Sdk> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let standalone: Vec<&Path> = STANDALONE_ADB.iter().map(Path::new).collect();
    discover(configured, |k| std::env::var_os(k).filter(|v| !v.is_empty()), home.as_deref(), &standalone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdk_at(root: &Path, emulator: bool) {
        std::fs::create_dir_all(root.join("platform-tools")).unwrap();
        std::fs::write(root.join("platform-tools/adb"), b"").unwrap();
        if emulator {
            std::fs::create_dir_all(root.join("emulator")).unwrap();
            std::fs::write(root.join("emulator/emulator"), b"").unwrap();
        }
    }

    #[test]
    fn the_first_root_with_adb_wins_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let (chosen, env_home, studio) = (dir.path().join("chosen"), dir.path().join("env"), dir.path().join("home"));
        sdk_at(&env_home, false);
        sdk_at(&studio.join("Library/Android/sdk"), true);
        let env = |k: &str| (k == "ANDROID_HOME").then(|| env_home.clone().into_os_string());

        // A chosen folder without adb is skipped; the environment is next.
        let found = discover(Some(&chosen), env, Some(&studio), &[]).unwrap();
        assert_eq!(found.root, env_home);
        assert!(!found.has_emulator(), "phones only");

        sdk_at(&chosen, false);
        assert_eq!(discover(Some(&chosen), env, Some(&studio), &[]).unwrap().root, chosen);

        let studio_sdk = discover(None, |_| None, Some(&studio), &[]).unwrap();
        assert_eq!(studio_sdk.root, studio.join("Library/Android/sdk"));
        assert!(studio_sdk.has_emulator());
        assert_eq!(discover(None, |_| None, Some(dir.path()), &[]), None);
    }

    /// Homebrew's `adb` is a symlink into a versioned `platform-tools`
    /// folder: the root is found through it (phones only), any SDK wins over
    /// it, and an `adb` outside a `platform-tools` folder is not taken.
    #[cfg(unix)]
    #[test]
    fn a_standalone_adb_is_found_through_its_symlink_last() {
        let dir = tempfile::tempdir().unwrap();
        let cask = dir.path().join("Caskroom/android-platform-tools/36.0.0");
        sdk_at(&cask, false);
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(cask.join("platform-tools/adb"), bin.join("adb")).unwrap();
        let link = bin.join("adb");

        let found = discover(None, |_| None, None, &[Path::new("/nonexistent/adb"), &link]).unwrap();
        assert_eq!(found.root, std::fs::canonicalize(&cask).unwrap());
        assert_eq!(found.adb(), std::fs::canonicalize(cask.join("platform-tools/adb")).unwrap());
        assert!(!found.has_emulator(), "phones only");

        let studio = dir.path().join("home");
        sdk_at(&studio.join("Library/Android/sdk"), true);
        assert_eq!(discover(None, |_| None, Some(&studio), &[&link]).unwrap().root, studio.join("Library/Android/sdk"), "an SDK wins");

        let loose = dir.path().join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        std::fs::write(loose.join("adb"), b"").unwrap();
        assert_eq!(discover(None, |_| None, None, &[&loose.join("adb")]), None, "not in a platform-tools folder");
    }
}
