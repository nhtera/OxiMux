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

/// The first SDK root that has `adb`, in order: the folder chosen in
/// Settings, `ANDROID_HOME`, `ANDROID_SDK_ROOT`, then Android Studio's default
/// `~/Library/Android/sdk`. A folder the user chose wins over the environment.
pub fn discover(configured: Option<&Path>, env: impl Fn(&str) -> Option<OsString>, home: Option<&Path>) -> Option<Sdk> {
    let candidates = configured
        .map(Path::to_path_buf)
        .into_iter()
        .chain(env("ANDROID_HOME").map(PathBuf::from))
        .chain(env("ANDROID_SDK_ROOT").map(PathBuf::from))
        .chain(home.map(|h| h.join("Library/Android/sdk")));
    candidates.map(|root| Sdk { root }).find(|sdk| sdk.adb().is_file())
}

/// [`discover`] against this process's environment.
pub fn discover_here(configured: Option<&Path>) -> Option<Sdk> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    discover(configured, |k| std::env::var_os(k).filter(|v| !v.is_empty()), home.as_deref())
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
        let found = discover(Some(&chosen), env, Some(&studio)).unwrap();
        assert_eq!(found.root, env_home);
        assert!(!found.has_emulator(), "phones only");

        sdk_at(&chosen, false);
        assert_eq!(discover(Some(&chosen), env, Some(&studio)).unwrap().root, chosen);

        let studio_sdk = discover(None, |_| None, Some(&studio)).unwrap();
        assert_eq!(studio_sdk.root, studio.join("Library/Android/sdk"));
        assert!(studio_sdk.has_emulator());
        assert_eq!(discover(None, |_| None, Some(dir.path())), None);
    }
}
