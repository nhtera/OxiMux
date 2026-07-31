//! Where OxiMux keeps its files, decided once.
//!
//! Ten modules used to spell `dirs::data_dir().map(|d| d.join("dev.nhtera.oximux"))`
//! themselves, each with a copy of the bundle identifier and a comment asking
//! the reader to keep it in lockstep with `main.rs`. That worked while there
//! was one platform convention to encode. It stops working the moment there are
//! two, because the choice — Application Support here, `%LOCALAPPDATA%` there,
//! logs somewhere else again — has to be made ten times identically.
//!
//! So it is made here, once, and everything else calls these.

use std::path::PathBuf;

/// Bundle identifier, and the name of the directory everything lands in.
///
/// Must stay in lockstep with `CFBundleIdentifier` in `assets/Info.plist` —
/// that one is not Rust and cannot be checked by anything.
pub const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";

/// The app's data root: settings TOMLs, `oximux.db`, the relay's socket/pid/
/// token, session snapshots.
///
/// `data_local_dir` rather than `data_dir`: the two are the same directory on
/// macOS (`~/Library/Application Support`), but on Windows `data_dir` is the
/// *roaming* profile, which syncs to a domain server on login. A SQLite
/// database, a live pid file, and a named-pipe token are all things that must
/// not follow a user to another machine — the token especially, since it is a
/// credential for a daemon that is not running there.
pub fn data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join(APP_DATA_SUBDIR))
}

/// Scratch space: downloaded updates, model archives mid-extraction. Anything
/// here must be safe for the OS to delete between runs.
pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join(APP_DATA_SUBDIR))
}

/// Per-app logs, following each platform's own convention.
///
/// macOS puts them in `~/Library/Logs`, where Console.app looks — hence the
/// only place in here that reaches for the home directory rather than a
/// `dirs` root. Windows has no equivalent well-known location, so they sit
/// beside the rest of the app's data.
pub fn log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|home| home.join("Library/Logs").join(APP_DATA_SUBDIR))
    }
    #[cfg(not(target_os = "macos"))]
    {
        data_dir().map(|d| d.join("logs"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_lands_under_the_bundle_id() {
        let dir = data_dir().expect("a test runner always has a data directory");
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some(APP_DATA_SUBDIR),
            "settings, the database, and the relay's token all resolve from \
             this; a changed leaf silently orphans an existing install"
        );
    }

    /// Guards the migration that introduced this module: every caller used to
    /// resolve through `dirs::data_dir()`, and on macOS the switch to
    /// `data_local_dir` has to be a no-op or existing installs lose their data.
    #[test]
    #[cfg(target_os = "macos")]
    fn data_local_dir_is_the_same_place_as_data_dir_on_macos() {
        assert_eq!(dirs::data_local_dir(), dirs::data_dir());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn logs_go_where_console_app_looks() {
        let dir = log_dir().expect("a test runner always has a home directory");
        assert!(
            dir.ends_with(format!("Library/Logs/{APP_DATA_SUBDIR}")),
            "got {}",
            dir.display()
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn logs_sit_beside_the_rest_of_the_app_data() {
        let dir = log_dir().expect("a test runner always has a data directory");
        assert!(dir.starts_with(data_dir().unwrap()), "got {}", dir.display());
    }
}
