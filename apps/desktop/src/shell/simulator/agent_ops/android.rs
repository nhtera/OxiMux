//! The app verbs on an Android device (`adb`, where iOS uses `simctl`):
//! launch a package, open a URL, install an APK. Everything else — touch,
//! keys, the AX tree, screenshots — goes through the session, as on iOS.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oximux_remote_proto::simulator::SimErrorWire;
use oximux_simulator::android::adb::Adb;
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::session::StreamSession;

const APP_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(180);

/// `adb` and the serial of a streaming Android session.
pub(super) struct Device {
    adb: PathBuf,
    serial: String,
}

impl Device {
    pub(super) fn of(session: &StreamSession, adb: Option<PathBuf>) -> Result<Self, SimErrorWire> {
        let serial = session.android().map(|a| a.serial().to_owned()).ok_or_else(|| SimErrorWire::Failed("not an Android device".into()))?;
        let adb = adb.ok_or_else(|| SimErrorWire::Unavailable("the Android SDK was not found; see `oximux sim status`".into()))?;
        Ok(Self { adb, serial })
    }

    fn shell(&self, args: &[&str], timeout: Duration) -> Result<String, String> {
        Adb::new(&SystemRunner, &self.adb).shell(&self.serial, args, timeout).map_err(|e| e.to_string())
    }

    /// Start `package`'s launcher activity (restarting it first when asked).
    pub(super) fn launch(&self, package: &str, relaunch: bool) -> Result<(), SimErrorWire> {
        if !is_package(package) {
            return Err(SimErrorWire::BadInput(format!("`{package}` is not an Android package name")));
        }
        if relaunch {
            // Not running is fine: launching is what matters.
            let _ = self.shell(&["am", "force-stop", package], APP_TIMEOUT);
        }
        let out = self
            .shell(&["monkey", "-p", package, "-c", "android.intent.category.LAUNCHER", "1"], APP_TIMEOUT)
            .map_err(|e| SimErrorWire::Failed(format!("launch failed: {e}")))?;
        // monkey exits 0 even when nothing was launched; it says so instead.
        if out.contains("No activities found") || out.contains("monkey aborted") {
            return Err(SimErrorWire::NotFound(format!("`{package}` is not installed, or has no launcher activity")));
        }
        Ok(())
    }

    /// Open `url` with whatever handles it (a browser, an app link).
    pub(super) fn open_url(&self, url: &str) -> Result<(), SimErrorWire> {
        // The device's shell reads the line: the URL is one quoted word.
        let quoted = format!("'{}'", url.replace('\'', r"'\''"));
        self.shell(&["am", "start", "-a", "android.intent.action.VIEW", "-d", &quoted], APP_TIMEOUT)
            .map(drop)
            .map_err(|e| SimErrorWire::Failed(format!("could not open the URL: {e}")))
    }

    /// Install (or replace) the APK at `path`.
    pub(super) fn install(&self, path: &Path) -> Result<(), SimErrorWire> {
        Adb::new(&SystemRunner, &self.adb)
            .install(&self.serial, path, INSTALL_TIMEOUT)
            .map_err(|e| SimErrorWire::Failed(format!("install failed: {e}")))
    }
}

/// `com.example.app`: letters, digits, `_`, dot-separated, each part starting
/// with a letter.
pub(super) fn is_package(name: &str) -> bool {
    name.split('.').count() >= 2
        && name.split('.').all(|part| {
            part.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// `path` (relative paths are the worktree's) as an APK inside the worktree.
pub(super) fn apk_path(path: &str, worktree: &Path) -> Result<PathBuf, SimErrorWire> {
    let path = Path::new(path);
    let path = if path.is_absolute() { path.to_path_buf() } else { worktree.join(path) };
    let path = std::fs::canonicalize(&path).map_err(|_| SimErrorWire::BadInput(format!("{} does not exist", path.display())))?;
    if path.extension().and_then(|e| e.to_str()) != Some("apk") || !path.is_file() {
        return Err(SimErrorWire::BadInput(format!("{} is not an .apk", path.display())));
    }
    let root = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    if !path.starts_with(&root) {
        return Err(SimErrorWire::PathOutsideWorktree);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_are_checked() {
        for good in ["com.example.app", "org.chromium.chrome", "a.b_c.D2"] {
            assert!(is_package(good), "{good}");
        }
        for bad in ["", "com", "com..app", "1com.app", "com.app;rm", "com.app name", "com.-app"] {
            assert!(!is_package(bad), "{bad}");
        }
    }

    #[test]
    fn only_an_apk_inside_the_worktree_installs() {
        let worktree = tempfile::tempdir().unwrap();
        let outputs = worktree.path().join("app/build/outputs/apk/debug");
        std::fs::create_dir_all(&outputs).unwrap();
        std::fs::write(outputs.join("app-debug.apk"), b"PK").unwrap();
        std::fs::write(outputs.join("notes.txt"), b"x").unwrap();
        assert!(apk_path("app/build/outputs/apk/debug/app-debug.apk", worktree.path()).is_ok());
        assert!(matches!(apk_path("app/build/outputs/apk/debug/notes.txt", worktree.path()), Err(SimErrorWire::BadInput(_))));
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("x.apk"), b"PK").unwrap();
        assert!(matches!(
            apk_path(&elsewhere.path().join("x.apk").to_string_lossy(), worktree.path()),
            Err(SimErrorWire::PathOutsideWorktree)
        ));
    }
}
