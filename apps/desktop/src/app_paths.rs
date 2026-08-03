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

use std::path::{Path, PathBuf};

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

/// Close the data root to every other account on the machine, on every boot.
///
/// Everything [`data_dir`] holds is private to the person running the app:
/// `oximux.db` is every transcript of every project, `computer-use-grants.json`
/// records which agent may drive this desktop, and the relay's token is a live
/// credential. None of it was ever meant to be world-readable, but until this
/// existed none of it was closed either — the directory was created with the
/// process umask and left that way, so a default install published its
/// transcripts to every other user on the machine.
///
/// Deliberately unconditional and deliberately early. The restriction used to
/// happen only as a side effect of enabling local CLI access, which is
/// default-off — so the exposed state was the *default* state, and the one
/// install least likely to be hardened was the one that had turned nothing on.
///
/// Returns the error rather than logging it: the caller decides whether an
/// unprotectable data directory is worth continuing past, and it is not this
/// module's business to decide that quietly. `Ok(())` with nothing done when
/// the platform has no data directory to resolve — there is no directory to
/// protect, and failing here would block boot over a hypothetical.
///
/// `oximux serve` (headless) must make the same call on its own data root; it
/// cannot reach this module, so it calls
/// [`oximux_owner_only::prepare_owner_only_dir`] directly.
pub fn harden_data_dir() -> std::io::Result<()> {
    let Some(dir) = data_dir() else {
        return Ok(());
    };
    oximux_owner_only::prepare_owner_only_dir(&dir)
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

/// The directory a pre-`app_paths` build would have used, when that is a
/// different place from [`data_dir`].
///
/// `None` on macOS and Linux, where `data_dir` and `data_local_dir` are the
/// same directory and there is nothing to migrate. `Some` on Windows, where
/// the old spelling resolved to the *roaming* profile.
fn legacy_data_dir() -> Option<PathBuf> {
    let legacy = dirs::data_dir()?.join(APP_DATA_SUBDIR);
    (Some(&legacy) != data_dir().as_ref()).then_some(legacy)
}

/// Adopt anything an older build left in the roaming profile.
///
/// Seven modules kept resolving through `dirs::data_dir()` after this module
/// was introduced, so on Windows their files went to `%APPDATA%` while
/// everything else went to `%LOCALAPPDATA%`. Repointing them is not enough on
/// its own: the files are already written, and one of them —
/// `computer-use-grants.json`, the record of which agent may drive this
/// desktop — is not something to silently abandon and re-prompt for.
///
/// Move-if-absent, never clobber. A name that already exists in the current
/// directory means the app has been writing there and that copy is the live
/// one; the stale roaming copy is left where it is rather than overwriting
/// real state. Anything that cannot be moved is skipped, because failing to
/// migrate is recoverable and losing the file is not.
///
/// Returns what moved, for the caller to log.
pub fn migrate_legacy_data() -> Vec<PathBuf> {
    let (Some(legacy), Some(current)) = (legacy_data_dir(), data_dir()) else {
        return Vec::new();
    };
    adopt_from(&legacy, &current)
}

/// The body of [`migrate_legacy_data`], against explicit directories so it is
/// testable without touching the real profile.
fn adopt_from(legacy: &Path, current: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(legacy) else {
        return Vec::new();
    };
    let mut moved = Vec::new();
    for entry in entries.flatten() {
        let from = entry.path();
        let Some(name) = from.file_name() else {
            continue;
        };
        let to = current.join(name);
        if to.exists() {
            // Both places hold this name. Normally that means the current one
            // is live and the legacy one is stale, and the stale one is left
            // alone rather than risking real state. The exception is when they
            // are byte-identical: then the legacy copy carries no information
            // that the current one does not, so dropping it is lossless — and
            // it is what lets the legacy directory finally go away instead of
            // lingering in a roaming profile forever.
            if let (Ok(a), Ok(b)) = (std::fs::read(&from), std::fs::read(&to))
                && a == b
            {
                let _ = std::fs::remove_file(&from);
            }
            continue;
        }
        if std::fs::create_dir_all(current).is_err() {
            return moved;
        }
        // Same volume in every real install (both live under the user's
        // profile), so this is a rename. Anything else — a redirected
        // roaming share, a locked file — is left alone rather than risking
        // a half-copied move.
        if std::fs::rename(&from, &to).is_ok() {
            moved.push(to);
        }
    }
    // Only when we emptied it. `remove_dir` refuses a non-empty directory,
    // which is exactly the check wanted: whatever we declined to move stays
    // reachable.
    let _ = std::fs::remove_dir(legacy);
    moved
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

    /// The CLI dials where the desktop binds: `remote-local` carries its own
    /// copy of this path (the CLI cannot depend on the app), and a drift
    /// between the two reads as "host unreachable" with both sides healthy.
    #[test]
    fn control_socket_convention_matches_the_data_dir() {
        assert_eq!(data_dir(), oximux_remote_local::default_runtime_dir());
    }

    /// A cold start with local CLI access **disabled** still leaves the data
    /// root closed to other accounts.
    ///
    /// This is the regression the whole change exists for. Hardening used to
    /// ride along on `LocalControlListener::bind`, which only runs when local
    /// access is enabled — default-off — so the shipped default was a
    /// world-readable directory holding every transcript. The assertion that
    /// matters is the one made *without* a listener anywhere in sight, which is
    /// why this test starts no socket and touches no setting.
    #[test]
    fn a_cold_start_without_local_access_still_closes_the_data_root() {
        let base = tempfile::tempdir().expect("tempdir");
        // Stand in for the real data root: an existing directory left
        // world-readable by an older build, holding the file that matters.
        let dir = base.path().join(APP_DATA_SUBDIR);
        std::fs::create_dir_all(&dir).expect("seed dir");
        std::fs::write(dir.join("oximux.db"), b"transcripts").expect("seed db");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
                .expect("widen to the pre-fix state");
        }

        // Exactly what `harden_data_dir` does, against a directory a test may
        // own — it resolves the real profile path, which a test must not touch.
        oximux_owner_only::prepare_owner_only_dir(&dir).expect("harden");

        assert!(
            oximux_owner_only::is_dir_restricted_to_owner(&dir).expect("read back"),
            "the data root must be owner-only with no listener ever bound"
        );
        assert_eq!(
            std::fs::read(dir.join("oximux.db")).expect("db survives"),
            b"transcripts".to_vec(),
            "hardening must not disturb existing state"
        );
    }


    /// Guards the migration that introduced this module: every caller used to
    /// resolve through `dirs::data_dir()`, and on macOS the switch to
    /// `data_local_dir` has to be a no-op or existing installs lose their data.
    ///
    /// Note what this cannot do. It is true by construction on macOS and says
    /// nothing anywhere else, which is why seven modules kept the old spelling
    /// for so long — there was no platform on which a test could notice. The
    /// check that actually holds the line is `xtask data-dir-lint`, which
    /// reads the source rather than the host.
    #[test]
    #[cfg(target_os = "macos")]
    fn data_local_dir_is_the_same_place_as_data_dir_on_macos() {
        assert_eq!(dirs::data_local_dir(), dirs::data_dir());
        assert!(
            legacy_data_dir().is_none(),
            "nothing to migrate where the two roots agree"
        );
    }

    /// Scratch pair of directories standing in for roaming and local.
    fn dirs_pair(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "oximux-adopt-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let legacy = base.join("Roaming").join(APP_DATA_SUBDIR);
        let current = base.join("Local").join(APP_DATA_SUBDIR);
        std::fs::create_dir_all(&legacy).expect("legacy dir");
        (legacy, current)
    }

    #[test]
    fn a_file_left_in_the_old_place_is_adopted() {
        let (legacy, current) = dirs_pair("adopt");
        std::fs::write(legacy.join("computer-use-grants.json"), b"{}").expect("seed");

        let moved = adopt_from(&legacy, &current);

        assert_eq!(moved.len(), 1, "the grant store should have moved");
        assert!(current.join("computer-use-grants.json").is_file());
        assert!(
            !legacy.exists(),
            "an emptied legacy directory should not be left behind"
        );
    }

    #[test]
    fn live_state_is_never_clobbered_by_a_stale_copy() {
        // The app has been writing to the current location; the roaming copy
        // is whatever an older build wrote before the paths were unified.
        // Overwriting the live one with it would be data loss.
        let (legacy, current) = dirs_pair("clobber");
        std::fs::create_dir_all(&current).expect("current dir");
        std::fs::write(current.join("grants.json"), b"live").expect("seed live");
        std::fs::write(legacy.join("grants.json"), b"stale").expect("seed stale");

        let moved = adopt_from(&legacy, &current);

        assert!(moved.is_empty(), "nothing should have moved");
        assert_eq!(
            std::fs::read(current.join("grants.json")).expect("read"),
            b"live".to_vec()
        );
        assert!(
            legacy.join("grants.json").is_file(),
            "the copy we declined to move must stay reachable, not be deleted"
        );
    }

    #[test]
    fn an_identical_copy_in_the_old_place_is_dropped() {
        // The case the real install landed in: both directories ended up with
        // an empty grant store, so there is nothing to carry over — but
        // leaving the legacy one behind means it keeps roaming.
        let (legacy, current) = dirs_pair("identical");
        std::fs::create_dir_all(&current).expect("current dir");
        let body = br#"{"grants":{}}"#;
        std::fs::write(current.join("computer-use-grants.json"), body).expect("seed current");
        std::fs::write(legacy.join("computer-use-grants.json"), body).expect("seed legacy");

        let moved = adopt_from(&legacy, &current);

        assert!(moved.is_empty(), "nothing moved — the current copy stands");
        assert_eq!(
            std::fs::read(current.join("computer-use-grants.json")).expect("read"),
            body.to_vec(),
            "the live copy must be untouched"
        );
        assert!(
            !legacy.exists(),
            "a redundant legacy copy should be dropped and its directory removed"
        );
    }

    #[test]
    fn directories_move_too_and_a_missing_legacy_root_is_a_no_op() {
        let (legacy, current) = dirs_pair("subdir");
        std::fs::create_dir_all(legacy.join("speech-models")).expect("seed dir");
        std::fs::write(legacy.join("speech-models/model.onnx"), b"x").expect("seed file");

        let moved = adopt_from(&legacy, &current);
        assert_eq!(moved.len(), 1);
        assert!(current.join("speech-models/model.onnx").is_file());

        // Second run has nothing to do and must not panic or report movement.
        assert!(adopt_from(&legacy, &current).is_empty());
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
